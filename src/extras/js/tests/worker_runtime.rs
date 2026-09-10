use std::io::{ErrorKind, Read, Write};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[cfg(not(feature = "skills"))]
use crate::extras::js::protocol::ArtifactInput;
use crate::extras::js::protocol::{
    AdvisoryAttribution, BuildIdentity, ConsoleLevel, DiagnosticClass, DiagnosticStage,
    EffectError, EffectErrorCode, EffectOperation, EffectRequest, EffectResponse, EffectResult,
    GrantId, InvocationId, JsErrorCode, JsExceptionClass, LaunchChallenge, ParentFrame,
    ParentHello, ParentProtocol, ParentWireFrame, ProtocolFault, ProtocolFaultCode, ProtocolStage,
    RunStep, ScriptRole, StepOutcome, StepResult, VerificationCase, VerificationCaseResult,
    VerificationResult, VerifyArtifact, WireFrame, WorkerFrame, WorkerProtocol, WorkerReady,
    WorkerWireFrame, read_frame, write_frame,
};
use crate::extras::js::supervisor::{
    EffectFuture, InvocationEffectHandler, JsWorkerSupervisor, WorkerError,
};
use crate::extras::js::types::PermCancellation;
#[cfg(windows)]
use crate::sandbox::worker::ProductionWorkerLauncher;
use crate::sandbox::worker::{
    TestSupervisorStartup, TestWorkerLauncher, WorkerLaunchError, WorkerLauncher, WorkerProcess,
};

const TEST_CREDENTIAL_CANARY: &str = "A07_CREDENTIAL_CANARY_MUST_NOT_LEAK";
const TEST_CONFIG_CANARY: &str = "A07_CONFIG_CANARY_MUST_NOT_LEAK";
const TEST_WORKSPACE_CANARY: &str = "A07_WORKSPACE_CANARY_MUST_NOT_LEAK";

#[cfg(feature = "skills")]
#[test]
fn worker_supervisor_partial_realm_loading_oom_is_closed_and_recovers() {
    use crate::extras::js::skills::{CapabilityManifest, SkillArtifact, SkillExport};
    let supervisor = JsWorkerSupervisor::with_launcher_for_test(
        TestWorkerLauncher::internal_worker_process_with_limits(10_000, 10_000),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Locate the boundary where source allocation succeeds but later wrapper allocations can
    // fail. Searching the whole budget avoids assuming a platform-specific bootstrap footprint.
    let (mut low, mut high) = (0, crate::extras::js::types::MEMORY_LIMIT);
    let mut saw_success = false;
    let mut saw_resource_failure = false;
    while low <= high {
        let count = low + (high - low) / 2;
        let exports: Vec<_> = (0..crate::extras::js::protocol::MAX_SKILL_EXPORTS_PER_ARTIFACT)
            .map(|i| SkillExport {
                name: format!("answer{i}"),
                signature: "()".into(),
            })
            .collect();
        let mut source = format!("const buffer=new ArrayBuffer({count});");
        for export in &exports {
            source.push_str(&format!("function {}() {{ return true; }}", export.name));
        }
        let artifact = SkillArtifact::new(
            source,
            "partial-publication pressure".into(),
            vec![],
            exports,
            vec!["true".into()],
            CapabilityManifest::pure(),
        )
        .unwrap();
        let request = VerifyArtifact {
            artifact,
            cases: vec![VerificationCase {
                case_id: "load".into(),
                script: "true".into(),
                kind: crate::extras::js::protocol::VerificationCaseKind::Embedded,
            }],
        };
        let result = supervisor
            .verify_blocking(request)
            .unwrap_or_else(|error| panic!("heap allocation {count}: {error:?}"));
        if result.passed {
            saw_success = true;
            low = count + 1;
        } else {
            saw_resource_failure = true;
            let diagnostic = result.cases[0]
                .diagnostic
                .as_ref()
                .expect("closed resource diagnostic");
            assert_eq!(
                diagnostic.class,
                DiagnosticClass::ResourceLimit,
                "heap allocation {count}"
            );
            assert_eq!(runtime.block_on(supervisor.generation_for_test()), None);
            high = count - 1;
        }
    }
    assert!(
        saw_success && saw_resource_failure,
        "search must straddle the heap boundary"
    );
    let next = runtime
        .block_on(supervisor.execute(
            RunStep::new("42".into()),
            RecordingEffects::default(),
            PermCancellation::new(),
        ))
        .unwrap();
    assert_eq!(next.outcome, StepOutcome::Value("42".into()));
    runtime.block_on(supervisor.shutdown_for_test()).unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn windows_production_supervisor_rejects_nonproduction_test_image() {
    let supervisor = JsWorkerSupervisor::with_launcher_for_test(ProductionWorkerLauncher);
    let error = supervisor
        .execute(
            RunStep::new("must-not-launch".into()),
            RecordingEffects::default(),
            PermCancellation::new(),
        )
        .await
        .expect_err("the libtest image must not become a production worker");
    assert_eq!(error, WorkerError::ContainmentUnavailable);
}

fn hello(parent: &ParentProtocol, sequence: u64) -> ParentWireFrame {
    WireFrame::connection(
        BuildIdentity::current(),
        sequence,
        ParentFrame::Hello(parent.hello()),
    )
}

fn test_launch_challenge() -> LaunchChallenge {
    LaunchChallenge::new(uuid::Uuid::from_u128(1)).unwrap()
}

fn shutdown(sequence: u64) -> ParentWireFrame {
    WireFrame::connection(BuildIdentity::current(), sequence, ParentFrame::Shutdown)
}

fn run_step(sequence: u64, invocation: &str, code: impl Into<String>) -> ParentWireFrame {
    WireFrame::invocation(
        BuildIdentity::current(),
        InvocationId::new(invocation).unwrap(),
        sequence,
        ParentFrame::RunStep(RunStep::new(code.into())),
    )
}

fn verify_artifact(
    sequence: u64,
    invocation: &str,
    source: impl Into<String>,
    tests: Vec<String>,
    cases: Vec<(&str, &str)>,
) -> ParentWireFrame {
    let source = source.into();
    #[cfg(feature = "skills")]
    let artifact = crate::extras::js::skills::SkillArtifact::new(
        if source.is_empty() {
            "function answer(_cap) { return 42; }".into()
        } else {
            source
        },
        format!("verification fixture {invocation}"),
        vec![],
        vec![crate::extras::js::skills::SkillExport {
            name: "answer".into(),
            signature: "answer()".into(),
        }],
        tests.clone(),
        crate::extras::js::skills::CapabilityManifest::pure(),
    )
    .unwrap();
    #[cfg(not(feature = "skills"))]
    let artifact = ArtifactInput {
        artifact_id: format!("artifact-{invocation}"),
        source,
        exports: vec!["answer".into()],
        tests: tests.clone(),
    };
    #[cfg(feature = "skills")]
    let mut verification_cases = tests
        .into_iter()
        .enumerate()
        .map(|(index, script)| VerificationCase {
            case_id: format!("embedded-{index}"),
            script,
            kind: crate::extras::js::protocol::VerificationCaseKind::Embedded,
        })
        .collect::<Vec<_>>();
    #[cfg(not(feature = "skills"))]
    let mut verification_cases = Vec::new();
    verification_cases.extend(cases.into_iter().map(|(case_id, script)| VerificationCase {
        case_id: case_id.into(),
        script: script.into(),
        #[cfg(feature = "skills")]
        kind: crate::extras::js::protocol::VerificationCaseKind::HeldOut {
            expected: crate::extras::js::protocol::VerificationExpectedValue::Boolean(true),
            fake_files: Default::default(),
            fake_spawns: Default::default(),
            fake_fetches: Default::default(),
        },
    }));
    WireFrame::invocation(
        BuildIdentity::current(),
        InvocationId::new(invocation).unwrap(),
        sequence,
        ParentFrame::VerifyArtifact(VerifyArtifact {
            artifact,
            cases: verification_cases,
        }),
    )
}

fn write_parent_frame(output: &mut impl Write, frame: &ParentWireFrame) {
    write_frame(output, frame).expect("parent frame should serialize");
    output.flush().expect("parent protocol pipe should flush");
}

/// The current executable is a libtest binary. Discard only libtest's bounded textual preamble,
/// then return the first valid worker frame. The worker child exits directly, so no harness text
/// can follow the protocol once bootstrap begins.
fn read_worker_frame_after_test_preamble(
    input: &mut impl Read,
) -> std::io::Result<(Vec<u8>, WorkerWireFrame)> {
    let mut preamble = Vec::new();
    let mut window = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        input.read_exact(&mut byte)?;
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
            input.read_exact(&mut tail)?;
            encoded.extend_from_slice(&tail);
            if let Ok(frame) = read_frame(&mut encoded.as_slice()) {
                return Ok((preamble, frame));
            }
        }

        preamble.push(window.remove(0));
        if preamble.len() > 4096 {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "worker emitted an unbounded non-protocol preamble",
            ));
        }
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
            let cleanup = process.terminate_and_reap(Duration::from_secs(1));
            panic!("worker child exceeded the five-second test deadline (cleanup: {cleanup:?})");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn run_worker_transcript(
    requests: Vec<ParentWireFrame>,
    test_timeout_ms: u64,
    test_max_pending_jobs: usize,
) -> (Vec<WorkerWireFrame>, Vec<u8>) {
    const MAX_STARTUP_ATTEMPTS: usize = 3;
    let (mut process, mut parent, preamble, ready) = 'startup: {
        for attempt in 1..=MAX_STARTUP_ATTEMPTS {
            let mut process = TestWorkerLauncher::internal_worker_process_with_limits(
                test_timeout_ms,
                test_max_pending_jobs,
            )
            .launch()
            .expect("test worker should launch");
            let mut parent = ParentProtocol::new(BuildIdentity::current());
            let hello = hello(&parent, 0);
            parent.on_send(&hello).unwrap();
            write_parent_frame(&mut process.input, &hello);
            match read_worker_frame_after_test_preamble(&mut process.output) {
                Ok((preamble, ready)) => {
                    break 'startup (process, parent, preamble, ready);
                }
                Err(error) => {
                    process
                        .terminate_and_reap(Duration::from_secs(1))
                        .expect("failed-startup worker should terminate and reap before retry");
                    if error.kind() == ErrorKind::UnexpectedEof && attempt < MAX_STARTUP_ATTEMPTS {
                        eprintln!(
                            "test worker exited before Ready on attempt {attempt}; retrying with a fresh worker"
                        );
                        continue;
                    }
                    panic!("test worker failed before Ready on attempt {attempt}: {error}")
                }
            }
        }
        unreachable!("the final startup failure returns through the error arm")
    };
    assert_redacted(&preamble);
    assert!(matches!(ready.message, WorkerFrame::Ready(_)));
    parent.on_receive(&ready).unwrap();
    let mut frames = vec![ready];
    for request in &requests {
        parent.on_send(request).unwrap();
        write_parent_frame(&mut process.input, request);
        let (interleaved_harness, response) =
            read_worker_frame_after_test_preamble(&mut process.output)
                .expect("worker exited before emitting the request response");
        assert_redacted(&interleaved_harness);
        parent.on_receive(&response).unwrap();
        frames.push(response);
    }
    let shutdown = shutdown((requests.len() as u64 + 1) * 2);
    parent.on_send(&shutdown).unwrap();
    write_parent_frame(&mut process.input, &shutdown);

    let status = wait_for_exit(&mut process);
    assert!(status.success(), "worker transcript should exit cleanly");
    assert_eq!(frames.len(), requests.len() + 1);

    let mut stderr = Vec::new();
    process.stderr.read_to_end(&mut stderr).unwrap();
    assert!(stderr.len() <= 4096, "worker stderr must remain bounded");
    assert_redacted(&stderr);
    (frames, stderr)
}

fn run_steps(codes: &[&str], timeout_ms: u64, max_jobs: usize) -> Vec<StepResult> {
    let requests = codes
        .iter()
        .enumerate()
        .map(|(index, code)| run_step((index as u64 + 1) * 2, &format!("step-{index}"), *code))
        .collect();
    run_worker_transcript(requests, timeout_ms, max_jobs)
        .0
        .into_iter()
        .skip(1)
        .map(|frame| match frame.message {
            WorkerFrame::StepResult(result) => result,
            message => panic!("expected StepResult, received {message:?}"),
        })
        .collect()
}

fn verification_results(
    requests: Vec<ParentWireFrame>,
    timeout_ms: u64,
    max_jobs: usize,
) -> Vec<VerificationResult> {
    run_worker_transcript(requests, timeout_ms, max_jobs)
        .0
        .into_iter()
        .skip(1)
        .map(|frame| match frame.message {
            WorkerFrame::VerificationResult(result) => result,
            message => panic!("expected VerificationResult, received {message:?}"),
        })
        .collect()
}

fn assert_closed_error(
    result: &StepResult,
    code: JsErrorCode,
    class: DiagnosticClass,
    stage: DiagnosticStage,
) {
    assert_eq!(result.outcome, StepOutcome::Error(code));
    let diagnostic = result.diagnostic.as_ref().expect("error needs diagnostic");
    assert_eq!(diagnostic.class, class);
    assert_eq!(diagnostic.stage, stage);
    assert_eq!(diagnostic.script_role, ScriptRole::Model);
}

fn assert_closed_exception(
    result: &StepResult,
    code: JsErrorCode,
    class: DiagnosticClass,
    exception_class: JsExceptionClass,
    location: Option<(u32, u32)>,
) {
    assert_closed_error(result, code, class, DiagnosticStage::Evaluation);
    let diagnostic = result.diagnostic.as_ref().unwrap();
    assert_eq!(diagnostic.exception_class, Some(exception_class));
    assert_eq!(diagnostic.line.zip(diagnostic.column), location);
    assert_eq!(diagnostic.line.is_some(), diagnostic.column.is_some());
}

#[tokio::test]
async fn worker_runtime_allows_exactly_the_effect_limit() {
    let supervisor =
        JsWorkerSupervisor::with_launcher_for_test(TestWorkerLauncher::internal_worker_process());
    let effects = RecordingEffects::default();
    let witness = effects.clone();
    let grant_id = GrantId::new(uuid::Uuid::from_u128(1)).unwrap();
    let result = supervisor
        .execute(
            RunStep::new(
                "for (let i = 0; i < 256; i += 1) read_file('fixture'); 'complete'".into(),
            )
            .with_model_grant(grant_id),
            effects,
            PermCancellation::new(),
        )
        .await
        .expect("the documented effect limit must remain allowed");

    assert_eq!(result.outcome, StepOutcome::Value("complete".into()));
    assert_eq!(
        *witness.ordinals.lock().unwrap(),
        (0..crate::extras::js::protocol::MAX_EFFECTS_PER_STEP).collect::<Vec<_>>()
    );
    supervisor.shutdown_for_test().await.unwrap();
}

#[tokio::test]
async fn worker_runtime_read_files_uses_one_ordered_effect_request() {
    let supervisor =
        JsWorkerSupervisor::with_launcher_for_test(TestWorkerLauncher::internal_worker_process());
    let effects = RecordingEffects::default();
    let witness = effects.clone();
    let grant_id = GrantId::new(uuid::Uuid::from_u128(2)).unwrap();
    let result = supervisor
        .execute(
            RunStep::new("read_files(['first.txt', 'second.txt', 'third.txt']).join('|')".into())
                .with_model_grant(grant_id),
            effects,
            PermCancellation::new(),
        )
        .await
        .expect("batched reads should complete");

    assert_eq!(
        result.outcome,
        StepOutcome::Value("content:first.txt|content:second.txt|content:third.txt".into())
    );
    assert_eq!(*witness.ordinals.lock().unwrap(), vec![0]);
    assert_eq!(
        *witness.operations.lock().unwrap(),
        vec![EffectOperation::ReadFiles {
            paths: vec!["first.txt".into(), "second.txt".into(), "third.txt".into()],
        }]
    );
    supervisor.shutdown_for_test().await.unwrap();
}

#[tokio::test]
async fn worker_runtime_read_files_rejects_invalid_batch_before_dispatch() {
    let supervisor =
        JsWorkerSupervisor::with_launcher_for_test(TestWorkerLauncher::internal_worker_process());
    let effects = RecordingEffects::default();
    let witness = effects.clone();
    let grant_id = GrantId::new(uuid::Uuid::from_u128(3)).unwrap();
    let result = supervisor
        .execute(
            RunStep::new(
                "let empty; try { read_files([]); } catch (e) { empty = e.code; } \
                 let large; try { read_files(Array(257).fill('x')); } catch (e) { large = e.code; } \
                 `${empty},${large}`"
                    .into(),
            )
            .with_model_grant(grant_id),
            effects,
            PermCancellation::new(),
        )
        .await
        .expect("invalid batches should be catchable");

    assert_eq!(
        result.outcome,
        StepOutcome::Value("invalid_target,too_large".into())
    );
    assert!(witness.ordinals.lock().unwrap().is_empty());
    supervisor.shutdown_for_test().await.unwrap();
}

