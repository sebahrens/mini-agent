//! Session-owned MCP tool discovery and bounded schema activation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use rig::completion::ToolDefinition;
use rig::tool::server::WeakToolServerHandle;
use rig::tool::{ToolCallExtensions, ToolDyn, ToolError, ToolExecutionResult};
use rig::wasm_compat::WasmBoxedFuture;

const MAX_SESSIONS: usize = 128;
const MAX_QUERY_BYTES: usize = 2048;
const MAX_SEARCH_RESULTS: usize = 8;
const MAX_SEARCH_BYTES: usize = 128 * 1024;

/// Runtime metadata only. It contains no SDK handle, tool implementation,
/// persisted approval, credential, answer or permission grant.
#[derive(Clone, Default)]
pub(crate) struct DiscoverySessions(Arc<Mutex<BTreeMap<String, Arc<Activation>>>>);

pub(crate) struct Activation {
    workspace: Weak<crate::paths::WorkspaceBinding>,
    fingerprint: String,
    current: AtomicBool,
    names: Mutex<BTreeSet<String>>,
}

impl DiscoverySessions {
    pub(crate) fn retire(&self, session_id: &str) {
        if let Some(state) = self
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(session_id)
        {
            state.current.store(false, Ordering::Release);
        }
    }
    pub(crate) fn capture(
        &self,
        session_id: &str,
        workspace: &Arc<crate::paths::WorkspaceBinding>,
        fingerprint: String,
    ) -> Result<Arc<Activation>, &'static str> {
        if session_id.is_empty()
            || session_id.len() > 128
            || session_id.chars().any(char::is_control)
            || workspace.validate().is_err()
        {
            return Err("MCP discovery session or workspace is unavailable");
        }
        let mut sessions = self.0.lock().unwrap_or_else(|error| error.into_inner());
        sessions.retain(|_, state| {
            let retained = state.workspace.upgrade().is_some();
            if !retained {
                state.current.store(false, Ordering::Release);
            }
            retained
        });
        if let Some(state) = sessions.get(session_id) {
            if state.fingerprint == fingerprint
                && state.current.load(Ordering::Acquire)
                && state
                    .workspace
                    .upgrade()
                    .is_some_and(|old| Arc::ptr_eq(&old, workspace))
            {
                return Ok(state.clone());
            }
            state.current.store(false, Ordering::Release);
        } else if sessions.len() >= MAX_SESSIONS {
            return Err("MCP discovery session limit reached");
        }
        let state = Arc::new(Activation {
            workspace: Arc::downgrade(workspace),
            fingerprint,
            current: AtomicBool::new(true),
            names: Mutex::new(BTreeSet::new()),
        });
        sessions.insert(session_id.to_owned(), state.clone());
        Ok(state)
    }
}

impl Activation {
    fn is_current(&self) -> bool {
        self.current.load(Ordering::Acquire)
            && self
                .workspace
                .upgrade()
                .is_some_and(|workspace| workspace.validate().is_ok())
    }

    pub(crate) fn is_active(&self, name: &str) -> bool {
        self.is_current()
            && self
                .names
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains(name)
    }
}

pub(crate) struct DeferredEntry {
    pub server_name: String,
    pub definition: ToolDefinition,
    pub tool: Arc<dyn ToolDyn>,
    pub source: Option<std::sync::Arc<super::project::ServerSourceProof>>,
    pub is_live: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl DeferredEntry {
    fn is_current(&self) -> bool {
        (self.is_live)()
            && self
                .source
                .as_ref()
                .is_none_or(|source| source.is_current())
    }
}

pub(crate) struct DiscoveryView {
    activation: Arc<Activation>,
    entries: Vec<DeferredEntry>,
}

impl DiscoveryView {
    pub(crate) fn new(activation: Arc<Activation>, entries: Vec<DeferredEntry>) -> Self {
        Self {
            activation,
            entries,
        }
    }

    pub(crate) fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions: Vec<_> = self
            .entries
            .iter()
            .map(|entry| entry.definition.clone())
            .collect();
        definitions.sort_by(|left, right| left.name.cmp(&right.name));
        definitions
    }

    pub(crate) fn initially_active(&self) -> Vec<Box<dyn ToolDyn>> {
        self.entries
            .iter()
            .filter(|entry| self.activation.is_active(&entry.definition.name))
            .map(|entry| Box::new(SharedTool(entry.tool.clone())) as Box<dyn ToolDyn>)
            .collect()
    }

