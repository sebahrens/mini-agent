//! Process-spawning regression tests for the general sandbox backends.
//!
//! These live under `src/tests/` so the subprocess inventory treats their
//! helper launches as test-only rather than production sites.

use crate::sandbox::*;
#[cfg(target_os = "macos")]
use std::path::{Path, PathBuf};

/// Unique scratch directory removed on drop.
#[cfg(target_os = "macos")]
struct ScratchDir(PathBuf);

#[cfg(target_os = "macos")]
impl ScratchDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("mini-agent-sbx-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

#[cfg(target_os = "macos")]
impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Regression for mini-agent-v54o6: a credential directory reached through
/// a symlink (here `/tmp` -> `/private/tmp` style aliasing via an explicit
/// link) must be unreadable by a real Seatbelt child under both spellings.
#[cfg(target_os = "macos")]
#[test]
fn macos_seatbelt_denies_symlinked_credential_reads() {
    if !seatbelt_exists() {
        eprintln!("skipping real macOS sandbox probe because Seatbelt preflight is denied");
        return;
    }
    let root = ScratchDir::new();
    let real_home = root.path().join("real-home");
    let credentials = real_home.join("credentials");
    std::fs::create_dir_all(&credentials).unwrap();
    std::fs::write(credentials.join("key"), "secret").unwrap();
    let linked_home = root.path().join("linked-home");
    std::os::unix::fs::symlink(&real_home, &linked_home).unwrap();
    let canonical_secret = std::fs::canonicalize(credentials.join("key")).unwrap();

    let denies = seatbelt_private_read_denies(&[(
        linked_home.join("credentials").as_path(),
        "credential directory",
    )])
    .unwrap();
    let workspace = std::fs::canonicalize(root.path()).unwrap();
    let workspace_str = seatbelt_string_literal(&workspace, "working directory").unwrap();
    let profile = seatbelt_shell_profile(&workspace_str, &workspace_str, &denies, "", true);

    for target in [
        canonical_secret.clone(),
        linked_home.join("credentials").join("key"),
    ] {
        let output = std::process::Command::new("/usr/bin/sandbox-exec")
            .arg("-p")
            .arg(profile.as_str())
            .arg("/bin/cat")
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            !output.status.success() && output.stdout.is_empty(),
            "credential read through {} escaped Seatbelt: {:?}",
            target.display(),
            output
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn terminate_process_group_async_kills_the_whole_group() {
    use std::os::unix::process::CommandExt as _;

    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "trap '' TERM; /bin/sleep 60 & wait"])
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id();
    terminate_process_group(pid).await;
    for _ in 0..200 {
        if child.try_wait().unwrap().is_some() {
            await_drained_process_group(pid, PROCESS_GROUP_DRAIN_BUDGET).await;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("async process-group termination did not terminate group {pid}");
}

/// Regression for mini-agent-2tdv3: Windows termination waits up to ~5 s
/// for a cooperative helper exit and must not stall the executor meanwhile.
#[cfg(windows)]
#[test]
fn terminate_process_group_does_not_block_current_thread_executor() {
    let mut child = std::process::Command::new("ping")
        .args(["-n", "30", "127.0.0.1"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let ticks = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let observed = runtime.block_on({
        let ticks = ticks.clone();
        async move {
            let ticker_ticks = ticks.clone();
            let ticker = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    ticker_ticks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            });
            // A non-helper process never opens the cancellation event, so
            // termination spends its full cooperative window (~4 s) here.
            terminate_process_group(pid).await;
            let observed = ticks.load(std::sync::atomic::Ordering::SeqCst);
            ticker.abort();
            observed
        }
    });
    let status = child.wait().unwrap();
    assert!(
        observed > 0,
        "the executor made no progress while Windows termination was in flight"
    );
    assert!(
        !status.success(),
        "terminated process must not exit cleanly"
    );
}
