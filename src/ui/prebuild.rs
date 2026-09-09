//! Owns background agent construction through publication or shutdown.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::agent::runner::AgentWorkScope;

use super::PrebuildPayload;

pub(crate) struct AgentPrebuild {
    task: tokio::task::JoinHandle<()>,
    scope: Arc<AgentWorkScope>,
    receiver: Option<mpsc::Receiver<PrebuildPayload>>,
}

impl AgentPrebuild {
    pub(crate) fn start(
        scope: Arc<AgentWorkScope>,
        build: impl Future<Output = PrebuildPayload> + Send + 'static,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(1);
        let task_scope = scope.clone();
        let task = tokio::spawn(async move {
            task_scope
                .run(async {
                    let payload = build.await;
                    if task_scope.is_cancelled() {
                        discard(payload).await;
                    } else if let Err(error) = sender.send(payload).await {
                        // Memory refresh or shutdown can reject a late result.
                        // Dropping an MCP manager alone does not await its servers.
                        discard(error.0).await;
                    }
                })
                .await;
        });
        Self {
            task,
            scope,
            receiver: Some(receiver),
        }
    }

    pub(crate) async fn recv(&mut self) -> Option<PrebuildPayload> {
        let payload = self.receiver.as_mut()?.recv().await;
        self.receiver = None;
        payload
    }

    pub(crate) fn try_recv(&mut self) -> Option<PrebuildPayload> {
        let payload = self.receiver.as_mut()?.try_recv().ok()?;
        self.receiver = None;
        Some(payload)
    }

    #[cfg(all(test, feature = "mcp"))]
    pub(crate) fn has_queued_result(&self) -> bool {
        self.receiver
            .as_ref()
            .is_some_and(|receiver| !receiver.is_empty())
    }

    pub(crate) async fn retire(mut self, timeout: Duration) -> anyhow::Result<()> {
        self.scope.cancellation_handle().cancel();
        if let Some(receiver) = self.receiver.as_mut() {
            receiver.close();
        }
        let result = tokio::time::timeout(timeout, async {
            if let Some(receiver) = self.receiver.as_mut() {
                while let Ok(payload) = receiver.try_recv() {
                    discard(payload).await;
                }
            }
            // MCP initialization observes scope cancellation and joins its
            // process cleanup. Aborting it immediately would skip that join.
            let joined = (&mut self.task).await;
            self.scope.wait_idle().await;
            joined.map_err(|error| anyhow::anyhow!("agent prebuild failed: {error}"))
        })
        .await;
        match result {
            Ok(result) => result,
            Err(_) => {
                self.task.abort();
                let _ = (&mut self.task).await;
                anyhow::bail!("timed out retiring the agent prebuild workspace")
            }
        }
    }
}

impl Drop for AgentPrebuild {
    fn drop(&mut self) {
        self.scope.cancellation_handle().cancel();
    }
}

async fn discard(payload: PrebuildPayload) {
    #[cfg(feature = "mcp")]
    {
        let (agent, manager) = payload;
        drop(agent);
        if let Some(manager) = manager {
            manager.shutdown().await;
        }
    }
    #[cfg(not(feature = "mcp"))]
    drop(payload);
}

#[cfg(test)]
pub(crate) fn test_agent() -> crate::provider::AnyAgent {
    use rig::client::CompletionClient;

    let client = rig::providers::openrouter::Client::new("unused-test-key").unwrap();
    crate::provider::AnyAgent::without_skills(crate::provider::AnyAgentInner::OpenRouter(
        client.agent("test-model").build(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn prebuild_retirement_waits_for_cooperative_cleanup_and_owned_work() {
        let (scope, started, release) = AgentWorkScope::new_with_blocking_test_gate();
        let (cleaned_tx, mut cleaned_rx) = tokio::sync::oneshot::channel();
        let prebuild = AgentPrebuild::start(scope.clone(), async move {
            let _child = crate::agent::runner::spawn_blocking_scoped(|| ());
            crate::agent::runner::current_work_scope_cancelled().await;
            let _ = cleaned_tx.send(());
            let agent = test_agent();
            #[cfg(feature = "mcp")]
            {
                (agent, None)
            }
            #[cfg(not(feature = "mcp"))]
            {
                agent
            }
        });
        tokio::task::spawn_blocking(move || started.recv_timeout(Duration::from_secs(2)).unwrap())
            .await
            .unwrap();
        let mut retirement = tokio::spawn(prebuild.retire(Duration::from_secs(2)));
        tokio::time::timeout(Duration::from_secs(1), &mut cleaned_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(scope.is_cancelled());
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut retirement)
                .await
                .is_err()
        );
        assert_eq!(scope.active_children(), 1);
        release.release();
        retirement.await.unwrap().unwrap();
        assert_eq!(scope.active_children(), 0);
    }

    #[tokio::test]
    async fn prebuild_retirement_reports_timeout_and_aborts_uncooperative_build() {
        let scope = AgentWorkScope::new();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel::<()>();
        let prebuild = AgentPrebuild::start(scope.clone(), async move {
            let _guard = dropped_tx;
            entered_tx.send(()).unwrap();
            std::future::pending().await
        });
        entered_rx.await.unwrap();
        let error = prebuild
            .retire(Duration::from_millis(20))
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "timed out retiring the agent prebuild workspace"
        );
        assert!(scope.is_cancelled());
        assert!(
            dropped_rx.await.is_err(),
            "timed-out build must have been dropped"
        );
    }
}
