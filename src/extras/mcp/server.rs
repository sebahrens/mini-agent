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
/// Most session ids the server remembers having issued (persisted; the
/// least recently used is dropped first).
const MAX_ISSUED_SESSIONS: usize = 4096;
/// Bytes of child stdout captured before the JSON envelope is parsed.
const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
/// Default for most `run` calls executing at once (`--mcp-server-max-runs`
/// overrides it); more are refused, not queued.
pub(crate) const MAX_CONCURRENT_RUNS: usize = 4;
/// Longest accepted `session_id` / `model` argument.
const MAX_ARG_BYTES: usize = 256;
/// Interval between progress heartbeats for requests with a progress token.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(15);
/// Protocol versions this server implements, newest first. `initialize`
/// answers the client's version only when listed here, else the newest.
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
/// How long a forwarded permission ask waits for the client's elicitation
/// answer before it is denied.
const ELICIT_WAIT: Duration = Duration::from_secs(300);
/// Longest ask text sent to the client; a longer request is denied unreviewed.
const MAX_ELICIT_MESSAGE_BYTES: usize = 64 * 1024;

/// The version to answer `initialize` with.
fn negotiate_protocol_version(requested: Option<&str>) -> &'static str {
    requested
        .and_then(|requested| {
            SUPPORTED_PROTOCOL_VERSIONS
                .iter()
                .copied()
                .find(|supported| *supported == requested)
        })
        .unwrap_or(SUPPORTED_PROTOCOL_VERSIONS[0])
}

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
    /// Whether a run's permission asks are forwarded to the client as MCP
    /// elicitation requests (default closed: asks fail).
    pub(crate) elicit_approvals: bool,
    /// The server's own workspace; a per-call `cwd` must stay inside it.
    pub(crate) workspace_root: PathBuf,
    /// Most `run` calls executing at once.
    pub(crate) max_runs: usize,
    /// Grace given to a timed-out child to save its session after SIGTERM.
    pub(crate) stop_grace: Duration,
    /// Session ids this server has returned (persisted across restarts per
    /// workspace); `run` resumes only these.
    pub(crate) issued_sessions: IssuedSessions,
}

/// Session ids handed to the client by this server (shared by clones). They
/// persist in a small bounded file in the state directory, keyed by workspace,
/// so a restarted server still resumes ids it issued earlier.
#[derive(Debug, Clone, Default)]
pub(crate) struct IssuedSessions(Arc<IssuedInner>);

#[derive(Debug, Default)]
struct IssuedInner {
    ids: std::sync::Mutex<std::collections::VecDeque<String>>,
    file: Option<PathBuf>,
}

/// Most bytes read from the persisted ledger.
const MAX_LEDGER_BYTES: u64 = 512 * 1024;

impl IssuedSessions {
    /// A ledger persisted at `file` (loaded now, rewritten on every change).
    pub(crate) fn persistent(file: PathBuf) -> Self {
        let ids = read_ledger(&file);
        Self(Arc::new(IssuedInner {
            ids: std::sync::Mutex::new(ids),
            file: Some(file),
        }))
    }

    /// The ledger for `workspace_root` under the process state directory;
    /// in-memory only when the state directory cannot be resolved.
    pub(crate) fn for_workspace(workspace_root: &std::path::Path) -> Self {
        use sha2::{Digest, Sha256};
        let Ok(paths) = crate::paths::process_paths_ref() else {
            return Self::default();
        };
        let canonical = workspace_root
            .canonicalize()
            .unwrap_or_else(|_| workspace_root.to_path_buf());
        let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
        let key: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
        Self::persistent(
            paths
                .state_dir
                .join("mcp-server")
                .join(format!("sessions-{key}.json")),
        )
    }

    pub(crate) fn record(&self, id: &str) {
        let mut ids = self.0.ids.lock().expect("issued sessions");
        self.merge_file(&mut ids);
        if let Some(at) = ids.iter().position(|known| known == id) {
            ids.remove(at);
        }
        ids.push_back(id.to_owned());
        while ids.len() > MAX_ISSUED_SESSIONS {
            ids.pop_front();
        }
        self.save(&ids);
    }

    /// Exact match only: never a prefix, name or foreign id. A hit counts as
    /// use, so ids a client keeps resuming are the last to be evicted.
    fn contains(&self, id: &str) -> bool {
        let mut ids = self.0.ids.lock().expect("issued sessions");
        if !ids.iter().any(|known| known == id) {
            // Another server process for the same workspace may have issued it.
            self.merge_file(&mut ids);
        }
        let Some(at) = ids.iter().position(|known| known == id) else {
            return false;
        };
        if at + 1 != ids.len() {
            let known = ids.remove(at).expect("position is in range");
            ids.push_back(known);
            self.save(&ids);
        }
        true
    }

    fn merge_file(&self, ids: &mut std::collections::VecDeque<String>) {
        let Some(file) = &self.0.file else { return };
        for id in read_ledger(file) {
            if !ids.iter().any(|known| *known == id) {
                ids.push_front(id);
            }
        }
        while ids.len() > MAX_ISSUED_SESSIONS {
            ids.pop_front();
        }
    }

    fn save(&self, ids: &std::collections::VecDeque<String>) {
        let Some(file) = &self.0.file else { return };
        let write = || -> std::io::Result<()> {
            if let Some(parent) = file.parent() {
                crate::paths::ensure_private_directory(parent)?;
            }
            let body = serde_json::to_vec(ids).map_err(std::io::Error::other)?;
            crate::fs::atomic_write_sync(file, &body)
        };
        if let Err(error) = write() {
            tracing::warn!(%error, "could not persist MCP server session ids");
        }
    }
}

