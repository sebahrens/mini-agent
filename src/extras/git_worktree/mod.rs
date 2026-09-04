//! Git worktree operations with explicit repository context.
//!
//! Production code in this module never changes the process working directory.
//! Every Git invocation uses `git -C <canonical-path>`, runs on Tokio's process
//! driver, and is owned by the bounded command worker so timeout, output-limit,
//! and caller-drop paths kill and reap the Unix child process group (or the
//! direct child on platforms without implemented descendant-tree cleanup).

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

#[cfg(test)]
use std::sync::Mutex as StdMutex;

use tokio::sync::{Mutex, OwnedMutexGuard, oneshot};

use crate::git::runner::{
    GitRunner, LOCAL_MUTATION_LIMITS, NETWORK_LIMITS, QUERY_LIMITS, command_failure,
};
use crate::sandbox::{CommandLimits, CommandOutput};

static PROCESS_WORKSPACE_LOCK: OnceLock<Arc<Mutex<()>>> = OnceLock::new();
const ZERO_OID: &str = "0000000000000000000000000000000000000000";
#[cfg(test)]
static STASH_PUBLISH_TEST_GATE: OnceLock<StdMutex<Option<StashPublishTestGate>>> = OnceLock::new();

#[cfg(test)]
struct StashPublishTestGate {
    repo_path: PathBuf,
    gate: Arc<TestMutationGate>,
}

#[derive(Debug, Clone)]
pub enum DeferredWorktreeAction {
    Switch { path: PathBuf, branch: String },
    Merge { info: WorktreeInfo, target: String },
    Exit { main_path: PathBuf },
}

impl fmt::Display for DeferredWorktreeAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Switch { path, branch } => {
                write!(
                    f,
                    "deferred worktree switch: {branch} at {}",
                    path.display()
                )
            }
            Self::Merge { info, target } => {
                write!(f, "deferred worktree merge: {} -> {}", info.branch, target)
            }
            Self::Exit { main_path, .. } => {
                write!(f, "deferred worktree exit: back to {}", main_path.display())
            }
        }
    }
}

impl std::error::Error for DeferredWorktreeAction {}

#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub branch: String,
    pub worktree_path: PathBuf,
    pub main_repo_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    Success,
    Conflicts(Vec<String>),
    Error(String),
}

type MergeResponse = (MergeState, MergeOutcome);
type CreateResponse = Result<(PathBuf, WorktreeInfo), String>;

enum MergeStepError {
    Command(String),
    CallerDropped,
}

enum SupervisedMerge {
    Deliver(MergeResponse),
    CallerDropped(MergeState),
}

enum CreateStepError {
    Command(String),
    CallerDropped,
}

struct CreateState {
    repo_path: PathBuf,
    target: PathBuf,
    branch: String,
    expected_branch_oid: String,
    ownership_ref: String,
    branch_reservation_owned: bool,
    branch_reservation_uncertain: bool,
    _repository_guard: OwnedMutexGuard<()>,
}

pub struct MergeState {
    pub info: WorktreeInfo,
    pub original_branch: String,
    /// The workspace the caller was operating in before the merge. It is kept
    /// for UI state and compatibility; Git operations never use it as CWD.
    pub orig_dir: PathBuf,
    pub stashed: bool,
    stash_before: Option<String>,
    stash_created: Option<String>,
    stash_attempted: bool,
    branch_switch_attempted: bool,
    index_mutation_attempted: bool,
    original_head_oid: String,
    source_head_oid: String,
    target_branch: String,
    target_head_before_pull: Option<String>,
    target_head_before_merge: Option<String>,
    target_head_after_commit: Option<String>,
    successful_merge_head: Option<String>,
    repository_guard: Option<OwnedMutexGuard<()>>,
    #[cfg(test)]
    rollback_test_gate: Option<Arc<TestMutationGate>>,
    #[cfg(test)]
    stash_test_gate: Option<Arc<TestMutationGate>>,
}

#[cfg(test)]
pub(crate) struct TestMutationGate {
    reached: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

#[cfg(test)]
impl TestMutationGate {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            reached: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
        })
    }

    pub(crate) async fn wait_until_reached(&self) {
        self.reached.notified().await;
    }

    pub(crate) fn resume(&self) {
        self.resume.notify_one();
    }
}

#[cfg(test)]
pub(crate) fn set_next_stash_publish_test_gate(repo_path: &Path, gate: Arc<TestMutationGate>) {
    let repo_path = repo_path
        .canonicalize()
        .expect("stash publication test repository must be canonicalizable");
    *STASH_PUBLISH_TEST_GATE
        .get_or_init(|| StdMutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
        Some(StashPublishTestGate { repo_path, gate });
}

#[cfg(test)]
impl MergeState {
    pub(crate) fn set_rollback_test_gate(&mut self, gate: Arc<TestMutationGate>) {
        self.rollback_test_gate = Some(gate);
    }
}

impl fmt::Debug for MergeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MergeState")
            .field("info", &self.info)
            .field("original_branch", &self.original_branch)
            .field("original_head_oid", &self.original_head_oid)
            .field("source_head_oid", &self.source_head_oid)
            .field("target_branch", &self.target_branch)
            .field("orig_dir", &self.orig_dir)
            .field("stashed", &self.stashed)
            .finish_non_exhaustive()
    }
}

fn canonical_path(path: &Path, label: &str) -> Result<PathBuf, String> {
    path.canonicalize()
        .map_err(|error| format!("failed to resolve {label} {}: {error}", path.display()))
}

fn ensure_worktree_is_not_process_workspace(worktree_path: &Path) -> Result<(), String> {
    let Ok(worktree_path) = worktree_path.canonicalize() else {
        return Ok(());
    };
    let current_dir = std::env::current_dir()
        .map_err(|error| format!("cannot verify process workspace before cleanup: {error}"))?;
    if current_dir.starts_with(&worktree_path) {
        return Err(format!(
            "refusing to remove active process workspace {}; transition to the main repository first",
            worktree_path.display()
        ));
    }
    Ok(())
}

async fn lock_process_workspace() -> OwnedMutexGuard<()> {
    PROCESS_WORKSPACE_LOCK
        .get_or_init(|| Arc::new(Mutex::new(())))
        .clone()
        .lock_owned()
        .await
}

async fn acquire_repository(repo_path: &Path) -> Result<OwnedMutexGuard<()>, String> {
    GitRunner::default().acquire_mutation(repo_path).await
}

fn trim_line(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 && matches!(bytes[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    &bytes[..end]
}

#[cfg(unix)]
fn output_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(OsString::from_vec(trim_line(bytes).to_vec()))
}

#[cfg(not(unix))]
fn output_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(trim_line(bytes)).into_owned())
}

async fn run_query<I, S>(
    repo_path: &Path,
    operation: &'static str,
    args: I,
) -> Result<CommandOutput, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    GitRunner::default()
        .run(repo_path, operation, args, QUERY_LIMITS)
        .await
}

async fn run_local<I, S>(
    repo_path: &Path,
    operation: &'static str,
    args: I,
) -> Result<CommandOutput, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    GitRunner::default()
        .run(repo_path, operation, args, LOCAL_MUTATION_LIMITS)
        .await
}

async fn run_network<I, S>(
    repo_path: &Path,
    operation: &'static str,
    args: I,
) -> Result<CommandOutput, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    GitRunner::default()
        .run(repo_path, operation, args, NETWORK_LIMITS)
        .await
}

async fn current_branch_at(repo_path: &Path) -> Result<Option<String>, String> {
    let output = GitRunner::default()
        .run_allow_exit(
            repo_path,
            "current-branch",
            ["symbolic-ref", "--quiet", "--short", "HEAD"],
            QUERY_LIMITS,
        )
        .await?;
    match output.exit_status.and_then(|status| status.code()) {
        Some(0) => {}
        Some(1) => return Ok(None),
        _ => return Err(command_failure("current-branch", &output)),
    }
    let branch = String::from_utf8_lossy(trim_line(&output.stdout)).into_owned();
    Ok(Some(branch))
}

async fn validate_branch_name(repo_path: &Path, branch: &str) -> Result<(), String> {
    if branch.is_empty() {
        return Err("Git branch name must not be empty".to_string());
    }
    run_query(
        repo_path,
        "validate-branch",
        ["check-ref-format", "--branch", branch],
    )
    .await
    .map(|_| ())
    .map_err(|_| format!("invalid Git branch name: {branch:?}"))
}

