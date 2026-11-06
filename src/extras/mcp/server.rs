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
//! the server was started with `--mcp-server-allow-model-override`, and a
//! per-call `cwd` (a directory inside the server's workspace) only with
//! `--mcp-server-allow-cwd-override`.
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
/// Default bound on one headless run (`--mcp-server-timeout` overrides it).
pub(crate) const RUN_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How long a timed-out child gets, after SIGTERM, to persist its partial
/// transcript before the whole process group is killed.
const STOP_GRACE: Duration = Duration::from_secs(12);
/// Most session ids this server remembers having issued.
const MAX_ISSUED_SESSIONS: usize = 1024;
/// Bytes of child stdout captured before the JSON envelope is parsed.
const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
/// Default for most `run` calls executing at once (`--mcp-server-max-runs`
/// overrides it); more are refused, not queued.
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
    /// Whether `run` may select a working directory inside the workspace
    /// per call (default closed).
    pub(crate) allow_cwd_override: bool,
    /// The server's own workspace; a per-call `cwd` must stay inside it.
    pub(crate) workspace_root: PathBuf,
    /// Most `run` calls executing at once.
    pub(crate) max_runs: usize,
    /// Grace given to a timed-out child to save its session after SIGTERM.
    pub(crate) stop_grace: Duration,
    /// Session ids this server process has returned; `run` resumes only these.
    pub(crate) issued_sessions: IssuedSessions,
}

/// Session ids handed to the client by this server process (shared by clones).
#[derive(Debug, Clone, Default)]
pub(crate) struct IssuedSessions(Arc<std::sync::Mutex<std::collections::VecDeque<String>>>);

impl IssuedSessions {
    pub(crate) fn record(&self, id: &str) {
        let mut ids = self.0.lock().expect("issued sessions");
        if ids.iter().any(|known| known == id) {
            return;
        }
        if ids.len() >= MAX_ISSUED_SESSIONS {
            ids.pop_front();
        }
        ids.push_back(id.to_owned());
    }

    /// Exact match only: never a prefix, name or foreign id.
    fn contains(&self, id: &str) -> bool {
        self.0
            .lock()
            .expect("issued sessions")
            .iter()
            .any(|known| known == id)
    }
}

