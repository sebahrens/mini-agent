use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use super::dispatcher::HookDispatcher;
use super::settings::{HookGroup, HookHandler, HooksConfig, parse_hooks_config};

/// Deterministic hash of a project hook binding (project root + event +
/// matcher + handler definition). Any change to the binding, or trusting the
/// same binding from a different project root, changes the hash, so trust
/// never crosses project boundaries. Hook subprocesses inherit zerostack's
/// CWD, so without the root a binding trusted in one project would silently
/// execute a same-named script in any other project.
pub(crate) fn hash_hook_binding(
    project_root: &Path,
    event: &str,
    matcher: Option<&str>,
    handler: &HookHandler,
) -> String {
    let mut hasher = DefaultHasher::new();
    project_root.to_string_lossy().hash(&mut hasher);
    event.hash(&mut hasher);
    matcher.unwrap_or("").hash(&mut hasher);
    handler.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn default_trust_store_path(paths: &crate::paths::AppPaths) -> PathBuf {
    paths.hook_trust_file()
}

pub(crate) fn load_trust_store(path: &Path) -> HashSet<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return HashSet::new();
    };
    serde_json::from_str::<HashSet<String>>(&content).unwrap_or_default()
}

pub(crate) fn save_trust_store(path: &Path, trusted: &HashSet<String>) {
    if crate::paths::artifact_disabled("hook trust") {
        return;
    }
    let Ok(json) = serde_json::to_string_pretty(trusted) else {
        return;
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!("hooks: failed to create trust store directory: {e}");
        return;
    }
    if let Err(e) = crate::session::storage::atomic_write(path, &json) {
        tracing::warn!("hooks: failed to save trust store (trust decisions won't persist): {e}");
    }
}

fn global_settings_path(paths: &crate::paths::AppPaths) -> PathBuf {
    paths.global_hook_settings_file()
}

fn project_settings_path(paths: &crate::paths::AppPaths) -> PathBuf {
    paths
        .project_hook_settings_file()
        .expect("startup workspace must have a project path")
}

#[cfg(target_os = "linux")]
fn managed_settings_path() -> PathBuf {
    PathBuf::from("/etc/zerostack/managed-settings.json")
}

#[cfg(target_os = "macos")]
fn managed_settings_path() -> PathBuf {
    PathBuf::from("/Library/Application Support/zerostack/managed-settings.json")
}

