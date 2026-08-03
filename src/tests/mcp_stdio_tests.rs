use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use compact_str::CompactString;
use rig::tool::ToolDyn;

use crate::config::{Config, merge_config_override};
use crate::extras::mcp::McpClientManager;
use crate::extras::mcp::client::McpClientHandle;
use crate::extras::mcp::config::McpServerConfig;
use crate::permission::checker::PermissionChecker;
use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};

const FIXTURE_SOURCE: &str = r#"
use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

fn escape(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(escaped, "\\u{:04x}", character as u32);
            }
            character => escaped.push(character),
        }
    }
    escaped
}

fn request_id(line: &str) -> String {
    let Some(rest) = line.split_once("\"id\":").map(|(_, rest)| rest) else {
        return "null".to_string();
    };
    if let Some(quoted) = rest.strip_prefix('"') {
        let end = quoted.find('"').unwrap_or(quoted.len());
        return format!("\"{}\"", &quoted[..end]);
    }
    rest.chars()
        .take_while(|character| *character != ',' && *character != '}')
        .collect()
}

fn write_response(stdout: &mut impl Write, response: &str) {
    stdout.write_all(response.as_bytes()).unwrap();
    stdout.write_all(b"\n").unwrap();
    stdout.flush().unwrap();
}

fn tool_payload() -> String {
    let args = env::args()
        .skip(1)
        .map(|arg| format!("\"{}\"", escape(&arg)))
        .collect::<Vec<_>>()
        .join(",");
    let configured = env::var("MCP_FIXTURE_CONFIGURED").unwrap_or_default();
    let inherited = env::var_os("PATH").is_some();
    let cwd = env::current_dir().unwrap_or_default();
    format!(
        "{{\"args\":[{args}],\"configured_env\":\"{}\",\"inherited_env\":{inherited},\"cwd\":\"{}\",\"pid\":{}}}",
        escape(&configured),
        escape(&cwd.to_string_lossy()),
        std::process::id(),
    )
}

fn main() {
    let mode = env::var("MCP_FIXTURE_MODE").unwrap_or_else(|_| "normal".to_string());
    let _lease = env::var_os("MCP_FIXTURE_LEASE_FILE").map(|path| {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        fs::write(path, listener.local_addr().unwrap().to_string()).unwrap();
        listener
    });

    let diagnostic_bytes = env::var("MCP_FIXTURE_STDERR_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut stderr = io::stderr().lock();
    writeln!(stderr, "fixture diagnostic: mode={mode}").unwrap();
    if diagnostic_bytes > 0 {
        stderr.write_all(&vec![b'x'; diagnostic_bytes]).unwrap();
        stderr.write_all(b"\n").unwrap();
    }
    stderr.flush().unwrap();

    if mode == "early-exit" {
        std::process::exit(23);
    }
    if mode == "malformed" {
        println!("this is not json-rpc");
        return;
    }

    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.unwrap();
        let compact = line.replace(' ', "").replace('\t', "");
        if mode == "hang-init" && compact.contains("\"method\":\"initialize\"") {
            continue;
        }
        if compact.contains("\"method\":\"initialize\"") {
            let id = request_id(&compact);
            write_response(
                &mut stdout,
                &format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{{\"tools\":{{}}}},\"serverInfo\":{{\"name\":\"stdio-fixture\",\"version\":\"1.0.0\"}}}}}}"
                ),
            );
        } else if compact.contains("\"method\":\"tools/list\"") {
            let id = request_id(&compact);
            write_response(
                &mut stdout,
                &format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"tools\":[{{\"name\":\"probe\",\"description\":\"report fixture process inputs\",\"inputSchema\":{{\"type\":\"object\"}}}}]}}}}"
                ),
            );
        } else if compact.contains("\"method\":\"tools/call\"") {
            let id = request_id(&compact);
            let payload = escape(&tool_payload());
            write_response(
                &mut stdout,
                &format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{payload}\"}}],\"isError\":false}}}}"
                ),
            );
        }
    }

    if mode == "hang-eof" {
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }
}
"#;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct FixtureBuild {
    root: PathBuf,
    executable: PathBuf,
    path_dir: PathBuf,
    path_command: String,
}

