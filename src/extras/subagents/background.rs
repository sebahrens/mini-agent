//! Session-owned background Task work. Runtime handles never come from JSON.
use crate::agent::runner::{AgentRunCleanupGuard, AgentWorkScope};
use crate::session::background_observation::{
    JobObservation, ObservationLifetime, ObservationOwner,
};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, Mutex, OnceLock, Weak},
};

pub(crate) const MAX_JOBS: usize = 32;
const MAX_RESULT_BYTES: usize = 256 * 1024;
const MAX_SAVED_RESULT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Running,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}
impl Status {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Cancelling => "cancelling",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }
    pub(crate) fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::Cancelling)
    }
}

/// Display/result data only. Restoring one never creates a task, a permission,
/// an account, a scope or an observation permit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedTask {
    pub id: String,
    pub status: Status,
    pub result: Option<String>,
    #[serde(skip)]
    pub notified: bool,
}

/// Equality of the persisted representation: like the serialized form, it
/// ignores the runtime-only `notified` flag.
impl PartialEq for SavedTask {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.status == other.status && self.result == other.result
    }
}

pub(crate) fn read_history<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<SavedTask>, D::Error> {
    struct HistoryVisitor;
    impl<'de> serde::de::Visitor<'de> for HistoryVisitor {
        type Value = Vec<SavedTask>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("bounded background task data")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut items = Vec::new();
            while let Some(item) = seq.next_element::<SavedTask>()? {
                if items.len() == MAX_JOBS
                    || item.id.len() > 64
                    || uuid::Uuid::parse_str(&item.id).is_err()
                    || item
                        .result
                        .as_ref()
                        .is_some_and(|text| text.len() > MAX_SAVED_RESULT_BYTES)
                    || items.iter().any(|prior: &SavedTask| prior.id == item.id)
                {
                    return Err(serde::de::Error::custom(
                        "invalid or oversized background task data",
                    ));
                }
                items.push(item);
            }
            Ok(items)
        }
    }
    deserializer.deserialize_seq(HistoryVisitor)
}

#[derive(Debug, Default)]
struct HistoryState {
    rows: Vec<SavedTask>,
    stamp: crate::session::background_observation::ObservationStamp,
}
/// A bounded data store only. Runtime jobs and permission bridges live in Pool.
#[derive(Debug, Clone, Default)]
pub(crate) struct History(Arc<Mutex<HistoryState>>);
impl Serialize for History {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rows
            .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for History {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(Arc::new(Mutex::new(HistoryState {
            rows: read_history(deserializer)?,
            stamp: Default::default(),
        }))))
    }
}
#[derive(Debug, Clone)]
pub(crate) struct HistoryObservation {
    original: History,
    rows: Vec<SavedTask>,
    stamp: crate::session::background_observation::ObservationStamp,
}
impl HistoryObservation {
    pub(crate) fn frozen(&self) -> History {
        History(Arc::new(Mutex::new(HistoryState {
            rows: self.rows.clone(),
            stamp: Default::default(),
        })))
    }
    pub(crate) fn validates_since(&self, previous: &Self, owner: &Arc<ObservationOwner>) -> bool {
        Arc::ptr_eq(&self.original.0, &previous.original.0)
            && self.stamp.validates_since(&previous.stamp, owner)
    }
}
impl History {
    pub(crate) fn is_empty(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rows
            .is_empty()
    }
    pub(crate) fn rows(&self) -> Vec<SavedTask> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rows
            .clone()
    }
    pub(crate) fn same_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub(crate) fn bind_observation_owner(
        &self,
        owner: &Arc<ObservationOwner>,
    ) -> anyhow::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stamp
            .bind(owner)
    }
    pub(crate) fn with_observation_snapshot<T>(
        &self,
        capture: impl FnOnce(HistoryObservation) -> T,
    ) -> T {
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        capture(HistoryObservation {
            original: self.clone(),
            rows: state.rows.clone(),
            stamp: state.stamp.clone(),
        })
    }
    fn set(&self, saved: &SavedTask, observation: Option<&JobObservation>) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        // notified is runtime-only and cannot rewrite persisted/native bytes.
        let mut saved = saved.clone();
        saved.notified = false;
        if let Some(row) = state.rows.iter_mut().find(|row| row.id == saved.id) {
            if *row == saved {
                return;
            }
            *row = saved;
        } else {
            assert!(
                state.rows.len() < MAX_JOBS,
                "admission enforces task history cap"
            );
            state.rows.push(saved);
        }
        state.stamp.changed(observation);
    }
}

struct Entry {
    saved: SavedTask,
    result: Option<String>,
    scope: Option<Arc<AgentWorkScope>>,
    observation: Option<JobObservation>,
}
#[derive(Default)]
struct State {
    jobs: BTreeMap<String, Entry>,
    closing: bool,
    history_paused: bool,
}

