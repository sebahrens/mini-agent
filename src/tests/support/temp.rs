//! Shared RAII temporary-directory helper for tests.
//!
//! Replaces the hand-rolled `struct TempDir(PathBuf)` guards. The directory is
//! created exclusively (never reusing a stale directory from an earlier run),
//! lives under `std::env::temp_dir()` and is removed on drop, ignoring errors.
//!
//! Integration tests under `tests/` include this file with
//! `#[path = "../src/tests/support/temp.rs"] mod temp;`, so it must not depend
//! on anything from the crate.
#![allow(dead_code)]

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A uniquely named directory removed (recursively) when dropped.
#[derive(Debug)]
pub(crate) struct TempDir {
    path: PathBuf,
    keep: bool,
}

impl TempDir {
    /// Creates `<temp_dir>/mini-agent-<label>-<pid>-<n>`.
    pub(crate) fn new(label: &str) -> Self {
        Self::create_in(&std::env::temp_dir(), label)
    }

    /// Like [`TempDir::new`], but rooted at the canonical temp directory so
    /// symlinked spellings (macOS `/var` -> `/private/var`) never appear.
    pub(crate) fn new_canonical(label: &str) -> Self {
        let root = std::env::temp_dir()
            .canonicalize()
            .expect("canonicalize the temp directory");
        Self::create_in(&root, label)
    }

    /// Like [`TempDir::new`], but under an explicit existing `parent`.
    pub(crate) fn new_in(parent: &Path, label: &str) -> Self {
        Self::create_in(parent, label)
    }

    fn create_in(parent: &Path, label: &str) -> Self {
        let pid = std::process::id();
        loop {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("mini-agent-{label}-{pid}-{n}"));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self { path, keep: false },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create temporary directory {}: {error}", path.display()),
            }
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Leaves the directory on disk when dropped (for debugging failures).
    pub(crate) fn keep(mut self) -> PathBuf {
        self.keep = true;
        self.path.clone()
    }
}

impl Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[test]
fn temp_dir_is_unique_exclusive_and_removed_on_drop() {
    let first = TempDir::new("support-temp");
    let second = TempDir::new("support-temp");
    assert_ne!(first.path(), second.path());
    assert!(first.is_dir() && second.is_dir());
    std::fs::write(first.join("file"), b"x").unwrap();
    let removed = first.path().to_path_buf();
    drop(first);
    assert!(!removed.exists());
    let canonical = TempDir::new_canonical("support-temp");
    assert_eq!(canonical.path(), canonical.canonicalize().unwrap());
    let kept = TempDir::new("support-keep").keep();
    assert!(kept.is_dir());
    std::fs::remove_dir_all(kept).unwrap();
}
