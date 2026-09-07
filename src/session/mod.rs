pub mod chat_history;
pub mod storage;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};

use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const TOOL_RESULT_SAVE_THRESHOLD: usize = 12_000;
pub const TOOL_RESULT_HEAD_CHARS: usize = 2_000;
pub const TOOL_RESULT_TAIL_CHARS: usize = 8_000;
pub const DEFAULT_KEEP_RECENT_TOOL_RESULTS: usize = 8;
pub const CLEARED_TOOL_RESULT_NOTICE: &str =
    "[result cleared: old tool output omitted from the live context; re-run the tool if needed]";

#[derive(Debug)]
struct PendingToolResultSpill {
    path: std::path::PathBuf,
    correlation_ids: Vec<String>,
    output_key: (String, [u8; 32]),
}

#[derive(Debug, Default)]
struct PendingToolResultSpills {
    by_id: HashMap<String, VecDeque<Arc<PendingToolResultSpill>>>,
    by_output: HashMap<(String, [u8; 32]), VecDeque<Arc<PendingToolResultSpill>>>,
}

/// Process-local handoff from the pre-model tool-result hook to transcript
/// persistence. Correlation IDs are framework-owned, so artifact paths never
/// need to be recovered by parsing or trusting model-visible tool output.
#[derive(Clone, Debug, Default)]
pub(crate) struct ToolResultSpillStore {
    pending: Arc<Mutex<PendingToolResultSpills>>,
}

impl ToolResultSpillStore {
    pub(crate) fn register(
        &self,
        internal_call_id: &str,
        provider_call_id: Option<&str>,
        tool_name: &str,
        model_output: &str,
        path: std::path::PathBuf,
    ) {
        let mut correlation_ids = vec![internal_call_id.to_owned()];
        if let Some(provider_call_id) = provider_call_id
            && !provider_call_id.is_empty()
            && provider_call_id != internal_call_id
        {
            correlation_ids.push(provider_call_id.to_owned());
        }
        let output_key = (tool_name.to_owned(), sha256(model_output));
        let spill = Arc::new(PendingToolResultSpill {
            path,
            correlation_ids: correlation_ids.clone(),
            output_key: output_key.clone(),
        });
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for id in correlation_ids {
            pending
                .by_id
                .entry(id)
                .or_default()
                .push_back(spill.clone());
        }
        pending
            .by_output
            .entry(output_key)
            .or_default()
            .push_back(spill);
    }

    fn take(
        &self,
        correlation_id: &str,
        tool_name: &str,
        output: &str,
    ) -> Option<std::path::PathBuf> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let spill = if correlation_id.is_empty() {
            None
        } else {
            pending
                .by_id
                .get_mut(correlation_id)
                .and_then(VecDeque::pop_front)
        }
        .or_else(|| {
            pending
                .by_output
                .get_mut(&(tool_name.to_owned(), sha256(output)))
                .and_then(VecDeque::pop_front)
        })?;
        for id in &spill.correlation_ids {
            let mut remove_key = false;
            if let Some(queue) = pending.by_id.get_mut(id) {
                queue.retain(|candidate| !Arc::ptr_eq(candidate, &spill));
                remove_key = queue.is_empty();
            }
            if remove_key {
                pending.by_id.remove(id);
            }
        }
        let mut remove_output_key = false;
        if let Some(queue) = pending.by_output.get_mut(&spill.output_key) {
            queue.retain(|candidate| !Arc::ptr_eq(candidate, &spill));
            remove_output_key = queue.is_empty();
        }
        if remove_output_key {
            pending.by_output.remove(&spill.output_key);
        }
        Some(spill.path.clone())
    }
}

fn sha256(value: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(value.as_bytes()).into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    System,
    ToolCall,
    ToolResult,
    SubagentToolCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMessage {
    pub role: MessageRole,
    pub content: CompactString,
    pub estimated_tokens: u64,
    /// Stable internal identity for auditable call/result correlation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<CompactString>,
    /// Model-visible structured payload for resumable tool history. `content`
    /// remains the bounded, human-readable transcript used by exports and the
    /// UI; legacy sessions simply omit this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<PersistedToolMessage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PersistedToolMessage {
    Call {
        name: CompactString,
        arguments: serde_json::Value,
    },
    Result {
        output: CompactString,
        /// Separately persisted full output for a truncated result. Kept in
        /// the durable session so live-context pruning can retain a recorded
        /// recovery path without parsing marker-like text from the tool.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        artifact_path: Option<CompactString>,
    },
}

/// One reasoning block exactly as the provider emitted it.
///
/// Kept as an ordered list of typed blocks rather than a flattened summary so
/// a replayed item is byte-identical to what the model produced: the Responses
/// API validates a `reasoning` item against the `function_call` it was emitted
/// with, and Anthropic validates a thinking block against its signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PersistedReasoningBlock {
    Text {
        text: CompactString,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<CompactString>,
    },
    Summary {
        text: CompactString,
    },
    Encrypted {
        data: CompactString,
    },
    Redacted {
        data: CompactString,
    },
}

/// A complete provider reasoning item persisted alongside the tool call it was
/// emitted for. `id` is the provider's own item id (`rs_...` on the Responses
/// API); without it the item cannot be replayed as a stored provider item and
/// the paired native `function_call` id must not be replayed either.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedReasoning {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<CompactString>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<PersistedReasoningBlock>,
}

impl PersistedReasoning {
    /// True when the item carries neither an identity nor any content, and so
    /// is not worth persisting.
    pub fn is_empty(&self) -> bool {
        self.id.is_none() && self.blocks.is_empty()
    }

    /// True when the item can be replayed as a stored provider reasoning item.
    fn is_replayable(&self) -> bool {
        self.id.is_some() && !self.blocks.is_empty()
    }

    pub fn from_rig(reasoning: &rig::message::Reasoning) -> Self {
        let blocks = reasoning
            .content
            .iter()
            .filter_map(|block| match block {
                rig::message::ReasoningContent::Text { text, signature } => {
                    Some(PersistedReasoningBlock::Text {
                        text: CompactString::new(text),
                        signature: signature.as_deref().map(CompactString::new),
                    })
                }
                rig::message::ReasoningContent::Summary(text) => {
                    Some(PersistedReasoningBlock::Summary {
                        text: CompactString::new(text),
                    })
                }
                rig::message::ReasoningContent::Encrypted(data) => {
                    Some(PersistedReasoningBlock::Encrypted {
                        data: CompactString::new(data),
                    })
                }
                rig::message::ReasoningContent::Redacted { data } => {
                    Some(PersistedReasoningBlock::Redacted {
                        data: CompactString::new(data),
                    })
                }
                // `ReasoningContent` is `#[non_exhaustive]`. An unknown block is
                // dropped rather than guessed at; a call left with no replayable
                // reasoning falls back to the rewritten identity, which is safe.
                _ => None,
            })
            .collect();
        Self {
            id: reasoning.id.as_deref().map(CompactString::new),
            blocks,
        }
    }

    pub fn to_rig(&self) -> rig::message::Reasoning {
        // `rig::message::Reasoning` is `#[non_exhaustive]` and has no
        // constructor that takes arbitrary blocks, so start from an empty item
        // and fill its public fields.
        let mut reasoning = rig::message::Reasoning::multi(Vec::new())
            .optional_id(self.id.as_ref().map(ToString::to_string));
        reasoning.content = self
            .blocks
            .iter()
            .map(|block| match block {
                PersistedReasoningBlock::Text { text, signature } => {
                    rig::message::ReasoningContent::Text {
                        text: text.to_string(),
                        signature: signature.as_ref().map(ToString::to_string),
                    }
                }
                PersistedReasoningBlock::Summary { text } => {
                    rig::message::ReasoningContent::Summary(text.to_string())
                }
                PersistedReasoningBlock::Encrypted { data } => {
                    rig::message::ReasoningContent::Encrypted(data.to_string())
                }
                PersistedReasoningBlock::Redacted { data } => {
                    rig::message::ReasoningContent::Redacted {
                        data: data.to_string(),
                    }
                }
            })
            .collect();
        reasoning
    }
}

/// The provider's own identity for one persisted tool call, plus the reasoning
/// items it emitted immediately before that call.
///
/// This lives in [`Session::tool_call_provenance`], keyed by the call record's
/// `tool_call_id`, rather than inside [`PersistedToolMessage::Call`]: the tool
/// record shape predates it, so a session file written before this existed
/// simply has no map and every one of its records keeps replaying through the
/// rewritten identity, exactly as it did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedCallProvenance {
    /// The provider's own `function_call` item id (`fc_...` on the Responses
    /// API). Replaying it asserts the item is already stored provider-side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_item_id: Option<CompactString>,
    /// The provider `call_id` that pairs a call with its output. This is the
    /// durable pairing key both run modes persist as `tool_call_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_call_id: Option<CompactString>,
    /// Reasoning items emitted before the call, in provider order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning: Vec<PersistedReasoning>,
}

