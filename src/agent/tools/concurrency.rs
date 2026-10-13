//! Concurrency boundary for model-issued tool batches.
//!
//! Rig can execute calls from one assistant message concurrently. Every tool
//! in an agent shares this reader/writer lane: explicitly classified
//! read-only tools take a shared lease, while mutating and unknown tools take
//! an exclusive lease. The conservative default keeps extension/MCP tools from
//! acquiring parallel mutation authority accidentally.

use std::sync::Arc;

use rig::tool::{ToolCallExtensions, ToolDyn, ToolError, ToolExecutionResult};
use rig::wasm_compat::WasmBoxedFuture;
use tokio::sync::RwLock;

pub(crate) const DEFAULT_TOOL_CONCURRENCY: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolAccess {
    Read,
    Write,
}

fn access_for(name: &str) -> ToolAccess {
    match name {
        "read" | "grep" | "find_files" | "list_dir" | "todo_read" | "memory_read"
        | "memory_search" | "advisor" | "skills_search" => ToolAccess::Read,
        _ => ToolAccess::Write,
    }
}

struct ConcurrencyBoundTool {
    inner: Box<dyn ToolDyn>,
    lane: Arc<RwLock<()>>,
    access: ToolAccess,
}

impl ConcurrencyBoundTool {
    fn new(inner: Box<dyn ToolDyn>, lane: Arc<RwLock<()>>) -> Self {
        let access = access_for(&inner.name());
        Self {
            inner,
            lane,
            access,
        }
    }
}

impl ToolDyn for ConcurrencyBoundTool {
    fn name(&self) -> String {
        self.inner.name()
    }

    fn description(&self) -> String {
        self.inner.description()
    }

    fn parameters(&self) -> serde_json::Value {
        self.inner.parameters()
    }

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        let spend = crate::spend::run::Run::current();
        let future = async move {
            match self.access {
                ToolAccess::Read => {
                    let _lease = self.lane.read().await;
                    crate::spend::run::admit_current_effect()
                        .await
                        .map_err(|error| ToolError::ToolCallError(error.into()))?;

                    self.inner.call(args).await
                }
                ToolAccess::Write => {
                    let _lease = self.lane.write().await;
                    crate::spend::run::admit_current_effect()
                        .await
                        .map_err(|error| ToolError::ToolCallError(error.into()))?;

                    self.inner.call(args).await
                }
            }
        };
        let future = crate::spend::run::scope(spend, future);
        #[cfg(feature = "acp")]
        let future = crate::permission::ask::scope_tool_call(self.inner.name(), future);
        Box::pin(future)
    }

    fn call_with_extensions<'a>(
        &'a self,
        args: String,
        extensions: &'a ToolCallExtensions,
    ) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        let spend = extensions
            .get::<crate::spend::run::Run>()
            .cloned()
            .or_else(crate::spend::run::Run::current);
        let future = async move {
            match self.access {
                ToolAccess::Read => {
                    let _lease = self.lane.read().await;
                    crate::spend::run::admit_current_effect()
                        .await
                        .map_err(|error| ToolError::ToolCallError(error.into()))?;

                    self.inner.call_with_extensions(args, extensions).await
                }
                ToolAccess::Write => {
                    let _lease = self.lane.write().await;
                    crate::spend::run::admit_current_effect()
                        .await
                        .map_err(|error| ToolError::ToolCallError(error.into()))?;

                    self.inner.call_with_extensions(args, extensions).await
                }
            }
        };
        let future = crate::spend::run::scope(spend, future);
        #[cfg(feature = "acp")]
        let future = crate::permission::ask::scope_tool_call(self.inner.name(), future);
        Box::pin(future)
    }

    fn call_structured<'a>(
        &'a self,
        args: String,
        extensions: &'a ToolCallExtensions,
    ) -> WasmBoxedFuture<'a, ToolExecutionResult> {
        let spend = extensions
            .get::<crate::spend::run::Run>()
            .cloned()
            .or_else(crate::spend::run::Run::current);
        let future = async move {
            match self.access {
                ToolAccess::Read => {
                    let _lease = self.lane.read().await;
                    if let Err(error) = crate::spend::run::admit_current_effect().await {
                        return ToolExecutionResult::denied(error.to_string());
                    }

                    self.inner.call_structured(args, extensions).await
                }
                ToolAccess::Write => {
                    let _lease = self.lane.write().await;
                    if let Err(error) = crate::spend::run::admit_current_effect().await {
                        return ToolExecutionResult::denied(error.to_string());
                    }

                    self.inner.call_structured(args, extensions).await
                }
            }
        };
        let future = crate::spend::run::scope(spend, future);
        #[cfg(feature = "acp")]
        let future = crate::permission::ask::scope_tool_call(self.inner.name(), future);
        Box::pin(future)
    }
}

