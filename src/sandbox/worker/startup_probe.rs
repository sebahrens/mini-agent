//! Own the unchanged full worker preflight while foreground startup proceeds.

use std::sync::Arc;

use crate::agent::runner::{AgentRunCleanupGuard, AgentWorkScope, spawn_blocking_scoped};

use super::WorkerContainmentStatus;

/// Runtime ownership only: availability remains the platform's completed full
/// preflight result. Dropping this owner never aborts a running native probe.
pub(crate) struct StartupWorkerProbe {
    task: tokio::task::JoinHandle<Option<WorkerContainmentStatus>>,
    scope: Arc<AgentWorkScope>,
    cleanup: AgentRunCleanupGuard,
}

impl StartupWorkerProbe {
    pub(crate) async fn start() -> Self {
        Self::start_in(AgentWorkScope::new(), super::containment_status).await
    }

    #[cfg(test)]
    pub(crate) async fn start_for_test(
        scope: Arc<AgentWorkScope>,
        probe: impl FnOnce() -> WorkerContainmentStatus + Send + 'static,
    ) -> Self {
        Self::start_in(scope, probe).await
    }

    async fn start_in(
        scope: Arc<AgentWorkScope>,
        probe: impl FnOnce() -> WorkerContainmentStatus + Send + 'static,
    ) -> Self {
        let cleanup = AgentRunCleanupGuard::auxiliary(scope.clone());
        let operation_scope = scope.clone();
        // Construct the blocking child inside the installed work scope, not as
        // an eagerly evaluated argument to scope.run(). Once it starts, the
        // original bounded native preflight runs through cleanup unchanged.
        let (task,) = scope
            .run(async move {
                (spawn_blocking_scoped(move || {
                    if operation_scope.is_cancelled() {
                        return None;
                    }
                    tracing::debug!("startup worker preflight started");
                    let attestation = probe();
                    tracing::debug!(
                        available =
                            matches!(&attestation, WorkerContainmentStatus::Available { .. }),
                        "startup worker preflight completed"
                    );
                    Some(attestation)
                }),)
            })
            .await;
        Self {
            task,
            scope,
            cleanup,
        }
    }

    /// Called once its owner (TUI, ACP or headless run) has finished, when
    /// nothing can consume the result any more. A probe still queued on the
    /// blocking pool observes the cancellation and skips the native preflight
    /// (an early quit no longer waits for it); a running preflight is joined
    /// unchanged through its cleanup.
    pub(crate) async fn retire_after_owner_exit(self) -> anyhow::Result<()> {
        self.scope.cancellation_handle().cancel();
        self.retire().await
    }