#[tokio::test]
async fn worker_runtime_effect_limit_returns_terminal_with_console_and_recovers() {
    let supervisor =
        JsWorkerSupervisor::with_launcher_for_test(TestWorkerLauncher::internal_worker_process());
    let effects = RecordingEffects::default();
    let witness = effects.clone();
    let grant_id = GrantId::new(uuid::Uuid::from_u128(1)).unwrap();
    let result = supervisor
        .execute(
            RunStep::new(
                "console.log('before limit'); \
                 for (let i = 0; i < 300; i += 1) { \
                     try { read_file('fixture'); } catch (_) {} \
                 } \
                 console.log('after limit'); \
                 'partial'"
                    .into(),
            )
            .with_model_grant(grant_id),
            effects,
            PermCancellation::new(),
        )
        .await
        .expect("effect exhaustion must return a valid terminal result");

    assert_closed_error(
        &result,
        JsErrorCode::EffectLimit,
        DiagnosticClass::ResourceLimit,
        DiagnosticStage::Evaluation,
    );
    assert_eq!(
        result.console,
        vec![
            crate::extras::js::protocol::ConsoleRecord {
                level: ConsoleLevel::Log,
                text: "before limit".into(),
                truncated: false,
            },
            crate::extras::js::protocol::ConsoleRecord {
                level: ConsoleLevel::Log,
                text: "after limit".into(),
                truncated: false,
            },
        ]
    );
    assert_eq!(
        *witness.ordinals.lock().unwrap(),
        (0..crate::extras::js::protocol::MAX_EFFECTS_PER_STEP).collect::<Vec<_>>()
    );
    #[cfg(feature = "skills")]
    assert!(!result.evidence_complete);
    let generation = supervisor
        .generation_for_test()
        .await
        .expect("effect exhaustion must leave the worker reusable");

    let recovered = supervisor
        .execute(
            RunStep::new("6 * 7".into()),
            RecordingEffects::default(),
            PermCancellation::new(),
        )
        .await
        .expect("a request after effect exhaustion must use a healthy worker");
    assert_eq!(recovered.outcome, StepOutcome::Value("42".into()));
    assert_eq!(supervisor.generation_for_test().await, Some(generation));
    supervisor.shutdown_for_test().await.unwrap();
}

#[tokio::test]
async fn worker_runtime_uncaught_effect_limit_uses_the_closed_limit_error() {
    let supervisor =
        JsWorkerSupervisor::with_launcher_for_test(TestWorkerLauncher::internal_worker_process());
    let effects = RecordingEffects::default();
    let witness = effects.clone();
    let grant_id = GrantId::new(uuid::Uuid::from_u128(1)).unwrap();
    let result = supervisor
        .execute(
            RunStep::new(
                "console.warn('before limit'); \
                 for (let i = 0; i < 257; i += 1) read_file('fixture'); \
                 console.error('unreachable')"
                    .into(),
            )
            .with_model_grant(grant_id),
            effects,
            PermCancellation::new(),
        )
        .await
        .expect("uncaught effect exhaustion must return a valid terminal result");

    assert_closed_error(
        &result,
        JsErrorCode::EffectLimit,
        DiagnosticClass::ResourceLimit,
        DiagnosticStage::Evaluation,
    );
    assert_eq!(
        result.console,
        vec![crate::extras::js::protocol::ConsoleRecord {
            level: ConsoleLevel::Warn,
            text: "before limit".into(),
            truncated: false,
        }]
    );
    assert_eq!(
        *witness.ordinals.lock().unwrap(),
        (0..crate::extras::js::protocol::MAX_EFFECTS_PER_STEP).collect::<Vec<_>>()
    );
    supervisor.shutdown_for_test().await.unwrap();
}

#[test]
fn worker_runtime_fresh_steps_cover_values_promises_console_and_absent_authority() {
    let results = run_steps(
        &[
            "globalThis.step_sentinel = 41; console.log('hello', 42); ({answer: 42, nested: [true, null]})",
            "typeof globalThis.step_sentinel",
            "Promise.resolve(42)",
            "undefined",
            "[typeof process, typeof require, typeof fetch, typeof read_file, typeof importScripts].join(',')",
        ],
        10_000,
        10_000,
    );
    assert_eq!(
        results[0].outcome,
        StepOutcome::Value(r#"{"answer":42,"nested":[true,null]}"#.into())
    );
    assert_eq!(results[0].console.len(), 1);
    assert_eq!(results[0].console[0].level, ConsoleLevel::Log);
    assert_eq!(results[0].console[0].text, "hello 42");
    assert!(!results[0].console[0].truncated);
    assert_eq!(results[1].outcome, StepOutcome::Value("undefined".into()));
    assert_eq!(results[2].outcome, StepOutcome::Value("42".into()));
    assert_eq!(results[3].outcome, StepOutcome::Void);
    assert_eq!(
        results[4].outcome,
        StepOutcome::Value("undefined,undefined,undefined,undefined,undefined".into())
    );
}

#[test]
fn worker_runtime_model_scripts_support_top_level_await_and_remain_strict() {
    let results = run_steps(
        &[
            "const value = await Promise.resolve(41); value + 1",
            "const value = await 41; value + 1",
            "undeclared_model_binding = 1",
            "await 0; throw new TypeError('TOP_LEVEL_AWAIT_SECRET')",
            "Object.defineProperty(Object.prototype, 'value', {get(){console.error('COMPLETION_GETTER_RAN'); return 7}, set(_){}}); 42",
            "40 + 2",
        ],
        10_000,
        10_000,
    );

    assert_eq!(results[0].outcome, StepOutcome::Value("42".into()));
    assert_eq!(results[1].outcome, StepOutcome::Value("42".into()));
    assert_closed_exception(
        &results[2],
        JsErrorCode::Exception,
        DiagnosticClass::Exception,
        JsExceptionClass::ReferenceError,
        Some((1, 1)),
    );
    assert_closed_exception(
        &results[3],
        JsErrorCode::Exception,
        DiagnosticClass::Exception,
        JsExceptionClass::TypeError,
        Some((1, 19)),
    );
    assert!(
        !serde_json::to_string(&results[3])
            .unwrap()
            .contains("TOP_LEVEL_AWAIT_SECRET")
    );
    assert_closed_error(
        &results[4],
        JsErrorCode::Internal,
        DiagnosticClass::Internal,
        DiagnosticStage::ResultConversion,
    );
    assert!(
        results[4].console.is_empty(),
        "async completion extraction invoked a model getter"
    );
    assert_eq!(results[5].outcome, StepOutcome::Value("42".into()));
}

#[test]
fn worker_runtime_rejects_module_loading_and_recovers() {
    let results = run_steps(
        &[
            "import('file:///tmp/mini-agent-no-loader.js')",
            "40 + 2",
            "import('file:///tmp/mini-agent-native-loader.so')",
            "40 + 2",
            "import value from 'file:///tmp/mini-agent-no-loader.js'; value",
            "40 + 2",
        ],
        10_000,
        10_000,
    );

    for index in [0, 2] {
        assert_closed_error(
            &results[index],
            JsErrorCode::Exception,
            DiagnosticClass::Exception,
            DiagnosticStage::Evaluation,
        );
    }
    assert_closed_exception(
        &results[4],
        JsErrorCode::Syntax,
        DiagnosticClass::Syntax,
        JsExceptionClass::SyntaxError,
        Some((1, 1)),
    );
    for index in [1, 3, 5] {
        assert_eq!(results[index].outcome, StepOutcome::Value("42".into()));
    }
}

#[test]
fn worker_runtime_classifies_and_redacts_exceptions_then_recovers() {
    const SECRET: &str = "A08_EXCEPTION_SECRET_MUST_NOT_LEAK";
    let results = run_steps(
        &[
            "function A08_EXCEPTION_SECRET_MUST_NOT_LEAK(",
            "40 + 2",
            "return 1",
            "40 + 2",
            "with ({}) {}",
            "40 + 2",
            "throw new Error('A08_EXCEPTION_SECRET_MUST_NOT_LEAK')",
            "40 + 2",
            "Promise.reject('A08_EXCEPTION_SECRET_MUST_NOT_LEAK')",
            "40 + 2",
            r#"(()=>{
                const huge="A08_EXCEPTION_SECRET_MUST_NOT_LEAK".repeat(256 * 1024);
                const error=new Error();
                Object.defineProperty(error,"name",{get(){console.error("ERROR_ACCESSOR_RAN"); return huge;}});
                Object.defineProperty(error,"message",{get(){console.error("ERROR_ACCESSOR_RAN"); return huge;}});
                throw error;
            })()"#,
            "40 + 2",
            "(()=>{const error=new Error(); Object.setPrototypeOf(error,SyntaxError.prototype); Object.defineProperty(error,'message',{value:'unexpected token'}); throw error;})()",
            "40 + 2",
            "(()=>{const error=new Error(); Object.setPrototypeOf(error,RangeError.prototype); Object.defineProperty(error,'message',{value:'Maximum call stack size exceeded'}); throw error;})()",
            "40 + 2",
            "(()=>{const error=new Error(); if(typeof InternalError==='function') Object.setPrototypeOf(error,InternalError.prototype); Object.defineProperty(error,'message',{value:'out of memory'}); throw error;})()",
            "40 + 2",
            "function recurse(){ return recurse(); } recurse()",
            "40 + 2",
        ],
        10_000,
        10_000,
    );
    assert_closed_exception(
        &results[0],
        JsErrorCode::Syntax,
        DiagnosticClass::Syntax,
        JsExceptionClass::SyntaxError,
        Some((1, 9)),
    );
    for (index, location) in [(2, (1, 1)), (4, (1, 1))] {
        assert_closed_exception(
            &results[index],
            JsErrorCode::Syntax,
            DiagnosticClass::Syntax,
            JsExceptionClass::SyntaxError,
            Some(location),
        );
    }
    assert_closed_exception(
        &results[6],
        JsErrorCode::Exception,
        DiagnosticClass::Exception,
        JsExceptionClass::Other,
        Some((1, 10)),
    );
    assert_closed_exception(
        &results[8],
        JsErrorCode::Exception,
        DiagnosticClass::Exception,
        JsExceptionClass::Other,
        None,
    );
    assert_closed_exception(
        &results[10],
        JsErrorCode::Exception,
        DiagnosticClass::Exception,
        JsExceptionClass::Other,
        Some((3, 33)),
    );
    assert_closed_exception(
        &results[12],
        JsErrorCode::Syntax,
        DiagnosticClass::Syntax,
        JsExceptionClass::SyntaxError,
        Some((1, 22)),
    );
    assert_closed_exception(
        &results[14],
        JsErrorCode::StackLimit,
        DiagnosticClass::ResourceLimit,
        JsExceptionClass::RangeError,
        Some((1, 22)),
    );
    assert_closed_exception(
        &results[16],
        JsErrorCode::Exception,
        DiagnosticClass::Exception,
        JsExceptionClass::InternalError,
        Some((1, 22)),
    );
    assert_closed_exception(
        &results[18],
        JsErrorCode::StackLimit,
        DiagnosticClass::ResourceLimit,
        JsExceptionClass::RangeError,
        Some((1, 27)),
    );
    assert!(
        results[10].console.is_empty(),
        "classifier invoked an error accessor"
    );
    for index in [1, 3, 5, 7, 9, 11, 13, 15, 17, 19] {
        assert_eq!(results[index].outcome, StepOutcome::Value("42".into()));
    }
    let encoded = serde_json::to_string(&results).unwrap();
    assert!(!encoded.contains(SECRET));
}

#[test]
fn worker_runtime_reports_only_valid_model_exception_locations() {
    const SECRET: &str = "MODEL_EXCEPTION_LOCATION_SECRET_MUST_NOT_LEAK";
    let results = run_steps(
        &[
            "const value = null;\nvalue.missing()",
            "const present = 1;\nmissing_name + present",
            "Promise.reject(new TypeError('MODEL_EXCEPTION_LOCATION_SECRET_MUST_NOT_LEAK'))",
            "(()=>{const error=new TypeError(); Object.defineProperty(error,'stack',{value:'    at forged (mini-agent-model.js:999:999)'}); throw error;})()",
            "(()=>{const error=new ReferenceError(); Object.defineProperty(error,'stack',{get(){console.error('STACK_ACCESSOR_RAN'); return 'mini-agent-model.js:1:1';}}); throw error;})()",
        ],
        10_000,
        10_000,
    );
    assert_closed_exception(
        &results[0],
        JsErrorCode::Exception,
        DiagnosticClass::Exception,
        JsExceptionClass::TypeError,
        Some((2, 1)),
    );
    assert_closed_exception(
        &results[1],
        JsErrorCode::Exception,
        DiagnosticClass::Exception,
        JsExceptionClass::ReferenceError,
        Some((2, 1)),
    );
    assert_closed_exception(
        &results[2],
        JsErrorCode::Exception,
        DiagnosticClass::Exception,
        JsExceptionClass::TypeError,
        Some((1, 19)),
    );
    assert_closed_exception(
        &results[3],
        JsErrorCode::Exception,
        DiagnosticClass::Exception,
        JsExceptionClass::TypeError,
        None,
    );
    assert_closed_exception(
        &results[4],
        JsErrorCode::Exception,
        DiagnosticClass::Exception,
        JsExceptionClass::ReferenceError,
        None,
    );
    assert!(!serde_json::to_string(&results).unwrap().contains(SECRET));
    assert!(
        results[4].console.is_empty(),
        "classifier invoked stack accessor"
    );
}

#[test]
fn worker_runtime_exception_inspector_uses_captured_intrinsics_and_bounded_stack_data() {
    let poisoned_intrinsics = "(()=>{const error=new TypeError(); Error.isError=()=>false; Object.getPrototypeOf=()=>{throw 1}; Object.getOwnPropertyDescriptor=()=>{throw 2}; String.prototype.indexOf=()=>{throw 3}; String.prototype.charCodeAt=()=>{throw 4}; throw error;})()";
    let oversized_stack = "(()=>{const error=new TypeError(); Object.defineProperty(error,'stack',{value:'x'.repeat(16385)+' mini-agent-model.js:1:1'}); throw error;})()";
    let results = run_steps(
        &[poisoned_intrinsics, oversized_stack, "40 + 2"],
        10_000,
        10_000,
    );

    assert_closed_error(
        &results[0],
        JsErrorCode::Exception,
        DiagnosticClass::Exception,
        DiagnosticStage::Evaluation,
    );
    let poisoned = results[0].diagnostic.as_ref().unwrap();
    assert_eq!(poisoned.exception_class, Some(JsExceptionClass::TypeError));
    assert_eq!(poisoned.line, Some(1));
    assert!(poisoned.column.is_some());

    assert_closed_exception(
        &results[1],
        JsErrorCode::Exception,
        DiagnosticClass::Exception,
        JsExceptionClass::TypeError,
        None,
    );
    assert_eq!(results[2].outcome, StepOutcome::Value("42".into()));
}

#[test]
fn worker_runtime_timeout_and_pending_job_limits_reset_before_next_request() {
    let timeout = run_steps(&["while (true) {}", "40 + 2"], 50, 10_000);
    assert_eq!(timeout[0].outcome, StepOutcome::Timeout);
    assert_eq!(timeout[1].outcome, StepOutcome::Value("42".into()));

    let jobs = run_steps(
        &[
            "function spin(){ Promise.resolve().then(spin); } spin()",
            "40 + 2",
        ],
        10_000,
        128,
    );
    assert_closed_error(
        &jobs[0],
        JsErrorCode::JobLimit,
        DiagnosticClass::ResourceLimit,
        DiagnosticStage::JobDrain,
    );
    assert_eq!(jobs[1].outcome, StepOutcome::Value("42".into()));
}