/// Valid ids of a persisted ledger; a missing, oversized or malformed file is
/// an empty ledger (ids are then simply not resumable).
fn read_ledger(file: &std::path::Path) -> std::collections::VecDeque<String> {
    use std::io::Read as _;
    let mut text = String::new();
    let read = std::fs::File::open(file)
        .and_then(|f| f.take(MAX_LEDGER_BYTES + 1).read_to_string(&mut text));
    if read.is_err() || text.len() as u64 > MAX_LEDGER_BYTES {
        return Default::default();
    }
    let mut ids: std::collections::VecDeque<String> = serde_json::from_str::<Vec<String>>(&text)
        .unwrap_or_default()
        .into_iter()
        .filter(|id| valid_token(id))
        .collect();
    while ids.len() > MAX_ISSUED_SESSIONS {
        ids.pop_front();
    }
    ids
}

impl RunCommand {
    /// This executable with the server's own arguments, minus the
    /// `--mcp-server` mode flag, so each run honours the same flags.
    pub(crate) fn current(args: impl IntoIterator<Item = OsString>) -> std::io::Result<Self> {
        let args: Vec<OsString> = args.into_iter().collect();
        let workspace_root = std::env::current_dir()?;
        Ok(Self {
            program: std::env::current_exe()?,
            args: forwarded_args(args.clone()),
            timeout: valued_flag(&args, "--mcp-server-timeout")
                .map_or(RUN_TIMEOUT, Duration::from_secs),
            progress_interval: PROGRESS_INTERVAL,
            allow_model_override: has_flag(&args, "--mcp-server-allow-model-override"),
            allow_cwd_override: has_flag(&args, "--mcp-server-allow-cwd-override"),
            elicit_approvals: has_flag(&args, "--mcp-server-elicit-approvals"),
            issued_sessions: IssuedSessions::for_workspace(&workspace_root),
            workspace_root,
            max_runs: valued_flag(&args, "--mcp-server-max-runs")
                .map_or(MAX_CONCURRENT_RUNS, |runs| runs as usize),
            stop_grace: STOP_GRACE,
        })
    }

    fn argv(&self, run: &RunRequest, elicit: bool) -> Vec<OsString> {
        let mut argv = self.args.clone();
        if let Some(model) = &run.model {
            argv = strip_valued_flag(argv, "--model");
            argv.extend(["--model".into(), model.into()]);
        }
        if elicit {
            // The control channel lets the child ask this server to approve
            // an action; the server owns all of these flags.
            let mut owned = strip_valued_flag(argv, "--input-format");
            owned = strip_valued_flag(owned, "--stream-json-ask-timeout");
            owned.retain(|arg| arg.as_os_str() != "--stream-json-control");
            argv = owned;
            argv.extend(
                [
                    "--input-format",
                    "stream-json",
                    "--output",
                    "stream-json",
                    "--stream-json-control",
                    "--stream-json-ask-timeout",
                    "330",
                ]
                .map(OsString::from),
            );
        } else {
            argv.extend(["--output".into(), "json".into()]);
        }
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
                    && arg.as_os_str() != "--mcp-server-elicit-approvals"
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
            "`session_id` must be an id returned by an earlier `run` call of this server in this workspace (exact value; prefixes, names and other sessions are refused)"
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
        "session_id": {"type": "string", "description": "Exact session id returned by an earlier `run` of this server in this workspace (valid across server restarts); continues that conversation instead of starting a fresh one"}
    });
    if command.allow_model_override {
        properties["model"] = json!({"type": "string", "description": "Model name for this call, overriding the server's"});
    }
    if command.allow_cwd_override {
        properties["cwd"] = json!({"type": "string", "description": "Directory inside the server's workspace to run in (relative to the workspace root); defaults to the workspace root"});
    }
    json!({
        "name": "run",
        "description": format!("Run one mini-agent prompt headlessly in the server's workspace with the server's configured provider, model, sandbox and permission flags, and return its final answer plus a session id that can be passed back as `session_id` to continue. {} A run is stopped after {} s (--mcp-server-timeout) and at most {} runs execute at once (--mcp-server-max-runs); a timed-out run reports its stderr tail and, when it saved one, its session id.", if command.elicit_approvals { "When the client supports elicitation, a permission ask is forwarded to it for approval (declined, cancelled or unanswered asks are refused); other actions not allowed by those flags are refused." } else { "Permission prompts cannot be answered: actions not allowed by those flags are refused." }, command.timeout.as_secs(), command.max_runs),
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
async fn kill_tree(child: &mut Box<dyn process_wrap::tokio::ChildWrapper>, pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        crate::sandbox::kill_process_group(pid);
    }
    #[cfg(not(unix))]
    let _ = pid;
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}

/// Kills the run's process group when the run future is dropped (the
/// server aborts its tasks on client loss, before `run_prompt` can reach its
/// own cancel branch). `kill_on_drop` alone SIGKILLs only the direct child,
/// leaving sandboxed grandchildren and shell jobs behind.
struct GroupKillOnDrop(#[cfg_attr(not(unix), allow(dead_code))] Option<u32>);

impl Drop for GroupKillOnDrop {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0 {
            crate::sandbox::kill_process_group_if_live(pid);
        }
    }
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

/// Server-issued requests awaiting a client answer (elicitation), plus whether
/// the client advertised the capability at `initialize`.
#[derive(Default)]
struct ElicitBridge {
    client_supports: std::sync::atomic::AtomicBool,
    next: std::sync::atomic::AtomicU64,
    pending: std::sync::Mutex<HashMap<String, tokio::sync::oneshot::Sender<Value>>>,
}

impl ElicitBridge {
    /// Hands a client response to the request waiting for it.
    fn resolve(&self, id: &str, response: Value) {
        if let Some(waiter) = self.pending.lock().expect("elicit map").remove(id) {
            let _ = waiter.send(response);
        }
    }
}

/// A run's way to ask the client for approval.
#[derive(Clone)]
struct Elicit {
    bridge: Arc<ElicitBridge>,
    out: tokio::sync::mpsc::Sender<Value>,
}

/// Withdraws an unanswered request when its run stops waiting.
struct PendingAsk<'a> {
    elicit: &'a Elicit,
    id: String,
}

impl Drop for PendingAsk<'_> {
    fn drop(&mut self) {
        let removed = self
            .elicit
            .bridge
            .pending
            .lock()
            .expect("elicit map")
            .remove(&self.id)
            .is_some();
        if removed {
            let _ = self.elicit.out.try_send(json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": {"requestId": self.id, "reason": "mini-agent run stopped waiting"}
            }));
        }
    }
}