#[cfg(target_os = "windows")]
fn managed_settings_path() -> PathBuf {
    PathBuf::from(r"C:\ProgramData\zerostack\managed-settings.json")
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn managed_settings_path() -> PathBuf {
    PathBuf::from("/etc/zerostack/managed-settings.json")
}

/// Prompts the user to confirm an untrusted project hook via stdin, matching
/// the plain y/N startup-prompt style used elsewhere (see `main.rs`).
pub(crate) fn confirm_untrusted_hook(description: &str) -> bool {
    let mut input = String::new();
    eprint!("Trust project hook: {description}? [y/N] ");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

fn hook_confirmation_description(handler: &HookHandler) -> String {
    let argv = std::iter::once(handler.command.as_deref())
        .chain(handler.args.iter().flatten().map(|arg| Some(arg.as_str())))
        .collect::<Vec<_>>();
    let argv = serde_json::to_string(&argv).expect("serializing hook argv strings cannot fail");

    match handler.condition.as_deref() {
        Some(condition) => {
            let condition = serde_json::to_string(condition)
                .expect("serializing a hook condition string cannot fail");
            format!("executable argv={argv}; shell condition={condition}")
        }
        None => format!("executable argv={argv}"),
    }
}

struct SourceConfig {
    hooks: HooksConfig,
    disable_all_hooks: bool,
}

fn load_settings_file(path: &Path) -> SourceConfig {
    let empty = SourceConfig {
        hooks: HashMap::new(),
        disable_all_hooks: false,
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return empty;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        tracing::warn!("hooks: {}: invalid JSON, ignoring", path.display());
        return empty;
    };
    let hooks = value
        .get("hooks")
        .map(|h| {
            parse_hooks_config(h).unwrap_or_else(|e| {
                tracing::warn!("hooks: {}: {e}", path.display());
                HashMap::new()
            })
        })
        .unwrap_or_default();
    let disable_all_hooks = value
        .get("disableAllHooks")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    SourceConfig {
        hooks,
        disable_all_hooks,
    }
}

fn merge_into(target: &mut HooksConfig, source: HooksConfig) {
    for (event, groups) in source {
        target.entry(event).or_default().extend(groups);
    }
}

/// Filters project-sourced hooks by trust: already-trusted bindings pass
/// through, headless contexts skip unconfirmed bindings with a warning, and
/// interactive contexts consult `confirm`, persisting an acceptance.
fn filter_trusted_project_hooks(
    hooks: HooksConfig,
    project_root: &Path,
    trust_store_path: &Path,
    headless: bool,
    confirm: &dyn Fn(&str) -> bool,
) -> HooksConfig {
    let mut trusted_hashes = load_trust_store(trust_store_path);
    let mut result: HooksConfig = HashMap::new();
    for (event, groups) in hooks {
        let mut kept_groups = Vec::with_capacity(groups.len());
        for group in groups {
            let mut kept_handlers = Vec::with_capacity(group.hooks.len());
            for handler in group.hooks {
                let hash =
                    hash_hook_binding(project_root, &event, group.matcher.as_deref(), &handler);
                if trusted_hashes.contains(&hash) {
                    kept_handlers.push(handler);
                } else if headless {
                    tracing::warn!(
                        "hooks: skipping unconfirmed project hook for event {event:?} \
                         (headless; run interactively once to confirm)"
                    );
                } else if confirm(&hook_confirmation_description(&handler)) {
                    trusted_hashes.insert(hash);
                    kept_handlers.push(handler);
                } else {
                    tracing::warn!("hooks: user declined project hook for event {event:?}");
                }
            }
            if !kept_handlers.is_empty() {
                kept_groups.push(HookGroup {
                    matcher: group.matcher,
                    hooks: kept_handlers,
                });
            }
        }
        if !kept_groups.is_empty() {
            result.insert(event, kept_groups);
        }
    }
    save_trust_store(trust_store_path, &trusted_hashes);
    result
}

/// Loads global/project/managed settings, applies `disableAllHooks`/
/// `--no-hooks` (never affecting managed hooks) and project-hook trust
/// filtering, and builds the resulting dispatcher. Explicit paths and a
/// confirmation callback make this fully unit-testable without a TUI.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_dispatcher_from_paths(
    global_path: &Path,
    project_path: &Path,
    managed_path: &Path,
    project_root: &Path,
    no_hooks_flag: bool,
    headless: bool,
    trust_store_path: &Path,
    confirm: &dyn Fn(&str) -> bool,
) -> HookDispatcher {
    let global = load_settings_file(global_path);
    let project = load_settings_file(project_path);
    let managed = load_settings_file(managed_path);

    let disable_non_managed =
        no_hooks_flag || global.disable_all_hooks || project.disable_all_hooks;

    let mut merged: HooksConfig = HashMap::new();

    if !disable_non_managed {
        merge_into(&mut merged, global.hooks);
        let filtered_project = filter_trusted_project_hooks(
            project.hooks,
            project_root,
            trust_store_path,
            headless,
            confirm,
        );
        merge_into(&mut merged, filtered_project);
    }

    merge_into(&mut merged, managed.hooks);

    HookDispatcher::from_config(&merged).unwrap_or_else(|e| {
        tracing::warn!("hooks: invalid merged config, disabling hooks: {e}");
        HookDispatcher::from_config(&HashMap::new()).expect("empty config is always valid")
    })
}

fn current_project_root(paths: &crate::paths::AppPaths) -> PathBuf {
    let project_dir = paths
        .project_dir
        .as_ref()
        .expect("startup workspace must have a project path");
    let root = project_dir
        .parent()
        .expect("project application directory must have a parent");
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

/// Top-level entry point: builds the process dispatcher from the real
/// global/project/managed settings locations, the real trust store, and the
/// current directory as the project root for trust hashing.
pub(crate) fn load_dispatcher(
    paths: &crate::paths::AppPaths,
    no_hooks_flag: bool,
    headless: bool,
) -> HookDispatcher {
    build_dispatcher_from_paths(
        &global_settings_path(paths),
        &project_settings_path(paths),
        &managed_settings_path(),
        &current_project_root(paths),
        no_hooks_flag || crate::paths::artifact_disabled("hook trust"),
        headless,
        &default_trust_store_path(paths),
        &confirm_untrusted_hook,
    )
}
