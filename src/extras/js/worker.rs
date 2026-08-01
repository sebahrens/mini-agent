//! Synchronous, pre-runtime bootstrap for the brokered JavaScript worker.
//!
//! This module intentionally initializes no QuickJS, configuration, paths, logging, providers,
//! hooks, MCP, or Tokio services. A07 validates the reserved marker, pipe-shaped standard handles,
//! and the fixed version/build/sequence Hello/Ready handshake. These forgeable structural checks
//! are not cryptographic parent authentication; later containment removes ambient authority.
//! Request execution is delivered separately by A08.

use std::io::Write;
use std::process::ExitCode;

use super::protocol::{
    BuildIdentity, ParentFrame, ParentWireFrame, WireFrame, WorkerFrame, WorkerProtocol,
    WorkerReady, WorkerWireFrame, read_frame, write_frame,
};
use crate::sandbox::worker::{
    INTERNAL_WORKER_MARKER, INTERNAL_WORKER_MARKER_VALUE, is_internal_worker_marker_present,
    standard_streams_are_protocol_pipes,
};

const EXIT_FAILURE: i32 = 1;

/// Enter internal-worker mode when and only when the reserved launcher marker is present.
///
/// Once the marker exists this always returns `Some`, including for malformed marker values,
/// invalid pipes, and failed handshakes. An attempted worker launch can therefore never fall
/// through into normal CLI startup.
pub(crate) fn maybe_run_internal_worker() -> Option<ExitCode> {
    if !is_internal_worker_marker_present() {
        return None;
    }
    Some(if run_marked_worker() == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn run_marked_worker() -> i32 {
    if std::env::var_os(INTERNAL_WORKER_MARKER).as_deref()
        != Some(std::ffi::OsStr::new(INTERNAL_WORKER_MARKER_VALUE))
    {
        return EXIT_FAILURE;
    }

    if !standard_streams_are_protocol_pipes() {
        return EXIT_FAILURE;
    }

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    if bootstrap(&mut input, &mut output).is_ok() {
        0
    } else {
        EXIT_FAILURE
    }
}

fn bootstrap(input: &mut impl std::io::Read, output: &mut impl Write) -> Result<(), ()> {
    let build = BuildIdentity::current();
    let mut protocol = WorkerProtocol::new(build.clone());

    let hello: ParentWireFrame = read_frame(input).map_err(|_| ())?;
    if !matches!(hello.message, ParentFrame::Hello(_)) {
        return Err(());
    }
    protocol.on_receive(&hello).map_err(|_| ())?;

    let ready: WorkerWireFrame =
        WireFrame::connection(build, 1, WorkerFrame::Ready(WorkerReady {}));
    protocol.on_send(&ready).map_err(|_| ())?;
    write_frame(output, &ready).map_err(|_| ())?;
    output.flush().map_err(|_| ())?;

    // A07 has no execution engine. Accept only a protocol-valid clean shutdown after Ready;
    // every request frame fails closed until A08 supplies fresh-runtime execution.
    let shutdown: ParentWireFrame = read_frame(input).map_err(|_| ())?;
    if !matches!(shutdown.message, ParentFrame::Shutdown) {
        return Err(());
    }
    protocol.on_receive(&shutdown).map_err(|_| ())?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn exit_test_worker() -> ! {
    std::process::exit(run_marked_worker())
}
