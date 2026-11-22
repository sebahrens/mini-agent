//! `mini-agent --mcp-server` end to end: the real binary completes the MCP
//! handshake, lists its `run` tool and executes a prompt as a headless child
//! that honours the server's own flags (here `--no-tools`) against a local
//! scripted provider.
#![cfg(all(unix, feature = "mcp"))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[path = "../src/tests/support/temp.rs"]
mod temp;

fn tested_binary() -> std::ffi::OsString {
    std::env::var_os("MINI_AGENT_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_mini-agent").into())
}

/// One scripted streaming completion; returns the request body it received.
fn provider(
    listener: TcpListener,
    reply: &'static str,
) -> std::thread::JoinHandle<serde_json::Value> {
    std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        let (headers, mut socket) = loop {
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "provider was never contacted");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
            }
            let headers = String::from_utf8(headers).unwrap();
            if headers.starts_with("GET ") {
                // The startup context-window probe lists models; serve none.
                socket
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                continue;
            }
            break (headers, socket);
        };
        let length: usize = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())
                    .flatten()
            })
            .unwrap();
        let mut body = vec![0; length];
        socket.read_exact(&mut body).unwrap();
        let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
            serde_json::json!({"id":"s","object":"chat.completion.chunk","created":0,"model":"test",
                "choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
        };
        let mut last = chunk(serde_json::json!({}), "stop".into());
        last["usage"] =
            serde_json::json!({"prompt_tokens":10,"completion_tokens":2,"total_tokens":12});
        let stream = format!(
            "data: {}\n\ndata: {last}\n\ndata: [DONE]\n\n",
            chunk(
                serde_json::json!({"role":"assistant","content":reply}),
                serde_json::Value::Null
            )
        );
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{stream}",
            stream.len()
        )
        .unwrap();
        serde_json::from_slice(&body).unwrap()
    })
}

#[test]
fn mcp_server_mode_initializes_lists_run_and_executes_a_headless_prompt() {
    let root = temp::TempDir::new_canonical("mcp-server-mode");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::fs::write(
        root.join("config.toml"),
        format!(
            "session_title_model=\"off\"\n[custom_providers.local-test]\nprovider_type=\"openai\"\nbase_url=\"http://{address}/v1\"\napi_key_env=\"MCP_SERVER_TEST_KEY\"\napi_style=\"completions\"\n"
        ),
    )
    .unwrap();
    let server = provider(listener, "pong from mini-agent");

    let mut command = Command::new(tested_binary());
    command
        .current_dir(root.path())
        .env("HOME", root.path())
        .env("MCP_SERVER_TEST_KEY", "local-test-key")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost");
    for name in [
        "MINI_AGENT_CONFIG_DIR",
        "MINI_AGENT_DATA_DIR",
        "MINI_AGENT_LOCAL_DATA_DIR",
        "MINI_AGENT_STATE_DIR",
        "MINI_AGENT_CACHE_DIR",
        "MINI_AGENT_CREDENTIALS_DIR",
    ] {
        command.env(name, root.path());
    }
    let mut child = command
        .args([
            "--mcp-server",
            "--no-sandbox",
            "--no-context-files",
            "--no-tools",
            "--provider",
            "local-test",
            "--model",
            "test",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut call = |request: serde_json::Value| -> serde_json::Value {
        writeln!(stdin, "{request}").unwrap();
        stdin.flush().unwrap();
        if request.get("id").is_none() {
            return serde_json::Value::Null;
        }
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON-RPC: {line:?}"))
    };
    let init = call(
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "mini-agent");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    call(serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    let tools = call(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
    assert_eq!(tools["result"]["tools"][0]["name"], "run");
    let run = call(
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
        "name":"run","arguments":{"prompt":"say pong"}}}),
    );
    let text = run["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    let request = server.join().unwrap();
    drop(stdin);
    let status = child.wait().unwrap();
    let mut stderr = String::new();
    let _ = child.stderr.take().unwrap().read_to_string(&mut stderr);
    assert_eq!(run["result"]["isError"], false, "{run} {stderr}");
    assert!(text.contains("pong from mini-agent"), "{text} {stderr}");
    assert!(status.success(), "{stderr}");
    // The forwarded `--no-tools` reached the headless child.
    assert!(
        request
            .get("tools")
            .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty)),
        "{request}"
    );
    assert!(request.to_string().contains("say pong"));
}

