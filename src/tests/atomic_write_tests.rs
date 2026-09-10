//! Tests for `crate::fs::atomic_write`.
//!
//! These exercise the atomic-write contract: writes are atomic (a reader never
//! sees a truncated file), permissions are preserved, symlink redirection and
//! containment escapes are rejected, and no temp-file residue is left behind.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(windows)]
use crate::fs::atomic_write_resolved_checked_cancellable;
use crate::fs::atomic_write_within_sync;
use crate::fs::{
    AtomicWriteCancellation, atomic_create_resolved_checked_cancellable, atomic_create_sync,
    atomic_write, atomic_write_with_failure_sync,
};

/// A unique temp directory per call, removed on drop. Uniqueness (process id +
/// monotonic counter) keeps parallel test runs from colliding without pulling
/// in an external temp-dir crate.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "zerostack_atomic_test_{}_{}_{}",
            tag,
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Count leftover temp files created by `atomic_write` in a directory.
fn temp_residue(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().contains(".zswrite."))
                .count()
        })
        .unwrap_or(0)
}

#[tokio::test]
async fn creates_new_file() {
    let dir = TempDir::new("new");
    let f = dir.join("new.txt");
    atomic_write(&f, b"hello world").await.unwrap();
    assert_eq!(std::fs::read(&f).unwrap(), b"hello world");
    assert_eq!(temp_residue(dir.path()), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn atomic_write_security_creates_new_file_with_restrictive_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new("restrictive_permissions");
    let target = dir.join("private.txt");
    atomic_write(&target, b"private").await.unwrap();

    let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[tokio::test]
async fn overwrites_existing_file() {
    let dir = TempDir::new("overwrite");
    let f = dir.join("f.txt");
    std::fs::write(&f, b"old contents").unwrap();
    atomic_write(&f, b"new contents").await.unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "new contents");
    assert_eq!(temp_residue(dir.path()), 0);
}

#[test]
fn atomic_write_security_create_only_never_replaces_existing_target() {
    let dir = TempDir::new("create_only");
    let target = dir.join("target.txt");
    std::fs::write(&target, b"attacker-created").unwrap();

    assert!(atomic_create_sync(&target, b"replacement").is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"attacker-created");
    assert_eq!(temp_residue(dir.path()), 0);
}

#[cfg(windows)]
#[test]
fn windows_atomic_create_publishes_only_in_the_approved_directory() {
    let dir = TempDir::new("windows_directory_bound_create");
    let leaf = format!(".zswrite-directory-bound-{}.txt", uuid::Uuid::new_v4());
    let target = dir.join(&leaf);
    let cwd_target = std::env::current_dir().unwrap().join(&leaf);
    assert!(!cwd_target.exists());

    atomic_create_sync(&target, b"directory-bound").unwrap();

    assert_eq!(std::fs::read(&target).unwrap(), b"directory-bound");
    assert!(!cwd_target.exists());
    assert_eq!(temp_residue(dir.path()), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn preserves_permissions_on_overwrite() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("perms");
    let f = dir.join("script.sh");
    std::fs::write(&f, b"#!/bin/sh\necho old\n").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();

    atomic_write(&f, b"#!/bin/sh\necho new\n").await.unwrap();

    let mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o755,
        "executable bit must survive the atomic replace"
    );
    assert_eq!(
        std::fs::read_to_string(&f).unwrap(),
        "#!/bin/sh\necho new\n"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn atomic_write_security_rejects_final_symlink() {
    use std::os::unix::fs::symlink;
    let dir = TempDir::new("symlink");
    let outside = TempDir::new("symlink_outside");
    let real = outside.join("real.txt");
    let link = dir.join("link.txt");
    std::fs::write(&real, b"old").unwrap();
    symlink(&real, &link).unwrap();

    assert!(atomic_write(&link, b"hostile write").await.is_err());

    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the hostile symlink must not be replaced"
    );
    assert_eq!(std::fs::read_to_string(&real).unwrap(), "old");
    assert_eq!(temp_residue(dir.path()), 0);
}

#[cfg(unix)]
#[test]
fn atomic_write_security_rejects_parent_symlink_swap() {
    use std::os::unix::fs::symlink;
    let root = TempDir::new("parent_swap");
    let outside = TempDir::new("parent_swap_outside");
    let approved_parent = root.join("approved");
    std::fs::create_dir(&approved_parent).unwrap();
    std::fs::remove_dir(&approved_parent).unwrap();
    symlink(outside.path(), &approved_parent).unwrap();

    let target = approved_parent.join("escaped.txt");
    assert!(atomic_write_within_sync(root.path(), &target, b"escape").is_err());
    assert!(!outside.join("escaped.txt").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn atomic_write_security_does_not_follow_symlinked_destination_directory() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new("symlinked_parent");
    let outside = TempDir::new("symlinked_parent_outside");
    let parent = root.join("approved");
    symlink(outside.path(), &parent).unwrap();

    let target = parent.join("escaped.txt");
    assert!(atomic_write(&target, b"escape").await.is_err());
    assert!(!outside.join("escaped.txt").exists());
}

#[cfg(unix)]
#[test]
fn atomic_write_security_rejects_sibling_prefix() {
    let parent = TempDir::new("sibling_prefix");
    let approved_root = parent.join("safe-root");
    let sibling = parent.join("safe-root-evil");
    std::fs::create_dir(&approved_root).unwrap();
    std::fs::create_dir(&sibling).unwrap();

    let target = sibling.join("escaped.txt");
    assert!(atomic_write_within_sync(&approved_root, &target, b"escape").is_err());
    assert!(!target.exists());
}

#[tokio::test]
async fn atomic_write_security_rejects_destination_directory_without_residue() {
    let dir = TempDir::new("destination_directory");
    let target = dir.join("target");
    std::fs::create_dir(&target).unwrap();

    assert!(atomic_write(&target, b"replacement").await.is_err());
    assert!(target.is_dir());
    assert_eq!(temp_residue(dir.path()), 0);
}

#[tokio::test]
async fn atomic_write_security_does_not_touch_precreated_predictable_temp_name() {
    let dir = TempDir::new("precreated_temp");
    let target = dir.join("target.txt");
    let predictable = dir
        .path()
        .join(format!(".target.txt.zswrite.{}.0.tmp", std::process::id()));
    std::fs::write(&predictable, b"attacker-owned").unwrap();

    atomic_write(&target, b"complete").await.unwrap();

    assert_eq!(std::fs::read(&target).unwrap(), b"complete");
    assert_eq!(
        std::fs::read(&predictable).unwrap(),
        b"attacker-owned",
        "cleanup must not delete or truncate a precreated attacker path"
    );
}

#[test]
fn atomic_write_security_injected_failures_preserve_prior_file_and_clean_temp() {
    let dir = TempDir::new("injected_failures");
    let target = dir.join("target.txt");
    std::fs::write(&target, b"prior complete contents").unwrap();

    for fail_rename in [false, true] {
        assert!(
            atomic_write_with_failure_sync(
                dir.path(),
                &target,
                b"incomplete replacement",
                fail_rename,
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"prior complete contents");
        assert_eq!(temp_residue(dir.path()), 0);
    }
}

#[tokio::test]
async fn concurrent_writes_to_distinct_files_leave_no_residue() {
    let dir = TempDir::new("concurrent");
    let mut handles = Vec::new();
    for i in 0..50 {
        let p = dir.join(&format!("p{i}.txt"));
        handles.push(tokio::spawn(async move {
            atomic_write(&p, format!("file {i}").into_bytes())
                .await
                .unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    for i in 0..50 {
        let p = dir.join(&format!("p{i}.txt"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), format!("file {i}"));
    }
    assert_eq!(temp_residue(dir.path()), 0);
}

#[tokio::test]
async fn atomic_write_security_concurrent_writers_publish_only_complete_values() {
    let dir = TempDir::new("concurrent_same_target");
    let target = dir.join("target.txt");
    let mut handles = Vec::new();
    for i in 0..32 {
        let path = target.clone();
        handles.push(tokio::spawn(async move {
            let byte = b'A' + (i % 26) as u8;
            atomic_write(&path, vec![byte; 16 * 1024]).await
        }));
    }
    let mut successes = 0;
    for handle in handles {
        if handle.await.unwrap().is_ok() {
            successes += 1;
        }
    }

    assert!(successes >= 1);
    let contents = std::fs::read(&target).unwrap();
    assert_eq!(contents.len(), 16 * 1024);
    assert!(contents.iter().all(|byte| *byte == contents[0]));
    assert_eq!(temp_residue(dir.path()), 0);
}

#[tokio::test]
async fn atomic_write_cancellation_before_and_after_publication_preserves_the_decision() {
    for publication_started in [false, true] {
        let dir = TempDir::new("cancel_publication_gate");
        let target = dir.join("target.txt");
        let parent = crate::fs::checked_path_metadata(dir.path()).unwrap();
        let (cancellation, probe) = if publication_started {
            AtomicWriteCancellation::with_blocking_publication_probe_for_test()
        } else {
            AtomicWriteCancellation::with_publication_probe_for_test()
        };
        let writer_cancellation = cancellation.clone();
        let writer_target = target.clone();
        let writer = tokio::spawn(async move {
            atomic_create_resolved_checked_cancellable(
                writer_target,
                b"complete publication",
                parent,
                writer_cancellation,
            )
            .await
        });
        let reached = probe.clone();
        tokio::task::spawn_blocking(move || reached.wait_until_reached())
            .await
            .unwrap();
        let (cancelled_tx, cancelled_rx) = std::sync::mpsc::channel();
        let cancel = std::thread::spawn(move || {
            cancellation.cancel();
            let _ = cancelled_tx.send(());
        });
        let observation = cancelled_rx.recv_timeout(std::time::Duration::from_secs(2));
        let published_while_paused = target.exists();
        // Release and settle first, including when testing a blocking cancel
        // regression. Assertions must never strand a writer at its checkpoint.
        probe.resume();
        let result = writer.await;
        let cancellation_result = cancel.join();
        observation.expect("cancellation waited for the writer's publication");
        cancellation_result.unwrap();
        assert!(!published_while_paused);
        let result = result.unwrap();
        if publication_started {
            result.unwrap();
            assert_eq!(std::fs::read(&target).unwrap(), b"complete publication");
        } else {
            assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Interrupted);
            assert!(
                !target.exists(),
                "cancelled writer published after its gate"
            );
        }
        assert_eq!(temp_residue(dir.path()), 0);
    }
}

#[cfg(windows)]
#[test]
fn windows_checked_metadata_and_live_reader_allow_atomic_target_replacement() {
    use std::io::Read;
    let directory = TempDir::new("windows_checked_metadata_replace");
    let target = directory.join("config.toml");
    std::fs::write(&target, b"before").unwrap();
    let mut reader = std::fs::File::open(&target).unwrap();
    let checked = crate::fs::checked_file_metadata(&reader).unwrap();

    atomic_write_within_sync(directory.path(), &target, b"after").unwrap();

    assert_eq!(std::fs::read(&target).unwrap(), b"after");
    let mut old = String::new();
    reader.read_to_string(&mut old).unwrap();
    assert_eq!(old, "before", "existing readers must retain the old object");
    let published = crate::fs::checked_path_metadata(&target).unwrap();
    assert!(crate::fs::ensure_same_file(&target, &checked, &published).is_err());
    assert_eq!(temp_residue(directory.path()), 0);
}

#[cfg(windows)]
fn windows_open_without_delete_sharing(path: &Path) -> std::fs::File {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)
        .unwrap()
}

#[test]
fn windows_lock_retry_classifies_errors_cancellation_and_deadline() {
    fn controlled<T>(
        operation: impl FnMut() -> std::io::Result<T>,
        cancelled: impl Fn() -> bool,
    ) -> std::io::Result<T> {
        let clock = std::cell::Cell::new(std::time::Instant::now());
        crate::fs::retry_windows_lock_violation_with(
            operation,
            cancelled,
            || clock.get(),
            |delay| clock.set(clock.get() + delay),
        )
    }
    use std::sync::atomic::AtomicUsize;

    for code in [32, 33] {
        let attempts = AtomicUsize::new(0);
        controlled(
            || {
                if attempts.fetch_add(1, Ordering::Relaxed) == 0 {
                    Err(std::io::Error::from_raw_os_error(code))
                } else {
                    Ok(())
                }
            },
            || false,
        )
        .unwrap();
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
    }

    let attempts = AtomicUsize::new(0);
    let error = controlled(
        || {
            attempts.fetch_add(1, Ordering::Relaxed);
            Err::<(), _>(std::io::Error::from_raw_os_error(5))
        },
        || false,
    )
    .unwrap_err();
    assert_eq!(error.raw_os_error(), Some(5));
    assert_eq!(attempts.load(Ordering::Relaxed), 1);

    let attempts = AtomicUsize::new(0);
    let error = controlled(
        || {
            attempts.fetch_add(1, Ordering::Relaxed);
            Err::<(), _>(std::io::Error::from_raw_os_error(32))
        },
        || true,
    )
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
    assert_eq!(attempts.load(Ordering::Relaxed), 1);

    let attempts = AtomicUsize::new(0);
    let error = controlled(
        || {
            attempts.fetch_add(1, Ordering::Relaxed);
            Err::<(), _>(std::io::Error::from_raw_os_error(33))
        },
        || false,
    )
    .unwrap_err();
    assert_eq!(error.raw_os_error(), Some(33));
    assert!(attempts.load(Ordering::Relaxed) > 1);
    let cancelled = std::cell::Cell::new(false);
    let clock = std::cell::Cell::new(std::time::Instant::now());
    let attempts = std::cell::Cell::new(0);
    let error = crate::fs::retry_windows_lock_violation_with(
        || {
            attempts.set(attempts.get() + 1);
            Err::<(), _>(std::io::Error::from_raw_os_error(32))
        },
        || cancelled.get(),
        || clock.get(),
        |delay| {
            clock.set(clock.get() + delay);
            cancelled.set(true);
        },
    )
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
    assert_eq!(
        attempts.get(),
        1,
        "cancellation during backoff must prevent another native attempt"
    );
}

#[cfg(windows)]
async fn exercise_windows_publication_retry(cancel: bool) {
    use futures::FutureExt;
    let directory = TempDir::new("windows_retry_boundary");
    let target = directory.join("target.txt");
    std::fs::write(&target, b"old-complete").unwrap();
    let mut locked = Some(windows_open_without_delete_sharing(&target));
    let parent = crate::fs::checked_path_metadata(directory.path()).unwrap();
    let (cancellation, probe) = AtomicWriteCancellation::with_retry_probe_for_test();
    let writer_cancellation = cancellation.clone();
    let writer_target = target.clone();
    let mut writer = Some(tokio::spawn(async move {
        atomic_write_resolved_checked_cancellable(
            writer_target,
            b"new-complete",
            parent,
            writer_cancellation,
        )
        .await
    }));
    let reached = probe.clone();
    let readiness = tokio::task::spawn_blocking(move || reached.wait_until_reached());
    let readiness = readiness.await;
    let scenario = std::panic::AssertUnwindSafe(async {
        readiness.expect("writer never reached actual sharing violation");
        assert_eq!(std::fs::read(&target).unwrap(), b"old-complete");
        if cancel {
            cancellation.cancel();
        } else {
            drop(locked.take());
        }
        probe.resume();
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(15), writer.as_mut().unwrap())
                .await
                .expect("writer stalled after retry release");
        writer.take();
        let result = result.unwrap();
        if cancel {
            assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Interrupted);
        } else {
            result.unwrap();
        }
        assert_eq!(
            std::fs::read(&target).unwrap(),
            if cancel {
                b"old-complete"
            } else {
                b"new-complete"
            }
        );
        assert_eq!(temp_residue(directory.path()), 0);
    })
    .catch_unwind()
    .await;
    cancellation.cancel();
    drop(locked);
    probe.resume();
    if let Some(writer) = writer {
        let _ = writer.await;
    }
    if let Err(payload) = scenario {
        std::panic::resume_unwind(payload);
    }
}

#[cfg(windows)]
#[tokio::test]
async fn windows_atomic_replace_retries_a_transient_target_lock() {
    exercise_windows_publication_retry(false).await;
}

#[cfg(windows)]
#[test]
fn windows_atomic_replace_persistent_lock_preserves_old_content_and_cleans_temp() {
    let directory = TempDir::new("windows_persistent_replace_lock");
    let target = directory.join("target.txt");
    std::fs::write(&target, b"old-complete").unwrap();
    let _locked = windows_open_without_delete_sharing(&target);

    let error = atomic_write_within_sync(directory.path(), &target, b"new-complete").unwrap_err();

    assert!(matches!(error.raw_os_error(), Some(32 | 33)));
    assert_eq!(std::fs::read(&target).unwrap(), b"old-complete");
    assert_eq!(temp_residue(directory.path()), 0);
}

#[cfg(windows)]
#[tokio::test]
async fn windows_atomic_replace_cancellation_interrupts_lock_retry() {
    exercise_windows_publication_retry(true).await;
}

#[cfg(windows)]
#[tokio::test]
async fn windows_atomic_replace_publishes_to_the_current_verified_target() {
    let container = TempDir::new("windows_replace_parent_swap");
    let approved_path = container.join("approved");
    let moved_approved_path = container.join("approved-held");
    std::fs::create_dir(&approved_path).unwrap();
    let target = approved_path.join("target.txt");
    std::fs::write(&target, b"approved-original").unwrap();
    std::fs::rename(&approved_path, &moved_approved_path).unwrap();
    let moved_target = moved_approved_path.join("target.txt");
    let attacker = moved_approved_path.join("attacker.txt");
    let displaced = moved_approved_path.join("displaced.txt");
    std::fs::write(&attacker, b"attacker-y").unwrap();
    std::fs::rename(&moved_target, &displaced).unwrap();
    std::fs::rename(&attacker, &moved_target).unwrap();

    atomic_write(&moved_target, b"replacement").await.unwrap();
    assert_eq!(std::fs::read(&moved_target).unwrap(), b"replacement");
    assert_eq!(std::fs::read(&displaced).unwrap(), b"approved-original");
}

#[cfg(windows)]
#[tokio::test]
async fn windows_atomic_creation_ignores_swapped_parent_and_retained_attacker_handle() {
    use std::io::{Seek, Write};

    let container = TempDir::new("windows_temp_parent_swap");
    let approved_path = container.join("approved");
    let moved_approved_path = container.join("approved-held");
    std::fs::create_dir(&approved_path).unwrap();
    let target = approved_path.join("target.txt");
    let approved_parent = crate::fs::checked_path_metadata(&approved_path).unwrap();
    let (cancellation, probe) = AtomicWriteCancellation::with_temp_creation_probe_for_test();
    let writer_target = target.clone();
    let writer = tokio::spawn(async move {
        atomic_create_resolved_checked_cancellable(
            writer_target,
            b"approved-directory-only",
            approved_parent,
            cancellation,
        )
        .await
    });

    let reached = probe.clone();
    tokio::task::spawn_blocking(move || reached.wait_until_reached())
        .await
        .unwrap();
    std::fs::rename(&approved_path, &moved_approved_path).unwrap();
    std::fs::create_dir(&approved_path).unwrap();
    let attacker_target = approved_path.join("target.txt");
    std::fs::write(&attacker_target, b"attacker-owned").unwrap();
    let mut attacker_handle = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&attacker_target)
        .unwrap();
    probe.resume();

    writer.await.unwrap().unwrap();
    attacker_handle.rewind().unwrap();
    attacker_handle.write_all(b"corrupted-by-b").unwrap();
    attacker_handle.flush().unwrap();

    assert_eq!(std::fs::read(&target).unwrap(), b"corrupted-by-b");
    assert_eq!(
        std::fs::read(moved_approved_path.join("target.txt")).unwrap(),
        b"approved-directory-only"
    );
}

/// The core guarantee: while one writer repeatedly replaces a file, a separate
/// OS-thread reader must never observe a truncated or partially-written state —
/// only the complete old value or the complete new value. This is what `rename`
/// buys us and what the old truncate-then-stream write could not.
#[tokio::test]
async fn no_torn_reads_during_rewrites() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    let dir = TempDir::new("torn");
    let target = dir.join("hot.txt");
    let size = 64 * 1024;
    atomic_write(&target, vec![b'A'; size]).await.unwrap();

    let done = Arc::new(AtomicBool::new(false));
    let reader = {
        let path = target.clone();
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let mut torn = 0u64;
            while !done.load(Ordering::Relaxed) {
                if let Ok(bytes) = std::fs::read(&path) {
                    let homogeneous =
                        bytes.iter().all(|&c| c == b'A') || bytes.iter().all(|&c| c == b'B');
                    if !(bytes.len() == size && homogeneous) {
                        torn += 1;
                    }
                }
            }
            torn
        })
    };

    for r in 0..300 {
        let fill = if r % 2 == 0 { b'B' } else { b'A' };
        atomic_write(&target, vec![fill; size]).await.unwrap();
    }
    done.store(true, Ordering::Relaxed);

    let torn = reader.join().unwrap();
    assert_eq!(torn, 0, "reader observed {torn} torn/partial states");
    assert_eq!(temp_residue(dir.path()), 0);
}

