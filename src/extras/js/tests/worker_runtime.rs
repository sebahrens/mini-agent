use std::io::{Read, Write};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[cfg(not(feature = "skills"))]
use crate::extras::js::protocol::ArtifactInput;
use crate::extras::js::protocol::{
    AdvisoryAttribution, BuildIdentity, ConsoleLevel, DiagnosticClass, DiagnosticStage,
    EffectOperation, EffectRequest, EffectResponse, EffectResult, GrantId, InvocationId,
    JsErrorCode, ParentFrame, ParentHello, ParentProtocol, ParentWireFrame, RunStep, ScriptRole,
    StepOutcome, StepResult, VerificationCase, VerificationCaseResult, VerificationResult,
    VerifyArtifact, WireFrame, WorkerFrame, WorkerProtocol, WorkerReady, WorkerWireFrame,
    read_frame, write_frame,
};
use crate::extras::js::supervisor::{
    EffectFuture, InvocationEffectHandler, JsWorkerSupervisor, WorkerError,
};
use crate::extras::js::types::PermCancellation;
use crate::sandbox::worker::{
    TestSupervisorStartup, TestWorkerLauncher, WorkerLaunchError, WorkerLauncher, WorkerProcess,
};

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
    let mut verification_cases = tests
        .into_iter()
        .enumerate()
        .map(|(index, script)| VerificationCase {
            case_id: format!("embedded-{index}"),
            script,
            #[cfg(feature = "skills")]
            kind: crate::extras::js::protocol::VerificationCaseKind::Embedded,
        })
        .collect::<Vec<_>>();
    verification_cases.extend(cases.into_iter().map(|(case_id, script)| VerificationCase {
        case_id: case_id.into(),
        script: script.into(),
        #[cfg(feature = "skills")]
        kind: crate::extras::js::protocol::VerificationCaseKind::HeldOut {
            expected: crate::extras::js::protocol::VerificationExpectedValue::Boolean(true),
            fake_files: Default::default(),
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

    for index in [0, 2, 4] {
        assert_closed_error(
            &results[index],
            JsErrorCode::Exception,
            DiagnosticClass::Exception,
            DiagnosticStage::Evaluation,
        );
    }
    for index in [1, 3, 5] {
        assert_eq!(results[index].outcome, StepOutcome::Value("42".into()));
    }
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

#[derive(Clone, Default)]
struct RecordingEffects {
    ordinals: Arc<Mutex<Vec<u32>>>,
}

impl InvocationEffectHandler for RecordingEffects {
    fn handle_effect(
        &mut self,
        request: EffectRequest,
        _cancellation: PermCancellation,
    ) -> EffectFuture<'_> {
        self.ordinals.lock().unwrap().push(request.effect_ordinal);
        Box::pin(async move {
            EffectResult::ReadFile {
                content: format!("effect-{}", request.effect_ordinal),
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
                    headers: Vec::new(),
                    body: "done".into(),
                    truncated: false,
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
        self.started.acquire().await.unwrap().forget();
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

#[derive(Clone)]
struct DelayedLaunchLauncher {
    launches: Arc<AtomicUsize>,
    completed_launches: Arc<AtomicUsize>,
    live_processes: Arc<AtomicUsize>,
    first_delay: Duration,
}

impl DelayedLaunchLauncher {
    fn new(first_delay: Duration) -> Self {
        Self {
            launches: Arc::new(AtomicUsize::new(0)),
            completed_launches: Arc::new(AtomicUsize::new(0)),
            live_processes: Arc::new(AtomicUsize::new(0)),
            first_delay,
        }
    }

    async fn wait_for_launches(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.launches.load(Ordering::Acquire) < expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("synchronous launcher was not entered");
    }

    async fn wait_for_completed_launches(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.completed_launches.load(Ordering::Acquire) < expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("delayed launch did not return");
    }

    async fn wait_for_live_processes(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.live_processes.load(Ordering::Acquire) != expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("late worker process was not reaped");
    }
}

impl WorkerLauncher for DelayedLaunchLauncher {
    fn containment_status(&self) -> crate::sandbox::worker::WorkerContainmentStatus {
        TestWorkerLauncher::scripted_internal_worker(0).containment_status()
    }

    fn launch(&self) -> Result<WorkerProcess, WorkerLaunchError> {
        if self.launches.fetch_add(1, Ordering::AcqRel) == 0 {
            std::thread::sleep(self.first_delay);
        }
        let mut process = TestWorkerLauncher::scripted_internal_worker(0).launch()?;
        process.observe_reap_for_test(self.live_processes.clone());
        self.completed_launches.fetch_add(1, Ordering::Release);
        Ok(process)
    }
}

#[derive(Clone)]
struct BlockedFirstLaunchLauncher {
    launches: Arc<AtomicUsize>,
    completed_launches: Arc<AtomicUsize>,
    live_processes: Arc<AtomicUsize>,
    max_live_processes: Arc<AtomicUsize>,
    release_first: Arc<(Mutex<bool>, Condvar)>,
}

impl BlockedFirstLaunchLauncher {
    fn new() -> Self {
        Self {
            launches: Arc::new(AtomicUsize::new(0)),
            completed_launches: Arc::new(AtomicUsize::new(0)),
            live_processes: Arc::new(AtomicUsize::new(0)),
            max_live_processes: Arc::new(AtomicUsize::new(0)),
            release_first: Arc::new((Mutex::new(false), Condvar::new())),
        }
    }

    fn release_first_launch(&self) {
        let (released, wake) = &*self.release_first;
        *released.lock().unwrap() = true;
        wake.notify_all();
    }

    async fn wait_for_launches(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.launches.load(Ordering::Acquire) < expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("blocked launcher was not entered");
    }

    async fn wait_for_completed_launches(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.completed_launches.load(Ordering::Acquire) < expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("released launcher did not return");
    }

    async fn wait_for_live_processes(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.live_processes.load(Ordering::Acquire) != expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker process count did not reach its expected value");
    }
}

impl WorkerLauncher for BlockedFirstLaunchLauncher {
    fn containment_status(&self) -> crate::sandbox::worker::WorkerContainmentStatus {
        TestWorkerLauncher::scripted_internal_worker(0).containment_status()
    }

    fn launch(&self) -> Result<WorkerProcess, WorkerLaunchError> {
        if self.launches.fetch_add(1, Ordering::AcqRel) == 0 {
            let (released, wake) = &*self.release_first;
            let mut released = released.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
        }
        let mut process = TestWorkerLauncher::scripted_internal_worker(0).launch()?;
        process.observe_reap_for_test(self.live_processes.clone());
        self.max_live_processes.fetch_max(
            self.live_processes.load(Ordering::Acquire),
            Ordering::AcqRel,
        );
        self.completed_launches.fetch_add(1, Ordering::Release);
        Ok(process)
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
        JSON.stringify(fetch("https://example.com", options));
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
        StepOutcome::Value(r#"{"status":200,"text":"done"}"#.into())
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

#[tokio::test]
async fn worker_supervisor_watchdog_bounds_synchronous_launch_and_reaps_late_process() {
    let launcher = DelayedLaunchLauncher::new(Duration::from_millis(500));
    let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
        launcher.clone(),
        Duration::from_millis(50),
    ));

    let started = Instant::now();
    let result = supervisor
        .execute(
            RunStep::new("success".into()),
            RecordingEffects::default(),
            PermCancellation::new(),
        )
        .await;
    assert_eq!(result, Err(WorkerError::TimedOut));
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "synchronous launcher escaped the whole-call watchdog"
    );

    launcher.wait_for_completed_launches(1).await;
    launcher.wait_for_live_processes(0).await;
    assert_eq!(
        execute_success(&supervisor).await.outcome,
        StepOutcome::Value("success".into())
    );
    supervisor.shutdown_for_test().await.unwrap();
    launcher.wait_for_live_processes(0).await;
}

#[tokio::test]
async fn worker_supervisor_cancellation_bounds_synchronous_launch_and_reaps_late_process() {
    let launcher = DelayedLaunchLauncher::new(Duration::from_millis(500));
    let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
        launcher.clone(),
        Duration::from_secs(2),
    ));
    let cancellation = PermCancellation::new();
    let request_cancellation = cancellation.clone();
    let request_supervisor = supervisor.clone();
    let request = tokio::spawn(async move {
        request_supervisor
            .execute(
                RunStep::new("success".into()),
                RecordingEffects::default(),
                request_cancellation,
            )
            .await
    });
    launcher.wait_for_launches(1).await;

    let started = Instant::now();
    cancellation.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(250), request)
            .await
            .expect("cancellation did not interrupt synchronous startup")
            .unwrap(),
        Err(WorkerError::Cancelled)
    );
    assert!(started.elapsed() < Duration::from_millis(250));

    launcher.wait_for_completed_launches(1).await;
    launcher.wait_for_live_processes(0).await;
    assert_eq!(
        execute_success(&supervisor).await.outcome,
        StepOutcome::Value("success".into())
    );
    supervisor.shutdown_for_test().await.unwrap();
    launcher.wait_for_live_processes(0).await;
}

async fn assert_blocked_startup_does_not_accumulate_launches(repeated_call_count: usize) {
    let launcher = BlockedFirstLaunchLauncher::new();
    let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
        launcher.clone(),
        Duration::from_millis(200),
    ));

    assert_eq!(
        supervisor
            .execute(
                RunStep::new("success".into()),
                RecordingEffects::default(),
                PermCancellation::new(),
            )
            .await,
        Err(WorkerError::TimedOut)
    );
    launcher.wait_for_launches(1).await;

    let repeated = (0..repeated_call_count)
        .map(|_| {
            let supervisor = supervisor.clone();
            tokio::spawn(async move {
                supervisor
                    .execute(
                        RunStep::new("success".into()),
                        RecordingEffects::default(),
                        PermCancellation::new(),
                    )
                    .await
            })
        })
        .collect::<Vec<_>>();
    let mut repeated_results = Vec::new();
    for request in repeated {
        repeated_results.push(request.await.unwrap());
    }
    let launches_while_first_was_blocked = launcher.launches.load(Ordering::Acquire);

    // Release and clean up before making assertions so the intentionally failing old behavior
    // cannot strand its synchronous launch thread or any worker process in the test binary.
    launcher.release_first_launch();
    launcher
        .wait_for_completed_launches(launches_while_first_was_blocked)
        .await;
    supervisor.shutdown_for_test().await.unwrap();
    launcher.wait_for_live_processes(0).await;

    assert_eq!(
        execute_success(&supervisor).await.outcome,
        StepOutcome::Value("success".into())
    );
    supervisor.shutdown_for_test().await.unwrap();
    launcher.wait_for_live_processes(0).await;
    let maximum_live = launcher.max_live_processes.load(Ordering::Acquire);

    assert_eq!(
        launches_while_first_was_blocked, 1,
        "immediate callers started additional detached launch threads"
    );
    assert!(
        repeated_results
            .iter()
            .all(|result| *result == Err(WorkerError::TimedOut)),
        "callers waiting behind an in-flight launch did not respect their own deadlines"
    );
    assert!(
        maximum_live <= 1,
        "overlapping launches created {maximum_live} workers"
    );
}

#[tokio::test]
async fn worker_supervisor_immediate_next_call_waits_behind_in_flight_launch() {
    assert_blocked_startup_does_not_accumulate_launches(1).await;
}

#[tokio::test]
async fn worker_supervisor_repeated_calls_do_not_accumulate_in_flight_launches() {
    assert_blocked_startup_does_not_accumulate_launches(8).await;
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
    tokio::time::timeout(Duration::from_secs(1), gated.wait_started())
        .await
        .expect("fake effect did not become pending");
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

#[tokio::test]
async fn worker_supervisor_recovery_watchdog_and_caller_drop() {
    let (supervisor, launcher) =
        recovery_supervisor(TestSupervisorStartup::Healthy, Duration::from_millis(100));
    assert_eq!(
        supervisor
            .execute(
                RunStep::new("deadline".into()),
                RecordingEffects::default(),
                PermCancellation::new(),
            )
            .await,
        Err(WorkerError::TimedOut)
    );
    assert_eq!(
        execute_success(&supervisor).await.outcome,
        StepOutcome::Value("success".into())
    );

    let gated = GatedEffects::new();
    let task_supervisor = supervisor.clone();
    let task_effects = gated.clone();
    let dropped = tokio::spawn(async move {
        task_supervisor
            .execute(
                RunStep::new("effect-pending".into()),
                task_effects,
                PermCancellation::new(),
            )
            .await
    });
    gated.wait_started().await;
    dropped.abort();
    assert!(dropped.await.unwrap_err().is_cancelled());
    assert_eq!(
        execute_success(&supervisor).await.outcome,
        StepOutcome::Value("success".into())
    );

    launcher.wait_for_live_processes(1).await;
    supervisor.shutdown_for_test().await.unwrap();
    launcher.wait_for_live_processes(0).await;
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
            },
        }],
    }
}

