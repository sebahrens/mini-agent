use crate::extras::hooks::settings::HookTrust;
use crate::extras::hooks::subprocess::{
    HookLimits, HookPolicy, HookStatus, OutputLimit, build_hook_invocation, run_hook,
    run_hook_with_limits, run_hook_with_policy,
};
use std::collections::BTreeMap;
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
        env!("CARGO_MANIFEST_DIR"),
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
    let output = run_hook(
        "sh",
        Some(&args),
        b"",
        Duration::from_secs(2),
        env!("CARGO_MANIFEST_DIR"),
    )
    .await;
    assert_eq!(output.exit_code, Some(7));
    assert_eq!(output.status, HookStatus::Completed);
}

#[test]
fn sandbox_wrapper_setup_failure_is_policy_denied_before_hook_start() {
    for (backend, stderr) in [
        ("bwrap", b"bwrap: Creating new namespace failed".as_slice()),
        (
            "seatbelt",
            b"sandbox_apply: Operation not permitted".as_slice(),
        ),
        ("zerobox", b"zerobox: failed to enter sandbox".as_slice()),
    ] {
        let policy = HookPolicy::new(HookTrust::Sandboxed, backend, BTreeMap::new());
        let output = crate::extras::hooks::subprocess::HookOutput {
            started: true,
            exit_code: Some(1),
            stdout: b"untrusted partial output".to_vec(),
            stderr: stderr.to_vec(),
            status: HookStatus::Completed,
            diagnostics: policy.diagnostics(),
        };

        let output = policy.classify_completed_output_for_test(output);

        assert_eq!(output.status, HookStatus::PolicyDenied, "{backend}");
        assert!(!output.started, "{backend}");
        assert_eq!(output.exit_code, None, "{backend}");
        assert!(output.stdout.is_empty(), "{backend}");
        assert_eq!(
            output.stderr, b"sandbox wrapper failed before hook launch readiness",
            "{backend}"
        );
    }
}

#[test]
fn sandbox_readiness_signal_is_removed_before_interpreting_hook_output() {
    let policy = HookPolicy::new(HookTrust::Sandboxed, "bwrap", BTreeMap::new());
    let mut stderr = crate::sandbox::HOOK_SANDBOX_READY_MARKER.to_vec();
    stderr.extend_from_slice(b"hook reason");
    let output = crate::extras::hooks::subprocess::HookOutput {
        started: true,
        exit_code: Some(2),
        stdout: Vec::new(),
        stderr,
        status: HookStatus::Completed,
        diagnostics: policy.diagnostics(),
    };

    let output = policy.classify_completed_output_for_test(output);

    assert!(output.started);
    assert_eq!(output.status, HookStatus::Completed);
    assert_eq!(output.exit_code, Some(2));
    assert_eq!(output.stderr, b"hook reason");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn seatbelt_starts_but_missing_inner_executable_is_policy_denied() {
    let policy = HookPolicy::new(HookTrust::Sandboxed, "seatbelt", BTreeMap::new());
    let args = Vec::new();

    let output = run_hook_with_policy(
        "__mini_agent_missing_hook_executable__",
        Some(&args),
        b"",
        Duration::from_secs(2),
        env!("CARGO_MANIFEST_DIR"),
        &policy,
    )
    .await;

    assert_eq!(output.status, HookStatus::PolicyDenied);
    assert!(!output.started);
    assert_eq!(output.exit_code, None);
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
        env!("CARGO_MANIFEST_DIR"),
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
        env!("CARGO_MANIFEST_DIR"),
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
        env!("CARGO_MANIFEST_DIR"),
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
        env!("CARGO_MANIFEST_DIR"),
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
        env!("CARGO_MANIFEST_DIR"),
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
        env!("CARGO_MANIFEST_DIR"),
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
        env!("CARGO_MANIFEST_DIR"),
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
#[tokio::test]
async fn hook_subprocess_async_cancellation_terminates_descendants() {
    let project = unique_temp_path("cancel-project");
    std::fs::create_dir_all(&project).unwrap();
    let pid_file = project.join("descendant-pid");
    let command = format!(
        "sh -c 'echo $$ > \"{}\"; while :; do sleep 1; done' & wait",
        pid_file.display()
    );
    let args = shell_args(command);
    let policy = HookPolicy::new(HookTrust::Trusted, "unused", BTreeMap::new());
    let project_text = project.to_string_lossy().into_owned();
    let task = tokio::spawn(async move {
        run_hook_with_policy(
            "sh",
            Some(&args),
            b"",
            Duration::from_secs(30),
            &project_text,
            &policy,
        )
        .await
    });

    let ready_deadline = Instant::now() + Duration::from_secs(2);
    while !pid_file.exists() && Instant::now() < ready_deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let descendant_pid: u32 = std::fs::read_to_string(&pid_file)
        .expect("descendant should start before cancellation")
        .trim()
        .parse()
        .unwrap();
    task.abort();
    let _ = task.await;

    let cleanup_deadline = Instant::now() + Duration::from_secs(2);
    while process_is_alive(descendant_pid) && Instant::now() < cleanup_deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!process_is_alive(descendant_pid));
    let _ = std::fs::remove_dir_all(project);
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
        env!("CARGO_MANIFEST_DIR"),
    )
    .await;
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        env!("CARGO_MANIFEST_DIR")
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

    let output = run_hook(
        &command,
        None,
        b"",
        Duration::from_secs(2),
        env!("CARGO_MANIFEST_DIR"),
    )
    .await;

    assert_eq!(output.exit_code, None);
    assert_eq!(output.status, HookStatus::PolicyDenied);
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires an `args` field"));
    assert!(!marker.exists());
}

