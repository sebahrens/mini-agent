//! Actual ACP stdio/wire/save/load contract; no paid provider or imported effects.
#![cfg(feature = "acp")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aFZkAAAAASUVORK5CYII=";

struct Roots(PathBuf);
impl Roots {
    fn new() -> Self {
        Self::new_with_setup(|_| {})
    }
    fn new_with_setup(setup: impl FnOnce(&std::path::Path)) -> Self {
        let path = std::env::temp_dir().join(format!("mini-agent-acp53-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        let mut roots = Self(path);
        setup(&roots.0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&roots.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        roots.0 = std::fs::canonicalize(&roots.0).unwrap();
        roots
    }
    fn command(&self) -> Command {
        // Exercise the unchanged physical Windows-default stack budget on
        // Unix too. "$@" passes the exact binary/argv; no path interpolation.
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", "ulimit -S -s 1024; exec \"$@\"", "acp53-one-mib"])
                .arg(env!("CARGO_BIN_EXE_mini-agent"));
            command
        };
        #[cfg(not(unix))]
        let mut command = Command::new(env!("CARGO_BIN_EXE_mini-agent"));
        command.current_dir(&self.0);
        for name in [
            "MINI_AGENT_CONFIG_DIR",
            "MINI_AGENT_DATA_DIR",
            "MINI_AGENT_LOCAL_DATA_DIR",
            "MINI_AGENT_STATE_DIR",
            "MINI_AGENT_CACHE_DIR",
            "MINI_AGENT_CREDENTIALS_DIR",
        ] {
            command.env(name, &self.0);
        }
        // Legacy discovery also consults the platform's former default roots;
        // point them at this fixture so a developer's real sessions or
        // configuration cannot leak into the session-file assertions.
        #[cfg(target_os = "linux")]
        command
            .env("XDG_CONFIG_HOME", &self.0)
            .env("XDG_DATA_HOME", &self.0)
            .env("XDG_STATE_HOME", &self.0)
            .env("XDG_CACHE_HOME", &self.0);
        #[cfg(target_os = "macos")]
        command.env("HOME", &self.0);
        #[cfg(target_os = "windows")]
        command.env("APPDATA", &self.0).env("LOCALAPPDATA", &self.0);
        command
            .env("HEADLESS_LOCAL_TEST_KEY", "fixture-only")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost");
        command
    }
}
impl Drop for Roots {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn acp53_fixture_setup_panic_retires_the_exclusively_created_root() {
    let mut created = None;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Roots::new_with_setup(|path| {
            created = Some(path.to_path_buf());
            assert!(path.is_dir());
            panic!("controlled ACP fixture setup failure");
        });
    }));
    assert!(result.is_err());
    assert!(!created.unwrap().exists());
}

