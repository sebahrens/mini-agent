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
//! network connections. Calls are handled one at a time.
use std::ffi::OsString;
use std::path::PathBuf;
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
const PROTOCOL_VERSION: &str = "2025-06-18";

/// How a `run` call launches its headless child.
#[derive(Debug, Clone)]
pub(crate) struct RunCommand {
    pub(crate) program: PathBuf,
    /// Arguments placed before `-p -- <prompt>`.
    pub(crate) args: Vec<OsString>,
    pub(crate) timeout: Duration,
}

impl RunCommand {
    /// This executable with the server's own arguments, minus the
    /// `--mcp-server` mode flag, so each run honours the same flags.
    pub(crate) fn current(args: impl IntoIterator<Item = OsString>) -> std::io::Result<Self> {
        Ok(Self {
            program: std::env::current_exe()?,
            args: forwarded_args(args),
            timeout: RUN_TIMEOUT,
        })
    }

    fn argv(&self, prompt: &str) -> Vec<OsString> {
        let mut argv = self.args.clone();
        argv.extend(["-p".into(), "--".into(), prompt.into()]);
        argv
    }
}

/// Server arguments forwarded to each headless run. Only the mode flag is
/// removed; a stray prompt cannot be present because `--mcp-server`
/// conflicts with `-p` and positional messages.
pub(crate) fn forwarded_args(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    args.into_iter()
        .filter(|arg| arg.as_os_str() != "--mcp-server")
        .collect()
}

fn run_tool_definition() -> Value {
    json!({
        "name": "run",
        "description": "Run one mini-agent prompt headlessly in the server's workspace with the server's configured provider, model, sandbox and permission flags, and return its final answer. Permission prompts cannot be answered: actions not allowed by those flags are refused.",
        "inputSchema": {
            "type": "object",
            "properties": {"prompt": {"type": "string", "description": "The task for mini-agent"}},
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

fn tool_result(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
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

async fn run_prompt(command: &RunCommand, prompt: &str) -> (String, bool) {
    use crate::process_creation::TokioCommandCreationExt as _;
    let mut child = tokio::process::Command::new(&command.program);
    child
        .args(command.argv(prompt))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = match child.spawn_guarded() {
        Ok(child) => child,
        Err(error) => return (format!("mini-agent run could not start: {error}"), true),
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let work = async {
        let (out, err, status) = tokio::join!(
            read_capped(stdout, MAX_OUTPUT_BYTES),
            read_capped(stderr, MAX_STDERR_BYTES),
            child.wait()
        );
        (out, err, status)
    };
    let Ok(((stdout, truncated), (stderr, _), status)) =
        tokio::time::timeout(command.timeout, work).await
    else {
        let _ = child.start_kill();
        return (
            format!(
                "mini-agent run timed out after {} s and was stopped",
                command.timeout.as_secs()
            ),
            true,
        );
    };
    let mut text = String::from_utf8_lossy(&stdout).trim_end().to_owned();
    if truncated {
        text.push_str(&format!(
            "\n[output truncated at {MAX_OUTPUT_BYTES} bytes; ask for a shorter answer or have the run write a file]"
        ));
    }
    match status {
        Ok(status) if status.success() => (text, false),
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
            (report, true)
        }
    }
}

/// Answer one JSON-RPC message; `None` for notifications.
async fn handle(command: &RunCommand, message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    Some(match method {
        "initialize" => {
            let version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION);
            result(
                &id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "mini-agent", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
        }
        "ping" => result(&id, json!({})),
        "tools/list" => result(&id, json!({"tools": [run_tool_definition()]})),
        "tools/call" => {
            if params.get("name").and_then(Value::as_str) != Some("run") {
                return Some(error(&id, -32602, "unknown tool; only `run` is offered"));
            }
            let Some(prompt) = params
                .get("arguments")
                .and_then(|args| args.get("prompt"))
                .and_then(Value::as_str)
                .filter(|prompt| !prompt.trim().is_empty())
            else {
                return Some(error(
                    &id,
                    -32602,
                    "`run` requires a nonempty `prompt` string",
                ));
            };
            if prompt.len() > MAX_PROMPT_BYTES {
                return Some(error(&id, -32602, "`prompt` exceeds 256 KiB"));
            }
            let (text, is_error) = run_prompt(command, prompt).await;
            result(&id, tool_result(text, is_error))
        }
        _ => error(&id, -32601, "method not found"),
    })
}

/// Serve until the client closes its end of the stream.
pub(crate) async fn serve(
    command: RunCommand,
    input: impl AsyncRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
) -> anyhow::Result<()> {
    let mut lines = tokio::io::BufReader::new(input);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = (&mut lines)
            .take(MAX_REQUEST_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)
            .await?;
        if read == 0 {
            return Ok(());
        }
        if line.len() > MAX_REQUEST_LINE_BYTES {
            anyhow::bail!("MCP request line exceeds {MAX_REQUEST_LINE_BYTES} bytes");
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let reply = match serde_json::from_slice::<Value>(&line) {
            Ok(message) => handle(&command, &message).await,
            Err(_) => Some(error(&Value::Null, -32700, "parse error")),
        };
        if let Some(reply) = reply {
            let mut bytes = serde_json::to_vec(&reply)?;
            bytes.push(b'\n');
            output.write_all(&bytes).await?;
            output.flush().await?;
        }
    }
}

/// Entry point for `--mcp-server`: serve on the process's stdio.
pub(crate) async fn serve_stdio() -> anyhow::Result<()> {
    let command = RunCommand::current(std::env::args_os().skip(1))?;
    serve(command, tokio::io::stdin(), tokio::io::stdout()).await
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    async fn exchange(command: RunCommand, requests: &[Value]) -> Vec<Value> {
        let mut input = Vec::new();
        for request in requests {
            input.extend(serde_json::to_vec(request).unwrap());
            input.push(b'\n');
        }
        let mut output = Vec::new();
        serve(command, input.as_slice(), &mut output).await.unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn shell(script: &str) -> RunCommand {
        RunCommand {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into(), "sh".into()],
            timeout: Duration::from_secs(10),
        }
    }

    #[tokio::test]
    async fn mcp_server_initializes_lists_run_and_executes_a_headless_child() {
        // The child sees the forwarded flags, then `-p -- <prompt>` literally.
        let replies = exchange(
            shell(r#"printf 'argv:%s|%s|%s' "$1" "$2" "$3""#),
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
            "argv:-p|--|--yolo $(touch x)"
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
