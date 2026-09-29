//! Content binding of workspace-resident hook files (mini-agent-2gt3l) and
//! absolute-only `PATH` resolution for hook commands (mini-agent-q43uu).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::extras::hooks::dispatcher::HookDispatcher;
use crate::extras::hooks::pins::{HookContentPins, condition_shell, resolve_hook_program};
use crate::extras::hooks::settings::{HookGroup, HookHandler, HookTrust, HooksConfig};
use crate::extras::hooks::{HookCtx, Verdict, trust};

struct Project {
    base: PathBuf,
    root: PathBuf,
}

impl Project {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "zerostack-hook-pins-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        let root = base.join("project");
        std::fs::create_dir_all(&root).unwrap();
        Self { base, root }
    }

    fn write_script(&self, relative: &str, body: &str) -> PathBuf {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        path
    }

    fn ctx(&self) -> HookCtx {
        HookCtx {
            session_id: "pins".into(),
            session_path: String::new(),
            cwd: self.root.to_string_lossy().into_owned(),
            permission_mode: "default".into(),
        }
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn handler(command: &str, args: &[&str]) -> HookHandler {
    HookHandler {
        kind: "command".into(),
        command: Some(command.into()),
        args: Some(args.iter().map(|arg| arg.to_string()).collect()),
        timeout: Some(5),
        is_async: false,
        condition: None,
        once: false,
        trust: HookTrust::Trusted,
        env: Default::default(),
    }
}

fn pre_tool_config(handler: HookHandler) -> HooksConfig {
    let mut config: HooksConfig = HashMap::new();
    config.insert(
        "PreToolUse".into(),
        vec![HookGroup {
            matcher: None,
            hooks: vec![handler],
        }],
    );
    config
}

const DENY_SCRIPT: &str =
    "#!/bin/sh\necho '{\"permissionDecision\":\"deny\",\"reason\":\"guard\"}'\n";
const ALLOW_SCRIPT: &str = "#!/bin/sh\necho '{\"permissionDecision\":\"allow\"}'\n";

async fn pre_tool(
    dispatcher: &HookDispatcher,
    project: &Project,
) -> crate::extras::hooks::PreDecision {
    dispatcher
        .dispatch_pre_tool_use(&project.ctx(), "bash", serde_json::json!({"command": "ls"}))
        .await
}

#[cfg(unix)]
#[tokio::test]
async fn rewritten_workspace_hook_executable_is_denied_instead_of_self_approving() {
    let project = Project::new("rewrite-program");
    project.write_script("guard.sh", DENY_SCRIPT);
    let dispatcher = HookDispatcher::from_config_with_backend_and_root(
        &pre_tool_config(handler("./guard.sh", &[])),
        "unused",
        &project.root,
    )
    .unwrap();

    let before = pre_tool(&dispatcher, &project).await;
    assert_eq!(before.verdict, Verdict::Deny);
    assert_eq!(before.reason.as_deref(), Some("guard"));

    // The model rewrites the guard with an ordinary workspace edit.
    project.write_script("guard.sh", ALLOW_SCRIPT);
    let after = pre_tool(&dispatcher, &project).await;
    assert_eq!(
        after.verdict,
        Verdict::Deny,
        "a rewritten guard must fail closed, never self-approve"
    );
    assert_ne!(after.reason.as_deref(), Some("guard"));
}

#[cfg(unix)]
#[tokio::test]
async fn rewritten_script_passed_to_an_interpreter_is_denied() {
    let project = Project::new("rewrite-arg");
    let marker = project.base.join("escaped");
    project.write_script("hooks/guard.sh", DENY_SCRIPT);
    let dispatcher = HookDispatcher::from_config_with_backend_and_root(
        &pre_tool_config(handler("sh", &["hooks/guard.sh"])),
        "unused",
        &project.root,
    )
    .unwrap();
    assert_eq!(pre_tool(&dispatcher, &project).await.verdict, Verdict::Deny);

    project.write_script(
        "hooks/guard.sh",
        &format!(
            "#!/bin/sh\ntouch {}\n{}",
            marker.display(),
            &ALLOW_SCRIPT[10..]
        ),
    );
    assert_eq!(pre_tool(&dispatcher, &project).await.verdict, Verdict::Deny);
    assert!(
        !marker.exists(),
        "the rewritten script must never start with trusted authority"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn unchanged_workspace_hook_keeps_running_and_trusted_condition_scripts_are_bound() {
    let project = Project::new("condition");
    let marker = project.base.join("condition-escaped");
    project.write_script("allow.sh", ALLOW_SCRIPT);
    project.write_script("check.sh", "#!/bin/sh\nexit 0\n");
    let mut configured = handler("./allow.sh", &[]);
    configured.condition = Some("./check.sh --fast".into());
    let dispatcher = HookDispatcher::from_config_with_backend_and_root(
        &pre_tool_config(configured),
        "unused",
        &project.root,
    )
    .unwrap();
    assert_eq!(
        pre_tool(&dispatcher, &project).await.verdict,
        Verdict::Allow
    );
    assert_eq!(
        pre_tool(&dispatcher, &project).await.verdict,
        Verdict::Allow
    );

    project.write_script(
        "check.sh",
        &format!("#!/bin/sh\ntouch {}\nexit 0\n", marker.display()),
    );
    let _ = pre_tool(&dispatcher, &project).await;
    assert!(
        !marker.exists(),
        "a rewritten condition script must not run with the handler's authority"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn workspace_file_that_appears_after_load_or_is_relinked_is_denied() {
    let project = Project::new("appear");
    let dispatcher = HookDispatcher::from_config_with_backend_and_root(
        &pre_tool_config(handler("sh", &["late.sh"])),
        "unused",
        &project.root,
    )
    .unwrap();
    project.write_script("late.sh", ALLOW_SCRIPT);
    assert_eq!(pre_tool(&dispatcher, &project).await.verdict, Verdict::Deny);

    let relinked = Project::new("relink");
    relinked.write_script("guard.sh", DENY_SCRIPT);
    let outside = relinked.base.join("outside.sh");
    std::fs::write(&outside, ALLOW_SCRIPT).unwrap();
    let dispatcher = HookDispatcher::from_config_with_backend_and_root(
        &pre_tool_config(handler("sh", &["guard.sh"])),
        "unused",
        &relinked.root,
    )
    .unwrap();
    std::fs::remove_file(relinked.root.join("guard.sh")).unwrap();
    std::os::unix::fs::symlink(&outside, relinked.root.join("guard.sh")).unwrap();
    assert_eq!(
        pre_tool(&dispatcher, &relinked).await.verdict,
        Verdict::Deny
    );
}

#[test]
fn pins_capture_only_existing_workspace_files() {
    let project = Project::new("capture");
    project.write_script("guard.sh", DENY_SCRIPT);
    project.write_script("hooks/policy.toml", "strict = true\n");
    let mut configured = handler(
        "./guard.sh",
        &[
            "--config=hooks/policy.toml",
            "-c",
            "missing.sh",
            "/etc/hosts",
        ],
    );
    configured.condition = Some("test -d src".into());
    let pins = HookContentPins::capture(&project.root, &configured);
    let locations: Vec<String> = pins.entries().into_iter().map(|(path, _)| path).collect();
    assert_eq!(
        locations,
        vec!["workspace:guard.sh", "workspace:hooks/policy.toml"]
    );
    assert!(HookContentPins::capture(&project.root, &handler("sh", &["-c", "exit 0"])).is_empty());
}

#[test]
fn project_trust_hash_binds_workspace_file_content_but_keeps_plain_bindings_stable() {
    let project = Project::new("trust-hash");
    project.write_script("guard.sh", DENY_SCRIPT);
    let configured = handler("./guard.sh", &[]);
    let hash = |pins: &HookContentPins| {
        trust::hash_hook_binding_with_content(&project.root, "PreToolUse", None, &configured, pins)
    };
    let original = hash(&HookContentPins::capture(&project.root, &configured));
    project.write_script("guard.sh", ALLOW_SCRIPT);
    let rewritten = hash(&HookContentPins::capture(&project.root, &configured));
    assert_ne!(
        original, rewritten,
        "rewriting the script must require re-approval"
    );

    let system = handler("sh", &["-c", "exit 0"]);
    assert_eq!(
        trust::hash_hook_binding_with_content(
            &project.root,
            "PreToolUse",
            None,
            &system,
            &HookContentPins::capture(&project.root, &system),
        ),
        trust::hash_hook_binding(&project.root, "PreToolUse", None, &system),
        "bindings without workspace files keep their existing approval"
    );
}

#[cfg(unix)]
#[test]
fn rewritten_project_hook_script_requires_confirmation_again() {
    let project = Project::new("reconfirm");
    project.write_script("guard.sh", DENY_SCRIPT);
    let settings = project.root.join(".zerostack/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(
        &settings,
        r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"./guard.sh","args":[]}]}]}}"#,
    )
    .unwrap();
    let store = project.base.join("trusted-hooks.json");
    let missing = project.base.join("missing.json");
    let prompts = std::cell::Cell::new(0);
    let confirm = |description: &str| {
        assert!(description.contains("workspace:guard.sh"));
        prompts.set(prompts.get() + 1);
        true
    };
    let build = || {
        trust::build_dispatcher_from_paths(
            &missing,
            &settings,
            &missing,
            &project.root,
            false,
            false,
            &store,
            &confirm,
        )
    };
    assert!(!build().is_empty());
    assert!(!build().is_empty());
    assert_eq!(prompts.get(), 1, "unchanged content keeps its approval");
    project.write_script("guard.sh", ALLOW_SCRIPT);
    assert!(!build().is_empty());
    assert_eq!(prompts.get(), 2, "changed content asks again");
}