impl RunCommand {
    /// This executable with the server's own arguments, minus the
    /// `--mcp-server` mode flag, so each run honours the same flags.
    pub(crate) fn current(args: impl IntoIterator<Item = OsString>) -> std::io::Result<Self> {
        let args: Vec<OsString> = args.into_iter().collect();
        Ok(Self {
            program: std::env::current_exe()?,
            args: forwarded_args(args.clone()),
            timeout: valued_flag(&args, "--mcp-server-timeout")
                .map_or(RUN_TIMEOUT, Duration::from_secs),
            progress_interval: PROGRESS_INTERVAL,
            allow_model_override: has_flag(&args, "--mcp-server-allow-model-override"),
            allow_cwd_override: has_flag(&args, "--mcp-server-allow-cwd-override"),
            workspace_root: std::env::current_dir()?,
            max_runs: valued_flag(&args, "--mcp-server-max-runs")
                .map_or(MAX_CONCURRENT_RUNS, |runs| runs as usize),
            stop_grace: STOP_GRACE,
            issued_sessions: IssuedSessions::default(),
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
    let args = strip_valued_flag(
        args.into_iter()
            .filter(|arg| {
                arg.as_os_str() != "--mcp-server"
                    && arg.as_os_str() != "--mcp-server-allow-model-override"
                    && arg.as_os_str() != "--mcp-server-allow-cwd-override"
            })
            .collect(),
        "--output",
    );
    let args = strip_valued_flag(args, "--mcp-server-timeout");
    strip_valued_flag(args, "--mcp-server-max-runs")
}

/// Numeric value of `--flag N` / `--flag=N`, if present and valid.
fn valued_flag(args: &[OsString], flag: &str) -> Option<u64> {
    let prefix = format!("{flag}=");
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let text = arg.to_string_lossy();
        if text == flag {
            return iter.next()?.to_string_lossy().parse().ok();
        }
        if let Some(value) = text.strip_prefix(&prefix) {
            return value.parse().ok();
        }
    }
    None
}

fn has_flag(args: &[OsString], flag: &str) -> bool {
    args.iter().any(|arg| arg.as_os_str() == flag)
}

/// Resolve a per-call `cwd` to an existing directory inside `root`.
/// Relative paths are taken from `root`; symlinks and `..` are resolved
/// first, so a link or traversal that leaves the workspace is refused.
fn resolve_cwd(root: &std::path::Path, requested: &str) -> Result<PathBuf, String> {
    let refuse =
        || "`cwd` must name an existing directory inside the server's workspace".to_string();
    if requested.is_empty() || requested.len() > 4096 || requested.contains('\0') {
        return Err(refuse());
    }
    let root = root.canonicalize().map_err(|_| refuse())?;
    let resolved = root.join(requested).canonicalize().map_err(|_| refuse())?;
    if resolved.is_dir() && resolved.starts_with(&root) {
        Ok(resolved)
    } else {
        Err(refuse())
    }
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
    cwd: Option<PathBuf>,
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
    // The headless child treats a message starting with `!` as a user-typed
    // shell command that skips permission rules. A remote prompt is model
    // input only, so refuse it (the child trims the same way).
    if prompt.trim().starts_with('!') {
        return Err(
            "`prompt` must not start with `!`: that is the interactive shell-command prefix, which bypasses the permission flags; rephrase the task as plain text"
                .into(),
        );
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
    if let Some(id) = &session_id
        && !command.issued_sessions.contains(id)
    {
        return Err(
            "`session_id` must be an id returned by an earlier `run` call of this server process (exact value; prefixes, names and other sessions are refused)"
                .into(),
        );
    }
    let model = optional("model")?;
    if model.is_some() && !command.allow_model_override {
        return Err(
            "per-call `model` is disabled; start the server with --mcp-server-allow-model-override"
                .into(),
        );
    }
    let cwd = match arguments.and_then(|args| args.get("cwd")) {
        None | Some(Value::Null) => None,
        Some(_) if !command.allow_cwd_override => {
            return Err(
                "per-call `cwd` is disabled; start the server with --mcp-server-allow-cwd-override"
                    .into(),
            );
        }
        Some(Value::String(value)) => Some(resolve_cwd(&command.workspace_root, value)?),
        Some(_) => return Err("`cwd` must be a string".into()),
    };
    Ok(RunRequest {
        prompt: prompt.to_owned(),
        session_id,
        model,
        cwd,
    })
}

fn run_tool_definition(command: &RunCommand) -> Value {
    let mut properties = json!({
        "prompt": {"type": "string", "description": "The task for mini-agent, as plain text (a prompt starting with `!` is refused)"},
        "session_id": {"type": "string", "description": "Exact session id returned by an earlier `run` of this server process; continues that conversation instead of starting a fresh one"}
    });
    if command.allow_model_override {
        properties["model"] = json!({"type": "string", "description": "Model name for this call, overriding the server's"});
    }
    if command.allow_cwd_override {
        properties["cwd"] = json!({"type": "string", "description": "Directory inside the server's workspace to run in (relative to the workspace root); defaults to the workspace root"});
    }
    json!({
        "name": "run",
        "description": format!("Run one mini-agent prompt headlessly in the server's workspace with the server's configured provider, model, sandbox and permission flags, and return its final answer plus a session id that can be passed back as `session_id` to continue. Permission prompts cannot be answered: actions not allowed by those flags are refused. A run is stopped after {} s (--mcp-server-timeout) and at most {} runs execute at once (--mcp-server-max-runs); a timed-out run reports its stderr tail and, when it saved one, its session id.", command.timeout.as_secs(), command.max_runs),
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
    if let Some(cwd) = &run.cwd {
        child.current_dir(cwd);
    }
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
    type Captured = (
        (Vec<u8>, bool),
        (Vec<u8>, bool),
        std::io::Result<std::process::ExitStatus>,
    );
    enum Stop {
        Done(Captured),
        /// The deadline passed; the child saved what it had after SIGTERM.
        TimedOut(Captured),
        /// The deadline passed and the child ignored the graceful stop.
        TimedOutKilled,
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
                () = &mut deadline => {
                    // Ask the child alone to stop: its headless run then
                    // persists the partial transcript and prints its envelope.
                    #[cfg(unix)]
                    if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
                        let _ = nix::sys::signal::kill(
                            nix::unistd::Pid::from_raw(pid),
                            nix::sys::signal::Signal::SIGTERM,
                        );
                    }
                    break match tokio::time::timeout(command.stop_grace, &mut work).await {
                        Ok(done) => Stop::TimedOut(done),
                        Err(_) => Stop::TimedOutKilled,
                    };
                }
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
    let mut timed_out = false;
    let ((stdout, captured_truncated), (stderr, _), status) = match stop {
        Stop::Done(done) => done,
        Stop::TimedOut(done) => {
            timed_out = true;
            done
        }
        Stop::TimedOutKilled => {
            kill_tree(&mut child, pid).await;
            return RunOutcome::failed(format!(
                "mini-agent run timed out after {} s and was stopped; it did not finish saving, so there is no session id to continue",
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
        command.issued_sessions.record(id);
        text.push_str(&format!("\n[session_id: {id}]"));
    }
    if timed_out {
        let mut report = format!(
            "mini-agent run timed out after {} s and was stopped (--mcp-server-timeout)",
            command.timeout.as_secs()
        );
        match &session_id {
            Some(id) => report.push_str(&format!(
                "; the partial work was saved, pass session_id \"{id}\" to continue"
            )),
            None => report.push_str("; no session was saved"),
        }
        if !text.is_empty() {
            report.push_str(&format!("\nstdout:\n{text}"));
        }
        let stderr = String::from_utf8_lossy(&stderr);
        if !stderr.trim().is_empty() {
            report.push_str(&format!("\nstderr:\n{}", stderr.trim_end()));
        }
        return RunOutcome::Finished {
            text: report,
            is_error: true,
            session_id,
        };
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
    let slots = Arc::new(tokio::sync::Semaphore::new(command.max_runs.max(1)));
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
                        write_message(&mut output, &error(&id, -32000, &format!("too many concurrent `run` calls (limit {}, --mcp-server-max-runs); retry when one finishes", command.max_runs.max(1)))).await?;
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
            allow_cwd_override: false,
            workspace_root: std::env::temp_dir(),
            max_runs: MAX_CONCURRENT_RUNS,
            stop_grace: Duration::from_secs(5),
            issued_sessions: IssuedSessions::default(),
        }
    }

    fn shell_with_session(script: &str, issued: &[&str]) -> RunCommand {
        let command = shell(script);
        for id in issued {
            command.issued_sessions.record(id);
        }
        command
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
            shell_with_session(
                r#"printf '{"result":"%s","session_id":"s","session_persisted":false}' "$*""#,
                &["sess-1"],
            ),
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
            json!({"prompt":"p","session_id":"valid-but-never-issued"}),
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
    async fn bang_prompts_are_refused_and_never_reach_the_child() {
        let base = std::env::temp_dir().join(format!(
            "mini-agent-mcp-bang-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let marker = base.join("x");
        // If the child ever started it would create the marker.
        let command = shell(&format!("touch '{}'; echo ran", marker.display()));
        let prompts = [
            format!("!touch {}", marker.display()),
            format!("  \n\t!touch {}", marker.display()),
            format!("\u{a0}\u{2003}!touch {}", marker.display()),
            "!".to_string(),
        ];
        let requests: Vec<Value> = prompts
            .iter()
            .enumerate()
            .map(|(i, p)| call(i as i64 + 1, json!({"prompt": p})))
            .collect();
        let replies = exchange(command, &requests).await;
        assert!(
            replies.iter().all(|r| r["error"]["code"] == -32602
                && r["error"]["message"].as_str().unwrap().contains('!')),
            "{replies:?}"
        );
        assert!(!marker.exists(), "a `!` prompt reached the child");
        // A bang that is not leading is ordinary model text.
        let replies = exchange(shell("echo ok"), &[call(1, json!({"prompt":"say hi!"}))]).await;
        assert_eq!(replies[0]["result"]["isError"], false);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn only_session_ids_this_server_issued_can_be_resumed() {
        let command = shell_with_session(
            r#"printf '{"result":"%s","session_id":"s","session_persisted":false}' "$*""#,
            &["0b9f6c1e-1111-2222-3333-444455556666"],
        );
        let replies = exchange(
            command,
            &[
                // A prefix of a (foreign or own) session id, a name, and a path.
                call(1, json!({"prompt":"p","session_id":"0b9f"})),
                call(2, json!({"prompt":"p","session_id":"my-interactive-session"})),
                call(3, json!({"prompt":"p","session_id":"../other"})),
                call(4, json!({"prompt":"p","session_id":"0b9f6c1e-1111-2222-3333-444455556666"})),
            ],
        )
        .await;
        for refused in &replies[..3] {
            assert_eq!(refused["error"]["code"], -32602, "{refused}");
            assert!(
                refused["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("returned by an earlier `run`")
            );
        }
        assert_eq!(replies[3]["result"]["isError"], false, "{:?}", replies[3]);
    }

    #[tokio::test]
    async fn a_returned_session_id_is_remembered_for_the_next_call() {
        let command = shell(
            r#"printf '{"result":"ok","session_id":"issued-1","session_persisted":true}'"#,
        );
        let (mut client_in, server_in) = tokio::io::duplex(1 << 16);
        let (server_out, client_out) = tokio::io::duplex(1 << 16);
        let server = tokio::spawn(serve(command, server_in, server_out));
        let mut lines = tokio::io::BufReader::new(client_out).lines();
        for request in [
            call(1, json!({"prompt":"p"})),
            call(2, json!({"prompt":"p","session_id":"issued-1"})),
            call(3, json!({"prompt":"p","session_id":"issued"})),
        ] {
            let mut bytes = serde_json::to_vec(&request).unwrap();
            bytes.push(b'\n');
            client_in.write_all(&bytes).await.unwrap();
            let reply: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            match request["id"].as_i64().unwrap() {
                3 => assert_eq!(reply["error"]["code"], -32602, "{reply}"),
                _ => assert_eq!(reply["result"]["isError"], false, "{reply}"),
            }
        }
        drop(client_in);
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_timed_out_run_reports_stderr_and_the_session_it_saved() {
        // Like the real headless child: on SIGTERM it saves and prints its envelope.
        let mut saves = shell(
            r#"trap 'echo "stopped at step 3" >&2; printf "%s\n" "{\"result\":\"partial\",\"session_id\":\"saved-1\",\"session_persisted\":true}"; exit 130' TERM; sleep 30 >/dev/null 2>&1 & wait"#,
        );
        saves.timeout = Duration::from_millis(300);
        let replies = exchange(
            saves.clone(),
            &[call(1, json!({"prompt":"x"}))],
        )
        .await;
        let result = &replies[0]["result"];
        let text = result["content"][0]["text"].as_str().unwrap();
        assert_eq!(result["isError"], true, "{text}");
        assert!(text.contains("timed out after"), "{text}");
        assert!(text.contains("saved-1") && text.contains("stopped at step 3"), "{text}");
        assert_eq!(result["structuredContent"]["session_id"], "saved-1");
        // The reported id is now resumable.
        assert!(saves.issued_sessions.contains("saved-1"));

        // A child that ignores SIGTERM is killed after the grace; no id.
        let mut stubborn = shell(r#"trap '' TERM; while :; do sleep 1; done"#);
        stubborn.timeout = Duration::from_millis(200);
        stubborn.stop_grace = Duration::from_millis(300);
        let replies = exchange(stubborn, &[call(1, json!({"prompt":"x"}))]).await;
        let text = replies[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(replies[0]["result"]["isError"], true);
        assert!(text.contains("timed out") && text.contains("no session id"), "{text}");
    }

    #[tokio::test]
    async fn max_runs_and_timeout_are_configurable_and_advertised() {
        let mut command = shell("sleep 1; echo ok");
        command.max_runs = 1;
        command.timeout = Duration::from_secs(90);
        let replies = exchange(
            command.clone(),
            &[
                json!({"jsonrpc":"2.0","id":10,"method":"tools/list"}),
                call(1, json!({"prompt":"x"})),
                call(2, json!({"prompt":"x"})),
            ],
        )
        .await;
        let refused = replies.iter().filter(|r| r["error"]["code"] == -32000).count();
        assert_eq!(refused, 1, "{replies:?}");
        let listed = replies.iter().find(|r| r["id"] == 10).unwrap();
        let description = listed["result"]["tools"][0]["description"].as_str().unwrap();
        assert!(description.contains("90 s") && description.contains("at most 1 runs"), "{description}");
        let args: Vec<OsString> = ["--mcp-server", "--mcp-server-timeout", "7", "--mcp-server-max-runs=2", "--read-only"]
            .map(OsString::from)
            .to_vec();
        assert_eq!(valued_flag(&args, "--mcp-server-timeout"), Some(7));
        assert_eq!(valued_flag(&args, "--mcp-server-max-runs"), Some(2));
        assert_eq!(forwarded_args(args), ["--read-only"].map(OsString::from));
    }

    #[tokio::test]
    async fn cwd_override_is_default_closed_and_confined_to_the_workspace() {
        let base = std::env::temp_dir().join(format!(
            "mini-agent-mcp-cwd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = base.join("ws");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("file"), "x").unwrap();
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        // Default closed: not advertised, and any `cwd` is refused.
        let mut closed = shell("pwd");
        closed.workspace_root = root.clone();
        let closed_replies = exchange(
            closed,
            &[
                json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
                call(2, json!({"prompt":"p","cwd":"sub"})),
            ],
        )
        .await;
        assert!(
            closed_replies[0]["result"]["tools"][0]["inputSchema"]["properties"]
                .get("cwd")
                .is_none()
        );
        assert_eq!(closed_replies[1]["error"]["code"], -32602);

        // Opt-in: a subdirectory works; escapes, files and links are refused.
        let mut open = shell("pwd");
        open.allow_cwd_override = true;
        open.workspace_root = root.clone();
        let mut calls = vec![call(1, json!({"prompt":"p","cwd":"sub"}))];
        let mut bad = vec![
            json!("../outside"),
            json!(outside.to_str().unwrap()),
            json!("file"),
            json!("missing"),
            json!("sub/../../outside"),
            json!(""),
            json!(7),
        ];
        if cfg!(unix) {
            bad.push(json!("link"));
        }
        let bad_count = bad.len();
        for (i, cwd) in bad.into_iter().enumerate() {
            calls.push(call(i as i64 + 2, json!({"prompt":"p","cwd":cwd})));
        }
        let replies = exchange(open, &calls).await;
        let text = replies[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(
            std::path::Path::new(text.trim()),
            root.join("sub").canonicalize().unwrap()
        );
        assert_eq!(replies.len(), bad_count + 1);
        assert!(
            replies[1..].iter().all(|r| r["error"]["code"] == -32602),
            "{replies:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
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
                "--mcp-server-allow-cwd-override",
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