struct Provider {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Vec<serde_json::Value>>>,
}
impl Provider {
    fn start(roots: &Roots) -> Self {
        Self::start_protocol(roots, false)
    }
    fn start_protocol(roots: &Roots, native: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        std::fs::write(roots.0.join("config.toml"), format!(
            "session_title_model='off'\n[retry]\nmax_attempts=2\ninitial_backoff_ms=0\nmax_backoff_ms=0\n[custom_providers.local-test]\nprovider_type='openai'\nbase_url='http://{}/v1'\napi_key_env='HEADLESS_LOCAL_TEST_KEY'\napi_style='completions'\n", listener.local_addr().unwrap()
        )).unwrap();
        if native {
            std::fs::write(roots.0.join("config.toml"), format!(
                "session_title_model='off'\n[retry]\nmax_attempts=1\n[custom_providers.local-test]\nprovider_type='anthropic'\nbase_url='http://{}'\napi_key_env='HEADLESS_LOCAL_TEST_KEY'\n", listener.local_addr().unwrap()
            )).unwrap();
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(30);
            while !stopping.load(Ordering::Acquire) && Instant::now() < deadline {
                let mut socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    assert!(headers.len() < 64 * 1024);
                    let mut byte = [0];
                    socket.read_exact(&mut byte).unwrap();
                    headers.push(byte[0]);
                }
                let headers = String::from_utf8(headers).unwrap();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                assert!(length <= 8 * 1024 * 1024);
                let mut body = vec![0; length];
                socket.read_exact(&mut body).unwrap();
                if headers.starts_with("GET ") {
                    let body = "{\"data\":[]}";
                    write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                    continue;
                }
                if native {
                    assert!(headers.starts_with("POST /v1/messages "), "{headers}");
                    requests.push(serde_json::from_slice(&body).unwrap());
                    let mut events = vec![
                        serde_json::json!({"type":"message_start","message":{"id":"acp53-native","role":"assistant","content":[],"model":"test","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":20,"output_tokens":0}}}),
                        serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
                        serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"opaque ACP native thought"}}),
                        serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"acp53-inert-signature"}}),
                        serde_json::json!({"type":"content_block_stop","index":0}),
                    ];
                    let stop_reason = if requests.len() == 1 {
                        events.extend([
                            serde_json::json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"acp53-native-read","name":"read","input":{}}}),
                            serde_json::json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"native-read.txt\"}"}}),
                            serde_json::json!({"type":"content_block_stop","index":1}),
                        ]);
                        "tool_use"
                    } else {
                        events.extend([
                            serde_json::json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
                            serde_json::json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"native answer"}}),
                            serde_json::json!({"type":"content_block_stop","index":1}),
                        ]);
                        "end_turn"
                    };
                    events.extend([
                        serde_json::json!({"type":"message_delta","delta":{"stop_reason":stop_reason,"stop_sequence":null},"usage":{"output_tokens":5}}),
                        serde_json::json!({"type":"message_stop"}),
                    ]);
                    let body = events
                        .iter()
                        .map(|event| {
                            format!(
                                "event: {}\ndata: {event}\n\n",
                                event["type"].as_str().unwrap()
                            )
                        })
                        .collect::<String>();
                    write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                    continue;
                }
                assert!(headers.starts_with("POST /v1/chat/completions "));
                requests.push(serde_json::from_slice(&body).unwrap());
                if requests.len() == 1 {
                    let body = "{\"error\":{\"message\":\"controlled retry\"}}";
                    write!(socket, "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                    continue;
                }
                let (text, input, output) = if requests.len() == 2 {
                    ("first answer", 20, 5)
                } else {
                    ("second answer", 30, 7)
                };
                let chunk = |delta: serde_json::Value,
                             finish: serde_json::Value,
                             usage: serde_json::Value| {
                    serde_json::json!({
                        "id":"acp53-fixture", "object":"chat.completion.chunk", "created":1, "model":"test",
                        "choices":[{"index":0,"delta":delta,"finish_reason":finish}], "usage":usage
                    })
                };
                let body = format!(
                    "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    chunk(
                        serde_json::json!({"role":"assistant","content":text}),
                        serde_json::Value::Null,
                        serde_json::Value::Null
                    ),
                    chunk(
                        serde_json::json!({}),
                        serde_json::json!("stop"),
                        serde_json::json!({"prompt_tokens":input,"completion_tokens":output,"total_tokens":input+output})
                    )
                );
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            requests
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
    fn finish(&mut self) -> Vec<serde_json::Value> {
        self.stop.store(true, Ordering::Release);
        self.handle.take().unwrap().join().unwrap()
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct Peer {
    child: Child,
    input: Option<ChildStdin>,
    messages: mpsc::Receiver<serde_json::Value>,
    readers: Vec<JoinHandle<()>>,
    notifications: Vec<serde_json::Value>,
    stderr: Arc<std::sync::Mutex<Vec<u8>>>,
}
impl Peer {
    fn start(roots: &Roots) -> Self {
        Self::start_with_read(roots, false)
    }
    fn start_with_read(roots: &Roots, read: bool) -> Self {
        Self::start_with_args(roots, read, &[])
    }
    fn start_with_args(roots: &Roots, read: bool, extra: &[&str]) -> Self {
        let tool_args: &[&str] = if read {
            &["--tools", "read", "--allow", "read:*"]
        } else {
            &["--no-tools"]
        };
        let mut child = roots
            .command()
            .args(tool_args)
            .args([
                "--acp",
                "--guarded",
                "--no-context-files",
                "--provider",
                "local-test",
                "--model",
                "test",
            ])
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (sender, messages) = mpsc::channel();
        let stderr_capture = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stderr_writer = stderr_capture.clone();
        let readers = vec![
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let line = line.unwrap();
                    assert!(line.len() <= 8 * 1024 * 1024);
                    if sender.send(serde_json::from_str(&line).unwrap()).is_err() {
                        break;
                    }
                }
            }),
            std::thread::spawn(move || {
                // Drain stderr independently; a pipe is never mistaken for retirement.
                let mut stderr = BufReader::new(stderr);
                let mut buffer = [0; 4096];
                loop {
                    let count = stderr.read(&mut buffer).unwrap();
                    if count == 0 {
                        break;
                    }
                    let mut captured = stderr_writer.lock().unwrap();
                    let remaining = (64 * 1024_usize).saturating_sub(captured.len());
                    captured.extend_from_slice(&buffer[..count.min(remaining)]);
                }
            }),
        ];
        Self {
            child,
            input,
            messages,
            readers,
            notifications: Vec::new(),
            stderr: stderr_capture,
        }
    }
    fn request(&mut self, id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
        let message = self.exchange(id, method, params);
        assert!(message.get("error").is_none(), "{message}");
        message["result"].clone()
    }
    fn exchange(&mut self, id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
        let input = self.input.as_mut().unwrap();
        writeln!(
            input,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )
        .unwrap();
        input.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let message = self
                .messages
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!(
                        "bounded ACP response {id} {method}: {error:?}; child={:?}; stderr={}",
                        self.child.try_wait(),
                        String::from_utf8_lossy(&self.stderr.lock().unwrap())
                    )
                });
            if message["id"] == id {
                return message;
            }
            self.notifications.push(message);
        }
    }
    fn finish(&mut self) {
        self.input.take();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "ACP process: {status}");
                break;
            }
            assert!(Instant::now() < deadline, "ACP did not retire after EOF");
            std::thread::sleep(Duration::from_millis(5));
        }
        for reader in self.readers.drain(..) {
            reader.join().unwrap();
        }
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.input.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

