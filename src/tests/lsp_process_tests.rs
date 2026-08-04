use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use compact_str::CompactString;
use tokio::sync::Notify;

use crate::config::types::{LspConfig, LspNetwork, LspServerConfig};
use crate::extras::lsp::LspManager;
use crate::extras::lsp::client::{DiagStore, LspClient, file_uri};

const FIXTURE_SOURCE: &str = r#"
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

#[cfg(unix)]
fn close_stdin() {
    unsafe extern "C" {
        fn close(fd: i32) -> i32;
    }
    unsafe {
        close(0);
    }
}

#[cfg(windows)]
fn close_stdin() {
    use std::ffi::c_void;
    unsafe extern "system" {
        fn GetStdHandle(kind: u32) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    const STD_INPUT_HANDLE: u32 = -10_i32 as u32;
    unsafe {
        CloseHandle(GetStdHandle(STD_INPUT_HANDLE));
    }
}

fn read_frame(reader: &mut impl BufRead) -> Option<String> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0_u8; content_length?];
    reader.read_exact(&mut body).ok()?;
    String::from_utf8(body).ok()
}

fn write_frame(writer: &mut impl Write, body: &str) {
    write!(writer, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
    writer.write_all(body.as_bytes()).unwrap();
    writer.flush().unwrap();
}

fn request_id(body: &str) -> String {
    let Some(rest) = body.split_once("\"id\":").map(|(_, rest)| rest) else {
        return "null".to_string();
    };
    rest.chars()
        .skip_while(|character| character.is_whitespace())
        .take_while(|character| *character != ',' && *character != '}')
        .collect()
}

fn write_probe(body: &str) {
    let Some(path) = env::var_os("LSP_FIXTURE_PROBE_FILE") else {
        return;
    };
    let value = format!(
        "cwd={}\nambient={}\nexplicit={}\ninherited={}\npath={}\ninitialize={}\n",
        env::current_dir().unwrap().display(),
        env::var("MINI_AGENT_LSP_AMBIENT_CANARY").unwrap_or_else(|_| "<missing>".to_string()),
        env::var("MINI_AGENT_LSP_EXPLICIT_CANARY").unwrap_or_else(|_| "<missing>".to_string()),
        env::var("MINI_AGENT_LSP_INHERITED_CANARY").unwrap_or_else(|_| "<missing>".to_string()),
        env::var("PATH").unwrap_or_else(|_| "<missing>".to_string()),
        body,
    );
    fs::write(path, value).unwrap();
}

fn append_launch(pid: u32) {
    let Some(path) = env::var_os("LSP_FIXTURE_LAUNCH_LOG") else {
        return;
    };
    let mut file = OpenOptions::new().create(true).append(true).open(path).unwrap();
    writeln!(file, "{pid}").unwrap();
}

fn main() {
    let mode = env::var("LSP_FIXTURE_MODE").unwrap_or_else(|_| "normal".to_string());
    if mode == "descendant-child" {
        fs::write(
            env::var_os("LSP_FIXTURE_DESCENDANT_FILE").unwrap(),
            std::process::id().to_string(),
        )
        .unwrap();
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }

    if let Some(path) = env::var_os("LSP_FIXTURE_LEASE_FILE") {
        fs::write(path, std::process::id().to_string()).unwrap();
    }
    append_launch(std::process::id());

    if mode == "descendant" || mode == "descendant-hang-init" {
        Command::new(env::current_exe().unwrap())
            .env("LSP_FIXTURE_MODE", "descendant-child")
            .env(
                "LSP_FIXTURE_DESCENDANT_FILE",
                env::var_os("LSP_FIXTURE_DESCENDANT_FILE").unwrap(),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
    }

    if mode == "early-exit" {
        return;
    }

    if mode == "stderr-flood" {
        let mut stderr = io::stderr().lock();
        let chunk = vec![b'x'; 2 * 1024 * 1024];
        stderr.write_all(&chunk).unwrap();
        stderr.flush().unwrap();
    }

    let restart_first = mode == "restart"
        && env::var_os("LSP_FIXTURE_RESTART_MARKER").is_some_and(|path| {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .is_ok()
        });

    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = io::stdout().lock();
    while let Some(body) = read_frame(&mut reader) {
        if body.contains("\"method\":\"initialize\"") {
            write_probe(&body);
            match mode.as_str() {
                "hang-init" | "descendant-hang-init" => continue,
                "malformed" => {
                    write_frame(&mut stdout, "{");
                    continue;
                }
                "oversized-frame" => {
                    stdout
                        .write_all(b"Content-Length: 99999999\r\n\r\n")
                        .unwrap();
                    stdout.flush().unwrap();
                    continue;
                }
                _ => {}
            }
            let id = request_id(&body);
            write_frame(
                &mut stdout,
                &format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"capabilities\":{{}}}}}}"),
            );
        } else if body.contains("\"method\":\"initialized\"") {
            if restart_first {
                return;
            }
            if mode == "close-stdin" {
                if let Some(path) = env::var_os("LSP_FIXTURE_STDIN_CLOSED_FILE") {
                    fs::write(path, "closed").unwrap();
                }
                break;
            }
        } else if body.contains("\"method\":\"textDocument/didOpen\"")
            || body.contains("\"method\":\"textDocument/didChange\"")
        {
            if let Some(path) = env::var_os("LSP_FIXTURE_SYNC_LOG") {
                let mut file = OpenOptions::new().create(true).append(true).open(path).unwrap();
                writeln!(file, "{body}").unwrap();
            }
        } else if body.contains("\"method\":\"mini-agent/test\"") {
            if let Some(path) = env::var_os("LSP_FIXTURE_REQUEST_FILE") {
                fs::write(path, "seen").unwrap();
            }
            // Deliberately leave the request pending.
        }
    }
    drop(reader);
    if mode == "close-stdin" {
        close_stdin();
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
}

impl FixtureBuild {
    fn compile(test_name: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "mini agent lsp process {test_name} {} {id}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("fixture.rs");
        fs::write(&source, FIXTURE_SOURCE).unwrap();
        let executable = root.join(if cfg!(windows) {
            "lsp-process-fixture.exe"
        } else {
            "lsp-process-fixture"
        });
        let output =
            Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc")))
                .arg("--edition=2024")
                .arg(&source)
                .arg("-o")
                .arg(&executable)
                .output()
                .expect("Rust toolchain must compile the repository-owned LSP fixture");
        assert!(
            output.status.success(),
            "fixture compilation failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self { root, executable }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn workspace(&self, name: &str) -> PathBuf {
        let workspace = self.root.join(name);
        fs::create_dir_all(&workspace).unwrap();
        workspace
    }

    fn config(&self, mode: &str, lease: &Path) -> LspServerConfig {
        LspServerConfig {
            command: CompactString::new(self.executable.display().to_string()),
            args: Vec::new(),
            extensions: vec![CompactString::new(".probe")],
            env: HashMap::from([
                ("LSP_FIXTURE_MODE".to_string(), mode.to_string()),
                (
                    "LSP_FIXTURE_LEASE_FILE".to_string(),
                    lease.display().to_string(),
                ),
            ]),
            inherit_env: Vec::new(),
            sandbox: None,
            network: LspNetwork::Inherit,
            initialization: None,
            disabled: false,
        }
    }

    fn cleanup(self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match fs::remove_dir_all(&self.root) {
                Ok(()) => return,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                #[cfg(windows)]
                Err(error)
                    if matches!(error.raw_os_error(), Some(32 | 33))
                        && Instant::now() < deadline =>
                {
                    // Windows can retain a reaped child's executable handle for
                    // a short interval after process termination.
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => {
                    panic!(
                        "fixture directory could not be removed; an LSP child may still be running ({}): {error}",
                        self.root.display()
                    )
                }
            }
        }
    }
}

struct EnvGuard(Vec<(String, Option<OsString>)>);

impl EnvGuard {
    fn set(values: &[(&str, &str)]) -> Self {
        let original = values
            .iter()
            .map(|(name, _)| ((*name).to_string(), std::env::var_os(name)))
            .collect();
        for (name, value) in values {
            // SAFETY: these test-only, uniquely named variables are restored by
            // Drop and no production code mutates them.
            unsafe { std::env::set_var(name, value) };
        }
        Self(original)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.0 {
            // SAFETY: restores the process environment snapshot from `set`.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

fn client_parts() -> (DiagStore, Arc<Notify>) {
    (DiagStore::default(), Arc::new(Notify::new()))
}

async fn spawn_client(
    cfg: &LspServerConfig,
    root: &Path,
    timeout: Duration,
) -> Option<Arc<LspClient>> {
    let (diags, notify) = client_parts();
    LspClient::spawn_with_timeout("fixture", cfg, root, diags, notify, timeout).await
}

async fn wait_for_file(path: &Path) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(value) = fs::read_to_string(path) {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fixture did not create {}", path.display()))
}

async fn wait_for_file_contains(path: &Path, pattern: &str) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(value) = fs::read_to_string(path)
                && value.contains(pattern)
            {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "fixture output {} did not contain {pattern}",
            path.display()
        )
    })
}

async fn wait_for_pid(path: &Path) -> u32 {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(value) = fs::read_to_string(path)
                && let Ok(pid) = value.trim().parse::<u32>()
                && pid != 0
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fixture PID was invalid in {}", path.display()))
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

fn launch_pids(value: &str) -> Vec<u32> {
    value
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

#[tokio::test]
async fn lsp_process_launch_uses_canonical_root_and_delegated_environment() {
    let fixture = FixtureBuild::compile("launch-policy");
    let workspace = fixture.workspace("workspace with spaces");
    let nested = workspace.join("nested");
    fs::create_dir_all(&nested).unwrap();
    let non_canonical_root = nested.join("..");
    let lease = fixture.path("launch.lease");
    let probe = fixture.path("launch.probe-output");
    let _env = EnvGuard::set(&[
        ("MINI_AGENT_LSP_AMBIENT_CANARY", "must-not-cross"),
        ("MINI_AGENT_LSP_EXPLICIT_CANARY", "parent-value"),
        ("MINI_AGENT_LSP_INHERITED_CANARY", "inherited-value"),
    ]);
    let mut cfg = fixture.config("normal", &lease);
    cfg.env.insert(
        "LSP_FIXTURE_PROBE_FILE".to_string(),
        probe.display().to_string(),
    );
    cfg.env.insert(
        "MINI_AGENT_LSP_EXPLICIT_CANARY".to_string(),
        "configured-value".to_string(),
    );
    cfg.inherit_env = vec![
        "MINI_AGENT_LSP_EXPLICIT_CANARY".to_string(),
        "MINI_AGENT_LSP_INHERITED_CANARY".to_string(),
    ];

    let client = spawn_client(&cfg, &non_canonical_root, Duration::from_secs(2))
        .await
        .expect("fixture must initialize");
    let parent_pid = wait_for_pid(&lease).await;
    let observed = wait_for_file(&probe).await;
    let canonical = workspace.canonicalize().unwrap();
    let observed_cwd = observed
        .lines()
        .find_map(|line| line.strip_prefix("cwd="))
        .expect("child probe should report its working directory");
    assert_eq!(
        PathBuf::from(observed_cwd).canonicalize().unwrap(),
        canonical,
        "child cwd was not canonical: {observed}"
    );
    assert!(observed.contains("ambient=<missing>"), "{observed}");
    assert!(observed.contains("explicit=configured-value"), "{observed}");
    assert!(observed.contains("inherited=inherited-value"), "{observed}");
    assert!(observed.contains("path=<missing>"), "{observed}");
    assert!(
        observed.contains(&format!(
            "\"rootUri\":\"{}\"",
            file_uri(&canonical).unwrap()
        )),
        "initialize rootUri did not use the canonical root: {observed}"
    );

    client.shutdown().await;
    assert_process_reaped(parent_pid).await;
    fixture.cleanup();
}

#[tokio::test]
async fn lsp_process_initialization_failures_are_bounded_and_reaped() {
    let fixture = FixtureBuild::compile("initialization-failures");
    let workspace = fixture.workspace("workspace");
    for (name, mode, timeout) in [
        ("timeout", "hang-init", Duration::from_millis(150)),
        ("early-exit", "early-exit", Duration::from_secs(2)),
        ("malformed", "malformed", Duration::from_secs(2)),
        ("oversized", "oversized-frame", Duration::from_secs(2)),
        ("stderr", "stderr-flood", Duration::from_secs(2)),
    ] {
        let lease = fixture.path(&format!("{name}.lease"));
        let cfg = fixture.config(mode, &lease);
        let client = spawn_client(&cfg, &workspace, timeout).await;
        assert!(client.is_none(), "{mode} server unexpectedly initialized");
        assert_process_reaped(wait_for_pid(&lease).await).await;
    }
    fixture.cleanup();
}

#[tokio::test]
async fn lsp_process_cancelled_initialization_reaps_descendants() {
    let fixture = FixtureBuild::compile("cancelled-initialization");
    let workspace = fixture.workspace("workspace");
    let lease = fixture.path("cancel.lease");
    let descendant = fixture.path("cancel.descendant");
    let mut cfg = fixture.config("descendant-hang-init", &lease);
    cfg.env.insert(
        "LSP_FIXTURE_DESCENDANT_FILE".to_string(),
        descendant.display().to_string(),
    );

    let spawn_root = workspace.clone();
    let spawn =
        tokio::spawn(async move { spawn_client(&cfg, &spawn_root, Duration::from_secs(30)).await });
    let parent_pid = wait_for_pid(&lease).await;
    let descendant_pid = wait_for_pid(&descendant).await;
    spawn.abort();
    assert!(matches!(spawn.await, Err(error) if error.is_cancelled()));
    assert_process_reaped(parent_pid).await;
    assert_process_reaped(descendant_pid).await;

    fixture.cleanup();
}

#[tokio::test]
async fn lsp_process_pending_request_cancellation_removes_entry() {
    let fixture = FixtureBuild::compile("pending-cancellation");
    let workspace = fixture.workspace("workspace");
    let lease = fixture.path("pending.lease");
    let request_seen = fixture.path("request.seen");
    let mut cfg = fixture.config("normal", &lease);
    cfg.env.insert(
        "LSP_FIXTURE_REQUEST_FILE".to_string(),
        request_seen.display().to_string(),
    );
    let client = spawn_client(&cfg, &workspace, Duration::from_secs(2))
        .await
        .expect("fixture must initialize");
    let parent_pid = wait_for_pid(&lease).await;

    let request_client = client.clone();
    let request = tokio::spawn(async move {
        request_client
            .request_for_test(Duration::from_secs(30))
            .await
    });
    wait_for_file(&request_seen).await;
    assert_eq!(client.pending_len_for_test(), 1);
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    assert_eq!(client.pending_len_for_test(), 0);

    client.shutdown().await;
    assert_process_reaped(parent_pid).await;
    fixture.cleanup();
}

#[tokio::test]
async fn lsp_process_oversized_document_does_not_poison_sync_state() {
    let fixture = FixtureBuild::compile("oversized-document");
    let workspace = fixture.workspace("workspace");
    let source = workspace.join("document.probe");
    let lease = fixture.path("document.lease");
    let sync_log = fixture.path("document.sync-log");
    let mut cfg = fixture.config("normal", &lease);
    cfg.env.insert(
        "LSP_FIXTURE_SYNC_LOG".to_string(),
        sync_log.display().to_string(),
    );
    let client = spawn_client(&cfg, &workspace, Duration::from_secs(2))
        .await
        .expect("fixture must initialize");
    let parent_pid = wait_for_pid(&lease).await;

    fs::write(&source, vec![b'x'; 4 * 1024 * 1024 + 1]).unwrap();
    client.sync_file(&source).await;
    assert!(!sync_log.exists(), "oversized document was synchronized");

    fs::write(&source, "small document").unwrap();
    client.sync_file(&source).await;
    let first = wait_for_file(&sync_log).await;
    assert!(first.contains("textDocument/didOpen"), "{first}");
    assert!(first.contains("\"version\":1"), "{first}");
    assert!(!first.contains("textDocument/didChange"), "{first}");

    fs::write(&source, "changed document").unwrap();
    client.sync_file(&source).await;
    let second = wait_for_file_contains(&sync_log, "textDocument/didChange").await;
    assert!(second.contains("textDocument/didChange"), "{second}");
    assert!(second.contains("\"version\":2"), "{second}");

    client.shutdown().await;
    assert_process_reaped(parent_pid).await;
    fixture.cleanup();
}

#[tokio::test]
async fn lsp_process_broken_stdin_is_terminal_and_reaped() {
    let fixture = FixtureBuild::compile("broken-stdin");
    let workspace = fixture.workspace("workspace");
    let source = workspace.join("document.probe");
    fs::write(&source, "document").unwrap();
    let lease = fixture.path("broken.lease");
    let closed = fixture.path("stdin.closed");
    let mut cfg = fixture.config("close-stdin", &lease);
    cfg.env.insert(
        "LSP_FIXTURE_STDIN_CLOSED_FILE".to_string(),
        closed.display().to_string(),
    );
    let client = spawn_client(&cfg, &workspace, Duration::from_secs(2))
        .await
        .expect("fixture must initialize before closing stdin");
    let parent_pid = wait_for_pid(&lease).await;
    wait_for_file(&closed).await;

    client.sync_file(&source).await;
    assert!(client.is_stopped());
    assert_process_reaped(parent_pid).await;
    fixture.cleanup();
}

#[tokio::test]
async fn lsp_process_shutdown_and_drop_reap_descendants() {
    let fixture = FixtureBuild::compile("descendants");
    let workspace = fixture.workspace("workspace");
    for (name, explicit_shutdown) in [("shutdown", true), ("drop", false)] {
        let lease = fixture.path(&format!("{name}.lease"));
        let descendant = fixture.path(&format!("{name}.descendant"));
        let mut cfg = fixture.config("descendant", &lease);
        cfg.env.insert(
            "LSP_FIXTURE_DESCENDANT_FILE".to_string(),
            descendant.display().to_string(),
        );
        let client = spawn_client(&cfg, &workspace, Duration::from_secs(2))
            .await
            .expect("fixture must initialize");
        let parent_pid = wait_for_pid(&lease).await;
        let descendant_pid = wait_for_pid(&descendant).await;
        if explicit_shutdown {
            client.shutdown().await;
        } else {
            drop(client);
        }
        assert_process_reaped(parent_pid).await;
        assert_process_reaped(descendant_pid).await;
    }
    fixture.cleanup();
}

#[tokio::test]
async fn lsp_process_requested_boundary_failure_starts_no_child() {
    let fixture = FixtureBuild::compile("boundary-failure");
    let workspace = fixture.workspace("workspace");

    let sandbox_lease = fixture.path("sandbox.lease");
    let mut unavailable = fixture.config("normal", &sandbox_lease);
    unavailable.sandbox = Some(CompactString::new("__mini_agent_missing_lsp_sandbox__"));
    assert!(
        spawn_client(&unavailable, &workspace, Duration::from_millis(200))
            .await
            .is_none()
    );
    assert!(
        !sandbox_lease.exists(),
        "unavailable sandbox started a child"
    );

    let network_lease = fixture.path("network.lease");
    let mut denied_network = fixture.config("normal", &network_lease);
    denied_network.network = LspNetwork::Deny;
    assert!(
        spawn_client(&denied_network, &workspace, Duration::from_millis(200))
            .await
            .is_none()
    );
    assert!(
        !network_lease.exists(),
        "unenforced network denial started a child"
    );

    fixture.cleanup();
}

#[tokio::test]
async fn lsp_process_manager_restarts_stopped_server() {
    let fixture = FixtureBuild::compile("manager-restart");
    let workspace = fixture.workspace("workspace");
    let source = workspace.join("restart.probe");
    fs::write(&source, "restart fixture").unwrap();
    let lease = fixture.path("restart.lease");
    let launch_log = fixture.path("restart.launches");
    let restart_marker = fixture.path("restart.marker");
    let mut server = fixture.config("restart", &lease);
    server.env.insert(
        "LSP_FIXTURE_LAUNCH_LOG".to_string(),
        launch_log.display().to_string(),
    );
    server.env.insert(
        "LSP_FIXTURE_RESTART_MARKER".to_string(),
        restart_marker.display().to_string(),
    );
    let manager = LspManager::new(
        &LspConfig {
            enabled: true,
            servers: HashMap::from([("fixture".to_string(), server)]),
        },
        workspace.clone(),
    );

    manager.notify_changed(&source).await;
    let first_pid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let launches = fs::read_to_string(&launch_log).unwrap_or_default();
            if let Some(pid) = launch_pids(&launches).first().copied() {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("first LSP launch was not recorded");
    assert_process_reaped(first_pid).await;

    let second_pid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            manager.notify_changed(&source).await;
            let launches = fs::read_to_string(&launch_log).unwrap_or_default();
            if let Some(pid) = launch_pids(&launches).get(1).copied() {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("manager did not restart the stopped LSP server");
    assert_ne!(first_pid, second_pid);
    assert!(process_is_alive(second_pid));

    manager.shutdown().await;
    assert_process_reaped(second_pid).await;
    // The manager deliberately retains a stable workspace directory handle.
    // Release it before asserting Windows can remove the fixture tree.
    drop(manager);
    fixture.cleanup();
}
