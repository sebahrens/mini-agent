//! Read-only context composition from captured preamble and retained history.
use std::io::{self, Write};

use crate::agent::builder::PreambleSnapshot;
use crate::config::Config;
use crate::session::{MessageRole, PersistedToolMessage, Session};

const MAX_SCHEMAS: usize = 128;
const MAX_SCHEMA_BYTES: usize = 1024 * 1024;
const MAX_SCHEMA_NODES: usize = 16_384;
const MAX_SCHEMA_DEPTH: usize = 64;
const MAX_TOP_RESULTS: usize = 5;
const MAX_LABEL_BYTES: usize = 96;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolMetadata {
    Known { count: usize, tokens: u64 },
    Unavailable(&'static str),
}

#[derive(Debug)]
pub(crate) struct ContextRow {
    pub(crate) label: &'static str,
    pub(crate) tokens: i128,
}

#[derive(Debug)]
pub(crate) struct ContextDiagnostic {
    pub(crate) rows: Vec<ContextRow>,
    pub(crate) total: u64,
    estimated: bool,
    source: &'static str,
    schemas: ToolMetadata,
    preamble_bytes: Option<usize>,
    preamble_parts: Option<usize>,
    largest: Vec<(u64, String, String)>,
    live_results: usize,
    compactions: usize,
    has_summary: bool,
    window: u64,
    threshold: u64,
    projected: u64,
    clearing: usize,
    compact_enabled: bool,
    mid_turn_threshold: Option<u64>,
    instructions: Option<crate::context::instructions::InstructionDiagnostics>,
    instruction_state: &'static str,
}

impl ContextDiagnostic {
    pub(crate) fn capture(
        session: &Session,
        cfg: &Config,
        memory_block: Option<&str>,
        actual: Option<&PreambleSnapshot>,
        schemas: ToolMetadata,
    ) -> Self {
        let total = session.effective_context_tokens();
        let parts = actual
            .map(|value| value.parts.as_slice())
            .unwrap_or(&session.overhead_parts);
        let mut rows = Vec::new();
        if parts.is_empty() {
            rows.push(ContextRow {
                label: "Preamble (unclassified snapshot)",
                tokens: i128::from(session.overhead_tokens),
            });
        } else {
            rows.extend(parts.iter().take(32).map(|(label, tokens)| ContextRow {
                label,
                tokens: i128::from(*tokens),
            }));
            if parts.len() > 32 {
                rows.push(ContextRow {
                    label: "Other captured preamble",
                    tokens: parts[32..]
                        .iter()
                        .map(|(_, tokens)| i128::from(*tokens))
                        .sum(),
                });
            }
        }
        if let ToolMetadata::Known { tokens, .. } = schemas {
            rows.push(ContextRow {
                label: "Registered tool definitions",
                tokens: i128::from(tokens),
            });
        }
        let (summary, first_live) = session.compacted_context();
        let mut categories = [0i128; 6];
        let mut top = Vec::<(u64, usize)>::new();
        let mut live_results = 0;
        for (index, message) in session.messages.iter().enumerate() {
            let category = match message.role {
                MessageRole::System if summary.is_some() && index + 1 == first_live => 0,
                MessageRole::System => 1,
                MessageRole::User | MessageRole::Assistant => 2,
                MessageRole::ToolCall => 3,
                MessageRole::ToolResult => 4,
                MessageRole::SubagentToolCall => 5,
            };
            categories[category] += i128::from(message.estimated_tokens);
            if index >= first_live && message.role == MessageRole::ToolResult {
                live_results += 1;
                top.push((message.estimated_tokens, index));
                top.sort_unstable_by(|left, right| {
                    right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1))
                });
                top.truncate(MAX_TOP_RESULTS);
            }
        }
        for (label, tokens) in [
            "Saved compaction summary",
            "Conversation system messages",
            "Conversation user/assistant",
            "Tool calls",
            "Retained tool results",
            "Nested child call records",
        ]
        .into_iter()
        .zip(categories)
        {
            rows.push(ContextRow { label, tokens });
        }
        let classified: i128 = rows.iter().map(|row| row.tokens).sum();
        rows.push(ContextRow {
            label: "Calibration/tokenizer adjustment",
            tokens: i128::from(total) - classified,
        });
        let largest = top
            .into_iter()
            .map(|(tokens, index)| {
                let id = session.messages[index].tool_call_id.as_deref();
                let name = id
                    .and_then(|id| {
                        session.messages[..index].iter().rev().find_map(|call| {
                            if call.tool_call_id.as_deref() != Some(id) {
                                return None;
                            }
                            match &call.tool {
                                Some(PersistedToolMessage::Call { name, .. }) => {
                                    Some(name.as_str())
                                }
                                _ => None,
                            }
                        })
                    })
                    .unwrap_or("unknown tool");
                (
                    tokens,
                    bounded_label(name),
                    bounded_label(id.unwrap_or("unrecorded id")),
                )
            })
            .collect();
        let reserve = cfg.resolve_reserve_tokens(
            &session.model,
            crate::config::quick_models_map(cfg),
            session.context_window,
        );
        #[cfg(feature = "memory")]
        let reserve =
            crate::extras::memory::effective_session_reserve(reserve, memory_block, session);
        #[cfg(not(feature = "memory"))]
        let _ = memory_block;
        let keep = cfg.resolve_keep_recent_tool_results();
        let mid_turn_threshold = cfg
            .mid_turn_compact_threshold
            .filter(|value| value.is_finite() && *value > 0.0 && *value <= 1.0)
            .map(|value| (session.context_window as f64 * value) as u64);
        Self {
            rows,
            total,
            estimated: session.ctx_is_estimated(),
            source: if actual.is_some() {
                "actual built-agent preamble snapshot"
            } else {
                "last captured cold preamble estimate; refreshed at agent build"
            },
            schemas,
            preamble_bytes: actual.map(|value| value.bytes),
            preamble_parts: actual.map(|value| value.ranges.len()),
            largest,
            live_results,
            compactions: session.compaction_count(),
            has_summary: summary.is_some(),
            window: session.context_window,
            threshold: session.context_window.saturating_sub(reserve),
            projected: session.effective_context_tokens_after_tool_result_pruning(keep),
            clearing: session.tool_results_cleared_for_retention(keep),
            compact_enabled: cfg.compact_enabled.unwrap_or(false),
            mid_turn_threshold,
            instructions: actual
                .and_then(|snapshot| snapshot.instruction_sources.as_ref())
                .and_then(|service| service.diagnostics()),
            instruction_state: match actual {
                None => "unavailable (cold agent)",
                Some(snapshot) if snapshot.instruction_sources.is_none() => "disabled",
                Some(_) => "busy (snapshot metadata unavailable)",
            },
        }
    }

    pub(crate) fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut text = String::with_capacity(4096);
        writeln!(
            text,
            "Context composition: {} tokens / {} window ({})",
            self.total,
            self.window,
            if self.estimated {
                "cold estimate"
            } else {
                "provider-calibrated anchor"
            }
        )
        .unwrap();
        writeln!(text, "Preamble source: {}", self.source).unwrap();
        if let (Some(bytes), Some(parts)) = (self.preamble_bytes, self.preamble_parts) {
            writeln!(
                text,
                "Captured preamble: {bytes} bytes in {parts} exact ranges"
            )
            .unwrap();
        }
        if let Some(instructions) = &self.instructions {
            writeln!(text, "Workspace instruction snapshots: {} sources; {} newly retained bytes; {} load failures; {} notice results",
                instructions.source_count, instructions.retained_bytes,
                instructions.failed_directories + usize::from(instructions.initial_failed),
                instructions.delivered_notices).unwrap();
            for (path, scope, project, bytes) in &instructions.sources {
                writeln!(
                    text,
                    "  {}: {} ({} bytes; scope={})",
                    if *project { "workspace" } else { "global" },
                    bounded_label(&path.to_string_lossy()),
                    bytes,
                    bounded_label(&scope.to_string_lossy())
                )
                .unwrap();
            }
            if instructions.source_count > instructions.sources.len() {
                writeln!(
                    text,
                    "  {} more captured sources omitted",
                    instructions.source_count - instructions.sources.len()
                )
                .unwrap();
            }
            writeln!(text,"Snapshot metadata only; loaded sources and notice counts are not runner delivery acknowledgements.").unwrap();
        } else {
            writeln!(
                text,
                "Workspace instruction snapshots: {}",
                self.instruction_state
            )
            .unwrap();
        }
        writeln!(
            text,
            "Section                              est. tokens   % of context"
        )
        .unwrap();
        for row in &self.rows {
            let percent = if self.total == 0 {
                0.0
            } else {
                row.tokens as f64 * 100.0 / self.total as f64
            };
            if row.label == "Calibration/tokenizer adjustment" {
                writeln!(
                    text,
                    "{:<36} {:>11} {percent:>13.1}%",
                    row.label,
                    format!("{:+}", row.tokens)
                )
                .unwrap();
            } else {
                writeln!(
                    text,
                    "{:<36} {:>11} {percent:>13.1}%",
                    row.label, row.tokens
                )
                .unwrap();
            }
        }
        writeln!(
            text,
            "{:<36} {:>11}",
            "TOTAL (matches current context)", self.total
        )
        .unwrap();
        match self.schemas {
            ToolMetadata::Known { count, .. } => writeln!(
                text,
                "Tool schemas: {count} already-registered definitions; serialized metadata estimate"
            )
            .unwrap(),
            ToolMetadata::Unavailable(reason) => writeln!(
                text,
                "Tool schemas: unavailable ({reason}); excluded from section estimates"
            )
            .unwrap(),
        }
        writeln!(text, "Section estimates are not billed usage. Signed adjustment reconciles provider/tokenizer differences, section rounding and cold schema accounting.").unwrap();
        writeln!(
            text,
            "Compactions: {}; active saved summary: {}",
            self.compactions,
            if self.has_summary { "yes" } else { "no" }
        )
        .unwrap();
        writeln!(
            text,
            "Next-request projection after pruning: {} tokens; {} old tool results replaced",
            self.projected, self.clearing
        )
        .unwrap();
        writeln!(
            text,
            "Compaction threshold: > {} tokens; automatic compaction: {}",
            self.threshold,
            if self.compact_enabled { "on" } else { "off" }
        )
        .unwrap();
        match self.mid_turn_threshold {
            Some(tokens) => writeln!(text, "Configured mid-turn threshold: {tokens} prompt tokens (requires automatic compaction)").unwrap(),
            None => writeln!(text, "Mid-turn compaction: off").unwrap(),
        }
        writeln!(text, "Largest live retained tool results (up to {MAX_TOP_RESULTS} of {}; estimates, no output content):", self.live_results).unwrap();
        for (tokens, name, id) in &self.largest {
            writeln!(text, "  {tokens:>8}  {name}  id={id}").unwrap();
        }
        if self.largest.is_empty() {
            writeln!(text, "  none").unwrap();
        }
        text
    }
}

