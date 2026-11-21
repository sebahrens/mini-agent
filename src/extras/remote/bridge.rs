//! Session-side glue for the remote view: the pure, non-TUI logic the
//! interactive app uses to drive a [`RemoteServer`].
//!
//! The app owns one [`RemoteLink`] while `/remote on` is active. It feeds the
//! link every agent event ([`RemoteLink::observe`]) and the permission prompt
//! currently on screen ([`RemoteLink::sync_ask`]), and drains browser commands
//! with [`RemoteLink::recv`]. Nothing here executes a command: steering text,
//! permission answers and cancel requests are returned to the app, which routes
//! them through the same steering queue, permission prompt and interrupt path
//! as local input.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Instant;

use super::{RemoteCommand, RemoteOptions, RemoteServer};
use crate::event::AgentEvent;
use crate::ui::events::sanitize_output;

/// Longest summary or text line sent to a browser.
const MAX_SUMMARY_CHARS: usize = 400;
/// Streamed text is flushed at a newline or once this many characters wait.
const MAX_PENDING_TEXT_CHARS: usize = 1000;

pub const USAGE: &str =
    "usage: /remote on [--port <n>] [--host <ip> --allow-non-loopback] | off | status";

/// What `/remote` asked for.
#[derive(Debug)]
pub enum RemoteRequest {
    On(RemoteOptions),
    Off,
    Status,
}

/// The arguments of a `/remote` submission, or `None` for any other text.
pub fn command_args(text: &str) -> Option<&str> {
    let rest = text.trim_start().strip_prefix("/remote")?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then_some(rest.trim())
}

/// Parses the arguments after `/remote`. A bare `/remote` is `status`.
pub fn parse_request(args: &str) -> Result<RemoteRequest, String> {
    let mut words = args.split_whitespace();
    match words.next() {
        None | Some("status") if words.next().is_none() => Ok(RemoteRequest::Status),
        Some("off") if words.next().is_none() => Ok(RemoteRequest::Off),
        Some("on") => {
            let mut options = RemoteOptions::default();
            let mut host_given = false;
            while let Some(word) = words.next() {
                match word {
                    "--port" => {
                        let value = words.next().ok_or("--port needs a number")?;
                        options.port = value
                            .parse()
                            .map_err(|_| format!("invalid port '{value}'"))?;
                    }
                    "--host" => {
                        let value = words.next().ok_or("--host needs an IP address")?;
                        options.host = value.parse::<IpAddr>().map_err(|_| {
                            format!("invalid host '{value}' (an IP address is required)")
                        })?;
                        host_given = true;
                    }
                    "--allow-non-loopback" => options.allow_non_loopback = true,
                    other => return Err(format!("unknown option '{other}'; {USAGE}")),
                }
            }
            if !host_given {
                options.host = IpAddr::V4(Ipv4Addr::LOCALHOST);
            }
            if !options.host.is_loopback() && !options.allow_non_loopback {
                return Err(format!(
                    "binding {} is not loopback; add --allow-non-loopback to accept the risk (there is no TLS)",
                    options.host
                ));
            }
            Ok(RemoteRequest::On(options))
        }
        _ => Err(USAGE.to_string()),
    }
}

/// Warning printed when the server listens beyond loopback.
pub fn non_loopback_warning(addr: std::net::SocketAddr) -> Option<String> {
    (!addr.ip().is_loopback()).then(|| {
        format!(
            "warning: remote control listens on {addr}, which other machines can reach. There is \
             no TLS, so the token travels in clear text; use an SSH tunnel or a TLS proxy."
        )
    })
}

fn bounded(text: &str) -> String {
    let clean = sanitize_output(text);
    let flat = clean.trim().replace('\n', " ");
    if flat.chars().count() <= MAX_SUMMARY_CHARS {
        return flat;
    }
    let mut cut: String = flat.chars().take(MAX_SUMMARY_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// Turns agent events into the JSON events the page renders, joining streamed
/// tokens into lines.
#[derive(Default)]
pub struct EventMapper {
    text: String,
}

impl EventMapper {
    fn flush(&mut self, out: &mut Vec<serde_json::Value>) {
        let text = std::mem::take(&mut self.text);
        let text = bounded(&text);
        if !text.is_empty() {
            out.push(serde_json::json!({"type": "text", "text": text}));
        }
    }

    pub fn map(&mut self, event: &AgentEvent) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        match event {
            AgentEvent::Token(token) => {
                self.text.push_str(token);
                if let Some(end) = self.text.rfind('\n') {
                    let rest = self.text.split_off(end + 1);
                    self.flush(&mut out);
                    self.text = rest;
                }
                if self.text.chars().count() > MAX_PENDING_TEXT_CHARS {
                    self.flush(&mut out);
                }
            }
            AgentEvent::ToolCall { name, args, .. } => {
                self.flush(&mut out);
                out.push(serde_json::json!({
                    "type": "tool",
                    "name": bounded(name),
                    "summary": bounded(&args.to_string()),
                }));
            }
            AgentEvent::ToolResult {
                name,
                output,
                is_error,
                ..
            } => {
                self.flush(&mut out);
                out.push(serde_json::json!({
                    "type": "result",
                    "name": bounded(name),
                    "summary": bounded(output),
                    "error": is_error,
                }));
            }
            AgentEvent::SteerConsumed { .. } => {
                out.push(serde_json::json!({"type": "steer", "summary": "steering delivered"}));
            }
            AgentEvent::SteerRejected { reason } => {
                out.push(serde_json::json!({"type": "steer", "summary": bounded(reason)}));
            }
            AgentEvent::Interrupted { .. } => {
                self.flush(&mut out);
                out.push(serde_json::json!({"type": "interrupted", "summary": "run interrupted"}));
            }
            AgentEvent::Error { message, .. } => {
                self.flush(&mut out);
                out.push(serde_json::json!({"type": "error", "summary": bounded(message)}));
            }
            AgentEvent::Done { .. } => {
                self.flush(&mut out);
                out.push(serde_json::json!({"type": "done", "summary": "turn finished"}));
            }
            _ => {}
        }
        out
    }
}

/// The permission prompt currently on the terminal.
pub struct AskView<'a> {
    /// Identity of the on-screen prompt; changes when a new one opens.
    pub key: Instant,
    pub tool: &'a str,
    pub input: &'a str,
}