#[tokio::test]
async fn worker_supervisor_oom_blocks_effects_and_recycles_generation() {
    for source in [
        "const chunks=[]; while(true){ chunks.push(new ArrayBuffer(1024 * 1024)); }",
        "(() => { const chunks=[]; while(true){ chunks.push(new ArrayBuffer(1024 * 1024)); } })()",
        "new ArrayBuffer(128 * 1024 * 1024)",
        "try { new ArrayBuffer(128 * 1024 * 1024); } catch (_) {} read_file('after-oom'); 42",
        "Promise.resolve().then(() => { try { new ArrayBuffer(128 * 1024 * 1024); } catch (_) {} read_file('after-job-oom'); return 42; })",
    ] {
        let supervisor = JsWorkerSupervisor::with_launcher_for_test(
            TestWorkerLauncher::internal_worker_process_with_limits(10_000, 10_000),
        );
        let effects = RecordingEffects::default();
        let result = supervisor
            .execute(
                RunStep::new(source.into()),
                effects.clone(),
                PermCancellation::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.outcome, StepOutcome::OutOfMemory, "{source}");
        assert!(
            effects.operations.lock().unwrap().is_empty(),
            "effect after OOM: {source}"
        );
        assert_eq!(supervisor.generation_for_test().await, None);
        let next = supervisor
            .execute(
                RunStep::new("42".into()),
                RecordingEffects::default(),
                PermCancellation::new(),
            )
            .await
            .unwrap();
        assert_eq!(next.outcome, StepOutcome::Value("42".into()));
        assert_eq!(supervisor.generation_for_test().await, Some(2));
        supervisor.shutdown_for_test().await.unwrap();
    }
}

#[test]
fn worker_runtime_strict_clone_rejects_accessors_cycles_depth_and_bytes_without_execution() {
    let deep = "let value={}; let cursor=value; for(let i=0;i<80;i++){ cursor.next={}; cursor=cursor.next; } value";
    let results = run_steps(
        &[
            "const value={}; Object.defineProperty(value,'secret',{enumerable:true,get(){console.error('ACCESSOR_RAN'); return 1;}}); value",
            "(()=>{ const value={}; value.self=value; return value; })()",
            deep,
            "'x'.repeat(8 * 1024 * 1024)",
            "({value: undefined})",
            "({value: '\\u0000'.repeat(16 * 1024)})",
            r#"(()=>{
                const value={safe:[1,"💥"]};
                const poison=()=>{ console.error("INTRINSIC_CANARY"); throw new Error("poisoned"); };
                const SetCtor=Set;
                const StringCtor=String;
                Set=poison;
                SetCtor.prototype.has=poison;
                SetCtor.prototype.add=poison;
                SetCtor.prototype.delete=poison;
                Number.isSafeInteger=poison;
                Number.isInteger=poison;
                String=poison;
                StringCtor.prototype[Symbol.iterator]=poison;
                StringCtor.prototype.codePointAt=poison;
                Array.prototype.push=poison;
                Array.prototype[Symbol.iterator]=poison;
                Array.prototype.toJSON=poison;
                Function.prototype.call=poison;
                Function.prototype.bind=poison;
                return value;
            })()"#,
            r#"(()=>{
                const value=[1,2];
                Object.defineProperty(Array.prototype,"0",{
                    set(){console.error("NUMERIC_SETTER_CANARY"); throw new Error("poisoned");},
                    configurable:true
                });
                return value;
            })()"#,
            "new Proxy({}, {ownKeys(){throw new Error('PROXY_CLONE_SECRET');}})",
            "40 + 2",
        ],
        10_000,
        10_000,
    );
    for result in &results[..6] {
        assert_closed_error(
            result,
            JsErrorCode::InvalidResult,
            DiagnosticClass::Contract,
            DiagnosticStage::ResultConversion,
        );
    }
    assert!(results[0].console.is_empty(), "clone invoked an accessor");
    assert_eq!(
        results[6].outcome,
        StepOutcome::Value(r#"{"safe":[1,"💥"]}"#.into())
    );
    assert!(
        results[6].console.is_empty(),
        "clone invoked a poisoned intrinsic"
    );
    assert_closed_error(
        &results[7],
        JsErrorCode::InvalidResult,
        DiagnosticClass::Contract,
        DiagnosticStage::ResultConversion,
    );
    assert!(
        results[7].console.is_empty(),
        "clone invoked a numeric setter"
    );
    assert_closed_error(
        &results[8],
        JsErrorCode::InvalidResult,
        DiagnosticClass::Contract,
        DiagnosticStage::ResultConversion,
    );
    assert!(
        !serde_json::to_string(&results)
            .unwrap()
            .contains("PROXY_CLONE_SECRET")
    );
    assert_eq!(results[9].outcome, StepOutcome::Value("42".into()));
}

#[test]
fn worker_runtime_console_is_bounded_and_reports_truncation() {
    let results = run_steps(
        &[
            "for(let i=0;i<300;i++){ console.warn('x'.repeat(2048)); }",
            "console.log('x'.repeat(8 * 1024 * 1024))",
            "40 + 2",
        ],
        10_000,
        10_000,
    );
    assert_eq!(results[0].outcome, StepOutcome::Void);
    assert!(results[0].console.len() <= 256);
    assert!(
        results[0]
            .console
            .iter()
            .map(|record| record.text.len())
            .sum::<usize>()
            <= 256 * 1024
    );
    assert!(results[0].console.iter().any(|record| record.truncated));
    assert_eq!(results[1].outcome, StepOutcome::Void);
    assert_eq!(results[1].console.len(), 1);
    assert!(results[1].console[0].text.len() <= 8 * 1024);
    assert!(results[1].console[0].truncated);
    assert_eq!(results[2].outcome, StepOutcome::Value("42".into()));
}

#[cfg(feature = "skills")]
#[test]
fn worker_runtime_verification_reloads_production_realm_for_every_case() {
    let results = verification_results(
        vec![verify_artifact(
            2,
            "isolated-production-loader",
            "let count = 0; function answer(_cap) { return ++count; }",
            vec!["answer() === 1".into(), "answer() === 1".into()],
            vec![("held-out-fresh", "answer() === 1")],
        )],
        10_000,
        10_000,
    );
    assert_eq!(results.len(), 1);
    assert!(results[0].passed);
    assert_eq!(results[0].loader_version, 1);
    assert_eq!(results[0].cases.len(), 3);
    assert!(results[0].cases.iter().all(|case| case.passed));
    assert!(
        results[0]
            .cases
            .iter()
            .all(|case| case.transcript.is_empty())
    );
}

#[test]
fn worker_runtime_verification_never_reports_source_positions() {
    const SECRET: &str = "VERIFICATION_EXCEPTION_SECRET_MUST_NOT_LEAK";
    let results = verification_results(
        vec![verify_artifact(
            2,
            "source-free-verification-diagnostics",
            "function answer(_cap) { return 42; }",
            vec!["throw new TypeError('VERIFICATION_EXCEPTION_SECRET_MUST_NOT_LEAK')".into()],
            vec![("held-out-reference", "missing_verification_name")],
        )],
        10_000,
        10_000,
    );

    assert!(!results[0].passed);
    assert_eq!(results[0].cases.len(), 2);
    for (case, expected_class) in results[0].cases.iter().zip([
        JsExceptionClass::TypeError,
        JsExceptionClass::ReferenceError,
    ]) {
        let diagnostic = case.diagnostic.as_ref().expect("failed case diagnostic");
        assert_eq!(diagnostic.class, DiagnosticClass::Exception);
        assert_eq!(diagnostic.exception_class, Some(expected_class));
        assert_eq!(diagnostic.line, None);
        assert_eq!(diagnostic.column, None);
    }
    assert!(!serde_json::to_string(&results).unwrap().contains(SECRET));
}

#[cfg(feature = "skills")]
#[test]
fn worker_runtime_verification_bounds_transcripts_across_the_whole_request() {
    use crate::extras::js::protocol::VerificationCaseKind;
    use crate::extras::js::skills::{
        CapabilityTier, HostCapability, SkillArtifact, SkillExport, test_manifest,
    };

    let artifact = SkillArtifact::new(
        "function answer(cap) { for (let i = 0; i < 129; i += 1) cap.write_file('tmp/out', String(i)); return true; }".into(),
        "aggregate transcript bound fixture".into(),
        vec![],
        vec![SkillExport {
            name: "answer".into(),
            signature: "answer(): boolean".into(),
        }],
        vec!["answer()".into()],
        test_manifest(CapabilityTier::SideEffecting, vec![HostCapability::WriteFile]).unwrap(),
    )
    .unwrap();
    let bounded = WireFrame::invocation(
        BuildIdentity::current(),
        InvocationId::new("bounded-transcripts").unwrap(),
        2,
        ParentFrame::VerifyArtifact(VerifyArtifact {
            artifact,
            cases: vec![
                VerificationCase {
                    case_id: "first".into(),
                    script: "answer()".into(),
                    kind: VerificationCaseKind::Embedded,
                },
                VerificationCase {
                    case_id: "second".into(),
                    script: "answer()".into(),
                    kind: VerificationCaseKind::Embedded,
                },
            ],
        }),
    );
    let results = verification_results(
        vec![
            bounded,
            verify_artifact(
                4,
                "bounded-transcript-fresh",
                "",
                vec![],
                vec![("fresh", "true")],
            ),
        ],
        10_000,
        10_000,
    );

    assert!(!results[0].passed);
    assert!(results[0].cases[0].passed);
    assert!(!results[0].cases[1].passed);
    assert!(
        results[0]
            .cases
            .iter()
            .map(|case| case.transcript.call_count())
            .sum::<usize>()
            <= crate::extras::js::skills::fakes::VERIFICATION_TRANSCRIPT_MAX_CALLS
    );
    assert!(results[1].passed);
}

#[cfg(feature = "skills")]
#[test]
fn worker_runtime_verification_rejects_large_write_transcripts_before_terminal_framing() {
    use crate::extras::js::protocol::VerificationCaseKind;
    use crate::extras::js::skills::{
        CapabilityTier, HostCapability, SkillArtifact, SkillExport, test_manifest,
    };

    let artifact = SkillArtifact::new(
        "function answer(cap) { const payload = 'x'.repeat(4096); for (let i = 0; i < 256; i += 1) cap.write_file('tmp/out', payload); return true; }".into(),
        "large write transcript fixture".into(),
        vec![],
        vec![SkillExport {
            name: "answer".into(),
            signature: "answer(): boolean".into(),
        }],
        vec!["true".into()],
        test_manifest(CapabilityTier::SideEffecting, vec![HostCapability::WriteFile]).unwrap(),
    )
    .unwrap();
    let request = WireFrame::invocation(
        BuildIdentity::current(),
        InvocationId::new("large-write-transcript").unwrap(),
        2,
        ParentFrame::VerifyArtifact(VerifyArtifact {
            artifact,
            cases: vec![VerificationCase {
                case_id: "large-writes".into(),
                script: "answer()".into(),
                kind: VerificationCaseKind::Embedded,
            }],
        }),
    );
    let results = verification_results(
        vec![
            request,
            verify_artifact(4, "large-write-fresh", "", vec![], vec![("fresh", "true")]),
        ],
        10_000,
        10_000,
    );

    assert!(!results[0].passed);
    assert!(results[0].cases[0].diagnostic.is_some());
    assert!(
        serde_json::to_vec(&results[0]).unwrap().len()
            < crate::extras::js::protocol::MAX_FRAME_BYTES
    );
    assert!(results[1].passed);
}

#[cfg(feature = "skills")]
#[test]
fn worker_runtime_verification_rejects_large_spawn_transcripts_before_cloning_arguments() {
    use crate::extras::js::protocol::VerificationCaseKind;
    use crate::extras::js::skills::{
        CapabilityTier, HostCapability, SkillArtifact, SkillExport, test_manifest,
    };

    let artifact = SkillArtifact::new(
        "function answer(cap) { const args = Array.from({length: 64}, () => 'x'.repeat(4096)); for (let i = 0; i < 256; i += 1) cap.spawn('printf', args); return true; }".into(),
        "large spawn transcript fixture".into(),
        vec![],
        vec![SkillExport {
            name: "answer".into(),
            signature: "answer(): boolean".into(),
        }],
        vec!["true".into()],
        test_manifest(CapabilityTier::SideEffecting, vec![HostCapability::Spawn]).unwrap(),
    )
    .unwrap();
    let request = WireFrame::invocation(
        BuildIdentity::current(),
        InvocationId::new("large-spawn-transcript").unwrap(),
        2,
        ParentFrame::VerifyArtifact(VerifyArtifact {
            artifact,
            cases: vec![VerificationCase {
                case_id: "large-spawns".into(),
                script: "answer()".into(),
                kind: VerificationCaseKind::Embedded,
            }],
        }),
    );
    let results = verification_results(
        vec![
            request,
            verify_artifact(4, "large-spawn-fresh", "", vec![], vec![("fresh", "true")]),
        ],
        10_000,
        10_000,
    );

    assert!(!results[0].passed);
    assert!(results[0].cases[0].diagnostic.is_some());
    assert!(
        serde_json::to_vec(&results[0]).unwrap().len()
            < crate::extras::js::protocol::MAX_FRAME_BYTES
    );
    assert!(results[1].passed);
}

#[test]
#[cfg(not(feature = "skills"))]
fn worker_runtime_verification_owns_one_fresh_runtime_per_whole_request() {
    let requests = vec![
        verify_artifact(
            2,
            "verify-0",
            "exports.answer=41; globalThis.verification_sentinel='first';",
            vec!["exports.answer === 41".into()],
            vec![
                ("increment", "++exports.answer === 42"),
                (
                    "promise",
                    "Promise.resolve(globalThis.verification_sentinel === 'first')",
                ),
            ],
        ),
        verify_artifact(
            4,
            "verify-1",
            "exports.answer=42;",
            vec![],
            vec![(
                "fresh",
                "typeof globalThis.verification_sentinel === 'undefined'",
            )],
        ),
    ];
    let frames = run_worker_transcript(requests, 10_000, 10_000).0;
    let results = frames
        .into_iter()
        .skip(1)
        .map(|frame| match frame.message {
            WorkerFrame::VerificationResult(result) => result,
            message => panic!("expected VerificationResult, received {message:?}"),
        })
        .collect::<Vec<VerificationResult>>();
    assert!(results[0].passed);
    assert_eq!(results[0].loader_version, 1);
    assert_eq!(results[0].cases.len(), 3);
    assert!(results[0].cases.iter().all(|case| case.passed));
    assert!(results[1].passed);
    assert_eq!(results[1].cases.len(), 1);
    assert_eq!(results[1].cases[0].case_id, "fresh");
}

#[test]
#[cfg(not(feature = "skills"))]
fn worker_runtime_verification_rejects_source_promises_and_recovers() {
    let results = verification_results(
        vec![
            verify_artifact(
                2,
                "source-rejected",
                "Promise.reject('SOURCE_SECRET')",
                vec![],
                vec![("rejected", "true")],
            ),
            verify_artifact(
                4,
                "source-pending",
                "new Promise(()=>{})",
                vec![],
                vec![("pending", "true")],
            ),
            verify_artifact(
                6,
                "source-fresh",
                "exports.answer=42",
                vec![],
                vec![("fresh", "exports.answer===42")],
            ),
        ],
        10_000,
        10_000,
    );
    for result in &results[..2] {
        assert!(!result.passed);
        assert_eq!(result.cases.len(), 1);
        assert_eq!(
            result.cases[0].diagnostic.as_ref().unwrap().script_role,
            ScriptRole::SkillSource
        );
    }
    assert!(results[2].passed);
    assert!(
        !serde_json::to_string(&results)
            .unwrap()
            .contains("SOURCE_SECRET")
    );
}

#[test]
#[cfg(not(feature = "skills"))]
fn worker_runtime_verification_stops_after_resource_faults_and_shares_job_budget() {
    let job_results = verification_results(
        vec![
            verify_artifact(
                2,
                "jobs-shared",
                "let n=0; function sourceJob(){if(++n<40)return Promise.resolve().then(sourceJob);} Promise.resolve().then(sourceJob)",
                vec![],
                vec![
                    (
                        "budget",
                        "let m=0; function caseJob(){if(++m<40)return Promise.resolve().then(caseJob);return true;} Promise.resolve().then(caseJob)",
                    ),
                    ("not-run", "true"),
                ],
            ),
            verify_artifact(4, "jobs-fresh", "", vec![], vec![("fresh", "true")]),
        ],
        10_000,
        64,
    );
    assert!(!job_results[0].passed);
    assert!(job_results[0].cases.iter().all(|case| !case.passed));
    assert_eq!(
        job_results[0].cases[0].diagnostic.as_ref().unwrap().stage,
        DiagnosticStage::JobDrain
    );
    assert!(job_results[1].passed);

    let oom_results = verification_results(
        vec![
            verify_artifact(
                2,
                "oom-stop",
                "",
                vec![],
                vec![
                    (
                        "oom",
                        "globalThis.chunks=[]; Promise.resolve().then(()=>{while(true){chunks.push(new ArrayBuffer(1024*1024));}})",
                    ),
                    ("not-run", "true"),
                ],
            ),
            verify_artifact(4, "oom-fresh", "", vec![], vec![("fresh", "true")]),
        ],
        10_000,
        10_000,
    );
    assert!(!oom_results[0].passed);
    assert!(oom_results[0].cases.iter().all(|case| !case.passed));
    assert!(
        oom_results[0].cases.iter().all(|case| {
            case.diagnostic.as_ref().unwrap().class == DiagnosticClass::ResourceLimit
        })
    );
    assert!(oom_results[1].passed);

    let timeout_results = verification_results(
        vec![
            verify_artifact(
                2,
                "timeout-stop",
                "",
                vec![],
                vec![("timeout", "while(true){}"), ("not-run", "true")],
            ),
            verify_artifact(4, "timeout-fresh", "", vec![], vec![("fresh", "true")]),
        ],
        50,
        10_000,
    );
    assert!(!timeout_results[0].passed);
    assert!(timeout_results[0].cases.iter().all(|case| !case.passed));
    assert!(timeout_results[1].passed);
}

