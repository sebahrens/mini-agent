//! Host-minted evidence records for one child run.
//!
//! After a child finishes, the host walks the child's own conversation and
//! records every tool result in call order under IDs `E1..En`. The IDs come
//! from actual tool calls, so a child cannot cite evidence it never gathered.
//! Record content (text and validated image parts) is written to the
//! session's private tool-output directory; only bounded metadata stays in
//! memory. Nothing here is model-visible by itself: the report renders
//! metadata rows only, and the parent dereferences content on demand through
//! the parent-only `task_evidence` tool under its own permissions.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rig::message::{AssistantContent, Message, ToolResultContent, UserContent};
use serde::{Deserialize, Serialize};

/// Records kept per child; later tool results are only counted.
pub(crate) const MAX_EVIDENCE_RECORDS: usize = 64;
/// Text retained per record (larger results keep a clipped prefix).
pub(crate) const MAX_RECORD_TEXT_BYTES: usize = 256 * 1024;
/// Content (text plus encoded image bytes) retained per child.
pub(crate) const MAX_CHILD_EVIDENCE_BYTES: usize = 16 * 1024 * 1024;
/// Child evidence stores a session keeps; the oldest is evicted and deleted.
pub(crate) const MAX_EVIDENCE_TASKS: usize = 64;
const MAX_TOOL_NAME_BYTES: usize = 48;
const MAX_TARGET_BYTES: usize = 60;
const MAX_PATH_BYTES: usize = 4096;
/// Largest stored record file read back (text cap plus the image ceiling).
const MAX_RECORD_FILE_BYTES: u64 = 24 * 1024 * 1024;

/// Whether the host records evidence and offers `task_evidence` for
/// delegations started now (`task_evidence = false` turns both off).
pub(crate) fn enabled(config: &crate::config::Config) -> bool {
    config.task_evidence.unwrap_or(true)
}

/// Metadata of one image part of a tool result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ImageMeta {
    pub mime: String,
    pub width: u32,
    pub height: u32,
    /// Decoded bytes.
    pub bytes: u64,
    pub est_tokens: u64,
}

/// Why a record's content is not (fully) retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Retention {
    Full,
    /// The text was longer than [`MAX_RECORD_TEXT_BYTES`]; a prefix is kept.
    TextClipped,
    /// No content kept: the child's evidence byte ceiling was reached.
    OverCeiling,
    /// No content kept: the session store could not write it.
    Unstored,
}

/// Host-computed metadata for one tool result. `target` is derived from the
/// child-chosen tool arguments, so it is sanitized and bounded before any
/// rendering; `path` is kept only to re-check the parent's read permission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EvidenceMeta {
    /// 1-based; rendered as `E{number}`.
    pub number: usize,
    pub tool: String,
    pub target: String,
    pub path: Option<String>,
    pub text_bytes: u64,
    pub images: Vec<ImageMeta>,
    /// Image parts that could not be validated (not retained).
    pub invalid_images: usize,
    pub est_tokens: u64,
    pub retention: Retention,
}

impl EvidenceMeta {
    pub(crate) fn id(&self) -> String {
        format!("E{}", self.number)
    }

    /// Short MIME summary: `text/plain`, an image MIME, or both.
    pub(crate) fn mime(&self) -> String {
        let mut kinds = Vec::new();
        if self.text_bytes > 0 || self.images.is_empty() {
            kinds.push("text/plain".to_string());
        }
        for image in &self.images {
            if !kinds.contains(&image.mime) {
                kinds.push(image.mime.clone());
            }
        }
        kinds.join("+")
    }

    pub(crate) fn size(&self) -> String {
        let mut parts = vec![human_bytes(self.text_bytes)];
        for image in &self.images {
            parts.push(format!("{}x{}", image.width, image.height));
        }
        parts.join(", ")
    }
}