#[derive(Clone)]
struct VerificationSchedulerLauncher {
    launches: Arc<AtomicUsize>,
    live_processes: Arc<AtomicUsize>,
    max_live_processes: Arc<AtomicUsize>,
    first_launch_started: Arc<tokio::sync::Semaphore>,
    release_first_launch: Arc<(Mutex<bool>, Condvar)>,
}

impl VerificationSchedulerLauncher {
    fn new() -> Self {
        Self {
            launches: Arc::new(AtomicUsize::new(0)),
            live_processes: Arc::new(AtomicUsize::new(0)),
            max_live_processes: Arc::new(AtomicUsize::new(0)),
            first_launch_started: Arc::new(tokio::sync::Semaphore::new(0)),
            release_first_launch: Arc::new((Mutex::new(false), Condvar::new())),
        }
    }

    async fn wait_for_first_launch(&self) {
        self.first_launch_started
            .acquire()
            .await
            .expect("verification launch barrier must remain open")
            .forget();
    }

    fn release_first_launch(&self) {
        let (released, wake) = &*self.release_first_launch;
        *released.lock().unwrap() = true;
        wake.notify_all();
    }
}

impl WorkerLauncher for VerificationSchedulerLauncher {
    fn containment_status(&self) -> crate::sandbox::worker::WorkerContainmentStatus {
        TestWorkerLauncher::scripted_internal_worker(0).containment_status()
    }

