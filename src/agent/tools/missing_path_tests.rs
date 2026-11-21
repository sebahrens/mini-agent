//! mini-agent-sskdu.84: a missing path or a directory given to a file tool
//! names the tool and the path and says what to do next, instead of a bare
//! `No such file or directory (os error 2)`.

use rig::tool::Tool;

use super::{EditTool, FindFilesTool, GrepTool, ListDirTool, ReadTool, ReadTracker};

fn args<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
    serde_json::from_value(value).unwrap()
}

fn assert_named(error: &str, tool: &str, path: &str, needle: &str) {
    assert!(error.starts_with(&format!("{tool}: '")), "{error}");
    assert!(error.contains(path), "path missing: {error}");
    assert!(error.contains(needle), "{needle} missing: {error}");
    assert!(!error.contains("os error"), "raw errno leaked: {error}");
}

#[tokio::test]
async fn missing_paths_name_the_tool_and_path_for_every_path_tool() {
    let base = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("mini-agent-missing-path-{}", uuid::Uuid::new_v4()));
    let workspace = base.join("workspace");
    let outside = base.join("outside");
    std::fs::create_dir_all(workspace.join("adir")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let absolute = outside.join("nope.rs").to_string_lossy().into_owned();

    for path in ["src/nope.rs", absolute.as_str()] {
        let read = ReadTool::new_with_tracker(None, None, None, 100, ReadTracker::new(true))
            .with_workspace(workspace.clone());
        let error = read
            .call(args(serde_json::json!({ "path": path })))
            .await
            .unwrap_err()
            .to_string();
        assert_named(&error, "read", "nope.rs", "does not exist");
        assert!(error.contains("find_files or list_dir"), "{error}");

        let edit = EditTool::new(None, None).with_workspace(workspace.clone());
        let error = edit
            .call(args(serde_json::json!({
                "path": path,
                "block": "<<<<<<< SEARCH\na\n=======\nb\n>>>>>>> REPLACE"
            })))
            .await
            .unwrap_err()
            .to_string();
        assert_named(&error, "edit", "nope.rs", "does not exist");
        assert!(error.contains("write to create a new file"), "{error}");

        let grep = GrepTool::new(None, None, 100).with_workspace(workspace.clone());
        let error = grep
            .call(args(serde_json::json!({ "pattern": "x", "path": path })))
            .await
            .unwrap_err()
            .to_string();
        assert_named(&error, "grep", "nope.rs", "does not exist");

        let find = FindFilesTool::new(None, None, 100).with_workspace(workspace.clone());
        let error = find
            .call(args(serde_json::json!({ "pattern": "*.rs", "path": path })))
            .await
            .unwrap_err()
            .to_string();
        assert_named(&error, "find_files", "nope.rs", "does not exist");

        let list = ListDirTool::new(None, None, None).with_workspace(workspace.clone());
        let error = list
            .call(args(serde_json::json!({ "path": path })))
            .await
            .unwrap_err()
            .to_string();
        assert_named(&error, "list_dir", "nope.rs", "does not exist");
    }

    let read = ReadTool::new_with_tracker(None, None, None, 100, ReadTracker::new(true))
        .with_workspace(workspace.clone());
    for path in [
        "adir".to_string(),
        workspace.join("adir").to_string_lossy().into_owned(),
    ] {
        let error = read
            .call(args(serde_json::json!({ "path": path })))
            .await
            .unwrap_err()
            .to_string();
        assert_named(&error, "read", "adir", "is a directory");
        assert!(error.contains("list_dir"), "{error}");
    }
    let _ = std::fs::remove_dir_all(&base);
}

// mini-agent-sskdu.97: the three search tools reject a per-call cap of 0, serve
// one above the configured cap at the cap with a note, and grep keeps one line shape.
#[tokio::test]
async fn search_tools_share_one_cap_policy_and_line_shape() {
    let base = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("mini-agent-cap-policy-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("f.txt"), "one\nneedle\nthree\n").unwrap();
    let zero = [
        (
            "grep",
            GrepTool::new(None, None, 5)
                .with_workspace(base.clone())
                .call(args(serde_json::json!({"pattern": "x", "max_results": 0})))
                .await,
        ),
        (
            "find_files",
            FindFilesTool::new(None, None, 5)
                .with_workspace(base.clone())
                .call(args(serde_json::json!({"pattern": "*", "limit": 0})))
                .await,
        ),
        (
            "list_dir",
            ListDirTool::new(None, None, Some(5))
                .with_workspace(base.clone())
                .call(args(serde_json::json!({"limit": 0})))
                .await,
        ),
    ];
    for (tool, result) in zero {
        let error = result.unwrap_err().to_string();
        assert!(
            error.starts_with(&format!("{tool} ")) && error.contains("at least 1"),
            "{error}"
        );
        assert!(error.contains("Example:"), "{error}");
    }
    let over = [
        GrepTool::new(None, None, 5)
            .with_workspace(base.clone())
            .call(args(
                serde_json::json!({"pattern": "needle", "max_results": 500}),
            ))
            .await
            .unwrap(),
        FindFilesTool::new(None, None, 5)
            .with_workspace(base.clone())
            .call(args(serde_json::json!({"pattern": "*", "limit": 500})))
            .await
            .unwrap(),
        ListDirTool::new(None, None, Some(5))
            .with_workspace(base.clone())
            .call(args(serde_json::json!({"limit": 500})))
            .await
            .unwrap(),
    ];
    for result in over {
        assert!(
            result.contains("500 exceeds the cap 5; showing up to 5"),
            "{result}"
        );
    }
    let result = GrepTool::new(None, None, 5)
        .with_workspace(base.clone())
        .call(args(
            serde_json::json!({"pattern": "needle", "context_lines": 1}),
        ))
        .await
        .unwrap();
    assert!(result.contains("f.txt:1-one"), "{result}");
    assert!(result.contains("f.txt:2:needle"), "{result}");
    let _ = std::fs::remove_dir_all(&base);
}

// mini-agent-sskdu.168: no built-in tool schema may carry a top-level
// combinator (Anthropic rejects them; other gateways ignore or rewrite them).
#[test]
fn built_in_tool_schemas_have_no_top_level_combinators() {
    use super::ask_user::AskUserTool;
    let schemas = [
        (
            "ask_user_question",
            AskUserTool::new(None, crate::config::HeadlessQuestions::Error).parameters(),
        ),
        ("read", ReadTool::new(None, None, None, 100).parameters()),
        ("edit", EditTool::new(None, None).parameters()),
        ("grep", GrepTool::new(None, None, 5).parameters()),
        ("find_files", FindFilesTool::new(None, None, 5).parameters()),
        ("list_dir", ListDirTool::new(None, None, None).parameters()),
    ];
    for (name, schema) in schemas {
        for combinator in ["oneOf", "anyOf", "allOf", "not", "enum"] {
            assert!(
                schema.get(combinator).is_none(),
                "{name} has a top-level {combinator}"
            );
        }
    }
}

// mini-agent-sskdu.177: descriptions follow the configured skip list.
#[test]
fn skip_dir_phrase_follows_the_configured_list() {
    let custom = super::skip_dirs_phrase_for(Some(&["vendor".to_string(), "dist".to_string()]));
    assert!(custom.contains("vendor, dist"), "{custom}");
    assert!(!custom.contains("node_modules"), "{custom}");
    let default = super::skip_dirs_phrase_for(None);
    assert!(default.contains("node_modules"), "{default}");
}
