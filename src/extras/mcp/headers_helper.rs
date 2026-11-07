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

/// Owns the helper's process group for the life of `run()`. Dropping the
/// `run()` future (connection or reconnect cancellation) skips the ordinary
/// whole-group kill that follows the timeout, and `kill_on_drop` alone signals
/// only the direct child, so descendants of a credential helper would survive.
struct GroupKillOnDrop(#[cfg_attr(not(unix), allow(dead_code))] Option<u32>);

impl Drop for GroupKillOnDrop {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0 {
            crate::sandbox::kill_process_group_if_live(pid);
        }
    }
}

async fn run(
    server_name: &str,
    helper: &McpHeadersHelper,
    workspace: &Path,
    timeout: Duration,
) -> anyhow::Result<HashMap<String, String>> {
    use crate::process_creation::CommandWrapCreationExt as _;
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
    let mut command = crate::sandbox::owned_workspace_service_tree(command);
    let mut child = command.spawn_guarded().map_err(|error| {
        anyhow::anyhow!("MCP server '{server_name}' headersHelper cannot start: {error}")
    })?;
    let pid = child.id();
    let _group_guard = GroupKillOnDrop(pid);
    let mut stdout = child.stdout().take().expect("piped stdout");
    let work = async {
        let read = async {
            let mut output = Vec::new();
            let read = (&mut stdout)
                .take(MAX_OUTPUT_BYTES as u64 + 1)
                .read_to_end(&mut output)
                .await;
            (read, output)
        };
        tokio::pin!(read);
        tokio::select! {
            status = child.inner_mut().wait() => {
                let _ = child.start_kill();
                match tokio::time::timeout(Duration::from_secs(1), &mut read).await {
                    Ok((read, output)) => (read, output, Some(status)),
                    Err(_) => (Err(std::io::Error::other("MCP helper output did not retire")), Vec::new(), Some(status)),
                }
            }
            (read, output) = &mut read => {
                // Over-limit writers are stopped without waiting for exit.
                let status = if output.len() > MAX_OUTPUT_BYTES || read.is_err() { None } else { Some(child.inner_mut().wait().await) };
                (read, output, status)
            }
        }
    };
    let finished = tokio::time::timeout(timeout, work).await;
    #[cfg(unix)]
    if let Some(pid) = pid {
        crate::sandbox::kill_process_group(pid);
    }
    #[cfg(not(unix))]
    let _ = pid;
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
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
    async fn successful_helper_leader_exit_retires_descendants_holding_stdout() {
        let result = merged(
            &helper("sleep 300 & printf '%s' '{\"x-owned\":\"done\"}'"),
            true,
            HELPER_TIMEOUT,
        )
        .await
        .unwrap();
        assert_eq!(result.get("x-owned").map(String::as_str), Some("done"));
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

    fn alive(pid: i32) -> bool {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
    }

    async fn read_pids(marker: &Path) -> (i32, i32) {
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(marker) {
                let pids: Vec<i32> = text
                    .split_whitespace()
                    .filter_map(|p| p.parse().ok())
                    .collect();
                if let [a, b] = pids[..] {
                    return (a, b);
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("helper never recorded its pids");
    }

    async fn assert_gone(pids: [i32; 2], context: &str) {
        for _ in 0..200 {
            if !pids.iter().any(|pid| alive(*pid)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        for pid in pids {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        panic!("helper process survived {context}: {pids:?}");
    }

    /// Dropping the future mid-run (what connection cancellation does) must
    /// take the helper's descendants down with it.
    #[tokio::test]
    async fn dropping_the_helper_future_kills_its_descendants() {
        let work = dir();
        let marker = work.join("pids");
        let script = format!("sleep 300 & echo \"$$ $!\" > '{}'; wait", marker.display());
        let probe = helper(&script);
        let statics = HashMap::new();
        let cancelled = tokio::select! {
            _ = async {
                read_pids(&marker).await;
            } => true,
            _ = merged_headers("srv", &probe, &statics, &work, true, Duration::from_secs(60)) => false,
        };
        assert!(cancelled, "the helper must still be running when cancelled");
        let (leader, grandchild) = read_pids(&marker).await;
        assert_gone([leader, grandchild], "dropping merged_headers").await;
        let _ = std::fs::remove_dir_all(&work);
    }

    /// The same, through the cancelled-work-scope race the MCP client uses.
    #[tokio::test]
    async fn work_scope_cancellation_kills_the_helper_descendants() {
        let work = dir();
        let marker = work.join("pids");
        let script = format!("sleep 300 & echo \"$$ $!\" > '{}'; wait", marker.display());
        let probe = helper(&script);
        let scope = crate::agent::runner::AgentWorkScope::new();
        let handle = scope.cancellation_handle();
        let canceller = {
            let marker = marker.clone();
            tokio::spawn(async move {
                read_pids(&marker).await;
                handle.cancel();
            })
        };
        let statics = HashMap::new();
        let outcome = scope
            .run(async {
                tokio::select! {
                    biased;
                    _ = crate::agent::runner::current_work_scope_cancelled() => None,
                    result = merged_headers("srv", &probe, &statics, &work, true, Duration::from_secs(60)) => Some(result),
                }
            })
            .await;
        assert!(outcome.is_none(), "cancellation must win the race");
        canceller.await.unwrap();
        let (leader, grandchild) = read_pids(&marker).await;
        assert_gone([leader, grandchild], "work-scope cancellation").await;
        let _ = std::fs::remove_dir_all(&work);
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

#[cfg(all(test, windows))]
mod windows_tree_tests {
    use super::*;
    use crate::extras::mcp::server::windows_tree_tests::Fixture;

    struct AbortOnDrop(tokio::task::AbortHandle);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    #[tokio::test]
    async fn mcp_stdio_headers_helper_owns_windows_descendants_on_success_timeout_and_drop() {
        for action in ["success", "success-inherited", "timeout", "drop"] {
            let fixture = if action == "success-inherited" {
                Fixture::inheriting_pipes()
            } else {
                Fixture::new()
            };
            let helper = McpHeadersHelper {
                command: fixture.argv(),
                inherit_env: vec!["SystemRoot".into()],
                sandbox: None,
            };
            let root = fixture.root().to_path_buf();
            let task = tokio::spawn(async move {
                run("tree-test", &helper, &root, Duration::from_secs(15)).await
            });
            let _task_owner = AbortOnDrop(task.abort_handle());
            let descendant = fixture.ready().await;
            match action {
                "success" | "success-inherited" => fixture.release(),
                "drop" => task.abort(),
                _ => {}
            }
            let result = tokio::time::timeout(Duration::from_secs(20), task)
                .await
                .expect("MCP helper retirement");
            match action {
                "success" | "success-inherited" => assert!(result.unwrap().is_ok()),
                "timeout" => assert!(result.unwrap().is_err()),
                _ => assert!(result.unwrap_err().is_cancelled()),
            }
            descendant.assert_retired().await;
        }
    }
}