    fn launch(&self) -> Result<WorkerProcess, WorkerLaunchError> {
        if self.launches.fetch_add(1, Ordering::AcqRel) == 0 {
            self.first_launch_started.add_permits(1);
            let (released, wake) = &*self.release_first_launch;
            let mut released = released.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
        }
        let mut process = TestWorkerLauncher::scripted_internal_worker(0).launch()?;
        process.observe_reap_for_test(self.live_processes.clone());
        self.max_live_processes.fetch_max(
            self.live_processes.load(Ordering::Acquire),
            Ordering::AcqRel,
        );
        Ok(process)
    }
}

#[tokio::test]
async fn verification_scheduler_prioritizes_interactive_between_atomic_requests() {
    let launcher = VerificationSchedulerLauncher::new();
    let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_for_test(launcher.clone()));

    let first_supervisor = supervisor.clone();
    let first =
        std::thread::spawn(move || first_supervisor.verify_blocking(held_out_verification()));
    launcher.wait_for_first_launch().await;

    let second_supervisor = supervisor.clone();
    let second =
        std::thread::spawn(move || second_supervisor.verify_blocking(held_out_verification()));
    supervisor
        .wait_for_verification_queue_depth_for_test(1)
        .await;

    let gated = GatedEffects::new();
    let interactive_supervisor = supervisor.clone();
    let interactive_effects = gated.clone();
    let interactive = tokio::spawn(async move {
        interactive_supervisor
            .execute(
                RunStep::new("effect-pending".into()),
                interactive_effects,
                PermCancellation::new(),
            )
            .await
    });
    supervisor.wait_for_interactive_waiters_for_test(1).await;
    launcher.release_first_launch();

    assert!(first.join().unwrap().unwrap().passed);
    gated.wait_started().await;
    assert_eq!(
        supervisor.verification_queue_depth_for_test(),
        1,
        "queued verification bypassed an already-waiting interactive request"
    );
    gated.release();
    assert_eq!(
        interactive.await.unwrap().unwrap().outcome,
        StepOutcome::Value("success".into())
    );
    assert!(second.join().unwrap().unwrap().passed);
    assert_eq!(launcher.launches.load(Ordering::Acquire), 1);
    assert_eq!(launcher.max_live_processes.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn verification_scheduler_cancels_before_dequeue_without_recycling_worker() {
    let launcher = VerificationSchedulerLauncher::new();
    let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_for_test(launcher.clone()));

    let first_supervisor = supervisor.clone();
    let first =
        std::thread::spawn(move || first_supervisor.verify_blocking(held_out_verification()));
    launcher.wait_for_first_launch().await;

    let cancellation = PermCancellation::new();
    let queued_cancellation = cancellation.clone();
    let queued_supervisor = supervisor.clone();
    let queued = std::thread::spawn(move || {
        queued_supervisor.verify_blocking_cancellable(held_out_verification(), queued_cancellation)
    });
    supervisor
        .wait_for_verification_queue_depth_for_test(1)
        .await;
    cancellation.cancel();

    assert_eq!(queued.join().unwrap(), Err(WorkerError::Cancelled));
    launcher.release_first_launch();
    assert!(first.join().unwrap().unwrap().passed);
    assert_eq!(launcher.launches.load(Ordering::Acquire), 1);
    assert_eq!(supervisor.generation_for_test().await, Some(1));
}

