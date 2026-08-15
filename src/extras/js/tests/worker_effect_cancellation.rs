use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::extras::js::protocol::{
    EffectError, EffectErrorCode, EffectRequest, EffectResult, RunStep, StepOutcome,
};
use crate::extras::js::supervisor::{
    EffectFuture, InvocationEffectHandler, JsWorkerSupervisor, WorkerError,
};
use crate::extras::js::types::PermCancellation;
use crate::extras::js::{
    host::await_mutating_effect, tool::PermissionBridgeOwner, types::EffectServiceError,
};
use crate::sandbox::worker::TestWorkerLauncher;
#[cfg(target_os = "linux")]
use crate::{extras::js::host::SpawnEffectService, sandbox::Sandbox};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Terminal {
    Finished,
    Recycled,
}

#[derive(Clone, Default)]
struct CancellationWitness {
    began: Arc<AtomicBool>,
    observed: Arc<AtomicBool>,
    reconciled: Arc<AtomicBool>,
    terminal: Arc<Mutex<Vec<Terminal>>>,
}

impl InvocationEffectHandler for CancellationWitness {
    fn handle_effect(
        &mut self,
        _request: EffectRequest,
        cancellation: PermCancellation,
    ) -> EffectFuture<'_> {
        let began = self.began.clone();
        let observed = self.observed.clone();
        let reconciled = self.reconciled.clone();
        Box::pin(async move {
            began.store(true, Ordering::Release);
            cancellation.cancelled().await;
            observed.store(true, Ordering::Release);
            // Model bounded cleanup which must finish before the broker is dropped: killing and
            // reaping a process tree, draining a proposal response, or persisting reconciliation.
            tokio::time::sleep(Duration::from_millis(30)).await;
            reconciled.store(true, Ordering::Release);
            EffectResult::Error(EffectError {
                code: EffectErrorCode::OutcomeUnknown,
            })
        })
    }

    fn finish_invocation(&mut self) {
        self.terminal.lock().unwrap().push(Terminal::Finished);
    }

    fn recycle_invocation(&mut self) {
        self.terminal.lock().unwrap().push(Terminal::Recycled);
    }
}

async fn wait_until(flag: &AtomicBool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !flag.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancellation phase was not reached");
}

#[tokio::test]
async fn worker_effect_cancellation_drains_reconciliation_then_recycles_and_recovers() {
    let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
        TestWorkerLauncher::scripted_internal_worker(0),
        Duration::from_secs(2),
    ));
    let cancellation = PermCancellation::new();
    let witness = CancellationWitness::default();
    let task_supervisor = supervisor.clone();
    let task_cancellation = cancellation.clone();
    let task_witness = witness.clone();
    let task = tokio::spawn(async move {
        task_supervisor
            .execute(
                RunStep::new("outcome-unknown".into()),
                task_witness,
                task_cancellation,
            )
            .await
    });

    wait_until(&witness.began).await;
    cancellation.cancel();
    assert_eq!(task.await.unwrap(), Err(WorkerError::EffectOutcomeUnknown));
    assert!(witness.observed.load(Ordering::Acquire));
    assert!(
        witness.reconciled.load(Ordering::Acquire),
        "supervisor returned before effect reconciliation completed"
    );
    assert_eq!(*witness.terminal.lock().unwrap(), vec![Terminal::Recycled]);
    assert_eq!(supervisor.generation_for_test().await, None);

    let next = supervisor
        .execute(
            RunStep::new("success".into()),
            CancellationWitness::default(),
            PermCancellation::new(),
        )
        .await
        .expect("next invocation must start with fresh authority and worker state");
    assert_eq!(next.outcome, StepOutcome::Value("success".into()));
    assert_eq!(supervisor.generation_for_test().await, Some(2));
    supervisor.shutdown_for_test().await.unwrap();
}

#[tokio::test]
async fn worker_effect_cancellation_deadline_drains_unknown_outcome_before_returning() {
    // The watchdog must comfortably include a fresh debug worker launch; the
    // behavior under test begins only after the worker requests its effect.
    let supervisor = JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
        TestWorkerLauncher::scripted_internal_worker(0),
        Duration::from_millis(500),
    );
    let witness = CancellationWitness::default();
    let result = supervisor
        .execute(
            RunStep::new("outcome-unknown".into()),
            witness.clone(),
            PermCancellation::new(),
        )
        .await;
    assert_eq!(result, Err(WorkerError::EffectOutcomeUnknown));
    assert!(witness.observed.load(Ordering::Acquire));
    assert!(witness.reconciled.load(Ordering::Acquire));
    assert_eq!(*witness.terminal.lock().unwrap(), vec![Terminal::Recycled]);

    let next = supervisor
        .execute(
            RunStep::new("success".into()),
            CancellationWitness::default(),
            PermCancellation::new(),
        )
        .await
        .expect("deadline cleanup leaked worker state");
    assert_eq!(next.outcome, StepOutcome::Value("success".into()));
    supervisor.shutdown_for_test().await.unwrap();
}

