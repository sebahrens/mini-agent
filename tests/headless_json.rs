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
    assert_eq!(
        value["result"].as_str().unwrap().trim(),
        "headless-json-smoke"
    );
    assert_eq!(value["files_changed"], serde_json::json!([]));
    assert_eq!(
        value["tool_calls"],
        serde_json::json!({"total": 0, "by_name": {}})
    );
    assert_eq!(value["usage"]["total_tokens"], 0);
    assert_eq!(value["cost"], 0.0);
    assert_eq!(value["stop_reason"], "completed");
}

#[cfg(unix)]
#[test]
fn explicit_print_interrupt_reaps_the_command_before_exiting() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    use std::io;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    for signal in [Signal::SIGINT, Signal::SIGTERM] {
        let root = TempRoot::new();
        let marker = root.0.join("command.pid");
        let mut child = Command::new(env!("CARGO_BIN_EXE_mini-agent"))
            .current_dir(&root.0)
            .env("ZS_CONFIG_DIR", &root.0)
            .env("ZS_DATA_DIR", &root.0)
            .env("ZS_LOCAL_DATA_DIR", &root.0)
            .env("ZS_STATE_DIR", &root.0)
            .env("ZS_CACHE_DIR", &root.0)
            .env("ZS_CREDENTIALS_DIR", &root.0)
            .env("OPENROUTER_API_KEY", "headless-interrupt-test-key")
            .args([
                "--no-sandbox",
                "--no-session",
                "--no-context-files",
                "--shell",
                "/bin/sh",
                "-p",
                "!echo $$ > command.pid; exec /bin/sleep 30",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start explicit print command");
        let mut command_pid = None;
        let observation = (|| -> io::Result<_> {
            let deadline = Instant::now() + Duration::from_secs(10);
            let pid = loop {
                if let Some(pid) = std::fs::read_to_string(&marker)
                    .ok()
                    .and_then(|text| text.trim().parse::<i32>().ok())
                    .filter(|pid| *pid > 0)
                {
                    break Pid::from_raw(pid);
                }
                if child.try_wait()?.is_some() || Instant::now() >= deadline {
                    return Err(io::Error::other("explicit command did not start"));
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            command_pid = Some(pid);
            kill(Pid::from_raw(child.id() as i32), signal).map_err(io::Error::other)?;
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(status) = child.try_wait()? {
                    return Ok((status, kill(pid, None).is_ok()));
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::other("interrupted print did not settle"));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        })();
        // Clean up even when testing the unfixed binary, which leaves its
        // separately grouped command alive after the parent is signalled.
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
        if let Some(pid) = command_pid
            && !matches!(&observation, Ok((_, false)))
        {
            let _ = kill(pid, Signal::SIGKILL);
        }
        let output = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        let (status, command_survived) = observation.expect("observe command interruption");
        assert!(!command_survived, "{signal:?} left the command alive");
        assert_eq!(
            status.signal(),
            None,
            "parent must finish cooperative cleanup"
        );
        assert_eq!(status.code(), Some(1), "{stderr}");
        assert!(stderr.contains("headless command interrupted"), "{stderr}");
    }
}