struct AskSlot {
    id: String,
    key: Instant,
}

/// One running remote view of the session.
pub struct RemoteLink {
    server: RemoteServer,
    mapper: EventMapper,
    ask: Option<AskSlot>,
    seq: u64,
}

impl RemoteLink {
    pub async fn start(options: RemoteOptions) -> anyhow::Result<Self> {
        Ok(Self::new(super::start(options).await?))
    }

    pub fn new(server: RemoteServer) -> Self {
        Self {
            server,
            mapper: EventMapper::default(),
            ask: None,
            seq: 0,
        }
    }

    pub fn addr(&self) -> std::net::SocketAddr {
        self.server.addr
    }

    /// URL with the token. Shown once, when the server starts.
    pub fn url(&self) -> String {
        self.server.url()
    }

    pub fn notice(&self, kind: &str, summary: &str) {
        self.server
            .hub
            .publish(&serde_json::json!({"type": kind, "summary": bounded(summary)}));
    }

    /// Streams one agent event to every connected browser.
    pub fn observe(&mut self, event: &AgentEvent) {
        for json in self.mapper.map(event) {
            self.server.hub.publish(&json);
        }
    }

    /// Publishes, replaces or withdraws the permission ask so the browser
    /// always offers exactly the prompt the terminal shows. A prompt answered
    /// locally is withdrawn, so a later remote answer to it is refused.
    pub fn sync_ask(&mut self, current: Option<AskView<'_>>) {
        if let (Some(view), Some(slot)) = (&current, &self.ask)
            && view.key == slot.key
        {
            return;
        }
        if let Some(old) = self.ask.take() {
            self.server.hub.withdraw_permission(&old.id);
            self.server
                .hub
                .publish(&serde_json::json!({"type": "permission_closed", "id": old.id}));
        }
        if let Some(view) = current {
            self.seq += 1;
            let id = format!("p{}", self.seq);
            let summary = bounded(&format!("{}: {}", view.tool, view.input));
            self.server.hub.publish_permission(&id, &summary);
            self.ask = Some(AskSlot { id, key: view.key });
        }
    }

    /// The prompt identity a browser answer for `id` refers to, when `id` is
    /// the ask currently offered.
    pub fn ask_key(&self, id: &str) -> Option<Instant> {
        self.ask
            .as_ref()
            .filter(|slot| slot.id == id)
            .map(|slot| slot.key)
    }