pub async fn detect(repo_path: &Path) -> Option<WorktreeInfo> {
    let repo_path = canonical_path(repo_path, "repository").ok()?;
    let common = run_query(
        &repo_path,
        "common-dir",
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await
    .ok()?;
    let git_dir = run_query(
        &repo_path,
        "git-dir",
        ["rev-parse", "--path-format=absolute", "--git-dir"],
    )
    .await
    .ok()?;
    let common_dir = canonical_path(&output_path(&common.stdout), "common Git directory").ok()?;
    let git_dir = canonical_path(&output_path(&git_dir.stdout), "Git directory").ok()?;
    if common_dir == git_dir {
        return None;
    }

    let root = run_query(
        &repo_path,
        "worktree-root",
        ["rev-parse", "--show-toplevel"],
    )
    .await
    .ok()?;
    let worktree_path = canonical_path(&output_path(&root.stdout), "worktree root").ok()?;
    let main_repo_path = canonical_path(common_dir.parent()?, "main repository").ok()?;
    let branch = current_branch(&repo_path).await?;

    Some(WorktreeInfo {
        branch,
        worktree_path,
        main_repo_path,
    })
}

pub async fn current_branch(repo_path: &Path) -> Option<String> {
    let repo_path = canonical_path(repo_path, "repository").ok()?;
    current_branch_at(&repo_path).await.ok().flatten()
}

pub async fn default_branch(repo_path: &Path) -> Option<String> {
    let repo_path = canonical_path(repo_path, "repository").ok()?;
    for name in ["main", "master"] {
        if run_query(&repo_path, "verify-branch", ["rev-parse", "--verify", name])
            .await
            .is_ok()
        {
            return Some(name.to_string());
        }
    }
    None
}

pub async fn create(
    repo_path: &Path,
    name: &str,
    base_dir: Option<&Path>,
) -> Result<(PathBuf, WorktreeInfo), String> {
    create_with_limits(
        repo_path,
        name,
        base_dir,
        LOCAL_MUTATION_LIMITS,
        LOCAL_MUTATION_LIMITS,
    )
    .await
}

async fn create_with_limits(
    repo_path: &Path,
    name: &str,
    base_dir: Option<&Path>,
    ref_limits: CommandLimits,
    add_limits: CommandLimits,
) -> Result<(PathBuf, WorktreeInfo), String> {
    let repo_path = canonical_path(repo_path, "repository")?;
    let target = match base_dir {
        Some(directory) => canonical_path(directory, "worktree base directory")?.join(name),
        None => repo_path
            .parent()
            .ok_or_else(|| "repository has no parent for default worktree path".to_string())?
            .join(name),
    };
    validate_branch_name(&repo_path, name).await?;
    let guard = acquire_repository(&repo_path).await?;
    if target.exists() {
        return Err(format!(
            "worktree target already exists: {}",
            target.display()
        ));
    }
    let source_ref = format!("refs/heads/{name}");
    if optional_ref_oid(&repo_path, &source_ref).await?.is_some() {
        return Err(format!("branch already exists: {name}"));
    }
    let expected_branch_oid = revision_oid(&repo_path, "HEAD").await?;
    let state = CreateState {
        repo_path,
        target,
        branch: name.to_string(),
        expected_branch_oid,
        ownership_ref: format!("refs/mini-agent/worktree-create/{}", uuid::Uuid::new_v4()),
        branch_reservation_owned: false,
        branch_reservation_uncertain: false,
        _repository_guard: guard,
    };
    let (response_tx, response_rx) = oneshot::channel();
    tokio::spawn(async move {
        supervise_create(state, ref_limits, add_limits, response_tx).await;
    });
    response_rx.await.unwrap_or_else(|_| {
        Err("worktree-create supervisor stopped before returning a result".into())
    })
}

async fn optional_ref_oid(repo_path: &Path, reference: &str) -> Result<Option<String>, String> {
    let output = run_query(
        repo_path,
        "read-ref",
        [
            "for-each-ref",
            "--format=%(refname)%00%(symref)%00%(objectname)%00",
            reference,
        ],
    )
    .await?;
    let mut found = None;
    for record in output.stdout.split(|byte| *byte == b'\n') {
        if record.is_empty() {
            continue;
        }
        let fields: Vec<_> = record.split(|byte| *byte == 0).collect();
        if fields.len() < 3 || fields[0] != reference.as_bytes() {
            continue;
        }
        if !fields[1].is_empty() {
            return Err(format!(
                "symbolic ref {reference} is not allowed for a worktree transaction"
            ));
        }
        if found.is_some() {
            return Err(format!("ambiguous ref identity for {reference}"));
        }
        let oid = String::from_utf8_lossy(fields[2]).into_owned();
        if oid.is_empty() {
            return Err(format!("direct ref {reference} has no object OID"));
        }
        found = Some(oid);
    }
    Ok(found)
}

async fn direct_ref_oid(repo_path: &Path, reference: &str) -> Result<String, String> {
    optional_ref_oid(repo_path, reference)
        .await?
        .ok_or_else(|| format!("required direct ref does not exist: {reference}"))
}

async fn supervise_create(
    mut state: CreateState,
    ref_limits: CommandLimits,
    add_limits: CommandLimits,
    mut response_tx: oneshot::Sender<CreateResponse>,
) {
    let result = create_transaction(&mut state, ref_limits, add_limits, &mut response_tx).await;
    match result {
        Ok(result) => {
            if response_tx.send(Ok(result)).is_err() {
                let error = rollback_failed_create(
                    &state,
                    "worktree creation result abandoned by caller".to_string(),
                )
                .await;
                if error.contains("rollback failed") {
                    tracing::error!(%error, "worktree create: abandoned-result rollback was incomplete");
                }
            }
        }
        Err(CreateStepError::Command(error)) => {
            let error = rollback_failed_create(&state, error).await;
            let _ = response_tx.send(Err(error));
        }
        Err(CreateStepError::CallerDropped) => {
            let error =
                rollback_failed_create(&state, "worktree creation cancelled by caller".to_string())
                    .await;
            if error.contains("rollback failed") {
                tracing::error!(%error, "worktree create: caller-drop rollback was incomplete");
            }
        }
    }
}

async fn await_create_step<T, F>(
    response_tx: &mut oneshot::Sender<CreateResponse>,
    future: F,
) -> Result<T, CreateStepError>
where
    F: Future<Output = Result<T, String>>,
{
    tokio::select! {
        biased;
        _ = response_tx.closed() => Err(CreateStepError::CallerDropped),
        result = future => result.map_err(CreateStepError::Command),
    }
}

async fn create_transaction(
    state: &mut CreateState,
    ref_limits: CommandLimits,
    add_limits: CommandLimits,
    response_tx: &mut oneshot::Sender<CreateResponse>,
) -> Result<(PathBuf, WorktreeInfo), CreateStepError> {
    let source_ref = format!("refs/heads/{}", state.branch);
    let reservation = format!(
        "start\noption no-deref\ncreate {source_ref} {}\ncreate {} {}\nprepare\ncommit\n",
        state.expected_branch_oid, state.ownership_ref, state.expected_branch_oid
    )
    .into_bytes();
    match await_create_step(
        response_tx,
        GitRunner::default().run_with_input(
            &state.repo_path,
            "create-branch-ref",
            ["update-ref", "--stdin"],
            reservation,
            ref_limits,
        ),
    )
    .await
    {
        Ok(_) => state.branch_reservation_owned = true,
        Err(CreateStepError::Command(error)) => {
            state.branch_reservation_uncertain = reservation_error_is_uncertain(&error);
            return Err(CreateStepError::Command(error));
        }
        Err(CreateStepError::CallerDropped) => {
            state.branch_reservation_uncertain = true;
            return Err(CreateStepError::CallerDropped);
        }
    }
    if response_tx.is_closed() {
        return Err(CreateStepError::CallerDropped);
    }
    await_create_step(
        response_tx,
        GitRunner::default().run(
            &state.repo_path,
            "worktree-add",
            [
                OsString::from("worktree"),
                OsString::from("add"),
                state.target.as_os_str().to_os_string(),
                OsString::from(&state.branch),
            ],
            add_limits,
        ),
    )
    .await?;
    let worktree_path =
        canonical_path(&state.target, "worktree path").map_err(CreateStepError::Command)?;
    let common = await_create_step(
        response_tx,
        run_query(
            &state.repo_path,
            "common-dir",
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ),
    )
    .await?;
    let common_dir = canonical_path(&output_path(&common.stdout), "common Git directory")
        .map_err(CreateStepError::Command)?;
    let main_repo_path = canonical_path(
        common_dir.parent().ok_or_else(|| {
            CreateStepError::Command("common Git directory has no repository parent".to_string())
        })?,
        "main repository",
    )
    .map_err(CreateStepError::Command)?;
    await_create_step(
        response_tx,
        delete_owned_ref(
            &state.repo_path,
            &state.ownership_ref,
            &state.expected_branch_oid,
            "release-create-ownership",
        ),
    )
    .await?;
    Ok((
        worktree_path.clone(),
        WorktreeInfo {
            branch: state.branch.clone(),
            worktree_path,
            main_repo_path,
        },
    ))
}

fn reservation_error_is_uncertain(error: &str) -> bool {
    error.contains("timed out")
        || error.contains("was cancelled")
        || error.contains("output limit")
        || error.contains("runner failed")
}

async fn delete_owned_ref(
    repo_path: &Path,
    reference: &str,
    expected_oid: &str,
    operation: &'static str,
) -> Result<CommandOutput, String> {
    GitRunner::default()
        .run(
            repo_path,
            operation,
            ["update-ref", "--no-deref", "-d", reference, expected_oid],
            LOCAL_MUTATION_LIMITS,
        )
        .await
}

async fn rollback_failed_create(state: &CreateState, error: String) -> String {
    let mut rollback_errors = Vec::new();
    let _workspace_guard = lock_process_workspace().await;
    let registration = worktree_registration(&state.repo_path, &state.target)
        .await
        .unwrap_or_else(|registration_error| {
            rollback_errors.push(registration_error);
            None
        });
    if registration.is_none() && state.target.exists() {
        rollback_errors.push(format!(
            "unregistered target {} was retained because ownership could not be proven",
            state.target.display()
        ));
    }
    if registration.is_some()
        && let Err(active_error) = ensure_worktree_is_not_process_workspace(&state.target)
    {
        rollback_errors.push(active_error);
        return append_rollback_errors(error, rollback_errors);
    }
    let expected_ref = format!("refs/heads/{}", state.branch);
    let removable = if let Some(registration) = registration.as_ref() {
        let exact_registration = registration.head.as_deref() == Some(&state.expected_branch_oid)
            && registration.branch.as_deref() == Some(expected_ref.as_str());
        let exact_head = revision_oid(&state.target, "HEAD")
            .await
            .is_ok_and(|oid| oid == state.expected_branch_oid);
        let clean = match has_uncommitted_changes_at(&state.target).await {
            Ok(false) => has_untracked_or_ignored_at(&state.target)
                .await
                .map(|dirty| !dirty),
            Ok(true) => Ok(false),
            Err(error) => Err(error),
        };
        match (exact_registration, exact_head, clean) {
            (true, true, Ok(true)) => true,
            (_, _, Ok(_)) => {
                rollback_errors.push(format!(
                    "worktree {} was retained because its exact branch, HEAD, and clean state could not be proven",
                    state.target.display()
                ));
                false
            }
            (_, _, Err(status_error)) => {
                rollback_errors.push(format!(
                    "worktree {} was retained because clean status could not be verified: {status_error}",
                    state.target.display()
                ));
                false
            }
        }
    } else {
        false
    };
    let removed = if removable {
        match run_local(
            &state.repo_path,
            "rollback-worktree-add",
            [
                OsString::from("worktree"),
                OsString::from("remove"),
                state.target.as_os_str().to_os_string(),
            ],
        )
        .await
        {
            Ok(_) => true,
            Err(remove_error) => {
                rollback_errors.push(remove_error);
                false
            }
        }
    } else {
        false
    };
    if removed
        && let Err(prune_error) = run_local(
            &state.repo_path,
            "rollback-worktree-prune",
            ["worktree", "prune"],
        )
        .await
    {
        rollback_errors.push(prune_error);
    }
    let marker_matches = match optional_ref_oid(&state.repo_path, &state.ownership_ref).await {
        Ok(Some(oid)) if oid == state.expected_branch_oid => true,
        Ok(Some(oid)) => {
            rollback_errors.push(format!(
                "create ownership ref changed (expected {}, observed {oid}); branch retained for recovery",
                state.expected_branch_oid
            ));
            false
        }
        Ok(None) => false,
        Err(ref_error) => {
            rollback_errors.push(ref_error);
            false
        }
    };
    let ownership_proven =
        state.branch_reservation_owned || (state.branch_reservation_uncertain && marker_matches);
    let artifact_gone = (registration.is_none() && !state.target.exists()) || removed;
    if ownership_proven && artifact_gone {
        match optional_ref_oid(&state.repo_path, &expected_ref).await {
            Ok(Some(oid)) if oid == state.expected_branch_oid => {
                let delete_result = if marker_matches {
                    delete_create_refs(
                        &state.repo_path,
                        &expected_ref,
                        &state.ownership_ref,
                        &state.expected_branch_oid,
                    )
                    .await
                } else {
                    delete_owned_ref(
                        &state.repo_path,
                        &expected_ref,
                        &state.expected_branch_oid,
                        "rollback-created-ref",
                    )
                    .await
                    .map(|_| ())
                };
                if let Err(delete_error) = delete_result {
                    rollback_errors.push(delete_error);
                }
            }
            Ok(Some(oid)) => rollback_errors.push(format!(
                "created branch changed during worktree creation (expected {}, observed {oid}); retained refs/heads/{} for recovery",
                state.expected_branch_oid, state.branch
            )),
            Ok(None) => {
                if marker_matches
                    && let Err(marker_error) = delete_owned_ref(
                        &state.repo_path,
                        &state.ownership_ref,
                        &state.expected_branch_oid,
                        "release-orphaned-create-ownership",
                    )
                    .await
                {
                    rollback_errors.push(marker_error);
                }
            }
            Err(ref_error) => rollback_errors.push(ref_error),
        }
    }
    append_rollback_errors(error, rollback_errors)
}

async fn delete_create_refs(
    repo_path: &Path,
    branch_ref: &str,
    ownership_ref: &str,
    expected_oid: &str,
) -> Result<(), String> {
    let transaction = format!(
        "start\noption no-deref\ndelete {branch_ref} {expected_oid}\ndelete {ownership_ref} {expected_oid}\nprepare\ncommit\n"
    )
    .into_bytes();
    GitRunner::default()
        .run_with_input(
            repo_path,
            "rollback-created-refs",
            ["update-ref", "--stdin"],
            transaction,
            LOCAL_MUTATION_LIMITS,
        )
        .await
        .map(|_| ())
}

struct WorktreeRegistration {
    head: Option<String>,
    branch: Option<String>,
}

async fn worktree_registration(
    repo_path: &Path,
    target: &Path,
) -> Result<Option<WorktreeRegistration>, String> {
    let output = run_query(
        repo_path,
        "worktree-registration",
        ["worktree", "list", "--porcelain", "-z"],
    )
    .await?;
    Ok(worktree_record(&output.stdout, target))
}

fn worktree_record(output: &[u8], target: &Path) -> Option<WorktreeRegistration> {
    let mut path_matches = false;
    let mut head = None;
    let mut branch = None;
    for field in output.split(|byte| *byte == 0) {
        if field.is_empty() {
            if path_matches {
                return Some(WorktreeRegistration { head, branch });
            }
            path_matches = false;
            head = None;
            branch = None;
        } else if let Some(path) = field.strip_prefix(b"worktree ") {
            path_matches = output_path_bytes_equal(path, target);
        } else if let Some(value) = field.strip_prefix(b"HEAD ") {
            head = Some(String::from_utf8_lossy(value).into_owned());
        } else if let Some(value) = field.strip_prefix(b"branch ") {
            branch = Some(String::from_utf8_lossy(value).into_owned());
        }
    }
    path_matches.then_some(WorktreeRegistration { head, branch })
}

#[cfg(unix)]
fn output_path_bytes_equal(output: &[u8], path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    output == path.as_os_str().as_bytes()
}

#[cfg(not(unix))]
fn output_path_bytes_equal(output: &[u8], path: &Path) -> bool {
    output == path.to_string_lossy().as_bytes()
}

pub fn repo_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string())
}