// Keep this exact crash regression in the native CI verifier suite. Exhausting
// the fake transcript before OOM used to strand Rust-held promise resolvers and
// abort JS_FreeRuntime instead of returning a closed resource failure.
#[cfg(feature = "skills")]
#[test]
fn worker_runtime_verification_oom_remains_terminal_after_fake_transcript_exhaustion() {
    use crate::extras::js::skills::verify::verify_skill;
    use crate::extras::js::skills::{
        CapabilityManifest, CapabilityScope, CapabilityTier, SkillArtifact, SkillExport,
    };

    let skill = SkillArtifact::new(
        "function f(cap) { for (let i=0; i<300; i++) { try { cap.write_file('virtual/a.txt', 'x'); } catch (_) {} } try { new ArrayBuffer(128 * 1024 * 1024); } catch (_) {} return true; }".into(),
        "combined transcript and allocation failure".into(),
        vec![],
        vec![SkillExport { name: "f".into(), signature: "f(): boolean".into() }],
        vec!["f() === true".into()],
        CapabilityManifest::new(
            CapabilityTier::SideEffecting,
            vec![CapabilityScope::WriteFile { workspace_prefixes: vec!["virtual".into()] }],
        ).unwrap(),
    ).unwrap();
    let error = verify_skill(&skill).expect_err("OOM cannot become a transcript-only failure");
    assert!(error.is_resource_limit(), "{error:?}");
}

// libtest's parallel harness emits a slow-test notice after 60 seconds. This
// explicit check is slow by construction; keep it out of ordinary unit runs.
#[tokio::test]
#[ignore = "holds a reused libtest worker past the 60-second slow-test notice"]
async fn worker_supervisor_reused_test_worker_stdout_stays_protocol_only() {
    let supervisor =
        JsWorkerSupervisor::with_launcher_for_test(TestWorkerLauncher::internal_worker_process());
    let first = supervisor
        .execute(
            RunStep::new("41".into()),
            RecordingEffects::default(),
            PermCancellation::new(),
        )
        .await
        .unwrap();
    assert_eq!(first.outcome, StepOutcome::Value("41".into()));
    let generation = supervisor.generation_for_test().await.unwrap();
    tokio::time::sleep(Duration::from_secs(62)).await;
    let second = supervisor
        .execute(
            RunStep::new("42".into()),
            RecordingEffects::default(),
            PermCancellation::new(),
        )
        .await
        .unwrap();
    assert_eq!(second.outcome, StepOutcome::Value("42".into()));
    assert_eq!(supervisor.generation_for_test().await, Some(generation));
    supervisor.shutdown_for_test().await.unwrap();
}

#[test]
fn worker_runtime_verification_bounds_terminal_result_expansion() {
    #[cfg(feature = "skills")]
    let artifact = crate::extras::js::skills::SkillArtifact::new(
        "function answer() { return 42; }".into(),
        "oversized verification fixture".into(),
        vec![],
        vec![crate::extras::js::skills::SkillExport {
            name: "answer".into(),
            signature: "answer()".into(),
        }],
        vec!["true".into()],
        crate::extras::js::skills::CapabilityManifest::pure(),
    )
    .unwrap();
    #[cfg(not(feature = "skills"))]
    let artifact = ArtifactInput {
        artifact_id: "oversized".into(),
        source: String::new(),
        exports: vec![],
        tests: vec![],
    };
    let oversized = WireFrame::invocation(
        BuildIdentity::current(),
        InvocationId::new("oversized-verification").unwrap(),
        2,
        ParentFrame::VerifyArtifact(VerifyArtifact {
            artifact,
            cases: (0..4_097)
                .map(|index| VerificationCase {
                    case_id: format!("case-{index}"),
                    script: "false".into(),
                    #[cfg(feature = "skills")]
                    kind: crate::extras::js::protocol::VerificationCaseKind::Embedded,
                })
                .collect(),
        }),
    );
    let results = verification_results(
        vec![
            oversized,
            verify_artifact(4, "bounded-fresh", "", vec![], vec![("fresh", "true")]),
        ],
        10_000,
        10_000,
    );
    assert!(!results[0].passed);
    assert!(results[0].cases.is_empty());
    assert!(results[1].passed);
}

#[test]
fn worker_bootstrap_protocol_valid_hello_ready_shutdown_round_trip() {
    let mut process = TestWorkerLauncher::internal_worker_process()
        .launch()
        .expect("test worker should launch");
    let mut parent = ParentProtocol::new(BuildIdentity::current());

    let hello = hello(&parent, 0);
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
    let (preamble, ready) = read_worker_frame_after_test_preamble(&mut stdout.as_slice())
        .expect("worker Ready fixture must contain one complete frame");
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
        ParentFrame::Hello(ParentHello {
            challenge: test_launch_challenge(),
        }),
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
fn worker_bootstrap_initializes_no_parent_authority_surface() {
    let worker_source = include_str!("../worker.rs");
    for forbidden in [
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

#[derive(Clone, Default)]
struct RecordingEffects {
    ordinals: Arc<Mutex<Vec<u32>>>,
    operations: Arc<Mutex<Vec<EffectOperation>>>,
}

impl InvocationEffectHandler for RecordingEffects {
    fn handle_effect(
        &mut self,
        request: EffectRequest,
        _cancellation: PermCancellation,
    ) -> EffectFuture<'_> {
        self.ordinals.lock().unwrap().push(request.effect_ordinal);
        self.operations
            .lock()
            .unwrap()
            .push(request.operation.clone());
        Box::pin(async move {
            match request.operation {
                EffectOperation::ReadFiles { paths } => EffectResult::ReadFiles {
                    contents: paths
                        .into_iter()
                        .map(|path| format!("content:{path}"))
                        .collect(),
                },
                _ => EffectResult::ReadFile {
                    content: format!("effect-{}", request.effect_ordinal),
                },
            }
        })
    }
}

#[cfg(feature = "sandbox")]
#[derive(Clone, Default)]
struct NestedFetchEffects {
    ordinals: Arc<Mutex<Vec<u32>>>,
    active: Arc<AtomicBool>,
}

#[cfg(feature = "sandbox")]
impl InvocationEffectHandler for NestedFetchEffects {
    fn handle_effect(
        &mut self,
        request: EffectRequest,
        _cancellation: PermCancellation,
    ) -> EffectFuture<'_> {
        assert!(
            !self.active.swap(true, Ordering::AcqRel),
            "worker issued overlapping effect requests"
        );
        self.ordinals.lock().unwrap().push(request.effect_ordinal);
        let active = self.active.clone();
        Box::pin(async move {
            let result = match request.operation {
                EffectOperation::ReadFile { .. } => EffectResult::ReadFile {
                    content: "nested".into(),
                },
                EffectOperation::Fetch { .. } => EffectResult::Fetch {
                    status: 200,
                    body: "done".into(),
                },
                _ => panic!("unexpected nested fetch operation"),
            };
            active.store(false, Ordering::Release);
            result
        })
    }
}

struct PendingEffectDrop {
    dropped: Arc<AtomicBool>,
    armed: bool,
}

impl Drop for PendingEffectDrop {
    fn drop(&mut self) {
        if self.armed {
            self.dropped.store(true, Ordering::Release);
        }
    }
}

#[derive(Clone)]
struct GatedEffects {
    started: Arc<tokio::sync::Semaphore>,
    release: Arc<tokio::sync::Semaphore>,
    ordinals: Arc<Mutex<Vec<u32>>>,
    cancellation: Arc<Mutex<Option<PermCancellation>>>,
    dropped: Arc<AtomicBool>,
}

impl GatedEffects {
    fn new() -> Self {
        Self {
            started: Arc::new(tokio::sync::Semaphore::new(0)),
            release: Arc::new(tokio::sync::Semaphore::new(0)),
            ordinals: Arc::new(Mutex::new(Vec::new())),
            cancellation: Arc::new(Mutex::new(None)),
            dropped: Arc::new(AtomicBool::new(false)),
        }
    }

    async fn wait_started(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.started.acquire())
            .await
            .expect("worker did not start the gated effect; check worker launch failures")
            .unwrap()
            .forget();
    }

    fn release(&self) {
        self.release.add_permits(1);
    }
}

impl InvocationEffectHandler for GatedEffects {
    fn handle_effect(
        &mut self,
        request: EffectRequest,
        cancellation: PermCancellation,
    ) -> EffectFuture<'_> {
        self.ordinals.lock().unwrap().push(request.effect_ordinal);
        *self.cancellation.lock().unwrap() = Some(cancellation);
        let started = self.started.clone();
        let release = self.release.clone();
        let dropped = self.dropped.clone();
        Box::pin(async move {
            if request.effect_ordinal == 0 {
                let mut pending = PendingEffectDrop {
                    dropped,
                    armed: true,
                };
                started.add_permits(1);
                release.acquire().await.unwrap().forget();
                pending.armed = false;
            }
            EffectResult::ReadFile {
                content: format!("effect-{}", request.effect_ordinal),
            }
        })
    }
}

fn scripted_supervisor(stderr_bytes: usize) -> Arc<JsWorkerSupervisor> {
    Arc::new(JsWorkerSupervisor::with_launcher_for_test(
        TestWorkerLauncher::scripted_internal_worker(stderr_bytes),
    ))
}

#[derive(Clone)]
struct ReadyFinalizationLauncher {
    startup: TestSupervisorStartup,
    finalized: Arc<AtomicUsize>,
}

impl WorkerLauncher for ReadyFinalizationLauncher {
    fn containment_status(&self) -> crate::sandbox::worker::WorkerContainmentStatus {
        TestWorkerLauncher::scripted_internal_worker(0).containment_status()
    }

    fn launch(&self) -> Result<WorkerProcess, WorkerLaunchError> {
        let mut process =
            TestWorkerLauncher::scripted_internal_worker_with_startup(0, self.startup).launch()?;
        process.observe_authenticated_ready_for_test(self.finalized.clone());
        Ok(process)
    }
}

#[derive(Clone)]
struct FailingReadyFinalizationLauncher {
    live_processes: Arc<AtomicUsize>,
    parent_writes: Arc<AtomicUsize>,
}

impl WorkerLauncher for FailingReadyFinalizationLauncher {
    fn containment_status(&self) -> crate::sandbox::worker::WorkerContainmentStatus {
        TestWorkerLauncher::scripted_internal_worker(0).containment_status()
    }

    fn launch(&self) -> Result<WorkerProcess, WorkerLaunchError> {
        let mut process = TestWorkerLauncher::scripted_internal_worker(0).launch()?;
        process.observe_reap_for_test(self.live_processes.clone());
        process.observe_parent_writes_for_test(self.parent_writes.clone());
        process.force_authenticated_ready_finalization_error_for_test();
        Ok(process)
    }
}

#[derive(Clone)]
struct RecoveryLauncher {
    first_startup: TestSupervisorStartup,
    launches: Arc<AtomicUsize>,
    live_processes: Arc<AtomicUsize>,
    last_process_id: Arc<AtomicUsize>,
}

impl RecoveryLauncher {
    fn new(first_startup: TestSupervisorStartup) -> Self {
        Self {
            first_startup,
            launches: Arc::new(AtomicUsize::new(0)),
            live_processes: Arc::new(AtomicUsize::new(0)),
            last_process_id: Arc::new(AtomicUsize::new(0)),
        }
    }

    async fn wait_for_live_processes(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.live_processes.load(Ordering::Acquire) != expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "expected {expected} live worker processes, observed {}",
                self.live_processes.load(Ordering::Acquire)
            )
        });
    }

    fn last_process_id(&self) -> u32 {
        u32::try_from(self.last_process_id.load(Ordering::Acquire))
            .expect("recorded worker PID should fit u32")
    }
}

impl WorkerLauncher for RecoveryLauncher {
    fn containment_status(&self) -> crate::sandbox::worker::WorkerContainmentStatus {
        TestWorkerLauncher::scripted_internal_worker(0).containment_status()
    }

    fn launch(&self) -> Result<WorkerProcess, WorkerLaunchError> {
        let startup = if self.launches.fetch_add(1, Ordering::AcqRel) == 0 {
            self.first_startup
        } else {
            TestSupervisorStartup::Healthy
        };
        let mut process =
            TestWorkerLauncher::scripted_internal_worker_with_startup(0, startup).launch()?;
        self.last_process_id
            .store(process.id() as usize, Ordering::Release);
        process.observe_reap_for_test(self.live_processes.clone());
        Ok(process)
    }
}

const LAUNCH_FIXTURE_GUARD: Duration = Duration::from_secs(15);
const LAUNCH_TEST_WATCHDOG: Duration = Duration::from_secs(60);

#[derive(Default)]
struct LaunchGateState {
    released: bool,
    closing: bool,
    rescued: bool,
    active: usize,
}

#[derive(Default)]
struct LaunchTrace {
    gate: Mutex<LaunchGateState>,
    wake: Condvar,
    entered: tokio::sync::Notify,
    launches: AtomicUsize,
    completed: AtomicUsize,
    created: AtomicUsize,
    live_processes: Arc<AtomicUsize>,
    max_live_processes: AtomicUsize,
}

struct ActiveTestLaunch(Arc<LaunchTrace>);

impl Drop for ActiveTestLaunch {
    fn drop(&mut self) {
        self.0.completed.fetch_add(1, Ordering::Release);
        self.0.gate.lock().unwrap().active -= 1;
    }
}

#[derive(Clone)]
struct BlockedFirstLaunchLauncher(Arc<LaunchTrace>);

impl WorkerLauncher for BlockedFirstLaunchLauncher {
    fn containment_status(&self) -> crate::sandbox::worker::WorkerContainmentStatus {
        TestWorkerLauncher::scripted_internal_worker(0).containment_status()
    }

    fn launch(&self) -> Result<WorkerProcess, WorkerLaunchError> {
        let first = {
            let mut state = self.0.gate.lock().unwrap();
            if state.closing {
                return Err(WorkerLaunchError::MissingPipe {
                    pipe: "closed test fixture",
                });
            }
            state.active += 1;
            self.0.launches.fetch_add(1, Ordering::AcqRel) == 0
        };
        let _active = ActiveTestLaunch(self.0.clone());
        if first {
            let state = self.0.gate.lock().unwrap();
            self.0.entered.notify_one();
            let (mut state, _) = self
                .0
                .wake
                .wait_timeout_while(state, LAUNCH_FIXTURE_GUARD, |state| !state.released)
                .unwrap();
            if !state.released {
                state.rescued = true;
                return Err(WorkerLaunchError::MissingPipe {
                    pipe: "test launch rescue",
                });
            }
        }
        let mut process = TestWorkerLauncher::scripted_internal_worker(0).launch()?;
        process.observe_reap_for_test(self.0.live_processes.clone());
        self.0.created.fetch_add(1, Ordering::Release);
        self.0.max_live_processes.fetch_max(
            self.0.live_processes.load(Ordering::Acquire),
            Ordering::AcqRel,
        );
        Ok(process)
    }
}

struct LaunchFixtureOwner(Arc<LaunchTrace>);

impl LaunchFixtureOwner {
    fn release(&self) {
        self.0.gate.lock().unwrap().released = true;
        self.0.wake.notify_all();
    }

