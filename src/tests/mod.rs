#![allow(unsafe_code)]

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard};

static PROCESS_ENV_LOCK: Mutex<()> = Mutex::new(());

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
    _lock: MutexGuard<'static, ()>,
}

impl ScopedProcessEnv {
    pub(crate) fn set(values: &[(&str, Option<OsString>)]) -> Self {
        let lock = PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
