//! Exact initial model content plus a separate literal text projection for host gates.
//!
//! A prompt may carry a non-persisted per-request host directive
//! ([`PromptInput::with_turn_directive`]). It is rendered only into the model
//! request built by [`PromptInput::to_message`], as a host-labelled text block
//! appended after the user's own content. It is never part of
//! [`PromptInput::literal_text`] or [`PromptInput::persisted_user`], so the
//! UserPromptSubmit hook, the saved transcript, titles, rewind and compaction
//! only ever see the original text.

/// Wording budget for a per-request host directive, in UTF-8 bytes. Longer
/// directives are truncated at a character boundary when attached. Directives
/// are host-authored routing hints: they may name personas, but must never
/// carry persona bodies, file contents or other user/project data.
pub(crate) const TURN_DIRECTIVE_MAX_BYTES: usize = 1024;

/// Label that frames the directive block so the model can tell it apart from
/// the user's words.
pub(crate) const TURN_DIRECTIVE_LABEL: &str =
    "[Host directive for this request only; not written by the user]";

#[derive(Clone, Debug)]
pub(crate) struct PromptInput {
    text: String,
    user: Option<crate::session::user::PersistedUserMessage>,
    /// Non-persisted host directive for this request only. Not serialised,
    /// not part of `literal_text()`, never stored in the session.
    turn_directive: Option<String>,
}
impl PromptInput {
    pub(crate) fn text(text: String) -> Self {
        Self {
            text,
            user: None,
            turn_directive: None,
        }
    }
    /// Attach (or, with `None`, clear) the per-request host directive. Blank
    /// directives are dropped; over-budget ones are truncated to
    /// [`TURN_DIRECTIVE_MAX_BYTES`] at a character boundary.
    pub(crate) fn with_turn_directive(mut self, directive: Option<String>) -> Self {
        self.turn_directive = directive.and_then(bounded_turn_directive);
        self
    }
    #[cfg(test)]
    pub(crate) fn turn_directive(&self) -> Option<&str> {
        self.turn_directive.as_deref()
    }
    #[cfg(any(feature = "acp", feature = "multimodal", test))]
    #[allow(
        dead_code,
        reason = "shared API is intentionally dormant before its frontend consumer lands"
    )]
    pub(crate) fn user(user: crate::session::user::PersistedUserMessage) -> Self {
        Self {
            text: user.literal_text(),
            user: Some(user),
            turn_directive: None,
        }
    }
    pub(crate) fn persisted_user(&self) -> Option<&crate::session::user::PersistedUserMessage> {
        self.user.as_ref()
    }
    pub(crate) fn literal_text(&self) -> &str {
        &self.text
    }
    /// The exact model content for this request: the user's content followed
    /// by the host directive block, if any. Never persist this message; use
    /// `persisted_user()` / `literal_text()` for durable surfaces.
    pub(crate) fn to_message(&self) -> rig::message::Message {
        let message = self.user.as_ref().map_or_else(
            || rig::message::Message::user(self.text.clone()),
            crate::session::user::PersistedUserMessage::to_message,
        );
        let Some(directive) = &self.turn_directive else {
            return message;
        };
        let rig::message::Message::User { content } = message else {
            unreachable!("a prompt always renders as a user message");
        };
        let mut blocks: Vec<_> = content.into_iter().collect();
        blocks.push(rig::message::UserContent::text(format!(
            "{TURN_DIRECTIVE_LABEL}\n{directive}"
        )));
        rig::message::Message::User {
            content: rig::OneOrMany::many(blocks).expect("nonempty user content"),
        }
    }
    #[cfg(any(feature = "hooks", test))]
    pub(crate) fn apply_hook_text(&mut self, text: String) -> Result<(), &'static str> {
        if self.user.is_some() && text != self.text {
            return Err(
                "a user-prompt hook cannot rewrite ordered media content; submit a text-only prompt or preserve the original text",
            );
        }
        self.text = text;
        Ok(())
    }
}

fn bounded_turn_directive(directive: String) -> Option<String> {
    let trimmed = directive.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() <= TURN_DIRECTIVE_MAX_BYTES {
        return Some(trimmed.to_owned());
    }
    let mut end = TURN_DIRECTIVE_MAX_BYTES;
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    tracing::warn!(
        bytes = trimmed.len(),
        limit = TURN_DIRECTIVE_MAX_BYTES,
        "host turn directive exceeds its budget; truncated"
    );
    Some(trimmed[..end].to_owned())
}

#[cfg(not(feature = "multimodal"))]
pub(crate) fn history_has_images(history: &[rig::message::Message]) -> bool {
    history.iter().any(|message| match message {
        rig::message::Message::User { content } => content
            .iter()
            .any(|block| matches!(block, rig::message::UserContent::Image(_)) || matches!(block, rig::message::UserContent::ToolResult(result) if result.content.iter().any(|part| matches!(part, rig::message::ToolResultContent::Image(_))))),
        rig::message::Message::Assistant { .. } | rig::message::Message::System { .. } => false,
    })
}