    fn wait_for_quiescence(&self) -> bool {
        let deadline = Instant::now() + LAUNCH_FIXTURE_GUARD;
        loop {
            if self.0.gate.lock().unwrap().active == 0
                && self.0.live_processes.load(Ordering::Acquire) == 0
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for LaunchFixtureOwner {
    fn drop(&mut self) {
        // Prevent a not-yet-entered detached callback from creating a process
        // after the owner has finished cleaning up.
        self.0.gate.lock().unwrap().closing = true;
        self.release();
        let settled = self.wait_for_quiescence();
        if !std::thread::panicking() {
            assert!(settled, "launch callback or late worker did not settle");
            assert!(
                !self.0.gate.lock().unwrap().rescued,
                "launch needed fixture rescue"
            );
        }
    }
}

struct TreeTerminationFailureLauncher {
    live_processes: Arc<AtomicUsize>,
}

impl WorkerLauncher for TreeTerminationFailureLauncher {
    fn containment_status(&self) -> crate::sandbox::worker::WorkerContainmentStatus {
        TestWorkerLauncher::scripted_internal_worker(0).containment_status()
    }

    fn launch(&self) -> Result<WorkerProcess, WorkerLaunchError> {
        let mut process = TestWorkerLauncher::scripted_internal_worker(0).launch()?;
        process.observe_reap_for_test(self.live_processes.clone());
        process.force_tree_termination_error_for_test();
        Ok(process)
    }
}

fn recovery_supervisor(
    first_startup: TestSupervisorStartup,
    watchdog: Duration,
) -> (Arc<JsWorkerSupervisor>, RecoveryLauncher) {
    let launcher = RecoveryLauncher::new(first_startup);
    let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
        launcher.clone(),
        watchdog,
    ));
    (supervisor, launcher)
}

async fn execute_success(supervisor: &JsWorkerSupervisor) -> StepResult {
    supervisor
        .execute(
            RunStep::new("success".into()),
            RecordingEffects::default(),
            PermCancellation::new(),
        )
        .await
        .expect("worker supervisor must recover on the next invocation")
}

#[cfg(feature = "sandbox")]
#[tokio::test]
async fn worker_runtime_nested_fetch_getter_serializes_monotonic_effects() {
    let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_for_test(
        TestWorkerLauncher::internal_worker_process(),
    ));
    let effects = NestedFetchEffects::default();
    let observed = effects.ordinals.clone();
    let request = RunStep::new(
        r#"
        const options = {};
        Object.defineProperty(options, "method", {
            enumerable: true,
            get() { read_file("nested.txt"); return "GET"; }
        });
        const response = fetch("https://example.com", options);
        JSON.stringify({
            status: typeof response.status,
            text: typeof response.text,
            json: typeof response.json
        });
        "#
        .into(),
    )
    .with_model_grant(GrantId::new(uuid::Uuid::new_v4()).unwrap());

    let result = supervisor
        .execute(request, effects, PermCancellation::new())
        .await
        .expect("nested fetch execution should complete");

    assert_eq!(
        result.outcome,
        StepOutcome::Value(r#"{"status":"number","text":"string","json":"undefined"}"#.into())
    );
    assert_eq!(*observed.lock().unwrap(), vec![0, 1]);
    supervisor.shutdown_for_test().await.unwrap();
}

async fn assert_fault_then_recovery(
    first_startup: TestSupervisorStartup,
    code: &str,
    expected_error: WorkerError,
) {
    let (supervisor, launcher) = recovery_supervisor(first_startup, Duration::from_secs(2));
    let error = supervisor
        .execute(
            RunStep::new(code.into()),
            RecordingEffects::default(),
            PermCancellation::new(),
        )
        .await;
    assert_eq!(error, Err(expected_error));
    let recovered = execute_success(&supervisor).await;
    assert_eq!(recovered.outcome, StepOutcome::Value("success".into()));
    launcher.wait_for_live_processes(1).await;
    supervisor.shutdown_for_test().await.unwrap();
    launcher.wait_for_live_processes(0).await;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchStop {
    Cancel,
    DeadlineAndRepeatedCallers,
    Panic,
}

fn launch_test_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn exercise_blocked_launch(stop: LaunchStop, trace: Arc<LaunchTrace>) {
    // Drop the supervisor (and any retained connection) before the fixture owner.
    let owner = LaunchFixtureOwner(trace.clone());
    let supervisor = JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
        BlockedFirstLaunchLauncher(trace.clone()),
        LAUNCH_TEST_WATCHDOG,
    );
    let first = launch_test_runtime().block_on(async {
        let cancellation = PermCancellation::new();
        let request = supervisor.execute(
            RunStep::new("success".into()),
            RecordingEffects::default(),
            cancellation.clone(),
        );
        tokio::pin!(request);
        tokio::time::timeout(LAUNCH_FIXTURE_GUARD, async {
            tokio::select! {
                biased;
                result = &mut request => panic!("request completed before launch entry: {result:?}"),
                () = trace.entered.notified() => {}
            }
        })
        .await
        .expect("first launch must announce readiness");
        assert_eq!(trace.completed.load(Ordering::Acquire), 0);
        match stop {
            LaunchStop::Cancel => cancellation.cancel(),
            LaunchStop::DeadlineAndRepeatedCallers => {
                tokio::time::pause();
                // Cross both the absolute deadline and its rounded timer tick.
                tokio::time::advance(LAUNCH_TEST_WATCHDOG + Duration::from_secs(1)).await;
            }
            LaunchStop::Panic => panic!("injected while first worker launch is held"),
        }
        tokio::time::timeout(LAUNCH_FIXTURE_GUARD, &mut request)
            .await
            .expect("caller must return while synchronous launch remains held")
    });
    assert_eq!(
        first,
        Err(if stop == LaunchStop::Cancel {
            WorkerError::Cancelled
        } else {
            WorkerError::TimedOut
        })
    );
    assert_eq!(trace.completed.load(Ordering::Acquire), 0);
    assert!(
        supervisor.launch_in_flight_for_test(),
        "timed-out caller released the launch lease"
    );

    if stop == LaunchStop::DeadlineAndRepeatedCallers {
        // A fresh clock prevents the first virtual timeout from making these
        // new callers already expired before they wait behind the launch.
        launch_test_runtime().block_on(async {
            let repeated = futures::future::join_all((0..8).map(|_| {
                supervisor.execute(
                    RunStep::new("success".into()),
                    RecordingEffects::default(),
                    PermCancellation::new(),
                )
            }));
            tokio::pin!(repeated);
            assert!(futures::poll!(&mut repeated).is_pending());
            tokio::time::pause();
            tokio::time::advance(LAUNCH_TEST_WATCHDOG + Duration::from_secs(1)).await;
            let results = tokio::time::timeout(LAUNCH_FIXTURE_GUARD, &mut repeated)
                .await
                .expect("queued callers must respect their own deadlines");
            assert!(
                results
                    .iter()
                    .all(|result| *result == Err(WorkerError::TimedOut))
            );
        });
        assert!(supervisor.launch_in_flight_for_test());
        assert_eq!(trace.launches.load(Ordering::Acquire), 1);
        assert_eq!(trace.completed.load(Ordering::Acquire), 0);
    }

    owner.release();
    assert!(owner.wait_for_quiescence(), "late worker was not reaped");
    assert_eq!(trace.completed.load(Ordering::Acquire), 1);
    assert_eq!(
        trace.created.load(Ordering::Acquire),
        1,
        "late worker was never created"
    );
    assert_eq!(trace.live_processes.load(Ordering::Acquire), 0);
    launch_test_runtime().block_on(async {
        assert_eq!(
            execute_success(&supervisor).await.outcome,
            StepOutcome::Value("success".into())
        );
        supervisor.shutdown_for_test().await.unwrap();
    });
    assert_eq!(trace.launches.load(Ordering::Acquire), 2);
    assert_eq!(trace.max_live_processes.load(Ordering::Acquire), 1);
}

#[test]
fn worker_supervisor_blocked_launch_is_owned_through_stop_repeated_calls_and_panic() {
    for stop in [
        LaunchStop::Cancel,
        LaunchStop::DeadlineAndRepeatedCallers,
        LaunchStop::Panic,
    ] {
        let trace = Arc::new(LaunchTrace::default());
        let observed = trace.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            exercise_blocked_launch(stop, observed);
        }));
        match result {
            Ok(()) => assert_ne!(stop, LaunchStop::Panic),
            Err(error) => {
                if stop != LaunchStop::Panic {
                    std::panic::resume_unwind(error);
                }
                assert_eq!(
                    error.downcast_ref::<&str>(),
                    Some(&"injected while first worker launch is held")
                );
            }
        }
        let state = trace.gate.lock().unwrap();
        assert!(!state.rescued, "{stop:?}: launch required rescue");
        assert_eq!(
            state.active, 0,
            "{stop:?}: launch callback survived teardown"
        );
        assert_eq!(
            trace.live_processes.load(Ordering::Acquire),
            0,
            "{stop:?}: worker survived teardown"
        );
        assert_eq!(
            trace.launches.load(Ordering::Acquire),
            trace.completed.load(Ordering::Acquire)
        );
        assert_eq!(
            trace.created.load(Ordering::Acquire),
            if stop == LaunchStop::Panic { 1 } else { 2 }
        );
    }
}

#[tokio::test]
async fn worker_supervisor_shutdown_propagates_tree_failure_after_root_exit() {
    let live_processes = Arc::new(AtomicUsize::new(0));
    let supervisor = JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
        TreeTerminationFailureLauncher {
            live_processes: live_processes.clone(),
        },
        Duration::from_secs(2),
    );
    assert_eq!(
        execute_success(&supervisor).await.outcome,
        StepOutcome::Value("success".into())
    );

    assert_eq!(
        supervisor.shutdown_for_test().await,
        Err(WorkerError::Transport)
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while live_processes.load(Ordering::Acquire) != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("root process was not reaped after tree-termination failure");
}

#[tokio::test]
async fn worker_supervisor_recovery_startup_exit_malformed_ready_and_pure_crash() {
    assert_fault_then_recovery(
        TestSupervisorStartup::ExitBeforeReady,
        "success",
        WorkerError::Transport,
    )
    .await;
    assert_fault_then_recovery(
        TestSupervisorStartup::MalformedReady,
        "success",
        WorkerError::Transport,
    )
    .await;
    assert_fault_then_recovery(
        TestSupervisorStartup::Healthy,
        "crash",
        WorkerError::Transport,
    )
    .await;
}

