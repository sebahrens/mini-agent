use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
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
use crate::extras::mcp::config::{McpServerConfig, McpStdioNetwork};
use crate::permission::checker::PermissionChecker;
use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};

const FIXTURE_SOURCE: &str = r#"
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::process::{Command, Stdio};
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
    let inherited_home = env::var("HOME").ok();
    let cwd = env::current_dir().unwrap();
    let executable = env::current_exe().unwrap();
    format!(
        "{{\"args\":[{args}],\"configured_env\":\"{}\",\"inherited_home\":{},\"cwd\":\"{}\",\"executable\":\"{}\",\"pid\":{}}}",
        escape(&configured),
        inherited_home
            .as_deref()
            .map(|value| format!("\"{}\"", escape(value)))
            .unwrap_or_else(|| "null".to_string()),
        escape(&cwd.display().to_string()),
        escape(&executable.display().to_string()),
        std::process::id(),
    )
}

fn main() {
    let mode = env::var("MCP_FIXTURE_MODE").unwrap_or_else(|_| "normal".to_string());
    if mode == "descendant-child" {
        let path = env::var_os("MCP_FIXTURE_DESCENDANT_FILE").unwrap();
        fs::write(path, std::process::id().to_string()).unwrap();
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }

    if mode == "descendant" {
        let mut child = Command::new(env::current_exe().unwrap());
        child
            .env("MCP_FIXTURE_MODE", "descendant-child")
            .env(
                "MCP_FIXTURE_DESCENDANT_FILE",
                env::var_os("MCP_FIXTURE_DESCENDANT_FILE").unwrap(),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        child.spawn().unwrap();
    }

    if let Some(path) = env::var_os("MCP_FIXTURE_LEASE_FILE") {
        fs::write(path, std::process::id().to_string()).unwrap();
    }
    let exclusive = env::var_os("MCP_FIXTURE_EXCLUSIVE_FILE").map(|path| {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap_or_else(|error| {
                eprintln!("exclusive fixture resource unavailable: {error}");
                std::process::exit(24);
            });
        (path, file)
    });

    let diagnostic_bytes = env::var("MCP_FIXTURE_STDERR_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut stderr = io::stderr().lock();
    writeln!(stderr, "fixture diagnostic: mode={mode}").unwrap();
    if mode == "invalid-utf8" {
        stderr.write_all(&vec![0xff; 20_000]).unwrap();
        stderr.flush().unwrap();
        return;
    }
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
            if mode == "large-error" {
                let message = format!("{}MCP_LARGE_ERROR_TAIL", "x".repeat(2 * 1024 * 1024));
                write_response(
                    &mut stdout,
                    &format!(
                        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":-32000,\"message\":\"{message}\"}}}}"
                    ),
                );
                continue;
            }
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
    if let Some((path, file)) = exclusive {
        drop(file);
        fs::remove_file(path).unwrap();
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

    fn descendant(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}.descendant"))
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
            cwd: None,
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
            inherit_env: Vec::new(),
            sandbox: None,
            network: McpStdioNetwork::Inherit,
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

async fn wait_for_pid(path: &Path) -> u32 {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(value) = fs::read_to_string(path)
                && let Ok(pid) = value.parse()
            {
                return pid;
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

fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(windows)]
    {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
    }
}

async fn assert_process_reaped(pid: u32) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if !process_is_alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fixture process {pid} is still alive"));
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
    let headless = crate::startup::connect_headless_mcp(&global)
        .await
        .expect("global command entry must connect in headless wiring");
    assert_eq!(call_fixture_tool(&headless).await["args"][0], "global");
    let global_pid = wait_for_pid(&global_lease).await;
    shutdown(headless).await;
    assert_process_reaped(global_pid).await;

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
    let manager = crate::ui::ensure_mcp_manager(&mut tui_manager, &local)
        .await
        .expect("project command entry must connect in TUI wiring");
    assert_eq!(call_fixture_tool(manager).await["args"][0], "project");
    let local_pid = wait_for_pid(&local_lease).await;
    shutdown(tui_manager.take().unwrap()).await;
    assert_process_reaped(local_pid).await;

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
    let explicit_cwd = fixture.root.join("explicit working directory");
    fs::create_dir_all(&explicit_cwd).unwrap();
    let mut absolute_config = fixture.config(
        fixture.executable.display().to_string(),
        vec!["argument with spaces".to_string(), metacharacter.clone()],
        "normal",
        &absolute_lease,
    );
    if let McpServerConfig::Command { cwd, .. } = &mut absolute_config {
        *cwd = Some(explicit_cwd.clone());
    }
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
    assert_eq!(payload["inherited_home"], serde_json::Value::Null);
    assert_eq!(
        PathBuf::from(payload["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        explicit_cwd.canonicalize().unwrap(),
    );
    assert_eq!(
        PathBuf::from(payload["executable"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        fixture.executable.canonicalize().unwrap(),
    );
    assert!(
        !marker.exists(),
        "configured arguments must not be parsed by a shell"
    );
    let absolute_pid = wait_for_pid(&absolute_lease).await;
    shutdown(absolute_manager).await;
    assert_process_reaped(absolute_pid).await;

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
        let path_pid = wait_for_pid(&path_lease).await;
        shutdown(path_manager).await;
        assert_process_reaped(path_pid).await;
    }

    let inherited_lease = fixture.lease("explicit-inherited-env");
    let mut inherited_config = fixture.config(
        fixture.executable.display().to_string(),
        Vec::new(),
        "normal",
        &inherited_lease,
    );
    if let McpServerConfig::Command { inherit_env, .. } = &mut inherited_config {
        inherit_env.push("HOME".to_string());
    }
    let inherited_handle =
        McpClientHandle::connect(CompactString::new("fixture"), &inherited_config)
            .await
            .unwrap();
    let inherited_manager = McpClientManager {
        handles: vec![inherited_handle],
        notices: Vec::new(),
    };
    assert_eq!(
        call_fixture_tool(&inherited_manager).await["inherited_home"],
        std::env::var("HOME").unwrap()
    );
    let inherited_pid = wait_for_pid(&inherited_lease).await;
    shutdown(inherited_manager).await;
    assert_process_reaped(inherited_pid).await;

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
    let denied_pid = wait_for_pid(&denied_lease).await;
    shutdown(denied_manager).await;
    assert_process_reaped(denied_pid).await;

    fixture.cleanup();
}

#[tokio::test]
async fn mcp_stdio_failure_cleanup_is_bounded_and_leaves_no_child() {
    let fixture = FixtureBuild::compile();

    let missing = McpServerConfig::Command {
        command: "mini-agent-mcp-fixture-that-does-not-exist".to_string(),
        args: Vec::new(),
        cwd: None,
        env: HashMap::new(),
        inherit_env: Vec::new(),
        sandbox: None,
        network: McpStdioNetwork::Inherit,
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
        ("invalid-utf8-server", "invalid-utf8"),
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
        let pid = wait_for_pid(&lease).await;
        assert_process_reaped(pid).await;
    }

    let large_error_lease = fixture.lease("large-error-server");
    let large_error = fixture.config(
        fixture.executable.display().to_string(),
        Vec::new(),
        "large-error",
        &large_error_lease,
    );
    let error = McpClientHandle::connect_with_timeout(
        CompactString::new("large-error-server"),
        &large_error,
        Duration::from_secs(2),
    )
    .await
    .err()
    .expect("large initialization error must reject the connection");
    let message = error.to_string();
    assert!(message.len() <= 8 * 1024);
    assert!(message.contains("initialization failed"));
    assert!(!message.contains("MCP_LARGE_ERROR_TAIL"));
    assert_process_reaped(wait_for_pid(&large_error_lease).await).await;

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
    let timeout_pid = wait_for_pid(&timeout_lease).await;
    assert_process_reaped(timeout_pid).await;

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
    let cancel_pid = wait_for_pid(&cancel_lease).await;
    connect_task.abort();
    assert!(matches!(
        connect_task.await,
        Err(error) if error.is_cancelled()
    ));
    assert_process_reaped(cancel_pid).await;

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
        let pid = wait_for_pid(&lease).await;
        shutdown(McpClientManager {
            handles: vec![handle],
            notices: Vec::new(),
        })
        .await;
        assert_process_reaped(pid).await;
    }

    let unavailable_lease = fixture.lease("unavailable-sandbox");
    let mut unavailable = fixture.config(
        fixture.executable.display().to_string(),
        Vec::new(),
        "normal",
        &unavailable_lease,
    );
    if let McpServerConfig::Command { sandbox, .. } = &mut unavailable {
        *sandbox = Some("__mini_agent_missing_mcp_sandbox__".to_string());
    }
    let unavailable_error =
        McpClientHandle::connect(CompactString::new("unavailable-sandbox"), &unavailable)
            .await
            .err()
            .expect("an unavailable requested sandbox must deny launch");
    assert!(
        unavailable_error
            .to_string()
            .contains("requested-but-unavailable")
    );
    assert!(
        !unavailable_lease.exists(),
        "denied launch must start no child"
    );

    let denied_network_lease = fixture.lease("unenforced-network");
    let mut denied_network = fixture.config(
        fixture.executable.display().to_string(),
        Vec::new(),
        "normal",
        &denied_network_lease,
    );
    if let McpServerConfig::Command { network, .. } = &mut denied_network {
        *network = McpStdioNetwork::Deny;
    }
    let network_error =
        McpClientHandle::connect(CompactString::new("unenforced-network"), &denied_network)
            .await
            .err()
            .expect("unenforced network denial must deny launch");
    assert!(network_error.to_string().contains("network denial"));
    assert!(!denied_network_lease.exists());

    fixture.cleanup();
}

#[cfg(unix)]
#[tokio::test]
async fn mcp_stdio_drop_and_reconnect_reap_process_trees() {
    let fixture = FixtureBuild::compile();

    let dropped_lease = fixture.lease("dropped-parent");
    let dropped_descendant = fixture.descendant("dropped-child");
    let mut dropped_config = fixture.config(
        fixture.executable.display().to_string(),
        Vec::new(),
        "descendant",
        &dropped_lease,
    );
    if let McpServerConfig::Command { env, .. } = &mut dropped_config {
        env.insert(
            "MCP_FIXTURE_DESCENDANT_FILE".to_string(),
            dropped_descendant.display().to_string(),
        );
    }
    let dropped = McpClientHandle::connect(CompactString::new("fixture"), &dropped_config)
        .await
        .unwrap();
    let dropped_pid = wait_for_pid(&dropped_lease).await;
    let descendant_pid = wait_for_pid(&dropped_descendant).await;
    drop(dropped);
    assert_process_reaped(dropped_pid).await;
    assert_process_reaped(descendant_pid).await;

    let first_lease = fixture.lease("reconnect-first");
    let exclusive = fixture.root.join("reconnect-exclusive-resource");
    let mut first = fixture.config(
        fixture.executable.display().to_string(),
        vec!["first".to_string()],
        "normal",
        &first_lease,
    );
    if let McpServerConfig::Command { env, .. } = &mut first {
        env.insert(
            "MCP_FIXTURE_EXCLUSIVE_FILE".to_string(),
            exclusive.display().to_string(),
        );
    }
    let first_handle = McpClientHandle::connect(CompactString::new("fixture"), &first)
        .await
        .unwrap();
    let mut manager = McpClientManager {
        handles: vec![first_handle],
        notices: Vec::new(),
    };
    let first_pid = wait_for_pid(&first_lease).await;

    let second_lease = fixture.lease("reconnect-second");
    let mut second = fixture.config(
        fixture.executable.display().to_string(),
        vec!["second".to_string()],
        "normal",
        &second_lease,
    );
    if let McpServerConfig::Command { env, .. } = &mut second {
        env.insert(
            "MCP_FIXTURE_EXCLUSIVE_FILE".to_string(),
            exclusive.display().to_string(),
        );
    }
    manager.reconnect("fixture", &second).await.unwrap();
    assert_process_reaped(first_pid).await;
    assert_eq!(call_fixture_tool(&manager).await["args"][0], "second");
    let second_pid = wait_for_pid(&second_lease).await;
    shutdown(manager).await;
    assert_process_reaped(second_pid).await;

    fixture.cleanup();
}
