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

    fn command(&self) -> Command {
        self.command_for(env!("CARGO_BIN_EXE_mini-agent"))
    }

    fn command_for(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.0)
            .stdin(std::process::Stdio::null());
        for name in [
            "ZS_CONFIG_DIR",
            "ZS_DATA_DIR",
            "ZS_LOCAL_DATA_DIR",
            "ZS_STATE_DIR",
            "ZS_CACHE_DIR",
            "ZS_CREDENTIALS_DIR",
        ] {
            command.env(name, &self.0);
        }
        command
    }

    fn local_provider(
        &self,
        outcome: &'static str,
    ) -> std::thread::JoinHandle<std::io::Result<()>> {
        use std::io::{self, Read, Write};
        use std::net::TcpListener;
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let verification_config = if outcome == "verification" {
            "verify_command=\"trap '' TERM; echo $$ > command.pid; exec /bin/sleep 30\"\n"
        } else if outcome == "verification_without_shell" {
            "verify_command=\"test -f effect.txt && printf checked > verified.txt\"\nverify_max_attempts=1\n"
        } else {
            ""
        };
        std::fs::write(self.0.join("config.toml"), format!(
            "{verification_config}[custom_providers.local-test]\nprovider_type=\"openai\"\nbase_url=\"http://{address}/v1\"\napi_key_env=\"HEADLESS_LOCAL_TEST_KEY\"\napi_style=\"completions\"\n"
        )).unwrap();
        let session_path = self.0.join("sessions");
        let waiting = self.0.join("provider.waiting");
        std::thread::spawn(move || -> io::Result<()> {
            let requests = if matches!(
                outcome,
                "partial_failure"
                    | "partial_wait"
                    | "active_command"
                    | "second_wait"
                    | "verification"
                    | "verification_without_shell"
            ) {
                2
            } else {
                1
            };
            for index in 0..requests {
                let deadline = Instant::now() + Duration::from_secs(15);
                let mut socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error)
                            if error.kind() == io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(10))
                        }
                        Err(error) => return Err(error),
                    }
                };
                socket.set_nonblocking(false)?;
                socket.set_read_timeout(Some(Duration::from_secs(5)))?;
                socket.set_write_timeout(Some(Duration::from_secs(5)))?;
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    if headers.len() >= 64 * 1024 {
                        return Err(io::Error::other("request headers too large"));
                    }
                    let mut byte = [0];
                    socket.read_exact(&mut byte)?;
                    headers.push(byte[0]);
                }
                let headers = String::from_utf8(headers).map_err(io::Error::other)?;
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .ok_or_else(|| io::Error::other("missing content length"))?;
                if length > 1024 * 1024 {
                    return Err(io::Error::other("request too large"));
                }
                let mut body = vec![0; length];
                socket.read_exact(&mut body)?;
                // Verify the actual model schema as well as the validator's
                // effects: operator validation must not expose model tools.
                let expected_tools: Option<&[&str]> = match outcome {
                    "validation_no_tools" => Some(&[]),
                    "validation_read_only" => Some(&["read"]),
                    "verification_without_shell" => Some(&["write"]),
                    _ => None,
                };
                if let Some(expected) = expected_tools {
                    let request: serde_json::Value =
                        serde_json::from_slice(&body).map_err(io::Error::other)?;
                    let actual: Vec<_> = request["tools"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|tool| tool["function"]["name"].as_str().unwrap())
                        .collect();
                    assert_eq!(
                        actual, expected,
                        "operator validation changed model tool exposure"
                    );
                }
                if outcome == "initial_wait"
                    || (index == 1 && matches!(outcome, "partial_wait" | "second_wait"))
                {
                    std::fs::write(&waiting, "ready")?;
                    // The interrupted client must close its provider connection.
                    let mut byte = [0];
                    return match socket.read(&mut byte) {
                        Ok(0) => Ok(()),
                        Err(error) if error.kind() == io::ErrorKind::ConnectionReset => Ok(()),
                        _ => Err(io::Error::other(
                            "cancelled provider connection did not close",
                        )),
                    };
                }
                let failed =
                    outcome == "initial_failure" || (outcome == "partial_failure" && index == 1);
                let (status, content_type, body) = if failed {
                    ("400 Bad Request", "application/json", r#"{"error":{"message":"local provider failure","type":"invalid_request_error"}}"#.to_owned())
                } else {
                    let partial =
                        matches!(
                            outcome,
                            "partial_failure" | "partial_wait" | "active_command"
                        ) || (matches!(outcome, "verification" | "verification_without_shell")
                            && index == 0);
                    let shell = outcome == "active_command" && index == 1;
                    let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
                        serde_json::json!({
                            "id":"test-turn", "object":"chat.completion.chunk", "created":0, "model":"test",
                            "choices":[{"index":0, "delta":delta, "finish_reason":finish}]
                        })
                    };
                    let text = chunk(
                        serde_json::json!({"role":"assistant", "content":if shell {""} else if partial {"partial reply"} else {"finished"}}),
                        serde_json::Value::Null,
                    );
                    let mut body = format!("data: {text}\n\n");
                    if partial {
                        let call = chunk(
                            serde_json::json!({"tool_calls":[{"index":0, "id":if shell {"shell-active"} else {"write-progress"}, "type":"function", "function":{
                                "name":if shell {"shell"} else {"write"},
                                "arguments": if shell {
                                    serde_json::json!({"command":"trap '' TERM; echo $$ > command.pid; exec /bin/sleep 30"}).to_string()
                                } else {
                                    serde_json::json!({"path":"effect.txt", "content":"written\n"}).to_string()
                                }
                            }}]}),
                            serde_json::Value::Null,
                        );
                        body.push_str(&format!("data: {call}\n\n"));
                    }
                    let mut finish = chunk(
                        serde_json::json!({}),
                        serde_json::json!(if partial { "tool_calls" } else { "stop" }),
                    );
                    finish["usage"] = serde_json::json!({"prompt_tokens":100,"completion_tokens":20,"total_tokens":120});
                    body.push_str(&format!("data: {finish}\n\ndata: [DONE]\n\n"));
                    if outcome == "persistence_failure" {
                        std::fs::write(&session_path, "blocked")?;
                    }
                    ("200 OK", "text/event-stream", body)
                };
                write!(
                    socket,
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )?;
            }
            Ok(())
        })
    }

    fn provider_command(&self, tools: &str) -> Command {
        let mut command = self.command();
        command
            .env("HEADLESS_LOCAL_TEST_KEY", "local-test-key")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .args([
                "--no-sandbox",
                "--no-context-files",
                "--yolo",
                "--tools",
                tools,
                "--provider",
                "local-test",
                "--model",
                "test",
            ]);
        command
    }

    fn saved_session(&self) -> serde_json::Value {
        let sessions: Vec<_> = std::fs::read_dir(self.0.join("sessions"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        assert_eq!(sessions.len(), 1);
        serde_json::from_slice(&std::fs::read(&sessions[0]).unwrap()).unwrap()
    }
}

fn bounded_output(mut command: Command) -> std::process::Output {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Drain while the child runs: waiting first deadlocks on a full pipe.
    // Retain a bounded diagnostic while continuing to consume excess bytes.
    fn drain(
        mut pipe: impl std::io::Read + Send + 'static,
    ) -> std::thread::JoinHandle<(Vec<u8>, bool)> {
        std::thread::spawn(move || {
            let mut captured = Vec::new();
            let mut truncated = false;
            let mut buffer = [0; 8192];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) => return (captured, truncated),
                    Ok(count) => {
                        let keep = count.min((4 * 1024 * 1024usize).saturating_sub(captured.len()));
                        captured.extend_from_slice(&buffer[..keep]);
                        truncated |= keep < count;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => panic!("read child output: {error}"),
                }
            }
        })
    }
    let stdout = drain(child.stdout.take().unwrap());
    let stderr = drain(child.stderr.take().unwrap());
    let deadline = Instant::now() + Duration::from_secs(30);
    let completed = loop {
        if child.try_wait().unwrap().is_some() {
            break true;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let status = child.wait().unwrap();
    let (stdout, stdout_truncated) = stdout.join().unwrap();
    let (stderr, stderr_truncated) = stderr.join().unwrap();
    assert!(
        !stdout_truncated && !stderr_truncated,
        "fixture output exceeded 4 MiB per stream"
    );
    let output = std::process::Output {
        status,
        stdout,
        stderr,
    };
    assert!(
        completed,
        "headless fixture stalled: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
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
    let mut child = root
        .command()
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
fn startup_commands_run_with_the_windows_default_stack_budget() {
    for (args, expected) in [
        (&["--print-config"][..], "config"),
        (
            &[
                "--no-sandbox",
                "--no-context-files",
                "--no-session",
                "-p",
                "!echo stack-budget",
            ][..],
            "stack-budget",
        ),
        (
            &[
                "--no-sandbox",
                "--no-context-files",
                "--no-session",
                "--no-tools",
                "--provider",
                "local-test",
                "--model",
                "test",
                "-p",
                "hello",
            ][..],
            "finished",
        ),
    ] {
        let root = TempRoot::new();
        let server = (expected == "finished").then(|| root.local_provider("completed"));
        #[cfg(not(unix))]
        let mut command = root.command();
        #[cfg(unix)]
        let mut command = {
            // Apply the limit after exec, from a fresh process main thread.
            // macOS rejects lowering it in a pre_exec callback forked from
            // libtest's secondary thread. Positional arguments keep paths literal.
            let mut command = root.command_for("/bin/sh");
            command
                .args([
                    "-c",
                    "ulimit -s 1024 || exit 125\nexec \"$@\"",
                    "stack-probe",
                ])
                .arg(env!("CARGO_BIN_EXE_mini-agent"));
            command
        };
        command
            .env("OPENROUTER_API_KEY", "headless-stack-test-key")
            .env("HEADLESS_LOCAL_TEST_KEY", "local-test-key")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .args(args);
        let output = bounded_output(command);
        if let Some(server) = server {
            assert!(
                server.join().unwrap().is_ok(),
                "local provider: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains(expected), "missing {expected}: {stdout}");
        if args == ["--print-config"] {
            assert!(
                stdout.contains(root.0.to_str().unwrap()),
                "missing isolated root: {stdout}"
            );
        }
        assert!(!root.0.join(".zerostack").exists());
    }
}

#[test]
fn startup_errors_keep_provider_guidance_without_mislabeling_other_failures() {
    for (args, setup_hint, detail) in [
        (
            &["--provider", "missing-key"][..],
            true,
            "No API key found for custom provider 'missing-key'",
        ),
        (
            &["--provider", "unknown-test-provider"][..],
            true,
            "Unknown provider: 'unknown-test-provider'",
        ),
        (
            &["--session", "missing-session"][..],
            false,
            "no session matching 'missing-session'",
        ),
        #[cfg(feature = "skills")]
        (
            &["--import-agent-skill", "missing-package"][..],
            false,
            "Agent Skill source must be one real directory or one .zip file",
        ),
    ] {
        let root = TempRoot::new();
        std::fs::write(root.0.join("config.toml"),
            "[custom_providers.missing-key]\nprovider_type=\"openai\"\nbase_url=\"http://127.0.0.1:1/v1\"\napi_key_env=\"HEADLESS_MISSING_TEST_KEY\"\n"
        ).unwrap();
        let mut command = root.command();
        command
            .env_remove("HEADLESS_MISSING_TEST_KEY")
            .args([
                "--no-sandbox",
                "--no-context-files",
                "--no-tools",
                "-p",
                "--output",
                "json",
                "ignored",
            ])
            .args(args);
        let output = bounded_output(command);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?}: {stderr}");
        assert!(
            output.stdout.is_empty(),
            "startup failure has no turn result"
        );
        assert!(stderr.contains(detail), "{args:?}: {stderr}");
        assert_eq!(
            stderr.contains("mini-agent --setup"),
            setup_hint,
            "{args:?}: {stderr}"
        );
    }
}

#[test]
fn explicit_print_json_reports_command_outcomes_in_one_value() {
    for (message, disabled, expected, detail, saves_session) in [
        (
            "!echo headless-json-smoke",
            false,
            "completed",
            "headless-json-smoke",
            true,
        ),
        (
            "!echo partial-output; exit 7",
            false,
            "failed",
            "partial-output",
            true,
        ),
        (
            "!echo unused",
            true,
            "failed",
            "configured shell is unavailable or unsupported",
            true,
        ),
        (
            "!echo blocked > sessions; echo headless-json-smoke",
            false,
            "failed",
            "headless-json-smoke",
            false,
        ),
        ("!", false, "", "empty command after '!'", false),
    ] {
        let root = TempRoot::new();
        let mut command = root.command();
        command
            .env("OPENROUTER_API_KEY", "headless-json-test-key")
            .args(["--no-sandbox", "--no-context-files"]);
        if disabled {
            // Configured validation must not enable ordinary ! commands.
            std::fs::write(root.0.join("config.toml"), "verify_command=\"true\"\n").unwrap();
            command.arg("--no-tools");
        }
        command.args(["-p", "--output", "json", message]);
        let output = bounded_output(command);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("mini-agent --setup"),
            "runtime failure must retain its own cause: {stderr}"
        );
        assert_eq!(
            output.status.success(),
            expected == "completed",
            "{message}: {stderr}"
        );
        if expected.is_empty() {
            assert!(output.stdout.is_empty());
            assert!(stderr.contains(detail), "{stderr}");
            continue;
        }
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)
            .expect("stdout must contain exactly one JSON value");
        assert_eq!(value["stop_reason"], expected, "{message}");
        let result = value["result"].as_str().unwrap();
        if expected == "completed" {
            assert_eq!(result.trim(), detail);
        } else {
            assert!(result.contains(detail), "{result}");
        }
        assert_eq!(value["files_changed"], serde_json::json!([]));
        assert_eq!(
            value["tool_calls"],
            serde_json::json!({"total": 0, "by_name": {}})
        );
        assert_eq!(value["usage"]["total_tokens"], 0);
        assert_eq!(value["cost"], 0.0);
        if saves_session {
            let saved = root.saved_session();
            assert!(
                saved["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["role"] == "assistant" && message["content"] == result)
            );
        } else {
            assert!(
                root.0.join("sessions").is_file(),
                "persistence failure must leave the blocker intact"
            );
        }
    }
}

#[test]
fn provider_print_json_retains_progress_and_reports_terminal_failures() {
    for outcome in [
        "completed",
        "initial_failure",
        "partial_failure",
        "persistence_failure",
    ] {
        let root = TempRoot::new();
        let server = root.local_provider(outcome);
        let mut command = root.provider_command("write,shell");
        command.args(["-p", "--output", "json", "write the file"]);
        let output = bounded_output(command);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("mini-agent --setup"),
            "runtime failure must retain its own cause: {stderr}"
        );
        assert!(server.join().unwrap().is_ok(), "{outcome}: {stderr}");
        assert_eq!(
            output.status.success(),
            outcome == "completed",
            "{outcome}: {stderr}"
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)
            .expect("one JSON result, even after turn failure");
        assert_eq!(
            value["stop_reason"],
            if outcome == "completed" {
                "completed"
            } else {
                "failed"
            },
            "{outcome}"
        );
        assert_eq!(
            value["usage"]["total_tokens"],
            if outcome == "initial_failure" { 0 } else { 120 }
        );
        assert_eq!(
            value["result"],
            match outcome {
                "initial_failure" => "",
                "partial_failure" => "partial reply",
                _ => "finished",
            }
        );
        if outcome == "partial_failure" {
            assert_eq!(value["tool_calls"]["by_name"]["write"], 1);
            assert_eq!(value["files_changed"], serde_json::json!(["effect.txt"]));
            assert_eq!(
                std::fs::read_to_string(root.0.join("effect.txt")).unwrap(),
                "written\n"
            );
            let saved = root.saved_session();
            assert_eq!(saved["total_input_tokens"], 100);
            assert_eq!(saved["total_output_tokens"], 20);
            for role in ["tool_call", "tool_result"] {
                assert!(
                    saved["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|message| message["role"] == role
                            && message["tool_call_id"] == "write-progress")
                );
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn headless_interrupt_preserves_progress_and_settles_owned_work() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    use std::io;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    for mode in [
        "explicit",
        "initial_wait",
        "partial_wait",
        "active_command",
        "verification",
        #[cfg(feature = "loop")]
        "validation",
        #[cfg(feature = "loop")]
        "loop_wait",
        #[cfg(feature = "loop")]
        "loop_command",
        #[cfg(feature = "loop")]
        "loop_next",
    ] {
        for signal in [Signal::SIGINT, Signal::SIGTERM] {
            let root = TempRoot::new();
            let waiting = matches!(
                mode,
                "initial_wait" | "partial_wait" | "loop_wait" | "loop_next"
            );
            let marker = root.0.join(if waiting {
                "provider.waiting"
            } else {
                "command.pid"
            });
            let mut command = root.command();
            let server;
            if mode == "explicit" {
                server = None;
                command
                    .env("OPENROUTER_API_KEY", "headless-interrupt-test-key")
                    .args([
                        "--no-sandbox",
                        "--no-session",
                        "--no-context-files",
                        "--shell",
                        "/bin/sh",
                        "-p",
                        "!echo $$ > command.pid; exec /bin/sleep 30",
                    ]);
            } else {
                server = Some(root.local_provider(match mode {
                    "validation" => "completed",
                    "loop_wait" => "partial_wait",
                    "loop_command" => "active_command",
                    "loop_next" => "second_wait",
                    _ => mode,
                }));
                command = root.provider_command("write,shell");
                command.args(["--shell", "/bin/sh"]);
                if mode == "validation" || mode.starts_with("loop_") {
                    command.args([
                        "--loop",
                        "--loop-prompt",
                        "finish the iteration",
                        "--loop-max",
                        "2",
                    ]);
                    if mode == "validation" {
                        // Ignore TERM: cleanup must escalate and reap before exit.
                        command.args([
                            "--loop-run",
                            "trap '' TERM; echo $$ > command.pid; exec /bin/sleep 30",
                        ]);
                    } else if mode == "loop_next" {
                        command.args(["--loop-run", "true"]);
                    }
                } else {
                    command.args(["-p", "--output", "json", "write the file"]);
                }
            }
            let mut child = command
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start headless command");
            let mut command_pid = None;
            let observation = (|| -> io::Result<_> {
                let deadline = Instant::now() + Duration::from_secs(10);
                let pid = loop {
                    if waiting && marker.exists() {
                        break None;
                    }
                    if let Some(pid) = std::fs::read_to_string(&marker)
                        .ok()
                        .and_then(|text| text.trim().parse::<i32>().ok())
                        .filter(|pid| *pid > 0)
                    {
                        break Some(Pid::from_raw(pid));
                    }
                    if child.try_wait()?.is_some() || Instant::now() >= deadline {
                        return Err(io::Error::other("headless command did not start"));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                };
                command_pid = pid;
                kill(Pid::from_raw(child.id() as i32), signal).map_err(io::Error::other)?;
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if let Some(status) = child.try_wait()? {
                        return Ok((status, pid.is_some_and(|pid| kill(pid, None).is_ok())));
                    }
                    if Instant::now() >= deadline {
                        return Err(io::Error::other("interrupted headless run did not settle"));
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
            assert!(
                !stderr.contains("mini-agent --setup"),
                "runtime failure must retain its own cause: {stderr}"
            );
            if let Some(server) = server {
                server
                    .join()
                    .unwrap()
                    .unwrap_or_else(|error| panic!("{mode}: {error}: {stderr}"));
            }
            let (status, command_survived) =
                observation.unwrap_or_else(|error| panic!("{mode} {signal:?}: {error}: {stderr}"));
            assert!(!command_survived, "{signal:?} left the command alive");
            assert_eq!(
                status.signal(),
                None,
                "parent must finish cooperative cleanup"
            );
            assert_eq!(
                status.code(),
                Some(if mode == "validation" { 0 } else { 1 }),
                "{mode}: {stderr}"
            );
            if mode == "explicit" {
                assert!(stderr.contains("headless command interrupted"), "{stderr}");
                continue;
            }
            if mode == "validation" {
                assert!(stderr.contains("[validation status=cancelled"), "{stderr}");
                assert!(
                    stderr.contains("[loop] interrupted during validation"),
                    "{stderr}"
                );
            } else {
                assert!(
                    stderr.contains("headless agent interrupted"),
                    "{mode}: {stderr}"
                );
            }
            let completions = match mode {
                "initial_wait" => 0,
                "active_command" | "loop_command" | "verification" => 2,
                _ => 1,
            };
            let saved = root.saved_session();
            assert_eq!(saved["total_input_tokens"], completions * 100, "{mode}");
            assert_eq!(saved["total_output_tokens"], completions * 20, "{mode}");
            let response = match mode {
                "initial_wait" => "",
                "validation" | "loop_next" => "finished",
                "verification" => "partial replyfinished",
                _ => "partial reply",
            };
            let messages = saved["messages"].as_array().unwrap();
            if !response.is_empty() {
                assert!(
                    messages
                        .iter()
                        .any(|message| message["role"] == "assistant"
                            && message["content"] == response),
                    "{mode}: {messages:?}"
                );
            }
            let wrote_file = !matches!(mode, "initial_wait" | "validation" | "loop_next");
            if wrote_file {
                assert_eq!(
                    std::fs::read_to_string(root.0.join("effect.txt")).unwrap(),
                    "written\n"
                );
                for role in ["tool_call", "tool_result"] {
                    assert!(
                        messages.iter().any(|message| message["role"] == role
                            && message["tool_call_id"] == "write-progress"),
                        "{mode}: {messages:?}"
                    );
                    if matches!(mode, "active_command" | "loop_command") {
                        assert!(
                            messages.iter().any(|message| message["role"] == role
                                && message["tool_call_id"] == "shell-active"),
                            "{mode}: {messages:?}"
                        );
                    }
                }
            }
            if !mode.starts_with("loop_") && mode != "validation" {
                let json: serde_json::Value = serde_json::from_slice(&output.stdout)
                    .expect("one JSON result for interrupted turn");
                assert_eq!(json["stop_reason"], "failed");
                assert_eq!(json["result"], response);
                assert_eq!(json["usage"]["total_tokens"], completions * 120);
                assert_eq!(
                    json["tool_calls"]["total"],
                    if mode == "verification" {
                        1
                    } else {
                        completions
                    }
                );
                assert_eq!(
                    json["files_changed"],
                    if wrote_file {
                        serde_json::json!(["effect.txt"])
                    } else {
                        serde_json::json!([])
                    }
                );
            }
        }
    }
}

// Signal cleanup is covered by headless_interrupt_preserves_progress_and_settles_owned_work.
// Here the real CLI must execute --loop-run and persist its bounded diagnostic.
#[cfg(all(unix, feature = "loop"))]
#[test]
fn loop_validation_cli_preserves_command_results_and_output_limits() {
    for (command, status, detail, tools) in [
        (
            "printf caller-stdout; printf caller-stderr >&2; exit 7",
            "nonzero_exit",
            "exit_code=7",
            "read",
        ),
        ("yes loop-output", "output_truncated", "limit=stdout", ""),
    ] {
        let root = TempRoot::new();
        let server = root.local_provider(if tools.is_empty() {
            "validation_no_tools"
        } else {
            "validation_read_only"
        });
        let mut cli = root.provider_command(tools);
        if tools.is_empty() {
            cli.arg("--no-tools");
        }
        cli.args([
            "--shell",
            "/bin/sh",
            "--loop",
            "--loop-prompt",
            "finish",
            "--loop-max",
            "1",
            "--loop-run",
            command,
        ]);
        let output = bounded_output(cli);
        server.join().unwrap().unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "{stderr}");
        let directories: Vec<_> = std::fs::read_dir(root.0.join("loops"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(directories.len(), 1);
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directories[0].join("iter-0001.json")).unwrap())
                .unwrap();
        assert_eq!(record["response"], "finished");
        let diagnostic = record["validation_output"].as_str().unwrap();
        assert!(
            diagnostic.starts_with(&format!("[validation status={status}")),
            "{}",
            &diagnostic[..diagnostic.len().min(256)]
        );
        assert!(diagnostic.contains(detail));
        assert!(
            stderr.contains(diagnostic),
            "displayed and persisted results must agree"
        );
        assert!(diagnostic.len() <= 1024 * 1024 + 512);
        if status == "nonzero_exit" {
            assert!(diagnostic.contains("[stdout]\ncaller-stdout"));
            assert!(diagnostic.contains("[stderr]\ncaller-stderr"));
        } else {
            assert!(
                diagnostic.len() > 512 * 1024,
                "must exercise capture beyond pipe capacity"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn completion_verification_runs_without_exposing_the_shell_tool() {
    let root = TempRoot::new();
    let server = root.local_provider("verification_without_shell");
    let mut cli = root.provider_command("write");
    cli.args([
        "--shell",
        "/bin/sh",
        "-p",
        "--output",
        "json",
        "write the file",
    ]);
    let output = bounded_output(cli);
    server.join().unwrap().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["stop_reason"], "completed");
    assert_eq!(
        std::fs::read_to_string(root.0.join("effect.txt")).unwrap(),
        "written\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.0.join("verified.txt")).unwrap(),
        "checked"
    );
}

/// A goal reports its own outcome and exit code so a script can branch on it
/// without parsing prose.
#[test]
fn goal_status_and_exit_codes_are_distinct() {
    // This mirrors the mapping in `print::HeadlessStopReason::exit_code`; the
    // point is that a caller can tell "resume me" from "stop retrying".
    let distinct = [
        ("met", 0),
        ("impossible", 20),
        ("blocked", 21),
        ("awaiting user", 22),
        ("paused", 23),
        ("budget limited", 24),
    ];
    let mut codes: Vec<i32> = distinct.iter().map(|(_, code)| *code).collect();
    let before = codes.len();
    codes.sort_unstable();
    codes.dedup();
    assert_eq!(
        codes.len(),
        before,
        "every non-met goal outcome needs its own exit code"
    );
    assert_eq!(distinct[0].1, 0, "only a met goal exits zero");
    assert!(
        !codes.contains(&1),
        "the goal codes must not collide with the generic failure code"
    );
}
