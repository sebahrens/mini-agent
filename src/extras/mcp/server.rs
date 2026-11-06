//! `mini-agent --mcp-server`: serve mini-agent itself over MCP on stdio.
//!
//! The server speaks newline-delimited JSON-RPC 2.0 and implements the
//! minimal tool-server surface (`initialize`, `ping`, `tools/list`,
//! `tools/call`). It exposes one tool, `run`, which executes a prompt
//! headlessly by launching this same executable as `mini-agent <server
//! flags> -p -- <prompt>`. The child therefore inherits exactly the
//! sandbox, permission, provider and tool flags the server was started with
//! and applies the ordinary headless policy (unmatched permission asks fail
//! closed). The server itself never runs model tools, reads files or opens
//! network connections.
//!
//! Requests are handled concurrently with reading: a long `run` never blocks
//! `ping`, other requests or `notifications/cancelled`. A cancelled (or
//! timed-out) run has its whole child process group killed and, per the MCP
//! cancellation utility, produces no response. Clients that send a
//! `progressToken` receive periodic `notifications/progress` heartbeats.
//! `run` returns the child's session id and accepts it back (`session_id`) to
//! continue that conversation. A per-call `model` override exists only when
//! the server was started with `--mcp-server-allow-model-override`.
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Longest JSON-RPC line accepted from the client.
const MAX_REQUEST_LINE_BYTES: usize = 4 * 1024 * 1024;
/// Longest prompt accepted by the `run` tool.
pub(crate) const MAX_PROMPT_BYTES: usize = 256 * 1024;
/// Most child stdout returned to the client.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// Most child stderr kept for a failure report.
const MAX_STDERR_BYTES: usize = 8 * 1024;
/// Bound on one headless run.
pub(crate) const RUN_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Bytes of child stdout captured before the JSON envelope is parsed.
const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
/// Most `run` calls executing at once; more are refused, not queued.
pub(crate) const MAX_CONCURRENT_RUNS: usize = 4;
/// Longest accepted `session_id` / `model` argument.
const MAX_ARG_BYTES: usize = 256;
/// Interval between progress heartbeats for requests with a progress token.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(15);
const PROTOCOL_VERSION: &str = "2025-06-18";

/// How a `run` call launches its headless child.
#[derive(Debug, Clone)]
pub(crate) struct RunCommand {
    pub(crate) program: PathBuf,
    /// Arguments placed before `-p -- <prompt>`.
    pub(crate) args: Vec<OsString>,
    pub(crate) timeout: Duration,
    pub(crate) progress_interval: Duration,
    /// Whether `run` may select a model per call (default closed).
    pub(crate) allow_model_override: bool,
}

impl RunCommand {
    /// This executable with the server's own arguments, minus the
    /// `--mcp-server` mode flag, so each run honours the same flags.
    pub(crate) fn current(args: impl IntoIterator<Item = OsString>) -> std::io::Result<Self> {
        let args: Vec<OsString> = args.into_iter().collect();
        Ok(Self {
            program: std::env::current_exe()?,
            args: forwarded_args(args.clone()),
            timeout: RUN_TIMEOUT,
            progress_interval: PROGRESS_INTERVAL,
            allow_model_override: allows_model_override(&args),
        })
    }

    fn argv(&self, run: &RunRequest) -> Vec<OsString> {
        let mut argv = self.args.clone();
        if let Some(model) = &run.model {
            argv = strip_valued_flag(argv, "--model");
            argv.extend(["--model".into(), model.into()]);
        }
        argv.extend(["--output".into(), "json".into()]);
        if let Some(session) = &run.session_id {
            argv.extend(["--session".into(), session.into()]);
        }
        argv.extend(["-p".into(), "--".into(), run.prompt.clone().into()]);
        argv
    }
}