#[tokio::test]
async fn run_hook_exec_form_bypasses_the_shell() {
    // In exec form the arg is passed literally to the program, with no shell
    // metacharacter expansion (a shell would expand "$HOME" or "*").
    let args = vec!["$HOME literally".to_string()];
    let output = run_hook(
        "echo",
        Some(&args),
        b"",
        Duration::from_secs(2),
        env!("CARGO_MANIFEST_DIR"),
    )
    .await;
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "$HOME literally"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn hook_subprocess_policy_sets_project_cwd_and_minimal_explicit_environment() {
    let project = unique_temp_path("policy-project");
    std::fs::create_dir_all(&project).unwrap();
    let mut env = BTreeMap::new();
    env.insert("HOOK_EXPLICIT".to_string(), "visible".to_string());
    let policy = HookPolicy::new(HookTrust::Trusted, "unused", env);
    let args = shell_args(
        "printf '%s|%s|%s|%s' \"$PWD\" \"$ZEROSTACK_PROJECT_DIR\" \"$HOOK_EXPLICIT\" \"${CARGO_MANIFEST_DIR-unset}\"",
    );

    let output = run_hook_with_policy(
        "sh",
        Some(&args),
        b"",
        Duration::from_secs(2),
        project.to_str().unwrap(),
        &policy,
    )
    .await;

    assert_eq!(output.status, HookStatus::Completed);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!(
            "{}|{}|visible|unset",
            project.canonicalize().unwrap().display(),
            project.canonicalize().unwrap().display()
        )
    );
    assert_eq!(output.diagnostics.environment, "minimal-explicit");
    assert_eq!(output.diagnostics.filesystem, "ambient-trusted-bypass");
    assert_eq!(output.diagnostics.network, "ambient-trusted-bypass");
    let _ = std::fs::remove_dir_all(project);
}

#[cfg(unix)]
#[tokio::test]
async fn hook_subprocess_policy_unavailable_sandbox_denies_before_child_creation() {
    let project = unique_temp_path("policy-denied-project");
    std::fs::create_dir_all(&project).unwrap();
    let marker = project.join("must-not-exist");
    let args = shell_args(format!("touch {}", marker.display()));
    let policy = HookPolicy::new(
        HookTrust::Sandboxed,
        "__mini_agent_missing_hook_sandbox__",
        BTreeMap::new(),
    );

    let output = run_hook_with_policy(
        "sh",
        Some(&args),
        b"",
        Duration::from_secs(2),
        project.to_str().unwrap(),
        &policy,
    )
    .await;

    assert_eq!(output.status, HookStatus::PolicyDenied);
    assert_eq!(output.exit_code, None);
    assert!(!marker.exists());
    assert_eq!(
        output.diagnostics.containment,
        "requested-but-unavailable; launch-denied"
    );
    assert_eq!(output.diagnostics.filesystem, "none; launch-denied");
    assert_eq!(output.diagnostics.network, "none; launch-denied");
    let _ = std::fs::remove_dir_all(project);
}

