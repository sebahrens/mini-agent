use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::dispatcher::HookDispatcher;
use super::pins::HookContentPins;
use super::settings::{HookGroup, HookHandler, HooksConfig, parse_hooks_config_with_skips};

/// Deterministic hash of a project hook binding (project root + event +
/// matcher + handler definition). Any change to the binding, or trusting the
/// same binding from a different project root, changes the hash, so trust
/// never crosses project boundaries. Hook subprocesses inherit mini-agent's
/// CWD, so without the root a binding trusted in one project would silently
/// execute a same-named script in any other project.
pub(crate) fn hash_hook_binding(
    project_root: &Path,
    event: &str,
    matcher: Option<&str>,
    handler: &HookHandler,
) -> String {
    let file_bindings = project_file_bindings(project_root, handler);
    let canonical = serde_json::to_vec(&(
        project_root.to_string_lossy(),
        event,
        matcher,
        handler,
        file_bindings,
    ))
    .expect("serializing hook trust bindings cannot fail");
    let mut hasher = Sha256::new();
    hasher.update(b"mini-agent-hook-binding-v3\0");
    hasher.update(canonical);
    crate::hex::encode_lower(hasher.finalize())
}

fn project_file_bindings(
    project_root: &Path,
    handler: &HookHandler,
) -> Vec<(usize, String, String)> {
    let canonical_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let candidates = handler
        .command
        .iter()
        .chain(handler.args.iter().flatten())
        .enumerate();
    let mut bindings = Vec::new();
    for (index, candidate) in candidates {
        let path = Path::new(candidate);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            canonical_root.join(path)
        };
        let Ok(canonical) = path.canonicalize() else {
            continue;
        };
        if !canonical.starts_with(&canonical_root) || !canonical.is_file() {
            continue;
        }
        let digest =
            hash_project_hook_file(&canonical).unwrap_or_else(|_| "unreadable".to_string());
        let relative = canonical
            .strip_prefix(&canonical_root)
            .unwrap_or(&canonical)
            .to_string_lossy()
            .into_owned();
        bindings.push((index, relative, digest));
    }
    bindings
}

fn hash_project_hook_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(crate::hex::encode_lower(hasher.finalize()))
}

/// Trust hash that also binds the content of every workspace-resident file
/// the handler executes (see [`super::pins`]). A binding without such files
/// keeps its [`hash_hook_binding`] identity, so existing approvals of
/// system executables stay valid; rewriting a bound script changes the hash
/// and requires a fresh confirmation.
pub(crate) fn hash_hook_binding_with_content(
    project_root: &Path,
    event: &str,
    matcher: Option<&str>,
    handler: &HookHandler,
    pins: &HookContentPins,
) -> String {
    if pins.is_empty() {
        return hash_hook_binding(project_root, event, matcher, handler);
    }
    let canonical = serde_json::to_vec(&(
        project_root.to_string_lossy(),
        event,
        matcher,
        handler,
        pins.entries(),
    ))
    .expect("serializing hook trust bindings cannot fail");
    let mut hasher = Sha256::new();
    hasher.update(b"mini-agent-hook-binding-v3\0");
    hasher.update(canonical);
    crate::hex::encode_lower(hasher.finalize())
}

fn default_trust_store_path(paths: &crate::paths::AppPaths) -> PathBuf {
    paths.hook_trust_file()
}

#[cfg(test)]
pub(crate) fn load_trust_store(path: &Path) -> HashSet<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return HashSet::new();
    };
    serde_json::from_str::<HashSet<String>>(&content).unwrap_or_default()
}

/// Like [`load_trust_store`], but an existing store that cannot be read or
/// parsed is an error instead of an empty set, so its content records are
/// never silently forgotten (and never overwritten).
fn load_trust_store_checked(path: &Path) -> Result<HashSet<String>, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(error) => {
            return Err(format!(
                "hook trust store {} is unreadable: {error}",
                path.display()
            ));
        }
    };
    serde_json::from_str::<HashSet<String>>(&content)
        .map_err(|error| format!("hook trust store {} is not valid: {error}", path.display()))
}