#[test]
fn mcp_server_run_with_a_progress_token_reports_the_model_turn() {
    let root = temp::TempDir::new_canonical("mcp-server-progress");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::fs::write(
        root.join("config.toml"),
        format!(
            "session_title_model=\"off\"\n[custom_providers.local-test]\nprovider_type=\"openai\"\nbase_url=\"http://{address}/v1\"\napi_key_env=\"MCP_SERVER_TEST_KEY\"\napi_style=\"completions\"\n"
        ),
    )
    .unwrap();
    let server = provider(listener, "pong from mini-agent");

    let mut command = Command::new(tested_binary());
    command
        .current_dir(root.path())
        .env("HOME", root.path())
        .env("MCP_SERVER_TEST_KEY", "local-test-key")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost");
    for name in [
        "MINI_AGENT_CONFIG_DIR",
        "MINI_AGENT_DATA_DIR",
        "MINI_AGENT_LOCAL_DATA_DIR",
        "MINI_AGENT_STATE_DIR",
        "MINI_AGENT_CACHE_DIR",
        "MINI_AGENT_CREDENTIALS_DIR",
    ] {
        command.env(name, root.path());
    }
    let mut child = command
        .args([
            "--mcp-server",
            "--no-sandbox",
            "--no-context-files",
            "--no-tools",
            "--provider",
            "local-test",
            "--model",
            "test",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut call = |request: serde_json::Value| -> serde_json::Value {
        writeln!(stdin, "{request}").unwrap();
        stdin.flush().unwrap();
        if request.get("id").is_none() {
            return serde_json::Value::Null;
        }
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON-RPC: {line:?}"))
    };
    let init = call(
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "mini-agent");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    call(serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
            "_meta":{"progressToken":"tok"},
            "name":"run","arguments":{"prompt":"say pong"}}})
    )
    .unwrap();
    stdin.flush().unwrap();
    let mut messages = Vec::new();
    let run = loop {
        let mut line = String::new();
        assert_ne!(stdout.read_line(&mut line).unwrap(), 0, "server closed");
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        if value["method"] == "notifications/progress" {
            assert_eq!(value["params"]["progressToken"], "tok");
            messages.push(value["params"]["message"].as_str().unwrap().to_owned());
        } else if value["id"] == 3 {
            break value;
        }
    };
    server.join().unwrap();
    drop(stdin);
    let _ = child.wait();
    assert_eq!(run["result"]["isError"], false, "{run}");
    assert!(
        run["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("pong from mini-agent"),
        "{run}"
    );
    assert!(
        messages.iter().any(|m| m == "model turn finished"),
        "{messages:?}"
    );
}

/// One scripted completion: a streamed delta and its finish reason.
type Scripted = (serde_json::Value, &'static str);

/// Answers each request from `script` in order and returns every request body.
fn scripted(
    listener: TcpListener,
    script: Vec<Scripted>,
) -> std::thread::JoinHandle<Vec<serde_json::Value>> {
    std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let mut bodies = Vec::new();
        for (delta, finish) in script {
            let deadline = Instant::now() + Duration::from_secs(60);
            let (headers, mut socket) = loop {
                let mut socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                return bodies;
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).unwrap();
                    headers.push(byte[0]);
                }
                let headers = String::from_utf8(headers).unwrap();
                if headers.starts_with("GET ") {
                    socket
                        .write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                    continue;
                }
                break (headers, socket);
            };
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().ok())
                        .flatten()
                })
                .unwrap();
            let mut body = vec![0; length];
            socket.read_exact(&mut body).unwrap();
            bodies.push(serde_json::from_slice(&body).unwrap());
            let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
                serde_json::json!({"id":"s","object":"chat.completion.chunk","created":0,"model":"test",
                    "choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
            };
            let mut last = chunk(serde_json::json!({}), finish.into());
            last["usage"] =
                serde_json::json!({"prompt_tokens":10,"completion_tokens":2,"total_tokens":12});
            let stream = format!(
                "data: {}\n\ndata: {last}\n\ndata: [DONE]\n\n",
                chunk(delta, serde_json::Value::Null)
            );
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{stream}",
                stream.len()
            )
            .unwrap();
        }
        bodies
    })
}

fn tool_call(name: &str, arguments: serde_json::Value) -> Scripted {
    (
        serde_json::json!({"role":"assistant","tool_calls":[{
            "index":0,"id":"call-1","type":"function",
            "function":{"name":name,"arguments":arguments.to_string()}
        }]}),
        "tool_calls",
    )
}

fn say(text: &str) -> Scripted {
    (
        serde_json::json!({"role":"assistant","content":text}),
        "stop",
    )
}

/// A running `--mcp-server` against a scripted provider, with the MCP
/// handshake done. `flags` extend the server's own arguments.
struct McpServer {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    _root: temp::TempDir,
}