#[test]
#[cfg(feature = "multimodal")]
fn acp53_stdio_ordered_images_retry_private_save_load_and_literal_commands() {
    let roots = Roots::new();
    let mut provider = Provider::start(&roots);
    let mut peer = Peer::start(&roots);
    let initialized = peer.request(
        1,
        "initialize",
        serde_json::json!({"protocolVersion":1,"clientCapabilities":{}}),
    );
    assert_eq!(initialized["agentCapabilities"]["loadSession"], true);
    assert_eq!(
        initialized["agentCapabilities"]["promptCapabilities"]["image"],
        true
    );
    let id = peer.request(
        2,
        "session/new",
        serde_json::json!({"cwd":roots.0,"mcpServers":[]}),
    )["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let blocks = serde_json::json!([
        {"type":"text","text":"/rename must remain literal image context"},
        {"type":"image","data":PNG,"mimeType":"image/png","uri":"file:///must-not-open"},
        {"type":"text","text":"tail $HOME @never-read"}
    ]);
    assert_eq!(
        peer.request(
            3,
            "session/prompt",
            serde_json::json!({"sessionId":id,"prompt":blocks})
        )["stopReason"],
        "end_turn"
    );
    let saved_path = roots.0.join("sessions").join(format!("{id}.json"));
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&saved_path).unwrap()).unwrap();
    assert_eq!(saved["messages"][0]["user"]["blocks"][1]["data"], PNG);
    assert_eq!(
        saved["messages"][0]["user"]["blocks"][2]["text"],
        "tail $HOME @never-read"
    );
    assert!(saved["name"].as_str().unwrap_or_default() != "must remain literal image context");
    peer.request(4,"session/prompt",serde_json::json!({"sessionId":id,"prompt":[{"type":"text","text":"/rename literal saved name"}]}));
    peer.request(5, "session/close", serde_json::json!({"sessionId":id}));
    let mut imported: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&saved_path).unwrap()).unwrap();
    imported["permission_allowlist"] = serde_json::json!([{"tool":"write", "pattern":"**"}]);
    std::fs::write(&saved_path, serde_json::to_vec(&imported).unwrap()).unwrap();
    let list = peer.request(6, "session/list", serde_json::json!({"cwd":roots.0}));
    assert!(
        list["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["sessionId"] == id && row["title"] == "[literal saved name]"),
        "{list}"
    );
    let loaded = peer.request(
        7,
        "session/load",
        serde_json::json!({"sessionId":id,"cwd":roots.0,"mcpServers":[]}),
    );
    // Editors re-read the persona selector from the load response.
    assert!(
        loaded["configOptions"]
            .as_array()
            .is_some_and(|options| options
                .iter()
                .any(|option| option["id"] == "mode" && option["type"] == "select")),
        "{loaded}"
    );
    assert!(
        peer.notifications
            .iter()
            .any(|notice| notice["params"]["sessionId"] == id
                && notice["params"]["update"]["sessionUpdate"] == "user_message_chunk"
                && notice["params"]["update"]["content"]["type"] == "image")
    );
    assert_eq!(
        peer.request(
            8,
            "session/prompt",
            serde_json::json!({"sessionId":id,"prompt":[{"type":"text","text":"continue literal"}]})
        )["stopReason"],
        "end_turn"
    );
    peer.request(9, "session/close", serde_json::json!({"sessionId":id}));
    peer.finish();
    let requests = provider.finish();
    assert_eq!(
        requests.len(),
        3,
        "rename/list/load must not make provider requests"
    );
    let initial = &requests[0]["messages"].as_array().unwrap().last().unwrap()["content"];
    assert_eq!(
        initial[0]["text"],
        "/rename must remain literal image context"
    );
    assert_eq!(
        initial[1]["image_url"]["url"],
        format!("data:image/png;base64,{PNG}")
    );
    assert_eq!(initial[2]["text"], "tail $HOME @never-read");
    assert_eq!(
        requests[0]["messages"], requests[1]["messages"],
        "retry must preserve actual typed wire input"
    );
    let reloaded = requests[2]["messages"].as_array().unwrap();
    assert!(
        reloaded
            .iter()
            .any(|row| row["role"] == "user" && &row["content"] == initial),
        "load must retain original ordered typed history"
    );
    assert_eq!(reloaded.last().unwrap()["content"], "continue literal");
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&saved_path).unwrap()).unwrap();
    assert_eq!(saved["total_input_tokens"], 50);
    assert_eq!(saved["total_output_tokens"], 12);
    assert_eq!(saved["messages"].as_array().unwrap().len(), 4);
    assert_eq!(saved["permission_allowlist"], serde_json::json!([]));
}

