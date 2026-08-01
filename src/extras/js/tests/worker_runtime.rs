use std::io::{Read, Write};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use crate::extras::js::protocol::{
    BuildIdentity, ParentFrame, ParentHello, ParentProtocol, ParentWireFrame, WireFrame,
    WorkerFrame, WorkerWireFrame, read_frame, write_frame,
};
use crate::sandbox::worker::{TestWorkerLauncher, WorkerLauncher};

const TEST_CREDENTIAL_CANARY: &str = "A07_CREDENTIAL_CANARY_MUST_NOT_LEAK";
const TEST_CONFIG_CANARY: &str = "A07_CONFIG_CANARY_MUST_NOT_LEAK";
const TEST_WORKSPACE_CANARY: &str = "A07_WORKSPACE_CANARY_MUST_NOT_LEAK";

fn hello(sequence: u64) -> ParentWireFrame {
    WireFrame::connection(
        BuildIdentity::current(),
        sequence,
        ParentFrame::Hello(ParentHello {}),
    )
}

fn shutdown(sequence: u64) -> ParentWireFrame {
    WireFrame::connection(BuildIdentity::current(), sequence, ParentFrame::Shutdown)
}

fn write_parent_frame(output: &mut impl Write, frame: &ParentWireFrame) {
    write_frame(output, frame).expect("parent frame should serialize");
    output.flush().expect("parent protocol pipe should flush");
}

/// The current executable is a libtest binary. Discard only libtest's bounded textual preamble,
/// then return the first valid worker frame. The worker child exits directly, so no harness text
/// can follow the protocol once bootstrap begins.
fn read_worker_frame_after_test_preamble(input: &mut impl Read) -> (Vec<u8>, WorkerWireFrame) {
    let mut preamble = Vec::new();
    let mut window = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        input
            .read_exact(&mut byte)
            .expect("worker exited before emitting Ready");
        window.push(byte[0]);
        if window.len() < 5 {
            continue;
        }

        let length = u32::from_be_bytes(window[..4].try_into().unwrap()) as usize;
        if length > 0 && length <= crate::extras::js::protocol::MAX_FRAME_BYTES && window[4] == b'{'
        {
            let mut encoded = window[..4].to_vec();
            encoded.push(window[4]);
            let mut tail = vec![0_u8; length - 1];
            input
                .read_exact(&mut tail)
                .expect("worker Ready frame was truncated");
            encoded.extend_from_slice(&tail);
            if let Ok(frame) = read_frame(&mut encoded.as_slice()) {
                return (preamble, frame);
            }
        }

        preamble.push(window.remove(0));
        assert!(
            preamble.len() <= 4096,
            "worker emitted an unbounded non-protocol preamble"
        );
    }
}

fn assert_redacted(bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    for canary in [
        TEST_CREDENTIAL_CANARY,
        TEST_CONFIG_CANARY,
        TEST_WORKSPACE_CANARY,
    ] {
        assert!(!text.contains(canary), "worker leaked bootstrap canary");
    }
}

