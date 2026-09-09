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
    use std::io::{self, Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    for outcome in [
        "completed",
        "initial_failure",
        "partial_failure",
        "persistence_failure",
    ] {
        let root = TempRoot::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        std::fs::write(root.0.join("config.toml"), format!(
            "[custom_providers.local-test]\nprovider_type=\"openai\"\nbase_url=\"http://{address}/v1\"\napi_key_env=\"HEADLESS_LOCAL_TEST_KEY\"\napi_style=\"completions\"\n"
        )).unwrap();
        let session_path = root.0.join("sessions");
        let server = std::thread::spawn(move || -> io::Result<()> {
            let requests = if outcome == "partial_failure" { 2 } else { 1 };
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
                let failed = outcome == "initial_failure" || index == 1;
                let (status, content_type, body) = if failed {
                    ("400 Bad Request", "application/json", r#"{"error":{"message":"local provider failure","type":"invalid_request_error"}}"#.to_owned())
                } else {
                    let partial = outcome == "partial_failure";
                    let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
                        serde_json::json!({
                            "id":"test-turn", "object":"chat.completion.chunk", "created":0, "model":"test",
                            "choices":[{"index":0, "delta":delta, "finish_reason":finish}]
                        })
                    };
                    let text = chunk(
                        serde_json::json!({"role":"assistant", "content":if partial {"partial reply"} else {"finished"}}),
                        serde_json::Value::Null,
                    );
                    let mut body = format!("data: {text}\n\n");
                    if partial {
                        let call = chunk(
                            serde_json::json!({"tool_calls":[{"index":0, "id":"write-progress", "type":"function", "function":{
                                "name":"write", "arguments":"{\"path\":\"effect.txt\",\"content\":\"written\\n\"}"
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
        });
        let mut command = root.command();
        command
            .env("HEADLESS_LOCAL_TEST_KEY", "local-test-key")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .args([
                "--no-sandbox",
                "--no-context-files",
                "--yolo",
                "--tools",
                "write",
                "--provider",
                "local-test",
                "--model",
                "test",
                "-p",
                "--output",
                "json",
                "write the file",
            ]);
        let output = bounded_output(command);
        let stderr = String::from_utf8_lossy(&output.stderr);
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
        let mut child = root
            .command()
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
