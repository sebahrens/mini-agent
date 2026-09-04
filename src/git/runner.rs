//! Identity-pinned, workspace-aware Git process boundary.
//!
//! The runner never invokes a shell. Model-visible calls use
//! [`Sandbox::wrap_workspace_service`] with network denial and a complete,
//! non-credential environment. Internal worktree calls retain their existing
//! direct-process behavior but share executable pinning, environment
//! hardening, bounded output, deadlines, and child cleanup.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::sandbox::{
    CommandLimits, CommandOutput, CommandStatus, Sandbox, configure_child_lifetime,
};

pub(crate) const QUERY_LIMITS: CommandLimits = CommandLimits {
    timeout: Duration::from_secs(10),
    stdout_bytes: 256 * 1024,
    stderr_bytes: 256 * 1024,
    combined_bytes: 384 * 1024,
};
pub(crate) const LOCAL_MUTATION_LIMITS: CommandLimits = CommandLimits {
    timeout: Duration::from_secs(60),
    stdout_bytes: 512 * 1024,
    stderr_bytes: 512 * 1024,
    combined_bytes: 768 * 1024,
};
pub(crate) const NETWORK_LIMITS: CommandLimits = CommandLimits {
    timeout: Duration::from_secs(120),
    stdout_bytes: 512 * 1024,
    stderr_bytes: 512 * 1024,
    combined_bytes: 768 * 1024,
};

const REDIRECTING_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_CONFIG",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_ATTR_NOSYSTEM",
];

static PROCESS_GIT_MUTATION_LOCKS: OnceLock<StdMutex<HashMap<PathBuf, Weak<Mutex<()>>>>> =
    OnceLock::new();

/// Cached git environment variables. Built once per process lifetime.
/// Assumes PATH and relevant environment variables do not change mid-session.
static CACHED_GIT_ENVIRONMENT: OnceLock<Vec<(OsString, OsString)>> = OnceLock::new();

/// Checked runner cached for the production process lifetime. Tests discover
/// from their scoped PATH because process environment fixtures can run
/// concurrently with unrelated tests. Every launch still revalidates identity.
#[cfg(not(test))]
static CACHED_GIT_RUNNER: OnceLock<Result<GitRunner, String>> = OnceLock::new();

