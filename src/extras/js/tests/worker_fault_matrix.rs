use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::extras::js::protocol::{
    EffectError, EffectErrorCode, EffectRequest, EffectResult, RunStep, StepOutcome,
};
use crate::extras::js::supervisor::{
    EffectFuture, InvocationEffectHandler, JsWorkerSupervisor, WorkerError, WorkerReusePolicy,
};
use crate::extras::js::types::PermCancellation;
use crate::sandbox::worker::{
    TestSupervisorStartup, TestWorkerLauncher, WorkerContainmentStatus, WorkerLaunchError,
    WorkerLauncher, WorkerProcess,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthorityTerminal {
    Finished,
    Recycled,
}

#[derive(Clone, Default)]
struct MatrixEffects {
    terminal: Arc<Mutex<Vec<AuthorityTerminal>>>,
    outcome_unknown: bool,
}

impl MatrixEffects {
    fn outcome_unknown() -> Self {
        Self {
            outcome_unknown: true,
            ..Self::default()
        }
    }

    fn terminals(&self) -> Vec<AuthorityTerminal> {
        self.terminal.lock().unwrap().clone()
    }
}

impl InvocationEffectHandler for MatrixEffects {
    fn handle_effect(
        &mut self,
        _request: EffectRequest,
        _cancellation: PermCancellation,
    ) -> EffectFuture<'_> {
        let outcome_unknown = self.outcome_unknown;
        Box::pin(async move {
            if outcome_unknown {
                EffectResult::Error(EffectError {
                    code: EffectErrorCode::OutcomeUnknown,
                })
            } else {
                EffectResult::ReadFile {
                    content: "fixture".into(),
                }
            }
        })
    }

    fn finish_invocation(&mut self) {
        self.terminal
            .lock()
            .unwrap()
            .push(AuthorityTerminal::Finished);
    }

    fn recycle_invocation(&mut self) {
        self.terminal
            .lock()
            .unwrap()
            .push(AuthorityTerminal::Recycled);
    }
}

#[derive(Clone)]
struct CountingLauncher {
    startup: TestSupervisorStartup,
    launches: Arc<AtomicUsize>,
    live: Arc<AtomicUsize>,
}

impl CountingLauncher {
    fn scripted(startup: TestSupervisorStartup) -> Self {
        Self {
            startup,
            launches: Arc::new(AtomicUsize::new(0)),
            live: Arc::new(AtomicUsize::new(0)),
        }
    }

    async fn wait_for_live(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.live.load(Ordering::Acquire) != expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker process count did not converge");
    }
}

impl WorkerLauncher for CountingLauncher {
    fn containment_status(&self) -> WorkerContainmentStatus {
        TestWorkerLauncher::scripted_internal_worker(0).containment_status()
    }

    fn launch(&self) -> Result<WorkerProcess, WorkerLaunchError> {
        let startup = if self.launches.fetch_add(1, Ordering::AcqRel) == 0 {
            self.startup
        } else {
            TestSupervisorStartup::Healthy
        };
        let mut process =
            TestWorkerLauncher::scripted_internal_worker_with_startup(0, startup).launch()?;
        process.observe_reap_for_test(self.live.clone());
        Ok(process)
    }
}

async fn call(
    supervisor: &JsWorkerSupervisor,
    code: &str,
    effects: MatrixEffects,
) -> Result<crate::extras::js::protocol::StepResult, WorkerError> {
    supervisor
        .execute(RunStep::new(code.into()), effects, PermCancellation::new())
        .await
}

#[tokio::test]
async fn worker_fault_matrix_reuses_only_success_and_allowlisted_javascript_errors() {
    let launcher = CountingLauncher::scripted(TestSupervisorStartup::Healthy);
    let supervisor = JsWorkerSupervisor::with_launcher_and_policy_for_test(
        launcher.clone(),
        Duration::from_secs(2),
        WorkerReusePolicy::new(Duration::from_secs(60), 32),
    );

    for (code, expected, expected_terminal) in [
        (
            "success",
            StepOutcome::Value("success".into()),
            AuthorityTerminal::Finished,
        ),
        (
            "js-error",
            StepOutcome::Error(crate::extras::js::protocol::JsErrorCode::Exception),
            AuthorityTerminal::Finished,
        ),
    ] {
        let effects = MatrixEffects::default();
        let witness = effects.clone();
        assert_eq!(
            call(&supervisor, code, effects).await.unwrap().outcome,
            expected
        );
        assert_eq!(witness.terminals(), vec![expected_terminal]);
        assert_eq!(launcher.launches.load(Ordering::Acquire), 1);
    }

    supervisor.shutdown_for_test().await.unwrap();
    launcher.wait_for_live(0).await;
}