/// The logical Session owns this Arc. Tools and workers receive Weak handles.
/// The root scope tracks producers; each producer joins its own effect scope
/// before its root registration drops or a terminal status becomes visible.
#[cfg(feature = "acp")]
pub(crate) struct PreparedAsk {
    pub(crate) sender: crate::permission::ask::AskSender,
    pub(crate) future: std::pin::Pin<Box<dyn Future<Output = ()> + Send>>,
}
#[cfg(feature = "acp")]
type AskFactory = dyn Fn(
        Option<crate::permission::checker::PermCheck>,
        Arc<AgentWorkScope>,
    ) -> Result<PreparedAsk, String>
    + Send
    + Sync;

pub(crate) struct Pool {
    session: String,
    workspace_path: std::path::PathBuf,
    workspace: OnceLock<Arc<crate::paths::WorkspaceBinding>>,
    account: crate::spend::Account,
    files: crate::session::file_checkpoints::FileCheckpoints,
    history: History,
    observations: Arc<ObservationOwner>,
    #[cfg(feature = "acp")]
    ask_factory: Mutex<Option<Arc<AskFactory>>>,
    state: Mutex<State>,
    changed: tokio::sync::Notify,
    scope: OnceLock<Arc<AgentWorkScope>>,
    cleanup: Mutex<Option<AgentRunCleanupGuard>>,
    /// Shared by background batches (sized by `task_max_concurrency`).
    pub(crate) concurrency: Arc<tokio::sync::Semaphore>,
    /// Foreground children use their own lane of the same size, so session
    /// background jobs cannot serialize the parent turn's own task call.
    pub(crate) foreground: Arc<tokio::sync::Semaphore>,
    pub(crate) write_lane: Arc<tokio::sync::Semaphore>,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundPool")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

/// One pending delivery from the exact live pool. Neither imported data nor
/// tool-return text constructs this value. Drop keeps notifications pending.
#[derive(Clone, Debug)]
pub(crate) struct AskOrigin {
    pool: Weak<Pool>,
    job: JobObservation,
    scope: Weak<AgentWorkScope>,
}
impl AskOrigin {
    pub(crate) fn valid(&self, session: &crate::session::Session) -> bool {
        self.pool.upgrade().is_some_and(|pool| {
            pool.matches(session)
                && pool.job_can_ask(&self.job)
                && self.job.active_for(&pool.observations)
                && self
                    .scope
                    .upgrade()
                    .is_some_and(|scope| !scope.is_cancelled())
        })
    }
}

pub(crate) struct NoticeLease {
    pool: Weak<Pool>,
    entries: Vec<(String, Status)>,
    scope: Option<Weak<AgentWorkScope>>,
}
impl NoticeLease {
    pub(crate) fn messages(&self) -> Vec<String> {
        self.entries.iter().map(|(id, status)| format!("[Background task {id} {}. Inspect its report with task_result; this notice grants no permission.]", status.as_str())).collect()
    }
    pub(crate) fn with_scope(mut self, scope: Weak<AgentWorkScope>) -> Self {
        self.scope = Some(scope);
        self
    }
    pub(crate) fn acknowledge(self) {
        if self
            .scope
            .as_ref()
            .is_some_and(|scope| scope.upgrade().is_none_or(|scope| scope.is_cancelled()))
        {
            return;
        }
        if let Some(pool) = self.pool.upgrade() {
            let mut state = pool.state.lock().unwrap_or_else(|e| e.into_inner());
            for (id, status) in self.entries {
                if let Some(entry) = state.jobs.get_mut(&id)
                    && entry.observation.is_some()
                    && entry.saved.status == status
                    && !status.is_active()
                {
                    entry.saved.notified = true;
                    pool.changed.notify_one();
                }
            }
        }
    }
}

pub(crate) struct Admission {
    pool: Arc<Pool>,
    id: String,
    job_scope: Arc<AgentWorkScope>,
    observation: JobObservation,
    recorder: crate::session::file_checkpoints::Recorder,
    lifetime: Option<Arc<ObservationLifetime>>,
    started: bool,
    bridge: Option<std::pin::Pin<Box<dyn Future<Output = ()> + Send>>>,
}

fn clip(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_owned();
    }
    let suffix = "\n[background task result truncated]";
    let mut end = cap.saturating_sub(suffix.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &text[..end])
}