/// Git's own index locks protect files. A process-local, canonical common-dir
/// lock additionally keeps mini-agent worktree and structured-tool mutations
/// for the same repository from racing between before/after snapshots without
/// serializing independent repositories.
fn repository_mutation_lock(repository_key: &Path) -> Arc<Mutex<()>> {
    let locks = PROCESS_GIT_MUTATION_LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(repository_key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(repository_key.to_path_buf(), Arc::downgrade(&lock));
    lock
}

#[derive(Clone)]
pub(crate) struct GitRunner {
    program: Arc<PathBuf>,
    identity: Arc<crate::fs::CheckedMetadata>,
}

impl Default for GitRunner {
    fn default() -> Self {
        Self::discover()
            .or_else(|_| Self::unavailable())
            .expect("Git runner: process executable identity unavailable")
    }
}

impl GitRunner {
    pub(crate) fn discover() -> Result<Self, String> {
        #[cfg(not(test))]
        {
            Self::discover_cached(&CACHED_GIT_RUNNER)
        }
        #[cfg(test)]
        {
            Self::discover_uncached(std::env::var_os("PATH").as_deref())
        }
    }

    fn discover_cached(cache: &OnceLock<Result<Self, String>>) -> Result<Self, String> {
        cache
            .get_or_init(|| Self::discover_uncached(std::env::var_os("PATH").as_deref()))
            .clone()
    }

    fn discover_uncached(path: Option<&OsStr>) -> Result<Self, String> {
        let program = resolve_git_executable(path)
            .ok_or_else(|| "Git executable is unavailable or unsupported".to_string())?;
        let identity = crate::fs::checked_path_metadata(&program)
            .map_err(|_| "Git executable identity is unavailable".to_string())?;
        if !identity.is_file() {
            return Err("Git executable is not a regular file".to_string());
        }
        Ok(Self {
            program: Arc::new(program),
            identity: Arc::new(identity),
        })
    }

    fn unavailable() -> Result<Self, String> {
        let program = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
        let identity = crate::fs::checked_path_metadata(&program)
            .map_err(|e| format!("process executable identity unavailable: {e}"))?;
        Ok(Self {
            program: Arc::new(PathBuf::new()),
            identity: Arc::new(identity),
        })
    }

    pub(crate) fn executable(&self) -> Option<&Path> {
        (!self.program.as_os_str().is_empty()).then_some(self.program.as_path())
    }

    pub(crate) fn verify_contained(
        &self,
        workspace: &crate::paths::WorkspaceBinding,
        sandbox: &Sandbox,
    ) -> Result<(), String> {
        self.validate()?;
        sandbox.verify_workspace_service_capability(
            self.program.as_path(),
            &["--version".to_string()],
            workspace,
        )
    }

    fn validate(&self) -> Result<(), String> {
        if self.program.as_os_str().is_empty() {
            return Err("Git executable is unavailable or unsupported".to_string());
        }
        let current = crate::fs::checked_path_metadata(self.program.as_path())
            .map_err(|_| "Git executable identity changed before launch".to_string())?;
        crate::fs::ensure_same_file(self.program.as_path(), &self.identity, &current)
            .map_err(|_| "Git executable identity changed before launch".to_string())
    }

    fn argv<I, S>(&self, repo_path: &Path, args: I) -> Vec<OsString>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut argv = vec![OsString::from("-C"), repo_path.as_os_str().to_owned()];
        argv.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        argv
    }

    fn internal_command<I, S>(&self, repo_path: &Path, args: I) -> Result<Command, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.validate()?;
        let mut command = Command::new(self.program.as_path());
        command.args(self.argv(repo_path, args));
        for name in REDIRECTING_ENV {
            command.env_remove(name);
        }
        apply_git_environment(&mut command);
        configure_child_lifetime(&mut command);
        Ok(command)
    }

    fn contained_command<I, S>(
        &self,
        workspace: &crate::paths::WorkspaceBinding,
        sandbox: &Sandbox,
        args: I,
    ) -> Result<Command, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.contained_command_with_env(workspace, sandbox, args, &[])
    }

    /// Like [`Self::contained_command`] but with additional environment
    /// entries appended to the hardened, non-credential base environment.
    fn contained_command_with_env<I, S>(
        &self,
        workspace: &crate::paths::WorkspaceBinding,
        sandbox: &Sandbox,
        args: I,
        extra_env: &[(OsString, OsString)],
    ) -> Result<Command, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.validate()?;
        workspace.validate()?;
        let argv = self
            .argv(workspace.root(), args)
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let mut env = git_environment().to_vec();
        env.extend_from_slice(extra_env);
        sandbox.wrap_workspace_service(self.program.as_path(), &argv, workspace.root(), &env, true)
    }

    /// Resolves the commit author/committer identity on the host.
    ///
    /// The contained environment deliberately carries no `HOME`, XDG, or
    /// global/system config access, so `git commit` inside the sandbox cannot
    /// discover `user.name` / `user.email` on its own. The identity is
    /// resolved here with uncontained `git config --get` calls (honouring
    /// `GIT_AUTHOR_*` / `GIT_COMMITTER_*` overrides from the parent process
    /// environment) and injected as explicit values.
    pub(crate) async fn resolve_commit_identity(
        &self,
        repo_path: &Path,
    ) -> Result<CommitIdentity, String> {
        self.resolve_commit_identity_with(repo_path, |name| std::env::var_os(name), &[])
            .await
    }

    /// Identity resolution with an explicit environment lookup and extra
    /// environment for the host `git config` query. `env` supplies the
    /// `GIT_AUTHOR_*` / `GIT_COMMITTER_*` overrides; `config_env` lets tests
    /// isolate the lookup from the developer's global config.
    pub(crate) async fn resolve_commit_identity_with(
        &self,
        repo_path: &Path,
        env: impl Fn(&str) -> Option<OsString>,
        config_env: &[(String, OsString)],
    ) -> Result<CommitIdentity, String> {
        let override_for = |name: &str| {
            env(name)
                .and_then(|value| value.into_string().ok())
                .and_then(|value| identity_value(&value))
        };
        let author_name = override_for("GIT_AUTHOR_NAME");
        let author_email = override_for("GIT_AUTHOR_EMAIL");
        let committer_name = override_for("GIT_COMMITTER_NAME");
        let committer_email = override_for("GIT_COMMITTER_EMAIL");

        let user_name = if author_name.is_none() || committer_name.is_none() {
            self.host_config_value(repo_path, "user.name", config_env)
                .await?
        } else {
            None
        };
        let user_email = if author_email.is_none() || committer_email.is_none() {
            self.host_config_value(repo_path, "user.email", config_env)
                .await?
        } else {
            None
        };

        let resolve = |explicit: Option<String>, configured: &Option<String>| {
            explicit
                .or_else(|| configured.clone())
                .ok_or_else(|| COMMIT_IDENTITY_UNRESOLVED.to_string())
        };
        Ok(CommitIdentity {
            author_name: resolve(author_name, &user_name)?,
            author_email: resolve(author_email, &user_email)?,
            committer_name: resolve(committer_name, &user_name)?,
            committer_email: resolve(committer_email, &user_email)?,
        })
    }

    /// Reads one config key with an uncontained `git config --get` in the
    /// repository. Returns `Ok(None)` when the key is unset.
    async fn host_config_value(
        &self,
        repo_path: &Path,
        key: &str,
        config_env: &[(String, OsString)],
    ) -> Result<Option<String>, String> {
        let mut command = self.internal_command(repo_path, ["config", "--get", key])?;
        for (name, value) in config_env {
            command.env(name, value);
        }
        let output = Sandbox::new(false, "git")
            .output_built_command_with_limits(command, QUERY_LIMITS)
            .await
            .map_err(|_| "git config runner failed".to_string())?;
        if output.status == CommandStatus::Completed {
            match output.exit_status.and_then(|status| status.code()) {
                Some(0) => {
                    return Ok(identity_value(&String::from_utf8_lossy(&output.stdout)));
                }
                // `git config --get` exits 1 when the key is absent.
                Some(1) => return Ok(None),
                _ => {}
            }
        }
        command_result("config", QUERY_LIMITS, output).map(|_| None)
    }

    pub(crate) async fn run<I, S>(
        &self,
        repo_path: &Path,
        operation: &'static str,
        args: I,
        limits: CommandLimits,
    ) -> Result<CommandOutput, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let command = self.internal_command(repo_path, args)?;
        let output = Sandbox::new(false, "git")
            .output_built_command_with_limits(command, limits)
            .await
            .map_err(|_| format!("git {operation} runner failed"))?;
        command_result(operation, limits, output)
    }

    pub(crate) async fn acquire_mutation(
        &self,
        repo_path: &Path,
    ) -> Result<OwnedMutexGuard<()>, String> {
        let output = self
            .run(
                repo_path,
                "repository-identity",
                ["rev-parse", "--path-format=absolute", "--git-common-dir"],
                QUERY_LIMITS,
            )
            .await
            .map_err(|error| format!("cannot establish repository identity: {error}"))?;
        let key = output_path(&output.stdout)
            .canonicalize()
            .map_err(|error| format!("failed to resolve common Git directory: {error}"))?;
        Ok(repository_mutation_lock(&key).lock_owned().await)
    }

    pub(crate) async fn run_with_input<I, S>(
        &self,
        repo_path: &Path,
        operation: &'static str,
        args: I,
        input: Vec<u8>,
        limits: CommandLimits,
    ) -> Result<CommandOutput, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let command = self.internal_command(repo_path, args)?;
        let output = Sandbox::new(false, "git")
            .output_built_command_with_input_and_limits(command, input, limits)
            .await
            .map_err(|_| format!("git {operation} runner failed"))?;
        command_result(operation, limits, output)
    }

    pub(crate) async fn run_allow_exit<I, S>(
        &self,
        repo_path: &Path,
        operation: &'static str,
        args: I,
        limits: CommandLimits,
    ) -> Result<CommandOutput, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let command = self.internal_command(repo_path, args)?;
        let output = Sandbox::new(false, "git")
            .output_built_command_with_limits(command, limits)
            .await
            .map_err(|_| format!("git {operation} runner failed"))?;
        if output.status == CommandStatus::Completed && output.exit_status.is_some() {
            Ok(output)
        } else {
            command_result(operation, limits, output)
        }
    }

    /// Runs a local mutation and returns every observed terminal outcome.
    ///
    /// Callers use this only when they must take a post-operation snapshot
    /// after a timeout, cancellation, output limit, or non-zero exit.
    #[cfg(test)]
    pub(crate) async fn run_observed<I, S>(
        &self,
        repo_path: &Path,
        operation: &'static str,
        args: I,
        limits: CommandLimits,
    ) -> Result<CommandOutput, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let command = self.internal_command(repo_path, args)?;
        Sandbox::new(false, "git")
            .output_built_command_with_limits(command, limits)
            .await
            .map_err(|_| format!("git {operation} runner failed"))
    }

    #[cfg(test)]
    pub(crate) async fn run_with_input_observed<I, S>(
        &self,
        repo_path: &Path,
        operation: &'static str,
        args: I,
        input: Vec<u8>,
        limits: CommandLimits,
    ) -> Result<CommandOutput, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let command = self.internal_command(repo_path, args)?;
        Sandbox::new(false, "git")
            .output_built_command_with_input_and_limits(command, input, limits)
            .await
            .map_err(|_| format!("git {operation} runner failed"))
    }

    pub(crate) async fn run_contained<I, S>(
        &self,
        workspace: &crate::paths::WorkspaceBinding,
        sandbox: &Sandbox,
        operation: &'static str,
        args: I,
        limits: CommandLimits,
        allow_nonzero_or_truncated: bool,
    ) -> Result<CommandOutput, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let command = self.contained_command(workspace, sandbox, args)?;
        let output = sandbox
            .output_built_command_with_limits(command, limits)
            .await
            .map_err(|_| format!("git {operation} runner failed"))?;
        if allow_nonzero_or_truncated
            && (matches!(output.status, CommandStatus::OutputLimitExceeded(_))
                || output.status == CommandStatus::Completed)
        {
            return Ok(output);
        }
        command_result(operation, limits, output)
    }

    /// Runs a contained mutation and returns every observed terminal outcome
    /// so the caller can capture the truthful post-operation repository state.
    pub(crate) async fn run_contained_observed<I, S>(
        &self,
        workspace: &crate::paths::WorkspaceBinding,
        sandbox: &Sandbox,
        operation: &'static str,
        args: I,
        limits: CommandLimits,
    ) -> Result<CommandOutput, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let command = self.contained_command(workspace, sandbox, args)?;
        sandbox
            .output_built_command_with_limits(command, limits)
            .await
            .map_err(|_| format!("git {operation} runner failed"))
    }

    pub(crate) async fn run_contained_with_input_observed<I, S>(
        &self,
        workspace: &crate::paths::WorkspaceBinding,
        sandbox: &Sandbox,
        operation: &'static str,
        args: I,
        input: Vec<u8>,
        limits: CommandLimits,
    ) -> Result<CommandOutput, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        // A contained commit has no HOME or global config to learn its
        // author from; resolve the identity on the host first and fail
        // before anything is spawned when it cannot be resolved.
        let extra_env = if operation == "commit" {
            let identity = self.resolve_commit_identity(workspace.root()).await?;
            identity.environment()
        } else {
            Vec::new()
        };
        let command = self.contained_command_with_env(workspace, sandbox, args, &extra_env)?;
        sandbox
            .output_built_command_with_input_and_limits(command, input, limits)
            .await
            .map_err(|_| format!("git {operation} runner failed"))
    }
}

