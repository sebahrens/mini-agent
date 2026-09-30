//! Identity-pinned, workspace-aware Git process boundary.
//!
//! The runner never invokes a shell. Model-visible calls use
//! [`Sandbox::wrap_workspace_service`] with network denial and a complete,
//! non-credential environment. Internal worktree calls retain their existing
//! direct-process behavior but share executable pinning, environment
//! hardening, bounded output, deadlines, and child cleanup.

#[cfg(any(test, feature = "git-worktree"))]
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
#[cfg(any(test, feature = "git-worktree"))]
use std::sync::{Mutex as StdMutex, Weak};
use std::time::Duration;

use tokio::process::Command;
#[cfg(any(test, feature = "git-worktree"))]
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
#[cfg(feature = "git-worktree")]
pub(crate) const NETWORK_LIMITS: CommandLimits = CommandLimits {
    timeout: Duration::from_secs(120),
    stdout_bytes: 512 * 1024,
    stderr_bytes: 512 * 1024,
    combined_bytes: 768 * 1024,
};

#[cfg(any(test, feature = "git-worktree"))]
static PROCESS_GIT_MUTATION_LOCKS: OnceLock<StdMutex<HashMap<PathBuf, Weak<Mutex<()>>>>> =
    OnceLock::new();

#[cfg(all(test, feature = "git-worktree", unix))]
tokio::task_local! {
    /// First poll of actual mutation admission: resolved common directory and
    /// whether the lock was immediately acquired. Scoped to the test caller.
    pub(crate) static MUTATION_LOCK_OBSERVER:
        tokio::sync::mpsc::UnboundedSender<(PathBuf, bool)>;
}