// ── Content-snapshot guard on approved absolute paths ───────────────────

#[tokio::test]
async fn expecting_write_rejects_an_in_place_rewrite_of_the_same_inode() {
    let dir = TempDir::new("expecting_stale");
    let path = dir.path().join("file.txt");
    std::fs::write(&path, "old contents").unwrap();
    let base = crate::fs::ContentDigest::of(b"old contents");
    let approved_parent = crate::fs::stable_path_metadata(dir.path()).await.unwrap();

    // Same inode, new contents.
    std::fs::write(&path, "concurrent user change").unwrap();
    let result = crate::fs::atomic_write_resolved_expecting(
        &path,
        "agent output from stale contents",
        approved_parent,
        base,
    )
    .await;

    assert!(result.is_err(), "a stale replacement must not publish");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "concurrent user change"
    );
    assert_eq!(temp_residue(dir.path()), 0);
}

#[tokio::test]
async fn expecting_write_publishes_when_the_snapshot_still_matches() {
    let dir = TempDir::new("expecting_fresh");
    let path = dir.path().join("file.txt");
    std::fs::write(&path, "old contents").unwrap();
    let base = crate::fs::ContentDigest::of(b"old contents");
    let approved_parent = crate::fs::stable_path_metadata(dir.path()).await.unwrap();

    crate::fs::atomic_write_resolved_expecting(&path, "agent output", approved_parent, base)
        .await
        .unwrap();

    assert_eq!(std::fs::read_to_string(&path).unwrap(), "agent output");
    assert_eq!(temp_residue(dir.path()), 0);
}

#[tokio::test]
async fn expecting_write_rejects_a_same_length_rewrite() {
    let dir = TempDir::new("expecting_same_length");
    let path = dir.path().join("file.txt");
    std::fs::write(&path, "aaaa").unwrap();
    let base = crate::fs::ContentDigest::of(b"aaaa");
    let approved_parent = crate::fs::stable_path_metadata(dir.path()).await.unwrap();

    std::fs::write(&path, "bbbb").unwrap();
    let result =
        crate::fs::atomic_write_resolved_expecting(&path, "cccc", approved_parent, base).await;

    assert!(result.is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "bbbb");
    assert_eq!(temp_residue(dir.path()), 0);
}
