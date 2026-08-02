//! Serialized ownership of the broker-only JavaScript worker transport.
//!
//! The process may stay warm, but exactly one invocation at a time leases its pipes. Invocation
//! authority is supplied as a method-local effect handler and is never retained in shared state.

use std::future::Future;
use std::io::{Read, Write};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::protocol::{
    BuildIdentity, EffectRequest, EffectResponse, EffectResult, FrameError, InvocationId,
    ParentFrame, ParentHello, ParentProtocol, ParentWireFrame, RunStep, StepResult,
    VerificationResult, VerifyArtifact, WireFrame, WorkerFrame, WorkerWireFrame, read_frame,
    write_frame,
};
#[cfg(feature = "skills")]
use super::protocol::{SkillCallRequest, SkillCallResponse};
use super::types::{PermCancellation, STEP_TIMEOUT};
#[cfg(not(test))]
use crate::sandbox::worker::ProductionWorkerLauncher;
use crate::sandbox::worker::{WorkerLaunchError, WorkerLauncher, WorkerProcess};

const MAX_STDERR_OBSERVED_BYTES: usize = 4 * 1024;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const PROCESS_REAP_TIMEOUT: Duration = Duration::from_millis(500);
const STDERR_JOIN_TIMEOUT: Duration = Duration::from_millis(500);
const VERIFICATION_QUEUE_CAPACITY: usize = 16;

pub(crate) type EffectFuture<'a> = Pin<Box<dyn Future<Output = EffectResult> + Send + 'a>>;

/// Per-invocation callback for one already protocol-validated effect request.
pub(crate) trait InvocationEffectHandler: Send {
    fn handle_effect(
        &mut self,
        request: EffectRequest,
        cancellation: PermCancellation,
    ) -> EffectFuture<'_>;