/// Cached git environment variables. Built once per process lifetime.
/// Assumes PATH and relevant environment variables do not change mid-session.
#[cfg(any(test, feature = "git-worktree"))]
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
#[cfg(any(test, feature = "git-worktree"))]
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
    /// Whether internal commands also neutralise the executable
    /// configuration of repositories nested in the work tree (see
    /// [`GitRunner::probing_nested_repositories`]).
    nested_repositories: bool,
    #[cfg(all(test, unix))]
    home_override: Option<Arc<PathBuf>>,
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
            nested_repositories: false,
            #[cfg(all(test, unix))]
            home_override: None,
        })
    }

    fn unavailable() -> Result<Self, String> {
        let program = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
        let identity = crate::fs::checked_path_metadata(&program)
            .map_err(|e| format!("process executable identity unavailable: {e}"))?;
        Ok(Self {
            program: Arc::new(PathBuf::new()),
            identity: Arc::new(identity),
            nested_repositories: false,
            #[cfg(all(test, unix))]
            home_override: None,
        })
    }

    /// Also probe every repository nested in the work tree (populated
    /// gitlinks, recursively) before each internal command and override its
    /// repository-scoped filter/diff/merge commands. Workspace mutations
    /// (the worktree stash/commit/merge flows and the undo stash) opt in:
    /// they descend into nested repositories that the model may have
    /// created. Callers that pass `--ignore-submodules` never descend and
    /// skip the extra `ls-files` per command.
    pub(crate) fn probing_nested_repositories(mut self) -> Self {
        self.nested_repositories = true;
        self
    }

    #[cfg(all(test, unix))]
    pub(crate) fn with_home_for_test(mut self, home: &Path) -> Self {
        self.home_override = Some(Arc::new(home.to_path_buf()));
        self
    }

    #[cfg(feature = "git-worktree")]
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

    /// Builds a host-side (uncontained) Git command.
    ///
    /// The model can author everything under the workspace, including
    /// `.git/config`, `.git/hooks`, `.git/info/attributes`, and
    /// `.gitattributes`, so every internal command is hardened before launch:
    ///
    /// * the environment is cleared to the [`internal_git_environment`]
    ///   allow-list (no provider keys; SSH agent, askpass, and proxy variables
    ///   only for [`InternalProfile::Network`]);
    /// * a fixed configuration baseline disables fsmonitor, the untracked
    ///   cache, hooks, signing, external diff, submodule recursion, and the
    ///   `ext::` transport (and, outside network operations, credential
    ///   helpers, `core.sshCommand`, and `core.askPass`);
    /// * every command-executing key from repository-controlled configuration
    ///   (`local`/`worktree` and global includes sourced from writable roots)
    ///   is overridden with a trusted global value or non-executing default,
    ///   so repository filter, diff, and merge drivers, credential helpers,
    ///   and `core.sshCommand` never run. Keys Git resolves first-value-wins
    ///   (`remote.*.uploadpack`/`receivepack`, `core.gitProxy`) cannot be
    ///   overridden, so a repository value refuses network operations.
    ///
    /// Overrides travel through `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n`/
    /// `GIT_CONFIG_VALUE_n`, which have command-line precedence and keep keys
    /// and values separate (a subsection containing `=` cannot split a key).
    async fn internal_command<I, S>(
        &self,
        repo_path: &Path,
        operation: &str,
        args: I,
        profile: InternalProfile,
    ) -> Result<Command, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.validate()?;
        let args = args
            .into_iter()
            .map(|arg| arg.as_ref().to_os_string())
            .collect::<Vec<_>>();
        // `git init` must also work before there is a repository to ask for
        // its administration paths. Select this narrow case from the actual
        // argv, never from the caller's diagnostic operation label.
        let initializing =
            profile == InternalProfile::Local && args.first().is_some_and(|arg| arg == "init");
        #[cfg(test)]
        let repository_execution = repository_execution_allowed_for_test(repo_path);
        #[cfg(not(test))]
        let repository_execution = false;
        let mut config = hardened_config(profile, repository_execution);
        if !repository_execution {
            config.extend(
                self.repository_config_overrides(repo_path, operation, profile, initializing)
                    .await?,
            );
            if self.nested_repositories {
                let nested = self
                    .nested_repository_overrides(repo_path, operation, &config)
                    .await?;
                config.extend(nested);
            }
        }
        Ok(self.build_internal_command(repo_path, args, profile, &config))
    }

    fn build_internal_command<I, S>(
        &self,
        repo_path: &Path,
        args: I,
        profile: InternalProfile,
        config: &[(String, String)],
    ) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(self.program.as_path());
        command.args(self.argv(repo_path, args));
        command.env_clear();
        command.envs(internal_git_environment(profile, |name| {
            std::env::var_os(name)
        }));
        #[cfg(all(test, unix))]
        if let Some(home) = self.home_override.as_ref() {
            command.env("HOME", home.as_path());
            // The fixture's HOME must not mix with the developer's XDG config.
            command.env("XDG_CONFIG_HOME", home.join(".config"));
        }
        command.env("GIT_CONFIG_COUNT", config.len().to_string());
        for (index, (key, value)) in config.iter().enumerate() {
            command
                .env(format!("GIT_CONFIG_KEY_{index}"), key)
                .env(format!("GIT_CONFIG_VALUE_{index}"), value);
        }
        configure_child_lifetime(&mut command);
        command
    }

    /// Lists every command-executing key the configuration sets, with its
    /// scope, and returns the overrides that neutralise the repository-scoped
    /// ones. `git config` only reads configuration: it runs no hooks,
    /// fsmonitor, or drivers.
    async fn repository_config_overrides(
        &self,
        repo_path: &Path,
        operation: &str,
        profile: InternalProfile,
        initializing: bool,
    ) -> Result<Vec<(String, String)>, String> {
        let entries = self
            .executable_config_entries(repo_path, operation, initializing)
            .await?;
        untrusted_config_overrides(&entries, profile)
    }

    /// Every command-executing key the configuration visible from
    /// `repo_path` sets, with its scope.
    async fn executable_config_entries(
        &self,
        repo_path: &Path,
        operation: &str,
        initializing: bool,
    ) -> Result<Vec<ScopedConfigEntry>, String> {
        let command = self.build_internal_command(
            repo_path,
            [
                "config",
                "--show-scope",
                "--show-origin",
                "--null",
                "--list",
            ],
            InternalProfile::Local,
            &[],
        );
        let output = Sandbox::new(false, "git")
            .output_built_command_with_limits(command, QUERY_LIMITS)
            .await
            .map_err(|_| format!("git {operation} runner failed"))?;
        if output.status != CommandStatus::Completed {
            return command_result(operation, QUERY_LIMITS, output).map(|_| Vec::new());
        }
        match output.exit_status.and_then(|status| status.code()) {
            Some(0) => {
                let mut entries = parse_scoped_config(&output.stdout)
                    .map_err(|error| format!("refusing git {operation}: {error}"))?;
                let roots = self
                    .untrusted_config_roots(repo_path, operation, initializing)
                    .await?;
                for entry in &mut entries {
                    entry.trusted_origin =
                        origin_is_outside_untrusted_roots(&entry.origin, repo_path, &roots);
                }
                Ok(entries)
            }
            _ => Err(command_failure(operation, &output)),
        }
    }

    /// Repositories can be linked outside their work trees, and callers may
    /// point at a subdirectory. Resolve all three places a model can author
    /// Git metadata before trusting a nominally global included file.
    async fn untrusted_config_roots(
        &self,
        repo_path: &Path,
        operation: &str,
        initializing: bool,
    ) -> Result<Vec<PathBuf>, String> {
        let command = self.build_internal_command(
            repo_path,
            [
                "rev-parse",
                "--path-format=absolute",
                "--git-dir",
                "--git-common-dir",
                "--is-bare-repository",
            ],
            InternalProfile::Local,
            &hardened_config(InternalProfile::Local, false),
        );
        let output = Sandbox::new(false, "git")
            .output_built_command_with_limits(command, QUERY_LIMITS)
            .await
            .map_err(|_| format!("git {operation} runner failed"))?;
        let unborn = initializing
            && output.status == CommandStatus::Completed
            && output.exit_status.and_then(|status| status.code()) != Some(0)
            && matches!(
                repo_path.join(".git").symlink_metadata(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            );
        if unborn {
            return self.pre_init_untrusted_roots(repo_path, operation);
        }
        let output = command_result(operation, QUERY_LIMITS, output)?;
        let lines = output
            .stdout
            .strip_suffix(b"\n")
            .ok_or_else(|| format!("refusing git {operation}: Git repository roots are malformed"))?
            .split(|byte| *byte == b'\n')
            .collect::<Vec<_>>();
        if lines.len() != 3
            || lines[..2].iter().any(|line| line.is_empty())
            || !matches!(lines[2], b"true" | b"false")
        {
            return Err(format!(
                "refusing git {operation}: Git repository roots are malformed"
            ));
        }
        let bare = lines[2] == b"true";
        let mut roots = std::iter::once(repo_path.to_path_buf())
            .chain(lines[..2].iter().copied().map(output_path_bytes))
            .map(|path| {
                path.canonicalize().map_err(|_| {
                    format!("refusing git {operation}: Git repository roots cannot be resolved")
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if !bare {
            let command = self.build_internal_command(
                repo_path,
                ["rev-parse", "--path-format=absolute", "--show-toplevel"],
                InternalProfile::Local,
                &hardened_config(InternalProfile::Local, false),
            );
            let output = Sandbox::new(false, "git")
                .output_built_command_with_limits(command, QUERY_LIMITS)
                .await
                .map_err(|_| format!("git {operation} runner failed"))?;
            let output = command_result(operation, QUERY_LIMITS, output)?;
            let top = output.stdout.strip_suffix(b"\n").ok_or_else(|| {
                format!("refusing git {operation}: Git repository roots are malformed")
            })?;
            if top.is_empty() || top.contains(&b'\n') {
                return Err(format!(
                    "refusing git {operation}: Git repository roots are malformed"
                ));
            }
            roots.push(output_path_bytes(top).canonicalize().map_err(|_| {
                format!("refusing git {operation}: Git repository roots cannot be resolved")
            })?);
        }
        add_sandbox_cache_root(&mut roots, operation)?;
        Ok(roots)
    }

    /// `git init` in a fresh directory has no Git administration paths yet.
    /// The directory itself covers any global include the model can already
    /// author there, including a future `.git` path. An existing `.git` entry
    /// never enters this path: malformed or unreadable repositories fail closed.
    fn pre_init_untrusted_roots(
        &self,
        repo_path: &Path,
        operation: &str,
    ) -> Result<Vec<PathBuf>, String> {
        let root = repo_path.canonicalize().map_err(|_| {
            format!("refusing git {operation}: Git repository roots cannot be resolved")
        })?;
        let mut roots = vec![root];
        add_sandbox_cache_root(&mut roots, operation)?;
        Ok(roots)
    }

    /// Overrides for repositories nested in the work tree.
    ///
    /// When an outer operation (`status`, `add`, `commit`, `diff-files`, ...)
    /// meets a gitlink whose directory holds a repository, Git spawns itself
    /// inside that repository to check whether it is dirty. The child
    /// inherits the command-scope overrides (`GIT_CONFIG_COUNT`), so hooks,
    /// fsmonitor, and every key [`hardened_config`] fixes stay neutralised,
    /// but the nested repository's own `.git/config` can name *different*
    /// filter and driver keys that the outer probe never saw. The model can
    /// create such a repository (and have it recorded as a gitlink by an
    /// auto-commit), so every populated gitlink reachable from the index is
    /// probed, recursively, and its repository-scoped filter/diff/merge
    /// commands are overridden too. Too many or too deeply nested
    /// repositories refuse the operation instead.
    async fn nested_repository_overrides(
        &self,
        repo_path: &Path,
        operation: &str,
        existing: &[(String, String)],
    ) -> Result<Vec<(String, String)>, String> {
        let mut overrides: Vec<(String, String)> = Vec::new();
        let mut visited: Vec<PathBuf> = Vec::new();
        let mut pending = self
            .populated_gitlinks(repo_path, operation)
            .await?
            .into_iter()
            .map(|path| (path, 1usize))
            .collect::<Vec<_>>();
        while let Some((nested, depth)) = pending.pop() {
            let refuse = |reason: &str| {
                format!(
                    "refusing git {operation}: nested repository {} {reason}; \
                     remove it from the index (`git rm --cached`) or commit it separately",
                    nested.display()
                )
            };
            let canonical = nested
                .canonicalize()
                .map_err(|_| refuse("cannot be resolved"))?;
            if visited.contains(&canonical) {
                continue;
            }
            if depth > MAX_NESTED_REPOSITORY_DEPTH {
                return Err(refuse(&format!(
                    "is nested more than {MAX_NESTED_REPOSITORY_DEPTH} levels deep"
                )));
            }
            if visited.len() >= MAX_NESTED_REPOSITORIES {
                return Err(refuse(&format!(
                    "exceeds the limit of {MAX_NESTED_REPOSITORIES} nested repositories"
                )));
            }
            visited.push(canonical);
            let entries = self
                .executable_config_entries(&nested, operation, false)
                .await
                .map_err(|error| refuse(&format!("has unreadable configuration ({error})")))?;
            // Transport and credential keys are never consulted by a dirty
            // check inside a nested repository; resetting them here could
            // only disturb the outer operation's own (e.g. network) values.
            let nested_overrides = untrusted_config_overrides(&entries, InternalProfile::Local)?;
            for (key, value) in nested_overrides {
                if nested_override_applies(&key)
                    && !existing
                        .iter()
                        .chain(overrides.iter())
                        .any(|(present, _)| *present == key)
                {
                    overrides.push((key, value));
                }
            }
            pending.extend(
                self.populated_gitlinks(&nested, operation)
                    .await
                    .map_err(|error| refuse(&format!("cannot be listed ({error})")))?
                    .into_iter()
                    .map(|path| (path, depth + 1)),
            );
        }
        Ok(overrides)
    }

    /// Work-tree paths of gitlinks in `repo_path`'s index (every stage) whose
    /// directory holds a `.git`, i.e. repositories Git would descend into.
    /// `ls-files` reads only the index; it runs under the fixed hardened
    /// configuration so reading the index cannot start a fsmonitor.
    async fn populated_gitlinks(
        &self,
        repo_path: &Path,
        operation: &str,
    ) -> Result<Vec<PathBuf>, String> {
        let mut command = self.build_internal_command(
            repo_path,
            ["ls-files", "--stage", "-z", "--", ":(top)"],
            InternalProfile::Local,
            &hardened_config(InternalProfile::Local, false),
        );
        // `:(top)` lists the whole index even when `repo_path` is a
        // subdirectory; paths stay relative to `repo_path`.
        command.env("GIT_LITERAL_PATHSPECS", "0");
        let output = Sandbox::new(false, "git")
            .output_built_command_with_limits(command, GITLINK_PROBE_LIMITS)
            .await
            .map_err(|_| format!("git {operation} runner failed"))?;
        if output.status != CommandStatus::Completed {
            return command_result(operation, GITLINK_PROBE_LIMITS, output).map(|_| Vec::new());
        }
        if !output.exit_status.is_some_and(|status| status.success()) {
            // No index Git could read (not a repository, a bare one, or a
            // corrupt index): the operation itself cannot descend either.
            return Ok(Vec::new());
        }
        let mut paths: Vec<PathBuf> = Vec::new();
        for relative in parse_gitlinks(&output.stdout) {
            let path = repo_path.join(relative);
            if path.join(".git").symlink_metadata().is_ok() && !paths.contains(&path) {
                paths.push(path);
            }
        }
        Ok(paths)
    }

    #[cfg(any(test, feature = "git-worktree"))]
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
    #[cfg(any(test, feature = "git-worktree"))]
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
    #[cfg(any(test, feature = "git-worktree"))]
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
    #[cfg(any(test, feature = "git-worktree"))]
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
    #[cfg(any(test, feature = "git-worktree"))]
    async fn host_config_value(
        &self,
        repo_path: &Path,
        key: &str,
        config_env: &[(String, OsString)],
    ) -> Result<Option<String>, String> {
        let mut command = self
            .internal_command(
                repo_path,
                "config",
                ["config", "--get", key],
                InternalProfile::Local,
            )
            .await?;
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
        self.run_profiled(repo_path, operation, args, limits, InternalProfile::Local)
            .await
    }

    /// Runs a host-side fetch/pull that may need the user's credentials: the
    /// SSH agent, askpass, and proxy environment are passed through and the
    /// user's *global* credential helpers and `core.sshCommand` stay active,
    /// while repository-scoped command-executing keys remain neutralised.
    #[cfg(any(test, feature = "git-worktree"))]
    pub(crate) async fn run_network<I, S>(
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
        self.run_profiled(repo_path, operation, args, limits, InternalProfile::Network)
            .await
    }

    #[cfg(all(test, unix))]
    pub(crate) async fn run_network_with_input_for_test<I, S>(
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
        let command = self
            .internal_command(repo_path, operation, args, InternalProfile::Network)
            .await?;
        let output = Sandbox::new(false, "git")
            .output_built_command_with_input_and_limits(command, input, limits)
            .await
            .map_err(|_| format!("git {operation} runner failed"))?;
        command_result(operation, limits, output)
    }

    async fn run_profiled<I, S>(
        &self,
        repo_path: &Path,
        operation: &'static str,
        args: I,
        limits: CommandLimits,
        profile: InternalProfile,
    ) -> Result<CommandOutput, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let command = self
            .internal_command(repo_path, operation, args, profile)
            .await?;
        let output = Sandbox::new(false, "git")
            .output_built_command_with_limits(command, limits)
            .await
            .map_err(|_| format!("git {operation} runner failed"))?;
        command_result(operation, limits, output)
    }

    #[cfg(any(test, feature = "git-worktree"))]
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
        let lock = repository_mutation_lock(&key).lock_owned();
        #[cfg(all(test, feature = "git-worktree", unix))]
        let lock = async {
            tokio::pin!(lock);
            let mut observed = false;
            std::future::poll_fn(|cx| {
                let result = std::future::Future::poll(lock.as_mut(), cx);
                if !observed {
                    observed = true;
                    let _ = MUTATION_LOCK_OBSERVER.try_with(|observer| {
                        let _ = observer.send((key.clone(), result.is_ready()));
                    });
                }
                result
            })
            .await
        };
        Ok(lock.await)
    }

    #[cfg(feature = "git-worktree")]
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
        let command = self
            .internal_command(repo_path, operation, args, InternalProfile::Local)
            .await?;
        let output = Sandbox::new(false, "git")
            .output_built_command_with_input_and_limits(command, input, limits)
            .await
            .map_err(|_| format!("git {operation} runner failed"))?;
        command_result(operation, limits, output)
    }

    #[cfg(any(test, feature = "git-worktree"))]
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
        let command = self
            .internal_command(repo_path, operation, args, InternalProfile::Local)
            .await?;
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
        let command = self
            .internal_command(repo_path, operation, args, InternalProfile::Local)
            .await?;
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
        let command = self
            .internal_command(repo_path, operation, args, InternalProfile::Local)
            .await?;
        Sandbox::new(false, "git")
            .output_built_command_with_input_and_limits(command, input, limits)
            .await
            .map_err(|_| format!("git {operation} runner failed"))
    }

    #[cfg(any(test, feature = "git-worktree"))]
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
    #[cfg(any(test, feature = "git-worktree"))]
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

    #[cfg(any(test, feature = "git-worktree"))]
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
#[cfg(any(test, feature = "git-worktree"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommitIdentity {
    pub(crate) author_name: String,
    pub(crate) author_email: String,
    pub(crate) committer_name: String,
    pub(crate) committer_email: String,
}