impl Pool {
    pub(crate) fn for_session(
        session: &mut crate::session::Session,
        concurrency: usize,
    ) -> anyhow::Result<Arc<Self>> {
        if let Some(pool) = &session.background_pool {
            anyhow::ensure!(pool.matches(session), "background task owner changed");
            return Ok(pool.clone());
        }
        let observations = ObservationOwner::bind(session)?;
        let mut state = State::default();
        let inherited = session.background_history.rows();
        anyhow::ensure!(inherited.len() <= MAX_JOBS, "background task history limit");
        for saved in &inherited {
            anyhow::ensure!(
                saved.id.len() <= 64
                    && uuid::Uuid::parse_str(&saved.id).is_ok()
                    && saved
                        .result
                        .as_ref()
                        .is_none_or(|r| r.len() <= MAX_SAVED_RESULT_BYTES),
                "invalid background task history"
            );
            let mut saved = saved.clone();
            if saved.status.is_active() {
                saved.status = Status::Interrupted;
            }
            // A restarted terminal notification is data only; no old waiter.
            saved.notified = true;
            session.background_history.set(&saved, None);
            anyhow::ensure!(
                !state.jobs.contains_key(&saved.id),
                "duplicate background task identity"
            );
            state.jobs.insert(
                saved.id.clone(),
                Entry {
                    result: saved.result.clone(),
                    saved,
                    scope: None,
                    observation: None,
                },
            );
        }
        let pool = Arc::new(Self {
            session: session.id.to_string(),
            workspace_path: session.working_dir.as_str().into(),
            workspace: OnceLock::new(),
            account: session.spend_account.clone(),
            files: session.file_checkpoints.clone(),
            history: session.background_history.clone(),
            observations,
            #[cfg(feature = "acp")]
            ask_factory: Mutex::new(None),
            state: Mutex::new(state),
            changed: tokio::sync::Notify::new(),
            scope: OnceLock::new(),
            cleanup: Mutex::new(None),
            concurrency: Arc::new(tokio::sync::Semaphore::new(concurrency.clamp(1, 32))),
            foreground: Arc::new(tokio::sync::Semaphore::new(concurrency.clamp(1, 32))),
            write_lane: Arc::new(tokio::sync::Semaphore::new(1)),
        });
        session.background_pool = Some(pool.clone());
        Ok(pool)
    }