/// Author and committer identity resolved on the host for a contained commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommitIdentity {
    pub(crate) author_name: String,
    pub(crate) author_email: String,
    pub(crate) committer_name: String,
    pub(crate) committer_email: String,
}

impl CommitIdentity {
    /// Environment entries that hand the identity to `git commit` without
    /// any config-file access inside the sandbox.
    pub(crate) fn environment(&self) -> Vec<(OsString, OsString)> {
        vec![
            (
                OsString::from("GIT_AUTHOR_NAME"),
                OsString::from(&self.author_name),
            ),
            (
                OsString::from("GIT_AUTHOR_EMAIL"),
                OsString::from(&self.author_email),
            ),
            (
                OsString::from("GIT_COMMITTER_NAME"),
                OsString::from(&self.committer_name),
            ),
            (
                OsString::from("GIT_COMMITTER_EMAIL"),
                OsString::from(&self.committer_email),
            ),
        ]
    }
}

pub(crate) const COMMIT_IDENTITY_UNRESOLVED: &str = "git commit requires an author identity: \
the contained git process has no HOME or global config, so set user.name and user.email in \
this repository (`git config user.name ...` / `git config user.email ...`) or export \
GIT_AUTHOR_NAME/GIT_AUTHOR_EMAIL (and GIT_COMMITTER_NAME/GIT_COMMITTER_EMAIL) before starting";

