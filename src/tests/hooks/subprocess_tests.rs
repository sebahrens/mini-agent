use crate::extras::hooks::subprocess::{
    HookLimits, HookStatus, OutputLimit, build_hook_invocation, run_hook, run_hook_with_limits,
};
use std::time::Instant;
use tokio::time::Duration;

fn limits(stdout_bytes: usize, stderr_bytes: usize, combined_bytes: usize) -> HookLimits {
    HookLimits {
        stdout_bytes,
        stderr_bytes,
        combined_bytes,
    }
}

fn shell_args(script: impl Into<String>) -> Vec<String> {
    vec!["-c".to_string(), script.into()]
}

#[cfg(unix)]
fn unique_temp_path(name: &str) -> std::path::PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock should be after the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "zerostack-hook-subprocess-{name}-{}-{nonce}",
        std::process::id()
    ))
}

#[test]
fn build_hook_invocation_requires_args() {
    let error = build_hook_invocation("echo hi", None).unwrap_err();
    assert!(error.contains("requires an `args` field"));
}

#[test]
fn build_hook_invocation_uses_exec_form_when_args_present() {
    let extra = vec!["hello".to_string(), "world".to_string()];
    let (program, args) = build_hook_invocation("echo", Some(&extra)).unwrap();
    assert_eq!(program, "echo");
    assert_eq!(args, vec!["hello".to_string(), "world".to_string()]);
}

#[test]
fn build_hook_invocation_accepts_empty_args() {
    let extra = Vec::new();
    let (program, args) = build_hook_invocation("echo", Some(&extra)).unwrap();
    assert_eq!(program, "echo");
    assert!(args.is_empty());
}

#[tokio::test]
async fn hook_subprocess_limits_normal_hook_preserves_stdin_and_exit_code() {
    let args = Vec::new();
    let output = run_hook_with_limits(
        "cat",
        Some(&args),
        b"hello",
        Duration::from_secs(2),
        "/repo",
        limits(64, 64, 128),
    )
    .await;
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout, b"hello");
    assert_eq!(output.status, HookStatus::Completed);
}

#[tokio::test]
async fn run_hook_reports_nonzero_exit_code() {
    let args = shell_args("exit 7");
    let output = run_hook("sh", Some(&args), b"", Duration::from_secs(2), "/repo").await;
    assert_eq!(output.exit_code, Some(7));
    assert_eq!(output.status, HookStatus::Completed);
}

