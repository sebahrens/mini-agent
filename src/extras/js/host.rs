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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileAccess {
    Read,
    Write,
}

impl FileAccess {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AllowPolicyReason {
    NoConfiguredRoots(FileAccess),
    InvalidConfiguration(FileAccess),
    InvalidTarget(FileAccess),
    AmbiguousSymlink(FileAccess),
    OutsideConfiguredRoots(FileAccess),
}

impl std::fmt::Display for AllowPolicyReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (access, reason) = match self {
            Self::NoConfiguredRoots(access) => (
                access,
                "no roots are configured; unrestricted access requires an explicit opt-in",
            ),
            Self::InvalidConfiguration(access) => {
                (access, "the configured roots are invalid or ambiguous")
            }
            Self::InvalidTarget(access) => {
                (access, "the target path is invalid or cannot be resolved")
            }
            Self::AmbiguousSymlink(access) => (access, "the target is a final or dangling symlink"),
            Self::OutsideConfiguredRoots(access) => (
                access,
                "the resolved target is outside the configured roots",
            ),
        };
        write!(formatter, "JS file {} denied: {reason}", access.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthorizationDecision {
    Allowed(PathBuf),
    Denied(AllowPolicyReason),
}

#[derive(Debug, Clone)]
enum PathPolicy {
    Deny(AllowPolicyReason),
    Roots(Vec<PathBuf>),
    Unrestricted,
}

/// Canonical, component-aware narrowing policy for the JS file host globals.
///
/// Relative configured roots are resolved against `base`, which is captured
/// once at startup. The resulting policy never consults the process CWD.
#[derive(Debug, Clone)]
pub(crate) struct AllowConfig {
    base: PathBuf,
    read: PathPolicy,
    write: PathPolicy,
}

impl AllowConfig {
    pub(crate) fn from_settings(
        startup_base: &Path,
        configured_base: Option<&str>,
        read_roots: Option<&[String]>,
        write_roots: Option<&[String]>,
        read_unrestricted: bool,
        write_unrestricted: bool,
    ) -> Self {
        let base = resolve_policy_base(startup_base, configured_base);
        let Ok(base) = base else {
            return Self {
                base: startup_base.to_path_buf(),
                read: PathPolicy::Deny(AllowPolicyReason::InvalidConfiguration(FileAccess::Read)),
                write: PathPolicy::Deny(AllowPolicyReason::InvalidConfiguration(FileAccess::Write)),
            };
        };

        Self {
            read: build_path_policy(&base, read_roots, read_unrestricted, FileAccess::Read),
            write: build_path_policy(&base, write_roots, write_unrestricted, FileAccess::Write),
            base,
        }
    }

    #[cfg(test)]
    pub(crate) fn unrestricted(base: &Path) -> Self {
        Self::from_settings(base, None, None, None, true, true)
    }

    pub(crate) fn authorize_read(&self, target: &Path) -> AuthorizationDecision {
        let resolved = match resolve_policy_read_target(&self.base, target) {
            Ok(path) => path,
            Err(reason) => return AuthorizationDecision::Denied(reason),
        };
        authorize_resolved(&self.read, resolved, FileAccess::Read)
    }

    pub(crate) fn authorize_write(&self, target: &Path) -> AuthorizationDecision {
        let resolved = match resolve_policy_write_target(&self.base, target) {
            Ok(path) => path,
            Err(reason) => return AuthorizationDecision::Denied(reason),
        };
        authorize_resolved(&self.write, resolved, FileAccess::Write)
    }
}

fn path_has_ambiguous_spelling(path: &Path) -> bool {
    let Some(path) = path.to_str() else {
        return true;
    };
    if path.is_empty() || path.contains('\0') {
        return true;
    }
    #[cfg(unix)]
    if path.contains('\\') {
        return true;
    }
    false
}

fn absolute_lexical_from(base: &Path, path: &Path) -> std::io::Result<PathBuf> {
    use std::path::Component;

    let source = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
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
    if normalized.is_absolute() {
        Ok(normalized)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path did not resolve to an absolute path",
        ))
    }
}

fn resolve_policy_base(
    startup_base: &Path,
    configured_base: Option<&str>,
) -> std::io::Result<PathBuf> {
    if path_has_ambiguous_spelling(startup_base) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid startup base",
        ));
    }
    let startup_base = std::fs::canonicalize(startup_base)?;
    if !startup_base.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "startup base is not a directory",
        ));
    }
    let configured = configured_base.map_or_else(|| Path::new("."), Path::new);
    if path_has_ambiguous_spelling(configured) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid configured base",
        ));
    }
    let base = absolute_lexical_from(&startup_base, configured)?;
    let base = std::fs::canonicalize(base)?;
    if !base.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "configured base is not a directory",
        ));
    }
    Ok(base)
}