/// Append host context as transcript prose, matching canonical System rows.
/// This never changes the provider's top-level system prefix.
#[cfg(any(feature = "hooks", feature = "memory", feature = "subagents"))]
pub(crate) fn append_context_history(
    history: std::sync::Arc<[rig::completion::Message]>,
    notices: &[String],
) -> std::sync::Arc<[rig::completion::Message]> {
    if notices.is_empty() {
        return history;
    }
    let mut history = history.to_vec();
    history.extend(
        notices
            .iter()
            .cloned()
            .map(rig::completion::Message::assistant),
    );
    history.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(feature = "multimodal"))]
    #[test]
    fn typed_user53_feature_off_detects_retained_images_without_flattening() {
        let user = crate::session::user::PersistedUserMessage::new(vec![
            crate::session::user::UserBlock::Image {
                data: "aW5lcnQ=".into(),
                mime_type: crate::session::user::ImageMime::Png,
            },
        ])
        .unwrap();
        let message = user.to_message();
        assert!(history_has_images(std::slice::from_ref(&message)));
        assert_eq!(message, user.to_message());
        assert!(!history_has_images(&[rig::message::Message::user(
            "{\"type\":\"image\"}"
        )]));
    }
    fn user_texts(message: &rig::message::Message) -> Vec<String> {
        let rig::message::Message::User { content } = message else {
            panic!("expected a user message");
        };
        content
            .iter()
            .filter_map(|block| match block {
                rig::message::UserContent::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn turn_directive_renders_last_for_the_model_but_not_in_literal_or_persisted_text() {
        let prompt = PromptInput::text("original words".into())
            .with_turn_directive(Some("  route to persona reviewer  ".into()));
        assert_eq!(prompt.literal_text(), "original words");
        assert!(prompt.persisted_user().is_none());
        assert_eq!(prompt.turn_directive(), Some("route to persona reviewer"));
        let texts = user_texts(&prompt.to_message());
        assert_eq!(texts.len(), 2);
        assert_eq!(texts[0], "original words");
        assert_eq!(
            texts[1],
            format!("{TURN_DIRECTIVE_LABEL}\nroute to persona reviewer")
        );
        // Without a directive the model content is exactly the literal text.
        let plain = PromptInput::text("original words".into());
        assert_eq!(
            plain.to_message(),
            rig::message::Message::user("original words")
        );
        assert_eq!(
            plain
                .clone()
                .with_turn_directive(Some(" \n ".into()))
                .to_message(),
            plain.to_message()
        );
        assert_eq!(
            prompt.with_turn_directive(None).to_message(),
            plain.to_message()
        );
    }

    #[test]
    fn turn_directive_is_bounded_at_a_char_boundary() {
        let long = "é".repeat(TURN_DIRECTIVE_MAX_BYTES);
        let prompt = PromptInput::text("x".into()).with_turn_directive(Some(long));
        let directive = prompt.turn_directive().unwrap();
        assert!(directive.len() <= TURN_DIRECTIVE_MAX_BYTES);
        assert!(directive.len() > TURN_DIRECTIVE_MAX_BYTES - 2);
        assert!(directive.chars().all(|c| c == 'é'));
        let exact = "a".repeat(TURN_DIRECTIVE_MAX_BYTES);
        let prompt = PromptInput::text("x".into()).with_turn_directive(Some(exact.clone()));
        assert_eq!(prompt.turn_directive(), Some(exact.as_str()));
    }

    #[test]
    fn turn_directive_follows_typed_user_content_and_survives_hook_rewrite() {
        let user = crate::session::user::PersistedUserMessage::new(vec![
            crate::session::user::UserBlock::Text {
                text: "literal".into(),
            },
            crate::session::user::UserBlock::Image {
                data: "aW5lcnQ=".into(),
                mime_type: crate::session::user::ImageMime::Png,
            },
        ])
        .unwrap();
        let prompt = PromptInput::user(user.clone()).with_turn_directive(Some("hint".into()));
        assert_eq!(prompt.persisted_user(), Some(&user));
        assert_eq!(prompt.literal_text(), "literal");
        let rig::message::Message::User { content } = prompt.to_message() else {
            panic!("expected a user message");
        };
        let blocks: Vec<_> = content.into_iter().collect();
        assert_eq!(blocks.len(), 3);
        assert!(matches!(blocks[1], rig::message::UserContent::Image(_)));
        assert!(
            matches!(&blocks[2], rig::message::UserContent::Text(text) if text.text.ends_with("\nhint"))
        );
        let mut text =
            PromptInput::text("original".into()).with_turn_directive(Some("hint".into()));
        text.apply_hook_text("rewritten".into()).unwrap();
        assert_eq!(text.literal_text(), "rewritten");
        assert_eq!(
            user_texts(&text.to_message()),
            vec![
                "rewritten".to_owned(),
                format!("{TURN_DIRECTIVE_LABEL}\nhint")
            ]
        );
    }

    #[test]
    fn typed_user53_hook_rewrite_refuses_media_and_keeps_text_wrapper_semantics() {
        let user = crate::session::user::PersistedUserMessage::new(vec![
            crate::session::user::UserBlock::Text {
                text: "literal".into(),
            },
            crate::session::user::UserBlock::Image {
                data: "aW5lcnQ=".into(),
                mime_type: crate::session::user::ImageMime::Png,
            },
        ])
        .unwrap();
        let mut prompt = PromptInput::user(user);
        let before = prompt.to_message();
        assert!(prompt.apply_hook_text("rewritten".into()).is_err());
        assert_eq!(prompt.to_message(), before);
        assert!(prompt.apply_hook_text("literal".into()).is_ok());
        let mut text = PromptInput::text("original".into());
        text.apply_hook_text("rewritten".into()).unwrap();
        assert_eq!(text.to_message(), rig::message::Message::user("rewritten"));
    }
}
