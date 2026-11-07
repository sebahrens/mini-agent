//! Workspace files the agent has read, edited, written or listed, remembered so
//! `paths:`-scoped skills can activate on files the session works on and not
//! only on files the user's request names.
//!
//! The record is process-wide, keyed by workspace root and bounded: at most
//! [`MAX_PER_ROOT`] distinct paths per root (oldest dropped first) and
//! [`MAX_ROOTS`] roots. Only workspace-relative, traversal-free paths are kept;
//! a path outside the workspace is ignored. Sessions that share a workspace
//! root share the record, which is conservative: it can only make a scoped
//! skill eligible, never grant any capability.

use std::collections::{HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::{LazyLock, Mutex};

const MAX_PER_ROOT: usize = 512;
const MAX_ROOTS: usize = 16;

static TOUCHED: LazyLock<Mutex<HashMap<PathBuf, VecDeque<String>>>> =
    LazyLock::new(Default::default);

/// Workspace-relative `/`-separated form of `path`, or `None` when it is empty,
/// escapes the workspace, or is an absolute path outside `root`.
pub(crate) fn relative_to_workspace(root: &Path, path: &Path) -> Option<String> {
    let relative = if path.is_absolute() {
        path.strip_prefix(root).ok()?
    } else {
        path
    };
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?.to_owned()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Remembers that the agent touched `path` (absolute under `root`, or relative to it).
pub(crate) fn record(root: &Path, path: &Path) {
    let Some(relative) = relative_to_workspace(root, path) else {
        return;
    };
    let mut map = TOUCHED.lock().unwrap_or_else(|e| e.into_inner());
    if !map.contains_key(root) && map.len() >= MAX_ROOTS {
        map.clear();
    }
    let list = map.entry(root.to_path_buf()).or_default();
    if let Some(position) = list.iter().position(|existing| *existing == relative) {
        list.remove(position);
    }
    list.push_back(relative);
    while list.len() > MAX_PER_ROOT {
        list.pop_front();
    }
}

/// Files touched so far under `root`, oldest first.
pub(crate) fn snapshot(root: &Path) -> Vec<String> {
    TOUCHED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(root)
        .map(|list| list.iter().cloned().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_relative_and_absolute_under_root_and_rejects_escapes() {
        let root = PathBuf::from("/tmp/touched-unit-root");
        record(&root, Path::new("src/a.rs"));
        record(&root, Path::new("/tmp/touched-unit-root/./src/b.rs"));
        record(&root, Path::new("../etc/passwd"));
        record(&root, Path::new("/etc/passwd"));
        record(&root, Path::new("src/a.rs"));
        assert_eq!(snapshot(&root), vec!["src/b.rs", "src/a.rs"]);
        assert!(snapshot(Path::new("/tmp/other-untouched-root")).is_empty());
    }

    #[test]
    fn record_is_bounded_per_root() {
        let root = PathBuf::from("/tmp/touched-bound-root");
        for i in 0..(MAX_PER_ROOT + 10) {
            record(&root, Path::new(&format!("f{i}.rs")));
        }
        let all = snapshot(&root);
        assert_eq!(all.len(), MAX_PER_ROOT);
        assert_eq!(all[0], "f10.rs");
    }
}