/// Server arguments forwarded to each headless run. Only the mode flag is
/// removed; a stray prompt cannot be present because `--mcp-server`
/// conflicts with `-p` and positional messages.
pub(crate) fn forwarded_args(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    // The server owns the child's output format (it needs the JSON envelope
    // for the session id), so a forwarded `--output` is dropped.
    strip_valued_flag(
        args.into_iter()
            .filter(|arg| {
                arg.as_os_str() != "--mcp-server"
                    && arg.as_os_str() != "--mcp-server-allow-model-override"
            })
            .collect(),
        "--output",
    )
}

fn allows_model_override(args: &[OsString]) -> bool {
    args.iter()
        .any(|arg| arg.as_os_str() == "--mcp-server-allow-model-override")
}

/// Remove `--flag value` and `--flag=value` occurrences.
fn strip_valued_flag(args: Vec<OsString>, flag: &str) -> Vec<OsString> {
    let prefix = format!("{flag}=");
    let mut out = Vec::with_capacity(args.len());
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if arg.as_os_str() == flag {
            skip = true;
            continue;
        }
        if arg.to_string_lossy().starts_with(&prefix) {
            continue;
        }
        out.push(arg);
    }
    out
}

/// One validated `run` call.
#[derive(Debug, Clone)]
struct RunRequest {
    prompt: String,
    session_id: Option<String>,
    model: Option<String>,
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ARG_BYTES
        && !value.starts_with('-')
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/' | '@' | '+')
        })
}

fn parse_run_request(
    command: &RunCommand,
    arguments: Option<&Value>,
) -> Result<RunRequest, String> {
    let Some(prompt) = arguments
        .and_then(|args| args.get("prompt"))
        .and_then(Value::as_str)
        .filter(|prompt| !prompt.trim().is_empty())
    else {
        return Err("`run` requires a nonempty `prompt` string".into());
    };
    if prompt.len() > MAX_PROMPT_BYTES {
        return Err("`prompt` exceeds 256 KiB".into());
    }
    let optional = |name: &str| -> Result<Option<String>, String> {
        match arguments.and_then(|args| args.get(name)) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) if valid_token(value) => Ok(Some(value.clone())),
            Some(_) => Err(format!(
                "`{name}` must be a short string of letters, digits and . _ - : / @ + not starting with `-`"
            )),
        }
    };
    let session_id = optional("session_id")?;
    let model = optional("model")?;
    if model.is_some() && !command.allow_model_override {
        return Err(
            "per-call `model` is disabled; start the server with --mcp-server-allow-model-override"
                .into(),
        );
    }
    Ok(RunRequest {
        prompt: prompt.to_owned(),
        session_id,
        model,
    })
}

fn run_tool_definition(command: &RunCommand) -> Value {
    let mut properties = json!({
        "prompt": {"type": "string", "description": "The task for mini-agent"},
        "session_id": {"type": "string", "description": "Session id returned by an earlier `run`; continues that conversation instead of starting a fresh one"}
    });
    if command.allow_model_override {
        properties["model"] = json!({"type": "string", "description": "Model name for this call, overriding the server's"});
    }
    json!({
        "name": "run",
        "description": "Run one mini-agent prompt headlessly in the server's workspace with the server's configured provider, model, sandbox and permission flags, and return its final answer plus a session id that can be passed back as `session_id` to continue. Permission prompts cannot be answered: actions not allowed by those flags are refused.",
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": ["prompt"],
            "additionalProperties": false
        }
    })
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn tool_result(text: String, is_error: bool, session_id: Option<&str>) -> Value {
    let mut value = json!({"content": [{"type": "text", "text": text}], "isError": is_error});
    if let Some(session_id) = session_id {
        value["structuredContent"] = json!({"session_id": session_id});
    }
    value
}

/// Read at most `cap` bytes, then keep draining so the child never blocks.
async fn read_capped(mut reader: impl AsyncRead + Unpin, cap: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => return (kept, truncated),
            Ok(read) => {
                let room = cap.saturating_sub(kept.len());
                kept.extend_from_slice(&buffer[..read.min(room)]);
                truncated |= read > room;
            }
        }
    }
}

