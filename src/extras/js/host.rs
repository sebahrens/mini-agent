use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rquickjs::{Context, Ctx, IntoJs, Object, Value, prelude::Func};
use tokio::io::AsyncReadExt;
use tokio::time::timeout;

use crate::extras::js::tool::{PermissionBridge, PermissionBridgeError};
use crate::extras::js::types::{
    READ_FILE_MAX_BYTES, STEP_TIMEOUT, SpawnResult, WRITE_FILE_MAX_BYTES,
};
use crate::sandbox::{CommandLimits, CommandOutputLimit, CommandStatus, Sandbox};

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

fn file_error(
    tool: &'static str,
    kind: &'static str,
    message: impl Into<String>,
) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message(kind, tool, message.into())
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

struct ResolvedReadTarget {
    path: PathBuf,
    identity: std::fs::Metadata,
}

#[derive(Clone, Copy)]
enum WriteMode {
    Create,
    Replace,
}

struct ResolvedWriteTarget {
    path: PathBuf,
    parent_identity: std::fs::Metadata,
    mode: WriteMode,
}

fn absolute_lexical(path: &Path) -> std::io::Result<PathBuf> {
    use std::path::Component;

    let source = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in source.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

fn permission_path(tool: &'static str, path: &Path) -> rquickjs::Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| file_error(tool, "invalid path", "resolved path is not valid UTF-8"))
}

async fn resolve_read_target(path: &str) -> rquickjs::Result<ResolvedReadTarget> {
    let expanded = crate::fs::expand_tilde(path);
    let absolute = absolute_lexical(Path::new(&expanded)).map_err(rquickjs::Error::Io)?;
    let canonical = tokio::fs::canonicalize(absolute)
        .await
        .map_err(rquickjs::Error::Io)?;
    permission_path("js/read_file", &canonical)?;
    let identity = crate::fs::stable_path_metadata(&canonical)
        .await
        .map_err(rquickjs::Error::Io)?;
    Ok(ResolvedReadTarget {
        path: canonical,
        identity,
    })
}

async fn read_approved_file(target: ResolvedReadTarget) -> rquickjs::Result<String> {
    if !target.identity.is_file() {
        return Err(file_error(
            "js/read_file",
            "invalid file type",
            "read_file only accepts regular files",
        ));
    }
    if target.identity.len() > READ_FILE_MAX_BYTES as u64 {
        return Err(file_error(
            "js/read_file",
            "resource limit",
            format!("file exceeds {READ_FILE_MAX_BYTES} byte read limit"),
        ));
    }
    let file = crate::fs::open_stable_file(&target.path)
        .await
        .map_err(rquickjs::Error::Io)?;
    let opened = file.metadata().await.map_err(rquickjs::Error::Io)?;
    crate::fs::ensure_same_file(&target.path, &target.identity, &opened)
        .map_err(rquickjs::Error::Io)?;
    if !opened.is_file() {
        return Err(file_error(
            "js/read_file",
            "invalid file type",
            "read_file only accepts regular files",
        ));
    }
    if opened.len() > READ_FILE_MAX_BYTES as u64 {
        return Err(file_error(
            "js/read_file",
            "resource limit",
            format!("file exceeds {READ_FILE_MAX_BYTES} byte read limit"),
        ));
    }

    let mut bytes = Vec::new();
    file.take((READ_FILE_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(rquickjs::Error::Io)?;
    if bytes.len() > READ_FILE_MAX_BYTES {
        return Err(file_error(
            "js/read_file",
            "resource limit",
            format!("file exceeds {READ_FILE_MAX_BYTES} byte read limit"),
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        file_error(
            "js/read_file",
            "invalid encoding",
            "file content is not valid UTF-8",
        )
    })
}

async fn resolve_write_target(path: &str) -> rquickjs::Result<ResolvedWriteTarget> {
    use std::path::Component;

    let expanded = crate::fs::expand_tilde(path);
    let absolute = absolute_lexical(Path::new(&expanded)).map_err(rquickjs::Error::Io)?;
    let (path, mode) = match tokio::fs::symlink_metadata(&absolute).await {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(file_error(
                    "js/write_file",
                    "invalid file type",
                    "write_file does not follow final symlinks",
                ));
            }
            if !metadata.is_file() {
                return Err(file_error(
                    "js/write_file",
                    "invalid file type",
                    "write_file only replaces regular files",
                ));
            }
            (
                tokio::fs::canonicalize(&absolute)
                    .await
                    .map_err(rquickjs::Error::Io)?,
                WriteMode::Replace,
            )
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut ancestor = absolute.as_path();
            let mut missing = Vec::new();
            let canonical_parent = loop {
                match tokio::fs::canonicalize(ancestor).await {
                    Ok(canonical) => break canonical,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        let name = ancestor.file_name().ok_or_else(|| {
                            file_error(
                                "js/write_file",
                                "invalid path",
                                "write target must name a file",
                            )
                        })?;
                        missing.push(name.to_os_string());
                        ancestor = ancestor.parent().ok_or_else(|| {
                            file_error(
                                "js/write_file",
                                "invalid path",
                                "write target has no existing parent",
                            )
                        })?;
                    }
                    Err(error) => return Err(rquickjs::Error::Io(error)),
                }
            };
            if missing.len() != 1 {
                return Err(file_error(
                    "js/write_file",
                    "invalid path",
                    "write target parent directory does not exist",
                ));
            }
            let relative = Path::new(&missing[0]);
            if relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            {
                return Err(file_error(
                    "js/write_file",
                    "invalid path",
                    "write target contains an invalid path component",
                ));
            }
            (canonical_parent.join(relative), WriteMode::Create)
        }
        Err(error) => return Err(rquickjs::Error::Io(error)),
    };
    permission_path("js/write_file", &path)?;
    let parent = path.parent().ok_or_else(|| {
        file_error(
            "js/write_file",
            "invalid path",
            "write target has no parent directory",
        )
    })?;
    let parent_identity = crate::fs::stable_path_metadata(parent)
        .await
        .map_err(rquickjs::Error::Io)?;
    if !parent_identity.is_dir() {
        return Err(file_error(
            "js/write_file",
            "invalid file type",
            "write target parent is not a directory",
        ));
    }
    Ok(ResolvedWriteTarget {
        path,
        parent_identity,
        mode,
    })
}

async fn write_approved_file(target: ResolvedWriteTarget, content: String) -> rquickjs::Result<()> {
    match target.mode {
        WriteMode::Create => {
            crate::fs::atomic_create_resolved_checked(
                target.path,
                content.as_bytes(),
                target.parent_identity,
            )
            .await
        }
        WriteMode::Replace => {
            crate::fs::atomic_write_resolved_checked(
                target.path,
                content.as_bytes(),
                target.parent_identity,
            )
            .await
        }
    }
    .map_err(rquickjs::Error::Io)
}

pub(crate) fn make_read_file(
    permission_bridge: PermissionBridge,
    runtime: tokio::runtime::Handle,
) -> impl Fn(String) -> rquickjs::Result<String> {
    move |path: String| {
        let target = block_on_host_call(
            &runtime,
            &permission_bridge,
            "js/read_file",
            STEP_TIMEOUT,
            resolve_read_target(&path),
        )?;
        let permission_path = permission_path("js/read_file", &target.path)?;
        permission_bridge
            .check_path("js/read_file", &permission_path)
            .map_err(|error| permission_error("js/read_file", error))?;
        block_on_host_call(
            &runtime,
            &permission_bridge,
            "js/read_file",
            STEP_TIMEOUT,
            read_approved_file(target),
        )
    }
}

pub(crate) fn make_write_file(
    permission_bridge: PermissionBridge,
    runtime: tokio::runtime::Handle,
) -> impl Fn(String, String) -> rquickjs::Result<()> {
    move |path: String, content: String| {
        if content.len() > WRITE_FILE_MAX_BYTES {
            return Err(file_error(
                "js/write_file",
                "resource limit",
                format!("content exceeds {WRITE_FILE_MAX_BYTES} byte write limit"),
            ));
        }
        let target = block_on_host_call(
            &runtime,
            &permission_bridge,
            "js/write_file",
            STEP_TIMEOUT,
            resolve_write_target(&path),
        )?;
        let permission_path = permission_path("js/write_file", &target.path)?;
        permission_bridge
            .check_path("js/write_file", &permission_path)
            .map_err(|error| permission_error("js/write_file", error))?;
        block_on_host_call(
            &runtime,
            &permission_bridge,
            "js/write_file",
            STEP_TIMEOUT,
            write_approved_file(target, content),
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

const SPAWN_STDOUT_MAX_BYTES: usize = 1024 * 1024;
const SPAWN_STDERR_MAX_BYTES: usize = 1024 * 1024;
const SPAWN_COMBINED_MAX_BYTES: usize = 1536 * 1024;
const CONSOLE_MAX_BYTES_PER_STEP: usize = 256 * 1024;

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
        let mut command = sandbox.wrap_command(r#"exec "$0" "$@""#).map_err(|e| {
            permission_error(
                "js/spawn",
                PermissionBridgeError::Denied(crate::extras::js::types::PermissionDenial::Policy(
                    e,
                )),
            )
        })?;
        command.arg(&cmd).args(&args);
        let limits = CommandLimits {
            timeout: duration,
            stdout_bytes: SPAWN_STDOUT_MAX_BYTES,
            stderr_bytes: SPAWN_STDERR_MAX_BYTES,
            combined_bytes: SPAWN_COMBINED_MAX_BYTES,
        };
        let bridge = permission_bridge.clone();
        let output = runtime.block_on(async {
            tokio::select! {
                result = sandbox.output_built_command_with_limits(command, limits) => {
                    result.map_err(rquickjs::Error::Io)
                }
                _ = bridge.cancelled() => {
                    Err(permission_error("js/spawn", PermissionBridgeError::Cancelled))
                }
            }
        })?;
        let timed_out = output.status == CommandStatus::TimedOut;
        let stdout_truncated = matches!(
            output.status,
            CommandStatus::OutputLimitExceeded(CommandOutputLimit::Stdout)
                | CommandStatus::OutputLimitExceeded(CommandOutputLimit::Combined)
        );
        let stderr_truncated = matches!(
            output.status,
            CommandStatus::OutputLimitExceeded(CommandOutputLimit::Stderr)
                | CommandStatus::OutputLimitExceeded(CommandOutputLimit::Combined)
        );
        Ok(SpawnResult {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            code: output.exit_status.and_then(|s| s.code()).unwrap_or(-1),
            timed_out,
            stdout_truncated,
            stderr_truncated,
        })
    }
}

pub(crate) fn register_host_globals(
    ctx: &Context,
    sandbox: Sandbox,
    permission_bridge: PermissionBridge,
    runtime: tokio::runtime::Handle,
) -> rquickjs::Result<()> {
    ctx.with(|ctx| {
        let globals = ctx.globals();

        globals.set(
            "read_file",
            Func::from(make_read_file(permission_bridge.clone(), runtime.clone())),
        )?;
        globals.set(
            "write_file",
            Func::from(make_write_file(permission_bridge.clone(), runtime.clone())),
        )?;
        globals.set(
            "spawn",
            Func::from(make_spawn(sandbox, permission_bridge, runtime)),
        )?;

        let console = Object::new(ctx.clone())?;
        let console_bytes_remaining = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
            CONSOLE_MAX_BYTES_PER_STEP,
        ));
        console.set(
            "log",
            Func::from(move |msg: Value| {
                let text = format!("{msg:?}");
                let len = text.len();
                let remaining = console_bytes_remaining
                    .fetch_update(
                        std::sync::atomic::Ordering::Relaxed,
                        std::sync::atomic::Ordering::Relaxed,
                        |r| r.checked_sub(len),
                    )
                    .unwrap_or(0);
                if remaining > 0 {
                    eprintln!("[js] {text}");
                }
            }),
        )?;
        globals.set("console", console)?;
        Ok(())
    })
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

    #[tokio::test]
    async fn register_host_globals_returns_error_under_memory_pressure() {
        let runtime = rquickjs::Runtime::new().expect("create QuickJS runtime");
        let ctx = Context::full(&runtime).expect("create QuickJS context");
        runtime.set_memory_limit(1);

        let permission_owner = PermissionBridgeOwner::new(None, None, STEP_TIMEOUT);
        let result = register_host_globals(
            &ctx,
            Sandbox::new(false, "bwrap"),
            permission_owner.bridge(),
            tokio::runtime::Handle::current(),
        );

        assert!(
            result.is_err(),
            "host-global registration should report allocation failure"
        );
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
    async fn js_file_host_permissions_host_call_timeout_reports_execution_timed_out() {
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
    async fn js_file_host_permissions_read_allows_paths_within_working_directory() {
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
    async fn js_file_host_permissions_read_ask_uses_exact_canonical_permission_key() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        let external_dir = temp.path().join("external");
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::create_dir_all(&external_dir).unwrap();
        let source = external_dir.join("source.txt");
        std::fs::write(&source, "approved").unwrap();
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);

        let read = tokio::spawn(call_read_file(
            standard_permission(working_dir),
            Some(ask_tx),
            source.clone(),
        ));
        let request = ask_rx.recv().await.expect("read should request permission");
        assert_eq!(request.tool.as_str(), "js/read_file");
        assert_eq!(
            request.input.as_str(),
            source.canonicalize().unwrap().to_string_lossy().as_ref()
        );
        request
            .reply
            .send(UserDecision::AllowOnce)
            .expect("permission request receiver dropped");

        assert_eq!(
            read.await
                .expect("read task panicked")
                .expect("approved read should succeed"),
            "approved"
        );
    }

    #[tokio::test]
    async fn js_file_host_permissions_read_rejects_oversized_and_non_utf8_content() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        std::fs::create_dir_all(&working_dir).unwrap();
        let oversized = working_dir.join("oversized.txt");
        let non_utf8 = working_dir.join("non-utf8.txt");
        std::fs::write(&oversized, vec![b'x'; READ_FILE_MAX_BYTES + 1]).unwrap();
        std::fs::write(&non_utf8, [0xff, 0xfe]).unwrap();

        let error = call_read_file(standard_permission(working_dir.clone()), None, oversized)
            .await
            .expect_err("oversized read should fail");
        assert!(
            error.contains("resource limit"),
            "unexpected error: {error}"
        );

        let error = call_read_file(standard_permission(working_dir), None, non_utf8)
            .await
            .expect_err("non-UTF-8 read should fail");
        assert!(
            error.contains("invalid encoding"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn js_file_host_permissions_read_denies_external_path_without_response() {
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
    async fn js_file_host_permissions_read_denies_relative_parent_traversal() {
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
    async fn js_file_host_permissions_read_denies_symlink_escape() {
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
    async fn js_file_host_permissions_write_allows_paths_within_working_directory() {
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
    async fn js_file_host_permissions_write_rejects_oversized_content_before_mutation() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        std::fs::create_dir_all(&working_dir).unwrap();
        let target = working_dir.join("oversized.txt");
        let runtime = tokio::runtime::Handle::current();
        let owner =
            PermissionBridgeOwner::new(Some(standard_permission(working_dir)), None, STEP_TIMEOUT);
        let bridge = owner.bridge();
        let target_for_call = target.clone();

        let error = tokio::task::spawn_blocking(move || {
            let _owner = owner;
            make_write_file(bridge, runtime)(
                target_for_call.to_string_lossy().into_owned(),
                "x".repeat(WRITE_FILE_MAX_BYTES + 1),
            )
            .expect_err("oversized write should fail")
            .to_string()
        })
        .await
        .expect("write task panicked");

        assert!(
            error.contains("resource limit"),
            "unexpected error: {error}"
        );
        assert!(!target.exists(), "oversized write created the target");
    }

    #[tokio::test]
    async fn js_file_host_permissions_write_denies_external_path_without_response() {
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
    async fn js_file_host_permissions_write_prompts_for_external_directory() {
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
        assert_eq!(request.tool.as_str(), "js/write_file");
        let expected_target = target
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap()
            .join(target.file_name().unwrap());
        assert_eq!(
            request.input.as_str(),
            expected_target.to_string_lossy().as_ref()
        );
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
    async fn js_file_host_permissions_use_js_tool_names_and_honor_denial() {
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
        assert_eq!(request.tool, "js/read_file");
        assert_eq!(
            request.input.as_str(),
            source.canonicalize().unwrap().to_string_lossy().as_ref()
        );
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
        assert_eq!(request.tool, "js/write_file");
        let expected_target = target
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap()
            .join(target.file_name().unwrap());
        assert_eq!(
            request.input.as_str(),
            expected_target.to_string_lossy().as_ref()
        );
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
    async fn js_file_host_permissions_repeated_reads_trigger_doom_loop_detection() {
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
    async fn js_file_host_permissions_write_rejects_broken_final_symlink() {
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
        .expect_err("final symlink should be rejected");

        assert!(
            error.to_string().contains("does not follow final symlinks"),
            "unexpected permission error: {error}"
        );
        assert!(
            !external_target.exists(),
            "permission denial must happen before the symlink target is written"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn js_file_host_permissions_write_denies_symlinked_parent_escape() {
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
    async fn js_file_host_permissions_reject_target_and_parent_swaps() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new();
        let workspace = temp.path().join("workspace");
        let parent = workspace.join("parent");
        let external = temp.path().join("external");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::create_dir_all(&external).unwrap();

        let source = workspace.join("source.txt");
        let original = workspace.join("original.txt");
        std::fs::write(&source, "approved identity").unwrap();
        let approved_read = resolve_read_target(source.to_str().unwrap())
            .await
            .expect("resolve read target");
        std::fs::rename(&source, &original).unwrap();
        std::fs::write(&source, "replacement").unwrap();
        let error = read_approved_file(approved_read)
            .await
            .expect_err("replaced read target must fail");
        assert!(
            error
                .to_string()
                .contains("Path changed after permission check"),
            "unexpected read swap error: {error}"
        );

        let target = parent.join("created.txt");
        let external_target = external.join("created.txt");
        let approved_write = resolve_write_target(target.to_str().unwrap())
            .await
            .expect("resolve write target");
        let original_parent = workspace.join("original-parent");
        std::fs::rename(&parent, &original_parent).unwrap();
        symlink(&external, &parent).unwrap();
        let error = write_approved_file(approved_write, "must not escape".to_string())
            .await
            .expect_err("swapped parent must fail");
        assert!(!error.to_string().is_empty(), "parent swap error was empty");
        assert!(
            !external_target.exists(),
            "parent swap redirected the approved write"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_timeout_reports_execution_timed_out() {
        let runtime = tokio::runtime::Handle::current();
        let owner = PermissionBridgeOwner::new(None, None, STEP_TIMEOUT);
        let bridge = owner.bridge();
        let result = tokio::task::spawn_blocking(move || {
            let _owner = owner;
            let spawn = make_spawn_with_timeout(
                Sandbox::new(false, "bwrap"),
                bridge,
                runtime,
                Duration::from_millis(25),
            );
            spawn("sleep".to_string(), vec!["5".to_string()])
                .expect("spawn should return Ok with timed_out=true, not Err")
        })
        .await
        .expect("spawn timeout test task panicked");

        assert!(
            result.timed_out,
            "timed_out must be true when deadline expires"
        );
        assert_eq!(result.code, -1, "timed-out process has no exit code");
    }
}
