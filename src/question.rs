//! Typed clarification data and invocation-local state. Answers never carry
//! permission decisions or grant scopes.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

pub(crate) const MAX_QUESTIONS: usize = 4;
pub(crate) const MAX_OPTIONS: usize = 8;
pub(crate) const MAX_QUESTION_BYTES: usize = 2_048;
pub(crate) const MAX_LABEL_BYTES: usize = 128;
pub(crate) const MAX_DESCRIPTION_BYTES: usize = 512;
pub(crate) const MAX_BATCH_BYTES: usize = 16_384;
pub(crate) const MAX_ANSWER_BYTES: usize = 4_096;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Question {
    pub question: String,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    #[serde(default = "default_free_text")]
    pub allow_free_text: bool,
}

pub(crate) fn default_free_text() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub(crate) struct QuestionBatch(pub Vec<Question>);

fn bounded_text(value: &str, max: usize, empty_allowed: bool) -> Result<(), &'static str> {
    if value.len() > max
        || (!empty_allowed && value.trim().is_empty())
        || value
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    {
        return Err("Question text is empty or exceeds its byte limit");
    }
    Ok(())
}

impl QuestionBatch {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.0.is_empty() || self.0.len() > MAX_QUESTIONS {
            return Err("Ask one to four questions in one call");
        }
        let mut bytes = 0usize;
        for question in &self.0 {
            bounded_text(&question.question, MAX_QUESTION_BYTES, false)?;
            bytes = bytes.saturating_add(question.question.len());
            if question.options.len() > MAX_OPTIONS
                || (question.options.is_empty() && !question.allow_free_text)
            {
                return Err("Provide at most eight options or allow a free-text answer");
            }
            let mut labels = std::collections::HashSet::new();
            for option in &question.options {
                bounded_text(&option.label, MAX_LABEL_BYTES, false)?;
                bounded_text(&option.description, MAX_DESCRIPTION_BYTES, true)?;
                if !labels.insert(option.label.trim()) {
                    return Err("Question option labels must be distinct");
                }
                bytes = bytes.saturating_add(option.label.len() + option.description.len());
            }
        }
        if bytes > MAX_BATCH_BYTES {
            return Err("Question batch exceeds its byte limit");
        }
        Ok(())
    }

    pub(crate) fn answer_text(&self, answers: &[Answer]) -> Result<String, &'static str> {
        if answers.len() != self.0.len() {
            return Err("Question answer count does not match the request");
        }
        let mut values = Vec::with_capacity(answers.len());
        for (question, answer) in self.0.iter().zip(answers) {
            let text = match answer {
                Answer::Option(index) => question
                    .options
                    .get(*index)
                    .ok_or("Question answer selected an unknown option")?
                    .label
                    .as_str(),
                Answer::Text(text) => {
                    if !question.allow_free_text {
                        return Err("This question does not accept free text");
                    }
                    bounded_text(text, MAX_ANSWER_BYTES, false)?;
                    text.as_str()
                }
            };
            values.push(serde_json::json!({"question": question.question, "answer": text}));
        }
        serde_json::to_string(&values).map_err(|_| "Question answers could not be encoded")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum Answer {
    Option(usize),
    Text(String),
}