#[cfg(any(test, feature = "git-worktree"))]
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

#[cfg(any(test, feature = "git-worktree"))]
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
#[cfg(any(test, feature = "git-worktree"))]
fn identity_value(raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.is_empty() || value.contains(['\0', '\n', '\r']) {
        None
    } else {
        Some(value.to_owned())
    }
}

#[cfg(any(test, feature = "git-worktree"))]
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

/// What a host-side Git command may reach beyond the local repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InternalProfile {
    /// Queries and local mutations: no credentials of any kind.
    Local,
    /// Explicit fetch/pull: the user's SSH agent, askpass, proxy settings,
    /// and *global* credential helpers remain available.
    #[cfg_attr(not(any(test, feature = "git-worktree")), allow(dead_code))]
    Network,
}

/// Process environment passed to every internal Git command. Everything else
/// (provider API keys, `GIT_DIR`/`GIT_WORK_TREE`/`GIT_CONFIG_*` redirection,
/// `GIT_CONFIG_PARAMETERS`, ...) is cleared.
const INTERNAL_BASE_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    // Commit identity overrides honoured by the auto-commit and merge flows.
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_AUTHOR_DATE",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "GIT_COMMITTER_DATE",
    "EMAIL",
    // Windows process essentials and home/config discovery.
    "SYSTEMROOT",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "TEMP",
    "TMP",
    "USERNAME",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "SYSTEMDRIVE",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
];

