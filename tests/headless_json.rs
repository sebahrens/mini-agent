use std::process::Command;

struct TempRoot(std::path::PathBuf);

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mini-agent-headless-json-process-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&path).expect("create private app root");
        let path = std::fs::canonicalize(path).expect("canonicalize private app root");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .expect("make app root private");
        }
        Self(path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(feature = "loop")]
#[test]
fn loop_resumes_existing_plan_without_reading_piped_stdin() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let root = TempRoot::new();
    let plan = root.0.join("custom plan.md");
    let contents = "- Keep existing progress\n";
    std::fs::write(&plan, contents).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_mini-agent"))
        .current_dir(&root.0)
        .env("ZS_CONFIG_DIR", &root.0)
        .env("ZS_DATA_DIR", &root.0)
        .env("ZS_LOCAL_DATA_DIR", &root.0)
        .env("ZS_STATE_DIR", &root.0)
        .env("ZS_CACHE_DIR", &root.0)
        .env("OPENROUTER_API_KEY", "loop-startup-test-key")
        .args([
            "--no-sandbox",
            "--no-session",
            "--no-tools",
            "--no-context-files",
            "--loop",
            "--loop-prompt",
            "resume work",
            "--loop-max",
            "0",
            "--loop-plan",
        ])
        .arg(&plan)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start headless loop");
    // Keep the pipe open, without an answer or EOF, until the child exits.
    let _stdin = child.stdin.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let exited = loop {
        if child.try_wait().unwrap().is_some() {
            break true;
        }
        if Instant::now() >= deadline {
            child.kill().expect("stop stalled loop");
            break false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = child.wait_with_output().expect("reap headless loop");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(exited, "loop waited for piped input: {stderr}");
    assert!(output.status.success(), "loop failed: {stderr}");
    assert!(stderr.contains("max iterations (0) reached"), "{stderr}");
    assert!(!stderr.contains("Restart from existing plan?"), "{stderr}");
    assert_eq!(std::fs::read_to_string(plan).unwrap(), contents);
}

#[test]
fn print_json_reserves_stdout_for_one_machine_readable_value() {
    let root = TempRoot::new();
    let output = Command::new(env!("CARGO_BIN_EXE_mini-agent"))
        .env("ZS_CONFIG_DIR", &root.0)
        .env("ZS_DATA_DIR", &root.0)
        .env("ZS_LOCAL_DATA_DIR", &root.0)
        .env("ZS_STATE_DIR", &root.0)
        .env("ZS_CACHE_DIR", &root.0)
        // Startup resolves provider authentication before dispatching the local
        // shell shortcut. Supply a non-secret test value so this process test is
        // hermetic on CI runners without user credentials.
        .env("OPENROUTER_API_KEY", "headless-json-test-key")
        .args([
            "--no-sandbox",
            "--no-session",
            "--no-tools",
            "--no-context-files",
            "-p",
            "--output",
            "json",
            "!echo headless-json-smoke",
        ])
        .output()
        .expect("run installed test binary");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8 JSON");
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout should contain only one JSON value");
    assert!(value["result"].is_string());
    assert_eq!(value["files_changed"], serde_json::json!([]));
    assert_eq!(
        value["tool_calls"],
        serde_json::json!({"total": 0, "by_name": {}})
    );
    assert_eq!(value["usage"]["total_tokens"], 0);
    assert_eq!(value["cost"], 0.0);
    assert_eq!(value["stop_reason"], "completed");
}