    pub(crate) fn matches(&self, session: &crate::session::Session) -> bool {
        session
            .background_pool
            .as_ref()
            .is_some_and(|pool| std::ptr::eq(self, pool.as_ref()))
            && self.session == session.id.as_str()
            && self.workspace_path == std::path::Path::new(session.working_dir.as_str())
            && self.account.same_owner(&session.spend_account)
            && self.files.same_owner(&session.file_checkpoints)
            && self.history.same_owner(&session.background_history)
            && session
                .background_observations
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, &self.observations))
    }

    pub(crate) fn validate_workspace(
        &self,
        workspace: &Arc<crate::paths::WorkspaceBinding>,
    ) -> anyhow::Result<()> {
        workspace.validate().map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            workspace.root() == self.workspace_path,
            "background task workspace changed"
        );
        let retained = self.workspace.get_or_init(|| workspace.clone());
        retained.validate().map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            retained
                .compatibility_fingerprint()
                .map_err(anyhow::Error::msg)?
                == workspace
                    .compatibility_fingerprint()
                    .map_err(anyhow::Error::msg)?,
            "background task workspace identity changed"
        );
        Ok(())
    }

    pub(crate) fn reserve(
        self: &Arc<Self>,
        workspace: &Arc<crate::paths::WorkspaceBinding>,
        run: &crate::spend::run::Run,
        recorder: &crate::session::file_checkpoints::Recorder,
    ) -> anyhow::Result<Admission> {
        self.validate_workspace(workspace)?;
        anyhow::ensure!(
            run.belongs_to_account(&self.session, &self.account)
                && recorder.belongs_to_store(&self.session, &self.files),
            "background task belongs to another captured run or checkpoint owner"
        );
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(
            !state.closing && !state.history_paused,
            "background task owner is closing or changing history"
        );
        anyhow::ensure!(
            state.jobs.len() < MAX_JOBS,
            "background task capacity reached; start a new session after inspecting existing results"
        );
        let (observation, lifetime) = self.observations.admit()?;
        let id = uuid::Uuid::new_v4().to_string();
        let job_scope = AgentWorkScope::new();
        state.jobs.insert(
            id.clone(),
            Entry {
                saved: SavedTask {
                    id: id.clone(),
                    status: Status::Running,
                    result: None,
                    notified: false,
                },
                result: None,
                scope: Some(job_scope.clone()),
                observation: Some(observation.clone()),
            },
        );
        self.history.set(&state.jobs[&id].saved, Some(&observation));
        self.changed.notify_one();
        Ok(Admission {
            pool: self.clone(),
            id,
            job_scope,
            recorder: recorder.for_background(observation.clone()),
            observation,
            lifetime: Some(Arc::new(lifetime)),
            started: false,
            bridge: None,
        })
    }

    fn producer_scope(&self) -> Arc<AgentWorkScope> {
        self.scope
            .get_or_init(|| {
                // First actual admission inherits the parent's captured hook root.
                // Creating a scope does not register it under the foreground turn.
                let scope = AgentWorkScope::new();
                *self.cleanup.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(AgentRunCleanupGuard::auxiliary(scope.clone()));
                scope
            })
            .clone()
    }

    #[cfg(feature = "acp")]
    pub(crate) fn install_ask_factory(&self, factory: Arc<AskFactory>) -> Result<(), &'static str> {
        let mut slot = self.ask_factory.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return Err("background permission owner already installed");
        }
        *slot = Some(factory);
        Ok(())
    }
    fn job_can_ask(&self, job: &JobObservation) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        !state.closing
            && !state.history_paused
            && state.jobs.values().any(|entry| {
                entry
                    .observation
                    .as_ref()
                    .is_some_and(|origin| origin.same_job(job))
                    && entry
                        .scope
                        .as_ref()
                        .is_some_and(|scope| !scope.is_cancelled())
                    && entry.saved.status.is_active()
            })
    }

    /// History owners hold this guard through compaction. Jobs can no longer
    /// write at removed/rebased launch rows, and new admission stays paused
    /// until the history owner finishes or abandons its transition.
    pub(crate) async fn pause_for_history(self: &Arc<Self>) -> anyhow::Result<HistoryPause> {
        let jobs = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            anyhow::ensure!(
                !state.closing && !state.history_paused,
                "background owner is unavailable during history change"
            );
            state.history_paused = true;
            for entry in state.jobs.values_mut() {
                if let Some(scope) = &entry.scope {
                    scope.cancellation_handle().cancel();
                    entry.saved.status = Status::Cancelling;
                    self.history.set(&entry.saved, entry.observation.as_ref());
                }
            }
            state
                .jobs
                .values()
                .filter_map(|entry| entry.scope.clone())
                .collect::<Vec<_>>()
        };
        // Create before the first await: abandoning preparation always releases
        // the pause, while the already-cancelled jobs retain their real owners.
        let pause = HistoryPause(self.clone());
        for scope in jobs {
            scope.wait_idle().await;
        }
        if let Some(scope) = self.scope.get() {
            scope.wait_idle().await;
        }
        Ok(pause)
    }

    pub(crate) fn ask_origin(self: &Arc<Self>, job: JobObservation) -> Option<AskOrigin> {
        let scope = crate::agent::runner::current_hook_work_scope()?;
        job.active_for(&self.observations).then(|| AskOrigin {
            pool: Arc::downgrade(self),
            job,
            scope: Arc::downgrade(&scope),
        })
    }
    #[cfg(any(feature = "acp", test))]
    pub(crate) fn active_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .jobs
            .values()
            .filter(|entry| entry.saved.status.is_active())
            .count()
    }

    pub(crate) fn counts(&self) -> (usize, usize) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (
            state
                .jobs
                .values()
                .filter(|e| e.saved.status.is_active())
                .count(),
            state
                .jobs
                .values()
                .filter(|e| {
                    e.observation.is_some() && !e.saved.status.is_active() && !e.saved.notified
                })
                .count(),
        )
    }
    pub(crate) async fn wait_changed(&self) {
        self.changed.notified().await;
    }

    pub(crate) fn ensure_open(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            {
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                !state.closing && !state.history_paused
            },
            "background session is closing"
        );
        Ok(())
    }

    pub(crate) fn pending_notices(self: &Arc<Self>) -> Option<NoticeLease> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let entries = state
            .jobs
            .values()
            .filter(|entry| {
                entry.observation.is_some()
                    && !entry.saved.notified
                    && !entry.saved.status.is_active()
            })
            .map(|entry| (entry.saved.id.clone(), entry.saved.status))
            .collect::<Vec<_>>();
        if entries.is_empty() {
            None
        } else {
            Some(NoticeLease {
                pool: Arc::downgrade(self),
                entries,
                scope: None,
            })
        }
    }

    pub(crate) fn list(&self) -> Vec<SavedTask> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .jobs
            .values()
            .map(|entry| entry.saved.clone())
            .collect()
    }
    pub(crate) fn result(&self, id: &str) -> anyhow::Result<SavedTask> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let entry = state
            .jobs
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("unknown task in this session"))?;
        let mut saved = entry.saved.clone();
        saved.result = entry.result.clone();
        Ok(saved)
    }
    pub(crate) fn request_cancel(&self, id: &str) -> anyhow::Result<SavedTask> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let entry = state
            .jobs
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("unknown task in this session"))?;
        if entry.saved.status.is_active() {
            entry.saved.status = Status::Cancelling;
            if let Some(scope) = &entry.scope {
                scope.cancellation_handle().cancel();
            }
            self.history.set(&entry.saved, entry.observation.as_ref());
            self.changed.notify_one();
        }
        Ok(entry.saved.clone())
    }
    pub(crate) async fn close(&self) {
        let jobs = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.closing = true;
            self.observations.stop_admission();
            for entry in state.jobs.values_mut() {
                if let Some(scope) = &entry.scope {
                    scope.cancellation_handle().cancel();
                }
            }
            state
                .jobs
                .values()
                .filter_map(|entry| entry.scope.clone())
                .collect::<Vec<_>>()
        };
        for scope in jobs {
            scope.wait_idle().await;
        }
        if let Some(scope) = self.scope.get() {
            scope.cancellation_handle().cancel();
            scope.wait_idle().await;
        }
        #[cfg(feature = "acp")]
        self.ask_factory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        // Existing native end publication still needs to record these exact
        // settled observations. Admission is closed; all job permits are now
        // settled, so no later mutation can acquire background provenance.
    }
    fn terminal(&self, id: &str, status: Status, result: Option<String>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = state.jobs.get_mut(id) {
            entry.saved.status = status;
            entry.saved.result = result
                .as_ref()
                .map(|text| clip(text, MAX_SAVED_RESULT_BYTES));
            entry.result = result;
            entry.scope = None;
            self.history.set(&entry.saved, entry.observation.as_ref());
            self.changed.notify_one();
        }
    }
}

