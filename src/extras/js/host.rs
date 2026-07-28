use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rquickjs::{Context, Ctx, IntoJs, Object, Value, prelude::Func};
use tokio::time::timeout;

use crate::extras::js::tool::{PermissionBridge, PermissionBridgeError};
use crate::extras::js::types::{STEP_TIMEOUT, SpawnResult};
use crate::sandbox::{Sandbox, kill_process_group};

impl<'js> IntoJs<'js> for SpawnResult {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        let obj = Object::new(ctx.clone())?;
        obj.set("stdout", self.stdout)?;
        obj.set("stderr", self.stderr)?;
        obj.set("code", self.code)?;
        obj.set("timed_out", self.timed_out)?;
        obj.set("stdout_truncated", self.stdout_truncated)?;
        obj.set("stderr_truncated", self.stderr_truncated)?;
        Ok(obj.into())
    }
}

fn permission_error(tool: &'static str, error: PermissionBridgeError) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("permission check", tool, error.to_string())
}

fn timeout_error(tool: &'static str) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("host call", tool, "execution timed out")
}

async fn timeout_host_call<T>(
    tool: &'static str,
    duration: Duration,
    call: impl Future<Output = rquickjs::Result<T>>,
) -> rquickjs::Result<T> {
    timeout(duration, call)
        .await
        .map_err(|_| timeout_error(tool))?
}

fn block_on_host_call<T>(
    runtime: &tokio::runtime::Handle,
    permission_bridge: &PermissionBridge,
    tool: &'static str,
    duration: Duration,
    call: impl Future<Output = rquickjs::Result<T>>,
) -> rquickjs::Result<T> {
    runtime.block_on(async {
        tokio::select! {
            result = timeout_host_call(tool, duration, call) => result,
            _ = permission_bridge.cancelled() => {
                Err(permission_error(tool, PermissionBridgeError::Cancelled))
            }
        }
    })
}

async fn resolve_write_target(path: &str) -> PathBuf {
    let expanded = crate::fs::expand_tilde(path);
    let resolved = crate::fs::resolve_symlink_target(Path::new(&expanded)).await;
    let absolute = if resolved.is_absolute() {
        resolved
    } else if let Ok(working_dir) = std::env::current_dir() {
        working_dir.join(resolved)
    } else {
        resolved
    };
    let mut ancestor = absolute.as_path();
    let mut missing_components = Vec::new();

    loop {
        match tokio::fs::canonicalize(ancestor).await {
            Ok(mut canonical) => {
                for component in missing_components.iter().rev() {
                    canonical.push(component);
                }
                return canonical;
            }
            Err(_) => {
                let Some(file_name) = ancestor.file_name() else {
                    return absolute;
                };
                let Some(parent) = ancestor.parent() else {
                    return absolute;
                };
                missing_components.push(file_name.to_os_string());
                ancestor = parent;
            }
        }
    }
}

pub(crate) fn make_read_file(
    permission_bridge: PermissionBridge,
    runtime: tokio::runtime::Handle,
) -> impl Fn(String) -> rquickjs::Result<String> {
    move |path: String| {
        permission_bridge
            .check_path("read", &path)
            .map_err(|error| permission_error("js/read_file", error))?;
        block_on_host_call(
            &runtime,
            &permission_bridge,
            "js/read_file",
            STEP_TIMEOUT,
            async move {
                tokio::fs::read_to_string(path)
                    .await
                    .map_err(rquickjs::Error::Io)
            },
        )
    }
}

pub(crate) fn make_write_file(
    permission_bridge: PermissionBridge,
    runtime: tokio::runtime::Handle,
) -> impl Fn(String, String) -> rquickjs::Result<()> {
    move |path: String, content: String| {
        let path = runtime.block_on(resolve_write_target(&path));
        let permission_path = path.to_string_lossy();
        permission_bridge
            .check_path("write", &permission_path)
            .map_err(|error| permission_error("js/write_file", error))?;
        block_on_host_call(
            &runtime,
            &permission_bridge,
            "js/write_file",
            STEP_TIMEOUT,
            async move {
                tokio::fs::write(path, content)
                    .await
                    .map_err(rquickjs::Error::Io)
            },
        )
    }
}