/// Additional variables for [`InternalProfile::Network`] only.
const INTERNAL_NETWORK_ENV: &[&str] = &[
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "SSH_ASKPASS",
    "GIT_ASKPASS",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_SSH_VARIANT",
    "DISPLAY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "GIT_SSL_CAINFO",
    "GIT_SSL_CAPATH",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// Fixed policy variables. `GIT_EDITOR=:` is Git's documented no-op editor.
const INTERNAL_POLICY_ENV: &[(&str, &str)] = &[
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_NO_LAZY_FETCH", "1"),
    ("GIT_PAGER", "cat"),
    ("GIT_EXTERNAL_DIFF", ""),
    ("GIT_LITERAL_PATHSPECS", "1"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_ATTR_NOSYSTEM", "1"),
    ("GIT_OPTIONAL_LOCKS", "0"),
    ("GIT_EDITOR", ":"),
    ("GIT_SEQUENCE_EDITOR", ":"),
    ("GIT_MERGE_AUTOEDIT", "no"),
];

/// The complete environment of an internal Git command.
pub(crate) fn internal_git_environment(
    profile: InternalProfile,
    lookup: impl Fn(&str) -> Option<OsString>,
) -> Vec<(OsString, OsString)> {
    let network: &[&str] = match profile {
        InternalProfile::Local => &[],
        InternalProfile::Network => INTERNAL_NETWORK_ENV,
    };
    let mut env = Vec::new();
    for name in INTERNAL_BASE_ENV.iter().chain(network) {
        if let Some(value) = lookup(name) {
            env.push((OsString::from(name), value));
        }
    }
    env.extend(
        INTERNAL_POLICY_ENV
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value))),
    );
    env
}

/// Configuration every internal command runs with, regardless of what the
/// repository or the user configured.
fn hardened_config(profile: InternalProfile, repository_execution: bool) -> Vec<(String, String)> {
    let mut config: Vec<(String, String)> = [
        ("core.fsmonitor", "false"),
        ("core.untrackedCache", "false"),
        ("commit.gpgSign", "false"),
        ("tag.gpgSign", "false"),
        ("merge.verifySignatures", "false"),
        ("log.showSignature", "false"),
        ("diff.external", ""),
        ("submodule.recurse", "false"),
        ("fetch.recurseSubmodules", "false"),
        ("status.submoduleSummary", "false"),
        ("protocol.ext.allow", "never"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value.to_string()))
    .collect();
    if !repository_execution {
        config.push(("core.hooksPath".into(), "/dev/null".into()));
    }
    if profile == InternalProfile::Local {
        for key in ["credential.helper", "core.sshCommand", "core.askPass"] {
            config.push((key.into(), String::new()));
        }
    }
    config
}

/// Classifies Git's canonical (lower-case section and variable) key bytes.
/// Do not ask Git to filter with a regex: locale-aware matching can skip a
/// subsection containing non-UTF8 bytes even though Git executes that driver.
fn executable_config_key(key: &[u8]) -> bool {
    matches!(
        key,
        b"core.sshcommand" | b"core.askpass" | b"core.gitproxy" | b"core.alternaterefscommand"
    ) || (key.starts_with(b"credential.") && key.ends_with(b".helper"))
        || (key.starts_with(b"remote.")
            && (key.ends_with(b".uploadpack") || key.ends_with(b".receivepack")))
        || (key.starts_with(b"filter.")
            && [b".clean".as_slice(), b".smudge", b".process", b".required"]
                .iter()
                .any(|suffix| key.ends_with(suffix)))
        || (key.starts_with(b"diff.")
            && (key.ends_with(b".command") || key.ends_with(b".textconv")))
        || (key.starts_with(b"merge.") && key.ends_with(b".driver"))
        || (key.starts_with(b"gpg.") && key.ends_with(b".program"))
}

/// Bounds for the nested-repository probe: nested repositories beyond these
/// refuse the operation rather than run unprobed.
const MAX_NESTED_REPOSITORIES: usize = 64;
const MAX_NESTED_REPOSITORY_DEPTH: usize = 8;

/// `ls-files --stage` prints one record per index entry, so the output limit
/// is sized for large repositories rather than for a status query.
const GITLINK_PROBE_LIMITS: CommandLimits = CommandLimits {
    timeout: Duration::from_secs(30),
    stdout_bytes: 64 * 1024 * 1024,
    stderr_bytes: 64 * 1024,
    combined_bytes: 64 * 1024 * 1024 + 64 * 1024,
};

/// Paths of gitlink (mode `160000`) entries in `git ls-files --stage -z`
/// output: `<mode> SP <oid> SP <stage> TAB <path> NUL` per entry.
fn parse_gitlinks(bytes: &[u8]) -> Vec<PathBuf> {
    bytes
        .split(|byte| *byte == 0)
        .filter_map(|record| {
            let tab = record.iter().position(|byte| *byte == b'\t')?;
            let (meta, path) = (&record[..tab], &record[tab + 1..]);
            if !meta.starts_with(b"160000 ") || path.is_empty() {
                return None;
            }
            Some(output_path_bytes(path))
        })
        .collect()
}

fn output_path_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(OsString::from_vec(bytes.to_vec()))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

fn add_sandbox_cache_root(roots: &mut Vec<PathBuf>, operation: &str) -> Result<(), String> {
    let paths = crate::paths::process_paths()
        .map_err(|_| format!("refusing git {operation}: application paths are unavailable"))?;
    roots.extend(sandbox_cache_roots(
        &paths,
        dirs::cache_dir().as_deref(),
        operation,
    )?);
    Ok(())
}

