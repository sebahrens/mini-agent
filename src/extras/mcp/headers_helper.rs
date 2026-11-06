//! `headersHelper`: a command that mints request headers for a URL server.
//!
//! This runs a command named by configuration, so it is closed by default:
//! nothing runs unless the top-level `mcp_headers_helper = true` opt-in is set
//! (project config reaches that key only through the ordinary content-bound
//! project trust; a project `.mcp.json` additionally needs its own trust).
//! The helper is never run through a shell, starts with a cleared environment
//! (only names in `inherit_env` are delegated), can be placed in a
//! workspace-service sandbox, is bounded to 10 s and 64 KiB of output, and
//! its failure text never includes its output, so tokens cannot leak into
//! notices. It runs once per (re)connect.
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::AsyncReadExt as _;

use super::config::{McpHeadersHelper, McpStdioNetwork};

/// Bound on one helper run.
pub(crate) const HELPER_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_HEADERS: usize = 32;
const MAX_VALUE_BYTES: usize = 8 * 1024;

/// Names a helper may not set: they would change framing or routing rather
/// than authenticate.
const FORBIDDEN: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "upgrade",
    "te",
    "trailer",
    "proxy-authorization",
    "proxy-connection",
    "keep-alive",
];

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Record the `mcp_headers_helper` opt-in of the configuration that is about
/// to connect servers. Defaults to closed.
pub(crate) fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Release);
}

pub(crate) fn is_enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// Static `headers` overlaid with the helper's output (helper wins,
/// case-insensitively). Refuses when `enabled` is false without running
/// anything.
pub(crate) async fn merged_headers(
    server_name: &str,
    helper: &McpHeadersHelper,
    static_headers: &HashMap<String, String>,
    workspace: &Path,
    enabled: bool,
    timeout: Duration,
) -> anyhow::Result<HashMap<String, String>> {
    anyhow::ensure!(
        enabled,
        "MCP server '{server_name}' sets headersHelper, which runs a command and is disabled; \
         set `mcp_headers_helper = true` in your configuration to allow it"
    );
    let produced = run(server_name, helper, workspace, timeout).await?;
    let mut merged = static_headers.clone();
    for (name, value) in produced {
        merged.retain(|existing, _| !existing.eq_ignore_ascii_case(&name));
        merged.insert(name, value);
    }
    Ok(merged)
}

async fn run(
    server_name: &str,
    helper: &McpHeadersHelper,
    workspace: &Path,
    timeout: Duration,
) -> anyhow::Result<HashMap<String, String>> {
    use crate::process_creation::TokioCommandCreationExt as _;
    helper
        .validate()
        .map_err(|error| anyhow::anyhow!("MCP server '{server_name}': {error}"))?;
    let program = &helper.command[0];
    // A path with a directory component is anchored at the workspace, like a
    // stdio server's explicit `cwd`; bare names search only absolute PATH
    // entries.
    let anchor = program.contains(['/', '\\']).then(|| Path::new("."));
    let mut command = super::client::stdio_command(
        program,
        &helper.command[1..],
        workspace,
        anchor,
        &HashMap::new(),
        &helper.inherit_env,
        helper.sandbox.as_deref(),
        McpStdioNetwork::Inherit,
        std::env::var_os("PATH").as_deref(),
    )
    .map_err(|error| {
        anyhow::anyhow!("MCP server '{server_name}' headersHelper cannot start: {error}")
    })?;
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn_guarded().map_err(|error| {
        anyhow::anyhow!("MCP server '{server_name}' headersHelper cannot start: {error}")
    })?;
    let pid = child.id();
    let mut stdout = child.stdout.take().expect("piped stdout");
    let work = async {
        let mut output = Vec::new();
        let read = (&mut stdout)
            .take(MAX_OUTPUT_BYTES as u64 + 1)
            .read_to_end(&mut output)
            .await;
        // Closing the pipe early (output over the cap) must not wait on a
        // child that is still writing.
        let status = if output.len() > MAX_OUTPUT_BYTES {
            None
        } else {
            Some(child.wait().await)
        };
        (read, output, status)
    };
    let finished = tokio::time::timeout(timeout, work).await;
    #[cfg(unix)]
    if let Some(pid) = pid {
        crate::sandbox::kill_process_group(pid);
    }
    #[cfg(not(unix))]
    let _ = pid;
    let _ = child.start_kill();
    let Ok((read, output, status)) = finished else {
        anyhow::bail!(
            "MCP server '{server_name}' headersHelper timed out after {} s",
            timeout.as_secs()
        );
    };
    anyhow::ensure!(
        read.is_ok(),
        "MCP server '{server_name}' headersHelper output could not be read"
    );
    anyhow::ensure!(
        output.len() <= MAX_OUTPUT_BYTES,
        "MCP server '{server_name}' headersHelper printed more than 64 KiB"
    );
    match status {
        Some(Ok(status)) if status.success() => {}
        Some(Ok(status)) => anyhow::bail!(
            "MCP server '{server_name}' headersHelper failed ({})",
            status
                .code()
                .map_or_else(|| "signal".to_owned(), |code| format!("exit code {code}"))
        ),
        _ => anyhow::bail!("MCP server '{server_name}' headersHelper did not finish"),
    }
    parse_output(&output)
        .map_err(|error| anyhow::anyhow!("MCP server '{server_name}' headersHelper {error}"))
}