#[tokio::test]
async fn worker_fault_matrix_allowlists_every_javascript_error_code() {
    use crate::extras::js::protocol::JsErrorCode;

    for (script, code, reusable) in [
        ("js-error-syntax", JsErrorCode::Syntax, true),
        ("js-error-exception", JsErrorCode::Exception, true),
        ("js-error-stack", JsErrorCode::StackLimit, false),
        ("js-error-jobs", JsErrorCode::JobLimit, false),
        ("js-error-result", JsErrorCode::InvalidResult, true),
        ("js-error-internal", JsErrorCode::Internal, false),
    ] {
        let launcher = CountingLauncher::scripted(TestSupervisorStartup::Healthy);
        let supervisor = JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
            launcher.clone(),
            Duration::from_secs(2),
        );
        let effects = MatrixEffects::default();
        let witness = effects.clone();
        assert_eq!(
            call(&supervisor, script, effects).await.unwrap().outcome,
            StepOutcome::Error(code),
            "error row {script}"
        );
        assert_eq!(
            witness.terminals(),
            vec![if reusable {
                AuthorityTerminal::Finished
            } else {
                AuthorityTerminal::Recycled
            }],
            "error row {script}"
        );
        assert_eq!(
            call(&supervisor, "success", MatrixEffects::default())
                .await
                .unwrap()
                .outcome,
            StepOutcome::Value("success".into())
        );
        assert_eq!(
            launcher.launches.load(Ordering::Acquire),
            if reusable { 1 } else { 2 },
            "error row {script}"
        );
        supervisor.shutdown_for_test().await.unwrap();
        launcher.wait_for_live(0).await;
    }
}