pub async fn try_merge(info: &WorktreeInfo, target: &str) -> MergeResponse {
    try_merge_with_switch_limits(info, target, LOCAL_MUTATION_LIMITS).await
}

async fn try_merge_with_switch_limits(
    info: &WorktreeInfo,
    target: &str,
    switch_limits: CommandLimits,
) -> MergeResponse {
    let fallback_info = info.clone();
    let (response_tx, response_rx) = oneshot::channel();
    let owned_info = info.clone();
    let owned_target = target.to_string();
    tokio::spawn(async move {
        supervise_merge(owned_info, owned_target, switch_limits, response_tx).await;
    });
    response_rx.await.unwrap_or_else(|_| {
        (
            empty_merge_state(&fallback_info),
            MergeOutcome::Error("merge supervisor stopped before returning a result".to_string()),
        )
    })
}

async fn supervise_merge(
    info: WorktreeInfo,
    target: String,
    switch_limits: CommandLimits,
    mut response_tx: oneshot::Sender<MergeResponse>,
) {
    match try_merge_transaction(&info, &target, switch_limits, &mut response_tx).await {
        SupervisedMerge::Deliver(result) => {
            if let Err(mut abandoned) = response_tx.send(result) {
                rollback_abandoned_merge(&mut abandoned.0).await;
            }
        }
        SupervisedMerge::CallerDropped(mut state) => {
            rollback_abandoned_merge(&mut state).await;
        }
    }
}

async fn await_merge_step<T, F>(
    response_tx: &mut oneshot::Sender<MergeResponse>,
    future: F,
) -> Result<T, MergeStepError>
where
    F: Future<Output = Result<T, String>>,
{
    tokio::select! {
        biased;
        _ = response_tx.closed() => Err(MergeStepError::CallerDropped),
        result = future => result.map_err(MergeStepError::Command),
    }
}