impl PersistedCallProvenance {
    /// True when this call can be replayed with the provider's own item ids:
    /// both identities are known and at least one reasoning item can be
    /// replayed with them. The Responses API rejects a native `function_call`
    /// id whose `reasoning` item is missing, so anything less must fall back to
    /// the rewritten identity.
    pub(crate) fn replayable_reasoning(&self) -> Option<(&str, &str, Vec<&PersistedReasoning>)> {
        let item_id = self.provider_item_id.as_deref()?;
        let call_id = self.provider_call_id.as_deref()?;
        let reasoning: Vec<&PersistedReasoning> = self
            .reasoning
            .iter()
            .filter(|item| item.is_replayable())
            .collect();
        if reasoning.is_empty() {
            return None;
        }
        Some((item_id, call_id, reasoning))
    }
}

/// One tool call as the provider itself described it, read back out of the
/// runner's canonical turn transcript.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProviderToolCall {
    pub(crate) name: String,
    pub(crate) provenance: PersistedCallProvenance,
}

/// Read every tool call out of a completed turn's canonical provider
/// transcript, attaching the reasoning items that preceded each one.
///
/// Reasoning accumulates across assistant text (a provider may narrate between
/// thinking and calling) but is dropped at a tool-result boundary, which is
/// where the provider's own item ordering restarts.
pub(crate) fn provider_tool_calls(
    interactions: &[rig::completion::Message],
) -> Vec<ProviderToolCall> {
    use rig::message::AssistantContent;

    let mut calls = Vec::new();
    let mut pending: Vec<PersistedReasoning> = Vec::new();
    for interaction in interactions {
        match interaction {
            rig::completion::Message::Assistant { content, .. } => {
                for item in content.iter() {
                    match item {
                        AssistantContent::Reasoning(reasoning) => {
                            let persisted = PersistedReasoning::from_rig(reasoning);
                            if !persisted.is_empty() {
                                pending.push(persisted);
                            }
                        }
                        AssistantContent::ToolCall(call) => calls.push(ProviderToolCall {
                            name: call.function.name.clone(),
                            provenance: PersistedCallProvenance {
                                provider_item_id: Some(CompactString::new(call.id.as_str())),
                                provider_call_id: call.call_id.as_deref().map(CompactString::new),
                                reasoning: std::mem::take(&mut pending),
                            },
                        }),
                        AssistantContent::Text(_) | AssistantContent::Image(_) => {}
                    }
                }
            }
            rig::completion::Message::User { .. } => pending.clear(),
            rig::completion::Message::System { .. } => {}
        }
    }
    calls
}

/// The identifier both run modes persist for a provider tool call.
///
/// The Responses API pairs a `function_call` with its output by `call_id`, so
/// that is the durable key whenever the provider supplies one. A provider that
/// supplies none (Anthropic) leaves the record on the identity it already has:
/// the provider's single item id headless, the live lifecycle id interactively.
/// Interactive and headless runs must agree here or `--continue` replays
/// differently depending on which mode wrote the session.
pub(crate) fn persisted_call_identifier<'a>(
    call_id: Option<&'a str>,
    fallback: &'a str,
) -> &'a str {
    match call_id {
        Some(call_id) if !call_id.is_empty() => call_id,
        _ => fallback,
    }
}

/// A single-step restore point captured before a conversation rewind, so the
/// destructive truncation can be undone with `/redo`. New records hold only
/// the removed tail; `tail_only = false` preserves compatibility with older
/// session files that stored a full duplicate message list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindUndo {
    messages: Vec<SessionMessage>,
    #[serde(default)]
    tail_only: bool,
    total_estimated_tokens: u64,
    calibrated_tokens: u64,
    calibrated_msg_count: usize,
    #[serde(default)]
    pub(crate) calibrated_tool_results_cleared: usize,
}

#[derive(Default)]
struct HistoryConversionCache {
    entry: std::sync::Mutex<Option<CachedHistoryConversion>>,
}

struct CachedHistoryConversion {
    revision: u64,
    keep_recent_tool_results: usize,
    messages: std::sync::Arc<[rig::completion::Message]>,
}

impl std::fmt::Debug for HistoryConversionCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HistoryConversionCache")
            .finish_non_exhaustive()
    }
}

impl Clone for HistoryConversionCache {
    fn clone(&self) -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Compaction {
    pub summary: CompactString,
    pub first_kept_index: usize,
    pub summarized_count: usize,
    pub token_savings: u64,
    pub created_at: CompactString,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionAllowEntry {
    pub tool: CompactString,
    pub pattern: CompactString,
}

/// An auditable provider/model identity change made while resuming a saved
/// session. The acknowledgement records that the caller used the explicit
/// resume override path after being warned that saved context may be disclosed
/// to a different provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderOverrideAudit {
    pub from_provider: CompactString,
    pub from_model: CompactString,
    pub to_provider: CompactString,
    pub to_model: CompactString,
    pub changed_at: CompactString,
    pub context_disclosure_acknowledged: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: CompactString,
    pub name: CompactString,
    pub messages: Vec<SessionMessage>,
    /// Provider-native identity and reasoning for persisted tool calls, keyed
    /// by the call record's `tool_call_id`. A session file written before this
    /// field existed omits it entirely and deserializes to an empty map, so
    /// every one of its calls replays exactly as it did before.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_call_provenance: BTreeMap<CompactString, PersistedCallProvenance>,
    pub compactions: Vec<Compaction>,
    pub created_at: CompactString,
    pub updated_at: CompactString,
    #[serde(default)]
    pub total_input_tokens: u64,
    #[serde(default)]
    pub total_output_tokens: u64,
    #[serde(default)]
    pub total_cached_input_tokens: u64,
    #[serde(default)]
    pub total_cache_creation_input_tokens: u64,
    /// Provider-normalized prompt tokens, including cached tiers exactly once.
    #[serde(default)]
    pub total_real_input_tokens: u64,
    pub total_cost: f64,
    pub total_estimated_tokens: u64,
    #[serde(default)]
    pub calibrated_tokens: u64,
    #[serde(default)]
    pub calibrated_msg_count: usize,
    /// Number of oldest live tool results represented by the provider
    /// calibration as cleared notices rather than full output.
    #[serde(default)]
    pub(crate) calibrated_tool_results_cleared: usize,
    #[serde(default)]
    pub input_token_cost: f64,
    #[serde(default)]
    pub output_token_cost: f64,
    pub context_window: u64,
    pub model: CompactString,
    pub provider: CompactString,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_override_audit: Vec<ProviderOverrideAudit>,
    pub working_dir: CompactString,
    #[serde(default)]
    pub permission_allowlist: Vec<PermissionAllowEntry>,
    /// Structured task state shared by the active agent tools and persisted
    /// with this logical session. Cloning a live session intentionally keeps
    /// the same store so rebuilt agents observe updates immediately.
    #[serde(
        default,
        skip_serializing_if = "crate::agent::tools::TodoStore::is_empty"
    )]
    pub todos: crate::agent::tools::TodoStore,
    /// Process-local repeated-read state for this logical session. It is never
    /// persisted; startup recreates it from the active configuration.
    #[serde(skip)]
    pub(crate) read_tracker: crate::agent::tools::ReadTracker,
    /// Artifact handoff shared by every rebuild of this logical session.
    #[serde(skip)]
    pub(crate) tool_result_spills: ToolResultSpillStore,
    /// Process-local JSON scratch state shared by JavaScript tools across agent rebuilds.
    #[cfg(feature = "js")]
    #[serde(skip)]
    pub(crate) js_session_state: crate::extras::js::session::JsSessionStateOwner,
    #[cfg(feature = "multimodal")]
    #[serde(skip)]
    pub pending_media: Vec<crate::extras::multimodal::MediaAttachment>,
    /// Display preference (set from config at startup, not persisted): show the
    /// session cost in the status bar even when it is $0.0000.
    #[serde(skip)]
    pub show_cost_always: bool,
    /// Current git branch of `working_dir`, for the status bar. Refreshed at
    /// runtime, not persisted.
    #[serde(skip)]
    pub git_branch: Option<CompactString>,
    /// Working-tree change counts and upstream sync, for the status bar.
    /// Computed only when the statusline uses a git change/status item. Not persisted.
    #[serde(skip)]
    pub git_status: Option<GitStatus>,
    /// Whether reasoning is currently enabled, for the status bar. Synced from
    /// the event loop, not persisted.
    #[serde(skip)]
    pub reasoning_enabled: bool,
    /// Estimated tokens for the fixed request overhead that never lives in
    /// `messages` — system prompt, tool-use preamble, context files, memory.
    /// Used only before the first real calibration (see
    /// [`effective_context_tokens`](Self::effective_context_tokens)); once the
    /// provider reports real usage, the calibration anchor already includes this
    /// overhead, so it must not be added again. Recomputed at runtime, not
    /// persisted.
    #[serde(skip)]
    pub overhead_tokens: u64,
    /// Restore point for the most recent `/redo`-able rewind, captured by
    /// [`rewind_to`](Self::rewind_to). Persisted so an immediately restarted
    /// session retains the same one-step undo/redo semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewind_undo: Option<RewindUndo>,
    /// Process-local Rig history conversion cache. Session clones deliberately
    /// start empty, and neither the cache nor its revision is persisted.
    #[serde(skip)]
    history_conversion_cache: HistoryConversionCache,
    #[serde(skip)]
    history_revision: u64,
}