#[tokio::test]
async fn worker_fault_matrix_recycles_resource_protocol_process_and_cancellation_faults() {
    let rows = [
        ("timeout-step", Ok(StepOutcome::Timeout)),
        ("oom-step", Ok(StepOutcome::OutOfMemory)),
        ("malformed-protocol", Err(WorkerError::Protocol)),
        ("protocol-fault", Err(WorkerError::Protocol)),
        ("crash", Err(WorkerError::Transport)),
        ("panic", Err(WorkerError::Transport)),
        ("os-kill", Err(WorkerError::Transport)),
        ("abnormal-exit", Err(WorkerError::Transport)),
    ];

    for (code, expected) in rows {
        let launcher = CountingLauncher::scripted(TestSupervisorStartup::Healthy);
        let supervisor = JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
            launcher.clone(),
            Duration::from_secs(2),
        );
        let effects = MatrixEffects::default();
        let witness = effects.clone();
        let actual = call(&supervisor, code, effects)
            .await
            .map(|result| result.outcome);
        assert_eq!(actual, expected, "fault row {code}");
        assert_eq!(witness.terminals(), vec![AuthorityTerminal::Recycled]);

        let next = call(&supervisor, "success", MatrixEffects::default())
            .await
            .expect("next request must succeed");
        assert_eq!(next.outcome, StepOutcome::Value("success".into()));
        assert_eq!(
            launcher.launches.load(Ordering::Acquire),
            2,
            "fault row {code}"
        );
        supervisor.shutdown_for_test().await.unwrap();
        launcher.wait_for_live(0).await;
    }

    let launcher = CountingLauncher::scripted(TestSupervisorStartup::Healthy);
    let supervisor = JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
        launcher.clone(),
        Duration::from_secs(2),
    );
    let effects = MatrixEffects::outcome_unknown();
    let witness = effects.clone();
    assert_eq!(
        call(&supervisor, "outcome-unknown", effects).await,
        Err(WorkerError::EffectOutcomeUnknown)
    );
    assert_eq!(witness.terminals(), vec![AuthorityTerminal::Recycled]);
    assert_eq!(
        call(&supervisor, "success", MatrixEffects::default())
            .await
            .unwrap()
            .outcome,
        StepOutcome::Value("success".into())
    );
    assert_eq!(launcher.launches.load(Ordering::Acquire), 2);
    supervisor.shutdown_for_test().await.unwrap();
    launcher.wait_for_live(0).await;

    let launcher = CountingLauncher::scripted(TestSupervisorStartup::Healthy);
    let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
        launcher.clone(),
        Duration::from_secs(2),
    ));
    let cancellation = PermCancellation::new();
    let task_supervisor = supervisor.clone();
    let task_cancellation = cancellation.clone();
    let effects = MatrixEffects::default();
    let witness = effects.clone();
    let task = tokio::spawn(async move {
        task_supervisor
            .execute(RunStep::new("deadline".into()), effects, task_cancellation)
            .await
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    cancellation.cancel();
    assert_eq!(task.await.unwrap(), Err(WorkerError::Cancelled));
    assert_eq!(witness.terminals(), vec![AuthorityTerminal::Recycled]);
    assert_eq!(
        call(&supervisor, "success", MatrixEffects::default())
            .await
            .unwrap()
            .outcome,
        StepOutcome::Value("success".into())
    );
    assert_eq!(launcher.launches.load(Ordering::Acquire), 2);
    supervisor.shutdown_for_test().await.unwrap();
    launcher.wait_for_live(0).await;
}

#[tokio::test]
async fn worker_fault_matrix_rejects_build_mismatch_and_clean_shutdown_restarts() {
    let launcher = CountingLauncher::scripted(TestSupervisorStartup::BuildMismatch);
    let supervisor = JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
        launcher.clone(),
        Duration::from_secs(2),
    );
    assert_eq!(
        call(&supervisor, "success", MatrixEffects::default()).await,
        Err(WorkerError::BuildMismatch)
    );
    assert_eq!(
        call(&supervisor, "success", MatrixEffects::default())
            .await
            .unwrap()
            .outcome,
        StepOutcome::Value("success".into())
    );
    assert_eq!(launcher.launches.load(Ordering::Acquire), 2);
    supervisor.shutdown_for_test().await.unwrap();
    launcher.wait_for_live(0).await;

    let launcher = CountingLauncher::scripted(TestSupervisorStartup::Healthy);
    let supervisor = JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
        launcher.clone(),
        Duration::from_secs(2),
    );
    call(&supervisor, "success", MatrixEffects::default())
        .await
        .unwrap();
    supervisor.shutdown_for_test().await.unwrap();
    launcher.wait_for_live(0).await;
    call(&supervisor, "success", MatrixEffects::default())
        .await
        .unwrap();
    assert_eq!(launcher.launches.load(Ordering::Acquire), 2);
    supervisor.shutdown_for_test().await.unwrap();
    launcher.wait_for_live(0).await;
}

#[tokio::test]
async fn worker_fault_matrix_bounds_process_call_count_and_age() {
    let launcher = CountingLauncher::scripted(TestSupervisorStartup::Healthy);
    let supervisor = JsWorkerSupervisor::with_launcher_and_policy_for_test(
        launcher.clone(),
        Duration::from_secs(2),
        WorkerReusePolicy::new(Duration::from_secs(60), 2),
    );
    call(&supervisor, "success", MatrixEffects::default())
        .await
        .unwrap();
    call(&supervisor, "success", MatrixEffects::default())
        .await
        .unwrap();
    assert_eq!(launcher.launches.load(Ordering::Acquire), 1);
    call(&supervisor, "success", MatrixEffects::default())
        .await
        .unwrap();
    assert_eq!(launcher.launches.load(Ordering::Acquire), 2);
    supervisor.shutdown_for_test().await.unwrap();
    launcher.wait_for_live(0).await;

    let launcher = CountingLauncher::scripted(TestSupervisorStartup::Healthy);
    let supervisor = JsWorkerSupervisor::with_launcher_and_policy_for_test(
        launcher.clone(),
        Duration::from_secs(2),
        WorkerReusePolicy::new(Duration::from_millis(200), 32),
    );
    call(&supervisor, "success", MatrixEffects::default())
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(3),
        supervisor.wait_for_idle_retirement_for_test(),
    )
    .await
    .expect("idle retirement barrier did not open");
    launcher.wait_for_live(0).await;
    assert_eq!(supervisor.generation_for_test().await, None);
    call(&supervisor, "success", MatrixEffects::default())
        .await
        .unwrap();
    assert_eq!(launcher.launches.load(Ordering::Acquire), 2);
    supervisor.shutdown_for_test().await.unwrap();
    launcher.wait_for_live(0).await;
}
