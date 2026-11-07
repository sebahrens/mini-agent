//! Physical standard-I/O retirement of the actual MCP server binary. Duplex
//! transports cannot reproduce Tokio's uncancellable blocking stdio adapters.
#![cfg(feature = "mcp")]

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[path = "../src/tests/support/temp.rs"]
mod temp;

struct OwnedProbe {
    child: Child,
    reader: Option<std::thread::JoinHandle<()>>,
    stderr_path: std::path::PathBuf,
}
impl OwnedProbe {
    fn diagnostics(&mut self) -> String {
        format!(
            "status={:?}, stderr={:?}",
            self.child.try_wait(),
            std::fs::read_to_string(&self.stderr_path)
                .unwrap_or_else(|error| format!("unavailable: {error}"))
        )
    }
}
impl Drop for OwnedProbe {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn probe(close_input: bool) {
    let root = temp::TempDir::new_canonical("mcp-physical-stdio");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let binary = std::env::var_os("MINI_AGENT_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_mini-agent").into());
    let mut command = Command::new(binary);
    command.current_dir(root.path()).env("HOME", root.path());
    for name in [
        "MINI_AGENT_CONFIG_DIR",
        "MINI_AGENT_DATA_DIR",
        "MINI_AGENT_LOCAL_DATA_DIR",
        "MINI_AGENT_STATE_DIR",
        "MINI_AGENT_CACHE_DIR",
        "MINI_AGENT_CREDENTIALS_DIR",
    ] {
        // Let startup create each private root with its native ACL. The
        // scratch workspace may have an administrative owner on elevated Windows.
        command.env(name, root.join(name.to_ascii_lowercase()));
    }
    let stderr_path = root.join("server.stderr");
    let stderr = std::fs::File::create(&stderr_path).unwrap();
    let child = command
        .args(["--mcp-server", "--no-tools", "--no-context-files"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr))
        .spawn()
        .unwrap();
    let mut owner = OwnedProbe {
        child,
        reader: None,
        stderr_path,
    };
    let mut input = Some(owner.child.stdin.take().unwrap());
    let mut output = owner.child.stdout.take().unwrap();
    let (first, received) = std::sync::mpsc::sync_channel(1);
    owner.reader = Some(std::thread::spawn(move || {
        let mut byte = [0u8; 1];
        let read = output.read_exact(&mut byte);
        let _ = first.send((read, output));
    }));
    // Larger than the native pipe capacity. The first byte proves the writer
    // reached physical stdout before input is closed or left parked open.
    let request =
        serde_json::json!({"jsonrpc":"2.0","id":"x".repeat(2 * 1024 * 1024),"method":"ping"});
    writeln!(input.as_mut().unwrap(), "{request}").unwrap_or_else(|error| {
        panic!(
            "write backpressure request: {error}; {}",
            owner.diagnostics()
        )
    });
    input.as_mut().unwrap().flush().unwrap_or_else(|error| {
        panic!(
            "flush backpressure request: {error}; {}",
            owner.diagnostics()
        )
    });
    let (read, _unread_output) = received
        .recv_timeout(Duration::from_secs(15))
        .unwrap_or_else(|error| {
            panic!(
                "server response did not begin: {error}; {}",
                owner.diagnostics()
            )
        });
    read.unwrap_or_else(|error| panic!("first response byte: {error}; {}", owner.diagnostics()));
    owner.reader.take().unwrap().join().unwrap();
    if close_input {
        drop(input.take());
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = owner.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "MCP process remained alive after bounded transport retirement (close_input={close_input}); {}",
            owner.diagnostics()
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    if close_input {
        assert!(
            status.success(),
            "EOF is ordinary client retirement: {status}; {}",
            owner.diagnostics()
        );
    } else {
        assert!(
            !status.success(),
            "a backpressured writer deadline is a transport failure; {}",
            owner.diagnostics()
        );
    }
}

#[test]
fn mcp_stdio_actual_binary_exits_on_eof_with_physically_backpressured_stdout() {
    probe(true);
}

#[test]
fn mcp_stdio_actual_binary_exits_on_writer_deadline_while_stdin_stays_open() {
    probe(false);
}
