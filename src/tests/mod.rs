#![allow(unsafe_code)]

use std::ffi::OsString;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Writers (`ScopedProcessEnv`) hold this exclusively for their whole scope;
/// process-path readers hold it shared only while snapshotting the environment
/// (`with_shared_process_env`), so a reader never observes a mutator's
/// temporary values.
///
/// Readers have priority over *queued* writers: a writer also waits while any
/// reader is waiting, so back-to-back mutator tests cannot starve a reader
/// (a polling `try_read` on a std `RwLock` did, stalling it for the full wait).
struct EnvLockState {
    writer_active: bool,
    readers: usize,
    waiting_readers: usize,
}

static PROCESS_ENV_LOCK: Mutex<EnvLockState> = Mutex::new(EnvLockState {
    writer_active: false,
    readers: 0,
    waiting_readers: 0,
});
static PROCESS_ENV_CHANGED: Condvar = Condvar::new();

fn env_state() -> std::sync::MutexGuard<'static, EnvLockState> {
    PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct EnvWriteGuard;

impl EnvWriteGuard {
    fn acquire() -> Self {
        let mut state = env_state();
        while state.writer_active || state.readers > 0 || state.waiting_readers > 0 {
            state = PROCESS_ENV_CHANGED
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        state.writer_active = true;
        Self
    }
}

impl Drop for EnvWriteGuard {
    fn drop(&mut self) {
        env_state().writer_active = false;
        PROCESS_ENV_CHANGED.notify_all();
    }
}

/// Longest a reader waits for a mutator scope to end before reading anyway.
const SHARED_ENV_WAIT: Duration = Duration::from_secs(30);

/// Which side of the process-env lock a thread's reads belong to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EnvRole {
    /// Owns a `ScopedProcessEnv` (or a helper spawned for such an owner): reads
    /// its own scope without locking.
    Writer,
    /// A test with no scope: its reads wait out other tests' scopes.
    Reader,
    /// A helper thread nobody attributed to a test (a runtime worker, a plain
    /// spawned thread): reads directly, because waiting could stall the scope
    /// of the test that spawned it.
    Unattributed,
}

thread_local! {
    static ENV_ROLE: std::cell::Cell<Option<EnvRole>> = const { std::cell::Cell::new(None) };
}

pub(crate) fn is_test_thread() -> bool {
    let current = std::thread::current();
    matches!(current.name(), Some(name) if name == "main" || name.contains("::"))
}

/// The role of the calling thread: an explicit assignment (writer scope or an
/// inherited role), else `Reader` on a libtest test thread, else `Unattributed`.
pub(crate) fn current_env_role() -> EnvRole {
    ENV_ROLE.with(std::cell::Cell::get).unwrap_or_else(|| {
        if is_test_thread() {
            EnvRole::Reader
        } else {
            EnvRole::Unattributed
        }
    })
}

/// Restores the previous role on drop.
pub(crate) struct EnvRoleGuard(Option<EnvRole>);

/// Gives the calling helper thread the role of the test that spawned it
/// (captured with [`current_env_role`] on the spawning thread).
pub(crate) fn inherit_env_role(role: EnvRole) -> EnvRoleGuard {
    EnvRoleGuard(ENV_ROLE.with(|cell| cell.replace(Some(role))))
}

impl Drop for EnvRoleGuard {
    fn drop(&mut self) {
        ENV_ROLE.with(|cell| cell.set(self.0));
    }
}

/// Runs `read` (an environment snapshot) so it cannot overlap any
/// `ScopedProcessEnv` scope owned by another test. Writers and unattributed
/// helpers read without locking; only a scope-less test waits.
pub(crate) fn with_shared_process_env<T>(read: impl FnOnce() -> T) -> T {
    if current_env_role() != EnvRole::Reader {
        return read();
    }
    let deadline = Instant::now() + SHARED_ENV_WAIT;
    let mut state = env_state();
    state.waiting_readers += 1;
    while state.writer_active {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            // A mutator scope outlived the bound: read anyway.
            state.waiting_readers -= 1;
            drop(state);
            PROCESS_ENV_CHANGED.notify_all();
            return read();
        }
        state = PROCESS_ENV_CHANGED
            .wait_timeout(state, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0;
    }
    state.waiting_readers -= 1;
    state.readers += 1;
    drop(state);
    struct Release;
    impl Drop for Release {
        fn drop(&mut self) {
            env_state().readers -= 1;
            PROCESS_ENV_CHANGED.notify_all();
        }
    }
    let _release = Release;
    read()
}