    /// Cached last-build observations only: no RPC, source-file read, SDK
    /// handle, permission query or authority change. A retired generation is
    /// omitted rather than presented as active schemas.
    pub(crate) fn server_counts(&self, server_name: &str) -> Option<(usize, usize)> {
        if !self.activation.current.load(Ordering::Acquire) {
            return None;
        }
        let names = self
            .activation
            .names
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut deferred = 0;
        let mut enabled = 0;
        for entry in &self.entries {
            if entry.server_name == server_name {
                deferred += 1;
                enabled += usize::from(names.contains(&entry.definition.name));
            }
        }
        (deferred > 0).then_some((deferred, enabled))
    }

    pub(crate) fn compact_index(&self) -> String {
        let names: Vec<&str> = self
            .entries
            .iter()
            .map(|entry| entry.definition.name.as_str())
            .collect();
        let mut text = String::from(
            "\n\nMCP tools below require mcp_search before use; search enables schemas only, never permissions.\n<mcp_tool_index>\n[",
        );
        let mut included = 0;
        for name in names {
            let value = serde_json::to_string(name).unwrap();
            if text.len().saturating_add(value.len()).saturating_add(96) > 64 * 1024 {
                break;
            }
            if included > 0 {
                text.push(',');
            }
            text.push_str(&value);
            included += 1;
        }
        text.push_str("]\n</mcp_tool_index>");
        if included < self.entries.len() {
            text.push_str("\n[index truncated; search still considers the captured catalogue]");
        }
        text
    }
}

/// A cheap forwarding adapter. Every activated tool is the SAME already
/// memoized/hooked/concurrency-bound instance, including typed extensions.
struct SharedTool(Arc<dyn ToolDyn>);
impl ToolDyn for SharedTool {
    fn name(&self) -> String {
        self.0.name()
    }
    fn description(&self) -> String {
        self.0.description()
    }
    fn parameters(&self) -> serde_json::Value {
        self.0.parameters()
    }
    fn shared_definition(&self) -> Option<Arc<ToolDefinition>> {
        self.0.shared_definition()
    }
    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        self.0.call(args)
    }
    fn call_with_extensions<'a>(
        &'a self,
        args: String,
        extensions: &'a ToolCallExtensions,
    ) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        self.0.call_with_extensions(args, extensions)
    }
    fn call_structured<'a>(
        &'a self,
        args: String,
        extensions: &'a ToolCallExtensions,
    ) -> WasmBoxedFuture<'a, ToolExecutionResult> {
        self.0.call_structured(args, extensions)
    }
}

#[derive(Clone, Default)]
pub(crate) struct SearchBinding {
    view: Arc<OnceLock<Arc<DiscoveryView>>>,
    server: Arc<OnceLock<WeakToolServerHandle>>,
    native_references: bool,
}

impl SearchBinding {
    pub(crate) fn with_native_references(mut self, enabled: bool) -> Self {
        self.native_references = enabled;
        self
    }
    pub(crate) fn set_view(&self, view: Arc<DiscoveryView>) -> Result<(), &'static str> {
        self.view
            .set(view)
            .map_err(|_| "MCP discovery view already bound")
    }
    pub(crate) fn set_server(&self, server: WeakToolServerHandle) -> Result<(), &'static str> {
        self.server
            .set(server)
            .map_err(|_| "MCP discovery server already bound")
    }
}

pub(crate) struct McpSearch(pub SearchBinding);

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
}

fn discovery_error(message: &'static str) -> ToolError {
    ToolError::ToolCallError(Box::new(super::tool::McpToolError(message.into())))
}