#[cfg(unix)]
#[tokio::test]
async fn hook_subprocess_policy_rejects_reserved_environment_override() {
    let project = unique_temp_path("policy-invalid-env");
    std::fs::create_dir_all(&project).unwrap();
    let mut env = BTreeMap::new();
    env.insert(
        "zerostack_project_dir".to_string(),
        "/attacker-controlled".to_string(),
    );
    let policy = HookPolicy::new(HookTrust::Trusted, "unused", env);

    let output = run_hook_with_policy(
        "true",
        Some(&[]),
        b"",
        Duration::from_secs(2),
        project.to_str().unwrap(),
        &policy,
    )
    .await;

    assert_eq!(output.status, HookStatus::PolicyDenied);
    assert!(String::from_utf8_lossy(&output.stderr).contains("reserved environment key"));
    let _ = std::fs::remove_dir_all(project);
}

#[cfg(unix)]
#[tokio::test]
async fn hook_subprocess_policy_rejects_case_colliding_environment_keys() {
    let project = unique_temp_path("policy-colliding-env");
    std::fs::create_dir_all(&project).unwrap();
    let env = [
        ("TOKEN".to_string(), "one".to_string()),
        ("token".to_string(), "two".to_string()),
    ]
    .into_iter()
    .collect();
    let policy = HookPolicy::new(HookTrust::Trusted, "unused", env);

    let output = run_hook_with_policy(
        "true",
        Some(&[]),
        b"",
        Duration::from_secs(2),
        project.to_str().unwrap(),
        &policy,
    )
    .await;

    assert_eq!(output.status, HookStatus::PolicyDenied);
    assert!(String::from_utf8_lossy(&output.stderr).contains("collide case-insensitively"));
    let _ = std::fs::remove_dir_all(project);
}

#[cfg(unix)]
#[tokio::test]
async fn hook_subprocess_policy_resolves_relative_program_from_project_without_rewriting_argv() {
    use std::os::unix::fs::PermissionsExt;

    let project = unique_temp_path("policy-relative-program");
    std::fs::create_dir_all(&project).unwrap();
    let script = project.join("show-argv.sh");
    std::fs::write(&script, "#!/bin/sh\nprintf '%s' \"$1\"\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let args = vec!["literal $HOME ; *".to_string()];
    let policy = HookPolicy::new(HookTrust::Trusted, "unused", BTreeMap::new());

    let output = run_hook_with_policy(
        "./show-argv.sh",
        Some(&args),
        b"",
        Duration::from_secs(2),
        project.to_str().unwrap(),
        &policy,
    )
    .await;

    assert_eq!(output.status, HookStatus::Completed);
    assert_eq!(output.stdout, b"literal $HOME ; *");
    let _ = std::fs::remove_dir_all(project);
}

#[cfg(unix)]
#[tokio::test]
async fn hook_subprocess_policy_denies_relative_program_escape() {
    use std::os::unix::fs::PermissionsExt;

    let parent = unique_temp_path("policy-relative-escape");
    let project = parent.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let script = parent.join("outside.sh");
    std::fs::write(&script, "#!/bin/sh\ntouch escaped\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let policy = HookPolicy::new(HookTrust::Trusted, "unused", BTreeMap::new());

    let output = run_hook_with_policy(
        "../outside.sh",
        Some(&[]),
        b"",
        Duration::from_secs(2),
        project.to_str().unwrap(),
        &policy,
    )
    .await;

    assert_eq!(output.status, HookStatus::PolicyDenied);
    assert!(!project.join("escaped").exists());
    let _ = std::fs::remove_dir_all(parent);
}