/// How one `run` ended.
enum RunOutcome {
    Finished {
        text: String,
        is_error: bool,
        session_id: Option<String>,
    },
    /// The client cancelled; the MCP cancellation utility says no response.
    Cancelled,
}

impl RunOutcome {
    fn failed(text: String) -> Self {
        Self::Finished {
            text,
            is_error: true,
            session_id: None,
        }
    }
}

/// Kill the child and its whole process group (sandboxed grandchildren
/// included on Unix).
async fn kill_tree(child: &mut tokio::process::Child, pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        crate::sandbox::kill_process_group(pid);
    }
    #[cfg(not(unix))]
    let _ = pid;
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}

fn truncate_utf8(text: &mut String, cap: usize) -> bool {
    if text.len() <= cap {
        return false;
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    true
}

/// Split the child's `--output json` envelope into answer text and session.
/// Anything that is not that envelope is returned as plain text.
fn parse_envelope(raw: &str) -> (String, Option<String>) {
    let envelope = raw
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter(|value| value.get("result").is_some_and(Value::is_string));
    let Some(envelope) = envelope else {
        return (raw.to_owned(), None);
    };
    let text = envelope["result"].as_str().unwrap_or_default().to_owned();
    let session = envelope
        .get("session_persisted")
        .and_then(Value::as_bool)
        .filter(|persisted| *persisted)
        .and_then(|_| envelope.get("session_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    (text, session)
}

struct Progress {
    token: Value,
    out: tokio::sync::mpsc::UnboundedSender<Value>,
}

async fn run_prompt(
    command: &RunCommand,
    run: &RunRequest,
    mut cancel: tokio::sync::oneshot::Receiver<()>,
    progress: Option<Progress>,
) -> RunOutcome {
    use crate::process_creation::TokioCommandCreationExt as _;
    let mut child = tokio::process::Command::new(&command.program);
    child
        .args(command.argv(run))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    child.process_group(0);
    let mut child = match child.spawn_guarded() {
        Ok(child) => child,
        Err(error) => {
            return RunOutcome::failed(format!("mini-agent run could not start: {error}"));
        }
    };
    let pid = child.id();
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    enum Stop {
        Done(
            (
                (Vec<u8>, bool),
                (Vec<u8>, bool),
                std::io::Result<std::process::ExitStatus>,
            ),
        ),
        TimedOut,
        Cancelled,
    }
    let started = tokio::time::Instant::now();
    let stop = {
        let work = async {
            tokio::join!(
                read_capped(stdout, MAX_CAPTURE_BYTES),
                read_capped(stderr, MAX_STDERR_BYTES),
                child.wait()
            )
        };
        tokio::pin!(work);
        let deadline = tokio::time::sleep(command.timeout);
        tokio::pin!(deadline);
        let cancelled = async {
            if (&mut cancel).await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        tokio::pin!(cancelled);
        let mut tick = tokio::time::interval_at(
            started + command.progress_interval,
            command.progress_interval,
        );
        let mut beat = 0_u64;
        loop {
            tokio::select! {
                done = &mut work => break Stop::Done(done),
                () = &mut deadline => break Stop::TimedOut,
                () = &mut cancelled => break Stop::Cancelled,
                _ = tick.tick(), if progress.is_some() => {
                    beat += 1;
                    if let Some(progress) = &progress {
                        let _ = progress.out.send(json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/progress",
                            "params": {
                                "progressToken": progress.token,
                                "progress": beat,
                                "message": format!("mini-agent run in progress ({} s elapsed)", started.elapsed().as_secs())
                            }
                        }));
                    }
                }
            }
        }
    };
    let ((stdout, captured_truncated), (stderr, _), status) = match stop {
        Stop::Done(done) => done,
        Stop::TimedOut => {
            kill_tree(&mut child, pid).await;
            return RunOutcome::failed(format!(
                "mini-agent run timed out after {} s and was stopped",
                command.timeout.as_secs()
            ));
        }
        Stop::Cancelled => {
            kill_tree(&mut child, pid).await;
            return RunOutcome::Cancelled;
        }
    };
    // A finished child may still have left descendants in its group.
    #[cfg(unix)]
    if let Some(pid) = pid {
        crate::sandbox::kill_process_group_if_live(pid);
    }
    let raw = String::from_utf8_lossy(&stdout).trim_end().to_owned();
    let (mut text, session_id) = if captured_truncated {
        (raw, None)
    } else {
        parse_envelope(&raw)
    };
    let truncated = captured_truncated | truncate_utf8(&mut text, MAX_OUTPUT_BYTES);
    if truncated {
        text.push_str(&format!(
            "\n[output truncated at {MAX_OUTPUT_BYTES} bytes; ask for a shorter answer or have the run write a file]"
        ));
    }
    if let Some(id) = &session_id {
        text.push_str(&format!("\n[session_id: {id}]"));
    }
    match status {
        Ok(status) if status.success() => RunOutcome::Finished {
            text,
            is_error: false,
            session_id,
        },
        status => {
            let code = status.ok().and_then(|status| status.code()).map_or_else(
                || "a signal".to_string(),
                |code| format!("exit code {code}"),
            );
            let stderr = String::from_utf8_lossy(&stderr);
            let mut report = format!("mini-agent run failed with {code}");
            if !text.is_empty() {
                report.push_str(&format!("\nstdout:\n{text}"));
            }
            if !stderr.trim().is_empty() {
                report.push_str(&format!("\nstderr:\n{}", stderr.trim_end()));
            }
            RunOutcome::Finished {
                text: report,
                is_error: true,
                session_id,
            }
        }
    }
}

/// Answer one non-`tools/call` JSON-RPC request.
fn handle_simple(command: &RunCommand, id: &Value, method: &str, params: &Value) -> Value {
    match method {
        "initialize" => {
            let version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION);
            result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "mini-agent", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
        }
        "ping" => result(id, json!({})),
        "tools/list" => result(id, json!({"tools": [run_tool_definition(command)]})),
        _ => error(id, -32601, "method not found"),
    }
}

type Cancels = HashMap<String, tokio::sync::oneshot::Sender<()>>;

fn id_key(id: &Value) -> String {
    id.to_string()
}

/// Serve until the client closes its end of the stream. Runs still executing
/// at end of input are allowed to finish and answer; a failed write (client
/// gone) cancels them.
pub(crate) async fn serve(
    command: RunCommand,
    input: impl AsyncRead + Send + Unpin + 'static,
    mut output: impl AsyncWrite + Unpin,
) -> anyhow::Result<()> {
    use tokio::sync::mpsc;
    let (line_tx, mut line_rx) = mpsc::channel::<anyhow::Result<Option<Vec<u8>>>>(8);
    let reader = tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(input);
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = (&mut lines)
                .take(MAX_REQUEST_LINE_BYTES as u64 + 1)
                .read_until(b'\n', &mut line)
                .await;
            let item = match read {
                Err(error) => Err(error.into()),
                Ok(0) => Ok(None),
                Ok(_) if line.len() > MAX_REQUEST_LINE_BYTES => Err(anyhow::anyhow!(
                    "MCP request line exceeds {MAX_REQUEST_LINE_BYTES} bytes"
                )),
                Ok(_) => Ok(Some(line.clone())),
            };
            let stop = !matches!(item, Ok(Some(_)));
            if line_tx.send(item).await.is_err() || stop {
                return;
            }
        }
    });
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<Value>();
    let cancels = Arc::new(std::sync::Mutex::new(Cancels::new()));
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_RUNS));
    let mut tasks = tokio::task::JoinSet::new();
    let mut input_open = true;
    let outcome: anyhow::Result<()> = async {
        loop {
            if !input_open && tasks.is_empty() {
                // Calls answer through `reply_rx`; flush what is queued.
                while let Ok(reply) = reply_rx.try_recv() {
                    write_message(&mut output, &reply).await?;
                }
                return Ok(());
            }
            tokio::select! {
                item = line_rx.recv(), if input_open => {
                    let Some(item) = item else { input_open = false; continue };
                    let Some(line) = item? else { input_open = false; continue };
                    if line.iter().all(u8::is_ascii_whitespace) {
                        continue;
                    }
                    let message = match serde_json::from_slice::<Value>(&line) {
                        Ok(message) => message,
                        Err(_) => {
                            write_message(&mut output, &error(&Value::Null, -32700, "parse error")).await?;
                            continue;
                        }
                    };
                    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
                    let params = message.get("params").cloned().unwrap_or(Value::Null);
                    let Some(id) = message.get("id").cloned() else {
                        if method == "notifications/cancelled"
                            && let Some(request_id) = params.get("requestId")
                            && let Some(cancel) = cancels.lock().expect("cancel map").remove(&id_key(request_id))
                        {
                            let _ = cancel.send(());
                        }
                        continue;
                    };
                    if method != "tools/call" {
                        write_message(&mut output, &handle_simple(&command, &id, method, &params)).await?;
                        continue;
                    }
                    if params.get("name").and_then(Value::as_str) != Some("run") {
                        write_message(&mut output, &error(&id, -32602, "unknown tool; only `run` is offered")).await?;
                        continue;
                    }
                    let request = match parse_run_request(&command, params.get("arguments")) {
                        Ok(request) => request,
                        Err(message) => {
                            write_message(&mut output, &error(&id, -32602, &message)).await?;
                            continue;
                        }
                    };
                    let Ok(permit) = slots.clone().try_acquire_owned() else {
                        write_message(&mut output, &error(&id, -32000, &format!("too many concurrent `run` calls (limit {MAX_CONCURRENT_RUNS}); retry when one finishes"))).await?;
                        continue;
                    };
                    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
                    let key = id_key(&id);
                    cancels.lock().expect("cancel map").insert(key.clone(), cancel_tx);
                    let progress = params
                        .get("_meta")
                        .and_then(|meta| meta.get("progressToken"))
                        .filter(|token| token.is_string() || token.is_number())
                        .map(|token| Progress { token: token.clone(), out: reply_tx.clone() });
                    let command = command.clone();
                    let reply_tx = reply_tx.clone();
                    let cancels = cancels.clone();
                    tasks.spawn(async move {
                        let outcome = run_prompt(&command, &request, cancel_rx, progress).await;
                        cancels.lock().expect("cancel map").remove(&key);
                        drop(permit);
                        if let RunOutcome::Finished { text, is_error, session_id } = outcome {
                            let _ = reply_tx.send(result(&id, tool_result(text, is_error, session_id.as_deref())));
                        }
                    });
                }
                Some(reply) = reply_rx.recv() => {
                    write_message(&mut output, &reply).await?;
                }
                Some(joined) = tasks.join_next(), if !tasks.is_empty() => {
                    let _ = joined;
                }
            }
        }
    }
    .await;
    // Client gone or protocol failure: stop every run still going.
    for (_, cancel) in cancels.lock().expect("cancel map").drain() {
        let _ = cancel.send(());
    }
    tasks.shutdown().await;
    reader.abort();
    outcome
}