#[tokio::test]
async fn worker_supervisor_finalizes_only_after_an_authenticated_ready() {
    let healthy_finalized = Arc::new(AtomicUsize::new(0));
    let healthy = JsWorkerSupervisor::with_launcher_for_test(ReadyFinalizationLauncher {
        startup: TestSupervisorStartup::Healthy,
        finalized: healthy_finalized.clone(),
    });
    assert_eq!(
        execute_success(&healthy).await.outcome,
        StepOutcome::Value("success".into())
    );
    assert_eq!(healthy_finalized.load(Ordering::Acquire), 1);
    healthy.shutdown_for_test().await.unwrap();

    let forged_finalized = Arc::new(AtomicUsize::new(0));
    let forged = JsWorkerSupervisor::with_launcher_for_test(ReadyFinalizationLauncher {
        startup: TestSupervisorStartup::ChallengeMismatch,
        finalized: forged_finalized.clone(),
    });
    assert_eq!(
        forged
            .execute(
                RunStep::new("success".into()),
                RecordingEffects::default(),
                PermCancellation::new(),
            )
            .await,
        Err(WorkerError::Protocol)
    );
    assert_eq!(forged_finalized.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn worker_supervisor_finalization_failure_is_launch_terminal_and_reaps() {
    let live_processes = Arc::new(AtomicUsize::new(0));
    let parent_writes = Arc::new(AtomicUsize::new(0));
    let supervisor = JsWorkerSupervisor::with_launcher_for_test(FailingReadyFinalizationLauncher {
        live_processes: live_processes.clone(),
        parent_writes: parent_writes.clone(),
    });

    assert_eq!(
        supervisor
            .execute(
                RunStep::new("must-not-run".into()),
                RecordingEffects::default(),
                PermCancellation::new(),
            )
            .await,
        Err(WorkerError::Launch)
    );
    assert_eq!(
        parent_writes.load(Ordering::Acquire),
        1,
        "only ParentHello may be written before finalization succeeds"
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while live_processes.load(Ordering::Acquire) != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("finalization failure did not reap the worker tree");
}

#[tokio::test]
async fn worker_supervisor_recovery_crash_while_effect_pending_cancels_handler() {
    let (supervisor, launcher) =
        recovery_supervisor(TestSupervisorStartup::Healthy, Duration::from_secs(2));
    let gated = GatedEffects::new();
    let task_supervisor = supervisor.clone();
    let task_effects = gated.clone();
    let task = tokio::spawn(async move {
        task_supervisor
            .execute(
                RunStep::new("crash-pending-effect".into()),
                task_effects,
                PermCancellation::new(),
            )
            .await
    });
    gated.wait_started().await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("worker crash waited for the whole-call watchdog")
            .unwrap(),
        Err(WorkerError::Transport)
    );
    assert!(gated.dropped.load(Ordering::Acquire));
    assert!(
        gated
            .cancellation
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_cancelled()
    );
    assert_eq!(
        execute_success(&supervisor).await.outcome,
        StepOutcome::Value("success".into())
    );
    launcher.wait_for_live_processes(1).await;
    supervisor.shutdown_for_test().await.unwrap();
    launcher.wait_for_live_processes(0).await;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InvocationStop {
    PermissionDeadline,
    ReadDeadline,
    CallerDrop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InterruptionFault {
    Pending,
    Cancelling,
    Dropped,
    Recovered,
}

fn exercise_invocation_interruption(stop: InvocationStop, fault: Option<InterruptionFault>) {
    use crate::extras::js::supervisor::WORKER_READ_OBSERVER;

    let trace = Arc::new(LaunchTrace::default());
    let launch = LaunchFixtureOwner(trace.clone());
    launch.release();
    let supervisor = JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
        BlockedFirstLaunchLauncher(trace.clone()),
        LAUNCH_TEST_WATCHDOG,
    );
    let gated = GatedEffects::new();
    let inject = |at| {
        if fault == Some(at) {
            std::panic::panic_any(at);
        }
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        launch_test_runtime().block_on(async {
            let cancellation = PermCancellation::new();
            let _permission_prompt = (stop == InvocationStop::PermissionDeadline)
                .then(|| cancellation.begin_permission_prompt());
            let (observed, mut reads) = tokio::sync::mpsc::unbounded_channel();
            // Own the allocation, so drop(request) drops the actual invocation,
            // not just a reference to a stack-pinned future.
            let mut request = Box::pin(WORKER_READ_OBSERVER.scope(
                observed,
                supervisor.execute(
                    RunStep::new(if stop == InvocationStop::ReadDeadline {
                        "deadline".into()
                    } else {
                        "effect-pending".into()
                    }),
                    gated.clone(),
                    cancellation,
                ),
            ));
            if stop == InvocationStop::ReadDeadline {
                tokio::time::timeout(LAUNCH_FIXTURE_GUARD, async {
                    tokio::select! {
                        result = &mut request => panic!("invocation settled before its protocol read: {result:?}"),
                        read = reads.recv() => read.expect("protocol read observer dropped"),
                    }
                }).await.expect("invocation never entered its protocol read");
                assert!(gated.ordinals.lock().unwrap().is_empty());
            } else {
                tokio::select! {
                    result = &mut request => panic!("invocation settled before its pending effect: {result:?}"),
                    () = gated.wait_started() => {}
                }
                assert_eq!(*gated.ordinals.lock().unwrap(), vec![0]);
                assert!(!gated.cancellation.lock().unwrap().as_ref().unwrap().is_cancelled());
            }
            assert_eq!(trace.live_processes.load(Ordering::Acquire), 1);
            inject(InterruptionFault::Pending);

            if stop == InvocationStop::CallerDrop {
                drop(request);
                assert!(gated.dropped.load(Ordering::Acquire), "caller drop retained the effect future");
                assert!(gated.cancellation.lock().unwrap().as_ref().unwrap().is_cancelled(), "caller drop did not cancel the effect");
                inject(InterruptionFault::Dropped);
            } else {
                // Cross the actual timer only after native handshake/write and
                // read/effect readiness. Native settlement uses a running clock.
                tokio::time::pause();
                tokio::time::advance(LAUNCH_TEST_WATCHDOG + Duration::from_secs(1)).await;
                let polled = futures::poll!(&mut request);
                tokio::time::resume();
                if stop == InvocationStop::PermissionDeadline {
                    assert!(polled.is_pending(), "deadline must first drain the held effect");
                    assert!(gated.cancellation.lock().unwrap().as_ref().unwrap().is_cancelled(), "deadline did not cancel the pending effect");
                    assert!(!gated.dropped.load(Ordering::Acquire), "effect was dropped before its cancellation drain");
                    inject(InterruptionFault::Cancelling);
                    gated.release();
                }
                let result = match polled {
                    std::task::Poll::Ready(result) => result,
                    std::task::Poll::Pending => tokio::time::timeout(LAUNCH_FIXTURE_GUARD, request)
                        .await.expect("invocation escaped its deadline"),
                };
                assert_eq!(result, Err(if stop == InvocationStop::PermissionDeadline {
                    WorkerError::PermissionPromptTimedOut
                } else {
                    WorkerError::TimedOut
                }));
                assert!(!gated.dropped.load(Ordering::Acquire));
            }
        });
        assert!(
            launch.wait_for_quiescence(),
            "interrupted worker was not reaped before recovery"
        );
        // A fresh clock gives native recovery its own whole-call deadline.
        launch_test_runtime().block_on(async {
            let recovered =
                tokio::time::timeout(LAUNCH_FIXTURE_GUARD, execute_success(&supervisor))
                    .await
                    .expect("interrupted caller retained transport ownership");
            assert_eq!(recovered.outcome, StepOutcome::Value("success".into()));
            assert_eq!(trace.launches.load(Ordering::Acquire), 2);
            assert_eq!(trace.live_processes.load(Ordering::Acquire), 1);
            assert_eq!(trace.max_live_processes.load(Ordering::Acquire), 1);
            inject(InterruptionFault::Recovered);
        });
    }));
    gated.release();
    let shutdown = launch_test_runtime().block_on(supervisor.shutdown());
    let quiescent = launch.wait_for_quiescence();
    shutdown.unwrap();
    assert!(
        quiescent,
        "interruption fixture left a launch or worker live"
    );
    assert!(!trace.gate.lock().unwrap().rescued);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn worker_supervisor_attributes_deadline_to_pending_permission_prompt() {
    exercise_invocation_interruption(InvocationStop::PermissionDeadline, None);
}

#[test]
fn worker_supervisor_interruption_fixture_settles_after_failures() {
    // Shared effect startup, cancellation drain, and recovered-worker cleanup
    // are covered once. Add only the distinct blocked-read and explicit-drop exits.
    for (stop, fault) in [
        (
            InvocationStop::PermissionDeadline,
            InterruptionFault::Pending,
        ),
        (
            InvocationStop::PermissionDeadline,
            InterruptionFault::Cancelling,
        ),
        (
            InvocationStop::PermissionDeadline,
            InterruptionFault::Recovered,
        ),
        (InvocationStop::ReadDeadline, InterruptionFault::Pending),
        (InvocationStop::CallerDrop, InterruptionFault::Dropped),
    ] {
        let result =
            std::panic::catch_unwind(|| exercise_invocation_interruption(stop, Some(fault)))
                .expect_err("injected interruption failure must propagate after cleanup");
        assert_eq!(result.downcast_ref::<InterruptionFault>(), Some(&fault));
    }
}

#[test]
fn worker_supervisor_protocol_read_deadline_reaps_and_recovers() {
    exercise_invocation_interruption(InvocationStop::ReadDeadline, None);
}

#[cfg(unix)]
fn stale_descendant_witness_path(root_pid: u32) -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    let temporary_directory = std::path::Path::new("/private/tmp");
    #[cfg(not(target_os = "macos"))]
    let temporary_directory = std::path::Path::new("/tmp");
    temporary_directory.join(format!("mini-agent-a10-descendant-{root_pid}"))
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn process_exists(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal zero performs an existence/permission check and does not mutate the target.
    (unsafe { libc::kill(pid, 0) == 0 })
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
async fn wait_for_process_exit(pid: u32) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while process_exists(pid) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("stale protocol-pipe descendant survived bounded tree teardown");
}

#[cfg(unix)]
#[tokio::test]
async fn worker_supervisor_reaps_root_exited_descendant_holding_protocol_pipe() {
    let (supervisor, launcher) =
        recovery_supervisor(TestSupervisorStartup::Healthy, Duration::from_secs(2));
    let stale = tokio::time::timeout(
        Duration::from_secs(2),
        supervisor.execute(
            RunStep::new("stale-response".into()),
            RecordingEffects::default(),
            PermCancellation::new(),
        ),
    )
    .await
    .expect("root-exited descendant kept recovery blocked");
    assert_eq!(stale, Err(WorkerError::Transport));

    let root_pid = launcher.last_process_id();
    let witness = stale_descendant_witness_path(root_pid);
    let descendant_pid: u32 = std::fs::read_to_string(&witness)
        .expect("descendant did not publish its PID before the root exited")
        .parse()
        .expect("descendant PID witness was invalid");
    assert_ne!(descendant_pid, root_pid);
    launcher.wait_for_live_processes(0).await;
    wait_for_process_exit(descendant_pid).await;
    let _ = std::fs::remove_file(&witness);

    assert_eq!(
        execute_success(&supervisor).await.outcome,
        StepOutcome::Value("success".into())
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        execute_success(&supervisor).await.outcome,
        StepOutcome::Value("success".into()),
        "a stale descendant poisoned the recovered connection"
    );
    supervisor.shutdown_for_test().await.unwrap();
    launcher.wait_for_live_processes(0).await;
}

#[tokio::test]
async fn worker_supervisor_recovery_clean_shutdown_reaps_and_restarts() {
    let (supervisor, launcher) =
        recovery_supervisor(TestSupervisorStartup::Healthy, Duration::from_secs(2));
    assert_eq!(
        execute_success(&supervisor).await.outcome,
        StepOutcome::Value("success".into())
    );
    let generation = supervisor.generation_for_test().await.unwrap();
    supervisor.shutdown_for_test().await.unwrap();
    launcher.wait_for_live_processes(0).await;

    assert_eq!(
        execute_success(&supervisor).await.outcome,
        StepOutcome::Value("success".into())
    );
    assert!(supervisor.generation_for_test().await.unwrap() > generation);
    supervisor.shutdown_for_test().await.unwrap();
    launcher.wait_for_live_processes(0).await;
}

fn held_out_verification() -> VerifyArtifact {
    #[cfg(feature = "skills")]
    let artifact = crate::extras::js::skills::SkillArtifact::new(
        "function answer() { return 42; }".into(),
        "supervisor verification fixture".into(),
        vec![],
        vec![crate::extras::js::skills::SkillExport {
            name: "answer".into(),
            signature: "answer()".into(),
        }],
        vec!["true".into()],
        crate::extras::js::skills::CapabilityManifest::pure(),
    )
    .unwrap();
    #[cfg(not(feature = "skills"))]
    let artifact = ArtifactInput {
        artifact_id: "supervisor-artifact".into(),
        source: "exports.answer = () => 42".into(),
        exports: vec!["answer".into()],
        tests: vec!["true".into()],
    };
    VerifyArtifact {
        artifact,
        cases: vec![VerificationCase {
            case_id: "held-out".into(),
            script: "true".into(),
            #[cfg(feature = "skills")]
            kind: crate::extras::js::protocol::VerificationCaseKind::HeldOut {
                expected: crate::extras::js::protocol::VerificationExpectedValue::Boolean(true),
                fake_files: Default::default(),
                fake_spawns: Default::default(),
                fake_fetches: Default::default(),
            },
        }],
    }
}

struct VerificationCaller {
    cancellation: PermCancellation,
    result: tokio::sync::oneshot::Receiver<Result<VerificationResult, WorkerError>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct SchedulerFixture {
    supervisor: Arc<JsWorkerSupervisor>,
    launch: LaunchFixtureOwner,
    callers: Vec<VerificationCaller>,
    effects: GatedEffects,
    interactive: Option<tokio::task::JoinHandle<Result<StepResult, WorkerError>>>,
}

impl SchedulerFixture {
    fn start_verification(&mut self) -> usize {
        let supervisor = self.supervisor.clone();
        let cancellation = PermCancellation::new();
        let caller_cancellation = cancellation.clone();
        let (send, result) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            let result = supervisor
                .verify_blocking_cancellable(held_out_verification(), caller_cancellation);
            let _ = send.send(result);
        });
        self.callers.push(VerificationCaller {
            cancellation,
            result,
            thread: Some(thread),
        });
        self.callers.len() - 1
    }

    async fn join_verification(&mut self, index: usize) -> Result<VerificationResult, WorkerError> {
        let caller = &mut self.callers[index];
        // Bound readiness without giving up ownership if the test fails here.
        let result = tokio::time::timeout(LAUNCH_FIXTURE_GUARD, &mut caller.result)
            .await
            .expect("verification caller did not finish");
        caller.thread.take().unwrap().join().unwrap();
        result.expect("verification caller dropped its result")
    }

    fn start_interactive(&mut self) {
        let supervisor = self.supervisor.clone();
        let effects = self.effects.clone();
        assert!(self.interactive.is_none());
        self.interactive = Some(tokio::spawn(async move {
            supervisor
                .execute(
                    RunStep::new("effect-pending".into()),
                    effects,
                    PermCancellation::new(),
                )
                .await
        }));
    }

    async fn finish_interactive(&mut self) {
        self.effects.release();
        let result = self.interactive.as_mut().unwrap().await;
        self.interactive.take();
        assert_eq!(
            result.unwrap().unwrap().outcome,
            StepOutcome::Value("success".into())
        );
    }

    async fn settle(&mut self) {
        self.launch.release();
        self.effects.release();
        for caller in &self.callers {
            caller.cancellation.cancel();
        }
        // Drive cancellation on this runtime before synchronously joining callers:
        // a waiting verification may need the interactive transport lease released.
        let interactive_result = if let Some(task) = self.interactive.take() {
            task.abort();
            Some(task.await)
        } else {
            None
        };
        let joins: Vec<_> = self
            .callers
            .iter_mut()
            .filter_map(|caller| caller.thread.take())
            .map(|thread| thread.join())
            .collect();
        let shutdown = self.supervisor.shutdown().await;
        // Check only after every resource has been settled, so one failed caller
        // cannot skip the remaining joins or worker shutdown.
        assert!(
            joins.iter().all(Result::is_ok),
            "verification caller panicked"
        );
        if let Some(Err(error)) = interactive_result {
            assert!(error.is_cancelled(), "interactive task panicked: {error}");
        }
        shutdown.unwrap();
        assert!(self.launch.wait_for_quiescence());
        assert!(!self.launch.0.gate.lock().unwrap().rescued);
    }

    async fn exercise(&mut self, case: SchedulerCase, fault: Option<SchedulerFault>) {
        if matches!(case, SchedulerCase::CancelPriorityWait) {
            self.launch.release();
            self.start_interactive();
            self.effects.wait_started().await;
            inject_scheduler_fault(fault, SchedulerFault::Effect);
            let queued = self.start_verification();
            self.supervisor
                .wait_for_verification_queue_depth_for_test(1)
                .await;
            self.callers[queued].cancellation.cancel();
            assert_eq!(
                self.join_verification(queued).await,
                Err(WorkerError::Cancelled)
            );
            self.supervisor
                .wait_for_verification_queue_depth_for_test(0)
                .await;
            assert!(
                !self.interactive.as_ref().unwrap().is_finished(),
                "cancellation must wake the scheduler without releasing interactive priority"
            );
            self.finish_interactive().await;
            return;
        }

        let first = self.start_verification();
        tokio::time::timeout(LAUNCH_FIXTURE_GUARD, self.launch.0.entered.notified())
            .await
            .expect("verification launch must announce readiness");
        inject_scheduler_fault(fault, SchedulerFault::Launch);
        let capacity = if matches!(case, SchedulerCase::Overflow) {
            self.supervisor.verification_queue_capacity_for_test()
        } else {
            1
        };
        let queued: Vec<_> = (0..capacity).map(|_| self.start_verification()).collect();
        self.supervisor
            .wait_for_verification_queue_depth_for_test(capacity)
            .await;
        inject_scheduler_fault(fault, SchedulerFault::Queue);

        match case {
            SchedulerCase::InteractivePriority => {
                self.start_interactive();
                self.supervisor
                    .wait_for_interactive_waiters_for_test(1)
                    .await;
                self.launch.release();
                assert!(self.join_verification(first).await.unwrap().passed);
                self.effects.wait_started().await;
                inject_scheduler_fault(fault, SchedulerFault::Effect);
                assert_eq!(
                    self.supervisor.verification_queue_depth_for_test(),
                    1,
                    "queued verification bypassed an already-waiting interactive request"
                );
                self.finish_interactive().await;
                assert!(self.join_verification(queued[0]).await.unwrap().passed);
            }
            SchedulerCase::CancelBeforeDequeue | SchedulerCase::Overflow => {
                if matches!(case, SchedulerCase::Overflow) {
                    let overflow = self.start_verification();
                    assert_eq!(
                        self.join_verification(overflow).await,
                        Err(WorkerError::VerificationQueueFull)
                    );
                    assert!(
                        WorkerError::VerificationQueueFull.is_retryable_admission_infrastructure()
                    );
                }
                for &index in &queued {
                    self.callers[index].cancellation.cancel();
                }
                for index in queued {
                    assert_eq!(
                        self.join_verification(index).await,
                        Err(WorkerError::Cancelled)
                    );
                }
                self.launch.release();
                assert!(self.join_verification(first).await.unwrap().passed);
                assert_eq!(self.supervisor.generation_for_test().await, Some(1));
            }
            SchedulerCase::CancelPriorityWait => unreachable!(),
        }
        assert_eq!(self.launch.0.launches.load(Ordering::Acquire), 1);
        assert_eq!(self.launch.0.max_live_processes.load(Ordering::Acquire), 1);
    }
}

#[derive(Clone, Copy)]
enum SchedulerCase {
    InteractivePriority,
    CancelBeforeDequeue,
    CancelPriorityWait,
    Overflow,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SchedulerFault {
    Launch,
    Queue,
    Effect,
}

fn inject_scheduler_fault(selected: Option<SchedulerFault>, point: SchedulerFault) {
    if selected == Some(point) {
        panic!("injected while scheduler fixture holds work");
    }
}

fn run_scheduler_case(case: SchedulerCase, fault: Option<SchedulerFault>) {
    let runtime = launch_test_runtime();
    let trace = Arc::new(LaunchTrace::default());
    let mut fixture = SchedulerFixture {
        supervisor: Arc::new(JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
            BlockedFirstLaunchLauncher(trace.clone()),
            LAUNCH_TEST_WATCHDOG,
        )),
        launch: LaunchFixtureOwner(trace.clone()),
        callers: Vec::new(),
        effects: GatedEffects::new(),
        interactive: None,
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async {
            tokio::time::timeout(LAUNCH_FIXTURE_GUARD, fixture.exercise(case, fault))
                .await
                .expect("scheduler scenario exceeded hang guard");
        });
    }));
    runtime.block_on(fixture.settle());
    assert!(fixture.callers.iter().all(|caller| caller.thread.is_none()));
    assert!(fixture.interactive.is_none());
    drop(fixture);
    assert_eq!(trace.gate.lock().unwrap().active, 0);
    assert_eq!(trace.live_processes.load(Ordering::Acquire), 0);
    assert_eq!(
        trace.launches.load(Ordering::Acquire),
        trace.completed.load(Ordering::Acquire)
    );
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    assert!(fault.is_none(), "injected fault point was not reached");
}

#[test]
fn verification_scheduler_prioritizes_interactive_between_atomic_requests() {
    run_scheduler_case(SchedulerCase::InteractivePriority, None);
}

#[test]
fn verification_scheduler_cancels_before_dequeue_without_recycling_worker() {
    run_scheduler_case(SchedulerCase::CancelBeforeDequeue, None);
}

#[test]
fn verification_scheduler_cancellation_wakes_priority_wait_while_interactive_stays_active() {
    run_scheduler_case(SchedulerCase::CancelPriorityWait, None);
}

#[test]
fn verification_scheduler_bounds_queue_and_reports_retryable_overflow() {
    run_scheduler_case(SchedulerCase::Overflow, None);
}

#[test]
fn verification_scheduler_failure_settles_launch_queue_and_effect_work() {
    for (case, fault) in [
        (SchedulerCase::InteractivePriority, SchedulerFault::Launch),
        (SchedulerCase::Overflow, SchedulerFault::Queue),
        (SchedulerCase::InteractivePriority, SchedulerFault::Effect),
        (SchedulerCase::CancelPriorityWait, SchedulerFault::Effect),
    ] {
        let panic = std::panic::catch_unwind(|| run_scheduler_case(case, Some(fault)))
            .expect_err("scheduler fault must propagate after cleanup");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"injected while scheduler fixture holds work")
        );
    }
}

#[test]
fn verification_scheduler_queue_close_fails_closed_as_retryable_infrastructure() {
    let supervisor =
        JsWorkerSupervisor::with_launcher_for_test(TestWorkerLauncher::scripted_internal_worker(0));
    supervisor.close_verification_queue_for_test();
    let error = supervisor
        .verify_blocking(held_out_verification())
        .expect_err("closed verification queue must fail closed");
    assert_eq!(error, WorkerError::VerificationQueueClosed);
    assert!(error.is_retryable_admission_infrastructure());
}

#[test]
fn verification_scheduler_worker_fault_does_not_close_the_single_queue() {
    let launcher = RecoveryLauncher::new(TestSupervisorStartup::ExitBeforeReady);
    let supervisor = JsWorkerSupervisor::with_launcher_for_test(launcher.clone());
    assert_eq!(
        supervisor.verify_blocking(held_out_verification()),
        Err(WorkerError::Transport)
    );
    assert!(
        supervisor
            .verify_blocking(held_out_verification())
            .unwrap()
            .passed
    );
    assert_eq!(launcher.launches.load(Ordering::Acquire), 2);
    assert!(launcher.live_processes.load(Ordering::Acquire) <= 1);
}

