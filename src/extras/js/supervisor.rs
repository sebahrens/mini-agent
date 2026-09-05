//! Serialized ownership of the broker-only JavaScript worker transport.
//!
//! The process may stay warm, but exactly one invocation at a time leases its pipes. Invocation
//! authority is supplied as a method-local effect handler and is never retained in shared state.

use std::future::Future;
use std::io::{Read, Write};
use std::pin::Pin;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::protocol::{
    BuildIdentity, DiagnosticClass, EffectErrorCode, EffectRequest, EffectResponse, EffectResult,
    FrameError, InvocationId, JsErrorCode, ParentFrame, ParentProtocol, ParentWireFrame, RunStep,
    StepOutcome, StepResult, VerificationResult, VerifyArtifact, WireFrame, WorkerFrame,
    WorkerWireFrame, read_frame, write_frame,
};
#[cfg(feature = "skills")]
use super::protocol::{SkillCallRequest, SkillCallResponse};
use super::types::{PermCancellation, STEP_TIMEOUT};
#[cfg(not(test))]
use crate::sandbox::worker::ProductionWorkerLauncher;
use crate::sandbox::worker::{WorkerLaunchError, WorkerLauncher, WorkerProcess};

const MAX_STDERR_OBSERVED_BYTES: usize = 4 * 1024;
/// Polling interval for detecting worker process exit. Raised from 10ms to 250ms to reduce
/// per-frame IPC overhead: thousands of unnecessary wakeups and syscalls during long effects are
/// now avoided. Worst-case worker-death detection latency is therefore 250ms.
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// A pipe can report EOF just before the OS makes the worker's exit status observable. Keep this
/// window short and bounded so native resource exits retain their closed classification without a
/// malformed or abandoned transport delaying the caller materially.
const PROCESS_EXIT_RECONCILIATION_TIMEOUT: Duration = Duration::from_millis(100);
const PROCESS_REAP_TIMEOUT: Duration = Duration::from_millis(500);
const STDERR_JOIN_TIMEOUT: Duration = Duration::from_millis(500);
/// Effect services get a short, independent drain window after invocation cancellation. This is
/// long enough for bounded process-tree teardown and durable unknown-outcome reconciliation, but
/// never extends an abandoned caller indefinitely.
const EFFECT_CANCELLATION_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const VERIFICATION_QUEUE_CAPACITY: usize = 16;
const MAX_PROCESS_AGE: Duration = Duration::from_secs(15 * 60);
const MAX_PROCESS_INVOCATIONS: u64 = 256;

pub(crate) type EffectFuture<'a> = Pin<Box<dyn Future<Output = EffectResult> + Send + 'a>>;

/// Per-invocation callback for one already protocol-validated effect request.
pub(crate) trait InvocationEffectHandler: Send {
    fn handle_effect(
        &mut self,
        request: EffectRequest,
        cancellation: PermCancellation,
    ) -> EffectFuture<'_>;

    /// Reconcile an effect future which did not stop within the bounded cancellation drain.
    /// Implementations backed by durable intent should append `OutcomeUnknown` here. The secure
    /// default is ambiguous rather than implying that an uncooperative effect did not happen.
    fn reconcile_interrupted_effect(&mut self) -> EffectResult {
        EffectResult::Error(super::protocol::EffectError {
            code: EffectErrorCode::OutcomeUnknown,
        })
    }

    #[cfg(feature = "skills")]
    fn handle_skill_call(&mut self, request: SkillCallRequest) -> SkillCallResponse {
        SkillCallResponse {
            request_ordinal: request.request_ordinal,
            authorization: None,
        }
    }

    /// Erase invocation authority after a terminal result that leaves the worker reusable.
    fn finish_invocation(&mut self) {}