/// Exclusive advisory lock on `<store>.lock`, held until dropped. The lock
/// file is never deleted (unlinking one another process has open would let
/// two owners lock different inodes).
fn lock_trust_store(path: &Path) -> Result<std::fs::File, String> {
    let mut name = path
        .file_name()
        .ok_or_else(|| "hook trust store path has no file name".to_string())?
        .to_os_string();
    name.push(".lock");
    let lock_path = path.with_file_name(name);
    match crate::fs::private_atomic_create_sync(&lock_path, b"") {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("failed to create hook trust store lock: {error}")),
    }
    let file = crate::fs::open_private_file(&lock_path)
        .map_err(|error| format!("failed to open hook trust store lock: {error}"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => {
                if std::time::Instant::now() >= deadline {
                    return Err(
                        "timed out waiting for another session's hook trust store lock".to_string(),
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(format!("failed to lock hook trust store: {error}"));
            }
        }
    }
}

/// Persist this session's trust changes without discarding another session's:
/// under the store lock, re-read the file and apply only the delta from
/// `loaded` to `current` (entries added, entries removed). An unreadable
/// existing store is left untouched and reported.
pub(crate) fn save_trust_store_merged(
    path: &Path,
    loaded: &HashSet<String>,
    current: &HashSet<String>,
) -> Result<(), String> {
    if crate::paths::artifact_disabled("hook trust") {
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        return Err(format!(
            "failed to create hook trust store directory: {error}"
        ));
    }
    let _lock = lock_trust_store(path)?;
    let on_disk =
        load_trust_store_checked(path).map_err(|error| format!("{error}; not overwriting it"))?;
    let mut merged = on_disk.clone();
    for removed in loaded.difference(current) {
        merged.remove(removed);
    }
    merged.extend(current.difference(loaded).cloned());
    if merged == on_disk {
        return Ok(());
    }
    save_trust_store(path, &merged)
}

pub(crate) fn save_trust_store(path: &Path, trusted: &HashSet<String>) -> Result<(), String> {
    if crate::paths::artifact_disabled("hook trust") {
        return Ok(());
    }
    let json = serde_json::to_string_pretty(trusted)
        .map_err(|error| format!("failed to serialize hook trust store: {error}"))?;
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        return Err(format!(
            "failed to create hook trust store directory: {error}"
        ));
    }
    crate::session::storage::atomic_write(path, &json)
        .map_err(|error| format!("failed to save hook trust store: {error}"))?;
    Ok(())
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
    PathBuf::from("/etc/mini-agent/managed-settings.json")
}

#[cfg(target_os = "macos")]
fn managed_settings_path() -> PathBuf {
    PathBuf::from("/Library/Application Support/mini-agent/managed-settings.json")
}