fn wait_for_exit(process: &mut crate::sandbox::worker::WorkerProcess) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = process
            .try_wait()
            .expect("worker child status should be readable")
        {
            return status;
        }
        if Instant::now() >= deadline {
            let termination = process.terminate_tree();
            let reap = process.wait();
            panic!(
                "worker child exceeded the five-second test deadline (termination: {termination:?}, reap: {reap:?})"
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn worker_bootstrap_protocol_valid_hello_ready_shutdown_round_trip() {
    let mut process = TestWorkerLauncher::internal_worker_process()
        .launch()
        .expect("test worker should launch");
    let mut parent = ParentProtocol::new(BuildIdentity::current());

    let hello = hello(0);
    parent.on_send(&hello).expect("Hello should be valid");
    write_parent_frame(&mut process.input, &hello);

    let shutdown = shutdown(2);
    write_parent_frame(&mut process.input, &shutdown);

    let status = wait_for_exit(&mut process);
    assert!(
        status.success(),
        "protocol-valid worker should exit cleanly"
    );

    let mut stdout = Vec::new();
    process.output.read_to_end(&mut stdout).unwrap();
    let (preamble, ready) = read_worker_frame_after_test_preamble(&mut stdout.as_slice());
    assert_redacted(&preamble);
    assert!(matches!(ready.message, WorkerFrame::Ready(_)));
    parent
        .on_receive(&ready)
        .expect("Ready should authenticate");
    parent
        .on_send(&shutdown)
        .expect("Shutdown should follow Ready");

    let mut encoded_ready = Vec::new();
    write_frame(&mut encoded_ready, &ready).unwrap();
    let consumed = preamble.len() + encoded_ready.len();
    assert!(
        stdout[consumed..].is_empty(),
        "worker stdout after Ready contained non-protocol bytes"
    );
    let mut stderr = Vec::new();
    process.stderr.read_to_end(&mut stderr).unwrap();
    assert!(
        stderr.len() <= 4096,
        "worker stderr exceeded its bootstrap bound"
    );
    assert_redacted(&stderr);
}

#[test]
fn worker_bootstrap_forged_marker_with_malformed_hello_fails_without_cli_fallthrough() {
    let mut process = TestWorkerLauncher::internal_worker_process()
        .launch()
        .expect("test worker should launch");
    process.input.write_all(b"forged-worker-input").unwrap();
    process.input.flush().unwrap();

    let status = wait_for_exit(&mut process);
    assert!(!status.success(), "malformed Hello must fail closed");

    let mut stdout = Vec::new();
    process.output.read_to_end(&mut stdout).unwrap();
    let stdout = String::from_utf8_lossy(&stdout);
    assert!(!stdout.contains("Usage:"), "worker fell through to Clap");
    assert!(!stdout.contains("mini-agent --setup"));
    assert_redacted(stdout.as_bytes());

    let mut stderr = Vec::new();
    process.stderr.read_to_end(&mut stderr).unwrap();
    assert!(stderr.len() <= 4096);
    assert!(!String::from_utf8_lossy(&stderr).contains("Usage:"));
    assert_redacted(&stderr);
}

#[test]
fn worker_bootstrap_rejects_wrong_build_without_ready() {
    let mut process = TestWorkerLauncher::internal_worker_process()
        .launch()
        .expect("test worker should launch");
    let wrong = WireFrame::connection(
        BuildIdentity::new("forged-build").unwrap(),
        0,
        ParentFrame::Hello(ParentHello {}),
    );
    write_parent_frame(&mut process.input, &wrong);

    let status = wait_for_exit(&mut process);
    assert!(!status.success(), "wrong build must fail closed");
    let mut stdout = Vec::new();
    process.output.read_to_end(&mut stdout).unwrap();
    assert!(
        !stdout
            .windows(b"\"kind\":\"ready\"".len())
            .any(|part| part == b"\"kind\":\"ready\""),
        "wrong-build worker emitted Ready"
    );
}

#[test]
fn worker_bootstrap_marker_absence_selects_normal_mode() {
    assert!(
        crate::extras::js::worker::maybe_run_internal_worker().is_none(),
        "ordinary test process must not enter worker mode"
    );
}

#[test]
fn worker_bootstrap_initializes_no_runtime_or_authority_surface() {
    let worker_source = include_str!("../worker.rs");
    for forbidden in [
        "rquickjs",
        "Runtime::new",
        "Context::full",
        "crate::config",
        "crate::paths",
        "crate::provider",
        "crate::logging",
        "crate::extras::js::host",
    ] {
        assert!(
            !worker_source.contains(forbidden),
            "bootstrap worker must not initialize {forbidden}"
        );
    }
}

#[test]
fn worker_bootstrap_test_child() {
    if crate::sandbox::worker::is_internal_worker_marker_present() {
        for key in [
            "PATH",
            "OPENROUTER_API_KEY",
            "MINI_AGENT_CONFIG",
            "MINI_AGENT_WORKSPACE",
        ] {
            assert!(
                std::env::var_os(key).is_none(),
                "test worker inherited forbidden environment key {key}"
            );
        }
        crate::extras::js::worker::exit_test_worker();
    }
}