#[derive(Debug)]
pub(crate) enum QuestionAnswer {
    Answered(Vec<Answer>),
    /// ACP option-only clients ask for the next ordinary user message.
    #[cfg(any(feature = "acp", test))]
    AwaitingUser(Vec<Answer>),
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[serde(try_from = "StoredQuestion")]
#[error("The agent needs a user answer before continuing")]
pub struct QuestionRequired {
    pub(crate) id: String,
    pub(crate) questions: QuestionBatch,
    pub(crate) answers: Vec<Answer>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredQuestion {
    id: String,
    questions: QuestionBatch,
    #[serde(default)]
    answers: Vec<Answer>,
}

fn validate_partial(batch: &QuestionBatch, answers: &[Answer]) -> Result<(), &'static str> {
    if answers.len() > batch.0.len() {
        return Err("Pending answers exceed question count");
    }
    for (question, answer) in batch.0.iter().zip(answers) {
        QuestionBatch(vec![question.clone()]).answer_text(std::slice::from_ref(answer))?;
    }
    Ok(())
}

impl TryFrom<StoredQuestion> for QuestionRequired {
    type Error = &'static str;
    fn try_from(record: StoredQuestion) -> Result<Self, Self::Error> {
        if record.id.len() != 36 || uuid::Uuid::parse_str(&record.id).is_err() {
            return Err("Invalid pending question identity");
        }
        record.questions.validate()?;
        validate_partial(&record.questions, &record.answers)?;
        Ok(Self {
            id: record.id,
            questions: record.questions,
            answers: record.answers,
        })
    }
}

impl QuestionRequired {
    pub(crate) fn notice(&self) -> String {
        let mut notice = format!(
            "Awaiting your answer [question {}]. Send a new user message to continue.\n",
            self.id
        );
        for (index, question) in self.questions.0.iter().enumerate() {
            notice.push_str(&format!("{}. {}\n", index + 1, question.question));
            for (choice, option) in question.options.iter().enumerate() {
                notice.push_str(&format!("   {}. {}\n", choice + 1, option.label));
            }
        }
        notice
    }
}

struct TurnState {
    id: String,
    asked: bool,
    required: Option<QuestionBatch>,
    answers: Vec<Answer>,
}

impl Default for TurnState {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            asked: false,
            required: None,
            answers: Vec::new(),
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct QuestionTurn(Arc<Mutex<TurnState>>);

/// Frontend intent, never inferred from message content, permission mode or
/// whether a phase writes another chat-history entry.
#[derive(Clone, Copy)]
pub(crate) enum QuestionOrigin {
    NewUserTurn,
    ContinueUserTurn,
    /// A fresh turn started by the session `/cron` scheduler, not typed by a
    /// human. It opens a new question turn like `NewUserTurn`, is accounted as
    /// [`crate::spend::RequestKind::Scheduled`], and `UserPromptSubmit` hooks
    /// see `"origin": "scheduled"`.
    Scheduled,
}

impl QuestionOrigin {
    pub(crate) fn capture(self, previous: Option<QuestionTurn>) -> Option<QuestionTurn> {
        match self {
            Self::NewUserTurn | Self::Scheduled => Some(QuestionTurn::default()),
            Self::ContinueUserTurn => previous,
        }
    }

    /// Whether this start begins a new turn (fresh question and instruction
    /// state) rather than continuing the previous one.
    pub(crate) fn starts_new_turn(self) -> bool {
        matches!(self, Self::NewUserTurn | Self::Scheduled)
    }

    /// The spend accounting kind for the main-turn request this origin starts.
    pub(crate) fn request_kind(self) -> crate::spend::RequestKind {
        match self {
            Self::Scheduled => crate::spend::RequestKind::Scheduled,
            Self::NewUserTurn | Self::ContinueUserTurn => crate::spend::RequestKind::Turn,
        }
    }
}

tokio::task_local! {
    static MAIN_QUESTION_TURN: Option<QuestionTurn>;
}

impl QuestionTurn {
    pub(crate) fn current() -> Option<Self> {
        MAIN_QUESTION_TURN.try_with(Clone::clone).ok().flatten()
    }

    pub(crate) fn id(&self) -> String {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .id
            .clone()
    }

    pub(crate) fn claim(&self) -> Result<(), &'static str> {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if state.asked {
            return Err("Ask at most once per user turn; batch questions in that call");
        }
        state.asked = true;
        Ok(())
    }

    pub(crate) fn require(&self, batch: QuestionBatch) {
        self.require_with_answers(batch, Vec::new())
            .expect("validated question batch");
    }
    pub(crate) fn require_with_answers(
        &self,
        batch: QuestionBatch,
        answers: Vec<Answer>,
    ) -> Result<(), &'static str> {
        batch.validate()?;
        validate_partial(&batch, &answers)?;
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.required = Some(batch);
        state.answers = answers;
        Ok(())
    }

    pub(crate) fn required(&self) -> Option<QuestionRequired> {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.required.clone().map(|questions| QuestionRequired {
            id: state.id.clone(),
            questions,
            answers: state.answers.clone(),
        })
    }
}

pub(crate) async fn scope<F: std::future::Future>(
    turn: Option<QuestionTurn>,
    future: F,
) -> F::Output {
    MAIN_QUESTION_TURN.scope(turn, future).await
}