impl McpSearch {
    async fn search(
        &self,
        query: String,
        cancellation: Option<crate::agent::runner::AgentWorkCancellation>,
        question: Option<crate::question::QuestionTurn>,
        spend: Option<crate::spend::run::Run>,
    ) -> Result<(String, Option<rig::tool::ToolReferences>), ToolError> {
        if query.trim().is_empty()
            || query.len() > MAX_QUERY_BYTES
            || query.chars().any(|c| c.is_control() && !c.is_whitespace())
        {
            return Err(discovery_error(
                "MCP search requires a nonempty query up to 2048 bytes",
            ));
        }
        let cancellation = cancellation
            .ok_or_else(|| discovery_error("MCP discovery requires an owned invocation"))?;
        let view = self
            .0
            .view
            .get()
            .ok_or_else(|| discovery_error("MCP discovery view is unavailable"))?;
        let server = self
            .0
            .server
            .get()
            .and_then(WeakToolServerHandle::upgrade)
            .ok_or_else(|| discovery_error("MCP discovery owner has retired"))?;
        let current = || {
            view.activation.is_current()
                && !cancellation.is_cancelled()
                && question
                    .as_ref()
                    .is_none_or(|turn| turn.required().is_none())
                && spend.as_ref().is_none_or(|run| run.admit_effect().is_ok())
        };
        if !current() {
            return Err(discovery_error("MCP discovery invocation has retired"));
        }
        let query = query.to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        if words.len() > 32 {
            return Err(discovery_error("MCP search query exceeds 32 terms"));
        }
        let mut matching: Vec<_> = view
            .entries
            .iter()
            .filter_map(|entry| {
                let name = entry.definition.name.to_lowercase();
                let description = entry.definition.description.to_lowercase();
                let score = if name == query {
                    0
                } else if words.iter().all(|word| name.contains(word)) {
                    1
                } else if words
                    .iter()
                    .all(|word| name.contains(word) || description.contains(word))
                {
                    2
                } else {
                    return None;
                };
                Some((score, entry))
            })
            .collect();
        matching.sort_by(|(left, a), (right, b)| {
            left.cmp(right)
                .then(a.definition.name.cmp(&b.definition.name))
        });
        let total = matching.len();
        let mut selected = Vec::new();
        // Reserve the complete JSON envelope (false is longer than true),
        // brackets and commas before selecting any schema for publication.
        let mut bytes = serde_json::to_vec(
            &serde_json::json!({"matches":[],"omitted":false,"permissions_changed":false}),
        )
        .map_err(ToolError::JsonError)?
        .len();
        for (_, entry) in matching.into_iter().take(MAX_SEARCH_RESULTS) {
            let size = serde_json::to_vec(&entry.definition)
                .map_err(ToolError::JsonError)?
                .len();
            let next = bytes
                .saturating_add(size)
                .saturating_add(usize::from(!selected.is_empty()));
            if next > MAX_SEARCH_BYTES {
                break;
            }
            bytes = next;
            selected.push(entry);
        }
        // Validate the whole selection before publishing ANY definition.
        if !current() || selected.iter().any(|entry| !entry.is_current()) {
            return Err(discovery_error(
                "MCP discovery source or invocation has changed",
            ));
        }
        let definitions: Vec<_> = selected.iter().map(|entry| &entry.definition).collect();
        let result = serde_json::to_string(&serde_json::json!({"matches":definitions,"omitted":total>selected.len(),"permissions_changed":false})).map_err(ToolError::JsonError)?;
        if result.len() > MAX_SEARCH_BYTES {
            return Err(discovery_error(
                "MCP discovery result exceeds its byte limit",
            ));
        }
        let names: Vec<_> = selected
            .iter()
            .map(|entry| entry.definition.name.clone())
            .collect();
        let references = if self.0.native_references {
            Some(
                rig::tool::ToolReferences::new(names.clone()).ok_or_else(|| {
                    discovery_error("MCP native reference name exceeds its bounds")
                })?,
            )
        } else {
            None
        };
        let batch = rig::tool::ToolSet::from_tools(
            selected
                .iter()
                .map(|entry| SharedTool(entry.tool.clone()))
                .collect(),
        );
        let admitted = server
            .append_toolset_if(batch, || {
                if !current() || selected.iter().any(|entry| !entry.is_current()) {
                    return false;
                }
                // No await separates this runtime commit and the registry batch;
                // a late cancellation cannot leave a half-published catalogue.
                view.activation
                    .names
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .extend(selected.iter().map(|entry| entry.definition.name.clone()));
                true
            })
            .await
            .map_err(|_| discovery_error("MCP schema publication failed"))?;
        if !admitted {
            return Err(discovery_error(
                "MCP discovery source or invocation has changed",
            ));
        }
        let result = if self.0.native_references {
            serde_json::to_string(&serde_json::json!({"matches": names, "omitted": total > selected.len(), "permissions_changed": false}))
                .map_err(ToolError::JsonError)?
        } else {
            result
        };
        Ok((result, references))
    }
}

