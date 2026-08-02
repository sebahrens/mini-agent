use std::io::{Read, Write};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use crate::extras::js::protocol::{
    ArtifactInput, BuildIdentity, ConsoleLevel, DiagnosticClass, DiagnosticStage, InvocationId,
    JsErrorCode, ParentFrame, ParentHello, ParentProtocol, ParentWireFrame, RunStep, ScriptRole,
    StepOutcome, StepResult, VerificationCase, VerificationResult, VerifyArtifact, WireFrame,
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

fn run_step(sequence: u64, invocation: &str, code: impl Into<String>) -> ParentWireFrame {
    WireFrame::invocation(
        BuildIdentity::current(),
        InvocationId::new(invocation).unwrap(),
        sequence,
        ParentFrame::RunStep(RunStep { code: code.into() }),
    )
}

fn verify_artifact(
    sequence: u64,
    invocation: &str,
    source: impl Into<String>,
    tests: Vec<String>,
    cases: Vec<(&str, &str)>,
) -> ParentWireFrame {
    WireFrame::invocation(
        BuildIdentity::current(),
        InvocationId::new(invocation).unwrap(),
        sequence,
        ParentFrame::VerifyArtifact(VerifyArtifact {
            artifact: ArtifactInput {
                artifact_id: format!("artifact-{invocation}"),
                source: source.into(),
                exports: vec!["answer".into()],
                tests,
            },
            cases: cases
                .into_iter()
                .map(|(case_id, script)| VerificationCase {
                    case_id: case_id.into(),
                    script: script.into(),
                })
                .collect(),
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

fn run_worker_transcript(
    requests: Vec<ParentWireFrame>,
    test_timeout_ms: u64,
    test_max_pending_jobs: usize,
) -> (Vec<WorkerWireFrame>, Vec<u8>) {
    let mut process = TestWorkerLauncher::internal_worker_process_with_limits(
        test_timeout_ms,
        test_max_pending_jobs,
    )
    .launch()
    .expect("test worker should launch");
    let mut parent = ParentProtocol::new(BuildIdentity::current());

    let hello = hello(0);
    parent.on_send(&hello).unwrap();
    write_parent_frame(&mut process.input, &hello);
    let (preamble, ready) = read_worker_frame_after_test_preamble(&mut process.output);
    assert_redacted(&preamble);
    assert!(matches!(ready.message, WorkerFrame::Ready(_)));
    parent.on_receive(&ready).unwrap();
    let mut frames = vec![ready];
    for request in &requests {
        parent.on_send(request).unwrap();
        write_parent_frame(&mut process.input, request);
        let (interleaved_harness, response) =
            read_worker_frame_after_test_preamble(&mut process.output);
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
    assert_eq!(diagnostic.line, None);
    assert_eq!(diagnostic.column, None);
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
fn worker_runtime_redacts_syntax_throw_rejection_and_stack_then_recovers() {
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
    assert_closed_error(
        &results[0],
        JsErrorCode::Exception,
        DiagnosticClass::Exception,
        DiagnosticStage::Evaluation,
    );
    for index in [2, 4, 6, 8, 10, 12, 14, 16, 18] {
        assert_closed_error(
            &results[index],
            JsErrorCode::Exception,
            DiagnosticClass::Exception,
            DiagnosticStage::Evaluation,
        );
    }
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

#[test]
fn worker_runtime_oom_drops_poisoned_heap_and_next_request_succeeds() {
    let results = run_steps(
        &[
            "const chunks=[]; while(true){ chunks.push(new ArrayBuffer(1024 * 1024)); }",
            "40 + 2",
        ],
        10_000,
        10_000,
    );
    assert_eq!(results[0].outcome, StepOutcome::OutOfMemory);
    assert_eq!(results[1].outcome, StepOutcome::Value("42".into()));
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

#[test]
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

#[test]
fn worker_runtime_verification_bounds_terminal_result_expansion() {
    let oversized = WireFrame::invocation(
        BuildIdentity::current(),
        InvocationId::new("oversized-verification").unwrap(),
        2,
        ParentFrame::VerifyArtifact(VerifyArtifact {
            artifact: ArtifactInput {
                artifact_id: "oversized".into(),
                source: String::new(),
                exports: vec![],
                tests: vec![],
            },
            cases: (0..4_097)
                .map(|index| VerificationCase {
                    case_id: format!("case-{index}"),
                    script: "false".into(),
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