pub(crate) fn bind(tools: Vec<Box<dyn ToolDyn>>) -> Vec<Box<dyn ToolDyn>> {
    let lane = Arc::new(RwLock::new(()));
    tools
        .into_iter()
        .map(|tool| {
            Box::new(ConcurrencyBoundTool::new(tool, Arc::clone(&lane))) as Box<dyn ToolDyn>
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rig::tool::Tool;

    use super::*;

    #[derive(Clone)]
    struct ProbeTool {
        name: &'static str,
        active: Arc<AtomicUsize>,
        maximum: Arc<AtomicUsize>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    impl Tool for ProbeTool {
        const NAME: &'static str = "probe";
        type Error = Infallible;
        type Args = serde_json::Value;
        type Output = String;

        fn name(&self) -> String {
            self.name.to_owned()
        }

        fn description(&self) -> String {
            String::new()
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.maximum.fetch_max(active, Ordering::SeqCst);
            self.entered.notify_one();
            self.release.notified().await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(self.name.to_owned())
        }
    }

    fn probe(
        name: &'static str,
    ) -> (
        ProbeTool,
        Arc<tokio::sync::Notify>,
        Arc<tokio::sync::Notify>,
        Arc<AtomicUsize>,
    ) {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let maximum = Arc::new(AtomicUsize::new(0));
        (
            ProbeTool {
                name,
                active: Arc::new(AtomicUsize::new(0)),
                maximum: Arc::clone(&maximum),
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            },
            entered,
            release,
            maximum,
        )
    }

    #[tokio::test]
    async fn read_tools_share_the_lane() {
        let (first, first_entered, first_release, maximum) = probe("read");
        let mut second = first.clone();
        second.entered = Arc::clone(&first_entered);
        second.release = Arc::clone(&first_release);
        let tools = bind(vec![Box::new(first), Box::new(second)]);
        let joined = tokio::spawn(async move {
            let one = tools[0].call("{}".to_owned());
            let two = tools[1].call("{}".to_owned());
            tokio::join!(one, two)
        });

        first_entered.notified().await;
        first_entered.notified().await;
        first_release.notify_waiters();
        let _ = joined.await.unwrap();
        assert_eq!(maximum.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn unknown_and_mutating_tools_take_the_exclusive_lane() {
        let (first, entered, release, maximum) = probe("write");
        let mut second = first.clone();
        second.name = "mcp_unknown";
        let tools = bind(vec![Box::new(first), Box::new(second)]);
        let joined = tokio::spawn(async move {
            let one = tools[0].call("{}".to_owned());
            let two = tools[1].call("{}".to_owned());
            tokio::join!(one, two)
        });

        entered.notified().await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), entered.notified())
                .await
                .is_err()
        );
        release.notify_one();
        entered.notified().await;
        release.notify_one();
        let _ = joined.await.unwrap();
        assert_eq!(maximum.load(Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
mod usd_budget_boundary_tests {
    use super::*;
    use crate::spend::{Account, Ledger, PriceSnapshot, Receipt, RequestKind};
    use rig::tool::{Tool, ToolOutcome};
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct GatedEffect {
        calls: Arc<AtomicUsize>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    impl Tool for GatedEffect {
        const NAME: &'static str = "write";
        type Args = serde_json::Value;
        type Error = Infallible;
        type Output = String;
        fn description(&self) -> String {
            "effect fixture".into()
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        async fn call(&self, _: Self::Args) -> Result<String, Infallible> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            self.release.notified().await;
            Ok("committed".into())
        }
    }
    #[tokio::test]
    async fn usd_budget_queued_dynamic_tool_rechecks_after_lane_with_exact_extensions() {
        let root = crate::tests::OwnedTestDirectory::new("cost-tool-lane");
        let account = Account::new("session", Some(1.0), 0.0).unwrap();
        let prices = PriceSnapshot::capture(Some((2.0, 4.0)), Default::default(), false);
        let ledger = Arc::new(Ledger::new(root.path()));
        let run = crate::spend::run::Run::fixture(account.clone(), ledger, "session", prices);
        run.prepare().await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let tools = bind(vec![Box::new(GatedEffect {
            calls: calls.clone(),
            entered: entered.clone(),
            release: release.clone(),
        })]);
        let task = tokio::spawn(async move {
            let extensions = run.extensions();
            let first = tools[0].call_structured("{}".into(), &extensions);
            let second = tools[0].call_structured("{}".into(), &extensions);
            tokio::join!(first, second)
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        let receipt = Receipt::new(
            "session",
            "openai",
            "fixture",
            RequestKind::Turn,
            crate::event::UsageDelta {
                input_tokens: 1_000_000,
                ..Default::default()
            },
            prices,
        );
        account.observe(&receipt).unwrap();
        release.notify_one();
        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(first.outcome(), ToolOutcome::Success));
        assert!(matches!(second.outcome(), ToolOutcome::Denied));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            second
                .model_output()
                .contains(crate::spend::run::BUDGET_STOP)
        );
        assert!(
            second.extensions().is_empty(),
            "denied effects have no mutation metadata"
        );
        assert_eq!(account.snapshot().recorded_spend(), 2.0);
    }
    #[tokio::test]
    async fn usd_budget_native_write_late_approval_cannot_mutate_or_leave_session_grant() {
        use crate::permission::ask::UserDecision;
        use crate::permission::checker::{CheckResult, PermissionChecker};
        use crate::permission::{PermissionConfigs, SecurityMode};
        let root = crate::tests::OwnedTestDirectory::new("cost-write-approval");
        let permission = Arc::new(std::sync::Mutex::new(
            PermissionChecker::new(
                &PermissionConfigs::from(crate::permission::PermissionConfig {
                    write: Some(crate::permission::ToolPerm::Simple(
                        crate::permission::Action::Ask,
                    )),
                    ..Default::default()
                }),
                SecurityMode::Standard,
                Some(root.path().to_path_buf()),
                Some(vec!["guarded".into(), "standard".into()]),
            )
            .unwrap(),
        ));
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let tools = bind(vec![Box::new(
            crate::agent::tools::write::WriteTool::new(Some(permission.clone()), Some(tx), None)
                .with_workspace_root(root.path().to_path_buf()),
        )]);
        let account = Account::new("session", Some(1.0), 0.0).unwrap();
        let prices = PriceSnapshot::capture(Some((2.0, 4.0)), Default::default(), false);
        let run = crate::spend::run::Run::fixture(
            account.clone(),
            Arc::new(Ledger::new(root.path())),
            "session",
            prices,
        );
        run.prepare().await.unwrap();
        let task = tokio::spawn(async move {
            tools[0]
                .call_structured(
                    serde_json::json!({"path":"late.txt","content":"effect","overwrite":false})
                        .to_string(),
                    &run.extensions(),
                )
                .await
        });
        let ask = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ask.tool.as_str(), "write");
        account
            .observe(&Receipt::new(
                "session",
                "openai",
                "fixture",
                RequestKind::Turn,
                crate::event::UsageDelta {
                    input_tokens: 1_000_000,
                    ..Default::default()
                },
                prices,
            ))
            .unwrap();
        ask.reply
            .send(UserDecision::AllowAlways("late.txt".into()))
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(!matches!(result.outcome(), ToolOutcome::Success));
        assert!(
            result
                .model_output()
                .contains(crate::spend::run::BUDGET_STOP)
        );
        assert!(!root.path().join("late.txt").exists());
        assert!(result.extensions().is_empty());
        assert!(matches!(
            permission
                .lock()
                .unwrap()
                .check_path("write", &root.path().join("late.txt").to_string_lossy()),
            CheckResult::Ask
        ));
    }
}
