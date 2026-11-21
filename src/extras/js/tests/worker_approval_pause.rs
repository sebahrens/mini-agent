//! Phase 6 timing rule: user-approval wait is excluded from the step budget, while the parent
//! keeps a hard overall bound (step budget plus the approval allowance) and the worker cannot
//! extend its own deadline (mini-agent-sskdu.267).

use std::time::{Duration, Instant};

use crate::extras::js::protocol::{
    EffectError, EffectErrorCode, EffectRequest, EffectResult, GrantId, RunStep, StepOutcome,
};
use crate::extras::js::supervisor::{
    EffectFuture, InvocationEffectHandler, JsWorkerSupervisor, WorkerError,
};
use crate::extras::js::types::PermCancellation;
use crate::sandbox::worker::TestWorkerLauncher;

/// Worker step clock for these tests (the real worker process reads it from its environment).
const WORKER_STEP_MS: u64 = 1_000;
/// Parent invocation deadline; larger than the worker clock so only the pause matters.
const PARENT_STEP: Duration = Duration::from_secs(2);

/// An effect that models the parent waiting `wait` on the user (`prompt`) or on something that is
/// not an approval (`!prompt`).
struct SlowEffect {
    wait: Duration,
    prompt: bool,
    invocation: PermCancellation,
}

impl InvocationEffectHandler for SlowEffect {
    fn handle_effect(
        &mut self,
        _request: EffectRequest,
        cancellation: PermCancellation,
    ) -> EffectFuture<'_> {
        let wait = self.wait;
        let prompt = self.prompt;
        let invocation = self.invocation.clone();
        Box::pin(async move {
            {
                let _guard = prompt.then(|| invocation.begin_permission_prompt());
                tokio::select! {
                    () = tokio::time::sleep(wait) => {}
                    // Cooperative: stop promptly when the parent's deadline cancels the effect.
                    () = cancellation.cancelled() => {
                        return EffectResult::Error(EffectError::new(EffectErrorCode::Denied));
                    }
                }
            }
            EffectResult::ReadFile {
                content: "ok".into(),
            }
        })
    }
}

fn step(code: &str) -> RunStep {
    RunStep::new(code.into()).with_model_grant(GrantId::new(uuid::Uuid::from_u128(1)).unwrap())
}

fn supervisor() -> JsWorkerSupervisor {
    JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
        TestWorkerLauncher::internal_worker_process_with_limits(WORKER_STEP_MS, 10_000),
        PARENT_STEP,
    )
}

fn invocation(allowance: Duration) -> PermCancellation {
    let cancellation = PermCancellation::new();
    cancellation.set_approval_allowance(allowance);
    cancellation
}

/// (a) An approval slower than the step budget, but inside the allowance, does not lose the call.
#[tokio::test]
async fn approval_longer_than_step_budget_within_overall_bound_succeeds() {
    let supervisor = supervisor();
    let cancellation = invocation(Duration::from_secs(3));
    let result = supervisor
        .execute(
            step("read_file('fixture') + ':done'"),
            SlowEffect {
                wait: Duration::from_millis(WORKER_STEP_MS + 600),
                prompt: true,
                invocation: cancellation.clone(),
            },
            cancellation,
        )
        .await
        .expect("a slow approval within the overall bound must not lose the call");
    assert_eq!(result.outcome, StepOutcome::Value("ok:done".into()));
    supervisor.shutdown_for_test().await.unwrap();
}

/// (b) The hard overall bound still ends a step whose approval never arrives.
#[tokio::test]
async fn overall_bound_still_kills_an_unanswered_approval() {
    let supervisor = supervisor();
    let allowance = Duration::from_millis(500);
    let cancellation = invocation(allowance);
    let started = Instant::now();
    let result = supervisor
        .execute(
            step("read_file('fixture')"),
            SlowEffect {
                wait: Duration::from_secs(60),
                prompt: true,
                invocation: cancellation.clone(),
            },
            cancellation,
        )
        .await;
    let elapsed = started.elapsed();
    assert_eq!(result, Err(WorkerError::PermissionPromptTimedOut));
    assert!(
        elapsed >= PARENT_STEP + allowance - Duration::from_millis(50),
        "the step must keep its full budget plus the allowance, ended after {elapsed:?}"
    );
    assert!(
        elapsed < PARENT_STEP + allowance + Duration::from_secs(5),
        "the overall bound must be enforced, ended after {elapsed:?}"
    );
    supervisor.shutdown_for_test().await.unwrap();
}

/// (c) Waiting that is not a parent-side approval earns no credit: the worker's own clock still
/// expires, and a worker cannot lengthen it by stalling an effect.
#[tokio::test]
async fn effect_wait_without_an_approval_does_not_extend_the_worker_deadline() {
    let supervisor = supervisor();
    // The allowance is configured, but no prompt is ever pending, so the pause stays zero.
    let cancellation = invocation(Duration::from_secs(3));
    let result = supervisor
        .execute(
            step("read_file('fixture'); for (let i = 0; i < 1e9; i += 1) {} 'unreachable'"),
            SlowEffect {
                wait: Duration::from_millis(WORKER_STEP_MS + 300),
                prompt: false,
                invocation: cancellation.clone(),
            },
            cancellation,
        )
        .await
        .expect("worker timeout is a closed step outcome");
    assert_eq!(result.outcome, StepOutcome::Timeout);
    supervisor.shutdown_for_test().await.unwrap();
}

/// Without a configured allowance (every non-`js` caller) a pending prompt excludes nothing.
#[tokio::test]
async fn approval_wait_is_not_excluded_without_an_allowance() {
    let supervisor = supervisor();
    let cancellation = PermCancellation::new();
    let result = supervisor
        .execute(
            step("read_file('fixture'); for (let i = 0; i < 1e9; i += 1) {} 'x'"),
            SlowEffect {
                wait: Duration::from_millis(WORKER_STEP_MS + 300),
                prompt: true,
                invocation: cancellation.clone(),
            },
            cancellation,
        )
        .await
        .expect("worker timeout is a closed step outcome");
    assert_eq!(result.outcome, StepOutcome::Timeout);
    supervisor.shutdown_for_test().await.unwrap();
}