#[tokio::test]
async fn hook_subprocess_limits_stdout_fill_does_not_deadlock() {
    let args = shell_args("dd if=/dev/zero bs=1024 count=128 2>/dev/null");
    let started = Instant::now();
    let output = run_hook_with_limits(
        "sh",
        Some(&args),
        b"",
        Duration::from_secs(2),
        "/repo",
        limits(256 * 1024, 256 * 1024, 300 * 1024),
    )
    .await;

    assert_eq!(output.status, HookStatus::Completed);
    assert_eq!(output.stdout.len(), 128 * 1024);
    assert!(output.stderr.is_empty());
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn hook_subprocess_limits_stderr_fill_does_not_deadlock() {
    let args = shell_args("dd if=/dev/zero bs=1024 count=128 >&2 2>/dev/null");
    let started = Instant::now();
    let output = run_hook_with_limits(
        "sh",
        Some(&args),
        b"",
        Duration::from_secs(2),
        "/repo",
        limits(256 * 1024, 256 * 1024, 300 * 1024),
    )
    .await;

    assert_eq!(output.status, HookStatus::Completed);
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr.len(), 128 * 1024);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn hook_subprocess_limits_stdout_cap_is_a_hard_failure() {
    let args =
        shell_args("i=0; while [ \"$i\" -lt 1000 ]; do printf 0123456789; i=$((i + 1)); done");
    let output = run_hook_with_limits(
        "sh",
        Some(&args),
        b"",
        Duration::from_secs(2),
        "/repo",
        limits(256, 1024, 2048),
    )
    .await;

    assert_eq!(
        output.status,
        HookStatus::OutputLimitExceeded(OutputLimit::Stdout)
    );
    assert_eq!(output.stdout.len(), 256);
    assert!(output.stderr.len() <= 1024);
    assert!(output.stdout.len() + output.stderr.len() <= 2048);
}

#[tokio::test]
async fn hook_subprocess_limits_stderr_cap_is_a_hard_failure() {
    let args =
        shell_args("i=0; while [ \"$i\" -lt 1000 ]; do printf 0123456789 >&2; i=$((i + 1)); done");
    let output = run_hook_with_limits(
        "sh",
        Some(&args),
        b"",
        Duration::from_secs(2),
        "/repo",
        limits(1024, 256, 2048),
    )
    .await;

    assert_eq!(
        output.status,
        HookStatus::OutputLimitExceeded(OutputLimit::Stderr)
    );
    assert!(output.stdout.len() <= 1024);
    assert_eq!(output.stderr.len(), 256);
    assert!(output.stdout.len() + output.stderr.len() <= 2048);
}

#[tokio::test]
async fn hook_subprocess_limits_mixed_fill_enforces_combined_cap() {
    let args = shell_args(
        "i=0; while [ \"$i\" -lt 1000 ]; do \
         printf 0123456789; printf 0123456789 >&2; i=$((i + 1)); done",
    );
    let output = run_hook_with_limits(
        "sh",
        Some(&args),
        b"",
        Duration::from_secs(2),
        "/repo",
        limits(1024, 1024, 512),
    )
    .await;

    assert_eq!(
        output.status,
        HookStatus::OutputLimitExceeded(OutputLimit::Combined)
    );
    assert!(output.stdout.len() <= 1024);
    assert!(output.stderr.len() <= 1024);
    assert_eq!(output.stdout.len() + output.stderr.len(), 512);
}

#[tokio::test]
async fn hook_subprocess_limits_infinite_process_times_out_promptly() {
    let args = shell_args("sleep 10");
    let started = Instant::now();
    let output = run_hook_with_limits(
        "sh",
        Some(&args),
        b"",
        Duration::from_millis(100),
        "/repo",
        limits(64, 64, 128),
    )
    .await;

    assert_eq!(output.status, HookStatus::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[cfg(unix)]
#[tokio::test]
async fn hook_subprocess_limits_forked_descendant_is_terminated() {
    let pid_file = unique_temp_path("descendant-pid");
    let command = format!(
        "sh -c 'echo $$ > \"{}\"; while :; do sleep 1; done' & wait",
        pid_file.display()
    );
    let args = shell_args(command);

    let output = run_hook_with_limits(
        "sh",
        Some(&args),
        b"",
        Duration::from_millis(250),
        "/repo",
        limits(64, 64, 128),
    )
    .await;
    assert_eq!(output.status, HookStatus::TimedOut);

    let descendant_pid: u32 = std::fs::read_to_string(&pid_file)
        .expect("forked helper should record its pid before timeout")
        .trim()
        .parse()
        .expect("recorded descendant pid should be numeric");
    let cleanup_deadline = Instant::now() + Duration::from_secs(2);
    while process_is_alive(descendant_pid) && Instant::now() < cleanup_deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !process_is_alive(descendant_pid),
        "forked hook descendant {descendant_pid} survived process-group cleanup"
    );
    let _ = std::fs::remove_file(pid_file);
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[tokio::test]
async fn run_hook_exposes_zerostack_project_dir_env_var() {
    let args = vec![
        "-c".to_string(),
        "echo \"$ZEROSTACK_PROJECT_DIR\"".to_string(),
    ];
    let output = run_hook(
        "sh",
        Some(&args),
        b"",
        Duration::from_secs(2),
        "/repo/project",
    )
    .await;
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "/repo/project"
    );
}

#[tokio::test]
async fn run_hook_rejects_shell_metacharacters_without_args() {
    let marker = std::env::temp_dir().join(format!(
        "zerostack-hook-subprocess-injection-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker);
    let command = format!("echo safe; touch {}", marker.display());

    let output = run_hook(&command, None, b"", Duration::from_secs(2), "/repo").await;

    assert_eq!(output.exit_code, None);
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires an `args` field"));
    assert!(!marker.exists());
}

#[tokio::test]
async fn run_hook_exec_form_bypasses_the_shell() {
    // In exec form the arg is passed literally to the program, with no shell
    // metacharacter expansion (a shell would expand "$HOME" or "*").
    let args = vec!["$HOME literally".to_string()];
    let output = run_hook("echo", Some(&args), b"", Duration::from_secs(2), "/repo").await;
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "$HOME literally"
    );
}
