#![allow(unsafe_code)]

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard};

static PROCESS_ENV_LOCK: Mutex<()> = Mutex::new(());

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
mod fake_model;
#[cfg(test)]
mod feed_tests;
#[cfg(test)]
mod grep_tests;
#[cfg(test)]
mod headless_ask_tests;
#[cfg(all(test, feature = "hooks"))]
mod hooks;
#[cfg(test)]
mod input_tests;
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
mod mcp_oauth_tests;
#[cfg(all(test, feature = "mcp"))]
mod mcp_stdio_tests;
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
#[cfg(test)]
mod prompt_mode_tests;
#[cfg(test)]
mod provider_tests;
#[cfg(test)]
mod renderer_tests;
#[cfg(test)]
mod resumed_history_tests;
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
#[cfg(all(test, feature = "git-worktree"))]
mod worktree_tests;