async fn try_merge_transaction(
    info: &WorktreeInfo,
    target: &str,
    switch_limits: CommandLimits,
    response_tx: &mut oneshot::Sender<MergeResponse>,
) -> SupervisedMerge {
    let main_repo_path = match canonical_path(&info.main_repo_path, "main repository") {
        Ok(path) => path,
        Err(error) => {
            return SupervisedMerge::Deliver((empty_merge_state(info), MergeOutcome::Error(error)));
        }
    };
    match await_merge_step(response_tx, validate_branch_name(&main_repo_path, target)).await {
        Ok(()) => {}
        Err(MergeStepError::Command(error)) => {
            return SupervisedMerge::Deliver((empty_merge_state(info), MergeOutcome::Error(error)));
        }
        Err(MergeStepError::CallerDropped) => {
            return SupervisedMerge::CallerDropped(empty_merge_state(info));
        }
    }
    match await_merge_step(
        response_tx,
        validate_branch_name(&main_repo_path, &info.branch),
    )
    .await
    {
        Ok(()) => {}
        Err(MergeStepError::Command(error)) => {
            return SupervisedMerge::Deliver((empty_merge_state(info), MergeOutcome::Error(error)));
        }
        Err(MergeStepError::CallerDropped) => {
            return SupervisedMerge::CallerDropped(empty_merge_state(info));
        }
    }
    let guard = match await_merge_step(response_tx, acquire_repository(&main_repo_path)).await {
        Ok(guard) => guard,
        Err(MergeStepError::Command(error)) => {
            return SupervisedMerge::Deliver((empty_merge_state(info), MergeOutcome::Error(error)));
        }
        Err(MergeStepError::CallerDropped) => {
            return SupervisedMerge::CallerDropped(empty_merge_state(info));
        }
    };
    let mut state = state_with_guard(info, String::new(), false, guard);
    state.target_branch = target.to_string();
    state.original_head_oid =
        match await_merge_step(response_tx, revision_oid(&main_repo_path, "HEAD")).await {
            Ok(oid) => oid,
            Err(MergeStepError::Command(error)) => {
                state.repository_guard.take();
                return SupervisedMerge::Deliver((state, MergeOutcome::Error(error)));
            }
            Err(MergeStepError::CallerDropped) => return SupervisedMerge::CallerDropped(state),
        };
    let original_branch =
        match await_merge_step(response_tx, current_branch_at(&main_repo_path)).await {
            Ok(Some(branch)) => branch,
            Ok(None) => String::new(),
            Err(MergeStepError::Command(error)) => {
                state.repository_guard.take();
                return SupervisedMerge::Deliver((state, MergeOutcome::Error(error)));
            }
            Err(MergeStepError::CallerDropped) => return SupervisedMerge::CallerDropped(state),
        };
    state.original_branch = original_branch.clone();
    let source_ref = format!("refs/heads/{}", info.branch);
    state.source_head_oid =
        match await_merge_step(response_tx, direct_ref_oid(&main_repo_path, &source_ref)).await {
            Ok(oid) => oid,
            Err(MergeStepError::Command(error)) => {
                state.repository_guard.take();
                return SupervisedMerge::Deliver((state, MergeOutcome::Error(error)));
            }
            Err(MergeStepError::CallerDropped) => return SupervisedMerge::CallerDropped(state),
        };
    let target_ref = format!("refs/heads/{target}");
    match await_merge_step(response_tx, direct_ref_oid(&main_repo_path, &target_ref)).await {
        Ok(_) => {}
        Err(MergeStepError::Command(error)) => {
            state.repository_guard.take();
            return SupervisedMerge::Deliver((state, MergeOutcome::Error(error)));
        }
        Err(MergeStepError::CallerDropped) => return SupervisedMerge::CallerDropped(state),
    }
    let has_changes = match await_merge_step(response_tx, async {
        let status = run_query(
            &main_repo_path,
            "status",
            ["status", "--porcelain", "--untracked-files=all"],
        )
        .await?;
        if trim_line(&status.stdout).is_empty() {
            return Ok(false);
        }
        let untracked = run_query(
            &main_repo_path,
            "untracked-safety-scan",
            ["ls-files", "--others", "-z"],
        )
        .await?;
        if !untracked.stdout.is_empty() {
            return Err(
                "merge refused because the main workspace contains untracked or ignored files; commit or stash them manually"
                    .to_string(),
            );
        }
        Ok(true)
    })
    .await
    {
        Ok(has_changes) => has_changes,
        Err(MergeStepError::Command(error)) => {
            state.repository_guard.take();
            return SupervisedMerge::Deliver((state, MergeOutcome::Error(error)));
        }
        Err(MergeStepError::CallerDropped) => return SupervisedMerge::CallerDropped(state),
    };
    if has_changes {
        state.stash_before =
            match await_merge_step(response_tx, optional_ref_oid(&main_repo_path, "refs/stash"))
                .await
            {
                Ok(stash) => stash,
                Err(MergeStepError::Command(error)) => {
                    state.repository_guard.take();
                    return SupervisedMerge::Deliver((state, MergeOutcome::Error(error)));
                }
                Err(MergeStepError::CallerDropped) => return SupervisedMerge::CallerDropped(state),
            };
        state.stash_attempted = true;
        match await_merge_step(
            response_tx,
            create_and_publish_stash(&main_repo_path, state.stash_before.as_deref()),
        )
        .await
        {
            Ok(Some(oid)) => {
                state.stashed = true;
                state.stash_created = Some(oid);
                match await_merge_step(
                    response_tx,
                    run_local(
                        &main_repo_path,
                        "clean-published-stash",
                        ["reset", "--hard", state.original_head_oid.as_str()],
                    ),
                )
                .await
                {
                    Ok(_) => {}
                    Err(MergeStepError::Command(error)) => {
                        let response = early_merge_error(
                            &mut state,
                            format!("published stash could not clean the workspace: {error}"),
                        )
                        .await;
                        return SupervisedMerge::Deliver(response);
                    }
                    Err(MergeStepError::CallerDropped) => {
                        return SupervisedMerge::CallerDropped(state);
                    }
                }
            }
            Ok(None) => {
                // `stash create` intentionally ignores untracked-only changes.
                // They remain in place and Git will fail closed if a branch
                // switch would overwrite one of them.
                state.stash_attempted = false;
            }
            Err(MergeStepError::Command(error)) => {
                let response =
                    early_merge_error(&mut state, format!("stash failed: {error}")).await;
                return SupervisedMerge::Deliver(response);
            }
            Err(MergeStepError::CallerDropped) => return SupervisedMerge::CallerDropped(state),
        }
    }

    match await_merge_step(
        response_tx,
        run_network(&main_repo_path, "fetch", ["fetch", "--all"]),
    )
    .await
    {
        Ok(_) => {}
        Err(MergeStepError::Command(error)) => {
            return SupervisedMerge::Deliver(
                early_merge_error(&mut state, format!("fetch failed: {error}")).await,
            );
        }
        Err(MergeStepError::CallerDropped) => return SupervisedMerge::CallerDropped(state),
    }
    state.branch_switch_attempted = true;
    match await_merge_step(
        response_tx,
        GitRunner::default().run(
            &main_repo_path,
            "checkout",
            ["switch", "--", target],
            switch_limits,
        ),
    )
    .await
    {
        Ok(_) => {}
        Err(MergeStepError::Command(error)) => {
            return SupervisedMerge::Deliver(
                early_merge_error_after_branch_change(
                    &mut state,
                    format!("checkout failed: {error}"),
                    Vec::new(),
                )
                .await,
            );
        }
        Err(MergeStepError::CallerDropped) => return SupervisedMerge::CallerDropped(state),
    }
    state.target_head_before_pull = match await_merge_step(
        response_tx,
        verify_target_checkout(&main_repo_path, target),
    )
    .await
    {
        Ok(oid) => Some(oid),
        Err(MergeStepError::Command(error)) => {
            return SupervisedMerge::Deliver(
                early_merge_error_after_branch_change(&mut state, error, Vec::new()).await,
            );
        }
        Err(MergeStepError::CallerDropped) => return SupervisedMerge::CallerDropped(state),
    };
    state.index_mutation_attempted = true;
    match await_merge_step(
        response_tx,
        run_network(
            &main_repo_path,
            "pull",
            ["pull", "--no-edit", "--no-rebase"],
        ),
    )
    .await
    {
        Ok(_) => {}
        Err(MergeStepError::Command(error)) => {
            let mut rollback_errors = Vec::new();
            if let Err(reset_error) = rollback_merge_index(&state).await {
                rollback_errors.push(reset_error);
            }
            return SupervisedMerge::Deliver(
                early_merge_error_after_branch_change(
                    &mut state,
                    format!("pull failed: {error}"),
                    rollback_errors,
                )
                .await,
            );
        }
        Err(MergeStepError::CallerDropped) => return SupervisedMerge::CallerDropped(state),
    }

    match await_merge_step(response_tx, direct_ref_oid(&main_repo_path, &target_ref)).await {
        Ok(head) => state.target_head_before_merge = Some(head),
        Err(MergeStepError::Command(error)) => {
            let rollback_errors = rollback_merge_index(&state)
                .await
                .err()
                .into_iter()
                .collect();
            return SupervisedMerge::Deliver(
                early_merge_error_after_branch_change(&mut state, error, rollback_errors).await,
            );
        }
        Err(MergeStepError::CallerDropped) => return SupervisedMerge::CallerDropped(state),
    }
    if let Err(error) = await_merge_step(
        response_tx,
        verify_target_checkout_at(
            &main_repo_path,
            target,
            state
                .target_head_before_merge
                .as_deref()
                .expect("target ref was captured after pull"),
        ),
    )
    .await
    {
        return match error {
            MergeStepError::Command(error) => {
                let rollback_errors = rollback_merge_index(&state)
                    .await
                    .err()
                    .into_iter()
                    .collect();
                SupervisedMerge::Deliver(
                    early_merge_error_after_branch_change(&mut state, error, rollback_errors).await,
                )
            }
            MergeStepError::CallerDropped => SupervisedMerge::CallerDropped(state),
        };
    }

    match await_merge_step(
        response_tx,
        run_local(
            &main_repo_path,
            "merge-squash",
            ["merge", "--squash", "--", info.branch.as_str()],
        ),
    )
    .await
    {
        Ok(_) => {
            let pre_merge_head = state
                .target_head_before_merge
                .clone()
                .expect("pre-merge HEAD was captured before squash");
            if let Err(error) = await_merge_step(
                response_tx,
                verify_target_checkout_at(&main_repo_path, target, &pre_merge_head),
            )
            .await
            {
                return match error {
                    MergeStepError::Command(error) => {
                        let rollback_errors = rollback_merge_index(&state)
                            .await
                            .err()
                            .into_iter()
                            .collect();
                        SupervisedMerge::Deliver(
                            early_merge_error_after_branch_change(
                                &mut state,
                                error,
                                rollback_errors,
                            )
                            .await,
                        )
                    }
                    MergeStepError::CallerDropped => SupervisedMerge::CallerDropped(state),
                };
            }
            let (expected_tree, index_tree, target_tree) = match await_merge_step(
                response_tx,
                squash_tree_state(&main_repo_path, &pre_merge_head, &state.source_head_oid),
            )
            .await
            {
                Ok(trees) => trees,
                Err(MergeStepError::Command(error)) => {
                    let rollback_errors = rollback_merge_index(&state)
                        .await
                        .err()
                        .into_iter()
                        .collect();
                    return SupervisedMerge::Deliver(
                        early_merge_error_after_branch_change(
                            &mut state,
                            format!("could not verify squash result: {error}"),
                            rollback_errors,
                        )
                        .await,
                    );
                }
                Err(MergeStepError::CallerDropped) => {
                    return SupervisedMerge::CallerDropped(state);
                }
            };
            if index_tree != expected_tree {
                let rollback_errors = rollback_merge_index(&state)
                    .await
                    .err()
                    .into_iter()
                    .collect();
                return SupervisedMerge::Deliver(
                    early_merge_error_after_branch_change(
                        &mut state,
                        "squash index tree did not match the captured source and target"
                            .to_string(),
                        rollback_errors,
                    )
                    .await,
                );
            }
            if index_tree == target_tree {
                state.successful_merge_head = Some(pre_merge_head);
                return SupervisedMerge::Deliver((state, MergeOutcome::Success));
            }
            match await_merge_step(
                response_tx,
                run_local(&main_repo_path, "merge-commit", ["commit", "--no-edit"]),
            )
            .await
            {
                Ok(_) => match await_merge_step(
                    response_tx,
                    verify_squash_commit(
                        &main_repo_path,
                        &target_ref,
                        &pre_merge_head,
                        &expected_tree,
                    ),
                )
                .await
                {
                    Ok(oid) => {
                        state.target_head_after_commit = Some(oid.clone());
                        match await_merge_step(
                            response_tx,
                            verify_target_checkout_at(&main_repo_path, target, &oid),
                        )
                        .await
                        {
                            Ok(_) => {
                                state.successful_merge_head = Some(oid);
                                SupervisedMerge::Deliver((state, MergeOutcome::Success))
                            }
                            Err(MergeStepError::Command(error)) => {
                                let rollback_errors = rollback_merge_index(&state)
                                    .await
                                    .err()
                                    .into_iter()
                                    .collect();
                                SupervisedMerge::Deliver(
                                    early_merge_error_after_branch_change(
                                        &mut state,
                                        error,
                                        rollback_errors,
                                    )
                                    .await,
                                )
                            }
                            Err(MergeStepError::CallerDropped) => {
                                SupervisedMerge::CallerDropped(state)
                            }
                        }
                    }
                    Err(MergeStepError::Command(error)) => {
                        let rollback_errors = rollback_merge_index(&state)
                            .await
                            .err()
                            .into_iter()
                            .collect();
                        SupervisedMerge::Deliver(
                            early_merge_error_after_branch_change(
                                &mut state,
                                format!("squash commit verification failed: {error}"),
                                rollback_errors,
                            )
                            .await,
                        )
                    }
                    Err(MergeStepError::CallerDropped) => SupervisedMerge::CallerDropped(state),
                },
                Err(MergeStepError::Command(error)) => {
                    let mut rollback_errors = Vec::new();
                    if let Err(rollback_error) = rollback_merge_index(&state).await {
                        rollback_errors.push(rollback_error);
                    }
                    SupervisedMerge::Deliver(
                        early_merge_error_after_branch_change(
                            &mut state,
                            format!("commit after squash failed: {error}"),
                            rollback_errors,
                        )
                        .await,
                    )
                }
                Err(MergeStepError::CallerDropped) => SupervisedMerge::CallerDropped(state),
            }
        }
        Err(MergeStepError::Command(error)) => {
            let conflict = match await_merge_step(response_tx, async {
                Ok(has_merge_conflict_at(&main_repo_path).await)
            })
            .await
            {
                Ok(conflict) => conflict,
                Err(MergeStepError::Command(_)) => false,
                Err(MergeStepError::CallerDropped) => {
                    return SupervisedMerge::CallerDropped(state);
                }
            };
            if conflict {
                let files = match await_merge_step(response_tx, async {
                    Ok(conflicted_files_at(&main_repo_path).await)
                })
                .await
                {
                    Ok(files) => files,
                    Err(MergeStepError::Command(_)) => Vec::new(),
                    Err(MergeStepError::CallerDropped) => {
                        return SupervisedMerge::CallerDropped(state);
                    }
                };
                SupervisedMerge::Deliver((state, MergeOutcome::Conflicts(files)))
            } else {
                tracing::error!(
                    branch = %info.branch,
                    target,
                    error = %error,
                    "worktree merge: merge failed, rolling back"
                );
                let mut rollback_errors = Vec::new();
                if let Err(rollback_error) = rollback_merge_index(&state).await {
                    rollback_errors.push(rollback_error);
                }
                SupervisedMerge::Deliver(
                    early_merge_error_after_branch_change(
                        &mut state,
                        format!("merge failed: {error}"),
                        rollback_errors,
                    )
                    .await,
                )
            }
        }
        Err(MergeStepError::CallerDropped) => SupervisedMerge::CallerDropped(state),
    }
}

