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
        command.env(name, root.path());
    }
    let child = command
        .args(["--mcp-server", "--no-tools", "--no-context-files"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut owner = OwnedProbe {
        child,
        reader: None,
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
    writeln!(input.as_mut().unwrap(), "{request}").unwrap();
    input.as_mut().unwrap().flush().unwrap();
    let (read, _unread_output) = received
        .recv_timeout(Duration::from_secs(15))
        .expect("server began writing its response");
    read.expect("first response byte");
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
            "MCP process remained alive after bounded transport retirement (close_input={close_input})"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    if close_input {
        assert!(status.success(), "EOF is ordinary client retirement");
    } else {
        assert!(
            !status.success(),
            "a backpressured writer deadline is a transport failure"
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