/// A JSON object of header names to string values, each valid and allowed.
fn parse_output(output: &[u8]) -> Result<HashMap<String, String>, &'static str> {
    let value: serde_json::Value =
        serde_json::from_slice(output).map_err(|_| "did not print a JSON object")?;
    let object = value
        .as_object()
        .ok_or("must print a JSON object of header names to strings")?;
    if object.len() > MAX_HEADERS {
        return Err("printed more than 32 headers");
    }
    let mut headers = HashMap::new();
    for (name, value) in object {
        let value = value.as_str().ok_or("header values must be strings")?;
        let header_name: http::HeaderName =
            name.parse().map_err(|_| "printed an invalid header name")?;
        if FORBIDDEN.contains(&header_name.as_str()) {
            return Err("tried to set a protected header");
        }
        if value.len() > MAX_VALUE_BYTES || http::HeaderValue::from_str(value).is_err() {
            return Err("printed an invalid or oversized header value");
        }
        headers.insert(name.clone(), value.to_owned());
    }
    Ok(headers)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn helper(script: &str) -> McpHeadersHelper {
        McpHeadersHelper {
            command: vec!["/bin/sh".into(), "-c".into(), script.into()],
            inherit_env: Vec::new(),
            sandbox: None,
        }
    }

    fn dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mini-agent-helper-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn merged(
        helper: &McpHeadersHelper,
        enabled: bool,
        timeout: Duration,
    ) -> anyhow::Result<HashMap<String, String>> {
        let statics = HashMap::from([
            ("x-static".to_owned(), "keep".to_owned()),
            ("Authorization".to_owned(), "static".to_owned()),
        ]);
        let work = dir();
        let result = merged_headers("srv", helper, &statics, &work, enabled, timeout).await;
        let _ = std::fs::remove_dir_all(&work);
        result
    }

    #[tokio::test]
    async fn default_closed_runs_nothing() {
        let marker =
            std::env::temp_dir().join(format!("mini-agent-helper-ran-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let probe = helper(&format!("touch '{}'; echo '{{}}'", marker.display()));
        let error = merged(&probe, false, HELPER_TIMEOUT).await.unwrap_err();
        assert!(error.to_string().contains("mcp_headers_helper"), "{error}");
        assert!(!marker.exists(), "a closed helper must not execute");
    }

    #[tokio::test]
    async fn enabled_helper_headers_override_static_ones_case_insensitively() {
        let headers = merged(
            &helper(r#"printf '{"authorization":"Bearer abc","X-Extra":"1"}'"#),
            true,
            HELPER_TIMEOUT,
        )
        .await
        .unwrap();
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer abc")
        );
        assert!(!headers.contains_key("Authorization"));
        assert_eq!(headers.get("x-static").map(String::as_str), Some("keep"));
        assert_eq!(headers.get("X-Extra").map(String::as_str), Some("1"));
    }

    #[tokio::test]
    async fn adversarial_output_exit_and_environment_are_contained() {
        let bad_outputs = [
            r#"printf 'not json'"#,
            r#"printf '["a"]'"#,
            r#"printf '{"x":1}'"#,
            r#"printf '{"Host":"evil.example"}'"#,
            r#"printf '{"content-length":"1"}'"#,
            r#"printf '{"bad name":"v"}'"#,
            r#"printf '{"x":"a\nb"}'"#,
            r#"printf '{"x":"\u0001"}'"#,
        ];
        for script in bad_outputs {
            let error = merged(&helper(script), true, HELPER_TIMEOUT).await;
            assert!(error.is_err(), "{script} must be refused");
        }
        // Over 64 KiB of output.
        let error = merged(
            &helper("head -c 70000 /dev/zero | tr '\\0' a"),
            true,
            HELPER_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("64 KiB"), "{error}");
        // A failing helper's output, including a secret, never reaches the error.
        let error = merged(&helper("echo SECRET-TOKEN; exit 3"), true, HELPER_TIMEOUT)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("exit code 3") && !error.contains("SECRET"),
            "{error}"
        );
        // Timeout kills a hung helper promptly.
        let started = std::time::Instant::now();
        let error = merged(&helper("sleep 30"), true, Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
        // The ambient environment is not delegated (HOME is set here).
        assert!(std::env::var_os("HOME").is_some());
        let headers = merged(
            &helper(r#"printf '{"x-env":"[%s]"}' "$HOME""#),
            true,
            HELPER_TIMEOUT,
        )
        .await
        .unwrap();
        assert_eq!(headers.get("x-env").map(String::as_str), Some("[]"));
    }

    #[tokio::test]
    async fn a_url_server_with_a_helper_does_not_run_it_without_the_opt_in() {
        let marker =
            std::env::temp_dir().join(format!("mini-agent-helper-connect-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let config: super::super::config::McpServerConfig = serde_json::from_value(serde_json::json!({
            "url": "http://127.0.0.1:9/mcp",
            "allow_local": true,
            "headers_helper": ["/bin/sh", "-c", format!("touch '{}'; echo '{{}}'", marker.display())],
        }))
        .unwrap();
        assert!(!is_enabled(), "the process-wide opt-in starts closed");
        let work = dir();
        let error = super::super::client::McpClientHandle::connect_in(
            compact_str::CompactString::new("srv"),
            &config,
            &work,
        )
        .await
        .err()
        .expect("must not connect")
        .to_string();
        let _ = std::fs::remove_dir_all(&work);
        assert!(error.contains("mcp_headers_helper"), "{error}");
        assert!(!marker.exists(), "the helper must not run while closed");
    }

    #[test]
    fn definitions_accept_string_argv_and_table_forms_with_bounds() {
        use super::super::project::parse_definition;
        let url = |helper: serde_json::Value| {
            parse_definition(serde_json::json!({"url":"https://e.com/mcp","headersHelper":helper}))
        };
        let helper = |value| url(value).unwrap().options().headers_helper.unwrap();
        assert_eq!(helper(serde_json::json!("./tok.sh")).command, ["./tok.sh"]);
        assert_eq!(helper(serde_json::json!(["a", "b"])).command, ["a", "b"]);
        assert_eq!(
            helper(serde_json::json!({"command":["a"],"inherit_env":["T"]})).inherit_env,
            ["T"]
        );
        assert!(url(serde_json::json!([])).is_err());
        assert!(url(serde_json::json!("")).is_err());
        assert!(url(serde_json::json!(vec!["a"; 17])).is_err());
        assert!(
            parse_definition(serde_json::json!({"command":"c","headersHelper":"x"})).is_err(),
            "command servers cannot use a headers helper"
        );
    }
}