/// The complete environment a contained commit runs with: the hardened base
/// plus the resolved identity, and nothing else.
#[cfg(test)]
pub(crate) fn contained_commit_environment(identity: &CommitIdentity) -> Vec<(OsString, OsString)> {
    let mut env = git_environment().to_vec();
    env.extend(identity.environment());
    env
}

/// Normalises a config/env identity value: trimmed, non-empty, single line.
fn identity_value(raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.is_empty() || value.contains(['\0', '\n', '\r']) {
        None
    } else {
        Some(value.to_owned())
    }
}

fn output_path(bytes: &[u8]) -> PathBuf {
    let mut end = bytes.len();
    while end > 0 && matches!(bytes[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(OsString::from_vec(bytes[..end].to_vec()))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(&bytes[..end]).into_owned())
    }
}

fn apply_git_environment(command: &mut Command) {
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_PAGER", "cat")
        .env("GIT_EXTERNAL_DIFF", "")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1");
}

fn build_git_environment() -> Vec<(OsString, OsString)> {
    let values = vec![
        (OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("0")),
        (OsString::from("GIT_NO_LAZY_FETCH"), OsString::from("1")),
        (OsString::from("GIT_PAGER"), OsString::from("cat")),
        (OsString::from("GIT_EXTERNAL_DIFF"), OsString::new()),
        (OsString::from("GIT_LITERAL_PATHSPECS"), OsString::from("1")),
        (OsString::from("GIT_CONFIG_NOSYSTEM"), OsString::from("1")),
    ];
    #[cfg(windows)]
    {
        let mut values = values;
        for name in ["SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(name) {
                values.push((OsString::from(name), value));
            }
        }
        values
    }
    #[cfg(not(windows))]
    values
}

