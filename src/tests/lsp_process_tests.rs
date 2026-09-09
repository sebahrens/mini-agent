use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use compact_str::CompactString;
use rig::tool::Tool;
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
            if mode == "stop-reading" {
                // Stay alive with stdin open but never read again: the client's
                // pipe fills and its writes block.
                loop {
                    thread::sleep(Duration::from_secs(60));
                }
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
            if mode == "diagnostics" {
                let wire_uri = body.split_once("\"uri\":").unwrap().1.split('"').nth(1).unwrap();
                let uri = env::var("LSP_FIXTURE_CANONICAL_URI").unwrap_or_else(|_| wire_uri.into());
                let version: i64 = body.split_once("\"version\":").unwrap().1
                    .chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap();
                if env::var("LSP_FIXTURE_DELAY_VERSION").ok().as_deref() == Some(&version.to_string())
                    && uri.ends_with("/document.probe")
                {
                    fs::write(env::var_os("LSP_FIXTURE_READY_FILE").unwrap(), "ready").unwrap();
                    let release = env::var_os("LSP_FIXTURE_RELEASE_FILE").unwrap();
                    let deadline = std::time::Instant::now() + Duration::from_secs(15);
                    while !std::path::Path::new(&release).exists() {
                        assert!(std::time::Instant::now() < deadline, "diagnostic gate timed out");
                        thread::sleep(Duration::from_millis(5));
                    }
                }
                let mut publish = |uri: &str, version: i64| {
                    write_frame(&mut stdout, &format!(
                        "{{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/publishDiagnostics\",\"params\":{{\"uri\":\"{uri}\",\"version\":{version},\"diagnostics\":[{{\"range\":{{\"start\":{{\"line\":0,\"character\":0}},\"end\":{{\"line\":0,\"character\":1}}}},\"severity\":1,\"message\":\"fixture diagnostic version {version}\"}}]}}}}"
                    ));
                };
                if let Ok(outside) = env::var("LSP_FIXTURE_OUTSIDE_URI") {
                    publish(&outside, version);
                }
                // A stale reply must not advance the parent's publish counter.
                for version in [version - 1, version] {
                    publish(&uri, version);
                }
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

    // Callers must first drop clients and managers: their workspace bindings
    // intentionally prevent directory deletion on Windows, even after reaping.
    async fn cleanup(self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match fs::remove_dir_all(&self.root) {
                Ok(()) => return,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                Err(error)
                    if cfg!(windows)
                        && matches!(error.raw_os_error(), Some(32 | 33))
                        && Instant::now() < deadline =>
                {
                    // Process exit can precede protocol-task drain. Yield so
                    // those tasks can release workspace/executable handles on
                    // the current-thread runtime used by these tests.
                    tokio::time::sleep(Duration::from_millis(20)).await;
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

struct EnvGuard {
    _environment: crate::tests::ScopedProcessEnv,
}

impl EnvGuard {
    fn set(values: &[(&str, &str)]) -> Self {
        let values = values
            .iter()
            .map(|(name, value)| (*name, Some(OsString::from(value))))
            .collect::<Vec<_>>();
        Self {
            _environment: crate::tests::ScopedProcessEnv::set(&values),
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
    drop(client);
    fixture.cleanup().await;
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
    fixture.cleanup().await;
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

    // A process can be gone while a protocol task still owns its directory
    // binding. Keep that case deterministic on Windows: this current-thread
    // runtime cannot release the extra binding until cleanup yields to it.
    #[cfg(windows)]
    let release_binding = {
        let binding = crate::paths::WorkspaceBinding::capture(&workspace).unwrap();
        tokio::spawn(async move { drop(binding) })
    };
    fixture.cleanup().await;
    #[cfg(windows)]
    release_binding.await.unwrap();
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
    drop(client);
    fixture.cleanup().await;
}

#[tokio::test]
async fn lsp_process_publications_reach_parent_cache_before_diagnostic_wait() {
    let fixture = FixtureBuild::compile("diagnostic-publication");
    let outside = fixture.path("outside.probe");
    fs::write(&outside, "outside workspace").unwrap();
    let outside_uri = file_uri(&outside.canonicalize().unwrap()).unwrap();
    let mut results = Vec::new();
    for (relative, canonical_reply) in [(false, false), (true, false), (false, true), (true, true)]
    {
        let workspace = fixture
            .workspace(&format!("workspace-{relative}-{canonical_reply}"))
            .canonicalize()
            .unwrap();
        let source = workspace.join("document.probe");
        let uri = file_uri(&source).unwrap();
        let mut server = fixture.config("diagnostics", &fixture.path("diagnostics.lease"));
        server
            .env
            .insert("LSP_FIXTURE_OUTSIDE_URI".into(), outside_uri.clone());
        if canonical_reply {
            server
                .env
                .insert("LSP_FIXTURE_CANONICAL_URI".into(), uri.clone());
        }
        let manager = LspManager::new(
            &LspConfig {
                enabled: true,
                servers: HashMap::from([("fixture".into(), server)]),
            },
            workspace.clone(),
        );
        for version in [1, 2] {
            fs::write(&source, format!("document version {version}")).unwrap();
            let baseline = if relative {
                manager
                    .notify_changed_relative(Path::new("document.probe"))
                    .await
            } else {
                manager.notify_changed(&source).await
            };
            // Let the real reply arrive before starting the waiter. A baseline
            // sampled after sync would miss this reply and wait for another.
            let published = tokio::time::timeout(Duration::from_secs(2), async {
                while manager
                    .diagnostic_cache_entry_metrics(&uri)
                    .is_none_or(|(counter, _)| counter <= baseline.unwrap_or(0))
                {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .is_ok();
            let output = tokio::time::timeout(Duration::from_millis(250), async {
                if relative {
                    manager
                        .diagnostics_block_for_relative_edit(Path::new("document.probe"), baseline)
                        .await
                } else {
                    manager.diagnostics_block_for_edit(&source, baseline).await
                }
            })
            .await;
            results.push((
                relative,
                canonical_reply,
                version,
                baseline,
                published,
                manager.diagnostic_cache_entry_metrics(&uri),
                manager.diagnostic_candidate_uris() == vec![uri.clone()],
                output,
            ));
        }
        manager.shutdown().await;
    }
    fixture.cleanup().await;
    for (
        relative,
        canonical_reply,
        version,
        baseline,
        published,
        entry,
        only_expected_uri,
        output,
    ) in results
    {
        let case =
            format!("relative={relative}, canonical_reply={canonical_reply}, version={version}");
        assert!(baseline.is_some(), "{case}: synchronization failed");
        assert!(
            published,
            "{case}: real publication never reached the parent cache"
        );
        assert_eq!(
            entry,
            Some((version, 1)),
            "{case}: stale reply was accepted"
        );
        assert!(
            only_expected_uri,
            "{case}: outside-workspace reply was accepted"
        );
        let output = output
            .expect("already-published diagnostics must not wait for a second publish")
            .expect("server diagnostic must reach the caller");
        assert!(
            output.contains(&format!("fixture diagnostic version {version}")),
            "{case}: {output}"
        );
    }
}

fn rewrite_preserving_length_and_mtime(path: &Path) {
    let before = crate::fs::checked_path_metadata(path).unwrap();
    fs::write(path, "x".repeat(before.len() as usize)).unwrap();
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(before.modified().unwrap()))
        .unwrap();
    let after = crate::fs::checked_path_metadata(path).unwrap();
    crate::fs::ensure_same_file(path, &before, &after).unwrap();
    assert_eq!(before.len(), after.len());
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
}

#[tokio::test]
async fn lsp_process_delayed_publication_rejects_changed_source() {
    let fixture = FixtureBuild::compile("delayed-identity");
    let mut results = Vec::new();
    for replace in [false, true] {
        for relative in [false, true] {
            for delayed_version in [1, 2] {
                let workspace = fixture
                    .workspace(&format!("workspace-{replace}-{relative}-{delayed_version}"))
                    .canonicalize()
                    .unwrap();
                let source = workspace.join("document.probe");
                let uri = file_uri(&source).unwrap();
                let ready = workspace.join("ready");
                let release = workspace.join("release");
                let mut server = fixture.config("diagnostics", &workspace.join("lease"));
                for (name, value) in [
                    ("LSP_FIXTURE_DELAY_VERSION", delayed_version.to_string()),
                    ("LSP_FIXTURE_READY_FILE", ready.display().to_string()),
                    ("LSP_FIXTURE_RELEASE_FILE", release.display().to_string()),
                ] {
                    server.env.insert(name.into(), value);
                }
                let manager = LspManager::new(
                    &LspConfig {
                        enabled: true,
                        servers: HashMap::from([("fixture".into(), server)]),
                    },
                    workspace.clone(),
                );
                for version in 1..=delayed_version {
                    fs::write(&source, format!("original version {version}")).unwrap();
                    let baseline = if relative {
                        manager
                            .notify_changed_relative(Path::new("document.probe"))
                            .await
                    } else {
                        manager.notify_changed(&source).await
                    };
                    assert!(baseline.is_some());
                    if version < delayed_version {
                        assert!(
                            manager
                                .diagnostics_block_for_edit(&source, baseline)
                                .await
                                .is_some()
                        );
                    }
                }
                wait_for_file(&ready).await;
                let before = manager.diagnostic_cache_entry_metrics(&uri);
                if replace {
                    fs::rename(&source, workspace.join("original.probe")).unwrap();
                    fs::write(&source, "replacement").unwrap();
                } else {
                    rewrite_preserving_length_and_mtime(&source);
                }
                fs::write(&release, "release").unwrap();
                // A second document is a protocol barrier: its accepted diagnostic
                // proves the reader has processed the preceding delayed reply.
                let marker = workspace.join("marker.probe");
                fs::write(&marker, "marker").unwrap();
                let baseline = manager.notify_changed(&marker).await;
                let barrier = manager.diagnostics_block_for_edit(&marker, baseline).await;
                let after = manager.diagnostic_cache_entry_metrics(&uri);
                let stale = manager
                    .diagnostics_block_since(&source, Duration::ZERO, None)
                    .await;
                let baseline = if relative {
                    manager
                        .notify_changed_relative(Path::new("document.probe"))
                        .await
                } else {
                    manager.notify_changed(&source).await
                };
                let refreshed = manager.diagnostics_block_for_edit(&source, baseline).await;
                results.push((
                    replace,
                    relative,
                    delayed_version,
                    before,
                    after,
                    barrier,
                    stale,
                    refreshed,
                ));
                manager.shutdown().await;
                drop(manager);
            }
        }
    }
    fixture.cleanup().await;
    for (replace, relative, version, before, after, barrier, stale, refreshed) in results {
        let case = format!("replace={replace}, relative={relative}, delayed_version={version}");
        assert!(barrier.is_some(), "{case}: protocol barrier failed");
        assert_eq!(
            before, after,
            "{case}: replaced-file publication changed cache"
        );
        assert!(
            stale.is_none(),
            "{case}: old-text diagnostics reached replacement"
        );
        assert!(
            refreshed
                .unwrap()
                .contains(&format!("fixture diagnostic version {}", version + 1)),
            "{case}: replacement could not synchronize"
        );
    }
}

#[tokio::test]
async fn lsp_process_sync_capacity_preserves_updates_to_tracked_documents() {
    let fixture = FixtureBuild::compile("sync-capacity");
    let workspace = fixture.workspace("workspace").canonicalize().unwrap();
    let sync_log = workspace.join("sync.log");
    let mut server = fixture.config("diagnostics", &workspace.join("lease"));
    server.env.insert(
        "LSP_FIXTURE_SYNC_LOG".into(),
        sync_log.display().to_string(),
    );
    let manager = LspManager::new(
        &LspConfig {
            enabled: true,
            servers: HashMap::from([("fixture".into(), server)]),
        },
        workspace.clone(),
    );
    let cap = crate::extras::lsp::client::MAX_DIAGNOSTIC_FILES;
    let mut accepted = Vec::new();
    for index in 0..=cap {
        let source = workspace.join(format!("document-{index}.probe"));
        fs::write(&source, "document").unwrap();
        accepted.push(manager.notify_changed(&source).await.is_some());
    }
    let first = workspace.join("document-0.probe");
    // Replacing a tracked file must release its old identity and keep the slot.
    fs::rename(&first, workspace.join("original.probe")).unwrap();
    fs::write(&first, "replacement").unwrap();
    let baseline = manager.notify_changed(&first).await;
    let updated = manager.diagnostics_block_for_edit(&first, baseline).await;
    // The update reply is also a barrier for all preceding protocol frames.
    let log = fs::read_to_string(&sync_log).unwrap();
    manager.shutdown().await;
    drop(manager);
    fixture.cleanup().await;
    assert!(accepted[..cap].iter().all(|accepted| *accepted));
    assert!(
        !accepted[cap],
        "new document accepted above the sync ceiling"
    );
    assert!(baseline.is_some());
    assert!(updated.unwrap().contains("fixture diagnostic version 2"));
    assert_eq!(log.matches("textDocument/didOpen").count(), cap);
    assert_eq!(log.matches("textDocument/didChange").count(), 1);
    assert!(!log.contains(&format!("/document-{cap}.probe")));
}

#[tokio::test]
async fn lsp_process_diagnostics_reach_real_write_edit_and_query_tools() {
    use crate::agent::tools::{
        EditArgs, WriteArgs,
        edit::EditTool,
        lsp::{LspArgs, LspTool},
        write::WriteTool,
    };
    let fixture = FixtureBuild::compile("tool-diagnostics");
    let workspace = fixture.workspace("workspace").canonicalize().unwrap();
    let manager = LspManager::new(
        &LspConfig {
            enabled: true,
            servers: HashMap::from([(
                "fixture".into(),
                fixture.config("diagnostics", &fixture.path("tools.lease")),
            )]),
        },
        workspace.clone(),
    );
    let write = WriteTool::new(None, None, None)
        .with_workspace(workspace.clone())
        .with_lsp(Some(manager.clone()));
    let edit = EditTool::new(None, None)
        .with_workspace(workspace.clone())
        .with_lsp(Some(manager.clone()));
    let query = LspTool::new(manager.clone(), None, None);
    crate::agent::tools::set_edit_system(crate::config::types::EditSystem::Similarity);
    let mut outputs = Vec::new();
    let mut stale_results = Vec::new();
    let mut rejected_edits = Vec::new();
    for relative in [false, true] {
        let name = format!("tool-{relative}.probe");
        let path = if relative {
            name
        } else {
            workspace.join(name).to_string_lossy().into_owned()
        };
        outputs.push((
            relative,
            "write",
            1,
            write
                .call(WriteArgs {
                    path: path.clone(),
                    content: "before".into(),
                    overwrite: false,
                })
                .await,
        ));
        let source = workspace.join(&path);
        let uri = file_uri(&source).unwrap();
        let edited = edit
            .call(EditArgs {
                path: path.clone(),
                block: Some("<<<<<<< SEARCH\nbefore\n=======\nafter\n>>>>>>> REPLACE".into()),
                replace_all: false,
                file_crc: None,
                edits: None,
            })
            .await;
        let checked_edit_unsupported = cfg!(windows) && relative;
        if checked_edit_unsupported {
            // Windows bound replacements deliberately refuse publication when
            // no atomic expected-identity exchange is available. The refusal
            // must leave both the file and its synchronized version unchanged.
            rejected_edits.push((
                edited,
                fs::read_to_string(&source).unwrap(),
                manager
                    .diagnostics_block_since(&source, Duration::ZERO, None)
                    .await,
            ));
        } else {
            outputs.push((relative, "edit", 2, edited));
        }
        outputs.push((
            relative,
            "query",
            if checked_edit_unsupported { 2 } else { 3 },
            query
                .call(LspArgs {
                    path: Some(path.clone()),
                })
                .await,
        ));
        let binding = manager.bind_diagnostic_uri(&uri).await.unwrap();
        rewrite_preserving_length_and_mtime(&source);
        stale_results.push((
            manager
                .diagnostics_block_since(&source, Duration::ZERO, None)
                .await,
            manager.snapshot_bound_diagnostics(&binding, 20).await,
            query.call(LspArgs { path: None }).await,
        ));
    }
    manager.shutdown().await;
    drop(write);
    drop(edit);
    drop(query);
    drop(manager);
    fixture.cleanup().await;
    for (single, bound, aggregate) in stale_results {
        assert!(
            single.is_none(),
            "stale cached single-file diagnostics survived rewrite"
        );
        assert!(
            bound.is_none(),
            "stale diagnostics survived rewrite during permission wait"
        );
        assert_eq!(aggregate.unwrap(), "No diagnostics.");
    }
    for (edited, content, diagnostic) in rejected_edits {
        let error = edited.expect_err("Windows checked edit must refuse publication");
        assert!(
            error
                .to_string()
                .contains("atomic compare-and-replace is unsupported"),
            "{error}"
        );
        assert_eq!(content, "before");
        assert!(diagnostic.unwrap().contains("fixture diagnostic version 1"));
    }
    for (relative, operation, version, result) in outputs {
        let output =
            result.unwrap_or_else(|error| panic!("{operation}, relative={relative}: {error}"));
        assert!(
            output.contains(&format!("fixture diagnostic version {version}")),
            "{output}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn lsp_process_concurrent_sync_preserves_document_version_order() {
    let fixture = FixtureBuild::compile("sync-order");
    let mut results = Vec::new();
    for rewrite_queued in [false, true] {
        let workspace = fixture.workspace(&format!("workspace-{rewrite_queued}"));
        let source = workspace.join("document.probe");
        fs::write(&source, "document").unwrap();
        let sync_log = workspace.join("sync.log");
        let mut cfg = fixture.config("normal", &fixture.path("lease"));
        cfg.env.insert(
            "LSP_FIXTURE_SYNC_LOG".into(),
            sync_log.display().to_string(),
        );
        let client = spawn_client(&cfg, &workspace, Duration::from_secs(2))
            .await
            .unwrap();
        let (_, first_advanced, release_first) = client.pause_next_sync_for_test();
        let first_client = client.clone();
        let first_path = source.clone();
        let first = tokio::spawn(async move {
            let document = crate::extras::lsp::client::read_stable_document(&first_path)
                .await
                .unwrap();
            first_client.sync_document(&first_path, document).await
        });
        first_advanced.await.unwrap();
        let (second_queued, mut second_advanced, release_second) =
            client.pause_next_sync_for_test();
        let second_client = client.clone();
        let second_path = source.clone();
        let second = tokio::spawn(async move {
            let document = crate::extras::lsp::client::read_stable_document(&second_path)
                .await
                .unwrap();
            second_client.sync_document(&second_path, document).await
        });
        second_queued.await.unwrap();
        // On this current-thread runtime the second task must reach its next
        // suspension before this receiver resumes: the writer lock, or its probe
        // if it incorrectly advanced state while the first frame was paused.
        let advanced_early = second_advanced.try_recv().is_ok();
        if rewrite_queued {
            rewrite_preserving_length_and_mtime(&source);
        }
        let (first_result, second_result) = if advanced_early {
            release_second.send(()).unwrap();
            let second_result = second.await.unwrap();
            release_first.send(()).unwrap();
            (first.await.unwrap(), second_result)
        } else {
            release_first.send(()).unwrap();
            let first_result = first.await.unwrap();
            if !rewrite_queued {
                second_advanced.await.unwrap();
            }
            let _ = release_second.send(());
            (first_result, second.await.unwrap())
        };
        let recovery = if rewrite_queued {
            let current = crate::extras::lsp::client::read_stable_document(&source)
                .await
                .unwrap();
            client.sync_document(&source, current).await
        } else {
            None
        };
        wait_for_file_contains(&sync_log, "textDocument/didOpen").await;
        let log = wait_for_file_contains(&sync_log, "textDocument/didChange").await;
        client.shutdown().await;
        results.push((
            rewrite_queued,
            advanced_early,
            first_result,
            second_result,
            recovery,
            log,
        ));
    }
    fixture.cleanup().await;
    for (rewrite_queued, advanced_early, first_result, second_result, recovery, log) in results {
        assert!(first_result.is_some());
        assert_eq!(
            second_result.is_none(),
            rewrite_queued,
            "queued content validation: {log}"
        );
        if rewrite_queued {
            assert!(recovery.is_some(), "skipped sync poisoned the client");
        }
        let messages: Vec<serde_json::Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(messages.len(), 2, "{log}");
        assert!(
            !advanced_early,
            "second sync advanced before the first frame was sent: {log}"
        );
        for (message, method, version) in [
            (&messages[0], "textDocument/didOpen", 1),
            (&messages[1], "textDocument/didChange", 2),
        ] {
            assert_eq!(message["method"], method, "{log}");
            assert_eq!(
                message["params"]["textDocument"]["version"], version,
                "{log}"
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn lsp_process_cancelled_document_sync_rejects_queued_calls_before_reaping() {
    let fixture = FixtureBuild::compile("cancelled-sync");
    let workspace = fixture.workspace("workspace").canonicalize().unwrap();
    let source = workspace.join("document.probe");
    fs::write(&source, "document").unwrap();
    let lease = fixture.path("lease");
    let request_seen = fixture.path("request.seen");
    let mut cfg = fixture.config("normal", &lease);
    cfg.env.insert(
        "LSP_FIXTURE_REQUEST_FILE".into(),
        request_seen.display().to_string(),
    );
    let manager = LspManager::new(
        &LspConfig {
            enabled: true,
            servers: HashMap::from([("fixture".into(), cfg)]),
        },
        workspace.clone(),
    );
    let client = manager.client_for_test(&source).await.unwrap();
    let pid = wait_for_pid(&lease).await;
    let (stopping, release_shutdown) = client.pause_shutdown_for_test();
    let (_, advanced, _release) = client.pause_next_sync_for_test();
    let caller = client.clone();
    let path = source.clone();
    let sync = tokio::spawn(async move { caller.sync_file(&path).await });
    advanced.await.unwrap();
    let (queued, queued_advanced, release_queued) = client.pause_next_sync_for_test();
    let caller = client.clone();
    let queued_path = source.clone();
    let queued_call = tokio::spawn(async move {
        let document = crate::extras::lsp::client::read_stable_document(&queued_path)
            .await
            .unwrap();
        caller.sync_document(&queued_path, document).await
    });
    queued.await.unwrap();
    sync.abort();
    assert!(sync.await.unwrap_err().is_cancelled());
    stopping.await.unwrap();
    assert!(
        process_is_alive(pid) && !client.is_stopped(),
        "cleanup pause must precede reaping"
    );
    let advanced_during_shutdown = queued_advanced.await.is_ok();
    let _ = release_queued.send(());
    let reused = queued_call.await.unwrap();
    let _ = client.request_for_test(Duration::from_millis(100)).await;
    let request_sent = request_seen.exists();
    let pending = client.pending_len_for_test();
    let mut retry = Box::pin(manager.client_for_test(&source));
    let retry_poll = std::future::poll_fn(|cx| {
        std::task::Poll::Ready(std::future::Future::poll(retry.as_mut(), cx))
    })
    .await;
    let waited_for_cleanup = retry_poll.is_pending();
    release_shutdown.send(()).unwrap();
    client.shutdown().await;
    let retried = match retry_poll {
        std::task::Poll::Pending => retry.as_mut().await,
        std::task::Poll::Ready(client) => client,
    };
    drop(retry);
    assert_process_reaped(pid).await;
    manager.shutdown().await;
    let cooled_down = retried.is_none();
    drop(retried);
    drop(client);
    drop(manager);
    fixture.cleanup().await;
    assert!(
        waited_for_cleanup,
        "manager returned a closing client before cleanup"
    );
    assert!(
        cooled_down,
        "restart must observe the existing failure cooldown"
    );
    assert!(
        !advanced_during_shutdown,
        "queued sync advanced state before failed transport was reaped"
    );
    assert!(
        reused.is_none(),
        "partially synchronized client remained reusable"
    );
    assert!(
        !request_sent,
        "a request was written after transport failure"
    );
    assert_eq!(pending, 0);
}

#[tokio::test]
async fn lsp_process_shutdown_drains_publication_before_manager_replacement() {
    let fixture = FixtureBuild::compile("publication-shutdown");
    let workspace = fixture.workspace("workspace").canonicalize().unwrap();
    let source = workspace.join("document.probe");
    fs::write(&source, "document").unwrap();
    let lease = fixture.path("lease");
    let manager = LspManager::new(
        &LspConfig {
            enabled: true,
            servers: HashMap::from([("fixture".into(), fixture.config("diagnostics", &lease))]),
        },
        workspace,
    );
    let scope = crate::agent::runner::AgentWorkScope::new();
    let client = scope.run(manager.client_for_test(&source)).await.unwrap();
    let pid = wait_for_pid(&lease).await;
    let (entered, release) = client.pause_diagnostic_for_test(1);
    assert!(scope.run(manager.notify_changed(&source)).await.is_some());
    tokio::time::timeout(Duration::from_secs(5), entered)
        .await
        .unwrap()
        .unwrap();
    // A persistent server's publication must not keep an otherwise finished
    // agent turn alive. Its client supervisor owns the remaining disk work.
    tokio::time::timeout(Duration::from_secs(2), scope.wait_idle())
        .await
        .unwrap();

    let mut shutdown = Box::pin(client.shutdown());
    assert!(
        tokio::time::timeout(Duration::from_secs(3), &mut shutdown)
            .await
            .is_err(),
        "reader drain timeout must not detach a running diagnostic publication"
    );
    assert_process_reaped(pid).await;
    assert!(!client.is_stopped(), "native publication is still running");
    let mut replacement = Box::pin(manager.client_for_test(&source));
    std::future::poll_fn(|cx| {
        assert!(
            std::future::Future::poll(replacement.as_mut(), cx).is_pending(),
            "manager replacement must wait for the old publication"
        );
        std::task::Poll::Ready(())
    })
    .await;
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), &mut shutdown)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), &mut replacement)
            .await
            .unwrap()
            .is_none(),
        "restart must honor the existing cooldown"
    );
    assert!(client.is_stopped());
    assert!(
        manager.diagnostic_candidate_uris().is_empty(),
        "old publication must finish before replacement clears the cache"
    );
    drop(replacement);
    drop(shutdown);
    manager.shutdown().await;
    drop(client);
    drop(manager);
    fixture.cleanup().await;
}

#[tokio::test]
async fn lsp_process_transport_deadlines_are_independent_between_clients() {
    let fixture = FixtureBuild::compile("independent-deadlines");
    let workspace = fixture.workspace("workspace");
    let source = workspace.join("document.probe");
    fs::write(&source, "document").unwrap();
    let fast_client = spawn_client(
        &fixture.config("normal", &fixture.path("fast.lease")),
        &workspace,
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    let slow_client = spawn_client(
        &fixture.config("normal", &fixture.path("slow.lease")),
        &workspace,
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    fast_client.set_write_timeout_for_test(Some(Duration::from_secs(1)));
    slow_client.set_write_timeout_for_test(Some(Duration::from_secs(30)));
    let (_, fast_advanced, _release_fast) = fast_client.pause_next_sync_for_test();
    let (_, slow_advanced, release_slow) = slow_client.pause_next_sync_for_test();
    let fast_document = crate::extras::lsp::client::read_stable_document(&source)
        .await
        .unwrap();
    let slow_document = crate::extras::lsp::client::read_stable_document(&source)
        .await
        .unwrap();
    let caller = fast_client.clone();
    let path = source.clone();
    let mut fast = tokio::spawn(async move { caller.sync_document(&path, fast_document).await });
    let caller = slow_client.clone();
    let slow = tokio::spawn(async move { caller.sync_document(&source, slow_document).await });
    fast_advanced.await.unwrap();
    slow_advanced.await.unwrap();
    let fast_expired = matches!(
        tokio::time::timeout(Duration::from_secs(5), &mut fast).await,
        Ok(Ok(None))
    );
    if !fast.is_finished() {
        fast.abort();
        let _ = fast.await;
    }
    fast_client.set_write_timeout_for_test(None);
    fast_client.shutdown().await;
    let slow_was_waiting = !slow.is_finished();
    let _ = release_slow.send(());
    let slow_result = slow.await.unwrap();
    slow_client.shutdown().await;
    drop(fast_client);
    drop(slow_client);
    fixture.cleanup().await;
    assert!(
        fast_expired,
        "the other client's override replaced the short deadline"
    );
    assert!(
        slow_was_waiting && slow_result.is_some(),
        "one client's expiry affected the other client"
    );
}

#[tokio::test]
async fn lsp_process_rejected_documents_do_not_poison_sync_state() {
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

    fs::write(&source, "original document").unwrap();
    let original = crate::extras::lsp::client::read_stable_document(&source)
        .await
        .unwrap();
    fs::rename(&source, workspace.join("original.probe")).unwrap();
    fs::write(&source, "small document").unwrap();
    assert!(
        client.sync_document(&source, original).await.is_none(),
        "a file replaced after reading must not advance synchronization"
    );
    let original = crate::extras::lsp::client::read_stable_document(&source)
        .await
        .unwrap();
    rewrite_preserving_length_and_mtime(&source);
    assert!(
        client.sync_document(&source, original).await.is_none(),
        "a file rewritten after reading must not advance synchronization"
    );
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
    drop(client);
    fixture.cleanup().await;
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
    drop(client);
    fixture.cleanup().await;
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
    fixture.cleanup().await;
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

    fixture.cleanup().await;
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

    let _ = manager.notify_changed(&source).await;
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
            let _ = manager.notify_changed(&source).await;
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
    fixture.cleanup().await;
}

/// A child whose stdin pipe fills is a deterministic Unix fixture: the parent's
/// write blocks once the pipe buffer is full and the child stops reading.
/// Windows anonymous pipes buffer a document of this size without blocking, so
/// the same fixture cannot stall the writer there. The deadline itself is
/// platform-independent — it wraps the shared writer lock plus the frame write
/// and flush for every request and notification on all platforms.
#[cfg(unix)]
#[tokio::test]
async fn lsp_process_stalled_and_queued_writers_are_bounded_and_reaped() {
    let fixture = FixtureBuild::compile("stalled-and-queued-writers");
    let workspace = fixture.workspace("workspace");
    let first = workspace.join("first.probe");
    let second = workspace.join("second.probe");
    // The first frame fills stdin; the second caller waits for its writer lock.
    fs::write(&first, vec![b'x'; 1024 * 1024]).unwrap();
    fs::write(&second, vec![b'y'; 1024 * 1024]).unwrap();
    let lease = fixture.path("stalled.lease");
    let cfg = fixture.config("stop-reading", &lease);
    let client = spawn_client(&cfg, &workspace, Duration::from_secs(5))
        .await
        .expect("fixture must initialize before it stops reading");
    let parent_pid = wait_for_pid(&lease).await;
    let first_document = crate::extras::lsp::client::read_stable_document(&first)
        .await
        .unwrap();
    let second_document = crate::extras::lsp::client::read_stable_document(&second)
        .await
        .unwrap();

    client.set_write_timeout_for_test(Some(Duration::from_millis(300)));
    let started = Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            client.sync_document(&first, first_document),
            client.sync_document(&second, second_document)
        )
    })
    .await
    .expect("the blocked and queued callers must both finish within the deadline");
    assert_eq!(
        outcome,
        (None, None),
        "neither document reached a complete frame"
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "the write deadline must bound every caller, took {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(200),
        "the first write must actually block on the full pipe, took {elapsed:?}"
    );

    // A partially written frame is terminal. Synchronization returns only
    // after shutdown, so callers cannot reuse a desynchronized server.
    assert!(client.is_stopped());
    assert_process_reaped(parent_pid).await;
    drop(client);
    fixture.cleanup().await;
}