/// Working-tree summary parsed from `git status --porcelain=v2 --branch`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitStatus {
    pub staged: u32,
    pub modified: u32,
    pub deleted: u32,
    pub untracked: u32,
    pub ahead: u32,
    pub behind: u32,
}

impl GitStatus {
    pub fn is_dirty(&self) -> bool {
        self.staged + self.modified + self.deleted + self.untracked > 0
    }
}

impl Session {
    pub(crate) fn cached_converted_history(
        &self,
        keep_recent_tool_results: usize,
    ) -> Option<std::sync::Arc<[rig::completion::Message]>> {
        self.history_conversion_cache
            .entry
            .lock()
            .ok()
            .and_then(|cache| {
                cache.as_ref().and_then(|entry| {
                    (entry.revision == self.history_revision
                        && entry.keep_recent_tool_results == keep_recent_tool_results)
                        .then(|| std::sync::Arc::clone(&entry.messages))
                })
            })
    }

    pub(crate) fn cache_converted_history(
        &self,
        keep_recent_tool_results: usize,
        messages: std::sync::Arc<[rig::completion::Message]>,
    ) {
        if let Ok(mut cache) = self.history_conversion_cache.entry.lock() {
            *cache = Some(CachedHistoryConversion {
                revision: self.history_revision,
                keep_recent_tool_results,
                messages,
            });
        }
    }

    pub(crate) fn mark_history_changed(&mut self) {
        self.history_revision = self.history_revision.wrapping_add(1);
        if let Ok(mut cache) = self.history_conversion_cache.entry.lock() {
            cache.take();
        }
    }

    /// Start a fresh process-local read history for a newly entered logical
    /// session. Ordinary agent rebuilds must keep cloning the existing tracker;
    /// only startup and explicit session replacement call this initializer.
    pub(crate) fn initialize_read_tracker(&mut self, deny_repeated_reads: bool) {
        self.read_tracker = crate::agent::tools::ReadTracker::new(deny_repeated_reads);
    }

    pub fn estimate_tokens(text: &str) -> u64 {
        let mut wide: u64 = 0;
        let mut narrow: u64 = 0;
        for ch in text.chars() {
            if Self::is_wide_char(ch) {
                wide += 1;
            } else {
                narrow += 1;
            }
        }
        // Wide text remains close to one token per character. For narrow text,
        // 4 chars/token undercounts code and JSON before provider calibration;
        // 13/4 (3.25 chars/token) is a deliberate conservative midpoint of the
        // commonly observed 3-3.5 range for syntax-heavy content.
        ((wide * 9 / 10) + narrow.saturating_mul(4) / 13).max(1)
    }

    fn is_wide_char(ch: char) -> bool {
        matches!(ch as u32,
            0x1100..=0x11FF |   // Hangul Jamo
            0x2E80..=0x9FFF |   // CJK radicals/Kangxi/punctuation/kana/Unified+ExtA
            0xA000..=0xA4CF |   // Yi
            0xAC00..=0xD7A3 |   // Hangul Syllables
            0xF900..=0xFAFF |   // CJK Compatibility Ideographs
            0xFF00..=0xFFEF |   // Halfwidth/Fullwidth Forms
            0x20000..=0x3FFFF   // Supplementary Ideographic Plane (Ext B–F)
        )
    }

    pub fn new(provider: &str, model: &str, context_window: u64, name: &str) -> Self {
        let now = CompactString::new(chrono::Utc::now().to_rfc3339());
        Session {
            id: CompactString::new(Uuid::new_v4().to_string()),
            name: CompactString::new(name),
            messages: Vec::new(),
            tool_call_provenance: BTreeMap::new(),
            compactions: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cached_input_tokens: 0,
            total_cache_creation_input_tokens: 0,
            total_real_input_tokens: 0,
            total_cost: 0.0,
            total_estimated_tokens: 0,
            calibrated_tokens: 0,
            calibrated_msg_count: 0,
            calibrated_tool_results_cleared: 0,
            input_token_cost: 0.0,
            output_token_cost: 0.0,
            context_window,
            model: CompactString::new(model),
            provider: CompactString::new(provider),
            provider_override_audit: Vec::new(),
            working_dir: std::env::current_dir()
                .map(|p| CompactString::new(p.to_string_lossy()))
                .unwrap_or_default(),
            permission_allowlist: Vec::new(),
            todos: crate::agent::tools::TodoStore::default(),
            read_tracker: crate::agent::tools::ReadTracker::default(),
            tool_result_spills: ToolResultSpillStore::default(),
            #[cfg(feature = "js")]
            js_session_state: crate::extras::js::session::JsSessionStateOwner::default(),
            #[cfg(feature = "multimodal")]
            pending_media: Vec::new(),
            show_cost_always: false,
            git_branch: None,
            git_status: None,
            reasoning_enabled: false,
            overhead_tokens: 0,
            rewind_undo: None,
            history_conversion_cache: HistoryConversionCache::default(),
            history_revision: 0,
        }
    }

    pub fn record_provider_override(
        &mut self,
        to_provider: &str,
        to_model: &str,
        context_disclosure_acknowledged: bool,
    ) {
        let audit = ProviderOverrideAudit {
            from_provider: self.provider.clone(),
            from_model: self.model.clone(),
            to_provider: CompactString::new(to_provider),
            to_model: CompactString::new(to_model),
            changed_at: CompactString::new(chrono::Utc::now().to_rfc3339()),
            context_disclosure_acknowledged,
        };
        self.provider = audit.to_provider.clone();
        self.model = audit.to_model.clone();
        self.updated_at = audit.changed_at.clone();
        self.provider_override_audit.push(audit);
    }

    /// Read the current git branch for `dir`, or `None` outside a repo / on a
    /// detached HEAD (then a short commit hash is returned instead). Reads
    /// `.git/HEAD` directly (cheap) rather than spawning git, and follows the
    /// `.git` file pointer used by worktrees and submodules.
    pub fn detect_git_branch(dir: &str) -> Option<CompactString> {
        use std::path::{Path, PathBuf};
        let dir_path = Path::new(dir);
        let dot_git = dir_path.join(".git");
        let gitdir = if dot_git.is_dir() {
            dot_git
        } else if dot_git.is_file() {
            let content = std::fs::read_to_string(&dot_git).ok()?;
            let rel = content.strip_prefix("gitdir:")?.trim();
            let p = PathBuf::from(rel);
            if p.is_absolute() { p } else { dir_path.join(p) }
        } else {
            return None;
        };
        let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
        let head = head.trim();
        if let Some(rest) = head.strip_prefix("ref:") {
            let r = rest.trim();
            Some(CompactString::new(
                r.strip_prefix("refs/heads/").unwrap_or(r),
            ))
        } else if !head.is_empty() {
            // Detached HEAD: show a short commit hash (char-boundary-safe).
            let mut end = head.len().min(8);
            while end > 0 && !head.is_char_boundary(end) {
                end -= 1;
            }
            Some(CompactString::new(&head[..end]))
        } else {
            None
        }
    }

    /// Refresh [`git_branch`](Self::git_branch) from the current `working_dir`.
    pub fn refresh_git_branch(&mut self) {
        self.git_branch = Self::detect_git_branch(&self.working_dir);
    }

