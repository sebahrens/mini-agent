//! Concurrency boundary for model-issued tool batches.
//!
//! Rig can execute calls from one assistant message concurrently. Every tool
//! in an agent shares this reader/writer lane: explicitly classified
//! read-only tools take a shared lease, while mutating and unknown tools take
//! an exclusive lease. The conservative default keeps extension/MCP tools from
//! acquiring parallel mutation authority accidentally.
//!
//! `edit`, `write`, `notebook_edit` and `shell` give the exclusive lease back
//! only while their permission prompt is open ([`prompt_unlocked`]) and
//! re-acquire it before using the answer; targets opened before the prompt are
//! re-captured and a changed identity is the ordinary path-changed error.

use std::sync::Arc;

use rig::tool::{ToolCallExtensions, ToolDyn, ToolError, ToolExecutionResult};
use rig::wasm_compat::WasmBoxedFuture;
use tokio::sync::RwLock;

pub(crate) const DEFAULT_TOOL_CONCURRENCY: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolAccess {
    Read,
    Write,
    /// `job_status`: poll, wait and list only read the session job registry
    /// (or await its change notification), so they share the lane. `stop`
    /// mutates a job and stays exclusive, as does any unparseable request.
    JobStatus,
}

fn access_for(name: &str) -> ToolAccess {
    match name {
        "read" | "grep" | "find_files" | "list_dir" | "todo_read" | "memory_read"
        | "memory_search" | "advisor" | "skills_search" => ToolAccess::Read,
        // Network-only: each call is independently permission-checked and
        // DNS-bounded, and the only local effect is a private per-call spill.
        // `lsp_diagnostics` stays exclusive: it syncs documents into the
        // shared language-server state (didChange/baseline) before querying.
        "web_fetch" | "web_search" => ToolAccess::Read,
        "job_status" => ToolAccess::JobStatus,
        _ => ToolAccess::Write,
    }
}

/// The lease one call needs. Only `job_status` depends on its arguments.
fn access_for_call(access: ToolAccess, args: &str) -> ToolAccess {
    match access {
        ToolAccess::JobStatus => {
            let action = serde_json::from_str::<serde_json::Value>(args)
                .ok()
                .and_then(|value| match value {
                    serde_json::Value::Object(map) => Some(
                        map.get("action")
                            .map(|action| action.as_str().map(str::to_owned)),
                    ),
                    _ => None,
                });
            match action {
                Some(None) => ToolAccess::Read,
                Some(Some(Some(action))) if matches!(action.as_str(), "poll" | "wait" | "list") => {
                    ToolAccess::Read
                }
                _ => ToolAccess::Write,
            }
        }
        access => access,
    }
}

