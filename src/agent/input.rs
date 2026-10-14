//! Exact initial model content plus a separate literal text projection for host gates.
#[derive(Clone, Debug)]
pub(crate) struct PromptInput {
    text: String,
    user: Option<crate::session::user::PersistedUserMessage>,
}
impl PromptInput {
    pub(crate) fn text(text: String) -> Self {
        Self { text, user: None }
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
        }
    }
    pub(crate) fn persisted_user(&self) -> Option<&crate::session::user::PersistedUserMessage> {
        self.user.as_ref()
    }
    pub(crate) fn literal_text(&self) -> &str {
        &self.text
    }
    pub(crate) fn to_message(&self) -> rig::message::Message {
        self.user.as_ref().map_or_else(
            || rig::message::Message::user(self.text.clone()),
            crate::session::user::PersistedUserMessage::to_message,
        )
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

#[cfg(not(feature = "multimodal"))]
pub(crate) fn history_has_images(history: &[rig::message::Message]) -> bool {
    history.iter().any(|message| match message {
        rig::message::Message::User { content } => content
            .iter()
            .any(|block| matches!(block, rig::message::UserContent::Image(_))),
        rig::message::Message::Assistant { .. } | rig::message::Message::System { .. } => false,
    })
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