pub(in crate::extras::js) fn verification_with_source(source: &str) -> VerifyArtifact {
    #[cfg(feature = "skills")]
    let artifact = crate::extras::js::skills::SkillArtifact::new(
        source.into(),
        "supervisor verification fault fixture".into(),
        vec![],
        vec![crate::extras::js::skills::SkillExport {
            name: "answer".into(),
            signature: "answer()".into(),
        }],
        vec!["true".into()],
        crate::extras::js::skills::CapabilityManifest::pure(),
    )
    .unwrap();
    #[cfg(not(feature = "skills"))]
    let artifact = ArtifactInput {
        artifact_id: "supervisor-fault-artifact".into(),
        source: source.into(),
        exports: vec!["answer".into()],
        tests: vec!["true".into()],
    };
    VerifyArtifact {
        artifact,
        cases: vec![VerificationCase {
            case_id: "held-out-fault".into(),
            script: "true".into(),
            #[cfg(feature = "skills")]
            kind: crate::extras::js::protocol::VerificationCaseKind::HeldOut {
                expected: crate::extras::js::protocol::VerificationExpectedValue::Boolean(true),
                fake_files: Default::default(),
                fake_spawns: Default::default(),
                fake_fetches: Default::default(),
            },
        }],
    }
}

fn verification_with_embedded_test(script: &str) -> VerifyArtifact {
    #[cfg(feature = "skills")]
    let artifact = crate::extras::js::skills::SkillArtifact::new(
        "function answer() { return true; }".into(),
        "supervisor verification resource fixture".into(),
        vec![],
        vec![crate::extras::js::skills::SkillExport {
            name: "answer".into(),
            signature: "answer()".into(),
        }],
        vec![script.into()],
        crate::extras::js::skills::CapabilityManifest::pure(),
    )
    .unwrap();
    #[cfg(not(feature = "skills"))]
    let artifact = ArtifactInput {
        artifact_id: "supervisor-resource-artifact".into(),
        source: "exports.answer = () => true".into(),
        exports: vec!["answer".into()],
        tests: vec![script.into()],
    };
    VerifyArtifact {
        artifact,
        #[cfg(feature = "skills")]
        cases: vec![VerificationCase {
            case_id: "embedded-resource".into(),
            script: script.into(),
            kind: crate::extras::js::protocol::VerificationCaseKind::Embedded,
        }],
        #[cfg(not(feature = "skills"))]
        cases: vec![],
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransportFault {
    FirstEffect,
    SecondAdmission,
    Released,
}

async fn exercise_transport_exclusion(fault: Option<TransportFault>) {
    use crate::extras::js::supervisor::TRANSPORT_LOCK_OBSERVER;
    use futures::FutureExt;

    let trace = Arc::new(LaunchTrace::default());
    let launch = LaunchFixtureOwner(trace.clone());
    launch.release();
    let supervisor = JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
        BlockedFirstLaunchLauncher(trace.clone()),
        LAUNCH_TEST_WATCHDOG,
    );
    let gated = GatedEffects::new();
    let inject = |at| {
        if fault == Some(at) {
            std::panic::panic_any(at);
        }
    };
    let result = std::panic::AssertUnwindSafe(async {
        // These futures stay owned by the scenario. Unwinding drops both before
        // shutdown, so no detached invocation can retain the effect/transport.
        let first = supervisor.execute(
            RunStep::new("two-effects".into()),
            gated.clone(),
            PermCancellation::new(),
        );
        tokio::pin!(first);
        tokio::select! {
            result = &mut first => panic!("first invocation settled before its effect gate: {result:?}"),
            () = gated.wait_started() => {}
        }
        inject(TransportFault::FirstEffect);

        let (observed, mut admission) = tokio::sync::mpsc::unbounded_channel();
        let second = TRANSPORT_LOCK_OBSERVER.scope(
            observed,
            supervisor.execute(
                RunStep::new("success".into()),
                RecordingEffects::default(),
                PermCancellation::new(),
            ),
        );
        tokio::pin!(second);
        let acquired = tokio::time::timeout(LAUNCH_FIXTURE_GUARD, async {
            tokio::select! {
                biased;
                acquired = admission.recv() => acquired.expect("transport observation was dropped"),
                result = &mut second => panic!("second invocation settled while the first effect was held: {result:?}"),
            }
        })
        .await
        .expect("second caller did not poll transport admission");
        assert!(
            !acquired,
            "second caller acquired transport while the first effect was held"
        );
        assert_eq!(*gated.ordinals.lock().unwrap(), vec![0]);
        assert_eq!(trace.launches.load(Ordering::Acquire), 1);
        assert_eq!(trace.live_processes.load(Ordering::Acquire), 1);
        inject(TransportFault::SecondAdmission);

        gated.release();
        inject(TransportFault::Released);
        let (first, second) = tokio::time::timeout(LAUNCH_FIXTURE_GUARD, async {
            tokio::join!(first, second)
        })
        .await
        .expect("released transport callers did not settle");
        assert_eq!(first.unwrap().outcome, StepOutcome::Value("effects-complete".into()));
        assert_eq!(second.unwrap().outcome, StepOutcome::Value("success".into()));
        assert_eq!(*gated.ordinals.lock().unwrap(), vec![0, 1]);
        assert_eq!(trace.launches.load(Ordering::Acquire), 1, "callers must reuse one worker");
        assert_eq!(trace.max_live_processes.load(Ordering::Acquire), 1);
        assert_eq!(trace.live_processes.load(Ordering::Acquire), 1);
    })
    .catch_unwind()
    .await;
    gated.release();
    let shutdown = supervisor.shutdown().await;
    let quiescent = launch.wait_for_quiescence();
    shutdown.unwrap();
    assert!(quiescent, "transport fixture left a launch or worker live");
    assert!(!trace.gate.lock().unwrap().rescued);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn worker_supervisor_transport_serializes_concurrent_callers_and_orders_effects() {
    exercise_transport_exclusion(None).await;
}

#[tokio::test]
async fn worker_supervisor_transport_fixture_settles_after_failures() {
    use futures::FutureExt;
    for fault in [
        TransportFault::FirstEffect,
        TransportFault::SecondAdmission,
        TransportFault::Released,
    ] {
        let result = std::panic::AssertUnwindSafe(exercise_transport_exclusion(Some(fault)))
            .catch_unwind()
            .await
            .expect_err("injected transport failure must propagate after cleanup");
        assert_eq!(result.downcast_ref::<TransportFault>(), Some(&fault));
    }
}

#[test]
fn worker_supervisor_transport_run_and_verify_reuse_one_serialized_connection() {
    let supervisor = scripted_supervisor(0);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let first = runtime
        .block_on(supervisor.execute(
            RunStep::new("success".into()),
            RecordingEffects::default(),
            PermCancellation::new(),
        ))
        .unwrap();
    assert_eq!(first.outcome, StepOutcome::Value("success".into()));
    let first_generation = runtime.block_on(supervisor.generation_for_test()).unwrap();
    drop(runtime);

    let verification = supervisor.verify_blocking(held_out_verification()).unwrap();
    assert!(verification.passed);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert_eq!(
        runtime.block_on(supervisor.generation_for_test()),
        Some(first_generation)
    );
}

#[test]
fn worker_supervisor_real_verification_resource_terminal_recycles_generation() {
    for (mut request, role) in [
        (
            verification_with_source("while (true) {} function answer() { return true; }"),
            ScriptRole::SkillSource,
        ),
        (
            verification_with_embedded_test("while (true) {}"),
            ScriptRole::EmbeddedTest,
        ),
        (
            verification_with_source(
                "try { new ArrayBuffer(128 * 1024 * 1024); } catch (_) {} function answer() { return true; }",
            ),
            ScriptRole::SkillSource,
        ),
        (
            verification_with_embedded_test(
                "try { new ArrayBuffer(128 * 1024 * 1024); } catch (_) {} true",
            ),
            ScriptRole::EmbeddedTest,
        ),
        (
            {
                let mut request = verification_with_source("function answer() { while (true) {} }");
                request.cases[0].script = "answer()".into();
                request
            },
            ScriptRole::HeldOutTest,
        ),
    ] {
        let supervisor = JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
            TestWorkerLauncher::internal_worker_process_with_limits(50, 10_000),
            Duration::from_secs(2),
        );
        request.cases.push(VerificationCase {
            case_id: "must-not-run-after-resource-failure".into(),
            script: "true".into(),
            #[cfg(feature = "skills")]
            kind: crate::extras::js::protocol::VerificationCaseKind::Embedded,
        });
        let result = supervisor.verify_blocking(request).unwrap();
        let first_resource_failure = result
            .cases
            .iter()
            .position(|case| {
                case.diagnostic
                    .as_ref()
                    .is_some_and(|diagnostic| diagnostic.class == DiagnosticClass::ResourceLimit)
            })
            .expect("resource failure must be reported");
        assert!(
            result.cases[first_resource_failure..]
                .iter()
                .all(|case| !case.passed)
        );
        assert!(!result.passed);
        assert!(
            result.cases.iter().any(|case| {
                case.diagnostic.as_ref().is_some_and(|diagnostic| {
                    diagnostic.class == DiagnosticClass::ResourceLimit
                        && diagnostic.script_role == role
                })
            }),
            "unexpected verification result: {result:?}"
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(runtime.block_on(supervisor.generation_for_test()), None);
        let next = runtime
            .block_on(supervisor.execute(
                RunStep::new("42".into()),
                RecordingEffects::default(),
                PermCancellation::new(),
            ))
            .unwrap();
        assert_eq!(next.outcome, StepOutcome::Value("42".into()));
        assert_eq!(runtime.block_on(supervisor.generation_for_test()), Some(2));
        runtime.block_on(supervisor.shutdown_for_test()).unwrap();
    }
}

#[tokio::test]
async fn worker_supervisor_real_stack_limit_recycles_generation() {
    let supervisor = JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
        TestWorkerLauncher::internal_worker_process_with_limits(10_000, 10_000),
        Duration::from_secs(2),
    );
    let result = supervisor
        .execute(
            RunStep::new("function recurse(){ return recurse(); } recurse()".into()),
            RecordingEffects::default(),
            PermCancellation::new(),
        )
        .await
        .unwrap();
    assert_closed_exception(
        &result,
        JsErrorCode::StackLimit,
        DiagnosticClass::ResourceLimit,
        JsExceptionClass::RangeError,
        Some((1, 27)),
    );
    assert_eq!(supervisor.generation_for_test().await, None);

    let next = supervisor
        .execute(
            RunStep::new("42".into()),
            RecordingEffects::default(),
            PermCancellation::new(),
        )
        .await
        .unwrap();
    assert_eq!(next.outcome, StepOutcome::Value("42".into()));
    assert_eq!(supervisor.generation_for_test().await, Some(2));
    supervisor.shutdown_for_test().await.unwrap();
}

#[test]
fn worker_supervisor_verification_internal_terminal_recycles_generation() {
    let supervisor = scripted_supervisor(0);
    let result = supervisor
        .verify_blocking(verification_with_source("__verification_internal__"))
        .unwrap();
    assert!(!result.passed);
    assert!(result.cases.iter().any(|case| {
        case.diagnostic
            .as_ref()
            .is_some_and(|diagnostic| diagnostic.class == DiagnosticClass::Internal)
    }));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert_eq!(runtime.block_on(supervisor.generation_for_test()), None);
    let next = runtime
        .block_on(supervisor.execute(
            RunStep::new("success".into()),
            RecordingEffects::default(),
            PermCancellation::new(),
        ))
        .unwrap();
    assert_eq!(next.outcome, StepOutcome::Value("success".into()));
    assert_eq!(runtime.block_on(supervisor.generation_for_test()), Some(2));
    runtime.block_on(supervisor.shutdown_for_test()).unwrap();
}

#[test]
fn worker_supervisor_verification_binds_case_identity_order_and_aggregate_verdict() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for fault in [
        "wrong-id",
        "duplicate",
        "reordered",
        "false-summary",
        "true-summary",
        "loader",
        "missing",
        "extra",
        "valid",
        "valid-failure",
    ] {
        let supervisor = JsWorkerSupervisor::with_launcher_for_test(
            TestWorkerLauncher::scripted_internal_worker(0),
        );
        let mut request = verification_with_source(&format!("__verification_contract:{fault}"));
        let mut second = request.cases[0].clone();
        second.case_id = "held-out-second".into();
        request.cases.push(second);
        let result = supervisor.verify_blocking(request);
        if fault == "valid" || fault == "valid-failure" {
            let result = result.unwrap();
            let implicit = usize::from(!cfg!(feature = "skills"));
            assert_eq!(result.cases.len(), implicit + 2);
            if implicit == 1 {
                assert_eq!(result.cases[0].case_id, "embedded-0");
            }
            assert_eq!(result.cases[implicit].case_id, "held-out-fault");
            assert_eq!(result.cases[implicit + 1].case_id, "held-out-second");
            assert_eq!(result.passed, fault == "valid");
        } else {
            let error = result.expect_err(fault);
            assert_eq!(error, WorkerError::Protocol, "{fault}");
            #[cfg(feature = "skills")]
            assert!(crate::extras::js::skills::verify::worker_error(error).is_infrastructure());
            assert_eq!(
                runtime.block_on(supervisor.generation_for_test()),
                None,
                "{fault}"
            );
            let recovered = supervisor
                .verify_blocking(verification_with_source("__verification_contract:valid"))
                .unwrap();
            assert!(recovered.passed);
            assert_eq!(
                recovered.cases.len(),
                1 + usize::from(!cfg!(feature = "skills"))
            );
            assert_eq!(runtime.block_on(supervisor.generation_for_test()), Some(2));
        }
        runtime.block_on(supervisor.shutdown_for_test()).unwrap();
    }
}

#[test]
fn worker_supervisor_rejects_verifier_source_positions_and_recovers() {
    let supervisor = scripted_supervisor(0);
    assert_eq!(
        supervisor.verify_blocking(verification_with_source("__verification_positions__")),
        Err(WorkerError::Protocol)
    );

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert_eq!(runtime.block_on(supervisor.generation_for_test()), None);
    let next = runtime
        .block_on(supervisor.execute(
            RunStep::new("success".into()),
            RecordingEffects::default(),
            PermCancellation::new(),
        ))
        .unwrap();
    assert_eq!(next.outcome, StepOutcome::Value("success".into()));
    assert_eq!(runtime.block_on(supervisor.generation_for_test()), Some(2));
    runtime.block_on(supervisor.shutdown_for_test()).unwrap();
}

#[tokio::test]
async fn worker_supervisor_transport_cancellation_marks_started_effect_unknown_and_recovers() {
    let supervisor = scripted_supervisor(0);
    let gated = GatedEffects::new();
    let cancellation = PermCancellation::new();
    let task_supervisor = supervisor.clone();
    let task_effects = gated.clone();
    let task_cancellation = cancellation.clone();
    let task = tokio::spawn(async move {
        task_supervisor
            .execute(
                RunStep::new("effect-pending".into()),
                task_effects,
                task_cancellation,
            )
            .await
    });
    gated.wait_started().await;
    let first_generation = supervisor.active_generation_for_test().await.unwrap();
    cancellation.cancel();
    assert_eq!(task.await.unwrap(), Err(WorkerError::EffectOutcomeUnknown));
    assert!(gated.dropped.load(Ordering::Acquire));
    assert!(
        gated
            .cancellation
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_cancelled()
    );

    let recovered = supervisor
        .execute(
            RunStep::new("success".into()),
            RecordingEffects::default(),
            PermCancellation::new(),
        )
        .await
        .unwrap();
    assert_eq!(recovered.outcome, StepOutcome::Value("success".into()));
    assert!(supervisor.generation_for_test().await.unwrap() > first_generation);
}

#[test]
fn worker_supervisor_transport_dropped_caller_releases_owner_and_cancels_handler() {
    exercise_invocation_interruption(InvocationStop::CallerDrop, None);
}

#[tokio::test]
async fn worker_supervisor_transport_rejects_stale_generation_before_protocol_state() {
    assert_eq!(
        crate::extras::js::supervisor::validate_generation_for_test(2, 1),
        Err(WorkerError::StaleGeneration)
    );
    assert!(crate::extras::js::supervisor::validate_generation_for_test(2, 2).is_ok());

    let supervisor = scripted_supervisor(0);
    let result = supervisor
        .execute(
            RunStep::new("success".into()),
            RecordingEffects::default(),
            PermCancellation::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.outcome, StepOutcome::Value("success".into()));
}