impl Elicit {
    /// Asks the client to approve one tool action. Only an explicit `accept`
    /// with `allow: true` approves; decline, cancel, an error, a malformed
    /// answer, a timeout or a closed connection all refuse.
    async fn approve(&self, tool: &str, input: &str) -> Result<(), String> {
        let message = format!("mini-agent wants to use `{tool}`. Allow this action?\n\n{input}");
        if message.len() > MAX_ELICIT_MESSAGE_BYTES {
            return Err("the request is too large to review, so it was refused".into());
        }
        let n = self
            .bridge
            .next
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = format!("mini-agent-elicit-{n}");
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.bridge
            .pending
            .lock()
            .expect("elicit map")
            .insert(id.clone(), tx);
        let guard = PendingAsk {
            elicit: self,
            id: id.clone(),
        };
        let sent = self.out.try_send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "elicitation/create",
            "params": {
                "message": message,
                "requestedSchema": {
                    "type": "object",
                    "properties": {
                        "allow": {"type": "boolean", "title": "Allow", "description": "Allow this single action"}
                    },
                    "required": ["allow"]
                }
            }
        }));
        if sent.is_err() {
            return Err("the MCP client connection is closed".into());
        }
        let response = match tokio::time::timeout(ELICIT_WAIT, rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => return Err("the approval request was dropped".into()),
            Err(_) => return Err("the client did not answer the approval request in time".into()),
        };
        drop(guard);
        let result = response.get("result");
        let accepted = result.and_then(|r| r.get("action")).and_then(Value::as_str)
            == Some("accept")
            && result
                .and_then(|r| r.get("content"))
                .and_then(|c| c.get("allow"))
                .and_then(Value::as_bool)
                == Some(true);
        if accepted {
            Ok(())
        } else {
            Err("the client declined the action".into())
        }
    }
}

/// Reads a `--output stream-json` child: answers its `can_use_tool` asks
/// through `elicit`, closes the child's stdin once the final `result` record
/// arrives (which ends its run), and returns that record as the captured
/// output.
async fn read_control_stream(
    stdout: impl AsyncRead + Unpin,
    mut stdin: Option<tokio::process::ChildStdin>,
    elicit: Elicit,
) -> (Vec<u8>, bool) {
    let mut lines = tokio::io::BufReader::new(stdout);
    let mut result = Vec::new();
    let mut truncated = false;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = (&mut lines)
            .take(MAX_CAPTURE_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)
            .await;
        match read {
            Ok(0) | Err(_) => return (result, truncated),
            Ok(_) if line.len() > MAX_CAPTURE_BYTES => {
                truncated = true;
                // Drain the rest of the oversized line.
                let mut sink = Vec::new();
                while !line.ends_with(b"\n") {
                    sink.clear();
                    match (&mut lines)
                        .take(64 * 1024)
                        .read_until(b'\n', &mut sink)
                        .await
                    {
                        Ok(0) | Err(_) => return (result, truncated),
                        Ok(_) => line.clone_from(&sink),
                    }
                }
                continue;
            }
            Ok(_) => {}
        }
        let Ok(record) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        match record.get("type").and_then(Value::as_str) {
            Some("result") => {
                result = line.clone();
                // Ending stdin lets the child finish and exit.
                stdin = None;
            }
            Some("control_request")
                if record.pointer("/request/subtype").and_then(Value::as_str)
                    == Some("can_use_tool") =>
            {
                let Some(request_id) = record.get("request_id").and_then(Value::as_str) else {
                    continue;
                };
                let tool = record
                    .pointer("/request/tool_name")
                    .and_then(Value::as_str)
                    .unwrap_or("a tool");
                let input = record
                    .pointer("/request/input")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let response = match elicit.approve(tool, input).await {
                    Ok(()) => json!({"behavior": "allow"}),
                    Err(message) => json!({"behavior": "deny", "message": message}),
                };
                let reply = json!({
                    "type": "control_response",
                    "response": {"subtype": "success", "request_id": request_id, "response": response}
                });
                if let Some(stdin) = stdin.as_mut() {
                    let mut bytes = serde_json::to_vec(&reply).unwrap_or_default();
                    bytes.push(b'\n');
                    if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
                        return (result, truncated);
                    }
                }
            }
            _ => {}
        }
    }
}

struct Progress {
    token: Value,
    out: tokio::sync::mpsc::Sender<Value>,
}

