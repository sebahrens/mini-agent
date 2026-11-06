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
