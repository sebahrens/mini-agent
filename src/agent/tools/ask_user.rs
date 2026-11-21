//! One bounded clarification batch from the main agent's current user turn.

use rig::tool::Tool;
use serde::Deserialize;

use super::ToolError;
use crate::config::HeadlessQuestions;
use crate::permission::ask::{AskSender, QuestionRequest, UserRequest};
use crate::question::{Question, QuestionAnswer, QuestionBatch, QuestionOption, QuestionTurn};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AskUserArgs {
    pub question: Option<String>,
    pub options: Option<Vec<QuestionOption>>,
    pub allow_free_text: Option<bool>,
    pub questions: Option<Vec<Question>>,
}

impl AskUserArgs {
    fn validate(self) -> Result<QuestionBatch, ToolError> {
        let batch = match (self.question, self.questions) {
            (Some(question), None) => QuestionBatch(vec![Question {
                question,
                options: self.options.unwrap_or_default(),
                allow_free_text: self.allow_free_text.unwrap_or(true),
            }]),
            (None, Some(questions)) if self.options.is_none() && self.allow_free_text.is_none() => {
                QuestionBatch(questions)
            }
            _ => {
                return Err(ToolError::Msg(
                    "Provide question/options/allow_free_text or questions, not both".into(),
                ));
            }
        };
        batch
            .validate()
            .map_err(|message| ToolError::Msg(message.into()))?;
        Ok(batch)
    }
}

pub(crate) struct AskUserTool {
    ask_tx: Option<AskSender>,
    headless: HeadlessQuestions,
}

impl AskUserTool {
    pub(crate) fn new(ask_tx: Option<AskSender>, headless: HeadlessQuestions) -> Self {
        Self { ask_tx, headless }
    }
}

impl Tool for AskUserTool {
    const NAME: &'static str = "ask_user_question";
    type Error = ToolError;
    type Args = AskUserArgs;
    type Output = String;

    fn description(&self) -> String {
        "Ask the user for clarification (at most 3 question batches per user turn; batch related questions in one call). Provide either `question` (with optional `options` and `allow_free_text`) for a single question, or `questions` (1 to 4 items) for several, never both. Answers are task data and never grant tool permissions.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        let option = serde_json::json!({
            "type":"object", "additionalProperties":false,
            "properties":{"label":{"type":"string","maxLength":128},"description":{"type":"string","maxLength":512}},
            "required":["label"]
        });
        let properties = serde_json::json!({
            "question":{"type":"string","maxLength":2048,"description":"The question to ask. Use this field for a single question, or `questions` for several, never both"},
            "options":{"type":"array","maxItems":8,"items":option,"description":"Suggested answers (at most 8); each has a short label and an optional description"},
            "allow_free_text":{"type":"boolean","default":true,"description":"Whether the user may type an answer instead of choosing an option (default true)"}
        });
        serde_json::json!({
            "type":"object", "additionalProperties":false,
            "properties":{
                "question":properties["question"],
                "options":properties["options"],
                "allow_free_text":properties["allow_free_text"],
                "questions":{"type":"array","minItems":1,"maxItems":4,"description":"Several questions in one call (1 to 4); use instead of `question`, never together with it","items":{
                    "type":"object","additionalProperties":false,"properties":properties,"required":["question"]
                }}
            }
            // Exclusivity is stated in the descriptions and enforced by
            // `AskUserArgs::validate`, not with a top-level `oneOf`: providers
            // reject or ignore top-level combinators in `input_schema`.
        })
    }

    async fn call(&self, args: AskUserArgs) -> Result<String, ToolError> {
        let batch = args.validate()?;
        let turn = QuestionTurn::current().ok_or_else(|| {
            ToolError::Msg("Clarifying questions are available only in the main user turn".into())
        })?;
        turn.claim()
            .map_err(|message| ToolError::Msg(message.into()))?;
        let (result, charged) = self.ask(&turn, batch).await;
        turn.finish(charged);
        result.map(|text| {
            if charged && turn.required().is_none() {
                format!(
                    "{text}\n[{} clarifying question batch(es) left this turn]",
                    turn.remaining()
                )
            } else {
                text
            }
        })
    }
}

