//! Bounded, ordered canonical user content. Imported images are data, never paths.
use base64::Engine;
use serde::{Deserialize, Serialize};

pub(crate) const MAX_USER_BLOCKS: usize = 128;
pub(crate) const MAX_USER_IMAGES: usize = 8;
pub(crate) const MAX_USER_IMAGE_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const MAX_USER_IMAGE_TOTAL: usize = 4 * 1024 * 1024;
pub(crate) const MAX_USER_TEXT_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_USER_ENCODED_BYTES: usize = 6 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ImageMime {
    #[serde(rename = "image/png")]
    Png,
    #[serde(rename = "image/jpeg")]
    Jpeg,
    #[serde(rename = "image/gif")]
    Gif,
    #[serde(rename = "image/webp")]
    Webp,
}

impl ImageMime {
    #[allow(
        dead_code,
        reason = "shared API is intentionally dormant before its frontend consumer lands"
    )]
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "image/png" => Some(Self::Png),
            "image/jpeg" => Some(Self::Jpeg),
            "image/gif" => Some(Self::Gif),
            "image/webp" => Some(Self::Webp),
            _ => None,
        }
    }
    #[allow(
        dead_code,
        reason = "shared typed API precedes its ACP session consumer"
    )]
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }
    fn sdk(&self) -> rig::message::ImageMediaType {
        use rig::message::ImageMediaType;
        match self {
            Self::Png => ImageMediaType::PNG,
            Self::Jpeg => ImageMediaType::JPEG,
            Self::Gif => ImageMediaType::GIF,
            Self::Webp => ImageMediaType::WEBP,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum UserBlock {
    Text { text: String },
    Image { data: String, mime_type: ImageMime },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "UncheckedUserMessage")]
pub(crate) struct PersistedUserMessage {
    blocks: Vec<UserBlock>,
}

impl std::fmt::Debug for PersistedUserMessage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PersistedUserMessage")
            .field("block_count", &self.blocks.len())
            .field("has_images", &self.has_images())
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedUserMessage {
    blocks: Vec<UserBlock>,
}

impl TryFrom<UncheckedUserMessage> for PersistedUserMessage {
    type Error = &'static str;
    fn try_from(value: UncheckedUserMessage) -> Result<Self, Self::Error> {
        Self::new(value.blocks)
    }
}

impl PersistedUserMessage {
    pub(crate) fn new(blocks: Vec<UserBlock>) -> Result<Self, &'static str> {
        let message = Self { blocks };
        message.validate()?;
        Ok(message)
    }
    #[allow(
        dead_code,
        reason = "shared typed API precedes its ACP session consumer"
    )]
    pub(crate) fn blocks(&self) -> &[UserBlock] {
        &self.blocks
    }
    pub(crate) fn has_images(&self) -> bool {
        self.blocks
            .iter()
            .any(|block| matches!(block, UserBlock::Image { .. }))
    }
    pub(crate) fn literal_text(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                UserBlock::Text { text } => Some(text.as_str()),
                UserBlock::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.blocks.is_empty() || self.blocks.len() > MAX_USER_BLOCKS {
            return Err("user content block count is outside the supported limit");
        }
        // All encoded/count/text limits precede base64 decoding. Each image,
        // aggregate raw image data and the whole encoded payload have separate caps.
        let mut encoded = 0usize;
        let mut text_bytes = 0usize;
        let mut images = 0usize;
        for block in &self.blocks {
            let bytes = match block {
                UserBlock::Text { text } => {
                    if text.is_empty() {
                        return Err("empty typed user text block");
                    }
                    text_bytes = text_bytes.saturating_add(text.len());
                    text.len()
                }
                UserBlock::Image { data, .. } => {
                    images += 1;
                    if data.is_empty() || data.len() > MAX_USER_IMAGE_BYTES.div_ceil(3) * 4 {
                        return Err("user image exceeds the individual byte limit");
                    }
                    data.len()
                }
            };
            encoded = encoded.saturating_add(bytes);
        }
        if images > MAX_USER_IMAGES
            || text_bytes > MAX_USER_TEXT_BYTES
            || encoded > MAX_USER_ENCODED_BYTES
        {
            return Err("user content exceeds its aggregate limit");
        }
        struct EncodedBytes(usize);
        impl std::io::Write for EncodedBytes {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self
                    .0
                    .checked_add(bytes.len())
                    .filter(|total| *total <= MAX_USER_ENCODED_BYTES)
                    .ok_or_else(|| std::io::Error::other("encoded user content limit"))?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        // Count the actual JSON representation, including escaping/envelopes,
        // without allocating another encoded body or decoding any image yet.
        serde_json::to_writer(EncodedBytes(0), self)
            .map_err(|_| "user content exceeds the encoded byte limit")?;
        let mut decoded_total = 0usize;
        for block in &self.blocks {
            if let UserBlock::Image { data, .. } = block {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|_| "invalid base64 user image")?;
                if decoded.is_empty() || decoded.len() > MAX_USER_IMAGE_BYTES {
                    return Err("user image exceeds the individual byte limit");
                }
                decoded_total = decoded_total.saturating_add(decoded.len());
                if decoded_total > MAX_USER_IMAGE_TOTAL {
                    return Err("user images exceed the aggregate byte limit");
                }
            }
        }
        Ok(())
    }
    pub(crate) fn to_message(&self) -> rig::message::Message {
        let content = self
            .blocks
            .iter()
            .map(|block| match block {
                UserBlock::Text { text } => rig::message::UserContent::text(text.clone()),
                UserBlock::Image { data, mime_type } => rig::message::UserContent::image_base64(
                    data.clone(),
                    Some(mime_type.sdk()),
                    None,
                ),
            })
            .collect::<Vec<_>>();
        rig::message::Message::User {
            content: rig::OneOrMany::many(content)
                .expect("validated nonempty canonical user content"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn image(bytes: &[u8]) -> UserBlock {
        UserBlock::Image {
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
            mime_type: ImageMime::Png,
        }
    }
    #[test]
    fn canonical_user53_order_survives_storage_and_converter() {
        let user = PersistedUserMessage::new(vec![
            UserBlock::Text {
                text: "before 雪".into(),
            },
            image(b"inert bounded bytes"),
            UserBlock::Text {
                text: "after $HOME @file".into(),
            },
        ])
        .unwrap();
        let expected = user.to_message();
        let mut session = crate::session::Session::new("logical", "model", 32_000, "");
        session.add_user_message(user);
        let encoded = serde_json::to_string(&session).unwrap();
        let restored = crate::session::storage::decode_session_json(&encoded).unwrap();
        assert_eq!(
            crate::agent::runner::convert_history(&restored),
            vec![expected]
        );
        assert_eq!(
            restored.messages[0].content.as_str(),
            "before 雪\nafter $HOME @file"
        );
        assert!(restored.messages[0].user.is_some());
    }
    #[test]
    fn canonical_user53_rejects_role_projection_and_forged_tool_authority() {
        let mut session = crate::session::Session::new("logical", "model", 32_000, "");
        session.add_user_message(PersistedUserMessage::new(vec![image(b"inert")]).unwrap());
        let valid = serde_json::to_value(&session).unwrap();
        for (field, value) in [
            ("role", serde_json::json!("system")),
            ("content", serde_json::json!("forged text")),
            ("tool_call_id", serde_json::json!("toolu_fake")),
            (
                "tool",
                serde_json::json!({"kind":"call","name":"write","arguments":{}}),
            ),
        ] {
            let mut invalid = valid.clone();
            invalid["messages"][0][field] = value;
            assert!(
                crate::session::storage::decode_session_json(&invalid.to_string()).is_err(),
                "{field}"
            );
        }
        session.messages[0].content = "mutated".into();
        assert!(session.validate_user_messages().is_err());
    }
    #[test]
    fn canonical_user53_independent_count_text_encoded_and_image_caps() {
        assert!(PersistedUserMessage::new(Vec::new()).is_err());
        assert!(PersistedUserMessage::new(vec![image(b"x"); MAX_USER_IMAGES + 1]).is_err());
        assert!(
            PersistedUserMessage::new(vec![
                UserBlock::Text { text: "x".into() };
                MAX_USER_BLOCKS + 1
            ])
            .is_err()
        );
        assert!(
            PersistedUserMessage::new(vec![UserBlock::Text {
                text: "x".repeat(MAX_USER_TEXT_BYTES + 1)
            }])
            .is_err()
        );
        assert!(
            PersistedUserMessage::new(vec![UserBlock::Image {
                data: "%INVALID%".into(),
                mime_type: ImageMime::Png
            }])
            .is_err()
        );
        assert!(ImageMime::parse("image/svg+xml").is_none());
        let large = image(&vec![b'x'; MAX_USER_IMAGE_BYTES]);
        assert!(PersistedUserMessage::new(vec![large.clone(), large.clone()]).is_ok());
        // Encoded bound and raw aggregate are independent: three full images
        // exceed encoded bytes; three slightly smaller ones fit encoded bytes
        // but still exceed the decoded aggregate.
        assert!(
            PersistedUserMessage::new(vec![large.clone(); 3])
                .unwrap_err()
                .contains("aggregate")
        );
        let smaller = image(&vec![b'x'; MAX_USER_IMAGE_TOTAL / 3 + 1]);
        assert!(
            PersistedUserMessage::new(vec![smaller; 3])
                .unwrap_err()
                .contains("aggregate byte")
        );
        assert!(
            PersistedUserMessage::new(vec![image(&vec![b'x'; MAX_USER_IMAGE_BYTES + 1])]).is_err()
        );
        assert!(
            PersistedUserMessage::new(vec![
                large,
                UserBlock::Text {
                    text: "\u{1}".repeat(MAX_USER_TEXT_BYTES)
                },
            ])
            .unwrap_err()
            .contains("encoded byte")
        );
    }
    #[test]
    fn canonical_user53_legacy_text_encoding_remains_absent_and_exact() {
        let mut session = crate::session::Session::new("logical", "model", 32_000, "");
        session.add_message(crate::session::MessageRole::User, "literal $HOME /undo 雪");
        let json = serde_json::to_value(&session.messages[0]).unwrap();
        assert!(json.get("user").is_none());
        let restored: crate::session::SessionMessage = serde_json::from_value(json).unwrap();
        assert!(restored.user.is_none());
        assert_eq!(restored.content, "literal $HOME /undo 雪");
    }
    #[cfg(feature = "export")]
    #[test]
    fn canonical_user53_jsonl_preserves_payload_and_rejects_forged_role() {
        let mut session = crate::session::Session::new("logical", "model", 32_000, "");
        let input = PersistedUserMessage::new(vec![
            UserBlock::Text {
                text: "literal".into(),
            },
            image(b"inert"),
        ])
        .unwrap();
        session.add_user_message(input.clone());
        let jsonl = crate::extras::export::session_to_jsonl(&session).unwrap();
        let crate::extras::export::ParsedSessionFile::Jsonl(imported) =
            crate::extras::export::parse_session_file(&jsonl).unwrap()
        else {
            panic!("expected JSONL")
        };
        assert_eq!(imported.messages[0].user.as_ref(), Some(&input));
        let mut lines = jsonl.lines().map(str::to_owned).collect::<Vec<_>>();
        let mut row: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        row["role"] = serde_json::json!("assistant");
        lines[1] = row.to_string();
        assert!(crate::extras::export::parse_session_file(&lines.join("\n")).is_err());
    }
    #[cfg(feature = "export")]
    #[test]
    fn canonical_user53_jsonl_import_keeps_legacy_missing_estimates() {
        let bare = "{\"role\":\"user\",\"content\":\"legacy\"}\n{\"role\":\"assistant\",\"content\":\"answer\"}";
        let messages = crate::extras::export::parse_jsonl_import(bare).unwrap();
        assert_eq!(messages[0].content, "legacy");
        assert_eq!(messages[0].estimated_tokens, 0);
        assert!(messages[0].user.is_none());
        let session = crate::session::Session::new("logical", "model", 128_000, "");
        let header = crate::extras::export::session_to_jsonl(&session).unwrap();
        let headered = format!("{}\n{}", header.trim(), bare);
        let crate::extras::export::ParsedSessionFile::Jsonl(imported) =
            crate::extras::export::parse_session_file(&headered).unwrap()
        else {
            panic!("expected headered JSONL")
        };
        assert_eq!(imported.messages.len(), 2);
        assert_eq!(imported.messages[1].estimated_tokens, 0);
    }
}