/// An exclusively owned scratch directory, removed even when a test panics.
/// Declare it before environment guards and retained filesystem capabilities
/// so those are released before directory cleanup during unwinding.
pub(crate) struct OwnedTestDirectory(std::path::PathBuf);

impl OwnedTestDirectory {
    pub(crate) fn new(label: &str) -> Self {
        let path = std::env::temp_dir()
            .canonicalize()
            .expect("resolve test scratch directory")
            .join(format!("mini-agent-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).expect("create exclusive test scratch directory");
        Self(path)
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for OwnedTestDirectory {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "failed to clean test directory {}: {error}",
                self.0.display()
            );
        }
    }
}

#[test]
fn owned_test_directory_cleans_up_after_panic() {
    let mut path = None;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let directory = OwnedTestDirectory::new("panic-cleanup");
        path = Some(directory.path().to_path_buf());
        std::fs::create_dir(directory.path().join("nested")).unwrap();
        std::fs::write(directory.path().join("nested/file"), "owned").unwrap();
        panic!("fixture failure");
    }));
    assert!(result.is_err());
    assert!(!path.unwrap().exists());
}

pub(crate) struct ScopedProcessEnv {
    previous: Vec<(OsString, Option<OsString>)>,
    _lock: EnvWriteGuard,
}