impl ToolDyn for McpSearch {
    fn name(&self) -> String {
        "mcp_search".into()
    }
    fn description(&self) -> String {
        "Search the captured MCP tool catalogue by names and descriptions. Returns matching schemas and enables those exact tools for this session. Does not grant tool permission or call an external server.".into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"query":{"type":"string","maxLength":MAX_QUERY_BYTES}},"required":["query"],"additionalProperties":false})
    }
    fn call(&self, args: String) -> WasmBoxedFuture<'_, Result<String, ToolError>> {
        let cancellation = crate::agent::runner::current_work_cancellation();
        let question = crate::question::QuestionTurn::current();
        let spend = crate::spend::run::Run::current();
        Box::pin(async move {
            let args: SearchArgs = serde_json::from_str(&args)?;
            self.search(args.query, cancellation, question, spend)
                .await
                .map(|(result, _)| result)
        })
    }
    fn call_structured<'a>(
        &'a self,
        args: String,
        _extensions: &'a rig::tool::ToolCallExtensions,
    ) -> WasmBoxedFuture<'a, rig::tool::ToolExecutionResult> {
        let cancellation = crate::agent::runner::current_work_cancellation();
        let question = crate::question::QuestionTurn::current();
        let spend = crate::spend::run::Run::current();
        Box::pin(async move {
            let result = async {
                let args: SearchArgs = serde_json::from_str(&args)?;
                self.search(args.query, cancellation, question, spend).await
            }
            .await;
            match result {
                Ok((text, references)) => {
                    let result = rig::tool::ToolExecutionResult::success(text);
                    match references {
                        Some(references) => result.with_extension(references),
                        None => result,
                    }
                }
                Err(error) => rig::tool::ToolExecutionResult::failed(
                    error.to_string(),
                    rig::tool::ToolFailure::other(error.to_string()),
                ),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::{Arc, OnceLock};
    use std::time::Duration;

    use rig::tool::server::{ToolServer, WeakToolServerHandle};
    use rig::tool::{Tool, ToolCallExtensions, ToolOutcome};
    use serde::Deserialize;

    #[test]
    fn server_deferral_accepts_only_boolean_or_auto_and_preserves_exact_threshold() {
        use crate::extras::mcp::config::{McpDefer, McpServerConfig};
        for (wire, expected) in [
            (serde_json::json!(false), McpDefer::Eager),
            (serde_json::json!(true), McpDefer::Deferred),
            (serde_json::json!("auto"), McpDefer::Auto),
        ] {
            for mut server in [
                serde_json::json!({"command":"owned-runner"}),
                serde_json::json!({"url":"https://example.invalid/mcp"}),
            ] {
                server["defer"] = wire.clone();
                let parsed: McpServerConfig = serde_json::from_value(server).unwrap();
                assert_eq!(parsed.defer(), expected);
                let roundtrip: McpServerConfig =
                    serde_json::from_value(serde_json::to_value(parsed).unwrap()).unwrap();
                assert_eq!(roundtrip.defer(), expected);
            }
        }
        for wire in [
            serde_json::json!(null),
            serde_json::json!(0),
            serde_json::json!("true"),
            serde_json::json!("AUTO"),
            serde_json::json!({}),
            serde_json::json!([]),
        ] {
            assert!(
                serde_json::from_value::<McpServerConfig>(
                    serde_json::json!({"command":"owned-runner","defer":wire})
                )
                .is_err()
            );
        }
        let absent: McpServerConfig =
            serde_json::from_value(serde_json::json!({"command":"owned-runner"})).unwrap();
        assert_eq!(absent.defer(), McpDefer::Auto);
        assert!(!McpDefer::Auto.resolves_deferred(32));
        assert!(McpDefer::Auto.resolves_deferred(33));
        assert!(!McpDefer::Eager.resolves_deferred(usize::MAX));
        assert!(McpDefer::Deferred.resolves_deferred(0));
    }

    #[derive(Deserialize)]
    struct EmptyArgs {}

    struct PublishedTool;
    impl Tool for PublishedTool {
        const NAME: &'static str = "published_by_search";
        type Args = EmptyArgs;
        type Output = String;
        type Error = Infallible;

        fn description(&self) -> String {
            "Owned inert publication target".into()
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        async fn call(&self, _: EmptyArgs) -> Result<String, Infallible> {
            Ok("owned publication".into())
        }
    }

    struct PublishDuringCall(Arc<OnceLock<WeakToolServerHandle>>);
    impl Tool for PublishDuringCall {
        const NAME: &'static str = "publish_during_call";
        type Args = EmptyArgs;
        type Output = String;
        type Error = Infallible;

        fn description(&self) -> String {
            "Exercise the actual SDK dispatch/publication lock boundary".into()
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        async fn call(&self, _: EmptyArgs) -> Result<String, Infallible> {
            let server = self.0.get().unwrap().upgrade().unwrap();
            server.add_tool(PublishedTool).await.unwrap();
            Ok("published".into())
        }
    }

    #[tokio::test]
    async fn actual_structured_discovery_can_publish_without_registry_deadlock_or_handle_cycle() {
        let binding = Arc::new(OnceLock::new());
        let server = ToolServer::new()
            .tool(PublishDuringCall(binding.clone()))
            .run();
        assert!(binding.set(server.downgrade()).is_ok());
        assert_eq!(server.get_tool_defs(None).await.unwrap().len(), 1);
        let extensions = ToolCallExtensions::new();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            server.call_tool_structured("publish_during_call", "{}", &extensions),
        )
        .await
        .expect("SDK dispatch retained its read lock while awaiting publication");
        assert!(matches!(result.outcome(), ToolOutcome::Success));
        assert_eq!(result.model_output(), "published");
        let definitions = server.get_tool_defs(None).await.unwrap();
        assert_eq!(
            definitions
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["publish_during_call", "published_by_search"]
        );
        drop(server);
        assert!(binding.get().unwrap().upgrade().is_none());
    }

    /// Release is owned before the lock actor can start. Assertions execute
    /// under catch_unwind, then the actor is always released and joined.
    struct RegistryRelease(Option<std::sync::mpsc::Sender<()>>);
    impl RegistryRelease {
        fn release(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }
    impl Drop for RegistryRelease {
        fn drop(&mut self) {
            self.release();
        }
    }

    #[tokio::test]
    async fn cancelled_lock_wait_and_revoked_admission_publish_no_schema_or_activation() {
        use super::*;
        use futures::FutureExt;
        use std::future::Future;
        use std::task::Poll;

        for scenario in ["drop", "cancel", "source", "question", "cost", "current"] {
            let root = crate::tests::OwnedTestDirectory::new("mcp-discovery-lock");
            let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
            let states = DiscoverySessions::default();
            let activation = states
                .capture("owned-session", &workspace, "catalogue-v1".into())
                .unwrap();
            let live = Arc::new(AtomicBool::new(true));
            let guarded_live = live.clone();
            let first_check = AtomicBool::new(true);
            let (start_tx, start_rx) = std::sync::mpsc::channel();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let ready_rx = Mutex::new(ready_rx);
            let entry = DeferredEntry {
                server_name: "owned".into(),
                definition: ToolDefinition {
                    name: PublishedTool::NAME.into(),
                    description: "owned inert publication".into(),
                    parameters: serde_json::json!({"type":"object"}),
                },
                tool: Arc::new(PublishedTool),
                source: None,
                is_live: Arc::new(move || {
                    if first_check.swap(false, Ordering::AcqRel) {
                        // SDK has already dropped its dispatch read lock. Hold
                        // the write lock only now, so the real search reaches
                        // its conditional publication await before retirement.
                        start_tx.send(()).unwrap();
                        ready_rx
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(3))
                            .expect("registry write lock readiness");
                    }
                    guarded_live.load(Ordering::Acquire)
                }),
            };
            let binding = SearchBinding::default();
            binding
                .set_view(Arc::new(DiscoveryView::new(
                    activation.clone(),
                    vec![entry],
                )))
                .unwrap();
            let server = ToolServer::new().run();
            server.add_tool(McpSearch(binding.clone())).await.unwrap();
            assert!(binding.set_server(server.downgrade()).is_ok());
            let scope = crate::agent::runner::AgentWorkScope::new();
            let account = crate::spend::Account::new("discovery-cost", Some(1.0), 0.0).unwrap();
            let run = crate::spend::run::Run::fixture(
                account.clone(),
                Arc::new(crate::spend::Ledger::new(&root.path().join("state"))),
                "discovery-cost",
                crate::spend::PriceSnapshot::capture(
                    Some((1_000_000.0, 0.0)),
                    Default::default(),
                    false,
                ),
            );
            scope.run(run.prepare()).await.unwrap();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let mut release = RegistryRelease(Some(release_tx));
            let held_server = server.clone();
            let runtime = tokio::runtime::Handle::current();
            let holder = tokio::task::spawn_blocking(move || {
                start_rx
                    .recv_timeout(Duration::from_secs(3))
                    .expect("search must release its dispatch read lock");
                runtime.block_on(async move {
                    held_server
                        .append_toolset_if(rig::tool::ToolSet::default(), move || {
                            let _ = ready_tx.send(());
                            release_rx
                                .recv_timeout(Duration::from_secs(5))
                                .expect("owned registry gate must be released");
                            false
                        })
                        .await
                        .unwrap()
                })
            });
            let assertions = std::panic::AssertUnwindSafe(async {
                let extensions = ToolCallExtensions::new();
                let question = crate::question::QuestionTurn::default();
                let request = crate::question::scope(
                    Some(question.clone()),
                    crate::spend::run::scope(
                        Some(run.clone()),
                        scope.run(server.call_tool_structured(
                            "mcp_search",
                            r#"{"query":"published_by_search"}"#,
                            &extensions,
                        )),
                    ),
                );
                let mut request = Box::pin(request);
                // Poll the real SDK dispatch, not an extracted publication helper.
                // It is pending on the registry lock held by the controlled actor.
                assert!(
                    std::future::poll_fn(|cx| Poll::Ready(request.as_mut().poll(cx).is_pending()))
                        .await
                );
                match scenario {
                    "drop" => {
                        drop(request);
                        release.release();
                    }
                    "cancel" => {
                        scope.cancellation_handle().cancel();
                        release.release();
                        let result = tokio::time::timeout(Duration::from_secs(3), request)
                            .await
                            .unwrap();
                        assert!(!matches!(result.outcome(), ToolOutcome::Success));
                    }
                    "source" => {
                        live.store(false, Ordering::Release);
                        release.release();
                        let result = tokio::time::timeout(Duration::from_secs(3), request)
                            .await
                            .unwrap();
                        assert!(!matches!(result.outcome(), ToolOutcome::Success));
                    }
                    "question" => {
                        question.require(crate::question::QuestionBatch(vec![
                            crate::question::Question {
                                question: "Owned clarification parks this invocation".into(),
                                options: Vec::new(),
                                allow_free_text: true,
                            },
                        ]));
                        release.release();
                        let result = tokio::time::timeout(Duration::from_secs(3), request)
                            .await
                            .unwrap();
                        assert!(!matches!(result.outcome(), ToolOutcome::Success));
                    }
                    "cost" => {
                        // Controlled canonical accounting seam, not a claim
                        // of HTTP-observed usage. Charge and durably settle the
                        // same captured profile while publication is suspended.
                        run.observe(crate::event::UsageDelta {
                            input_tokens: 1,
                            ..Default::default()
                        });
                        scope.run(run.flush()).await;
                        assert_eq!(account.snapshot().recorded_spend(), 1.0);
                        assert!(!account.has_pending());
                        release.release();
                        let result = tokio::time::timeout(Duration::from_secs(3), request)
                            .await
                            .unwrap();
                        assert!(!matches!(result.outcome(), ToolOutcome::Success));
                    }
                    "current" => {
                        release.release();
                        let result = tokio::time::timeout(Duration::from_secs(3), request)
                            .await
                            .unwrap();
                        assert!(matches!(result.outcome(), ToolOutcome::Success));
                        let value: serde_json::Value =
                            serde_json::from_str(result.model_output()).unwrap();
                        assert_eq!(value["matches"][0]["name"], PublishedTool::NAME);
                        assert_eq!(value["permissions_changed"], false);
                    }
                    _ => unreachable!(),
                }
                let definitions = server.get_tool_defs(None).await.unwrap();
                let names: Vec<_> = definitions
                    .iter()
                    .map(|definition| definition.name.as_str())
                    .collect();
                assert_eq!(names.contains(&PublishedTool::NAME), scenario == "current");
                assert_eq!(
                    activation.is_active(PublishedTool::NAME),
                    scenario == "current"
                );
            })
            .catch_unwind()
            .await;
            release.release();
            assert!(
                !tokio::time::timeout(Duration::from_secs(6), holder)
                    .await
                    .expect("owned registry holder must join")
                    .unwrap()
            );
            scope.cancellation_handle().cancel();
            tokio::time::timeout(Duration::from_secs(3), scope.wait_idle())
                .await
                .expect("owned discovery accounting children must settle");
            if let Err(panic) = assertions {
                std::panic::resume_unwind(panic);
            }
        }
    }

    #[test]
    fn discovery_names_are_runtime_only_and_rekeyed_by_session_workspace_and_catalogue() {
        use super::*;
        let root = crate::tests::OwnedTestDirectory::new("mcp-discovery-session");
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let states = DiscoverySessions::default();
        let first = states
            .capture("session-a", &workspace, "catalogue-a".into())
            .unwrap();
        first.names.lock().unwrap().insert("mcp_owned_echo".into());
        assert!(Arc::ptr_eq(
            &first,
            &states
                .capture("session-a", &workspace, "catalogue-a".into())
                .unwrap()
        ));
        assert!(
            !states
                .capture("session-b", &workspace, "catalogue-a".into())
                .unwrap()
                .is_active("mcp_owned_echo")
        );
        let changed = states
            .capture("session-a", &workspace, "catalogue-b".into())
            .unwrap();
        assert!(!first.is_current());
        assert!(!changed.is_active("mcp_owned_echo"));
        changed
            .names
            .lock()
            .unwrap()
            .insert("mcp_owned_echo".into());
        let rebound = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let new_binding = states
            .capture("session-a", &rebound, "catalogue-b".into())
            .unwrap();
        assert!(!changed.is_current());
        assert!(!new_binding.is_active("mcp_owned_echo"));
        assert!(
            !DiscoverySessions::default()
                .capture("session-a", &rebound, "catalogue-b".into())
                .unwrap()
                .is_active("mcp_owned_echo")
        );
    }

    #[test]
    fn sealed_builtin_fingerprint_stays_private_stable_and_distinct_from_authored_url() {
        use crate::extras::mcp::config::{McpServerConfig, TrustedMcpServer};
        use crate::extras::mcp::project::ServerSourceProof;
        let root = crate::tests::OwnedTestDirectory::new("mcp-built-in-fingerprint");
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let fingerprint = |server| {
            ServerSourceProof::capture(workspace.clone(), "owned".into(), Some(server), None, false)
                .fingerprint()
        };
        let builtin = |headers| McpServerConfig::BuiltIn {
            identity: TrustedMcpServer::EXA,
            headers,
        };
        let first = builtin(std::collections::HashMap::from([
            ("one".into(), "private-one".into()),
            ("two".into(), "private-two".into()),
        ]));
        assert!(
            serde_json::to_value(&first).is_err(),
            "built-in authority must remain nonserializable"
        );
        let first = fingerprint(first);
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        let reordered = fingerprint(builtin(std::collections::HashMap::from([
            ("two".into(), "private-two".into()),
            ("one".into(), "private-one".into()),
        ])));
        assert_eq!(first, reordered);
        assert_ne!(
            first,
            fingerprint(builtin(std::collections::HashMap::from([
                ("one".into(), "changed".into()),
                ("two".into(), "private-two".into())
            ])))
        );
        let authored: McpServerConfig = serde_json::from_value(serde_json::json!({"url":TrustedMcpServer::EXA.endpoint(),"headers":{"one":"private-one","two":"private-two"}})).unwrap();
        assert_ne!(first, fingerprint(authored));
    }
    struct MetadataOnlyTool(rig::completion::ToolDefinition);
    impl rig::tool::ToolDyn for MetadataOnlyTool {
        fn name(&self) -> String {
            self.0.name.clone()
        }
        fn description(&self) -> String {
            self.0.description.clone()
        }
        fn parameters(&self) -> serde_json::Value {
            self.0.parameters.clone()
        }
        fn call<'a>(
            &'a self,
            _: String,
        ) -> rig::wasm_compat::WasmBoxedFuture<'a, Result<String, rig::tool::ToolError>> {
            Box::pin(async { panic!("metadata discovery must never invoke a candidate tool") })
        }
    }

    #[tokio::test]
    async fn actual_search_bounds_queries_result_bytes_and_exact_name_ranking_without_candidate_calls()
     {
        use super::*;
        let root = crate::tests::OwnedTestDirectory::new("mcp-search-bounds");
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let activation = DiscoverySessions::default()
            .capture("bounds-session", &workspace, "bounds-catalogue".into())
            .unwrap();
        let entries = (0..64).map(|index| {
            let definition = ToolDefinition {
                name: format!("owned_{index:03}"),
                description: "common catalogue ".repeat(200),
                parameters: serde_json::json!({"type":"object","properties":{"value":{"type":"string","description":"bounded schema ".repeat(1000)}}}),
            };
            DeferredEntry {server_name:"owned".into(),tool:Arc::new(MetadataOnlyTool(definition.clone())),definition,source:None,is_live:Arc::new(||true)}
        }).collect();
        let view = Arc::new(DiscoveryView::new(activation.clone(), entries));
        let index = view.compact_index();
        assert!(index.len() <= 64 * 1024);
        assert!(index.contains("owned_000") && index.contains("owned_063"));
        assert!(!index.contains("common catalogue") && !index.contains("bounded schema"));
        let binding = SearchBinding::default();
        binding.set_view(view).unwrap();
        let server = ToolServer::new().run();
        server.add_tool(McpSearch(binding.clone())).await.unwrap();
        binding.set_server(server.downgrade()).unwrap();
        let scope = crate::agent::runner::AgentWorkScope::new();
        let extensions = ToolCallExtensions::new();
        for query in [
            String::new(),
            "x".repeat(2049),
            "\u{001b}".into(),
            "word ".repeat(33),
        ] {
            let result = scope
                .run(server.call_tool_structured(
                    "mcp_search",
                    &serde_json::json!({"query":query}).to_string(),
                    &extensions,
                ))
                .await;
            assert!(!matches!(result.outcome(), ToolOutcome::Success));
            assert_eq!(server.get_tool_defs(None).await.unwrap().len(), 1);
            assert!(activation.names.lock().unwrap().is_empty());
        }
        let result = scope
            .run(server.call_tool_structured("mcp_search", r#"{"query":"owned_042"}"#, &extensions))
            .await;
        assert!(matches!(result.outcome(), ToolOutcome::Success));
        let exact: serde_json::Value = serde_json::from_str(result.model_output()).unwrap();
        assert_eq!(exact["matches"].as_array().unwrap().len(), 1);
        assert_eq!(exact["matches"][0]["name"], "owned_042");
        assert!(activation.is_active("owned_042"));
        let result = scope
            .run(server.call_tool_structured(
                "mcp_search",
                r#"{"query":"common catalogue"}"#,
                &extensions,
            ))
            .await;
        assert!(matches!(result.outcome(), ToolOutcome::Success));
        assert!(result.model_output().len() <= 128 * 1024);
        let ranked: serde_json::Value = serde_json::from_str(result.model_output()).unwrap();
        let matched = ranked["matches"].as_array().unwrap();
        assert!(
            !matched.is_empty() && matched.len() < 8,
            "byte bound must limit these large schemas before the eight-result bound"
        );
        assert_eq!(matched[0]["name"], "owned_000");
        assert_eq!(ranked["omitted"], true);
        assert_eq!(ranked["permissions_changed"], false);
        for (position, definition) in matched.iter().enumerate() {
            assert_eq!(definition["name"], format!("owned_{position:03}"));
            assert!(activation.is_active(definition["name"].as_str().unwrap()));
        }
        assert_eq!(
            server.get_tool_defs(None).await.unwrap().len(),
            matched.len() + 2,
            "only returned names plus the earlier exact result may be published"
        );
        assert!(!activation.is_active("owned_063"));
        scope.cancellation_handle().cancel();
        scope.wait_idle().await;
    }
    #[tokio::test]
    async fn actual_search_reserves_full_result_envelope_before_atomic_publication() {
        use super::*;
        let root = crate::tests::OwnedTestDirectory::new("mcp-search-envelope");
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let activation = DiscoverySessions::default()
            .capture("envelope-session", &workspace, "envelope-catalogue".into())
            .unwrap();
        let entries = (0..8).map(|index| {
            let mut definition = ToolDefinition {name:format!("owned_{index:03}"),description:"common".into(),parameters:serde_json::json!({"type":"object","properties":{"value":{"type":"string","description":""}}})};
            let base = serde_json::to_vec(&definition).unwrap().len();
            definition.parameters["properties"]["value"]["description"]=serde_json::Value::String("x".repeat(16*1024-base));
            assert_eq!(serde_json::to_vec(&definition).unwrap().len(),16*1024);
            assert!(serde_json::to_vec(&definition.parameters).unwrap().len()<=16*1024);
            DeferredEntry {server_name:"owned".into(),tool:Arc::new(MetadataOnlyTool(definition.clone())),definition,source:None,is_live:Arc::new(||true)}
        }).collect();
        let binding = SearchBinding::default();
        binding
            .set_view(Arc::new(DiscoveryView::new(activation.clone(), entries)))
            .unwrap();
        let server = ToolServer::new().run();
        server.add_tool(McpSearch(binding.clone())).await.unwrap();
        binding.set_server(server.downgrade()).unwrap();
        let scope = crate::agent::runner::AgentWorkScope::new();
        let result = scope
            .run(server.call_tool_structured(
                "mcp_search",
                r#"{"query":"common"}"#,
                &ToolCallExtensions::new(),
            ))
            .await;
        assert!(matches!(result.outcome(), ToolOutcome::Success));
        assert!(result.model_output().len() <= 128 * 1024);
        let body: serde_json::Value = serde_json::from_str(result.model_output()).unwrap();
        assert_eq!(
            body["matches"].as_array().unwrap().len(),
            7,
            "eight definitions consume the full budget without room for the result envelope"
        );
        assert_eq!(body["omitted"], true);
        assert_eq!(server.get_tool_defs(None).await.unwrap().len(), 8);
        assert!(activation.is_active("owned_006"));
        assert!(!activation.is_active("owned_007"));
        scope.cancellation_handle().cancel();
        scope.wait_idle().await;
    }
}

#[cfg(test)]
mod native_reference_bounds_tests {
    use super::*;
    #[tokio::test]
    async fn native_reference_name_bounds_refuse_before_schema_publication() {
        let root = crate::tests::OwnedTestDirectory::new("mcp-native-reference-bounds");
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let activation = DiscoverySessions::default()
            .capture("native-bounds", &workspace, "native-bounds".into())
            .unwrap();
        let definition = ToolDefinition {
            name: "x".repeat(129),
            description: "match".into(),
            parameters: serde_json::json!({}),
        };
        let view = Arc::new(DiscoveryView::new(
            activation.clone(),
            vec![DeferredEntry {
                server_name: "owned".into(),
                definition,
                tool: Arc::new(McpSearch(SearchBinding::default())),
                source: None,
                is_live: Arc::new(|| true),
            }],
        ));
        let binding = SearchBinding::default().with_native_references(true);
        binding.set_view(view).unwrap();
        let server = rig::tool::server::ToolServer::new().run();
        server.add_tool(McpSearch(binding.clone())).await.unwrap();
        binding.set_server(server.downgrade()).unwrap();
        let scope = crate::agent::runner::AgentWorkScope::new();
        let extensions = rig::tool::ToolCallExtensions::new();
        let result = scope
            .run(server.call_tool_structured("mcp_search", r#"{"query":"match"}"#, &extensions))
            .await;
        assert!(result.outcome().is_error());
        assert!(result.model_output().contains("name exceeds its bounds"));
        assert!(
            result
                .extensions()
                .get::<rig::tool::ToolReferences>()
                .is_none()
        );
        assert!(activation.names.lock().unwrap().is_empty());
        assert_eq!(server.get_tool_defs(None).await.unwrap().len(), 1);
        scope.wait_idle().await;
    }
}
