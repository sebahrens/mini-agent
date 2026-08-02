//! Wire-only types and validation for the brokered JavaScript worker.
//!
//! This module deliberately contains no policy decisions and no QuickJS types. It owns only the
//! bounded frame codec, closed serialized types, connection identity, and the alternating
//! parent/worker state machines.

use std::fmt;
use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub(crate) const PROTOCOL_VERSION: u16 = 1;
pub(crate) const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_EFFECTS_PER_STEP: u32 = 256;
#[cfg(feature = "skills")]
pub(crate) const MAX_SKILL_ARTIFACTS_PER_STEP: usize = 64;
#[cfg(feature = "skills")]
pub(crate) const MAX_SKILL_EXPORTS_PER_ARTIFACT: usize = 32;
#[cfg(feature = "skills")]
pub(crate) const MAX_SKILL_CAPABILITY_GRANTS_PER_STEP: usize = 1024;

const MAX_ID_BYTES: usize = 128;

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub(crate) enum WireIdError {
    #[error("wire identity must not be empty")]
    Empty,
    #[error("wire identity exceeds {maximum} bytes")]
    TooLong { maximum: usize },
    #[error("wire identity contains a disallowed character")]
    InvalidCharacter,
    #[error("grant identity must not be nil")]
    NilGrant,
}

