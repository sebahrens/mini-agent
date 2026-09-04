use std::io::Cursor;

use uuid::Uuid;

use crate::extras::js::protocol::*;

#[cfg(feature = "skills")]
fn verification_artifact(
    source: String,
    tests: Vec<String>,
) -> crate::extras::js::skills::SkillArtifact {
    crate::extras::js::skills::SkillArtifact::new(
        source,
        "worker protocol fixture".into(),
        vec![],
        vec![crate::extras::js::skills::SkillExport {
            name: "answer".into(),
            signature: "answer()".into(),
        }],
        tests,
        crate::extras::js::skills::CapabilityManifest::pure(),
    )
    .unwrap()
}

#[cfg(not(feature = "skills"))]
fn verification_artifact(source: String, tests: Vec<String>) -> ArtifactInput {
    ArtifactInput {
        artifact_id: "advisory-id".into(),
        source,
        exports: vec!["answer".into()],
        tests,
    }
}

fn build() -> BuildIdentity {
    BuildIdentity::new("test-build-1").unwrap()
}

fn invocation() -> InvocationId {
    InvocationId::new("invocation-1").unwrap()
}

fn protocol_challenge() -> LaunchChallenge {
    challenge(1)
}

fn parent_protocol() -> ParentProtocol {
    ParentProtocol::with_challenge_for_test(build(), protocol_challenge())
}

fn connection<M>(sequence: u64, message: M) -> WireFrame<M> {
    WireFrame::connection(build(), sequence, message)
}

fn invoked<M>(sequence: u64, message: M) -> WireFrame<M> {
    WireFrame::invocation(build(), invocation(), sequence, message)
}

fn hello() -> ParentWireFrame {
    connection(
        0,
        ParentFrame::Hello(ParentHello {
            challenge: protocol_challenge(),
        }),
    )
}

fn ready() -> WorkerWireFrame {
    connection(
        1,
        WorkerFrame::Ready(WorkerReady {
            challenge: protocol_challenge(),
        }),
    )
}

fn containment_probe(sequence: u64) -> ParentWireFrame {
    connection(sequence, ParentFrame::ContainmentProbe(ContainmentProbe {}))
}

fn containment_attested(sequence: u64) -> WorkerWireFrame {
    connection(
        sequence,
        WorkerFrame::ContainmentAttested(ContainmentAttestation::Passed),
    )
}

fn run(sequence: u64) -> ParentWireFrame {
    invoked(
        sequence,
        ParentFrame::RunStep(RunStep::new("40 + 2".into())),
    )
}

fn verify(sequence: u64) -> ParentWireFrame {
    invoked(
        sequence,
        ParentFrame::VerifyArtifact(VerifyArtifact {
            artifact: verification_artifact(
                "function answer() { return 42; }".into(),
                vec!["answer() === 42".into()],
            ),
            cases: vec![VerificationCase {
                case_id: "embedded-0".into(),
                script: "answer() === 42".into(),
                #[cfg(feature = "skills")]
                kind: VerificationCaseKind::Embedded,
            }],
        }),
    )
}

fn grant() -> GrantId {
    GrantId::new(Uuid::from_u128(1)).unwrap()
}

fn challenge(value: u128) -> LaunchChallenge {
    LaunchChallenge::new(Uuid::from_u128(value)).unwrap()
}

fn effect_request(sequence: u64, ordinal: u32) -> WorkerWireFrame {
    invoked(
        sequence,
        WorkerFrame::EffectRequest(Box::new(EffectRequest {
            effect_ordinal: ordinal,
            grant_id: grant(),
            advisory: AdvisoryAttribution::default(),
            operation: EffectOperation::ReadFile {
                path: "README.md".into(),
            },
        })),
    )
}

fn effect_response(sequence: u64, ordinal: u32) -> ParentWireFrame {
    invoked(
        sequence,
        ParentFrame::EffectResponse(EffectResponse {
            effect_ordinal: ordinal,
            result: EffectResult::ReadFile {
                content: "contents".into(),
            },
        }),
    )
}

