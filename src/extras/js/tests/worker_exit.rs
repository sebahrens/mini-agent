//! Deterministic ordering of a flushed terminal frame and native process exit.

use super::*;
use crate::sandbox::worker::TestWorkerLauncher;
use std::sync::atomic::AtomicUsize;

async fn requested_connection(code: &str) -> WorkerConnection {
    let cancellation = PermCancellation::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut connection = launch_connection(
        Arc::new(TestWorkerLauncher::scripted_internal_worker(0)),
        Arc::new(tokio::sync::Mutex::new(())),
        1,
        true,
        &cancellation,
        deadline,
    )
    .await
    .unwrap();
    let request = WireFrame::invocation(
        connection.build.clone(),
        InvocationId::new("exit-ordering").unwrap(),
        connection.sequence,
        if code == "__verification_internal__" {
            ParentFrame::VerifyArtifact(
                crate::extras::js::tests::worker_runtime::verification_with_source(code),
            )
        } else {
            ParentFrame::RunStep(RunStep::new(code.into()))
        },
    );
    connection.protocol.on_send(&request).unwrap();
    write_parent(&connection, request, &cancellation, deadline)
        .await
        .unwrap();
    connection.sequence += 1;
    connection
}

#[tokio::test]
async fn buffered_terminal_survives_clean_exit_and_keeps_validation() {
    let cases = [
        ("terminal-exit-clean", None),
        ("__verification_internal__", None),
        ("terminal-exit-fault", None),
        ("terminal-exit-effect", Some(WorkerError::Transport)),
        (
            "terminal-exit-invalid-sequence",
            Some(WorkerError::Protocol),
        ),
        ("terminal-exit-abnormal", Some(WorkerError::Transport)),
        #[cfg(unix)]
        ("terminal-exit-cpu", Some(WorkerError::NativeCpuLimit)),
    ];
    for (code, expected_error) in cases {
        let mut connection = requested_connection(code).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while connection.process.try_wait().unwrap().is_none() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("worker must exit before the parent reads its terminal frame");
        let result = read_worker(
            &mut connection,
            false,
            &PermCancellation::new(),
            Instant::now() + Duration::from_secs(2),
        )
        .await
        .and_then(|frame| {
            connection
                .protocol
                .on_receive(&frame)
                .map_err(map_protocol_error)?;
            Ok(frame.message)
        });
        match expected_error {
            Some(error) => assert_eq!(result.unwrap_err(), error, "{code}"),
            None if code == "terminal-exit-fault" => {
                assert!(matches!(result.unwrap(), WorkerFrame::ProtocolFault(_)));
            }
            None if code == "__verification_internal__" => {
                let WorkerFrame::VerificationResult(result) = result.unwrap() else {
                    panic!("expected a verification terminal");
                };
                assert!(!result.passed);
                assert!(verification_diagnostics_are_closed(&result));
                assert!(!verification_result_is_reusable(&result));
            }
            None => assert!(
                matches!(result.unwrap(), WorkerFrame::StepResult(StepResult {
                outcome: StepOutcome::Value(ref value), ..
            }) if value == "success")
            ),
        }
    }
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // Deliberately stall only the blocking pipe reader.
async fn exit_poll_drains_the_pending_read_with_cancellation_and_a_bound() {
    for action in ["release", "cancel", "expire"] {
        let supervisor = JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
            TestWorkerLauncher::scripted_internal_worker(0),
            Duration::from_secs(5),
        );
        supervisor
            .execute(
                RunStep::new("success".into()),
                RejectEffects,
                PermCancellation::new(),
            )
            .await
            .unwrap();
        let live = Arc::new(AtomicUsize::new(0));
        let output = {
            let mut state = supervisor.0.transport.lock().await;
            let connection = state.idle.as_mut().unwrap();
            connection.process.observe_reap_for_test(live.clone());
            connection.output_handle.clone()
        };
        let guard = output.lock().unwrap();
        let cancellation = PermCancellation::new();
        let read = supervisor.execute(
            RunStep::new("terminal-exit-clean".into()),
            RejectEffects,
            cancellation.clone(),
        );
        tokio::pin!(read);
        tokio::select! {
            result = &mut read => panic!("exit poll discarded the pending frame: {result:?}"),
            _ = async {
                tokio::time::timeout(Duration::from_secs(2), async {
                    while live.load(Ordering::Acquire) != 0 {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }).await.expect("the read loop must observe and reap the exited worker");
            } => {}
        }
        if action == "release" {
            drop(guard);
            assert_eq!(
                read.await.unwrap().outcome,
                StepOutcome::Value("success".into())
            );
        } else {
            if action == "cancel" {
                cancellation.cancel();
            }
            let result = tokio::time::timeout(Duration::from_secs(1), &mut read).await;
            drop(guard);
            assert_eq!(
                result.expect("exit drain must be bounded").unwrap_err(),
                if action == "cancel" {
                    WorkerError::Cancelled
                } else {
                    WorkerError::Transport
                }
            );
        }
        assert_eq!(supervisor.generation_for_test().await, None);
        let recovered = supervisor
            .execute(
                RunStep::new("success".into()),
                RejectEffects,
                PermCancellation::new(),
            )
            .await
            .unwrap();
        assert_eq!(recovered.outcome, StepOutcome::Value("success".into()));
        assert_eq!(supervisor.generation_for_test().await, Some(2));
        supervisor.shutdown_for_test().await.unwrap();
    }
}

#[test]
fn stderr_drain_retries_interruptions_and_stops_on_eof_or_error() {
    use std::collections::VecDeque;
    use std::io::{self, ErrorKind};

    struct ScriptedReader(VecDeque<Result<usize, ErrorKind>>);
    impl Read for ScriptedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            assert!(!buffer.is_empty() && buffer.len() <= 4096);
            let available = self
                .0
                .pop_front()
                .expect("drain read past its terminal event")?;
            let count = available.min(buffer.len());
            if available > count {
                self.0.push_front(Ok(available - count));
            }
            buffer[..count].fill(b'x');
            Ok(count)
        }
    }

    for terminal in [Ok(0), Err(ErrorKind::BrokenPipe)] {
        let mut reader = ScriptedReader(VecDeque::from([
            Err(ErrorKind::Interrupted),
            Ok(17),
            Err(ErrorKind::Interrupted),
            Ok(4096),
            terminal,
            Ok(1),
        ]));
        discard_worker_stderr(&mut reader);
        assert_eq!(
            reader.0,
            VecDeque::from([Ok(1)]),
            "drain must consume through the terminal event, retrying interruptions"
        );
    }
}