    /// Join the preflight to completion through its cleanup.
    pub(crate) async fn retire(self) -> anyhow::Result<()> {
        let result = self.task.await;
        self.cleanup.settle().await;
        result.map_err(|error| anyhow::anyhow!("worker startup preflight failed: {error}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Condvar, Mutex, OnceLock};
    use std::time::Duration;

    struct Release(Arc<(Mutex<bool>, Condvar)>);
    impl Release {
        fn release(&self) {
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
        }
    }
    impl Drop for Release {
        fn drop(&mut self) {
            self.release();
        }
    }

    fn available() -> WorkerContainmentStatus {
        WorkerContainmentStatus::Available {
            backend: super::super::WorkerBackend::for_current_platform(),
            assurance: super::super::WorkerContainmentAssurance::Enforced,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn startup_worker_probe_keeps_real_input_polling_and_both_race_orders() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let scope = AgentWorkScope::new();
        let result = Arc::new(OnceLock::new());
        let release = Release(Arc::new((Mutex::new(false), Condvar::new())));
        let held = release.0.clone();
        let completed = result.clone();
        let (started, entered) = tokio::sync::oneshot::channel();
        let probe = StartupWorkerProbe::start_in(scope.clone(), move || {
            let _ = started.send(());
            let mut ready = held.0.lock().unwrap();
            while !*ready {
                ready = held.1.wait(ready).unwrap();
            }
            completed.set(available()).unwrap();
            available()
        })
        .await;
        tokio::time::timeout(Duration::from_secs(2), entered)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(scope.active_children(), 1);
        assert!(
            result.get().is_none(),
            "model-first cannot advertise an unfinished probe"
        );
        let input = tokio::spawn(async {
            let mut input = crate::ui::input::InputEditor::new();
            input.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
            input.handle_paste("two\nlines".into());
            input.buffer
        });
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), input)
                .await
                .unwrap()
                .unwrap(),
            "xtwo\nlines"
        );
        assert!(
            result.get().is_none(),
            "input must progress while full probe remains held"
        );
        release.release();
        probe.retire().await.unwrap();
        assert_eq!(scope.active_children(), 0);
        assert!(
            matches!(
                result.get(),
                Some(WorkerContainmentStatus::Available { .. })
            ),
            "probe-first construction may use only the completed attestation"
        );
    }

    #[tokio::test]
    async fn startup_worker_probe_dropped_retirement_keeps_physical_work_owned() {
        let scope = AgentWorkScope::new();
        let completed = Arc::new(OnceLock::new());
        let result = completed.clone();
        let release = Release(Arc::new((Mutex::new(false), Condvar::new())));
        let held = release.0.clone();
        let (started, entered) = tokio::sync::oneshot::channel();
        let probe = StartupWorkerProbe::start_in(scope.clone(), move || {
            let _ = started.send(());
            let mut ready = held.0.lock().unwrap();
            while !*ready {
                ready = held.1.wait(ready).unwrap();
            }
            result.set(available()).unwrap();
            available()
        })
        .await;
        entered.await.unwrap();
        let retirement = tokio::spawn(probe.retire());
        tokio::task::yield_now().await;
        retirement.abort();
        let _ = retirement.await;
        assert!(scope.is_cancelled());
        assert_eq!(
            scope.active_children(),
            1,
            "dropped waiter cannot detach the blocking probe"
        );
        release.release();
        tokio::time::timeout(Duration::from_secs(2), scope.wait_idle())
            .await
            .unwrap();
        assert_eq!(scope.active_children(), 0);
        assert!(matches!(
            completed.get(),
            Some(WorkerContainmentStatus::Available { .. })
        ));
    }

    #[tokio::test]
    async fn startup_worker_probe_cancel_before_start_does_not_poison_retry() {
        let (scope, entered, release) = AgentWorkScope::new_with_blocking_test_gate();
        let result = Arc::new(OnceLock::new());
        let completed = result.clone();
        let probe = StartupWorkerProbe::start_in(scope.clone(), move || {
            completed.set(available()).unwrap();
            available()
        })
        .await;
        tokio::task::spawn_blocking(move || entered.recv_timeout(Duration::from_secs(2)).unwrap())
            .await
            .unwrap();
        drop(probe);
        assert!(scope.is_cancelled());
        release.release();
        tokio::time::timeout(Duration::from_secs(2), scope.wait_idle())
            .await
            .unwrap();
        assert!(
            result.get().is_none(),
            "cancellation must not initialize an unavailable cache"
        );
        let completed = result.clone();
        StartupWorkerProbe::start_in(AgentWorkScope::new(), move || {
            completed.set(available()).unwrap();
            available()
        })
        .await
        .retire()
        .await
        .unwrap();
        assert!(matches!(
            result.get(),
            Some(WorkerContainmentStatus::Available { .. })
        ));
    }

    #[tokio::test]
    async fn startup_worker_probe_owner_exit_skips_a_queued_probe_and_joins_a_running_one() {
        // Queued: the gate holds the blocking child before the probe body.
        let (scope, entered, release) = AgentWorkScope::new_with_blocking_test_gate();
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = ran.clone();
        let probe = StartupWorkerProbe::start_in(scope.clone(), move || {
            observed.store(true, std::sync::atomic::Ordering::SeqCst);
            available()
        })
        .await;
        tokio::task::spawn_blocking(move || entered.recv_timeout(Duration::from_secs(2)).unwrap())
            .await
            .unwrap();
        let retirement = tokio::spawn(probe.retire_after_owner_exit());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !scope.is_cancelled() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        release.release();
        retirement.await.unwrap().unwrap();
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(scope.active_children(), 0);

        // Running: the native preflight completes and is joined unchanged.
        let completed = Arc::new(OnceLock::new());
        let result = completed.clone();
        let (started, entered) = tokio::sync::oneshot::channel();
        let probe = StartupWorkerProbe::start_in(AgentWorkScope::new(), move || {
            let _ = started.send(());
            std::thread::sleep(Duration::from_millis(20));
            result.set(available()).unwrap();
            available()
        })
        .await;
        entered.await.unwrap();
        probe.retire_after_owner_exit().await.unwrap();
        assert!(matches!(
            completed.get(),
            Some(WorkerContainmentStatus::Available { .. })
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn startup_worker_probe_dropped_retirement_waiter_keeps_native_proof_owned() {
        let scope = AgentWorkScope::new();
        let release = Release(Arc::new((Mutex::new(false), Condvar::new())));
        let held = release.0.clone();
        let completed = Arc::new(OnceLock::new());
        let result = completed.clone();
        let (started, entered) = tokio::sync::oneshot::channel();
        let probe = StartupWorkerProbe::start_for_test(scope.clone(), move || {
            let _ = started.send(());
            let mut ready = held.0.lock().unwrap();
            while !*ready {
                ready = held.1.wait(ready).unwrap();
            }
            result.set(available()).unwrap();
            available()
        })
        .await;
        entered.await.unwrap();
        let (polling, polled) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(async move {
            let _ = polling.send(());
            probe.retire().await
        });
        polled.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert_eq!(scope.active_children(), 1);
        assert!(completed.get().is_none());
        release.release();
        tokio::time::timeout(Duration::from_secs(2), scope.wait_idle())
            .await
            .unwrap();
        assert_eq!(scope.active_children(), 0);
        assert!(matches!(
            completed.get(),
            Some(WorkerContainmentStatus::Available { .. })
        ));
    }
}
