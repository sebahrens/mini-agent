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
        "mini-agent-bash-permission-sentinel-{}",
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
        for background in [false, true] {
            let _ = std::fs::remove_file(&marker);
            let tool = bash_tool_with_rules([("*".to_string(), Action::Allow)]);
            let result = tool
                .call(BashArgs {
                    command: command.clone(),
                    timeout: None,
                    background,
                })
                .await;

            assert!(
                result.is_err(),
                "compound script must not be authorized by a broad allow rule: {command:?}"
            );
            assert_eq!(tool.sandbox.running_background_job_count(), 0);
            assert!(
                !marker.exists(),
                "permission denial must happen before any child creates the sentinel: {command:?}"
            );
        }
    }
}

#[tokio::test]
async fn bash_compound_command_permission_rejects_malformed_script_before_launch() {
    let tool = bash_tool_with_rules([("*".to_string(), Action::Allow)]);
    let result = tool
        .call(BashArgs {
            command: "echo $(".to_string(),
            timeout: None,
            background: false,
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
            background: false,
        })
        .await
        .unwrap();

    assert_eq!(output, "[stdout]\nexact-script");
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
            background: false,
        })
        .await
        .unwrap();

    let lines: Vec<&str> = output.lines().collect();
    assert!(
        lines.len() <= 20 + 4,
        "expected at most 24 lines (20 kept plus marker), got {}:\n{output}",
        lines.len()
    );
    assert!(
        output.starts_with("[stdout]\n1\n2\n"),
        "head missing:\n{output}"
    );
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
async fn bash_output_within_line_cap_is_returned_with_a_stream_label() {
    let tool = bash_tool_with_line_cap(Some(20));
    let output = tool
        .call(BashArgs {
            command: "seq 1 5".to_string(),
            timeout: None,
            background: false,
        })
        .await
        .unwrap();
    assert_eq!(output, "[stdout]\n1\n2\n3\n4\n5\n");
}

#[tokio::test]
async fn bash_large_output_runs_to_completion_with_head_tail_and_line_cap() {
    let tool = bash_tool_with_line_cap(Some(20));
    // 4 MB of output used to kill the command at the 1 MiB stdout cap. It now
    // completes and keeps a bounded head/tail.
    let output = tool
        .call(BashArgs {
            command: "yes | head -n 2000000; echo finished".to_string(),
            timeout: None,
            background: false,
        })
        .await
        .expect("large output must not stop the command");
    assert!(
        output.lines().count() <= 20 + 8,
        "{} lines",
        output.lines().count()
    );
    assert!(output.contains("lines omitted"), "line marker missing");
    assert!(output.contains("finished"), "tail must keep the last line");
    assert!(
        output.contains("4000009 bytes of output in total"),
        "{output}"
    );
}

#[tokio::test]
async fn bash_five_mebibytes_completes_and_spools_the_complete_output_to_a_private_log() {
    let dir = crate::tests::support::temp::TempDir::new("shell-log");
    let tool = bash_tool_with_line_cap(None).with_output_log_dir(Some(dir.path().join("logs")));
    let output = tool
        .call(BashArgs {
            command: "yes | head -c 5000000".to_string(),
            timeout: None,
            background: false,
        })
        .await
        .expect("5 MiB of output completes");
    assert!(output.starts_with("[stdout]\ny\ny\n"));
    assert!(
        output.contains("bytes of output omitted between the retained head and tail"),
        "omitted marker missing"
    );
    assert!(!output.contains("Exit code"), "exit status 0 is preserved");
    let path = output
        .rsplit_once("saved to ")
        .and_then(|(_, rest)| rest.split_once("; use read"))
        .map(|(path, _)| std::path::PathBuf::from(path))
        .unwrap_or_else(|| panic!("log footer missing: {}", &output[output.len() - 400..]));
    assert!(path.starts_with(dir.path()));
    let log = std::fs::read(&path).unwrap();
    assert_eq!(log.len(), 5_000_000);
    assert!(log.chunks(2).all(|pair| pair == b"y\n"));
}

#[tokio::test]
async fn bash_build_like_script_keeps_its_real_exit_status_and_final_lines() {
    let tool = bash_tool_with_line_cap(Some(50));
    let output = tool
        .call(BashArgs {
            command: "i=0; while [ $i -lt 120000 ]; do echo \"   Compiling crate-$i v0.1.0\"; i=$((i+1)); done; echo 'error: build failed' >&2; exit 3".to_string(),
            timeout: None,
            background: false,
        })
        .await
        .expect("a nonzero exit is a completed result");
    assert!(output.contains("Compiling crate-0 v0.1.0"));
    assert!(output.contains("Compiling crate-119999 v0.1.0"));
    assert!(output.contains("error: build failed"));
    assert!(
        output.contains("Exit code: 3"),
        "{}",
        &output[output.len().saturating_sub(600)..]
    );
}

#[tokio::test]
async fn bash_output_beyond_the_hard_ceiling_is_stopped_with_a_clear_status() {
    let tool = bash_tool_with_line_cap(Some(20));
    let error = tool
        .call(BashArgs {
            command: "yes | head -c 80000000; echo never-reached".to_string(),
            timeout: None,
            background: false,
        })
        .await
        .expect_err("pathological volume must still be stopped");
    let message = error.to_string();
    assert!(
        message.contains("[status: output_truncated; limit: combined; byte_cap: 67108864"),
        "unexpected error: {}",
        &message[..message.len().min(300)]
    );
    assert!(message.contains("hard ceiling"));
    assert!(!message.contains("never-reached"));
    assert!(message.lines().count() <= 20 + 8);
}

#[tokio::test]
async fn bash_without_line_cap_returns_all_lines() {
    let tool = bash_tool_with_line_cap(None);
    let output = tool
        .call(BashArgs {
            command: "seq 1 100".to_string(),
            timeout: None,
            background: false,
        })
        .await
        .unwrap();
    assert_eq!(output.lines().count(), 101);
    assert!(output.starts_with("[stdout]\n1\n"));
    assert!(!output.contains("omitted"));
}