fn build_path_policy(
    base: &Path,
    roots: Option<&[String]>,
    unrestricted: bool,
    access: FileAccess,
) -> PathPolicy {
    if unrestricted {
        return if roots.is_some() {
            PathPolicy::Deny(AllowPolicyReason::InvalidConfiguration(access))
        } else {
            PathPolicy::Unrestricted
        };
    }
    let Some(roots) = roots else {
        return PathPolicy::Deny(AllowPolicyReason::NoConfiguredRoots(access));
    };
    if roots.is_empty() {
        return PathPolicy::Deny(AllowPolicyReason::NoConfiguredRoots(access));
    }

    let mut canonical_roots = Vec::with_capacity(roots.len());
    for root in roots {
        let root = Path::new(root);
        if path_has_ambiguous_spelling(root) {
            return PathPolicy::Deny(AllowPolicyReason::InvalidConfiguration(access));
        }
        let Ok(root) = absolute_lexical_from(base, root)
            .and_then(std::fs::canonicalize)
            .and_then(|root| {
                if root.is_dir() {
                    Ok(root)
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "configured root is not a directory",
                    ))
                }
            })
        else {
            return PathPolicy::Deny(AllowPolicyReason::InvalidConfiguration(access));
        };
        if !canonical_roots.contains(&root) {
            canonical_roots.push(root);
        }
    }
    PathPolicy::Roots(canonical_roots)
}

fn resolve_policy_read_target(base: &Path, target: &Path) -> Result<PathBuf, AllowPolicyReason> {
    if path_has_ambiguous_spelling(target) {
        return Err(AllowPolicyReason::InvalidTarget(FileAccess::Read));
    }
    let absolute = absolute_lexical_from(base, target)
        .map_err(|_| AllowPolicyReason::InvalidTarget(FileAccess::Read))?;
    std::fs::canonicalize(absolute).map_err(|_| AllowPolicyReason::InvalidTarget(FileAccess::Read))
}

fn resolve_policy_write_target(base: &Path, target: &Path) -> Result<PathBuf, AllowPolicyReason> {
    use std::path::Component;

    if path_has_ambiguous_spelling(target) {
        return Err(AllowPolicyReason::InvalidTarget(FileAccess::Write));
    }
    let absolute = absolute_lexical_from(base, target)
        .map_err(|_| AllowPolicyReason::InvalidTarget(FileAccess::Write))?;
    match std::fs::symlink_metadata(&absolute) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(AllowPolicyReason::AmbiguousSymlink(FileAccess::Write));
            }
            std::fs::canonicalize(absolute)
                .map_err(|_| AllowPolicyReason::InvalidTarget(FileAccess::Write))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut ancestor = absolute.as_path();
            let mut missing = Vec::new();
            let canonical_parent = loop {
                match std::fs::canonicalize(ancestor) {
                    Ok(canonical) => break canonical,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        let Some(name) = ancestor.file_name() else {
                            return Err(AllowPolicyReason::InvalidTarget(FileAccess::Write));
                        };
                        missing.push(name.to_os_string());
                        let Some(parent) = ancestor.parent() else {
                            return Err(AllowPolicyReason::InvalidTarget(FileAccess::Write));
                        };
                        ancestor = parent;
                    }
                    Err(_) => return Err(AllowPolicyReason::InvalidTarget(FileAccess::Write)),
                }
            };
            if !canonical_parent.is_dir() {
                return Err(AllowPolicyReason::InvalidTarget(FileAccess::Write));
            }
            let mut resolved = canonical_parent;
            for part in missing.into_iter().rev() {
                let part = Path::new(&part);
                if !part
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
                {
                    return Err(AllowPolicyReason::InvalidTarget(FileAccess::Write));
                }
                resolved.push(part);
            }
            Ok(resolved)
        }
        Err(_) => Err(AllowPolicyReason::InvalidTarget(FileAccess::Write)),
    }
}