#[cfg(target_os = "windows")]
fn managed_settings_path() -> PathBuf {
    PathBuf::from(r"C:\ProgramData\mini-agent\managed-settings.json")
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn managed_settings_path() -> PathBuf {
    PathBuf::from("/etc/mini-agent/managed-settings.json")
}

/// Prompts the user to confirm an untrusted project hook via stdin, matching
/// the plain y/N startup-prompt style used elsewhere (see `main.rs`).
pub(crate) fn confirm_untrusted_hook(description: &str) -> bool {
    let mut input = String::new();
    eprint!("Trust {description}? [y/N] ");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

fn hook_confirmation_description(
    event: &str,
    matcher: Option<&str>,
    handler: &HookHandler,
    pins: &HookContentPins,
) -> String {
    let argv = std::iter::once(handler.command.as_deref())
        .chain(handler.args.iter().flatten().map(|arg| Some(arg.as_str())))
        .collect::<Vec<_>>();
    let argv = serde_json::to_string(&argv).expect("serializing hook argv strings cannot fail");

    let trust = match handler.trust {
        super::settings::HookTrust::Sandboxed => "sandboxed",
        super::settings::HookTrust::Trusted => "trusted",
    };
    let env_keys = serde_json::to_string(&handler.env.keys().collect::<Vec<_>>())
        .expect("serializing hook environment key strings cannot fail");
    let env_binding = serde_json::to_vec(&handler.env)
        .expect("serializing hook environment bindings cannot fail");
    let env_binding_sha256 = crate::hex::encode_lower(Sha256::digest(env_binding));
    let event = serde_json::to_string(event).expect("serializing hook event cannot fail");
    let matcher = serde_json::to_string(&matcher).expect("serializing hook matcher cannot fail");
    let content = if pins.is_empty() {
        String::new()
    } else {
        let files = serde_json::to_string(&pins.entries())
            .expect("serializing hook content bindings cannot fail");
        format!("; workspace files sha256={files}")
    };
    let policy = format!(
        "event={event}; matcher={matcher}; subprocess trust={trust:?}; explicit env keys={env_keys}; env binding sha256={env_binding_sha256:?}{content}"
    );

    match handler.kind.as_str() {
        "mcp_tool" => {
            return format!(
                "mcp_tool call server+tool+input={argv} (runs without a per-call prompt when hooks_mcp_tool_enabled is set); {policy}"
            );
        }
        "prompt" | "agent" => {
            return format!(
                "{} hook prompt+model={argv} (sends the hook event to the model when hooks_model_enabled is set); {policy}",
                handler.kind
            );
        }
        _ => {}
    }
    if handler.kind == "http" {
        // The URL lives in `command`, literal request headers in `env`.
        return format!("http POST url={argv} (headers are the env binding); {policy}");
    }
    match handler.condition.as_deref() {
        Some(condition) => {
            let condition = serde_json::to_string(condition)
                .expect("serializing a hook condition string cannot fail");
            format!("executable argv={argv}; shell condition={condition}; {policy}")
        }
        None => format!("executable argv={argv}; {policy}"),
    }
}

struct SourceConfig {
    hooks: HooksConfig,
    disable_all_hooks: bool,
    /// Why this file's hooks could not be loaded (the whole file is dropped),
    /// or which hooks inside it were skipped. Never contains file content
    /// beyond parser messages, with control characters removed.
    problems: Vec<String>,
    /// The file exists but none of its hooks could be loaded.
    unloadable: bool,
    /// A `PreToolUse` hook was dropped from an otherwise loaded file.
    dropped_pre_tool_use: bool,
}

/// One printable line of a parser message: no control characters, bounded.
fn printable(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .take(300)
        .collect()
}

fn load_settings_file(path: &Path) -> SourceConfig {
    let mut config = SourceConfig {
        hooks: HashMap::new(),
        disable_all_hooks: false,
        problems: Vec::new(),
        unloadable: false,
        dropped_pre_tool_use: false,
    };
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return config,
        Err(error) => {
            return unloadable(config, path, format!("cannot be opened: {error}"));
        }
    };
    let mut content = String::new();
    if file
        .take(1024 * 1024 + 1)
        .read_to_string(&mut content)
        .is_err()
        || content.len() > 1024 * 1024
    {
        return unloadable(config, path, "exceeds bounds or is not UTF-8".to_string());
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return unloadable(config, path, "is not valid JSON".to_string());
    };
    config.disable_all_hooks = value
        .get("disableAllHooks")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if let Some(h) = value.get("hooks") {
        match parse_hooks_config_with_skips(h) {
            Ok((hooks, skipped)) => {
                config.hooks = hooks;
                for skipped in skipped {
                    config.dropped_pre_tool_use |= skipped.event == "PreToolUse";
                    config.problems.push(format!(
                        "hooks: {}: {}",
                        path.display(),
                        printable(&skipped.reason)
                    ));
                }
            }
            Err(error) => return unloadable(config, path, printable(&error)),
        }
    }
    config
}

fn unloadable(mut config: SourceConfig, path: &Path, why: String) -> SourceConfig {
    tracing::warn!("hooks: {}: {why}, ignoring its hooks", path.display());
    config.problems.push(format!(
        "hooks: {} {why}; none of its hooks were loaded",
        path.display()
    ));
    config.unloadable = true;
    config
}

fn merge_into(target: &mut HooksConfig, source: HooksConfig) {
    for (event, groups) in source {
        target.entry(event).or_default().extend(groups);
    }
}

/// Trust-store entry prefix for the persisted workspace-file digests of
/// global and managed hooks. Entries are `hook-content-v1:<binding>:<digest>`
/// strings inside the same JSON string set as project trust hashes, so older
/// builds read them as unknown hashes and ignore them.
const CONTENT_RECORD_PREFIX: &str = "hook-content-v1:";

/// Identity of one global/managed binding in one project: source, project
/// root, event, matcher, and handler definition.
fn content_record_binding(
    source: &str,
    project_root: &Path,
    event: &str,
    matcher: Option<&str>,
    handler: &HookHandler,
) -> String {
    let canonical = serde_json::to_vec(&(
        source,
        project_root.to_string_lossy(),
        event,
        matcher,
        handler,
    ))
    .expect("serializing hook content records cannot fail");
    let mut hasher = Sha256::new();
    hasher.update(b"mini-agent-hook-content-record-v1\0");
    hasher.update(canonical);
    crate::hex::encode_lower(hasher.finalize())
}

/// Records the content binding approved for `handler`. A rejection always
/// wins, so a handler shared by several bindings stays fail-closed when any
/// of them was rejected.
fn record_approved_pins(
    approved: &mut HashMap<HookHandler, HookContentPins>,
    handler: &HookHandler,
    pins: HookContentPins,
    rejected: bool,
) {
    if rejected {
        approved.insert(handler.clone(), pins);
    } else {
        approved.entry(handler.clone()).or_insert(pins);
    }
}

/// Persists the workspace-file digests of global and managed hooks across
/// sessions. Global and managed hooks are trusted by configuration, but a
/// workspace file they execute (`./guard.sh`) is content the model can
/// rewrite during one session to take effect in the next. The first time a
/// binding executes workspace files in a project its digests are recorded
/// (trust on first use, matching the configured intent). A later session whose
/// files differ asks interactively; a declined or headless change keeps the
/// hook installed but denies every launch (fail-closed for `PreToolUse`), so
/// the guard is never silently replaced nor silently dropped.
#[allow(clippy::too_many_arguments)]
fn pin_configured_hook_content(
    hooks: &HooksConfig,
    source: &str,
    project_root: &Path,
    store: &mut HashSet<String>,
    store_readable: bool,
    headless: bool,
    confirm: &dyn Fn(&str) -> bool,
    approved: &mut HashMap<HookHandler, HookContentPins>,
) {
    for (event, groups) in hooks {
        for group in groups {
            let matcher = group.matcher.as_deref();
            for handler in &group.hooks {
                let pins = HookContentPins::capture(project_root, handler);
                let binding = content_record_binding(source, project_root, event, matcher, handler);
                let prefix = format!("{CONTENT_RECORD_PREFIX}{binding}:");
                let current = format!("{prefix}{}", pins.content_digest());
                let recorded: Vec<String> = store
                    .iter()
                    .filter(|entry| entry.starts_with(&prefix))
                    .cloned()
                    .collect();
                // An unreadable store holds records we cannot see, so "no
                // record" is not first use: review it like a changed file.
                if recorded.is_empty() && (store_readable || pins.is_empty()) {
                    if !pins.is_empty() {
                        store.insert(current);
                    }
                    record_approved_pins(approved, handler, pins, false);
                    continue;
                }
                if recorded.len() == 1 && recorded[0] == current {
                    record_approved_pins(approved, handler, pins, false);
                    continue;
                }
                let accepted = !headless
                    && confirm(&format!(
                        "{source} hook whose workspace files changed since they were last approved: {}",
                        hook_confirmation_description(event, matcher, handler, &pins)
                    ));
                if accepted {
                    for entry in recorded {
                        store.remove(&entry);
                    }
                    if !pins.is_empty() {
                        store.insert(current);
                    }
                    record_approved_pins(approved, handler, pins, false);
                } else {
                    tracing::warn!(
                        "hooks: {source} hook for event {event:?} executes workspace files that \
                         changed since they were last approved; denying its launches \
                         (run interactively to review the change)"
                    );
                    record_approved_pins(
                        approved,
                        handler,
                        HookContentPins::rejected(format!(
                            "{source} hook workspace files changed since they were last approved; \
                             restart interactively to review them"
                        )),
                        true,
                    );
                }
            }
        }
    }
}

/// Filters project-sourced hooks by trust: already-trusted bindings pass
/// through, headless contexts skip unconfirmed bindings with a warning, and
/// interactive contexts consult `confirm`, recording an acceptance in
/// `trusted_hashes`. The content binding each kept handler was approved
/// with is recorded in `approved`.
fn filter_trusted_project_hooks(
    hooks: HooksConfig,
    project_root: &Path,
    trusted_hashes: &mut HashSet<String>,
    headless: bool,
    confirm: &dyn Fn(&str) -> bool,
    approved: &mut HashMap<HookHandler, HookContentPins>,
) -> HooksConfig {
    let mut result: HooksConfig = HashMap::new();
    for (event, groups) in hooks {
        let mut kept_groups = Vec::with_capacity(groups.len());
        for group in groups {
            let mut kept_handlers = Vec::with_capacity(group.hooks.len());
            for handler in group.hooks {
                let pins = HookContentPins::capture(project_root, &handler);
                let hash = hash_hook_binding_with_content(
                    project_root,
                    &event,
                    group.matcher.as_deref(),
                    &handler,
                    &pins,
                );
                if trusted_hashes.contains(&hash) {
                    record_approved_pins(approved, &handler, pins, false);
                    kept_handlers.push(handler);
                } else if headless {
                    tracing::warn!(
                        "hooks: skipping unconfirmed project hook for event {event:?} \
                         (headless; run interactively once to confirm)"
                    );
                } else if confirm(&format!(
                    "project hook: {}",
                    hook_confirmation_description(
                        &event,
                        group.matcher.as_deref(),
                        &handler,
                        &pins,
                    )
                )) {
                    trusted_hashes.insert(hash);
                    record_approved_pins(approved, &handler, pins, false);
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
    result
}

/// Loads global/project/managed settings, applies `disableAllHooks`/
/// `--no-hooks` (never affecting managed hooks) and project-hook trust
/// filtering, and builds the resulting dispatcher. Explicit paths and a
/// confirmation callback make this fully unit-testable without a TUI.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
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
    let backend = if cfg!(target_os = "macos") {
        "seatbelt"
    } else {
        "bwrap"
    };
    build_dispatcher_from_paths_with_backend(
        global_path,
        project_path,
        managed_path,
        project_root,
        no_hooks_flag,
        headless,
        trust_store_path,
        confirm,
        backend,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_dispatcher_from_paths_with_backend(
    global_path: &Path,
    project_path: &Path,
    managed_path: &Path,
    project_root: &Path,
    no_hooks_flag: bool,
    headless: bool,
    trust_store_path: &Path,
    confirm: &dyn Fn(&str) -> bool,
    sandbox_backend: &str,
) -> HookDispatcher {
    let mut global = load_settings_file(global_path);
    let compatibility_path = project_root.join(".claude/settings.json");
    let mut project = load_settings_file(&compatibility_path);
    if project_path != compatibility_path {
        let native_project = load_settings_file(project_path);
        project.disable_all_hooks |= native_project.disable_all_hooks;
        merge_into(&mut project.hooks, native_project.hooks);
    }
    let mut managed = load_settings_file(managed_path);

    // A repository-controlled `disableAllHooks` value must not disable
    // user-global guard hooks. Project hooks are individually trust-gated
    // below, so the project-level disable switch is intentionally inert.
    let disable_non_managed = no_hooks_flag || global.disable_all_hooks;
    if project.disable_all_hooks {
        tracing::warn!(
            "hooks: ignoring project-local disableAllHooks because project settings cannot disable global hooks"
        );
    }

    // Hook files that fail to load must be visible, and for the sources that
    // carry guards (managed, and global unless the user turned hooks off) the
    // lost guards must not turn into "no guard": deny every PreToolUse.
    let mut notices: Vec<String> = Vec::new();
    let mut fail_closed: Vec<String> = Vec::new();
    for (name, source, guards_active) in [
        ("managed", &mut managed, true),
        ("global", &mut global, !disable_non_managed),
    ] {
        if guards_active && (source.unloadable || source.dropped_pre_tool_use) {
            fail_closed.push(format!(
                "{name} hooks file could not be fully loaded, so its PreToolUse guards are unknown; denying tool calls until it is fixed ({})",
                source.problems.join("; ")
            ));
        }
        notices.append(&mut source.problems);
    }
    notices.append(&mut project.problems);
    let fail_closed = (!fail_closed.is_empty()).then(|| fail_closed.join(" | "));

    let mut merged: HooksConfig = HashMap::new();
    let (mut store, store_readable) = match load_trust_store_checked(trust_store_path) {
        Ok(store) => (store, true),
        Err(error) => {
            notices.push(format!("hooks: {error}; workspace hook files will be re-reviewed and trust decisions will not persist"));
            (HashSet::new(), false)
        }
    };
    let loaded_store = store.clone();
    let mut approved_pins = HashMap::new();

    if !disable_non_managed {
        pin_configured_hook_content(
            &global.hooks,
            "global",
            project_root,
            &mut store,
            store_readable,
            headless,
            confirm,
            &mut approved_pins,
        );
        merge_into(&mut merged, global.hooks);
        let filtered_project = filter_trusted_project_hooks(
            project.hooks,
            project_root,
            &mut store,
            headless,
            confirm,
            &mut approved_pins,
        );
        merge_into(&mut merged, filtered_project);
    }

    pin_configured_hook_content(
        &managed.hooks,
        "managed",
        project_root,
        &mut store,
        store_readable,
        headless,
        confirm,
        &mut approved_pins,
    );
    merge_into(&mut merged, managed.hooks);

    if store != loaded_store
        && let Err(error) = save_trust_store_merged(trust_store_path, &loaded_store, &store)
    {
        tracing::warn!("hooks: {error} (trust decisions won't persist)");
        notices.push(format!("hooks: {error} (trust decisions won't persist)"));
    }

    HookDispatcher::from_config_with_backend_root_and_pins(
        &merged,
        sandbox_backend,
        project_root,
        approved_pins,
    )
    .map(|dispatcher| dispatcher.with_load_problems(fail_closed.clone(), notices.clone()))
    .unwrap_or_else(|e| {
        tracing::warn!("hooks: invalid merged config, disabling hooks: {e}");
        notices.push(format!("hooks: invalid merged config, hooks disabled: {e}"));
        let reason = fail_closed.clone().or_else(|| {
            Some(format!(
                "hooks could not be built from the merged config ({e}); denying tool calls"
            ))
        });
        HookDispatcher::from_config_with_backend_and_root(
            &HashMap::new(),
            sandbox_backend,
            project_root,
        )
        .expect("empty config is always valid")
        .with_load_problems(reason, notices.clone())
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
    sandbox_backend: &str,
) -> HookDispatcher {
    let trust_unavailable = crate::paths::artifact_disabled("hook trust");
    let dispatcher = build_dispatcher_from_paths_with_backend(
        &global_settings_path(paths),
        &project_settings_path(paths),
        &managed_settings_path(),
        &current_project_root(paths),
        no_hooks_flag,
        headless || trust_unavailable,
        &default_trust_store_path(paths),
        &confirm_untrusted_hook,
        sandbox_backend,
    );
    for notice in dispatcher.startup_notices() {
        eprintln!("warning: {notice}");
    }
    dispatcher
}

#[cfg(test)]
mod confirmation_tests {
    use super::*;

    #[test]
    fn confirmation_displays_event_and_matcher_that_are_bound_by_the_hash() {
        let handler = HookHandler {
            kind: "command".into(),
            command: Some("guard".into()),
            args: Some(vec![]),
            timeout: None,
            is_async: false,
            condition: None,
            once: false,
            trust: super::super::settings::HookTrust::Sandboxed,
            env: Default::default(),
        };
        let description = hook_confirmation_description(
            "PreToolUse",
            Some("bash"),
            &handler,
            &HookContentPins::default(),
        );
        assert!(description.contains("event=\"PreToolUse\""));
        assert!(description.contains("matcher=\"bash\""));
    }
}
