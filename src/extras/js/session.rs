//! Process-local state shared by JavaScript tools in one logical session.
//!
//! QuickJS runtimes remain disposable. Only validated JSON strings live here, under a
//! workspace-bound owner that survives agent rebuilds and is never serialized with a session.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub(crate) const STRUCTURED_RESULT_MAX_BYTES: usize = 256 * 1024;
pub(crate) const SCRATCH_KEY_MAX_BYTES: usize = 128;
pub(crate) const SCRATCH_VALUE_MAX_BYTES: usize = 1024 * 1024;
pub(crate) const SCRATCH_TOTAL_MAX_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const SCRATCH_MAX_ENTRIES: usize = 128;
pub(crate) const JSON_MAX_DEPTH: usize = 64;
pub(crate) const JSON_MAX_NODES: usize = 100_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionStateError {
    Invalid,
    TooLarge,
    /// A new key while the store already holds [`SCRATCH_MAX_ENTRIES`] entries.
    EntryLimit,
    /// A value that would push the store past [`SCRATCH_TOTAL_MAX_BYTES`].
    TotalBytesLimit,
    Unavailable,
}

#[derive(Debug, Default)]
struct ScratchEntries {
    values: BTreeMap<String, String>,
    total_bytes: usize,
    /// Any `scratch_put` has succeeded in this process.
    ever_written: bool,
    /// The one-time "store is empty, values may have been lost" note was already queued.
    miss_noted: bool,
    /// A note is waiting for the next `js` result.
    pending_note: bool,
}

/// One-line note appended to a `js` result when `scratch_get` finds nothing in a store that has
/// never been written in this process: the store is process-local and is not serialized with a
/// session, so after `--resume`, a restart or a new headless process an earlier `scratch_put`
/// in the transcript is gone. Without the note a null reads as "never stored".
pub(crate) const SCRATCH_RESET_NOTE: &str = "[note: the scratch store is empty. It is process-local and does not survive --resume, a restart or a new headless run, so a null from scratch_get may mean a value stored earlier in the conversation was lost, not that it was never stored. Recompute it and scratch_put it again.]";

/// Bounded, parent-owned JSON scratch values for one logical session and workspace.
#[derive(Clone, Debug, Default)]
pub(crate) struct ScratchStore(Arc<Mutex<ScratchEntries>>);

impl ScratchStore {
    pub(crate) fn get(&self, key: &str) -> Result<Option<String>, SessionStateError> {
        validate_scratch_key(key)?;
        let mut entries = self.0.lock().map_err(|_| SessionStateError::Unavailable)?;
        let found = entries.values.get(key).cloned();
        if found.is_none() && !entries.ever_written && !entries.miss_noted {
            entries.miss_noted = true;
            entries.pending_note = true;
        }
        Ok(found)
    }