impl ScopedProcessEnv {
    pub(crate) fn set(values: &[(&str, Option<OsString>)]) -> Self {
        let lock = EnvWriteGuard::acquire();
        ENV_ROLE.with(|cell| cell.set(Some(EnvRole::Writer)));
        let previous = values
            .iter()
            .map(|(name, _)| (OsString::from(name), std::env::var_os(name)))
            .collect();
        for (name, value) in values {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
        Self {
            previous,
            _lock: lock,
        }
    }
}

impl Drop for ScopedProcessEnv {
    fn drop(&mut self) {
        ENV_ROLE.with(|cell| cell.set(None));
        for (name, value) in &self.previous {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

#[test]
fn scoped_process_environment_restores_after_panic() {
    const NAME: &str = "ZS_SCOPED_ENV_UNWIND_TEST";
    let before = std::env::var_os(NAME);
    let result = std::panic::catch_unwind(|| {
        let _environment = ScopedProcessEnv::set(&[(NAME, Some(OsString::from("temporary")))]);
        assert_eq!(std::env::var_os(NAME), Some(OsString::from("temporary")));
        panic!("exercise unwind restoration");
    });
    assert!(result.is_err());
    assert_eq!(std::env::var_os(NAME), before);
}

#[test]
fn shared_process_env_reader_never_observes_a_mutator_scope() {
    const NAME: &str = "ZS_SHARED_READER_TEST";
    let environment = ScopedProcessEnv::set(&[(NAME, Some(OsString::from("temporary")))]);
    // The owning thread reads without locking.
    assert_eq!(
        with_shared_process_env(|| std::env::var_os(NAME)),
        Some(OsString::from("temporary"))
    );
    // A libtest test thread with no scope is a `Reader`; a raw spawned thread
    // is `Unattributed` and reads directly, so give it the test-reader role.
    let reader = std::thread::spawn(|| {
        let _role = inherit_env_role(EnvRole::Reader);
        with_shared_process_env(|| std::env::var_os(NAME))
    });
    std::thread::sleep(Duration::from_millis(50));
    assert!(!reader.is_finished(), "reader must wait for the mutator");
    drop(environment);
    assert_eq!(reader.join().unwrap(), None);
}

#[cfg(all(test, feature = "acp"))]
mod acp_tests;
#[cfg(all(test, feature = "advisor"))]
mod advisor_tests;
#[cfg(all(test, feature = "archmd"))]
mod archmd_tests;
#[cfg(test)]
mod atomic_write_tests;
#[cfg(test)]
mod auth_tests;
#[cfg(test)]
mod bash_tests;
#[cfg(test)]
mod btw_tests;
mod build_inputs_tests;
#[cfg(test)]
mod chain_tests;
#[cfg(test)]
mod checker_tests;
#[cfg(test)]
mod config_persistence_permissions_tests;
#[cfg(test)]
mod config_tests;
#[cfg(test)]
mod convert_history_tests;
#[cfg(test)]
mod crc_tests;
#[cfg(test)]
mod edit_tests;
#[cfg(test)]
pub(crate) mod fake_model;
#[cfg(test)]
mod feed_tests;
#[cfg(test)]
mod grep_tests;
#[cfg(all(test, feature = "js", feature = "subagents"))]
mod harness_eval_tests;
#[cfg(test)]
mod headless_ask_tests;
#[cfg(all(test, unix))]
mod headless_forced_stop_tests;
#[cfg(all(test, feature = "hooks"))]
mod hooks;
#[cfg(test)]
mod image_read_tests;
#[cfg(test)]
mod input_tests;
#[cfg(all(test, unix))]
mod internal_git_hardening_tests;
#[cfg(test)]
mod list_dir_tests;
#[cfg(test)]
mod logging_tests;
#[cfg(all(test, feature = "loop"))]
mod loop_tests;
#[cfg(all(test, feature = "lsp"))]
mod lsp_process_tests;
#[cfg(all(test, feature = "lsp"))]
mod lsp_tests;
#[cfg(test)]
mod markdown_tests;
#[cfg(all(test, feature = "mcp"))]
mod mcp_http_tests;
#[cfg(all(test, feature = "mcp"))]
mod mcp_oauth_tests;
#[cfg(all(test, feature = "mcp"))]
pub(crate) mod mcp_stdio_tests;
#[cfg(all(test, feature = "memory"))]
mod memory_tests;
#[cfg(test)]
mod models_catalog_tests;
#[cfg(all(test, feature = "multimodal"))]
mod multimodal_tests;
#[cfg(test)]
mod normalize_tests;
#[cfg(test)]
mod paste_burst_tests;
#[cfg(test)]
mod picker_tests;
#[cfg(test)]
mod platform_paths_tests;
#[cfg(test)]
mod portable_filename_tests;
#[cfg(unix)]
pub(crate) mod process_gate;
#[cfg(all(test, unix))]
mod process_group_guard_tests;
#[cfg(all(
    any(feature = "acp", feature = "git-worktree", feature = "hooks"),
    any(target_os = "linux", target_os = "macos")
))]
pub(crate) mod process_state;
#[cfg(test)]
mod prompt_mode_tests;
#[cfg(test)]
mod provider_tests;
#[cfg(test)]
mod renderer_tests;
#[cfg(test)]
mod resumed_history_tests;
#[cfg(test)]
mod sandbox_platform_tests;
#[cfg(all(test, feature = "export"))]
mod session_export_tests;
#[cfg(test)]
mod session_storage_tests;
#[cfg(test)]
mod session_tests;
#[cfg(test)]
mod shell_mode_tests;
#[cfg(test)]
mod singleflight_tests;
#[cfg(test)]
mod slash_add_tests;
#[cfg(test)]
mod slash_init_tests;
#[cfg(test)]
mod startup_prompt_mode_tests;
#[cfg(all(test, unix))]
mod status_signals_tests;
#[cfg(test)]
mod statusline_tests;
#[cfg(all(test, feature = "subagents"))]
mod subagents_tests;
#[cfg(test)]
mod subprocess_inventory_tests;
#[cfg(test)]
mod todo_tests;
#[cfg(test)]
mod tools_filter_tests;
#[cfg(test)]
mod tools_mod_tests;
#[cfg(test)]
mod turn_persistence_tests;
pub(crate) mod update_tests;
#[cfg(all(test, feature = "git-worktree"))]
mod worktree_tests;

#[cfg(unix)]
mod ledger_lock_tests;
