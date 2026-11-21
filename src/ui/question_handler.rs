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
    /// The free-text answer draft: a real line editor (cursor, word delete,
    /// newline, paste) like the main prompt, bounded to `MAX_ANSWER_BYTES`.
    editor: crate::ui::input::InputEditor,
    text_mode: bool,
    /// The highlighted option of the current choice question.
    selected: usize,
    /// Options ticked so far on a multi-select question.
    checked: std::collections::BTreeSet<usize>,
    was_at_bottom: bool,
    /// When the overlay opened or last discarded a key (quiet period).
    opened: std::time::Instant,
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
            editor: crate::ui::input::InputEditor::new(),
            text_mode,
            selected: 0,
            checked: Default::default(),
            was_at_bottom: !renderer.is_scrolling(),
            opened: std::time::Instant::now(),
        };
        state.show(renderer)?;
        renderer.set_activity(super::terminal::AgentActivity::WaitingForUserAnswer)?;
        Ok(state)
    }
    fn draft(&self) -> &str {
        &self.editor.buffer
    }
    fn clear_draft(&mut self) {
        self.editor.load_text("");
    }
    /// Reset the per-question selection state for the question now shown.
    fn begin_question(&mut self) {
        self.selected = 0;
        self.checked.clear();
        self.clear_draft();
        let request = self.request.as_ref().expect("question owns reply");
        self.text_mode = request.questions.0[self.answers.len()].options.is_empty();
    }
    /// Re-open the previous question with its earlier answer restored, before
    /// the reply is sent.
    fn go_back(&mut self) -> bool {
        let Some(previous) = self.answers.pop() else {
            return false;
        };
        self.begin_question();
        match previous {
            Answer::Option(index) => self.selected = index,
            Answer::Options(indices) => {
                self.selected = indices.first().copied().unwrap_or(0);
                self.checked = indices.into_iter().collect();
            }
            Answer::Text(text) => {
                self.text_mode = true;
                self.editor.load_text(&text);
            }
        }
        true
    }
    #[cfg(test)]
    pub(crate) fn abandoned(&self) -> bool {
        self.abandoned_with(true)
    }
    /// `revalidate == false` skips the bound origin's source revalidation and
    /// checks only the cheap reply-channel state; replies stay authoritative.
    pub(crate) fn abandoned_with(&self, revalidate: bool) -> bool {
        #[cfg(feature = "mcp")]
        if revalidate && self.origin.as_ref().is_some_and(|origin| !(origin.valid)()) {
            return true;
        }
        #[cfg(not(feature = "mcp"))]
        let _ = revalidate;
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
                let pointer = if !self.text_mode && index == self.selected {
                    "›"
                } else {
                    " "
                };
                let mark = if !question.multi_select {
                    String::new()
                } else if self.checked.contains(&index) {
                    "[x] ".into()
                } else {
                    "[ ] ".into()
                };
                format!(
                    "{pointer} ({}) {mark}{} — {}",
                    index + 1,
                    sanitize_for_review(&option.label),
                    sanitize_for_review(&option.description)
                )
            })
            .collect::<Vec<_>>();
        let can_go_back = !self.answers.is_empty();
        if self.text_mode {
            let mut draft = self.draft().to_string();
            draft.insert(self.editor.cursor.min(draft.len()), '▏');
            options.push(format!("Answer: {}", sanitize_for_review(&draft)));
            let back = if can_go_back && self.draft().is_empty() {
                "  [Backspace] previous question"
            } else {
                ""
            };
            options.push(
                if question.options.is_empty() {
                    format!("[Enter] submit answer{back}  [Esc/Ctrl+C] cancel and stop")
                } else {
                    format!("[Enter] submit answer  [Esc] back to the options{back}  [Ctrl+C] cancel and stop")
                },
            );
        } else {
            let mut hint = String::from(if question.multi_select {
                "[↑/↓] move  [Space] tick  [Enter] confirm  [1–8] tick"
            } else {
                "[↑/↓] move  [Enter] choose  [1–8] choose directly"
            });
            if question.allow_free_text {
                hint.push_str("  [t] type another answer");
            }
            if can_go_back {
                hint.push_str("  [←] previous question");
            }
            hint.push_str("  [Esc/Ctrl+C] cancel and stop");
            options.push(hint);
        }
        renderer.permission_prompt = Some(PermissionPrompt {
            tool: format!(
                "[question {}/{}] {}",
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
        // Plain Up/Down move the highlighted option (or the draft's cursor),
        // so only the page, wheel and jump keys scroll the overlay text.
        let moves_selection = matches!(
            event,
            UserEvent::Key(key)
                if matches!(key.code, KeyCode::Up | KeyCode::Down) && key.modifiers.is_empty()
        );
        if !moves_selection && let Some(scroll) = super::permission_handler::prompt_scroll(event) {
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
        if super::permission_handler::prompt_key_premature(
            &mut self.opened,
            event,
            std::time::Instant::now(),
            super::permission_handler::prompt_arming_delay(),
        ) {
            return Ok(QuestionEvent::Waiting);
        }
        let question = self
            .request
            .as_ref()
            .expect("question owns reply")
            .questions
            .0[self.answers.len()]
        .clone();
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
            // Esc in free-text mode of a choice question goes back to the
            // options (as it does in the permission prompt's note draft);
            // from the option list, or for a text-only question, it stops.
            UserEvent::Key(key)
                if key.code == KeyCode::Esc
                    && key.modifiers.is_empty()
                    && self.text_mode
                    && !question.options.is_empty() =>
            {
                self.text_mode = false;
                self.clear_draft();
            }
            UserEvent::Key(key)
                if key.code == KeyCode::Esc
                    || (key.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(key.code, KeyCode::Char('c' | 'C' | 'd' | 'D'))) =>
            {
                self.finish(renderer)?;
                return Ok(QuestionEvent::Stop);
            }
            UserEvent::Key(key)
                if self.text_mode
                    && key.code == KeyCode::Backspace
                    && key.modifiers.is_empty()
                    && self.draft().is_empty()
                    && !self.answers.is_empty() =>
            {
                self.go_back();
            }
            UserEvent::Key(key) if self.text_mode => {
                if key.code == KeyCode::Enter && self.draft().trim().is_empty() {
                    // Nothing to submit yet.
                } else {
                    let before = self.draft().to_string();
                    let cursor = self.editor.cursor;
                    let submitted = self.editor.handle_operator_draft_key(*key);
                    if let Some(text) = submitted {
                        if text.len() <= MAX_ANSWER_BYTES && !text.trim().is_empty() {
                            answer = Some(Answer::Text(text.to_string()));
                        }
                    } else if self.draft().len() > MAX_ANSWER_BYTES {
                        // Over the byte limit: keep the previous draft.
                        self.editor.load_text(&before);
                        self.editor.cursor = cursor.min(before.len());
                    }
                }
            }
            UserEvent::Key(key)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                let count = question.options.len();
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') if count > 0 => {
                        self.selected = (self.selected + count - 1) % count;
                    }
                    KeyCode::Down | KeyCode::Char('j') if count > 0 => {
                        self.selected = (self.selected + 1) % count;
                    }
                    KeyCode::Left | KeyCode::Backspace => {
                        self.go_back();
                    }
                    KeyCode::Enter if count > 0 => {
                        answer = Some(if question.multi_select {
                            let mut picked: Vec<usize> = self.checked.iter().copied().collect();
                            if picked.is_empty() {
                                picked.push(self.selected);
                            }
                            Answer::Options(picked)
                        } else {
                            Answer::Option(self.selected)
                        });
                    }
                    KeyCode::Char(' ')
                        if question.multi_select
                            && count > 0
                            && !self.checked.remove(&self.selected) =>
                    {
                        self.checked.insert(self.selected);
                    }
                    KeyCode::Char(c @ '1'..='8') => {
                        let index = (c as usize) - ('1' as usize);
                        if index < count {
                            self.selected = index;
                            if question.multi_select {
                                if !self.checked.remove(&index) {
                                    self.checked.insert(index);
                                }
                            } else {
                                answer = Some(Answer::Option(index));
                            }
                        }
                    }
                    KeyCode::Char('t' | 'T') if question.allow_free_text => self.text_mode = true,
                    _ => {}
                }
            }
            // Check the existing input without allocating an unbounded
            // filtered copy or silently changing the user's task answer.
            UserEvent::Paste(text) if question.allow_free_text => {
                // Terminals send pasted newlines as `\r` (iTerm2, VTE):
                // normalise like the editor does, and drop escapes.
                let text = crate::ui::input::sanitize_paste(text);
                if self.draft().len().saturating_add(text.len()) > MAX_ANSWER_BYTES {
                    renderer.show_notice(
                        &format!("answer limit is {MAX_ANSWER_BYTES} bytes; paste not added"),
                        crossterm::style::Color::Yellow,
                    );
                } else if !text.is_empty() {
                    self.text_mode = true;
                    let joined = format!("{}{}", self.draft(), text);
                    self.editor.load_text(&joined);
                }
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
            self.begin_question();
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
                        multi_select: false,
                    },
                    Question {
                        question: "Anything else?".into(),
                        options: Vec::new(),
                        allow_free_text: true,
                        multi_select: false,
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
        let header = &renderer.permission_prompt.as_ref().unwrap().tool;
        assert!(header.starts_with("[question 1/2]"), "{header}");
        assert!(!header.contains(&id), "the internal id is not shown");
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
        // CR-separated lines (iTerm2, VTE) arrive as `\n`; tabs are kept
        // (the overlay expands them only when drawing).
        let answer = "first line\n\t雪; allow_once";
        state
            .handle(
                &UserEvent::Paste("first line\r\n\t雪; allow_once".into()),
                &mut renderer,
            )
            .unwrap();
        assert_eq!(state.draft(), answer);
        state
            .handle(
                &UserEvent::Paste("\x1b]0;ignored\x07".into()),
                &mut renderer,
            )
            .unwrap();
        assert_eq!(
            state.draft(),
            answer,
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
            state.draft(),
            full,
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
        assert!(state.answers.is_empty() && state.draft().is_empty());
        assert!(!state.text_mode);
    }

    #[test]
    fn esc_in_free_text_mode_returns_to_the_options_and_only_then_stops() {
        let (request, _reply) = request();
        let mut renderer = Renderer::for_app_test().unwrap();
        let mut state = QuestionState::begin(request, &mut renderer).unwrap();
        state
            .handle(&key(KeyCode::Char('t')), &mut renderer)
            .unwrap();
        state
            .handle(&key(KeyCode::Char('x')), &mut renderer)
            .unwrap();
        assert!(state.text_mode && state.draft() == "x");
        assert!(matches!(
            state.handle(&key(KeyCode::Esc), &mut renderer).unwrap(),
            QuestionEvent::Waiting
        ));
        assert!(!state.text_mode && state.draft().is_empty());
        assert!(matches!(
            state.handle(&key(KeyCode::Esc), &mut renderer).unwrap(),
            QuestionEvent::Stop
        ));
    }

    fn multi_request() -> (
        QuestionRequest,
        tokio::sync::oneshot::Receiver<QuestionAnswer>,
    ) {
        let (reply, received) = tokio::sync::oneshot::channel();
        let option = |label: &str| QuestionOption {
            label: label.into(),
            description: String::new(),
        };
        (
            QuestionRequest {
                id: uuid::Uuid::new_v4().to_string(),
                questions: QuestionBatch(vec![
                    Question {
                        question: "Pick one".into(),
                        options: vec![option("a"), option("b"), option("c")],
                        allow_free_text: true,
                        multi_select: false,
                    },
                    Question {
                        question: "Pick some".into(),
                        options: vec![option("x"), option("y"), option("z")],
                        allow_free_text: false,
                        multi_select: true,
                    },
                ]),
                #[cfg(feature = "acp")]
                tool_call_id: None,
                reply,
            },
            received,
        )
    }

    #[tokio::test]
    async fn arrows_enter_space_and_back_drive_a_batch() {
        let (request, reply) = multi_request();
        let mut renderer = Renderer::for_app_test().unwrap();
        let mut state = QuestionState::begin(request, &mut renderer).unwrap();
        // Down, Down, Enter selects the third option.
        for code in [KeyCode::Down, KeyCode::Down, KeyCode::Enter] {
            state.handle(&key(code), &mut renderer).unwrap();
        }
        assert_eq!(state.answers, vec![Answer::Option(2)]);
        // Left re-opens question 1 with its answer highlighted.
        state.handle(&key(KeyCode::Left), &mut renderer).unwrap();
        assert!(state.answers.is_empty());
        assert_eq!(state.selected, 2);
        // Up wraps, then the first option is chosen with a digit.
        state
            .handle(&key(KeyCode::Char('1')), &mut renderer)
            .unwrap();
        assert_eq!(state.answers, vec![Answer::Option(0)]);
        // Multi-select: Space ticks the highlighted option, a digit another,
        // pressing Space twice unticks.
        state
            .handle(&key(KeyCode::Char(' ')), &mut renderer)
            .unwrap();
        state
            .handle(&key(KeyCode::Char(' ')), &mut renderer)
            .unwrap();
        state
            .handle(&key(KeyCode::Char(' ')), &mut renderer)
            .unwrap();
        state
            .handle(&key(KeyCode::Char('3')), &mut renderer)
            .unwrap();
        assert_eq!(
            state.checked.iter().copied().collect::<Vec<_>>(),
            vec![0, 2]
        );
        assert!(matches!(
            state.handle(&key(KeyCode::Enter), &mut renderer).unwrap(),
            QuestionEvent::Finished
        ));
        match reply.await.unwrap() {
            QuestionAnswer::Answered(answers) => {
                assert_eq!(
                    answers,
                    vec![Answer::Option(0), Answer::Options(vec![0, 2])]
                )
            }
            _ => panic!("expected answers"),
        }
    }

    #[test]
    fn text_mode_is_a_line_editor_with_word_delete_and_back_on_empty() {
        let (request, _reply) = request();
        let mut renderer = Renderer::for_app_test().unwrap();
        let mut state = QuestionState::begin(request, &mut renderer).unwrap();
        state
            .handle(&key(KeyCode::Char('1')), &mut renderer)
            .unwrap();
        assert!(state.text_mode, "question 2 is text-only");
        for c in "alpha beta".chars() {
            state.handle(&key(KeyCode::Char(c)), &mut renderer).unwrap();
        }
        state
            .handle(
                &UserEvent::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)),
                &mut renderer,
            )
            .unwrap();
        assert_eq!(state.draft(), "alpha ");
        state.handle(&key(KeyCode::Left), &mut renderer).unwrap();
        assert_eq!(state.editor.cursor, 5, "Left moves the cursor in text mode");
        // Clear the draft; Backspace on the empty draft returns to question 1.
        state.clear_draft();
        state
            .handle(&key(KeyCode::Backspace), &mut renderer)
            .unwrap();
        assert!(state.answers.is_empty() && !state.text_mode);
    }
}
