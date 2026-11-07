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
    pub(crate) fn sdk(&self) -> rig::message::ImageMediaType {
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
    /// Cheap per-save recheck. A value of this type can only be built through
    /// `new`/deserialization, which run the full [`Self::validate`] (JSON
    /// envelope count and base64 decode), and its blocks are private and never
    /// mutated, so a save repeats only the O(blocks) structural limits and
    /// never decodes an image again (mini-agent-gjuyl).
    pub(crate) fn validate_stored(&self) -> Result<(), &'static str> {
        self.validate_limits()
    }
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        self.validate_limits()?;
        self.validate_encoding()
    }
    fn validate_limits(&self) -> Result<(), &'static str> {
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
        Ok(())
    }
    fn validate_encoding(&self) -> Result<(), &'static str> {
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
    fn image_heavy_session_with_few_tokens_reports_size_pressure() {
        let mut session = crate::session::Session::new("logical", "model", 1_000_000, "");
        // 5 prompts of two ~2.4 MiB base64 images: far under any token threshold,
        // past 75% of the 32 MiB persisted-session ceiling.
        let raw = vec![7u8; 1_900_000];
        for _ in 0..5 {
            session.add_user_message(
                PersistedUserMessage::new(vec![image(&raw), image(&raw)]).unwrap(),
            );
        }
        assert!(session.approx_persisted_bytes() > 24 * 1024 * 1024);
        assert!(session.effective_context_tokens() < 100_000);
        assert!(!session.needs_compaction_after_tool_result_pruning(0, 0));
        assert!(session.size_pressure(Some(0.75)));
        assert!(!session.size_pressure(Some(1.0)));
        assert!(!session.size_pressure(None), "0 disables the byte trigger");
        let small = crate::session::Session::new("logical", "model", 1_000_000, "");
        assert!(!small.size_pressure(Some(0.75)));
    }
    #[test]
    fn mid_turn_byte_pressure_stops_when_compaction_cannot_shrink_retained_images() {
        let mut session = crate::session::Session::new("logical", "model", 1_000_000, "");
        let raw = vec![7u8; 1_900_000];
        for _ in 0..7 {
            session.add_user_message(
                PersistedUserMessage::new(vec![image(&raw), image(&raw)]).unwrap(),
            );
        }
        assert!(session.mid_turn_size_pressure(Some(0.75)));
        // A relief that summarizes one prompt leaves the kept images over the
        // threshold: arming at every tool batch would only repeat it.
        session.compress("recap".to_owned(), 1, 0);
        assert!(session.size_pressure(Some(0.75)));
        assert!(!session.mid_turn_size_pressure(Some(0.75)));
        // Material growth after the futile compaction re-arms the relief
        // (a long tool-heavy turn adding images toward the hard ceiling).
        session
            .add_user_message(PersistedUserMessage::new(vec![image(&raw), image(&raw)]).unwrap());
        assert!(session.mid_turn_size_pressure(Some(0.75)));
        // A compaction that does bring the session under the threshold keeps
        // the trigger meaningful for later growth.
        session.compress("recap".to_owned(), 5, 0);
        assert!(!session.size_pressure(Some(0.75)));
        for _ in 0..3 {
            session.add_user_message(
                PersistedUserMessage::new(vec![image(&raw), image(&raw)]).unwrap(),
            );
        }
        assert!(session.size_pressure(Some(0.75)));
        assert!(session.mid_turn_size_pressure(Some(0.75)));
    }
    fn image_session(prompts: usize) -> crate::session::Session {
        let mut session = crate::session::Session::new("logical", "model", 1_000_000, "");
        let raw = vec![7u8; 1_900_000];
        for _ in 0..prompts {
            session.add_user_message(
                PersistedUserMessage::new(vec![image(&raw), image(&raw)]).unwrap(),
            );
        }
        session
    }
    #[test]
    fn compaction_floor_is_lowered_when_messages_are_removed() {
        // mini-agent-24lo9
        let mut session = image_session(7);
        session.compress("recap".to_owned(), 1, 0);
        assert!(
            session
                .bytes_after_last_compaction
                .is_some_and(|f| f > 24 << 20)
        );
        session.truncate_to(0);
        let floor = session.bytes_after_last_compaction.unwrap();
        assert!(floor <= session.approx_persisted_bytes());
        // Regrow past the threshold: relief must arm again.
        let raw = vec![7u8; 1_900_000];
        for _ in 0..6 {
            session.add_user_message(
                PersistedUserMessage::new(vec![image(&raw), image(&raw)]).unwrap(),
            );
        }
        assert!(session.size_pressure(Some(0.75)));
        assert!(session.mid_turn_size_pressure(Some(0.75)));
    }
    #[test]
    fn compaction_floor_is_lowered_after_retained_images_are_dropped() {
        // mini-agent-24lo9: the floor must be measured after the rewind drops
        // the compaction (and the PNGs it retained), not before.
        const PNG: &[u8] = include_bytes!("../agent/tools/fixtures/read-image64/3x2.png");
        let mut session = crate::session::Session::new("logical", "model", 1_000_000, "");
        for n in 0..3u8 {
            let mut bytes = PNG.to_vec();
            bytes.resize(1_500_000, 0);
            bytes.push(n);
            session.add_user_message(PersistedUserMessage::new(vec![image(&bytes)]).unwrap());
        }
        session.compress("recap".to_owned(), 2, 0);
        assert!(
            session.compactions.iter().any(|c| !c.images.is_empty()),
            "fixture must retain images"
        );
        let before = session.bytes_after_last_compaction.unwrap();
        assert!(before > 1_000_000);
        session.truncate_to(0);
        assert!(session.compactions.is_empty());
        let floor = session.bytes_after_last_compaction.unwrap();
        assert!(floor <= session.approx_persisted_bytes(), "floor={floor}");
    }
    #[test]
    fn redo_discards_a_floor_measured_before_the_restore() {
        let mut session = image_session(7);
        session.compress("recap".to_owned(), 1, 0);
        session.bytes_after_last_compaction = Some(40 << 20);
        session.rewind_to(1);
        session.redo();
        assert_eq!(session.bytes_after_last_compaction, None);
    }
    #[test]
    fn between_turn_pressure_honours_the_futility_floor() {
        // mini-agent-stgyu
        let mut session = image_session(7);
        assert!(session.actionable_size_pressure(Some(0.75)));
        session.compress("recap".to_owned(), 1, 0);
        assert!(session.size_pressure(Some(0.75)));
        assert!(
            !session.actionable_size_pressure(Some(0.75)),
            "a futile compaction must not re-arm the paid summarizer every turn"
        );
    }
    #[test]
    fn rewind_and_redo_clear_the_stale_futile_compaction_byte_floor() {
        let mut session = crate::session::Session::new("logical", "model", 1_000_000, "");
        let raw = vec![7u8; 1_900_000];
        for _ in 0..7 {
            session.add_user_message(
                PersistedUserMessage::new(vec![image(&raw), image(&raw)]).unwrap(),
            );
        }
        session.compress("recap".to_owned(), 1, 0);
        assert!(session.bytes_after_last_compaction.is_some());
        assert!(!session.mid_turn_size_pressure(Some(0.75)));
        // Rewind to a small history, then regrow just past the threshold,
        // below floor + re-arm growth.
        let live = session.messages.len();
        session.rewind_to(live - 3);
        assert!(session.bytes_after_last_compaction.is_none());
        session.truncate_to(2);
        for _ in 0..5 {
            session.add_user_message(
                PersistedUserMessage::new(vec![image(&raw), image(&raw)]).unwrap(),
            );
        }
        assert!(session.size_pressure(Some(0.75)));
        assert!(session.mid_turn_size_pressure(Some(0.75)));
        // redo of a rewind also drops the floor.
        session.bytes_after_last_compaction = Some(usize::MAX / 2);
        session.rewind_to(1);
        session.bytes_after_last_compaction = Some(usize::MAX / 2);
        assert!(session.redo());
        assert!(session.bytes_after_last_compaction.is_none());
    }
    #[test]
    fn history_prefix_conversion_leaves_the_session_cache_and_messages_alone() {
        let mut session = crate::session::Session::new("logical", "model", 32_000, "");
        session.add_user_message(
            PersistedUserMessage::new(vec![UserBlock::Text { text: "one".into() }]).unwrap(),
        );
        session.add_message(crate::session::MessageRole::Assistant, "two");
        session.add_user_message(
            PersistedUserMessage::new(vec![UserBlock::Text {
                text: "three".into(),
            }])
            .unwrap(),
        );
        assert!(!session.prefix_has_images(3));
        let full = crate::agent::runner::convert_history_shared_with_tool_result_retention(
            &session,
            usize::MAX,
        );
        let prefix = crate::agent::runner::convert_history_prefix(&session, 2, usize::MAX);
        assert_eq!(prefix.len(), 2);
        assert_eq!(session.messages.len(), 3);
        let again = crate::agent::runner::convert_history_shared_with_tool_result_retention(
            &session,
            usize::MAX,
        );
        assert_eq!(again.len(), full.len(), "the full history is still served");
        assert_eq!(full.len(), 3);
        session.add_user_message(PersistedUserMessage::new(vec![image(b"px")]).unwrap());
        assert!(session.prefix_has_images(4));
        assert!(!session.prefix_has_images(3));
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
    // mini-agent-gjuyl: the per-save recheck keeps the structural limits but
    // never decodes an image; construction/deserialization still do.
    #[test]
    fn stored_user_rows_recheck_limits_without_decoding_images() {
        let forged = PersistedUserMessage {
            blocks: vec![UserBlock::Image {
                data: "not base64 !!".into(),
                mime_type: ImageMime::Png,
            }],
        };
        assert!(forged.validate().is_err());
        assert!(forged.validate_stored().is_ok());
        let empty = PersistedUserMessage { blocks: Vec::new() };
        assert!(empty.validate_stored().is_err());
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