#[test]
#[cfg(not(feature = "multimodal"))]
fn acp53_stdio_feature_off_refuses_input_and_saved_images_without_mutation_or_http() {
    let roots = Roots::new();
    let mut provider = Provider::start(&roots);
    let mut peer = Peer::start(&roots);
    let initialized = peer.request(
        1,
        "initialize",
        serde_json::json!({"protocolVersion":1,"clientCapabilities":{}}),
    );
    assert_eq!(
        initialized["agentCapabilities"]["promptCapabilities"]["image"],
        false
    );
    let id = peer.request(
        2,
        "session/new",
        serde_json::json!({"cwd":roots.0,"mcpServers":[]}),
    )["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let input = peer.exchange(3, "session/prompt", serde_json::json!({"sessionId":id,"prompt":[{"type":"image","data":PNG,"mimeType":"image/png"}]}));
    assert_eq!(input["error"]["code"], -32602);
    peer.request(4, "session/prompt", serde_json::json!({"sessionId":id,"prompt":[{"type":"text","text":"/rename feature off checkpoint"}]}));
    peer.request(5, "session/close", serde_json::json!({"sessionId":id}));
    let path = roots.0.join("sessions").join(format!("{id}.json"));
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    saved["messages"] = serde_json::json!([{"role":"user", "content":"before\nafter", "estimated_tokens":3,
        "user":{"blocks":[{"type":"text","text":"before"}, {"type":"image","data":PNG,"mime_type":"image/png"}, {"type":"text","text":"after"}]}}]);
    let original = serde_json::to_vec(&saved).unwrap();
    std::fs::write(&path, &original).unwrap();
    let foreign = roots.0.join("foreign-workspace");
    std::fs::create_dir(&foreign).unwrap();
    let wrong_root = peer.exchange(
        6,
        "session/load",
        serde_json::json!({"sessionId":id,"cwd":foreign,"mcpServers":[]}),
    );
    assert_eq!(wrong_root["error"]["code"], -32602);
    let extra_roots = peer.exchange(
        7,
        "session/new",
        serde_json::json!({"cwd":roots.0,"mcpServers":[],"additionalDirectories":[foreign]}),
    );
    assert_eq!(extra_roots["error"]["code"], -32602);
    let loaded = peer.exchange(
        8,
        "session/load",
        serde_json::json!({"sessionId":id,"cwd":roots.0,"mcpServers":[]}),
    );
    assert_eq!(loaded["error"]["code"], -32602);
    assert!(
        loaded["error"]["message"]
            .as_str()
            .unwrap()
            .contains("multimodal support is not compiled"),
        "{loaded}"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        original,
        "refusal must retain the original canonical typed row"
    );
    peer.finish();
    assert!(
        provider.finish().is_empty(),
        "neither unsupported input nor loaded history can reach the provider"
    );
}

#[test]
fn acp53_stdio_live_writer_refusal_does_not_fork_or_release_another_process() {
    let roots = Roots::new();
    let mut provider = Provider::start(&roots);
    let mut owner = Peer::start(&roots);
    owner.request(
        1,
        "initialize",
        serde_json::json!({"protocolVersion":1,"clientCapabilities":{}}),
    );
    let id = owner.request(
        2,
        "session/new",
        serde_json::json!({"cwd":roots.0,"mcpServers":[]}),
    )["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    owner.request(3, "session/prompt", serde_json::json!({"sessionId":id,"prompt":[{"type":"text","text":"/rename owned before competing load"}]}));
    let path = roots.0.join("sessions").join(format!("{id}.json"));
    let before = std::fs::read(&path).unwrap();
    let mut contender = Peer::start(&roots);
    contender.request(
        1,
        "initialize",
        serde_json::json!({"protocolVersion":1,"clientCapabilities":{}}),
    );
    let refused = contender.exchange(
        2,
        "session/load",
        serde_json::json!({"sessionId":id,"cwd":roots.0,"mcpServers":[]}),
    );
    assert_eq!(refused["error"]["code"], -32000, "{refused}");
    assert_eq!(std::fs::read(&path).unwrap(), before);
    contender.request(3, "session/close", serde_json::json!({"sessionId":id}));
    contender.finish();
    owner.request(4, "session/prompt", serde_json::json!({"sessionId":id,"prompt":[{"type":"text","text":"/rename owner still holds exact writer"}]}));
    owner.request(5, "session/close", serde_json::json!({"sessionId":id}));
    owner.finish();
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(saved["id"], id);
    assert_eq!(saved["name"], "owner still holds exact writer");
    assert_eq!(
        std::fs::read_dir(roots.0.join("sessions"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json"))
            .count(),
        1
    );
    assert!(
        provider.finish().is_empty(),
        "writer admission and local commands do not call a provider"
    );
}

#[test]
fn acp53_stdio_native_turns_publish_restore_and_refuse_prefix_rewrite() {
    let roots = Roots::new();
    std::fs::write(roots.0.join("native-read.txt"), "ACP53_OWNED_NATIVE_READ").unwrap();
    let mut provider = Provider::start_protocol(&roots, true);
    let mut peer = Peer::start_with_read(&roots, true);
    peer.request(
        1,
        "initialize",
        serde_json::json!({"protocolVersion":1,"clientCapabilities":{}}),
    );
    let id = peer.request(
        2,
        "session/new",
        serde_json::json!({"cwd":roots.0,"mcpServers":[]}),
    )["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    for (request, text) in [(3, "native first"), (4, "native second")] {
        assert_eq!(
            peer.request(
                request,
                "session/prompt",
                serde_json::json!({"sessionId":id,"prompt":[{"type":"text","text":text}]})
            )["stopReason"],
            "end_turn"
        );
    }
    let path = roots.0.join("sessions").join(format!("{id}.json"));
    let before = std::fs::read(&path).unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&before).unwrap();
    assert!(saved["native_history"].is_object(), "{saved}");
    assert_eq!(saved["total_input_tokens"], 60);
    assert_eq!(saved["total_output_tokens"], 15);
    let live_calls: Vec<_> = peer
        .notifications
        .iter()
        .map(|notice| &notice["params"]["update"])
        .filter(|update| update["sessionUpdate"] == "tool_call")
        .collect();
    assert_eq!(live_calls.len(), 1, "one actual read is dispatched");
    assert_eq!(live_calls[0]["title"], "read");
    assert_eq!(live_calls[0]["rawInput"]["path"], "native-read.txt");
    let live_id = live_calls[0]["toolCallId"].as_str().unwrap();
    assert!(!live_id.is_empty());
    assert_eq!(
        peer.notifications
            .iter()
            .map(|notice| &notice["params"]["update"])
            .filter(|update| update["sessionUpdate"] == "tool_call_update"
                && update["toolCallId"] == live_id
                && update["status"] == "completed"
                && update.to_string().contains("ACP53_OWNED_NATIVE_READ"))
            .count(),
        1
    );
    for (request, text) in [(5, "/undo"), (6, "/compact"), (7, "/rename changed native")] {
        assert!(
            peer.exchange(
                request,
                "session/prompt",
                serde_json::json!({"sessionId":id,"prompt":[{"type":"text","text":text}]})
            )
            .get("error")
            .is_some()
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "refused local changes preserve the native file"
        );
    }
    #[cfg(feature = "goal")]
    {
        assert!(
            peer.exchange(
                70,
                "session/prompt",
                serde_json::json!({"sessionId":id,"prompt":[{"type":"text","text":"/goal clear"}]})
            )
            .get("error")
            .is_some()
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "native goal changes cannot mutate saved context"
        );
    }
    peer.request(8, "session/close", serde_json::json!({"sessionId":id}));
    peer.finish();
    let mut resumed = Peer::start_with_read(&roots, true);
    resumed.request(
        1,
        "initialize",
        serde_json::json!({"protocolVersion":1,"clientCapabilities":{}}),
    );
    resumed.request(
        2,
        "session/load",
        serde_json::json!({"sessionId":id,"cwd":roots.0,"mcpServers":[]}),
    );
    // Live notifications use SDK lifecycle IDs; restored canonical records use
    // model call IDs. Both feeds must retain their own exact call/result pair.
    for kind in ["tool_call", "tool_call_update"] {
        assert_eq!(
            resumed
                .notifications
                .iter()
                .map(|notice| &notice["params"]["update"])
                .filter(|update| update["sessionUpdate"] == kind
                    && update["toolCallId"] == "acp53-native-read")
                .count(),
            1
        );
    }
    assert!(
        !resumed.notifications.iter().any(|notice| notice
            .to_string()
            .contains("opaque ACP native thought")
            || notice.to_string().contains("acp53-inert-signature")),
        "opaque native data is not editor display text"
    );
    assert_eq!(
        resumed.request(
            3,
            "session/prompt",
            serde_json::json!({"sessionId":id,"prompt":[{"type":"text","text":"native third"}]})
        )["stopReason"],
        "end_turn"
    );
    resumed.request(4, "session/close", serde_json::json!({"sessionId":id}));
    resumed.finish();
    let requests = provider.finish();
    assert_eq!(
        requests.len(),
        4,
        "load and local commands are not completions"
    );
    let tool_result = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|row| row["content"].as_array().into_iter().flatten())
        .find(|block| block["type"] == "tool_result")
        .unwrap();
    assert_eq!(tool_result["tool_use_id"], "acp53-native-read");
    assert!(tool_result.to_string().contains("ACP53_OWNED_NATIVE_READ"));
    let first_prefix = requests[2]["messages"].as_array().unwrap();
    assert!(
        first_prefix
            .iter()
            .any(|row| row["content"].as_array().is_some_and(|blocks| blocks
                .iter()
                .any(|block| block["signature"] == "acp53-inert-signature")))
    );
    // The signed assistant prefix is immutable. The prior request's final
    // unsigned User cache marker legitimately moves to the new conversation tail.
    assert_eq!(
        &requests[3]["messages"].as_array().unwrap()[..first_prefix.len() - 1],
        &first_prefix[..first_prefix.len() - 1]
    );
    assert!(
        requests[3]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| {
                row["role"] == "user"
                    && row["content"].as_array().is_some_and(|blocks| {
                        blocks.iter().any(|block| block["text"] == "native second")
                    })
            })
    );
    assert_eq!(requests[0]["system"], requests[1]["system"]);
    assert_eq!(requests[1]["system"], requests[2]["system"]);
    assert_eq!(requests[2]["system"], requests[3]["system"]);
    assert_eq!(requests[0]["tools"], requests[3]["tools"]);
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["total_input_tokens"], 80);
    assert_eq!(saved["total_output_tokens"], 20);
    let messages = saved["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 8);
    for role in ["tool_call", "tool_result"] {
        let rows: Vec<_> = messages.iter().filter(|row| row["role"] == role).collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["tool_call_id"], "acp53-native-read");
    }
}

