use rig::tool::{ToolDyn, ToolError};
use rig::wasm_compat::WasmBoxedFuture;

use crate::agent::builder::filter_tools_by_allowlist;

struct NamedTool(&'static str);

impl ToolDyn for NamedTool {
    fn name(&self) -> String {
        self.0.to_string()
    }

    fn description(&self) -> String {
        String::new()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    fn call<'a>(&'a self, _args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        Box::pin(async { Ok(String::new()) })
    }
}

fn make_tools(names: &[&'static str]) -> Vec<Box<dyn ToolDyn>> {
    names
        .iter()
        .map(|n| Box::new(NamedTool(n)) as Box<dyn ToolDyn>)
        .collect()
}

fn tool_names(tools: &[Box<dyn ToolDyn>]) -> Vec<String> {
    tools.iter().map(|t| t.name()).collect()
}

#[test]
fn empty_allowlist_passes_all_tools_through() {
    let tools = make_tools(&["read", "write", "bash"]);
    let filtered = filter_tools_by_allowlist(tools, &[]);
    assert_eq!(tool_names(&filtered), vec!["read", "write", "bash"]);
}

#[test]
fn allowlist_retains_only_matching_tools() {
    let tools = make_tools(&["read", "write", "bash", "grep"]);
    let allowlist = vec!["read".to_string(), "grep".to_string()];
    let filtered = filter_tools_by_allowlist(tools, &allowlist);
    assert_eq!(tool_names(&filtered), vec!["read", "grep"]);
}

#[test]
fn unknown_names_in_allowlist_are_ignored() {
    let tools = make_tools(&["read", "write"]);
    let allowlist = vec!["read".to_string(), "bogus".to_string()];
    let filtered = filter_tools_by_allowlist(tools, &allowlist);
    assert_eq!(tool_names(&filtered), vec!["read"]);
}

#[test]
fn allowlist_with_no_matches_returns_empty() {
    let tools = make_tools(&["read", "write"]);
    let allowlist = vec!["bash".to_string(), "grep".to_string()];
    let filtered = filter_tools_by_allowlist(tools, &allowlist);
    assert!(filtered.is_empty());
}

#[test]
fn single_tool_allowlist() {
    let tools = make_tools(&["read", "write", "bash", "job_status"]);
    let allowlist = vec!["bash".to_string()];
    let filtered = filter_tools_by_allowlist(tools, &allowlist);
    assert_eq!(tool_names(&filtered), vec!["bash", "job_status"]);
}

#[test]
fn task_allowlist_keeps_background_task_companions() {
    let tools = make_tools(&[
        "task",
        "task_result",
        "task_list",
        "task_cancel",
        "task_send",
        "read",
        "write",
    ]);
    let allowlist = vec!["read".to_string(), "task".to_string()];
    let filtered = filter_tools_by_allowlist(tools, &allowlist);
    assert_eq!(
        tool_names(&filtered),
        vec![
            "task",
            "task_result",
            "task_list",
            "task_cancel",
            "task_send",
            "read"
        ]
    );
}

#[test]
fn task_companions_are_dropped_without_task_allowed() {
    let tools = make_tools(&["task", "task_result", "read"]);
    let allowlist = vec!["read".to_string()];
    let filtered = filter_tools_by_allowlist(tools, &allowlist);
    assert_eq!(tool_names(&filtered), vec!["read"]);
}