fn sandbox_cache_roots(
    paths: &crate::paths::AppPaths,
    system_cache: Option<&Path>,
    operation: &str,
) -> Result<Vec<PathBuf>, String> {
    let mut roots = Vec::new();
    let cache_parent = paths
        .cache_dir
        .canonicalize()
        .unwrap_or(paths.cache_dir.clone());
    let configured = cache_parent.join("sandbox-runtime");
    // The configured cache remains model-writable even when Seatbelt chooses
    // its disjoint system-cache fallback for command scratch storage.
    roots.push(configured.clone());
    if let Ok(resolved) = configured.canonicalize() {
        roots.push(resolved);
    }
    #[cfg(target_os = "macos")]
    {
        let selected = crate::sandbox::select_macos_runtime_cache(
            paths.cache_dir.join("sandbox-runtime"),
            paths,
            system_cache,
        )
        .map_err(|error| format!("refusing git {operation}: {error}"))?;
        roots.push(selected.clone());
        if let Ok(resolved) = selected.canonicalize() {
            roots.push(resolved);
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (system_cache, operation);
    Ok(roots)
}

/// Nested repositories contribute only the keys a dirty check inside them
/// can execute: content filters and diff/merge drivers.
fn nested_override_applies(key: &str) -> bool {
    key.starts_with("filter.") || key.starts_with("diff.") || key.starts_with("merge.")
}

#[derive(Debug, PartialEq, Eq)]
struct ScopedConfigEntry {
    scope: String,
    origin: String,
    key: String,
    value: Option<String>,
    trusted_origin: bool,
}

/// Selects executable keys from `git config --show-scope --show-origin --null
/// --list` output: `scope NUL origin NUL key LF value NUL` per entry, with no
/// `LF value` for a value-less boolean key.
fn parse_scoped_config(bytes: &[u8]) -> Result<Vec<ScopedConfigEntry>, String> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let invalid = || "executable Git configuration is malformed or not UTF-8".to_string();
    let records = bytes.strip_suffix(b"\0").ok_or_else(invalid)?;
    let mut fields = records.split(|byte| *byte == 0);
    let mut entries = Vec::new();
    while let Some(scope) = fields.next() {
        let origin = fields.next().ok_or_else(invalid)?;
        let pair = fields.next().ok_or_else(invalid)?;
        // A replacement character would name a different driver from the one
        // Git reads. Refuse the operation instead of emitting a lossy override.
        // Values must also survive exactly when restoring a trusted command.
        let scope = std::str::from_utf8(scope).map_err(|_| invalid())?;
        let key = pair
            .split(|byte| *byte == b'\n')
            .next()
            .ok_or_else(invalid)?;
        if scope.is_empty() || origin.is_empty() || key.is_empty() {
            return Err(invalid());
        }
        if !executable_config_key(key) {
            continue;
        }
        let pair = std::str::from_utf8(pair).map_err(|_| invalid())?;
        let origin = std::str::from_utf8(origin).map_err(|_| invalid())?;
        let (key, value) = match pair.split_once('\n') {
            Some((key, value)) => (key, Some(value.to_string())),
            None => (pair, None),
        };
        entries.push(ScopedConfigEntry {
            scope: scope.to_string(),
            origin: origin.to_string(),
            key: key.to_string(),
            value,
            trusted_origin: false,
        });
    }
    Ok(entries)
}

/// Git reports an included file with the including file's `global` scope.
/// Scope alone is therefore insufficient: a global include can point into a
/// model-writable work tree or its Git administration directory. Check both
/// the path Git named and its resolved target so a symlink under an untrusted
/// root cannot confer global authority. An unknown or vanished origin is not
/// trusted. This classifies the probed config; it does not freeze later edits.
fn origin_is_outside_untrusted_roots(origin: &str, repo_path: &Path, roots: &[PathBuf]) -> bool {
    let Some(file) = origin.strip_prefix("file:") else {
        return false;
    };
    let path = Path::new(file);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_path.join(path)
    };
    // Check every ancestor before resolving the complete path. A symlink
    // *inside* the work tree can point to an outside file, while /tmp itself
    // may be a symlink to /private/tmp. Either spelling must remain untrusted.
    for ancestor in path.ancestors() {
        if roots.iter().any(|root| ancestor.starts_with(root)) {
            return false;
        }
        let Ok(resolved) = ancestor.canonicalize() else {
            return false;
        };
        if roots.iter().any(|root| resolved.starts_with(root)) {
            return false;
        }
    }
    true
}

/// Overrides for every executable key that repository-scoped configuration
/// sets. Only the user's `global` (and, if ever enabled, `system`) scope is
/// trusted; `local`, `worktree`, `command`, and unknown scopes are
/// model-writable or unexplained.
fn untrusted_config_overrides(
    entries: &[ScopedConfigEntry],
    profile: InternalProfile,
) -> Result<Vec<(String, String)>, String> {
    let trusted = |entry: &ScopedConfigEntry| {
        entry.trusted_origin && matches!(entry.scope.as_str(), "global" | "system")
    };
    let mut keys: Vec<&str> = Vec::new();
    for entry in entries.iter().filter(|entry| !trusted(entry)) {
        if !keys.contains(&entry.key.as_str()) {
            keys.push(entry.key.as_str());
        }
    }
    let mut overrides = Vec::new();
    for key in keys {
        let trusted_values = entries
            .iter()
            .filter(|entry| entry.key == key && trusted(entry))
            .map(|entry| entry.value.clone().unwrap_or_default())
            .collect::<Vec<_>>();
        if first_value_wins(key) {
            // Git uses the first value it reads for these keys, so a later
            // override cannot displace a repository value. A user (global)
            // value is read before the repository's and already wins; only
            // transports use these keys, so local operations are unaffected.
            let first_is_trusted = entries
                .iter()
                .find(|entry| entry.key == key)
                .is_some_and(trusted);
            if profile == InternalProfile::Network && !first_is_trusted {
                return Err(format!(
                    "refusing network Git operation: repository configuration sets {key}; \
                     remove it from the repository's .git/config to continue"
                ));
            }
            continue;
        }
        if key.starts_with("credential.") {
            // An empty helper resets the list; re-add only the user's own.
            overrides.push((key.to_string(), String::new()));
            if profile == InternalProfile::Network {
                overrides.extend(
                    trusted_values
                        .into_iter()
                        .map(|value| (key.to_string(), value)),
                );
            }
            continue;
        }
        let fallback = if key.ends_with(".required") {
            "false"
        } else {
            // Git never executes an empty filter command, and an empty
            // driver/ssh/askpass/gpg command fails to launch instead of
            // running repository-chosen text.
            ""
        };
        let value = trusted_values
            .last()
            .cloned()
            .unwrap_or_else(|| fallback.to_string());
        overrides.push((key.to_string(), value));
    }
    Ok(overrides)
}

/// Keys for which Git keeps the first value read (with an error for later
/// ones) instead of the last: `core.gitProxy` and a remote's upload/receive
/// pack, which a local-path remote executes on this host.
fn first_value_wins(key: &str) -> bool {
    key == "core.gitproxy"
        || (key.starts_with("remote.")
            && (key.ends_with(".uploadpack") || key.ends_with(".receivepack")))
}

/// Test-only escape hatch: fixture repositories that inject faults through
/// real hooks or `remote.*.uploadpack` register their owned directory here.
/// Registration never exists outside `cfg(test)`.
#[cfg(test)]
static REPOSITORY_EXECUTION_FOR_TEST: OnceLock<StdMutex<Vec<PathBuf>>> = OnceLock::new();

#[cfg(test)]
pub(crate) struct RepositoryExecutionForTest(Vec<PathBuf>);

