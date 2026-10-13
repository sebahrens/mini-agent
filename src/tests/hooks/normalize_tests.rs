use crate::extras::hooks::normalize::canonical_tool_name;

#[test]
fn bash_normalizes_to_bash() {
    assert_eq!(canonical_tool_name("Bash"), "bash");
}

#[test]
fn bash_is_idempotent() {
    assert_eq!(canonical_tool_name("bash"), "bash");
}

#[test]
fn glob_normalizes_to_find_files() {
    assert_eq!(canonical_tool_name("Glob"), "find_files");
}

#[test]
fn find_files_is_idempotent() {
    assert_eq!(canonical_tool_name("find_files"), "find_files");
}

#[test]
fn task_normalizes_to_task() {
    assert_eq!(canonical_tool_name("Task"), "task");
}

#[test]
fn todo_write_normalizes_to_todo_write() {
    assert_eq!(canonical_tool_name("TodoWrite"), "todo_write");
}

#[test]
fn web_fetch_normalizes_to_web_fetch() {
    assert_eq!(canonical_tool_name("WebFetch"), "web_fetch");
}

#[test]
fn web_search_normalizes_to_web_search() {
    assert_eq!(canonical_tool_name("WebSearch"), "web_search");
}

#[test]
fn mcp_matcher_names_preserve_raw_case_and_component_identity() {
    use crate::extras::hooks::normalize::McpToolIdentity;
    let identity = McpToolIdentity {
        server: "Alpha.Space".into(),
        tool: "SearchValue".into(),
    };
    assert_eq!(identity.matcher_name(), "mcp__Alpha.Space__SearchValue");
    assert_eq!(
        canonical_tool_name(&identity.matcher_name()),
        identity.matcher_name()
    );
    assert_eq!(
        identity.permission_input(),
        "mcp_tool:Alpha.Space:SearchValue"
    );
    let first = McpToolIdentity {
        server: "a__b".into(),
        tool: "c".into(),
    };
    let second = McpToolIdentity {
        server: "a".into(),
        tool: "b__c".into(),
    };
    assert_eq!(first.matcher_name(), second.matcher_name());
    assert_ne!(
        first, second,
        "a compatibility label is never a provenance tuple"
    );
}