async fn run_prompt(
    command: &RunCommand,
    run: &RunRequest,
    mut cancel: tokio::sync::oneshot::Receiver<()>,
    progress: Option<Progress>,
    elicit: Option<Elicit>,
) -> RunOutcome {
    use crate::process_creation::CommandWrapCreationExt as _;
    let mut child = tokio::process::Command::new(&command.program);
    child
        .args(command.argv(run, elicit.is_some()))
        .stdin(if elicit.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &run.cwd {
        child.current_dir(cwd);
    }
    let mut owned_command = crate::sandbox::owned_workspace_service_tree(child);
    let mut child = match owned_command.spawn_guarded() {
        Ok(child) => child,
        Err(error) => {
            return RunOutcome::failed(format!("mini-agent run could not start: {error}"));
        }
    };
    let pid = child.id();
    let _group_guard = GroupKillOnDrop(pid);
    let control_stdin = if elicit.is_some() {
        child.stdin().take()
    } else {
        None
    };
    let stdout = child.stdout().take().expect("piped stdout");
    let stderr = child.stderr().take().expect("piped stderr");
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
            let reads = async {
                tokio::join!(
                    async {
                        match elicit.clone() {
                            Some(elicit) => {
                                read_control_stream(stdout, control_stdin, elicit).await
                            }
                            None => read_capped(stdout, MAX_CAPTURE_BYTES).await,
                        }
                    },
                    read_capped(stderr, MAX_STDERR_BYTES),
                )
            };
            tokio::pin!(reads);
            tokio::select! {
                status = child.inner_mut().wait() => {
                    // A descendant may still own the pipes after its leader
                    // exits. Retire the tree before waiting for pipe EOF.
                    let _ = child.start_kill();
                    match tokio::time::timeout(Duration::from_secs(1), &mut reads).await {
                        Ok((stdout, stderr)) => (stdout, stderr, status),
                        Err(_) => ((Vec::new(), true), (Vec::new(), true), Err(std::io::Error::other("MCP child output did not retire"))),
                    }
                }
                (stdout, stderr) = &mut reads => {
                    let status = child.inner_mut().wait().await;
                    let _ = child.start_kill();
                    (stdout, stderr, status)
                }
            }
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
                        let _ = progress.out.try_send(json!({
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
    // Tree retirement is independent of who still owns the capture pipes.
    kill_tree(&mut child, pid).await;
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
            let version =
                negotiate_protocol_version(params.get("protocolVersion").and_then(Value::as_str));
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

/// Serve until the client closes its end of the stream. End of input is
/// transport retirement: runs still executing are cancelled (and their process
/// groups killed) at once, since no client remains to read their answers.
/// Completed replies drain with a short deadline after active work retires.
pub(crate) async fn serve(
    command: RunCommand,
    input: impl AsyncRead + Send + Unpin + 'static,
    mut output: impl AsyncWrite + Unpin + Send + 'static,
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
    // A bounded, independently owned writer cannot block the request/cancel loop.
    let (reply_tx, mut reply_rx) = mpsc::channel::<Value>(32);
    let mut writer = tokio::spawn(async move {
        while let Some(reply) = reply_rx.recv().await {
            tokio::time::timeout(Duration::from_secs(5), write_message(&mut output, &reply))
                .await
                .map_err(|_| anyhow::anyhow!("MCP client stopped reading responses"))??;
        }
        Ok::<_, anyhow::Error>(())
    });
    let _reader_owner = AbortTaskOnDrop(reader.abort_handle());
    let _writer_owner = AbortTaskOnDrop(writer.abort_handle());
    let cancels = Arc::new(std::sync::Mutex::new(Cancels::new()));
    let bridge = Arc::new(ElicitBridge::default());
    let slots = Arc::new(tokio::sync::Semaphore::new(command.max_runs.max(1)));
    let mut tasks = tokio::task::JoinSet::new();
    let mut writer_finished = false;
    let outcome: anyhow::Result<()> = async {
        loop {
            tokio::select! {
                item = line_rx.recv() => {
                    let Some(item) = item else { return Ok(()) };
                    let Some(line) = item? else { return Ok(()) };
                    if line.iter().all(u8::is_ascii_whitespace) {
                        continue;
                    }
                    let message = match serde_json::from_slice::<Value>(&line) {
                        Ok(message) => message,
                        Err(_) => {
                            queue_reply(&reply_tx, error(&Value::Null, -32700, "parse error"))?;
                            continue;
                        }
                    };
                    if message.get("method").is_none()
                        && (message.get("result").is_some() || message.get("error").is_some())
                    {
                        // A client answer to a request this server issued.
                        if let Some(id) = message.get("id").and_then(Value::as_str) {
                            bridge.resolve(id, message.clone());
                        }
                        continue;
                    }
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
                    if method == "initialize" {
                        bridge.client_supports.store(
                            params.pointer("/capabilities/elicitation").is_some_and(Value::is_object),
                            std::sync::atomic::Ordering::Relaxed,
                        );
                    }
                    if method != "tools/call" {
                        queue_reply(&reply_tx, handle_simple(&command, &id, method, &params))?;
                        continue;
                    }
                    if params.get("name").and_then(Value::as_str) != Some("run") {
                        queue_reply(&reply_tx, error(&id, -32602, "unknown tool; only `run` is offered"))?;
                        continue;
                    }
                    let request = match parse_run_request(&command, params.get("arguments")) {
                        Ok(request) => request,
                        Err(message) => {
                            queue_reply(&reply_tx, error(&id, -32602, &message))?;
                            continue;
                        }
                    };
                    let Ok(permit) = slots.clone().try_acquire_owned() else {
                        queue_reply(&reply_tx, error(&id, -32000, &format!("too many concurrent `run` calls (limit {}, --mcp-server-max-runs); retry when one finishes", command.max_runs.max(1))))?;
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
                    let elicit = (command.elicit_approvals
                        && bridge.client_supports.load(std::sync::atomic::Ordering::Relaxed))
                    .then(|| Elicit { bridge: bridge.clone(), out: reply_tx.clone() });
                    let command = command.clone();
                    let reply_tx = reply_tx.clone();
                    let cancels = cancels.clone();
                    tasks.spawn(async move {
                        let outcome = run_prompt(&command, &request, cancel_rx, progress, elicit).await;
                        cancels.lock().expect("cancel map").remove(&key);
                        enqueue_run_reply(&reply_tx, &id, outcome, permit).await;
                    });
                }
                result = &mut writer => {
                    writer_finished = true;
                    return result.map_err(anyhow::Error::from)?;
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
    // Give cooperative cancellation its bounded tree wait before falling
    // back to drop ownership. Completed tasks blocked on replies cannot hold
    // EOF indefinitely, and the writer continues independently throughout.
    let retired = tokio::time::timeout(Duration::from_secs(5), async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    if retired.is_err() {
        tasks.shutdown().await;
    }
    reader.abort();
    drop(reply_tx);
    // Retirement of active work precedes any best-effort response drain.
    if !writer_finished {
        match tokio::time::timeout(Duration::from_millis(250), &mut writer).await {
            Ok(result) => {
                result??;
            }
            Err(_) => {
                writer.abort();
                let _ = writer.await;
            }
        }
    }
    outcome
}

/// Retain the run slot until its bounded reply queue owns the payload. A
/// blocked writer cannot turn completed replies into unlimited detached tasks.
async fn enqueue_run_reply(
    out: &tokio::sync::mpsc::Sender<Value>,
    id: &Value,
    outcome: RunOutcome,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    if let RunOutcome::Finished {
        text,
        is_error,
        session_id,
    } = outcome
    {
        let _ = out
            .send(result(
                id,
                tool_result(text, is_error, session_id.as_deref()),
            ))
            .await;
    }
    drop(permit);
}

struct AbortTaskOnDrop(tokio::task::AbortHandle);
impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn queue_reply(out: &tokio::sync::mpsc::Sender<Value>, message: Value) -> anyhow::Result<()> {
    out.try_send(message)
        .map_err(|_| anyhow::anyhow!("MCP response queue is full or closed"))
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
            elicit_approvals: false,
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
    async fn completed_run_keeps_its_slot_until_reply_enters_bounded_queue() {
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = slots.clone().acquire_owned().await.unwrap();
        let (out, mut replies) = tokio::sync::mpsc::channel(1);
        out.send(json!({"already":"queued"})).await.unwrap();
        let id = json!(1);
        let pending = enqueue_run_reply(
            &out,
            &id,
            RunOutcome::Finished {
                text: "completed payload".into(),
                is_error: false,
                session_id: None,
            },
            permit,
        );
        tokio::pin!(pending);
        assert!(matches!(
            futures::poll!(&mut pending),
            std::task::Poll::Pending
        ));
        assert!(
            slots.clone().try_acquire_owned().is_err(),
            "completed payload must still occupy its run slot"
        );
        replies.recv().await.unwrap();
        pending.await;
        assert!(slots.try_acquire_owned().is_ok());
        assert_eq!(replies.recv().await.unwrap()["id"], 1);
    }

    #[tokio::test]
    async fn successful_leader_exit_retires_descendants_that_inherit_capture_pipes() {
        let dir = crate::tests::OwnedTestDirectory::new("mcp-inherited-pipes");
        let marker = dir.path().join("descendant.pid");
        let script = format!(
            "sleep 300 & echo $! > '{}'; printf '%s' '{{\"result\":\"done\"}}'",
            marker.display()
        );
        let replies = exchange(shell(&script), &[call(1, json!({"prompt":"x"}))]).await;
        assert_eq!(replies[0]["result"]["isError"], false, "{replies:?}");
        assert_eq!(replies[0]["result"]["content"][0]["text"], "done");
    }

    #[tokio::test]
    async fn eof_retires_runs_while_stdout_is_backpressured() {
        let marker =
            std::env::temp_dir().join(format!("mcp-eof-backpressure-{}.pid", uuid::Uuid::new_v4()));
        let script = format!("sleep 300 & echo $! > '{}'; wait", marker.display());
        let (mut client_in, server_in) = tokio::io::duplex(1 << 16);
        let (server_out, mut client_out) = tokio::io::duplex(1);
        let mut server = tokio::spawn(serve(shell(&script), server_in, server_out));
        let mut bytes = serde_json::to_vec(&call(7, json!({"prompt":"x"}))).unwrap();
        bytes.push(b'\n');
        client_in.write_all(&bytes).await.unwrap();
        let grandchild = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&marker)
                    && let Ok(pid) = text.trim().parse::<i32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fixture child startup");
        client_in
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"ping\"}\n")
            .await
            .unwrap();
        // Prove dispatch is writing its reply, then leave stdout open but unread.
        let mut first = [0u8; 1];
        tokio::time::timeout(Duration::from_secs(2), client_out.read_exact(&mut first))
            .await
            .unwrap()
            .unwrap();
        drop(client_in);
        let retired = tokio::time::timeout(Duration::from_secs(2), &mut server)
            .await
            .is_ok();
        let child_live =
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(grandchild), None).is_ok();
        // Always retire the probe before asserting so the negative case leaks nothing.
        if !retired {
            server.abort();
            let _ = server.await;
        }
        let _ = std::fs::remove_file(&marker);
        assert!(
            retired && !child_live,
            "stdin EOF did not retire a backpressured server: retired={retired}, child_live={child_live}"
        );
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
    async fn client_loss_kills_the_whole_process_group_of_a_running_call() {
        let marker = std::env::temp_dir().join(format!(
            "mini-agent-mcp-lost-{}-{}.pid",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let script = format!("sleep 300 & echo $! > '{}'; wait", marker.display());
        let (mut client_in, server_in) = tokio::io::duplex(1 << 16);
        let (server_out, _client_out) = tokio::io::duplex(1 << 16);
        let server = tokio::spawn(serve(shell(&script), server_in, server_out));
        let mut bytes = serde_json::to_vec(&call(7, json!({"prompt":"x"}))).unwrap();
        bytes.push(b'\n');
        client_in.write_all(&bytes).await.unwrap();
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
        // A protocol failure (an oversize request line) ends serve() with the
        // run still going.
        let oversize = vec![b'x'; MAX_REQUEST_LINE_BYTES + 1];
        let _ = client_in.write_all(&oversize).await;
        assert!(server.await.unwrap().is_err());
        for _ in 0..200 {
            if nix::sys::signal::kill(nix::unistd::Pid::from_raw(grandchild), None).is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let survived = nix::sys::signal::kill(nix::unistd::Pid::from_raw(grandchild), None).is_ok();
        if survived {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(grandchild),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = std::fs::remove_file(&marker);
        assert!(!survived, "grandchild {grandchild} survived client loss");
    }

    /// Closing the client's input (EOF, no oversize line, no progress token so
    /// the silent run never writes) must retire the active run and its whole
    /// process group at once.
    #[tokio::test]
    async fn stdin_eof_retires_a_silent_running_call_and_its_process_group() {
        let marker = std::env::temp_dir().join(format!(
            "mini-agent-mcp-eof-{}-{}.pid",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let script = format!("sleep 300 & echo $! > '{}'; wait", marker.display());
        let (mut client_in, server_in) = tokio::io::duplex(1 << 16);
        let (server_out, _client_out) = tokio::io::duplex(1 << 16);
        let server = tokio::spawn(serve(shell(&script), server_in, server_out));
        let mut bytes = serde_json::to_vec(&call(7, json!({"prompt":"x"}))).unwrap();
        bytes.push(b'\n');
        client_in.write_all(&bytes).await.unwrap();
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
        drop(client_in);
        let served = tokio::time::timeout(Duration::from_secs(10), server).await;
        let alive =
            |pid: i32| nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok();
        for _ in 0..200 {
            if !alive(grandchild) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let survived = alive(grandchild);
        if survived {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(grandchild),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = std::fs::remove_file(&marker);
        assert!(served.is_ok(), "serve() did not return after stdin EOF");
        assert!(!survived, "grandchild {grandchild} survived stdin EOF");
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
                call(
                    2,
                    json!({"prompt":"p","session_id":"my-interactive-session"}),
                ),
                call(3, json!({"prompt":"p","session_id":"../other"})),
                call(
                    4,
                    json!({"prompt":"p","session_id":"0b9f6c1e-1111-2222-3333-444455556666"}),
                ),
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
        let command =
            shell(r#"printf '{"result":"ok","session_id":"issued-1","session_persisted":true}'"#);
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
        let replies = exchange(saves.clone(), &[call(1, json!({"prompt":"x"}))]).await;
        let result = &replies[0]["result"];
        let text = result["content"][0]["text"].as_str().unwrap();
        assert_eq!(result["isError"], true, "{text}");
        assert!(text.contains("timed out after"), "{text}");
        assert!(
            text.contains("saved-1") && text.contains("stopped at step 3"),
            "{text}"
        );
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
        assert!(
            text.contains("timed out") && text.contains("no session id"),
            "{text}"
        );
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
        let refused = replies
            .iter()
            .filter(|r| r["error"]["code"] == -32000)
            .count();
        assert_eq!(refused, 1, "{replies:?}");
        let listed = replies.iter().find(|r| r["id"] == 10).unwrap();
        let description = listed["result"]["tools"][0]["description"]
            .as_str()
            .unwrap();
        assert!(
            description.contains("90 s") && description.contains("at most 1 runs"),
            "{description}"
        );
        let args: Vec<OsString> = [
            "--mcp-server",
            "--mcp-server-timeout",
            "7",
            "--mcp-server-max-runs=2",
            "--read-only",
        ]
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

    #[test]
    fn protocol_version_is_negotiated_not_echoed() {
        assert_eq!(negotiate_protocol_version(Some("2025-06-18")), "2025-06-18");
        assert_eq!(negotiate_protocol_version(Some("2024-11-05")), "2024-11-05");
        // Unknown, future, malformed or absent versions get the newest supported one.
        for requested in [Some("2099-01-01"), Some("garbage"), Some(""), None] {
            assert_eq!(
                negotiate_protocol_version(requested),
                SUPPORTED_PROTOCOL_VERSIONS[0]
            );
        }
    }

    #[tokio::test]
    async fn initialize_answers_a_supported_version_or_the_newest() {
        let init = |id, version: &str| json!({"jsonrpc":"2.0","id":id,"method":"initialize","params":{"protocolVersion":version,"capabilities":{},"clientInfo":{"name":"t","version":"0"}}});
        let replies = exchange(
            shell("true"),
            &[init(1, "2025-03-26"), init(2, "2099-12-31"), init(3, "x")],
        )
        .await;
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(replies[1]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(replies[2]["result"]["protocolVersion"], "2025-06-18");
    }

    #[test]
    fn issued_session_ids_survive_a_restart_and_eviction_keeps_recent_ones() {
        let dir = crate::tests::OwnedTestDirectory::new("mcp-issued-sessions");
        let file = dir.path().join("state/sessions.json");
        let first = IssuedSessions::persistent(file.clone());
        first.record("sess-1");
        first.record("sess-2");
        // A restarted server (fresh object, same file) still resumes them.
        let second = IssuedSessions::persistent(file.clone());
        assert!(second.contains("sess-1") && second.contains("sess-2"));
        assert!(!second.contains("sess"), "prefixes stay refused");
        assert!(!second.contains("other"));
        // Eviction drops the least recently used id; use refreshes an id.
        let memory = IssuedSessions::default();
        memory.record("sess-1");
        memory.record("sess-2");
        assert!(memory.contains("sess-1"));
        for index in 0..MAX_ISSUED_SESSIONS - 1 {
            memory.record(&format!("bulk-{index}"));
        }
        assert!(
            memory.contains("sess-1"),
            "recently used id survives eviction"
        );
        assert!(
            !memory.contains("sess-2"),
            "least recently used id is evicted"
        );
        // An oversized ledger loads only its newest MAX_ISSUED_SESSIONS ids.
        let ids: Vec<String> = (0..MAX_ISSUED_SESSIONS + 10)
            .map(|i| format!("id-{i}"))
            .collect();
        std::fs::write(&file, serde_json::to_vec(&ids).unwrap()).unwrap();
        let loaded = IssuedSessions::persistent(file.clone());
        assert!(
            !loaded.contains("id-0") && loaded.contains(&format!("id-{}", MAX_ISSUED_SESSIONS + 9))
        );
        // A ledger id that is not a valid token (a flag, a path trick) is never loaded.
        std::fs::write(&file, r#"["--yolo","ok-1","a b",5]"#).unwrap();
        assert!(!IssuedSessions::persistent(file.clone()).contains("--yolo"));
        std::fs::write(&file, "not json").unwrap();
        assert!(!IssuedSessions::persistent(file.clone()).contains("ok-1"));
        // Another server process's new id is picked up.
        let a = IssuedSessions::persistent(dir.path().join("shared.json"));
        let b = IssuedSessions::persistent(dir.path().join("shared.json"));
        a.record("from-a");
        assert!(b.contains("from-a"));
    }

    /// Drives one `run` against a fake stream-json child while playing the
    /// MCP client: initialize with `capabilities`, then answer the server's
    /// `elicitation/create` with `answer` (None = never answer a request).
    async fn elicit_roundtrip(
        elicit_approvals: bool,
        capabilities: Value,
        answer: Option<Value>,
    ) -> (Option<Value>, String) {
        let mut command = shell(
            r#"case "$*" in *stream-json*) ;; *) printf 'plain:%s' "$*"; exit 0;; esac
printf '%s\n' '{"type":"control_request","request_id":"perm_1","request":{"subtype":"can_use_tool","tool_name":"bash","input":"rm -rf build","input_bytes":12,"truncated":false}}'
read reply
case "$reply" in *'"behavior":"allow"'*) r=allowed;; *) r=denied;; esac
printf '{"type":"result","result":"%s","session_id":"s-elicit","session_persisted":true}\n' "$r""#,
        );
        command.elicit_approvals = elicit_approvals;
        let (mut client_in, server_in) = tokio::io::duplex(1 << 20);
        let (server_out, client_out) = tokio::io::duplex(1 << 20);
        let server = tokio::spawn(serve(command, server_in, server_out));
        let send = |message: Value| {
            let mut bytes = serde_json::to_vec(&message).unwrap();
            bytes.push(b'\n');
            bytes
        };
        client_in
            .write_all(&send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":capabilities,"clientInfo":{"name":"t","version":"0"}}})))
            .await
            .unwrap();
        client_in
            .write_all(&send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"run","arguments":{"prompt":"clean up"}}})))
            .await
            .unwrap();
        let mut lines = tokio::io::BufReader::new(client_out).lines();
        let mut elicitation = None;
        let text = loop {
            let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
                .await
                .expect("message in time")
                .unwrap()
                .expect("server closed early");
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["method"] == "elicitation/create" {
                elicitation = Some(message.clone());
                if let Some(answer) = &answer {
                    let reply = json!({"jsonrpc":"2.0","id":message["id"],"result":answer});
                    client_in.write_all(&send(reply)).await.unwrap();
                }
            } else if message["id"] == 2 {
                break message["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .to_owned();
            }
        };
        drop(client_in);
        server.await.unwrap().unwrap();
        (elicitation, text)
    }

    #[tokio::test]
    async fn a_permission_ask_is_forwarded_as_elicitation_and_only_an_explicit_accept_allows() {
        let caps = json!({"elicitation":{}});
        let (request, text) = elicit_roundtrip(
            true,
            caps.clone(),
            Some(json!({"action":"accept","content":{"allow":true}})),
        )
        .await;
        let request = request.expect("an elicitation request was sent");
        assert!(
            request["params"]["message"]
                .as_str()
                .unwrap()
                .contains("rm -rf build")
        );
        assert_eq!(
            request["params"]["requestedSchema"]["properties"]["allow"]["type"],
            "boolean"
        );
        assert!(text.starts_with("allowed"), "{text}");
        assert!(
            text.contains("s-elicit"),
            "session id still reported: {text}"
        );
        // Everything else refuses: decline, cancel, accept without or with a false allow.
        for answer in [
            json!({"action":"decline"}),
            json!({"action":"cancel"}),
            json!({"action":"accept"}),
            json!({"action":"accept","content":{"allow":false}}),
            json!({"action":"accept","content":{"allow":"true"}}),
            json!({"action":"allow","content":{"allow":true}}),
            json!("accept"),
        ] {
            let (_, text) = elicit_roundtrip(true, caps.clone(), Some(answer.clone())).await;
            assert!(text.starts_with("denied"), "{answer}: {text}");
        }
    }

    #[tokio::test]
    async fn approvals_are_not_forwarded_by_default_or_without_client_support() {
        // Flag off: the child is the ordinary one-shot JSON run even for a
        // client that supports elicitation.
        let (request, text) = elicit_roundtrip(false, json!({"elicitation":{}}), None).await;
        assert!(request.is_none());
        assert!(
            text.starts_with("plain:") && !text.contains("stream-json"),
            "{text}"
        );
        // Flag on but the client did not advertise elicitation.
        let (request, text) = elicit_roundtrip(true, json!({}), None).await;
        assert!(request.is_none());
        assert!(
            text.starts_with("plain:") && !text.contains("control"),
            "{text}"
        );
    }
}

/// Native Windows proof: retain the exact descendant handle and observe its
/// signal after each real MCP owner retires. No PID lookup after retirement.
#[cfg(all(test, windows))]
#[allow(unsafe_code)]
pub(crate) mod windows_tree_tests {
    use super::*;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, TerminateProcess,
        WaitForSingleObject,
    };

    pub(crate) struct Fixture {
        dir: crate::tests::OwnedTestDirectory,
        program: PathBuf,
        parent: PathBuf,
    }
    pub(crate) struct Descendant(OwnedHandle);
    impl Drop for Descendant {
        fn drop(&mut self) {
            // Failure cleanup uses only the retained process capability.
            unsafe {
                TerminateProcess(self.0.as_raw_handle(), 125);
                WaitForSingleObject(self.0.as_raw_handle(), 2_000);
            }
        }
    }
    impl Descendant {
        pub(crate) async fn assert_retired(&self, action: &str) {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    // SAFETY: this exact live process handle stays owned here.
                    match unsafe { WaitForSingleObject(self.0.as_raw_handle(), 0) } {
                        WAIT_OBJECT_0 => break,
                        WAIT_TIMEOUT => tokio::time::sleep(Duration::from_millis(10)).await,
                        other => {
                            panic!("MCP descendant observation failed after {action}: {other}")
                        }
                    }
                }
            })
            .await
            .unwrap_or_else(|error| {
                panic!("MCP descendant survived retirement after {action}: {error}")
            });
        }
    }
    impl Fixture {
        pub(crate) fn new() -> Self {
            Self::with_inherited_pipes(false)
        }
        pub(crate) fn inheriting_pipes() -> Self {
            Self::with_inherited_pipes(true)
        }
        fn with_inherited_pipes(inherited: bool) -> Self {
            let dir = crate::tests::OwnedTestDirectory::new("mcp-windows-tree");
            let program = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"))
                .join("System32/WindowsPowerShell/v1.0/powershell.exe");
            let quote = |path: &std::path::Path| {
                crate::sandbox::windows::child_process_path(path)
                    .display()
                    .to_string()
                    .replace('\'', "''")
            };
            let child = dir.path().join("child.ps1");
            std::fs::write(
                &child,
                format!(
                    "[IO.File]::WriteAllText('{}', $PID.ToString()); Start-Sleep -Seconds 300",
                    quote(&dir.path().join("child.pid"))
                ),
            )
            .unwrap();
            let parent = dir.path().join("parent.ps1");
            // UseShellExecute=false creates an ordinary descendant, without a
            // broker or shell escaping the Job. Output is separate owned files.
            let script = format!(
                r#"
$i=[System.Diagnostics.ProcessStartInfo]::new();
$i.FileName='{}';
$i.Arguments='-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "{}"';
$i.UseShellExecute=$false;
$i.RedirectStandardOutput=$true; $i.RedirectStandardError=$true;
$p=[System.Diagnostics.Process]::new(); $p.StartInfo=$i;
if(-not $p.Start()) {{ exit 9 }}
while(-not [IO.File]::Exists('{}')) {{ Start-Sleep -Milliseconds 10 }}
while(-not [IO.File]::Exists('{}')) {{ Start-Sleep -Milliseconds 10 }}
$bytes=[System.Text.Encoding]::UTF8.GetBytes('{{}}');
$stdout=[Console]::OpenStandardOutput();
$stdout.Write($bytes,0,$bytes.Length); $stdout.Flush();
"#,
                quote(&program),
                quote(&child),
                quote(&dir.path().join("child.pid")),
                quote(&dir.path().join("release"))
            );
            let script = if inherited {
                script.replace(
                    "$i.RedirectStandardOutput=$true; $i.RedirectStandardError=$true;",
                    "$i.RedirectStandardOutput=$false; $i.RedirectStandardError=$false;",
                )
            } else {
                script
            };
            std::fs::write(&parent, script).unwrap();
            Self {
                dir,
                program,
                parent,
            }
        }
        pub(crate) fn argv(&self) -> Vec<String> {
            vec![
                self.program.display().to_string(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                crate::sandbox::windows::child_process_path(&self.parent)
                    .display()
                    .to_string(),
            ]
        }
        pub(crate) fn root(&self) -> &std::path::Path {
            self.dir.path()
        }
        pub(crate) fn release(&self) {
            std::fs::write(self.dir.path().join("release"), "go").unwrap();
        }
        pub(crate) async fn ready(&self, action: &str) -> Descendant {
            let marker = self.dir.path().join("child.pid");
            let pid = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Ok(text) = std::fs::read_to_string(&marker)
                        && let Ok(pid) = text.parse::<u32>()
                    {
                        break pid;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap_or_else(|error| panic!("MCP descendant startup failed in {action}: {error}"));
            // SAFETY: marker belongs to this held fixture and its child is
            // blocked until release. Retain the handle before any retirement.
            let handle = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION
                        | PROCESS_TERMINATE
                        | windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE,
                    0,
                    pid,
                )
            };
            assert!(
                !handle.is_null(),
                "retain live fixture descendant in {action}: {}",
                std::io::Error::last_os_error()
            );
            let child = Descendant(unsafe { OwnedHandle::from_raw_handle(handle) });
            assert_eq!(
                unsafe { WaitForSingleObject(child.0.as_raw_handle(), 0) },
                WAIT_TIMEOUT,
                "fixture descendant must be live before {action}"
            );
            child
        }
        fn command(&self) -> RunCommand {
            let args = self.argv();
            RunCommand {
                program: args[0].clone().into(),
                args: args[1..].iter().map(Into::into).collect(),
                timeout: Duration::from_secs(15),
                progress_interval: Duration::from_secs(60),
                allow_model_override: false,
                allow_cwd_override: false,
                elicit_approvals: false,
                workspace_root: self.root().to_path_buf(),
                max_runs: 1,
                stop_grace: Duration::ZERO,
                issued_sessions: IssuedSessions::default(),
            }
        }
    }

    #[tokio::test]
    async fn mcp_stdio_server_owns_windows_descendants_on_success_timeout_cancel_and_drop() {
        for action in ["success", "success-inherited", "timeout", "cancel", "drop"] {
            let fixture = if action == "success-inherited" {
                Fixture::inheriting_pipes()
            } else {
                Fixture::new()
            };
            let command = fixture.command();
            let request = RunRequest {
                prompt: "x".into(),
                session_id: None,
                model: None,
                cwd: None,
            };
            let (cancel, cancelled) = tokio::sync::oneshot::channel();
            let task =
                tokio::spawn(
                    async move { run_prompt(&command, &request, cancelled, None, None).await },
                );
            let _task_owner = AbortTaskOnDrop(task.abort_handle());
            let descendant = fixture.ready(action).await;
            match action {
                "success" | "success-inherited" => fixture.release(),
                "cancel" => {
                    cancel
                        .send(())
                        .unwrap_or_else(|_| panic!("MCP run completed before {action}"));
                }
                "drop" => task.abort(),
                _ => {}
            }
            let result = tokio::time::timeout(Duration::from_secs(20), task)
                .await
                .unwrap_or_else(|error| panic!("MCP run failed to retire in {action}: {error}"));
            match action {
                "drop" => assert!(
                    matches!(result, Err(error) if error.is_cancelled()),
                    "MCP run drop must cancel its task"
                ),
                "cancel" => assert!(
                    matches!(
                        result.unwrap_or_else(|error| panic!(
                            "MCP run task failed in {action}: {error}"
                        )),
                        RunOutcome::Cancelled
                    ),
                    "MCP run cancel must return cancellation"
                ),
                "timeout" => match result
                    .unwrap_or_else(|error| panic!("MCP run task failed in {action}: {error}"))
                {
                    RunOutcome::Finished { is_error: true, .. } => {}
                    _ => panic!("MCP run timeout did not return a tool error"),
                },
                _ => match result
                    .unwrap_or_else(|error| panic!("MCP run task failed in {action}: {error}"))
                {
                    RunOutcome::Finished {
                        is_error: false, ..
                    } => {}
                    RunOutcome::Finished { text, .. } => panic!("MCP run {action} failed: {text}"),
                    RunOutcome::Cancelled => panic!("MCP run {action} was unexpectedly cancelled"),
                },
            }
            descendant.assert_retired(action).await;
        }
    }
}