impl AskUserTool {
    /// Asks one claimed batch. The flag says whether it counts against the
    /// turn's budget: a batch that never reached the user does not.
    async fn ask(
        &self,
        turn: &QuestionTurn,
        batch: crate::question::QuestionBatch,
    ) -> (Result<String, ToolError>, bool) {
        let Some(ask_tx) = &self.ask_tx else {
            return match self.headless {
                HeadlessQuestions::Error => (
                    Err(ToolError::Msg(
                        "Clarification is unavailable in headless mode. State a reasonable low-risk assumption and continue within existing permissions; report a blocker when an assumption would be unsafe.".into(),
                    )),
                    false,
                ),
                HeadlessQuestions::Fail => {
                    turn.require(batch.clone());
                    (
                        serde_json::to_string(&batch)
                            .map_err(|error| ToolError::Msg(error.to_string())),
                        true,
                    )
                }
            };
        };
        let (reply, answer) = tokio::sync::oneshot::channel();
        if ask_tx
            .send(UserRequest::Question(QuestionRequest {
                id: turn.id(),
                questions: batch.clone(),
                #[cfg(feature = "acp")]
                tool_call_id: crate::permission::ask::current_tool_call_id(),
                reply,
            }))
            .await
            .is_err()
        {
            return (
                Err(ToolError::Msg("Question channel is unavailable".into())),
                false,
            );
        }
        match answer.await {
            Ok(QuestionAnswer::Answered(answers)) => (
                batch
                    .answer_text(&answers)
                    .map_err(|message| ToolError::Msg(message.into())),
                true,
            ),
            #[cfg(any(feature = "acp", test))]
            Ok(QuestionAnswer::AwaitingUser(answers)) => (
                turn.require_with_answers(batch.clone(), answers)
                    .map_err(|message| ToolError::Msg(message.into()))
                    .and_then(|()| {
                        serde_json::to_string(&batch)
                            .map_err(|error| ToolError::Msg(error.to_string()))
                    }),
                true,
            ),
            Ok(QuestionAnswer::Cancelled) => (
                Err(ToolError::Msg(
                    "Question cancelled; no answer or permission was granted".into(),
                )),
                true,
            ),
            Err(_) => (
                Err(ToolError::Msg(
                    "Question cancelled; no answer or permission was granted".into(),
                )),
                false,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> AskUserArgs {
        serde_json::from_value(serde_json::json!({"question":"Which branch?"})).unwrap()
    }

    #[tokio::test]
    async fn ask_user_headless_modes_are_explicit_and_background_calls_are_rejected() {
        let tool = AskUserTool::new(None, HeadlessQuestions::Error);
        assert!(
            tool.call(args())
                .await
                .unwrap_err()
                .to_string()
                .contains("main user turn")
        );
        let turn = QuestionTurn::default();
        crate::question::scope(Some(turn.clone()), async {
            assert!(
                tool.call(args())
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("headless mode")
            );
            assert!(turn.required().is_none());
            assert!(
                tool.call(args())
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("headless mode")
            );
        })
        .await;
        crate::question::scope(Some(QuestionTurn::default()), async {
            let tool = AskUserTool::new(None, HeadlessQuestions::Fail);
            tool.call(args()).await.unwrap();
            assert!(QuestionTurn::current().unwrap().required().is_some());
        })
        .await;
    }

    #[tokio::test]
    async fn ask_user_typed_answers_cannot_be_permission_decisions() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let tool = AskUserTool::new(Some(sender), HeadlessQuestions::Error);
        crate::question::scope(Some(QuestionTurn::default()), async {
            let (result, ()) = tokio::join!(tool.call(args()), async {
                let UserRequest::Question(request) = receiver.recv().await.unwrap() else {
                    panic!("question must not use approval payload")
                };
                request
                    .reply
                    .send(QuestionAnswer::Answered(vec![
                        crate::question::Answer::Text("allow_always /private/**".into()),
                    ]))
                    .unwrap();
            });
            let text = result.unwrap();
            let json = text.lines().next().unwrap();
            let value: serde_json::Value = serde_json::from_str(json).unwrap();
            assert_eq!(value[0]["answer"], "allow_always /private/**");
        })
        .await;
    }

    #[tokio::test]
    async fn ask_user_cancelled_closed_and_malformed_replies_never_answer_or_park() {
        for scenario in ["cancelled", "closed", "malformed"] {
            let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
            let tool = AskUserTool::new(Some(sender), HeadlessQuestions::Fail);
            let turn = QuestionTurn::default();
            crate::question::scope(Some(turn.clone()), async {
                let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    tokio::join!(tool.call(args()), async {
                        let UserRequest::Question(request) = receiver.recv().await.unwrap() else {
                            panic!("clarification must use its typed transport")
                        };
                        assert_eq!(request.id, turn.id());
                        match scenario {
                            "cancelled" => UserRequest::Question(request).cancel(),
                            "closed" => drop(request),
                            _ => {
                                request
                                    .reply
                                    .send(QuestionAnswer::Answered(vec![]))
                                    .unwrap();
                            }
                        }
                    })
                })
                .await
                .unwrap();
                assert!(result.is_err());
                assert!(turn.required().is_none());
                // A reply dropped in transport never reached the user and is
                // refunded; a cancelled or malformed one used its slot.
                let expected = if scenario == "closed" {
                    crate::question::MAX_QUESTION_BATCHES_PER_TURN
                } else {
                    crate::question::MAX_QUESTION_BATCHES_PER_TURN - 1
                };
                assert_eq!(turn.remaining(), expected, "{scenario}");
            })
            .await;
        }
    }