#[derive(Clone, Default)]
struct CompletedWriteAfterCancellation {
    began: Arc<AtomicBool>,
}

impl InvocationEffectHandler for CompletedWriteAfterCancellation {
    fn handle_effect(
        &mut self,
        _request: EffectRequest,
        cancellation: PermCancellation,
    ) -> EffectFuture<'_> {
        let began = self.began.clone();
        Box::pin(async move {
            began.store(true, Ordering::Release);
            cancellation.cancelled().await;
            EffectResult::WriteFile
        })
    }
}

#[tokio::test]
async fn worker_effect_cancellation_never_reports_completed_mutation_as_exact_cancel() {
    let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
        TestWorkerLauncher::scripted_internal_worker(0),
        Duration::from_secs(2),
    ));
    let cancellation = PermCancellation::new();
    let witness = CompletedWriteAfterCancellation::default();
    let task_supervisor = supervisor.clone();
    let task_cancellation = cancellation.clone();
    let task_witness = witness.clone();
    let task = tokio::spawn(async move {
        task_supervisor
            .execute(
                RunStep::new("outcome-unknown".into()),
                task_witness,
                task_cancellation,
            )
            .await
    });

    wait_until(&witness.began).await;
    cancellation.cancel();
    assert_eq!(task.await.unwrap(), Err(WorkerError::EffectOutcomeUnknown));
    assert_eq!(supervisor.generation_for_test().await, None);

    let next = supervisor
        .execute(
            RunStep::new("success".into()),
            CancellationWitness::default(),
            PermCancellation::new(),
        )
        .await
        .expect("completed cancellation race leaked worker state");
    assert_eq!(next.outcome, StepOutcome::Value("success".into()));
    supervisor.shutdown_for_test().await.unwrap();
}

