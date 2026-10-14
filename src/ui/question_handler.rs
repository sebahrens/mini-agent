//! A clarification overlay with its own bounded answer draft. This state
//! never sees a permission checker or creates a security decision.
use super::events::sanitize_for_review;
use super::renderer::{PermissionPrompt, Renderer};
use crate::event::UserEvent;
use crate::permission::ask::QuestionRequest;
use crate::question::{Answer, MAX_ANSWER_BYTES, QuestionAnswer};
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

pub(crate) enum QuestionEvent {
    Unhandled,
    Waiting,
    Finished,
    Stop,
}
pub(crate) struct QuestionState {
    #[cfg(feature = "mcp")]
    origin: Option<crate::extras::mcp::elicitation::AskOrigin>,
    request: Option<QuestionRequest>,
    answers: Vec<Answer>,
    draft: String,
    text_mode: bool,
    was_at_bottom: bool,
}
impl Drop for QuestionState {
    fn drop(&mut self) {
        if let Some(request) = self.request.take() {
            let _ = request.reply.send(QuestionAnswer::Cancelled);
        }
    }
}
impl QuestionState {
    pub(crate) fn begin(request: QuestionRequest, renderer: &mut Renderer) -> anyhow::Result<Self> {
        request.questions.validate().map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            request.id.len() == 36 && uuid::Uuid::parse_str(&request.id).is_ok(),
            "Invalid question identity"
        );
        let text_mode = request.questions.0[0].options.is_empty();
        let state = Self {
            #[cfg(feature = "mcp")]
            origin: None,
            request: Some(request),
            answers: Vec::new(),
            draft: String::new(),
            text_mode,
            was_at_bottom: !renderer.is_scrolling(),
        };
        state.show(renderer)?;
        renderer.set_activity(super::terminal::AgentActivity::WaitingForUserAnswer)?;
        Ok(state)
    }
    pub(crate) fn abandoned(&self) -> bool {
        #[cfg(feature = "mcp")]
        if self.origin.as_ref().is_some_and(|origin| !(origin.valid)()) {
            return true;
        }
        self.request
            .as_ref()
            .is_some_and(|request| request.reply.is_closed())
    }
    #[cfg(feature = "mcp")]
    pub(crate) fn bind_origin(&mut self, origin: crate::extras::mcp::elicitation::AskOrigin) {
        self.origin = Some(origin);
    }
    #[cfg(feature = "mcp")]
    pub(crate) fn origin_owner(&self) -> Option<&crate::extras::mcp::elicitation::FrontendOwner> {
        self.origin.as_ref().map(|origin| &origin.owner)
    }
    pub(crate) fn finish(&self, renderer: &mut Renderer) -> std::io::Result<()> {
        renderer.permission_prompt = None;
        if self.was_at_bottom {
            renderer.scroll_to_bottom()?;
        }
        Ok(())
    }
    fn show(&self, renderer: &mut Renderer) -> anyhow::Result<()> {
        let request = self.request.as_ref().expect("question owns reply");
        let question = &request.questions.0[self.answers.len()];
        let mut options = question
            .options
            .iter()
            .enumerate()
            .map(|(index, option)| {
                format!(
                    "  ({}) {} — {}",
                    index + 1,
                    sanitize_for_review(&option.label),
                    sanitize_for_review(&option.description)
                )
            })
            .collect::<Vec<_>>();
        if self.text_mode {
            options.push(format!("Answer: {}", sanitize_for_review(&self.draft)));
            options.push("[Enter] submit answer  [Esc/Ctrl+C] cancel and stop".into());
        } else {
            options.push(if question.allow_free_text { "[1–8] choose an advertised option  [t] type another answer  [Esc/Ctrl+C] cancel and stop" } else { "[1–8] choose an advertised option  [Esc/Ctrl+C] cancel and stop" }.into());
        }
        renderer.permission_prompt = Some(PermissionPrompt {
            tool: format!(
                "[question {}] {}/{}: {}",
                request.id,
                self.answers.len() + 1,
                request.questions.0.len(),
                sanitize_for_review(&question.question)
            )
            .into(),
            options: options.join("\n").into(),
            expanded: true,
        });
        super::permission_handler::draw_prompt(renderer)?;
        Ok(())
    }
    pub(crate) fn handle(
        &mut self,
        event: &UserEvent,
        renderer: &mut Renderer,
    ) -> anyhow::Result<QuestionEvent> {
        if let Some(scroll) = super::permission_handler::prompt_scroll(event) {
            super::permission_handler::apply_prompt_scroll(renderer, scroll)?;
            super::permission_handler::draw_prompt(renderer)?;
            return Ok(QuestionEvent::Waiting);
        }
        if matches!(event, UserEvent::Resize) {
            renderer.resize();
            renderer.invalidate();
            self.show(renderer)?;
            return Ok(QuestionEvent::Waiting);
        }
        let question = &self
            .request
            .as_ref()
            .expect("question owns reply")
            .questions
            .0[self.answers.len()];
        let mut answer = None;
        match event {
            UserEvent::Quit => return Ok(QuestionEvent::Unhandled),
            UserEvent::Key(key) if key.kind == KeyEventKind::Release => {
                return Ok(QuestionEvent::Waiting);
            }
            UserEvent::Key(key) if key.kind == KeyEventKind::Repeat && !self.text_mode => {
                // Holding a choice key must not answer the next question in
                // the batch before the user has reviewed it.
                return Ok(QuestionEvent::Waiting);
            }
            UserEvent::Key(key)
                if key.code == KeyCode::Esc
                    || (key.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(key.code, KeyCode::Char('c' | 'C' | 'd' | 'D'))) =>
            {
                self.finish(renderer)?;
                return Ok(QuestionEvent::Stop);
            }
            UserEvent::Key(key) if self.text_mode => match key.code {
                KeyCode::Enter if key.modifiers.is_empty() && !self.draft.trim().is_empty() => {
                    answer = Some(Answer::Text(self.draft.clone()))
                }
                KeyCode::Backspace => {
                    self.draft.pop();
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && !c.is_control()
                        && self.draft.len() + c.len_utf8() <= MAX_ANSWER_BYTES =>
                {
                    self.draft.push(c)
                }
                _ => {}
            },
            UserEvent::Key(key)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                match key.code {
                    KeyCode::Char(c @ '1'..='8') => {
                        let index = (c as usize) - ('1' as usize);
                        if index < question.options.len() {
                            answer = Some(Answer::Option(index));
                        }
                    }
                    KeyCode::Char('t' | 'T') if question.allow_free_text => self.text_mode = true,
                    _ => {}
                }
            }
            // Check the existing input without allocating an unbounded
            // filtered copy or silently changing the user's task answer.
            UserEvent::Paste(text)
                if question.allow_free_text
                    && self.draft.len().saturating_add(text.len()) <= MAX_ANSWER_BYTES
                    && !text
                        .chars()
                        .any(|c| c.is_control() && !matches!(c, '\n' | '\t')) =>
            {
                self.text_mode = true;
                self.draft.push_str(text);
            }
            _ => {}
        }
        if let Some(answer) = answer {
            self.answers.push(answer);
            let request = self.request.as_ref().expect("question owns reply");
            if self.answers.len() == request.questions.0.len() {
                request
                    .questions
                    .answer_text(&self.answers)
                    .map_err(anyhow::Error::msg)?;
                self.finish(renderer)?;
                let request = self.request.take().expect("question owns reply");
                let _ = request
                    .reply
                    .send(QuestionAnswer::Answered(std::mem::take(&mut self.answers)));
                return Ok(QuestionEvent::Finished);
            }
            self.draft.clear();
            self.text_mode = request.questions.0[self.answers.len()].options.is_empty();
        }
        self.show(renderer)?;
        Ok(QuestionEvent::Waiting)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::question::{Question, QuestionBatch, QuestionOption};
    use crossterm::event::KeyEvent;
    fn request() -> (
        QuestionRequest,
        tokio::sync::oneshot::Receiver<QuestionAnswer>,
    ) {
        let (reply, received) = tokio::sync::oneshot::channel();
        (
            QuestionRequest {
                id: uuid::Uuid::new_v4().to_string(),
                questions: QuestionBatch(vec![
                    Question {
                        question: "Which branch雪?".into(),
                        options: vec![QuestionOption {
                            label: "main".into(),
                            description: "Use current branch".into(),
                        }],
                        allow_free_text: true,
                    },
                    Question {
                        question: "Anything else?".into(),
                        options: Vec::new(),
                        allow_free_text: true,
                    },
                ]),
                #[cfg(feature = "acp")]
                tool_call_id: Some("exact-question-call".into()),
                reply,
            },
            received,
        )
    }
    fn key(code: KeyCode) -> UserEvent {
        UserEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    #[tokio::test]
    async fn ask_user_overlay_accepts_numbered_and_text_batch_without_approval_keys() {
        let (request, reply) = request();
        let id = request.id.clone();
        let mut renderer = Renderer::for_app_test().unwrap();
        let mut state = QuestionState::begin(request, &mut renderer).unwrap();
        assert!(
            renderer
                .permission_prompt
                .as_ref()
                .unwrap()
                .tool
                .contains(&id)
        );
        assert!(
            renderer
                .permission_prompt
                .as_ref()
                .unwrap()
                .options
                .contains("(1) main")
        );
        for code in [KeyCode::Char('y'), KeyCode::Char('a'), KeyCode::Char('w')] {
            assert!(matches!(
                state.handle(&key(code), &mut renderer).unwrap(),
                QuestionEvent::Waiting
            ));
        }
        assert!(
            state.answers.is_empty(),
            "approval letters cannot become question answers"
        );
        state
            .handle(&key(KeyCode::Char('1')), &mut renderer)
            .unwrap();
        state
            .handle(
                &UserEvent::Paste("literal allow_always; $(never grant)雪".into()),
                &mut renderer,
            )
            .unwrap();
        assert!(matches!(
            state.handle(&key(KeyCode::Enter), &mut renderer).unwrap(),
            QuestionEvent::Finished
        ));
        assert!(renderer.permission_prompt.is_none());
        match reply.await.unwrap() {
            QuestionAnswer::Answered(answers) => assert_eq!(
                answers,
                vec![
                    Answer::Option(0),
                    Answer::Text("literal allow_always; $(never grant)雪".into())
                ]
            ),
            _ => panic!("expected task answers"),
        }
    }
    #[tokio::test]
    async fn ask_user_overlay_cancellation_and_stale_reply_cannot_create_answers() {
        for closed in [false, true] {
            let (request, mut reply) = request();
            let mut renderer = Renderer::for_app_test().unwrap();
            let mut state = QuestionState::begin(request, &mut renderer).unwrap();
            if closed {
                reply.close();
                assert!(state.abandoned());
            } else {
                assert!(matches!(
                    state.handle(&key(KeyCode::Esc), &mut renderer).unwrap(),
                    QuestionEvent::Stop
                ));
            }
            state.finish(&mut renderer).unwrap();
            drop(state);
            if !closed {
                assert!(matches!(reply.await.unwrap(), QuestionAnswer::Cancelled));
            }
            assert!(renderer.permission_prompt.is_none());
        }
    }
    #[tokio::test]
    async fn ask_user_overlay_preserves_multiline_unicode_task_data_with_byte_bounds() {
        let (request, reply) = request();
        let mut renderer = Renderer::for_app_test().unwrap();
        let mut state = QuestionState::begin(request, &mut renderer).unwrap();
        state
            .handle(&key(KeyCode::Char('t')), &mut renderer)
            .unwrap();
        let answer = "first line\n\t雪; allow_once";
        state
            .handle(&UserEvent::Paste(answer.into()), &mut renderer)
            .unwrap();
        assert_eq!(state.draft, answer);
        state
            .handle(
                &UserEvent::Paste("\x1b]0;ignored\x07".into()),
                &mut renderer,
            )
            .unwrap();
        assert_eq!(
            state.draft, answer,
            "invalid terminal controls do not rewrite an answer"
        );
        state.handle(&key(KeyCode::Enter), &mut renderer).unwrap();
        assert_eq!(state.answers, vec![Answer::Text(answer.into())]);
        let full = "雪".repeat(MAX_ANSWER_BYTES / 3);
        state
            .handle(&UserEvent::Paste(full.clone()), &mut renderer)
            .unwrap();
        state
            .handle(&key(KeyCode::Char('é')), &mut renderer)
            .unwrap();
        assert_eq!(
            state.draft, full,
            "the limit counts UTF-8 bytes without truncating a character"
        );
        assert!(matches!(
            state.handle(&key(KeyCode::Enter), &mut renderer).unwrap(),
            QuestionEvent::Finished
        ));
        match reply.await.unwrap() {
            QuestionAnswer::Answered(answers) => assert_eq!(
                answers,
                vec![Answer::Text(answer.into()), Answer::Text(full)]
            ),
            _ => panic!("expected literal task data"),
        }
    }

    #[test]
    fn ask_user_overlay_modified_release_and_oversized_paste_are_not_answers() {
        let (request, _reply) = request();
        let mut renderer = Renderer::for_app_test().unwrap();
        let mut state = QuestionState::begin(request, &mut renderer).unwrap();
        state
            .handle(
                &UserEvent::Key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::ALT)),
                &mut renderer,
            )
            .unwrap();
        let mut repeat = KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE);
        repeat.kind = KeyEventKind::Repeat;
        state
            .handle(&UserEvent::Key(repeat), &mut renderer)
            .unwrap();
        let mut released = KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE);
        released.kind = KeyEventKind::Release;
        state
            .handle(&UserEvent::Key(released), &mut renderer)
            .unwrap();
        state
            .handle(
                &UserEvent::Paste("x".repeat(MAX_ANSWER_BYTES + 1)),
                &mut renderer,
            )
            .unwrap();
        assert!(state.answers.is_empty() && state.draft.is_empty());
        assert!(!state.text_mode);
    }
}