/// Opt-in `headless_agent_mentions` over the real ACP wire: an unknown
/// canonical name is a -32602 invalid-params error before any provider
/// request; a resolved alias is normalised, sends the routing note as a final
/// block of that request only, and persists only the literal text.
#[test]
#[cfg(feature = "subagents")]
fn acp_agent_mentions_refuse_unknown_and_route_resolved_without_persisting() {
    let roots = Roots::new();
    let mut provider = Provider::start(&roots);
    let config = roots.0.join("config.toml");
    let base = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("headless_agent_mentions=true\n{base}")).unwrap();
    let mut peer = Peer::start(&roots);
    peer.request(
        1,
        "initialize",
        serde_json::json!({"protocolVersion":1,"clientCapabilities":{}}),
    );
    let id = peer.request(
        2,
        "session/new",
        serde_json::json!({"cwd":roots.0,"mcpServers":[]}),
    )["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let refused = peer.exchange(
        3,
        "session/prompt",
        serde_json::json!({"sessionId":id,"prompt":[{"type":"text","text":"run @agent:no-such-persona now"}]}),
    );
    assert_eq!(refused["error"]["code"], -32602, "{refused}");
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("unknown agent '@agent:no-such-persona'")
            && message.contains("rust-maintainer"),
        "{message}"
    );
    assert_eq!(
        peer.request(
            4,
            "session/prompt",
            serde_json::json!({"sessionId":id,"prompt":[{"type":"text","text":"ask @agent-rust-maintainer to review"}]})
        )["stopReason"],
        "end_turn"
    );
    peer.request(5, "session/close", serde_json::json!({"sessionId":id}));
    peer.finish();
    let requests = provider.finish();
    // The refusal made no request; the routed prompt's first attempt is the
    // fixture's controlled 500 and its retry must carry identical input.
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["messages"], requests[1]["messages"]);
    let user = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|row| row["role"] == "user")
        .unwrap();
    let texts: Vec<&str> = user["content"]
        .as_array()
        .unwrap_or_else(|| panic!("routed content is a block list: {user}"))
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect();
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert_eq!(texts[0], "ask @agent:rust-maintainer to review");
    assert!(
        texts[1].contains("Host routing note") && texts[1].contains("\"rust-maintainer\""),
        "{texts:?}"
    );
    let saved =
        std::fs::read_to_string(roots.0.join("sessions").join(format!("{id}.json"))).unwrap();
    assert!(
        saved.contains("ask @agent:rust-maintainer to review"),
        "{saved}"
    );
    assert!(!saved.contains("Host routing note"), "{saved}");
}