fn authorize_resolved(
    policy: &PathPolicy,
    target: PathBuf,
    access: FileAccess,
) -> AuthorizationDecision {
    if !target.is_absolute() || path_has_ambiguous_spelling(&target) {
        return AuthorizationDecision::Denied(AllowPolicyReason::InvalidTarget(access));
    }
    match policy {
        PathPolicy::Deny(reason) => AuthorizationDecision::Denied(*reason),
        PathPolicy::Unrestricted => AuthorizationDecision::Allowed(target),
        PathPolicy::Roots(roots) => {
            if roots.iter().any(|root| target.starts_with(root)) {
                AuthorizationDecision::Allowed(target)
            } else {
                AuthorizationDecision::Denied(AllowPolicyReason::OutsideConfiguredRoots(access))
            }
        }
    }
}

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

fn allow_policy_error(tool: &'static str, reason: AllowPolicyReason) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("file access policy", tool, reason.to_string())
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
    allow_config: AllowConfig,
) -> impl Fn(String) -> rquickjs::Result<String> {
    move |path: String| {
        let target = block_on_host_call(
            &runtime,
            &permission_bridge,
            "js/read_file",
            STEP_TIMEOUT,
            resolve_read_target(&path),
        )?;
        if let AuthorizationDecision::Denied(reason) = allow_config.authorize_read(&target.path) {
            return Err(allow_policy_error("js/read_file", reason));
        }
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
    allow_config: AllowConfig,
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
        if let AuthorizationDecision::Denied(reason) = allow_config.authorize_write(&target.path) {
            return Err(allow_policy_error("js/write_file", reason));
        }
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
    allow_config: AllowConfig,
) -> rquickjs::Result<()> {
    ctx.with(|ctx| {
        let globals = ctx.globals();

        globals.set(
            "read_file",
            Func::from(make_read_file(
                permission_bridge.clone(),
                runtime.clone(),
                allow_config.clone(),
            )),
        )?;
        globals.set(
            "write_file",
            Func::from(make_write_file(
                permission_bridge.clone(),
                runtime.clone(),
                allow_config,
            )),
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

    fn expect_allowed(decision: AuthorizationDecision) -> PathBuf {
        match decision {
            AuthorizationDecision::Allowed(path) => path,
            AuthorizationDecision::Denied(reason) => panic!("expected allow, got {reason}"),
        }
    }

    fn expect_denied(
        decision: AuthorizationDecision,
        expected: AllowPolicyReason,
    ) -> AllowPolicyReason {
        match decision {
            AuthorizationDecision::Allowed(path) => {
                panic!("expected denial, allowed {}", path.display())
            }
            AuthorizationDecision::Denied(reason) => {
                assert_eq!(reason, expected);
                reason
            }
        }
    }

    #[test]
    fn js_file_allow_policy_uses_canonical_component_containment() {
        let temp = TempDir::new();
        let safe = temp.path().join("safe");
        let sibling = temp.path().join("safe-evil");
        std::fs::create_dir_all(&safe).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let descendant = safe.join("child.txt");
        let sibling_file = sibling.join("secret.txt");
        std::fs::write(&descendant, "allowed").unwrap();
        std::fs::write(&sibling_file, "denied").unwrap();
        let roots = vec![safe.to_string_lossy().into_owned()];
        let policy =
            AllowConfig::from_settings(temp.path(), None, Some(&roots), Some(&roots), false, false);

        assert_eq!(
            expect_allowed(policy.authorize_read(&safe)),
            safe.canonicalize().unwrap()
        );
        assert_eq!(
            expect_allowed(policy.authorize_read(&descendant)),
            descendant.canonicalize().unwrap()
        );
        expect_denied(
            policy.authorize_read(&sibling_file),
            AllowPolicyReason::OutsideConfiguredRoots(FileAccess::Read),
        );
        expect_denied(
            policy.authorize_read(&safe.join("..").join("safe-evil").join("secret.txt")),
            AllowPolicyReason::OutsideConfiguredRoots(FileAccess::Read),
        );
    }

    #[test]
    fn js_file_allow_policy_resolves_relative_roots_against_explicit_base() {
        let temp = TempDir::new();
        let configured_base = temp.path().join("policy-base");
        let safe = configured_base.join("safe");
        std::fs::create_dir_all(&safe).unwrap();
        let source = safe.join("source.txt");
        std::fs::write(&source, "allowed").unwrap();
        let roots = vec!["safe".to_string()];
        let policy = AllowConfig::from_settings(
            temp.path(),
            Some("policy-base"),
            Some(&roots),
            Some(&roots),
            false,
            false,
        );

        assert_ne!(std::env::current_dir().unwrap(), configured_base);
        assert_eq!(
            expect_allowed(policy.authorize_read(&source)),
            source.canonicalize().unwrap()
        );
    }

    #[test]
    fn js_file_allow_policy_keeps_read_and_write_roots_separate() {
        let temp = TempDir::new();
        let read_root = temp.path().join("read");
        let write_root = temp.path().join("write");
        std::fs::create_dir_all(&read_root).unwrap();
        std::fs::create_dir_all(&write_root).unwrap();
        let read_roots = vec![read_root.to_string_lossy().into_owned()];
        let write_roots = vec![write_root.to_string_lossy().into_owned()];
        let policy = AllowConfig::from_settings(
            temp.path(),
            None,
            Some(&read_roots),
            Some(&write_roots),
            false,
            false,
        );

        expect_allowed(policy.authorize_read(&read_root));
        expect_denied(
            policy.authorize_write(&read_root.join("new.txt")),
            AllowPolicyReason::OutsideConfiguredRoots(FileAccess::Write),
        );
        expect_allowed(policy.authorize_write(&write_root.join("new.txt")));
        expect_denied(
            policy.authorize_read(&write_root),
            AllowPolicyReason::OutsideConfiguredRoots(FileAccess::Read),
        );
    }

    #[test]
    fn js_file_allow_policy_nonexistent_write_uses_nearest_canonical_parent() {
        let temp = TempDir::new();
        let safe = temp.path().join("safe");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&safe).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let roots = vec![safe.to_string_lossy().into_owned()];
        let policy =
            AllowConfig::from_settings(temp.path(), None, None, Some(&roots), false, false);

        assert_eq!(
            expect_allowed(policy.authorize_write(&safe.join("missing/child/file.txt"))),
            safe.canonicalize().unwrap().join("missing/child/file.txt")
        );
        expect_denied(
            policy.authorize_write(&safe.join("../outside/escaped.txt")),
            AllowPolicyReason::OutsideConfiguredRoots(FileAccess::Write),
        );
    }

    #[test]
    fn js_file_allow_policy_empty_malformed_and_ambiguous_settings_deny() {
        let temp = TempDir::new();
        let empty = Vec::new();
        let empty_policy =
            AllowConfig::from_settings(temp.path(), None, Some(&empty), Some(&empty), false, false);
        expect_denied(
            empty_policy.authorize_read(temp.path()),
            AllowPolicyReason::NoConfiguredRoots(FileAccess::Read),
        );

        let malformed = vec!["missing-root".to_string()];
        let malformed_policy = AllowConfig::from_settings(
            temp.path(),
            None,
            Some(&malformed),
            Some(&malformed),
            false,
            false,
        );
        expect_denied(
            malformed_policy.authorize_read(temp.path()),
            AllowPolicyReason::InvalidConfiguration(FileAccess::Read),
        );

        let valid = vec![temp.path().to_string_lossy().into_owned()];
        let ambiguous =
            AllowConfig::from_settings(temp.path(), None, Some(&valid), Some(&valid), true, true);
        expect_denied(
            ambiguous.authorize_read(temp.path()),
            AllowPolicyReason::InvalidConfiguration(FileAccess::Read),
        );
    }

    #[test]
    fn js_file_allow_policy_requires_explicit_unrestricted_opt_in() {
        let temp = TempDir::new();
        let denied = AllowConfig::from_settings(temp.path(), None, None, None, false, false);
        expect_denied(
            denied.authorize_read(temp.path()),
            AllowPolicyReason::NoConfiguredRoots(FileAccess::Read),
        );

        let unrestricted = AllowConfig::unrestricted(temp.path());
        expect_allowed(unrestricted.authorize_read(temp.path()));
        expect_allowed(unrestricted.authorize_write(&temp.path().join("missing/file.txt")));
    }

    #[cfg(unix)]
    #[test]
    fn js_file_allow_policy_defines_symlink_behavior() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new();
        let safe = temp.path().join("safe");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&safe).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let inside_file = safe.join("inside.txt");
        let outside_file = outside.join("outside.txt");
        std::fs::write(&inside_file, "inside").unwrap();
        std::fs::write(&outside_file, "outside").unwrap();
        let inside_link = safe.join("inside-link");
        let escape_link = safe.join("escape-link");
        let dangling_link = safe.join("dangling-link");
        let parent_link = safe.join("parent-link");
        symlink(&inside_file, &inside_link).unwrap();
        symlink(&outside_file, &escape_link).unwrap();
        symlink(safe.join("missing"), &dangling_link).unwrap();
        symlink(&outside, &parent_link).unwrap();
        let roots = vec![safe.to_string_lossy().into_owned()];
        let policy =
            AllowConfig::from_settings(temp.path(), None, Some(&roots), Some(&roots), false, false);

        assert_eq!(
            expect_allowed(policy.authorize_read(&inside_link)),
            inside_file.canonicalize().unwrap()
        );
        expect_denied(
            policy.authorize_read(&escape_link),
            AllowPolicyReason::OutsideConfiguredRoots(FileAccess::Read),
        );
        expect_denied(
            policy.authorize_read(&dangling_link),
            AllowPolicyReason::InvalidTarget(FileAccess::Read),
        );
        expect_denied(
            policy.authorize_write(&inside_link),
            AllowPolicyReason::AmbiguousSymlink(FileAccess::Write),
        );
        expect_denied(
            policy.authorize_write(&parent_link.join("new.txt")),
            AllowPolicyReason::OutsideConfiguredRoots(FileAccess::Write),
        );
    }

    #[cfg(unix)]
    #[test]
    fn js_file_allow_policy_rejects_alternate_separator_spelling() {
        let temp = TempDir::new();
        let roots = vec![temp.path().to_string_lossy().into_owned()];
        let policy =
            AllowConfig::from_settings(temp.path(), None, Some(&roots), Some(&roots), false, false);

        expect_denied(
            policy.authorize_write(Path::new(r"safe\..\outside.txt")),
            AllowPolicyReason::InvalidTarget(FileAccess::Write),
        );
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
            AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
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
        call_read_file_with_policy(
            permission,
            ask_tx,
            path,
            AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
        )
        .await
    }

    async fn call_read_file_with_policy(
        permission: PermCheck,
        ask_tx: Option<AskSender>,
        path: PathBuf,
        allow_config: AllowConfig,
    ) -> Result<String, String> {
        let runtime = tokio::runtime::Handle::current();
        let owner = PermissionBridgeOwner::new(Some(permission), ask_tx, STEP_TIMEOUT);
        let bridge = owner.bridge();
        tokio::task::spawn_blocking(move || {
            let _owner = owner;
            make_read_file(bridge, runtime, allow_config)(path.to_string_lossy().into_owned())
                .map_err(|error| error.to_string())
        })
        .await
        .expect("read_file test task panicked")
    }

    #[tokio::test]
    async fn js_file_allow_policy_denies_before_mandatory_permission() {
        let temp = TempDir::new();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let source = workspace.join("source.txt");
        std::fs::write(&source, "must not be read").unwrap();
        let policy = AllowConfig::from_settings(&workspace, None, None, None, false, false);
        let permission = host_permission(workspace, Action::Ask, Action::Allow);
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);

        let error = call_read_file_with_policy(permission, Some(ask_tx), source, policy)
            .await
            .expect_err("empty read policy must deny");

        assert!(error.contains("JS file read denied: no roots are configured"));
        assert!(
            ask_rx.try_recv().is_err(),
            "policy denial reached the permission service"
        );
    }

    async fn call_write_file(
        permission: PermCheck,
        ask_tx: Option<AskSender>,
        path: PathBuf,
        content: &'static str,
    ) -> Result<(), String> {
        call_write_file_with_policy(
            permission,
            ask_tx,
            path,
            content,
            AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
        )
        .await
    }

    async fn call_write_file_with_policy(
        permission: PermCheck,
        ask_tx: Option<AskSender>,
        path: PathBuf,
        content: &'static str,
        allow_config: AllowConfig,
    ) -> Result<(), String> {
        let runtime = tokio::runtime::Handle::current();
        let owner = PermissionBridgeOwner::new(Some(permission), ask_tx, STEP_TIMEOUT);
        let bridge = owner.bridge();
        tokio::task::spawn_blocking(move || {
            let _owner = owner;
            make_write_file(bridge, runtime, allow_config)(
                path.to_string_lossy().into_owned(),
                content.to_string(),
            )
            .map_err(|error| error.to_string())
        })
        .await
        .expect("write_file test task panicked")
    }

    #[tokio::test]
    async fn js_file_allow_enforcement_narrows_allowed_permissions_for_reads_and_writes() {
        let temp = TempDir::new();
        let allowed_root = temp.path().join("safe");
        let sibling_root = temp.path().join("safe-evil");
        std::fs::create_dir_all(&allowed_root).unwrap();
        std::fs::create_dir_all(&sibling_root).unwrap();
        let allowed_source = allowed_root.join("source.txt");
        let denied_source = sibling_root.join("source.txt");
        std::fs::write(&allowed_source, "allowed").unwrap();
        std::fs::write(&denied_source, "must stay private").unwrap();
        let roots = vec![allowed_root.to_string_lossy().into_owned()];
        let policy =
            AllowConfig::from_settings(temp.path(), None, Some(&roots), Some(&roots), false, false);
        let permission = host_permission(temp.path().to_path_buf(), Action::Allow, Action::Allow);

        assert_eq!(
            call_read_file_with_policy(permission.clone(), None, allowed_source, policy.clone(),)
                .await
                .expect("in-root read should succeed"),
            "allowed"
        );
        let read_error =
            call_read_file_with_policy(permission.clone(), None, denied_source, policy.clone())
                .await
                .expect_err("an allowed permission must not override the read roots");
        assert!(
            read_error.contains("outside the configured roots"),
            "unexpected read policy error: {read_error}"
        );

        let allowed_target = allowed_root.join("created.txt");
        call_write_file_with_policy(
            permission.clone(),
            None,
            allowed_target.clone(),
            "created safely",
            policy.clone(),
        )
        .await
        .expect("in-root write should succeed");
        assert_eq!(
            std::fs::read_to_string(allowed_target).unwrap(),
            "created safely"
        );

        let denied_target = sibling_root.join("must-not-exist.txt");
        let write_error = call_write_file_with_policy(
            permission,
            None,
            denied_target.clone(),
            "must not escape",
            policy,
        )
        .await
        .expect_err("an allowed permission must not override the write roots");
        assert!(
            write_error.contains("outside the configured roots"),
            "unexpected write policy error: {write_error}"
        );
        assert!(
            !denied_target.exists(),
            "policy-denied write created an out-of-root file"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn js_file_allow_enforcement_revalidates_after_permission_wait() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new();
        let allowed_root = temp.path().join("safe");
        let write_parent = allowed_root.join("write-parent");
        let outside_root = temp.path().join("outside");
        std::fs::create_dir_all(&write_parent).unwrap();
        std::fs::create_dir_all(&outside_root).unwrap();
        let source = allowed_root.join("source.txt");
        let original_source = allowed_root.join("original-source.txt");
        let outside_source = outside_root.join("outside.txt");
        std::fs::write(&source, "approved identity").unwrap();
        std::fs::write(&outside_source, "must not be read").unwrap();
        let roots = vec![allowed_root.to_string_lossy().into_owned()];
        let policy =
            AllowConfig::from_settings(temp.path(), None, Some(&roots), Some(&roots), false, false);
        let permission = host_permission(temp.path().to_path_buf(), Action::Ask, Action::Allow);
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);

        let read = tokio::spawn(call_read_file_with_policy(
            permission.clone(),
            Some(ask_tx.clone()),
            source.clone(),
            policy.clone(),
        ));
        let request = ask_rx.recv().await.expect("read should request permission");
        std::fs::rename(&source, &original_source).unwrap();
        symlink(&outside_source, &source).unwrap();
        request
            .reply
            .send(UserDecision::AllowOnce)
            .expect("read permission request receiver dropped");
        let read_error = read
            .await
            .expect("read task panicked")
            .expect_err("swapped read target must fail");
        assert!(
            read_error.contains("Path changed after permission check"),
            "unexpected read swap error: {read_error}"
        );

        let target = write_parent.join("created.txt");
        let original_parent = allowed_root.join("original-write-parent");
        let outside_target = outside_root.join("created.txt");
        let write = tokio::spawn(call_write_file_with_policy(
            permission,
            Some(ask_tx),
            target,
            "must not escape",
            policy,
        ));
        let request = ask_rx
            .recv()
            .await
            .expect("write should request permission");
        std::fs::rename(&write_parent, &original_parent).unwrap();
        symlink(&outside_root, &write_parent).unwrap();
        request
            .reply
            .send(UserDecision::AllowOnce)
            .expect("write permission request receiver dropped");
        let write_error = write
            .await
            .expect("write task panicked")
            .expect_err("swapped write parent must fail");
        assert!(
            !write_error.is_empty(),
            "swapped write parent returned an empty error"
        );
        assert!(
            !outside_target.exists(),
            "swapped parent redirected the policy-approved write"
        );
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
        let allow_config = AllowConfig::unrestricted(&std::env::current_dir().unwrap());

        let error = tokio::task::spawn_blocking(move || {
            let _owner = owner;
            make_write_file(bridge, runtime, allow_config)(
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