async fn squash_tree_state(
    repo_path: &Path,
    target_oid: &str,
    source_oid: &str,
) -> Result<(String, String, String), String> {
    let expected = run_query(
        repo_path,
        "expected-squash-tree",
        ["merge-tree", "--write-tree", target_oid, source_oid],
    )
    .await?;
    let expected = String::from_utf8_lossy(trim_line(&expected.stdout)).into_owned();
    if expected.is_empty() || expected.split_ascii_whitespace().count() != 1 {
        return Err("git merge-tree returned an invalid tree OID".to_string());
    }
    let index = run_query(repo_path, "squash-index-tree", ["write-tree"]).await?;
    let index = String::from_utf8_lossy(trim_line(&index.stdout)).into_owned();
    let target_tree = revision_oid(repo_path, &format!("{target_oid}^{{tree}}")).await?;
    Ok((expected, index, target_tree))
}

async fn verify_squash_commit(
    repo_path: &Path,
    target_ref: &str,
    expected_parent: &str,
    expected_tree: &str,
) -> Result<String, String> {
    let final_oid = direct_ref_oid(repo_path, target_ref).await?;
    let parents = run_query(
        repo_path,
        "verify-squash-parent",
        ["rev-list", "--parents", "-n", "1", final_oid.as_str()],
    )
    .await?;
    let fields: Vec<_> = String::from_utf8_lossy(trim_line(&parents.stdout))
        .split_ascii_whitespace()
        .map(str::to_string)
        .collect();
    if fields.len() != 2 || fields[0] != final_oid || fields[1] != expected_parent {
        return Err(
            "final target ref was not a single-parent commit on the captured target".to_string(),
        );
    }
    let actual_tree = revision_oid(repo_path, &format!("{final_oid}^{{tree}}")).await?;
    if actual_tree != expected_tree {
        return Err("final commit tree did not match the exact squash result".to_string());
    }
    Ok(final_oid)
}

async fn verify_target_checkout(repo_path: &Path, target: &str) -> Result<String, String> {
    let target_ref = format!("refs/heads/{target}");
    let target_oid = direct_ref_oid(repo_path, &target_ref).await?;
    verify_target_checkout_at(repo_path, target, &target_oid).await?;
    Ok(target_oid)
}

async fn verify_target_checkout_at(
    repo_path: &Path,
    target: &str,
    expected_oid: &str,
) -> Result<(), String> {
    match current_branch_at(repo_path).await? {
        Some(branch) if branch == target => {}
        Some(branch) => {
            return Err(format!(
                "target branch was not checked out after a hook-capable Git operation (expected {target:?}, observed {branch:?})"
            ));
        }
        None => {
            return Err(format!(
                "target branch was not checked out after a hook-capable Git operation (expected {target:?}, observed detached HEAD)"
            ));
        }
    }
    let target_ref = format!("refs/heads/{target}");
    let target_oid = direct_ref_oid(repo_path, &target_ref).await?;
    if target_oid != expected_oid {
        return Err(format!(
            "target ref changed after a hook-capable Git operation (expected {expected_oid}, observed {target_oid})"
        ));
    }
    let head_oid = revision_oid(repo_path, "HEAD").await?;
    if head_oid != expected_oid {
        return Err(format!(
            "HEAD no longer matched the exact target ref (expected {expected_oid}, observed {head_oid})"
        ));
    }
    Ok(())
}