#[tokio::test]
async fn worker_effect_cancellation_bounds_queued_invocation_without_interrupting_owner() {
    let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
        TestWorkerLauncher::scripted_internal_worker(0),
        Duration::from_secs(2),
    ));
    let owner_cancellation = PermCancellation::new();
    let owner_supervisor = supervisor.clone();
    let task_owner_cancellation = owner_cancellation.clone();
    let owner = tokio::spawn(async move {
        owner_supervisor
            .execute(
                RunStep::new("deadline".into()),
                CancellationWitness::default(),
                task_owner_cancellation,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while supervisor.active_generation_for_test().await != Some(1) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owning invocation did not acquire the worker");

    let queued_cancellation = PermCancellation::new();
    let queued_supervisor = supervisor.clone();
    let task_queued_cancellation = queued_cancellation.clone();
    let queued = tokio::spawn(async move {
        queued_supervisor
            .execute(
                RunStep::new("success".into()),
                CancellationWitness::default(),
                task_queued_cancellation,
            )
            .await
    });
    tokio::task::yield_now().await;
    queued_cancellation.cancel();
    assert_eq!(queued.await.unwrap(), Err(WorkerError::Cancelled));
    assert_eq!(
        supervisor.active_generation_for_test().await,
        Some(1),
        "queued cancellation killed the current invocation"
    );

    owner_cancellation.cancel();
    assert_eq!(owner.await.unwrap(), Err(WorkerError::Cancelled));
    let next = supervisor
        .execute(
            RunStep::new("success".into()),
            CancellationWitness::default(),
            PermCancellation::new(),
        )
        .await
        .expect("next invocation must succeed after owner cleanup");
    assert_eq!(next.outcome, StepOutcome::Value("success".into()));
    supervisor.shutdown_for_test().await.unwrap();
}

struct PendingMutation(Arc<AtomicBool>);

impl Drop for PendingMutation {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[tokio::test]
async fn worker_effect_cancellation_distinguishes_pre_write_from_post_open_and_mid_write() {
    let owner = PermissionBridgeOwner::new(None, None, Duration::from_secs(1));

    let exact = PermCancellation::new();
    exact.cancel();
    let exact_bridge = owner.bridge().for_host_call(exact);
    assert_eq!(
        await_mutating_effect(&exact_bridge, Duration::from_secs(1), async {
            panic!("pre-cancelled mutation was polled");
            #[allow(unreachable_code)]
            Ok::<_, EffectServiceError>(())
        })
        .await,
        Err(EffectServiceError::Cancelled)
    );

    for phase in ["post_open", "mid_write"] {
        let cancellation = PermCancellation::new();
        let bridge = owner.bridge().for_host_call(cancellation.clone());
        let live = Arc::new(AtomicBool::new(false));
        let task_live = live.clone();
        let (began_tx, began_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            await_mutating_effect(&bridge, Duration::from_secs(1), async move {
                task_live.store(true, Ordering::Release);
                let _drop_witness = PendingMutation(task_live);
                let _ = began_tx.send(());
                std::future::pending::<Result<(), EffectServiceError>>().await
            })
            .await
        });
        began_rx
            .await
            .expect("mutation did not cross its open boundary");
        cancellation.cancel();
        assert_eq!(
            task.await.unwrap(),
            Err(EffectServiceError::OutcomeUnknown),
            "phase {phase}"
        );
        assert!(!live.load(Ordering::Acquire), "phase {phase} leaked a task");
    }

    let deadline_bridge = owner.bridge().for_host_call(PermCancellation::new());
    assert_eq!(
        await_mutating_effect(&deadline_bridge, Duration::from_millis(10), async {
            std::future::pending::<Result<(), EffectServiceError>>().await
        })
        .await,
        Err(EffectServiceError::OutcomeUnknown)
    );
    assert_eq!(
        await_mutating_effect(&deadline_bridge, Duration::from_secs(1), async { Ok(42) }).await,
        Ok(42),
        "next mutation did not recover"
    );
    owner.shutdown();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn worker_effect_cancellation_kills_spawn_tree_and_marks_dispatched_outcome_unknown() {
    let heartbeat_path = std::env::current_dir().unwrap().join(format!(
        ".mini-agent-js-cancel-child-{}.heartbeat",
        uuid::Uuid::new_v4()
    ));
    // The setsid child starts a background grandchild and exits. That grandchild is reparented and
    // has left the command process group; only the bwrap PID namespace still owns it.
    let script = "heartbeat=$1; setsid sh -c 'heartbeat=$1; while :; do printf x >> \"$heartbeat\"; sleep 0.02; done &' daemon \"$heartbeat\" & launcher=$!; wait \"$launcher\"; sleep 30";
    let permission_owner = PermissionBridgeOwner::new(None, None, Duration::from_secs(2));
    let sandbox = Sandbox::new(true, "bwrap");
    if !sandbox.owns_complete_process_tree() {
        permission_owner.shutdown();
        return;
    }
    let service = Arc::new(SpawnEffectService::new(
        sandbox,
        permission_owner.bridge(),
        Duration::from_secs(30),
    ));
    let cancellation = PermCancellation::new();
    let task_service = service.clone();
    let task_cancellation = cancellation.clone();
    let task_heartbeat_path = heartbeat_path.clone();
    let task = tokio::spawn(async move {
        task_service
            .execute(
                "sh",
                &[
                    "-c".to_string(),
                    script.to_string(),
                    "mini-agent-cancel-probe".to_string(),
                    task_heartbeat_path.to_string_lossy().into_owned(),
                ],
                task_cancellation,
            )
            .await
    });

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if std::fs::metadata(&heartbeat_path).is_ok_and(|metadata| metadata.len() > 1) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("spawned descendant did not write its heartbeat");

    cancellation.cancel();
    assert!(matches!(
        task.await.unwrap(),
        Err(crate::extras::js::types::EffectServiceError::OutcomeUnknown)
    ));
    let stopped_len = std::fs::metadata(&heartbeat_path).unwrap().len();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        std::fs::metadata(&heartbeat_path).unwrap().len(),
        stopped_len,
        "cancelled spawn left a descendant writing after service completion"
    );

    let next = service
        .execute(
            "printf",
            &["%s".to_string(), "recovered".to_string()],
            PermCancellation::new(),
        )
        .await
        .expect("next spawn must succeed without a leaked cancellation task");
    assert_eq!(next.stdout, "recovered");
    let _ = std::fs::remove_file(heartbeat_path);
    permission_owner.shutdown();
}
