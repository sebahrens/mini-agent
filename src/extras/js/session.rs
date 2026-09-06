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
    Unavailable,
}

#[derive(Debug, Default)]
struct ScratchEntries {
    values: BTreeMap<String, String>,
    total_bytes: usize,
}

/// Bounded, parent-owned JSON scratch values for one logical session and workspace.
#[derive(Clone, Debug, Default)]
pub(crate) struct ScratchStore(Arc<Mutex<ScratchEntries>>);

impl ScratchStore {
    pub(crate) fn get(&self, key: &str) -> Result<Option<String>, SessionStateError> {
        validate_scratch_key(key)?;
        self.0
            .lock()
            .map(|entries| entries.values.get(key).cloned())
            .map_err(|_| SessionStateError::Unavailable)
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
            return Err(SessionStateError::TooLarge);
        }
        if !entries.values.contains_key(&key) && entries.values.len() >= SCRATCH_MAX_ENTRIES {
            return Err(SessionStateError::TooLarge);
        }
        entries.values.insert(key, json);
        entries.total_bytes = next_total;
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
            Err(SessionStateError::TooLarge)
        );

        let bytes = ScratchStore::default();
        let chunk = format!("\"{}\"", "0".repeat(SCRATCH_VALUE_MAX_BYTES - 2));
        for index in 0..(SCRATCH_TOTAL_MAX_BYTES / SCRATCH_VALUE_MAX_BYTES) {
            bytes.put(format!("chunk:{index}"), chunk.clone()).unwrap();
        }
        assert_eq!(
            bytes.put("chunk:overflow".into(), "0".into()),
            Err(SessionStateError::TooLarge)
        );

        assert_eq!(
            ScratchStore::default().put("invalid".into(), "not-json".into()),
            Err(SessionStateError::Invalid)
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