fn append_rollback_errors(error: String, rollback_errors: Vec<String>) -> String {
    if rollback_errors.is_empty() {
        error
    } else {
        format!("{error}; rollback failed: {}", rollback_errors.join("; "))
    }
}

async fn revision_oid(repo_path: &Path, revision: &str) -> Result<String, String> {
    run_query(
        repo_path,
        "resolve-revision",
        ["rev-parse", "--verify", revision],
    )
    .await
    .map(|output| String::from_utf8_lossy(trim_line(&output.stdout)).into_owned())
}

async fn rollback_merge_index(state: &MergeState) -> Result<(), String> {
    let Some(rollback_oid) = state
        .target_head_before_pull
        .as_deref()
        .or(state.target_head_before_merge.as_deref())
    else {
        return Err(
            "merge state was retained because no exact target rollback OID was captured"
                .to_string(),
        );
    };
    let target_ref = format!("refs/heads/{}", state.target_branch);
    #[cfg(test)]
    if let Some(gate) = state.rollback_test_gate.as_ref() {
        gate.reached.notify_one();
        gate.resume.notified().await;
    }
    let observed_target = optional_ref_oid(&state.info.main_repo_path, &target_ref).await?;
    if observed_target.is_none() {
        compare_and_set_direct_ref(
            &state.info.main_repo_path,
            &target_ref,
            rollback_oid,
            "0000000000000000000000000000000000000000",
            "restore-missing-target-ref",
        )
        .await?;
        return Err(
            "missing target ref was restored safely, but index/tree state was retained for recovery"
                .to_string(),
        );
    }
    let observed_target = observed_target.expect("missing target handled above");
    let observed_is_owned = state.target_head_before_pull.as_deref() == Some(&observed_target)
        || state.target_head_before_merge.as_deref() == Some(&observed_target)
        || state.target_head_after_commit.as_deref() == Some(&observed_target);
    if !observed_is_owned {
        return Err(format!(
            "target ref and index were retained for recovery because target OID {observed_target} was not owned by this transaction"
        ));
    }
    if observed_target != rollback_oid {
        compare_and_set_direct_ref(
            &state.info.main_repo_path,
            &target_ref,
            rollback_oid,
            &observed_target,
            "rollback-target-ref",
        )
        .await?;
    }
    Err(
        "target ref was rolled back safely, but index/tree state was retained for recovery"
            .to_string(),
    )
}

async fn compare_and_set_direct_ref(
    repo_path: &Path,
    reference: &str,
    new_oid: &str,
    expected_old_oid: &str,
    operation: &'static str,
) -> Result<(), String> {
    GitRunner::default()
        .run(
            repo_path,
            operation,
            [
                "update-ref",
                "--no-deref",
                reference,
                new_oid,
                expected_old_oid,
            ],
            LOCAL_MUTATION_LIMITS,
        )
        .await
        .map(|_| ())
}

/// Create the stash commit without publishing it, then publish that exact OID
/// only if `refs/stash` still has the value captured by this transaction.
/// This prevents a concurrent external stash from ever being mistaken for an
/// object owned by the merge transaction.
async fn create_and_publish_stash(
    repo_path: &Path,
    stash_before: Option<&str>,
) -> Result<Option<String>, String> {
    let created = run_query(repo_path, "stash-create", ["stash", "create"]).await?;
    let created = String::from_utf8_lossy(trim_line(&created.stdout)).into_owned();
    if created.is_empty() {
        return Ok(None);
    }
    if created.split_ascii_whitespace().count() != 1
        || revision_oid(repo_path, &created).await? != created
    {
        return Err("git stash create returned an invalid object OID".to_string());
    }

    #[cfg(test)]
    let publish_gate = {
        let mut slot = STASH_PUBLISH_TEST_GATE
            .get_or_init(|| StdMutex::new(None))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot
            .as_ref()
            .is_some_and(|entry| entry.repo_path == repo_path)
        {
            slot.take().map(|entry| entry.gate)
        } else {
            None
        }
    };
    #[cfg(test)]
    if let Some(gate) = publish_gate {
        gate.reached.notify_one();
        gate.resume.notified().await;
    }

    GitRunner::default()
        .run(
            repo_path,
            "publish-created-stash",
            [
                "update-ref",
                "--no-deref",
                "--create-reflog",
                "-m",
                "mini-agent merge safety stash",
                "refs/stash",
                created.as_str(),
                stash_before.unwrap_or(ZERO_OID),
            ],
            LOCAL_MUTATION_LIMITS,
        )
        .await?;
    Ok(Some(created))
}

async fn restore_stash_after_abandonment(state: &mut MergeState) -> Result<(), String> {
    let current_stash = optional_ref_oid(&state.info.main_repo_path, "refs/stash").await?;
    let stash_changed =
        state.stash_attempted && current_stash.is_some() && current_stash != state.stash_before;
    if state.stashed || stash_changed {
        let expected = state.stash_created.as_ref().ok_or_else(|| {
            "stash retained because the newly created stash OID was not verified".to_string()
        })?;
        if current_stash.as_ref() != Some(expected) {
            return Err(
                "stash retained because refs/stash changed after the merge transaction captured it"
                    .to_string(),
            );
        }
        run_local(
            &state.info.main_repo_path,
            "stash-apply-exact",
            ["stash", "apply", "--index", expected.as_str()],
        )
        .await?;
        #[cfg(test)]
        if let Some(gate) = state.stash_test_gate.as_ref() {
            gate.reached.notify_one();
            gate.resume.notified().await;
        }
        let after_apply = optional_ref_oid(&state.info.main_repo_path, "refs/stash").await?;
        if after_apply.as_ref() != Some(expected) {
            return Err(
                "exact captured stash was applied but retained because refs/stash changed concurrently"
                    .to_string(),
            );
        }
        verify_applied_stash_exact(&state.info.main_repo_path, expected).await?;
        if let Some(previous) = state.stash_before.as_deref() {
            compare_and_set_direct_ref(
                &state.info.main_repo_path,
                "refs/stash",
                previous,
                expected,
                "restore-stash-ref",
            )
            .await?;
        } else {
            delete_owned_ref(
                &state.info.main_repo_path,
                "refs/stash",
                expected,
                "drop-exact-restored-stash",
            )
            .await?;
        }
        state.stashed = false;
        state.stash_created = None;
    }
    Ok(())
}

async fn verify_applied_stash_exact(repo_path: &Path, stash_oid: &str) -> Result<(), String> {
    let expected_index = revision_oid(repo_path, &format!("{stash_oid}^2^{{tree}}")).await?;
    let actual_index = run_query(repo_path, "verify-restored-stash-index", ["write-tree"]).await?;
    if String::from_utf8_lossy(trim_line(&actual_index.stdout)) != expected_index {
        return Err(
            "restored stash retained because the index changed after exact apply".to_string(),
        );
    }
    let expected_worktree = revision_oid(repo_path, &format!("{stash_oid}^{{tree}}")).await?;
    let worktree = GitRunner::default()
        .run_allow_exit(
            repo_path,
            "verify-restored-stash-worktree",
            ["diff", "--quiet", expected_worktree.as_str(), "--"],
            QUERY_LIMITS,
        )
        .await?;
    if !worktree.exit_status.is_some_and(|status| status.success()) {
        return Err(
            "restored stash retained because tracked workspace content changed after exact apply"
                .to_string(),
        );
    }
    let untracked = run_query(
        repo_path,
        "verify-restored-stash-untracked",
        ["ls-files", "--others", "-z"],
    )
    .await?;
    if !untracked.stdout.is_empty() {
        return Err(
            "restored stash retained because untracked or ignored workspace content appeared after exact apply"
                .to_string(),
        );
    }
    Ok(())
}

async fn rollback_abandoned_merge(state: &mut MergeState) {
    if state.repository_guard.is_none() {
        return;
    }
    let mut errors = Vec::new();
    let mut rollback_clean = true;
    if state.index_mutation_attempted
        && let Err(error) = rollback_merge_index(state).await
    {
        errors.push(error);
        rollback_clean = false;
    }
    let branch_restored = if state.branch_switch_attempted {
        let (restored, restore_errors) = restore_and_verify_original(state).await;
        errors.extend(restore_errors);
        restored
    } else {
        true
    };
    if branch_restored && rollback_clean {
        if let Err(error) = restore_stash_after_abandonment(state).await {
            errors.push(error);
        }
    } else if state.stashed || state.stash_attempted {
        errors.push("stash retained because the original branch was not verified".to_string());
    }
    if !errors.is_empty() {
        tracing::error!(
            branch = %state.info.branch,
            errors = %errors.join("; "),
            "worktree merge: caller-drop rollback was incomplete"
        );
    }
    state.repository_guard.take();
}

