use compact_str::CompactString;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

tokio::task_local! {
    static TOOL_CALL_CONTEXT: std::cell::RefCell<std::collections::HashMap<String, std::collections::VecDeque<String>>>;
}

pub(crate) async fn scope_tool_call_context<F: std::future::Future>(future: F) -> F::Output {
    TOOL_CALL_CONTEXT
        .scope(
            std::cell::RefCell::new(std::collections::HashMap::new()),
            future,
        )
        .await
}

pub(crate) fn record_tool_call(tool: &str, id: &str) {
    let _ = TOOL_CALL_CONTEXT.try_with(|context| {
        context
            .borrow_mut()
            .entry(tool.to_string())
            .or_default()
            .push_back(id.to_string());
    });
}

pub(crate) fn finish_tool_call(tool: &str, id: &str) {
    let _ = TOOL_CALL_CONTEXT.try_with(|context| {
        let mut context = context.borrow_mut();
        if let Some(ids) = context.get_mut(tool)
            && let Some(index) = ids.iter().position(|candidate| candidate == id)
        {
            ids.remove(index);
        }
    });
}

pub(crate) fn take_tool_call_id(tool: &str) -> Option<String> {
    TOOL_CALL_CONTEXT
        .try_with(|context| {
            context
                .borrow_mut()
                .get_mut(tool)
                .and_then(std::collections::VecDeque::pop_front)
        })
        .ok()
        .flatten()
}

pub type AskSender = mpsc::Sender<AskRequest>;
pub type AskReceiver = mpsc::Receiver<AskRequest>;

#[derive(Debug)]
pub struct AskRequest {
    pub tool: CompactString,
    pub input: String,
    /// Internal call identity emitted to frontends for this exact tool call.
    pub tool_call_id: Option<String>,
    /// Optional caller-supplied AllowAlways scope when the operation knows a
    /// safer boundary than the generic UI heuristic.
    pub suggested_pattern: Option<String>,
    /// Additional scopes persisted with AllowAlways. This lets a project-tree
    /// grant cover both the exact root and its descendants without widening to
    /// the parent directory.
    pub additional_allow_patterns: Vec<String>,
    pub reply: oneshot::Sender<UserDecision>,
}

#[derive(Debug, Clone)]
pub enum UserDecision {
    AllowOnce,
    AllowAlways(String),
    Deny,
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn tool_call_context_correlates_parallel_tools_by_name() {
        super::scope_tool_call_context(async {
            super::record_tool_call("read", "read-1");
            super::record_tool_call("shell", "shell-1");
            super::record_tool_call("read", "read-2");

            assert_eq!(super::take_tool_call_id("read").as_deref(), Some("read-1"));
            super::finish_tool_call("shell", "shell-1");
            assert_eq!(super::take_tool_call_id("shell"), None);
            assert_eq!(super::take_tool_call_id("read").as_deref(), Some("read-2"));
        })
        .await;
    }
}