/// `/cron` over real stdio: v1 refusals, arming, and `--idle-exit` waiting
/// while a schedule is armed and exiting once it is stopped and the session
/// closed. (A fire needs the 1m minimum interval to elapse, so the scheduled
/// turn itself is covered by the in-process protocol test
/// `acp_cron_fire_runs_a_scheduled_turn_and_refuses_a_concurrent_prompt`.)
#[test]
fn acp_cron_stdio_refuses_v1_verbs_and_idle_exit_waits_for_the_armed_schedule() {
    let roots = Roots::new();
    let mut provider = Provider::start(&roots);
    let mut peer = Peer::start_with_args(&roots, false, &["--idle-exit", "1"]);
    peer.request(
        1,
        "initialize",
        serde_json::json!({"protocolVersion":1,"clientCapabilities":{}}),
    );
    let id = peer.request(
        2,
        "session/new",
        serde_json::json!({"cwd":roots.0,"mcpServers":[]}),
    )["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let prompt =
        |text: &str| serde_json::json!({"sessionId":id,"prompt":[{"type":"text","text":text}]});
    for (request, text, expected) in [
        (3, "/cron run c1", "/cron run is not available over ACP"),
        (4, "/cron resume", "/cron resume is not available over ACP"),
        (
            5,
            "/cron check the build",
            "self-paced /cron <prompt> is not available over ACP",
        ),
        (
            6,
            "/cron 5m /cost",
            "slash commands and shell (!) lines cannot be scheduled",
        ),
        (
            7,
            "/cron 5m !ls",
            "slash commands and shell (!) lines cannot be scheduled",
        ),
    ] {
        let refused = peer.exchange(request, "session/prompt", prompt(text));
        assert_eq!(refused["error"]["code"], -32602, "{text}: {refused}");
        let message = refused["error"]["message"].as_str().unwrap();
        assert!(message.contains(expected), "{text}: {message}");
    }
    assert_eq!(
        peer.request(8, "session/prompt", prompt("/cron 5m check the build"))["stopReason"],
        "end_turn"
    );
    let armed = peer.notifications.iter().any(|message| {
        message["method"] == "session/update"
            && message["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
            && message["params"]["update"]["content"]["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("cron c1: every 5m, first fire in 5m"))
    });
    assert!(armed, "{:?}", peer.notifications);
    // Several idle-exit intervals pass with the schedule armed.
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        peer.child.try_wait().unwrap().is_none(),
        "idle-exit must wait while a /cron schedule is armed"
    );
    assert_eq!(
        peer.request(9, "session/prompt", prompt("/cron list"))["stopReason"],
        "end_turn"
    );
    assert_eq!(
        peer.request(10, "session/prompt", prompt("/cron stop all"))["stopReason"],
        "end_turn"
    );
    peer.request(11, "session/close", serde_json::json!({"sessionId":id}));
    // No EOF: only the idle timer can end the process now.
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = peer.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "idle-exit did not fire after /cron stop all and session/close"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "ACP process: {status}");
    assert!(provider.finish().is_empty(), "no model request was made");
}