    /// Read working-tree status without blocking the async caller. Executable
    /// discovery runs on the blocking pool, while the shared Git runner owns
    /// the child lifetime, output caps, and deadline.
    pub(crate) async fn detect_git_status(dir: &Path) -> Option<GitStatus> {
        let runner = tokio::task::spawn_blocking(crate::git::runner::GitRunner::discover)
            .await
            .ok()?
            .ok()?;
        let result = runner
            .run(
                dir,
                "status",
                ["status", "--porcelain=v2", "--branch"],
                crate::git::runner::QUERY_LIMITS,
            )
            .await
            .ok()?;
        Some(Self::parse_porcelain(&String::from_utf8_lossy(
            &result.stdout,
        )))
    }

    /// Parse `git status --porcelain=v2 --branch` output into a [`GitStatus`].
    pub fn parse_porcelain(text: &str) -> GitStatus {
        let mut s = GitStatus::default();
        for line in text.lines() {
            if let Some(ab) = line.strip_prefix("# branch.ab ") {
                // Format: "+<ahead> -<behind>"
                for tok in ab.split_whitespace() {
                    if let Some(n) = tok.strip_prefix('+') {
                        s.ahead = n.parse().unwrap_or(0);
                    } else if let Some(n) = tok.strip_prefix('-') {
                        s.behind = n.parse().unwrap_or(0);
                    }
                }
            } else if let Some(rest) = line.strip_prefix("1 ").or_else(|| line.strip_prefix("2 ")) {
                // Changed/renamed entry. The XY field is the first token: index
                // status (staged) then worktree status.
                if let Some(xy) = rest.split_whitespace().next() {
                    let mut chars = xy.chars();
                    let x = chars.next().unwrap_or('.');
                    let y = chars.next().unwrap_or('.');
                    if x != '.' {
                        s.staged += 1;
                    }
                    match y {
                        'M' => s.modified += 1,
                        'D' => s.deleted += 1,
                        _ => {}
                    }
                }
            } else if line.starts_with("u ") {
                // Unmerged paths count as a working-tree modification.
                s.modified += 1;
            } else if line.starts_with("? ") {
                s.untracked += 1;
            }
        }
        s
    }

    pub fn add_message(&mut self, role: MessageRole, content: &str) {
        self.add_message_with_tool_data(role, content, None, None);
    }

    /// Record the provider's own identity and reasoning items for the tool call
    /// already persisted under `tool_call_id`.
    pub(crate) fn record_tool_call_provenance(
        &mut self,
        tool_call_id: &str,
        provenance: PersistedCallProvenance,
    ) {
        if tool_call_id.is_empty() || provenance == PersistedCallProvenance::default() {
            return;
        }
        self.tool_call_provenance
            .insert(CompactString::new(tool_call_id), provenance);
        self.mark_history_changed();
    }

    pub(crate) fn provenance_for_tool_call(
        &self,
        tool_call_id: &str,
    ) -> Option<&PersistedCallProvenance> {
        self.tool_call_provenance.get(tool_call_id)
    }

    /// Adopt the provider's own tool identity and reasoning items for the tool
    /// records the interactive path already persisted live.
    ///
    /// `AgentEvent::ToolCall` carries rig's internal lifecycle id and no
    /// reasoning at all, so without this the interactive transcript would
    /// persist a different identifier than headless `-p` (making `--continue`
    /// mode-dependent) and could never replay a native `function_call` item.
    ///
    /// `interactions` is the runner's canonical transcript for the turn that
    /// just ended. Pairing is positional over the turn's trailing tool-call
    /// records — the same provider order both sides observe — and is guarded
    /// three ways: the run of records must be exactly as long as the provider's
    /// call list, the tool names must agree pairwise, and no record in the run
    /// may already carry provenance (which would mean it belongs to an earlier
    /// committed turn). Anything that fails to line up abandons the whole
    /// adoption and leaves the live records exactly as they were.
    ///
    /// Returns the number of records that adopted a provider identity.
    pub(crate) fn adopt_provider_tool_identity(
        &mut self,
        interactions: &[rig::completion::Message],
    ) -> usize {
        let calls = provider_tool_calls(interactions);
        if calls.is_empty() {
            return 0;
        }
        let Some(indices) = self.trailing_unrecorded_tool_call_indices(calls.len()) else {
            return 0;
        };
        for (index, call) in indices.iter().zip(calls.iter()) {
            let Some(PersistedToolMessage::Call { name, .. }) = &self.messages[*index].tool else {
                return 0;
            };
            if name.as_str() != call.name {
                return 0;
            }
        }

        let mut adopted = 0;
        for (index, call) in indices.iter().zip(calls) {
            let Some(previous) = self.messages[*index].tool_call_id.clone() else {
                continue;
            };
            // Only a provider that supplies its own pairing key renames the
            // record; otherwise the live lifecycle id stays the identity the
            // call and its result share, and keys the provenance too.
            let identifier = CompactString::new(persisted_call_identifier(
                call.provenance.provider_call_id.as_deref(),
                previous.as_str(),
            ));
            if identifier != previous {
                self.rename_tool_record_identity(*index, &previous, &identifier);
            }
            self.record_tool_call_provenance(&identifier, call.provenance);
            adopted += 1;
        }
        adopted
    }

    /// Indices of the last `count` tool-call records of the current turn, or
    /// `None` when that run cannot be identified unambiguously.
    fn trailing_unrecorded_tool_call_indices(&self, count: usize) -> Option<Vec<usize>> {
        let mut indices = Vec::with_capacity(count);
        for (index, message) in self.messages.iter().enumerate().rev() {
            if message.role == MessageRole::User {
                break;
            }
            if !matches!(message.tool, Some(PersistedToolMessage::Call { .. })) {
                continue;
            }
            let id = message
                .tool_call_id
                .as_deref()
                .filter(|id| !id.is_empty())?;
            if self.tool_call_provenance.contains_key(id) {
                break;
            }
            indices.push(index);
            if indices.len() == count {
                break;
            }
        }
        if indices.len() != count {
            return None;
        }
        indices.reverse();
        Some(indices)
    }

    /// Rewrite the identity a call record and its result record share.
    fn rename_tool_record_identity(&mut self, call_index: usize, previous: &str, next: &str) {
        self.messages[call_index].tool_call_id = Some(CompactString::new(next));
        for message in &mut self.messages[call_index + 1..] {
            if message.role == MessageRole::ToolResult
                && message.tool_call_id.as_deref() == Some(previous)
            {
                message.tool_call_id = Some(CompactString::new(next));
                break;
            }
        }
        self.mark_history_changed();
    }

    /// Drop provenance for calls that are no longer in the transcript. Called
    /// where messages are removed for good; a rewind keeps its entries so
    /// `/redo` restores complete records.
    fn prune_tool_call_provenance(&mut self) {
        if self.tool_call_provenance.is_empty() {
            return;
        }
        let live: HashSet<&str> = self
            .messages
            .iter()
            .filter_map(|message| message.tool_call_id.as_deref())
            .collect();
        self.tool_call_provenance
            .retain(|id, _| live.contains(id.as_str()));
    }

    fn add_message_with_tool_data(
        &mut self,
        role: MessageRole,
        content: &str,
        tool_call_id: Option<&str>,
        tool: Option<PersistedToolMessage>,
    ) {
        let tokens = Self::estimate_tokens(content);
        self.messages.push(SessionMessage {
            role,
            content: CompactString::new(content),
            estimated_tokens: tokens,
            tool_call_id: tool_call_id.map(CompactString::new),
            tool,
        });
        self.mark_history_changed();
        self.total_estimated_tokens = self.total_estimated_tokens.saturating_add(tokens);
        self.updated_at = CompactString::new(chrono::Utc::now().to_rfc3339());
        // The conversation has moved forward, so the last rewind's restore point
        // no longer lines up — drop it so /redo can't splice in a stale tail.
        self.rewind_undo = None;
    }

    pub fn add_tool_call_with_id(&mut self, id: &str, name: &str, args: &serde_json::Value) {
        self.add_message_with_tool_data(
            MessageRole::ToolCall,
            &crate::ui::utils::format_tool_call_summary(name, args),
            (!id.is_empty()).then_some(id),
            Some(PersistedToolMessage::Call {
                name: CompactString::new(name),
                arguments: args.clone(),
            }),
        );
    }

    #[cfg(test)]
    pub fn add_tool_call(&mut self, name: &str, args: &serde_json::Value) {
        self.add_tool_call_with_id("", name, args);
    }

    pub fn add_tool_result_with_id(&mut self, id: &str, name: &str, output: &str) -> String {
        self.add_tool_result_with_id_and_artifact(id, name, output)
            .0
    }

