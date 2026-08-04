use std::io::Write;

use crate::extras::hooks::settings::HookHandler;
use crate::extras::hooks::trust;

fn handler(command: &str) -> HookHandler {
    HookHandler {
        kind: "command".to_string(),
        command: Some(command.to_string()),
        args: None,
        timeout: Some(5),
        is_async: false,
        condition: None,
        once: false,
        trust: crate::extras::hooks::settings::HookTrust::Trusted,
        env: Default::default(),
    }
}

fn unique_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir()
        .canonicalize()
        .expect("temporary directory must be canonicalizable")
        .join(format!(
            "zerostack-hooks-trust-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
        .join("artifact.json")
}

fn write_settings(path: &std::path::Path, json: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(json.as_bytes()).unwrap();
}

fn missing_path(name: &str) -> std::path::PathBuf {
    unique_path(&format!("missing-{name}"))
}

fn project_root() -> std::path::PathBuf {
    std::path::PathBuf::from("/repo/test-project")
}

#[test]
fn hash_changes_when_command_changes() {
    let root = std::path::Path::new("/repo/a");
    let h1 = handler("echo one");
    let h2 = handler("echo two");
    let hash1 = trust::hash_hook_binding(root, "PreToolUse", Some("Bash"), &h1);
    let hash2 = trust::hash_hook_binding(root, "PreToolUse", Some("Bash"), &h2);
    assert_ne!(hash1, hash2);
}

#[test]
fn hash_stable_for_identical_binding() {
    let root = std::path::Path::new("/repo/a");
    let h1 = handler("echo one");
    let h2 = handler("echo one");
    let hash1 = trust::hash_hook_binding(root, "PreToolUse", Some("Bash"), &h1);
    let hash2 = trust::hash_hook_binding(root, "PreToolUse", Some("Bash"), &h2);
    assert_eq!(hash1, hash2);
}

#[test]
fn hash_changes_when_matcher_changes() {
    let root = std::path::Path::new("/repo/a");
    let h = handler("echo one");
    let hash1 = trust::hash_hook_binding(root, "PreToolUse", Some("Bash"), &h);
    let hash2 = trust::hash_hook_binding(root, "PreToolUse", Some("*"), &h);
    assert_ne!(hash1, hash2);
}

#[test]
fn hash_changes_when_project_root_changes() {
    let h = handler("./guard.sh");
    let hash1 = trust::hash_hook_binding(
        std::path::Path::new("/repo/project-a"),
        "PreToolUse",
        Some("Bash"),
        &h,
    );
    let hash2 = trust::hash_hook_binding(
        std::path::Path::new("/repo/project-b"),
        "PreToolUse",
        Some("Bash"),
        &h,
    );
    assert_ne!(
        hash1, hash2,
        "trusting a binding in one project must not trust the identical binding in another"
    );
}

#[test]
fn hash_changes_when_subprocess_trust_or_explicit_environment_changes() {
    let root = std::path::Path::new("/repo/a");
    let baseline = handler("echo one");
    let mut sandboxed = baseline.clone();
    sandboxed.trust = crate::extras::hooks::settings::HookTrust::Sandboxed;
    let mut with_env = baseline.clone();
    with_env
        .env
        .insert("TOKEN_FILE".into(), "/safe/path".into());

    let baseline_hash = trust::hash_hook_binding(root, "PreToolUse", None, &baseline);
    assert_ne!(
        baseline_hash,
        trust::hash_hook_binding(root, "PreToolUse", None, &sandboxed)
    );
    assert_ne!(
        baseline_hash,
        trust::hash_hook_binding(root, "PreToolUse", None, &with_env)
    );
}

#[test]
fn trust_store_round_trips_and_is_visible_to_a_fresh_load() {
    let path = unique_path("store");
    let _ = std::fs::remove_file(&path);

    assert!(!trust::load_trust_store(&path).contains("abc123"));
    let mut store = trust::load_trust_store(&path);
    store.insert("abc123".to_string());
    trust::save_trust_store(&path, &store).unwrap();

    // Simulates a child process independently loading the same trust file.
    assert!(trust::load_trust_store(&path).contains("abc123"));
    assert!(!trust::load_trust_store(&path).contains("does-not-exist"));
}

#[test]
fn global_only_settings_load_without_consulting_confirmation() {
    let global = unique_path("global");
    write_settings(
        &global,
        r#"{"hooks": {"PreToolUse": [{"hooks": [{"type": "command", "command": "true"}]}]}}"#,
    );
    let project = missing_path("project");
    let managed = missing_path("managed");
    let trust_path = unique_path("trust");

    let dispatcher = trust::build_dispatcher_from_paths(
        &global,
        &project,
        &managed,
        &project_root(),
        false,
        false,
        &trust_path,
        &|_| panic!("global-sourced hooks must never require confirmation"),
    );

    assert!(!dispatcher.is_empty());
}

#[test]
fn missing_files_are_not_an_error() {
    let global = missing_path("g");
    let project = missing_path("p");
    let managed = missing_path("m");
    let trust_path = unique_path("trust");

    let dispatcher = trust::build_dispatcher_from_paths(
        &global,
        &project,
        &managed,
        &project_root(),
        false,
        false,
        &trust_path,
        &|_| false,
    );
    assert!(dispatcher.is_empty());
}

#[test]
fn project_config_trust_project_disable_all_hooks_cannot_disable_global_or_managed_hooks() {
    let global = unique_path("global2");
    write_settings(
        &global,
        r#"{"hooks": {"PreToolUse": [{"hooks": [{"type": "command", "command": "true"}]}]}}"#,
    );
    let project = unique_path("project2");
    write_settings(&project, r#"{"disableAllHooks": true}"#);
    let managed = unique_path("managed2");
    write_settings(
        &managed,
        r#"{"hooks": {"PreToolUse": [{"hooks": [{"type": "command", "command": "echo", "args": ["managed"]}]}]}}"#,
    );
    let trust_path = unique_path("trust2");

    let dispatcher = trust::build_dispatcher_from_paths(
        &global,
        &project,
        &managed,
        &project_root(),
        false,
        false,
        &trust_path,
        &|_| panic!("no project hooks to confirm here"),
    );

    // Repository-controlled settings cannot turn off either trusted source.
    assert!(!dispatcher.is_empty());
    assert_eq!(dispatcher.handlers_for("PreToolUse", "bash").len(), 2);
}

#[test]
fn headless_unconfirmed_project_hook_is_skipped_without_confirmation() {
    let global = missing_path("g3");
    let project = unique_path("project3");
    write_settings(
        &project,
        r#"{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo untrusted"}]}]}}"#,
    );
    let managed = missing_path("m3");
    let trust_path = unique_path("trust3");
    let _ = std::fs::remove_file(&trust_path);

    let dispatcher = trust::build_dispatcher_from_paths(
        &global,
        &project,
        &managed,
        &project_root(),
        false,
        true, // headless
        &trust_path,
        &|_| panic!("headless must never prompt for confirmation"),
    );

    assert!(dispatcher.is_empty());
}

#[test]
fn headless_unconfirmed_project_hook_does_not_disable_global_hooks() {
    let global = unique_path("global-headless");
    write_settings(
        &global,
        r#"{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo global"}]}]}}"#,
    );
    let project = unique_path("project-headless");
    write_settings(
        &project,
        r#"{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo untrusted"}]}]}}"#,
    );
    let managed = missing_path("managed-headless");
    let trust_path = unique_path("trust-headless");
    let _ = std::fs::remove_file(&trust_path);

    let dispatcher = trust::build_dispatcher_from_paths(
        &global,
        &project,
        &managed,
        &project_root(),
        false,
        true,
        &trust_path,
        &|_| panic!("headless must never prompt for confirmation"),
    );

    assert_eq!(dispatcher.handlers_for("PreToolUse", "bash").len(), 1);
}

#[test]
fn interactive_confirmation_exposes_args_and_condition_and_persists_binding() {
    let global = missing_path("g4");
    let project = unique_path("project4");
    write_settings(
        &project,
        r#"{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "sh", "args": ["-c", "echo ARG; touch /tmp/pwned && printf '%s' \"$TOKEN\""], "if": "test -f \"$HOME/.allow\" && echo CONDITION; false || true", "trust": "trusted", "env": {"TOKEN_FILE": "top-secret-value"}}]}]}}"#,
    );
    let managed = missing_path("m4");
    let trust_path = unique_path("trust4");
    let _ = std::fs::remove_file(&trust_path);
    let expected_handler = HookHandler {
        kind: "command".to_string(),
        command: Some("sh".to_string()),
        args: Some(vec![
            "-c".to_string(),
            "echo ARG; touch /tmp/pwned && printf '%s' \"$TOKEN\"".to_string(),
        ]),
        timeout: None,
        is_async: false,
        condition: Some("test -f \"$HOME/.allow\" && echo CONDITION; false || true".to_string()),
        once: false,
        trust: crate::extras::hooks::settings::HookTrust::Trusted,
        env: [("TOKEN_FILE".to_string(), "top-secret-value".to_string())]
            .into_iter()
            .collect(),
    };
    let expected_hash = trust::hash_hook_binding(
        &project_root(),
        "PreToolUse",
        Some("Bash"),
        &expected_handler,
    );

    let dispatcher = trust::build_dispatcher_from_paths(
        &global,
        &project,
        &managed,
        &project_root(),
        false,
        false,
        &trust_path,
        &|description| {
            assert!(description.starts_with(
                "executable argv=[\"sh\",\"-c\",\"echo ARG; touch /tmp/pwned && printf '%s' \\\"$TOKEN\\\"\"]; shell condition=\"test -f \\\"$HOME/.allow\\\" && echo CONDITION; false || true\"; subprocess trust=\"trusted\"; explicit env keys=[\"TOKEN_FILE\"]; env binding sha256=\""
            ));
            assert!(description.ends_with('"'));
            assert!(!description.contains("top-secret-value"));
            true
        },
    );
    assert!(!dispatcher.is_empty());
    assert!(
        trust::load_trust_store(&trust_path).contains(&expected_hash),
        "acceptance must persist the unchanged full binding hash"
    );

    // Re-running against the same trust store should not need confirmation
    // again (a changed/declined confirm callback would panic/return false).
    let dispatcher2 = trust::build_dispatcher_from_paths(
        &global,
        &project,
        &managed,
        &project_root(),
        false,
        false,
        &trust_path,
        &|_| panic!("should already be trusted from the previous run"),
    );
    assert!(!dispatcher2.is_empty());
}

#[test]
fn trust_from_one_project_root_does_not_carry_over_to_another() {
    let global = missing_path("g4b");
    let project = unique_path("project4b");
    write_settings(
        &project,
        r#"{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo trust-me"}]}]}}"#,
    );
    let managed = missing_path("m4b");
    let trust_path = unique_path("trust4b");
    let _ = std::fs::remove_file(&trust_path);
    let root_a = std::path::PathBuf::from("/repo/project-a");
    let root_b = std::path::PathBuf::from("/repo/project-b");

    // Trust the binding under project root A.
    let dispatcher = trust::build_dispatcher_from_paths(
        &global,
        &project,
        &managed,
        &root_a,
        false,
        false,
        &trust_path,
        &|_| true,
    );
    assert!(!dispatcher.is_empty());

    // The identical binding (same settings file, same command) under a
    // different project root must still require confirmation: declining it
    // must exclude the hook, proving trust did not carry over.
    let dispatcher2 = trust::build_dispatcher_from_paths(
        &global,
        &project,
        &managed,
        &root_b,
        false,
        false,
        &trust_path,
        &|_| false,
    );
    assert!(dispatcher2.is_empty());
}

#[test]
fn interactive_confirmation_declined_excludes_the_hook() {
    let global = missing_path("g5");
    let project = unique_path("project5");
    write_settings(
        &project,
        r#"{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo nope"}]}]}}"#,
    );
    let managed = missing_path("m5");
    let trust_path = unique_path("trust5");
    let _ = std::fs::remove_file(&trust_path);

    let dispatcher = trust::build_dispatcher_from_paths(
        &global,
        &project,
        &managed,
        &project_root(),
        false,
        false,
        &trust_path,
        &|_| false,
    );
    assert!(dispatcher.is_empty());
}

#[test]
fn no_hooks_flag_excludes_non_managed_but_not_managed() {
    let global = unique_path("global6");
    write_settings(
        &global,
        r#"{"hooks": {"PreToolUse": [{"hooks": [{"type": "command", "command": "true"}]}]}}"#,
    );
    let project = missing_path("p6");
    let managed = unique_path("managed6");
    write_settings(
        &managed,
        r#"{"hooks": {"PreToolUse": [{"hooks": [{"type": "command", "command": "true"}]}]}}"#,
    );
    let trust_path = unique_path("trust6");

    let dispatcher = trust::build_dispatcher_from_paths(
        &global,
        &project,
        &managed,
        &project_root(),
        true, // --no-hooks
        false,
        &trust_path,
        &|_| panic!("no project hooks here"),
    );

    assert!(!dispatcher.is_empty());
    assert_eq!(dispatcher.handlers_for("PreToolUse", "bash").len(), 1);
}