enum Lease {
    Shared(#[allow(dead_code)] tokio::sync::OwnedRwLockReadGuard<()>),
    Exclusive(#[allow(dead_code)] tokio::sync::OwnedRwLockWriteGuard<()>),
}

/// The lease one tool call holds, reachable from the permission prompt.
///
/// An exclusive lease exists to stop concurrent mutation authority, but a
/// permission wait precedes any effect. Mutating tools that opt in through
/// [`prompt_unlocked`] let the prompt give the lease back so read-only calls
/// from the same batch (and other prompts) are not stalled behind a human
/// answer. The lease is re-acquired before the tool continues, and such a tool
/// must treat every identity it captured before the prompt as stale and
/// re-capture it under the re-acquired lease.
struct LaneSlot {
    lane: Arc<RwLock<()>>,
    lease: std::sync::Mutex<Option<Lease>>,
}

/// Set by a tool around its permission check to allow that check to release
/// the exclusive lease while the prompt is open.
#[derive(Default)]
struct PromptUnlock {
    released: std::sync::atomic::AtomicBool,
}

tokio::task_local! {
    static LANE: Arc<LaneSlot>;
    static PROMPT_UNLOCK: Arc<PromptUnlock>;
}

/// Run a permission check that may release the exclusive lease while the user
/// is asked. Returns the check's output and whether the lease was released
/// (so every file identity captured before the prompt must be re-captured).
pub(crate) async fn prompt_unlocked<F: std::future::Future>(check: F) -> (F::Output, bool) {
    let marker = Arc::new(PromptUnlock::default());
    let output = PROMPT_UNLOCK.scope(Arc::clone(&marker), check).await;
    (
        output,
        marker.released.load(std::sync::atomic::Ordering::SeqCst),
    )
}

/// An exclusive lease given back for the duration of one prompt.
pub(crate) struct SuspendedLease {
    slot: Arc<LaneSlot>,
}

/// Give back the calling tool's exclusive lease if the tool opted in through
/// [`prompt_unlocked`]. Shared leases, unscoped callers and tools that did not
/// opt in keep what they hold.
pub(crate) fn suspend_for_prompt() -> Option<SuspendedLease> {
    let marker = PROMPT_UNLOCK.try_with(Arc::clone).ok()?;
    let slot = LANE.try_with(Arc::clone).ok()?;
    {
        let mut lease = slot.lease.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(*lease, Some(Lease::Exclusive(_))) {
            return None;
        }
        *lease = None;
    }
    marker
        .released
        .store(true, std::sync::atomic::Ordering::SeqCst);
    Some(SuspendedLease { slot })
}

impl SuspendedLease {
    /// Wait for the exclusive lease again. Queues behind any other holder.
    pub(crate) async fn resume(self) {
        let guard = Arc::clone(&self.slot.lane).write_owned().await;
        *self.slot.lease.lock().unwrap_or_else(|e| e.into_inner()) = Some(Lease::Exclusive(guard));
    }
}

struct ConcurrencyBoundTool {
    inner: Box<dyn ToolDyn>,
    lane: Arc<RwLock<()>>,
    access: ToolAccess,
    /// Foreground `shell` calls claim their announced internal call id so
    /// their live output preview is labelled like the transcript.
    is_shell: bool,
}

impl ConcurrencyBoundTool {
    fn new(inner: Box<dyn ToolDyn>, lane: Arc<RwLock<()>>) -> Self {
        let name = inner.name();
        let access = access_for(&name);
        Self {
            inner,
            lane,
            access,
            is_shell: name == "shell",
        }
    }

    async fn lease(&self, args: &str) -> Arc<LaneSlot> {
        let lease = match access_for_call(self.access, args) {
            ToolAccess::Read => Lease::Shared(Arc::clone(&self.lane).read_owned().await),
            ToolAccess::Write | ToolAccess::JobStatus => {
                Lease::Exclusive(Arc::clone(&self.lane).write_owned().await)
            }
        };
        Arc::new(LaneSlot {
            lane: Arc::clone(&self.lane),
            lease: std::sync::Mutex::new(Some(lease)),
        })
    }

    fn question_admission() -> Result<(), ToolError> {
        match crate::question::required() {
            Some(required) => Err(ToolError::ToolCallError(Box::new(required))),
            None => Ok(()),
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
            let slot = self.lease(&args).await;
            LANE.scope(slot, async {
                crate::spend::run::admit_current_effect()
                    .await
                    .map_err(|error| ToolError::ToolCallError(error.into()))?;
                Self::question_admission()?;
                self.inner.call(args).await
            })
            .await
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
            let slot = self.lease(&args).await;
            LANE.scope(slot, async {
                crate::spend::run::admit_current_effect()
                    .await
                    .map_err(|error| ToolError::ToolCallError(error.into()))?;
                Self::question_admission()?;
                self.inner.call_with_extensions(args, extensions).await
            })
            .await
        };
        let future = crate::spend::run::scope(spend, future);
        let future = crate::question::scope_extensions(extensions, future);
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
        // Claimed synchronously, right after the SDK announced this call.
        let call_id = if self.is_shell {
            crate::agent::tools::bash::claim_shell_call_id(extensions, &args)
        } else {
            None
        };
        let future = async move {
            let slot = self.lease(&args).await;
            LANE.scope(slot, async {
                if let Err(error) = crate::spend::run::admit_current_effect().await {
                    return ToolExecutionResult::denied(error.to_string());
                }
                if let Err(error) = Self::question_admission() {
                    return ToolExecutionResult::denied(error.to_string());
                }
                self.inner.call_structured(args, extensions).await
            })
            .await
        };
        let future = crate::agent::tools::bash::scope_shell_call_id(call_id, future);
        let future = crate::spend::run::scope(spend, future);
        let future = crate::question::scope_extensions(extensions, future);
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
    pub(super) struct ProbeTool {
        pub(super) name: &'static str,
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

    pub(super) fn probe(
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
    async fn web_tools_share_the_lane() {
        let (first, first_entered, first_release, maximum) = probe("web_fetch");
        let mut second = first.clone();
        second.name = "web_search";
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
        assert_eq!(access_for("lsp_diagnostics"), ToolAccess::Write);
    }

    #[test]
    fn job_status_reads_share_and_stop_stays_exclusive() {
        for (args, expected) in [
            (r#"{"action":"wait","id":"job-1"}"#, ToolAccess::Read),
            (r#"{"action":"poll","id":"job-1"}"#, ToolAccess::Read),
            (r#"{"id":"job-1"}"#, ToolAccess::Read),
            (r#"{"action":"list"}"#, ToolAccess::Read),
            (r#"{"action":"stop","id":"job-1"}"#, ToolAccess::Write),
            (r#"{"action":"bogus"}"#, ToolAccess::Write),
            (r#"{"action":7}"#, ToolAccess::Write),
            ("not json", ToolAccess::Write),
            ("[]", ToolAccess::Write),
        ] {
            assert_eq!(
                access_for_call(access_for("job_status"), args),
                expected,
                "{args}"
            );
        }
        assert_eq!(
            access_for_call(access_for("shell"), r#"{"action":"wait"}"#),
            ToolAccess::Write
        );
    }

    #[tokio::test]
    async fn job_status_wait_does_not_block_a_concurrent_read() {
        let (wait, entered, release, maximum) = probe("job_status");
        let mut read = wait.clone();
        read.name = "read";
        let tools = bind(vec![Box::new(wait), Box::new(read)]);
        let joined = tokio::spawn(async move {
            let one = tools[0].call(r#"{"action":"wait","id":"job-1"}"#.to_owned());
            let two = tools[1].call("{}".to_owned());
            tokio::join!(one, two)
        });
        entered.notified().await;
        tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
            .await
            .expect("read must run while job_status wait holds a shared lease");
        release.notify_waiters();
        let _ = joined.await.unwrap();
        assert_eq!(maximum.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn job_status_stop_takes_the_exclusive_lane() {
        let (stop, entered, release, maximum) = probe("job_status");
        let mut read = stop.clone();
        read.name = "read";
        let tools = bind(vec![Box::new(stop), Box::new(read)]);
        let joined = tokio::spawn(async move {
            let one = tools[0].call(r#"{"action":"stop","id":"job-1"}"#.to_owned());
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
            .unwrap()
            .into_permission();
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

#[cfg(test)]
mod question_boundary_tests {
    use super::*;
    use crate::permission::ask::UserDecision;
    use crate::permission::checker::{CheckResult, PermissionChecker};
    use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};
    #[tokio::test]
    async fn ask_user_parked_question_rejects_late_real_write_approval_before_grant_or_effect() {
        for parked in [false, true] {
            let root = crate::tests::OwnedTestDirectory::new("question-late-write");
            let permission = Arc::new(std::sync::Mutex::new(
                PermissionChecker::new(
                    &PermissionConfigs::from(PermissionConfig {
                        write: Some(ToolPerm::Simple(Action::Ask)),
                        ..Default::default()
                    }),
                    SecurityMode::Standard,
                    Some(root.path().to_path_buf()),
                    None,
                )
                .unwrap(),
            ));
            let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
            let tools = bind(vec![Box::new(
                crate::agent::tools::write::WriteTool::new(
                    Some(permission.clone()),
                    Some(sender),
                    None,
                )
                .with_workspace_root(root.path().to_path_buf()),
            )]);
            let turn = crate::question::QuestionTurn::default();
            let mut extensions = ToolCallExtensions::new();
            extensions.insert(turn.clone());
            let task = tokio::spawn(async move {
                tools[0]
                    .call_structured(
                        serde_json::json!({"path":"late.txt","content":"actual effect"})
                            .to_string(),
                        &extensions,
                    )
                    .await
            });
            let request = tokio::time::timeout(std::time::Duration::from_secs(3), receiver.recv())
                .await
                .unwrap()
                .unwrap()
                .into_permission();
            if parked {
                turn.require(crate::question::QuestionBatch(vec![
                    crate::question::Question {
                        question: "Clarify first".into(),
                        options: vec![],
                        allow_free_text: true,
                    },
                ]));
            }
            request
                .reply
                .send(UserDecision::AllowAlways("late.txt".into()))
                .unwrap();
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(root.path().join("late.txt").exists(), !parked);
            assert_eq!(
                matches!(outcome.outcome(), rig::tool::ToolOutcome::Success),
                !parked
            );
            let policy = permission
                .lock()
                .unwrap()
                .check_path("write", &root.path().join("late.txt").to_string_lossy());
            assert_eq!(matches!(policy, CheckResult::Ask), parked);
            if parked {
                assert!(outcome.extensions().is_empty());
                assert!(outcome.model_output().contains("user answer"));
            }
        }
    }
}

#[cfg(test)]
mod prompt_release_tests {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use rig::tool::{Tool, ToolOutcome};

    use super::tests::{ProbeTool, probe};
    use super::*;
    use crate::permission::ask::{AskRequest, UserDecision, UserRequest};
    use crate::permission::checker::PermissionChecker;
    use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};

    const WAIT: Duration = Duration::from_secs(3);

    fn asking_permission(root: &std::path::Path) -> crate::permission::checker::PermCheck {
        Arc::new(std::sync::Mutex::new(
            PermissionChecker::new(
                &PermissionConfigs::from(PermissionConfig {
                    write: Some(ToolPerm::Simple(Action::Ask)),
                    edit: Some(ToolPerm::Simple(Action::Ask)),
                    notebook_edit: Some(ToolPerm::Simple(Action::Ask)),
                    bash: Some(ToolPerm::Simple(Action::Ask)),
                    read: Some(ToolPerm::Simple(Action::Allow)),
                    ..Default::default()
                }),
                SecurityMode::Standard,
                Some(root.to_path_buf()),
                None,
            )
            .unwrap(),
        ))
    }

    async fn next_request(receiver: &mut tokio::sync::mpsc::Receiver<UserRequest>) -> AskRequest {
        tokio::time::timeout(WAIT, receiver.recv())
            .await
            .expect("permission request")
            .expect("ask channel open")
            .into_permission()
    }

    /// A mutating probe that asks through the real permission path (opted in)
    /// and then holds the lane until released.
    #[derive(Clone)]
    struct AskingProbe {
        permission: crate::permission::checker::PermCheck,
        ask_tx: crate::permission::ask::AskSender,
        probe: ProbeTool,
        name: &'static str,
    }

    impl Tool for AskingProbe {
        const NAME: &'static str = "write";
        type Error = ToolError;
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

        async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
            let (checked, released) = prompt_unlocked(crate::agent::tools::check_perm(
                &Some(self.permission.clone()),
                &Some(self.ask_tx.clone()),
                "shell",
                "touch effect",
            ))
            .await;
            checked.map_err(|error| ToolError::ToolCallError(error.to_string().into()))?;
            assert!(released, "the answered prompt must report the release");
            Tool::call(&self.probe, args)
                .await
                .map_err(|never| match never {})
        }
    }

    #[tokio::test]
    async fn read_enters_while_a_mutating_call_awaits_approval_then_exclusivity_returns() {
        let root = crate::tests::OwnedTestDirectory::new("prompt-release-probe");
        let (probe, entered, release, maximum) = probe("write");
        let mut read = probe.clone();
        read.name = "read";
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let asking = AskingProbe {
            permission: asking_permission(root.path()),
            ask_tx: sender,
            probe: probe.clone(),
            name: "write",
        };
        let tools = Arc::new(bind(vec![Box::new(asking), Box::new(read.clone())]));
        let write_tools = Arc::clone(&tools);
        let write = tokio::spawn(async move { write_tools[0].call("{}".to_owned()).await });
        let request = next_request(&mut receiver).await;

        // The prompt is unanswered, yet a read from the same batch runs.
        let read_tools = Arc::clone(&tools);
        let first_read = tokio::spawn(async move { read_tools[1].call("{}".to_owned()).await });
        tokio::time::timeout(WAIT, entered.notified())
            .await
            .expect("read must enter while the write waits for approval");
        release.notify_one();
        first_read.await.unwrap().unwrap();
        assert_eq!(maximum.load(Ordering::SeqCst), 1);

        // After approval the write is exclusive again: a new read cannot enter.
        request.reply.send(UserDecision::AllowOnce).unwrap();
        tokio::time::timeout(WAIT, entered.notified())
            .await
            .expect("approved write proceeds");
        let read_tools = Arc::clone(&tools);
        let second_read = tokio::spawn(async move { read_tools[1].call("{}".to_owned()).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(60), entered.notified())
                .await
                .is_err(),
            "the re-acquired lane must exclude reads while the write runs"
        );
        assert_eq!(maximum.load(Ordering::SeqCst), 1);
        release.notify_one();
        write.await.unwrap().unwrap();
        tokio::time::timeout(WAIT, entered.notified())
            .await
            .expect("read runs after the write finished");
        release.notify_one();
        second_read.await.unwrap().unwrap();
        assert_eq!(maximum.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn denied_prompt_returns_the_lane_and_reports_denial() {
        let root = crate::tests::OwnedTestDirectory::new("prompt-release-denied");
        let permission = asking_permission(root.path());
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let tools = Arc::new(bind(vec![
            Box::new(
                crate::agent::tools::write::WriteTool::new(Some(permission), Some(sender), None)
                    .with_workspace_root(root.path().to_path_buf()),
            ),
            Box::new(crate::agent::tools::read::ReadTool::new(
                None, None, None, 100,
            )),
        ]));
        let write_tools = Arc::clone(&tools);
        let write = tokio::spawn(async move {
            write_tools[0]
                .call(serde_json::json!({"path":"new.txt","content":"x"}).to_string())
                .await
        });
        let request = next_request(&mut receiver).await;
        request.reply.send(UserDecision::Deny).unwrap();
        assert!(write.await.unwrap().is_err());
        assert!(!root.path().join("new.txt").exists());
        // The exclusive lane is free again: a following write is not stuck.
        let again_tools = Arc::clone(&tools);
        let again = tokio::spawn(async move {
            again_tools[0]
                .call(serde_json::json!({"path":"again.txt","content":"x"}).to_string())
                .await
        });
        next_request(&mut receiver)
            .await
            .reply
            .send(UserDecision::AllowOnce)
            .unwrap();
        tokio::time::timeout(WAIT, again)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(root.path().join("again.txt").exists());
    }

    #[tokio::test]
    async fn real_read_runs_while_a_real_write_awaits_approval() {
        let root = crate::tests::OwnedTestDirectory::new("prompt-release-real-write");
        std::fs::write(root.path().join("existing.txt"), "hello\n").unwrap();
        let permission = asking_permission(root.path());
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let tools = Arc::new(bind(vec![
            Box::new(
                crate::agent::tools::write::WriteTool::new(
                    Some(permission.clone()),
                    Some(sender),
                    None,
                )
                .with_workspace_root(root.path().to_path_buf()),
            ),
            Box::new(
                crate::agent::tools::read::ReadTool::new(Some(permission), None, None, 100)
                    .with_workspace_root(root.path().to_path_buf()),
            ),
        ]));
        let write_tools = Arc::clone(&tools);
        let write = tokio::spawn(async move {
            write_tools[0]
                .call(serde_json::json!({"path":"fresh.txt","content":"new"}).to_string())
                .await
        });
        let request = next_request(&mut receiver).await;
        let read = tokio::time::timeout(
            WAIT,
            tools[1].call(serde_json::json!({"path":"existing.txt"}).to_string()),
        )
        .await
        .expect("read must not wait for the write prompt")
        .unwrap();
        assert!(read.contains("hello"));
        assert!(!root.path().join("fresh.txt").exists());
        request.reply.send(UserDecision::AllowOnce).unwrap();
        tokio::time::timeout(WAIT, write)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("fresh.txt")).unwrap(),
            "new"
        );
    }

    fn edit_args(search: &str, replace: &str) -> String {
        serde_json::json!({
            "path": "shared.txt",
            "block": format!("<<<<<<< SEARCH\n{search}\n=======\n{replace}\n>>>>>>> REPLACE"),
        })
        .to_string()
    }

    fn edit_tool(
        root: &std::path::Path,
        permission: &crate::permission::checker::PermCheck,
        sender: &crate::permission::ask::AskSender,
    ) -> Box<dyn ToolDyn> {
        Box::new(
            crate::agent::tools::edit::EditTool::new(
                Some(permission.clone()),
                Some(sender.clone()),
            )
            .with_workspace(root),
        )
    }

    #[tokio::test]
    async fn two_same_file_edits_needing_approval_never_publish_a_stale_identity() {
        let root = crate::tests::OwnedTestDirectory::new("prompt-release-two-edits");
        let file = root.path().join("shared.txt");
        std::fs::write(&file, "one\ntwo\n").unwrap();
        let permission = asking_permission(root.path());
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let tools = Arc::new(bind(vec![
            edit_tool(root.path(), &permission, &sender),
            edit_tool(root.path(), &permission, &sender),
        ]));
        let extensions = Arc::new(ToolCallExtensions::new());
        let mut calls = Vec::new();
        let mut requests = Vec::new();
        for (index, (search, replace)) in [("one", "ONE"), ("two", "TWO")].into_iter().enumerate() {
            let tools = Arc::clone(&tools);
            let extensions = Arc::clone(&extensions);
            let args = edit_args(search, replace);
            calls.push(tokio::spawn(async move {
                tools[index].call_structured(args, &extensions).await
            }));
            // Both prompts must be open at once: the first edit gave the lane back.
            requests.push(next_request(&mut receiver).await);
        }
        let second = requests.pop().unwrap();
        let first = requests.pop().unwrap();
        first.reply.send(UserDecision::AllowOnce).unwrap();
        let first_result = tokio::time::timeout(WAIT, calls.remove(0))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(first_result.outcome(), ToolOutcome::Success));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "ONE\ntwo\n");

        second.reply.send(UserDecision::AllowOnce).unwrap();
        let second_result = tokio::time::timeout(WAIT, calls.remove(0))
            .await
            .unwrap()
            .unwrap();
        assert!(!matches!(second_result.outcome(), ToolOutcome::Success));
        assert!(
            second_result
                .model_output()
                .contains("Path changed after permission check"),
            "{}",
            second_result.model_output()
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "ONE\ntwo\n");
    }

    #[tokio::test]
    async fn external_replacement_between_prompt_and_approval_is_a_stale_identity_error() {
        let root = crate::tests::OwnedTestDirectory::new("prompt-release-external-edit");
        let file = root.path().join("shared.txt");
        std::fs::write(&file, "one\ntwo\n").unwrap();
        let permission = asking_permission(root.path());
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let tools = Arc::new(bind(vec![edit_tool(root.path(), &permission, &sender)]));
        let extensions = Arc::new(ToolCallExtensions::new());
        let call_tools = Arc::clone(&tools);
        let call_extensions = Arc::clone(&extensions);
        let call = tokio::spawn(async move {
            call_tools[0]
                .call_structured(edit_args("one", "ONE"), &call_extensions)
                .await
        });
        let request = next_request(&mut receiver).await;
        // Replace the file by rename, as an editor or another process would.
        let replacement = root.path().join("replacement.tmp");
        std::fs::write(&replacement, "external\ntwo\n").unwrap();
        std::fs::rename(&replacement, &file).unwrap();
        request.reply.send(UserDecision::AllowOnce).unwrap();
        let result = tokio::time::timeout(WAIT, call).await.unwrap().unwrap();
        assert!(!matches!(result.outcome(), ToolOutcome::Success));
        assert!(
            result
                .model_output()
                .contains("Path changed after permission check"),
            "{}",
            result.model_output()
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "external\ntwo\n");
    }

    #[tokio::test]
    async fn notebook_replaced_between_prompt_and_approval_is_a_stale_identity_error() {
        const NOTEBOOK: &str = r#"{"cells":[{"cell_type":"code","id":"a","metadata":{},"source":"x","outputs":[],"execution_count":null}],"metadata":{},"nbformat":4,"nbformat_minor":5}"#;
        let root = crate::tests::OwnedTestDirectory::new("prompt-release-notebook");
        let file = root.path().join("nb.ipynb");
        std::fs::write(&file, NOTEBOOK).unwrap();
        let permission = asking_permission(root.path());
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let workspace = crate::agent::tools::capture_workspace_binding(root.path().to_path_buf());
        let tools = Arc::new(bind(vec![Box::new(
            crate::agent::tools::notebook::NotebookTool::new(
                Some(permission),
                Some(sender),
                workspace,
                crate::agent::tools::ReadTracker::new(true),
                None,
                Default::default(),
            ),
        )]));
        let extensions = Arc::new(ToolCallExtensions::new());
        let call_tools = Arc::clone(&tools);
        let call_extensions = Arc::clone(&extensions);
        let call = tokio::spawn(async move {
            call_tools[0]
                .call_structured(
                    serde_json::json!({"path":"nb.ipynb","mode":"replace","cell_index":0,"source":"y"})
                        .to_string(),
                    &call_extensions,
                )
                .await
        });
        let request = next_request(&mut receiver).await;
        let external = NOTEBOOK.replace("\"x\"", "\"external\"");
        let replacement = root.path().join("replacement.tmp");
        std::fs::write(&replacement, &external).unwrap();
        std::fs::rename(&replacement, &file).unwrap();
        request.reply.send(UserDecision::AllowOnce).unwrap();
        let result = tokio::time::timeout(WAIT, call).await.unwrap().unwrap();
        assert!(!matches!(result.outcome(), ToolOutcome::Success));
        assert!(
            result
                .model_output()
                .contains("Path changed after permission check"),
            "{}",
            result.model_output()
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), external);
    }
}