    /// Add a correlated tool result and return the separately persisted
    /// artifact, when the long-output path was used. The interactive UI tracks
    /// that artifact in its pending-turn transaction for rollback.
    pub(crate) fn add_tool_result_with_id_and_artifact(
        &mut self,
        id: &str,
        name: &str,
        output: &str,
    ) -> (String, Option<std::path::PathBuf>) {
        let (content, replay_output, artifact) = self.tool_result_content(id, name, output);
        let artifact_path = artifact
            .as_ref()
            .map(|path| CompactString::new(path.to_string_lossy()));
        self.add_message_with_tool_data(
            MessageRole::ToolResult,
            &content,
            (!id.is_empty()).then_some(id),
            Some(PersistedToolMessage::Result {
                output: CompactString::new(replay_output),
                artifact_path,
            }),
        );
        (content, artifact)
    }

    /// Apply one reconciled provider-usage delta to every persisted accounting
    /// total. Terminal responses are deliberately absent from this API so UI
    /// and headless callers cannot charge an aggregate a second time.
    pub fn charge_usage_delta(&mut self, usage: crate::event::UsageDelta, anthropic_native: bool) {
        self.total_input_tokens = self.total_input_tokens.saturating_add(usage.input_tokens);
        self.total_output_tokens = self.total_output_tokens.saturating_add(usage.output_tokens);
        self.total_cached_input_tokens = self
            .total_cached_input_tokens
            .saturating_add(usage.cached_input_tokens);
        self.total_cache_creation_input_tokens = self
            .total_cache_creation_input_tokens
            .saturating_add(usage.cache_creation_input_tokens);
        self.total_real_input_tokens =
            self.total_real_input_tokens
                .saturating_add(Self::real_input_tokens(
                    anthropic_native,
                    usage.input_tokens,
                    usage.total_tokens,
                    usage.output_tokens,
                    usage.cached_input_tokens,
                    usage.cache_creation_input_tokens,
                ));
        self.total_cost += crate::pricing::estimate_cost(
            crate::pricing::billable_input_tokens(
                anthropic_native,
                usage.input_tokens,
                usage.cached_input_tokens,
                usage.cache_creation_input_tokens,
            ),
            usage.output_tokens,
            self.input_token_cost,
            self.output_token_cost,
        );
    }

    #[cfg(test)]
    pub fn add_tool_result(&mut self, name: &str, output: &str) -> String {
        self.add_tool_result_with_artifact(name, output).0
    }

    /// Add a tool result and return the separately persisted artifact, when
    /// the long-output path was used. The interactive UI records that path in
    /// its pending-turn transaction so a later failure can remove it.
    #[cfg(test)]
    pub(crate) fn add_tool_result_with_artifact(
        &mut self,
        name: &str,
        output: &str,
    ) -> (String, Option<std::path::PathBuf>) {
        let (content, _, artifact) = self.tool_result_content("", name, output);
        self.add_message(MessageRole::ToolResult, &content);
        (content, artifact)
    }

    fn tool_result_content(
        &self,
        id: &str,
        name: &str,
        output: &str,
    ) -> (String, String, Option<std::path::PathBuf>) {
        if let Some(path) = self.tool_result_spills.take(id, name, output) {
            return (format!("{name}:\n{output}"), output.to_string(), Some(path));
        }
        let output_chars = output.chars().count();
        if output_chars <= TOOL_RESULT_SAVE_THRESHOLD {
            return (format!("{name}:\n{output}"), output.to_string(), None);
        }

        match storage::save_tool_output(&self.id, name, output) {
            Ok(path) => {
                let replay_output = format_truncated_tool_output(output, output_chars, &path);
                (
                    format!("{name}:\n{replay_output}"),
                    replay_output,
                    Some(path),
                )
            }
            Err(err) => {
                let replay_output = format!(
                    "{output}\n\n[failed to save long tool output separately; kept full output in session to avoid data loss: {err}]"
                );
                (format!("{name}:\n{replay_output}"), replay_output, None)
            }
        }
    }

    #[cfg(any(feature = "subagents", feature = "acp"))]
    pub fn add_subagent_tool_call(&mut self, name: &str, args: &serde_json::Value) {
        self.add_message(
            MessageRole::SubagentToolCall,
            &crate::ui::utils::format_tool_call_summary(name, args),
        );
    }

    #[cfg(feature = "multimodal")]
    pub fn drain_media(&mut self) -> Vec<crate::extras::multimodal::MediaAttachment> {
        std::mem::take(&mut self.pending_media)
    }

    /// The true prompt size occupying the context window, normalizing across
    /// providers' differing cache-usage reporting.
    ///
    /// The Anthropic-native route reports `input_tokens` counting *only* the
    /// uncached portion of the prompt; the cache-read and cache-creation tokens
    /// are reported in separate fields even though they still occupy the context
    /// window. So there the real prompt size is the sum of all three. The
    /// OpenAI, Gemini and OpenRouter fold the cached subset into `input_tokens`,
    /// so adding cache-detail fields would double-count. Some compatible
    /// gateways report a smaller `input_tokens` value but preserve the full
    /// prompt in `total_tokens - output_tokens`; that normalized total is used
    /// when it is larger than the primary input count.
    ///
    /// `anthropic_native` must be the *resolved protocol route*, not a literal
    /// provider-name comparison — a custom gateway with `provider_type =
    /// "anthropic"` uses the native route under a different name, while
    /// OpenRouter serving a Claude model does not. Compute it with
    /// [`Config::is_anthropic_native`](crate::config::Config::is_anthropic_native).
    pub fn real_input_tokens(
        anthropic_native: bool,
        input_tokens: u64,
        total_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
        cache_creation_input_tokens: u64,
    ) -> u64 {
        if anthropic_native {
            input_tokens
                .saturating_add(cached_input_tokens)
                .saturating_add(cache_creation_input_tokens)
        } else {
            input_tokens.max(total_tokens.saturating_sub(output_tokens))
        }
    }

    pub fn set_calibration(&mut self, input_tokens: u64, output_tokens: u64) {
        self.set_calibration_with_cleared_tool_results(input_tokens, output_tokens, 0);
    }

    pub fn set_calibration_with_cleared_tool_results(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        cleared_tool_results: usize,
    ) {
        if input_tokens == 0 {
            return;
        }
        self.calibrated_tokens = input_tokens.saturating_add(output_tokens);
        self.calibrated_msg_count = self.messages.len();
        let (_, first_live) = self.compacted_context();
        let live_results = self.messages[first_live..]
            .iter()
            .filter(|message| message.role == MessageRole::ToolResult)
            .count();
        self.calibrated_tool_results_cleared = cleared_tool_results.min(live_results);
    }

    /// Mark messages appended after the most recent provider usage event as
    /// covered by that calibration. The usage already includes the completed
    /// assistant output, even though the UI persists that message on `Done`.
    pub fn reanchor_calibration_to_current_messages(&mut self) {
        if self.calibrated_tokens > 0 {
            self.calibrated_msg_count = self.messages.len();
        }
    }

    pub fn reset_calibration(&mut self) {
        self.calibrated_tokens = 0;
        self.calibrated_msg_count = 0;
        self.calibrated_tool_results_cleared = 0;
    }

    /// Truncate the conversation to `new_len` messages while keeping the context
    /// figure accurate (used by `/undo` and the failed-send rollback).
    ///
    /// If any removed message was part of the calibration anchor, subtract its
    /// estimated tokens from the anchor rather than discarding the whole
    /// calibration. Resetting to a cold estimate would undercount (the estimate
    /// omits tool schemas), and leaving the anchor untouched would overcount by
    /// the removed turn — subtracting keeps the figure ≈ the real remaining
    /// size. Messages beyond the anchor were never in it, so removing them only
    /// shrinks the estimated tail.
    pub fn truncate_to(&mut self, new_len: usize) {
        if new_len >= self.messages.len() {
            return;
        }
        let cal = self.calibrated_msg_count.min(self.messages.len());
        if self.calibrated_tokens > 0 && new_len < cal {
            let (_, first_live) = self.compacted_context();
            let retained_live_start = first_live.min(new_len);
            let mut result_ordinal = self.messages[retained_live_start..new_len]
                .iter()
                .filter(|message| message.role == MessageRole::ToolResult)
                .count();
            let removed = self.messages[new_len..cal]
                .iter()
                .enumerate()
                .map(|(offset, message)| {
                    let index = new_len + offset;
                    let is_live_result =
                        index >= first_live && message.role == MessageRole::ToolResult;
                    let tokens = if is_live_result
                        && result_ordinal < self.calibrated_tool_results_cleared
                    {
                        projected_tool_result_tokens(message)
                    } else {
                        message.estimated_tokens
                    };
                    if is_live_result {
                        result_ordinal = result_ordinal.saturating_add(1);
                    }
                    tokens
                })
                .fold(0u64, u64::saturating_add);
            self.calibrated_tokens = self.calibrated_tokens.saturating_sub(removed);
            self.calibrated_msg_count = new_len;
            let remaining_results = self.messages[retained_live_start..new_len]
                .iter()
                .filter(|message| message.role == MessageRole::ToolResult)
                .count();
            self.calibrated_tool_results_cleared =
                self.calibrated_tool_results_cleared.min(remaining_results);
        }
        self.messages.truncate(new_len);
        self.mark_history_changed();
        self.total_estimated_tokens = self.messages.iter().map(|m| m.estimated_tokens).sum();
        self.read_tracker.clear();
    }