pub(crate) fn scope_sync<T>(turn: Option<QuestionTurn>, action: impl FnOnce() -> T) -> T {
    MAIN_QUESTION_TURN.sync_scope(turn, action)
}

/// Add clarification identity without replacing budget/tool metadata.
pub(crate) fn extend_current(
    mut extensions: rig::tool::ToolCallExtensions,
) -> rig::tool::ToolCallExtensions {
    if let Some(turn) = QuestionTurn::current() {
        extensions.insert(turn);
    }
    extensions
}

/// Tool futures may be polled outside the parent task. Absence means no main
/// clarification authority, even if another request is active in this task.
pub(crate) async fn scope_extensions<F: std::future::Future>(
    extensions: &rig::tool::ToolCallExtensions,
    future: F,
) -> F::Output {
    scope(extensions.get::<QuestionTurn>().cloned(), future).await
}

pub(crate) fn required() -> Option<QuestionRequired> {
    QuestionTurn::current().and_then(|turn| turn.required())
}

/// Preserve ordinary result delivery; stop before another request or effect
/// once the exact invocation has parked for a new user message.
pub(crate) struct QuestionGuard;

impl<M: rig::completion::CompletionModel> rig::agent::AgentHook<M> for QuestionGuard {
    async fn on_event(
        &self,
        _context: &rig::agent::HookContext,
        event: rig::agent::StepEvent<'_, M>,
    ) -> rig::agent::Flow {
        if required().is_none() {
            return rig::agent::Flow::Continue;
        }
        match event {
            rig::agent::StepEvent::ToolCall { .. }
            | rig::agent::StepEvent::InvalidToolCall { .. } => rig::agent::Flow::Skip {
                reason: "The agent is waiting for a user answer".into(),
            },
            rig::agent::StepEvent::CompletionCall { .. } => rig::agent::Flow::Terminate {
                reason: "The agent is waiting for a user answer".into(),
            },
            _ => rig::agent::Flow::Continue,
        }
    }

