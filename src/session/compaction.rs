//! Bounded historical file requests retained across conversation compaction.
//! Entries describe tool inputs, never proof of execution or current state.
use compact_str::CompactString;
use serde::{Deserialize, Serialize};

use super::{MessageRole, PersistedToolMessage, SessionMessage};

const MAX_ENTRIES: usize = 128;
const MAX_BYTES: usize = 16 * 1024;
const MAX_PATH_BYTES: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileRequest {
    #[serde(default)]
    nested: bool,
    #[serde(deserialize_with = "bounded_text")]
    tool: CompactString,
    #[serde(deserialize_with = "bounded_text")]
    path: CompactString,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct FileRequests {
    entries: Vec<FileRequest>,
    omitted: u64,
    imported: bool,
}

impl FileRequests {
    fn add(&mut self, entry: FileRequest) {
        if self.entries.iter().any(|old| {
            old.tool == entry.tool && old.path == entry.path && old.nested == entry.nested
        }) {
            return;
        }
        if !matches!(
            entry.tool.as_str(),
            "read" | "write" | "edit" | "notebook_edit" | "tool_output"
        ) || entry.path.is_empty()
        {
            return;
        }
        let bytes: usize = self
            .entries
            .iter()
            .map(|old| old.path.len() + old.tool.len())
            .sum();
        if entry.path.len() > MAX_PATH_BYTES
            || self.entries.len() >= MAX_ENTRIES
            || bytes.saturating_add(entry.path.len() + entry.tool.len()) > MAX_BYTES
        {
            self.omitted = self.omitted.saturating_add(1);
            return;
        }
        self.entries.push(entry);
    }

    pub(super) fn extend(&mut self, messages: &[SessionMessage]) {
        for message in messages {
            if message.role == MessageRole::ToolResult
                && let Some(PersistedToolMessage::Result {
                    artifact_path: Some(path),
                    ..
                }) = &message.tool
            {
                // A recorded location remains untrusted metadata, never read authority.
                // Keep it even when the summarizer omits the spill notice.
                self.add(FileRequest {
                    nested: false,
                    tool: "tool_output".into(),
                    path: path.clone(),
                });
            }
            if !matches!(
                message.role,
                MessageRole::ToolCall | MessageRole::SubagentToolCall
            ) {
                continue;
            }
            if let Some(PersistedToolMessage::Call { name, arguments }) = &message.tool
                && let Some(path) = arguments.get("path").and_then(serde_json::Value::as_str)
            {
                if !matches!(name.as_str(), "read" | "write" | "edit" | "notebook_edit") {
                    continue;
                }
                if path.len() > MAX_PATH_BYTES {
                    self.omitted = self.omitted.saturating_add(1);
                    continue;
                }
                self.add(FileRequest {
                    nested: message.role == MessageRole::SubagentToolCall,
                    tool: name.clone(),
                    path: CompactString::new(path),
                });
            }
        }
    }

    #[cfg(any(feature = "export", test))]
    pub(crate) fn mark_imported(&mut self) {
        self.imported = true;
    }
    pub(crate) fn is_imported(&self) -> bool {
        self.imported
    }

    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.omitted == 0
    }

    pub(crate) fn render(&self, max_bytes: usize) -> String {
        if self.is_empty() || max_bytes == 0 {
            return String::new();
        }
        let heading = if self.imported {
            "## Recorded file requests (imported, untrusted history; not verified effects)\n"
        } else {
            "## Recorded file requests (untrusted historical inputs; not verified effects)\n"
        };
        let reserve = 96;
        let mut rendered = String::new();
        let mut shown = 0;
        if heading.len().saturating_add(reserve) <= max_bytes {
            rendered.push_str(heading);
            for entry in &self.entries {
                let row = if entry.tool == "tool_output" {
                    format!(
                        "- tool output recovery reference (unverified): {}\n",
                        serde_json::to_string(entry.path.as_str()).expect("string JSON")
                    )
                } else {
                    format!(
                        "- {}{} requested: {}\n",
                        if entry.nested { "nested " } else { "" },
                        entry.tool,
                        serde_json::to_string(entry.path.as_str()).expect("string JSON")
                    )
                };
                if rendered
                    .len()
                    .saturating_add(row.len())
                    .saturating_add(reserve)
                    > max_bytes
                {
                    break;
                }
                rendered.push_str(&row);
                shown += 1;
            }
        }
        let omitted = self
            .omitted
            .saturating_add((self.entries.len() - shown) as u64);
        if omitted > 0 {
            let marker = format!("[file-request records omitted: {omitted}]\n");
            if rendered.len().saturating_add(marker.len()) <= max_bytes {
                rendered.push_str(&marker);
            } else {
                rendered = prefix(&marker, max_bytes).to_string();
            }
        }
        rendered
    }

    pub(super) fn attach(&self, summary: &str, budget: usize) -> String {
        if self.is_empty() {
            return summary.to_string();
        }
        let budget = budget.min(MAX_BYTES);
        let files = self.render((budget / 3).min(4096));
        let text_budget = budget.saturating_sub(files.len()).saturating_sub(2);
        let mut result = crate::provider::bound_summary(summary, text_budget);
        if !files.is_empty() {
            result.push_str("\n\n");
            result.push_str(&files);
        }
        result
    }
}