/// Returns cached git environment variables. On first call, constructs the
/// environment vector once and caches it; subsequent calls return a borrowed
/// reference to the cached vector at no cost.
///
/// Assumption: PATH and environment variables do not change mid-session.
fn git_environment() -> &'static [(OsString, OsString)] {
    CACHED_GIT_ENVIRONMENT.get_or_init(build_git_environment)
}

pub(crate) fn command_result(
    operation: &str,
    limits: CommandLimits,
    output: CommandOutput,
) -> Result<CommandOutput, String> {
    match output.status {
        CommandStatus::Completed if output.exit_status.is_some_and(|status| status.success()) => {
            Ok(output)
        }
        CommandStatus::Completed | CommandStatus::Failed => {
            Err(command_failure(operation, &output))
        }
        CommandStatus::TimedOut => Err(format!(
            "git {operation} timed out after {}ms",
            limits.timeout.as_millis()
        )),
        CommandStatus::Cancelled => Err(format!("git {operation} was cancelled")),
        CommandStatus::OutputLimitExceeded(limit) => Err(format!(
            "git {operation} exceeded bounded output limit ({limit:?})"
        )),
    }
}

pub(crate) fn command_failure(operation: &str, output: &CommandOutput) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        format!("git {operation} failed")
    } else {
        format!("git {operation} failed: {stderr}")
    }
}

fn find_git_executable(path: Option<&OsStr>) -> Option<PathBuf> {
    let path = path?;
    let names: &[&str] = if cfg!(windows) {
        &["git.exe"]
    } else {
        &["git"]
    };
    for directory in std::env::split_paths(path) {
        for name in names {
            let candidate = directory.join(name);
            let Ok(candidate) = candidate.canonicalize() else {
                continue;
            };
            let Ok(metadata) = std::fs::metadata(&candidate) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 == 0 {
                    continue;
                }
            }
            return Some(candidate);
        }
    }
    None
}

