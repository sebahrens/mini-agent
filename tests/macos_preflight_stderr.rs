//! Ordinary startup must not print CI machine tokens when the macOS containment preflight fails.
//!
//! Running mini-agent inside an outer Seatbelt sandbox makes the nested worker launch fail, which
//! is the "nested sandbox" case operators hit in practice. `--print-config` and `-p` must report
//! that failure as a human-readable reason and keep `*_FAILED=` stage tokens off stderr; the CI
//! evidence step opts into those tokens explicitly.
#![cfg(all(feature = "js", target_os = "macos"))]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const OUTER_PROFILE: &str = "(version 1)(allow default)";

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mini-agent-preflight-stderr-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&path).expect("create private app root");
        let path = std::fs::canonicalize(path).expect("canonicalize private app root");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("make app root private");
        Self(path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn nested_command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(SANDBOX_EXEC);
    command
        .env_clear()
        .env("HOME", root)
        .env("PATH", "/usr/bin:/bin")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(["-p", OUTER_PROFILE, env!("CARGO_BIN_EXE_mini-agent")])
        .args(args);
    for name in [
        "ZS_CONFIG_DIR",
        "ZS_DATA_DIR",
        "ZS_LOCAL_DATA_DIR",
        "ZS_STATE_DIR",
        "ZS_CACHE_DIR",
        "ZS_CREDENTIALS_DIR",
    ] {
        command.env(name, root);
    }
    command
}

fn bounded_output(mut command: Command) -> Output {
    let mut child = command.spawn().expect("spawn nested mini-agent");
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if child.try_wait().expect("poll nested mini-agent").is_some() {
            return child.wait_with_output().expect("collect nested mini-agent");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("nested mini-agent did not exit within the deadline");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether this host can run a process under an outer Seatbelt profile at all.
fn outer_sandbox_usable() -> bool {
    Command::new(SANDBOX_EXEC)
        .args(["-p", OUTER_PROFILE, "/usr/bin/true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn assert_no_failure_tokens(label: &str, output: &Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("_FAILED="),
        "{label} printed a *_FAILED= token on stderr: {stderr}"
    );
}

#[test]
fn print_config_reports_nested_preflight_failure_without_stderr_tokens() {
    if !outer_sandbox_usable() {
        eprintln!("skipping: sandbox-exec cannot apply an outer profile on this host");
        return;
    }
    let root = TempRoot::new();
    let output = bounded_output(nested_command(&root.0, &["--print-config"]));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "--print-config failed: {output:?}");
    assert!(
        stdout
            .lines()
            .any(|line| line.trim_start().starts_with("status") && line.contains("unavailable")),
        "the nested preflight must fail so this test exercises the failure path: {stdout}"
    );
    assert_no_failure_tokens("--print-config", &output);
    let reason = stdout
        .lines()
        .find(|line| line.trim_start().starts_with("reason"))
        .unwrap_or_default();
    assert!(
        !reason.contains("_FAILED="),
        "the unavailable reason must be a human sentence: {reason}"
    );
}

#[test]
fn print_mode_emits_no_stderr_tokens_when_nested_preflight_fails() {
    if !outer_sandbox_usable() {
        eprintln!("skipping: sandbox-exec cannot apply an outer profile on this host");
        return;
    }
    let root = TempRoot::new();
    // A closed local port makes the provider request fail fast after the agent (and with it the
    // JavaScript containment preflight) has been built.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("reserve a local port")
        .port();
    std::fs::write(
        root.0.join("config.toml"),
        format!(
            "enable_exa_mcp=false\nenable_context7_mcp=false\n[custom_providers.local-test]\nprovider_type=\"openai\"\nbase_url=\"http://127.0.0.1:{port}/v1\"\napi_key_env=\"PREFLIGHT_LOCAL_TEST_KEY\"\napi_style=\"completions\"\n"
        ),
    )
    .expect("write provider config");
    let mut command = nested_command(
        &root.0,
        &[
            "--no-context-files",
            "--provider",
            "local-test",
            "--model",
            "test",
            "-p",
            "hello",
        ],
    );
    command.env("PREFLIGHT_LOCAL_TEST_KEY", "local-test-key");
    let output = bounded_output(command);
    assert_no_failure_tokens("-p", &output);
}