fn bounded_label(text: &str) -> String {
    // Bound before sanitization, then bound expansion as well. Never turn
    // a provider-owned id/name into multiple diagnostic rows or terminal code.
    let mut end = text.len().min(MAX_LABEL_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let safe = crate::ui::events::sanitize_for_review(&text[..end]).replace('\n', "\\n");
    let mut safe_end = safe.len().min(MAX_LABEL_BYTES);
    while !safe.is_char_boundary(safe_end) {
        safe_end -= 1;
    }
    let truncated = end < text.len() || safe_end < safe.len();
    format!("{}{}", &safe[..safe_end], if truncated { "…" } else { "" })
}

struct BoundedEncoding(Vec<u8>);
impl Write for BoundedEncoding {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > MAX_SCHEMA_BYTES {
            return Err(io::Error::other("schema metadata exceeds bound"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn schema_shape_bounded(value: &serde_json::Value) -> bool {
    let mut pending = vec![(value, 0usize)];
    let mut nodes = 0usize;
    while let Some((value, depth)) = pending.pop() {
        nodes += 1;
        if nodes > MAX_SCHEMA_NODES || depth > MAX_SCHEMA_DEPTH {
            return false;
        }
        let children: Vec<_> = match value {
            serde_json::Value::Object(object) => {
                if object.len() > MAX_SCHEMA_NODES.saturating_sub(nodes + pending.len()) {
                    return false;
                }
                object.values().collect()
            }
            serde_json::Value::Array(array) => {
                if array.len() > MAX_SCHEMA_NODES.saturating_sub(nodes + pending.len()) {
                    return false;
                }
                array.iter().collect()
            }
            serde_json::Value::String(string) if string.len() > MAX_SCHEMA_BYTES => return false,
            _ => Vec::new(),
        };
        pending.extend(children.into_iter().map(|value| (value, depth + 1)));
    }
    true
}

pub(crate) fn metadata_estimate(definitions: &[rig::completion::ToolDefinition]) -> ToolMetadata {
    if definitions.len() > MAX_SCHEMAS
        || definitions.iter().any(|definition| {
            definition.name.len() > MAX_SCHEMA_BYTES
                || definition.description.len() > MAX_SCHEMA_BYTES
                || !schema_shape_bounded(&definition.parameters)
        })
    {
        return ToolMetadata::Unavailable("registered metadata exceeds diagnostic bounds");
    }
    if definitions.is_empty() {
        return ToolMetadata::Known {
            count: 0,
            tokens: 0,
        };
    }
    let mut output = BoundedEncoding(Vec::new());
    if serde_json::to_writer(&mut output, definitions).is_err() {
        return ToolMetadata::Unavailable("registered metadata exceeds encoding bound");
    }
    let encoded = String::from_utf8(output.0).expect("JSON encoding is UTF-8");
    ToolMetadata::Known {
        count: definitions.len(),
        tokens: Session::estimate_tokens(&encoded),
    }
}

async fn bounded_existing_metadata(
    pending: impl std::future::Future<Output = anyhow::Result<Vec<rig::completion::ToolDefinition>>>,
) -> ToolMetadata {
    match tokio::time::timeout(std::time::Duration::from_millis(100), pending).await {
        Ok(Ok(definitions)) => metadata_estimate(&definitions),
        _ => ToolMetadata::Unavailable("existing metadata handle unavailable"),
    }
}

pub(crate) async fn handle(parts: &[&str], ctx: &mut super::SlashCtx<'_>) -> anyhow::Result<()> {
    if parts.len() != 1 {
        ctx.renderer.write_line("usage: /context", super::C_ERROR)?;
        return Ok(());
    }
    let schemas = match ctx.agent.as_ref() {
        None => ToolMetadata::Unavailable("agent not built; no lazy initialization performed"),
        Some(agent) => {
            bounded_existing_metadata(crate::startup::heap_future(|| {
                agent.context_tool_definitions()
            }))
            .await
        }
    };
    #[cfg(feature = "memory")]
    let memory_block = ctx.context.memory.as_deref();
    #[cfg(not(feature = "memory"))]
    let memory_block = None;
    let diagnostic = ContextDiagnostic::capture(
        ctx.session,
        ctx.cfg,
        memory_block,
        ctx.agent
            .as_ref()
            .and_then(crate::provider::AnyAgent::context_snapshot),
        schemas,
    );
    let report = diagnostic.render();
    // The whole report paints as one frame.
    ctx.renderer.frame(|renderer| {
        for line in report.lines() {
            renderer.write_line(line, super::C_RESULT)?;
        }
        Ok(())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(session: &Session) -> ContextDiagnostic {
        ContextDiagnostic::capture(
            session,
            &Config::default(),
            None,
            None,
            ToolMetadata::Unavailable("cold test"),
        )
    }

    #[test]
    fn cold_and_signed_calibration_reconcile_exactly_without_charging() {
        let mut session = Session::new("test", "test", 128_000, "");
        session.overhead_tokens = 15;
        session.overhead_parts = vec![("System prompt", 9), ("Added files", 5)];
        session.add_message(MessageRole::User, "several ordinary words");
        let before = serde_json::to_vec(&session).unwrap();
        let cold = report(&session);
        assert_eq!(
            cold.rows.iter().map(|row| row.tokens).sum::<i128>(),
            i128::from(session.effective_context_tokens())
        );
        assert_eq!(cold.rows.last().unwrap().tokens, 1);
        assert!(cold.render().contains("cold estimate"));
        session.calibrated_tokens = 3;
        session.calibrated_msg_count = session.messages.len();
        let actual = PreambleSnapshot {
            tokens: 15,
            parts: session.overhead_parts.clone(),
            ranges: vec![("System prompt", 0..1)],
            bytes: 1,
            instruction_sources: None,
        };
        let calibrated = ContextDiagnostic::capture(
            &session,
            &Config::default(),
            None,
            Some(&actual),
            ToolMetadata::Known {
                count: 2,
                tokens: 90,
            },
        );
        assert!(calibrated.rows.last().unwrap().tokens < 0);
        assert_eq!(
            calibrated.rows.iter().map(|row| row.tokens).sum::<i128>(),
            3
        );
        assert!(calibrated.render().contains("provider-calibrated anchor"));
        assert!(calibrated.render().contains("not billed usage"));
        session.calibrated_tokens = 0;
        session.calibrated_msg_count = 0;
        assert_eq!(serde_json::to_vec(&session).unwrap(), before);
        let mut untrusted: serde_json::Value = serde_json::from_slice(&before).unwrap();
        untrusted["overhead_parts"] = serde_json::json!([["FORGED_LABEL", 999999]]);
        let imported: Session = serde_json::from_value(untrusted).unwrap();
        assert!(
            imported.overhead_parts.is_empty(),
            "runtime snapshot cannot be supplied by imported session fields"
        );
    }

    #[cfg(feature = "memory")]
    #[test]
    fn excessive_configured_reserve_with_captured_memory_saturates_read_only() {
        let session = Session::new("test", "test", 128_000, "");
        let before = serde_json::to_vec(&session).unwrap();
        let cfg = Config {
            reserve_tokens: Some(u64::MAX),
            ..Default::default()
        };
        let report = ContextDiagnostic::capture(
            &session,
            &cfg,
            Some("captured memory"),
            None,
            ToolMetadata::Unavailable("cold test"),
        );
        assert_eq!(report.threshold, 0);
        assert!(report.render().contains("Context composition:"));
        assert_eq!(serde_json::to_vec(&session).unwrap(), before);
        let normal_cfg = Config {
            reserve_tokens: Some(100),
            ..Default::default()
        };
        let normal = ContextDiagnostic::capture(
            &session,
            &normal_cfg,
            Some("captured memory"),
            None,
            ToolMetadata::Unavailable("cold test"),
        );
        assert_eq!(
            normal.threshold,
            session.context_window
                - crate::extras::memory::effective_session_reserve(
                    100,
                    Some("captured memory"),
                    &session
                )
        );
    }

    #[test]
    fn live_top_results_are_bounded_sanitized_and_never_expand_hidden_redo_or_spill() {
        let mut session = Session::new("test", "test", 128_000, "");
        for index in 0..10 {
            let id = format!("id-{index}\n\u{202e}{}", "界".repeat(100));
            session.add_tool_call_with_id(
                &id,
                "read\u{202e}\x1b[31mNAME",
                &serde_json::json!({"path":"private"}),
            );
            session.add_tool_result_with_id_and_artifact(
                &id,
                "read",
                &format!("PRIVATE_RESULT_{index}{}", "x".repeat(index * 60)),
            );
            if let Some(PersistedToolMessage::Result { artifact_path, .. }) =
                &mut session.messages.last_mut().unwrap().tool
            {
                *artifact_path = Some("/unreadable/private-spill".into());
            }
        }
        session.add_tool_call_with_id("hidden", "HIDDEN_REDO_TOOL", &serde_json::json!({}));
        session.add_tool_result_with_id("hidden", "read", &"HIDDEN_REDO_OUTPUT".repeat(100));
        session.rewind_to(20);
        let before = serde_json::to_vec(&session).unwrap();
        let diagnostic = report(&session);
        assert_eq!(diagnostic.largest.len(), MAX_TOP_RESULTS);
        assert!(
            diagnostic
                .largest
                .windows(2)
                .all(|pair| pair[0].0 >= pair[1].0)
        );
        let text = diagnostic.render();
        assert!(text.contains("<U+202E>"));
        assert!(!text.contains('\u{202e}') && !text.contains('\x1b'));
        assert!(
            !text.contains("PRIVATE_RESULT_")
                && !text.contains("HIDDEN_REDO")
                && !text.contains("/unreadable/private-spill")
        );
        assert!(text.len() < 16 * 1024);
        assert!(
            diagnostic
                .largest
                .iter()
                .all(|(_, name, id)| name.len() <= MAX_LABEL_BYTES + 3
                    && id.len() <= MAX_LABEL_BYTES + 3)
        );
        assert_eq!(
            diagnostic.projected,
            session.effective_context_tokens_after_tool_result_pruning(
                Config::default().resolve_keep_recent_tool_results()
            )
        );
        assert_eq!(serde_json::to_vec(&session).unwrap(), before);
        assert!(
            session.redo(),
            "inspection must preserve hidden redo ownership"
        );
    }

    #[test]
    fn metadata_encoding_has_shape_count_and_final_byte_bounds_without_partial_claims() {
        let definition = rig::completion::ToolDefinition {
            name: "read".into(),
            description: "Read known data".into(),
            parameters: serde_json::json!({"type":"object","properties":{"path":{"type":"string"}}}),
        };
        let expected = Session::estimate_tokens(
            &serde_json::to_string(std::slice::from_ref(&definition)).unwrap(),
        );
        assert_eq!(
            metadata_estimate(std::slice::from_ref(&definition)),
            ToolMetadata::Known {
                count: 1,
                tokens: expected
            }
        );
        assert_eq!(
            metadata_estimate(&[]),
            ToolMetadata::Known {
                count: 0,
                tokens: 0
            }
        );
        assert!(matches!(
            metadata_estimate(&vec![definition.clone(); MAX_SCHEMAS + 1]),
            ToolMetadata::Unavailable(_)
        ));
        let mut too_large = definition.clone();
        too_large.description = "\u{0001}".repeat(MAX_SCHEMA_BYTES / 2);
        assert!(
            matches!(
                metadata_estimate(&[too_large]),
                ToolMetadata::Unavailable(_)
            ),
            "JSON escaping must count toward the final byte cap"
        );
        let mut deep = serde_json::json!(null);
        for _ in 0..MAX_SCHEMA_DEPTH + 1 {
            deep = serde_json::json!([deep]);
        }
        let mut definition = definition;
        definition.parameters = deep;
        assert!(matches!(
            metadata_estimate(&[definition]),
            ToolMetadata::Unavailable(_)
        ));
        let empty = report(&Session::new("test", "test", 0, "")).render();
        assert!(!empty.contains("NaN") && !empty.contains("inf"));
    }

    #[tokio::test]
    async fn metadata_timeout_drops_the_borrowed_read_and_errors_expose_no_details() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        struct ReadLifetime(Arc<AtomicBool>);
        impl Drop for ReadLifetime {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let lifetime = ReadLifetime(dropped.clone());
        let started = std::time::Instant::now();
        let timed_out = bounded_existing_metadata(async move {
            let _lifetime = lifetime;
            std::future::pending().await
        })
        .await;
        assert_eq!(
            timed_out,
            ToolMetadata::Unavailable("existing metadata handle unavailable")
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(
            dropped.load(Ordering::SeqCst),
            "metadata read must be dropped before returning"
        );
        let failed =
            bounded_existing_metadata(async { Err(anyhow::anyhow!("PRIVATE_ERROR_DETAIL")) }).await;
        let shown = ContextDiagnostic::capture(
            &Session::new("test", "test", 0, ""),
            &Config::default(),
            None,
            None,
            failed,
        )
        .render();
        assert!(shown.contains("existing metadata handle unavailable"));
        assert!(!shown.contains("PRIVATE_ERROR_DETAIL"));
        assert_eq!(
            bounded_existing_metadata(async { Ok(Vec::new()) }).await,
            ToolMetadata::Known {
                count: 0,
                tokens: 0
            }
        );
    }

    #[test]
    fn saved_summary_and_pruning_threshold_use_live_session_contract() {
        let mut session = Session::new("test", "test", 1000, "");
        session.add_message(MessageRole::User, "first");
        session.add_message(MessageRole::Assistant, "second");
        session.add_message(MessageRole::User, "kept");
        session.compress("SUMMARY_PRIVATE_TEXT".into(), 2, 0);
        session.add_tool_call_with_id("live", "read", &serde_json::json!({}));
        session.add_tool_result_with_id("live", "read", &"private".repeat(100));
        let cfg = Config {
            reserve_tokens: Some(100),
            keep_recent_tool_results: Some(0),
            compact_enabled: Some(true),
            mid_turn_compact_threshold: Some(0.8),
            ..Default::default()
        };
        let diagnostic = ContextDiagnostic::capture(
            &session,
            &cfg,
            None,
            None,
            ToolMetadata::Unavailable("not built"),
        );
        assert!(diagnostic.has_summary);
        assert_eq!(diagnostic.compactions, 1);
        assert_eq!(diagnostic.threshold, 900);
        assert_eq!(diagnostic.mid_turn_threshold, Some(800));
        assert_eq!(diagnostic.clearing, 1);
        assert_eq!(
            diagnostic.projected,
            session.effective_context_tokens_after_tool_result_pruning(0)
        );
        assert!(!diagnostic.render().contains("SUMMARY_PRIVATE_TEXT"));
        #[cfg(feature = "memory")]
        {
            let with_memory = ContextDiagnostic::capture(
                &session,
                &cfg,
                Some("CAPTURED_PRIVATE_MEMORY"),
                None,
                ToolMetadata::Unavailable("not built"),
            );
            assert_eq!(
                with_memory.threshold,
                1000 - crate::extras::memory::effective_session_reserve(
                    100,
                    Some("CAPTURED_PRIVATE_MEMORY"),
                    &session,
                )
            );
            assert!(!with_memory.render().contains("CAPTURED_PRIVATE_MEMORY"));
        }

        assert_eq!(
            diagnostic.rows.iter().map(|row| row.tokens).sum::<i128>(),
            i128::from(diagnostic.total)
        );
    }
    #[test]
    fn context63_source_report_sanitizes_bounds_and_does_not_change_context_totals() {
        let session = Session::new("fixture", "model", 1024, "context-metadata");
        let mut diagnostic = report(&session);
        diagnostic.instructions = Some(crate::context::instructions::InstructionDiagnostics {
            sources: vec![(
                std::path::PathBuf::from(format!("name\n\u{1b}[31m{}", "界".repeat(200))),
                std::path::PathBuf::from("scope\u{202e}\r\n"),
                true,
                42,
            )],
            source_count: 33,
            retained_bytes: 84,
            failed_directories: 1,
            initial_failed: true,
            delivered_notices: 5,
        });
        let rendered = diagnostic.render();
        assert!(
            rendered
                .contains("33 sources; 84 newly retained bytes; 2 load failures; 5 notice results")
        );
        assert!(rendered.contains("32 more captured sources omitted"));
        assert!(rendered.contains("not runner delivery acknowledgements"));
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{202e}'));
        assert!(!rendered.contains('\r'));
        assert!(rendered.len() < 8192);
        assert_eq!(
            diagnostic.rows.iter().map(|row| row.tokens).sum::<i128>(),
            i128::from(session.effective_context_tokens())
        );
        assert_eq!(diagnostic.total, session.effective_context_tokens());
    }
}