    #[cfg(feature = "skills")]
    fn handle_skill_call(&mut self, request: SkillCallRequest) -> SkillCallResponse {
        SkillCallResponse {
            request_ordinal: request.request_ordinal,
            authorization: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum WorkerError {
    #[error("JavaScript worker containment is unavailable")]
    ContainmentUnavailable,
    #[error("JavaScript worker launch failed")]
    Launch,
    #[error("JavaScript worker transport failed")]
    Transport,
    #[error("JavaScript worker violated its protocol")]
    Protocol,
    #[error("JavaScript worker invocation was cancelled")]
    Cancelled,
    #[error("JavaScript worker invocation exceeded its deadline")]
    TimedOut,
    #[error("JavaScript worker returned a stale process generation")]
    StaleGeneration,
    #[error("JavaScript worker supervisor identity space is exhausted")]
    IdentityExhausted,
    #[error("blocking JavaScript verification cannot run inside a Tokio runtime")]
    BlockingVerifyInAsyncRuntime,
    #[error("JavaScript verification attempted an external effect")]
    UnexpectedVerificationEffect,
    #[error("JavaScript verification queue is at capacity")]
    VerificationQueueFull,
    #[error("JavaScript verification queue is unavailable")]
    VerificationQueueClosed,
}

impl WorkerError {
    pub(crate) fn is_retryable_admission_infrastructure(self) -> bool {
        matches!(
            self,
            Self::VerificationQueueFull | Self::VerificationQueueClosed
        )
    }
}

#[derive(Clone)]
pub(crate) struct JsWorkerSupervisor(Arc<SupervisorInner>);

// BEGIN AUTHORITY-FREE SUPERVISOR STATE
struct SupervisorInner {
    transport: tokio::sync::Mutex<SupervisorState>,
    launch_gate: Arc<tokio::sync::Mutex<()>>,
    launcher: Arc<dyn WorkerLauncher>,
    active_generation: AtomicU64,
    accepts_test_preamble: bool,
    watchdog: Duration,
    priority: Arc<InvocationPriority>,
    verification_scheduler: OnceLock<Result<VerificationScheduler, WorkerError>>,
}

struct SupervisorState {
    idle: Option<WorkerConnection>,
    next_generation: u64,
    next_invocation: u64,
}

struct WorkerConnection {
    generation: u64,
    sequence: u64,
    build: BuildIdentity,
    process: WorkerProcess,
    protocol: ParentProtocol,
    stderr_drain: BoundedStderrDrain,
}

struct BoundedStderrDrain {
    observed: Arc<AtomicUsize>,
    truncated: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct InvocationPriority {
    state: Mutex<InvocationPriorityState>,
    changed: Condvar,
    observed: tokio::sync::Notify,
}

#[derive(Default)]
struct InvocationPriorityState {
    interactive_waiters: usize,
    interactive_active: usize,
    verification_running: bool,
}

struct VerificationScheduler {
    sender: Mutex<Option<mpsc::SyncSender<VerificationJob>>>,
    queue: Arc<VerificationQueueObservation>,
}

#[derive(Default)]
struct VerificationQueueObservation {
    depth: Mutex<usize>,
    changed: tokio::sync::Notify,
}

struct VerificationJob {
    request: VerifyArtifact,
    cancellation: PermCancellation,
    deadline: Instant,
    reply: mpsc::SyncSender<Result<VerificationResult, WorkerError>>,
}
// END AUTHORITY-FREE SUPERVISOR STATE

impl Drop for BoundedStderrDrain {
    fn drop(&mut self) {
        // A standalone drain still never makes Drop unbounded. WorkerConnection performs the
        // ordered process teardown and bounded join before its fields are destroyed.
        let _ = self.thread.take();
    }
}

impl BoundedStderrDrain {
    fn join_bounded(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self
            .thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        if self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

impl Drop for WorkerConnection {
    fn drop(&mut self) {
        let _ = self.process.terminate_and_reap(PROCESS_REAP_TIMEOUT);
        self.stderr_drain.join_bounded(STDERR_JOIN_TIMEOUT);
    }
}

impl JsWorkerSupervisor {
    pub(crate) fn shared() -> Arc<Self> {
        static SHARED: OnceLock<Arc<JsWorkerSupervisor>> = OnceLock::new();
        SHARED
            .get_or_init(|| {
                #[cfg(test)]
                let launcher =
                    Arc::new(crate::sandbox::worker::TestWorkerLauncher::internal_worker_process());
                #[cfg(not(test))]
                let launcher = Arc::new(ProductionWorkerLauncher);
                Arc::new(Self::new(launcher, cfg!(test), STEP_TIMEOUT))
            })
            .clone()
    }

    fn new(
        launcher: Arc<dyn WorkerLauncher>,
        accepts_test_preamble: bool,
        watchdog: Duration,
    ) -> Self {
        Self(Arc::new(SupervisorInner {
            transport: tokio::sync::Mutex::new(SupervisorState {
                idle: None,
                next_generation: 1,
                next_invocation: 1,
            }),
            launch_gate: Arc::new(tokio::sync::Mutex::new(())),
            launcher,
            active_generation: AtomicU64::new(0),
            accepts_test_preamble,
            watchdog,
            priority: Arc::new(InvocationPriority::default()),
            verification_scheduler: OnceLock::new(),
        }))
    }

    #[cfg(test)]
    pub(crate) fn with_launcher_for_test(launcher: impl WorkerLauncher + 'static) -> Self {
        Self::new(Arc::new(launcher), true, STEP_TIMEOUT)
    }

    #[cfg(test)]
    pub(crate) fn with_launcher_and_watchdog_for_test(
        launcher: impl WorkerLauncher + 'static,
        watchdog: Duration,
    ) -> Self {
        Self::new(Arc::new(launcher), true, watchdog)
    }

    pub(crate) async fn execute(
        &self,
        request: RunStep,
        mut effects: impl InvocationEffectHandler,
        cancellation: PermCancellation,
    ) -> Result<StepResult, WorkerError> {
        self.execute_inner(request, &mut effects, cancellation, None)
            .await
    }

    /// Execute using the parent-created identity that also binds the invocation broker.
    ///
    /// The identity remains method-local and is never retained once the invocation finishes.
    pub(crate) async fn execute_bound(
        &self,
        invocation: InvocationId,
        request: RunStep,
        mut effects: impl InvocationEffectHandler,
        cancellation: PermCancellation,
    ) -> Result<StepResult, WorkerError> {
        self.execute_inner(request, &mut effects, cancellation, Some(invocation))
            .await
    }

    async fn execute_inner(
        &self,
        request: RunStep,
        effects: &mut impl InvocationEffectHandler,
        cancellation: PermCancellation,
        invocation: Option<InvocationId>,
    ) -> Result<StepResult, WorkerError> {
        match self
            .invoke_interactive(
                InvocationRequest::Run(request),
                Some(effects),
                cancellation,
                invocation,
            )
            .await?
        {
            InvocationTerminal::Step(result) => Ok(result),
            InvocationTerminal::Verification(_) => Err(WorkerError::Protocol),
        }
    }

    pub(crate) fn verify_blocking(
        &self,
        request: VerifyArtifact,
    ) -> Result<VerificationResult, WorkerError> {
        self.verify_blocking_cancellable(request, PermCancellation::new())
    }

    pub(crate) fn verify_blocking_cancellable(
        &self,
        request: VerifyArtifact,
        cancellation: PermCancellation,
    ) -> Result<VerificationResult, WorkerError> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(WorkerError::BlockingVerifyInAsyncRuntime);
        }
        if cancellation.is_cancelled() {
            return Err(WorkerError::Cancelled);
        }
        let deadline = Instant::now() + self.0.watchdog;
        self.verification_scheduler()?
            .submit(request, cancellation, deadline)
    }

    fn verification_scheduler(&self) -> Result<&VerificationScheduler, WorkerError> {
        self.0
            .verification_scheduler
            .get_or_init(|| VerificationScheduler::start(Arc::downgrade(&self.0)))
            .as_ref()
            .map_err(|error| *error)
    }

    async fn invoke_interactive<H: InvocationEffectHandler>(
        &self,
        request: InvocationRequest,
        mut effects: Option<&mut H>,
        cancellation: PermCancellation,
        invocation: Option<InvocationId>,
    ) -> Result<InvocationTerminal, WorkerError> {
        // The single deadline starts before lease acquisition and therefore bounds queueing,
        // startup, protocol I/O, JavaScript execution, and parent-brokered effect handling.
        let deadline = Instant::now() + self.0.watchdog;
        let waiter = self.0.priority.register_interactive();
        let mut state = await_controlled(self.0.transport.lock(), &cancellation, deadline).await?;
        let _active_interactive = waiter.activate();
        self.invoke_with_state(
            &mut state,
            request,
            &mut effects,
            cancellation,
            invocation,
            deadline,
        )
        .await
    }

    async fn invoke_scheduled_verification(
        &self,
        request: VerifyArtifact,
        cancellation: PermCancellation,
        deadline: Instant,
    ) -> Result<VerificationResult, WorkerError> {
        let mut state = await_controlled(self.0.transport.lock(), &cancellation, deadline).await?;
        match self
            .invoke_with_state::<RejectEffects>(
                &mut state,
                InvocationRequest::Verify(request),
                &mut None,
                cancellation,
                None,
                deadline,
            )
            .await?
        {
            InvocationTerminal::Verification(result) => Ok(result),
            InvocationTerminal::Step(_) => Err(WorkerError::Protocol),
        }
    }

    async fn invoke_with_state<H: InvocationEffectHandler>(
        &self,
        state: &mut SupervisorState,
        request: InvocationRequest,
        effects: &mut Option<&mut H>,
        cancellation: PermCancellation,
        invocation: Option<InvocationId>,
        deadline: Instant,
    ) -> Result<InvocationTerminal, WorkerError> {
        let mut connection = match state.idle.take() {
            Some(connection) => connection,
            None => {
                let generation = allocate_counter(&mut state.next_generation)?;
                launch_connection(
                    self.0.launcher.clone(),
                    self.0.launch_gate.clone(),
                    generation,
                    self.0.accepts_test_preamble,
                    &cancellation,
                    deadline,
                )
                .await?
            }
        };
        let invocation = match invocation {
            Some(invocation) => invocation,
            None => InvocationId::new(format!(
                "js-{}-{}",
                connection.generation,
                allocate_counter(&mut state.next_invocation)?
            ))
            .map_err(|_| WorkerError::IdentityExhausted)?,
        };
        self.0
            .active_generation
            .store(connection.generation, Ordering::Release);
        let mut active = ActiveGeneration {
            value: &self.0.active_generation,
            armed: true,
        };

        let result = run_invocation(
            &mut connection,
            invocation,
            request,
            effects,
            &cancellation,
            deadline,
        )
        .await;
        if result.is_ok() {
            state.idle = Some(connection);
        }
        active.armed = false;
        self.0.active_generation.store(0, Ordering::Release);
        result
    }

    /// Gracefully retires the current idle generation, with forced tree cleanup on any fault.
    pub(crate) async fn shutdown(&self) -> Result<(), WorkerError> {
        let cancellation = PermCancellation::new();
        let deadline = Instant::now() + self.0.watchdog;
        let mut state = await_controlled(self.0.transport.lock(), &cancellation, deadline).await?;
        let Some(mut connection) = state.idle.take() else {
            return Ok(());
        };
        let frame = WireFrame::connection(
            connection.build.clone(),
            connection.sequence,
            ParentFrame::Shutdown,
        );
        connection
            .protocol
            .on_send(&frame)
            .map_err(|_| WorkerError::Protocol)?;
        write_parent(&connection, frame, &cancellation, deadline).await?;
        loop {
            if connection
                .process
                .try_wait()
                .map_err(|_| WorkerError::Transport)?
                .is_some()
            {
                let status = connection
                    .process
                    .terminate_and_reap(PROCESS_REAP_TIMEOUT)
                    .map_err(|_| WorkerError::Transport)?;
                return if status.success() {
                    Ok(())
                } else {
                    Err(WorkerError::Transport)
                };
            }
            tokio::select! {
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    return Err(WorkerError::TimedOut);
                }
                _ = tokio::time::sleep(PROCESS_POLL_INTERVAL) => {}
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn shutdown_for_test(&self) -> Result<(), WorkerError> {
        self.shutdown().await
    }

    #[cfg(test)]
    pub(crate) async fn generation_for_test(&self) -> Option<u64> {
        self.0
            .transport
            .lock()
            .await
            .idle
            .as_ref()
            .map(|connection| connection.generation)
    }

    #[cfg(test)]
    pub(crate) async fn process_id_for_test(&self) -> Option<u32> {
        self.0
            .transport
            .lock()
            .await
            .idle
            .as_ref()
            .map(|connection| connection.process.id())
    }

    #[cfg(test)]
    pub(crate) async fn active_generation_for_test(&self) -> Option<u64> {
        match self.0.active_generation.load(Ordering::Acquire) {
            0 => None,
            generation => Some(generation),
        }
    }

    #[cfg(test)]
    pub(crate) async fn stderr_stats_for_test(&self) -> Option<StderrStats> {
        let state = self.0.transport.lock().await;
        let drain = &state.idle.as_ref()?.stderr_drain;
        Some(StderrStats {
            observed_bytes: drain.observed.load(Ordering::Acquire),
            retained_bytes: 0,
            truncated: drain.truncated.load(Ordering::Acquire),
        })
    }

    #[cfg(test)]
    pub(crate) fn verification_queue_capacity_for_test(&self) -> usize {
        VERIFICATION_QUEUE_CAPACITY
    }

    #[cfg(test)]
    pub(crate) fn verification_queue_depth_for_test(&self) -> usize {
        self.verification_scheduler()
            .map_or(0, VerificationScheduler::queue_depth)
    }

    #[cfg(test)]
    pub(crate) async fn wait_for_verification_queue_depth_for_test(&self, expected: usize) {
        let scheduler = self
            .verification_scheduler()
            .expect("verification scheduler must start for tests");
        scheduler.wait_for_queue_depth(expected).await;
    }

    #[cfg(test)]
    pub(crate) async fn wait_for_interactive_waiters_for_test(&self, expected: usize) {
        self.0.priority.wait_for_interactive_waiters(expected).await;
    }

    #[cfg(test)]
    pub(crate) fn close_verification_queue_for_test(&self) {
        if let Ok(scheduler) = self.verification_scheduler() {
            scheduler.close();
        }
    }
}

impl InvocationPriority {
    fn register_interactive(self: &Arc<Self>) -> InteractiveWaiter {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.interactive_waiters = state.interactive_waiters.saturating_add(1);
        drop(state);
        self.changed.notify_all();
        self.observed.notify_waiters();
        InteractiveWaiter {
            priority: Arc::clone(self),
            waiting: true,
        }
    }

    fn begin_verification(
        self: &Arc<Self>,
        cancellation: &PermCancellation,
        deadline: Instant,
    ) -> Result<VerificationLease, WorkerError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        while state.verification_running
            || state.interactive_waiters != 0
            || state.interactive_active != 0
        {
            if cancellation.is_cancelled() {
                return Err(WorkerError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(WorkerError::TimedOut);
            }
            let (next, _) = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|error| error.into_inner());
            state = next;
        }
        if cancellation.is_cancelled() {
            return Err(WorkerError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(WorkerError::TimedOut);
        }
        state.verification_running = true;
        Ok(VerificationLease {
            priority: Arc::clone(self),
        })
    }

    #[cfg(test)]
    async fn wait_for_interactive_waiters(&self, expected: usize) {
        loop {
            let changed = self.observed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let observed = self
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .interactive_waiters;
            if observed == expected {
                return;
            }
            changed.await;
        }
    }
}

struct InteractiveWaiter {
    priority: Arc<InvocationPriority>,
    waiting: bool,
}

impl InteractiveWaiter {
    fn activate(mut self) -> ActiveInteractive {
        let mut state = self
            .priority
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.interactive_waiters = state.interactive_waiters.saturating_sub(1);
        state.interactive_active = state.interactive_active.saturating_add(1);
        self.waiting = false;
        drop(state);
        self.priority.changed.notify_all();
        self.priority.observed.notify_waiters();
        ActiveInteractive {
            priority: Arc::clone(&self.priority),
        }
    }
}

impl Drop for InteractiveWaiter {
    fn drop(&mut self) {
        if !self.waiting {
            return;
        }
        let mut state = self
            .priority
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.interactive_waiters = state.interactive_waiters.saturating_sub(1);
        drop(state);
        self.priority.changed.notify_all();
        self.priority.observed.notify_waiters();
    }
}

struct ActiveInteractive {
    priority: Arc<InvocationPriority>,
}

impl Drop for ActiveInteractive {
    fn drop(&mut self) {
        let mut state = self
            .priority
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.interactive_active = state.interactive_active.saturating_sub(1);
        drop(state);
        self.priority.changed.notify_all();
        self.priority.observed.notify_waiters();
    }
}

struct VerificationLease {
    priority: Arc<InvocationPriority>,
}

impl Drop for VerificationLease {
    fn drop(&mut self) {
        let mut state = self
            .priority
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.verification_running = false;
        drop(state);
        self.priority.changed.notify_all();
        self.priority.observed.notify_waiters();
    }
}

impl VerificationScheduler {
    fn start(supervisor: Weak<SupervisorInner>) -> Result<Self, WorkerError> {
        let (sender, receiver) = mpsc::sync_channel(VERIFICATION_QUEUE_CAPACITY);
        let queue = Arc::new(VerificationQueueObservation::default());
        let worker_queue = Arc::clone(&queue);
        std::thread::Builder::new()
            .name("mini-agent-js-verification".into())
            .spawn(move || run_verification_scheduler(supervisor, receiver, worker_queue))
            .map_err(|_| WorkerError::VerificationQueueClosed)?;
        Ok(Self {
            sender: Mutex::new(Some(sender)),
            queue,
        })
    }

    fn submit(
        &self,
        request: VerifyArtifact,
        cancellation: PermCancellation,
        deadline: Instant,
    ) -> Result<VerificationResult, WorkerError> {
        if cancellation.is_cancelled() {
            return Err(WorkerError::Cancelled);
        }
        let (reply, response) = mpsc::sync_channel(1);
        let job = VerificationJob {
            request,
            cancellation,
            deadline,
            reply,
        };
        let sender = self
            .sender
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .ok_or(WorkerError::VerificationQueueClosed)?;
        let mut depth = self
            .queue
            .depth
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *depth >= VERIFICATION_QUEUE_CAPACITY {
            return Err(WorkerError::VerificationQueueFull);
        }
        match sender.try_send(job) {
            Ok(()) => {
                *depth = depth.saturating_add(1);
                drop(depth);
                self.queue.changed.notify_waiters();
            }
            Err(mpsc::TrySendError::Full(_)) => {
                return Err(WorkerError::VerificationQueueFull);
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                return Err(WorkerError::VerificationQueueClosed);
            }
        }
        response
            .recv()
            .unwrap_or(Err(WorkerError::VerificationQueueClosed))
    }

    #[cfg(test)]
    fn queue_depth(&self) -> usize {
        *self
            .queue
            .depth
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    #[cfg(test)]
    async fn wait_for_queue_depth(&self, expected: usize) {
        loop {
            let changed = self.queue.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.queue_depth() == expected {
                return;
            }
            changed.await;
        }
    }

    #[cfg(test)]
    fn close(&self) {
        self.sender
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }
}

fn run_verification_scheduler(
    supervisor: Weak<SupervisorInner>,
    receiver: mpsc::Receiver<VerificationJob>,
    queue: Arc<VerificationQueueObservation>,
) {
    while let Ok(job) = receiver.recv() {
        if job.cancellation.is_cancelled() {
            verification_dequeued(&queue);
            let _ = job.reply.send(Err(WorkerError::Cancelled));
            continue;
        }
        let Some(inner) = supervisor.upgrade() else {
            verification_dequeued(&queue);
            let _ = job.reply.send(Err(WorkerError::VerificationQueueClosed));
            break;
        };
        let _verification_lease = match inner
            .priority
            .begin_verification(&job.cancellation, job.deadline)
        {
            Ok(lease) => lease,
            Err(error) => {
                verification_dequeued(&queue);
                let _ = job.reply.send(Err(error));
                continue;
            }
        };
        verification_dequeued(&queue);
        if job.cancellation.is_cancelled() {
            let _ = job.reply.send(Err(WorkerError::Cancelled));
            continue;
        }
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
        {
            Ok(runtime) => runtime,
            Err(_) => {
                let _ = job.reply.send(Err(WorkerError::Transport));
                continue;
            }
        };
        let result = runtime.block_on(JsWorkerSupervisor(inner).invoke_scheduled_verification(
            job.request,
            job.cancellation,
            job.deadline,
        ));
        let _ = job.reply.send(result);
    }
}

fn verification_dequeued(queue: &VerificationQueueObservation) {
    let mut depth = queue
        .depth
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    *depth = depth.saturating_sub(1);
    drop(depth);
    queue.changed.notify_waiters();
}

struct ActiveGeneration<'a> {
    value: &'a AtomicU64,
    armed: bool,
}

impl Drop for ActiveGeneration<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.value.store(0, Ordering::Release);
        }
    }
}

struct EffectCancellation {
    cancellation: PermCancellation,
    armed: bool,
}

impl Drop for EffectCancellation {
    fn drop(&mut self) {
        if self.armed {
            self.cancellation.cancel();
        }
    }
}

enum InvocationRequest {
    Run(RunStep),
    Verify(VerifyArtifact),
}

enum InvocationTerminal {
    Step(StepResult),
    Verification(VerificationResult),
}

struct RejectEffects;

impl InvocationEffectHandler for RejectEffects {
    fn handle_effect(
        &mut self,
        _request: EffectRequest,
        _cancellation: PermCancellation,
    ) -> EffectFuture<'_> {
        Box::pin(async {
            EffectResult::Error(super::protocol::EffectError {
                code: super::protocol::EffectErrorCode::Denied,
            })
        })
    }
}

async fn launch_connection(
    launcher: Arc<dyn WorkerLauncher>,
    launch_gate: Arc<tokio::sync::Mutex<()>>,
    generation: u64,
    accepts_test_preamble: bool,
    cancellation: &PermCancellation,
    deadline: Instant,
) -> Result<WorkerConnection, WorkerError> {
    let process = launch_process(launcher, launch_gate, cancellation, deadline).await?;
    let stderr_drain = start_stderr_drain(&process)?;
    let build = BuildIdentity::current();
    let mut connection = WorkerConnection {
        generation,
        sequence: 0,
        build: build.clone(),
        process,
        protocol: ParentProtocol::new(build.clone()),
        stderr_drain,
    };
    let hello = WireFrame::connection(build, 0, ParentFrame::Hello(ParentHello {}));
    connection
        .protocol
        .on_send(&hello)
        .map_err(|_| WorkerError::Protocol)?;
    write_parent(&connection, hello, cancellation, deadline).await?;
    connection.sequence = 1;
    let ready = read_worker(
        &mut connection,
        accepts_test_preamble,
        cancellation,
        deadline,
    )
    .await?;
    connection
        .protocol
        .on_receive(&ready)
        .map_err(|_| WorkerError::Protocol)?;
    if !matches!(ready.message, WorkerFrame::Ready(_)) {
        return Err(WorkerError::Protocol);
    }
    connection.sequence = 2;
    Ok(connection)
}

async fn launch_process(
    launcher: Arc<dyn WorkerLauncher>,
    launch_gate: Arc<tokio::sync::Mutex<()>>,
    cancellation: &PermCancellation,
    deadline: Instant,
) -> Result<WorkerProcess, WorkerError> {
    // This lease belongs to the supervisor rather than one invocation. It moves through the
    // synchronous launch and any late-result cleanup, so a timed-out caller cannot enable another
    // OS launch while its abandoned launcher thread is still running.
    let launch_lease = await_controlled(launch_gate.lock_owned(), cancellation, deadline).await?;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("mini-agent-js-worker-launch".into())
        .spawn(move || {
            let delivered = LaunchDelivery::new(launcher.launch(), launch_lease);
            if let Err(mut rejected) = sender.send(delivered) {
                // Cancellation, caller-future drop, or the whole-call watchdog may win while the
                // synchronous platform launcher is still running. A process returned afterward
                // has no supervisor owner, so retire its complete tree on this detached thread.
                rejected.retire_now();
            }
        })
        .map_err(|_| WorkerError::Launch)?;

    let mut delivered = await_controlled(receiver, cancellation, deadline)
        .await?
        .map_err(|_| WorkerError::Launch)?;
    delivered
        .take()
        .expect("launch delivery is consumed exactly once")
        .map_err(map_launch_error)
}

struct LaunchDelivery {
    result: Option<Result<WorkerProcess, WorkerLaunchError>>,
    launch_lease: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl LaunchDelivery {
    fn new(
        result: Result<WorkerProcess, WorkerLaunchError>,
        launch_lease: tokio::sync::OwnedMutexGuard<()>,
    ) -> Self {
        Self {
            result: Some(result),
            launch_lease: Some(launch_lease),
        }
    }

    fn take(&mut self) -> Option<Result<WorkerProcess, WorkerLaunchError>> {
        self.result.take()
    }

    fn retire_now(&mut self) {
        if let Some(Ok(mut process)) = self.result.take() {
            let _ = process.terminate_and_reap(PROCESS_REAP_TIMEOUT);
        }
        self.launch_lease.take();
    }
}

impl Drop for LaunchDelivery {
    fn drop(&mut self) {
        let Some(Ok(process)) = self.result.take() else {
            return;
        };
        let retirement = LateLaunchRetirement {
            process: Some(process),
            launch_lease: self.launch_lease.take(),
        };
        // If a ready channel value loses a cancellation/deadline race, dropping the receiver must
        // not run the bounded process teardown on the async executor. The cleanup thread owns the
        // only remaining process handle and the launch lease until full-tree retirement finishes.
        // If spawning that thread fails, dropping its closure invokes the same ordered cleanup.
        let _ = std::thread::Builder::new()
            .name("mini-agent-js-late-launch-reap".into())
            .spawn(move || {
                retirement.retire();
            });
    }
}

struct LateLaunchRetirement {
    process: Option<WorkerProcess>,
    launch_lease: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl LateLaunchRetirement {
    fn retire(mut self) {
        self.retire_now();
    }

    fn retire_now(&mut self) {
        if let Some(mut process) = self.process.take() {
            let _ = process.terminate_and_reap(PROCESS_REAP_TIMEOUT);
        }
        self.launch_lease.take();
    }
}

impl Drop for LateLaunchRetirement {
    fn drop(&mut self) {
        self.retire_now();
    }
}

async fn run_invocation<H: InvocationEffectHandler>(
    connection: &mut WorkerConnection,
    invocation: InvocationId,
    request: InvocationRequest,
    effects: &mut Option<&mut H>,
    cancellation: &PermCancellation,
    deadline: Instant,
) -> Result<InvocationTerminal, WorkerError> {
    let parent_message = match request {
        InvocationRequest::Run(request) => ParentFrame::RunStep(request),
        InvocationRequest::Verify(request) => ParentFrame::VerifyArtifact(request),
    };
    let frame = WireFrame::invocation(
        connection.build.clone(),
        invocation.clone(),
        connection.sequence,
        parent_message,
    );
    connection
        .protocol
        .on_send(&frame)
        .map_err(|_| WorkerError::Protocol)?;
    write_parent(connection, frame, cancellation, deadline).await?;
    connection.sequence = advance(connection.sequence)?;

    loop {
        let frame = read_worker(connection, false, cancellation, deadline).await?;
        connection
            .protocol
            .on_receive(&frame)
            .map_err(|_| WorkerError::Protocol)?;
        connection.sequence = advance(connection.sequence)?;
        match frame.message {
            WorkerFrame::EffectRequest(request) => {
                let Some(handler) = effects.as_deref_mut() else {
                    return Err(WorkerError::UnexpectedVerificationEffect);
                };
                let effect_cancellation = PermCancellation::new();
                let mut cancel_on_drop = EffectCancellation {
                    cancellation: effect_cancellation.clone(),
                    armed: true,
                };
                let mut effect = handler.handle_effect(request.clone(), effect_cancellation);
                let result = loop {
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => return Err(WorkerError::Cancelled),
                        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                            return Err(WorkerError::TimedOut);
                        }
                        result = &mut effect => break result,
                        _ = tokio::time::sleep(PROCESS_POLL_INTERVAL) => {
                            if connection.process.try_wait().map_err(|_| WorkerError::Transport)?.is_some() {
                                return Err(WorkerError::Transport);
                            }
                        }
                    }
                };
                cancel_on_drop.armed = false;
                let response = WireFrame::invocation(
                    connection.build.clone(),
                    invocation.clone(),
                    connection.sequence,
                    ParentFrame::EffectResponse(EffectResponse {
                        effect_ordinal: request.effect_ordinal,
                        result,
                    }),
                );
                connection
                    .protocol
                    .on_send(&response)
                    .map_err(|_| WorkerError::Protocol)?;
                write_parent(connection, response, cancellation, deadline).await?;
                connection.sequence = advance(connection.sequence)?;
            }
            #[cfg(feature = "skills")]
            WorkerFrame::SkillCallRequest(request) => {
                let Some(handler) = effects.as_deref_mut() else {
                    return Err(WorkerError::UnexpectedVerificationEffect);
                };
                if cancellation.is_cancelled() {
                    return Err(WorkerError::Cancelled);
                }
                if Instant::now() >= deadline {
                    return Err(WorkerError::TimedOut);
                }
                let response = WireFrame::invocation(
                    connection.build.clone(),
                    invocation.clone(),
                    connection.sequence,
                    ParentFrame::SkillCallResponse(handler.handle_skill_call(request)),
                );
                connection
                    .protocol
                    .on_send(&response)
                    .map_err(|_| WorkerError::Protocol)?;
                write_parent(connection, response, cancellation, deadline).await?;
                connection.sequence = advance(connection.sequence)?;
            }
            WorkerFrame::StepResult(result) => return Ok(InvocationTerminal::Step(result)),
            WorkerFrame::VerificationResult(result) => {
                return Ok(InvocationTerminal::Verification(result));
            }
            WorkerFrame::ProtocolFault(_) => return Err(WorkerError::Protocol),
            WorkerFrame::Ready(_) => return Err(WorkerError::Protocol),
        }
    }
}

async fn write_parent(
    connection: &WorkerConnection,
    frame: ParentWireFrame,
    cancellation: &PermCancellation,
    deadline: Instant,
) -> Result<(), WorkerError> {
    let generation = connection.generation;
    let mut input = connection
        .process
        .input
        .try_clone()
        .map_err(|_| WorkerError::Transport)?;
    let task = tokio::task::spawn_blocking(move || {
        let result = write_frame(&mut input, &frame)
            .and_then(|()| input.flush().map_err(|error| FrameError::Io(error.kind())));
        TaggedIo { generation, result }
    });
    let tagged = await_controlled(task, cancellation, deadline)
        .await?
        .map_err(|_| WorkerError::Transport)?;
    validate_generation(connection.generation, tagged.generation)?;
    tagged.result.map_err(|_| WorkerError::Transport)
}

async fn read_worker(
    connection: &mut WorkerConnection,
    accepts_test_preamble: bool,
    cancellation: &PermCancellation,
    deadline: Instant,
) -> Result<WorkerWireFrame, WorkerError> {
    let generation = connection.generation;
    let mut output = connection
        .process
        .output
        .try_clone()
        .map_err(|_| WorkerError::Transport)?;
    let mut task = tokio::task::spawn_blocking(move || {
        let result = read_worker_frame(&mut output, accepts_test_preamble);
        TaggedIo { generation, result }
    });
    let tagged = loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(WorkerError::Cancelled),
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                return Err(WorkerError::TimedOut);
            }
            tagged = &mut task => break tagged.map_err(|_| WorkerError::Transport)?,
            _ = tokio::time::sleep(PROCESS_POLL_INTERVAL) => {
                if connection.process.try_wait().map_err(|_| WorkerError::Transport)?.is_some() {
                    return Err(WorkerError::Transport);
                }
            }
        }
    };
    validate_generation(connection.generation, tagged.generation)?;
    if connection
        .process
        .try_wait()
        .map_err(|_| WorkerError::Transport)?
        .is_some()
    {
        return Err(WorkerError::Transport);
    }
    tagged.result.map_err(|_| WorkerError::Transport)
}