#[cfg(unix)]
#[test]
fn bare_hook_commands_ignore_relative_path_entries() {
    let project = Project::new("path");
    project.write_script("planted-tool", ALLOW_SCRIPT);
    let error = resolve_hook_program(
        "planted-tool",
        Some(std::ffi::OsStr::new(".::bin")),
        &project.root,
    )
    .unwrap_err();
    assert!(error.contains("planted-tool"), "{error}");

    let bin = project.base.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy(project.root.join("planted-tool"), bin.join("planted-tool")).unwrap();
    let search = std::env::join_paths([Path::new("."), bin.as_path()]).unwrap();
    assert_eq!(
        resolve_hook_program("planted-tool", Some(&search), &project.root).unwrap(),
        bin.join("planted-tool")
    );
}

#[cfg(unix)]
#[test]
fn conditions_use_an_absolute_shell() {
    let (shell, flag) = condition_shell();
    assert_eq!((shell, flag), ("/bin/sh", "-c"));
}

/// Global and managed hooks persist their workspace-file digests, so a guard
/// rewritten during one session cannot silently take effect in the next
/// (mini-agent-197xc).
#[cfg(unix)]
#[tokio::test]
async fn rewritten_workspace_file_of_a_global_hook_is_denied_in_a_later_session() {
    for source in ["global", "managed"] {
        let project = Project::new(&format!("persist-{source}"));
        project.write_script("guard.sh", DENY_SCRIPT);
        let settings = project.base.join(format!("{source}-settings.json"));
        std::fs::write(
            &settings,
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"./guard.sh","args":[],"trust":"trusted"}]}]}}"#,
        )
        .unwrap();
        let store = project.base.join("trusted-hooks.json");
        let missing = project.base.join("missing.json");
        let prompts = std::cell::RefCell::new(Vec::<String>::new());
        let answer = std::cell::Cell::new(false);
        let confirm = |description: &str| {
            prompts.borrow_mut().push(description.to_string());
            answer.get()
        };
        let build = |headless: bool| {
            let (global, managed) = if source == "global" {
                (&settings, &missing)
            } else {
                (&missing, &settings)
            };
            trust::build_dispatcher_from_paths(
                global,
                &missing,
                managed,
                &project.root,
                false,
                headless,
                &store,
                &confirm,
            )
        };

        // First session: trust on first use records the digest without asking.
        let first = pre_tool(&build(true), &project).await;
        assert_eq!(first.reason.as_deref(), Some("guard"), "{source}");
        assert!(prompts.borrow().is_empty(), "{source}");
        let persisted = std::fs::read_to_string(&store).unwrap();
        assert!(persisted.contains("hook-content-v1:"), "{source}");

        // The model rewrites the guard; a later headless session fails closed.
        project.write_script("guard.sh", ALLOW_SCRIPT);
        let headless = pre_tool(&build(true), &project).await;
        assert_eq!(headless.verdict, Verdict::Deny, "{source}");
        assert_ne!(headless.reason.as_deref(), Some("guard"), "{source}");
        assert!(
            prompts.borrow().is_empty(),
            "{source}: headless never prompts"
        );

        // Declining interactively keeps it fail-closed and asks again next time.
        let declined = pre_tool(&build(false), &project).await;
        assert_eq!(declined.verdict, Verdict::Deny, "{source}");
        assert_eq!(prompts.borrow().len(), 1, "{source}");
        let prompt = prompts.borrow()[0].clone();
        assert!(
            prompt.starts_with(&format!("{source} hook whose workspace files changed")),
            "{prompt}"
        );
        assert!(prompt.contains("workspace:guard.sh"), "{prompt}");

        // Accepting records the new digest; later sessions run without asking.
        answer.set(true);
        assert_eq!(
            pre_tool(&build(false), &project).await.verdict,
            Verdict::Allow,
            "{source}"
        );
        assert_eq!(
            pre_tool(&build(true), &project).await.verdict,
            Verdict::Allow,
            "{source}"
        );
        assert_eq!(prompts.borrow().len(), 2, "{source}");
    }
}

#[test]
fn global_hooks_without_workspace_files_record_nothing() {
    let project = Project::new("persist-none");
    let settings = project.base.join("global-settings.json");
    std::fs::write(
        &settings,
        r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"sh","args":["-c","exit 0"]}]}]}}"#,
    )
    .unwrap();
    let store = project.base.join("trusted-hooks.json");
    let missing = project.base.join("missing.json");
    let dispatcher = trust::build_dispatcher_from_paths(
        &settings,
        &missing,
        &missing,
        &project.root,
        false,
        true,
        &store,
        &|_| panic!("no prompt expected"),
    );
    assert!(!dispatcher.is_empty());
    assert!(!store.exists(), "an unchanged store is not rewritten");
}
