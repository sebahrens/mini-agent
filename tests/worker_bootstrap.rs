#![cfg(feature = "js")]

use std::io::Write;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::fs::File;

const MARKER: &str = "MINI_AGENT_INTERNAL_JS_WORKER";
const MARKER_VALUE: &str = "brokered-v1";
static WORKER_TEST_LOCK: Mutex<()> = Mutex::new(());
const BUILD_ID: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "+",
    env!("MINI_AGENT_BUILD_FINGERPRINT")
);
const LAUNCH_CHALLENGE: &str = "00000000-0000-0000-0000-000000000001";

fn frame(payload: serde_json::Value) -> Vec<u8> {
    let payload = serde_json::to_vec(&payload).unwrap();
    let mut encoded = (payload.len() as u32).to_be_bytes().to_vec();
    encoded.extend_from_slice(&payload);
    encoded
}

fn hello() -> Vec<u8> {
    frame(serde_json::json!({
        "protocol_version": 3,
        "build_id": BUILD_ID,
        "invocation_id": null,
        "sequence": 0,
        "message": { "kind": "hello", "data": { "challenge": LAUNCH_CHALLENGE } }
    }))
}

fn shutdown() -> Vec<u8> {
    frame(serde_json::json!({
        "protocol_version": 3,
        "build_id": BUILD_ID,
        "invocation_id": null,
        "sequence": 2,
        "message": { "kind": "shutdown" }
    }))
}

fn worker_command(marker: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mini-agent"));
    command
        .env_clear()
        .env(MARKER, marker)
        // Worker dispatch must happen before the application Clap parser sees this invalid flag.
        .arg("--a07-invalid-in-normal-cli-mode")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn serial_worker_test() -> MutexGuard<'static, ()> {
    WORKER_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait_bounded(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("production worker exceeded the fifteen-second test deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn decode_single_frame(bytes: &[u8]) -> serde_json::Value {
    assert!(bytes.len() >= 4, "worker emitted no complete frame");
    let length = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    assert_eq!(bytes.len(), length + 4, "stdout contained non-frame bytes");
    serde_json::from_slice(&bytes[4..]).expect("worker frame should contain JSON")
}

#[test]
fn worker_bootstrap_production_main_emits_ready_from_byte_zero_before_clap_or_tokio() {
    let _serial = serial_worker_test();
    let mut child = worker_command(MARKER_VALUE).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(&hello()).unwrap();
    input.write_all(&shutdown()).unwrap();
    drop(input);

    let status = wait_bounded(&mut child);
    assert!(status.success());
    let output = child.wait_with_output().unwrap();
    let ready = decode_single_frame(&output.stdout);
    assert_eq!(ready["sequence"], 1);
    assert_eq!(ready["message"]["kind"], "ready");
    assert_eq!(ready["message"]["data"]["challenge"], LAUNCH_CHALLENGE);
    assert!(output.stderr.is_empty(), "worker stderr must be silent");
}

#[test]
fn worker_bootstrap_production_main_rejects_malformed_hello_without_fallthrough() {
    let _serial = serial_worker_test();
    let mut child = worker_command(MARKER_VALUE).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(b"forged-worker-input").unwrap();
    drop(input);

    let status = wait_bounded(&mut child);
    assert!(!status.success());
    let output = child.wait_with_output().unwrap();
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn worker_bootstrap_production_main_rejects_invalid_marker_value() {
    let _serial = serial_worker_test();
    let mut child = worker_command("forged-value").spawn().unwrap();
    drop(child.stdin.take());

    let status = wait_bounded(&mut child);
    assert!(!status.success());
    let output = child.wait_with_output().unwrap();
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn worker_bootstrap_production_main_rejects_regular_file_stdio() {
    let _serial = serial_worker_test();
    let base = std::env::temp_dir().join(format!("mini-agent-a07-{}", std::process::id()));
    let input_path = base.with_extension("stdin");
    let output_path = base.with_extension("stdout");
    let error_path = base.with_extension("stderr");
    let mut input = File::create(&input_path).unwrap();
    input.write_all(&hello()).unwrap();
    input.write_all(&shutdown()).unwrap();
    drop(input);

    let mut command = Command::new(env!("CARGO_BIN_EXE_mini-agent"));
    command
        .env_clear()
        .env(MARKER, MARKER_VALUE)
        .stdin(File::open(&input_path).unwrap())
        .stdout(File::create(&output_path).unwrap())
        .stderr(File::create(&error_path).unwrap());
    let mut child = command.spawn().unwrap();
    let status = wait_bounded(&mut child);
    assert!(!status.success(), "regular files are not protocol pipes");

    assert_eq!(std::fs::metadata(&output_path).unwrap().len(), 0);
    assert_eq!(std::fs::metadata(&error_path).unwrap().len(), 0);
    std::fs::remove_file(input_path).unwrap();
    std::fs::remove_file(output_path).unwrap();
    std::fs::remove_file(error_path).unwrap();
}

#[cfg(unix)]
#[test]
fn worker_bootstrap_production_main_rejects_null_stdout() {
    let _serial = serial_worker_test();
    let mut command = worker_command(MARKER_VALUE);
    command.stdout(File::options().write(true).open("/dev/null").unwrap());
    let mut child = command.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(&hello()).unwrap();
    input.write_all(&shutdown()).unwrap();
    drop(input);

    let status = wait_bounded(&mut child);
    assert!(!status.success(), "null stdout is not a protocol pipe");
}

#[cfg(unix)]
#[test]
fn worker_bootstrap_production_main_rejects_regular_file_stderr() {
    let _serial = serial_worker_test();
    let error_path =
        std::env::temp_dir().join(format!("mini-agent-a07-stderr-{}", std::process::id()));
    let mut command = worker_command(MARKER_VALUE);
    command.stderr(File::create(&error_path).unwrap());
    let mut child = command.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(&hello()).unwrap();
    input.write_all(&shutdown()).unwrap();
    drop(input);

    let status = wait_bounded(&mut child);
    assert!(
        !status.success(),
        "regular-file stderr is not a protocol pipe"
    );
    assert_eq!(std::fs::metadata(&error_path).unwrap().len(), 0);
    std::fs::remove_file(error_path).unwrap();
}

#[cfg(unix)]
#[test]
fn worker_bootstrap_production_main_rejects_socket_stdio() {
    let _serial = serial_worker_test();
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    let (parent_input, child_input) = UnixStream::pair().unwrap();
    let (child_output, _parent_output) = UnixStream::pair().unwrap();
    let child_input: OwnedFd = child_input.into();
    let child_output: OwnedFd = child_output.into();

    let mut command = worker_command(MARKER_VALUE);
    command
        .stdin(Stdio::from(child_input))
        .stdout(Stdio::from(child_output));
    let mut child = command.spawn().unwrap();
    let mut parent_input = parent_input;
    parent_input.write_all(&hello()).unwrap();
    parent_input.write_all(&shutdown()).unwrap();
    drop(parent_input);

    let status = wait_bounded(&mut child);
    assert!(!status.success(), "sockets are not protocol pipes");
}
