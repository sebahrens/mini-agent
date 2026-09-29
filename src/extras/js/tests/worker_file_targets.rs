//! Contained-worker regressions for mini-agent-c1o7m: JS file effects decide
//! bound-vs-ambient from the home-expanded path, and JS discovery hides the
//! entries that `read` path deny rules deny. Each test drives the model-facing
//! `js` tool, so the script runs in the internal worker process and every
//! effect crosses the parent broker.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rig::tool::Tool;

use super::{TestTempDir, make_test_tool_in_workspace};
use crate::extras::js::tool::JsArgs;
use crate::permission::checker::{PermCheck, PermissionChecker};
use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};

fn checker(workspace: &Path, config: PermissionConfig) -> PermCheck {
    Arc::new(Mutex::new(
        PermissionChecker::new(
            &PermissionConfigs::from(config),
            SecurityMode::Standard,
            Some(workspace.to_path_buf()),
            Some(vec!["standard".to_string()]),
        )
        .expect("valid permission test configuration"),
    ))
}

#[tokio::test]
async fn contained_worker_home_paths_resolve_from_the_expanded_path() {
    let temp = TestTempDir::new("js-home-targets");
    let base = temp.path().canonicalize().unwrap();
    let workspace_root = base.join("workspace");
    let home = base.join("home");
    std::fs::create_dir_all(&workspace_root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("in.txt"), "from home").unwrap();
    // The parent broker expands `~`/`$HOME` on this (current-thread) runtime.
    let _home = crate::paths::ScopedTestHome::set(&home);
    let permission = checker(
        &workspace_root,
        PermissionConfig {
            external_directory: Some(
                [
                    (home.display().to_string(), Action::Allow),
                    (format!("{}/**", home.display()), Action::Allow),
                ]
                .into(),
            ),
            doom_loop: Some(Action::Allow),
            ..PermissionConfig::default()
        },
    );
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(&workspace_root).unwrap());
    let tool = make_test_tool_in_workspace(workspace, permission, &base.join("audit"));

    let result = tool
        .call(JsArgs {
            code: "write_file('$HOME/out.txt', 'to home'); \
                   JSON.stringify({ \
                     read: read_file('$HOME/in.txt'), \
                     tilde: read_file('~/in.txt'), \
                     listed: list_dir('$HOME').entries.map(e => e.name) \
                   })"
            .to_string(),
        })
        .await
        .expect("js call");
    assert_eq!(
        result,
        r#"{"read":"from home","tilde":"from home","listed":["in.txt","out.txt"]}"#
    );
    assert_eq!(
        std::fs::read_to_string(home.join("out.txt")).unwrap(),
        "to home"
    );
    assert!(
        !workspace_root.join("$HOME").exists(),
        "a literal $HOME directory must never be created inside the workspace"
    );
}

#[tokio::test]
async fn contained_worker_discovery_omits_read_denied_entries() {
    let temp = TestTempDir::new("js-discovery-deny");
    let base = temp.path().canonicalize().unwrap();
    let workspace_root = base.join("workspace");
    std::fs::create_dir_all(workspace_root.join("config/secrets")).unwrap();
    std::fs::write(
        workspace_root.join("config/secrets/key.txt"),
        "needle key\n",
    )
    .unwrap();
    std::fs::write(workspace_root.join("config/token.txt"), "needle token\n").unwrap();
    std::fs::write(workspace_root.join("config/app.txt"), "needle app\n").unwrap();
    let permission = checker(
        &workspace_root,
        PermissionConfig {
            read: Some(ToolPerm::Granular(
                [
                    ("config/secrets/**".to_string(), Action::Deny),
                    ("config/token.txt".to_string(), Action::Deny),
                ]
                .into(),
            )),
            doom_loop: Some(Action::Allow),
            ..PermissionConfig::default()
        },
    );
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(&workspace_root).unwrap());
    let tool = make_test_tool_in_workspace(workspace, permission, &base.join("audit"));

    let result = tool
        .call(JsArgs {
            code: "JSON.stringify({ \
                     listed: list_dir('config').entries.map(e => e.name).sort(), \
                     globbed: glob('**/*.txt').paths, \
                     grepped: grep('needle').matches.map(m => m.path) \
                   })"
            .to_string(),
        })
        .await
        .expect("js call");
    let value: serde_json::Value = serde_json::from_str(&result).expect(&result);
    let listed = value["listed"].as_array().expect(&result);
    assert!(listed.iter().any(|name| name == "app.txt"), "{result}");
    assert!(!listed.iter().any(|name| name == "token.txt"), "{result}");
    assert_eq!(
        value["globbed"],
        serde_json::json!(["config/app.txt"]),
        "{result}"
    );
    assert_eq!(
        value["grepped"],
        serde_json::json!(["config/app.txt"]),
        "{result}"
    );

    // Reading a denied entry directly stays denied as well.
    let denied = tool
        .call(JsArgs {
            code: "let outcome; try { read_file('config/secrets/key.txt'); outcome = 'read'; } \
                   catch (_) { outcome = 'denied'; } outcome"
                .to_string(),
        })
        .await
        .expect("js call");
    assert_eq!(denied, "denied");
}