impl Admission {
    pub(crate) fn prepare_ask(
        &mut self,
        permission: Option<crate::permission::checker::PermCheck>,
        fallback: Option<crate::permission::ask::AskSender>,
    ) -> Result<Option<crate::permission::ask::AskSender>, String> {
        #[cfg(feature = "acp")]
        if let Some(factory) = self
            .pool
            .ask_factory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            && permission.is_some()
        {
            let prepared = factory(permission, self.job_scope.clone())?;
            self.bridge = Some(prepared.future);
            return Ok(Some(prepared.sender));
        }
        #[cfg(not(feature = "acp"))]
        let _ = permission;
        Ok(fallback)
    }
    pub(crate) fn observation(&self) -> JobObservation {
        self.observation.clone()
    }
    pub(crate) fn start<F>(mut self, work: F, run: crate::spend::run::Run) -> anyhow::Result<String>
    where
        F: Future<Output = Result<String, String>> + Send + 'static,
    {
        anyhow::ensure!(
            run.belongs_to_account(&self.pool.session, &self.pool.account),
            "background run owner changed"
        );
        let run = run
            .with_background_pool(&self.pool)
            .with_background_observation(self.observation.clone());
        let recorder = self.recorder.clone();
        let work = crate::session::file_checkpoints::scope(Some(recorder), work);
        let scope = self.pool.producer_scope();
        let job_scope = self.job_scope.clone();
        let root = scope.clone();
        let weak: Weak<Pool> = Arc::downgrade(&self.pool);
        let id = self.id.clone();
        let returned_id = id.clone();
        let mut cleanup = AgentRunCleanupGuard::auxiliary(job_scope.clone());
        let lifetime = self.lifetime.take().expect("unstarted admission");
        cleanup.retain_until_settled(lifetime.clone());
        let bridge = self.bridge.take();
        let worker = async move {
            if let Some(bridge) = bridge {
                // A staged bridge has no spawned lifetime until this producer
                // has its root registration. Exact job cleanup joins it too.
                job_scope.run_sync(|| drop(crate::agent::runner::spawn_async_scoped(bridge)));
            }
            let result = std::panic::AssertUnwindSafe(job_scope.run(async {
                tokio::select! {
                    biased;
                    _ = root.cancelled() => None,
                    _ = job_scope.cancelled() => None,
                    result = work => Some(result),
                }
            }))
            .catch_unwind()
            .await;
            // work is destroyed by select/catch before its actual children join.
            cleanup.settle().await;
            let (status, result) = match result {
                Ok(Some(Ok(text))) => (Status::Completed, Some(clip(&text, MAX_RESULT_BYTES))),
                Ok(Some(Err(text))) => (Status::Failed, Some(clip(&text, MAX_RESULT_BYTES))),
                Ok(None) => (Status::Cancelled, None),
                Err(_) => (Status::Failed, Some("background task panicked".into())),
            };
            if let Some(pool) = weak.upgrade() {
                pool.terminal(&id, status, result);
            }
            // The terminal data transition is observed only AFTER effect
            // retirement, while this admitted producer still owns provenance.
            drop(lifetime);
        };
        // This exact registration is owned by the Session scope, never the
        // parent's turn. No JoinHandle or strong pool back edge escapes.
        let state = self.pool.state.lock().unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(
            !state.closing && !state.history_paused,
            "background owner closed or changed history before launch"
        );
        // Close/admit use this SAME lock. Registration happens synchronously;
        // close cannot observe zero producers and then permit a later spawn.
        crate::spend::run::scope_sync(Some(run), || {
            scope.run_sync(|| drop(crate::agent::runner::spawn_async_scoped(worker)));
        });
        self.started = true;
        drop(state);
        Ok(returned_id)
    }
}
impl Drop for Admission {
    fn drop(&mut self) {
        if !self.started {
            self.pool.terminal(&self.id, Status::Cancelled, None);
        }
    }
}
pub(crate) struct HistoryPause(Arc<Pool>);
impl Drop for HistoryPause {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .history_paused = false;
        self.0.changed.notify_one();
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(|e| e.into_inner());
        for entry in state.jobs.values() {
            if let Some(scope) = &entry.scope {
                scope.cancellation_handle().cancel();
            }
        }
        self.observations.retire();
        // Existing cleanup transfers to its retained settlement owner. This
        // fallback does not claim a successful terminal status or cleanup.
        drop(
            self.cleanup
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .take(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_task_equality_matches_its_persisted_representation() {
        let id = uuid::Uuid::new_v4().to_string();
        let rows = [
            (Status::Running, None, false),
            (Status::Running, None, true),
            (Status::Completed, Some("done".to_string()), false),
            (Status::Completed, Some("other".to_string()), false),
            (Status::Failed, Some("done".to_string()), true),
        ]
        .map(|(status, result, notified)| SavedTask {
            id: id.clone(),
            status,
            result,
            notified,
        });
        let other_id = SavedTask {
            id: uuid::Uuid::new_v4().to_string(),
            ..rows[0].clone()
        };
        for left in rows.iter().chain([&other_id]) {
            for right in rows.iter().chain([&other_id]) {
                assert_eq!(
                    left == right,
                    serde_json::to_value(left).unwrap() == serde_json::to_value(right).unwrap()
                );
            }
        }
    }

    fn fixture(
        root: &crate::tests::OwnedTestDirectory,
    ) -> (
        crate::session::Session,
        Arc<Pool>,
        Arc<crate::paths::WorkspaceBinding>,
        crate::spend::run::Run,
        crate::session::file_checkpoints::Recorder,
    ) {
        let mut session = crate::session::Session::new("openai", "fixture", 32000, "");
        session.working_dir = root.path().to_str().unwrap().into();
        session.initialize_spend_account(None).unwrap();
        let pool = Pool::for_session(&mut session, 2).unwrap();
        let root = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let run = crate::spend::run::Run::fixture(
            session.spend_account.clone(),
            Arc::new(crate::spend::Ledger::new(&root.root().join("state"))),
            &session.id,
            crate::spend::PriceSnapshot::capture(Some((2.0, 4.0)), Default::default(), false),
        );
        let recorder = session.file_checkpoints.recorder(&session.id, 0, true);
        (session, pool, root, run, recorder)
    }
    async fn settled(pool: &Pool, id: &str) -> SavedTask {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let task = pool.result(id).unwrap();
                if !task.status.is_active() {
                    return task;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background task must actually settle")
    }
    #[tokio::test]
    async fn background56_immediate_id_outlives_parent_and_result_is_session_owned() {
        let root = crate::tests::OwnedTestDirectory::new("background56-pool");
        let (session, pool, workspace, run, recorder) = fixture(&root);
        let parent = AgentWorkScope::new();
        let parent_cleanup = AgentRunCleanupGuard::auxiliary(parent.clone());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let admission = pool.reserve(&workspace, &run, &recorder).unwrap();
        let id = parent
            .run(async {
                admission.start(
                    async {
                        started_tx.send(()).unwrap();
                        finish_rx.await.unwrap();
                        Ok("exact child result".into())
                    },
                    run,
                )
            })
            .await
            .unwrap();
        started_rx.await.unwrap();
        parent_cleanup.settle().await;
        assert_eq!(pool.result(&id).unwrap().status, Status::Running);
        assert!(pool.matches(&session));
        assert_eq!(parent.active_children(), 0);
        let foreign_root = crate::tests::OwnedTestDirectory::new("background56-foreign");
        let (_, foreign, _, _, _) = fixture(&foreign_root);
        assert!(foreign.result(&id).is_err());
        assert!(foreign.request_cancel(&id).is_err());
        finish_tx.send(()).unwrap();
        let result = settled(&pool, &id).await;
        assert_eq!(result.status, Status::Completed);
        assert_eq!(result.result.as_deref(), Some("exact child result"));
        pool.close().await;
        foreign.close().await;
    }
    #[tokio::test]
    async fn background56_actual_compaction_joins_old_effects_before_rebasing_and_reopens_admission()
     {
        let root = crate::tests::OwnedTestDirectory::new("background56-compaction");
        let (mut session, pool, workspace, run, recorder) = fixture(&root);
        session.add_message(crate::session::MessageRole::User, "launch row");
        session.add_message(crate::session::MessageRole::Assistant, "parent complete");
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        struct Release(Arc<(Mutex<bool>, std::sync::Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0.0.lock().unwrap() = true;
                self.0.1.notify_all();
            }
        }
        let release = Release(gate.clone());
        let marker = root.path().join("settled-before-history-cut");
        let effect = marker.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let mut release = Some(release);
        let outcome = std::panic::AssertUnwindSafe(async {
            let id = pool
                .reserve(&workspace, &run, &recorder)
                .unwrap()
                .start(
                    async move {
                        crate::agent::runner::spawn_blocking_scoped(move || {
                            started.send(()).unwrap();
                            let mut open = gate.0.lock().unwrap();
                            while !*open {
                                open = gate.1.wait(open).unwrap();
                            }
                            std::fs::write(effect, "owned blocking effect retired").unwrap();
                        })
                        .await
                        .unwrap();
                        Ok("completed".into())
                    },
                    run.clone(),
                )
                .unwrap();
            ready.await.unwrap();
            let inspected = marker.clone();
            let compaction = crate::ui::slash::compact_session_with(
                &mut session,
                2,
                4000,
                1000,
                0,
                move |_, _, _, _, _| async move {
                    assert!(
                        inspected.exists(),
                        "summarizer/history cut cannot race an old blocking effect"
                    );
                    Ok(crate::provider::CompactionOutput {
                        summary: "bounded recap".into(),
                        summary_byte_budget: 4096,
                        messages_included: 2,
                        usage: Default::default(),
                    })
                },
                |_, _| {},
            );
            tokio::pin!(compaction);
            assert!(futures::poll!(&mut compaction).is_pending());
            assert!(
                pool.reserve(&workspace, &run, &recorder).is_err(),
                "history transition pauses new producers"
            );
            assert!(!marker.exists());
            drop(release.take());
            compaction.await.unwrap();
            assert!(marker.exists());
            assert_eq!(pool.result(&id).unwrap().status, Status::Cancelled);
            let next = pool
                .reserve(&workspace, &run, &recorder)
                .unwrap()
                .start(async { Ok("new admitted job".into()) }, run)
                .unwrap();
            assert_eq!(settled(&pool, &next).await.status, Status::Completed);
        })
        .catch_unwind()
        .await;
        drop(release.take());
        pool.close().await;
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    #[tokio::test]
    async fn background56_capacity_import_and_notification_delivery_are_data_only() {
        let root = crate::tests::OwnedTestDirectory::new("background56-capacity");
        let (session, pool, workspace, run, recorder) = fixture(&root);
        for _ in 0..MAX_JOBS {
            let admission = pool.reserve(&workspace, &run, &recorder).unwrap();
            drop(admission);
        }
        assert!(pool.reserve(&workspace, &run, &recorder).is_err());
        assert_eq!(pool.list().len(), MAX_JOBS);
        let pending = pool.pending_notices().unwrap();
        assert_eq!(pending.messages().len(), MAX_JOBS);
        drop(pending);
        assert!(pool.pending_notices().is_some());
        let pending = pool.pending_notices().unwrap();
        let cancelled = AgentWorkScope::new();
        cancelled.cancellation_handle().cancel();
        pending.with_scope(Arc::downgrade(&cancelled)).acknowledge();
        assert_eq!(
            pool.counts(),
            (0, MAX_JOBS),
            "cancelled preparation cannot acknowledge delivery"
        );
        pool.pending_notices().unwrap().acknowledge();
        assert_eq!(pool.counts(), (0, 0));
        let mut imported: crate::session::Session =
            serde_json::from_value(serde_json::to_value(&session).unwrap()).unwrap();
        assert!(imported.background_pool.is_none() && imported.background_observations.is_none());
        let reloaded = Pool::for_session(&mut imported, 1).unwrap();
        assert!(
            reloaded.pending_notices().is_none(),
            "imported data cannot recreate a notification waiter"
        );
        let id = pool.list()[0].id.clone();
        assert!(
            reloaded.result(&id).is_ok(),
            "bounded data remains inspectable"
        );
        assert_eq!(reloaded.active_count(), 0);
        let too_many = serde_json::json!(
            (0..MAX_JOBS + 1)
                .map(|_| SavedTask {
                    id: uuid::Uuid::new_v4().to_string(),
                    status: Status::Running,
                    result: None,
                    notified: false
                })
                .collect::<Vec<_>>()
        );
        assert!(serde_json::from_value::<History>(too_many).is_err());
        pool.close().await;
        reloaded.close().await;
    }

    #[tokio::test]
    async fn background56_closed_owner_refuses_reserved_but_not_registered_work() {
        let root = crate::tests::OwnedTestDirectory::new("background56-close-admission");
        let (_session, pool, workspace, run, recorder) = fixture(&root);
        let admission = pool.reserve(&workspace, &run, &recorder).unwrap();
        let id = admission.id.clone();
        pool.close().await;
        let effect = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called = effect.clone();
        assert!(
            admission
                .start(
                    async move {
                        called.store(true, std::sync::atomic::Ordering::Release);
                        Ok("forbidden".into())
                    },
                    run
                )
                .is_err()
        );
        assert!(!effect.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(pool.result(&id).unwrap().status, Status::Cancelled);
    }

    #[tokio::test]
    async fn background56_cancel_does_not_publish_terminal_until_blocking_child_retires() {
        let root = crate::tests::OwnedTestDirectory::new("background56-held-effect");
        let (_session, pool, workspace, run, recorder) = fixture(&root);
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        struct Release(Arc<(Mutex<bool>, std::sync::Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0.0.lock().unwrap() = true;
                self.0.1.notify_all();
            }
        }
        let release = Release(gate.clone());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let admission = pool.reserve(&workspace, &run, &recorder).unwrap();
        let id = admission
            .start(
                async move {
                    crate::agent::runner::spawn_blocking_scoped(move || {
                        started_tx.send(()).unwrap();
                        let mut open = gate.0.lock().unwrap();
                        while !*open {
                            open = gate.1.wait(open).unwrap();
                        }
                    })
                    .await
                    .unwrap();
                    Ok("should be cancelled".into())
                },
                run,
            )
            .unwrap();
        started_rx.await.unwrap();
        pool.request_cancel(&id).unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), pool.close())
                .await
                .is_err()
        );
        assert_eq!(pool.result(&id).unwrap().status, Status::Cancelling);
        drop(release);
        pool.close().await;
        assert_eq!(pool.result(&id).unwrap().status, Status::Cancelled);
    }
}

/// Closed task-control operations use the captured Run's pool, never an ID
/// registry or a runtime handle reconstructed from user/model text.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ControlArgs {
    #[serde(default)]
    id: Option<String>,
}

pub(crate) struct ControlTool<const OP: u8> {
    permission: Option<crate::permission::checker::PermCheck>,
    ask: Option<crate::permission::ask::AskSender>,
    workspace: Arc<crate::paths::WorkspaceBinding>,
}
pub(crate) type ResultTool = ControlTool<0>;
pub(crate) type ListTool = ControlTool<1>;
pub(crate) type CancelTool = ControlTool<2>;
impl<const OP: u8> ControlTool<OP> {
    pub(crate) fn new(
        permission: Option<crate::permission::checker::PermCheck>,
        ask: Option<crate::permission::ask::AskSender>,
        workspace: Arc<crate::paths::WorkspaceBinding>,
    ) -> Self {
        Self {
            permission,
            ask,
            workspace,
        }
    }
}
impl<const OP: u8> rig::tool::Tool for ControlTool<OP> {
    const NAME: &'static str = match OP {
        0 => "task_result",
        1 => "task_list",
        2 => "task_cancel",
        _ => "invalid_task_control",
    };
    type Error = crate::agent::tools::ToolError;
    type Args = ControlArgs;
    type Output = String;
    fn description(&self) -> String {
        match OP {
            0 => "Read the bounded result/status of a background task ID belonging to this session. Running tasks return immediately; use task_list to check completion.".into(),
            1 => "List this session's background task IDs and current statuses. This never waits for jobs or creates model work.".into(),
            2 => "Request cancellation of one background task belonging to this session. Cancelling is not terminal until its owned model/effects retire; inspect task_result or task_list.".into(),
            _ => "Invalid internal operation".into(),
        }
    }
    fn parameters(&self) -> serde_json::Value {
        if OP == 1 {
            serde_json::json!({"type":"object", "properties":{}, "additionalProperties":false})
        } else {
            serde_json::json!({"type":"object", "properties":{"id":{"type":"string", "maxLength":64,"description":"Exact task ID returned by task or task_list"}},"required":["id"],"additionalProperties":false})
        }
    }
    async fn call(&self, args: ControlArgs) -> Result<String, Self::Error> {
        use crate::agent::tools::ToolError;
        let run = crate::spend::run::Run::current()
            .ok_or_else(|| ToolError::Msg("task control requires the owning session run".into()))?;
        let pool = run
            .background_pool()
            .ok_or_else(|| ToolError::Msg("task control has no owning session pool".into()))?;
        let validate = || -> anyhow::Result<()> {
            anyhow::ensure!(
                run.belongs_to_account(&pool.session, &pool.account),
                "task control account changed"
            );
            pool.validate_workspace(&self.workspace)
        };
        validate().map_err(|error| ToolError::Msg(error.to_string()))?;
        let id = match OP {
            1 if args.id.is_none() => "",
            0 | 2 => args
                .id
                .as_deref()
                .filter(|id| id.len() <= 64 && uuid::Uuid::parse_str(id).is_ok())
                .ok_or_else(|| ToolError::Msg("task control requires a valid task ID".into()))?,
            _ => return Err(ToolError::Msg("invalid task-control arguments".into())),
        };
        crate::agent::tools::check_perm(&self.permission, &self.ask, Self::NAME, id).await?;
        validate().map_err(|error| ToolError::Msg(error.to_string()))?;
        let value = match OP {
            0 => serde_json::to_value(
                pool.result(id)
                    .map_err(|error| ToolError::Msg(error.to_string()))?,
            ),
            1 => Ok(serde_json::Value::Array(
                pool.list()
                    .into_iter()
                    .map(|task| serde_json::json!({"id":task.id,"status":task.status}))
                    .collect(),
            )),
            2 => serde_json::to_value(
                pool.request_cancel(id)
                    .map_err(|error| ToolError::Msg(error.to_string()))?,
            ),
            _ => unreachable!(),
        }
        .map_err(|error| ToolError::Msg(error.to_string()))?;
        serde_json::to_string(&value).map_err(|error| ToolError::Msg(error.to_string()))
    }
}