impl McpServer {
    fn start(
        root: temp::TempDir,
        listener: &TcpListener,
        flags: &[&str],
        capabilities: serde_json::Value,
    ) -> Self {
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let address = listener.local_addr().unwrap();
        std::fs::write(
            root.join("config.toml"),
            format!(
                "session_title_model=\"off\"\n[custom_providers.local-test]\nprovider_type=\"openai\"\nbase_url=\"http://{address}/v1\"\napi_key_env=\"MCP_SERVER_TEST_KEY\"\napi_style=\"completions\"\n"
            ),
        )
        .unwrap();
        let mut command = Command::new(tested_binary());
        command
            .current_dir(root.path())
            .env("HOME", root.path())
            .env("MCP_SERVER_TEST_KEY", "local-test-key")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost");
        for name in [
            "MINI_AGENT_CONFIG_DIR",
            "MINI_AGENT_DATA_DIR",
            "MINI_AGENT_LOCAL_DATA_DIR",
            "MINI_AGENT_STATE_DIR",
            "MINI_AGENT_CACHE_DIR",
            "MINI_AGENT_CREDENTIALS_DIR",
        ] {
            command.env(name, root.path());
        }
        let mut child = command
            .args([
                "--mcp-server",
                "--no-sandbox",
                "--no-context-files",
                "--provider",
                "local-test",
                "--model",
                "test",
            ])
            .args(flags)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut server = Self {
            child,
            stdin,
            stdout,
            _root: root,
        };
        server.send(
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":capabilities,
            "clientInfo":{"name":"test","version":"0"}}}),
        );
        assert_eq!(server.read()["result"]["serverInfo"]["name"], "mini-agent");
        server.send(serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        server
    }

    fn send(&mut self, message: serde_json::Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn read(&mut self) -> serde_json::Value {
        let mut line = String::new();
        assert_ne!(
            self.stdout.read_line(&mut line).unwrap(),
            0,
            "server closed"
        );
        serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON-RPC: {line:?}"))
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// mini-agent-1ptzh: a run with a progress token reports each tool call's start
/// and finish, against a provider that really makes a tool call.
#[test]
fn mcp_server_run_reports_tool_call_progress_notifications() {
    let root = temp::TempDir::new_canonical("mcp-server-tool-progress");
    std::fs::write(root.join("evidence.txt"), "PROGRESS_EVIDENCE").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let provider = scripted(
        listener.try_clone().unwrap(),
        vec![
            tool_call("read", serde_json::json!({"path":"evidence.txt"})),
            say("read it"),
        ],
    );
    let mut server = McpServer::start(
        root,
        &listener,
        &["--yolo", "--tools", "read"],
        serde_json::json!({}),
    );
    server.send(
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
        "_meta":{"progressToken":"tok"},
        "name":"run","arguments":{"prompt":"read the evidence"}}}),
    );
    let mut messages = Vec::new();
    let run = loop {
        let value = server.read();
        if value["method"] == "notifications/progress" {
            assert_eq!(value["params"]["progressToken"], "tok");
            messages.push(value["params"]["message"].as_str().unwrap().to_owned());
        } else if value["id"] == 3 {
            break value;
        }
    };
    let bodies = provider.join().unwrap();
    assert_eq!(run["result"]["isError"], false, "{run}");
    assert!(
        bodies
            .iter()
            .any(|b| b.to_string().contains("PROGRESS_EVIDENCE"))
    );
    let position = |wanted: &str| messages.iter().position(|m| m == wanted);
    let (started, finished) = (
        position("tool read started"),
        position("tool read finished"),
    );
    assert!(
        matches!((started, finished), (Some(a), Some(b)) if a < b),
        "{messages:?}"
    );
}

/// mini-agent-mt2f2: a permission ask the client never answers is refused
/// before the run timeout, the model sees the channel-neutral denial text, and
/// the run still returns its answer and session id.
#[test]
fn mcp_server_unanswered_elicitation_is_refused_before_the_timeout() {
    let root = temp::TempDir::new_canonical("mcp-server-elicit");
    let target = root.join("denied.txt");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let provider = scripted(
        listener.try_clone().unwrap(),
        vec![
            tool_call("shell", serde_json::json!({"command":"touch denied.txt"})),
            say("the write was refused"),
        ],
    );
    let mut server = McpServer::start(
        root,
        &listener,
        &[
            "--tools",
            "shell",
            "--mcp-server-elicit-approvals",
            // The server leaves 12 s of grace, so asks wait about 3 s.
            "--mcp-server-timeout",
            "15",
        ],
        serde_json::json!({"elicitation":{}}),
    );
    let started = Instant::now();
    server.send(
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
        "name":"run","arguments":{"prompt":"create the file"}}}),
    );
    let mut asked = 0;
    let mut cancelled = false;
    let run = loop {
        let value = server.read();
        if value["method"] == "elicitation/create" {
            asked += 1; // never answered
        } else if value["method"] == "notifications/cancelled" {
            cancelled = true;
        } else if value["id"] == 3 {
            break value;
        }
    };
    let elapsed = started.elapsed();
    let bodies = provider.join().unwrap();
    assert_eq!(asked, 1, "{run}");
    assert!(cancelled, "the unanswered ask was withdrawn");
    assert!(
        elapsed < Duration::from_secs(12),
        "finished in {elapsed:?}: {run}"
    );
    assert_eq!(run["result"]["isError"], false, "{run}");
    let text = run["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(text.contains("the write was refused"), "{text}");
    assert!(!target.exists(), "the refused command must not run");
    // The emitting side of the channel-neutral denial wording.
    let followup = bodies.last().unwrap().to_string();
    assert!(
        followup.contains("Permission denied by the client:"),
        "{followup}"
    );
    assert!(
        !followup.contains("stream-json control channel"),
        "{followup}"
    );
}