    /// Erase invocation authority after any worker/process/protocol fault or poisoned runtime.
    fn recycle_invocation(&mut self) {
        self.finish_invocation();
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
    #[error("JavaScript worker exhausted its native CPU allowance")]
    NativeCpuLimit,
    #[error("JavaScript worker violated its protocol")]
    Protocol,
    #[error("JavaScript worker build identity differs from the parent")]
    BuildMismatch,
    #[error("JavaScript worker invocation was cancelled")]
    Cancelled,
    #[error("JavaScript worker invocation exceeded its deadline")]
    TimedOut,
    #[error("JavaScript effect completed with an unknown outcome")]
    EffectOutcomeUnknown,
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

static SHARED_SUPERVISOR: OnceLock<Arc<JsWorkerSupervisor>> = OnceLock::new();

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
    reuse_policy: WorkerReusePolicy,
    #[cfg(test)]
    idle_retirement: tokio::sync::Notify,
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
    created_at: Instant,
    completed_invocations: u64,
    retirement: Option<Arc<RetirementTicket>>,
    /// Dedicated protocol handles cloned once when the connection is established.
    input_handle: Arc<Mutex<std::fs::File>>,
    output_handle: Arc<Mutex<std::fs::File>>,
}

struct RetirementTicket {
    cancelled: Mutex<bool>,
    wake: Condvar,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct WorkerReusePolicy {
    max_age: Duration,
    max_invocations: u64,
}

impl WorkerReusePolicy {
    pub(crate) const fn new(max_age: Duration, max_invocations: u64) -> Self {
        Self {
            max_age,
            max_invocations,
        }
    }

    fn permits(&self, connection: &WorkerConnection, now: Instant) -> bool {
        self.max_invocations > 0
            && connection.completed_invocations < self.max_invocations
            && now.saturating_duration_since(connection.created_at) < self.max_age
    }
}

impl Default for WorkerReusePolicy {
    fn default() -> Self {
        Self::new(MAX_PROCESS_AGE, MAX_PROCESS_INVOCATIONS)
    }
}

impl RetirementTicket {
    fn new() -> Self {
        Self {
            cancelled: Mutex::new(false),
            wake: Condvar::new(),
        }
    }

    fn cancel(&self) {
        *self
            .cancelled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        self.wake.notify_all();
    }

    fn wait_until(&self, deadline: Instant) -> bool {
        let mut cancelled = self
            .cancelled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if *cancelled {
                return false;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return true;
            }
            let (next, timeout) = self
                .wake
                .wait_timeout(cancelled, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            cancelled = next;
            if timeout.timed_out() && !*cancelled {
                return true;
            }
        }
    }
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
    reply: VerificationReplySender,
}

#[derive(Default)]
struct VerificationReply {
    result: Mutex<Option<Result<VerificationResult, WorkerError>>>,
    changed: Condvar,
}

struct VerificationReplySender {
    reply: Arc<VerificationReply>,
    armed: bool,
}
// END AUTHORITY-FREE SUPERVISOR STATE

impl VerificationReply {
    fn complete(&self, result: Result<VerificationResult, WorkerError>) {
        let mut slot = self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if slot.is_none() {
            *slot = Some(result);
        }
        drop(slot);
        self.changed.notify_all();
    }

    fn wait(
        self: &Arc<Self>,
        cancellation: &PermCancellation,
    ) -> Result<VerificationResult, WorkerError> {
        let weak_reply = Arc::downgrade(self);
        let _cancel_wake = cancellation.register_blocking_wake(Arc::new(move || {
            let Some(reply) = weak_reply.upgrade() else {
                return;
            };
            let _result = reply
                .result
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            reply.changed.notify_all();
        }));
        let mut slot = self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        loop {
            if let Some(result) = slot.take() {
                return result;
            }
            if cancellation.is_cancelled() {
                return Err(WorkerError::Cancelled);
            }
            slot = self
                .changed
                .wait(slot)
                .unwrap_or_else(|error| error.into_inner());
        }
    }
}

impl VerificationReplySender {
    fn new(reply: Arc<VerificationReply>) -> Self {
        Self { reply, armed: true }
    }