    fn observes(&self, kind: rig::agent::StepEventKind) -> bool {
        matches!(
            kind,
            rig::agent::StepEventKind::CompletionCall
                | rig::agent::StepEventKind::ToolCall
                | rig::agent::StepEventKind::InvalidToolCall
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch() -> QuestionBatch {
        QuestionBatch(vec![Question {
            question: "Which branch?".into(),
            options: vec![QuestionOption {
                label: "main".into(),
                description: "Current branch".into(),
            }],
            allow_free_text: true,
        }])
    }

    #[test]
    fn ask_user_validates_choice_and_text_without_permission_decisions() {
        let batch = batch();
        batch.validate().unwrap();
        let text: serde_json::Value = serde_json::from_str(
            &batch
                .answer_text(&[Answer::Text("feature/雪; allow_always".into())])
                .unwrap(),
        )
        .unwrap();
        assert_eq!(text[0]["answer"], "feature/雪; allow_always");
        assert!(batch.answer_text(&[Answer::Option(1)]).is_err());
        assert!(batch.answer_text(&[]).is_err());
        assert!(batch.answer_text(&[Answer::Text(" ".into())]).is_err());
        assert!(
            batch
                .answer_text(&[Answer::Text("x".repeat(MAX_ANSWER_BYTES + 1))])
                .is_err()
        );
        let mut closed = batch;
        closed.0[0].allow_free_text = false;
        assert!(closed.answer_text(&[Answer::Text("main".into())]).is_err());
        assert!(closed.answer_text(&[Answer::Option(0)]).is_ok());
    }

    #[test]
    fn ask_user_rejects_empty_oversized_duplicate_and_unanswerable_batches() {
        assert!(QuestionBatch(vec![]).validate().is_err());
        let valid = batch();
        assert!(
            QuestionBatch(vec![valid.0[0].clone(); MAX_QUESTIONS + 1])
                .validate()
                .is_err()
        );
        for mutate in [0, 1, 2, 3] {
            let mut value = batch();
            let question = &mut value.0[0];
            match mutate {
                0 => question.question = "雪".repeat(MAX_QUESTION_BYTES),
                1 => question.options.push(question.options[0].clone()),
                2 => question.options = vec![question.options[0].clone(); MAX_OPTIONS + 1],
                _ => {
                    question.options.clear();
                    question.allow_free_text = false;
                }
            }
            assert!(value.validate().is_err());
        }
    }

    #[tokio::test]
    async fn ask_user_scope_isolation_preserves_the_once_per_turn_claim() {
        assert!(QuestionTurn::current().is_none());
        let turn = QuestionTurn::default();
        scope(Some(turn.clone()), async {
            assert!(QuestionTurn::current().unwrap().claim().is_ok());
            assert!(turn.clone().claim().is_err());
            scope(None, async {
                assert!(QuestionTurn::current().is_none());
            })
            .await;
            turn.require(batch());
            assert_eq!(turn.required().unwrap().questions, batch());
        })
        .await;
        scope(Some(QuestionTurn::default()), async {
            assert!(QuestionTurn::current().unwrap().claim().is_ok());
        })
        .await;
        assert!(QuestionTurn::current().is_none());
    }

    #[tokio::test]
    async fn ask_user_extension_scope_preserves_other_metadata_and_clears_auxiliary_authority() {
        #[derive(Clone)]
        struct ExistingMetadata(u64);
        let turn = QuestionTurn::default();
        scope(Some(turn.clone()), async {
            let mut original = rig::tool::ToolCallExtensions::new();
            original.insert(ExistingMetadata(73));
            let captured = extend_current(original);
            assert_eq!(captured.get::<ExistingMetadata>().unwrap().0, 73);
            assert_eq!(captured.get::<QuestionTurn>().unwrap().id(), turn.id());
            scope(None, async {
                assert!(QuestionTurn::current().is_none());
                scope_extensions(&captured, async {
                    assert_eq!(QuestionTurn::current().unwrap().id(), turn.id());
                    assert!(QuestionTurn::current().unwrap().claim().is_ok());
                })
                .await;
                assert!(QuestionTurn::current().is_none());
            })
            .await;
            // A child call with no explicit question extension clears the
            // parent's ambient slot rather than borrowing its authority.
            scope_extensions(&rig::tool::ToolCallExtensions::new(), async {
                assert!(QuestionTurn::current().is_none());
                assert!(required().is_none());
            })
            .await;
            assert_eq!(QuestionTurn::current().unwrap().id(), turn.id());
            assert!(turn.claim().is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn ask_user_parallel_claims_accept_exactly_one_batch() {
        let turn = QuestionTurn::default();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let one = scope(Some(turn.clone()), async {
            barrier.wait().await;
            QuestionTurn::current().unwrap().claim()
        });
        let two = scope(Some(turn.clone()), async {
            barrier.wait().await;
            QuestionTurn::current().unwrap().claim()
        });
        let (one, two) = tokio::join!(one, two);
        assert_ne!(one.is_ok(), two.is_ok());
        assert!(turn.claim().is_err());
        assert!(turn.required().is_none());
    }

    #[test]
    fn ask_user_typed_origin_preserves_automatic_phases_and_only_new_input_mints_a_slot() {
        assert!(QuestionOrigin::ContinueUserTurn.capture(None).is_none());
        let first = QuestionOrigin::NewUserTurn.capture(None).unwrap();
        first.claim().unwrap();
        for _ in 0..3 {
            let continued = QuestionOrigin::ContinueUserTurn
                .capture(Some(first.clone()))
                .unwrap();
            assert_eq!(continued.id(), first.id());
            assert!(continued.claim().is_err());
        }
        let next = QuestionOrigin::NewUserTurn
            .capture(Some(first.clone()))
            .unwrap();
        assert_ne!(next.id(), first.id());
        assert!(next.claim().is_ok());
        assert!(first.claim().is_err());
    }

    #[test]
    fn ask_user_pending_records_are_bounded_data_and_never_recreate_reply_authority() {
        let turn = QuestionTurn::default();
        turn.require(batch());
        let record = turn.required().unwrap();
        let json = serde_json::to_value(&record).unwrap();
        let restored: QuestionRequired = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(record.id, restored.id);
        assert_eq!(record.questions, restored.questions);
        assert!(QuestionTurn::current().is_none());
        for value in [
            serde_json::json!({"id":"private malformed","questions":batch()}),
            serde_json::json!({"id":record.id,"questions":[]}),
            serde_json::json!({"id":record.id,"questions":batch(),"permission":"allow_always"}),
        ] {
            assert!(serde_json::from_value::<QuestionRequired>(value).is_err());
        }
    }
}