impl FixtureBuild {
    fn compile() -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("mini agent mcp stdio {} {id}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("fixture.rs");
        fs::write(&source, FIXTURE_SOURCE).unwrap();
        let executable = root.join(if cfg!(windows) {
            "mcp-stdio-fixture.exe"
        } else {
            "mcp-stdio-fixture"
        });
        let output =
            Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc")))
                .arg("--edition=2024")
                .arg(&source)
                .arg("-o")
                .arg(&executable)
                .output()
                .expect("Rust toolchain must compile the repository-owned MCP fixture");
        assert!(
            output.status.success(),
            "fixture compilation failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let path_dir = root.join("PATH directory with spaces");
        fs::create_dir_all(&path_dir).unwrap();
        let path_command = "mini-agent-mcp-fixture".to_string();
        #[cfg(windows)]
        fs::write(
            path_dir.join(format!("{path_command}.cmd")),
            format!("@echo off\r\n\"{}\" %*\r\n", executable.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let shim = path_dir.join(&path_command);
            fs::copy(&executable, &shim).unwrap();
            fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
        }

        Self {
            root,
            executable,
            path_dir,
            path_command,
        }
    }

    fn lease(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}.lease"))
    }

    fn config(
        &self,
        command: String,
        args: Vec<String>,
        mode: &str,
        lease: &Path,
    ) -> McpServerConfig {
        McpServerConfig::Command {
            command,
            args,
            env: HashMap::from([
                ("MCP_FIXTURE_MODE".to_string(), mode.to_string()),
                (
                    "MCP_FIXTURE_LEASE_FILE".to_string(),
                    lease.display().to_string(),
                ),
                (
                    "MCP_FIXTURE_CONFIGURED".to_string(),
                    "configured exactly".to_string(),
                ),
            ]),
        }
    }

    fn cleanup(self) {
        fs::remove_dir_all(&self.root).unwrap_or_else(|error| {
            panic!(
                "fixture directory could not be removed; a child may still be running ({}): {error}",
                self.root.display()
            )
        });
    }
}

struct PathGuard {
    original: Option<OsString>,
}

impl PathGuard {
    fn prepend(path: &Path) -> Self {
        let original = std::env::var_os("PATH");
        let mut entries = vec![path.to_path_buf()];
        if let Some(existing) = &original {
            entries.extend(std::env::split_paths(existing));
        }
        let joined = std::env::join_paths(entries).unwrap();
        // SAFETY: this test restores PATH before returning and the mutation is
        // limited to resolving the uniquely named fixture command.
        unsafe { std::env::set_var("PATH", joined) };
        Self { original }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        // SAFETY: restores the process environment value saved by `prepend`.
        unsafe {
            match &self.original {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
    }
}

async fn wait_for_lease(path: &Path) -> SocketAddr {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(value) = fs::read_to_string(path)
                && let Ok(address) = value.parse()
            {
                return address;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "fixture never published its process lease: {}",
            path.display()
        )
    })
}

async fn assert_lease_released(address: SocketAddr) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match TcpListener::bind(address) {
                Ok(listener) => {
                    drop(listener);
                    return;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fixture child still owns process lease {address}"));
}

fn permission_for(action: Action) -> Arc<Mutex<PermissionChecker>> {
    let permission = PermissionConfig {
        default: Some(Action::Deny),
        mcp_tool: Some(ToolPerm::Granular(HashMap::from([(
            "mcp_tool:fixture:probe".to_string(),
            action,
        )]))),
        ..PermissionConfig::default()
    };
    Arc::new(Mutex::new(PermissionChecker::new(
        &PermissionConfigs::from(permission),
        SecurityMode::Standard,
        None,
        Some(vec!["standard".to_string()]),
    )))
}

async fn call_fixture_tool(manager: &McpClientManager) -> serde_json::Value {
    let mut tools = manager
        .collect_tools(Some(permission_for(Action::Allow)), None)
        .await;
    let tool = tools
        .iter_mut()
        .find(|tool| tool.name() == "probe")
        .expect("fixture tool must be listed");
    let output = tokio::time::timeout(Duration::from_secs(3), tool.call("{}".to_string()))
        .await
        .expect("fixture tool call timed out")
        .expect("fixture tool call failed");
    serde_json::from_str(&output).expect("fixture tool output must be JSON")
}

async fn shutdown(manager: McpClientManager) {
    tokio::time::timeout(Duration::from_secs(6), manager.shutdown())
        .await
        .expect("MCP manager shutdown must be bounded");
}

#[tokio::test]
async fn mcp_stdio_config_global_and_project_entries_reach_headless_and_tui_wiring() {
    let fixture = FixtureBuild::compile();

    let global_lease = fixture.lease("global");
    let global_server = fixture.config(
        fixture.executable.display().to_string(),
        vec!["global".to_string()],
        "normal",
        &global_lease,
    );
    let serialized_global = toml::to_string(&Config {
        mcp_servers: Some(HashMap::from([("fixture".to_string(), global_server)])),
        ..Config::default()
    })
    .unwrap();
    let global: Config = toml::from_str(&serialized_global).unwrap();
    let headless = crate::startup::connect_headless_mcp(&global, std::path::Path::new("."))
        .await
        .expect("global command entry must connect in headless wiring");
    assert_eq!(call_fixture_tool(&headless).await["args"][0], "global");
    let global_address = wait_for_lease(&global_lease).await;
    shutdown(headless).await;
    assert_lease_released(global_address).await;

    let local_lease = fixture.lease("project");
    let local_fragment = toml::to_string(&Config {
        mcp_servers: Some(HashMap::from([(
            "fixture".to_string(),
            fixture.config(
                fixture.executable.display().to_string(),
                vec!["project".to_string()],
                "normal",
                &local_lease,
            ),
        )])),
        ..Config::default()
    })
    .unwrap();
    let local = merge_config_override(&Config::default(), &local_fragment).unwrap();
    let mut tui_manager = None;
    let manager =
        crate::ui::ensure_mcp_manager(&mut tui_manager, &local, std::path::Path::new("."))
            .await
            .expect("project command entry must connect in TUI wiring");
    assert_eq!(call_fixture_tool(manager).await["args"][0], "project");
    let local_address = wait_for_lease(&local_lease).await;
    shutdown(tui_manager.take().unwrap()).await;
    assert_lease_released(local_address).await;

    fixture.cleanup();
}

#[tokio::test]
async fn mcp_stdio_end_to_end_path_absolute_args_env_and_permissions() {
    let fixture = FixtureBuild::compile();
    let marker = fixture.root.join("shell-metacharacter-was-executed");
    #[cfg(unix)]
    let metacharacter = format!("; touch {}", marker.display());
    #[cfg(windows)]
    let metacharacter = format!("& type nul > {}", marker.display());

    let absolute_lease = fixture.lease("absolute");
    let absolute_config = fixture.config(
        fixture.executable.display().to_string(),
        vec!["argument with spaces".to_string(), metacharacter.clone()],
        "normal",
        &absolute_lease,
    );
    let absolute = McpClientHandle::connect(CompactString::new("fixture"), &absolute_config)
        .await
        .expect("absolute fixture executable must initialize");
    assert_eq!(absolute.list_tools().await.unwrap()[0].name, "probe");
    let absolute_manager = McpClientManager {
        handles: vec![absolute],
        notices: Vec::new(),
    };
    let payload = call_fixture_tool(&absolute_manager).await;
    assert_eq!(payload["args"][0], "argument with spaces");
    assert_eq!(payload["args"][1], metacharacter);
    assert_eq!(payload["configured_env"], "configured exactly");
    assert_eq!(payload["inherited_env"], true);
    assert!(
        !marker.exists(),
        "configured arguments must not be parsed by a shell"
    );
    let absolute_address = wait_for_lease(&absolute_lease).await;
    shutdown(absolute_manager).await;
    assert_lease_released(absolute_address).await;

    let path_lease = fixture.lease("path");
    let path_config = fixture.config(
        fixture.path_command.clone(),
        vec!["from-path".to_string()],
        "normal",
        &path_lease,
    );
    {
        let _path_guard = PathGuard::prepend(&fixture.path_dir);
        let path_handle = McpClientHandle::connect(CompactString::new("fixture"), &path_config)
            .await
            .expect("bare PATH fixture command must initialize");
        let path_manager = McpClientManager {
            handles: vec![path_handle],
            notices: Vec::new(),
        };
        assert_eq!(
            call_fixture_tool(&path_manager).await["args"][0],
            "from-path"
        );
        let path_address = wait_for_lease(&path_lease).await;
        shutdown(path_manager).await;
        assert_lease_released(path_address).await;
    }

    let denied_lease = fixture.lease("denied");
    let denied_config = fixture.config(
        fixture.executable.display().to_string(),
        Vec::new(),
        "normal",
        &denied_lease,
    );
    let denied_handle = McpClientHandle::connect(CompactString::new("fixture"), &denied_config)
        .await
        .unwrap();
    let denied_manager = McpClientManager {
        handles: vec![denied_handle],
        notices: Vec::new(),
    };
    let mut denied_tools = denied_manager
        .collect_tools(Some(permission_for(Action::Deny)), None)
        .await;
    let denied = denied_tools
        .iter_mut()
        .find(|tool| tool.name() == "probe")
        .unwrap()
        .call("{}".to_string())
        .await
        .unwrap_err();
    assert!(denied.to_string().contains("Permission denied"));
    let denied_address = wait_for_lease(&denied_lease).await;
    shutdown(denied_manager).await;
    assert_lease_released(denied_address).await;

    fixture.cleanup();
}

#[tokio::test]
async fn mcp_command_transport_uses_the_explicit_workspace() {
    let fixture = FixtureBuild::compile();
    let workspace = fixture.root.join("selected workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let config = McpServerConfig::Command {
        command: fixture.executable.display().to_string(),
        args: Vec::new(),
        env: HashMap::from([("MCP_FIXTURE_MODE".to_string(), "normal".to_string())]),
    };
    let handle = McpClientHandle::connect_in(CompactString::new("fixture"), &config, &workspace)
        .await
        .unwrap();
    let manager = McpClientManager {
        handles: vec![handle],
        notices: Vec::new(),
    };

    assert_eq!(
        call_fixture_tool(&manager).await["cwd"],
        workspace.canonicalize().unwrap().display().to_string()
    );
    shutdown(manager).await;
    fixture.cleanup();
}

#[tokio::test]
async fn mcp_stdio_failure_cleanup_is_bounded_and_leaves_no_child() {
    let fixture = FixtureBuild::compile();

    let missing = McpServerConfig::Command {
        command: "mini-agent-mcp-fixture-that-does-not-exist".to_string(),
        args: Vec::new(),
        env: HashMap::new(),
    };
    let missing_error = McpClientHandle::connect_with_timeout(
        CompactString::new("missing-server"),
        &missing,
        Duration::from_millis(200),
    )
    .await
    .err()
    .expect("missing fixture command must fail");
    assert!(missing_error.to_string().contains("missing-server"));
    assert!(missing_error.to_string().contains("resolution failed"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let no_exec = fixture.root.join("fixture-without-execute-permission");
        fs::copy(&fixture.executable, &no_exec).unwrap();
        fs::set_permissions(&no_exec, fs::Permissions::from_mode(0o644)).unwrap();
        let config = fixture.config(
            no_exec.display().to_string(),
            Vec::new(),
            "normal",
            &fixture.lease("no-exec"),
        );
        let error = McpClientHandle::connect_with_timeout(
            CompactString::new("no-exec-server"),
            &config,
            Duration::from_millis(200),
        )
        .await
        .err()
        .expect("non-executable fixture path must fail");
        assert!(error.to_string().contains("no-exec-server"));
    }

    for (name, mode) in [
        ("malformed-server", "malformed"),
        ("early-exit-server", "early-exit"),
    ] {
        let lease = fixture.lease(name);
        let mut config = fixture.config(
            fixture.executable.display().to_string(),
            Vec::new(),
            mode,
            &lease,
        );
        if let McpServerConfig::Command { env, .. } = &mut config {
            env.insert("MCP_FIXTURE_STDERR_BYTES".to_string(), "20000".to_string());
        }
        let error = McpClientHandle::connect_with_timeout(
            CompactString::new(name),
            &config,
            Duration::from_secs(2),
        )
        .await
        .err()
        .expect("invalid fixture protocol must fail");
        let message = error.to_string();
        assert!(message.contains(name));
        assert!(message.contains("fixture diagnostic"));
        assert!(message.len() < 9_000, "stderr diagnostics must be bounded");
        let address = wait_for_lease(&lease).await;
        assert_lease_released(address).await;
    }

    let timeout_lease = fixture.lease("timeout");
    let timeout_config = fixture.config(
        fixture.executable.display().to_string(),
        Vec::new(),
        "hang-init",
        &timeout_lease,
    );
    let timeout_error = McpClientHandle::connect_with_timeout(
        CompactString::new("timeout-server"),
        &timeout_config,
        Duration::from_millis(150),
    )
    .await
    .err()
    .expect("fixture that never initializes must time out");
    assert!(timeout_error.to_string().contains("timed out"));
    let timeout_address = wait_for_lease(&timeout_lease).await;
    assert_lease_released(timeout_address).await;

    let cancel_lease = fixture.lease("cancel");
    let cancel_config = fixture.config(
        fixture.executable.display().to_string(),
        Vec::new(),
        "hang-init",
        &cancel_lease,
    );
    let connect_task = tokio::spawn(async move {
        McpClientHandle::connect(CompactString::new("cancel-server"), &cancel_config).await
    });
    let cancel_address = wait_for_lease(&cancel_lease).await;
    connect_task.abort();
    assert!(matches!(
        connect_task.await,
        Err(error) if error.is_cancelled()
    ));
    assert_lease_released(cancel_address).await;

    for (name, mode) in [("graceful", "normal"), ("forced", "hang-eof")] {
        let lease = fixture.lease(name);
        let config = fixture.config(
            fixture.executable.display().to_string(),
            Vec::new(),
            mode,
            &lease,
        );
        let handle = McpClientHandle::connect(CompactString::new(name), &config)
            .await
            .unwrap();
        let address = wait_for_lease(&lease).await;
        shutdown(McpClientManager {
            handles: vec![handle],
            notices: Vec::new(),
        })
        .await;
        assert_lease_released(address).await;
    }

    fixture.cleanup();
}