fn human_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Literal content of one record, as written to and read from the store.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvidenceContent {
    pub text: String,
    #[serde(default)]
    pub images: Vec<StoredImage>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredImage {
    pub mime: String,
    /// Base64, validated against `mime` when recorded and again when read.
    pub data: String,
}

/// Evidence gathered from one child's conversation(s), before storage.
#[derive(Debug, Default)]
pub(crate) struct ChildEvidence {
    records: Vec<(EvidenceMeta, Option<EvidenceContent>)>,
    /// Tool results past [`MAX_EVIDENCE_RECORDS`], counted only.
    unrecorded: usize,
    content_bytes: usize,
}

impl ChildEvidence {
    pub(crate) fn len(&self) -> usize {
        self.records.len()
    }

    pub(crate) fn unrecorded(&self) -> usize {
        self.unrecorded
    }

    pub(crate) fn metas(&self) -> impl Iterator<Item = &EvidenceMeta> {
        self.records.iter().map(|(meta, _)| meta)
    }

    /// Record every tool result of `history` in tool-call order, continuing
    /// the numbering of anything recorded earlier (a forced continuation is
    /// a second conversation of the same child).
    pub(crate) fn record_history(&mut self, history: &[Message]) {
        let mut calls: Vec<&rig::message::ToolCall> = Vec::new();
        let mut results: std::collections::HashMap<&str, &rig::message::ToolResult> =
            std::collections::HashMap::new();
        for message in history {
            match message {
                Message::Assistant { content, .. } => {
                    for item in content.iter() {
                        if let AssistantContent::ToolCall(call) = item {
                            calls.push(call);
                        }
                    }
                }
                Message::User { content } => {
                    for item in content.iter() {
                        if let UserContent::ToolResult(result) = item {
                            results.entry(result.id.as_str()).or_insert(result);
                            if let Some(call_id) = result.call_id.as_deref() {
                                results.entry(call_id).or_insert(result);
                            }
                        }
                    }
                }
                Message::System { .. } => {}
            }
        }
        let mut used = std::collections::HashSet::new();
        for call in calls {
            let result = results.get(call.id.as_str()).copied().or_else(|| {
                call.call_id
                    .as_deref()
                    .and_then(|id| results.get(id).copied())
            });
            let Some(result) = result else {
                continue;
            };
            // One result answers one call, even if an ID repeats.
            if !used.insert(std::ptr::from_ref(result)) {
                continue;
            }
            self.record(&call.function.name, &call.function.arguments, result);
        }
    }

    fn record(
        &mut self,
        tool: &str,
        arguments: &serde_json::Value,
        result: &rig::message::ToolResult,
    ) {
        if self.records.len() >= MAX_EVIDENCE_RECORDS {
            self.unrecorded = self.unrecorded.saturating_add(1);
            return;
        }
        let mut text = String::new();
        let mut images = Vec::new();
        let mut stored_images = Vec::new();
        let mut invalid_images = 0usize;
        for part in result.content.iter() {
            match part {
                ToolResultContent::Text(part) => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&part.text);
                }
                ToolResultContent::Image(image) => match validated_image(image) {
                    Some((meta, stored)) => {
                        images.push(meta);
                        stored_images.push(stored);
                    }
                    None => invalid_images += 1,
                },
            }
        }
        let text_bytes = text.len() as u64;
        let mut retention = Retention::Full;
        if text.len() > MAX_RECORD_TEXT_BYTES {
            text = crate::extras::subagents::background::clip_bytes(&text, MAX_RECORD_TEXT_BYTES);
            retention = Retention::TextClipped;
        }
        let content_len = text.len()
            + stored_images
                .iter()
                .map(|image: &StoredImage| image.data.len())
                .sum::<usize>();
        let content = if self.content_bytes.saturating_add(content_len) > MAX_CHILD_EVIDENCE_BYTES {
            retention = Retention::OverCeiling;
            None
        } else {
            self.content_bytes += content_len;
            Some(EvidenceContent {
                text,
                images: stored_images,
            })
        };
        let est_tokens = text_bytes.div_ceil(4) + images.iter().map(|i| i.est_tokens).sum::<u64>();
        let path = (tool == "read")
            .then(|| arguments.get("path").and_then(serde_json::Value::as_str))
            .flatten()
            .filter(|path| path.len() <= MAX_PATH_BYTES)
            .map(str::to_owned);
        self.records.push((
            EvidenceMeta {
                number: self.records.len() + 1,
                tool: sanitize(tool, MAX_TOOL_NAME_BYTES),
                target: sanitize(&target_summary(arguments), MAX_TARGET_BYTES),
                path,
                text_bytes,
                images,
                invalid_images,
                est_tokens,
                retention,
            },
            content,
        ));
    }
}

/// Validate a literal base64 image part the same way history retention does.
fn validated_image(image: &rig::message::Image) -> Option<(ImageMeta, StoredImage)> {
    use rig::message::{DocumentSourceKind, MimeType};
    let DocumentSourceKind::Base64(data) = &image.data else {
        return None;
    };
    let mime = image
        .media_type
        .as_ref()
        .and_then(|mime| crate::session::user::ImageMime::parse(mime.to_mime_type()))?;
    let info = crate::session::tool_image::inspect_literal_image(data, &mime).ok()?;
    let tiles = u64::from(info.width.div_ceil(512)) * u64::from(info.height.div_ceil(512));
    Some((
        ImageMeta {
            mime: mime.as_str().to_string(),
            width: info.width,
            height: info.height,
            bytes: info.bytes as u64,
            est_tokens: 256 + tiles * 256,
        },
        StoredImage {
            mime: mime.as_str().to_string(),
            data: data.clone(),
        },
    ))
}