    /// Next browser command. Pends forever if the server's channel closed.
    pub async fn recv(&mut self) -> RemoteCommand {
        match self.server.commands.recv().await {
            Some(command) => command,
            None => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn token(text: &str) -> AgentEvent {
        AgentEvent::Token(text.into())
    }

    #[test]
    fn command_args_match_only_the_remote_command() {
        assert_eq!(command_args("/remote"), Some(""));
        assert_eq!(command_args("  /remote on --port 9"), Some("on --port 9"));
        assert_eq!(command_args("/remotely"), None);
        assert_eq!(command_args("/cron 5m x"), None);
    }

    #[test]
    fn remote_controls_pass_while_a_run_is_active() {
        let prompts = std::collections::HashMap::new();
        for command in ["/remote", "/remote status", "/remote off", "/remote on"] {
            assert_eq!(
                crate::ui::classify_submission(true, command, &prompts),
                crate::ui::SubmitAction::Run,
                "{command}"
            );
        }
    }

    #[test]
    fn parse_defaults_to_status_and_loopback() {
        assert!(matches!(parse_request(""), Ok(RemoteRequest::Status)));
        assert!(matches!(parse_request("status"), Ok(RemoteRequest::Status)));
        assert!(matches!(parse_request("off"), Ok(RemoteRequest::Off)));
        let Ok(RemoteRequest::On(options)) = parse_request("on --port 8123") else {
            panic!("on parses");
        };
        assert!(options.host.is_loopback() && options.port == 8123 && !options.allow_non_loopback);
        assert!(parse_request("on --port nope").is_err());
        assert!(parse_request("on --bogus").is_err());
        assert!(parse_request("off now").is_err());
        assert!(parse_request("restart").is_err());
    }

    #[test]
    fn a_non_loopback_host_needs_the_explicit_flag() {
        let refused = parse_request("on --host 0.0.0.0").unwrap_err();
        assert!(refused.contains("--allow-non-loopback"), "{refused}");
        let Ok(RemoteRequest::On(options)) =
            parse_request("on --host 0.0.0.0 --allow-non-loopback")
        else {
            panic!("explicit opt-in parses");
        };
        assert!(!options.host.is_loopback() && options.allow_non_loopback);
        // The flag alone never widens the default bind.
        let Ok(RemoteRequest::On(options)) = parse_request("on --allow-non-loopback") else {
            panic!("parses");
        };
        assert!(options.host.is_loopback());
        assert!(non_loopback_warning("127.0.0.1:1".parse().unwrap()).is_none());
        let warning = non_loopback_warning("0.0.0.0:1".parse().unwrap()).unwrap();
        assert!(warning.contains("no TLS"), "{warning}");
    }

    #[test]
    fn tokens_are_joined_into_lines_and_flushed_at_boundaries() {
        let mut mapper = EventMapper::default();
        assert!(mapper.map(&token("hel")).is_empty());
        let lines = mapper.map(&token("lo\nwor"));
        assert_eq!(
            lines,
            vec![serde_json::json!({"type":"text","text":"hello"})]
        );
        let call = AgentEvent::ToolCall {
            id: "1".into(),
            name: "shell".into(),
            args: serde_json::json!({"command": "ls"}),
        };
        let events = mapper.map(&call);
        assert_eq!(events[0]["text"], "wor");
        assert_eq!(events[1]["type"], "tool");
        assert_eq!(events[1]["name"], "shell");
    }

    #[test]
    fn results_are_sanitized_and_bounded() {
        let mut mapper = EventMapper::default();
        let long = format!("\u{1b}[31mred\u{1b}[0m {}", "x".repeat(2000));
        let events = mapper.map(&AgentEvent::ToolResult {
            id: "1".into(),
            name: "shell".into(),
            output: long.into(),
            images: Vec::new(),
            is_error: true,
        });
        let summary = events[0]["summary"].as_str().unwrap();
        assert!(summary.starts_with("red x"), "{summary}");
        assert!(summary.chars().count() <= MAX_SUMMARY_CHARS);
        assert_eq!(events[0]["error"], true);
    }

    async fn http(addr: std::net::SocketAddr, request: String) -> String {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            stream.read_to_end(&mut out),
        )
        .await;
        String::from_utf8_lossy(&out).to_string()
    }

    fn answer(link: &RemoteLink, id: &str) -> String {
        let body = format!(r#"{{"id":"{id}","allow":true}}"#);
        format!(
            "POST /permission HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n{body}",
            link.addr(),
            link.server.token,
            body.len()
        )
    }

    #[tokio::test]
    async fn a_remote_answer_reaches_the_session_only_for_the_prompt_on_screen() {
        let mut link = RemoteLink::start(RemoteOptions::default()).await.unwrap();
        let first = Instant::now();
        link.sync_ask(Some(AskView {
            key: first,
            tool: "shell",
            input: "ls",
        }));
        assert_eq!(link.ask_key("p1"), Some(first));
        assert_eq!(link.ask_key("p9"), None);

        let reply = http(link.addr(), answer(&link, "p1")).await;
        assert!(reply.starts_with("HTTP/1.1 202"), "{reply}");
        assert_eq!(
            link.recv().await,
            RemoteCommand::Permission {
                id: "p1".into(),
                allow: true
            }
        );

        // The terminal answered first: the ask is withdrawn and a late
        // browser answer is refused before it reaches the session.
        link.sync_ask(Some(AskView {
            key: Instant::now(),
            tool: "edit",
            input: "a.rs",
        }));
        assert_eq!(link.ask_key("p1"), None);
        assert!(link.ask_key("p2").is_some());
        let stale = http(link.addr(), answer(&link, "p1")).await;
        assert!(stale.starts_with("HTTP/1.1 404"), "{stale}");
        link.sync_ask(None);
        assert_eq!(link.ask_key("p2"), None);
        let late = http(link.addr(), answer(&link, "p2")).await;
        assert!(late.starts_with("HTTP/1.1 404"), "{late}");
    }

    #[tokio::test]
    async fn an_unchanged_prompt_is_published_once() {
        let mut link = RemoteLink::start(RemoteOptions::default()).await.unwrap();
        let key = Instant::now();
        for _ in 0..3 {
            link.sync_ask(Some(AskView {
                key,
                tool: "shell",
                input: "ls",
            }));
        }
        assert_eq!(link.seq, 1);
    }
}