#[tokio::test]
async fn verification_scheduler_cancellation_wakes_priority_wait_while_interactive_stays_active() {
    let supervisor = scripted_supervisor(0);
    let gated = GatedEffects::new();
    let interactive_supervisor = supervisor.clone();
    let interactive_effects = gated.clone();
    let interactive = tokio::spawn(async move {
        interactive_supervisor
            .execute(
                RunStep::new("effect-pending".into()),
                interactive_effects,
                PermCancellation::new(),
            )
            .await
    });
    gated.wait_started().await;

    let cancellation = PermCancellation::new();
    let queued_cancellation = cancellation.clone();
    let queued_supervisor = supervisor.clone();
    let queued = std::thread::spawn(move || {
        queued_supervisor.verify_blocking_cancellable(held_out_verification(), queued_cancellation)
    });
    supervisor
        .wait_for_verification_queue_depth_for_test(1)
        .await;

    cancellation.cancel();
    assert_eq!(queued.join().unwrap(), Err(WorkerError::Cancelled));
    supervisor
        .wait_for_verification_queue_depth_for_test(0)
        .await;
    assert!(
        !interactive.is_finished(),
        "cancellation must wake the scheduler without releasing interactive priority"
    );

    gated.release();
    assert_eq!(
        interactive.await.unwrap().unwrap().outcome,
        StepOutcome::Value("success".into())
    );
}