    /// Remove a key; reports whether it existed. Frees its bytes and its entry slot.
    pub(crate) fn delete(&self, key: &str) -> Result<bool, SessionStateError> {
        validate_scratch_key(key)?;
        let mut entries = self.0.lock().map_err(|_| SessionStateError::Unavailable)?;
        match entries.values.remove(key) {
            Some(removed) => {
                entries.total_bytes = entries.total_bytes.saturating_sub(removed.len());
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Every stored key, sorted (at most [`SCRATCH_MAX_ENTRIES`]).
    pub(crate) fn keys(&self) -> Result<Vec<String>, SessionStateError> {
        self.0
            .lock()
            .map(|entries| entries.values.keys().cloned().collect())
            .map_err(|_| SessionStateError::Unavailable)
    }

    /// Takes the reset note queued by a `scratch_get` miss on a never-written store.
    pub(crate) fn take_reset_note(&self) -> Option<&'static str> {
        let mut entries = self.0.lock().ok()?;
        std::mem::take(&mut entries.pending_note).then_some(SCRATCH_RESET_NOTE)
    }

    pub(crate) fn put(&self, key: String, json: String) -> Result<(), SessionStateError> {
        validate_scratch_key(&key)?;
        let json = canonical_json(&json, SCRATCH_VALUE_MAX_BYTES)?;
        let mut entries = self.0.lock().map_err(|_| SessionStateError::Unavailable)?;
        let prior_bytes = entries.values.get(&key).map_or(0, String::len);
        let next_total = entries
            .total_bytes
            .checked_sub(prior_bytes)
            .and_then(|bytes| bytes.checked_add(json.len()))
            .ok_or(SessionStateError::TooLarge)?;
        if next_total > SCRATCH_TOTAL_MAX_BYTES {
            return Err(SessionStateError::TotalBytesLimit);
        }
        if !entries.values.contains_key(&key) && entries.values.len() >= SCRATCH_MAX_ENTRIES {
            return Err(SessionStateError::EntryLimit);
        }
        entries.values.insert(key, json);
        entries.total_bytes = next_total;
        entries.ever_written = true;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values
            .len()
    }
}

/// Parent-side proof that a structured result completed its audited effect lifecycle.
///
/// The worker's `StepOutcome` is untrusted wire data. A fresh receipt is attached to each
/// invocation and populated by the broker only after the result effect's completion record is
/// durable, so the tool can reject fabricated or mismatched structured outcomes.
#[derive(Clone, Debug, Default)]
pub(crate) struct StructuredResultReceipt(Arc<Mutex<Option<String>>>);

impl StructuredResultReceipt {
    pub(crate) fn record(&self, json: String) -> Result<(), SessionStateError> {
        let mut accepted = self.0.lock().map_err(|_| SessionStateError::Unavailable)?;
        if accepted.is_some() {
            return Err(SessionStateError::Invalid);
        }
        *accepted = Some(json);
        Ok(())
    }

    pub(crate) fn accepted(&self) -> Result<Option<String>, SessionStateError> {
        self.0
            .lock()
            .map(|accepted| accepted.clone())
            .map_err(|_| SessionStateError::Unavailable)
    }
}

#[derive(Debug)]
struct WorkspaceSlot {
    root: PathBuf,
    scratch: ScratchStore,
}

/// Session lifetime owner. Rebuilding an agent in the same workspace reuses its scratch store;
/// changing workspace authority replaces it with a fresh store.
#[derive(Clone, Debug, Default)]
pub(crate) struct JsSessionStateOwner(Arc<Mutex<Option<WorkspaceSlot>>>);

impl JsSessionStateOwner {
    pub(crate) fn for_workspace(&self, root: &Path) -> ScratchStore {
        let mut slot = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(slot) = slot.as_ref()
            && slot.root == root
        {
            return slot.scratch.clone();
        }
        let scratch = ScratchStore::default();
        *slot = Some(WorkspaceSlot {
            root: root.to_path_buf(),
            scratch: scratch.clone(),
        });
        scratch
    }
}

pub(crate) fn validate_scratch_key(key: &str) -> Result<(), SessionStateError> {
    if key.is_empty()
        || key.len() > SCRATCH_KEY_MAX_BYTES
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
    {
        return Err(SessionStateError::Invalid);
    }
    Ok(())
}

/// Parse, structurally bound, and reserialize JSON in the parent process.
pub(crate) fn canonical_json(
    encoded: &str,
    maximum_bytes: usize,
) -> Result<String, SessionStateError> {
    if encoded.len() > maximum_bytes {
        return Err(SessionStateError::TooLarge);
    }
    let value: serde_json::Value =
        serde_json::from_str(encoded).map_err(|_| SessionStateError::Invalid)?;
    let mut nodes = 0usize;
    validate_json_shape(&value, 1, &mut nodes)?;
    let canonical = serde_json::to_string(&value).map_err(|_| SessionStateError::Unavailable)?;
    if canonical.len() > maximum_bytes {
        return Err(SessionStateError::TooLarge);
    }
    Ok(canonical)
}

fn validate_json_shape(
    value: &serde_json::Value,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), SessionStateError> {
    if depth > JSON_MAX_DEPTH {
        return Err(SessionStateError::TooLarge);
    }
    *nodes = nodes.checked_add(1).ok_or(SessionStateError::TooLarge)?;
    if *nodes > JSON_MAX_NODES {
        return Err(SessionStateError::TooLarge);
    }
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                validate_json_shape(value, depth + 1, nodes)?;
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values() {
                validate_json_shape(value, depth + 1, nodes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_is_atomic_when_total_limit_would_be_exceeded() {
        let store = ScratchStore::default();
        store.put("kept".into(), "1".into()).unwrap();
        let oversized = "x".repeat(SCRATCH_VALUE_MAX_BYTES + 1);
        assert_eq!(
            store.put("kept".into(), oversized),
            Err(SessionStateError::TooLarge)
        );
        assert_eq!(store.get("kept").unwrap().as_deref(), Some("1"));
    }

    #[test]
    fn scratch_store_enforces_entry_and_total_byte_caps() {
        let entries = ScratchStore::default();
        for index in 0..SCRATCH_MAX_ENTRIES {
            entries.put(format!("entry:{index}"), "0".into()).unwrap();
        }
        assert_eq!(entries.len(), SCRATCH_MAX_ENTRIES);
        assert_eq!(
            entries.put("entry:overflow".into(), "0".into()),
            Err(SessionStateError::EntryLimit)
        );

        let bytes = ScratchStore::default();
        let chunk = format!("\"{}\"", "0".repeat(SCRATCH_VALUE_MAX_BYTES - 2));
        for index in 0..(SCRATCH_TOTAL_MAX_BYTES / SCRATCH_VALUE_MAX_BYTES) {
            bytes.put(format!("chunk:{index}"), chunk.clone()).unwrap();
        }
        assert_eq!(
            bytes.put("chunk:overflow".into(), "0".into()),
            Err(SessionStateError::TotalBytesLimit)
        );

        assert_eq!(
            ScratchStore::default().put("invalid".into(), "not-json".into()),
            Err(SessionStateError::Invalid)
        );
    }

    #[test]
    fn delete_frees_the_entry_slot_and_its_bytes_and_keys_lists_what_is_left() {
        let store = ScratchStore::default();
        for index in 0..SCRATCH_MAX_ENTRIES {
            store.put(format!("k:{index:03}"), "\"v\"".into()).unwrap();
        }
        assert_eq!(
            store.put("extra".into(), "1".into()),
            Err(SessionStateError::EntryLimit)
        );
        assert_eq!(store.delete("k:000"), Ok(true));
        assert_eq!(
            store.delete("k:000"),
            Ok(false),
            "a second delete is a no-op"
        );
        store.put("extra".into(), "1".into()).unwrap();
        let keys = store.keys().unwrap();
        assert_eq!(keys.len(), SCRATCH_MAX_ENTRIES);
        assert!(keys.contains(&"extra".to_string()) && !keys.contains(&"k:000".to_string()));
        assert!(
            keys.windows(2).all(|pair| pair[0] < pair[1]),
            "keys are sorted"
        );
        assert_eq!(store.delete("bad/key"), Err(SessionStateError::Invalid));

        // Byte accounting: a big value's bytes come back after delete.
        let big = ScratchStore::default();
        let chunk = format!("\"{}\"", "0".repeat(SCRATCH_VALUE_MAX_BYTES - 2));
        for index in 0..(SCRATCH_TOTAL_MAX_BYTES / SCRATCH_VALUE_MAX_BYTES) {
            big.put(format!("chunk:{index}"), chunk.clone()).unwrap();
        }
        assert_eq!(
            big.put("more".into(), chunk.clone()),
            Err(SessionStateError::TotalBytesLimit)
        );
        assert_eq!(big.delete("chunk:0"), Ok(true));
        big.put("more".into(), chunk).unwrap();
    }

    #[test]
    fn a_miss_on_a_never_written_store_queues_one_reset_note() {
        let store = ScratchStore::default();
        assert_eq!(store.take_reset_note(), None);
        assert_eq!(store.get("answer").unwrap(), None);
        assert_eq!(store.take_reset_note(), Some(SCRATCH_RESET_NOTE));
        assert_eq!(store.get("answer").unwrap(), None);
        assert_eq!(store.take_reset_note(), None, "the note is queued once");

        let written = ScratchStore::default();
        written.put("a".into(), "1".into()).unwrap();
        assert_eq!(written.get("missing").unwrap(), None);
        assert_eq!(
            written.take_reset_note(),
            None,
            "a written store is not reset"
        );
    }

    #[test]
    fn workspace_rebind_clears_values_but_rebuild_reuses_them() {
        let owner = JsSessionStateOwner::default();
        let first = owner.for_workspace(Path::new("workspace-a"));
        first.put("answer".into(), "42".into()).unwrap();
        assert_eq!(
            owner
                .for_workspace(Path::new("workspace-a"))
                .get("answer")
                .unwrap()
                .as_deref(),
            Some("42")
        );
        assert_eq!(
            owner
                .for_workspace(Path::new("workspace-b"))
                .get("answer")
                .unwrap(),
            None
        );

        assert_eq!(
            JsSessionStateOwner::default()
                .for_workspace(Path::new("workspace-a"))
                .get("answer")
                .unwrap(),
            None,
            "independently opened sessions must not share scratch state"
        );
    }

    #[test]
    fn canonical_json_rejects_excessive_depth_and_keys_are_closed() {
        let nested = format!(
            "{}0{}",
            "[".repeat(JSON_MAX_DEPTH),
            "]".repeat(JSON_MAX_DEPTH)
        );
        assert_eq!(
            canonical_json(&nested, SCRATCH_VALUE_MAX_BYTES),
            Err(SessionStateError::TooLarge)
        );
        assert!(validate_scratch_key("turn:1.result").is_ok());
        assert_eq!(
            validate_scratch_key("secret/key"),
            Err(SessionStateError::Invalid)
        );

        assert_eq!(
            canonical_json("{not-json}", SCRATCH_VALUE_MAX_BYTES),
            Err(SessionStateError::Invalid)
        );
        let excessive_nodes = format!("[{}]", vec!["0"; JSON_MAX_NODES].join(","));
        assert_eq!(
            canonical_json(&excessive_nodes, SCRATCH_VALUE_MAX_BYTES),
            Err(SessionStateError::TooLarge)
        );
    }

    #[test]
    fn structured_result_receipt_is_single_assignment() {
        let receipt = StructuredResultReceipt::default();
        receipt.record("{\"answer\":42}".into()).unwrap();
        assert_eq!(
            receipt.accepted().unwrap().as_deref(),
            Some("{\"answer\":42}")
        );
        assert_eq!(
            receipt.record("{\"answer\":43}".into()),
            Err(SessionStateError::Invalid)
        );
    }
}