struct TaggedIo<T> {
    generation: u64,
    result: T,
}

fn validate_generation(expected: u64, actual: u64) -> Result<(), WorkerError> {
    if expected == actual {
        Ok(())
    } else {
        Err(WorkerError::StaleGeneration)
    }
}

#[cfg(test)]
pub(crate) fn validate_generation_for_test(expected: u64, actual: u64) -> Result<(), WorkerError> {
    validate_generation(expected, actual)
}

fn read_worker_frame(
    reader: &mut impl Read,
    accepts_test_preamble: bool,
) -> Result<WorkerWireFrame, FrameError> {
    #[cfg(test)]
    if accepts_test_preamble {
        return read_worker_frame_after_test_preamble(reader);
    }
    #[cfg(not(test))]
    let _ = accepts_test_preamble;
    read_frame(reader)
}

#[cfg(test)]
fn read_worker_frame_after_test_preamble(
    reader: &mut impl Read,
) -> Result<WorkerWireFrame, FrameError> {
    let mut preamble = Vec::new();
    let mut window = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        reader
            .read_exact(&mut byte)
            .map_err(|error| FrameError::Io(error.kind()))?;
        window.push(byte[0]);
        if window.len() < 5 {
            continue;
        }
        let length = u32::from_be_bytes(window[..4].try_into().expect("four-byte window")) as usize;
        if length > 0 && length <= super::protocol::MAX_FRAME_BYTES && window[4] == b'{' {
            let mut encoded = window[..5].to_vec();
            let mut tail = vec![0_u8; length - 1];
            reader
                .read_exact(&mut tail)
                .map_err(|error| FrameError::Io(error.kind()))?;
            encoded.extend_from_slice(&tail);
            if let Ok(frame) = read_frame(&mut encoded.as_slice()) {
                return Ok(frame);
            }
        }
        preamble.push(window.remove(0));
        if preamble.len() > 4096 {
            return Err(FrameError::InvalidJson);
        }
    }
}