fn empty_merge_state(info: &WorktreeInfo) -> MergeState {
    MergeState {
        info: info.clone(),
        original_branch: String::new(),
        orig_dir: info.worktree_path.clone(),
        stashed: false,
        stash_before: None,
        stash_created: None,
        stash_attempted: false,
        branch_switch_attempted: false,
        index_mutation_attempted: false,
        original_head_oid: String::new(),
        source_head_oid: String::new(),
        target_branch: String::new(),
        target_head_before_pull: None,
        target_head_before_merge: None,
        target_head_after_commit: None,
        successful_merge_head: None,
        repository_guard: None,
        #[cfg(test)]
        rollback_test_gate: None,
        #[cfg(test)]
        stash_test_gate: None,
    }
}

#[cfg(test)]
pub(crate) fn empty_state_for_ui(info: &WorktreeInfo) -> MergeState {
    empty_merge_state(info)
}

fn state_with_guard(
    info: &WorktreeInfo,
    original_branch: String,
    stashed: bool,
    guard: OwnedMutexGuard<()>,
) -> MergeState {
    MergeState {
        info: info.clone(),
        original_branch,
        orig_dir: info.worktree_path.clone(),
        stashed,
        stash_before: None,
        stash_created: None,
        stash_attempted: false,
        branch_switch_attempted: false,
        index_mutation_attempted: false,
        original_head_oid: String::new(),
        source_head_oid: String::new(),
        target_branch: String::new(),
        target_head_before_pull: None,
        target_head_before_merge: None,
        target_head_after_commit: None,
        successful_merge_head: None,
        repository_guard: Some(guard),
        #[cfg(test)]
        rollback_test_gate: None,
        #[cfg(test)]
        stash_test_gate: None,
    }
}

async fn early_merge_error(state: &mut MergeState, error: String) -> (MergeState, MergeOutcome) {
    let mut error = error;
    if let Err(stash_error) = restore_stash_after_abandonment(state).await {
        tracing::error!(
            branch = %state.info.branch,
            error = %stash_error,
            "worktree merge: failed to restore stash during early cleanup; stashed changes may require a manual `git stash pop`"
        );
        error = append_rollback_errors(error, vec![stash_error]);
    }
    state.repository_guard.take();
    (
        std::mem::replace(state, empty_merge_state(&state.info)),
        MergeOutcome::Error(error),
    )
}

async fn early_merge_error_after_branch_change(
    state: &mut MergeState,
    error: String,
    mut rollback_errors: Vec<String>,
) -> (MergeState, MergeOutcome) {
    let rollback_clean = rollback_errors.is_empty();
    let (restored, restore_errors) = restore_and_verify_original(state).await;
    let restore_clean = restore_errors.is_empty();
    rollback_errors.extend(restore_errors);
    let mut error = append_rollback_errors(error, rollback_errors);
    if restored && rollback_clean && restore_clean {
        return early_merge_error(state, error).await;
    }
    if state.stashed || state.stash_attempted {
        error.push_str("; stash retained because rollback cleanliness was not verified");
    }
    state.repository_guard.take();
    (
        std::mem::replace(state, empty_merge_state(&state.info)),
        MergeOutcome::Error(error),
    )
}

async fn restore_and_verify_original(state: &MergeState) -> (bool, Vec<String>) {
    let repo_path = &state.info.main_repo_path;
    let mut errors = Vec::new();
    let restore = if state.original_branch.is_empty() {
        run_local(
            repo_path,
            "restore-detached-head",
            ["switch", "--detach", state.original_head_oid.as_str()],
        )
        .await
    } else {
        run_local(
            repo_path,
            "restore-branch",
            ["switch", "--", state.original_branch.as_str()],
        )
        .await
    };
    if let Err(error) = restore {
        errors.push(error);
    }
    let branch_matches = match current_branch_at(repo_path).await {
        Ok(None) if state.original_branch.is_empty() => true,
        Ok(Some(current)) if current == state.original_branch => true,
        Ok(Some(current)) => {
            errors.push(format!(
                "branch verification failed: expected {:?}, observed {current:?}",
                state.original_branch
            ));
            false
        }
        Ok(None) => {
            errors.push(format!(
                "branch verification failed: expected {:?}, observed detached HEAD",
                state.original_branch
            ));
            false
        }
        Err(error) => {
            errors.push(format!("branch verification failed: {error}"));
            false
        }
    };
    let oid_matches = match revision_oid(repo_path, "HEAD").await {
        Ok(oid) if oid == state.original_head_oid => true,
        Ok(oid) => {
            errors.push(format!(
                "HEAD verification failed: expected {}, observed {oid}",
                state.original_head_oid
            ));
            false
        }
        Err(error) => {
            errors.push(format!("HEAD verification failed: {error}"));
            false
        }
    };
    (branch_matches && oid_matches, errors)
}

pub async fn complete_merge(state: &mut MergeState) -> Result<(), String> {
    complete_merge_with_force(state, false).await
}

pub async fn complete_merge_force(state: &mut MergeState) -> Result<(), String> {
    // Compatibility entry point: destructive dirty-worktree cleanup is no
    // longer supported, even when the deprecated flag is configured.
    complete_merge_with_force(state, true).await
}