#[tokio::test]
async fn verification_scheduler_bounds_queue_and_reports_retryable_overflow() {
    let launcher = VerificationSchedulerLauncher::new();
    let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_for_test(launcher.clone()));

    let first_supervisor = supervisor.clone();
    let first =
        std::thread::spawn(move || first_supervisor.verify_blocking(held_out_verification()));
    launcher.wait_for_first_launch().await;

    let mut cancellations = Vec::new();
    let mut queued = Vec::new();
    for _ in 0..supervisor.verification_queue_capacity_for_test() {
        let cancellation = PermCancellation::new();
        let queued_cancellation = cancellation.clone();
        let queued_supervisor = supervisor.clone();
        queued.push(std::thread::spawn(move || {
            queued_supervisor
                .verify_blocking_cancellable(held_out_verification(), queued_cancellation)
        }));
        cancellations.push(cancellation);
    }
    supervisor
        .wait_for_verification_queue_depth_for_test(
            supervisor.verification_queue_capacity_for_test(),
        )
        .await;

    let overflow_supervisor = supervisor.clone();
    let overflow =
        std::thread::spawn(move || overflow_supervisor.verify_blocking(held_out_verification()))
            .join()
            .unwrap();
    assert_eq!(overflow, Err(WorkerError::VerificationQueueFull));
    assert!(WorkerError::VerificationQueueFull.is_retryable_admission_infrastructure());

    for cancellation in cancellations {
        cancellation.cancel();
    }
    launcher.release_first_launch();
    assert!(first.join().unwrap().unwrap().passed);
    for task in queued {
        assert_eq!(task.join().unwrap(), Err(WorkerError::Cancelled));
    }
    assert_eq!(launcher.max_live_processes.load(Ordering::Acquire), 1);
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