fn validate_text_id(value: &str) -> Result<(), WireIdError> {
    if value.is_empty() {
        return Err(WireIdError::Empty);
    }
    if value.len() > MAX_ID_BYTES {
        return Err(WireIdError::TooLong {
            maximum: MAX_ID_BYTES,
        });
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+' | b':')
    }) {
        return Err(WireIdError::InvalidCharacter);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct InvocationId(String);

impl InvocationId {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, WireIdError> {
        let value = value.into();
        validate_text_id(&value)?;
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for InvocationId {
    type Error = WireIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<InvocationId> for String {
    fn from(value: InvocationId) -> Self {
        value.0
    }
}

impl fmt::Display for InvocationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct BuildIdentity(String);

impl BuildIdentity {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, WireIdError> {
        let value = value.into();
        validate_text_id(&value)?;
        Ok(Self(value))
    }

    pub(crate) fn current() -> Self {
        Self::new(concat!(
            env!("CARGO_PKG_VERSION"),
            "+",
            env!("MINI_AGENT_BUILD_FINGERPRINT")
        ))
        .expect("exact build identity is a valid wire identity")
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for BuildIdentity {
    type Error = WireIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<BuildIdentity> for String {
    fn from(value: BuildIdentity) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(try_from = "Uuid", into = "Uuid")]
pub(crate) struct GrantId(Uuid);

impl GrantId {
    pub(crate) fn new(value: Uuid) -> Result<Self, WireIdError> {
        if value.is_nil() {
            return Err(WireIdError::NilGrant);
        }
        Ok(Self(value))
    }

    pub(crate) fn get(&self) -> Uuid {
        self.0
    }
}

impl TryFrom<Uuid> for GrantId {
    type Error = WireIdError;

    fn try_from(value: Uuid) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<GrantId> for Uuid {
    fn from(value: GrantId) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireFrame<M> {
    pub(crate) protocol_version: u16,
    pub(crate) build_id: BuildIdentity,
    pub(crate) invocation_id: Option<InvocationId>,
    pub(crate) sequence: u64,
    pub(crate) message: M,
}

impl<M> WireFrame<M> {
    pub(crate) fn connection(build_id: BuildIdentity, sequence: u64, message: M) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            build_id,
            invocation_id: None,
            sequence,
            message,
        }
    }

    pub(crate) fn invocation(
        build_id: BuildIdentity,
        invocation_id: InvocationId,
        sequence: u64,
        message: M,
    ) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            build_id,
            invocation_id: Some(invocation_id),
            sequence,
            message,
        }
    }
}

pub(crate) type ParentWireFrame = WireFrame<ParentFrame>;
pub(crate) type WorkerWireFrame = WireFrame<WorkerFrame>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum ParentFrame {
    Hello(ParentHello),
    RunStep(RunStep),
    VerifyArtifact(VerifyArtifact),
    EffectResponse(EffectResponse),
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum WorkerFrame {
    Ready(WorkerReady),
    EffectRequest(EffectRequest),
    StepResult(StepResult),
    VerificationResult(VerificationResult),
    ProtocolFault(ProtocolFault),
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ParentHello {}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerReady {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunStep {
    pub(crate) code: String,
    pub(crate) model_grant_id: Option<GrantId>,
    #[cfg(feature = "skills")]
    pub(crate) proposal_grant_id: Option<GrantId>,
    #[cfg(feature = "skills")]
    pub(crate) artifacts: Vec<super::skills::SkillArtifact>,
    #[cfg(feature = "skills")]
    pub(crate) skill_invocations: Vec<SkillInvocationGrant>,
    #[cfg(feature = "skills")]
    pub(crate) turn_id: String,
    #[cfg(feature = "skills")]
    pub(crate) tool_call_id: String,
}

impl RunStep {
    pub(crate) fn new(code: String) -> Self {
        Self {
            code,
            model_grant_id: None,
            #[cfg(feature = "skills")]
            proposal_grant_id: None,
            #[cfg(feature = "skills")]
            artifacts: Vec::new(),
            #[cfg(feature = "skills")]
            skill_invocations: Vec::new(),
            #[cfg(feature = "skills")]
            turn_id: String::new(),
            #[cfg(feature = "skills")]
            tool_call_id: String::new(),
        }
    }

    #[cfg(feature = "skills")]
    pub(crate) fn with_proposal_grant(mut self, grant_id: GrantId) -> Self {
        self.proposal_grant_id = Some(grant_id);
        self
    }

    pub(crate) fn with_model_grant(mut self, grant_id: GrantId) -> Self {
        self.model_grant_id = Some(grant_id);
        self
    }

    #[cfg(feature = "skills")]
    pub(crate) fn with_skills(
        mut self,
        artifacts: Vec<super::skills::SkillArtifact>,
        skill_invocations: Vec<SkillInvocationGrant>,
        turn_id: String,
        tool_call_id: String,
    ) -> Self {
        self.artifacts = artifacts;
        self.skill_invocations = skill_invocations;
        self.turn_id = turn_id;
        self.tool_call_id = tool_call_id;
        self
    }
}

/// Parent-issued authority for one exact selected ABI-v2 export call.
#[cfg(feature = "skills")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SkillInvocationGrant {
    pub(crate) artifact_id: String,
    pub(crate) export_name: String,
    pub(crate) invocation_id: InvocationId,
    pub(crate) grants: Vec<SkillCapabilityGrant>,
}

#[cfg(feature = "skills")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SkillCapabilityGrant {
    pub(crate) capability: super::skills::HostCapability,
    pub(crate) grant_id: GrantId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerifyArtifact {
    #[cfg(feature = "skills")]
    pub(crate) artifact: super::skills::SkillArtifact,
    #[cfg(not(feature = "skills"))]
    pub(crate) artifact: ArtifactInput,
    pub(crate) cases: Vec<VerificationCase>,
}

#[cfg(not(feature = "skills"))]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArtifactInput {
    /// Advisory only. The parent remains authoritative for persisted identity.
    pub(crate) artifact_id: String,
    pub(crate) source: String,
    pub(crate) exports: Vec<String>,
    pub(crate) tests: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerificationCase {
    pub(crate) case_id: String,
    pub(crate) script: String,
    #[cfg(feature = "skills")]
    pub(crate) kind: VerificationCaseKind,
}

#[cfg(feature = "skills")]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum VerificationCaseKind {
    Embedded,
    Mutation {
        export_name: String,
    },
    Inherited,
    HeldOut {
        expected: VerificationExpectedValue,
        fake_files: std::collections::BTreeMap<String, String>,
    },
}

#[cfg(feature = "skills")]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub(crate) enum VerificationExpectedValue {
    Boolean(bool),
    String(String),
    Integer(i64),
    Float(f64),
    Null,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdvisoryAttribution {
    pub(crate) artifact_id: Option<String>,
    pub(crate) export: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EffectRequest {
    pub(crate) effect_ordinal: u32,
    pub(crate) grant_id: GrantId,
    pub(crate) advisory: AdvisoryAttribution,
    pub(crate) operation: EffectOperation,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum EffectOperation {
    ReadFile {
        path: String,
    },
    WriteFile {
        path: String,
        content: String,
    },
    Fetch {
        url: String,
        method: HttpMethod,
        headers: Vec<HttpHeader>,
        body: Option<String>,
    },
    Spawn {
        program: String,
        arguments: Vec<String>,
    },
    ProposeSkill {
        draft: SkillProposalDraft,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HttpMethod {
    Get,
    Post,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HttpHeader {
    pub(crate) name: String,
    pub(crate) value: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Deserialization input remains capped by the enclosing [`MAX_FRAME_BYTES`]
/// frame. The parent applies the tighter proposal and nested-scope limits via
/// the fallible `JsProposal` conversion before writing audit intent or enqueueing;
/// the proposal frame regression preserves that residual transport bound.
pub(crate) struct SkillProposalDraft {
    pub(crate) source: String,
    pub(crate) description: String,
    pub(crate) exports: Vec<SkillProposalExport>,
    pub(crate) tests: Vec<String>,
    pub(crate) capability: SkillProposalCapability,
    pub(crate) tags: Vec<String>,
    pub(crate) predecessor_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SkillProposalExport {
    pub(crate) name: String,
    pub(crate) signature: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SkillProposalCapability {
    pub(crate) tier: String,
    pub(crate) grants: Vec<SkillProposalScope>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum SkillProposalScope {
    ReadFile {
        workspace_prefixes: Vec<String>,
    },
    WriteFile {
        workspace_prefixes: Vec<String>,
    },
    Fetch {
        origins: Vec<String>,
        methods: Vec<String>,
    },
    Spawn {
        programs: Vec<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EffectResponse {
    pub(crate) effect_ordinal: u32,
    pub(crate) result: EffectResult,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum EffectResult {
    ReadFile {
        content: String,
    },
    WriteFile,
    Fetch {
        status: u16,
        headers: Vec<HttpHeader>,
        body: String,
        truncated: bool,
    },
    Spawn {
        stdout: String,
        stderr: String,
        exit_code: i32,
        timed_out: bool,
        stdout_truncated: bool,
        stderr_truncated: bool,
    },
    ProposalAccepted {
        skill_id: String,
        proposal_id: String,
        status: ProposalStatus,
        report_id: Option<String>,
    },
    Error(EffectError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProposalStatus {
    Pending,
    Verified,
    Rejected,
    AwaitingApproval,
    Approved,
}

impl ProposalStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Verified => "verified",
            Self::Rejected => "rejected",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Approved => "approved",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EffectError {
    pub(crate) code: EffectErrorCode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EffectErrorCode {
    Denied,
    InvalidTarget,
    Cancelled,
    TimedOut,
    OutputLimit,
    BackendFailure,
    AuditFailure,
    OutcomeUnknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StepResult {
    pub(crate) outcome: StepOutcome,
    pub(crate) console: Vec<ConsoleRecord>,
    pub(crate) diagnostic: Option<Diagnostic>,
    #[cfg(feature = "skills")]
    pub(crate) skill_events: Vec<super::skills::telemetry::SkillEvent>,
    #[cfg(feature = "skills")]
    pub(crate) evidence_complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum StepOutcome {
    Value(String),
    Void,
    Error(JsErrorCode),
    Timeout,
    OutOfMemory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JsErrorCode {
    Syntax,
    Exception,
    StackLimit,
    JobLimit,
    InvalidResult,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConsoleRecord {
    pub(crate) level: ConsoleLevel,
    pub(crate) text: String,
    pub(crate) truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConsoleLevel {
    Log,
    Warn,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Diagnostic {
    pub(crate) class: DiagnosticClass,
    pub(crate) stage: DiagnosticStage,
    pub(crate) script_role: ScriptRole,
    pub(crate) line: Option<u32>,
    pub(crate) column: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiagnosticClass {
    Syntax,
    Exception,
    ResourceLimit,
    Contract,
    Internal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiagnosticStage {
    Initialization,
    Evaluation,
    JobDrain,
    ResultConversion,
    Verification,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ScriptRole {
    Model,
    SkillSource,
    EmbeddedTest,
    MutationTest,
    InheritedTest,
    HeldOutTest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerificationResult {
    pub(crate) passed: bool,
    pub(crate) cases: Vec<VerificationCaseResult>,
    pub(crate) loader_version: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerificationCaseResult {
    pub(crate) case_id: String,
    pub(crate) passed: bool,
    pub(crate) diagnostic: Option<Diagnostic>,
    #[cfg(feature = "skills")]
    pub(crate) transcript: super::skills::fakes::FakeTranscript,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProtocolFault {
    pub(crate) code: ProtocolFaultCode,
    pub(crate) stage: ProtocolStage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProtocolFaultCode {
    MalformedFrame,
    VersionMismatch,
    BuildMismatch,
    SequenceMismatch,
    InvalidIdentity,
    InvalidState,
    EffectLimit,
    Internal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProtocolStage {
    Handshake,
    Invocation,
    Effect,
    Terminal,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub(crate) enum FrameError {
    #[error("frame header ended after {read} bytes")]
    TruncatedHeader { read: usize },
    #[error("zero-length frames are invalid")]
    ZeroLength,
    #[error("frame length {length} exceeds maximum {maximum}")]
    FrameTooLarge { length: usize, maximum: usize },
    #[error("frame body ended after {read} of {expected} bytes")]
    TruncatedBody { expected: usize, read: usize },
    #[error("frame contains invalid JSON or an unknown wire field")]
    InvalidJson,
    #[error("frame serialization failed")]
    Serialization,
    #[error("frame I/O failed with {0:?}")]
    Io(io::ErrorKind),
}

struct BoundedPayload {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl BoundedPayload {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            exceeded: false,
        }
    }
}

impl Write for BoundedPayload {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.len() > MAX_FRAME_BYTES.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(io::Error::other("wire frame limit exceeded"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn write_frame<W: Write, M: Serialize>(
    writer: &mut W,
    frame: &M,
) -> Result<(), FrameError> {
    let mut payload = BoundedPayload::new();
    if serde_json::to_writer(&mut payload, frame).is_err() {
        return if payload.exceeded {
            Err(FrameError::FrameTooLarge {
                length: MAX_FRAME_BYTES + 1,
                maximum: MAX_FRAME_BYTES,
            })
        } else {
            Err(FrameError::Serialization)
        };
    }
    if payload.bytes.is_empty() {
        return Err(FrameError::ZeroLength);
    }
    let length = u32::try_from(payload.bytes.len()).map_err(|_| FrameError::FrameTooLarge {
        length: payload.bytes.len(),
        maximum: MAX_FRAME_BYTES,
    })?;
    writer
        .write_all(&length.to_be_bytes())
        .and_then(|()| writer.write_all(&payload.bytes))
        .map_err(|error| FrameError::Io(error.kind()))
}

pub(crate) fn read_frame<R: Read, M: DeserializeOwned>(reader: &mut R) -> Result<M, FrameError> {
    let mut header = [0_u8; 4];
    let header_read = read_until_full(reader, &mut header).map_err(FrameError::Io)?;
    if header_read != header.len() {
        return Err(FrameError::TruncatedHeader { read: header_read });
    }
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 {
        return Err(FrameError::ZeroLength);
    }
    if length > MAX_FRAME_BYTES {
        return Err(FrameError::FrameTooLarge {
            length,
            maximum: MAX_FRAME_BYTES,
        });
    }
    let mut payload = vec![0_u8; length];
    let body_read = read_until_full(reader, &mut payload).map_err(FrameError::Io)?;
    if body_read != length {
        return Err(FrameError::TruncatedBody {
            expected: length,
            read: body_read,
        });
    }
    serde_json::from_slice(&payload).map_err(|_| FrameError::InvalidJson)
}

fn read_until_full(reader: &mut impl Read, buffer: &mut [u8]) -> Result<usize, io::ErrorKind> {
    let mut read = 0;
    while read < buffer.len() {
        match reader.read(&mut buffer[read..]) {
            Ok(0) => break,
            Ok(count) => read += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.kind()),
        }
    }
    Ok(read)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InvocationKind {
    RunStep,
    VerifyArtifact,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ParentState {
    AwaitReady,
    Idle,
    AwaitWorker {
        invocation: InvocationId,
        next_effect: u32,
    },
    AwaitEffectResponseSent {
        invocation: InvocationId,
        effect: u32,
    },
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WorkerState {
    AwaitHello,
    Idle,
    Running {
        invocation: InvocationId,
        next_effect: u32,
    },
    AwaitParentEffect {
        invocation: InvocationId,
        effect: u32,
    },
    Closed,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub(crate) enum ProtocolError {
    #[error("protocol version mismatch: expected {expected}, received {actual}")]
    VersionMismatch { expected: u16, actual: u16 },
    #[error("worker build identity differs from the parent build")]
    BuildMismatch {
        expected: BuildIdentity,
        actual: BuildIdentity,
    },
    #[error("wire sequence mismatch: expected {expected}, received {actual}")]
    Sequence { expected: u64, actual: u64 },
    #[error("wire sequence space is exhausted")]
    SequenceExhausted,
    #[error("frame requires an invocation identity")]
    MissingInvocation,
    #[error("connection frame must not carry an invocation identity")]
    UnexpectedInvocation,
    #[error("frame invocation differs from the active invocation")]
    Invocation {
        expected: InvocationId,
        actual: InvocationId,
    },
    #[error("effect ordinal mismatch: expected {expected}, received {actual}")]
    EffectOrdinal { expected: u32, actual: u32 },
    #[error("invocation exceeded the maximum of {maximum} effects")]
    TooManyEffects { maximum: u32 },
    #[error("terminal result does not match the invocation request")]
    WrongTerminal {
        expected: &'static str,
        actual: &'static str,
    },
    #[error("invalid protocol transition from {state} using {message}")]
    InvalidTransition {
        state: &'static str,
        message: &'static str,
    },
}

pub(crate) struct ParentProtocol {
    expected_build: BuildIdentity,
    next_sequence: u64,
    state: ParentState,
    hello_sent: bool,
    active_kind: Option<InvocationKind>,
}

impl ParentProtocol {
    pub(crate) fn new(expected_build: BuildIdentity) -> Self {
        Self {
            expected_build,
            next_sequence: 0,
            state: ParentState::AwaitReady,
            hello_sent: false,
            active_kind: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_next_sequence_for_test(
        expected_build: BuildIdentity,
        next_sequence: u64,
    ) -> Self {
        Self {
            next_sequence,
            ..Self::new(expected_build)
        }
    }

    pub(crate) fn state(&self) -> &ParentState {
        &self.state
    }

    pub(crate) fn on_send(&mut self, frame: &ParentWireFrame) -> Result<(), ProtocolError> {
        self.validate_header(frame)?;
        let transition = match (&self.state, &frame.message) {
            (ParentState::AwaitReady, ParentFrame::Hello(_)) if !self.hello_sent => {
                require_connection(frame)?;
                self.hello_sent = true;
                None
            }
            (ParentState::Idle, ParentFrame::RunStep(_)) => {
                let invocation = require_invocation(frame)?.clone();
                self.active_kind = Some(InvocationKind::RunStep);
                Some(ParentState::AwaitWorker {
                    invocation,
                    next_effect: 0,
                })
            }
            (ParentState::Idle, ParentFrame::VerifyArtifact(_)) => {
                let invocation = require_invocation(frame)?.clone();
                self.active_kind = Some(InvocationKind::VerifyArtifact);
                Some(ParentState::AwaitWorker {
                    invocation,
                    next_effect: 0,
                })
            }
            (
                ParentState::AwaitEffectResponseSent { invocation, effect },
                ParentFrame::EffectResponse(response),
            ) => {
                validate_invocation(frame, invocation)?;
                validate_effect(*effect, response.effect_ordinal)?;
                Some(ParentState::AwaitWorker {
                    invocation: invocation.clone(),
                    next_effect: effect + 1,
                })
            }
            (ParentState::Idle, ParentFrame::Shutdown) => {
                require_connection(frame)?;
                Some(ParentState::Closed)
            }
            (state, message) => {
                return Err(invalid_parent_transition(state, message));
            }
        };
        if let Some(state) = transition {
            self.state = state;
        }
        self.advance_sequence()
    }

    pub(crate) fn on_receive(&mut self, frame: &WorkerWireFrame) -> Result<(), ProtocolError> {
        self.validate_header(frame)?;
        let transition = match (&self.state, &frame.message) {
            (ParentState::AwaitReady, WorkerFrame::Ready(_)) if self.hello_sent => {
                require_connection(frame)?;
                Some(ParentState::Idle)
            }
            (
                ParentState::AwaitWorker {
                    invocation,
                    next_effect,
                },
                WorkerFrame::EffectRequest(request),
            ) => {
                validate_invocation(frame, invocation)?;
                if *next_effect >= MAX_EFFECTS_PER_STEP {
                    return Err(ProtocolError::TooManyEffects {
                        maximum: MAX_EFFECTS_PER_STEP,
                    });
                }
                validate_effect(*next_effect, request.effect_ordinal)?;
                Some(ParentState::AwaitEffectResponseSent {
                    invocation: invocation.clone(),
                    effect: request.effect_ordinal,
                })
            }
            (ParentState::AwaitWorker { invocation, .. }, WorkerFrame::StepResult(_)) => {
                validate_invocation(frame, invocation)?;
                self.validate_terminal(InvocationKind::RunStep, "step_result")?;
                self.active_kind = None;
                Some(ParentState::Idle)
            }
            (ParentState::AwaitWorker { invocation, .. }, WorkerFrame::VerificationResult(_)) => {
                validate_invocation(frame, invocation)?;
                self.validate_terminal(InvocationKind::VerifyArtifact, "verification_result")?;
                self.active_kind = None;
                Some(ParentState::Idle)
            }
            (ParentState::Closed, message) => {
                return Err(invalid_worker_transition_for_parent(&self.state, message));
            }
            (_, WorkerFrame::ProtocolFault(_)) => {
                validate_fault_invocation(frame, &self.state)?;
                self.active_kind = None;
                Some(ParentState::Closed)
            }
            (state, message) => {
                return Err(invalid_worker_transition_for_parent(state, message));
            }
        };
        if let Some(state) = transition {
            self.state = state;
        }
        self.advance_sequence()
    }

    fn validate_terminal(
        &self,
        actual_kind: InvocationKind,
        actual: &'static str,
    ) -> Result<(), ProtocolError> {
        let expected_kind = self
            .active_kind
            .expect("active state has an invocation kind");
        if expected_kind != actual_kind {
            return Err(ProtocolError::WrongTerminal {
                expected: match expected_kind {
                    InvocationKind::RunStep => "step_result",
                    InvocationKind::VerifyArtifact => "verification_result",
                },
                actual,
            });
        }
        Ok(())
    }

    fn validate_header<M>(&self, frame: &WireFrame<M>) -> Result<(), ProtocolError> {
        validate_header(&self.expected_build, self.next_sequence, frame)
    }

    fn advance_sequence(&mut self) -> Result<(), ProtocolError> {
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(ProtocolError::SequenceExhausted)?;
        Ok(())
    }
}

pub(crate) struct WorkerProtocol {
    expected_build: BuildIdentity,
    next_sequence: u64,
    state: WorkerState,
    hello_received: bool,
    active_kind: Option<InvocationKind>,
}

impl WorkerProtocol {
    pub(crate) fn new(expected_build: BuildIdentity) -> Self {
        Self {
            expected_build,
            next_sequence: 0,
            state: WorkerState::AwaitHello,
            hello_received: false,
            active_kind: None,
        }
    }

    pub(crate) fn state(&self) -> &WorkerState {
        &self.state
    }

    pub(crate) fn on_receive(&mut self, frame: &ParentWireFrame) -> Result<(), ProtocolError> {
        self.validate_header(frame)?;
        let transition = match (&self.state, &frame.message) {
            (WorkerState::AwaitHello, ParentFrame::Hello(_)) if !self.hello_received => {
                require_connection(frame)?;
                self.hello_received = true;
                None
            }
            (WorkerState::Idle, ParentFrame::RunStep(_)) => {
                let invocation = require_invocation(frame)?.clone();
                self.active_kind = Some(InvocationKind::RunStep);
                Some(WorkerState::Running {
                    invocation,
                    next_effect: 0,
                })
            }
            (WorkerState::Idle, ParentFrame::VerifyArtifact(_)) => {
                let invocation = require_invocation(frame)?.clone();
                self.active_kind = Some(InvocationKind::VerifyArtifact);
                Some(WorkerState::Running {
                    invocation,
                    next_effect: 0,
                })
            }
            (
                WorkerState::AwaitParentEffect { invocation, effect },
                ParentFrame::EffectResponse(response),
            ) => {
                validate_invocation(frame, invocation)?;
                validate_effect(*effect, response.effect_ordinal)?;
                Some(WorkerState::Running {
                    invocation: invocation.clone(),
                    next_effect: effect + 1,
                })
            }
            (WorkerState::Idle, ParentFrame::Shutdown) => {
                require_connection(frame)?;
                Some(WorkerState::Closed)
            }
            (state, message) => return Err(invalid_parent_transition_for_worker(state, message)),
        };
        if let Some(state) = transition {
            self.state = state;
        }
        self.advance_sequence()
    }

    pub(crate) fn on_send(&mut self, frame: &WorkerWireFrame) -> Result<(), ProtocolError> {
        self.validate_header(frame)?;
        let transition = match (&self.state, &frame.message) {
            (WorkerState::AwaitHello, WorkerFrame::Ready(_)) if self.hello_received => {
                require_connection(frame)?;
                Some(WorkerState::Idle)
            }
            (
                WorkerState::Running {
                    invocation,
                    next_effect,
                },
                WorkerFrame::EffectRequest(request),
            ) => {
                validate_invocation(frame, invocation)?;
                if *next_effect >= MAX_EFFECTS_PER_STEP {
                    return Err(ProtocolError::TooManyEffects {
                        maximum: MAX_EFFECTS_PER_STEP,
                    });
                }
                validate_effect(*next_effect, request.effect_ordinal)?;
                Some(WorkerState::AwaitParentEffect {
                    invocation: invocation.clone(),
                    effect: request.effect_ordinal,
                })
            }
            (WorkerState::Running { invocation, .. }, WorkerFrame::StepResult(_)) => {
                validate_invocation(frame, invocation)?;
                self.validate_terminal(InvocationKind::RunStep, "step_result")?;
                self.active_kind = None;
                Some(WorkerState::Idle)
            }
            (WorkerState::Running { invocation, .. }, WorkerFrame::VerificationResult(_)) => {
                validate_invocation(frame, invocation)?;
                self.validate_terminal(InvocationKind::VerifyArtifact, "verification_result")?;
                self.active_kind = None;
                Some(WorkerState::Idle)
            }
            (WorkerState::Closed, message) => {
                return Err(invalid_worker_transition(&self.state, message));
            }
            (_, WorkerFrame::ProtocolFault(_)) => {
                validate_fault_invocation(frame, &self.state)?;
                self.active_kind = None;
                Some(WorkerState::Closed)
            }
            (state, message) => return Err(invalid_worker_transition(state, message)),
        };
        if let Some(state) = transition {
            self.state = state;
        }
        self.advance_sequence()
    }

    fn validate_terminal(
        &self,
        actual_kind: InvocationKind,
        actual: &'static str,
    ) -> Result<(), ProtocolError> {
        let expected_kind = self
            .active_kind
            .expect("active state has an invocation kind");
        if expected_kind != actual_kind {
            return Err(ProtocolError::WrongTerminal {
                expected: match expected_kind {
                    InvocationKind::RunStep => "step_result",
                    InvocationKind::VerifyArtifact => "verification_result",
                },
                actual,
            });
        }
        Ok(())
    }

    fn validate_header<M>(&self, frame: &WireFrame<M>) -> Result<(), ProtocolError> {
        validate_header(&self.expected_build, self.next_sequence, frame)
    }

    fn advance_sequence(&mut self) -> Result<(), ProtocolError> {
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(ProtocolError::SequenceExhausted)?;
        Ok(())
    }
}

fn validate_header<M>(
    expected_build: &BuildIdentity,
    expected_sequence: u64,
    frame: &WireFrame<M>,
) -> Result<(), ProtocolError> {
    if frame.protocol_version != PROTOCOL_VERSION {
        return Err(ProtocolError::VersionMismatch {
            expected: PROTOCOL_VERSION,
            actual: frame.protocol_version,
        });
    }
    if &frame.build_id != expected_build {
        return Err(ProtocolError::BuildMismatch {
            expected: expected_build.clone(),
            actual: frame.build_id.clone(),
        });
    }
    if frame.sequence != expected_sequence {
        return Err(ProtocolError::Sequence {
            expected: expected_sequence,
            actual: frame.sequence,
        });
    }
    if expected_sequence == u64::MAX {
        return Err(ProtocolError::SequenceExhausted);
    }
    Ok(())
}

fn require_connection<M>(frame: &WireFrame<M>) -> Result<(), ProtocolError> {
    if frame.invocation_id.is_some() {
        Err(ProtocolError::UnexpectedInvocation)
    } else {
        Ok(())
    }
}

fn require_invocation<M>(frame: &WireFrame<M>) -> Result<&InvocationId, ProtocolError> {
    frame
        .invocation_id
        .as_ref()
        .ok_or(ProtocolError::MissingInvocation)
}

fn validate_invocation<M>(
    frame: &WireFrame<M>,
    expected: &InvocationId,
) -> Result<(), ProtocolError> {
    let actual = require_invocation(frame)?;
    if actual != expected {
        return Err(ProtocolError::Invocation {
            expected: expected.clone(),
            actual: actual.clone(),
        });
    }
    Ok(())
}

fn validate_effect(expected: u32, actual: u32) -> Result<(), ProtocolError> {
    if expected != actual {
        Err(ProtocolError::EffectOrdinal { expected, actual })
    } else {
        Ok(())
    }
}

fn validate_fault_invocation<M, S>(frame: &WireFrame<M>, state: &S) -> Result<(), ProtocolError>
where
    S: ActiveInvocation,
{
    match state.active_invocation() {
        Some(invocation) => validate_invocation(frame, invocation),
        None => require_connection(frame),
    }
}

trait ActiveInvocation {
    fn active_invocation(&self) -> Option<&InvocationId>;
}

impl ActiveInvocation for ParentState {
    fn active_invocation(&self) -> Option<&InvocationId> {
        match self {
            Self::AwaitWorker { invocation, .. }
            | Self::AwaitEffectResponseSent { invocation, .. } => Some(invocation),
            Self::AwaitReady | Self::Idle | Self::Closed => None,
        }
    }
}

impl ActiveInvocation for WorkerState {
    fn active_invocation(&self) -> Option<&InvocationId> {
        match self {
            Self::Running { invocation, .. } | Self::AwaitParentEffect { invocation, .. } => {
                Some(invocation)
            }
            Self::AwaitHello | Self::Idle | Self::Closed => None,
        }
    }
}

fn parent_state_name(state: &ParentState) -> &'static str {
    match state {
        ParentState::AwaitReady => "await_ready",
        ParentState::Idle => "idle",
        ParentState::AwaitWorker { .. } => "await_worker",
        ParentState::AwaitEffectResponseSent { .. } => "await_effect_response_sent",
        ParentState::Closed => "closed",
    }
}

fn worker_state_name(state: &WorkerState) -> &'static str {
    match state {
        WorkerState::AwaitHello => "await_hello",
        WorkerState::Idle => "idle",
        WorkerState::Running { .. } => "running",
        WorkerState::AwaitParentEffect { .. } => "await_parent_effect",
        WorkerState::Closed => "closed",
    }
}

fn parent_message_name(message: &ParentFrame) -> &'static str {
    match message {
        ParentFrame::Hello(_) => "hello",
        ParentFrame::RunStep(_) => "run_step",
        ParentFrame::VerifyArtifact(_) => "verify_artifact",
        ParentFrame::EffectResponse(_) => "effect_response",
        ParentFrame::Shutdown => "shutdown",
    }
}

fn worker_message_name(message: &WorkerFrame) -> &'static str {
    match message {
        WorkerFrame::Ready(_) => "ready",
        WorkerFrame::EffectRequest(_) => "effect_request",
        WorkerFrame::StepResult(_) => "step_result",
        WorkerFrame::VerificationResult(_) => "verification_result",
        WorkerFrame::ProtocolFault(_) => "protocol_fault",
    }
}

fn invalid_parent_transition(state: &ParentState, message: &ParentFrame) -> ProtocolError {
    ProtocolError::InvalidTransition {
        state: parent_state_name(state),
        message: parent_message_name(message),
    }
}

fn invalid_worker_transition_for_parent(
    state: &ParentState,
    message: &WorkerFrame,
) -> ProtocolError {
    ProtocolError::InvalidTransition {
        state: parent_state_name(state),
        message: worker_message_name(message),
    }
}

fn invalid_parent_transition_for_worker(
    state: &WorkerState,
    message: &ParentFrame,
) -> ProtocolError {
    ProtocolError::InvalidTransition {
        state: worker_state_name(state),
        message: parent_message_name(message),
    }
}

fn invalid_worker_transition(state: &WorkerState, message: &WorkerFrame) -> ProtocolError {
    ProtocolError::InvalidTransition {
        state: worker_state_name(state),
        message: worker_message_name(message),
    }
}