fn start_stderr_drain(process: &WorkerProcess) -> Result<BoundedStderrDrain, WorkerError> {
    let mut stderr = process
        .stderr
        .try_clone()
        .map_err(|_| WorkerError::Transport)?;
    let observed = Arc::new(AtomicUsize::new(0));
    let truncated = Arc::new(AtomicBool::new(false));
    let thread_observed = observed.clone();
    let thread_truncated = truncated.clone();
    let thread = std::thread::Builder::new()
        .name("mini-agent-js-worker-stderr".into())
        .spawn(move || {
            let mut buffer = [0_u8; 4096];
            while let Ok(count) = stderr.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                let previous = thread_observed.load(Ordering::Relaxed);
                let total = previous.saturating_add(count);
                thread_observed.store(total.min(MAX_STDERR_OBSERVED_BYTES), Ordering::Relaxed);
                if total > MAX_STDERR_OBSERVED_BYTES {
                    thread_truncated.store(true, Ordering::Relaxed);
                }
            }
        })
        .map_err(|_| WorkerError::Transport)?;
    Ok(BoundedStderrDrain {
        observed,
        truncated,
        thread: Some(thread),
    })
}

fn map_launch_error(error: WorkerLaunchError) -> WorkerError {
    match error {
        WorkerLaunchError::Unavailable { .. } => WorkerError::ContainmentUnavailable,
        WorkerLaunchError::Io { .. } | WorkerLaunchError::MissingPipe { .. } => WorkerError::Launch,
    }
}

fn allocate_counter(counter: &mut u64) -> Result<u64, WorkerError> {
    let value = *counter;
    *counter = counter
        .checked_add(1)
        .ok_or(WorkerError::IdentityExhausted)?;
    Ok(value)
}

fn advance(sequence: u64) -> Result<u64, WorkerError> {
    sequence
        .checked_add(1)
        .ok_or(WorkerError::IdentityExhausted)
}

async fn await_controlled<F: Future>(
    future: F,
    cancellation: &PermCancellation,
    deadline: Instant,
) -> Result<F::Output, WorkerError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(WorkerError::Cancelled),
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
            Err(WorkerError::TimedOut)
        }
        output = future => Ok(output),
    }
}

#[cfg(test)]
pub(crate) struct StderrStats {
    pub(crate) observed_bytes: usize,
    pub(crate) retained_bytes: usize,
    pub(crate) truncated: bool,
}