    fn send(mut self, result: Result<VerificationResult, WorkerError>) {
        self.reply.complete(result);
        self.armed = false;
    }
}

impl Drop for VerificationReplySender {
    fn drop(&mut self) {
        if self.armed {
            self.reply
                .complete(Err(WorkerError::VerificationQueueClosed));
        }
    }
}

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
        if self.thread.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        }
    }
}

impl Drop for WorkerConnection {
    fn drop(&mut self) {
        if let Some(retirement) = &self.retirement {
            retirement.cancel();
        }
        let _ = self.process.terminate_and_reap(PROCESS_REAP_TIMEOUT);
        self.stderr_drain.join_bounded(STDERR_JOIN_TIMEOUT);
    }
}

impl JsWorkerSupervisor {
    pub(crate) fn shared() -> Arc<Self> {
        SHARED_SUPERVISOR
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

    /// Shuts down the process-wide worker if JavaScript was used, without initializing it merely
    /// to perform application cleanup.
    pub(crate) async fn shutdown_shared() -> Result<(), WorkerError> {
        match SHARED_SUPERVISOR.get() {
            Some(supervisor) => supervisor.shutdown().await,
            None => Ok(()),
        }
    }

    fn new(
        launcher: Arc<dyn WorkerLauncher>,
        accepts_test_preamble: bool,
        watchdog: Duration,
    ) -> Self {
        Self::new_with_policy(
            launcher,
            accepts_test_preamble,
            watchdog,
            WorkerReusePolicy::default(),
        )
    }

    fn new_with_policy(
        launcher: Arc<dyn WorkerLauncher>,
        accepts_test_preamble: bool,
        watchdog: Duration,
        reuse_policy: WorkerReusePolicy,
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
            reuse_policy,
            #[cfg(test)]
            idle_retirement: tokio::sync::Notify::new(),
        }))
    }

    #[cfg(test)]
    pub(crate) fn with_launcher_for_test(launcher: impl WorkerLauncher + 'static) -> Self {
        Self::new(Arc::new(launcher), true, STEP_TIMEOUT)
    }

    /// Test-owned supervisor for benchmarking an installed production executable. Unlike
    /// libtest worker launchers, the installed binary emits no test-runner preamble.
    #[cfg(test)]
    pub(crate) fn with_production_launcher_for_benchmark(
        launcher: impl WorkerLauncher + 'static,
    ) -> Self {
        Self::new(Arc::new(launcher), false, STEP_TIMEOUT)
    }

    #[cfg(test)]
    pub(crate) fn with_launcher_and_watchdog_for_test(
        launcher: impl WorkerLauncher + 'static,
        watchdog: Duration,
    ) -> Self {
        Self::new(Arc::new(launcher), true, watchdog)
    }

    #[cfg(test)]
    pub(crate) fn with_launcher_and_policy_for_test(
        launcher: impl WorkerLauncher + 'static,
        watchdog: Duration,
        reuse_policy: WorkerReusePolicy,
    ) -> Self {
        Self::new_with_policy(Arc::new(launcher), true, watchdog, reuse_policy)
    }

    #[cfg(test)]
    pub(crate) async fn wait_for_idle_retirement_for_test(&self) {
        self.0.idle_retirement.notified().await;
    }

    pub(crate) async fn execute(
        &self,
        request: RunStep,
        mut effects: impl InvocationEffectHandler,
        cancellation: PermCancellation,
    ) -> Result<StepResult, WorkerError> {
        self.execute_inner(request, &mut effects, cancellation, None, None)
            .await
    }

    /// Execute using the parent-created identity that also binds the invocation broker.
    ///
    /// The identity remains method-local and is never retained once the invocation finishes.
    pub(crate) async fn execute_bound(
        &self,
        invocation: InvocationId,
        request: RunStep,
        effects: impl InvocationEffectHandler,
        cancellation: PermCancellation,
    ) -> Result<StepResult, WorkerError> {
        self.execute_bound_with_deadline(invocation, request, effects, cancellation, None)
            .await
    }

    pub(crate) async fn execute_bound_with_deadline(
        &self,
        invocation: InvocationId,
        request: RunStep,
        mut effects: impl InvocationEffectHandler,
        cancellation: PermCancellation,
        deadline: Option<Instant>,
    ) -> Result<StepResult, WorkerError> {
        self.execute_inner(
            request,
            &mut effects,
            cancellation,
            Some(invocation),
            deadline,
        )
        .await
    }

    async fn execute_inner(
        &self,
        request: RunStep,
        effects: &mut impl InvocationEffectHandler,
        cancellation: PermCancellation,
        invocation: Option<InvocationId>,
        deadline_override: Option<Instant>,
    ) -> Result<StepResult, WorkerError> {
        match self
            .invoke_interactive(
                InvocationRequest::Run(request),
                Some(effects),
                cancellation,
                invocation,
                deadline_override,
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
        deadline_override: Option<Instant>,
    ) -> Result<InvocationTerminal, WorkerError> {
        // The single deadline starts before lease acquisition and therefore bounds queueing,
        // startup, protocol I/O, JavaScript execution, and parent-brokered effect handling.
        // If a deadline is provided from the caller (e.g., from JsTool::call), use that absolute
        // deadline. Otherwise, create a fresh deadline from the watchdog duration.
        let deadline = deadline_override.unwrap_or_else(|| Instant::now() + self.0.watchdog);
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
        let mut authority = InvocationAuthority::new(effects.take());
        let mut connection = match state
            .idle
            .take()
            .filter(|connection| self.0.reuse_policy.permits(connection, Instant::now()))
        {
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
            &mut authority,
            &cancellation,
            deadline,
        )
        .await;
        let reusable_terminal = result.as_ref().is_ok_and(terminal_is_reusable);
        if reusable_terminal {
            authority.finish();
            connection.completed_invocations = connection.completed_invocations.saturating_add(1);
        } else {
            authority.recycle();
        }
        if reusable_terminal && self.0.reuse_policy.permits(&connection, Instant::now()) {
            store_idle_with_retirement(&self.0, state, connection);
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

    #[cfg(all(test, windows))]
    pub(crate) async fn windows_process_observation_for_test(
        &self,
    ) -> std::io::Result<Option<crate::sandbox::worker::WindowsWorkerProcessObservation>> {
        self.0
            .transport
            .lock()
            .await
            .idle
            .as_ref()
            .map(|connection| connection.process.windows_process_observation_for_test())
            .transpose()
    }

    /// Launch through the configured production containment backend and stop at the authenticated
    /// `Ready` boundary. This exists only for the reproducible worker benchmark: ordinary callers
    /// must continue to launch lazily as part of an invocation.
    #[cfg(test)]
    pub(crate) async fn prepare_ready_for_benchmark_for_test(&self) -> Result<(), WorkerError> {
        let cancellation = PermCancellation::new();
        let deadline = Instant::now() + self.0.watchdog;
        let mut state = await_controlled(self.0.transport.lock(), &cancellation, deadline).await?;
        if state.idle.is_none() {
            let generation = allocate_counter(&mut state.next_generation)?;
            let connection = launch_connection(
                self.0.launcher.clone(),
                self.0.launch_gate.clone(),
                generation,
                self.0.accepts_test_preamble,
                &cancellation,
                deadline,
            )
            .await?;
            store_idle_with_retirement(&self.0, &mut state, connection);
        }
        Ok(())
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
        let weak_priority = Arc::downgrade(self);
        let _cancel_wake = cancellation.register_blocking_wake(Arc::new(move || {
            let Some(priority) = weak_priority.upgrade() else {
                return;
            };
            let _state = priority
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            priority.changed.notify_all();
        }));
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
        let reply = Arc::new(VerificationReply::default());
        let job = VerificationJob {
            request,
            cancellation: cancellation.clone(),
            deadline,
            reply: VerificationReplySender::new(Arc::clone(&reply)),
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
        reply.wait(&cancellation)
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
            job.reply.send(Err(WorkerError::Cancelled));
            continue;
        }
        let Some(inner) = supervisor.upgrade() else {
            verification_dequeued(&queue);
            job.reply.send(Err(WorkerError::VerificationQueueClosed));
            break;
        };
        let _verification_lease = match inner
            .priority
            .begin_verification(&job.cancellation, job.deadline)
        {
            Ok(lease) => lease,
            Err(error) => {
                verification_dequeued(&queue);
                job.reply.send(Err(error));
                continue;
            }
        };
        verification_dequeued(&queue);
        if job.cancellation.is_cancelled() {
            job.reply.send(Err(WorkerError::Cancelled));
            continue;
        }
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
        {
            Ok(runtime) => runtime,
            Err(_) => {
                job.reply.send(Err(WorkerError::Transport));
                continue;
            }
        };
        let result = runtime.block_on(JsWorkerSupervisor(inner).invoke_scheduled_verification(
            job.request,
            job.cancellation,
            job.deadline,
        ));
        job.reply.send(result);
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

struct InvocationAuthority<'a, H: InvocationEffectHandler> {
    handler: Option<&'a mut H>,
    terminal: bool,
}

impl<'a, H: InvocationEffectHandler> InvocationAuthority<'a, H> {
    fn new(handler: Option<&'a mut H>) -> Self {
        Self {
            handler,
            terminal: false,
        }
    }

    fn handler(&mut self) -> &mut Option<&'a mut H> {
        &mut self.handler
    }

    fn finish(&mut self) {
        if self.terminal {
            return;
        }
        if let Some(handler) = self.handler.as_deref_mut() {
            handler.finish_invocation();
        }
        self.terminal = true;
    }

    fn recycle(&mut self) {
        if self.terminal {
            return;
        }
        if let Some(handler) = self.handler.as_deref_mut() {
            handler.recycle_invocation();
        }
        self.terminal = true;
    }
}

impl<H: InvocationEffectHandler> Drop for InvocationAuthority<'_, H> {
    fn drop(&mut self) {
        if !self.terminal {
            self.recycle();
        }
    }
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

fn terminal_is_reusable(terminal: &InvocationTerminal) -> bool {
    match terminal {
        InvocationTerminal::Step(result) => step_outcome_is_reusable(&result.outcome),
        InvocationTerminal::Verification(result) => verification_result_is_reusable(result),
    }
}

fn step_outcome_is_reusable(outcome: &StepOutcome) -> bool {
    matches!(
        outcome,
        StepOutcome::Value(_)
            | StepOutcome::Void
            | StepOutcome::Error(
                JsErrorCode::Syntax
                    | JsErrorCode::Exception
                    | JsErrorCode::EffectLimit
                    | JsErrorCode::InvalidResult
            )
    )
}

fn verification_result_is_reusable(result: &VerificationResult) -> bool {
    result.cases.iter().all(|case| {
        !case.diagnostic.as_ref().is_some_and(|diagnostic| {
            matches!(
                diagnostic.class,
                DiagnosticClass::ResourceLimit | DiagnosticClass::Internal
            )
        })
    })
}

fn store_idle_with_retirement(
    inner: &Arc<SupervisorInner>,
    state: &mut SupervisorState,
    mut connection: WorkerConnection,
) {
    if connection.retirement.is_some() {
        state.idle = Some(connection);
        return;
    }
    let generation = connection.generation;
    let retire_at = connection
        .created_at
        .checked_add(inner.reuse_policy.max_age)
        .unwrap_or(connection.created_at);
    let ticket = Arc::new(RetirementTicket::new());
    let waiting_ticket = ticket.clone();
    let weak = Arc::downgrade(inner);
    connection.retirement = Some(ticket);
    let retirement = std::thread::Builder::new()
        .name(format!("mini-agent-js-idle-retire-{generation}"))
        .spawn(move || retire_idle_generation(weak, generation, retire_at, waiting_ticket));
    if retirement.is_ok() {
        state.idle = Some(connection);
    }
}

fn retire_idle_generation(
    weak: Weak<SupervisorInner>,
    generation: u64,
    retire_at: Instant,
    ticket: Arc<RetirementTicket>,
) {
    if !ticket.wait_until(retire_at) {
        return;
    }
    let Some(inner) = weak.upgrade() else {
        return;
    };
    let retired = {
        let mut state = inner.transport.blocking_lock();
        if state
            .idle
            .as_ref()
            .is_some_and(|connection| connection.generation == generation)
        {
            state.idle.take()
        } else {
            None
        }
    };
    let did_retire = retired.is_some();
    drop(retired);
    #[cfg(test)]
    if did_retire {
        inner.idle_retirement.notify_one();
    }
    #[cfg(not(test))]
    let _ = did_retire;
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

    // Clone the protocol handles once per connection instead of issuing dup() for every frame.
    // A clone failure is launch-terminal: retaining a connection without a usable transport would
    // only defer the same closed Transport error until the first frame.
    let input_handle = process
        .input
        .try_clone()
        .map_err(|_| WorkerError::Transport)?;
    let output_handle = process
        .output
        .try_clone()
        .map_err(|_| WorkerError::Transport)?;

    let mut connection = WorkerConnection {
        generation,
        sequence: 0,
        build: build.clone(),
        process,
        protocol: ParentProtocol::new(build.clone()),
        stderr_drain,
        created_at: Instant::now(),
        completed_invocations: 0,
        retirement: None,
        input_handle: Arc::new(Mutex::new(input_handle)),
        output_handle: Arc::new(Mutex::new(output_handle)),
    };
    let hello = WireFrame::connection(build, 0, ParentFrame::Hello(connection.protocol.hello()));
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
        .map_err(map_protocol_error)?;
    match ready.message {
        WorkerFrame::Ready(_) => {}
        WorkerFrame::ProtocolFault(fault)
            if matches!(
                fault.code,
                super::protocol::ProtocolFaultCode::BuildMismatch
                    | super::protocol::ProtocolFaultCode::VersionMismatch
            ) =>
        {
            return Err(WorkerError::BuildMismatch);
        }
        _ => return Err(WorkerError::Protocol),
    }
    connection
        .process
        .finalize_authenticated_ready()
        .map_err(|_| WorkerError::Launch)?;
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
    authority: &mut InvocationAuthority<'_, H>,
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
            .map_err(map_protocol_error)?;
        connection.sequence = advance(connection.sequence)?;
        match frame.message {
            WorkerFrame::EffectRequest(request) => {
                let Some(handler) = authority.handler().as_deref_mut() else {
                    return Err(WorkerError::UnexpectedVerificationEffect);
                };
                let effect_cancellation = PermCancellation::new();
                let mut cancel_on_drop = EffectCancellation {
                    cancellation: effect_cancellation.clone(),
                    armed: true,
                };
                let effect_ordinal = request.effect_ordinal;
                let mut effect = handler.handle_effect(*request, effect_cancellation);
                enum EffectWait {
                    Completed(EffectResult),
                    Cancelled,
                    TimedOut,
                }
                let wait = loop {
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => break EffectWait::Cancelled,
                        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                            break EffectWait::TimedOut;
                        }
                        result = &mut effect => break EffectWait::Completed(result),
                        _ = tokio::time::sleep(PROCESS_POLL_INTERVAL) => {
                            if let Some(exit_status) = connection.process.try_wait().map_err(|_| WorkerError::Transport)? {
                                return Err(classify_worker_exit(exit_status));
                            }
                        }
                    }
                };
                let caller_cancelled = matches!(&wait, EffectWait::Cancelled);
                let (result, interrupted) = match wait {
                    EffectWait::Completed(result) => (result, None),
                    EffectWait::Cancelled | EffectWait::TimedOut => {
                        // Do not drop an in-flight service as soon as the caller leaves. Signal it,
                        // then let it kill/reap owned processes and append a truthful completion
                        // (including OutcomeUnknown) before invocation authority is erased.
                        cancel_on_drop.cancellation.cancel();
                        let interrupted = if caller_cancelled {
                            WorkerError::Cancelled
                        } else {
                            WorkerError::TimedOut
                        };
                        let result =
                            tokio::time::timeout(EFFECT_CANCELLATION_DRAIN_TIMEOUT, &mut effect)
                                .await;
                        match result {
                            Ok(result) => (result, Some(interrupted)),
                            Err(_) => {
                                drop(effect);
                                cancel_on_drop.armed = false;
                                let result = handler.reconcile_interrupted_effect();
                                let outcome_unknown = interrupted_effect_requires_unknown(&result);
                                authority.recycle();
                                return Err(if outcome_unknown {
                                    WorkerError::EffectOutcomeUnknown
                                } else {
                                    interrupted
                                });
                            }
                        }
                    }
                };
                cancel_on_drop.armed = false;
                drop(effect);
                let outcome_unknown = matches!(
                    &result,
                    EffectResult::Error(super::protocol::EffectError {
                        code: EffectErrorCode::OutcomeUnknown,
                    })
                );
                let interrupted_outcome_unknown = interrupted
                    .as_ref()
                    .is_some_and(|_| interrupted_effect_requires_unknown(&result));
                if outcome_unknown {
                    authority.recycle();
                }
                if let Some(interrupted) = interrupted {
                    // Invocation cancellation breaks protocol continuation even when the service
                    // proves that it stopped before mutation. Unknown outcomes take precedence so
                    // callers are never invited to replay an effect which may have happened.
                    return Err(if interrupted_outcome_unknown {
                        WorkerError::EffectOutcomeUnknown
                    } else {
                        interrupted
                    });
                }
                let response = WireFrame::invocation(
                    connection.build.clone(),
                    invocation.clone(),
                    connection.sequence,
                    ParentFrame::EffectResponse(EffectResponse {
                        effect_ordinal,
                        result,
                    }),
                );
                connection
                    .protocol
                    .on_send(&response)
                    .map_err(|_| WorkerError::Protocol)?;
                if outcome_unknown {
                    // The durable effect outcome remains unknown even if the poisoned worker can
                    // no longer receive its terminal response. Best-effort delivery closes the
                    // worker state machine; the parent always returns the truthful effect error.
                    let _ = write_parent(connection, response, cancellation, deadline).await;
                    return Err(WorkerError::EffectOutcomeUnknown);
                }
                write_parent(connection, response, cancellation, deadline).await?;
                connection.sequence = advance(connection.sequence)?;
            }
            #[cfg(feature = "skills")]
            WorkerFrame::SkillCallRequest(request) => {
                let Some(handler) = authority.handler().as_deref_mut() else {
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
            WorkerFrame::ProtocolFault(fault) => {
                return Err(
                    if fault.code == super::protocol::ProtocolFaultCode::BuildMismatch {
                        WorkerError::BuildMismatch
                    } else {
                        WorkerError::Protocol
                    },
                );
            }
            WorkerFrame::Ready(_) | WorkerFrame::ContainmentAttested(_) => {
                return Err(WorkerError::Protocol);
            }
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
    let input_handle = connection.input_handle.clone();
    let task = tokio::task::spawn_blocking(move || {
        let mut input = input_handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = write_frame(&mut *input, &frame)
            .and_then(|()| input.flush().map_err(|error| FrameError::Io(error.kind())));
        TaggedIo { generation, result }
    });
    let tagged = await_controlled(task, cancellation, deadline)
        .await?
        .map_err(|_| WorkerError::Transport)?;
    validate_generation(connection.generation, tagged.generation)?;
    tagged.result.map_err(map_frame_error)?;
    #[cfg(test)]
    connection.process.notify_parent_write_for_test();
    Ok(())
}

/// Cancellation can win the parent select after a mutating service has already completed but
/// before its response is delivered to the worker. Those successful results are ambiguous to the
/// caller even though the durable audit knows they completed. Audit failures are also ambiguous:
/// the parent cannot safely prove whether the effect preceded the failed completion append.
fn interrupted_effect_requires_unknown(result: &EffectResult) -> bool {
    matches!(
        result,
        EffectResult::WriteFile
            | EffectResult::Fetch { .. }
            | EffectResult::Spawn { .. }
            | EffectResult::ProposalAccepted { .. }
            | EffectResult::Error(super::protocol::EffectError {
                code: EffectErrorCode::AuditFailure | EffectErrorCode::OutcomeUnknown,
            })
    )
}

async fn read_worker(
    connection: &mut WorkerConnection,
    accepts_test_preamble: bool,
    cancellation: &PermCancellation,
    deadline: Instant,
) -> Result<WorkerWireFrame, WorkerError> {
    let generation = connection.generation;
    let output_handle = connection.output_handle.clone();
    let mut task = tokio::task::spawn_blocking(move || {
        let mut output = output_handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = read_worker_frame(&mut *output, accepts_test_preamble);
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
                if let Some(exit_status) = connection.process.try_wait().map_err(|_| WorkerError::Transport)? {
                    return Err(classify_worker_exit(exit_status));
                }
            }
        }
    };
    validate_generation(connection.generation, tagged.generation)?;
    let exit_status = connection
        .process
        .try_wait()
        .map_err(|_| WorkerError::Transport)?;
    match tagged.result {
        // A handshake mismatch is terminal by design: the worker writes this authenticated fault
        // and exits immediately. Preserve the frame long enough for the protocol state machine to
        // validate it instead of flattening it into a transport failure.
        Ok(frame) if matches!(frame.message, WorkerFrame::ProtocolFault(_)) => Ok(frame),
        Ok(frame) => match exit_status {
            Some(status) => Err(classify_worker_exit(status)),
            None => Ok(frame),
        },
        Err(error) => {
            let error = map_frame_error(error);
            if error == WorkerError::Transport {
                match exit_status {
                    Some(status) => Err(classify_worker_exit(status)),
                    None => Err(reconcile_transport_exit(connection, deadline).await),
                }
            } else {
                Err(error)
            }
        }
    }
}

async fn reconcile_transport_exit(
    connection: &mut WorkerConnection,
    invocation_deadline: Instant,
) -> WorkerError {
    let deadline = invocation_deadline.min(Instant::now() + PROCESS_EXIT_RECONCILIATION_TIMEOUT);
    loop {
        match connection.process.try_wait() {
            Ok(Some(status)) => return classify_worker_exit(status),
            Err(_) => return WorkerError::Transport,
            Ok(None) if Instant::now() >= deadline => return WorkerError::Transport,
            Ok(None) => {
                tokio::time::sleep(reconciliation_poll_delay(Instant::now(), deadline)).await
            }
        }
    }
}

fn reconciliation_poll_delay(now: Instant, deadline: Instant) -> Duration {
    PROCESS_POLL_INTERVAL.min(deadline.saturating_duration_since(now))
}

fn classify_worker_exit(status: ExitStatus) -> WorkerError {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        if status.signal() == Some(libc::SIGXCPU) || status.code() == Some(128 + libc::SIGXCPU) {
            return WorkerError::NativeCpuLimit;
        }
    }
    WorkerError::Transport
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

#[cfg(test)]
pub(crate) fn reconciliation_poll_delay_for_test(remaining: Duration) -> Duration {
    let now = Instant::now();
    reconciliation_poll_delay(now, now + remaining)
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

fn map_protocol_error(error: super::protocol::ProtocolError) -> WorkerError {
    match error {
        super::protocol::ProtocolError::BuildMismatch { .. }
        | super::protocol::ProtocolError::VersionMismatch { .. } => WorkerError::BuildMismatch,
        _ => WorkerError::Protocol,
    }
}

fn map_frame_error(error: FrameError) -> WorkerError {
    match error {
        FrameError::Io(_)
        | FrameError::TruncatedHeader { .. }
        | FrameError::TruncatedBody { .. } => WorkerError::Transport,
        FrameError::ZeroLength
        | FrameError::FrameTooLarge { .. }
        | FrameError::InvalidJson
        | FrameError::Serialization => WorkerError::Protocol,
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