async fn write_message(
    output: &mut (impl AsyncWrite + Unpin),
    message: &Value,
) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(message)?;
    bytes.push(b'\n');
    output.write_all(&bytes).await?;
    output.flush().await?;
    Ok(())
}

/// Entry point for `--mcp-server`: serve on the process's stdio.
pub(crate) async fn serve_stdio() -> anyhow::Result<()> {
    let command = RunCommand::current(std::env::args_os().skip(1))?;
    serve(command, tokio::io::stdin(), tokio::io::stdout()).await
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Send `requests`, wait until every one with an id has been answered
    /// (plus `extra` further messages), then close the input.
    async fn exchange_with(command: RunCommand, requests: &[Value], extra: usize) -> Vec<Value> {
        let expected = requests.iter().filter(|r| r.get("id").is_some()).count() + extra;
        let (mut client_in, server_in) = tokio::io::duplex(1 << 20);
        let (server_out, client_out) = tokio::io::duplex(1 << 20);
        let server = tokio::spawn(serve(command, server_in, server_out));
        for request in requests {
            let mut bytes = serde_json::to_vec(request).unwrap();
            bytes.push(b'\n');
            client_in.write_all(&bytes).await.unwrap();
        }
        let mut lines = tokio::io::BufReader::new(client_out).lines();
        let mut replies = Vec::new();
        while replies
            .iter()
            .filter(|r: &&Value| r.get("id").is_some())
            .count()
            < expected
        {
            let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
                .await
                .expect("reply in time")
                .unwrap()
                .expect("server closed early");
            replies.push(serde_json::from_str(&line).unwrap());
        }
        drop(client_in);
        server.await.unwrap().unwrap();
        replies
    }

    async fn exchange(command: RunCommand, requests: &[Value]) -> Vec<Value> {
        let mut replies = exchange_with(command, requests, 0).await;
        // Calls run concurrently, so replies arrive in completion order.
        replies.sort_by_key(|r| r["id"].as_i64().unwrap_or(0));
        replies
    }

    fn shell(script: &str) -> RunCommand {
        RunCommand {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into(), "sh".into()],
            timeout: Duration::from_secs(10),
            progress_interval: Duration::from_secs(60),
            allow_model_override: false,
        }
    }

    #[tokio::test]
    async fn mcp_server_initializes_lists_run_and_executes_a_headless_child() {
        // The child sees the forwarded flags, then `-p -- <prompt>` literally.
        let replies = exchange(
            shell(r#"printf 'argv:%s' "$*""#),
            &[
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}),
                json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
                json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"run","arguments":{"prompt":"--yolo $(touch x)"}}}),
                json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"shell","arguments":{}}}),
                json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"run","arguments":{"prompt":" "}}}),
                json!({"jsonrpc":"2.0","id":6,"method":"resources/list"}),
            ],
        )
        .await;
        assert_eq!(replies.len(), 6, "{replies:?}");
        assert_eq!(replies[0]["result"]["serverInfo"]["name"], "mini-agent");
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(replies[1]["result"]["tools"][0]["name"], "run");
        assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 1);
        assert_eq!(replies[2]["result"]["isError"], false);
        assert_eq!(
            replies[2]["result"]["content"][0]["text"],
            "argv:--output json -p -- --yolo $(touch x)"
        );
        assert_eq!(replies[3]["error"]["code"], -32602);
        assert_eq!(replies[4]["error"]["code"], -32602);
        assert_eq!(replies[5]["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn mcp_server_reports_failed_and_timed_out_runs_as_tool_errors() {
        let replies = exchange(
            shell("echo partial; echo 'permission denied' >&2; exit 3"),
            &[json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"run","arguments":{"prompt":"x"}}})],
        )
        .await;
        let text = replies[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(replies[0]["result"]["isError"], true);
        assert!(
            text.contains("exit code 3") && text.contains("partial"),
            "{text}"
        );
        assert!(text.contains("permission denied"), "{text}");

        let mut slow = shell("sleep 30");
        slow.timeout = Duration::from_millis(200);
        let replies = exchange(
            slow,
            &[json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"run","arguments":{"prompt":"x"}}})],
        )
        .await;
        assert_eq!(replies[0]["result"]["isError"], true);
        assert!(
            replies[0]["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("timed out")
        );
    }

    fn call(id: i64, args: Value) -> Value {
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"run","arguments":args}})
    }

    #[tokio::test]
    async fn ping_is_answered_and_cancel_kills_the_whole_process_group_without_a_reply() {
        let marker = std::env::temp_dir().join(format!(
            "mini-agent-mcp-cancel-{}-{}.pid",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let script = format!("sleep 300 & echo $! > '{}'; wait", marker.display());
        let (mut client_in, server_in) = tokio::io::duplex(1 << 16);
        let (server_out, client_out) = tokio::io::duplex(1 << 16);
        let server = tokio::spawn(serve(shell(&script), server_in, server_out));
        let mut lines = tokio::io::BufReader::new(client_out).lines();
        let send = |value: Value| {
            let mut bytes = serde_json::to_vec(&value).unwrap();
            bytes.push(b'\n');
            bytes
        };
        client_in
            .write_all(&send(call(7, json!({"prompt":"x"}))))
            .await
            .unwrap();
        // Wait for the grandchild to exist, then ping while the run is live.
        for _ in 0..200 {
            if marker.exists() && !std::fs::read_to_string(&marker).unwrap().trim().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let grandchild: i32 = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        client_in
            .write_all(&send(json!({"jsonrpc":"2.0","id":8,"method":"ping"})))
            .await
            .unwrap();
        let pong: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(pong["id"], 8);
        client_in
            .write_all(&send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}})))
            .await
            .unwrap();
        // The cancelled call gets no reply: the next message is the ping below.
        client_in
            .write_all(&send(json!({"jsonrpc":"2.0","id":9,"method":"ping"})))
            .await
            .unwrap();
        let next: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(
            next["id"], 9,
            "cancelled request must not be answered: {next}"
        );
        for _ in 0..200 {
            if nix::sys::signal::kill(nix::unistd::Pid::from_raw(grandchild), None).is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(grandchild), None).is_err(),
            "grandchild {grandchild} survived cancellation"
        );
        drop(client_in);
        server.await.unwrap().unwrap();
        let _ = std::fs::remove_file(&marker);
    }

    #[tokio::test]
    async fn run_emits_progress_for_a_token_and_returns_the_continuation_session_id() {
        let mut command = shell(
            r#"sleep 1; printf '%s\n' '{"result":"done","session_id":"abc123","session_persisted":true}'"#,
        );
        command.progress_interval = Duration::from_millis(200);
        let mut request = call(1, json!({"prompt":"x"}));
        request["params"]["_meta"] = json!({"progressToken":"tok"});
        let replies = exchange_with(command, &[request], 0).await;
        assert!(replies.len() >= 2, "{replies:?}");
        let progress = replies
            .iter()
            .find(|r| r["method"] == "notifications/progress")
            .expect("progress notification");
        assert_eq!(progress["params"]["progressToken"], "tok");
        let done = replies.iter().find(|r| r["id"] == 1).unwrap();
        assert_eq!(done["result"]["structuredContent"]["session_id"], "abc123");
        let text = done["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.starts_with("done") && text.contains("[session_id: abc123]"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn unpersisted_sessions_are_not_offered_and_session_id_is_passed_to_the_child() {
        let replies = exchange(
            shell(r#"printf '{"result":"%s","session_id":"s","session_persisted":false}' "$*""#),
            &[call(1, json!({"prompt":"p","session_id":"sess-1"}))],
        )
        .await;
        let text = replies[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("--session sess-1 -p -- p"), "{text}");
        assert!(!text.contains("[session_id"), "{text}");
        assert!(replies[0]["result"].get("structuredContent").is_none());
    }

    #[tokio::test]
    async fn crafted_session_and_model_arguments_are_rejected_and_model_override_is_default_closed()
    {
        let bad = [
            json!({"prompt":"p","session_id":"--yolo"}),
            json!({"prompt":"p","session_id":"a b"}),
            json!({"prompt":"p","session_id":"x".repeat(300)}),
            json!({"prompt":"p","session_id":42}),
            json!({"prompt":"p","session_id":"a\nb"}),
            json!({"prompt":"p","model":"gpt"}),
        ];
        let requests: Vec<Value> = bad
            .into_iter()
            .enumerate()
            .map(|(i, a)| call(i as i64 + 1, a))
            .collect();
        let replies = exchange(shell("echo ran"), &requests).await;
        assert!(
            replies.iter().all(|r| r["error"]["code"] == -32602),
            "{replies:?}"
        );
        let listed = exchange(
            shell("true"),
            &[json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})],
        )
        .await;
        assert!(
            listed[0]["result"]["tools"][0]["inputSchema"]["properties"]
                .get("model")
                .is_none()
        );

        let mut open = shell(r#"printf '%s' "$*""#);
        open.allow_model_override = true;
        let replies = exchange(open, &[call(1, json!({"prompt":"p","model":"m-1"}))]).await;
        let text = replies[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("--model m-1 --output json"), "{text}");
    }

    #[tokio::test]
    async fn concurrent_runs_beyond_the_limit_are_refused_not_queued() {
        let requests: Vec<Value> = (1..=(MAX_CONCURRENT_RUNS as i64 + 1))
            .map(|i| call(i, json!({"prompt":"x"})))
            .collect();
        let replies = exchange(shell("sleep 1; echo ok"), &requests).await;
        let refused = replies
            .iter()
            .filter(|r| r["error"]["code"] == -32000)
            .count();
        assert_eq!(refused, 1, "{replies:?}");
    }

    #[test]
    fn forwarded_args_drop_output_and_override_flags_but_keep_the_rest() {
        let forwarded = forwarded_args(
            [
                "--output",
                "text",
                "--mcp-server",
                "--mcp-server-allow-model-override",
                "--output=json",
                "--read-only",
            ]
            .map(OsString::from),
        );
        assert_eq!(forwarded, ["--read-only"].map(OsString::from));
        assert_eq!(
            strip_valued_flag(
                ["--model", "a", "--model=b", "-x"]
                    .map(OsString::from)
                    .to_vec(),
                "--model"
            ),
            ["-x"].map(OsString::from).to_vec()
        );
    }

    #[test]
    fn envelope_parsing_falls_back_to_plain_text_and_truncates_on_char_boundaries() {
        assert_eq!(parse_envelope("plain"), ("plain".into(), None));
        assert_eq!(parse_envelope("{\"x\":1}"), ("{\"x\":1}".into(), None));
        let mut text = "é".repeat(10);
        assert!(truncate_utf8(&mut text, 5));
        assert_eq!(text, "éé");
    }

    #[test]
    fn mcp_server_mode_is_off_by_default_and_excludes_print_prompts() {
        use clap::Parser as _;
        assert!(
            !crate::cli::Cli::try_parse_from(["mini-agent"])
                .unwrap()
                .mcp_server
        );
        assert!(
            crate::cli::Cli::try_parse_from(["mini-agent", "--mcp-server"])
                .unwrap()
                .mcp_server
        );
        assert!(crate::cli::Cli::try_parse_from(["mini-agent", "--mcp-server", "-p"]).is_err());
        assert!(crate::cli::Cli::try_parse_from(["mini-agent", "--mcp-server", "hello"]).is_err());
    }

    #[test]
    fn mcp_server_forwards_every_flag_except_its_own_mode_flag() {
        let forwarded = forwarded_args(
            ["--sandbox", "bwrap", "--mcp-server", "--read-only"].map(OsString::from),
        );
        assert_eq!(
            forwarded,
            ["--sandbox", "bwrap", "--read-only"].map(OsString::from)
        );
    }
}