    /// Rewind the conversation to `new_len` messages, capturing a single-step
    /// restore point first so the cut can be undone with [`redo`](Self::redo).
    ///
    /// This is the shared primitive behind both `/undo` (rewind by one turn) and
    /// the double-Esc rewind picker (rewind to a chosen earlier point): the only
    /// difference between them is which `new_len` they pass. Returns the number
    /// of messages removed (0 if `new_len` is already at or past the end, in
    /// which case no restore point is recorded).
    pub fn rewind_to(&mut self, new_len: usize) -> usize {
        if new_len >= self.messages.len() {
            return 0;
        }
        let removed = self.messages.len() - new_len;
        self.rewind_undo = Some(RewindUndo {
            messages: self.messages[new_len..].to_vec(),
            tail_only: true,
            total_estimated_tokens: self.total_estimated_tokens,
            calibrated_tokens: self.calibrated_tokens,
            calibrated_msg_count: self.calibrated_msg_count,
            calibrated_tool_results_cleared: self.calibrated_tool_results_cleared,
        });
        self.truncate_to(new_len);
        removed
    }

    /// Restore the messages removed by the most recent [`rewind_to`](Self::rewind_to)
    /// (i.e. the last `/undo` or rewind). Returns false when there is nothing to
    /// restore. The restore point is consumed, and is also invalidated as soon
    /// as the conversation moves forward again (see [`add_message`](Self::add_message)),
    /// so `/redo` only ever reaches back to the cut it directly reverses.
    pub fn redo(&mut self) -> bool {
        match self.rewind_undo.take() {
            Some(u) => {
                if u.tail_only {
                    self.messages.extend(u.messages);
                } else {
                    // Compatibility with the pre-tail-only persisted shape.
                    self.messages = u.messages;
                }
                self.mark_history_changed();
                self.total_estimated_tokens = u.total_estimated_tokens;
                self.calibrated_tokens = u.calibrated_tokens;
                self.calibrated_msg_count = u.calibrated_msg_count;
                self.calibrated_tool_results_cleared = u.calibrated_tool_results_cleared;
                self.read_tracker.clear();
                true
            }
            None => false,
        }
    }

    /// True while the context figure is still an estimate — no provider usage
    /// has been reported yet (or it was reset by `/clear`). The status bar marks
    /// the estimated value so the snap to the real number on the first response
    /// reads as a refinement rather than a surprise.
    pub fn ctx_is_estimated(&self) -> bool {
        self.calibrated_tokens == 0
    }

    pub fn effective_context_tokens(&self) -> u64 {
        if self.calibrated_tokens == 0 {
            // No real usage yet: per-message estimates cover only `messages`, so
            // add the fixed overhead (system prompt, tools, context files) that
            // every request also carries. After calibration this overhead is
            // already inside the anchor, so it is not added in that branch.
            return self
                .overhead_tokens
                .saturating_add(self.total_estimated_tokens);
        }
        let start = self.calibrated_msg_count.min(self.messages.len());
        let delta: u64 = self.messages[start..]
            .iter()
            .map(|m| m.estimated_tokens)
            .sum();
        self.calibrated_tokens.saturating_add(delta)
    }

    /// Clone the durable transcript into the model-visible form used for the
    /// next request, replacing all but the newest `keep_recent` tool results
    /// with compact recovery notices. Calls and message identities remain
    /// intact, and the persisted session is never mutated.
    pub fn context_messages_with_pruned_tool_results(
        &self,
        keep_recent: usize,
    ) -> Vec<SessionMessage> {
        let (_, first_live) = self.compacted_context();
        let mut remaining_to_clear = self.live_tool_result_clear_count(first_live, keep_recent);
        let mut messages = self.messages.clone();

        for message in &mut messages[first_live..] {
            if message.role != MessageRole::ToolResult || remaining_to_clear == 0 {
                continue;
            }
            remaining_to_clear -= 1;
            let (content, notice) = projected_cleared_tool_result(message);
            message.content = CompactString::new(content);
            message.estimated_tokens = Self::estimate_tokens(&message.content);
            if let Some(PersistedToolMessage::Result { output, .. }) = &mut message.tool {
                *output = CompactString::new(notice);
            }
        }

        messages
    }

    /// Estimated live-context pressure after old tool results are cleared.
    /// Provider calibration remains the best available absolute anchor; the
    /// estimator applies the per-message delta in either direction so clearing
    /// an unusually short result cannot make the gate undercount the request.
    pub fn effective_context_tokens_after_tool_result_pruning(&self, keep_recent: usize) -> u64 {
        let (_, first_live) = self.compacted_context();
        let current_clear = self.live_tool_result_clear_count(first_live, keep_recent);
        let anchor = self.calibrated_msg_count.min(self.messages.len());
        let mut effective = if self.calibrated_tokens == 0 {
            self.overhead_tokens
        } else {
            self.calibrated_tokens
        };
        let mut result_ordinal = 0usize;

        for (index, message) in self.messages.iter().enumerate() {
            let is_live_result = index >= first_live && message.role == MessageRole::ToolResult;
            let current_pruned = is_live_result && result_ordinal < current_clear;
            let old_pruned = is_live_result
                && result_ordinal < self.calibrated_tool_results_cleared
                && index < anchor;
            let replacement_tokens =
                (current_pruned || old_pruned).then(|| projected_tool_result_tokens(message));
            let current_tokens = if current_pruned {
                replacement_tokens.unwrap_or(message.estimated_tokens)
            } else {
                message.estimated_tokens
            };

            if self.calibrated_tokens == 0 || index >= anchor {
                effective = effective.saturating_add(current_tokens);
            } else if old_pruned != current_pruned {
                let old_tokens = if old_pruned {
                    replacement_tokens.unwrap_or(message.estimated_tokens)
                } else {
                    message.estimated_tokens
                };
                effective = apply_token_estimate_delta(effective, old_tokens, current_tokens);
            }

            if is_live_result {
                result_ordinal = result_ordinal.saturating_add(1);
            }
        }
        effective
    }

    pub fn needs_compaction_after_tool_result_pruning(
        &self,
        reserve_tokens: u64,
        keep_recent: usize,
    ) -> bool {
        if self.context_window == 0 {
            return false;
        }
        self.effective_context_tokens_after_tool_result_pruning(keep_recent)
            > self.context_window.saturating_sub(reserve_tokens)
    }

    pub fn needs_compaction_with_pending_after_tool_result_pruning(
        &self,
        reserve_tokens: u64,
        pending_tokens: u64,
        keep_recent: usize,
    ) -> bool {
        if self.context_window == 0 {
            return false;
        }
        self.effective_context_tokens_after_tool_result_pruning(keep_recent)
            .saturating_add(pending_tokens)
            > self.context_window.saturating_sub(reserve_tokens)
    }

    fn live_tool_result_clear_count(&self, first_live: usize, keep_recent: usize) -> usize {
        self.messages[first_live..]
            .iter()
            .filter(|message| message.role == MessageRole::ToolResult)
            .count()
            .saturating_sub(keep_recent)
    }

    pub(crate) fn tool_results_cleared_for_retention(&self, keep_recent: usize) -> usize {
        let (_, first_live) = self.compacted_context();
        self.live_tool_result_clear_count(first_live, keep_recent)
    }

