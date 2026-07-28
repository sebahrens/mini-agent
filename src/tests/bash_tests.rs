use std::sync::{Arc, Mutex};

use rig::tool::Tool;

use crate::agent::tools::{BashArgs, BashTool};
use crate::permission::checker::PermissionChecker;
use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};
use crate::sandbox::Sandbox;

fn bash_tool_with_rules(rules: impl IntoIterator<Item = (String, Action)>) -> BashTool {
    let config = PermissionConfig {
        bash: Some(ToolPerm::Granular(rules.into_iter().collect())),
        ..PermissionConfig::default()
    };
    let permission = PermissionChecker::new(
        &PermissionConfigs::from(config),
        SecurityMode::Standard,
        None,
        Some(vec!["standard".to_string()]),
    );
    BashTool::new(
        Some(Arc::new(Mutex::new(permission))),
        None,
        Sandbox::new(false, "bwrap"),
        None,
    )
}

fn shell_quote(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[tokio::test]
async fn bash_compound_command_permission_denies_before_starting_any_child() {
    let marker = std::env::temp_dir().join(format!(
        "zerostack-bash-permission-sentinel-{}",
        std::process::id()
    ));
    let quoted_marker = shell_quote(&marker);
    let cases = [
        format!("echo \"$(printf leaked > {quoted_marker})\""),
        format!("echo `printf leaked > {quoted_marker}`"),
        format!("cat <<'EOF' > {quoted_marker}\nleaked\nEOF"),
        format!("cat <<< leaked > {quoted_marker}"),
        format!("cat <(printf leaked > {quoted_marker})"),
        format!("printf leaked > >(tee {quoted_marker})"),
        format!("echo leaked | tee {quoted_marker}"),
        format!("echo leaked > {quoted_marker}"),
        format!("echo safe; printf leaked > {quoted_marker}"),
        format!("(printf leaked > {quoted_marker})"),
        format!("printf leaked > {quoted_marker} & wait"),
    ];

    for command in cases {
        let _ = std::fs::remove_file(&marker);
        let tool = bash_tool_with_rules([("*".to_string(), Action::Allow)]);
        let result = tool
            .call(BashArgs {
                command: command.clone(),
                timeout: None,
            })
            .await;

        assert!(
            result.is_err(),
            "compound script must not be authorized by a broad allow rule: {command:?}"
        );
        assert!(
            !marker.exists(),
            "permission denial must happen before any child creates the sentinel: {command:?}"
        );
    }
}

#[tokio::test]
async fn bash_compound_command_permission_rejects_malformed_script_before_launch() {
    let tool = bash_tool_with_rules([("*".to_string(), Action::Allow)]);
    let result = tool
        .call(BashArgs {
            command: "echo $(".to_string(),
            timeout: None,
        })
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn bash_compound_command_permission_allows_exact_complete_script() {
    let command = "printf exact-script";
    let tool = bash_tool_with_rules([(command.to_string(), Action::Allow)]);
    let output = tool
        .call(BashArgs {
            command: command.to_string(),
            timeout: None,
        })
        .await
        .unwrap();

    assert_eq!(output, "exact-script");
}