pub(crate) fn make_spawn(
    sandbox: Sandbox,
    permission_bridge: PermissionBridge,
    runtime: tokio::runtime::Handle,
) -> impl Fn(String, Vec<String>) -> rquickjs::Result<SpawnResult> {
    make_spawn_with_timeout(sandbox, permission_bridge, runtime, STEP_TIMEOUT)
}

fn make_spawn_with_timeout(
    sandbox: Sandbox,
    permission_bridge: PermissionBridge,
    runtime: tokio::runtime::Handle,
    duration: Duration,
) -> impl Fn(String, Vec<String>) -> rquickjs::Result<SpawnResult> {
    move |cmd: String, args: Vec<String>| {
        let permission_command = std::iter::once(cmd.as_str())
            .chain(args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        permission_bridge
            .check("bash", &permission_command)
            .map_err(|error| permission_error("js/spawn", error))?;
        let mut command = sandbox.wrap_command(r#"exec "$0" "$@""#);
        command
            .arg(&cmd)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let cancellation = permission_bridge.clone();
        let output = runtime.block_on(async {
            let child = command.spawn().map_err(rquickjs::Error::Io)?;
            let pid = child.id();
            tokio::select! {
                output = child.wait_with_output() => output.map_err(rquickjs::Error::Io),
                _ = tokio::time::sleep(duration) => {
                    if let Some(pid) = pid {
                        kill_process_group(pid);
                    }
                    Err(timeout_error("js/spawn"))
                }
                _ = cancellation.cancelled() => {
                    if let Some(pid) = pid {
                        kill_process_group(pid);
                    }
                    Err(permission_error(
                        "js/spawn",
                        PermissionBridgeError::Cancelled,
                    ))
                }
            }
        })?;
        Ok(SpawnResult {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            code: output.status.code().unwrap_or(-1),
            timed_out: false,
            stdout_truncated: false,
            stderr_truncated: false,
        })
    }
}

pub(crate) fn register_host_globals(
    ctx: &Context,
    sandbox: Sandbox,
    permission_bridge: PermissionBridge,
    runtime: tokio::runtime::Handle,
) {
    ctx.with(|ctx| {
        let globals = ctx.globals();

        globals
            .set(
                "read_file",
                Func::from(make_read_file(permission_bridge.clone(), runtime.clone())),
            )
            .expect("register read_file");
        globals
            .set(
                "write_file",
                Func::from(make_write_file(permission_bridge.clone(), runtime.clone())),
            )
            .expect("register write_file");
        globals
            .set(
                "spawn",
                Func::from(make_spawn(sandbox, permission_bridge, runtime)),
            )
            .expect("register spawn");

        let console = Object::new(ctx.clone()).expect("console object");
        console
            .set(
                "log",
                Func::from(|msg: Value| {
                    eprintln!("[js] {:?}", msg);
                }),
            )
            .expect("register console.log");
        globals.set("console", console).expect("register console");
    });
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::extras::js::tool::PermissionBridgeOwner;
    use crate::permission::ask::{AskSender, UserDecision};
    use crate::permission::checker::{PermCheck, PermissionChecker};
    use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "zerostack_js_write_permission_test_{}_{}",
                std::process::id(),
                n
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn standard_permission(working_dir: PathBuf) -> PermCheck {
        Arc::new(Mutex::new(PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Standard,
            Some(working_dir),
            Some(vec!["standard".to_string()]),
        )))
    }

    fn host_permission(working_dir: PathBuf, action: Action, doom_loop: Action) -> PermCheck {
        let config = PermissionConfig {
            bash: Some(ToolPerm::Simple(action)),
            read: Some(ToolPerm::Simple(action)),
            write: Some(ToolPerm::Simple(action)),
            doom_loop: Some(doom_loop),
            ..PermissionConfig::default()
        };
        Arc::new(Mutex::new(PermissionChecker::new(
            &PermissionConfigs::from(config),
            SecurityMode::Standard,
            Some(working_dir),
            Some(vec!["standard".to_string()]),
        )))
    }

    async fn call_read_file(
        permission: PermCheck,
        ask_tx: Option<AskSender>,
        path: PathBuf,
    ) -> Result<String, String> {
        let runtime = tokio::runtime::Handle::current();
        let owner = PermissionBridgeOwner::new(Some(permission), ask_tx, STEP_TIMEOUT);
        let bridge = owner.bridge();
        tokio::task::spawn_blocking(move || {
            let _owner = owner;
            make_read_file(bridge, runtime)(path.to_string_lossy().into_owned())
                .map_err(|error| error.to_string())
        })
        .await
        .expect("read_file test task panicked")
    }

    async fn call_write_file(
        permission: PermCheck,
        ask_tx: Option<AskSender>,
        path: PathBuf,
        content: &'static str,
    ) -> Result<(), String> {
        let runtime = tokio::runtime::Handle::current();
        let owner = PermissionBridgeOwner::new(Some(permission), ask_tx, STEP_TIMEOUT);
        let bridge = owner.bridge();
        tokio::task::spawn_blocking(move || {
            let _owner = owner;
            make_write_file(bridge, runtime)(
                path.to_string_lossy().into_owned(),
                content.to_string(),
            )
            .map_err(|error| error.to_string())
        })
        .await
        .expect("write_file test task panicked")
    }

    async fn call_spawn(
        permission: PermCheck,
        ask_tx: Option<AskSender>,
        cmd: &'static str,
        args: Vec<String>,
    ) -> Result<SpawnResult, String> {
        let runtime = tokio::runtime::Handle::current();
        let owner = PermissionBridgeOwner::new(Some(permission), ask_tx, STEP_TIMEOUT);
        let bridge = owner.bridge();
        tokio::task::spawn_blocking(move || {
            let _owner = owner;
            make_spawn(Sandbox::new(false, "bwrap"), bridge, runtime)(cmd.to_string(), args)
                .map_err(|error| error.to_string())
        })
        .await
        .expect("spawn test task panicked")
    }

    #[tokio::test]
    async fn file_host_call_timeout_reports_execution_timed_out() {
        for tool in ["js/read_file", "js/write_file"] {
            let error = timeout_host_call(
                tool,
                Duration::from_millis(1),
                std::future::pending::<rquickjs::Result<()>>(),
            )
            .await
            .expect_err("pending host call should time out");

            assert!(
                error.to_string().contains("execution timed out"),
                "unexpected {tool} timeout error: {error}"
            );
        }
    }

    #[tokio::test]
    async fn read_file_allows_paths_within_working_directory() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        std::fs::create_dir_all(&working_dir).unwrap();
        let source = working_dir.join("source.txt");
        std::fs::write(&source, "allowed").unwrap();

        let contents = call_read_file(standard_permission(working_dir), None, source)
            .await
            .expect("workspace read should be allowed");

        assert_eq!(contents, "allowed");
    }

    #[tokio::test]
    async fn read_file_denies_external_absolute_path_without_permission_response() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        let external_dir = temp.path().join("external");
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::create_dir_all(&external_dir).unwrap();
        let source = external_dir.join("secret.txt");
        std::fs::write(&source, "secret").unwrap();

        let error = call_read_file(standard_permission(working_dir), None, source)
            .await
            .expect_err("external read should require permission");

        assert!(
            error.contains("Permission denied"),
            "unexpected permission error: {error}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_file_denies_relative_parent_traversal_without_permission_response() {
        let working_dir = std::env::current_dir().unwrap();
        let path = PathBuf::from("../".repeat(32)).join("etc/passwd");

        let error = call_read_file(standard_permission(working_dir), None, path)
            .await
            .expect_err("relative traversal should require permission");

        assert!(
            error.contains("Permission denied"),
            "unexpected permission error: {error}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_file_denies_symlink_escape_without_permission_response() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        let external_dir = temp.path().join("external");
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::create_dir_all(&external_dir).unwrap();
        let external_source = external_dir.join("secret.txt");
        let allowed_link = working_dir.join("source-link.txt");
        std::fs::write(&external_source, "secret").unwrap();
        symlink(&external_source, &allowed_link).unwrap();

        let error = call_read_file(standard_permission(working_dir), None, allowed_link)
            .await
            .expect_err("symlinked external read should require permission");

        assert!(
            error.contains("Permission denied"),
            "unexpected permission error: {error}"
        );
    }

    #[tokio::test]
    async fn write_file_allows_paths_within_working_directory() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        std::fs::create_dir_all(&working_dir).unwrap();
        let target = working_dir.join("created.txt");

        call_write_file(
            standard_permission(working_dir),
            None,
            target.clone(),
            "allowed",
        )
        .await
        .expect("workspace write should be allowed");

        assert_eq!(std::fs::read_to_string(target).unwrap(), "allowed");
    }

    #[tokio::test]
    async fn write_file_denies_external_path_without_permission_response() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        let external_dir = temp.path().join("external");
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::create_dir_all(&external_dir).unwrap();
        let target = external_dir.join("denied.txt");

        let error = call_write_file(
            standard_permission(working_dir),
            None,
            target.clone(),
            "forbidden",
        )
        .await
        .expect_err("external write should require permission");

        assert!(
            error.to_string().contains("Permission denied"),
            "unexpected permission error: {error}"
        );
        assert!(!target.exists(), "denied external target was written");
    }

    #[tokio::test]
    async fn write_file_prompts_for_external_directory_permission() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        let external_dir = temp.path().join("external");
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::create_dir_all(&external_dir).unwrap();
        let target = external_dir.join("approved.txt");
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);

        let write = tokio::spawn(call_write_file(
            standard_permission(working_dir),
            Some(ask_tx),
            target.clone(),
            "approved",
        ));
        let request = ask_rx
            .recv()
            .await
            .expect("external write should request permission");
        assert_eq!(request.tool.as_str(), "write");
        assert_eq!(request.input.as_str(), target.to_string_lossy().as_ref());
        request
            .reply
            .send(UserDecision::AllowOnce)
            .expect("permission request receiver dropped");

        write
            .await
            .expect("write task panicked")
            .expect("approved external write should succeed");
        assert_eq!(std::fs::read_to_string(target).unwrap(), "approved");
    }

    #[tokio::test]
    async fn host_calls_prompt_with_standard_tool_names_and_honor_denial() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        std::fs::create_dir_all(&working_dir).unwrap();
        let source = working_dir.join("source.txt");
        let target = working_dir.join("target.txt");
        std::fs::write(&source, "secret").unwrap();
        let permission = host_permission(working_dir, Action::Ask, Action::Deny);
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);

        let read = tokio::spawn(call_read_file(
            permission.clone(),
            Some(ask_tx.clone()),
            source.clone(),
        ));
        let request = ask_rx.recv().await.expect("read should request permission");
        assert_eq!(request.tool, "read");
        assert_eq!(request.input.as_str(), source.to_string_lossy().as_ref());
        request
            .reply
            .send(UserDecision::Deny)
            .expect("read permission request receiver dropped");
        let error = read
            .await
            .expect("read task panicked")
            .expect_err("denied read must fail");
        assert!(error.contains("Permission denied by user"));

        let write = tokio::spawn(call_write_file(
            permission.clone(),
            Some(ask_tx.clone()),
            target.clone(),
            "forbidden",
        ));
        let request = ask_rx
            .recv()
            .await
            .expect("write should request permission");
        assert_eq!(request.tool, "write");
        assert_eq!(request.input.as_str(), target.to_string_lossy().as_ref());
        request
            .reply
            .send(UserDecision::Deny)
            .expect("write permission request receiver dropped");
        let error = write
            .await
            .expect("write task panicked")
            .expect_err("denied write must fail");
        assert!(error.contains("Permission denied by user"));
        assert!(!target.exists(), "denied write created the target");

        let spawn = tokio::spawn(call_spawn(
            permission,
            Some(ask_tx),
            "touch",
            vec![target.to_string_lossy().into_owned()],
        ));
        let request = ask_rx
            .recv()
            .await
            .expect("spawn should request permission");
        assert_eq!(request.tool, "bash");
        assert_eq!(request.input, format!("touch {}", target.to_string_lossy()));
        request
            .reply
            .send(UserDecision::Deny)
            .expect("spawn permission request receiver dropped");
        let error = spawn
            .await
            .expect("spawn task panicked")
            .expect_err("denied spawn must fail");
        assert!(error.contains("Permission denied by user"));
        assert!(!target.exists(), "denied spawn created the target");
    }

    #[tokio::test]
    async fn spawn_restricted_command_is_denied_before_execution() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        std::fs::create_dir_all(&working_dir).unwrap();
        let target = working_dir.join("must-not-exist.txt");
        let permission = host_permission(working_dir, Action::Deny, Action::Deny);

        let error = call_spawn(
            permission,
            None,
            "touch",
            vec![target.to_string_lossy().into_owned()],
        )
        .await
        .expect_err("restricted spawn must fail");

        assert!(
            error.contains("Permission denied"),
            "unexpected permission error: {error}"
        );
        assert!(!target.exists(), "denied spawn created the target");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_preserves_output_and_exit_code_through_sandbox_wrapper() {
        let runtime = tokio::runtime::Handle::current();
        let owner = PermissionBridgeOwner::new(None, None, STEP_TIMEOUT);
        let bridge = owner.bridge();
        let result = tokio::task::spawn_blocking(move || {
            let _owner = owner;
            make_spawn(Sandbox::new(false, "bwrap"), bridge, runtime)(
                "sh".to_string(),
                vec![
                    "-c".to_string(),
                    "printf stdout; printf stderr >&2; exit 7".to_string(),
                ],
            )
        })
        .await
        .expect("spawn output test task panicked")
        .expect("wrapped spawn should succeed");

        assert_eq!(result.stdout, "stdout");
        assert_eq!(result.stderr, "stderr");
        assert_eq!(result.code, 7);
    }

    #[tokio::test]
    async fn repeated_js_read_calls_trigger_doom_loop_detection() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        std::fs::create_dir_all(&working_dir).unwrap();
        let source = working_dir.join("source.txt");
        std::fs::write(&source, "allowed").unwrap();
        let permission = host_permission(working_dir, Action::Allow, Action::Deny);

        for _ in 0..2 {
            let contents = call_read_file(permission.clone(), None, source.clone())
                .await
                .expect("first two reads should be allowed");
            assert_eq!(contents, "allowed");
        }

        let error = call_read_file(permission, None, source)
            .await
            .expect_err("third identical read should trigger doom-loop denial");
        assert!(
            error.contains("Doom loop: repeated identical tool call"),
            "unexpected doom-loop error: {error}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_denies_broken_symlink_escape() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        let external_dir = temp.path().join("external");
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::create_dir_all(&external_dir).unwrap();
        let external_target = external_dir.join("created-through-link.txt");
        let allowed_link = working_dir.join("safe-link.txt");
        symlink(&external_target, &allowed_link).unwrap();

        let error = call_write_file(
            standard_permission(working_dir),
            None,
            allowed_link,
            "forbidden",
        )
        .await
        .expect_err("resolved external symlink target should require permission");

        assert!(
            error.to_string().contains("Permission denied"),
            "unexpected permission error: {error}"
        );
        assert!(
            !external_target.exists(),
            "permission denial must happen before the symlink target is written"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_denies_symlinked_parent_escape_for_new_file() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        let external_dir = temp.path().join("external");
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::create_dir_all(&external_dir).unwrap();
        let linked_dir = working_dir.join("linked-dir");
        symlink(&external_dir, &linked_dir).unwrap();
        let allowed_path = linked_dir.join("created-through-parent-link.txt");
        let external_target = external_dir.join("created-through-parent-link.txt");

        let error = call_write_file(
            standard_permission(working_dir),
            None,
            allowed_path,
            "forbidden",
        )
        .await
        .expect_err("external target beneath symlinked parent should require permission");

        assert!(
            error.to_string().contains("Permission denied"),
            "unexpected permission error: {error}"
        );
        assert!(
            !external_target.exists(),
            "permission denial must happen before the symlinked parent target is written"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_timeout_reports_execution_timed_out() {
        let runtime = tokio::runtime::Handle::current();
        let owner = PermissionBridgeOwner::new(None, None, STEP_TIMEOUT);
        let bridge = owner.bridge();
        let error = tokio::task::spawn_blocking(move || {
            let _owner = owner;
            let spawn = make_spawn_with_timeout(
                Sandbox::new(false, "bwrap"),
                bridge,
                runtime,
                Duration::from_millis(25),
            );
            spawn("sleep".to_string(), vec!["5".to_string()])
                .expect_err("sleep should time out")
                .to_string()
        })
        .await
        .expect("spawn timeout test task panicked");

        assert!(
            error.contains("execution timed out"),
            "unexpected spawn timeout error: {error}"
        );
    }
}