fn outcome_unknown_response(sequence: u64, ordinal: u32) -> ParentWireFrame {
    invoked(
        sequence,
        ParentFrame::EffectResponse(EffectResponse {
            effect_ordinal: ordinal,
            result: EffectResult::Error(crate::extras::js::protocol::EffectError {
                code: crate::extras::js::protocol::EffectErrorCode::OutcomeUnknown,
            }),
        }),
    )
}

#[cfg(feature = "skills")]
fn skill_call_request(sequence: u64, request_ordinal: u32, call_ordinal: u32) -> WorkerWireFrame {
    invoked(
        sequence,
        WorkerFrame::SkillCallRequest(SkillCallRequest {
            request_ordinal,
            artifact_id: "a".repeat(64),
            export_name: "increment".into(),
            call_ordinal,
        }),
    )
}

#[cfg(feature = "skills")]
fn skill_call_response(sequence: u64, request_ordinal: u32) -> ParentWireFrame {
    invoked(
        sequence,
        ParentFrame::SkillCallResponse(SkillCallResponse {
            request_ordinal,
            authorization: None,
        }),
    )
}

fn step_result(sequence: u64) -> WorkerWireFrame {
    invoked(
        sequence,
        WorkerFrame::StepResult(StepResult {
            outcome: StepOutcome::Value("42".into()),
            console: vec![],
            diagnostic: None,
            #[cfg(feature = "skills")]
            skill_events: vec![],
            #[cfg(feature = "skills")]
            evidence_complete: true,
        }),
    )
}

fn verification_result(sequence: u64) -> WorkerWireFrame {
    invoked(
        sequence,
        WorkerFrame::VerificationResult(VerificationResult {
            passed: true,
            cases: vec![VerificationCaseResult {
                case_id: "embedded-0".into(),
                passed: true,
                diagnostic: None,
                #[cfg(feature = "skills")]
                transcript: Default::default(),
            }],
            loader_version: 1,
        }),
    )
}

fn encode<M: serde::Serialize>(frame: &M) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, frame).unwrap();
    bytes
}