/// The most identifying argument of a call, as plain text.
fn target_summary(arguments: &serde_json::Value) -> String {
    const KEYS: &[&str] = &[
        "pattern", "glob", "query", "url", "command", "code", "name", "path",
    ];
    if let Some(object) = arguments.as_object() {
        let mut parts = Vec::new();
        for key in KEYS {
            if let Some(value) = object.get(*key).and_then(serde_json::Value::as_str) {
                parts.push(value.to_string());
            }
            if parts.len() == 2 {
                break;
            }
        }
        if !parts.is_empty() {
            return parts.join(" in ");
        }
        if object.is_empty() {
            return String::new();
        }
    }
    arguments.to_string()
}

/// Child-chosen text rendered outside the quote fence: one line, no
/// Markdown table/link/fence/heading syntax, no host-marker brackets.
pub(crate) fn sanitize(value: &str, cap: usize) -> String {
    let cleaned: String = value
        .chars()
        .map(|ch| match ch {
            '|' | '`' | '[' | ']' | '<' | '>' | '#' | '*' | '_' | '\\' => '.',
            ch if ch.is_control() => ' ',
            ch => ch,
        })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::extras::subagents::background::clip_bytes(&collapsed, cap)
}

/// One child's stored evidence: metadata in memory, content on disk.
#[derive(Debug)]
pub(crate) struct StoredEvidence {
    pub task_id: String,
    pub metas: Vec<EvidenceMeta>,
    pub unrecorded: usize,
    files: Vec<Option<PathBuf>>,
}

impl StoredEvidence {
    pub(crate) fn get(&self, id: &str) -> Option<(&EvidenceMeta, Option<&PathBuf>)> {
        let number = parse_id(id)?;
        let index = number.checked_sub(1)?;
        let meta = self.metas.get(index)?;
        Some((meta, self.files.get(index).and_then(Option::as_ref)))
    }
}

/// `E<n>` with `n >= 1` and no leading zero.
pub(crate) fn parse_id(id: &str) -> Option<usize> {
    let digits = id.strip_prefix('E')?;
    if digits.is_empty()
        || digits.len() > 6
        || digits.starts_with('0')
        || !digits.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    digits.parse().ok()
}

/// A session's bounded set of child evidence stores. Owned by the session's
/// background pool; never deserialized, so a restarted session has none.
#[derive(Debug, Default)]
pub(crate) struct EvidenceIndex {
    tasks: Mutex<std::collections::VecDeque<Arc<StoredEvidence>>>,
}

impl EvidenceIndex {
    pub(crate) fn get(&self, task_id: &str) -> Option<Arc<StoredEvidence>> {
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|stored| stored.task_id == task_id)
            .cloned()
    }

    /// Write each record's content to the session's private tool-output
    /// directory, then index the child under `task_id`. A record whose write
    /// fails keeps its metadata and reports its content as unstored.
    pub(crate) async fn store(
        &self,
        session_id: &str,
        task_id: String,
        evidence: ChildEvidence,
    ) -> Arc<StoredEvidence> {
        let session = session_id.to_string();
        let records = evidence.records;
        let unrecorded = evidence.unrecorded;
        let written = crate::agent::runner::spawn_blocking_scoped(move || {
            let mut metas = Vec::with_capacity(records.len());
            let mut files = Vec::with_capacity(records.len());
            for (mut meta, content) in records {
                let file = content.and_then(|content| {
                    let json = serde_json::to_string(&content).ok()?;
                    crate::session::storage::save_tool_output(&session, "subagent-evidence", &json)
                        .inspect_err(|error| {
                            tracing::warn!(%error, "failed to store subagent evidence record");
                        })
                        .ok()
                });
                if file.is_none() && meta.retention != Retention::OverCeiling {
                    meta.retention = Retention::Unstored;
                }
                metas.push(meta);
                files.push(file);
            }
            (metas, files)
        })
        .await;
        let (metas, files) = written.unwrap_or_default();
        let stored = Arc::new(StoredEvidence {
            task_id,
            metas,
            unrecorded,
            files,
        });
        let evicted = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            tasks.push_back(stored.clone());
            let mut evicted = Vec::new();
            while tasks.len() > MAX_EVIDENCE_TASKS {
                if let Some(old) = tasks.pop_front() {
                    evicted.push(old);
                }
            }
            evicted
        };
        if !evicted.is_empty() {
            let session = session_id.to_string();
            let _ = crate::agent::runner::spawn_blocking_scoped(move || {
                for old in evicted {
                    for path in old.files.iter().flatten() {
                        if let Err(error) =
                            crate::session::storage::delete_uncommitted_tool_output(&session, path)
                        {
                            tracing::warn!(%error, "failed to delete evicted subagent evidence");
                        }
                    }
                }
            })
            .await;
        }
        stored
    }
}