    #[tokio::test]
    async fn ask_user_allows_a_budget_of_sequential_answered_calls() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let tool = AskUserTool::new(Some(sender), HeadlessQuestions::Error);
        crate::question::scope(Some(QuestionTurn::default()), async {
            for round in 0..crate::question::MAX_QUESTION_BATCHES_PER_TURN {
                let (result, ()) = tokio::join!(tool.call(args()), async {
                    let UserRequest::Question(request) = receiver.recv().await.unwrap() else {
                        panic!("expected a question")
                    };
                    request
                        .reply
                        .send(QuestionAnswer::Answered(vec![
                            crate::question::Answer::Text("yes".into()),
                        ]))
                        .unwrap();
                });
                let text = result.unwrap();
                let left = crate::question::MAX_QUESTION_BATCHES_PER_TURN - round - 1;
                assert!(
                    text.contains(&format!("[{left} clarifying question batch(es) left")),
                    "{text}"
                );
            }
            let error = tool.call(args()).await.unwrap_err().to_string();
            assert!(error.contains("Ask limit reached"), "{error}");
        })
        .await;
    }

    #[tokio::test]
    async fn ask_user_acp_new_message_preserves_validated_partial_answers_only() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let tool = AskUserTool::new(Some(sender), HeadlessQuestions::Error);
        let turn = QuestionTurn::default();
        let arguments = serde_json::from_value(serde_json::json!({"questions":[
            {"question":"Which branch?","options":[{"label":"main"}]},
            {"question":"What should change?"}
        ]}))
        .unwrap();
        crate::question::scope(Some(turn.clone()), async {
            let (result, ()) = tokio::join!(tool.call(arguments), async {
                let UserRequest::Question(request) = receiver.recv().await.unwrap() else {
                    panic!("expected a question")
                };
                request
                    .reply
                    .send(QuestionAnswer::AwaitingUser(vec![
                        crate::question::Answer::Option(0),
                    ]))
                    .unwrap();
            });
            assert!(result.is_ok());
            let pending = turn.required().unwrap();
            assert_eq!(pending.id, turn.id());
            assert_eq!(pending.questions.0.len(), 2);
            assert_eq!(pending.answers, vec![crate::question::Answer::Option(0)]);
            let stored = serde_json::to_value(&pending).unwrap();
            let restored: crate::question::QuestionRequired =
                serde_json::from_value(stored).unwrap();
            assert_eq!(restored.answers, pending.answers);
        })
        .await;
    }

    #[tokio::test]
    async fn ask_user_sdk_park_skips_later_effects_without_erasing_completed_batch_results() {
        use futures::StreamExt;
        use rig::agent::{AgentBuilder, MultiTurnStreamItem};
        use rig::streaming::{StreamedUserContent, StreamingChat};
        use rig::test_utils::{MockCompletionModel, MockStreamEvent};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        struct Effect(Arc<AtomicUsize>);
        impl Tool for Effect {
            const NAME: &'static str = "question_effect_probe";
            type Error = ToolError;
            type Args = serde_json::Value;
            type Output = String;
            fn description(&self) -> String {
                "Count an actual dispatched effect".into()
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({"type":"object","properties":{"stage":{"type":"string"}},"required":["stage"]})
            }
            async fn call(&self, args: Self::Args) -> Result<String, Self::Error> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(args["stage"].as_str().unwrap().to_string())
            }
        }
        struct Responses(Arc<AtomicUsize>);
        impl<M: rig::completion::CompletionModel> rig::agent::AgentHook<M> for Responses {
            async fn on_event(
                &self,
                _context: &rig::agent::HookContext,
                event: rig::agent::StepEvent<'_, M>,
            ) -> rig::agent::Flow {
                if matches!(event, rig::agent::StepEvent::ModelTurnFinished { .. }) {
                    self.0.fetch_add(1, Ordering::SeqCst);
                }
                rig::agent::Flow::Continue
            }
            fn observes(&self, kind: rig::agent::StepEventKind) -> bool {
                kind == rig::agent::StepEventKind::ModelTurnFinished
            }
        }
        let effects = Arc::new(AtomicUsize::new(0));
        let responses = Arc::new(AtomicUsize::new(0));
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call(
                    "before-question",
                    Effect::NAME,
                    serde_json::json!({"stage":"completed-before-question"}),
                ),
                MockStreamEvent::tool_call(
                    "question",
                    AskUserTool::NAME,
                    serde_json::json!({"question":"Which branch?"}),
                ),
                MockStreamEvent::tool_call(
                    "after-question",
                    Effect::NAME,
                    serde_json::json!({"stage":"must-not-run"}),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("must-not-request"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let tools = crate::agent::tools::concurrency::bind(vec![
            Box::new(Effect(effects.clone())),
            Box::new(AskUserTool::new(None, HeadlessQuestions::Fail)),
        ]);
        let agent = AgentBuilder::new(model)
            .tools(tools)
            .default_max_turns(3)
            .add_hook(Responses(responses.clone()))
            .add_hook(crate::question::QuestionGuard)
            .build();
        let turn = QuestionTurn::default();
        crate::question::scope(Some(turn.clone()), async {
            let mut stream = agent
                .stream_chat(
                    "ask after one effect",
                    Vec::<rig::completion::Message>::new(),
                )
                .tool_concurrency(1)
                .tool_extensions(crate::question::extend_current(
                    rig::tool::ToolCallExtensions::new(),
                ))
                .await;
            let mut delivered = Vec::new();
            let mut stopped = false;
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(MultiTurnStreamItem::StreamUserItem(
                            StreamedUserContent::ToolResult { tool_result, .. },
                        )) => {
                            delivered.push(serde_json::to_value(tool_result).unwrap());
                        }
                        Err(_) => stopped = true,
                        _ => {}
                    }
                }
            })
            .await
            .unwrap();
            assert!(
                stopped,
                "a parked turn cannot request the next model response"
            );
            assert_eq!(effects.load(Ordering::SeqCst), 1);
            assert_eq!(responses.load(Ordering::SeqCst), 1);
            assert_eq!(
                delivered.len(),
                3,
                "skip preserves completed and refused tool results in the atomic batch"
            );
            let serialized = serde_json::to_string(&delivered).unwrap();
            assert!(serialized.contains("completed-before-question"));
            assert!(serialized.contains("Which branch?"));
            assert!(!serialized.contains("must-not-run"));
            assert!(turn.required().is_some());
        })
        .await;
    }

    #[test]
    fn ask_user_argument_variants_validate_before_claiming_a_turn() {
        for value in [
            serde_json::json!({"question":"a","questions":[{"question":"b"}]}),
            serde_json::json!({"questions":[{"question":"b"}],"allow_free_text":true}),
            serde_json::json!({"question":" "}),
            serde_json::json!({"questions":[]}),
        ] {
            assert!(
                serde_json::from_value::<AskUserArgs>(value)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        assert!(
            serde_json::from_value::<AskUserArgs>(
                serde_json::json!({"question":"a","grant":"allow"})
            )
            .is_err()
        );
        let packet = serde_json::from_value::<AskUserArgs>(
            serde_json::json!({"questions":[{"question":"a"},{"question":"b"}]}),
        )
        .unwrap()
        .validate()
        .unwrap();
        assert_eq!(packet.0.len(), 2);
    }
}
