#![cfg(unix)]

use crate::extras::status_signals::StatusSignals;
use std::io::Read;
use std::os::unix::net::UnixListener;

fn temp_socket_path(name: &str) -> (std::path::PathBuf, UnixListener) {
    let dir = std::env::temp_dir().join(format!("zs_status_test_{}_{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let socket_path = dir.join("status.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    (socket_path, listener)
}

fn cleanup(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn send_start_writes_expected_message() {
    let (socket_path, listener) = temp_socket_path("start");
    let ss = StatusSignals::new(socket_path.to_string_lossy().to_string());
    ss.send_start();

    let (mut stream, _) = listener.accept().unwrap();
    let mut buf = String::new();
    stream.read_to_string(&mut buf).unwrap();
    assert_eq!(buf, "start\n");
    cleanup(&socket_path);
}

#[test]
fn send_stop_writes_expected_message() {
    let (socket_path, listener) = temp_socket_path("stop");
    let ss = StatusSignals::new(socket_path.to_string_lossy().to_string());
    ss.send_stop();

    let (mut stream, _) = listener.accept().unwrap();
    let mut buf = String::new();
    stream.read_to_string(&mut buf).unwrap();
    assert_eq!(buf, "stop\n");
    cleanup(&socket_path);
}

#[test]
fn nonexistent_socket_does_not_panic() {
    let socket = std::env::temp_dir().join(format!(
        "mini-agent-missing-status-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let ss = StatusSignals::new(socket.to_string_lossy().to_string());
    ss.send_start();
    ss.send_stop();
    ss.send_git_conflict();
    assert!(
        !socket.exists(),
        "signals must not create a missing endpoint"
    );
}

#[test]
fn send_git_conflict_writes_expected_message() {
    let (socket_path, listener) = temp_socket_path("conflict");
    let ss = StatusSignals::new(socket_path.to_string_lossy().to_string());
    ss.send_git_conflict();

    let (mut stream, _) = listener.accept().unwrap();
    let mut buf = String::new();
    stream.read_to_string(&mut buf).unwrap();
    assert_eq!(buf, "git-conflict\n");
    cleanup(&socket_path);
}