    /// Pick the compaction boundary: `messages[..cut]` get summarized and
    /// `messages[cut..]` are kept as recent context. Walks backward summing
    /// per-message `estimated_tokens` until `keep_recent` is covered.
    ///
    /// This deliberately stays in the per-message estimate scale rather than
    /// the calibrated total: it is a *relative* comparison among messages (how
    /// far back does `keep_recent` reach), so any uniform estimator bias
    /// cancels out. Calibration only matters for the absolute total in
    /// `effective_context_tokens`.
    ///
    /// Returns 0 ("nothing old enough to summarize") when every message fits
    /// within `keep_recent`. The initial value is 0, not `messages.len()`, so
    /// an oversized `keep_recent` keeps the recent messages instead of
    /// summarizing the entire history, a case made reachable now that the
    /// compaction gate measures context against real usage (system prompt and
    /// context files can push the real total over budget while the messages
    /// themselves stay small).
    pub fn select_compaction_cut(messages: &[SessionMessage], keep_recent: u64) -> usize {
        let mut accumulated = 0u64;
        let mut cut_idx = 0;
        for (i, msg) in messages.iter().enumerate().rev() {
            if accumulated >= keep_recent {
                cut_idx = i + 1;
                break;
            }
            accumulated = accumulated.saturating_add(msg.estimated_tokens);
        }
        if cut_idx > 0 && cut_idx < messages.len() {
            while cut_idx > 0
                && !matches!(
                    messages[cut_idx].role,
                    MessageRole::User | MessageRole::Assistant
                )
            {
                cut_idx -= 1;
            }
        }
        cut_idx
    }

    pub fn needs_compaction(&self, reserve_tokens: u64) -> bool {
        if self.context_window == 0 {
            return false;
        }
        self.effective_context_tokens() > self.context_window.saturating_sub(reserve_tokens)
    }

    /// Like `needs_compaction` but also accounts for `pending_tokens` that
    /// will be added to the request (pending user prompt, pending media).
    /// Use this for pre-dispatch preflight so compaction decisions include
    /// the payload that is about to be sent.
    pub fn needs_compaction_with_pending(&self, reserve_tokens: u64, pending_tokens: u64) -> bool {
        if self.context_window == 0 {
            return false;
        }
        self.effective_context_tokens()
            .saturating_add(pending_tokens)
            > self.context_window.saturating_sub(reserve_tokens)
    }

    /// Returns true when the pending payload cannot fit even if the entire
    /// message history were removed. When this is true, compaction cannot
    /// create enough room and the request must be rejected locally.
    pub fn is_irreducible_with_pending(&self, reserve_tokens: u64, pending_tokens: u64) -> bool {
        if self.context_window == 0 {
            return false;
        }
        self.overhead_tokens.saturating_add(pending_tokens)
            >= self.context_window.saturating_sub(reserve_tokens)
    }

    pub fn update_context_window(&mut self, cw: u64) {
        self.context_window = cw;
    }

    pub fn compacted_context(&self) -> (Option<&str>, usize) {
        let c = match self.compactions.last() {
            Some(c) => c,
            None => return (None, 0),
        };
        // Locate the summary System message at runtime rather than trusting
        // a stored index, which drifts if messages are inserted before it.
        for (i, msg) in self.messages.iter().enumerate() {
            if msg.role == MessageRole::System && msg.content.as_str() == c.summary.as_str() {
                return (Some(c.summary.as_str()), i + 1);
            }
        }
        (None, 0)
    }

    pub fn compress(&mut self, mut summary: String, first_kept_index: usize, token_savings: u64) {
        if let Some(todo_context) = self.todos.critical_context() {
            summary.push_str("\n\n");
            summary.push_str(&todo_context);
        }
        let summarized_count = first_kept_index;
        let summary_tokens = Self::estimate_tokens(&summary);

        // Insert a System message with the summary at the boundary
        let summary_msg = SessionMessage {
            role: MessageRole::System,
            content: CompactString::from(summary.clone()),
            estimated_tokens: summary_tokens,
            tool_call_id: None,
            tool: None,
        };

        // Remove summarized messages and insert summary
        self.messages.drain(..first_kept_index);
        self.messages.insert(0, summary_msg);
        self.prune_tool_call_provenance();

        // Recompute total from remaining messages so the count is always
        // consistent — no underflow risk when token_savings is stale.
        self.total_estimated_tokens = self.messages.iter().map(|m| m.estimated_tokens).sum();

        self.compactions.push(Compaction {
            summary: CompactString::from(summary),
            first_kept_index: 1, // The summary is at index 0
            summarized_count,
            token_savings,
            created_at: CompactString::new(chrono::Utc::now().to_rfc3339()),
        });

        // Compaction reindexes messages, so the calibration anchor no longer
        // lines up and an older redo snapshot is no longer composable. Drop
        // both; the next completed turn re-anchors.
        self.reset_calibration();
        self.rewind_undo = None;
        self.mark_history_changed();
        self.read_tracker.clear();
        self.updated_at = CompactString::new(chrono::Utc::now().to_rfc3339());
    }

    /// Number of leading messages to drain after a compaction whose summarizer
    /// reported `messages_included` summarized messages for a cut of
    /// `cut_idx`. The summarizer contract (`provider::compress_messages`)
    /// counts an oldest prefix of the cut slice, so with full coverage this is
    /// `cut_idx`; a partial count drains only the summarized prefix and leaves
    /// the unsummarized remainder for a later pass. Zero is an error because
    /// inserting a summary without removing anything grows the session and
    /// re-triggers auto-compaction on every turn.
    pub fn compaction_drain_len(cut_idx: usize, messages_included: usize) -> anyhow::Result<usize> {
        let drain = messages_included.min(cut_idx);
        if drain == 0 {
            anyhow::bail!("compaction summarized no messages; nothing to drain");
        }
        Ok(drain)
    }
}

pub(crate) fn format_truncated_tool_output(
    output: &str,
    output_chars: usize,
    path: &Path,
) -> String {
    format_truncated_tool_output_with_notice(
        output,
        output_chars,
        &format!(
            "[full output saved to: {}; use the read tool on this path to inspect the complete output]",
            path.display()
        ),
    )
}

/// Preserve the live context bound even when artifact persistence fails. The
/// head and tail remain useful, while the notice makes the loss explicit.
pub(crate) fn format_unpersisted_truncated_tool_output(
    output: &str,
    output_chars: usize,
    error: &str,
) -> String {
    format_truncated_tool_output_with_notice(
        output,
        output_chars,
        &format!("[full output could not be saved: {error}]"),
    )
}

fn format_truncated_tool_output_with_notice(
    output: &str,
    output_chars: usize,
    notice: &str,
) -> String {
    let head: String = output.chars().take(TOOL_RESULT_HEAD_CHARS).collect();
    let tail_start = output_chars.saturating_sub(TOOL_RESULT_TAIL_CHARS);
    let tail: String = output.chars().skip(tail_start).collect();
    let omitted = output_chars.saturating_sub(TOOL_RESULT_HEAD_CHARS + TOOL_RESULT_TAIL_CHARS);

    format!(
        "{head}\n\n[tool output truncated: {output_chars} characters; {omitted} omitted]\n{notice}\n\n{tail}"
    )
}

pub fn cleared_tool_result_notice(artifact_path: Option<&str>) -> String {
    match artifact_path {
        Some(path) => format!(
            "[result cleared: old tool output omitted from the live context; read the spill file at {path} or re-run the tool]"
        ),
        None => CLEARED_TOOL_RESULT_NOTICE.to_string(),
    }
}

fn projected_cleared_tool_result(message: &SessionMessage) -> (String, String) {
    let artifact_path = match &message.tool {
        Some(PersistedToolMessage::Result { artifact_path, .. }) => artifact_path.as_deref(),
        _ => None,
    };
    let notice = cleared_tool_result_notice(artifact_path);
    let content = match message.content.split_once('\n') {
        Some((label, _)) => format!("{label}\n{notice}"),
        None => notice.clone(),
    };
    (content, notice)
}

fn projected_tool_result_tokens(message: &SessionMessage) -> u64 {
    let (content, _) = projected_cleared_tool_result(message);
    Session::estimate_tokens(&content)
}

fn apply_token_estimate_delta(value: u64, old: u64, new: u64) -> u64 {
    if old >= new {
        value.saturating_sub(old.saturating_sub(new))
    } else {
        value.saturating_add(new.saturating_sub(old))
    }
}

#[cfg(test)]
mod preflight_tests {
    use super::*;

    fn session_with(context_window: u64, overhead: u64, message_tokens: u64) -> Session {
        let mut s = Session::new("openai", "model", context_window, "");
        s.overhead_tokens = overhead;
        if message_tokens > 0 {
            s.add_message(MessageRole::User, &"x".repeat(message_tokens as usize));
        }
        s
    }

    #[test]
    fn needs_compaction_with_pending_zero_window_never_triggers() {
        let s = session_with(0, 0, 0);
        assert!(!s.needs_compaction_with_pending(0, 999_999));
    }

    #[test]
    fn needs_compaction_with_pending_fits_when_sum_under_budget() {
        // window=100, reserve=20, budget=80; effective=30, pending=40 => 70 <= 80
        let s = session_with(100, 20, 10);
        assert!(!s.needs_compaction_with_pending(20, 40));
    }