#[tokio::test]
async fn worker_supervisor_transport_serializes_concurrent_callers_and_orders_effects() {
    let supervisor = scripted_supervisor(0);
    let gated = GatedEffects::new();
    let first_effects = gated.clone();
    let first_supervisor = supervisor.clone();
    let first = tokio::spawn(async move {
        first_supervisor
            .execute(
                RunStep::new("two-effects".into()),
                first_effects,
                PermCancellation::new(),
            )
            .await
    });
    gated.wait_started().await;

    let second_supervisor = supervisor.clone();
    let second = tokio::spawn(async move {
        second_supervisor
            .execute(
                RunStep::new("success".into()),
                RecordingEffects::default(),
                PermCancellation::new(),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !second.is_finished(),
        "second caller bypassed transport owner"
    );

    gated.release();
    let first = first.await.unwrap().unwrap();
    let second = second.await.unwrap().unwrap();
    assert_eq!(first.outcome, StepOutcome::Value("effects-complete".into()));
    assert_eq!(second.outcome, StepOutcome::Value("success".into()));
    assert_eq!(*gated.ordinals.lock().unwrap(), vec![0, 1]);
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

#[tokio::test]
async fn worker_supervisor_transport_cancellation_drops_effect_and_starts_next_generation() {
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
    assert_eq!(task.await.unwrap(), Err(WorkerError::Cancelled));
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

#[tokio::test]
async fn worker_supervisor_transport_dropped_caller_releases_owner_and_cancels_handler() {
    let supervisor = scripted_supervisor(0);
    let gated = GatedEffects::new();
    let task_supervisor = supervisor.clone();
    let task_effects = gated.clone();
    let task = tokio::spawn(async move {
        task_supervisor
            .execute(
                RunStep::new("effect-pending".into()),
                task_effects,
                PermCancellation::new(),
            )
            .await
    });
    gated.wait_started().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());

    let recovered = tokio::time::timeout(
        Duration::from_secs(5),
        supervisor.execute(
            RunStep::new("success".into()),
            RecordingEffects::default(),
            PermCancellation::new(),
        ),
    )
    .await
    .expect("dropped caller retained transport ownership")
    .unwrap();
    assert_eq!(recovered.outcome, StepOutcome::Value("success".into()));
    assert!(gated.dropped.load(Ordering::Acquire));
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

#[tokio::test]
async fn worker_supervisor_transport_bounds_stderr_without_blocking_worker() {
    let supervisor = scripted_supervisor(256 * 1024);
    let result = supervisor
        .execute(
            RunStep::new("success".into()),
            RecordingEffects::default(),
            PermCancellation::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.outcome, StepOutcome::Value("success".into()));
    let stats = supervisor.stderr_stats_for_test().await.unwrap();
    assert_eq!(stats.retained_bytes, 0);
    assert!(stats.observed_bytes <= 4096);
    assert!(stats.truncated);
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
    let ready = WireFrame::connection(build.clone(), 1, WorkerFrame::Ready(WorkerReady {}));
    protocol.on_send(&ready).unwrap();
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
                    "deadline" => std::thread::park_timeout(Duration::from_secs(30)),
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
                    "effect-pending" | "crash-pending-effect" => 1,
                    _ => 0,
                };
                for ordinal in 0..effect_count {
                    let effect = WireFrame::invocation(
                        build.clone(),
                        invocation.clone(),
                        sequence,
                        WorkerFrame::EffectRequest(EffectRequest {
                            effect_ordinal: ordinal,
                            grant_id: GrantId::new(uuid::Uuid::from_u128(1)).unwrap(),
                            advisory: AdvisoryAttribution::default(),
                            operation: EffectOperation::ReadFile {
                                path: "fixture".into(),
                            },
                        }),
                    );
                    protocol.on_send(&effect).unwrap();
                    write_frame(&mut output, &effect).unwrap();
                    output.flush().unwrap();
                    if step.code == "crash-pending-effect" {
                        std::thread::sleep(Duration::from_millis(30));
                        std::process::exit(75);
                    }
                    let response: ParentWireFrame = read_frame(&mut input).unwrap();
                    assert!(matches!(response.message, ParentFrame::EffectResponse(_)));
                    protocol.on_receive(&response).unwrap();
                    sequence = response.sequence + 1;
                }
                let value = if step.code == "two-effects" {
                    "effects-complete"
                } else {
                    "success"
                };
                let terminal = WireFrame::invocation(
                    build.clone(),
                    invocation,
                    sequence,
                    WorkerFrame::StepResult(StepResult {
                        outcome: StepOutcome::Value(value.into()),
                        console: Vec::new(),
                        diagnostic: None,
                        #[cfg(feature = "skills")]
                        skill_events: Vec::new(),
                        #[cfg(feature = "skills")]
                        evidence_complete: true,
                    }),
                );
                protocol.on_send(&terminal).unwrap();
                write_frame(&mut output, &terminal).unwrap();
                output.flush().unwrap();
            }
            ParentFrame::VerifyArtifact(verification) => {
                let invocation = request.invocation_id.clone().unwrap();
                let mut cases = verification
                    .artifact
                    .tests
                    .iter()
                    .enumerate()
                    .map(|(index, _)| VerificationCaseResult {
                        case_id: format!("embedded-{index}"),
                        passed: true,
                        diagnostic: None,
                        #[cfg(feature = "skills")]
                        transcript: Default::default(),
                    })
                    .collect::<Vec<_>>();
                cases.extend(
                    verification
                        .cases
                        .iter()
                        .map(|case| VerificationCaseResult {
                            case_id: case.case_id.clone(),
                            passed: true,
                            diagnostic: None,
                            #[cfg(feature = "skills")]
                            transcript: Default::default(),
                        }),
                );
                let terminal = WireFrame::invocation(
                    build.clone(),
                    invocation,
                    request.sequence + 1,
                    WorkerFrame::VerificationResult(VerificationResult {
                        passed: true,
                        cases,
                        loader_version: 1,
                    }),
                );
                protocol.on_send(&terminal).unwrap();
                write_frame(&mut output, &terminal).unwrap();
                output.flush().unwrap();
            }
            ParentFrame::Shutdown => std::process::exit(0),
            ParentFrame::Hello(_) | ParentFrame::EffectResponse(EffectResponse { .. }) => {
                std::process::exit(1)
            }
            #[cfg(feature = "skills")]
            ParentFrame::SkillCallResponse(_) => std::process::exit(1),
        }
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
        if std::env::var_os("MINI_AGENT_TEST_SUPERVISOR_SCRIPT").is_some() {
            run_scripted_supervisor_worker();
        }
        crate::extras::js::worker::exit_test_worker();
    }
}