#[cfg(test)]
impl Drop for RepositoryExecutionForTest {
    fn drop(&mut self) {
        let registry = REPOSITORY_EXECUTION_FOR_TEST.get_or_init(Default::default);
        let mut registry = registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for path in &self.0 {
            if let Some(index) = registry.iter().position(|entry| entry == path) {
                registry.remove(index);
            }
        }
    }
}

/// Lets repository hooks and repository-configured commands run for Git
/// commands under `root` until the returned guard drops.
#[cfg(test)]
pub(crate) fn allow_repository_execution_for_test(root: &Path) -> RepositoryExecutionForTest {
    let mut paths = vec![root.to_path_buf()];
    if let Ok(canonical) = root.canonicalize()
        && canonical != root
    {
        paths.push(canonical);
    }
    REPOSITORY_EXECUTION_FOR_TEST
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .extend(paths.iter().cloned());
    RepositoryExecutionForTest(paths)
}

#[cfg(test)]
fn repository_execution_allowed_for_test(repo_path: &Path) -> bool {
    REPOSITORY_EXECUTION_FOR_TEST
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .any(|root| repo_path.starts_with(root))
}

#[cfg(any(test, feature = "git-worktree"))]
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
#[cfg(any(test, feature = "git-worktree"))]
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
            descendants_escaped: false,
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

    #[tokio::test]
    async fn unavailable_runner_rejects_internal_and_contained_execution() {
        let runner = GitRunner::unavailable().unwrap();
        let root = std::env::current_dir().unwrap();
        let workspace = crate::paths::WorkspaceBinding::capture(&root).unwrap();
        let internal = runner.run(&root, "status", ["status"], limits()).await;
        let contained = runner
            .run_contained(
                &workspace,
                &Sandbox::new(false, "git"),
                "status",
                ["status"],
                limits(),
                false,
            )
            .await;
        for result in [internal, contained] {
            assert_eq!(
                result.err().as_deref(),
                Some("Git executable is unavailable or unsupported")
            );
        }
    }

    #[test]
    fn internal_environment_is_an_allow_list() {
        let everything = |name: &str| Some(OsString::from(format!("value-of-{name}")));
        let hostile = [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_GLOBAL",
            "GIT_EXEC_PATH",
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
        ];
        for profile in [InternalProfile::Local, InternalProfile::Network] {
            let env = internal_git_environment(profile, |name| {
                // The lookup is only ever asked for allow-listed names.
                assert!(!hostile.contains(&name), "{name} must never be looked up");
                everything(name)
            });
            let names = env
                .iter()
                .map(|(name, _)| name.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            assert!(names.iter().any(|name| name == "PATH"));
            assert!(names.iter().any(|name| name == "HOME"));
            assert_eq!(
                names.iter().any(|name| name == "SSH_AUTH_SOCK"),
                profile == InternalProfile::Network,
                "only network operations see the SSH agent"
            );
            for (name, value) in INTERNAL_POLICY_ENV {
                assert!(
                    env.iter()
                        .any(|(key, actual)| key == name && actual == value),
                    "missing policy {name}"
                );
            }
        }
    }

    #[test]
    fn scoped_config_output_parses_values_and_valueless_keys() {
        let entries = parse_scoped_config(
            b"global\0file:/home/user/.gitconfig\0filter.lfs.clean\ngit-lfs clean -- %f\0local\0file:.git/config\0filter.x.required\0local\0file:.git/config\0core.sshcommand\nevil\nline\0",
        ).unwrap();
        assert_eq!(
            entries,
            vec![
                ScopedConfigEntry {
                    scope: "global".into(),
                    origin: "file:/home/user/.gitconfig".into(),
                    key: "filter.lfs.clean".into(),
                    value: Some("git-lfs clean -- %f".into()),
                    trusted_origin: false,
                },
                ScopedConfigEntry {
                    scope: "local".into(),
                    origin: "file:.git/config".into(),
                    key: "filter.x.required".into(),
                    value: None,
                    trusted_origin: false,
                },
                ScopedConfigEntry {
                    scope: "local".into(),
                    origin: "file:.git/config".into(),
                    key: "core.sshcommand".into(),
                    value: Some("evil\nline".into()),
                    trusted_origin: false,
                },
            ]
        );
    }

    #[test]
    fn scoped_config_selects_every_executable_key() {
        let keys = [
            "core.sshcommand",
            "core.askpass",
            "core.gitproxy",
            "core.alternaterefscommand",
            "credential.helper",
            "credential.https://example.com.helper",
            "remote.origin.uploadpack",
            "remote.origin.receivepack",
            "filter.a=b.clean",
            "filter.x.smudge",
            "filter.x.process",
            "filter.x.required",
            "diff.x.command",
            "diff.x.textconv",
            "merge.x.driver",
            "gpg.program",
            "gpg.ssh.program",
        ];
        let mut records =
            b"local\0file:.git/config\0remote.origin.url\nfile:///local/repo\0".to_vec();
        for key in keys {
            records
                .extend_from_slice(format!("local\0file:.git/config\0{key}\nvalue\0").as_bytes());
        }
        records.extend_from_slice(b"local\0file:.git/config\0filter.x.description\ntext\xff\0");
        let entries = parse_scoped_config(&records).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.key.as_str())
                .collect::<Vec<_>>(),
            keys
        );
    }

    #[test]
    fn scoped_config_rejects_lossy_or_incomplete_records() {
        assert_eq!(parse_scoped_config(b""), Ok(Vec::new()));
        assert_eq!(
            parse_scoped_config(b"local\0file:.git/config\0user.name\nname\xff\0"),
            Ok(Vec::new())
        );
        for bytes in [
            &b"local\0file:.git/config\0filter.x\xff.clean\nevil\0"[..],
            &b"loc\xffal\0file:.git/config\0filter.x.clean\nevil\0"[..],
            &b"global\0file:/home/user/.gitconfig\0filter.x.clean\nevil\xff\0"[..],
            &b"global\0file:/home/user/config\xff\0filter.x.clean\nevil\0"[..],
            &b"local\0file:.git/config\0filter.x.clean\nevil"[..],
            &b"local\0"[..],
            &b"\0file:.git/config\0filter.x.clean\nevil\0"[..],
            &b"local\0\0filter.x.clean\nevil\0"[..],
            &b"local\0file:.git/config\0filter.x.clean\nevil\0worktree\0"[..],
        ] {
            assert!(parse_scoped_config(bytes).is_err(), "accepted {bytes:?}");
        }
        // Literal replacement characters are valid UTF-8 and must stay intact.
        let entries =
            parse_scoped_config("local\0file:.git/config\0filter.\u{fffd}.clean\n\0".as_bytes())
                .unwrap();
        assert_eq!(entries[0].key, "filter.\u{fffd}.clean");
        assert_eq!(entries[0].value.as_deref(), Some(""));
    }

    #[cfg(unix)]
    #[test]
    fn global_origin_under_workspace_or_cache_is_not_trusted() {
        use std::os::unix::fs::symlink;

        let owned =
            std::env::temp_dir().join(format!("mini-agent-origin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&owned).unwrap();
        let repo = owned.join("repo");
        let cache = owned.join("sandbox-runtime");
        let trusted = owned.join("trusted");
        for directory in [&repo, &cache, &trusted] {
            std::fs::create_dir(directory).unwrap();
        }
        let trusted_file = trusted.join("config");
        let cache_file = cache.join("config");
        std::fs::write(&trusted_file, "").unwrap();
        std::fs::write(&cache_file, "").unwrap();
        let repo_file = repo.join("config");
        std::fs::write(&repo_file, "").unwrap();
        let alias = repo.join("alias");
        symlink(&trusted_file, &alias).unwrap();
        let outside_alias = trusted.join("alias-to-repo");
        symlink(&repo_file, &outside_alias).unwrap();
        let roots = vec![repo.canonicalize().unwrap(), cache.canonicalize().unwrap()];
        let origin = |path: &Path| format!("file:{}", path.display());

        assert!(origin_is_outside_untrusted_roots(
            &origin(&trusted_file),
            &repo,
            &roots
        ));
        assert!(!origin_is_outside_untrusted_roots(
            &origin(&cache_file),
            &repo,
            &roots
        ));
        assert!(!origin_is_outside_untrusted_roots(
            &origin(&alias),
            &repo,
            &roots
        ));
        assert!(!origin_is_outside_untrusted_roots(
            &origin(&outside_alias),
            &repo,
            &roots
        ));
        assert!(!origin_is_outside_untrusted_roots(
            "command line:",
            &repo,
            &roots
        ));
        assert!(!origin_is_outside_untrusted_roots(
            &origin(&repo.join("vanished")),
            &repo,
            &roots
        ));
        std::fs::remove_dir_all(owned).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn global_include_from_selected_seatbelt_cache_cannot_supply_executable_config() {
        struct Owned(PathBuf);
        impl Drop for Owned {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let owned = Owned(std::env::temp_dir().join(format!(
            "mini-agent-seatbelt-global-include-{}",
            uuid::Uuid::new_v4()
        )));
        let repo = owned.0.join("repo");
        let home = owned.0.join("home");
        let system_cache = owned.0.join("system-cache");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let paths = crate::paths::AppPaths {
            config_dir: owned.0.join("private"),
            credentials_dir: owned.0.join("credentials"),
            cache_dir: owned.0.join("private/cache"),
            data_dir: owned.0.join("data"),
            local_data_dir: owned.0.join("local"),
            state_dir: owned.0.join("state"),
            project_dir: None,
        };
        let fallback = crate::sandbox::select_macos_runtime_cache(
            paths.cache_dir.join("sandbox-runtime"),
            &paths,
            Some(&system_cache),
        )
        .unwrap();
        std::fs::create_dir_all(&fallback).unwrap();
        let included = fallback.join("included-config");
        let marker = owned.0.join("filter-ran");
        std::fs::write(
            &included,
            format!(
                "[filter \"late\"]\n\tclean = /usr/bin/touch {}\n[credential]\n\thelper = attacker-helper\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::write(
            home.join(".gitconfig"),
            format!(
                "[credential]\n\thelper = trusted-helper\n[include]\n\tpath = {}\n",
                included.display()
            ),
        )
        .unwrap();
        let git = GitRunner::discover().unwrap();
        let init = std::process::Command::new(git.program.as_path())
            .args(["init", "-q"])
            .arg(&repo)
            .env_clear()
            .env("HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        std::fs::write(repo.join("payload"), "content\n").unwrap();
        std::fs::write(repo.join(".gitattributes"), "payload filter=late\n").unwrap();
        let probe = std::process::Command::new(git.program.as_path())
            .args([
                "config",
                "--show-scope",
                "--show-origin",
                "--null",
                "--list",
            ])
            .current_dir(&repo)
            .env_clear()
            .env("HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            probe.status.success(),
            "{}",
            String::from_utf8_lossy(&probe.stderr)
        );
        let roots = sandbox_cache_roots(&paths, Some(&system_cache), "probe").unwrap();
        assert!(roots.iter().any(|root| fallback.starts_with(root)));
        assert!(
            roots
                .iter()
                .any(|root| paths.cache_dir.join("sandbox-runtime").starts_with(root))
        );
        let mut entries = parse_scoped_config(&probe.stdout).unwrap();
        let configured_only = vec![repo.clone(), paths.cache_dir.join("sandbox-runtime")];
        let mut old_entries = parse_scoped_config(&probe.stdout).unwrap();
        for entry in &mut old_entries {
            entry.trusted_origin =
                origin_is_outside_untrusted_roots(&entry.origin, &repo, &configured_only);
        }
        assert!(
            old_entries
                .iter()
                .any(|entry| entry.key == "filter.late.clean" && entry.trusted_origin)
        );
        let old_overrides =
            untrusted_config_overrides(&old_entries, InternalProfile::Network).unwrap();
        assert!(
            !old_overrides
                .iter()
                .any(|(key, _)| key == "filter.late.clean")
        );
        let git_add = |overrides: &[(String, String)]| {
            let mut command = std::process::Command::new(git.program.as_path());
            command.current_dir(&repo);
            command
                .env_clear()
                .env("HOME", &home)
                .env("GIT_CONFIG_NOSYSTEM", "1");
            for (key, value) in overrides {
                command.arg("-c").arg(format!("{key}={value}"));
            }
            command.args(["add", "--", "payload"]);
            command.output().unwrap()
        };
        let old_add = git_add(&old_overrides);
        assert!(
            old_add.status.success(),
            "{}",
            String::from_utf8_lossy(&old_add.stderr)
        );
        assert!(
            marker.exists(),
            "the old classifier must permit the clean driver"
        );
        std::fs::remove_file(&marker).unwrap();
        std::fs::write(repo.join("payload"), "changed\n").unwrap();
        for entry in &mut entries {
            entry.trusted_origin = origin_is_outside_untrusted_roots(&entry.origin, &repo, &roots);
        }
        assert!(entries.iter().any(|entry| {
            entry.scope == "global" && entry.key == "filter.late.clean" && !entry.trusted_origin
        }));
        let overrides = untrusted_config_overrides(&entries, InternalProfile::Network).unwrap();
        assert!(overrides.contains(&("filter.late.clean".into(), String::new())));
        assert!(overrides.contains(&("credential.helper".into(), "trusted-helper".into())));
        assert!(
            !overrides
                .iter()
                .any(|(_, value)| value == "attacker-helper")
        );
        let guarded_add = git_add(&overrides);
        assert!(
            guarded_add.status.success(),
            "{}",
            String::from_utf8_lossy(&guarded_add.stderr)
        );
        assert!(
            !marker.exists(),
            "selected-cache origin must be neutralized"
        );
    }

    #[test]
    fn repository_scoped_executables_are_overridden_and_global_ones_kept() {
        let entry = |scope: &str, key: &str, value: &str| ScopedConfigEntry {
            scope: scope.into(),
            origin: if scope == "global" {
                "file:/home/user/.gitconfig".into()
            } else {
                "file:.git/config".into()
            },
            key: key.into(),
            value: Some(value.into()),
            trusted_origin: scope == "global",
        };
        let entries = vec![
            entry("global", "filter.lfs.clean", "git-lfs clean -- %f"),
            entry("global", "credential.helper", "osxkeychain"),
            entry("local", "credential.helper", "!evil"),
            entry("worktree", "credential.https://example.com.helper", "!evil"),
            entry("global", "core.sshcommand", "ssh -i key"),
            entry("local", "core.sshcommand", "evil"),
            entry("local", "filter.a=b.clean", "evil"),
            entry("local", "filter.x.clean", "evil"),
            entry("local", "filter.x.required", "true"),
            entry("local", "merge.m.driver", "evil"),
            entry("command", "diff.d.textconv", "evil"),
        ];
        let local = untrusted_config_overrides(&entries, InternalProfile::Local).unwrap();
        let network = untrusted_config_overrides(&entries, InternalProfile::Network).unwrap();
        let pair = |key: &str, value: &str| (key.to_string(), value.to_string());
        for overrides in [&local, &network] {
            assert!(
                !overrides.iter().any(|(key, _)| key == "filter.lfs.clean"),
                "global-only drivers are the user's own and stay active"
            );
            for expected in [
                pair("credential.https://example.com.helper", ""),
                pair("core.sshcommand", "ssh -i key"),
                pair("filter.a=b.clean", ""),
                pair("filter.x.clean", ""),
                pair("filter.x.required", "false"),
                pair("merge.m.driver", ""),
                pair("diff.d.textconv", ""),
            ] {
                assert!(overrides.contains(&expected), "missing {expected:?}");
            }
        }
        let helpers = |overrides: &[(String, String)]| {
            overrides
                .iter()
                .filter(|(key, _)| key == "credential.helper")
                .map(|(_, value)| value.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(helpers(&local), vec![String::new()]);
        assert_eq!(
            helpers(&network),
            vec![String::new(), "osxkeychain".to_string()],
            "network operations reset the list and re-add only global helpers"
        );

        // First-value-wins keys cannot be overridden: a repository value
        // refuses network operations unless a user value is read first.
        for key in [
            "core.gitproxy",
            "remote.origin.uploadpack",
            "remote.a=b.receivepack",
        ] {
            let repository_only = [entry("local", key, "evil")];
            assert_eq!(
                untrusted_config_overrides(&repository_only, InternalProfile::Local),
                Ok(Vec::new())
            );
            let error = untrusted_config_overrides(&repository_only, InternalProfile::Network)
                .expect_err("repository first-wins key must refuse the network operation");
            assert!(error.contains(key), "{error}");
            let user_first = [entry("global", key, "mine"), entry("local", key, "evil")];
            assert_eq!(
                untrusted_config_overrides(&user_first, InternalProfile::Network),
                Ok(Vec::new())
            );
        }
    }

    #[test]
    fn gitlinks_are_parsed_from_every_stage_and_nothing_else() {
        let listing = b"100644 e79c5e8f964493290a409888d5413a737e8e5dd5 0\ttracked.txt\0\
160000 1cf00f1ef18823225a1d454284e344b2e4e92d17 0\t../vendor/inner\0\
120000 1cf00f1ef18823225a1d454284e344b2e4e92d17 0\tlink\0\
160000 1cf00f1ef18823225a1d454284e344b2e4e92d17 2\tname with\ttab\0\
160000 malformed-without-tab\0";
        assert_eq!(
            parse_gitlinks(listing),
            vec![
                PathBuf::from("../vendor/inner"),
                PathBuf::from("name with\ttab")
            ]
        );
        assert!(parse_gitlinks(b"").is_empty());
    }

    #[test]
    fn nested_repositories_contribute_only_filters_and_drivers() {
        for key in [
            "filter.x.clean",
            "filter.x.required",
            "diff.d.textconv",
            "merge.m.driver",
        ] {
            assert!(nested_override_applies(key), "{key}");
        }
        for key in [
            "credential.helper",
            "core.sshcommand",
            "core.askpass",
            "remote.origin.uploadpack",
            "gpg.program",
        ] {
            assert!(!nested_override_applies(key), "{key}");
        }
    }

    #[test]
    fn cached_git_environment_enforces_exact_policy() {
        let env = git_environment();
        let required = [
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_NO_LAZY_FETCH", "1"),
            ("GIT_PAGER", "cat"),
            ("GIT_EXTERNAL_DIFF", ""),
            ("GIT_LITERAL_PATHSPECS", "1"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
        ];
        let values = env
            .iter()
            .cloned()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(values.len(), env.len(), "duplicate environment keys");
        for (key, value) in required {
            assert_eq!(
                values.get(OsStr::new(key)).map(OsString::as_os_str),
                Some(OsStr::new(value)),
                "incorrect policy for {key}"
            );
        }
        for key in values.keys() {
            let policy_key = required.iter().any(|(name, _)| key == name);
            #[cfg(windows)]
            let platform_key = ["SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT", "TEMP", "TMP"]
                .iter()
                .any(|name| key == name);
            #[cfg(not(windows))]
            let platform_key = false;
            assert!(
                policy_key || platform_key,
                "unexpected environment key {key:?}"
            );
        }
        assert!(
            std::ptr::eq(env, git_environment()),
            "the cached environment must reuse its allocation"
        );
    }
}

#[cfg(all(test, unix))]
mod configuration_encoding_tests {
    use super::*;
    struct Fixture {
        root: PathBuf,
        repo: PathBuf,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    impl Fixture {
        async fn new() -> Self {
            let root = std::env::temp_dir()
                .join(format!("mini-agent-git-encoding-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            let fixture = Self {
                repo: root.join("repo"),
                root,
            };
            std::fs::create_dir(&fixture.repo).unwrap();
            GitRunner::discover()
                .unwrap()
                .run(&fixture.repo, "init", ["init", "-q"], LOCAL_MUTATION_LIMITS)
                .await
                .unwrap();
            std::fs::write(fixture.repo.join("file.txt"), "payload\n").unwrap();
            fixture
        }
        fn arm(&self) -> PathBuf {
            let marker = self.root.join("host-executed");
            let mut name = b"review".to_vec();
            name.push(0xff);
            let mut config = std::fs::read(self.repo.join(".git/config")).unwrap();
            config.extend_from_slice(b"\n[filter \"");
            config.extend_from_slice(&name);
            config.extend_from_slice(
                b"\"]\n\tclean = \"sh -c 'printf EXECUTED > ../host-executed; cat'\"\n",
            );
            std::fs::write(self.repo.join(".git/config"), config).unwrap();
            let mut attributes = b"* filter=".to_vec();
            attributes.extend_from_slice(&name);
            attributes.push(b'\n');
            std::fs::write(self.repo.join(".gitattributes"), attributes).unwrap();
            marker
        }
    }
    #[tokio::test]
    async fn non_utf8_git_filter_is_rejected_before_host_mutation() {
        let fixture = Fixture::new().await;
        let marker = fixture.arm();
        let runner = GitRunner::discover().unwrap();
        let result = runner
            .run(
                &fixture.repo,
                "add",
                ["add", "file.txt"],
                LOCAL_MUTATION_LIMITS,
            )
            .await;
        let error = result
            .err()
            .expect("undecodable filter configuration must refuse add");
        assert!(error.contains("executable Git configuration"), "{error}");
        assert!(!marker.exists(), "non-UTF8 filter ran outside containment");
        let index = runner
            .run(&fixture.repo, "ls-files", ["ls-files"], QUERY_LIMITS)
            .await;
        assert!(
            index.is_err(),
            "queries must use the same fail-closed probe"
        );
        assert!(
            !fixture.repo.join(".git/index").exists(),
            "refused add mutated the index"
        );
    }
}