/// Read a stored record back. The path must be an immediate child of this
/// session's private tool-output directory; images are re-validated.
pub(crate) fn load(session_id: &str, path: &std::path::Path) -> anyhow::Result<EvidenceContent> {
    use std::io::Read;
    let expected = crate::session::storage::tool_output_dir(session_id);
    anyhow::ensure!(
        path.parent() == Some(expected.as_path()),
        "evidence record outside the session store"
    );
    let file = crate::fs::open_private_file(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_RECORD_FILE_BYTES,
        "evidence record exceeds its size limit"
    );
    let content: EvidenceContent = serde_json::from_slice(&bytes)?;
    for image in &content.images {
        let mime = crate::session::user::ImageMime::parse(&image.mime)
            .ok_or_else(|| anyhow::anyhow!("unsupported evidence image MIME type"))?;
        crate::session::tool_image::inspect_literal_image(&image.data, &mime)
            .map_err(anyhow::Error::msg)?;
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::OneOrMany;
    use rig::message::{Image, ImageMediaType, Text, ToolCall, ToolFunction, ToolResult};

    pub(crate) const PNG_3X2: &[u8] =
        include_bytes!("../../agent/tools/fixtures/read-image64/3x2.png");

    fn call(id: &str, name: &str, args: serde_json::Value) -> Message {
        Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::ToolCall(ToolCall {
                id: id.into(),
                call_id: None,
                function: ToolFunction {
                    name: name.into(),
                    arguments: args,
                },
                signature: None,
                additional_params: None,
            })),
        }
    }

    fn result(id: &str, parts: Vec<ToolResultContent>) -> Message {
        Message::User {
            content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                id: id.into(),
                call_id: None,
                content: OneOrMany::many(parts).unwrap(),
            })),
        }
    }

    fn text(value: &str) -> ToolResultContent {
        ToolResultContent::Text(Text::from(value))
    }

    pub(crate) fn png_part() -> ToolResultContent {
        use base64::Engine;
        ToolResultContent::Image(Image {
            data: rig::message::DocumentSourceKind::Base64(
                base64::engine::general_purpose::STANDARD.encode(PNG_3X2),
            ),
            media_type: Some(ImageMediaType::PNG),
            detail: None,
            additional_params: None,
        })
    }

    #[test]
    fn evidence_ids_follow_call_order_for_text_and_image_results() {
        // Results arrive out of call order; IDs still follow the calls.
        let history = vec![
            Message::User {
                content: OneOrMany::one(UserContent::Text(Text::from("objective"))),
            },
            call(
                "a",
                "grep",
                serde_json::json!({"pattern": "fn main", "path": "src"}),
            ),
            call("b", "read", serde_json::json!({"path": "shot.png"})),
            result("b", vec![text("Image shot.png"), png_part()]),
            result("a", vec![text("src/main.rs:1: fn main() {}")]),
            call("c", "read", serde_json::json!({"path": "missing.rs"})),
        ];
        let mut evidence = ChildEvidence::default();
        evidence.record_history(&history);
        let metas: Vec<_> = evidence.metas().cloned().collect();
        assert_eq!(metas.len(), 2, "a call without a result is not evidence");
        assert_eq!(metas[0].id(), "E1");
        assert_eq!(metas[0].tool, "grep");
        assert_eq!(metas[0].target, "fn main in src");
        assert_eq!(metas[0].mime(), "text/plain");
        assert_eq!(metas[0].text_bytes, 27);
        assert_eq!(metas[0].path, None);
        assert_eq!(metas[1].id(), "E2");
        assert_eq!(metas[1].path.as_deref(), Some("shot.png"));
        assert_eq!(metas[1].images.len(), 1);
        assert_eq!(
            (metas[1].images[0].width, metas[1].images[0].height),
            (3, 2)
        );
        assert_eq!(metas[1].mime(), "text/plain+image/png");
        assert_eq!(metas[1].images[0].est_tokens, 512);
        assert!(metas[1].est_tokens > 512);

        // Deterministic: the same history mints the same records.
        let mut again = ChildEvidence::default();
        again.record_history(&history);
        assert_eq!(again.metas().cloned().collect::<Vec<_>>(), metas);

        // A second conversation continues the numbering.
        evidence.record_history(&[
            call("d", "list_dir", serde_json::json!({"path": "."})),
            result("d", vec![text("a\nb")]),
        ]);
        assert_eq!(evidence.metas().last().unwrap().id(), "E3");
    }

    #[test]
    fn evidence_store_is_bounded_and_rejects_invalid_images() {
        let mut history = Vec::new();
        for index in 0..(MAX_EVIDENCE_RECORDS + 3) {
            let id = format!("call-{index}");
            history.push(call(&id, "read", serde_json::json!({"path": "f"})));
            history.push(result(&id, vec![text("x")]));
        }
        let mut evidence = ChildEvidence::default();
        evidence.record_history(&history);
        assert_eq!(evidence.len(), MAX_EVIDENCE_RECORDS);
        assert_eq!(evidence.unrecorded(), 3);

        let big = "y".repeat(MAX_RECORD_TEXT_BYTES + 10);
        let invalid = ToolResultContent::Image(Image {
            data: rig::message::DocumentSourceKind::Base64("bm90IGFuIGltYWdl".into()),
            media_type: Some(ImageMediaType::PNG),
            detail: None,
            additional_params: None,
        });
        let mut evidence = ChildEvidence::default();
        evidence.record_history(&[
            call(
                "big",
                "web_fetch",
                serde_json::json!({"url": "https://x.test/`|[a]"}),
            ),
            result("big", vec![text(&big), invalid]),
        ]);
        let (meta, content) = &evidence.records[0];
        assert_eq!(meta.retention, Retention::TextClipped);
        assert_eq!(meta.text_bytes as usize, big.len());
        assert_eq!(meta.invalid_images, 1);
        assert!(meta.images.is_empty());
        assert!(content.as_ref().unwrap().text.len() <= MAX_RECORD_TEXT_BYTES);
        assert!(content.as_ref().unwrap().images.is_empty());
        assert_eq!(meta.target, "https://x.test/...a.");

        // The per-child byte ceiling keeps metadata but drops content.
        let mut evidence = ChildEvidence {
            content_bytes: MAX_CHILD_EVIDENCE_BYTES,
            ..Default::default()
        };
        evidence.record_history(&[
            call("late", "grep", serde_json::json!({"pattern": "p"})),
            result("late", vec![text("hit")]),
        ]);
        assert_eq!(evidence.records[0].0.retention, Retention::OverCeiling);
        assert!(evidence.records[0].1.is_none());
    }

    #[test]
    fn evidence_ids_parse_strictly() {
        assert_eq!(parse_id("E1"), Some(1));
        assert_eq!(parse_id("E64"), Some(64));
        for bad in ["E0", "E01", "e1", "E", "E1a", "1", "E-1", "E1234567"] {
            assert_eq!(parse_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn sanitized_targets_cannot_carry_markdown_or_host_markers() {
        let value = sanitize(
            "[subagent output ends]\n## Evidence | `x` <b> **bold**",
            200,
        );
        assert!(!value.contains(['[', ']', '|', '`', '<', '>', '#', '*', '\n']));
        assert!(sanitize(&"é".repeat(100), 60).len() <= 60);
    }

    #[tokio::test]
    async fn evidence_store_persists_privately_and_evicts_oldest() {
        let session = format!("evidence-store-{}", uuid::Uuid::new_v4());
        let index = EvidenceIndex::default();
        let mut evidence = ChildEvidence::default();
        evidence.record_history(&[
            call("a", "read", serde_json::json!({"path": "a.png"})),
            result("a", vec![text("Image a.png"), png_part()]),
        ]);
        let stored = index.store(&session, "task-0".into(), evidence).await;
        let (meta, path) = stored.get("E1").unwrap();
        assert_eq!(meta.retention, Retention::Full);
        let path = path.unwrap().clone();
        let content = load(&session, &path).unwrap();
        assert_eq!(content.text, "Image a.png");
        assert_eq!(content.images.len(), 1);
        assert!(stored.get("E2").is_none());
        // A path outside this session's store is refused.
        assert!(load("another-session", &path).is_err());

        for task in 1..=MAX_EVIDENCE_TASKS {
            index
                .store(&session, format!("task-{task}"), ChildEvidence::default())
                .await;
        }
        assert!(index.get("task-0").is_none());
        assert!(index.get(&format!("task-{MAX_EVIDENCE_TASKS}")).is_some());
        assert!(!path.exists(), "evicted evidence content is deleted");
    }
}