    #[test]
    fn needs_compaction_with_pending_triggers_when_pending_overflows() {
        // window=100, reserve=20, budget=80; effective=30, pending=60 => 90 > 80
        let s = session_with(100, 20, 10);
        assert!(s.needs_compaction_with_pending(20, 60));
    }

    #[test]
    fn needs_compaction_with_pending_saturates_arithmetic_safely() {
        let s = session_with(100, 0, 0);
        // Huge pending: saturating_add must not panic or wrap.
        assert!(s.needs_compaction_with_pending(0, u64::MAX));
    }

    #[test]
    fn is_irreducible_zero_window_never_triggers() {
        let s = session_with(0, 0, 0);
        assert!(!s.is_irreducible_with_pending(0, 999_999));
    }

    #[test]
    fn is_irreducible_when_overhead_plus_pending_fills_window() {
        // window=100, reserve=20, budget=80; overhead=40, pending=40 => 80 >= 80
        let s = session_with(100, 40, 0);
        assert!(s.is_irreducible_with_pending(20, 40));
    }

    #[test]
    fn is_irreducible_false_when_overhead_plus_pending_fits() {
        // window=100, reserve=20, budget=80; overhead=20, pending=30 => 50 < 80
        let s = session_with(100, 20, 0);
        assert!(!s.is_irreducible_with_pending(20, 30));
    }

    #[test]
    fn is_irreducible_saturates_arithmetic_safely() {
        let s = session_with(100, 0, 0);
        assert!(s.is_irreducible_with_pending(0, u64::MAX));
    }

    #[test]
    fn calibrated_session_needs_compaction_with_pending_uses_calibrated_anchor() {
        let mut s = session_with(200, 0, 0);
        // Simulate calibration: 150 tokens used, 5 messages counted.
        s.calibrated_tokens = 150;
        s.calibrated_msg_count = 0;
        // calibrated=150, pending=30 => 180; window=200, reserve=10, budget=190 => no compact
        assert!(!s.needs_compaction_with_pending(10, 30));
        // pending=60 => 210 > 190 => compact needed
        assert!(s.needs_compaction_with_pending(10, 60));
    }

    #[test]
    fn unknown_context_window_zero_never_triggers() {
        // A session where context_window is still 0 (unknown) must not fire.
        let s = session_with(0, 50, 20);
        assert!(!s.needs_compaction_with_pending(10, 100));
        assert!(!s.is_irreducible_with_pending(10, 100));
    }

    #[test]
    fn request_time_tool_result_pruning_keeps_recent_results_and_spill_recovery() {
        let mut session = Session::new("openai", "model", 10_000, "");
        for index in 0..3 {
            let id = format!("call-{index}");
            session.add_tool_call_with_id(
                &id,
                "read",
                &serde_json::json!({"path": format!("file-{index}")}),
            );
            session.add_tool_result_with_id(
                &id,
                "read",
                &format!("output-{index}-{}", "x".repeat(400)),
            );
        }
        if let Some(PersistedToolMessage::Result { artifact_path, .. }) =
            &mut session.messages[1].tool
        {
            *artifact_path = Some("/trusted/spill/call-0.txt".into());
        } else {
            panic!("first result must have structured storage");
        }

        let durable_first = session.messages[1].clone();
        let projected = session.context_messages_with_pruned_tool_results(1);
        let projected_outputs = projected
            .iter()
            .filter_map(|message| match &message.tool {
                Some(PersistedToolMessage::Result { output, .. }) => Some(output.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(projected_outputs.len(), 3);
        assert!(projected_outputs[0].contains("/trusted/spill/call-0.txt"));
        assert_eq!(projected_outputs[1], CLEARED_TOOL_RESULT_NOTICE);
        assert!(projected_outputs[2].starts_with("output-2-"));
        assert_eq!(session.messages[1].content, durable_first.content);
        assert_eq!(session.messages[1].tool, durable_first.tool);
    }

    #[test]
    fn compaction_pressure_uses_the_pruned_live_projection() {
        let mut session = Session::new("openai", "model", 10_000, "");
        for index in 0..3 {
            let id = format!("call-{index}");
            session.add_tool_call_with_id(&id, "bash", &serde_json::json!({"command": index}));
            session.add_tool_result_with_id(&id, "bash", &"x".repeat(800));
        }

        let full = session.effective_context_tokens();
        let pruned = session.effective_context_tokens_after_tool_result_pruning(1);
        assert!(pruned < full);
        let budget = pruned + (full - pruned) / 2;
        session.context_window = full;
        let reserve = full - budget;

        assert!(session.needs_compaction(reserve));
        assert!(!session.needs_compaction_after_tool_result_pruning(reserve, 1));
        assert!(
            session.needs_compaction_with_pending_after_tool_result_pruning(
                reserve,
                full - pruned,
                1,
            )
        );
    }

    #[test]
    fn calibrated_pruned_context_is_not_discounted_twice() {
        let mut session = Session::new("openai", "model", 10_000, "");
        for index in 0..3 {
            let id = format!("call-{index}");
            session.add_tool_call_with_id(&id, "bash", &serde_json::json!({"command": index}));
            session.add_tool_result_with_id(&id, "bash", &"x".repeat(800));
        }

        let cleared = session.tool_results_cleared_for_retention(1);
        session.set_calibration_with_cleared_tool_results(1_000, 50, cleared);
        assert_eq!(
            session.effective_context_tokens_after_tool_result_pruning(1),
            1_050,
            "the provider snapshot already includes the same cleared results"
        );
        assert!(
            session.effective_context_tokens_after_tool_result_pruning(0) < 1_050,
            "clearing the formerly retained result should reduce the calibrated snapshot"
        );
    }

    #[test]
    fn results_added_after_a_pruned_request_are_not_assumed_to_be_cleared() {
        let mut session = Session::new("openai", "model", 10_000, "");
        for index in 0..3 {
            let id = format!("prior-{index}");
            session.add_tool_call_with_id(&id, "read", &serde_json::json!({"path": index}));
            session.add_tool_result_with_id(&id, "read", &"p".repeat(800));
        }
        let cleared_in_request = session.tool_results_cleared_for_retention(1);
        assert_eq!(cleared_in_request, 2);

        // Results produced inside that request remain verbatim in Rig's
        // continuation context, even when the turn produces more than K of
        // them. Provider calibration therefore only represents the two
        // results cleared when the request history was assembled.
        for index in 0..2 {
            let id = format!("current-{index}");
            session.add_tool_call_with_id(&id, "read", &serde_json::json!({"path": index}));
            session.add_tool_result_with_id(&id, "read", &"c".repeat(800));
        }
        session.set_calibration_with_cleared_tool_results(2_000, 50, cleared_in_request);

        let next_turn = session.effective_context_tokens_after_tool_result_pruning(1);
        assert!(
            next_turn < 2_050,
            "the next turn should newly clear the two in-turn results that the calibrated request kept verbatim"
        );
    }

    #[test]
    fn live_spill_handoff_accepts_internal_id_or_exact_output_fallback_once() {
        let store = ToolResultSpillStore::default();
        let first = std::path::PathBuf::from("/private/first.txt");
        store.register("internal-1", None, "grep", "bounded-one", first.clone());
        assert_eq!(store.take("internal-1", "grep", "bounded-one"), Some(first));
        assert!(store.take("internal-1", "grep", "bounded-one").is_none());

        let second = std::path::PathBuf::from("/private/second.txt");
        store.register("internal-2", None, "shell", "bounded-two", second.clone());
        assert_eq!(
            store.take("provider-result-id", "shell", "bounded-two"),
            Some(second)
        );
        assert!(store.take("internal-2", "shell", "bounded-two").is_none());
    }

    #[test]
    fn failed_live_spill_still_drops_the_oversized_middle() {
        let output = format!(
            "{}{}{}",
            "H".repeat(TOOL_RESULT_HEAD_CHARS),
            "M".repeat(TOOL_RESULT_SAVE_THRESHOLD),
            "T".repeat(TOOL_RESULT_TAIL_CHARS),
        );
        let bounded = format_unpersisted_truncated_tool_output(
            &output,
            output.chars().count(),
            "storage unavailable",
        );

        assert!(bounded.starts_with(&"H".repeat(TOOL_RESULT_HEAD_CHARS)));
        assert!(bounded.ends_with(&"T".repeat(TOOL_RESULT_TAIL_CHARS)));
        assert!(bounded.contains("storage unavailable"));
        assert!(!bounded.contains(&"M".repeat(80)));
        assert!(bounded.chars().count() < TOOL_RESULT_SAVE_THRESHOLD);
    }
}
