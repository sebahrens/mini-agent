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
    )
    .expect("valid permission test configuration");
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

fn bash_tool_with_line_cap(max_output_lines: Option<u64>) -> BashTool {
    BashTool::new(None, None, Sandbox::new(false, "bwrap"), max_output_lines)
}

#[test]
fn bash_output_line_cap_defaults_to_2000_and_honours_overrides() {
    use crate::config::{Config, DEFAULT_MAX_BASH_OUTPUT_LINES};

    assert_eq!(DEFAULT_MAX_BASH_OUTPUT_LINES, 2000);
    assert_eq!(
        Config::default().resolve_max_bash_output_lines(),
        Some(DEFAULT_MAX_BASH_OUTPUT_LINES)
    );
    let explicit = Config {
        max_bash_output_lines: Some(50),
        ..Config::default()
    };
    assert_eq!(explicit.resolve_max_bash_output_lines(), Some(50));
    let disabled = Config {
        max_bash_output_lines: Some(0),
        ..Config::default()
    };
    assert_eq!(disabled.resolve_max_bash_output_lines(), None);
}

#[tokio::test]
async fn bash_success_output_is_bounded_to_head_and_tail_with_omitted_marker() {
    let tool = bash_tool_with_line_cap(Some(20));
    let output = tool
        .call(BashArgs {
            command: "seq 1 100".to_string(),
            timeout: None,
        })
        .await
        .unwrap();

    let lines: Vec<&str> = output.lines().collect();
    assert!(
        lines.len() <= 20 + 4,
        "expected at most 24 lines (20 kept plus marker), got {}:\n{output}",
        lines.len()
    );
    assert!(output.starts_with("1\n2\n3\n"), "head missing:\n{output}");
    assert!(output.ends_with("99\n100"), "tail missing:\n{output}");
    assert!(
        output.contains("lines omitted"),
        "omitted-count marker missing:\n{output}"
    );
    assert!(
        !output.contains("\n50\n"),
        "middle of the output should be elided:\n{output}"
    );
}

#[tokio::test]
async fn bash_output_within_line_cap_is_returned_verbatim() {
    let tool = bash_tool_with_line_cap(Some(20));
    let output = tool
        .call(BashArgs {
            command: "seq 1 5".to_string(),
            timeout: None,
        })
        .await
        .unwrap();
    assert_eq!(output, "1\n2\n3\n4\n5\n");
}

#[tokio::test]
async fn bash_resource_limit_error_bounds_partial_output_to_line_cap() {
    let tool = bash_tool_with_line_cap(Some(20));
    // 2,000,000 two-byte lines exceed the 1 MiB stdout cap, so the command is
    // stopped with a partial prefix that must still be line-bounded.
    let error = tool
        .call(BashArgs {
            command: "yes | head -n 2000000".to_string(),
            timeout: None,
        })
        .await
        .expect_err("output limit must surface as a tool error");
    let message = error.to_string();
    assert!(
        message.contains("[status: output_truncated"),
        "unexpected error: {}",
        &message[..message.len().min(300)]
    );
    assert!(
        message.lines().count() <= 20 + 6,
        "partial output was not line-bounded: {} lines",
        message.lines().count()
    );
    assert!(
        message.contains("lines omitted"),
        "omitted-count marker missing"
    );
}

#[tokio::test]
async fn bash_without_line_cap_returns_all_lines() {
    let tool = bash_tool_with_line_cap(None);
    let output = tool
        .call(BashArgs {
            command: "seq 1 100".to_string(),
            timeout: None,
        })
        .await
        .unwrap();
    assert_eq!(output.lines().count(), 100);
    assert!(!output.contains("omitted"));
}