fn resolve_git_executable(path: Option<&OsStr>) -> Option<PathBuf> {
    find_git_executable(path)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::sandbox::{CommandLimits, CommandOutput, CommandOutputLimit, CommandStatus};

    fn limits() -> CommandLimits {
        CommandLimits {
            timeout: Duration::from_secs(5),
            stdout_bytes: 1024,
            stderr_bytes: 1024,
            combined_bytes: 2048,
        }
    }

    fn make_output(status: CommandStatus, stderr: &[u8]) -> CommandOutput {
        CommandOutput {
            exit_status: None,
            stdout: vec![],
            stderr: stderr.to_vec(),
            status,
        }
    }

    #[test]
    fn command_failure_empty_stderr() {
        let out = make_output(CommandStatus::Failed, b"");
        assert_eq!(command_failure("log", &out), "git log failed");
    }

    #[test]
    fn repeated_discovery_reuses_the_cached_runner_identity() {
        let cache = OnceLock::new();
        let first =
            GitRunner::discover_cached(&cache).expect("git is available for repository tests");
        let second = GitRunner::discover_cached(&cache).expect("cached git remains available");
        assert!(
            Arc::ptr_eq(&first.identity, &second.identity),
            "discovery should reuse the checked executable identity, not only its path"
        );
    }

    #[test]
    fn command_failure_strips_whitespace_from_stderr() {
        let out = make_output(CommandStatus::Failed, b"  not a repository  \n");
        assert_eq!(
            command_failure("log", &out),
            "git log failed: not a repository"
        );
    }

    #[test]
    fn command_result_timed_out() {
        let err = command_result("fetch", limits(), make_output(CommandStatus::TimedOut, b""))
            .err()
            .expect("expected Err");
        assert!(err.contains("timed out"), "unexpected: {err}");
        assert!(err.contains("fetch"), "unexpected: {err}");
    }

    #[test]
    fn command_result_cancelled() {
        let err = command_result(
            "fetch",
            limits(),
            make_output(CommandStatus::Cancelled, b""),
        )
        .err()
        .expect("expected Err");
        assert!(err.contains("cancelled"), "unexpected: {err}");
    }

    #[test]
    fn command_result_output_limit_exceeded() {
        let err = command_result(
            "log",
            limits(),
            make_output(
                CommandStatus::OutputLimitExceeded(CommandOutputLimit::Stdout),
                b"",
            ),
        )
        .err()
        .expect("expected Err");
        assert!(
            err.contains("exceeded bounded output limit"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn command_result_failed_includes_stderr() {
        let err = command_result(
            "push",
            limits(),
            make_output(CommandStatus::Failed, b"access denied"),
        )
        .err()
        .expect("expected Err");
        assert!(err.contains("access denied"), "unexpected: {err}");
    }

    #[test]
    fn command_result_completed_without_exit_status_is_error() {
        // Completed + no exit_status: is_some_and returns false, falls to failure arm
        assert!(
            command_result(
                "status",
                limits(),
                make_output(CommandStatus::Completed, b"")
            )
            .is_err()
        );
    }

    #[test]
    fn unavailable_runner_executable_is_none() {
        let runner = GitRunner::unavailable()
            .expect("unavailable() must succeed in a normal test environment");
        assert!(runner.executable().is_none());
    }

    #[test]
    fn git_environment_contains_required_keys() {
        let env = git_environment();
        let keys: Vec<String> = env
            .iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        for required in &[
            "GIT_TERMINAL_PROMPT",
            "GIT_NO_LAZY_FETCH",
            "GIT_PAGER",
            "GIT_EXTERNAL_DIFF",
            "GIT_LITERAL_PATHSPECS",
            "GIT_CONFIG_NOSYSTEM",
        ] {
            assert!(
                keys.contains(&(*required).to_string()),
                "missing key {required} in git_environment"
            );
        }
    }

    #[test]
    fn git_environment_is_cached_and_not_reallocated() {
        // Fails if the OnceLock cache is removed: two calls would then return
        // distinct allocations rather than the same borrowed slice.
        let first = git_environment();
        let second = git_environment();
        assert!(
            std::ptr::eq(first, second),
            "git_environment must hand back the same cached slice on every call"
        );
    }

    #[test]
    fn cached_git_environment_matches_freshly_built() {
        // The cache must not change what git actually receives.
        assert_eq!(
            git_environment(),
            build_git_environment().as_slice(),
            "cached git environment diverged from a freshly built one"
        );
    }
}