#[test]
fn worker_protocol_codec_round_trips_a_frame() {
    let current = BuildIdentity::current();
    let fingerprint = current
        .as_str()
        .strip_prefix(concat!(env!("CARGO_PKG_VERSION"), "+"))
        .expect("ordinary builds must bind an exact fingerprint after the package version");
    assert_eq!(fingerprint.len(), 64);
    assert!(
        fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    assert_eq!(grant().get(), Uuid::from_u128(1));
    let expected = run(2);
    let actual: ParentWireFrame = read_frame(&mut Cursor::new(encode(&expected))).unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn worker_protocol_rejects_same_version_with_a_different_build_fingerprint() {
    let parent_build =
        BuildIdentity::new(format!("{}+{}", env!("CARGO_PKG_VERSION"), "a".repeat(64))).unwrap();
    let worker_build =
        BuildIdentity::new(format!("{}+{}", env!("CARGO_PKG_VERSION"), "b".repeat(64))).unwrap();
    let mut parent = ParentProtocol::with_challenge_for_test(parent_build, protocol_challenge());
    let frame = WireFrame::connection(
        worker_build,
        0,
        ParentFrame::Hello(ParentHello {
            challenge: protocol_challenge(),
        }),
    );

    assert!(matches!(
        parent.on_send(&frame),
        Err(ProtocolError::BuildMismatch { .. })
    ));
}

#[test]
fn launch_challenge_rejects_nil_values_and_is_fresh_per_parent_protocol() {
    assert_eq!(
        LaunchChallenge::new(Uuid::nil()),
        Err(WireIdError::NilLaunchChallenge)
    );

    let first = ParentProtocol::new(build()).hello();
    let second = ParentProtocol::new(build()).hello();
    assert_ne!(first.challenge, second.challenge);
}

#[test]
fn parent_protocol_rejects_a_forged_ready_challenge_without_advancing() {
    let expected = challenge(10);
    let mut parent = ParentProtocol::with_challenge_for_test(build(), expected.clone());
    let hello = connection(
        0,
        ParentFrame::Hello(ParentHello {
            challenge: expected,
        }),
    );
    parent.on_send(&hello).unwrap();

    let forged = connection(
        1,
        WorkerFrame::Ready(WorkerReady {
            challenge: challenge(11),
        }),
    );
    assert_eq!(
        parent.on_receive(&forged),
        Err(ProtocolError::LaunchChallengeMismatch)
    );
    assert_eq!(parent.state(), &ParentState::AwaitReady);
}

#[test]
fn parent_protocol_rejects_a_mismatched_hello_without_consuming_the_handshake() {
    let expected = challenge(30);
    let mut parent = ParentProtocol::with_challenge_for_test(build(), expected.clone());
    let mismatched = connection(
        0,
        ParentFrame::Hello(ParentHello {
            challenge: challenge(31),
        }),
    );
    assert_eq!(
        parent.on_send(&mismatched),
        Err(ProtocolError::LaunchChallengeMismatch)
    );
    assert_eq!(parent.state(), &ParentState::AwaitReady);

    let exact = connection(
        0,
        ParentFrame::Hello(ParentHello {
            challenge: expected,
        }),
    );
    parent.on_send(&exact).unwrap();
    assert_eq!(parent.state(), &ParentState::AwaitReady);
}

#[test]
fn parent_protocol_rejects_ready_replayed_from_another_launch() {
    let old_challenge = challenge(40);
    let current_challenge = challenge(41);
    let mut old = ParentProtocol::with_challenge_for_test(build(), old_challenge.clone());
    old.on_send(&connection(
        0,
        ParentFrame::Hello(ParentHello {
            challenge: old_challenge.clone(),
        }),
    ))
    .unwrap();
    let replayed_ready = connection(
        1,
        WorkerFrame::Ready(WorkerReady {
            challenge: old_challenge,
        }),
    );
    old.on_receive(&replayed_ready).unwrap();

    let mut current = ParentProtocol::with_challenge_for_test(build(), current_challenge.clone());
    current
        .on_send(&connection(
            0,
            ParentFrame::Hello(ParentHello {
                challenge: current_challenge.clone(),
            }),
        ))
        .unwrap();

    assert_eq!(
        current.on_receive(&replayed_ready),
        Err(ProtocolError::LaunchChallengeMismatch)
    );
    assert_eq!(current.state(), &ParentState::AwaitReady);
    current
        .on_receive(&connection(
            1,
            WorkerFrame::Ready(WorkerReady {
                challenge: current_challenge,
            }),
        ))
        .unwrap();
    assert_eq!(current.state(), &ParentState::Idle);
}

#[test]
fn worker_protocol_only_echoes_the_received_launch_challenge() {
    let expected = challenge(20);
    let mut worker = WorkerProtocol::new(build());
    worker
        .on_receive(&connection(
            0,
            ParentFrame::Hello(ParentHello {
                challenge: expected.clone(),
            }),
        ))
        .unwrap();

    let forged = connection(
        1,
        WorkerFrame::Ready(WorkerReady {
            challenge: challenge(21),
        }),
    );
    assert_eq!(
        worker.on_send(&forged),
        Err(ProtocolError::LaunchChallengeMismatch)
    );
    let exact = connection(
        1,
        WorkerFrame::Ready(WorkerReady {
            challenge: expected,
        }),
    );
    worker.on_send(&exact).unwrap();
    assert_eq!(worker.state(), &WorkerState::Idle);
}

#[test]
fn worker_protocol_rejects_zero_and_oversized_lengths_before_payload_allocation() {
    let zero = 0_u32.to_be_bytes();
    assert_eq!(
        read_frame::<_, ParentWireFrame>(&mut Cursor::new(zero)),
        Err(FrameError::ZeroLength)
    );

    let oversized = ((MAX_FRAME_BYTES as u32) + 1).to_be_bytes();
    assert_eq!(
        read_frame::<_, ParentWireFrame>(&mut Cursor::new(oversized)),
        Err(FrameError::FrameTooLarge {
            length: MAX_FRAME_BYTES + 1,
            maximum: MAX_FRAME_BYTES,
        })
    );
}

#[test]
fn worker_protocol_reports_truncated_header_and_body_at_every_eof_offset() {
    let bytes = encode(&run(2));
    for offset in 0..bytes.len() {
        let error =
            read_frame::<_, ParentWireFrame>(&mut Cursor::new(&bytes[..offset])).unwrap_err();
        if offset < 4 {
            assert_eq!(error, FrameError::TruncatedHeader { read: offset });
        } else {
            assert_eq!(
                error,
                FrameError::TruncatedBody {
                    expected: bytes.len() - 4,
                    read: offset - 4,
                }
            );
        }
    }
}

#[test]
fn worker_protocol_rejects_malformed_unknown_and_wrong_direction_json() {
    fn framed(payload: &[u8]) -> Vec<u8> {
        let mut bytes = (payload.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    assert!(matches!(
        read_frame::<_, ParentWireFrame>(&mut Cursor::new(framed(b"{"))),
        Err(FrameError::InvalidJson)
    ));
    assert!(matches!(
        read_frame::<_, ParentWireFrame>(&mut Cursor::new(framed(
            br#"{"protocol_version":3,"build_id":"test-build-1","invocation_id":null,"sequence":0,"message":{"kind":"future_parent_message"}}"#
        ))),
        Err(FrameError::InvalidJson)
    ));
    assert!(matches!(
        read_frame::<_, ParentWireFrame>(&mut Cursor::new(encode(&ready()))),
        Err(FrameError::InvalidJson)
    ));
}

#[test]
fn worker_protocol_rejects_unknown_fields_and_invalid_wire_ids() {
    let payload = br#"{"protocol_version":3,"build_id":"test-build-1","invocation_id":null,"sequence":0,"message":{"kind":"shutdown"},"smuggled":"value"}"#;
    let mut bytes = (payload.len() as u32).to_be_bytes().to_vec();
    bytes.extend_from_slice(payload);
    assert!(matches!(
        read_frame::<_, ParentWireFrame>(&mut Cursor::new(bytes)),
        Err(FrameError::InvalidJson)
    ));

    assert!(InvocationId::new("").is_err());
    assert!(InvocationId::new("contains spaces").is_err());
    assert!(BuildIdentity::new("").is_err());
    assert!(GrantId::new(Uuid::nil()).is_err());

    for hello in [
        br#"{"protocol_version":3,"build_id":"test-build-1","invocation_id":null,"sequence":0,"message":{"kind":"hello","data":{}}}"#.as_slice(),
        br#"{"protocol_version":3,"build_id":"test-build-1","invocation_id":null,"sequence":0,"message":{"kind":"hello","data":{"challenge":"00000000-0000-0000-0000-000000000000"}}}"#.as_slice(),
    ] {
        let mut bytes = (hello.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(hello);
        assert!(matches!(
            read_frame::<_, ParentWireFrame>(&mut Cursor::new(bytes)),
            Err(FrameError::InvalidJson)
        ));
    }
}

#[test]
fn worker_protocol_bounds_outbound_and_nested_payloads() {
    let frame = invoked(
        2,
        ParentFrame::RunStep(RunStep::new("x".repeat(MAX_FRAME_BYTES))),
    );
    assert_eq!(
        write_frame(&mut Vec::new(), &frame),
        Err(FrameError::FrameTooLarge {
            length: MAX_FRAME_BYTES + 1,
            maximum: MAX_FRAME_BYTES,
        })
    );

    let nested = invoked(
        2,
        ParentFrame::VerifyArtifact(VerifyArtifact {
            artifact: verification_artifact(
                "function answer() { return 42; }".into(),
                vec!["x".repeat(MAX_FRAME_BYTES)],
            ),
            cases: vec![],
        }),
    );
    assert!(matches!(
        write_frame(&mut Vec::new(), &nested),
        Err(FrameError::FrameTooLarge { .. })
    ));

    let proposal = invoked(
        3,
        WorkerFrame::EffectRequest(Box::new(EffectRequest {
            effect_ordinal: 0,
            grant_id: grant(),
            advisory: AdvisoryAttribution::default(),
            operation: EffectOperation::ProposeSkill {
                draft: SkillProposalDraft {
                    source: "function run() { return true; }".into(),
                    description: "bounded proposal".into(),
                    exports: vec![SkillProposalExport {
                        name: "run".into(),
                        signature: "run(): boolean".into(),
                    }],
                    tests: vec!["run() === true".into()],
                    capability: SkillProposalCapability {
                        tier: "read_only".into(),
                        grants: vec![SkillProposalScope::ReadFile {
                            workspace_prefixes: vec!["x".repeat(MAX_FRAME_BYTES)],
                        }],
                    },
                    tags: vec![],
                    predecessor_id: None,
                },
            },
        })),
    );
    assert!(matches!(
        write_frame(&mut Vec::new(), &proposal),
        Err(FrameError::FrameTooLarge { .. })
    ));
}

#[test]
fn worker_protocol_run_step_complete_transition_table() {
    let mut parent = parent_protocol();
    let mut worker = WorkerProtocol::new(build());

    parent.on_send(&hello()).unwrap();
    worker.on_receive(&hello()).unwrap();
    worker.on_send(&ready()).unwrap();
    parent.on_receive(&ready()).unwrap();
    assert_eq!(parent.state(), &ParentState::Idle);
    assert_eq!(worker.state(), &WorkerState::Idle);

    parent.on_send(&run(2)).unwrap();
    worker.on_receive(&run(2)).unwrap();
    worker.on_send(&effect_request(3, 0)).unwrap();
    parent.on_receive(&effect_request(3, 0)).unwrap();
    parent.on_send(&effect_response(4, 0)).unwrap();
    worker.on_receive(&effect_response(4, 0)).unwrap();
    worker.on_send(&effect_request(5, 1)).unwrap();
    parent.on_receive(&effect_request(5, 1)).unwrap();
    parent.on_send(&effect_response(6, 1)).unwrap();
    worker.on_receive(&effect_response(6, 1)).unwrap();
    worker.on_send(&step_result(7)).unwrap();
    parent.on_receive(&step_result(7)).unwrap();
    assert_eq!(parent.state(), &ParentState::Idle);
    assert_eq!(worker.state(), &WorkerState::Idle);

    let shutdown = connection(8, ParentFrame::Shutdown);
    parent.on_send(&shutdown).unwrap();
    worker.on_receive(&shutdown).unwrap();
    assert_eq!(parent.state(), &ParentState::Closed);
    assert_eq!(worker.state(), &WorkerState::Closed);
}

#[test]
fn worker_protocol_containment_attestation_is_closed_and_connection_scoped() {
    let mut parent = parent_protocol();
    let mut worker = WorkerProtocol::new(build());

    parent.on_send(&hello()).unwrap();
    worker.on_receive(&hello()).unwrap();
    worker.on_send(&ready()).unwrap();
    parent.on_receive(&ready()).unwrap();
    parent.on_send(&containment_probe(2)).unwrap();
    worker.on_receive(&containment_probe(2)).unwrap();
    assert_eq!(parent.state(), &ParentState::AwaitContainmentAttestation);
    assert_eq!(worker.state(), &WorkerState::AttestingContainment);
    worker.on_send(&containment_attested(3)).unwrap();
    parent.on_receive(&containment_attested(3)).unwrap();
    assert_eq!(parent.state(), &ParentState::Idle);
    assert_eq!(worker.state(), &WorkerState::Idle);

    let invoked_probe = invoked(4, ParentFrame::ContainmentProbe(ContainmentProbe {}));
    assert!(matches!(
        parent.on_send(&invoked_probe),
        Err(ProtocolError::UnexpectedInvocation)
    ));
}

#[test]
fn worker_protocol_outcome_unknown_response_is_invocation_terminal() {
    let mut parent = parent_protocol();
    let mut worker = WorkerProtocol::new(build());

    parent.on_send(&hello()).unwrap();
    worker.on_receive(&hello()).unwrap();
    worker.on_send(&ready()).unwrap();
    parent.on_receive(&ready()).unwrap();
    parent.on_send(&run(2)).unwrap();
    worker.on_receive(&run(2)).unwrap();
    worker.on_send(&effect_request(3, 0)).unwrap();
    parent.on_receive(&effect_request(3, 0)).unwrap();
    parent.on_send(&outcome_unknown_response(4, 0)).unwrap();
    worker.on_receive(&outcome_unknown_response(4, 0)).unwrap();

    assert_eq!(parent.state(), &ParentState::Closed);
    assert_eq!(worker.state(), &WorkerState::Closed);
    assert!(matches!(
        worker.on_send(&effect_request(5, 1)),
        Err(ProtocolError::InvalidTransition { .. })
    ));
}

#[cfg(feature = "skills")]
#[test]
fn worker_protocol_serializes_reusable_skill_calls_without_consuming_effect_ordinals() {
    let mut parent = parent_protocol();
    let mut worker = WorkerProtocol::new(build());
    parent.on_send(&hello()).unwrap();
    worker.on_receive(&hello()).unwrap();
    worker.on_send(&ready()).unwrap();
    parent.on_receive(&ready()).unwrap();
    parent.on_send(&run(2)).unwrap();
    worker.on_receive(&run(2)).unwrap();

    worker.on_send(&skill_call_request(3, 0, 0)).unwrap();
    parent.on_receive(&skill_call_request(3, 0, 0)).unwrap();
    parent.on_send(&skill_call_response(4, 0)).unwrap();
    worker.on_receive(&skill_call_response(4, 0)).unwrap();
    worker.on_send(&effect_request(5, 0)).unwrap();
    parent.on_receive(&effect_request(5, 0)).unwrap();
    parent.on_send(&effect_response(6, 0)).unwrap();
    worker.on_receive(&effect_response(6, 0)).unwrap();
    worker.on_send(&skill_call_request(7, 1, 1)).unwrap();
    parent.on_receive(&skill_call_request(7, 1, 1)).unwrap();
    parent.on_send(&skill_call_response(8, 1)).unwrap();
    worker.on_receive(&skill_call_response(8, 1)).unwrap();
    worker.on_send(&step_result(9)).unwrap();
    parent.on_receive(&step_result(9)).unwrap();
    assert_eq!(parent.state(), &ParentState::Idle);
    assert_eq!(worker.state(), &WorkerState::Idle);

    let mut parent = parent_protocol();
    parent.on_send(&hello()).unwrap();
    parent.on_receive(&ready()).unwrap();
    parent.on_send(&run(2)).unwrap();
    assert!(matches!(
        parent.on_receive(&skill_call_request(3, 1, 0)),
        Err(ProtocolError::SkillCallOrdinal {
            expected: 0,
            actual: 1
        })
    ));
}

#[test]
fn worker_protocol_verify_artifact_complete_transition_table() {
    let mut parent = parent_protocol();
    let mut worker = WorkerProtocol::new(build());
    parent.on_send(&hello()).unwrap();
    worker.on_receive(&hello()).unwrap();
    worker.on_send(&ready()).unwrap();
    parent.on_receive(&ready()).unwrap();
    parent.on_send(&verify(2)).unwrap();
    worker.on_receive(&verify(2)).unwrap();
    worker.on_send(&effect_request(3, 0)).unwrap();
    parent.on_receive(&effect_request(3, 0)).unwrap();
    parent.on_send(&effect_response(4, 0)).unwrap();
    worker.on_receive(&effect_response(4, 0)).unwrap();
    worker.on_send(&verification_result(5)).unwrap();
    parent.on_receive(&verification_result(5)).unwrap();
    assert_eq!(parent.state(), &ParentState::Idle);
    assert_eq!(worker.state(), &WorkerState::Idle);
}

#[test]
fn worker_protocol_rejects_version_build_sequence_replay_gap_and_wrap() {
    let mut parent = parent_protocol();

    let mut wrong_version = hello();
    wrong_version.protocol_version += 1;
    assert!(matches!(
        parent.on_send(&wrong_version),
        Err(ProtocolError::VersionMismatch { .. })
    ));

    let mut wrong_build = hello();
    wrong_build.build_id = BuildIdentity::new("other-build").unwrap();
    assert!(matches!(
        parent.on_send(&wrong_build),
        Err(ProtocolError::BuildMismatch { .. })
    ));

    parent.on_send(&hello()).unwrap();
    let replay = connection(
        0,
        WorkerFrame::Ready(WorkerReady {
            challenge: protocol_challenge(),
        }),
    );
    assert!(matches!(
        parent.on_receive(&replay),
        Err(ProtocolError::Sequence { .. })
    ));
    let gap = connection(
        2,
        WorkerFrame::Ready(WorkerReady {
            challenge: protocol_challenge(),
        }),
    );
    assert!(matches!(
        parent.on_receive(&gap),
        Err(ProtocolError::Sequence { .. })
    ));

    let mut near_wrap = ParentProtocol::with_next_sequence_for_test(build(), u64::MAX);
    let max = connection(
        u64::MAX,
        ParentFrame::Hello(ParentHello {
            challenge: protocol_challenge(),
        }),
    );
    assert_eq!(
        near_wrap.on_send(&max),
        Err(ProtocolError::SequenceExhausted)
    );
}

#[test]
fn worker_protocol_rejects_wrong_invocation_and_effect_identity() {
    let mut parent = parent_protocol();
    parent.on_send(&hello()).unwrap();
    parent.on_receive(&ready()).unwrap();
    parent.on_send(&run(2)).unwrap();

    let wrong_invocation = WireFrame::invocation(
        build(),
        InvocationId::new("other-invocation").unwrap(),
        3,
        WorkerFrame::EffectRequest(Box::new(EffectRequest {
            effect_ordinal: 0,
            grant_id: grant(),
            advisory: AdvisoryAttribution::default(),
            operation: EffectOperation::ReadFile { path: "x".into() },
        })),
    );
    assert!(matches!(
        parent.on_receive(&wrong_invocation),
        Err(ProtocolError::Invocation { .. })
    ));

    parent.on_receive(&effect_request(3, 0)).unwrap();
    assert!(matches!(
        parent.on_receive(&step_result(4)),
        Err(ProtocolError::InvalidTransition { .. })
    ));
    assert!(matches!(
        parent.on_send(&effect_response(4, 1)),
        Err(ProtocolError::EffectOrdinal { .. })
    ));
    parent.on_send(&effect_response(4, 0)).unwrap();

    let mut worker = WorkerProtocol::new(build());
    worker.on_receive(&hello()).unwrap();
    worker.on_send(&ready()).unwrap();
    worker.on_receive(&run(2)).unwrap();
    worker.on_send(&effect_request(3, 0)).unwrap();
    assert!(matches!(
        worker.on_receive(&effect_response(4, 1)),
        Err(ProtocolError::EffectOrdinal { .. })
    ));
    worker.on_receive(&effect_response(4, 0)).unwrap();
}

#[test]
fn worker_protocol_rejects_invalid_terminal_and_alternation_sequences() {
    let mut parent = parent_protocol();
    assert!(matches!(
        parent.on_receive(&step_result(0)),
        Err(ProtocolError::InvalidTransition { .. })
    ));
    parent.on_send(&hello()).unwrap();
    parent.on_receive(&ready()).unwrap();
    parent.on_send(&run(2)).unwrap();
    assert!(matches!(
        parent.on_receive(&verification_result(3)),
        Err(ProtocolError::WrongTerminal { .. })
    ));

    let mut worker = WorkerProtocol::new(build());
    worker.on_receive(&hello()).unwrap();
    worker.on_send(&ready()).unwrap();
    worker.on_receive(&run(2)).unwrap();
    worker.on_send(&effect_request(3, 0)).unwrap();
    assert!(matches!(
        worker.on_send(&effect_request(4, 1)),
        Err(ProtocolError::InvalidTransition { .. })
    ));
    assert!(matches!(
        worker.on_send(&step_result(4)),
        Err(ProtocolError::InvalidTransition { .. })
    ));
}

#[test]
fn worker_protocol_rejects_duplicate_terminal_and_more_than_256_effects() {
    let mut parent = parent_protocol();
    parent.on_send(&hello()).unwrap();
    parent.on_receive(&ready()).unwrap();
    parent.on_send(&run(2)).unwrap();
    parent.on_receive(&step_result(3)).unwrap();
    assert!(matches!(
        parent.on_receive(&step_result(4)),
        Err(ProtocolError::InvalidTransition { .. })
    ));

    let mut worker = WorkerProtocol::new(build());
    worker.on_receive(&hello()).unwrap();
    worker.on_send(&ready()).unwrap();
    worker.on_receive(&run(2)).unwrap();
    for ordinal in 0..MAX_EFFECTS_PER_STEP {
        let request_sequence = 3 + u64::from(ordinal) * 2;
        let response_sequence = request_sequence + 1;
        worker
            .on_send(&effect_request(request_sequence, ordinal))
            .unwrap();
        worker
            .on_receive(&effect_response(response_sequence, ordinal))
            .unwrap();
    }
    let sequence = 3 + u64::from(MAX_EFFECTS_PER_STEP) * 2;
    assert_eq!(
        worker.on_send(&effect_request(sequence, MAX_EFFECTS_PER_STEP)),
        Err(ProtocolError::TooManyEffects {
            maximum: MAX_EFFECTS_PER_STEP,
        })
    );

    let mut parent = parent_protocol();
    parent.on_send(&hello()).unwrap();
    parent.on_receive(&ready()).unwrap();
    parent.on_send(&run(2)).unwrap();
    for ordinal in 0..MAX_EFFECTS_PER_STEP {
        let request_sequence = 3 + u64::from(ordinal) * 2;
        let response_sequence = request_sequence + 1;
        parent
            .on_receive(&effect_request(request_sequence, ordinal))
            .unwrap();
        parent
            .on_send(&effect_response(response_sequence, ordinal))
            .unwrap();
    }
    assert_eq!(
        parent.on_receive(&effect_request(sequence, MAX_EFFECTS_PER_STEP)),
        Err(ProtocolError::TooManyEffects {
            maximum: MAX_EFFECTS_PER_STEP,
        })
    );
}

#[test]
fn worker_protocol_protocol_fault_is_terminal_and_source_free() {
    let fault = ProtocolFault {
        code: ProtocolFaultCode::InvalidState,
        stage: ProtocolStage::Invocation,
    };
    let frame = connection(0, WorkerFrame::ProtocolFault(fault));
    let bytes = encode(&frame);
    let json = std::str::from_utf8(&bytes[4..]).unwrap();
    assert!(!json.contains("source"));
    let value: serde_json::Value = serde_json::from_str(json).unwrap();
    let data = value["message"]["data"].as_object().unwrap();
    assert_eq!(
        data.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["code", "stage"]
    );
}

#[test]
fn worker_protocol_protocol_fault_closes_both_sides() {
    let mut parent = parent_protocol();
    let mut worker = WorkerProtocol::new(build());
    parent.on_send(&hello()).unwrap();
    worker.on_receive(&hello()).unwrap();

    let fault = connection(
        1,
        WorkerFrame::ProtocolFault(ProtocolFault {
            code: ProtocolFaultCode::InvalidState,
            stage: ProtocolStage::Handshake,
        }),
    );
    worker.on_send(&fault).unwrap();
    parent.on_receive(&fault).unwrap();
    assert_eq!(parent.state(), &ParentState::Closed);
    assert_eq!(worker.state(), &WorkerState::Closed);
    let duplicate = connection(
        2,
        WorkerFrame::ProtocolFault(ProtocolFault {
            code: ProtocolFaultCode::InvalidState,
            stage: ProtocolStage::Handshake,
        }),
    );
    assert!(matches!(
        worker.on_send(&duplicate),
        Err(ProtocolError::InvalidTransition { .. })
    ));
    assert!(matches!(
        parent.on_receive(&duplicate),
        Err(ProtocolError::InvalidTransition { .. })
    ));
}