async fn complete_merge_with_force(state: &mut MergeState, _force: bool) -> Result<(), String> {
    if state.repository_guard.is_none() {
        state.repository_guard = Some(acquire_repository(&state.info.main_repo_path).await?);
    }
    let _workspace_guard = lock_process_workspace().await;
    if let Err(error) = ensure_worktree_is_not_process_workspace(&state.info.worktree_path) {
        state.repository_guard.take();
        return Err(error);
    }
    let Some(expected_target_head) = state.successful_merge_head.clone() else {
        state.repository_guard.take();
        return Err("cannot clean up an unverified merge transaction".to_string());
    };
    let current_branch = match current_branch_at(&state.info.main_repo_path).await {
        Ok(branch) => branch,
        Err(error) => {
            state.repository_guard.take();
            return Err(error);
        }
    };
    if current_branch.as_deref() != Some(state.target_branch.as_str()) {
        state.repository_guard.take();
        return Err(
            "target branch changed before cleanup; worktree and source branch retained".into(),
        );
    }
    let target_ref = format!("refs/heads/{}", state.target_branch);
    let target_head = match direct_ref_oid(&state.info.main_repo_path, &target_ref).await {
        Ok(oid) => oid,
        Err(error) => {
            state.repository_guard.take();
            return Err(error);
        }
    };
    if target_head != expected_target_head {
        state.repository_guard.take();
        return Err(
            "target HEAD changed before cleanup; worktree and source branch retained".into(),
        );
    }
    let source_ref = format!("refs/heads/{}", state.info.branch);
    let source_head = match direct_ref_oid(&state.info.main_repo_path, &source_ref).await {
        Ok(oid) => oid,
        Err(error) => {
            state.repository_guard.take();
            return Err(error);
        }
    };
    if source_head != state.source_head_oid {
        state.repository_guard.take();
        return Err(
            "source branch changed before cleanup; worktree and source branch retained".into(),
        );
    }
    match has_uncommitted_changes_at(&state.info.worktree_path).await {
        Ok(false) => {}
        Ok(true) => {
            state.repository_guard.take();
            return Err(
                "source worktree became dirty before cleanup; worktree and source branch retained"
                    .to_string(),
            );
        }
        Err(error) => {
            state.repository_guard.take();
            return Err(format!(
                "cannot verify source worktree status before cleanup; worktree and source branch retained: {error}"
            ));
        }
    }
    match has_untracked_or_ignored_at(&state.info.worktree_path).await {
        Ok(false) => {}
        Ok(true) => {
            state.repository_guard.take();
            return Err(
                "source worktree contains untracked or ignored files before cleanup; worktree and source branch retained"
                    .to_string(),
            );
        }
        Err(error) => {
            state.repository_guard.take();
            return Err(format!(
                "cannot verify source worktree untracked/ignored files before cleanup; worktree and source branch retained: {error}"
            ));
        }
    }
    let worktree_args = vec![
        OsString::from("worktree"),
        OsString::from("remove"),
        state.info.worktree_path.as_os_str().to_os_string(),
    ];
    let result = async {
        let mut errors = Vec::new();
        let worktree_removed = match run_local(
            &state.info.main_repo_path,
            "worktree-remove",
            worktree_args,
        )
        .await
        {
            Ok(_) => true,
            Err(error) => {
                errors.push(error);
                false
            }
        };
        if worktree_removed
            && let Err(error) = verify_target_and_delete_source(
                &state.info.main_repo_path,
                &target_ref,
                &expected_target_head,
                &source_ref,
                &state.source_head_oid,
            )
            .await
        {
            errors.push(error);
        }
        if state.stashed {
            let restore_required = state.original_branch.is_empty()
                || state.original_branch != state.target_branch;
            let restored = if restore_required {
                let (restored, restore_errors) = restore_and_verify_original(state).await;
                errors.extend(restore_errors);
                restored
            } else {
                true
            };
            if restored {
                if let Err(error) = restore_stash_after_abandonment(state).await {
                    errors.push(format!(
                        "merge succeeded but stash pop failed: {error}. Your changes remain in the stash; run `git -C {} stash pop` manually.",
                        state.info.main_repo_path.display()
                    ));
                }
            } else {
                errors.push(
                    "merge succeeded but stash was retained because the original HEAD was not verified"
                        .to_string(),
                );
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
    .await;
    state.repository_guard.take();
    result
}

async fn verify_target_and_delete_source(
    repo_path: &Path,
    target_ref: &str,
    expected_target_oid: &str,
    source_ref: &str,
    expected_source_oid: &str,
) -> Result<(), String> {
    let transaction = format!(
        "start\noption no-deref\nverify {target_ref} {expected_target_oid}\ndelete {source_ref} {expected_source_oid}\nprepare\ncommit\n"
    )
    .into_bytes();
    GitRunner::default()
        .run_with_input(
            repo_path,
            "verify-target-and-delete-source",
            ["update-ref", "--stdin"],
            transaction,
            LOCAL_MUTATION_LIMITS,
        )
        .await
        .map(|_| ())
}

#[cfg(test)]
pub async fn cleanup_worktree(
    worktree_path: &Path,
    branch: &str,
    main_repo_path: &Path,
    force: bool,
) -> Result<(), String> {
    let main_repo_path = canonical_path(main_repo_path, "main repository")?;
    validate_branch_name(&main_repo_path, branch).await?;
    let _guard = acquire_repository(&main_repo_path).await?;
    let _workspace_guard = lock_process_workspace().await;
    ensure_worktree_is_not_process_workspace(worktree_path)?;
    let source_ref = format!("refs/heads/{branch}");
    let source_oid = revision_oid(&main_repo_path, &source_ref).await?;
    if has_untracked_or_ignored_at(worktree_path).await? {
        return Err(
            "source worktree contains untracked or ignored files before cleanup; worktree and source branch retained"
                .to_string(),
        );
    }
    let mut remove_args = vec![OsString::from("worktree"), OsString::from("remove")];
    if force {
        remove_args.push(OsString::from("--force"));
    }
    remove_args.push(worktree_path.as_os_str().to_os_string());
    run_local(&main_repo_path, "worktree-remove", remove_args).await?;
    run_local(
        &main_repo_path,
        "delete-source-ref",
        [
            OsString::from("update-ref"),
            OsString::from("-d"),
            OsString::from(source_ref),
            OsString::from(source_oid),
        ],
    )
    .await?;
    Ok(())
}

pub async fn cancel_merge(state: &mut MergeState) -> Result<(), String> {
    if state.repository_guard.is_none() {
        state.repository_guard = Some(acquire_repository(&state.info.main_repo_path).await?);
    }
    let mut errors = Vec::new();
    let mut rollback_clean = true;
    if state.index_mutation_attempted
        && let Err(error) = rollback_merge_index(state).await
    {
        errors.push(error);
        rollback_clean = false;
    }
    let (restored, restore_errors) = restore_and_verify_original(state).await;
    errors.extend(restore_errors);
    if state.stashed {
        if restored && rollback_clean {
            if let Err(error) = restore_stash_after_abandonment(state).await {
                tracing::error!(
                    branch = %state.info.branch,
                    error = %error,
                    "cancel_merge: failed to pop stash; run `git -C <repo> stash pop` manually"
                );
                errors.push(error);
            }
        } else {
            errors.push(
                "stash retained because the original branch/HEAD or rollback cleanliness was not verified".to_string(),
            );
        }
    }
    state.repository_guard.take();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[cfg(test)]
pub async fn has_merge_conflict(repo_path: &Path) -> bool {
    let Ok(repo_path) = canonical_path(repo_path, "repository") else {
        return false;
    };
    has_merge_conflict_at(&repo_path).await
}

async fn has_merge_conflict_at(repo_path: &Path) -> bool {
    let git_path = run_query(
        repo_path,
        "merge-head-path",
        ["rev-parse", "--git-path", "MERGE_HEAD"],
    )
    .await
    .ok()
    .map(|output| {
        let path = output_path(&output.stdout);
        if path.is_absolute() {
            path
        } else {
            repo_path.join(path)
        }
    });
    if git_path.is_some_and(|path| path.exists()) {
        return true;
    }
    !conflicted_files_at(repo_path).await.is_empty()
}

#[cfg(test)]
pub async fn conflicted_files(repo_path: &Path) -> Vec<String> {
    let Ok(repo_path) = canonical_path(repo_path, "repository") else {
        return Vec::new();
    };
    conflicted_files_at(&repo_path).await
}

async fn conflicted_files_at(repo_path: &Path) -> Vec<String> {
    let Ok(output) = run_query(
        repo_path,
        "conflicted-files",
        ["diff", "--name-only", "--diff-filter=U"],
    )
    .await
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

async fn has_uncommitted_changes_at(repo_path: &Path) -> Result<bool, String> {
    run_query(
        repo_path,
        "status",
        ["status", "--porcelain", "--untracked-files=all"],
    )
    .await
    .map(|output| !trim_line(&output.stdout).is_empty())
}

async fn has_untracked_or_ignored_at(repo_path: &Path) -> Result<bool, String> {
    run_query(
        repo_path,
        "worktree-removal-untracked-safety-scan",
        ["ls-files", "--others", "-z"],
    )
    .await
    .map(|output| !output.stdout.is_empty())
}

pub async fn worktree_has_uncommitted(worktree_path: &Path) -> Result<bool, String> {
    let worktree_path = canonical_path(worktree_path, "worktree")?;
    has_uncommitted_changes_at(&worktree_path).await
}

pub async fn worktree_auto_commit_all(worktree_path: &Path) -> Result<(), String> {
    let worktree_path = canonical_path(worktree_path, "worktree")?;
    let _guard = acquire_repository(&worktree_path).await?;
    run_local(&worktree_path, "add-all", ["add", "--all"]).await?;
    run_local(
        &worktree_path,
        "auto-commit",
        ["commit", "-m", "auto-commit: save changes before merge"],
    )
    .await?;
    if has_uncommitted_changes_at(&worktree_path).await? {
        return Err("worktree remained dirty after auto-commit; merge aborted".to_string());
    }
    Ok(())
}

#[cfg(test)]
pub(crate) async fn run_git_with_limits_for_test(
    repo_path: &Path,
    args: &[&str],
    limits: CommandLimits,
) -> Result<CommandOutput, String> {
    let repo_path = canonical_path(repo_path, "test repository")?;
    GitRunner::default()
        .run(&repo_path, "test", args, limits)
        .await
}

#[cfg(test)]
pub(crate) async fn run_locked_git_with_limits_for_test(
    repo_path: &Path,
    args: &[&str],
    limits: CommandLimits,
) -> Result<CommandOutput, String> {
    let repo_path = canonical_path(repo_path, "test repository")?;
    let _guard = acquire_repository(&repo_path).await?;
    GitRunner::default()
        .run(&repo_path, "locked-test", args, limits)
        .await
}

#[cfg(test)]
pub(crate) async fn try_merge_with_switch_limits_for_test(
    info: &WorktreeInfo,
    target: &str,
    limits: CommandLimits,
) -> MergeResponse {
    try_merge_with_switch_limits(info, target, limits).await
}

#[cfg(test)]
pub(crate) async fn create_with_limits_for_test(
    repo_path: &Path,
    name: &str,
    base_dir: Option<&Path>,
    limits: CommandLimits,
) -> Result<(PathBuf, WorktreeInfo), String> {
    create_with_limits(repo_path, name, base_dir, LOCAL_MUTATION_LIMITS, limits).await
}

#[cfg(test)]
pub(crate) async fn create_with_ref_limits_for_test(
    repo_path: &Path,
    name: &str,
    base_dir: Option<&Path>,
    limits: CommandLimits,
) -> Result<(PathBuf, WorktreeInfo), String> {
    create_with_limits(repo_path, name, base_dir, limits, LOCAL_MUTATION_LIMITS).await
}

#[cfg(test)]
pub(crate) async fn create_and_publish_stash_for_test(
    repo_path: &Path,
) -> Result<Option<String>, String> {
    create_and_publish_stash(repo_path, None).await
}

#[cfg(test)]
pub(crate) async fn verify_target_and_delete_source_for_test(
    repo_path: &Path,
    target_ref: &str,
    expected_target_oid: &str,
    source_ref: &str,
    expected_source_oid: &str,
) -> Result<(), String> {
    verify_target_and_delete_source(
        repo_path,
        target_ref,
        expected_target_oid,
        source_ref,
        expected_source_oid,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn restore_stash_with_gate_for_test(
    repo_path: &Path,
    stash_before: Option<String>,
    stash_created: String,
    gate: Arc<TestMutationGate>,
) -> Result<(), String> {
    let repo_path = canonical_path(repo_path, "test stash repository")?;
    let guard = acquire_repository(&repo_path).await?;
    let info = WorktreeInfo {
        branch: "test-stash".to_string(),
        worktree_path: repo_path.clone(),
        main_repo_path: repo_path,
    };
    let mut state = state_with_guard(&info, String::new(), true, guard);
    state.stash_attempted = true;
    state.stash_before = stash_before;
    state.stash_created = Some(stash_created);
    state.stash_test_gate = Some(gate);
    restore_stash_after_abandonment(&mut state).await
}