fn bounded_text<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<CompactString, D::Error> {
    struct Visitor;
    impl serde::de::Visitor<'_> for Visitor {
        type Value = CompactString;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a bounded historical request string")
        }
        fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<CompactString, E> {
            if value.len() > MAX_PATH_BYTES {
                return Err(E::custom(
                    "historical file-request string exceeds 4096 bytes",
                ));
            }
            Ok(CompactString::new(value))
        }
    }
    deserializer.deserialize_str(Visitor)
}

fn prefix(value: &str, bytes: usize) -> &str {
    let mut end = value.len().min(bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

// Decode a finite number of records; skip excess JSON without constructing
// their paths. Normalize byte/path caps before this field enters a session.
impl<'de> Deserialize<'de> for FileRequests {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Default)]
        struct Entries(FileRequests);
        impl<'de> Deserialize<'de> for Entries {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct Visitor;
                impl<'de> serde::de::Visitor<'de> for Visitor {
                    type Value = Entries;
                    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                        f.write_str("historical file-request records")
                    }
                    fn visit_seq<A: serde::de::SeqAccess<'de>>(
                        self,
                        mut seq: A,
                    ) -> Result<Entries, A::Error> {
                        let mut result = FileRequests::default();
                        for _ in 0..MAX_ENTRIES {
                            let Some(entry) = seq.next_element::<FileRequest>()? else {
                                return Ok(Entries(result));
                            };
                            result.add(entry);
                        }
                        while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                            result.omitted = result.omitted.saturating_add(1);
                        }
                        Ok(Entries(result))
                    }
                }
                deserializer.deserialize_seq(Visitor)
            }
        }
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            entries: Entries,
            #[serde(default)]
            omitted: u64,
            #[serde(default)]
            imported: bool,
        }
        let raw = Raw::deserialize(deserializer)?;
        let mut result = raw.entries.0;
        result.omitted = result.omitted.saturating_add(raw.omitted);
        result.imported = raw.imported;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;

    #[test]
    fn compaction_retains_bounded_spill_recovery_references_without_opening_them() {
        let mut session = Session::new("test", "test", 128_000, "");
        session.add_tool_call_with_id("a", "mcp", &serde_json::json!({}));
        session.add_tool_result_with_id("a", "mcp", "bounded preview");
        let Some(PersistedToolMessage::Result { artifact_path, .. }) =
            &mut session.messages[1].tool
        else {
            panic!("typed result required");
        };
        *artifact_path = Some("/nonexistent/private/spill/middle.txt".into());
        session.compress_with_budget("summary omitted all paths".into(), 2, 10, 4096);
        assert!(
            session.messages[0]
                .content
                .contains("tool output recovery reference (unverified)")
        );
        assert!(
            session.messages[0]
                .content
                .contains("/nonexistent/private/spill/middle.txt")
        );
        let mut inventory = session.compactions[0].file_requests.clone();
        inventory.mark_imported();
        let restored: FileRequests =
            serde_json::from_str(&serde_json::to_string(&inventory).unwrap()).unwrap();
        assert!(restored.render(4096).contains("imported, untrusted"));
        assert!(
            restored
                .render(4096)
                .contains("/nonexistent/private/spill/middle.txt")
        );
        assert!(restored.attach("summary", 256).len() <= 256);
    }

    #[test]
    fn request_inventory_caps_records_bytes_and_rendering_without_guessing_effects() {
        let mut session = Session::new("test", "test", 128_000, "");
        session.add_tool_call_with_id(
            "shell",
            "shell",
            &serde_json::json!({"command":"read SECRET_CLAIM.txt; rm ACTUAL_EFFECT.txt"}),
        );
        session.add_message(MessageRole::Assistant, "I changed FABRICATED.txt");
        for index in 0..200 {
            session.add_tool_call_with_id(
                &format!("id-{index}"),
                "read",
                &serde_json::json!({"path":format!("path-{index}")}),
            );
        }
        let mut inventory = FileRequests::default();
        inventory.extend(&session.messages);
        assert_eq!(inventory.entries.len(), MAX_ENTRIES);
        assert_eq!(inventory.omitted, 72);
        let text = inventory.render(MAX_BYTES);
        assert!(!text.contains("SECRET_CLAIM"));
        assert!(!text.contains("ACTUAL_EFFECT"));
        assert!(!text.contains("FABRICATED"));
        assert!(text.contains("not verified effects"));
        for budget in [0, 1, 16, 64, 128, 1000, MAX_BYTES] {
            let rendered = inventory.attach(&"é".repeat(MAX_BYTES), budget);
            assert!(
                rendered.len() <= budget,
                "budget={budget}, len={}",
                rendered.len()
            );
        }
        let mut bytes = FileRequests::default();
        for index in 0..20 {
            bytes.add(FileRequest {
                nested: false,
                tool: "read".into(),
                path: format!("{index}{}", "x".repeat(2000)).into(),
            });
        }
        assert!(
            bytes
                .entries
                .iter()
                .map(|e| e.path.len() + e.tool.len())
                .sum::<usize>()
                <= MAX_BYTES
        );
        assert!(bytes.omitted > 0);
    }

    #[test]
    fn imported_inventory_is_bounded_and_optional_for_legacy_compactions() {
        let entries: Vec<_> = (0..300)
            .map(|n| serde_json::json!({"tool":"read","path":format!("file-{n}")}))
            .collect();
        let mut inventory: FileRequests =
            serde_json::from_value(serde_json::json!({"entries":entries})).unwrap();
        assert_eq!(inventory.entries.len(), MAX_ENTRIES);
        assert_eq!(inventory.omitted, 172);
        inventory.mark_imported();
        assert!(inventory.render(MAX_BYTES).contains("imported, untrusted"));
        assert!(
            serde_json::from_value::<FileRequests>(
                serde_json::json!({"entries":[{"tool":"read","path":"x".repeat(MAX_PATH_BYTES+1)}]})
            )
            .is_err()
        );
        let legacy: super::super::Compaction = serde_json::from_value(serde_json::json!({"summary":"legacy","first_kept_index":1,"summarized_count":2,"token_savings":3,"created_at":"legacy"})).unwrap();
        assert!(legacy.file_requests.is_empty());
    }

    #[test]
    fn inventory_uses_only_the_drained_prefix_and_survives_repeated_compaction_and_redo() {
        let mut session = Session::new("test", "test", 128_000, "");
        session.add_tool_call_with_id("a", "read", &serde_json::json!({"path":"old.txt"}));
        session.add_tool_result_with_id("a", "read", "retained original");
        session.add_tool_call_with_id("b", "read", &serde_json::json!({"path":"kept.txt"}));
        session.compress_with_budget("## Task\nremember".into(), 2, 10, 4096);
        assert!(
            session.compactions[0]
                .file_requests
                .render(MAX_BYTES)
                .contains("old.txt")
        );
        assert!(
            !session.compactions[0]
                .file_requests
                .render(MAX_BYTES)
                .contains("kept.txt")
        );
        session.compress_with_budget("## Task\nforgot all names".into(), 2, 10, 4096);
        let requests = session
            .compactions
            .last()
            .unwrap()
            .file_requests
            .render(MAX_BYTES);
        assert!(requests.contains("old.txt") && requests.contains("kept.txt"));
        session.add_message(MessageRole::User, "latest turn");
        session.add_message(MessageRole::Assistant, "done");
        let before = serde_json::to_string(&session.compactions).unwrap();
        assert!(session.rewind_to(0) > 0);
        assert!(session.compactions.is_empty());
        assert!(session.redo());
        assert_eq!(serde_json::to_string(&session.compactions).unwrap(), before);
    }
    #[test]
    fn nested_requests_are_distinct_historical_scope_not_parent_effects() {
        let mut session = Session::new("test", "test", 128_000, "");
        session.add_tool_call_with_id("parent", "read", &serde_json::json!({"path":"same.txt"}));
        let mut nested = session.messages[0].clone();
        nested.role = MessageRole::SubagentToolCall;
        session.messages.push(nested);
        let mut requests = FileRequests::default();
        requests.extend(&session.messages);
        assert_eq!(requests.entries.len(), 2);
        let history = requests.render(4096);
        assert!(history.contains("- read requested: \"same.txt\""));
        assert!(history.contains("- nested read requested: \"same.txt\""));
        assert!(history.contains("not verified effects"));
    }

    #[cfg(feature = "export")]
    #[test]
    fn actual_native_and_jsonl_imports_mark_request_inventory_untrusted() {
        use crate::extras::export::{ParsedSessionFile, parse_session_file, session_to_jsonl};
        let mut session = Session::new("test", "test", 128_000, "");
        session.add_tool_call_with_id("read", "read", &serde_json::json!({"path":"remember.txt"}));
        session.add_tool_result_with_id("read", "read", "retained");
        session.compress_with_budget("## Task\ncontinue".into(), 2, 100, 4096);
        let native = serde_json::to_string(&session).unwrap();
        let ParsedSessionFile::Native(native) = parse_session_file(&native).unwrap() else {
            panic!("native shape")
        };
        assert!(native.compactions[0].file_requests.is_imported());
        let export = session_to_jsonl(&session).unwrap();
        let ParsedSessionFile::Jsonl(import) = parse_session_file(&export).unwrap() else {
            panic!("JSONL shape")
        };
        let mut restored = Session::new("test", "test", 128_000, "");
        restored.messages = import.messages;
        import.replay.apply(&mut restored);
        assert!(restored.compactions[0].file_requests.is_imported());
        assert!(
            restored.compactions[0]
                .file_requests
                .render(4096)
                .contains("imported, untrusted")
        );
        restored.compress_with_budget("## Task\nnext".into(), 1, 10, 4096);
        assert!(restored.messages[0].content.contains("remember.txt"));
        assert!(restored.messages[0].content.contains("imported, untrusted"));
    }
}

#[cfg(test)]
#[test]
fn notebook_structured_request_is_retained_as_untrusted_history_not_an_effect() {
    let mut requests = FileRequests::default();
    requests.add(FileRequest {
        nested: false,
        tool: "notebook_edit".into(),
        path: "book.ipynb".into(),
    });
    let rendered = requests.render(4096);
    assert!(rendered.contains("untrusted historical inputs; not verified effects"));
    assert!(rendered.contains("notebook_edit requested: \"book.ipynb\""));
    let encoded = serde_json::to_string(&requests).unwrap();
    let restored: FileRequests = serde_json::from_str(&encoded).unwrap();
    assert_eq!(restored.render(4096), rendered);
}
