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
        let mut command = Command::new(env!("CARGO_BIN_EXE_mini-agent"));
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
                socket.read_exact(&mut vec![0; length])?;
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
                    let partial = matches!(
                        outcome,
                        "partial_failure" | "partial_wait" | "active_command"
                    ) || (outcome == "verification" && index == 0);
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
    let output = child.wait_with_output().unwrap();
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