#[test]
fn worker_test_failure_diagnostics_use_closed_classes() {
    use crate::extras::js::protocol::FrameError;
    use crate::extras::js::supervisor::{test_frame_failure_class, test_worker_exit_class};
    use std::io::ErrorKind;
    use std::process::ExitStatus;

    for (error, expected) in [
        (FrameError::Io(ErrorKind::BrokenPipe), "broken_pipe"),
        (FrameError::Io(ErrorKind::UnexpectedEof), "unexpected_eof"),
        (
            FrameError::Io(ErrorKind::ConnectionReset),
            "connection_reset",
        ),
        (FrameError::Io(ErrorKind::Interrupted), "interrupted"),
        (FrameError::Io(ErrorKind::PermissionDenied), "other_io"),
        (
            FrameError::TruncatedHeader { read: usize::MAX },
            "truncated_header",
        ),
        (
            FrameError::TruncatedBody {
                read: usize::MAX,
                expected: usize::MAX,
            },
            "truncated_body",
        ),
        (FrameError::ZeroLength, "zero_length"),
        (
            FrameError::FrameTooLarge {
                length: usize::MAX,
                maximum: usize::MAX,
            },
            "oversized_frame",
        ),
        (FrameError::InvalidJson, "invalid_json"),
        (FrameError::Serialization, "serialization"),
    ] {
        assert_eq!(test_frame_failure_class(&error), expected);
    }
    // Unrecognized worker-selected exit values must not be formatted into logs.
    for (code, expected) in [
        (0, "success"),
        (1, "failure"),
        (2, "other_exit"),
        (255, "other_exit"),
    ] {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus::from_raw(code << 8)
        };
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            ExitStatus::from_raw(code)
        };
        assert_eq!(test_worker_exit_class(status), expected);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        for (signal, expected) in [
            (libc::SIGABRT, "signal_abort"),
            (libc::SIGKILL, "signal_kill"),
            (libc::SIGSEGV, "signal_segv"),
            (libc::SIGBUS, "signal_bus"),
            (libc::SIGXCPU, "native_cpu_limit"),
            (libc::SIGTERM, "other_signal"),
        ] {
            assert_eq!(
                test_worker_exit_class(ExitStatus::from_raw(signal)),
                expected
            );
        }
        assert_eq!(
            test_worker_exit_class(ExitStatus::from_raw((128 + libc::SIGXCPU) << 8)),
            "native_cpu_limit"
        );
    }
}

#[test]
fn worker_exit_reconciliation_never_sleeps_past_its_deadline() {
    assert_eq!(
        crate::extras::js::supervisor::reconciliation_poll_delay_for_test(Duration::from_millis(
            100
        )),
        Duration::from_millis(100)
    );
    assert_eq!(
        crate::extras::js::supervisor::reconciliation_poll_delay_for_test(Duration::ZERO),
        Duration::ZERO
    );
}

#[tokio::test]
async fn worker_supervisor_transport_drains_large_stderr_without_blocking_worker() {
    let supervisor = JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
        TestWorkerLauncher::scripted_internal_worker(256 * 1024),
        Duration::from_secs(5),
    );
    let result = supervisor
        .execute(
            RunStep::new("success".into()),
            RecordingEffects::default(),
            PermCancellation::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.outcome, StepOutcome::Value("success".into()));
    supervisor.shutdown_for_test().await.unwrap();
}

#[test]
fn worker_supervisor_transport_rejects_verify_blocking_inside_tokio_and_keeps_state_authority_free()
{
    let supervisor = scripted_supervisor(0);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let error = runtime.block_on(async { supervisor.verify_blocking(held_out_verification()) });
    assert_eq!(error, Err(WorkerError::BlockingVerifyInAsyncRuntime));

    let source = include_str!("../supervisor.rs");
    let declarations = source
        .split_once("// BEGIN AUTHORITY-FREE SUPERVISOR STATE")
        .unwrap()
        .1
        .split_once("// END AUTHORITY-FREE SUPERVISOR STATE")
        .unwrap()
        .0;
    for forbidden in [
        "Sandbox",
        "AllowConfig",
        "permission",
        "approval",
        "SkillBundle",
        "proposal",
        "audit",
        "GrantId",
        "InvocationEffectHandler",
    ] {
        assert!(
            !declarations.contains(forbidden),
            "supervisor state retained invocation authority: {forbidden}"
        );
    }
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<JsWorkerSupervisor>();
}

// The stale-response case deliberately leaves a descendant for the supervisor to reap.
#[allow(clippy::zombie_processes)]
fn run_scripted_supervisor_worker() -> ! {
    let startup =
        std::env::var("MINI_AGENT_TEST_SUPERVISOR_STARTUP").unwrap_or_else(|_| "healthy".into());
    if startup == "exit-before-ready" {
        std::process::exit(71);
    }
    let stderr_bytes = std::env::var("MINI_AGENT_TEST_SUPERVISOR_STDERR_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if stderr_bytes > 0 {
        let mut stderr = std::io::stderr().lock();
        let chunk = [b'x'; 4096];
        let mut remaining = stderr_bytes;
        while remaining > 0 {
            let length = remaining.min(chunk.len());
            stderr.write_all(&chunk[..length]).unwrap();
            remaining -= length;
        }
        stderr.flush().unwrap();
    }

    let build = BuildIdentity::current();
    let mut protocol = WorkerProtocol::new(build.clone());
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    let hello: ParentWireFrame = read_frame(&mut input).unwrap();
    protocol.on_receive(&hello).unwrap();
    if startup == "malformed-ready" {
        output.write_all(&[0, 0, 0, 1, b'{']).unwrap();
        output.flush().unwrap();
        std::process::exit(72);
    }
    if startup == "build-mismatch" {
        let fault = WireFrame {
            protocol_version: hello.protocol_version,
            build_id: hello.build_id,
            invocation_id: None,
            sequence: 1,
            message: WorkerFrame::ProtocolFault(ProtocolFault {
                code: ProtocolFaultCode::BuildMismatch,
                stage: ProtocolStage::Handshake,
            }),
        };
        write_frame(&mut output, &fault).unwrap();
        output.flush().unwrap();
        std::process::exit(74);
    }
    let ready_payload = if startup == "challenge-mismatch" {
        WorkerReady {
            challenge: test_launch_challenge(),
        }
    } else {
        protocol.ready().unwrap()
    };
    let ready = WireFrame::connection(build.clone(), 1, WorkerFrame::Ready(ready_payload));
    if startup != "challenge-mismatch" {
        protocol.on_send(&ready).unwrap();
    }
    write_frame(&mut output, &ready).unwrap();
    output.flush().unwrap();

    loop {
        let request: ParentWireFrame = read_frame(&mut input).unwrap();
        protocol.on_receive(&request).unwrap();
        match request.message {
            ParentFrame::RunStep(step) => {
                let invocation = request.invocation_id.clone().unwrap();
                match step.code.as_str() {
                    "crash" => std::process::exit(73),
                    "panic" => {
                        std::panic::set_hook(Box::new(|_| {}));
                        let _ = std::panic::catch_unwind(|| panic!("scripted worker panic"));
                        std::process::exit(77);
                    }
                    "os-kill" => std::process::abort(),
                    "abnormal-exit" => std::process::exit(76),
                    #[cfg(unix)]
                    "native-cpu-limit" => exit_with_native_cpu_limit(),
                    "deadline" => std::thread::park_timeout(Duration::from_secs(30)),
                    "malformed-protocol" => {
                        output.write_all(&[0, 0, 0, 1, b'{']).unwrap();
                        output.flush().unwrap();
                        std::thread::park_timeout(Duration::from_secs(30));
                    }
                    "protocol-fault" | "terminal-exit-fault" => {
                        let fault = WireFrame::invocation(
                            build.clone(),
                            invocation.clone(),
                            request.sequence + 1,
                            WorkerFrame::ProtocolFault(
                                crate::extras::js::protocol::ProtocolFault {
                                    code:
                                        crate::extras::js::protocol::ProtocolFaultCode::InvalidState,
                                    stage: crate::extras::js::protocol::ProtocolStage::Invocation,
                                },
                            ),
                        );
                        protocol.on_send(&fault).unwrap();
                        write_frame(&mut output, &fault).unwrap();
                        output.flush().unwrap();
                        if step.code == "terminal-exit-fault" {
                            std::process::exit(74);
                        }
                        std::thread::park_timeout(Duration::from_secs(30));
                    }
                    "stale-response" => {
                        #[cfg(unix)]
                        {
                            use std::process::{Command, Stdio};

                            let witness = stale_descendant_witness_path(std::process::id());
                            let _ = std::fs::remove_file(&witness);
                            let descendant = Command::new("/bin/sleep")
                                .env_clear()
                                .arg("30")
                                .stdin(Stdio::null())
                                .stdout(Stdio::inherit())
                                .stderr(Stdio::null())
                                .spawn()
                                .unwrap();
                            let pending_witness = witness.with_extension("pending");
                            std::fs::write(&pending_witness, descendant.id().to_string()).unwrap();
                            std::fs::rename(pending_witness, witness).unwrap();
                        }
                        std::process::exit(74);
                    }
                    _ => {}
                }
                let mut sequence = request.sequence + 1;
                let effect_count = match step.code.as_str() {
                    "two-effects" => 2,
                    "effect-pending"
                    | "crash-pending-effect"
                    | "outcome-unknown"
                    | "terminal-exit-effect" => 1,
                    _ => 0,
                };
                for ordinal in 0..effect_count {
                    let effect = WireFrame::invocation(
                        build.clone(),
                        invocation.clone(),
                        sequence,
                        WorkerFrame::EffectRequest(Box::new(EffectRequest {
                            effect_ordinal: ordinal,
                            grant_id: GrantId::new(uuid::Uuid::from_u128(1)).unwrap(),
                            advisory: AdvisoryAttribution::default(),
                            operation: EffectOperation::ReadFile {
                                path: "fixture".into(),
                            },
                        })),
                    );
                    protocol.on_send(&effect).unwrap();
                    write_frame(&mut output, &effect).unwrap();
                    output.flush().unwrap();
                    if step.code == "terminal-exit-effect" {
                        std::process::exit(0);
                    }
                    if step.code == "crash-pending-effect" {
                        std::thread::sleep(Duration::from_millis(30));
                        std::process::exit(75);
                    }
                    let response: ParentWireFrame = read_frame(&mut input).unwrap();
                    assert!(matches!(response.message, ParentFrame::EffectResponse(_)));
                    let outcome_unknown = matches!(
                        response.message,
                        ParentFrame::EffectResponse(EffectResponse {
                            result: EffectResult::Error(EffectError {
                                code: EffectErrorCode::OutcomeUnknown,
                            }),
                            ..
                        })
                    );
                    protocol.on_receive(&response).unwrap();
                    sequence = response.sequence + 1;
                    if outcome_unknown {
                        break;
                    }
                }
                let outcome = if step.code == "js-error" || step.code == "outcome-unknown" {
                    StepOutcome::Error(JsErrorCode::Exception)
                } else if let Some(code) = step.code.strip_prefix("js-error-") {
                    StepOutcome::Error(match code {
                        "syntax" => JsErrorCode::Syntax,
                        "exception" => JsErrorCode::Exception,
                        "stack" => JsErrorCode::StackLimit,
                        "jobs" => JsErrorCode::JobLimit,
                        "effects" => JsErrorCode::EffectLimit,
                        "result" => JsErrorCode::InvalidResult,
                        "internal" => JsErrorCode::Internal,
                        _ => std::process::exit(78),
                    })
                } else if step.code == "timeout-step" {
                    StepOutcome::Timeout
                } else if step.code == "oom-step" {
                    StepOutcome::OutOfMemory
                } else {
                    StepOutcome::Value(
                        if step.code == "two-effects" {
                            "effects-complete"
                        } else {
                            "success"
                        }
                        .into(),
                    )
                };
                let mut terminal = WireFrame::invocation(
                    build.clone(),
                    invocation,
                    sequence,
                    WorkerFrame::StepResult(StepResult {
                        outcome,
                        console: Vec::new(),
                        diagnostic: None,
                        #[cfg(feature = "skills")]
                        skill_events: Vec::new(),
                        #[cfg(feature = "skills")]
                        evidence_complete: true,
                    }),
                );
                protocol.on_send(&terminal).unwrap();
                if step.code == "terminal-exit-invalid-sequence" {
                    terminal.sequence += 1;
                }
                write_frame(&mut output, &terminal).unwrap();
                output.flush().unwrap();
                match step.code.as_str() {
                    "terminal-exit-clean" | "terminal-exit-invalid-sequence" => {
                        std::process::exit(0);
                    }
                    "terminal-exit-abnormal" => std::process::exit(76),
                    #[cfg(unix)]
                    "terminal-exit-cpu" => exit_with_native_cpu_limit(),
                    _ => {}
                }
            }
            ParentFrame::VerifyArtifact(verification) => {
                let invocation = request.invocation_id.clone().unwrap();
                let internal = verification.artifact.source == "__verification_internal__";
                let positions = verification.artifact.source == "__verification_positions__";
                let failed = internal || positions;
                let case_result = |case_id: String, script_role| VerificationCaseResult {
                    case_id,
                    passed: !failed,
                    diagnostic: failed.then_some(crate::extras::js::protocol::Diagnostic {
                        class: if internal {
                            DiagnosticClass::Internal
                        } else {
                            DiagnosticClass::Exception
                        },
                        stage: DiagnosticStage::Verification,
                        script_role,
                        exception_class: positions.then_some(JsExceptionClass::TypeError),
                        line: positions.then_some(1),
                        column: positions.then_some(1),
                    }),
                    #[cfg(feature = "skills")]
                    transcript: Default::default(),
                };
                let mut cases = Vec::new();
                // Skills requests already enumerate embedded/mutation/held-out cases explicitly.
                #[cfg(not(feature = "skills"))]
                cases.extend(
                    verification
                        .artifact
                        .tests
                        .iter()
                        .enumerate()
                        .map(|(index, _)| {
                            case_result(format!("embedded-{index}"), ScriptRole::EmbeddedTest)
                        }),
                );
                cases.extend(
                    verification
                        .cases
                        .iter()
                        .map(|case| case_result(case.case_id.clone(), ScriptRole::HeldOutTest)),
                );
                let mut result = VerificationResult {
                    passed: !failed,
                    cases,
                    loader_version: 1,
                };
                if let Some(fault) = verification
                    .artifact
                    .source
                    .strip_prefix("__verification_contract:")
                {
                    match fault {
                        "loader" => result.loader_version += 1,
                        "missing" => {
                            result.cases.pop();
                        }
                        "extra" => result.cases.push(result.cases[0].clone()),
                        "wrong-id" => result.cases[0].case_id = "unrequested-case".into(),
                        "duplicate" => result.cases[1].case_id = result.cases[0].case_id.clone(),
                        "reordered" => result.cases.swap(0, 1),
                        "false-summary" => result.passed = false,
                        "true-summary" | "valid-failure" => {
                            result.cases[0].passed = false;
                            result.cases[0].diagnostic =
                                Some(crate::extras::js::protocol::Diagnostic {
                                    class: DiagnosticClass::Contract,
                                    stage: DiagnosticStage::Verification,
                                    script_role: ScriptRole::HeldOutTest,
                                    exception_class: None,
                                    line: None,
                                    column: None,
                                });
                            result.passed = fault == "true-summary";
                        }
                        "valid" => {}
                        _ => panic!("unknown verification contract fixture"),
                    }
                }
                let terminal = WireFrame::invocation(
                    build.clone(),
                    invocation,
                    request.sequence + 1,
                    WorkerFrame::VerificationResult(result),
                );
                protocol.on_send(&terminal).unwrap();
                write_frame(&mut output, &terminal).unwrap();
                output.flush().unwrap();
                if internal {
                    std::process::exit(0);
                }
            }
            ParentFrame::Shutdown => std::process::exit(0),
            ParentFrame::Hello(_)
            | ParentFrame::ContainmentProbe(_)
            | ParentFrame::EffectResponse(EffectResponse { .. }) => std::process::exit(1),
            #[cfg(feature = "skills")]
            ParentFrame::SkillCallResponse(_) => std::process::exit(1),
        }
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn exit_with_native_cpu_limit() -> ! {
    // SAFETY: this scripted worker is a sacrificial child with no application signal handlers.
    // The default SIGXCPU disposition terminates it exactly as the native RLIMIT_CPU ceiling does
    // in production.
    unsafe { libc::raise(libc::SIGXCPU) };
    std::process::exit(78)
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
        if std::env::var_os("MINI_AGENT_TEST_SUPERVISOR_SCRIPT").is_some() {
            run_scripted_supervisor_worker();
        }
        crate::extras::js::worker::exit_test_worker();
    }
}
