#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    use rig::tool::Tool;

    use crate::cli::Cli;
    use crate::config::Config;
    use crate::extras::git_worktree::*;
    use crate::sandbox::CommandLimits;

    #[cfg(feature = "hooks")]
    static ACTIVE_WORKSPACE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(feature = "hooks")]
    struct ScopedActiveWorkspace {
        previous: PathBuf,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    #[cfg(feature = "hooks")]
    impl ScopedActiveWorkspace {
        fn capture() -> Self {
            let guard = ACTIVE_WORKSPACE_TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            Self {
                previous: crate::extras::hooks::active_workspace(),
                _guard: guard,
            }
        }
    }

    #[cfg(feature = "hooks")]
    impl Drop for ScopedActiveWorkspace {
        fn drop(&mut self) {
            crate::extras::hooks::set_active_workspace(&self.previous);
        }
    }

    struct TempRepo(PathBuf);

    impl TempRepo {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("mini-agent-8tbo-{label}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("create temporary repository directory");
            git(&path, ["init", "-b", "main"]);
            git(
                &path,
                ["config", "user.email", "mini-agent@example.invalid"],
            );
            git(&path, ["config", "user.name", "Mini Agent Test"]);
            std::fs::write(path.join("tracked.txt"), "initial\n").expect("write tracked fixture");
            git(&path, ["add", "tracked.txt"]);
            git(&path, ["commit", "-m", "initial"]);
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn install_relative_shell(workspace: &Path) -> PathBuf {
        let bin = workspace.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let executable = bin.join(if cfg!(windows) { "bash.exe" } else { "bash" });
        #[cfg(windows)]
        std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        #[cfg(not(windows))]
        std::fs::write(&executable, b"#!/bin/sh\nexec /bin/sh \"$@\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        executable
    }

    fn git<I, S>(repo: &Path, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("run fixture git");
        assert!(
            output.status.success(),
            "fixture git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout<I, S>(repo: &Path, args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("run fixture git");
        assert!(
            output.status.success(),
            "fixture git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn optional_test_ref_exists(repo: &Path, reference: &str) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["show-ref", "--verify", reference])
            .output()
            .expect("query fixture ref")
            .status
            .success()
    }

    fn test_limits(timeout: Duration) -> CommandLimits {
        CommandLimits {
            timeout,
            stdout_bytes: 16 * 1024,
            stderr_bytes: 16 * 1024,
            combined_bytes: 24 * 1024,
        }
    }

    const TEST_MUTATION_ADMISSION_TIMEOUT: Duration = Duration::from_secs(15);

    async fn wait_for_mutation_marker(path: &Path, task_finished: impl Fn() -> bool, label: &str) {
        tokio::time::timeout(TEST_MUTATION_ADMISSION_TIMEOUT, async {
            while !path.exists() {
                assert!(
                    !task_finished(),
                    "{label} stopped before its marker was written"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{label} did not acquire the process Git mutation lock within {:?}",
                TEST_MUTATION_ADMISSION_TIMEOUT
            )
        });
    }

    async fn acquire_released_mutation_lock(label: &str) -> tokio::sync::OwnedMutexGuard<()> {
        tokio::time::timeout(
            TEST_MUTATION_ADMISSION_TIMEOUT,
            crate::git::runner::acquire_process_git_mutation(),
        )
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{label} did not release the process Git mutation lock within {:?}",
                TEST_MUTATION_ADMISSION_TIMEOUT
            )
        })
    }

    #[test]
    fn production_worktree_module_never_mutates_process_cwd() {
        let source = include_str!("../extras/git_worktree/mod.rs");
        let forbidden = ["set_current_dir", "ChdirGuard"];

        for needle in forbidden {
            assert!(
                !source.contains(needle),
                "production git-worktree code must not contain {needle}"
            );
        }
        assert!(
            !source.contains("error.contains(\"nothing to commit\")"),
            "no-op detection must use staged/tree state, not localized command output"
        );
        assert!(
            !source.contains("[\"reset\", \"--hard\"]"),
            "rollback must never use a symbolic-HEAD precheck followed by unqualified hard reset"
        );
        assert!(
            !source.contains("[\"stash\", \"pop\"]"),
            "stash recovery must apply the exact captured OID and CAS cleanup"
        );
    }

    #[test]
    fn production_tree_has_no_worktree_cwd_transition_helper() {
        fn visit(dir: &Path, files: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|name| name == "tests") {
                        continue;
                    }
                    visit(&path, files);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    files.push(path);
                }
            }
        }

        let mut files = Vec::new();
        visit(Path::new("src"), &mut files);
        for path in files {
            let source = std::fs::read_to_string(&path).unwrap();
            assert!(
                !source.contains("set_worktree_current_dir"),
                "{} reintroduced a process-global worktree CWD transition",
                path.display()
            );
        }
        for path in ["src/ui/mod.rs", "src/ui/app.rs", "src/ui/slash/features.rs"] {
            let source = std::fs::read_to_string(path).unwrap();
            assert!(
                !source.contains("std::env::set_current_dir"),
                "{path} must rebind explicit workspace state"
            );
        }
    }

    #[test]
    fn active_workspace_consumers_do_not_trust_serialized_session_path() {
        for path in [
            "src/ui/app.rs",
            "src/ui/events.rs",
            "src/ui/statusline.rs",
            "src/ui/slash/add.rs",
            "src/ui/slash/features.rs",
            "src/ui/slash/init.rs",
            "src/ui/slash/review.rs",
        ] {
            let source = std::fs::read_to_string(path).unwrap();
            assert!(
                !source.contains("session.working_dir"),
                "{path} must use the active WorkspaceBinding, not serialized session state"
            );
        }

        let startup = include_str!("../startup.rs");
        assert!(
            !startup.contains("self.session.working_dir"),
            "startup runtime consumers must use the captured WorkspaceBinding"
        );

        let slash = include_str!("../ui/slash/mod.rs");
        let replacement = slash
            .split("pub async fn replace_session")
            .nth(1)
            .unwrap()
            .split("pub async fn rebuild_agent_with_client")
            .next()
            .unwrap();
        let prepare = replacement.find("create_client").unwrap();
        for commit in ["*self.client =", "*self.agent =", "*self.session ="] {
            assert!(
                prepare < replacement.find(commit).unwrap(),
                "session replacement must prepare fallible provider state before committing {commit}"
            );
        }
        assert!(
            !replacement.contains("mem::replace"),
            "session replacement must not expose staged session state before activation succeeds"
        );
    }

    #[test]
    fn session_restore_paths_do_not_rebind_shell_capability() {
        for path in [
            "src/startup.rs",
            "src/ui/slash/mod.rs",
            "src/ui/slash/session.rs",
        ] {
            let source = std::fs::read_to_string(path).unwrap();
            assert!(
                !source.contains("rebind_workspace_binding"),
                "{path} must retain the active shell capability outside explicit worktree switches"
            );
        }
    }

    #[tokio::test]
    async fn windows_workspace_authority_worktree_rebind_all_surfaces() {
        let repo = TempRepo::new("ui explicit workspace");
        let repo_shell = install_relative_shell(repo.path());
        git(repo.path(), ["add", "bin"]);
        git(repo.path(), ["commit", "-m", "add workspace shell"]);
        let worktree = repo.path().with_extension("ui explicit linked worktree");
        git(
            repo.path(),
            vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("-b"),
                OsString::from("feature"),
                worktree.as_os_str().to_os_string(),
            ],
        );
        let process_cwd = std::env::current_dir().unwrap();
        let mut session = crate::session::Session::new("test", "test", 1, "test");
        let mut context = crate::context::load(true);
        let mut workspace =
            std::sync::Arc::new(crate::paths::WorkspaceBinding::capture(repo.path()).unwrap());
        let configured_shell = if cfg!(windows) {
            "bin/bash.exe"
        } else {
            "bin/bash"
        };
        let shell_capability =
            crate::sandbox::ShellCapability::resolve(configured_shell, workspace.root(), None)
                .unwrap();
        let mut sandbox = crate::sandbox::Sandbox::new(false, "bwrap")
            .with_resolved_shell(Some(shell_capability))
            .with_workspace_binding(workspace.clone());

        #[cfg(feature = "hooks")]
        let _active_workspace_guard = ScopedActiveWorkspace::capture();

        crate::ui::rebind_worktree_workspace(
            &mut session,
            &mut context,
            &None,
            &mut workspace,
            &mut sandbox,
            &worktree,
            false,
        )
        .unwrap();
        let canonical_worktree = worktree.canonicalize().unwrap();
        assert_eq!(Path::new(session.working_dir.as_str()), workspace.root());
        assert_eq!(workspace.root(), canonical_worktree);
        assert_eq!(context.workspace_root, canonical_worktree);
        assert_eq!(sandbox.workspace_root_for_test(), Some(workspace.root()));
        assert_eq!(
            sandbox.shell_capability().unwrap().executable(),
            worktree
                .join("bin")
                .join(if cfg!(windows) { "bash.exe" } else { "bash" })
                .canonicalize()
                .unwrap()
        );
        assert_eq!(std::env::current_dir().unwrap(), process_cwd);
        assert!(
            crate::agent::builder::build_preamble(&context, false)
                .contains(&canonical_worktree.display().to_string())
        );
        let listed = crate::agent::tools::ListDirTool::new(None, None, None)
            .with_workspace_binding(workspace.clone())
            .call(crate::agent::tools::ListDirArgs { path: None })
            .await
            .unwrap();
        assert!(listed.contains("tracked.txt"));
        #[cfg(unix)]
        {
            let shell_cwd = sandbox.output_command("pwd").await.unwrap();
            assert_eq!(
                String::from_utf8_lossy(&shell_cwd.stdout).trim(),
                canonical_worktree.display().to_string()
            );
            let bang_cwd =
                crate::ui::run_shell_in_workspace("sh", "pwd", workspace.root()).unwrap();
            assert_eq!(
                String::from_utf8_lossy(&bang_cwd.stdout).trim(),
                canonical_worktree.display().to_string()
            );
        }
        let lazygit = crate::ui::lazygit_in_workspace(&worktree);
        assert_eq!(lazygit.get_current_dir(), Some(worktree.as_path()));
        #[cfg(feature = "hooks")]
        assert_eq!(
            crate::extras::hooks::best_effort_ctx().cwd,
            worktree.canonicalize().unwrap().display().to_string()
        );
        std::fs::write(worktree.join("tracked.txt"), "undo stash workspace\n").unwrap();
        let undo_stash = crate::ui::git_stash_in_workspace(&worktree).unwrap();
        assert!(undo_stash.status.success());
        assert_eq!(
            std::fs::read_to_string(worktree.join("tracked.txt")).unwrap(),
            "initial\n"
        );

        crate::ui::rebind_worktree_workspace(
            &mut session,
            &mut context,
            &None,
            &mut workspace,
            &mut sandbox,
            repo.path(),
            false,
        )
        .unwrap();
        let canonical_repo = repo.path().canonicalize().unwrap();
        assert_eq!(Path::new(session.working_dir.as_str()), workspace.root());
        assert_eq!(workspace.root(), canonical_repo);
        assert_eq!(context.workspace_root, canonical_repo);
        assert_eq!(sandbox.workspace_root_for_test(), Some(workspace.root()));
        assert_eq!(
            sandbox.shell_capability().unwrap().executable(),
            repo_shell.canonicalize().unwrap()
        );
        assert_eq!(std::env::current_dir().unwrap(), process_cwd);

        cleanup_worktree(&worktree, "feature", repo.path(), true)
            .await
            .unwrap();
        assert_eq!(std::env::current_dir().unwrap(), process_cwd);
    }

    #[test]
    fn windows_workspace_authority_failed_rebind_retains_previous_state() {
        let root = std::env::temp_dir().join(format!(
            "mini-agent-workspace-rebind-{}",
            uuid::Uuid::new_v4()
        ));
        let original = root.join("original");
        let missing = root.join("missing");
        std::fs::create_dir_all(&original).unwrap();
        let process_cwd = std::env::current_dir().unwrap();
        let mut workspace =
            std::sync::Arc::new(crate::paths::WorkspaceBinding::capture(&original).unwrap());
        let original_root = workspace.root().to_path_buf();
        let mut session = crate::session::Session::new("test", "test", 1, "test");
        session.working_dir = original_root.to_string_lossy().into_owned().into();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let permission = std::sync::Arc::new(std::sync::Mutex::new(
            crate::permission::checker::PermissionChecker::new(
                &crate::permission::PermissionConfigs::default(),
                crate::permission::SecurityMode::Standard,
                Some(original.clone()),
                Some(vec!["standard".to_string()]),
            )
            .unwrap(),
        ));
        let permission = Some(permission);
        let mut sandbox =
            crate::sandbox::Sandbox::new(false, "bwrap").with_workspace_binding(workspace.clone());

        crate::ui::rebind_worktree_workspace(
            &mut session,
            &mut context,
            &permission,
            &mut workspace,
            &mut sandbox,
            &missing,
            false,
        )
        .expect_err("a missing workspace must fail closed");

        assert_eq!(workspace.root(), original_root);
        assert_eq!(Path::new(&session.working_dir), original_root);
        assert_eq!(context.workspace_root, original_root);
        assert_eq!(
            sandbox.workspace_root_for_test(),
            Some(original_root.as_path())
        );
        assert_eq!(std::env::current_dir().unwrap(), process_cwd);
        drop(sandbox);
        drop(workspace);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_relative_shell_failure_rolls_back_every_workspace_authority() {
        let root = std::env::temp_dir().join(format!(
            "mini-agent-shell-rebind-rollback-{}",
            uuid::Uuid::new_v4()
        ));
        let original = root.join("original");
        let replacement = root.join("replacement");
        std::fs::create_dir_all(&original).unwrap();
        std::fs::create_dir_all(&replacement).unwrap();
        let original_shell = install_relative_shell(&original);
        let mut workspace =
            std::sync::Arc::new(crate::paths::WorkspaceBinding::capture(&original).unwrap());
        let original_root = workspace.root().to_path_buf();
        let configured_shell = if cfg!(windows) {
            "bin/bash.exe"
        } else {
            "bin/bash"
        };
        let shell_capability =
            crate::sandbox::ShellCapability::resolve(configured_shell, workspace.root(), None)
                .unwrap();
        let mut sandbox = crate::sandbox::Sandbox::new(false, "bwrap")
            .with_resolved_shell(Some(shell_capability))
            .with_workspace_binding(workspace.clone());
        let mut session = crate::session::Session::new("test", "test", 1, "test");
        session.working_dir = original_root.to_string_lossy().into_owned().into();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let permission = std::sync::Arc::new(std::sync::Mutex::new(
            crate::permission::checker::PermissionChecker::new(
                &crate::permission::PermissionConfigs::default(),
                crate::permission::SecurityMode::PlanWrite,
                Some(original.clone()),
                Some(vec!["planwrite".to_string()]),
            )
            .unwrap(),
        ));
        let permission = Some(permission);
        let original_plan = original.join("PLAN.md");
        let replacement_plan = replacement.join("PLAN.md");

        let error = crate::ui::rebind_worktree_workspace(
            &mut session,
            &mut context,
            &permission,
            &mut workspace,
            &mut sandbox,
            &replacement,
            true,
        )
        .expect_err("missing replacement shell must fail before publication");

        assert!(error.to_string().contains("workspace-relative shell"));
        assert_eq!(workspace.root(), original_root);
        assert_eq!(Path::new(session.working_dir.as_str()), original_root);
        assert_eq!(context.workspace_root, original_root);
        assert_eq!(
            sandbox.workspace_root_for_test(),
            Some(original_root.as_path())
        );
        assert_eq!(
            sandbox.shell_capability().unwrap().executable(),
            original_shell.canonicalize().unwrap()
        );
        let checker = permission
            .as_ref()
            .unwrap()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            checker
                .plan_write_authorization("write", &original_plan.to_string_lossy())
                .is_some()
        );
        assert!(
            checker
                .plan_write_authorization("write", &replacement_plan.to_string_lossy())
                .is_none()
        );
        drop(checker);

        drop((sandbox, workspace, permission));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn windows_workspace_authority_spawn_and_cleanup_keep_one_binding() {
        let sandbox = include_str!("../sandbox.rs");
        let direct = sandbox
            .split("pub(crate) fn wrap_direct_command")
            .nth(2)
            .unwrap()
            .split("fn build_seatbelt_command")
            .next()
            .unwrap();
        assert!(direct.contains(".validate()"));
        assert!(direct.contains("self.working_dir()"));
        assert!(!direct.contains("std::env::current_dir()"));

        let app = include_str!("../ui/app.rs");
        let success = app
            .split("MergeOutcome::Success")
            .nth(1)
            .unwrap()
            .split("MergeOutcome::Conflicts")
            .next()
            .unwrap();
        let retire = success
            .find("retire_workspace_owners_before_cleanup")
            .unwrap();
        let cleanup = success.find("complete_merge(&mut state)").unwrap();
        assert!(
            retire < cleanup,
            "workspace owners must retire before deletion"
        );
    }

    #[test]
    fn worktree_rebind_preserves_no_context_files() {
        let repo = TempRepo::new("no context rebind");
        std::fs::write(repo.path().join("AGENTS.md"), "DO_NOT_INJECT_CONTEXT\n").unwrap();
        git(repo.path(), ["add", "AGENTS.md"]);
        git(repo.path(), ["commit", "-m", "add context fixture"]);

        let process_cwd = std::env::current_dir().unwrap();
        let mut session = crate::session::Session::new("test", "test", 1, "test");
        let mut context = crate::context::load(true);
        let mut workspace =
            std::sync::Arc::new(crate::paths::WorkspaceBinding::capture(&process_cwd).unwrap());
        let mut sandbox =
            crate::sandbox::Sandbox::new(false, "bwrap").with_workspace_binding(workspace.clone());

        crate::ui::rebind_worktree_workspace(
            &mut session,
            &mut context,
            &None,
            &mut workspace,
            &mut sandbox,
            repo.path(),
            true,
        )
        .unwrap();

        assert!(context.agents.is_none());
        assert!(
            !crate::agent::builder::build_preamble(&context, false)
                .contains("DO_NOT_INJECT_CONTEXT")
        );
    }

    #[tokio::test]
    async fn workspace_owner_retirement_waits_for_scoped_blocking_child() {
        let app = include_str!("../ui/app.rs");
        let btw_interrupt = app
            .split("InterruptTarget::Btw =>")
            .nth(1)
            .unwrap()
            .split("InterruptTarget::Validation")
            .next()
            .unwrap();
        assert!(btw_interrupt.contains("retire_scoped_task"));

        let (scope, started_rx, release) =
            crate::agent::runner::AgentWorkScope::new_with_blocking_test_gate();
        let task_scope = scope.clone();
        let task = tokio::spawn(async move {
            task_scope
                .run(async {
                    let _child = crate::agent::runner::spawn_blocking_scoped(|| ());
                    std::future::pending::<()>().await;
                })
                .await;
        });
        tokio::task::spawn_blocking(move || {
            started_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("scoped blocking child should start");
        })
        .await
        .unwrap();

        let mut retirement = tokio::spawn(crate::ui::retire_scoped_task(
            task,
            scope,
            "test owner",
            Duration::from_secs(1),
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut retirement)
                .await
                .is_err(),
            "retirement must wait for scoped blocking children"
        );
        release.release();
        tokio::time::timeout(Duration::from_secs(1), retirement)
            .await
            .expect("retirement should finish after child release")
            .expect("retirement task should not panic")
            .expect("retirement should succeed");
    }

    #[cfg(feature = "hooks")]
    #[tokio::test]
    async fn worktree_rebind_updates_production_hook_child_envelope_and_project_dir() {
        use std::collections::HashMap;

        use crate::extras::hooks::dispatcher::HookDispatcher;
        use crate::extras::hooks::settings::{HookGroup, HookHandler, HookTrust};

        let _dispatcher_guard = crate::tests::fake_model::dispatcher_guard::acquire();
        let _active_workspace_guard = ScopedActiveWorkspace::capture();
        let repo = TempRepo::new("hook workspace rebind");
        let worktree = repo.path().with_extension("hook workspace linked worktree");
        git(
            repo.path(),
            vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("-b"),
                OsString::from("hook-workspace"),
                worktree.as_os_str().to_os_string(),
            ],
        );
        let observed = worktree.join("hook-observed.txt");
        let handler = HookHandler {
            kind: "command".to_string(),
            command: Some("sh".to_string()),
            args: Some(vec![
                "-c".to_string(),
                "printf '%s\\n%s\\n' \"$PWD\" \"$ZEROSTACK_PROJECT_DIR\" > \"$1\"; cat >> \"$1\""
                    .to_string(),
                "hook-workspace-observer".to_string(),
                observed.to_string_lossy().into_owned(),
            ]),
            timeout: Some(5),
            is_async: false,
            condition: None,
            once: false,
            trust: HookTrust::Trusted,
            env: Default::default(),
        };
        let mut config = HashMap::new();
        config.insert(
            "PreToolUse".to_string(),
            vec![HookGroup {
                matcher: None,
                hooks: vec![handler],
            }],
        );
        let dispatcher =
            HookDispatcher::from_config_with_backend_and_root(&config, "unused", repo.path())
                .unwrap();
        crate::extras::hooks::init_dispatcher(dispatcher);

        let process_cwd = std::env::current_dir().unwrap();
        let mut session = crate::session::Session::new("test", "test", 1, "test");
        let mut context = crate::context::load(true);
        let mut workspace =
            std::sync::Arc::new(crate::paths::WorkspaceBinding::capture(&process_cwd).unwrap());
        let mut sandbox =
            crate::sandbox::Sandbox::new(false, "bwrap").with_workspace_binding(workspace.clone());
        crate::ui::rebind_worktree_workspace(
            &mut session,
            &mut context,
            &None,
            &mut workspace,
            &mut sandbox,
            &worktree,
            false,
        )
        .unwrap();

        let ctx = crate::extras::hooks::best_effort_ctx();
        let dispatcher = crate::extras::hooks::get_dispatcher().expect("production dispatcher");
        let _ = dispatcher
            .dispatch_pre_tool_use(&ctx, "bash", serde_json::json!({"command": "true"}))
            .await;

        let captured = std::fs::read_to_string(&observed).expect("hook observation");
        let mut sections = captured.splitn(3, '\n');
        let expected = worktree.canonicalize().unwrap().display().to_string();
        assert_eq!(sections.next(), Some(expected.as_str()));
        assert_eq!(sections.next(), Some(expected.as_str()));
        let envelope: serde_json::Value = serde_json::from_str(
            sections
                .next()
                .expect("hook envelope follows cwd observations"),
        )
        .unwrap();
        assert_eq!(envelope["cwd"], expected);
        assert_eq!(ctx.cwd, expected);
        assert_eq!(std::env::current_dir().unwrap(), process_cwd);

        std::fs::remove_file(observed).unwrap();
        crate::ui::rebind_worktree_workspace(
            &mut session,
            &mut context,
            &None,
            &mut workspace,
            &mut sandbox,
            repo.path(),
            false,
        )
        .unwrap();
        cleanup_worktree(&worktree, "hook-workspace", repo.path(), true)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn mutation_fails_closed_when_repository_identity_is_unavailable() {
        let directory = std::env::temp_dir().join(format!(
            "mini-agent-8tbo-not-a-repo-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("untouched.txt"), "untouched\n").unwrap();

        let error = worktree_auto_commit_all(&directory)
            .await
            .expect_err("mutation must require a common Git directory identity");

        assert!(
            error.contains("repository identity"),
            "unexpected error: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(directory.join("untouched.txt")).unwrap(),
            "untouched\n"
        );
        assert!(!directory.join(".git").exists());
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn auto_commit_all_includes_untracked_files_and_leaves_a_verified_clean_tree() {
        let repo = TempRepo::new("auto commit untracked");
        std::fs::write(repo.path().join("new.txt"), "new\n").unwrap();

        worktree_auto_commit_all(repo.path()).await.unwrap();

        assert_eq!(git_stdout(repo.path(), ["show", "HEAD:new.txt"]), "new");
        assert_eq!(worktree_has_uncommitted(repo.path()).await, Ok(false));
    }

    #[tokio::test]
    async fn worktree_status_errors_fail_closed() {
        let missing = std::env::temp_dir().join(format!(
            "mini-agent-8tbo-missing-status-{}",
            uuid::Uuid::new_v4()
        ));
        let error = worktree_has_uncommitted(&missing)
            .await
            .expect_err("missing worktree status must not be interpreted as clean");
        assert!(error.contains("failed to resolve worktree"));
    }

    #[test]
    fn direct_merge_uses_the_typed_supervised_transaction() {
        let app = include_str!("../ui/app.rs");
        let direct_merge = app
            .split("DeferredWorktreeAction::Merge")
            .nth(1)
            .expect("direct merge action");
        assert!(
            direct_merge.contains("handle_worktree_merge(info.clone(), target.clone(), true)"),
            "direct /wt-merge must dispatch the typed merge transaction"
        );
        assert!(!app.contains("spawn_merge_agent"));
        assert!(
            !app.contains("cleanup_worktree("),
            "conflict abort must retain the source worktree even in force mode"
        );
        let ui_module = include_str!("../ui/mod.rs");
        assert!(!ui_module.contains("git -C {main_path}"));
        assert!(!app.contains("std::env::set_current_dir"));
        assert!(
            !app.contains("set_worktree_current_dir(&path).await.ok()"),
            "startup must not ignore a failed process-workspace transition"
        );
        assert!(app.contains("rebind_worktree_workspace"));
        let cli = include_str!("../cli.rs");
        assert!(
            cli.contains("Deprecated compatibility flag; cleanup always preserves dirty worktrees")
        );
        assert!(!cli.contains("Force worktree remove and branch delete even if dirty"));
        assert!(app.contains("--wt-force is deprecated; cleanup still preserves dirty worktrees"));
        assert!(ui_module.contains("context.reload_from_binding(no_context_files, &replacement)"));
    }

    #[test]
    fn test_worktree_info_clone() {
        let info = WorktreeInfo {
            branch: "feature-x".into(),
            worktree_path: PathBuf::from("/tmp/wt"),
            main_repo_path: PathBuf::from("/tmp/repo"),
        };
        let cloned = info.clone();
        assert_eq!(cloned.branch, "feature-x");
        assert_eq!(cloned.worktree_path, PathBuf::from("/tmp/wt"));
        assert_eq!(cloned.main_repo_path, PathBuf::from("/tmp/repo"));
    }

    #[test]
    fn test_merge_outcome_success_eq() {
        assert_eq!(MergeOutcome::Success, MergeOutcome::Success);
    }

    #[test]
    fn test_merge_outcome_conflicts_eq() {
        let a = MergeOutcome::Conflicts(vec!["a".into(), "b".into()]);
        let b = MergeOutcome::Conflicts(vec!["a".into(), "b".into()]);
        assert_eq!(a, b);
    }

    #[test]
    fn test_merge_outcome_conflicts_ne() {
        let a = MergeOutcome::Conflicts(vec!["a".into()]);
        let b = MergeOutcome::Conflicts(vec!["b".into()]);
        assert_ne!(a, b);
    }

    #[test]
    fn test_merge_outcome_error_eq() {
        let a = MergeOutcome::Error("msg".into());
        let b = MergeOutcome::Error("msg".into());
        assert_eq!(a, b);
    }

    #[test]
    fn test_merge_outcome_error_ne() {
        let a = MergeOutcome::Error("a".into());
        let b = MergeOutcome::Error("b".into());
        assert_ne!(a, b);
    }

    #[test]
    fn test_merge_outcome_cross_variant_ne() {
        assert_ne!(MergeOutcome::Success, MergeOutcome::Error("err".into()));
        assert_ne!(
            MergeOutcome::Success,
            MergeOutcome::Conflicts(vec!["f".into()])
        );
    }

    #[test]
    fn test_empty_merge_state_preserves_explicit_paths() {
        let info = WorktreeInfo {
            branch: "feat".into(),
            worktree_path: PathBuf::from("/tmp/wt"),
            main_repo_path: PathBuf::from("/tmp/repo"),
        };
        let state = empty_state_for_ui(&info);
        assert_eq!(state.orig_dir, PathBuf::from("/tmp/wt"));
        assert_eq!(state.info.main_repo_path, PathBuf::from("/tmp/repo"));
        assert!(!state.stashed);
    }

    #[test]
    fn test_repo_name_basic() {
        assert_eq!(
            repo_name(&PathBuf::from("/home/user/my-project")),
            "my-project"
        );
    }

    #[test]
    fn test_repo_name_trailing_slash() {
        assert_eq!(repo_name(&PathBuf::from("/home/user/repo/")), "repo");
    }

    #[test]
    fn test_repo_name_empty() {
        assert_eq!(repo_name(&PathBuf::from("")), "unknown");
    }

    #[test]
    fn test_repo_name_root() {
        assert_eq!(repo_name(&PathBuf::from("/")), "unknown");
    }

    #[test]
    fn test_wt_cli_flags_default() {
        let cli = Cli::default();
        assert!(cli.worktree.is_none());
        assert!(!cli.wt_auto_merge);
        assert!(!cli.parallel);
        assert!(cli.wt_base_dir.is_none());
        assert!(!cli.wt_force);
    }

    #[test]
    fn test_wt_cli_flags_enabled() {
        let cli = Cli {
            worktree: Some("feature-x".into()),
            wt_auto_merge: true,
            wt_force: true,
            wt_base_dir: Some("/tmp".into()),
            ..Default::default()
        };
        assert_eq!(cli.worktree.as_deref(), Some("feature-x"));
        assert!(cli.wt_auto_merge);
        assert!(cli.wt_force);
        assert_eq!(cli.wt_base_dir.as_deref(), Some("/tmp"));
    }

    #[test]
    fn test_resolve_wt_auto_merge_cli() {
        let cli = Cli {
            wt_auto_merge: true,
            ..Default::default()
        };
        let cfg = Config::default();
        assert!(cli.resolve_wt_auto_merge(&cfg));
    }

    #[test]
    fn test_resolve_wt_auto_merge_parallel() {
        let cli = Cli {
            parallel: true,
            ..Default::default()
        };
        let cfg = Config::default();
        assert!(cli.resolve_wt_auto_merge(&cfg));
    }

    #[test]
    fn test_resolve_wt_auto_merge_config() {
        let cli = Cli::default();
        let cfg = Config {
            wt_auto_merge: Some(true),
            ..Default::default()
        };
        assert!(cli.resolve_wt_auto_merge(&cfg));
    }

    #[test]
    fn test_resolve_wt_auto_merge_default_false() {
        let cli = Cli::default();
        let cfg = Config::default();
        assert!(!cli.resolve_wt_auto_merge(&cfg));
    }

    #[test]
    fn test_resolve_wt_force_cli() {
        let cli = Cli {
            wt_force: true,
            ..Default::default()
        };
        let cfg = Config::default();
        assert!(cli.resolve_wt_force(&cfg));
    }

    #[test]
    fn test_resolve_wt_force_config() {
        let cli = Cli::default();
        let cfg = Config {
            wt_force: Some(true),
            ..Default::default()
        };
        assert!(cli.resolve_wt_force(&cfg));
    }

    #[test]
    fn test_resolve_wt_force_default_false() {
        let cli = Cli::default();
        let cfg = Config::default();
        assert!(!cli.resolve_wt_force(&cfg));
    }

    #[test]
    fn test_resolve_wt_base_dir_cli() {
        let cli = Cli {
            wt_base_dir: Some("/custom/base".into()),
            ..Default::default()
        };
        let cfg = Config::default();
        assert_eq!(
            cli.resolve_wt_base_dir(&cfg),
            Some(PathBuf::from("/custom/base"))
        );
    }

    #[test]
    fn test_resolve_wt_base_dir_config() {
        let cli = Cli::default();
        let cfg = Config {
            wt_base_dir: Some("/config/base".into()),
            ..Default::default()
        };
        assert_eq!(
            cli.resolve_wt_base_dir(&cfg),
            Some(PathBuf::from("/config/base"))
        );
    }

    #[test]
    fn test_resolve_wt_base_dir_default_none() {
        let cli = Cli::default();
        let cfg = Config::default();
        assert_eq!(cli.resolve_wt_base_dir(&cfg), None);
    }

    #[test]
    fn test_resolve_wt_base_dir_cli_overrides_config() {
        let cli = Cli {
            wt_base_dir: Some("/cli".into()),
            ..Default::default()
        };
        let cfg = Config {
            wt_base_dir: Some("/config".into()),
            ..Default::default()
        };
        assert_eq!(cli.resolve_wt_base_dir(&cfg), Some(PathBuf::from("/cli")));
    }

    #[tokio::test]
    async fn test_default_branch_is_refutable() {
        // Pure-logic: the function returns None for non-existent paths (no git init)
        assert!(
            default_branch(&PathBuf::from("/tmp/nonexistent_repo"))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn git_runner_uses_explicit_repo_with_spaces_and_preserves_relative_reads() {
        let repo = TempRepo::new("repo with spaces");
        let original_cwd = std::env::current_dir().expect("current directory");
        let manifest = std::fs::read_to_string("Cargo.toml").expect("relative manifest read");

        let output = run_git_with_limits_for_test(
            repo.path(),
            &["rev-parse", "--show-toplevel"],
            test_limits(Duration::from_secs(2)),
        )
        .await
        .expect("explicit repository query");

        assert_eq!(
            PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()),
            repo.path().canonicalize().expect("canonical repo")
        );
        assert_eq!(std::env::current_dir().unwrap(), original_cwd);
        assert_eq!(std::fs::read_to_string("Cargo.toml").unwrap(), manifest);
        assert!(!has_merge_conflict(repo.path()).await);
        assert!(conflicted_files(repo.path()).await.is_empty());
    }

    #[tokio::test]
    async fn git_runner_reports_command_failure_without_changing_cwd() {
        let repo = TempRepo::new("failure");
        let original_cwd = std::env::current_dir().unwrap();
        let error = match run_git_with_limits_for_test(
            repo.path(),
            &["definitely-not-a-git-subcommand"],
            test_limits(Duration::from_secs(2)),
        )
        .await
        {
            Ok(_) => panic!("invalid Git command must fail"),
            Err(error) => error,
        };

        assert!(
            error.contains("git test failed"),
            "unexpected error: {error}"
        );
        assert_eq!(std::env::current_dir().unwrap(), original_cwd);
    }

    #[tokio::test]
    async fn worktree_branch_operands_cannot_be_reinterpreted_as_git_options() {
        let repo = TempRepo::new("branch operands");

        let create_error = create(repo.path(), "--orphan", None)
            .await
            .expect_err("option-like branch must be rejected");
        assert!(create_error.contains("invalid Git branch name"));

        let info = WorktreeInfo {
            branch: "--upload-pack=surprise".into(),
            worktree_path: repo.path().with_extension("never-created"),
            main_repo_path: repo.path().to_path_buf(),
        };
        let (_state, outcome) = try_merge(&info, "main").await;
        assert!(
            matches!(outcome, MergeOutcome::Error(error) if error.contains("invalid Git branch name"))
        );

        let base = repo.path().with_extension("metachar worktree base");
        std::fs::create_dir_all(&base).unwrap();
        let (worktree, info) = create(repo.path(), "topic;echo-not-a-shell", Some(&base))
            .await
            .expect("shell metacharacters valid in Git refs must remain one argv operand");
        assert_eq!(info.branch, "topic;echo-not-a-shell");
        assert!(worktree.exists());
        cleanup_worktree(&worktree, "topic;echo-not-a-shell", repo.path(), true)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    fn configure_delay_alias(repo: &Path, name: &str, seconds: &str) {
        git(
            repo,
            [
                OsStr::new("config"),
                OsStr::new(&format!("alias.{name}")),
                OsStr::new(&format!("!sleep {seconds}")),
            ],
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn worktree_concurrent_cwd_isolation() {
        let first = TempRepo::new("concurrent first");
        let second = TempRepo::new("concurrent second");
        configure_delay_alias(first.path(), "delay", "1");
        configure_delay_alias(second.path(), "delay", "1");
        let original_cwd = std::env::current_dir().unwrap();
        let original_manifest = std::fs::read_to_string("Cargo.toml").unwrap();
        let started = Instant::now();

        let relative_reader = async {
            for _ in 0..20 {
                assert_eq!(std::env::current_dir().unwrap(), original_cwd);
                assert_eq!(
                    std::fs::read_to_string("Cargo.toml").unwrap(),
                    original_manifest
                );
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        };
        let (first_result, second_result, ()) = tokio::join!(
            run_git_with_limits_for_test(
                first.path(),
                &["delay"],
                test_limits(Duration::from_secs(3)),
            ),
            run_git_with_limits_for_test(
                second.path(),
                &["delay"],
                test_limits(Duration::from_secs(3)),
            ),
            relative_reader,
        );

        first_result.expect("first independent repository command");
        second_result.expect("second independent repository command");
        assert!(
            started.elapsed() < Duration::from_millis(1800),
            "independent repositories were serialized: {:?}",
            started.elapsed()
        );
        assert_eq!(std::env::current_dir().unwrap(), original_cwd);
        assert_eq!(
            std::fs::read_to_string("Cargo.toml").unwrap(),
            original_manifest
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn same_repository_mutations_are_serialized() {
        let repo = TempRepo::new("same repo serialization");
        configure_delay_alias(repo.path(), "delay", "1");
        let started = Instant::now();

        let (first, second) = tokio::join!(
            run_locked_git_with_limits_for_test(
                repo.path(),
                &["delay"],
                test_limits(Duration::from_secs(4)),
            ),
            run_locked_git_with_limits_for_test(
                repo.path(),
                &["delay"],
                test_limits(Duration::from_secs(4)),
            ),
        );

        first.expect("first same-repository command");
        second.expect("second same-repository command");
        assert!(
            started.elapsed() >= Duration::from_millis(1800),
            "same repository commands overlapped: {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delayed_git_hook_does_not_block_runtime_or_change_cwd() {
        use std::os::unix::fs::PermissionsExt;

        let repo = TempRepo::new("delayed hook");
        std::fs::write(repo.path().join("tracked.txt"), "changed\n").unwrap();
        let hook = repo.path().join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nsleep 1\n").unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();
        let original_cwd = std::env::current_dir().unwrap();
        let timer_started = Instant::now();

        let (commit, timer_elapsed) = tokio::join!(worktree_auto_commit_all(repo.path()), async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            timer_started.elapsed()
        });

        commit.expect("commit with delayed hook");
        assert!(
            timer_elapsed < Duration::from_millis(400),
            "delayed hook blocked the async runtime: {:?}",
            timer_elapsed
        );
        assert_eq!(std::env::current_dir().unwrap(), original_cwd);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_worktree_create_rolls_back_the_new_ref_and_registration() {
        use std::os::unix::fs::PermissionsExt;

        let repo = TempRepo::new("failed create rollback");
        let base = repo.path().with_extension("create base");
        std::fs::create_dir_all(&base).unwrap();
        let target = base.join("create-fail");
        let hook = repo.path().join(".git/hooks/post-checkout");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();

        let error = create(repo.path(), "create-fail", Some(&base))
            .await
            .expect_err("post-checkout failure must fail creation");

        assert!(error.contains("worktree-add"), "unexpected error: {error}");
        assert!(
            !target.exists(),
            "failed create left its worktree directory"
        );
        let ref_status = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["show-ref", "--verify", "refs/heads/create-fail"])
            .status()
            .unwrap();
        assert!(!ref_status.success(), "failed create left its branch ref");
        assert!(
            !git_stdout(repo.path(), ["worktree", "list", "--porcelain"])
                .contains(&target.to_string_lossy().into_owned())
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_create_retains_dirty_hook_output_and_its_exact_branch() {
        use std::os::unix::fs::PermissionsExt;

        let repo = TempRepo::new("dirty failed create");
        let base = repo.path().with_extension("dirty failed create base");
        std::fs::create_dir_all(&base).unwrap();
        let target = base.join("dirty-create");
        let hook = repo.path().join(".git/hooks/post-checkout");
        std::fs::write(&hook, "#!/bin/sh\nprintf recovery > recovery.txt\nexit 1\n").unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();
        let expected = git_stdout(repo.path(), ["rev-parse", "HEAD"]);

        let error = create(repo.path(), "dirty-create", Some(&base))
            .await
            .expect_err("dirty failed create must require manual recovery");

        assert!(error.contains("retained"), "unexpected error: {error}");
        assert_eq!(
            std::fs::read_to_string(target.join("recovery.txt")).unwrap(),
            "recovery"
        );
        assert_eq!(git_stdout(&target, ["rev-parse", "HEAD"]), expected);
        assert_eq!(
            git_stdout(repo.path(), ["rev-parse", "refs/heads/dirty-create"]),
            expected
        );
        assert!(
            git_stdout(repo.path(), ["worktree", "list", "--porcelain"])
                .contains(&target.to_string_lossy().into_owned())
        );
        std::fs::remove_file(target.join("recovery.txt")).unwrap();
        cleanup_worktree(&target, "dirty-create", repo.path(), true)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timed_out_branch_reservation_compare_deletes_its_exact_side_effect() {
        use std::os::unix::fs::PermissionsExt;

        let repo = TempRepo::new("timed branch reservation");
        let base = repo.path().with_extension("timed branch reservation base");
        std::fs::create_dir_all(&base).unwrap();
        let hook = repo.path().join(".git/hooks/reference-transaction");
        std::fs::write(
            &hook,
            "#!/bin/sh\nif [ \"$1\" = committed ]; then sleep 1; fi\nexit 0\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();

        let error = create_with_ref_limits_for_test(
            repo.path(),
            "ref-timeout",
            Some(&base),
            test_limits(Duration::from_millis(100)),
        )
        .await
        .expect_err("branch reservation hook must time out");

        assert!(error.contains("timed out"), "unexpected error: {error}");
        let status = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["show-ref", "--verify", "refs/heads/ref-timeout"])
            .status()
            .unwrap();
        assert!(!status.success(), "timed-out reservation leaked its ref");
        assert!(!base.join("ref-timeout").exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn definite_reservation_failure_never_deletes_a_concurrent_same_oid_branch() {
        use std::os::unix::fs::PermissionsExt;

        let repo = TempRepo::new("definite reservation failure");
        let base = repo
            .path()
            .with_extension("definite reservation failure base");
        std::fs::create_dir_all(&base).unwrap();
        let expected = git_stdout(repo.path(), ["rev-parse", "HEAD"]);
        let hook = repo.path().join(".git/hooks/reference-transaction");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\nif [ \"$1\" = prepared ] && [ ! -e '{}' ]; then\n  printf '%s\\n' {} > '{}'\n  exit 1\nfi\nexit 0\n",
                repo.path().join(".git/refs/heads/definite-race").display(),
                expected,
                repo.path().join(".git/refs/heads/definite-race").display(),
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();

        let error = create(repo.path(), "definite-race", Some(&base))
            .await
            .expect_err("prepared hook rejection must fail reservation");
        assert!(error.contains("create-branch-ref"), "unexpected: {error}");
        assert!(optional_test_ref_exists(
            repo.path(),
            "refs/heads/definite-race"
        ));
        assert_eq!(
            git_stdout(repo.path(), ["rev-parse", "refs/heads/definite-race"]),
            expected
        );
        git(
            repo.path(),
            ["update-ref", "-d", "refs/heads/definite-race"],
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timed_out_worktree_create_rolls_back_the_new_ref_and_registration() {
        use std::os::unix::fs::PermissionsExt;

        let repo = TempRepo::new("timed create rollback");
        let base = repo.path().with_extension("timed create base");
        std::fs::create_dir_all(&base).unwrap();
        let target = base.join("create-timeout");
        let started = repo.path().join("create-timeout-hook-started");
        let hook = repo.path().join(".git/hooks/post-checkout");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\nprintf started > '{}'\nsleep 1\n",
                started.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();

        let repo_path = repo.path().to_path_buf();
        let base_path = base.clone();
        let create_task = tokio::spawn(async move {
            create_with_limits_for_test(
                &repo_path,
                "create-timeout",
                Some(&base_path),
                // Leave enough time for `git worktree add` to reach the hook in
                // large feature-enabled test binaries while still timing out
                // well before the hook's one-second sleep completes.
                test_limits(Duration::from_millis(500)),
            )
            .await
        });
        wait_for_mutation_marker(
            &started,
            || create_task.is_finished(),
            "timed worktree create",
        )
        .await;
        // Keep the Tokio timer driver running while the hook sleeps. Blocking
        // this current-thread test runtime makes the 500 ms production
        // deadline and the one-second hook completion become ready together,
        // which can let the child-success branch win under a loaded suite.
        tokio::time::sleep(Duration::from_millis(1100)).await;

        let error = create_task
            .await
            .expect("create task should not panic")
            .expect_err("timed out create must fail");

        assert!(error.contains("timed out"), "unexpected error: {error}");
        assert!(
            !target.exists(),
            "timed out create left its worktree directory"
        );
        let ref_status = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["show-ref", "--verify", "refs/heads/create-timeout"])
            .status()
            .unwrap();
        assert!(
            !ref_status.success(),
            "timed out create left its branch ref"
        );
        assert!(
            !git_stdout(repo.path(), ["worktree", "list", "--porcelain"])
                .contains(&target.to_string_lossy().into_owned())
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_worktree_create_rolls_back_before_releasing_the_repository() {
        use std::os::unix::fs::PermissionsExt;

        let repo = TempRepo::new("dropped create rollback");
        let base = repo.path().with_extension("dropped create base");
        std::fs::create_dir_all(&base).unwrap();
        let target = base.join("create-dropped");
        let started = repo.path().join("create-hook-started");
        let hook = repo.path().join(".git/hooks/post-checkout");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\nprintf started > '{}'\nsleep 30\n",
                started.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();
        let repo_path = repo.path().to_path_buf();
        let base_path = base.clone();
        let task =
            tokio::spawn(
                async move { create(&repo_path, "create-dropped", Some(&base_path)).await },
            );

        for _ in 0..200 {
            if started.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(started.exists(), "post-checkout hook did not start");
        task.abort();
        let _ = task.await;
        tokio::time::timeout(
            Duration::from_secs(4),
            run_locked_git_with_limits_for_test(
                repo.path(),
                &["status", "--porcelain"],
                test_limits(Duration::from_secs(2)),
            ),
        )
        .await
        .expect("create rollback did not release the repository lock")
        .expect("status after dropped create");

        assert!(
            !target.exists(),
            "dropped create left its worktree directory"
        );
        let ref_status = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["show-ref", "--verify", "refs/heads/create-dropped"])
            .status()
            .unwrap();
        assert!(!ref_status.success(), "dropped create left its branch ref");
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn git_runner_timeout_kills_delayed_alias_tree() {
        let repo = TempRepo::new("timeout");
        configure_delay_alias(repo.path(), "delay", "30");
        let error = match run_git_with_limits_for_test(
            repo.path(),
            &["delay"],
            test_limits(Duration::from_millis(100)),
        )
        .await
        {
            Ok(_) => panic!("slow Git alias must time out"),
            Err(error) => error,
        };

        assert!(error.contains("timed out"), "unexpected error: {error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_git_caller_cancels_owned_process_tree() {
        let repo = TempRepo::new("caller drop");
        let started = repo.path().join("started");
        let leaked = repo.path().join("leaked");
        let alias = format!(
            "!sh -c 'echo started > \"{}\"; sleep 2; echo leaked > \"{}\"'",
            started.display(),
            leaked.display()
        );
        git(
            repo.path(),
            [
                OsStr::new("config"),
                OsStr::new("alias.delayed-write"),
                OsStr::new(&alias),
            ],
        );
        let repo_path = repo.path().to_path_buf();
        let task = tokio::spawn(async move {
            run_git_with_limits_for_test(
                &repo_path,
                &["delayed-write"],
                test_limits(Duration::from_secs(10)),
            )
            .await
        });

        for _ in 0..100 {
            if started.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(started.exists(), "Git alias did not start");
        task.abort();
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(2300)).await;
        assert!(
            !leaked.exists(),
            "cancelled Git descendant survived caller drop"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_merge_during_fetch_restores_stash_and_releases_lock() {
        use std::os::unix::fs::PermissionsExt;

        let repo = TempRepo::new("drop during fetch");
        let remote = repo.path().with_extension("drop fetch bare remote");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, ["init", "--bare"]);
        git(
            repo.path(),
            vec![
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote.as_os_str().to_os_string(),
            ],
        );
        git(repo.path(), ["push", "-u", "origin", "main"]);
        git(repo.path(), ["branch", "feature"]);
        std::fs::write(repo.path().join("tracked.txt"), "dirty\n").unwrap();
        let fixture_id = uuid::Uuid::new_v4();
        let started = std::env::temp_dir().join(format!("mini-agent-8tbo-fetch-{fixture_id}"));
        let upload_pack =
            std::env::temp_dir().join(format!("mini-agent-8tbo-uploadpack-{fixture_id}"));
        std::fs::write(
            &upload_pack,
            format!(
                "#!/bin/sh\nprintf started > '{}'\nsleep 30\nexit 1\n",
                started.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&upload_pack).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&upload_pack, permissions).unwrap();
        git(
            repo.path(),
            vec![
                OsString::from("config"),
                OsString::from("remote.origin.uploadpack"),
                upload_pack.as_os_str().to_os_string(),
            ],
        );
        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: repo.path().with_extension("unused fetch worktree"),
            main_repo_path: repo.path().to_path_buf(),
        };
        let task = tokio::spawn(async move { try_merge(&info, "main").await });

        wait_for_mutation_marker(&started, || task.is_finished(), "delayed merge fetch").await;
        task.abort();
        let _ = task.await;
        let admission = acquire_released_mutation_lock("caller-drop rollback").await;
        run_git_with_limits_for_test(
            repo.path(),
            &["status", "--porcelain"],
            test_limits(Duration::from_secs(2)),
        )
        .await
        .expect("status after caller-drop rollback");
        drop(admission);

        assert_eq!(current_branch(repo.path()).await.as_deref(), Some("main"));
        assert_eq!(
            std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(),
            "dirty\n"
        );
        assert!(git_stdout(repo.path(), ["stash", "list"]).is_empty());
        assert!(!has_merge_conflict(repo.path()).await);
        let _ = std::fs::remove_file(started);
        let _ = std::fs::remove_file(upload_pack);
        let _ = std::fs::remove_dir_all(remote);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_merge_during_commit_rolls_back_squash_and_stash() {
        use std::os::unix::fs::PermissionsExt;

        let repo = TempRepo::new("drop during commit");
        let remote = repo.path().with_extension("drop commit bare remote");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, ["init", "--bare"]);
        git(
            repo.path(),
            vec![
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote.as_os_str().to_os_string(),
            ],
        );
        git(repo.path(), ["push", "-u", "origin", "main"]);
        let original_head = git_stdout(repo.path(), ["rev-parse", "HEAD"]);
        git(repo.path(), ["switch", "-c", "feature"]);
        std::fs::write(repo.path().join("tracked.txt"), "feature\n").unwrap();
        git(repo.path(), ["add", "tracked.txt"]);
        git(repo.path(), ["commit", "-m", "feature"]);
        git(repo.path(), ["switch", "main"]);
        std::fs::write(repo.path().join("tracked.txt"), "dirty\n").unwrap();
        let started = repo.path().join("commit-started");
        let hook = repo.path().join(".git/hooks/pre-commit");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\nprintf started > '{}'\nsleep 30\n",
                started.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();
        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: repo.path().with_extension("unused commit worktree"),
            main_repo_path: repo.path().to_path_buf(),
        };
        let task = tokio::spawn(async move { try_merge(&info, "main").await });

        for _ in 0..300 {
            if started.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(started.exists(), "delayed commit hook did not start");
        task.abort();
        let _ = task.await;
        tokio::time::timeout(
            Duration::from_secs(4),
            run_locked_git_with_limits_for_test(
                repo.path(),
                &["status", "--porcelain"],
                test_limits(Duration::from_secs(2)),
            ),
        )
        .await
        .expect("caller-drop rollback did not release repository lock")
        .expect("status after commit cancellation");

        assert_eq!(current_branch(repo.path()).await.as_deref(), Some("main"));
        assert_eq!(
            git_stdout(repo.path(), ["rev-parse", "HEAD"]),
            original_head
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(),
            "feature\n"
        );
        assert!(!git_stdout(repo.path(), ["stash", "list"]).is_empty());
        assert!(!has_merge_conflict(repo.path()).await);
        let _ = std::fs::remove_dir_all(remote);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn git_runner_bounds_unlimited_output() {
        let repo = TempRepo::new("output limit");
        git(
            repo.path(),
            ["config", "alias.spam", "!yes unbounded-git-output"],
        );
        let limits = CommandLimits {
            timeout: Duration::from_secs(2),
            stdout_bytes: 1024,
            stderr_bytes: 1024,
            combined_bytes: 1536,
        };
        let error = match run_git_with_limits_for_test(repo.path(), &["spam"], limits).await {
            Ok(_) => panic!("unlimited output must be terminated"),
            Err(error) => error,
        };

        assert!(error.contains("output limit"), "unexpected error: {error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn git_runner_supports_non_utf8_repository_paths() {
        use std::os::unix::ffi::OsStringExt;

        let mut name = b"mini-agent-8tbo-nonutf8-".to_vec();
        name.push(0xff);
        name.extend_from_slice(uuid::Uuid::new_v4().to_string().as_bytes());
        let path = std::env::temp_dir().join(std::ffi::OsString::from_vec(name));
        if let Err(error) = std::fs::create_dir_all(&path) {
            if error.kind() == std::io::ErrorKind::Unsupported
                || matches!(error.raw_os_error(), Some(1 | 22 | 92))
            {
                eprintln!("filesystem does not support this non-UTF8 fixture: {error}");
                return;
            }
            panic!("failed to create non-UTF8 fixture for an unrelated reason: {error}");
        }
        git(&path, ["init", "-b", "main"]);

        let result = run_git_with_limits_for_test(
            &path,
            &["status", "--porcelain"],
            test_limits(Duration::from_secs(2)),
        )
        .await;
        let _ = std::fs::remove_dir_all(&path);
        result.expect("non-UTF8 repository path must remain an OsStr argument");
    }

    #[tokio::test]
    async fn merge_and_non_force_cleanup_use_explicit_repository_context() {
        let repo = TempRepo::new("merge main");
        let remote = repo.path().with_extension("bare remote");
        let worktree = repo.path().with_extension("linked worktree");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, ["init", "--bare"]);
        git(
            repo.path(),
            vec![
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote.as_os_str().to_os_string(),
            ],
        );
        git(repo.path(), ["push", "-u", "origin", "main"]);
        git(
            repo.path(),
            vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("-b"),
                OsString::from("feature"),
                worktree.as_os_str().to_os_string(),
            ],
        );
        std::fs::write(worktree.join("tracked.txt"), "feature\n").unwrap();
        git(&worktree, ["add", "tracked.txt"]);
        git(&worktree, ["commit", "-m", "feature"]);
        let original_cwd = std::env::current_dir().unwrap();
        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: worktree.clone(),
            main_repo_path: repo.path().to_path_buf(),
        };

        let (mut state, outcome) = try_merge(&info, "main").await;
        assert_eq!(outcome, MergeOutcome::Success);
        complete_merge(&mut state)
            .await
            .expect("normal cleanup after squash merge");

        assert_eq!(
            std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(),
            "feature\n"
        );
        assert!(!worktree.exists(), "linked worktree was not removed");
        assert_eq!(std::env::current_dir().unwrap(), original_cwd);
        let branch = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["rev-parse", "--verify", "feature"])
            .output()
            .unwrap();
        assert!(!branch.status.success(), "feature branch was not deleted");
        let _ = std::fs::remove_dir_all(remote);
    }

    #[tokio::test]
    async fn force_cleanup_never_removes_a_worktree_that_became_dirty() {
        let repo = TempRepo::new("dirty force cleanup");
        let remote = repo.path().with_extension("dirty force cleanup remote");
        let worktree = repo.path().with_extension("dirty force cleanup worktree");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, ["init", "--bare"]);
        git(
            repo.path(),
            vec![
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote.as_os_str().to_os_string(),
            ],
        );
        git(repo.path(), ["push", "-u", "origin", "main"]);
        git(
            repo.path(),
            vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("-b"),
                OsString::from("feature"),
                worktree.as_os_str().to_os_string(),
            ],
        );
        std::fs::write(worktree.join("tracked.txt"), "feature\n").unwrap();
        git(&worktree, ["add", "tracked.txt"]);
        git(&worktree, ["commit", "-m", "feature"]);
        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: worktree.clone(),
            main_repo_path: repo.path().to_path_buf(),
        };
        let (mut state, outcome) = try_merge(&info, "main").await;
        assert_eq!(outcome, MergeOutcome::Success);
        std::fs::write(worktree.join("late-untracked.txt"), "keep me\n").unwrap();

        let error = complete_merge_force(&mut state)
            .await
            .expect_err("force cleanup must fail closed on late dirt");

        assert!(error.contains("became dirty"), "unexpected error: {error}");
        assert_eq!(
            std::fs::read_to_string(worktree.join("late-untracked.txt")).unwrap(),
            "keep me\n"
        );
        assert!(!git_stdout(repo.path(), ["rev-parse", "refs/heads/feature"]).is_empty());
        std::fs::remove_file(worktree.join("late-untracked.txt")).unwrap();
        cleanup_worktree(&worktree, "feature", repo.path(), true)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(remote);
    }

    #[tokio::test]
    async fn ignored_source_files_refuse_cleanup_and_survive_a_successful_merge() {
        let repo = TempRepo::new("ignored source cleanup");
        let remote = repo.path().with_extension("ignored source cleanup remote");
        let worktree = repo
            .path()
            .with_extension("ignored source cleanup worktree");
        std::fs::write(repo.path().join(".gitignore"), ".env\ncache/\n").unwrap();
        git(repo.path(), ["add", ".gitignore"]);
        git(repo.path(), ["commit", "-m", "ignore local source data"]);
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, ["init", "--bare"]);
        git(
            repo.path(),
            vec![
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote.as_os_str().to_os_string(),
            ],
        );
        git(repo.path(), ["push", "-u", "origin", "main"]);
        git(
            repo.path(),
            vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("-b"),
                OsString::from("feature"),
                worktree.as_os_str().to_os_string(),
            ],
        );
        std::fs::write(worktree.join("tracked.txt"), "feature\n").unwrap();
        git(&worktree, ["add", "tracked.txt"]);
        git(&worktree, ["commit", "-m", "feature"]);
        std::fs::write(worktree.join(".env"), b"SECRET=preserve\n").unwrap();
        std::fs::create_dir(worktree.join("cache")).unwrap();
        std::fs::write(worktree.join("cache/blob.bin"), b"cache\0\xff").unwrap();
        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: worktree.clone(),
            main_repo_path: repo.path().to_path_buf(),
        };

        let (mut state, outcome) = try_merge(&info, "main").await;
        assert_eq!(outcome, MergeOutcome::Success);
        let error = complete_merge(&mut state)
            .await
            .expect_err("ignored source data must refuse cleanup");

        assert!(
            error.contains("untracked or ignored files"),
            "unexpected: {error}"
        );
        assert_eq!(
            std::fs::read(worktree.join(".env")).unwrap(),
            b"SECRET=preserve\n"
        );
        assert_eq!(
            std::fs::read(worktree.join("cache/blob.bin")).unwrap(),
            b"cache\0\xff"
        );
        assert!(
            !git_stdout(repo.path(), ["rev-parse", "refs/heads/feature"]).is_empty(),
            "source branch must be retained with the refused worktree cleanup"
        );

        std::fs::remove_file(worktree.join(".env")).unwrap();
        std::fs::remove_dir_all(worktree.join("cache")).unwrap();
        cleanup_worktree(&worktree, "feature", repo.path(), true)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(remote);
    }

    #[tokio::test]
    async fn cleanup_ref_transaction_does_not_delete_source_if_target_changed() {
        let repo = TempRepo::new("atomic cleanup refs");
        git(repo.path(), ["branch", "feature"]);
        let expected_target = git_stdout(repo.path(), ["rev-parse", "refs/heads/main"]);
        let expected_source = git_stdout(repo.path(), ["rev-parse", "refs/heads/feature"]);
        std::fs::write(repo.path().join("target.txt"), "advanced\n").unwrap();
        git(repo.path(), ["add", "target.txt"]);
        git(repo.path(), ["commit", "-m", "advance target"]);

        let error = verify_target_and_delete_source_for_test(
            repo.path(),
            "refs/heads/main",
            &expected_target,
            "refs/heads/feature",
            &expected_source,
        )
        .await
        .expect_err("target verification and source deletion must be atomic");

        assert!(error.contains("verify-target-and-delete-source"));
        assert_eq!(
            git_stdout(repo.path(), ["rev-parse", "refs/heads/feature"]),
            expected_source
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_post_commit_hook_cannot_replace_the_verified_squash_commit() {
        use std::os::unix::fs::PermissionsExt;

        let repo = TempRepo::new("post commit reset");
        let remote = repo.path().with_extension("post commit reset remote");
        let worktree = repo.path().with_extension("post commit reset worktree");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, ["init", "--bare"]);
        git(
            repo.path(),
            vec![
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote.as_os_str().to_os_string(),
            ],
        );
        git(repo.path(), ["push", "-u", "origin", "main"]);
        let original = git_stdout(repo.path(), ["rev-parse", "HEAD"]);
        git(
            repo.path(),
            vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("-b"),
                OsString::from("feature"),
                worktree.as_os_str().to_os_string(),
            ],
        );
        std::fs::write(worktree.join("tracked.txt"), "feature\n").unwrap();
        git(&worktree, ["add", "tracked.txt"]);
        git(&worktree, ["commit", "-m", "feature"]);
        let hook = repo.path().join(".git/hooks/post-commit");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\ngit reset --hard {original}\nexit 0\n"),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();
        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: worktree.clone(),
            main_repo_path: repo.path().to_path_buf(),
        };

        let (_state, outcome) = try_merge(&info, "main").await;

        assert!(
            matches!(outcome, MergeOutcome::Error(error) if error.contains("verification failed"))
        );
        assert_eq!(git_stdout(repo.path(), ["rev-parse", "HEAD"]), original);
        assert!(worktree.exists());
        assert!(!git_stdout(repo.path(), ["rev-parse", "refs/heads/feature"]).is_empty());
        cleanup_worktree(&worktree, "feature", repo.path(), true)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(remote);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn post_commit_branch_switch_rolls_back_only_target_and_preserves_unrelated_ref() {
        use std::os::unix::fs::PermissionsExt;

        let repo = TempRepo::new("post commit branch switch");
        let remote = repo
            .path()
            .with_extension("post commit branch switch remote");
        let worktree = repo
            .path()
            .with_extension("post commit branch switch worktree");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, ["init", "--bare"]);
        git(
            repo.path(),
            vec![
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote.as_os_str().to_os_string(),
            ],
        );
        git(repo.path(), ["push", "-u", "origin", "main"]);
        let original = git_stdout(repo.path(), ["rev-parse", "HEAD"]);
        git(repo.path(), ["branch", "unrelated"]);
        let unrelated = git_stdout(repo.path(), ["rev-parse", "refs/heads/unrelated"]);
        git(
            repo.path(),
            vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("-b"),
                OsString::from("feature"),
                worktree.as_os_str().to_os_string(),
            ],
        );
        std::fs::write(worktree.join("tracked.txt"), "feature\n").unwrap();
        git(&worktree, ["add", "tracked.txt"]);
        git(&worktree, ["commit", "-m", "feature"]);
        let hook = repo.path().join(".git/hooks/post-commit");
        std::fs::write(&hook, "#!/bin/sh\ngit switch unrelated\nexit 0\n").unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();
        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: worktree.clone(),
            main_repo_path: repo.path().to_path_buf(),
        };

        let (_state, outcome) = try_merge(&info, "main").await;

        assert!(
            matches!(outcome, MergeOutcome::Error(error) if error.contains("target branch was not checked out"))
        );
        assert_eq!(
            git_stdout(repo.path(), ["rev-parse", "refs/heads/main"]),
            original
        );
        assert_eq!(
            git_stdout(repo.path(), ["rev-parse", "refs/heads/unrelated"]),
            unrelated
        );
        assert_eq!(current_branch(repo.path()).await.as_deref(), Some("main"));
        assert_eq!(
            std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(),
            "initial\n"
        );
        cleanup_worktree(&worktree, "feature", repo.path(), true)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(remote);
    }

    #[tokio::test]
    async fn linked_worktree_symbolic_source_ref_is_rejected_without_dereference() {
        let repo = TempRepo::new("symbolic source ref");
        let remote = repo.path().with_extension("symbolic source ref remote");
        let worktree = repo.path().with_extension("symbolic source ref worktree");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, ["init", "--bare"]);
        git(
            repo.path(),
            vec![
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote.as_os_str().to_os_string(),
            ],
        );
        git(repo.path(), ["push", "-u", "origin", "main"]);
        git(repo.path(), ["branch", "actual-feature"]);
        git(
            repo.path(),
            vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("-b"),
                OsString::from("feature"),
                worktree.as_os_str().to_os_string(),
            ],
        );
        git(
            repo.path(),
            [
                "symbolic-ref",
                "refs/heads/feature",
                "refs/heads/actual-feature",
            ],
        );
        let main = git_stdout(repo.path(), ["rev-parse", "refs/heads/main"]);
        let actual = git_stdout(repo.path(), ["rev-parse", "refs/heads/actual-feature"]);
        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: worktree.clone(),
            main_repo_path: repo.path().to_path_buf(),
        };

        let (_state, outcome) = try_merge(&info, "main").await;

        assert!(
            matches!(outcome, MergeOutcome::Error(error) if error.contains("symbolic ref refs/heads/feature"))
        );
        assert_eq!(
            git_stdout(repo.path(), ["rev-parse", "refs/heads/main"]),
            main
        );
        assert_eq!(
            git_stdout(repo.path(), ["rev-parse", "refs/heads/actual-feature"]),
            actual
        );
        assert!(worktree.exists());
        git(
            repo.path(),
            ["symbolic-ref", "--delete", "refs/heads/feature"],
        );
        cleanup_worktree(&worktree, "actual-feature", repo.path(), true)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(remote);
    }

    #[tokio::test]
    async fn symbolic_target_ref_is_rejected_before_checkout_or_pull() {
        let repo = TempRepo::new("symbolic target ref");
        git(repo.path(), ["branch", "feature"]);
        git(
            repo.path(),
            ["symbolic-ref", "refs/heads/target-alias", "refs/heads/main"],
        );
        let main = git_stdout(repo.path(), ["rev-parse", "refs/heads/main"]);
        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: repo
                .path()
                .with_extension("unused symbolic target worktree"),
            main_repo_path: repo.path().to_path_buf(),
        };

        let (_state, outcome) = try_merge(&info, "target-alias").await;

        assert!(
            matches!(outcome, MergeOutcome::Error(error) if error.contains("symbolic ref refs/heads/target-alias"))
        );
        assert_eq!(
            git_stdout(repo.path(), ["rev-parse", "refs/heads/main"]),
            main
        );
        assert_eq!(current_branch(repo.path()).await.as_deref(), Some("main"));
    }

    #[tokio::test]
    async fn already_integrated_source_is_detected_from_tree_state_without_a_commit() {
        let repo = TempRepo::new("tree no-op");
        let remote = repo.path().with_extension("tree no-op remote");
        let worktree = repo.path().with_extension("tree no-op worktree");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, ["init", "--bare"]);
        git(
            repo.path(),
            vec![
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote.as_os_str().to_os_string(),
            ],
        );
        git(repo.path(), ["push", "-u", "origin", "main"]);
        let original = git_stdout(repo.path(), ["rev-parse", "HEAD"]);
        git(
            repo.path(),
            vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("-b"),
                OsString::from("feature"),
                worktree.as_os_str().to_os_string(),
            ],
        );
        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: worktree.clone(),
            main_repo_path: repo.path().to_path_buf(),
        };

        let (mut state, outcome) = try_merge(&info, "main").await;
        assert_eq!(outcome, MergeOutcome::Success);
        assert_eq!(git_stdout(repo.path(), ["rev-parse", "HEAD"]), original);
        complete_merge(&mut state).await.unwrap();
        assert!(!worktree.exists());
        let _ = std::fs::remove_dir_all(remote);
    }

    #[tokio::test]
    async fn cleanup_retains_a_source_branch_that_changed_after_merge() {
        let repo = TempRepo::new("source ref changed");
        let remote = repo.path().with_extension("source changed bare remote");
        let worktree = repo.path().with_extension("source changed worktree");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, ["init", "--bare"]);
        git(
            repo.path(),
            vec![
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote.as_os_str().to_os_string(),
            ],
        );
        git(repo.path(), ["push", "-u", "origin", "main"]);
        git(
            repo.path(),
            vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("-b"),
                OsString::from("feature"),
                worktree.as_os_str().to_os_string(),
            ],
        );
        std::fs::write(worktree.join("tracked.txt"), "feature\n").unwrap();
        git(&worktree, ["add", "tracked.txt"]);
        git(&worktree, ["commit", "-m", "feature"]);
        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: worktree.clone(),
            main_repo_path: repo.path().to_path_buf(),
        };

        let (mut state, outcome) = try_merge(&info, "main").await;
        assert_eq!(outcome, MergeOutcome::Success);
        std::fs::write(worktree.join("later.txt"), "later\n").unwrap();
        git(&worktree, ["add", "later.txt"]);
        git(&worktree, ["commit", "-m", "later source commit"]);
        let later_oid = git_stdout(&worktree, ["rev-parse", "HEAD"]);

        let error = complete_merge_force(&mut state)
            .await
            .expect_err("changed source ref must fail closed");

        assert!(
            error.contains("source branch changed"),
            "unexpected: {error}"
        );
        assert!(worktree.exists());
        assert_eq!(
            git_stdout(repo.path(), ["rev-parse", "refs/heads/feature"]),
            later_oid
        );
        cleanup_worktree(&worktree, "feature", repo.path(), true)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(remote);
    }

    #[tokio::test]
    async fn cancelling_squash_conflict_releases_repository_and_restores_main() {
        let repo = TempRepo::new("cancel squash conflict");
        let remote = repo.path().with_extension("cancel bare remote");
        let worktree = repo.path().with_extension("cancel linked worktree");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, ["init", "--bare"]);
        git(
            repo.path(),
            vec![
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote.as_os_str().to_os_string(),
            ],
        );
        git(repo.path(), ["push", "-u", "origin", "main"]);
        git(
            repo.path(),
            vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("-b"),
                OsString::from("feature"),
                worktree.as_os_str().to_os_string(),
            ],
        );
        std::fs::write(worktree.join("tracked.txt"), "feature\n").unwrap();
        git(&worktree, ["add", "tracked.txt"]);
        git(&worktree, ["commit", "-m", "feature"]);
        std::fs::write(repo.path().join("tracked.txt"), "main\n").unwrap();
        git(repo.path(), ["add", "tracked.txt"]);
        git(repo.path(), ["commit", "-m", "main"]);
        git(repo.path(), ["push"]);
        let original_cwd = std::env::current_dir().unwrap();
        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: worktree.clone(),
            main_repo_path: repo.path().to_path_buf(),
        };

        let (mut state, outcome) = try_merge(&info, "main").await;
        assert!(matches!(outcome, MergeOutcome::Conflicts(_)));
        let error = cancel_merge(&mut state)
            .await
            .expect_err("unsafe index reset must be retained for recovery");
        assert!(error.contains("index/tree state was retained"));
        let feature_oid = git_stdout(repo.path(), ["rev-parse", "refs/heads/feature"]);
        assert!(
            !feature_oid.is_empty(),
            "source commit must remain reachable"
        );
        assert!(worktree.exists(), "cancel must retain the source worktree");
        assert_eq!(current_branch(repo.path()).await.as_deref(), Some("main"));
        let retained_conflict = std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap();
        assert!(retained_conflict.contains("<<<<<<< HEAD"));
        assert!(retained_conflict.contains("main\n"));
        assert!(retained_conflict.contains("feature\n"));
        assert!(has_merge_conflict(repo.path()).await);
        let admission = acquire_released_mutation_lock("cancelled squash merge").await;
        drop(admission);
        cleanup_worktree(&worktree, "feature", repo.path(), true)
            .await
            .expect("cleanup test worktree");
        assert_eq!(std::env::current_dir().unwrap(), original_cwd);
        assert!(!worktree.exists(), "cancelled worktree was not cleaned up");
        let _ = std::fs::remove_dir_all(remote);
    }

    #[tokio::test]
    async fn rollback_boundary_branch_switch_never_resets_the_unrelated_ref() {
        let repo = TempRepo::new("rollback boundary branch switch");
        let remote = repo.path().with_extension("rollback boundary remote");
        let worktree = repo.path().with_extension("rollback boundary worktree");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, ["init", "--bare"]);
        git(
            repo.path(),
            vec![
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote.as_os_str().to_os_string(),
            ],
        );
        git(repo.path(), ["push", "-u", "origin", "main"]);
        git(repo.path(), ["branch", "unrelated"]);
        let unrelated = git_stdout(repo.path(), ["rev-parse", "refs/heads/unrelated"]);
        git(
            repo.path(),
            vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("-b"),
                OsString::from("feature"),
                worktree.as_os_str().to_os_string(),
            ],
        );
        std::fs::write(worktree.join("tracked.txt"), "feature\n").unwrap();
        git(&worktree, ["add", "tracked.txt"]);
        git(&worktree, ["commit", "-m", "feature"]);
        std::fs::write(repo.path().join("tracked.txt"), "main\n").unwrap();
        git(repo.path(), ["add", "tracked.txt"]);
        git(repo.path(), ["commit", "-m", "main"]);
        git(repo.path(), ["push"]);
        let main = git_stdout(repo.path(), ["rev-parse", "refs/heads/main"]);
        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: worktree.clone(),
            main_repo_path: repo.path().to_path_buf(),
        };
        let (mut state, outcome) = try_merge(&info, "main").await;
        assert!(matches!(outcome, MergeOutcome::Conflicts(_)));
        let gate = TestMutationGate::new();
        state.set_rollback_test_gate(gate.clone());
        let task = tokio::spawn(async move {
            let result = cancel_merge(&mut state).await;
            (state, result)
        });
        tokio::time::timeout(Duration::from_secs(5), gate.wait_until_reached())
            .await
            .expect("target repository must reach the stash publication gate");
        git(
            repo.path(),
            ["symbolic-ref", "HEAD", "refs/heads/unrelated"],
        );
        gate.resume();
        let (_state, result) = task.await.unwrap();

        assert!(result.is_err(), "unsafe index recovery must be retained");
        assert_eq!(
            git_stdout(repo.path(), ["rev-parse", "refs/heads/unrelated"]),
            unrelated
        );
        assert_eq!(
            git_stdout(repo.path(), ["rev-parse", "refs/heads/main"]),
            main
        );
        let _ = std::fs::remove_dir_all(worktree);
        let _ = std::fs::remove_dir_all(remote);
    }

    #[tokio::test]
    async fn concurrent_stash_push_after_exact_apply_retains_both_stashes() {
        let repo = TempRepo::new("concurrent exact stash");
        std::fs::write(repo.path().join("tracked.txt"), "captured\n").unwrap();
        git(repo.path(), ["stash", "push", "-m", "captured"]);
        let captured = git_stdout(repo.path(), ["rev-parse", "refs/stash"]);
        let gate = TestMutationGate::new();
        let repo_path = repo.path().to_path_buf();
        let task = tokio::spawn({
            let gate = gate.clone();
            let captured = captured.clone();
            async move { restore_stash_with_gate_for_test(&repo_path, None, captured, gate).await }
        });
        gate.wait_until_reached().await;
        assert_eq!(
            std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(),
            "captured\n"
        );
        git(repo.path(), ["stash", "push", "-m", "concurrent"]);
        let concurrent = git_stdout(repo.path(), ["rev-parse", "refs/stash"]);
        gate.resume();

        let error = task
            .await
            .unwrap()
            .expect_err("changed stash stack must be retained");
        assert!(
            error.contains("changed concurrently"),
            "unexpected: {error}"
        );
        assert_eq!(
            git_stdout(repo.path(), ["rev-parse", "refs/stash"]),
            concurrent
        );
        git(repo.path(), ["cat-file", "-e", captured.as_str()]);
        let stash_list = git_stdout(repo.path(), ["stash", "list"]);
        assert!(stash_list.contains("concurrent"));
        assert!(stash_list.contains("captured"));
    }

    #[tokio::test]
    async fn editor_overwrite_after_exact_apply_retains_the_durable_stash() {
        let repo = TempRepo::new("post apply editor overwrite");
        std::fs::write(repo.path().join("tracked.txt"), "captured\n").unwrap();
        git(repo.path(), ["stash", "push", "-m", "captured"]);
        let captured = git_stdout(repo.path(), ["rev-parse", "refs/stash"]);
        let gate = TestMutationGate::new();
        let repo_path = repo.path().to_path_buf();
        let task = tokio::spawn({
            let gate = gate.clone();
            let captured = captured.clone();
            async move { restore_stash_with_gate_for_test(&repo_path, None, captured, gate).await }
        });
        gate.wait_until_reached().await;
        std::fs::write(repo.path().join("tracked.txt"), "editor wins\n").unwrap();
        gate.resume();

        let error = task
            .await
            .unwrap()
            .expect_err("changed content must retain stash");
        assert!(
            error.contains("workspace content changed"),
            "unexpected: {error}"
        );
        assert_eq!(
            git_stdout(repo.path(), ["rev-parse", "refs/stash"]),
            captured
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(),
            "editor wins\n"
        );
    }

    #[tokio::test]
    async fn untracked_file_after_exact_apply_retains_the_durable_stash() {
        let repo = TempRepo::new("post apply untracked file");
        std::fs::write(repo.path().join("tracked.txt"), "captured\n").unwrap();
        git(repo.path(), ["stash", "push", "-m", "captured"]);
        let captured = git_stdout(repo.path(), ["rev-parse", "refs/stash"]);
        let gate = TestMutationGate::new();
        let repo_path = repo.path().to_path_buf();
        let task = tokio::spawn({
            let gate = gate.clone();
            let captured = captured.clone();
            async move { restore_stash_with_gate_for_test(&repo_path, None, captured, gate).await }
        });
        gate.wait_until_reached().await;
        std::fs::write(repo.path().join("editor-note.txt"), b"editor bytes\0\xff").unwrap();
        gate.resume();

        let error = task
            .await
            .unwrap()
            .expect_err("untracked content must retain stash");
        assert!(
            error.contains("untracked or ignored workspace content"),
            "unexpected: {error}"
        );
        assert_eq!(
            git_stdout(repo.path(), ["rev-parse", "refs/stash"]),
            captured
        );
        assert_eq!(
            std::fs::read(repo.path().join("editor-note.txt")).unwrap(),
            b"editor bytes\0\xff"
        );
    }

    #[tokio::test]
    async fn tracked_file_replaced_by_untracked_directory_is_never_reset_or_lost() {
        let repo = TempRepo::new("untracked obstruction");
        git(repo.path(), ["branch", "feature"]);
        std::fs::remove_file(repo.path().join("tracked.txt")).unwrap();
        std::fs::create_dir(repo.path().join("tracked.txt")).unwrap();
        let preserved = repo.path().join("tracked.txt/preserve.bin");
        let bytes = b"untracked obstruction bytes\0\xff";
        std::fs::write(&preserved, bytes).unwrap();
        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: repo.path().with_extension("unused obstruction worktree"),
            main_repo_path: repo.path().to_path_buf(),
        };

        let (_state, outcome) = try_merge(&info, "main").await;

        assert!(
            matches!(outcome, MergeOutcome::Error(error) if error.contains("untracked or ignored files"))
        );
        assert_eq!(std::fs::read(&preserved).unwrap(), bytes);
        assert!(repo.path().join("tracked.txt").is_dir());
        assert!(!optional_test_ref_exists(repo.path(), "refs/stash"));
    }

    #[tokio::test]
    async fn concurrent_stash_before_exact_publication_is_never_captured_as_owned() {
        let repo = TempRepo::new("concurrent stash publication");
        let remote = repo
            .path()
            .with_extension("concurrent stash publication remote");
        let worktree = repo
            .path()
            .with_extension("concurrent stash publication worktree");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, ["init", "--bare"]);
        git(
            repo.path(),
            vec![
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote.as_os_str().to_os_string(),
            ],
        );
        git(repo.path(), ["push", "-u", "origin", "main"]);
        git(
            repo.path(),
            vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("-b"),
                OsString::from("feature"),
                worktree.as_os_str().to_os_string(),
            ],
        );
        std::fs::write(worktree.join("tracked.txt"), "feature\n").unwrap();
        git(&worktree, ["add", "tracked.txt"]);
        git(&worktree, ["commit", "-m", "feature"]);
        std::fs::write(repo.path().join("tracked.txt"), "dirty main\n").unwrap();

        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: worktree.clone(),
            main_repo_path: repo.path().to_path_buf(),
        };
        let gate = TestMutationGate::new();
        set_next_stash_publish_test_gate(repo.path(), gate.clone());

        let unrelated = TempRepo::new("unrelated stash publication");
        std::fs::write(unrelated.path().join("tracked.txt"), "unrelated dirty\n").unwrap();
        let unrelated_stash = tokio::time::timeout(
            Duration::from_secs(2),
            create_and_publish_stash_for_test(unrelated.path()),
        )
        .await
        .expect("an unrelated repository must not consume the targeted publication gate")
        .expect("unrelated stash publication should succeed");
        assert!(unrelated_stash.is_some());

        let task = tokio::spawn(async move { try_merge(&info, "main").await });
        gate.wait_until_reached().await;

        git(repo.path(), ["stash", "push", "-m", "concurrent external"]);
        let concurrent = git_stdout(repo.path(), ["rev-parse", "refs/stash"]);
        gate.resume();
        let (_state, outcome) = task.await.unwrap();

        assert!(
            matches!(outcome, MergeOutcome::Error(error) if error.contains("publish-created-stash")),
            "exact publication must fail when refs/stash changed"
        );
        assert_eq!(
            git_stdout(repo.path(), ["rev-parse", "refs/stash"]),
            concurrent
        );
        assert!(git_stdout(repo.path(), ["stash", "list"]).contains("concurrent external"));
        assert!(
            worktree.exists(),
            "failed merge must retain the source worktree"
        );
        cleanup_worktree(&worktree, "feature", repo.path(), true)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(remote);
    }

    #[tokio::test]
    async fn cancelling_conflict_restores_an_exact_detached_head_before_stash_pop() {
        let repo = TempRepo::new("detached cancellation");
        let remote = repo.path().with_extension("detached bare remote");
        let worktree = repo.path().with_extension("detached source worktree");
        std::fs::write(repo.path().join("local.txt"), "initial local\n").unwrap();
        git(repo.path(), ["add", "local.txt"]);
        git(repo.path(), ["commit", "-m", "local fixture"]);
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, ["init", "--bare"]);
        git(
            repo.path(),
            vec![
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote.as_os_str().to_os_string(),
            ],
        );
        git(repo.path(), ["push", "-u", "origin", "main"]);
        git(
            repo.path(),
            vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("-b"),
                OsString::from("feature"),
                worktree.as_os_str().to_os_string(),
            ],
        );
        std::fs::write(worktree.join("tracked.txt"), "feature\n").unwrap();
        git(&worktree, ["add", "tracked.txt"]);
        git(&worktree, ["commit", "-m", "feature"]);
        std::fs::write(repo.path().join("tracked.txt"), "main\n").unwrap();
        git(repo.path(), ["add", "tracked.txt"]);
        git(repo.path(), ["commit", "-m", "main"]);
        git(repo.path(), ["push"]);
        let detached_oid = git_stdout(repo.path(), ["rev-parse", "HEAD"]);
        git(repo.path(), ["switch", "--detach", detached_oid.as_str()]);
        std::fs::write(repo.path().join("local.txt"), "restore me\n").unwrap();
        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: worktree.clone(),
            main_repo_path: repo.path().to_path_buf(),
        };

        let (mut state, outcome) = try_merge(&info, "main").await;
        assert!(matches!(outcome, MergeOutcome::Conflicts(_)));
        let error = cancel_merge(&mut state)
            .await
            .expect_err("unsafe detached recovery must retain conflict state");
        assert!(error.contains("index/tree state was retained"));

        assert_eq!(current_branch(repo.path()).await.as_deref(), Some("main"));
        assert_eq!(
            git_stdout(repo.path(), ["rev-parse", "refs/heads/main"]),
            detached_oid
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("local.txt")).unwrap(),
            "initial local\n"
        );
        assert!(!git_stdout(repo.path(), ["stash", "list"]).is_empty());
        assert!(worktree.exists(), "cancel must retain the source worktree");
        cleanup_worktree(&worktree, "feature", repo.path(), true)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(remote);
    }

    #[tokio::test]
    async fn failed_merge_restores_stash_and_branch_without_changing_cwd() {
        let repo = TempRepo::new("failed merge");
        let remote = repo.path().with_extension("failed bare remote");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, ["init", "--bare"]);
        git(
            repo.path(),
            vec![
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote.as_os_str().to_os_string(),
            ],
        );
        git(repo.path(), ["push", "-u", "origin", "main"]);
        std::fs::write(repo.path().join("tracked.txt"), "dirty\n").unwrap();
        let original_cwd = std::env::current_dir().unwrap();
        let info = WorktreeInfo {
            branch: "missing-feature".into(),
            worktree_path: repo.path().with_extension("missing worktree"),
            main_repo_path: repo.path().to_path_buf(),
        };

        let (state, outcome) = try_merge(&info, "missing-target").await;

        assert!(matches!(outcome, MergeOutcome::Error(_)));
        assert_eq!(state.original_branch, "main");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(),
            "dirty\n"
        );
        assert_eq!(current_branch(repo.path()).await.as_deref(), Some("main"));
        assert_eq!(std::env::current_dir().unwrap(), original_cwd);
        let _ = std::fs::remove_dir_all(remote);
    }

    #[tokio::test]
    async fn conflicting_pull_is_rolled_back_before_returning_error() {
        let repo = TempRepo::new("pull conflict");
        let remote = repo.path().with_extension("pull conflict bare remote");
        let peer = repo.path().with_extension("pull conflict peer");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, ["init", "--bare"]);
        git(
            repo.path(),
            vec![
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote.as_os_str().to_os_string(),
            ],
        );
        git(repo.path(), ["push", "-u", "origin", "main"]);
        git(repo.path(), ["branch", "feature"]);
        git(
            repo.path(),
            vec![
                OsString::from("clone"),
                OsString::from("-b"),
                OsString::from("main"),
                remote.as_os_str().to_os_string(),
                peer.as_os_str().to_os_string(),
            ],
        );
        git(&peer, ["config", "user.email", "peer@example.invalid"]);
        git(&peer, ["config", "user.name", "Peer Test"]);
        std::fs::write(peer.join("tracked.txt"), "remote\n").unwrap();
        git(&peer, ["add", "tracked.txt"]);
        git(&peer, ["commit", "-m", "remote"]);
        git(&peer, ["push"]);
        std::fs::write(repo.path().join("tracked.txt"), "local\n").unwrap();
        git(repo.path(), ["add", "tracked.txt"]);
        git(repo.path(), ["commit", "-m", "local"]);
        let pre_pull_oid = git_stdout(repo.path(), ["rev-parse", "HEAD"]);
        let original_cwd = std::env::current_dir().unwrap();
        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: repo.path().with_extension("unused pull worktree"),
            main_repo_path: repo.path().to_path_buf(),
        };

        let (_state, outcome) = try_merge(&info, "main").await;

        assert!(matches!(outcome, MergeOutcome::Error(error) if error.contains("pull failed")));
        assert_eq!(current_branch(repo.path()).await.as_deref(), Some("main"));
        assert_eq!(git_stdout(repo.path(), ["rev-parse", "HEAD"]), pre_pull_oid);
        let retained_conflict = std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap();
        assert!(retained_conflict.contains("<<<<<<< HEAD"));
        assert!(retained_conflict.contains("local\n"));
        assert!(retained_conflict.contains("remote\n"));
        assert!(has_merge_conflict(repo.path()).await);
        assert_eq!(std::env::current_dir().unwrap(), original_cwd);
        let _ = std::fs::remove_dir_all(peer);
        let _ = std::fs::remove_dir_all(remote);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn post_pull_head_read_failure_rolls_target_back_to_its_exact_pre_pull_oid() {
        use std::os::unix::fs::PermissionsExt;

        let repo = TempRepo::new("post pull head failure");
        let remote = repo.path().with_extension("post pull head failure remote");
        let peer = repo.path().with_extension("post pull head failure peer");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, ["init", "--bare"]);
        git(
            repo.path(),
            vec![
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote.as_os_str().to_os_string(),
            ],
        );
        git(repo.path(), ["push", "-u", "origin", "main"]);
        git(repo.path(), ["branch", "feature"]);
        let pre_pull = git_stdout(repo.path(), ["rev-parse", "HEAD"]);
        git(
            repo.path(),
            vec![
                OsString::from("clone"),
                OsString::from("-b"),
                OsString::from("main"),
                remote.as_os_str().to_os_string(),
                peer.as_os_str().to_os_string(),
            ],
        );
        git(&peer, ["config", "user.email", "peer@example.invalid"]);
        git(&peer, ["config", "user.name", "Peer Test"]);
        std::fs::write(peer.join("remote.txt"), "remote\n").unwrap();
        git(&peer, ["add", "remote.txt"]);
        git(&peer, ["commit", "-m", "advance remote"]);
        git(&peer, ["push"]);
        let hook = repo.path().join(".git/hooks/post-merge");
        std::fs::write(
            &hook,
            "#!/bin/sh\ngit update-ref -d refs/heads/main\nexit 0\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();
        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: repo.path().with_extension("unused post-pull worktree"),
            main_repo_path: repo.path().to_path_buf(),
        };

        let (_state, outcome) = try_merge(&info, "main").await;

        let MergeOutcome::Error(error) = outcome else {
            panic!("unexpected outcome: {outcome:?}");
        };
        assert!(error.contains("required direct ref does not exist"));
        assert_eq!(current_branch(repo.path()).await.as_deref(), Some("main"));
        assert_eq!(git_stdout(repo.path(), ["rev-parse", "HEAD"]), pre_pull);
        assert!(repo.path().join("remote.txt").exists());
        let _ = std::fs::remove_dir_all(peer);
        let _ = std::fs::remove_dir_all(remote);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn post_pull_hook_branch_switch_is_detected_and_only_target_is_rolled_back() {
        use std::os::unix::fs::PermissionsExt;

        let repo = TempRepo::new("post pull branch switch");
        let remote = repo.path().with_extension("post pull branch switch remote");
        let peer = repo.path().with_extension("post pull branch switch peer");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, ["init", "--bare"]);
        git(
            repo.path(),
            vec![
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote.as_os_str().to_os_string(),
            ],
        );
        git(repo.path(), ["push", "-u", "origin", "main"]);
        git(repo.path(), ["branch", "feature"]);
        git(repo.path(), ["branch", "hook-other"]);
        let pre_pull = git_stdout(repo.path(), ["rev-parse", "refs/heads/main"]);
        let unrelated = git_stdout(repo.path(), ["rev-parse", "refs/heads/hook-other"]);
        git(
            repo.path(),
            vec![
                OsString::from("clone"),
                OsString::from("-b"),
                OsString::from("main"),
                remote.as_os_str().to_os_string(),
                peer.as_os_str().to_os_string(),
            ],
        );
        git(&peer, ["config", "user.email", "peer@example.invalid"]);
        git(&peer, ["config", "user.name", "Peer Test"]);
        std::fs::write(peer.join("remote.txt"), "remote\n").unwrap();
        git(&peer, ["add", "remote.txt"]);
        git(&peer, ["commit", "-m", "advance remote"]);
        git(&peer, ["push"]);
        let hook = repo.path().join(".git/hooks/post-merge");
        std::fs::write(&hook, "#!/bin/sh\ngit switch hook-other\nexit 0\n").unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();
        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: repo
                .path()
                .with_extension("unused post-pull switch worktree"),
            main_repo_path: repo.path().to_path_buf(),
        };

        let (_state, outcome) = try_merge(&info, "main").await;

        assert!(
            matches!(outcome, MergeOutcome::Error(error) if error.contains("target branch was not checked out"))
        );
        assert_eq!(
            git_stdout(repo.path(), ["rev-parse", "refs/heads/main"]),
            pre_pull
        );
        assert_eq!(
            git_stdout(repo.path(), ["rev-parse", "refs/heads/hook-other"]),
            unrelated
        );
        assert_eq!(current_branch(repo.path()).await.as_deref(), Some("main"));
        assert!(!repo.path().join("remote.txt").exists());
        let _ = std::fs::remove_dir_all(peer);
        let _ = std::fs::remove_dir_all(remote);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_post_checkout_hook_restores_and_verifies_branch_before_stash_pop() {
        use std::os::unix::fs::PermissionsExt;

        let repo = TempRepo::new("failed checkout hook");
        let remote = repo.path().with_extension("failed checkout bare remote");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, ["init", "--bare"]);
        git(
            repo.path(),
            vec![
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote.as_os_str().to_os_string(),
            ],
        );
        git(repo.path(), ["push", "-u", "origin", "main"]);
        git(repo.path(), ["branch", "feature"]);
        git(repo.path(), ["branch", "target"]);
        std::fs::write(repo.path().join("tracked.txt"), "dirty\n").unwrap();
        let hook = repo.path().join(".git/hooks/post-checkout");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();
        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: repo
                .path()
                .with_extension("unused failed checkout worktree"),
            main_repo_path: repo.path().to_path_buf(),
        };

        let (_state, outcome) = try_merge(&info, "target").await;

        assert!(
            matches!(outcome, MergeOutcome::Error(error) if error.contains("checkout failed") && error.contains("rollback failed"))
        );
        assert_eq!(current_branch(repo.path()).await.as_deref(), Some("main"));
        assert_eq!(
            std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(),
            "initial\n"
        );
        assert!(!git_stdout(repo.path(), ["stash", "list"]).is_empty());
        let _ = std::fs::remove_dir_all(remote);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timed_out_post_checkout_hook_restores_branch_before_stash_pop() {
        use std::os::unix::fs::PermissionsExt;

        let repo = TempRepo::new("timed checkout hook");
        let remote = repo.path().with_extension("timed checkout bare remote");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, ["init", "--bare"]);
        git(
            repo.path(),
            vec![
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote.as_os_str().to_os_string(),
            ],
        );
        git(repo.path(), ["push", "-u", "origin", "main"]);
        git(repo.path(), ["branch", "feature"]);
        git(repo.path(), ["branch", "target"]);
        std::fs::write(repo.path().join("tracked.txt"), "dirty\n").unwrap();
        let hook = repo.path().join(".git/hooks/post-checkout");
        std::fs::write(&hook, "#!/bin/sh\nsleep 1\n").unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();
        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: repo.path().with_extension("unused timed checkout worktree"),
            main_repo_path: repo.path().to_path_buf(),
        };

        let (_state, outcome) = try_merge_with_switch_limits_for_test(
            &info,
            "target",
            test_limits(Duration::from_millis(100)),
        )
        .await;

        assert!(
            matches!(outcome, MergeOutcome::Error(error) if error.contains("checkout failed") && error.contains("timed out"))
        );
        assert_eq!(current_branch(repo.path()).await.as_deref(), Some("main"));
        assert_eq!(
            std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(),
            "dirty\n"
        );
        assert!(git_stdout(repo.path(), ["stash", "list"]).is_empty());
        let _ = std::fs::remove_dir_all(remote);
    }

    #[test]
    fn active_workspace_cleanup_child() {
        let (Ok(main_path), Ok(worktree_path)) = (
            std::env::var("MINI_AGENT_ACTIVE_CWD_MAIN"),
            std::env::var("MINI_AGENT_ACTIVE_CWD_WORKTREE"),
        ) else {
            return;
        };
        std::env::set_current_dir(&worktree_path).expect("enter child worktree");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(cleanup_worktree(
            Path::new(&worktree_path),
            "feature",
            Path::new(&main_path),
            true,
        ));
        assert!(result.is_err());
        assert!(Path::new(&worktree_path).exists());
        assert_eq!(
            std::env::current_dir().unwrap(),
            Path::new(&worktree_path).canonicalize().unwrap()
        );
    }

    #[tokio::test]
    async fn cleanup_refuses_to_delete_the_process_active_workspace() {
        let repo = TempRepo::new("active cwd cleanup");
        let worktree = repo.path().with_extension("active cwd linked worktree");
        git(
            repo.path(),
            vec![
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("-b"),
                OsString::from("feature"),
                worktree.as_os_str().to_os_string(),
            ],
        );
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::worktree_tests::tests::active_workspace_cleanup_child",
                "--nocapture",
            ])
            .env("MINI_AGENT_ACTIVE_CWD_MAIN", repo.path())
            .env("MINI_AGENT_ACTIVE_CWD_WORKTREE", &worktree)
            .output()
            .expect("run active-workspace cleanup child");
        assert!(
            output.status.success(),
            "active-workspace child failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(worktree.exists(), "active worktree was deleted");

        cleanup_worktree(&worktree, "feature", repo.path(), true)
            .await
            .unwrap();
        assert!(!worktree.exists(), "worktree cleanup did not resume safely");
    }
}
