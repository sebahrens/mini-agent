#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;

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

    struct OwnedDirectory(PathBuf);

    impl OwnedDirectory {
        fn create(path: PathBuf) -> Self {
            // Exclusive creation: a failed setup must not delete a path that
            // was already present before this fixture acquired ownership.
            std::fs::create_dir(&path).expect("create fixture directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for OwnedDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct TempRepo(OwnedDirectory);

    impl TempRepo {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("mini-agent-8tbo-{label}-{}", uuid::Uuid::new_v4()));
            Self::initialize(OwnedDirectory::create(path))
        }

        fn initialize(directory: OwnedDirectory) -> Self {
            let repo = Self(directory);
            let path = repo.path();
            git(path, ["init", "-b", "main"]);
            git(path, ["config", "user.email", "mini-agent@example.invalid"]);
            git(path, ["config", "user.name", "Mini Agent Test"]);
            std::fs::write(path.join("tracked.txt"), "initial\n").expect("write tracked fixture");
            git(path, ["add", "tracked.txt"]);
            git(path, ["commit", "-m", "initial"]);
            repo
        }

        fn path(&self) -> &Path {
            self.0.path()
        }
    }

    #[test]
    fn temporary_repository_owns_failed_git_initialization() {
        let path =
            std::env::temp_dir().join(format!("mini-agent-failed-init-{}", uuid::Uuid::new_v4()));
        let directory = OwnedDirectory::create(path.clone());
        // A malformed Git file makes the real git init fail after directory
        // creation, without changing process environment or global Git config.
        std::fs::write(path.join(".git"), "invalid gitfile\n").unwrap();
        let result = std::panic::catch_unwind(|| TempRepo::initialize(directory));
        let remained = path.exists();
        // Rescue only after taking the acceptance snapshot, including mutants
        // that deliberately discard the directory owner.
        let _ = std::fs::remove_dir_all(&path);
        let panic = result
            .err()
            .expect("malformed Git file must fail initialization");
        assert!(
            panic
                .downcast_ref::<String>()
                .is_some_and(|text| text.starts_with("fixture git failed:"))
        );
        assert!(
            !remained,
            "failed Git initialization leaked its fixture directory"
        );
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

    async fn acquire_released_mutation_lock(
        repo_path: &Path,
        label: &str,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        tokio::time::timeout(
            TEST_MUTATION_ADMISSION_TIMEOUT,
            crate::git::runner::GitRunner::default().acquire_mutation(repo_path),
        )
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{label} did not release the process Git mutation lock within {:?}",
                TEST_MUTATION_ADMISSION_TIMEOUT
            )
        })
        .unwrap_or_else(|error| panic!("{label} could not resolve repository identity: {error}"))
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
            let bang_cwd = sandbox
                .run_explicit_shell("!pwd", crate::sandbox::DEFAULT_COMMAND_LIMITS, None)
                .await
                .unwrap();
            assert!(bang_cwd.succeeded());
            assert_eq!(
                bang_cwd.rendered_output().trim(),
                canonical_worktree.display().to_string()
            );
            assert_eq!(bang_cwd.audit.cwd, canonical_worktree);

            // Exercise the runners used by lazygit's probe and interactive launch.
            // A command's stale cwd must not override the rebound workspace.
            let mut probe = tokio::process::Command::new("sh");
            probe.args(["-c", "pwd"]).current_dir(repo.path());
            let probe_cwd = sandbox
                .output_support_command(probe, crate::sandbox::DEFAULT_COMMAND_LIMITS)
                .await
                .unwrap();
            assert_eq!(probe_cwd.status, crate::sandbox::CommandStatus::Completed);
            assert!(probe_cwd.exit_status.unwrap().success());
            assert_eq!(
                String::from_utf8_lossy(&probe_cwd.stdout).trim(),
                canonical_worktree.display().to_string()
            );

            let mut utility = tokio::process::Command::new("sh");
            utility
                .args(["-c", "pwd > support-cwd.txt"])
                .current_dir(repo.path());
            let utility_status = sandbox
                .status_support_command(
                    utility,
                    crate::sandbox::SupportCommandLimits {
                        timeout: std::time::Duration::from_secs(5),
                    },
                    crate::sandbox::SupportCommandAudit::new(
                        "worktree-test",
                        "user-trusted-bypass",
                    ),
                )
                .await
                .unwrap();
            assert_eq!(
                utility_status.status,
                crate::sandbox::CommandStatus::Completed
            );
            assert!(utility_status.exit_status.unwrap().success());
            assert_eq!(
                std::fs::read_to_string(worktree.join("support-cwd.txt"))
                    .unwrap()
                    .trim(),
                canonical_worktree.display().to_string()
            );
            assert!(!repo.path().join("support-cwd.txt").exists());
            std::fs::remove_file(worktree.join("support-cwd.txt")).unwrap();
        }
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
        let mut checker = permission
            .as_ref()
            .unwrap()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(matches!(
            checker.check_path("write", &original_plan.to_string_lossy()),
            crate::permission::checker::CheckResult::Allowed,
        ));
        assert!(matches!(
            checker.check_path("write", &replacement_plan.to_string_lossy()),
            crate::permission::checker::CheckResult::Denied(_),
        ));
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
        assert!(!cli.contains("wt-force"));
        assert!(!app.contains("wt-force"));
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
    }

    #[test]
    fn test_wt_cli_flags_enabled() {
        let cli = Cli {
            worktree: Some("feature-x".into()),
            wt_auto_merge: true,
            wt_base_dir: Some("/tmp".into()),
            ..Default::default()
        };
        assert_eq!(cli.worktree.as_deref(), Some("feature-x"));
        assert!(cli.wt_auto_merge);
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
    mod concurrency {
        use super::*;
        use std::io::Write;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::{Arc, Condvar, Mutex};

        const FIXTURE_GUARD: Duration = Duration::from_secs(15);
        type GitTask = tokio::task::JoinHandle<Result<(), String>>;

        struct ReleaseState {
            writer: std::fs::File,
            released: bool,
            rescued: bool,
            write_failed: bool,
        }

        impl ReleaseState {
            fn release(&mut self) {
                if !self.released {
                    self.write_failed = self.writer.write_all(b"release\n").is_err();
                    self.released = true;
                }
            }
        }

        pub(super) struct CommandGate {
            pub(super) script: PathBuf,
            ready: PathBuf,
            finished: PathBuf,
            release: Arc<(Mutex<ReleaseState>, Condvar)>,
            rescuer: Option<std::thread::JoinHandle<()>>,
        }

        pub(super) fn quote(path: &Path) -> String {
            format!("'{}'", path.to_str().unwrap().replace('\'', "'\"'\"'"))
        }

        impl CommandGate {
            pub(super) fn new(repo: &Path, name: &str) -> Self {
                let dir = repo.join(".git").join(name);
                std::fs::create_dir(&dir).unwrap();
                let fifo = dir.join("release");
                let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
                // SAFETY: c_path is a valid, NUL-terminated path owned for this call.
                assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
                // Keep both ends open so the shell can announce readiness before
                // blocking in read, and cleanup can always write without a reader.
                let writer = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&fifo)
                    .unwrap();
                let script = dir.join("command");
                let ready = dir.join("ready");
                let finished = dir.join("finished");
                std::fs::write(&script, format!(
                    "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$PWD\" > {}\nIFS= read -r reply < {}\nprintf finished > {}\n",
                    quote(&ready), quote(&fifo), quote(&finished),
                )).unwrap();
                std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
                let release = Arc::new((
                    Mutex::new(ReleaseState {
                        writer,
                        released: false,
                        rescued: false,
                        write_failed: false,
                    }),
                    Condvar::new(),
                ));
                let rescue_release = release.clone();
                let rescuer = std::thread::spawn(move || {
                    let (state, wake) = &*rescue_release;
                    let (mut state, _) = wake
                        .wait_timeout_while(state.lock().unwrap(), FIXTURE_GUARD, |state| {
                            !state.released
                        })
                        .unwrap();
                    if !state.released {
                        state.rescued = true;
                        state.release();
                    }
                });
                Self {
                    script,
                    ready,
                    finished,
                    release,
                    rescuer: Some(rescuer),
                }
            }

            pub(super) fn release(&self) {
                let (state, wake) = &*self.release;
                state.lock().unwrap().release();
                wake.notify_all();
            }

            pub(super) fn settle(&mut self) -> Result<(), &'static str> {
                self.release();
                let joined = self.rescuer.take().map(|rescuer| rescuer.join());
                if joined.is_some_and(|result| result.is_err()) {
                    return Err("command rescue thread panicked");
                }
                let state = self.release.0.lock().unwrap();
                if state.rescued {
                    return Err("command needed fixture rescue");
                }
                if state.write_failed {
                    return Err("command gate release failed");
                }
                Ok(())
            }

            pub(super) async fn wait_started(&self, task: &GitTask, expected_cwd: &Path) {
                self.wait_ready(|| task.is_finished(), expected_cwd).await;
            }

            pub(super) async fn wait_ready(&self, stopped: impl Fn() -> bool, expected_cwd: &Path) {
                let cwd = tokio::time::timeout(FIXTURE_GUARD, async {
                    loop {
                        // Opening the marker precedes printf's write. Wait for
                        // its complete line instead of treating existence as readiness.
                        if let Ok(cwd) = std::fs::read_to_string(&self.ready)
                            && cwd.ends_with('\n')
                        {
                            break cwd;
                        }
                        assert!(!stopped(), "Git command stopped before readiness");
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .expect("Git command did not announce readiness");
                assert_eq!(cwd.strip_suffix('\n'), expected_cwd.to_str());
                assert!(!stopped(), "held Git command already finished");
                assert!(
                    !self.finished.exists(),
                    "Git command bypassed its release gate"
                );
            }
        }

        impl Drop for CommandGate {
            fn drop(&mut self) {
                self.release();
                if let Some(rescuer) = self.rescuer.take() {
                    let joined = rescuer.join();
                    if !std::thread::panicking() {
                        assert!(joined.is_ok(), "command rescue thread panicked");
                    }
                }
            }
        }

        struct Fixture {
            gates: Vec<CommandGate>,
            tasks: Vec<Option<GitTask>>,
        }

        impl Fixture {
            fn start(
                &mut self,
                future: impl std::future::Future<Output = Result<(), String>> + Send + 'static,
                fail: bool,
            ) {
                self.tasks.push(Some(tokio::spawn(async move {
                    let result = future.await;
                    if fail {
                        panic!("injected Git caller failure");
                    }
                    result
                })));
            }

            fn start_alias(
                &mut self,
                repo: &Path,
                gate: usize,
                fail: bool,
            ) -> tokio::sync::mpsc::UnboundedReceiver<(PathBuf, bool)> {
                let path = repo.to_path_buf();
                let alias = format!("held-{gate}");
                let (observer, events) = tokio::sync::mpsc::unbounded_channel();
                self.start(
                    async move {
                        crate::git::runner::MUTATION_LOCK_OBSERVER
                            .scope(observer, async {
                                run_locked_git_with_limits_for_test(
                                    &path,
                                    &[&alias],
                                    test_limits(Duration::from_secs(60)),
                                )
                                .await
                                .map(|_| ())
                            })
                            .await
                    },
                    fail,
                );
                events
            }

            async fn settle(&mut self) {
                for gate in &self.gates {
                    gate.release();
                }
                let mut results = Vec::new();
                for task in &mut self.tasks {
                    if let Some(task) = task.take() {
                        results.push(task.await);
                    }
                }
                let settled: Vec<_> = self.gates.iter_mut().map(CommandGate::settle).collect();
                // Validate only after every command and rescue thread is joined.
                for result in results {
                    result.unwrap().unwrap();
                }
                for result in settled {
                    result.unwrap();
                }
            }
        }

        #[derive(Clone, Copy)]
        enum Case {
            Independent,
            SameRepository,
            LinkedWorktree,
            Hook,
        }

        #[derive(Clone, Copy)]
        enum Failure {
            None,
            Controller,
            Caller,
        }

        fn run_case(case: Case, failure: Failure) {
            let repo = TempRepo::new("held first 'repo'");
            let second =
                matches!(case, Case::Independent).then(|| TempRepo::new("held second repo"));
            let linked = repo.path().join("linked checkout");
            if matches!(case, Case::LinkedWorktree) {
                git(
                    repo.path(),
                    [
                        OsStr::new("worktree"),
                        OsStr::new("add"),
                        OsStr::new("-b"),
                        OsStr::new("held-linked"),
                        linked.as_os_str(),
                    ],
                );
            }
            let second_path = match case {
                Case::Independent => second.as_ref().unwrap().path(),
                Case::LinkedWorktree => &linked,
                _ => repo.path(),
            };
            let count = if matches!(case, Case::Hook) { 1 } else { 2 };
            let gates: Vec<_> = (0..count)
                .map(|i| CommandGate::new(repo.path(), &format!("held-gate-{i}")))
                .collect();
            if matches!(case, Case::Hook) {
                std::fs::write(repo.path().join("tracked.txt"), "changed\n").unwrap();
                std::fs::copy(&gates[0].script, repo.path().join(".git/hooks/pre-commit")).unwrap();
            } else {
                for (i, (path, gate)) in [repo.path(), second_path]
                    .into_iter()
                    .zip(&gates)
                    .enumerate()
                {
                    git(
                        path,
                        [
                            OsStr::new("config"),
                            OsStr::new(&format!("alias.held-{i}")),
                            OsStr::new(&format!("!{}", quote(&gate.script))),
                        ],
                    );
                }
            }
            let original_cwd = std::env::current_dir().unwrap();
            let original_manifest = std::fs::read_to_string("Cargo.toml").unwrap();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let mut fixture = Fixture {
                gates,
                tasks: Vec::new(),
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                runtime.block_on(async {
                    tokio::time::timeout(FIXTURE_GUARD, async {
                        if matches!(case, Case::Hook) {
                            let path = repo.path().to_path_buf();
                            fixture.start(
                                async move { worktree_auto_commit_all(&path).await.map(|_| ()) },
                                matches!(failure, Failure::Caller),
                            );
                            fixture.gates[0]
                                .wait_started(
                                    fixture.tasks[0].as_ref().unwrap(),
                                    &repo.path().canonicalize().unwrap(),
                                )
                                .await;
                        } else {
                            let mut first_events = fixture.start_alias(
                                repo.path(),
                                0,
                                matches!(failure, Failure::Caller),
                            );
                            let (first_key, ready) = first_events.recv().await.unwrap();
                            assert!(ready, "first repository lock was unexpectedly busy");
                            fixture.gates[0]
                                .wait_started(
                                    fixture.tasks[0].as_ref().unwrap(),
                                    &repo.path().canonicalize().unwrap(),
                                )
                                .await;
                            let mut second_events = fixture.start_alias(second_path, 1, false);
                            let (second_key, ready) = second_events.recv().await.unwrap();
                            if matches!(case, Case::Independent) {
                                assert_ne!(first_key, second_key);
                                assert!(ready, "independent repositories shared admission");
                                fixture.gates[1]
                                    .wait_started(
                                        fixture.tasks[1].as_ref().unwrap(),
                                        &second_path.canonicalize().unwrap(),
                                    )
                                    .await;
                            } else {
                                assert_eq!(
                                    first_key, second_key,
                                    "linked worktrees must resolve one common directory"
                                );
                                assert!(
                                    !ready,
                                    "second caller acquired an already-held repository lock"
                                );
                                assert!(
                                    !fixture.gates[1].ready.exists(),
                                    "second command bypassed repository admission"
                                );
                            }
                        }
                        // This controller runs on the same single-threaded runtime
                        // while a real Git alias or pre-commit hook remains held.
                        assert_eq!(std::env::current_dir().unwrap(), original_cwd);
                        assert_eq!(
                            std::fs::read_to_string("Cargo.toml").unwrap(),
                            original_manifest
                        );
                        if matches!(failure, Failure::Controller) {
                            panic!("injected with Git work held");
                        }
                        fixture.gates[0].release();
                        let first_result = fixture.tasks[0].as_mut().unwrap().await;
                        fixture.tasks[0].take();
                        if let Err(error) = first_result {
                            std::panic::resume_unwind(error.into_panic());
                        }
                        first_result.unwrap().unwrap();
                        if count == 2 {
                            fixture.gates[1]
                                .wait_started(
                                    fixture.tasks[1].as_ref().unwrap(),
                                    &second_path.canonicalize().unwrap(),
                                )
                                .await;
                            fixture.gates[1].release();
                        }
                    })
                    .await
                    .expect("worktree scenario exceeded hang guard");
                });
            }));
            runtime.block_on(fixture.settle());
            assert!(fixture.tasks.iter().all(Option::is_none));
            for gate in &fixture.gates {
                assert!(
                    gate.finished.exists(),
                    "command was not released to completion"
                );
            }
            if matches!(case, Case::Hook) {
                assert_eq!(
                    git_stdout(repo.path(), ["show", "HEAD:tracked.txt"]),
                    "changed"
                );
            }
            assert_eq!(std::env::current_dir().unwrap(), original_cwd);
            assert_eq!(
                std::fs::read_to_string("Cargo.toml").unwrap(),
                original_manifest
            );
            if let Err(panic) = result {
                std::panic::resume_unwind(panic);
            }
            assert!(
                matches!(failure, Failure::None),
                "injected worktree fault was not reached"
            );
        }

        #[test]
        fn worktree_concurrency_independent_repositories_preserve_cwd() {
            run_case(Case::Independent, Failure::None);
        }

        #[test]
        fn worktree_concurrency_shared_repository_lock() {
            for case in [Case::SameRepository, Case::LinkedWorktree] {
                run_case(case, Failure::None);
            }
        }

        #[test]
        fn worktree_concurrency_hook_preserves_runtime_and_cwd() {
            run_case(Case::Hook, Failure::None);
        }

        #[test]
        fn worktree_concurrency_failure_releases_commands_and_joins_tasks() {
            for (failure, expected) in [
                (Failure::Controller, "injected with Git work held"),
                (Failure::Caller, "injected Git caller failure"),
            ] {
                for case in [Case::Independent, Case::LinkedWorktree, Case::Hook] {
                    let panic = std::panic::catch_unwind(|| run_case(case, failure))
                        .expect_err("fault must propagate after cleanup");
                    assert_eq!(panic.downcast_ref::<&str>(), Some(&expected));
                }
            }
        }
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
        run_create_rollback_case(CreateStop::ReservationTimeout).await;
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
        run_create_rollback_case(CreateStop::CheckoutTimeout).await;
    }

    #[cfg(unix)]
    const CREATE_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

    #[cfg(unix)]
    #[derive(Clone, Copy)]
    enum CreateStop {
        CallerDrop,
        CheckoutTimeout,
        ReservationTimeout,
    }

    #[cfg(unix)]
    #[derive(Clone, Copy)]
    enum CreateFixtureFailure {
        None,
        BeforeReadiness,
        AfterReadiness,
        DuringRollback,
        Rollback,
    }

    #[cfg(unix)]
    async fn exercise_create_rollback(
        stop: CreateStop,
        failure: CreateFixtureFailure,
        base: &Path,
        cleanup_ok: &mut bool,
    ) {
        use futures::FutureExt;

        struct WorktreeBase<'a>(&'a Path);
        impl Drop for WorktreeBase<'_> {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(self.0);
            }
        }

        let repo = TempRepo::new("held create 'rollback'");
        // Own the sibling directory before setup can fail, through the final
        // rollback rendezvous. TempRepo owns the hook and FIFO separately.
        let _base = WorktreeBase(base);
        std::fs::create_dir_all(base).unwrap();
        let target = base.canonicalize().unwrap().join("create-held");
        let mut rollback_gate = concurrency::CommandGate::new(repo.path(), "rollback-gate");
        let mut gate = concurrency::CommandGate::new(repo.path(), "create-gate");
        let hook = repo
            .path()
            .join(if matches!(stop, CreateStop::ReservationTimeout) {
                ".git/hooks/reference-transaction"
            } else {
                ".git/hooks/post-checkout"
            });
        std::fs::copy(&gate.script, &hook).unwrap();
        if matches!(stop, CreateStop::ReservationTimeout) {
            // Cleanup may run before this hook has been replaced by the
            // rollback gate. Never hold a second ref transaction on this FIFO.
            let once = concurrency::quote(&repo.path().join(".git/reservation-held"));
            std::fs::write(&hook, format!(
                "#!/bin/sh\nif [ \"$1\" = committed ] && [ ! -e {once} ]; then\n: > {once}\n. {}\nfi\nexit 0\n",
                concurrency::quote(&gate.script),
            )).unwrap();
        }
        let expected_oid = git_stdout(repo.path(), ["rev-parse", "HEAD"]);
        let repo_path = repo.path().to_path_buf();
        let base_path = base.to_path_buf();
        let mut task = Some(tokio::spawn(async move {
            match stop {
                CreateStop::CallerDrop => create(&repo_path, "create-held", Some(&base_path)).await,
                CreateStop::CheckoutTimeout => {
                    create_with_limits_for_test(
                        &repo_path,
                        "create-held",
                        Some(&base_path),
                        test_limits(CREATE_COMMAND_TIMEOUT),
                    )
                    .await
                }
                CreateStop::ReservationTimeout => {
                    create_with_ref_limits_for_test(
                        &repo_path,
                        "create-held",
                        Some(&base_path),
                        test_limits(CREATE_COMMAND_TIMEOUT),
                    )
                    .await
                }
            }
            .map(|_| ())
        }));

        let outcome = std::panic::AssertUnwindSafe(async {
            if matches!(failure, CreateFixtureFailure::BeforeReadiness) {
                panic!("injected before create readiness");
            }
            let expected_cwd = if matches!(stop, CreateStop::ReservationTimeout) {
                repo.path().canonicalize().unwrap()
            } else {
                target.clone()
            };
            gate.wait_started(task.as_ref().unwrap(), &expected_cwd).await;
            // Prove the committed side effects before selecting a stop path.
            // An absent ref after a timeout before reservation is not rollback.
            assert_eq!(git_stdout(repo.path(), ["rev-parse", "refs/heads/create-held"]), expected_oid);
            let ownership = git_stdout(repo.path(), [
                "for-each-ref", "--format=%(refname) %(objectname)", "refs/mini-agent/worktree-create/",
            ]);
            assert_eq!(ownership.lines().count(), 1, "reservation must own exactly one private ref");
            let (reference, oid) = ownership.split_once(' ').unwrap();
            assert!(reference.starts_with("refs/mini-agent/worktree-create/"));
            assert_eq!(oid, expected_oid);
            let registered = git_stdout(repo.path(), ["worktree", "list", "--porcelain"])
                .contains(&target.to_string_lossy().into_owned());
            if matches!(stop, CreateStop::ReservationTimeout) {
                assert!(!target.exists() && !registered, "reservation advanced into worktree add before its hook completed");
            } else {
                assert!(target.exists() && registered, "checkout hook ran without its worktree registration");
                assert_eq!(git_stdout(&target, ["rev-parse", "HEAD"]), expected_oid);
            }
            if matches!(failure, CreateFixtureFailure::AfterReadiness) {
                panic!("injected after create readiness");
            }
            // Reservation has committed. Hold the later ref-deletion
            // transaction to observe admission during actual rollback.
            let hook = repo.path().join(".git/hooks/reference-transaction");
            std::fs::copy(&gate.script, &hook).unwrap();
            std::fs::write(&hook, format!(
                "#!/bin/sh\nif [ \"$1\" = prepared ]; then\n. {}\nexit {}\nfi\nexit 0\n",
                concurrency::quote(&rollback_gate.script),
                u8::from(matches!(failure, CreateFixtureFailure::Rollback)),
            )).unwrap();
            match stop {
                CreateStop::CallerDrop => {
                    task.as_ref().unwrap().abort();
                    let joined = task.take().unwrap().await;
                    assert!(joined.unwrap_err().is_cancelled());
                }
                CreateStop::CheckoutTimeout | CreateStop::ReservationTimeout => {
                    // Native readiness and exact side effects precede the
                    // clock advance. Both hooks remain held by the fixture.
                    tokio::time::pause();
                    tokio::time::advance(CREATE_COMMAND_TIMEOUT + Duration::from_secs(1)).await;
                    tokio::time::resume();
                }
            }
            rollback_gate.wait_ready(
                || task.as_ref().is_some_and(|task| task.is_finished()),
                &repo.path().canonicalize().unwrap(),
            ).await;
            if matches!(failure, CreateFixtureFailure::DuringRollback) {
                panic!("injected during create rollback");
            }
            let (observer, mut events) = tokio::sync::mpsc::unbounded_channel();
            let runner = crate::git::runner::GitRunner::default();
            let mut admission = Box::pin(crate::git::runner::MUTATION_LOCK_OBSERVER.scope(
                observer, runner.acquire_mutation(repo.path()),
            ));
            let (key, ready) = tokio::time::timeout(TEST_MUTATION_ADMISSION_TIMEOUT, async {
                tokio::select! {
                    _ = &mut admission => panic!("repository admission escaped unfinished create rollback"),
                    event = events.recv() => event.expect("repository admission was not observed"),
                }
            }).await.expect("repository admission did not reach its lock");
            assert_eq!(key, repo.path().join(".git").canonicalize().unwrap());
            assert!(!ready, "repository admission escaped unfinished create rollback");
            rollback_gate.release();
            let _admission = tokio::time::timeout(TEST_MUTATION_ADMISSION_TIMEOUT, admission)
                .await.expect("create rollback did not release repository admission").unwrap();

            if let Some(caller) = task.as_mut() {
                let joined = tokio::time::timeout(TEST_MUTATION_ADMISSION_TIMEOUT, caller)
                    .await.expect("timed out create did not return after rollback");
                task.take();
                let error = joined.expect("create caller panicked").expect_err("held create must time out");
                let operation = match stop {
                    CreateStop::CheckoutTimeout => "worktree-add",
                    CreateStop::ReservationTimeout => "create-branch-ref",
                    CreateStop::CallerDrop => unreachable!(),
                };
                assert!(error.contains(&format!("git {operation} timed out")), "unexpected create error: {error}");
            }

            // Inspect rollback while owning repository admission and before
            // the fixture releases its hook. Release/rescue cannot make these
            // assertions pass after a broken production cancellation path.
            assert!(
                !target.exists(),
                "create rollback left its worktree directory"
            );
            assert!(
                !optional_test_ref_exists(repo.path(), "refs/heads/create-held"),
                "create rollback left its branch ref"
            );
            assert!(
                !git_stdout(repo.path(), ["worktree", "list", "--porcelain"])
                    .contains(&target.to_string_lossy().into_owned()),
                "create rollback left its worktree registration"
            );
            assert!(
                git_stdout(
                    repo.path(),
                    [
                        "for-each-ref",
                        "--format=%(refname)",
                        "refs/mini-agent/worktree-create/"
                    ]
                )
                .is_empty(),
                "create rollback left its ownership ref"
            );
        })
        .catch_unwind()
        .await;

        // Always cancel/join the caller, release the native hook, and await
        // the transaction supervisor's lock release, including readiness and
        // rollback assertion failures. Keep its repository alive until then.
        let joined = if let Some(task) = task.take() {
            task.abort();
            Some(task.await)
        } else {
            None
        };
        gate.release();
        rollback_gate.release();
        let cleanup = std::panic::AssertUnwindSafe(async {
            drop(acquire_released_mutation_lock(repo.path(), "create fixture cleanup").await);
        })
        .catch_unwind()
        .await;
        let settled = [gate.settle(), rollback_gate.settle()];
        let caller_settled = joined.as_ref().is_none_or(|result| {
            result.is_ok() || result.as_ref().is_err_and(|error| error.is_cancelled())
        });
        *cleanup_ok = cleanup.is_ok() && settled.iter().all(Result::is_ok) && caller_settled;
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
        if let Err(panic) = cleanup {
            std::panic::resume_unwind(panic);
        }
        assert!(
            caller_settled,
            "create fixture caller panicked during cleanup"
        );
        for result in settled {
            result.unwrap();
        }
    }

    #[cfg(unix)]
    async fn run_create_rollback_case(stop: CreateStop) {
        let base =
            std::env::temp_dir().join(format!("mini-agent-create-base-{}", uuid::Uuid::new_v4()));
        let mut cleanup_ok = false;
        exercise_create_rollback(stop, CreateFixtureFailure::None, &base, &mut cleanup_ok).await;
        assert!(cleanup_ok, "create fixture cleanup did not settle");
        assert!(!base.exists(), "create fixture left its sibling base");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_worktree_create_rolls_back_before_releasing_the_repository() {
        run_create_rollback_case(CreateStop::CallerDrop).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_rollback_fixture_cleans_up_readiness_and_rollback_failures() {
        use CreateStop::{CallerDrop, CheckoutTimeout, ReservationTimeout};
        use futures::FutureExt;
        // Before the caller first runs, all modes have the same owned cleanup.
        // Checkout timeout and caller-drop also share checkout readiness; they
        // diverge only when selecting their stop path. Exercise those prefixes
        // once, then retain every distinct rollback/response lifecycle.
        let cases: &[(CreateFixtureFailure, &[CreateStop], &str)] = &[
            (
                CreateFixtureFailure::BeforeReadiness,
                &[CallerDrop],
                "injected before create readiness",
            ),
            (
                CreateFixtureFailure::AfterReadiness,
                &[CallerDrop, ReservationTimeout],
                "injected after create readiness",
            ),
            (
                CreateFixtureFailure::DuringRollback,
                &[CallerDrop, CheckoutTimeout, ReservationTimeout],
                "injected during create rollback",
            ),
            (
                CreateFixtureFailure::Rollback,
                &[CallerDrop, CheckoutTimeout, ReservationTimeout],
                "create rollback left its branch ref",
            ),
        ];
        for &(failure, stops, expected) in cases {
            for &stop in stops {
                let base = std::env::temp_dir()
                    .join(format!("mini-agent-create-base-{}", uuid::Uuid::new_v4()));
                let mut cleanup_ok = false;
                let panic = std::panic::AssertUnwindSafe(exercise_create_rollback(
                    stop,
                    failure,
                    &base,
                    &mut cleanup_ok,
                ))
                .catch_unwind()
                .await
                .expect_err("fixture failure must propagate after cleanup");
                assert_eq!(panic.downcast_ref::<&str>(), Some(&expected));
                assert!(cleanup_ok, "failed create fixture cleanup did not settle");
                assert!(
                    !base.exists(),
                    "failed create fixture left its sibling base"
                );
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    mod process_tree {
        use super::*;
        use crate::agent::runner::AgentWorkScope;
        use crate::tests::process_gate::ProcessGate;
        use crate::tests::process_state::{ProcessIdentity, ProcessState};
        use futures::FutureExt;

        const FIXTURE_GUARD: Duration = Duration::from_secs(15);
        const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

        enum Stop {
            Timeout,
            CallerDrop,
        }

        fn quote(path: &Path) -> String {
            format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
        }

        async fn pid_record(path: &Path) -> u32 {
            loop {
                if let Ok(text) = std::fs::read_to_string(path)
                    && text.ends_with('\n')
                    && let Ok(pid) = text.trim().parse::<u32>()
                    && pid > 0
                {
                    return pid;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        async fn exercise(stop: Stop, fail_after_readiness: bool) {
            let repo = TempRepo::new("process tree 'repo'");
            let script = repo.path().join(".git/held-alias");
            let alias_pid_file = repo.path().join(".git/alias.pid");
            let descendant_pid_file = repo.path().join(".git/descendant.pid");
            let release = repo.path().join(".git/release");
            let mut gate = ProcessGate::new(&release).unwrap();
            std::fs::write(
                &script,
                r#"#!/bin/sh
set -eu
printf '%s\n' "$$" > "$1"
/bin/sh -c 'printf "%s\n" "$$" > "$1"; IFS= read -r release < "$2"' held-descendant "$2" "$3" &
wait
"#,
            )
            .unwrap();
            let alias = format!(
                "!/bin/sh {} {} {} {}",
                quote(&script),
                quote(&alias_pid_file),
                quote(&descendant_pid_file),
                quote(&release),
            );
            git(repo.path(), ["config", "alias.held-tree", &alias]);
            let scope = AgentWorkScope::new();
            // Dropping this future drops the real GitRunner response receiver.
            // The scope retains the production output worker until native cleanup.
            let mut command = Some(Box::pin(scope.run(run_git_with_limits_for_test(
                repo.path(),
                &["held-tree"],
                test_limits(COMMAND_TIMEOUT),
            ))));
            let mut observed_tree = None;
            let result = std::panic::AssertUnwindSafe(async {
                let (alias_pid, descendant_pid) = tokio::time::timeout(FIXTURE_GUARD, async {
                    tokio::select! {
                        _ = command.as_mut().unwrap() => panic!("Git stopped before publishing its process tree"),
                        pids = async { (pid_record(&alias_pid_file).await, pid_record(&descendant_pid_file).await) } => pids,
                    }
                }).await.expect("Git alias did not publish its complete process tree");
                let alias = ProcessIdentity::capture(alias_pid).unwrap();
                let descendant = ProcessIdentity::capture(descendant_pid).unwrap();
                let ProcessState::Live { group, .. } = alias.state().unwrap() else {
                    panic!("Git alias exited before cancellation readiness");
                };
                // GitRunner configures process_group(0), so this group's leader
                // is the direct Git child rather than an inferred shell parent.
                assert_ne!(group, std::process::id());
                let git = ProcessIdentity::capture(group).unwrap();
                assert!(matches!(descendant.state().unwrap(), ProcessState::Live { group: descendant_group, .. } if descendant_group == group));
                observed_tree = Some([git, alias, descendant]);
                assert!(scope.active_children() > 0);
                if fail_after_readiness {
                    panic!("injected after Git process-tree readiness");
                }

                match stop {
                    Stop::Timeout => {
                        // Start-up uses real native progress. Only the already
                        // armed command deadline advances under the test clock.
                        tokio::time::pause();
                        tokio::time::advance(COMMAND_TIMEOUT + Duration::from_secs(1)).await;
                        tokio::time::resume();
                        let response = tokio::time::timeout(FIXTURE_GUARD, command.as_mut().unwrap())
                            .await.expect("Git deadline did not settle");
                        drop(command.take());
                        let error = match response {
                            Ok(_) => panic!("held Git alias must time out"),
                            Err(error) => error,
                        };
                        assert!(error.contains("timed out"), "unexpected Git result: {error}");
                    }
                    Stop::CallerDrop => {
                        drop(command.take());
                        tokio::time::timeout(FIXTURE_GUARD, scope.wait_idle())
                            .await.expect("dropped Git caller did not settle owned work");
                    }
                }
                // Take the acceptance snapshot before releasing fixture gates.
                let [git, alias, descendant] = observed_tree.unwrap().map(|process| process.state().unwrap());
                assert!(matches!(git, ProcessState::Gone | ProcessState::Replaced), "direct Git child was not reaped: {git:?}");
                for (label, state) in [("alias", alias), ("descendant", descendant)] {
                    assert!(matches!(state, ProcessState::Exited | ProcessState::Gone | ProcessState::Replaced), "Git {label} remained live at settlement: {state:?}");
                }
                assert_eq!(scope.active_children(), 0);
            }).catch_unwind().await;

            drop(command.take());
            let released = gate.release();
            scope.cancellation_handle().cancel();
            tokio::time::timeout(FIXTURE_GUARD, scope.wait_idle())
                .await
                .expect("Git fixture output worker did not settle");
            released.unwrap();
            if let Some(tree) = observed_tree {
                // This is cleanup only; it cannot change the earlier acceptance
                // snapshot or turn a live-survivor assertion into success.
                tokio::time::timeout(FIXTURE_GUARD, async {
                    loop {
                        let states = tree.map(|process| process.state().unwrap());
                        if states
                            .iter()
                            .all(|state| !matches!(state, ProcessState::Live { .. }))
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .expect("Git fixture process survived its release gate");
            }
            if let Err(panic) = result {
                std::panic::resume_unwind(panic);
            }
        }

        #[tokio::test]
        async fn git_runner_timeout_kills_delayed_alias_tree() {
            exercise(Stop::Timeout, false).await;
        }

        #[tokio::test]
        async fn dropping_git_caller_cancels_owned_process_tree() {
            exercise(Stop::CallerDrop, false).await;
        }

        #[tokio::test]
        async fn git_process_tree_fixture_settles_after_readiness_failure() {
            let panic = std::panic::AssertUnwindSafe(exercise(Stop::CallerDrop, true))
                .catch_unwind()
                .await
                .expect_err("fixture failure must propagate after cleanup");
            assert_eq!(
                panic.downcast_ref::<&str>(),
                Some(&"injected after Git process-tree readiness")
            );
        }
    }

    #[cfg(unix)]
    #[derive(Clone, Copy)]
    enum MergeStop {
        Fetch,
        Commit,
    }

    #[cfg(unix)]
    #[derive(Clone, Copy)]
    enum MergeFixtureFailure {
        None,
        BeforeReadiness,
        AfterReadiness,
        DuringRollback,
        Verification,
    }

    #[cfg(unix)]
    async fn exercise_merge_cancellation(
        stop: MergeStop,
        failure: MergeFixtureFailure,
        path: &Path,
        cleanup_ok: &mut bool,
    ) {
        use futures::FutureExt;

        // The fixture owns every repository, remote, hook and marker before
        // starting the caller, and keeps them until rollback has settled.
        let directory = OwnedDirectory::create(path.to_path_buf());
        let repo =
            TempRepo::initialize(OwnedDirectory::create(directory.path().join("repository")));
        let remote = directory.path().join("bare remote");
        std::fs::create_dir(&remote).unwrap();
        git(&remote, ["init", "--bare"]);
        git(
            repo.path(),
            [
                OsStr::new("remote"),
                OsStr::new("add"),
                OsStr::new("origin"),
                remote.as_os_str(),
            ],
        );
        git(repo.path(), ["push", "-u", "origin", "main"]);
        let original_head = git_stdout(repo.path(), ["rev-parse", "HEAD"]);
        let original_tree = git_stdout(repo.path(), ["rev-parse", "HEAD^{tree}"]);
        git(repo.path(), ["switch", "-c", "feature"]);
        std::fs::write(repo.path().join("tracked.txt"), "feature\n").unwrap();
        git(repo.path(), ["add", "tracked.txt"]);
        git(repo.path(), ["commit", "-m", "feature"]);
        let feature_head = git_stdout(repo.path(), ["rev-parse", "HEAD"]);
        let feature_tree = git_stdout(repo.path(), ["rev-parse", "HEAD^{tree}"]);
        git(repo.path(), ["switch", "main"]);
        std::fs::write(repo.path().join("tracked.txt"), "dirty\n").unwrap();
        let mut gate = concurrency::CommandGate::new(repo.path(), "merge-gate");
        let mut rollback_gate = concurrency::CommandGate::new(repo.path(), "rollback-gate");
        match stop {
            MergeStop::Fetch => git(
                repo.path(),
                [
                    OsStr::new("config"),
                    OsStr::new("remote.origin.uploadpack"),
                    OsStr::new(&concurrency::quote(&gate.script)),
                ],
            ),
            MergeStop::Commit => {
                std::fs::copy(&gate.script, repo.path().join(".git/hooks/pre-commit")).unwrap();
            }
        }
        let info = WorktreeInfo {
            branch: "feature".into(),
            worktree_path: directory.path().join("unused worktree"),
            main_repo_path: repo.path().to_path_buf(),
        };
        let mut task = Some(tokio::spawn(async move {
            let (_, outcome) = try_merge(&info, "main").await;
            Err::<(), _>(format!("merge returned before cancellation: {outcome:?}"))
        }));
        let outcome = std::panic::AssertUnwindSafe(async {
            if matches!(failure, MergeFixtureFailure::BeforeReadiness) {
                panic!("injected before merge readiness");
            }
            // Both the custom local upload-pack command and the commit hook
            // start from the working repository selected by GitRunner.
            gate.wait_started(task.as_ref().unwrap(), &repo.path().canonicalize().unwrap()).await;
            let stash = git_stdout(repo.path(), ["rev-parse", "refs/stash"]);
            assert_eq!(git_stdout(repo.path(), ["show", &format!("{stash}:tracked.txt")]), "dirty");
            assert_eq!(git_stdout(repo.path(), ["rev-parse", &format!("{stash}^1")]), original_head);
            assert_eq!(git_stdout(repo.path(), ["stash", "list", "--format=%H"]), stash);
            assert_eq!(git_stdout(repo.path(), ["rev-parse", "HEAD"]), original_head);
            let expected_tree = match stop { MergeStop::Fetch => &original_tree, MergeStop::Commit => &feature_tree };
            assert_eq!(git_stdout(repo.path(), ["write-tree"]), *expected_tree);
            assert_eq!(std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(), match stop {
                MergeStop::Fetch => "initial\n", MergeStop::Commit => "feature\n",
            });
            if matches!(failure, MergeFixtureFailure::AfterReadiness) {
                panic!("injected after merge readiness");
            }
            let rollback_hook = repo.path().join(match stop {
                MergeStop::Fetch => ".git/hooks/reference-transaction",
                MergeStop::Commit => ".git/hooks/post-checkout",
            });
            std::fs::copy(&rollback_gate.script, &rollback_hook).unwrap();
            if matches!(stop, MergeStop::Fetch) {
                std::fs::write(&rollback_hook, format!(
                    "#!/bin/sh\nif [ \"$1\" = prepared ]; then\nwhile read -r old new reference; do\nif [ \"$reference\" = refs/stash ] && [ \"$old\" = {stash} ] && [ \"$new\" = 0000000000000000000000000000000000000000 ]; then\n. {}\nexit {}\nfi\ndone\nfi\nexit 0\n",
                    concurrency::quote(&rollback_gate.script),
                    u8::from(matches!(failure, MergeFixtureFailure::Verification)),
                )).unwrap();
            } else if matches!(failure, MergeFixtureFailure::Verification) {
                // Make rollback encounter an unowned target ref. Its failure
                // must not skip fixture settlement or erase the original panic.
                git(repo.path(), ["update-ref", "refs/heads/main", &feature_head]);
            }
            task.as_ref().unwrap().abort();
            let joined = task.take().unwrap().await;
            assert!(joined.unwrap_err().is_cancelled());
            rollback_gate.wait_ready(|| false, &repo.path().canonicalize().unwrap()).await;
            if matches!(stop, MergeStop::Fetch) {
                assert_eq!(std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(), "dirty\n");
                assert_eq!(git_stdout(repo.path(), ["rev-parse", "refs/stash"]), stash);
            }
            if matches!(failure, MergeFixtureFailure::DuringRollback) {
                panic!("injected during merge rollback");
            }
            let runner = crate::git::runner::GitRunner::default();
            let (observer, mut events) = tokio::sync::mpsc::unbounded_channel();
            let mut admission = Box::pin(crate::git::runner::MUTATION_LOCK_OBSERVER.scope(
                observer, runner.acquire_mutation(repo.path()),
            ));
            let (key, ready) = tokio::time::timeout(TEST_MUTATION_ADMISSION_TIMEOUT, async {
                tokio::select! {
                    _ = &mut admission => panic!("repository admission escaped unfinished merge rollback"),
                    event = events.recv() => event.expect("merge admission was not observed"),
                }
            }).await.expect("merge admission did not reach its lock");
            assert_eq!(key, repo.path().join(".git").canonicalize().unwrap());
            assert!(!ready, "repository admission escaped unfinished merge rollback");
            rollback_gate.release();
            let _admission = tokio::time::timeout(TEST_MUTATION_ADMISSION_TIMEOUT, admission)
                .await.expect("merge rollback did not release repository admission").unwrap();
            // Take acceptance evidence while holding admission and before any
            // release/rescue of the interrupted fetch or commit fixture.
            assert_eq!(current_branch(repo.path()).await.as_deref(), Some("main"));
            assert_eq!(git_stdout(repo.path(), ["rev-parse", "HEAD"]), original_head, "merge cancellation changed HEAD");
            assert_eq!(git_stdout(repo.path(), ["rev-parse", "refs/heads/feature"]), feature_head);
            assert_eq!(git_stdout(repo.path(), ["write-tree"]), *expected_tree, "merge cancellation changed the retained index tree");
            assert!(!has_merge_conflict(repo.path()).await);
            match stop {
                MergeStop::Fetch => {
                    assert_eq!(std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(), "dirty\n");
                    assert!(git_stdout(repo.path(), ["stash", "list"]).is_empty(), "fetch cancellation retained its restored stash");
                }
                MergeStop::Commit => {
                    assert_eq!(std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(), "feature\n");
                    assert_eq!(git_stdout(repo.path(), ["stash", "list", "--format=%H"]), stash);
                    assert_eq!(git_stdout(repo.path(), ["show", &format!("{stash}:tracked.txt")]), "dirty");
                }
            }
        }).catch_unwind().await;

        let joined = if let Some(task) = task.take() {
            task.abort();
            Some(task.await)
        } else {
            None
        };
        gate.release();
        rollback_gate.release();
        let cleanup = std::panic::AssertUnwindSafe(async {
            drop(acquire_released_mutation_lock(repo.path(), "merge fixture cleanup").await);
        })
        .catch_unwind()
        .await;
        let settled = [gate.settle(), rollback_gate.settle()];
        let caller_settled = joined.as_ref().is_none_or(|result| {
            result.is_ok() || result.as_ref().is_err_and(|error| error.is_cancelled())
        });
        *cleanup_ok = caller_settled && cleanup.is_ok() && settled.iter().all(Result::is_ok);
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
        if let Err(panic) = cleanup {
            std::panic::resume_unwind(panic);
        }
        assert!(
            caller_settled,
            "merge fixture caller panicked during cleanup"
        );
        for result in settled {
            result.unwrap();
        }
    }

    #[cfg(unix)]
    async fn run_merge_cancellation(stop: MergeStop) {
        let path = std::env::temp_dir().join(format!(
            "mini-agent-merge 'cancel'-{}",
            uuid::Uuid::new_v4()
        ));
        let mut cleanup_ok = false;
        exercise_merge_cancellation(stop, MergeFixtureFailure::None, &path, &mut cleanup_ok).await;
        assert!(cleanup_ok);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_merge_during_fetch_restores_stash_and_releases_lock() {
        run_merge_cancellation(MergeStop::Fetch).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_merge_during_commit_retains_squash_and_owned_stash() {
        run_merge_cancellation(MergeStop::Commit).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn merge_cancellation_fixture_settles_readiness_and_rollback_failures() {
        use MergeStop::{Commit, Fetch};
        use futures::FutureExt;
        let cases: &[(MergeFixtureFailure, &[MergeStop], &str)] = &[
            (
                MergeFixtureFailure::BeforeReadiness,
                &[Fetch],
                "injected before merge readiness",
            ),
            (
                MergeFixtureFailure::AfterReadiness,
                &[Fetch, Commit],
                "injected after merge readiness",
            ),
            (
                MergeFixtureFailure::DuringRollback,
                &[Fetch, Commit],
                "injected during merge rollback",
            ),
            (
                MergeFixtureFailure::Verification,
                &[Fetch],
                "fetch cancellation retained its restored stash",
            ),
            (
                MergeFixtureFailure::Verification,
                &[Commit],
                "merge cancellation changed HEAD",
            ),
        ];
        for &(failure, stops, expected) in cases {
            for &stop in stops {
                let path = std::env::temp_dir().join(format!(
                    "mini-agent-merge 'cancel'-{}",
                    uuid::Uuid::new_v4()
                ));
                let mut cleanup_ok = false;
                let panic = std::panic::AssertUnwindSafe(exercise_merge_cancellation(
                    stop,
                    failure,
                    &path,
                    &mut cleanup_ok,
                ))
                .catch_unwind()
                .await
                .expect_err("merge fixture failure must propagate");
                let message = panic
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                    .unwrap();
                assert!(message.contains(expected), "unexpected failure: {message}");
                assert!(cleanup_ok, "merge failure skipped fixture cleanup");
                assert!(!path.exists());
            }
        }
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
    async fn merge_and_cleanup_use_explicit_repository_context() {
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
    async fn cleanup_never_removes_a_worktree_that_became_dirty() {
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

        let error = complete_merge(&mut state)
            .await
            .expect_err("cleanup must fail closed on late dirt");

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

        let error = complete_merge(&mut state)
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
        let admission = acquire_released_mutation_lock(repo.path(), "cancelled squash merge").await;
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
        tokio::time::timeout(TEST_MUTATION_ADMISSION_TIMEOUT, gate.wait_until_reached())
            .await
            .expect("exact stash restore must reach the post-apply mutation gate");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(),
            "captured\n"
        );
        git(repo.path(), ["stash", "push", "-m", "concurrent"]);
        let concurrent = git_stdout(repo.path(), ["rev-parse", "refs/stash"]);
        gate.resume();

        let error = tokio::time::timeout(TEST_MUTATION_ADMISSION_TIMEOUT, task)
            .await
            .expect("exact stash restore must finish after the mutation gate resumes")
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
        tokio::time::timeout(TEST_MUTATION_ADMISSION_TIMEOUT, gate.wait_until_reached())
            .await
            .expect("exact stash restore must reach the post-apply mutation gate");
        std::fs::write(repo.path().join("tracked.txt"), "editor wins\n").unwrap();
        gate.resume();

        let error = tokio::time::timeout(TEST_MUTATION_ADMISSION_TIMEOUT, task)
            .await
            .expect("exact stash restore must finish after the mutation gate resumes")
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
        tokio::time::timeout(TEST_MUTATION_ADMISSION_TIMEOUT, gate.wait_until_reached())
            .await
            .expect("exact stash restore must reach the post-apply mutation gate");
        std::fs::write(repo.path().join("editor-note.txt"), b"editor bytes\0\xff").unwrap();
        gate.resume();

        let error = tokio::time::timeout(TEST_MUTATION_ADMISSION_TIMEOUT, task)
            .await
            .expect("exact stash restore must finish after the mutation gate resumes")
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
        tokio::time::timeout(TEST_MUTATION_ADMISSION_TIMEOUT, gate.wait_until_reached())
            .await
            .expect("stash creation must reach the pre-publication mutation gate");

        git(repo.path(), ["stash", "push", "-m", "concurrent external"]);
        let concurrent = git_stdout(repo.path(), ["rev-parse", "refs/stash"]);
        gate.resume();
        let (_state, outcome) = tokio::time::timeout(TEST_MUTATION_ADMISSION_TIMEOUT, task)
            .await
            .expect("merge must finish after the stash publication gate resumes")
            .unwrap();

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
