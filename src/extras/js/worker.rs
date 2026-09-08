//! Synchronous bootstrap and fresh-runtime execution for the brokered JavaScript worker.
//!
//! Every request owns its QuickJS [`Runtime`] and [`Context`]. Neither is stored in worker state,
//! and every JavaScript value is converted to a bounded, closed Rust protocol value before the
//! terminal frame is written. The only global installed here is a bounded `console`; authority
//! globals and module loaders are deliberately absent.

use std::io::Write;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rquickjs::context::EvalOptions;
use rquickjs::prelude::Opt;
use rquickjs::promise::PromiseState;
use rquickjs::{
    Context, Ctx, Error, Exception, Function, IntoJs, Module, Object, Persistent, Runtime, Value,
    WriteOptions,
};

use super::protocol::{
    AdvisoryAttribution, BuildIdentity, ConsoleLevel, ConsoleRecord, Diagnostic, DiagnosticClass,
    DiagnosticStage, DirectoryEntry, DirectoryEntryKind, EffectErrorCode, EffectOperation,
    EffectRequest, EffectResponse, EffectResult, GrepMatch, GrepOptions, JsErrorCode,
    JsExceptionClass, ModelEffectProfile, ParentFrame, ParentWireFrame, ProtocolError,
    ProtocolFault, ProtocolFaultCode, ProtocolStage, RunStep, ScriptRole, StepOutcome, StepResult,
    VerificationCaseResult, VerificationResult, VerifyArtifact, WireFrame, WorkerFrame,
    WorkerProtocol, WorkerWireFrame, read_frame, source_position_is_valid, write_frame,
};
#[cfg(feature = "sandbox")]
use super::protocol::{HttpHeader, HttpMethod};
#[cfg(feature = "skills")]
use super::protocol::{
    InvocationId, MAX_SKILL_ARTIFACTS_PER_STEP, MAX_SKILL_CALLS_PER_STEP,
    MAX_SKILL_CAPABILITY_GRANTS_PER_STEP, MAX_SKILL_EXPORTS_PER_ARTIFACT, SkillCallRequest,
    SkillCallResponse, SkillInvocationGrant,
};
use super::session::{SCRATCH_KEY_MAX_BYTES, SCRATCH_VALUE_MAX_BYTES, STRUCTURED_RESULT_MAX_BYTES};
use super::types::{
    DISCOVERY_PATTERN_MAX_BYTES, MEMORY_LIMIT, READ_FILE_MAX_BYTES, READ_FILES_MAX_PATH_BYTES,
    READ_FILES_MAX_PATHS, STACK_LIMIT, STEP_TIMEOUT, WRITE_FILE_MAX_BYTES,
};
#[cfg(feature = "skills")]
use crate::extras::js::skills::capability::{InvocationAuthorization, InvocationCapabilityRuntime};
#[cfg(feature = "skills")]
use crate::extras::js::skills::telemetry::{SkillEvent, SkillEventKind, stable_invocation_id};
use crate::sandbox::worker::{
    INTERNAL_WORKER_MARKER, INTERNAL_WORKER_MARKER_VALUE, finalize_internal_worker,
    is_internal_worker_marker_present, standard_streams_are_protocol_pipes,
};

const EXIT_FAILURE: i32 = 1;
const MAX_PENDING_JOBS: usize = 10_000;
const MAX_RESULT_BYTES: usize = 64 * 1024;
const MAX_CONSOLE_RECORDS: usize = 256;
const MAX_CONSOLE_BYTES: usize = 256 * 1024;
const MAX_CONSOLE_RECORD_BYTES: usize = 8 * 1024;
const MAX_VERIFICATION_CASES: usize = 4_096;
const MAX_VERIFICATION_CASE_ID_BYTES: usize = 128;
const VERIFICATION_LOADER_VERSION: u16 = 1;
const EFFECT_PATH_MAX_BYTES: usize = READ_FILE_MAX_BYTES;
const SPAWN_ARGUMENT_MAX_COUNT: usize = 4_096;
const SPAWN_ARGUMENTS_MAX_BYTES: usize = 1024 * 1024;
#[cfg(feature = "sandbox")]
const FETCH_URL_MAX_BYTES: usize = 64 * 1024;
#[cfg(feature = "sandbox")]
const FETCH_REQUEST_HEADER_MAX_COUNT: usize = 64;
#[cfg(feature = "sandbox")]
const FETCH_REQUEST_HEADER_MAX_BYTES: usize = 16 * 1024;
#[cfg(feature = "sandbox")]
const FETCH_REQUEST_BODY_MAX_BYTES: usize = 256 * 1024;

type ModelEffectDispatcher = Rc<dyn Fn(EffectOperation) -> EffectResult>;
type WorkerEffectDispatcher = Arc<
    dyn Fn(super::protocol::GrantId, AdvisoryAttribution, EffectOperation) -> EffectResult
        + Send
        + Sync,
>;
#[cfg(feature = "skills")]
type WorkerSkillCallAuthorizer =
    Arc<dyn Fn(String, String, u32) -> Result<SkillInvocationGrant, ()> + Send + Sync>;

#[cfg(feature = "skills")]
#[derive(Clone)]
struct WorkerEventMetadata {
    skill_id: String,
    export_name: String,
    turn_id: String,
    tool_call_id: String,
}

#[cfg(feature = "skills")]
#[derive(Default)]
struct WorkerEventState {
    events: Vec<SkillEvent>,
    pending: std::collections::HashMap<String, (WorkerEventMetadata, Instant)>,
}

#[cfg(feature = "skills")]
impl WorkerEventState {
    fn injected(&mut self, skill_id: String, turn_id: String, tool_call_id: String) {
        self.events.push(worker_event(
            skill_id,
            turn_id,
            tool_call_id,
            None,
            None,
            SkillEventKind::Injected,
            None,
            None,
            None,
        ));
    }

    fn start(&mut self, id: String, metadata: WorkerEventMetadata, shape: String) {
        self.pending
            .insert(id.clone(), (metadata.clone(), Instant::now()));
        self.events.push(worker_event(
            metadata.skill_id,
            metadata.turn_id,
            metadata.tool_call_id,
            Some(id),
            Some(metadata.export_name),
            SkillEventKind::Invoked,
            None,
            None,
            Some(shape),
        ));
    }

    fn terminal(&mut self, id: &str, success: bool) {
        let Some((metadata, started)) = self.pending.remove(id) else {
            return;
        };
        self.events.push(worker_event(
            metadata.skill_id,
            metadata.turn_id,
            metadata.tool_call_id,
            Some(id.to_string()),
            Some(metadata.export_name),
            if success {
                SkillEventKind::Returned
            } else {
                SkillEventKind::Threw
            },
            Some(if success { "fulfilled" } else { "exception" }.into()),
            Some(started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64),
            None,
        ));
    }

    fn finalize_pending(&mut self, outcome: &StepOutcome) {
        let pending = self.pending.keys().cloned().collect::<Vec<_>>();
        for id in pending {
            let Some((metadata, started)) = self.pending.remove(&id) else {
                continue;
            };
            let (kind, code) = match outcome {
                StepOutcome::Timeout => (SkillEventKind::TimedOut, "step_timeout"),
                StepOutcome::OutOfMemory => (SkillEventKind::Oom, "step_oom"),
                _ => (SkillEventKind::Threw, "step_failed"),
            };
            self.events.push(worker_event(
                metadata.skill_id,
                metadata.turn_id,
                metadata.tool_call_id,
                Some(id),
                Some(metadata.export_name),
                kind,
                Some(code.into()),
                Some(started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64),
                None,
            ));
        }
    }
}

#[cfg(feature = "skills")]
#[allow(clippy::too_many_arguments)]
fn worker_event(
    skill_id: String,
    turn_id: String,
    tool_call_id: String,
    invocation_id: Option<String>,
    export_name: Option<String>,
    kind: SkillEventKind,
    outcome: Option<String>,
    latency_us: Option<u64>,
    argument_shape: Option<String>,
) -> SkillEvent {
    SkillEvent {
        invocation_id,
        skill_id,
        turn_id,
        tool_call_id: Some(tool_call_id),
        kind,
        export_name,
        outcome,
        latency_us,
        retrieval_score: None,
        retrieval_rank: None,
        query_fingerprint: None,
        index_generation: 0,
        evidence_complete: false,
        production: false,
        argument_shape,
        created_at: 0,
    }
}

struct WorkerSpawnResult {
    stdout: String,
    stderr: String,
    code: i32,
    timed_out: bool,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

impl<'js> IntoJs<'js> for WorkerSpawnResult {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        let object = Object::new(ctx.clone())?;
        object.set("stdout", self.stdout)?;
        object.set("stderr", self.stderr)?;
        object.set("code", self.code)?;
        object.set("timed_out", self.timed_out)?;
        object.set("stdout_truncated", self.stdout_truncated)?;
        object.set("stderr_truncated", self.stderr_truncated)?;
        Ok(object.into())
    }
}

#[cfg(feature = "sandbox")]
struct WorkerFetchResult {
    status: u16,
    text: String,
}

#[cfg(feature = "sandbox")]
impl<'js> IntoJs<'js> for WorkerFetchResult {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        let object = Object::new(ctx.clone())?;
        object.set("status", self.status)?;
        object.set("text", self.text)?;
        Ok(object.into())
    }
}

struct WorkerDirectoryEntry(DirectoryEntry);

impl<'js> IntoJs<'js> for WorkerDirectoryEntry {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        let object = Object::new(ctx.clone())?;
        object.set("name", self.0.name)?;
        object.set(
            "kind",
            match self.0.kind {
                DirectoryEntryKind::File => "file",
                DirectoryEntryKind::Directory => "directory",
            },
        )?;
        object.set("size", self.0.size)?;
        Ok(object.into())
    }
}

struct WorkerListDirResult {
    entries: Vec<DirectoryEntry>,
    truncated: bool,
}

impl<'js> IntoJs<'js> for WorkerListDirResult {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        let object = Object::new(ctx.clone())?;
        object.set(
            "entries",
            self.entries
                .into_iter()
                .map(WorkerDirectoryEntry)
                .collect::<Vec<_>>(),
        )?;
        object.set("truncated", self.truncated)?;
        Ok(object.into())
    }
}

struct WorkerGlobResult {
    paths: Vec<String>,
    truncated: bool,
}

impl<'js> IntoJs<'js> for WorkerGlobResult {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        let object = Object::new(ctx.clone())?;
        object.set("paths", self.paths)?;
        object.set("truncated", self.truncated)?;
        Ok(object.into())
    }
}

struct WorkerGrepMatch(GrepMatch);

impl<'js> IntoJs<'js> for WorkerGrepMatch {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        let object = Object::new(ctx.clone())?;
        object.set("path", self.0.path)?;
        object.set("line", self.0.line)?;
        object.set("text", self.0.text)?;
        Ok(object.into())
    }
}

struct WorkerGrepResult {
    matches: Vec<GrepMatch>,
    truncated: bool,
}

impl<'js> IntoJs<'js> for WorkerGrepResult {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        let object = Object::new(ctx.clone())?;
        object.set(
            "matches",
            self.matches
                .into_iter()
                .map(WorkerGrepMatch)
                .collect::<Vec<_>>(),
        )?;
        object.set("truncated", self.truncated)?;
        Ok(object.into())
    }
}

fn install_model_effect_globals(
    context: &Context,
    effects: ModelEffectDispatcher,
    spawn_available: bool,
    profile: ModelEffectProfile,
) -> rquickjs::Result<()> {
    context.with(|ctx| {
        let read_effects = effects.clone();
        let read_file = Function::new(ctx.clone(), move |ctx: Ctx<'_>, path: String| {
            validate_path(&path).map_err(|code| effect_error(&ctx, "read_file", code))?;
            match read_effects(EffectOperation::ReadFile { path }) {
                EffectResult::ReadFile { content } => Ok(content),
                EffectResult::Error(error) => Err(effect_error(&ctx, "read_file", error.code)),
                _ => Err(rquickjs::Error::Unknown),
            }
        })?;
        let read_many_effects = effects.clone();
        let read_files = Function::new(ctx.clone(), move |ctx: Ctx<'_>, paths: Vec<String>| {
            validate_read_files_paths(&paths)
                .map_err(|code| effect_error(&ctx, "read_files", code))?;
            match read_many_effects(EffectOperation::ReadFiles { paths }) {
                EffectResult::ReadFiles { contents } => Ok(contents),
                EffectResult::Error(error) => Err(effect_error(&ctx, "read_files", error.code)),
                _ => Err(rquickjs::Error::Unknown),
            }
        })?;
        let list_effects = effects.clone();
        let list_dir = Function::new(ctx.clone(), move |ctx: Ctx<'_>, path: Opt<String>| {
            let path = path.0.unwrap_or_else(|| ".".to_string());
            validate_path(&path).map_err(|code| effect_error(&ctx, "list_dir", code))?;
            match list_effects(EffectOperation::ListDir { path }) {
                EffectResult::ListDir { entries, truncated } => {
                    Ok(WorkerListDirResult { entries, truncated })
                }
                EffectResult::Error(error) => Err(effect_error(&ctx, "list_dir", error.code)),
                _ => Err(rquickjs::Error::Unknown),
            }
        })?;
        let glob_effects = effects.clone();
        let glob = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>, pattern: String, options: Opt<Object<'_>>| {
                validate_discovery_pattern(&pattern)
                    .map_err(|code| effect_error(&ctx, "glob", code))?;
                let path = parse_glob_options(options.0.as_ref())
                    .map_err(|code| effect_error(&ctx, "glob", code))?;
                match glob_effects(EffectOperation::Glob { path, pattern }) {
                    EffectResult::Glob { paths, truncated } => {
                        Ok(WorkerGlobResult { paths, truncated })
                    }
                    EffectResult::Error(error) => Err(effect_error(&ctx, "glob", error.code)),
                    _ => Err(rquickjs::Error::Unknown),
                }
            },
        )?;
        let grep_effects = effects.clone();
        let grep = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>, pattern: String, options: Opt<Object<'_>>| {
                validate_discovery_pattern(&pattern)
                    .map_err(|code| effect_error(&ctx, "grep", code))?;
                let (path, options) = parse_grep_options(options.0.as_ref())
                    .map_err(|code| effect_error(&ctx, "grep", code))?;
                match grep_effects(EffectOperation::Grep {
                    path,
                    pattern,
                    options,
                }) {
                    EffectResult::Grep { matches, truncated } => {
                        Ok(WorkerGrepResult { matches, truncated })
                    }
                    EffectResult::Error(error) => Err(effect_error(&ctx, "grep", error.code)),
                    _ => Err(rquickjs::Error::Unknown),
                }
            },
        )?;
        let write_effects = effects.clone();
        let write_file = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>, path: String, content: String| {
                validate_path(&path).map_err(|code| effect_error(&ctx, "write_file", code))?;
                if content.len() > WRITE_FILE_MAX_BYTES {
                    return Err(effect_error(&ctx, "write_file", EffectErrorCode::TooLarge));
                }
                match write_effects(EffectOperation::WriteFile { path, content }) {
                    EffectResult::WriteFile => Ok(()),
                    EffectResult::Error(error) => Err(effect_error(&ctx, "write_file", error.code)),
                    _ => Err(rquickjs::Error::Unknown),
                }
            },
        )?;
        #[cfg(feature = "sandbox")]
        let fetch = {
            let fetch_effects = effects.clone();
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'_>, url: String, options: Opt<Object<'_>>| {
                    if url.is_empty() || url.contains('\0') || url.len() > FETCH_URL_MAX_BYTES {
                        return Err(effect_error(&ctx, "fetch", EffectErrorCode::InvalidTarget));
                    }
                    let (method, headers, body) = parse_fetch_options(options.0.as_ref())?;
                    match fetch_effects(EffectOperation::Fetch {
                        url,
                        method,
                        headers,
                        body,
                    }) {
                        EffectResult::Fetch { status, body, .. } => {
                            Ok(WorkerFetchResult { status, text: body })
                        }
                        EffectResult::Error(error) => Err(effect_error(&ctx, "fetch", error.code)),
                        _ => Err(rquickjs::Error::Unknown),
                    }
                },
            )?
        };
        ctx.globals().set("read_file", read_file)?;
        ctx.globals().set("list_dir", list_dir)?;
        ctx.globals().set("grep", grep)?;
        if profile == ModelEffectProfile::Full {
            ctx.globals().set("read_files", read_files)?;
            ctx.globals().set("glob", glob)?;
            ctx.globals().set("write_file", write_file)?;
        }
        if profile == ModelEffectProfile::Full && spawn_available {
            let spawn_effects = effects.clone();
            let spawn = Function::new(
                ctx.clone(),
                move |ctx: Ctx<'_>, program: String, arguments: Vec<String>| {
                    validate_spawn(&program, &arguments)
                        .map_err(|code| effect_error(&ctx, "spawn", code))?;
                    match spawn_effects(EffectOperation::Spawn { program, arguments }) {
                        EffectResult::Spawn {
                            stdout,
                            stderr,
                            exit_code,
                            timed_out,
                            stdout_truncated,
                            stderr_truncated,
                        } => Ok(WorkerSpawnResult {
                            stdout,
                            stderr,
                            code: exit_code,
                            timed_out,
                            stdout_truncated,
                            stderr_truncated,
                        }),
                        EffectResult::Error(error) => Err(effect_error(&ctx, "spawn", error.code)),
                        _ => Err(rquickjs::Error::Unknown),
                    }
                },
            )?;
            ctx.globals().set("spawn", spawn)?;
        }
        #[cfg(feature = "sandbox")]
        if profile == ModelEffectProfile::Full {
            ctx.globals().set("fetch", fetch)?;
        }
        Ok(())
    })
}

fn install_session_state_globals(
    context: &Context,
    effects: ModelEffectDispatcher,
    result_wrapper: Persistent<Function<'static>>,
    scratch_put_wrapper: Persistent<Function<'static>>,
    scratch_get_wrapper: Persistent<Function<'static>>,
) -> rquickjs::Result<()> {
    context.with(|ctx| {
        let result_effects = effects.clone();
        let result_dispatch = Function::new(ctx.clone(), move |ctx: Ctx<'_>, json: String| {
            if json.len() > STRUCTURED_RESULT_MAX_BYTES {
                return Err(effect_error(&ctx, "result", EffectErrorCode::TooLarge));
            }
            match result_effects(EffectOperation::Result { json }) {
                EffectResult::ResultAccepted { .. } => Ok(()),
                EffectResult::Error(error) => Err(effect_error(&ctx, "result", error.code)),
                _ => Err(rquickjs::Error::Unknown),
            }
        })?;
        let result: Function = result_wrapper.restore(&ctx)?.call((result_dispatch,))?;

        let put_effects = effects.clone();
        let scratch_put_dispatch = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>, key: String, json: String| {
                validate_scratch_key_worker(&key)
                    .map_err(|code| effect_error(&ctx, "scratch_put", code))?;
                if json.len() > SCRATCH_VALUE_MAX_BYTES {
                    return Err(effect_error(&ctx, "scratch_put", EffectErrorCode::TooLarge));
                }
                match put_effects(EffectOperation::ScratchPut { key, json }) {
                    EffectResult::ScratchPut => Ok(()),
                    EffectResult::Error(error) => {
                        Err(effect_error(&ctx, "scratch_put", error.code))
                    }
                    _ => Err(rquickjs::Error::Unknown),
                }
            },
        )?;
        let scratch_put: Function = scratch_put_wrapper
            .restore(&ctx)?
            .call((scratch_put_dispatch,))?;

        let scratch_get_dispatch = Function::new(ctx.clone(), move |ctx: Ctx<'_>, key: String| {
            validate_scratch_key_worker(&key)
                .map_err(|code| effect_error(&ctx, "scratch_get", code))?;
            match effects(EffectOperation::ScratchGet { key }) {
                EffectResult::ScratchGet { json } => Ok(json),
                EffectResult::Error(error) => Err(effect_error(&ctx, "scratch_get", error.code)),
                _ => Err(rquickjs::Error::Unknown),
            }
        })?;
        let scratch_get: Function = scratch_get_wrapper
            .restore(&ctx)?
            .call((scratch_get_dispatch,))?;

        ctx.globals().set("result", result)?;
        ctx.globals().set("scratch_put", scratch_put)?;
        ctx.globals().set("scratch_get", scratch_get)?;
        Ok(())
    })
}

fn validate_scratch_key_worker(key: &str) -> Result<(), EffectErrorCode> {
    if key.is_empty()
        || key.len() > SCRATCH_KEY_MAX_BYTES
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
    {
        return Err(EffectErrorCode::InvalidTarget);
    }
    Ok(())
}

#[cfg(feature = "skills")]
fn install_proposal_global(
    context: &Context,
    effects: ModelEffectDispatcher,
) -> rquickjs::Result<()> {
    context.with(|ctx| {
        let propose_skill = Function::new(ctx.clone(), move |ctx: Ctx<'_>, draft: Object<'_>| {
            let proposal = super::skills::proposal::JsProposal::from_object(&draft)
                .map_err(|_| effect_error(&ctx, "propose_skill", EffectErrorCode::InvalidTarget))?;
            match effects(EffectOperation::ProposeSkill {
                draft: proposal.into(),
            }) {
                EffectResult::ProposalAccepted {
                    skill_id,
                    proposal_id,
                    status,
                    report_id,
                } => serde_json::to_string(&serde_json::json!({
                    "id": skill_id,
                    "proposal_id": proposal_id,
                    "status": status.as_str(),
                    "report_id": report_id,
                }))
                .map_err(|_| effect_error(&ctx, "propose_skill", EffectErrorCode::BackendFailure)),
                EffectResult::Error(error) => Err(effect_error(&ctx, "propose_skill", error.code)),
                _ => Err(rquickjs::Error::Unknown),
            }
        })?;
        ctx.globals().set("propose_skill", propose_skill)
    })
}

fn validate_path(path: &str) -> Result<(), EffectErrorCode> {
    if path.is_empty() || path.contains('\0') || path.len() > EFFECT_PATH_MAX_BYTES {
        Err(EffectErrorCode::InvalidTarget)
    } else {
        Ok(())
    }
}

fn validate_read_files_paths(paths: &[String]) -> Result<(), EffectErrorCode> {
    if paths.is_empty() {
        return Err(EffectErrorCode::InvalidTarget);
    }
    if paths.len() > READ_FILES_MAX_PATHS {
        return Err(EffectErrorCode::TooLarge);
    }
    let mut total = 0_usize;
    for path in paths {
        validate_path(path)?;
        total = total
            .checked_add(path.len())
            .ok_or(EffectErrorCode::TooLarge)?;
        if total > READ_FILES_MAX_PATH_BYTES {
            return Err(EffectErrorCode::TooLarge);
        }
    }
    Ok(())
}

fn validate_discovery_pattern(pattern: &str) -> Result<(), EffectErrorCode> {
    if pattern.is_empty() || pattern.contains('\0') || pattern.len() > DISCOVERY_PATTERN_MAX_BYTES {
        Err(EffectErrorCode::InvalidTarget)
    } else {
        Ok(())
    }
}

fn parse_glob_options(options: Option<&Object<'_>>) -> Result<String, EffectErrorCode> {
    let Some(options) = options else {
        return Ok(".".to_string());
    };
    for key in options.keys::<String>() {
        let key = key.map_err(|_| EffectErrorCode::InvalidTarget)?;
        if key != "path" {
            return Err(EffectErrorCode::InvalidTarget);
        }
    }
    let path = options
        .get::<_, Option<String>>("path")
        .map_err(|_| EffectErrorCode::InvalidTarget)?
        .unwrap_or_else(|| ".".to_string());
    validate_path(&path)?;
    Ok(path)
}

fn parse_grep_options(
    options: Option<&Object<'_>>,
) -> Result<(String, GrepOptions), EffectErrorCode> {
    let Some(options) = options else {
        return Ok((".".to_string(), GrepOptions::default()));
    };
    for key in options.keys::<String>() {
        let key = key.map_err(|_| EffectErrorCode::InvalidTarget)?;
        if !matches!(key.as_str(), "path" | "include" | "case_sensitive") {
            return Err(EffectErrorCode::InvalidTarget);
        }
    }
    let path = options
        .get::<_, Option<String>>("path")
        .map_err(|_| EffectErrorCode::InvalidTarget)?
        .unwrap_or_else(|| ".".to_string());
    validate_path(&path)?;
    let include = options
        .get::<_, Option<String>>("include")
        .map_err(|_| EffectErrorCode::InvalidTarget)?;
    if let Some(include) = &include {
        validate_discovery_pattern(include)?;
    }
    let case_sensitive = options
        .get::<_, Option<bool>>("case_sensitive")
        .map_err(|_| EffectErrorCode::InvalidTarget)?
        .unwrap_or(true);
    Ok((
        path,
        GrepOptions {
            include,
            case_sensitive,
        },
    ))
}

fn validate_spawn(program: &str, arguments: &[String]) -> Result<(), EffectErrorCode> {
    if program.is_empty()
        || program.contains('\0')
        || arguments.len() > SPAWN_ARGUMENT_MAX_COUNT
        || arguments.iter().any(|argument| argument.contains('\0'))
    {
        return Err(EffectErrorCode::InvalidTarget);
    }
    let total_bytes = arguments.iter().try_fold(program.len(), |total, argument| {
        total.checked_add(argument.len())
    });
    if total_bytes.is_none_or(|total| total > SPAWN_ARGUMENTS_MAX_BYTES) {
        Err(EffectErrorCode::TooLarge)
    } else {
        Ok(())
    }
}

#[cfg(feature = "sandbox")]
fn parse_fetch_options(
    options: Option<&Object<'_>>,
) -> rquickjs::Result<(HttpMethod, Vec<HttpHeader>, Option<String>)> {
    let Some(options) = options else {
        return Ok((HttpMethod::Get, Vec::new(), None));
    };
    for key in options.keys::<String>() {
        let key = key?;
        if !matches!(key.as_str(), "method" | "headers" | "body") {
            return Err(rquickjs::Error::new_from_js_message(
                "fetch options",
                "fetch",
                format!("unsupported field '{key}'"),
            ));
        }
    }
    let method = options
        .get::<_, Option<String>>("method")?
        .unwrap_or_else(|| "GET".into())
        .to_ascii_uppercase();
    let method = match method.as_str() {
        "GET" => HttpMethod::Get,
        "POST" => HttpMethod::Post,
        _ => {
            return Err(rquickjs::Error::new_from_js_message(
                "fetch options",
                "fetch",
                "method must be GET or POST",
            ));
        }
    };
    let mut headers = Vec::new();
    let mut header_bytes = 0_usize;
    if let Some(object) = options.get::<_, Option<Object<'_>>>("headers")? {
        for property in object.props::<String, String>() {
            let (name, value) = property?;
            if headers.len() == FETCH_REQUEST_HEADER_MAX_COUNT {
                return Err(fetch_options_error(
                    "request headers exceed the configured limit",
                ));
            }
            reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| fetch_options_error("invalid header name"))?;
            reqwest::header::HeaderValue::from_str(&value)
                .map_err(|_| fetch_options_error("invalid header value"))?;
            header_bytes = header_bytes
                .checked_add(name.len())
                .and_then(|total| total.checked_add(value.len()))
                .ok_or_else(|| {
                    fetch_options_error("request headers exceed the configured limit")
                })?;
            if header_bytes > FETCH_REQUEST_HEADER_MAX_BYTES {
                return Err(fetch_options_error(
                    "request headers exceed the configured limit",
                ));
            }
            let lower = name.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "host"
                    | "content-length"
                    | "transfer-encoding"
                    | "connection"
                    | "proxy-connection"
                    | "upgrade"
                    | "te"
                    | "proxy-authorization"
                    | "authorization"
                    | "cookie"
                    | "forwarded"
                    | "x-forwarded-for"
                    | "x-forwarded-host"
                    | "x-forwarded-proto"
                    | "x-real-ip"
                    | "via"
            ) {
                return Err(fetch_options_error(format!(
                    "header '{lower}' is controlled by the host"
                )));
            }
            headers.push(HttpHeader { name, value });
        }
    }
    let body = options.get::<_, Option<String>>("body")?;
    if body
        .as_ref()
        .is_some_and(|body| body.len() > FETCH_REQUEST_BODY_MAX_BYTES)
    {
        return Err(rquickjs::Error::new_from_js_message(
            "fetch options",
            "fetch",
            "request body exceeds the configured limit",
        ));
    }
    if method == HttpMethod::Get && body.is_some() {
        return Err(rquickjs::Error::new_from_js_message(
            "fetch options",
            "fetch",
            "GET requests cannot have a body",
        ));
    }
    Ok((method, headers, body))
}

#[cfg(feature = "sandbox")]
fn fetch_options_error(message: impl Into<String>) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("fetch options", "fetch", message.into())
}

fn effect_error(ctx: &Ctx<'_>, tool: &'static str, code: EffectErrorCode) -> rquickjs::Error {
    let code = match code {
        EffectErrorCode::Denied => "denied",
        EffectErrorCode::CapabilityDenied => "capability_denied",
        EffectErrorCode::InvalidTarget => "invalid_target",
        EffectErrorCode::NotFound => "not_found",
        EffectErrorCode::IsDirectory => "is_directory",
        EffectErrorCode::Cancelled => "cancelled",
        EffectErrorCode::TimedOut => "timed_out",
        EffectErrorCode::TooLarge => "too_large",
        EffectErrorCode::BackendFailure => "backend_failure",
        EffectErrorCode::AuditFailure => "audit_failure",
        EffectErrorCode::OutcomeUnknown => "outcome_unknown",
    };
    let message = format!("{tool}: {code}");
    let Ok(exception) = Exception::from_message(ctx.clone(), &message) else {
        return rquickjs::Error::Unknown;
    };
    if exception.as_object().prop("code", code).is_err() {
        return rquickjs::Error::Unknown;
    }
    exception.throw()
}

#[cfg(test)]
mod effect_error_tests {
    use super::*;

    #[test]
    fn read_only_profile_installs_only_the_three_read_effect_globals() {
        let runtime = Runtime::new().expect("create runtime");
        let context = Context::full(&runtime).expect("create context");
        let effects = Rc::new(|_| -> EffectResult {
            panic!("global-shape test must not dispatch an effect")
        }) as ModelEffectDispatcher;
        install_model_effect_globals(&context, effects, true, ModelEffectProfile::ReadOnly)
            .expect("install read-only effect globals");

        let observed = context
            .with(|ctx| {
                ctx.eval::<String, _>(
                    "JSON.stringify({\
                       read_file: typeof read_file, \
                       list_dir: typeof list_dir, \
                       grep: typeof grep, \
                       read_files: typeof read_files, \
                       glob: typeof glob, \
                       write_file: typeof write_file, \
                       fetch: typeof fetch, \
                       spawn: typeof spawn, \
                       result: typeof result, \
                       scratch_put: typeof scratch_put, \
                       scratch_get: typeof scratch_get\
                     })",
                )
            })
            .expect("inspect read-only globals");
        let observed: serde_json::Value = serde_json::from_str(&observed).unwrap();
        for name in ["read_file", "list_dir", "grep"] {
            assert_eq!(observed[name], "function", "{name}");
        }
        for name in [
            "read_files",
            "glob",
            "write_file",
            "fetch",
            "spawn",
            "result",
            "scratch_put",
            "scratch_get",
        ] {
            assert_eq!(observed[name], "undefined", "{name}");
        }
    }

    #[test]
    fn discovery_globals_return_typed_bounded_shapes_and_forward_options() {
        let runtime = Runtime::new().expect("create runtime");
        let context = Context::full(&runtime).expect("create context");
        let observed_operations = Rc::new(std::cell::RefCell::new(Vec::new()));
        let calls = observed_operations.clone();
        let effects = Rc::new(move |operation: EffectOperation| {
            calls.borrow_mut().push(operation.clone());
            match operation {
                EffectOperation::ListDir { .. } => EffectResult::ListDir {
                    entries: vec![DirectoryEntry {
                        name: "src".into(),
                        kind: DirectoryEntryKind::Directory,
                        size: 0,
                    }],
                    truncated: false,
                },
                EffectOperation::Glob { .. } => EffectResult::Glob {
                    paths: vec!["src/main.rs".into()],
                    truncated: true,
                },
                EffectOperation::Grep { .. } => EffectResult::Grep {
                    matches: vec![GrepMatch {
                        path: "src/main.rs".into(),
                        line: 7,
                        text: "let needle = true;".into(),
                    }],
                    truncated: false,
                },
                _ => EffectResult::Error(super::super::protocol::EffectError {
                    code: EffectErrorCode::BackendFailure,
                }),
            }
        }) as ModelEffectDispatcher;
        install_model_effect_globals(&context, effects, true, ModelEffectProfile::Full)
            .expect("install effect globals");

        let value = context
            .with(|ctx| {
                ctx.eval::<String, _>(
                    "JSON.stringify({\
                       list: list_dir(), \
                       globbed: glob('**/*.rs', {path: 'src'}), \
                       matches: grep('needle', {path: 'src', include: '*.rs', case_sensitive: false}), \
                       defaults: grep('Needle')\
                     })",
                )
            })
            .expect("execute discovery globals");
        let value: serde_json::Value = serde_json::from_str(&value).unwrap();
        assert_eq!(value["list"]["entries"][0]["kind"], "directory");
        assert_eq!(value["globbed"]["paths"][0], "src/main.rs");
        assert_eq!(value["globbed"]["truncated"], true);
        assert_eq!(value["matches"]["matches"][0]["line"], 7);

        assert_eq!(
            *observed_operations.borrow(),
            vec![
                EffectOperation::ListDir { path: ".".into() },
                EffectOperation::Glob {
                    path: "src".into(),
                    pattern: "**/*.rs".into(),
                },
                EffectOperation::Grep {
                    path: "src".into(),
                    pattern: "needle".into(),
                    options: GrepOptions {
                        include: Some("*.rs".into()),
                        case_sensitive: false,
                    },
                },
                EffectOperation::Grep {
                    path: ".".into(),
                    pattern: "Needle".into(),
                    options: GrepOptions::default(),
                },
            ]
        );
    }

    #[test]
    fn invalid_discovery_options_fail_before_dispatch() {
        let runtime = Runtime::new().expect("create runtime");
        let context = Context::full(&runtime).expect("create context");
        let calls = Rc::new(std::cell::Cell::new(0));
        let observed = calls.clone();
        let effects = Rc::new(move |_| {
            observed.set(observed.get() + 1);
            EffectResult::Error(super::super::protocol::EffectError {
                code: EffectErrorCode::BackendFailure,
            })
        }) as ModelEffectDispatcher;
        install_model_effect_globals(&context, effects, true, ModelEffectProfile::Full)
            .expect("install effect globals");

        let code = context
            .with(|ctx| {
                ctx.eval::<String, _>(
                    "try { grep('x', {path: '.', surprise: true}) } \
                     catch (error) { error.code }",
                )
            })
            .expect("catch validation error");
        assert_eq!(code, "invalid_target");
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn parent_effect_failures_are_plain_errors_with_stable_codes() {
        for (code, expected) in [
            (EffectErrorCode::Denied, "denied"),
            (EffectErrorCode::NotFound, "not_found"),
            (EffectErrorCode::IsDirectory, "is_directory"),
            (EffectErrorCode::TooLarge, "too_large"),
        ] {
            let runtime = Runtime::new().expect("create runtime");
            let context = Context::full(&runtime).expect("create context");
            let effects =
                Rc::new(move |_| EffectResult::Error(super::super::protocol::EffectError { code }))
                    as ModelEffectDispatcher;
            install_model_effect_globals(&context, effects, true, ModelEffectProfile::Full)
                .expect("install effect globals");

            let observed = context
                .with(|ctx| {
                    ctx.eval::<String, _>(
                        "Object.defineProperty(Error.prototype, 'code', { \
                           configurable: true, get() { return 'poisoned' }, \
                           set() { throw new Error('prototype setter called') } \
                         }); \
                         try { read_file('fixture'); 'not-thrown' } catch (error) { \
                         JSON.stringify({ \
                           isError: error instanceof Error, \
                           ownsCode: Object.prototype.hasOwnProperty.call(error, 'code'), \
                           name: error.name, \
                           message: error.message, \
                           code: error.code \
                         }) \
                         }",
                    )
                })
                .expect("catch parent effect error");
            let observed: serde_json::Value =
                serde_json::from_str(&observed).expect("parse caught error fields");

            assert_eq!(observed["isError"], true);
            assert_eq!(observed["ownsCode"], true);
            assert_eq!(observed["name"], "Error");
            assert_eq!(observed["message"], format!("read_file: {expected}"));
            assert_eq!(observed["code"], expected);
        }
    }

    #[test]
    fn local_size_validation_uses_too_large() {
        assert_eq!(
            validate_spawn("program", &["x".repeat(SPAWN_ARGUMENTS_MAX_BYTES)]),
            Err(EffectErrorCode::TooLarge)
        );
    }
}

/// Worker-owned revocation boundary for all invocation capabilities tied to one fresh runtime.
/// Dropping it covers timeout, protocol cancellation, panic unwinding, and worker recycle paths.
#[cfg(feature = "skills")]
pub(crate) struct WorkerCapabilityLifecycle {
    capabilities: InvocationCapabilityRuntime,
}

#[cfg(feature = "skills")]
impl WorkerCapabilityLifecycle {
    pub(crate) fn new(capabilities: InvocationCapabilityRuntime) -> Self {
        Self { capabilities }
    }

    pub(crate) fn cancel(&self, invocation_id: &super::protocol::InvocationId) {
        self.capabilities.cancel(invocation_id);
    }
}

#[cfg(feature = "skills")]
impl Drop for WorkerCapabilityLifecycle {
    fn drop(&mut self) {
        self.capabilities.recycle();
    }
}

const CONSOLE_WRAPPER_SOURCE: &str = r#"
(emit => {
    const string = String;
    const stringify = JSON.stringify;
    const uncurryThis = Function.prototype.bind.bind(Function.prototype.call);
    const slice = uncurryThis(String.prototype.slice);
    const charCodeAt = uncurryThis(String.prototype.charCodeAt);
    const maximum = 8192;
    function take(value, budget) {
        let bytes = 0;
        let end = 0;
        for (let index = 0; index < value.length; index += 1) {
            const unit = charCodeAt(value, index);
            let cost;
            if (unit <= 0x7f) cost = 1;
            else if (unit <= 0x7ff) cost = 2;
            else if (unit >= 0xd800 && unit <= 0xdbff && index + 1 < value.length) {
                const next = charCodeAt(value, index + 1);
                if (next >= 0xdc00 && next <= 0xdfff) { cost = 4; index += 1; }
                else cost = 3;
            } else cost = 3;
            if (bytes + cost > budget) break;
            bytes += cost;
            end = index + 1;
        }
        return slice(value, 0, end);
    }
    function byteLength(value) {
        let bytes = 0;
        for (let index = 0; index < value.length; index += 1) {
            const unit = charCodeAt(value, index);
            if (unit <= 0x7f) bytes += 1;
            else if (unit <= 0x7ff) bytes += 2;
            else if (unit >= 0xd800 && unit <= 0xdbff && index + 1 < value.length) {
                const next = charCodeAt(value, index + 1);
                if (next >= 0xdc00 && next <= 0xdfff) { bytes += 4; index += 1; }
                else bytes += 3;
            } else bytes += 3;
        }
        return bytes;
    }
    function render(value) {
        if (value === null || typeof value !== "object") return string(value);
        let encoded;
        try { encoded = stringify(value); } catch (_) {}
        let fallback;
        try { fallback = string(value); } catch (_) {
            return typeof encoded === "string" ? encoded : "<unprintable>";
        }
        if (typeof encoded === "string" && (encoded !== "{}" || fallback === "[object Object]")) {
            return encoded;
        }
        return fallback;
    }
    return (...values) => {
        let text = "";
        let remaining = maximum;
        let truncated = false;
        for (let index = 0; index < values.length; index += 1) {
            const part = render(values[index]);
            if (index !== 0) {
                if (remaining === 0) { truncated = true; break; }
                text += " ";
                remaining -= 1;
            }
            const bounded = take(part, remaining);
            text += bounded;
            remaining -= byteLength(bounded);
            if (bounded.length !== part.length) {
                truncated = true;
                break;
            }
        }
        emit(text, truncated);
    };
})
"#;

const STRING_GATE_SOURCE: &str = r#"
(() => {
    const uncurryThis = Function.prototype.bind.bind(Function.prototype.call);
    const charCodeAt = uncurryThis(String.prototype.charCodeAt);
    return value => {
        let bytes = 0;
        for (let index = 0; index < value.length; index += 1) {
            const unit = charCodeAt(value, index);
            if (unit <= 0x7f) bytes += 1;
            else if (unit <= 0x7ff) bytes += 2;
            else if (unit >= 0xd800 && unit <= 0xdbff && index + 1 < value.length) {
                const next = charCodeAt(value, index + 1);
                if (next >= 0xdc00 && next <= 0xdfff) { bytes += 4; index += 1; }
                else bytes += 3;
            } else bytes += 3;
            if (bytes > 65536) throw 0;
        }
        return value;
    };
})()
"#;

const ASYNC_COMPLETION_VALUE_SOURCE: &str = r#"
(() => {
    const getOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;
    return completion => {
        const descriptor = getOwnPropertyDescriptor(completion, "value");
        if (!descriptor || !("value" in descriptor)) throw 0;
        return descriptor.value;
    };
})()
"#;

const MODEL_SCRIPT_NAME: &str = "mini-agent-model.js";
const EXCEPTION_INSPECTOR_SOURCE: &str = r#"
(() => {
    const uncurryThis = Function.prototype.bind.bind(Function.prototype.call);
    const charCodeAt = uncurryThis(String.prototype.charCodeAt);
    const indexOf = uncurryThis(String.prototype.indexOf);
    const isError = Error.isError;
    const getPrototypeOf = Object.getPrototypeOf;
    const getOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;
    const syntaxPrototype = SyntaxError.prototype;
    const typePrototype = TypeError.prototype;
    const referencePrototype = ReferenceError.prototype;
    const rangePrototype = RangeError.prototype;
    const internalPrototype = InternalError.prototype;
    const modelMarker = "mini-agent-model.js:";
    const maxStackCharacters = 16384;

    function ownData(object, key) {
        const descriptor = getOwnPropertyDescriptor(object, key);
        if (!descriptor || !("value" in descriptor)) return undefined;
        return descriptor.value;
    }

    function decimal(text, start, end) {
        if (start === end) return 0;
        let value = 0;
        for (let index = start; index < end; index += 1) {
            const digit = charCodeAt(text, index) - 48;
            if (digit < 0 || digit > 9) return 0;
            value = value * 10 + digit;
            if (value > 4294967295) return 0;
        }
        return value;
    }

    function modelLocation(stack) {
        if (typeof stack !== "string" || stack.length > maxStackCharacters) return [0, 0];
        let searchFrom = 0;
        while (searchFrom < stack.length) {
            const marker = indexOf(stack, modelMarker, searchFrom);
            if (marker < 0) return [0, 0];
            const before = marker === 0 ? 0 : charCodeAt(stack, marker - 1);
            if (marker !== 0 && before !== 32 && before !== 40) {
                searchFrom = marker + modelMarker.length;
                continue;
            }
            const lineStart = marker + modelMarker.length;
            const separator = indexOf(stack, ":", lineStart);
            if (separator < 0) return [0, 0];
            let end = separator + 1;
            while (end < stack.length) {
                const code = charCodeAt(stack, end);
                if (code < 48 || code > 57) break;
                end += 1;
            }
            const terminator = end === stack.length ? 0 : charCodeAt(stack, end);
            if (terminator !== 0 && terminator !== 10 && terminator !== 13 && terminator !== 41) {
                searchFrom = marker + modelMarker.length;
                continue;
            }
            const line = decimal(stack, lineStart, separator);
            const column = decimal(stack, separator + 1, end);
            if (line !== 0 && column !== 0) return [line, column];
            searchFrom = marker + modelMarker.length;
        }
        return [0, 0];
    }

    return value => {
        // Error.isError is an engine class check and does not run Proxy traps or user getters.
        if (!isError(value)) return [5, false, 0, 0];
        const prototype = getPrototypeOf(value);
        let kind = 5;
        if (prototype === syntaxPrototype) kind = 0;
        else if (prototype === typePrototype) kind = 1;
        else if (prototype === referencePrototype) kind = 2;
        else if (prototype === rangePrototype) kind = 3;
        else if (prototype === internalPrototype) kind = 4;

        const message = ownData(value, "message");
        const stackLimit =
            kind === 3 && message === "Maximum call stack size exceeded";
        const location = modelLocation(ownData(value, "stack"));
        return [kind, stackLimit, location[0], location[1]];
    };
})()
"#;

pub(super) const STRICT_CLONE_SOURCE: &str = r#"
(() => {
    const uncurryThis = Function.prototype.bind.bind(Function.prototype.call);
    const getPrototypeOf = Object.getPrototypeOf;
    const setPrototypeOf = Object.setPrototypeOf;
    const getOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;
    const create = Object.create;
    const defineProperty = Object.defineProperty;
    const ownKeys = Reflect.ownKeys;
    const isArray = Array.isArray;
    const stringify = JSON.stringify;
    const objectPrototype = Object.prototype;
    const arrayPrototype = Array.prototype;
    const finite = Number.isFinite;
    const safeInteger = Number.isSafeInteger;
    const integer = Number.isInteger;
    const number = Number;
    const string = String;
    const charCodeAt = uncurryThis(String.prototype.charCodeAt);
    const SetCtor = Set;
    const setAdd = uncurryThis(Set.prototype.add);
    const setHas = uncurryThis(Set.prototype.has);
    const setDelete = uncurryThis(Set.prototype.delete);
    const maxDepth = 64;
    const maxNodes = 10000;
    const maxBytes = 65536;

    function utf8Bytes(text) {
        let bytes = 0;
        for (let index = 0; index < text.length; index += 1) {
            const unit = charCodeAt(text, index);
            if (unit <= 0x7f) bytes += 1;
            else if (unit <= 0x7ff) bytes += 2;
            else if (unit >= 0xd800 && unit <= 0xdbff && index + 1 < text.length) {
                const next = charCodeAt(text, index + 1);
                if (next >= 0xdc00 && next <= 0xdfff) { bytes += 4; index += 1; }
                else bytes += 3;
            } else bytes += 3;
            if (bytes > maxBytes) throw 0;
        }
        return bytes;
    }

    return function strictClone(candidate) {
        // Reflect.ownKeys creates an engine Array before its length can be budgeted. Reject
        // numeric Array-prototype pollution up to and including the accepted key budget first,
        // so construction cannot dispatch an attacker setter along any accepted path.
        for (let index = 0; index <= maxNodes; index += 1) {
            if (getOwnPropertyDescriptor(arrayPrototype, string(index)) !== undefined) throw 0;
        }
        let nodes = 0;
        let bytes = 0;
        const active = new SetCtor();

        function clone(value, depth) {
            if (depth > maxDepth || ++nodes > maxNodes) throw 0;
            if (value === null || typeof value === "boolean") return value;
            if (typeof value === "number") {
                if (!finite(value)) throw 0;
                return value;
            }
            if (typeof value === "string") {
                bytes += utf8Bytes(value);
                if (bytes > maxBytes) throw 0;
                return value;
            }
            if (typeof value !== "object" || setHas(active, value)) throw 0;

            setAdd(active, value);
            const keys = ownKeys(value);
            if (keys.length > maxNodes - nodes) throw 0;
            let copy;
            if (isArray(value)) {
                const length = value.length;
                if (!safeInteger(length) || length < 0 || length > maxNodes - nodes) throw 0;
                if (keys.length !== length + 1) throw 0;
                copy = [];
                setPrototypeOf(copy, null);
                for (let index = 0; index < length; index += 1) {
                    const key = string(index);
                    const descriptor = getOwnPropertyDescriptor(value, key);
                    if (!descriptor || !descriptor.enumerable || !("value" in descriptor)) throw 0;
                    defineProperty(copy, key, {
                        value: clone(descriptor.value, depth + 1),
                        enumerable: true,
                        configurable: true,
                        writable: true,
                    });
                }
                const lengthDescriptor = getOwnPropertyDescriptor(value, "length");
                if (!lengthDescriptor || !("value" in lengthDescriptor)) throw 0;
                for (let keyIndex = 0; keyIndex < keys.length; keyIndex += 1) {
                    const key = keys[keyIndex];
                    if (key === "length") continue;
                    if (typeof key !== "string") throw 0;
                    const index = number(key);
                    if (!integer(index) || index < 0 || index >= length || string(index) !== key) throw 0;
                }
            } else {
                const prototype = getPrototypeOf(value);
                if (prototype !== objectPrototype && prototype !== null) throw 0;
                copy = create(null);
                for (let keyIndex = 0; keyIndex < keys.length; keyIndex += 1) {
                    const key = keys[keyIndex];
                    if (typeof key !== "string") throw 0;
                    bytes += utf8Bytes(key);
                    if (bytes > maxBytes) throw 0;
                    const descriptor = getOwnPropertyDescriptor(value, key);
                    if (!descriptor || !descriptor.enumerable || !("value" in descriptor)) throw 0;
                    defineProperty(copy, key, {
                        value: clone(descriptor.value, depth + 1),
                        enumerable: true,
                        configurable: true,
                        writable: true,
                    });
                }
            }
            setDelete(active, value);
            return copy;
        }

        const encoded = stringify(clone(candidate, 0));
        if (utf8Bytes(encoded) > maxBytes) throw 0;
        return encoded;
    };
})()
"#;

const TRUSTED_BOOTSTRAP_MODULE_NAME: &str = "mini-agent:trusted-bootstrap";
static TRUSTED_BOOTSTRAP_BYTECODE: OnceLock<Option<Vec<u8>>> = OnceLock::new();

fn trusted_bootstrap_source() -> String {
    let session_json_clone = STRICT_CLONE_SOURCE
        .replace("const maxNodes = 10000;", "const maxNodes = 100000;")
        .replace("const maxBytes = 65536;", "const maxBytes = 1048576;");
    format!(
        "export const strictClone = {STRICT_CLONE_SOURCE};\n\
         export const sessionJsonClone = {session_json_clone};\n\
         const sessionJsonParse = JSON.parse;\n\
         export const sessionResultWrapper = dispatch => value => dispatch(sessionJsonClone(value));\n\
         export const scratchPutWrapper = dispatch => (key, value) => dispatch(key, sessionJsonClone(value));\n\
         export const scratchGetWrapper = dispatch => key => {{ const encoded = dispatch(key); return encoded == null ? null : sessionJsonParse(encoded); }};\n\
         export const consoleWrapper = {CONSOLE_WRAPPER_SOURCE};\n\
         export const stringGate = {STRING_GATE_SOURCE};\n\
         export const exceptionInspector = {EXCEPTION_INSPECTOR_SOURCE};\n\
         export const asyncCompletionValue = {ASYNC_COMPLETION_VALUE_SOURCE};"
    )
}

fn compile_trusted_bootstrap_bytecode() -> rquickjs::Result<Vec<u8>> {
    let runtime = Runtime::new()?;
    runtime.set_memory_limit(MEMORY_LIMIT);
    runtime.set_max_stack_size(STACK_LIMIT);
    let deadline = Instant::now() + STEP_TIMEOUT;
    runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));
    let context = Context::full(&runtime)?;
    context.with(|ctx| {
        Module::declare(
            ctx,
            TRUSTED_BOOTSTRAP_MODULE_NAME,
            trusted_bootstrap_source(),
        )?
        .write(WriteOptions::default())
    })
}

fn trusted_bootstrap_bytecode() -> Option<&'static [u8]> {
    TRUSTED_BOOTSTRAP_BYTECODE
        .get_or_init(|| compile_trusted_bootstrap_bytecode().ok())
        .as_deref()
}

struct TrustedBootstrapFunctions {
    strict_clone: Persistent<Function<'static>>,
    session_result_wrapper: Persistent<Function<'static>>,
    scratch_put_wrapper: Persistent<Function<'static>>,
    scratch_get_wrapper: Persistent<Function<'static>>,
    console_wrapper: Persistent<Function<'static>>,
    string_gate: Persistent<Function<'static>>,
    exception_inspector: Persistent<Function<'static>>,
    async_completion_value: Persistent<Function<'static>>,
}

#[allow(unsafe_code)]
fn load_trusted_bootstrap_functions(
    context: &Context,
    bytecode: &[u8],
) -> rquickjs::Result<TrustedBootstrapFunctions> {
    context.with(|ctx| {
        // SAFETY: these bytes are compiled once in this process from the
        // trusted constants above, with the same linked QuickJS ABI, and are
        // never accepted from disk, IPC, model output, or any other input.
        let module = unsafe { Module::load(ctx.clone(), bytecode)? };
        let (module, evaluation) = module.eval()?;
        evaluation.finish::<()>()?;
        let clone = module.get::<_, Function>("strictClone")?;
        let session_result_wrapper = module.get::<_, Function>("sessionResultWrapper")?;
        let scratch_put_wrapper = module.get::<_, Function>("scratchPutWrapper")?;
        let scratch_get_wrapper = module.get::<_, Function>("scratchGetWrapper")?;
        let console_wrapper = module.get::<_, Function>("consoleWrapper")?;
        let string_gate = module.get::<_, Function>("stringGate")?;
        let exception_inspector = module.get::<_, Function>("exceptionInspector")?;
        let async_completion_value = module.get::<_, Function>("asyncCompletionValue")?;
        Ok(TrustedBootstrapFunctions {
            strict_clone: Persistent::save(&ctx, clone),
            session_result_wrapper: Persistent::save(&ctx, session_result_wrapper),
            scratch_put_wrapper: Persistent::save(&ctx, scratch_put_wrapper),
            scratch_get_wrapper: Persistent::save(&ctx, scratch_get_wrapper),
            console_wrapper: Persistent::save(&ctx, console_wrapper),
            string_gate: Persistent::save(&ctx, string_gate),
            exception_inspector: Persistent::save(&ctx, exception_inspector),
            async_completion_value: Persistent::save(&ctx, async_completion_value),
        })
    })
}

#[cfg(test)]
mod trusted_bootstrap_bytecode_tests {
    use super::*;

    const BENCHMARK_WARMUPS: usize = 5;
    const BENCHMARK_SAMPLES: usize = 50;

    fn configure_benchmark_runtime() -> rquickjs::Result<(Runtime, Context)> {
        let runtime = Runtime::new()?;
        runtime.set_memory_limit(MEMORY_LIMIT);
        runtime.set_max_stack_size(STACK_LIMIT);
        let deadline = Instant::now() + STEP_TIMEOUT;
        runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));
        let context = Context::full(&runtime)?;
        Ok((runtime, context))
    }

    fn evaluate_trusted_bootstrap_source() -> rquickjs::Result<()> {
        let (_runtime, context) = configure_benchmark_runtime()?;
        context.with(|ctx| {
            let (module, evaluation) = Module::declare(
                ctx,
                TRUSTED_BOOTSTRAP_MODULE_NAME,
                trusted_bootstrap_source(),
            )?
            .eval()?;
            evaluation.finish::<()>()?;
            let _: Function = module.get("strictClone")?;
            let _: Function = module.get("sessionResultWrapper")?;
            let _: Function = module.get("scratchPutWrapper")?;
            let _: Function = module.get("scratchGetWrapper")?;
            let _: Function = module.get("consoleWrapper")?;
            let _: Function = module.get("stringGate")?;
            let _: Function = module.get("exceptionInspector")?;
            let _: Function = module.get("asyncCompletionValue")?;
            Ok(())
        })
    }

    #[test]
    fn session_clone_bootstrap_contains_the_declared_expanded_limits() {
        let source = trusted_bootstrap_source();
        assert_eq!(source.matches("const maxNodes = 100000;").count(), 1);
        assert_eq!(source.matches("const maxBytes = 1048576;").count(), 1);
        assert!(source.contains("export const sessionResultWrapper"));
        assert!(source.contains("export const scratchPutWrapper"));
        assert!(source.contains("export const scratchGetWrapper"));
        assert!(source.contains("export const consoleWrapper"));
    }

    fn load_trusted_bootstrap_bytecode_for_benchmark(bytecode: &[u8]) -> rquickjs::Result<()> {
        let (_runtime, context) = configure_benchmark_runtime()?;
        let _ = load_trusted_bootstrap_functions(&context, bytecode)?;
        Ok(())
    }

    fn percentile_microseconds(samples: &[Duration], percentile: usize) -> f64 {
        assert!(!samples.is_empty());
        assert!((1..=100).contains(&percentile));
        let mut ordered = samples.to_vec();
        ordered.sort_unstable();
        let rank = (ordered.len() * percentile).div_ceil(100);
        ordered[rank.saturating_sub(1)].as_secs_f64() * 1_000_000.0
    }

    #[test]
    fn trusted_bootstrap_bytecode_loads_into_distinct_fresh_runtimes() {
        let bytecode = compile_trusted_bootstrap_bytecode().expect("compile trusted bootstrap");

        for expected in ["{\"runtime\":1}", "{\"runtime\":2}"] {
            let runtime = Runtime::new().expect("create fresh runtime");
            runtime.set_memory_limit(MEMORY_LIMIT);
            runtime.set_max_stack_size(STACK_LIMIT);
            let deadline = Instant::now() + STEP_TIMEOUT;
            runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));
            let context = Context::full(&runtime).expect("create fresh context");
            let functions =
                load_trusted_bootstrap_functions(&context, &bytecode).expect("load bytecode");

            context.with(|ctx| {
                let clone = functions
                    .strict_clone
                    .clone()
                    .restore(&ctx)
                    .expect("restore strict clone");
                let string_gate = functions
                    .string_gate
                    .clone()
                    .restore(&ctx)
                    .expect("restore string gate");
                let exception_inspector = functions
                    .exception_inspector
                    .clone()
                    .restore(&ctx)
                    .expect("restore exception inspector");
                let async_completion_value = functions
                    .async_completion_value
                    .clone()
                    .restore(&ctx)
                    .expect("restore async completion extractor");
                let value: Object = ctx.eval(format!("({expected})")).expect("create value");
                let encoded: String = clone.call((value,)).expect("clone value");
                assert_eq!(encoded, expected);
                let gated: String = string_gate.call((expected,)).expect("gate string");
                assert_eq!(gated, expected);
                let metadata: rquickjs::prelude::List<(u8, bool, u32, u32)> =
                    exception_inspector.call((42,)).expect("inspect primitive");
                assert_eq!(metadata.0, (5, false, 0, 0));
                let completion: Object = ctx.eval("({value: 42})").expect("create completion");
                let completion_value: i32 = async_completion_value
                    .call((completion,))
                    .expect("extract completion value");
                assert_eq!(completion_value, 42);
            });
        }
    }

    #[test]
    fn bootstrap_benchmark_percentiles_use_nearest_rank() {
        let samples = [
            Duration::from_micros(1),
            Duration::from_micros(2),
            Duration::from_micros(3),
            Duration::from_micros(4),
            Duration::from_micros(100),
        ];
        assert_eq!(percentile_microseconds(&samples, 50), 3.0);
        assert_eq!(percentile_microseconds(&samples, 95), 100.0);
    }

    #[test]
    #[ignore = "run explicitly for bounded trusted-bootstrap before/after measurements"]
    fn trusted_bootstrap_latency_benchmark() {
        assert_eq!(
            std::env::var("MINI_AGENT_JS_BOOTSTRAP_BENCH").as_deref(),
            Ok("1"),
            "set MINI_AGENT_JS_BOOTSTRAP_BENCH=1 for an intentional benchmark run"
        );
        let bytecode = compile_trusted_bootstrap_bytecode().expect("compile trusted bootstrap");
        let mut source_samples = Vec::with_capacity(BENCHMARK_SAMPLES);
        let mut bytecode_samples = Vec::with_capacity(BENCHMARK_SAMPLES);

        println!(
            "trusted bootstrap benchmark: {} warmups + {} samples per path",
            BENCHMARK_WARMUPS, BENCHMARK_SAMPLES
        );
        for iteration in 0..(BENCHMARK_WARMUPS + BENCHMARK_SAMPLES) {
            let (source_elapsed, bytecode_elapsed) = if iteration % 2 == 0 {
                let source_started = Instant::now();
                evaluate_trusted_bootstrap_source().expect("evaluate trusted bootstrap source");
                let source_elapsed = source_started.elapsed();

                let bytecode_started = Instant::now();
                load_trusted_bootstrap_bytecode_for_benchmark(&bytecode)
                    .expect("load trusted bootstrap bytecode");
                (source_elapsed, bytecode_started.elapsed())
            } else {
                let bytecode_started = Instant::now();
                load_trusted_bootstrap_bytecode_for_benchmark(&bytecode)
                    .expect("load trusted bootstrap bytecode");
                let bytecode_elapsed = bytecode_started.elapsed();

                let source_started = Instant::now();
                evaluate_trusted_bootstrap_source().expect("evaluate trusted bootstrap source");
                (source_started.elapsed(), bytecode_elapsed)
            };

            if iteration >= BENCHMARK_WARMUPS {
                source_samples.push(source_elapsed);
                bytecode_samples.push(bytecode_elapsed);
            }
        }

        println!(
            "TRUSTED_BOOTSTRAP_BENCHMARK source_eval_p50_us={:.1} source_eval_p95_us={:.1} bytecode_p50_us={:.1} bytecode_p95_us={:.1}",
            percentile_microseconds(&source_samples, 50),
            percentile_microseconds(&source_samples, 95),
            percentile_microseconds(&bytecode_samples, 50),
            percentile_microseconds(&bytecode_samples, 95),
        );
    }
}

#[derive(Clone, Copy)]
struct ExecutionLimits {
    timeout: Duration,
    max_pending_jobs: usize,
}

impl ExecutionLimits {
    fn current() -> Self {
        #[cfg(test)]
        {
            let timeout = std::env::var("MINI_AGENT_TEST_WORKER_TIMEOUT_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|value| *value > 0)
                .map(Duration::from_millis)
                .unwrap_or(STEP_TIMEOUT);
            let max_pending_jobs = std::env::var("MINI_AGENT_TEST_WORKER_MAX_PENDING_JOBS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(MAX_PENDING_JOBS);
            Self {
                timeout,
                max_pending_jobs,
            }
        }
        #[cfg(not(test))]
        Self {
            timeout: STEP_TIMEOUT,
            max_pending_jobs: MAX_PENDING_JOBS,
        }
    }
}

#[derive(Clone)]
struct ClosedFailure {
    outcome: StepOutcome,
    diagnostic: Diagnostic,
}

impl ClosedFailure {
    fn error(code: JsErrorCode, stage: DiagnosticStage, role: ScriptRole) -> Self {
        let class = match code {
            JsErrorCode::Syntax => DiagnosticClass::Syntax,
            JsErrorCode::Exception => DiagnosticClass::Exception,
            JsErrorCode::StackLimit | JsErrorCode::JobLimit | JsErrorCode::EffectLimit => {
                DiagnosticClass::ResourceLimit
            }
            JsErrorCode::InvalidResult => DiagnosticClass::Contract,
            JsErrorCode::Internal => DiagnosticClass::Internal,
        };
        Self {
            outcome: StepOutcome::Error(code),
            diagnostic: diagnostic(class, stage, role),
        }
    }

    fn timeout(stage: DiagnosticStage, role: ScriptRole) -> Self {
        Self {
            outcome: StepOutcome::Timeout,
            diagnostic: diagnostic(DiagnosticClass::ResourceLimit, stage, role),
        }
    }

    fn out_of_memory(stage: DiagnosticStage, role: ScriptRole) -> Self {
        Self {
            outcome: StepOutcome::OutOfMemory,
            diagnostic: diagnostic(DiagnosticClass::ResourceLimit, stage, role),
        }
    }

    fn javascript_exception(
        inspected: InspectedException,
        stage: DiagnosticStage,
        role: ScriptRole,
    ) -> Self {
        let (code, class) = if inspected.stack_limit {
            (JsErrorCode::StackLimit, DiagnosticClass::ResourceLimit)
        } else if inspected.class == JsExceptionClass::SyntaxError {
            (JsErrorCode::Syntax, DiagnosticClass::Syntax)
        } else {
            (JsErrorCode::Exception, DiagnosticClass::Exception)
        };
        Self {
            outcome: StepOutcome::Error(code),
            diagnostic: Diagnostic {
                class,
                stage,
                script_role: role,
                exception_class: Some(inspected.class),
                line: inspected.line,
                column: inspected.column,
            },
        }
    }
}

fn diagnostic(class: DiagnosticClass, stage: DiagnosticStage, role: ScriptRole) -> Diagnostic {
    Diagnostic {
        class,
        stage,
        script_role: role,
        exception_class: None,
        line: None,
        column: None,
    }
}

#[derive(Clone, Copy)]
struct InspectedException {
    class: JsExceptionClass,
    stack_limit: bool,
    line: Option<u32>,
    column: Option<u32>,
}

struct ExceptionDiagnostics<'a> {
    inspector: &'a Persistent<Function<'static>>,
    model_source: Option<&'a str>,
}

impl ExceptionDiagnostics<'_> {
    fn inspect<'js>(&self, ctx: &Ctx<'js>, thrown: Value<'js>) -> InspectedException {
        let fallback = InspectedException {
            class: JsExceptionClass::Other,
            stack_limit: false,
            line: None,
            column: None,
        };
        let result = self.inspector.clone().restore(ctx).and_then(|inspector| {
            inspector.call::<_, rquickjs::prelude::List<(u8, bool, u32, u32)>>((thrown,))
        });
        let rquickjs::prelude::List((class, stack_limit, line, column)) = match result {
            Ok(result) => result,
            Err(error) => {
                if matches!(error, Error::Exception) {
                    let _ = ctx.catch();
                }
                return fallback;
            }
        };
        let class = match class {
            0 => JsExceptionClass::SyntaxError,
            1 => JsExceptionClass::TypeError,
            2 => JsExceptionClass::ReferenceError,
            3 => JsExceptionClass::RangeError,
            4 => JsExceptionClass::InternalError,
            5 => JsExceptionClass::Other,
            _ => return fallback,
        };
        let location = self.model_source.and_then(|source| {
            source_position_is_valid(source, line, column).then_some((line, column))
        });
        InspectedException {
            class,
            stack_limit,
            line: location.map(|location| location.0),
            column: location.map(|location| location.1),
        }
    }
}

fn classify_thrown_exception<'js>(
    ctx: &Ctx<'js>,
    thrown: Value<'js>,
    deadline: Instant,
    interrupted: &AtomicBool,
    stage: DiagnosticStage,
    role: ScriptRole,
    diagnostics: &ExceptionDiagnostics<'_>,
) -> ClosedFailure {
    let inspected = diagnostics.inspect(ctx, thrown);
    if interrupted.load(Ordering::Relaxed) || Instant::now() >= deadline {
        ClosedFailure::timeout(stage, role)
    } else {
        ClosedFailure::javascript_exception(inspected, stage, role)
    }
}

/// Enter internal-worker mode when and only when the reserved launcher marker is present.
pub(crate) fn maybe_run_internal_worker() -> Option<ExitCode> {
    if !is_internal_worker_marker_present() {
        return None;
    }
    #[cfg(target_os = "linux")]
    if std::env::var_os(INTERNAL_WORKER_MARKER).as_deref()
        == Some(std::ffi::OsStr::new(
            crate::sandbox::worker::LINUX_PREFLIGHT_MARKER_VALUE,
        ))
    {
        return Some(
            if standard_streams_are_protocol_pipes() && finalize_internal_worker().is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            },
        );
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

    if bootstrap(std::io::stdin(), std::io::stdout()).is_ok() {
        0
    } else {
        EXIT_FAILURE
    }
}

#[cfg(feature = "skills")]
#[derive(Clone)]
struct CachedSkillArtifact {
    artifact: Arc<super::skills::SkillArtifact>,
    bytecode: Arc<super::realm::CompiledArtifactBytecode>,
}

#[cfg(feature = "skills")]
const MAX_SKILL_SOURCE_BYTES_PER_STEP: usize = 64 * 1024;
#[cfg(feature = "skills")]
const MAX_SKILL_BYTECODE_CACHE_BYTES: usize = 4 * 1024 * 1024;

#[cfg(feature = "skills")]
#[derive(Default)]
struct WorkerSkillCache {
    turn_id: Option<String>,
    artifacts: std::collections::HashMap<String, CachedSkillArtifact>,
}

#[cfg(feature = "skills")]
#[derive(Debug)]
enum SkillCacheError {
    Artifact,
    Protocol,
}

#[cfg(feature = "skills")]
impl WorkerSkillCache {
    fn resolve(
        &mut self,
        request: &mut RunStep,
    ) -> Result<Vec<CachedSkillArtifact>, SkillCacheError> {
        if !request.artifacts.is_empty() && !request.cached_artifact_ids.is_empty() {
            return Err(SkillCacheError::Protocol);
        }
        if request.artifacts.is_empty() && request.cached_artifact_ids.is_empty() {
            if !request.turn_id.is_empty()
                && self.turn_id.as_deref() != Some(request.turn_id.as_str())
            {
                self.turn_id = Some(request.turn_id.clone());
                self.artifacts.clear();
            }
            return Ok(Vec::new());
        }
        if request.turn_id.is_empty() {
            return Err(SkillCacheError::Protocol);
        }
        if !request.artifacts.is_empty() {
            let artifacts = std::mem::take(&mut request.artifacts);
            validate_skill_artifacts_bounds(&artifacts).map_err(|_| SkillCacheError::Artifact)?;
            let mut next = std::collections::HashMap::with_capacity(artifacts.len());
            let mut ordered = Vec::with_capacity(artifacts.len());
            let mut bytecode_bytes = 0_usize;
            for artifact in artifacts {
                if next.contains_key(&artifact.id) {
                    return Err(SkillCacheError::Artifact);
                }
                let bytecode = super::realm::compile_artifact_bytecode(&artifact)
                    .map_err(|_| SkillCacheError::Artifact)?;
                bytecode_bytes = bytecode_bytes
                    .checked_add(bytecode.len())
                    .ok_or(SkillCacheError::Artifact)?;
                if bytecode_bytes > MAX_SKILL_BYTECODE_CACHE_BYTES {
                    return Err(SkillCacheError::Artifact);
                }
                let id = artifact.id.clone();
                let cached = CachedSkillArtifact {
                    artifact: Arc::new(artifact),
                    bytecode: Arc::new(bytecode),
                };
                next.insert(id, cached.clone());
                ordered.push(cached);
            }
            self.turn_id = Some(request.turn_id.clone());
            self.artifacts = next;
            return Ok(ordered);
        }

        if self.turn_id.as_deref() != Some(request.turn_id.as_str())
            || request.cached_artifact_ids.len() > MAX_SKILL_ARTIFACTS_PER_STEP
        {
            return Err(SkillCacheError::Protocol);
        }
        let identities = std::mem::take(&mut request.cached_artifact_ids);
        let mut seen = std::collections::HashSet::with_capacity(identities.len());
        let mut ordered = Vec::with_capacity(identities.len());
        for identity in identities {
            if !seen.insert(identity.clone()) {
                return Err(SkillCacheError::Protocol);
            }
            ordered.push(
                self.artifacts
                    .get(&identity)
                    .cloned()
                    .ok_or(SkillCacheError::Protocol)?,
            );
        }
        Ok(ordered)
    }
}

fn bootstrap<R: std::io::Read + Send + 'static, W: Write + Send + 'static>(
    mut input: R,
    mut output: W,
) -> Result<(), ()> {
    let build = BuildIdentity::current();
    let limits = ExecutionLimits::current();
    let mut protocol = WorkerProtocol::new(build.clone());

    let hello: ParentWireFrame = read_frame(&mut input).map_err(|_| ())?;
    if !matches!(hello.message, ParentFrame::Hello(_)) {
        return Err(());
    }
    if let Err(error) = protocol.on_receive(&hello) {
        let code = match error {
            ProtocolError::VersionMismatch { .. } => ProtocolFaultCode::VersionMismatch,
            ProtocolError::BuildMismatch { .. } => ProtocolFaultCode::BuildMismatch,
            _ => return Err(()),
        };
        let fault = WireFrame {
            // Echo the parent's connection identity so it can authenticate and classify the
            // fault even though this worker belongs to an older in-place installation.
            protocol_version: hello.protocol_version,
            build_id: hello.build_id,
            invocation_id: None,
            sequence: hello.sequence.checked_add(1).ok_or(())?,
            message: WorkerFrame::ProtocolFault(ProtocolFault {
                code,
                stage: ProtocolStage::Handshake,
            }),
        };
        write_terminal(&mut output, &fault)?;
        return Err(());
    }

    finalize_internal_worker().map_err(|_| ())?;
    // Compile trusted, static helpers before advertising readiness. Request
    // runtimes load these bytes but remain fresh and request-local.
    trusted_bootstrap_bytecode().ok_or(())?;

    let ready: WorkerWireFrame = WireFrame::connection(
        build.clone(),
        1,
        WorkerFrame::Ready(protocol.ready().map_err(|_| ())?),
    );
    protocol.on_send(&ready).map_err(|_| ())?;
    write_terminal(&mut output, &ready)?;

    let transport = Arc::new(Mutex::new(WorkerTransport {
        input,
        output,
        protocol,
    }));
    #[cfg(feature = "skills")]
    let mut skill_cache = WorkerSkillCache::default();

    loop {
        let request: ParentWireFrame = {
            let mut transport = transport.lock().map_err(|_| ())?;
            let request = read_frame(&mut transport.input).map_err(|_| ())?;
            transport.protocol.on_receive(&request).map_err(|_| ())?;
            request
        };
        let invocation_id = request.invocation_id.clone();
        let mut sequence = request.sequence.checked_add(1).ok_or(())?;
        let message = match request.message {
            ParentFrame::RunStep(step) => {
                #[cfg(feature = "skills")]
                let (step, artifacts) = {
                    let mut step = step;
                    let artifacts = match skill_cache.resolve(&mut step) {
                        Ok(artifacts) => artifacts,
                        Err(SkillCacheError::Artifact) => {
                            return write_skill_cache_failure(
                                &transport,
                                &build,
                                invocation_id.ok_or(())?,
                                sequence,
                            );
                        }
                        Err(SkillCacheError::Protocol) => return Err(()),
                    };
                    (step, artifacts)
                };
                let (result, terminal_sequence) = execute_brokered_run_step(
                    step,
                    #[cfg(feature = "skills")]
                    artifacts,
                    limits,
                    transport.clone(),
                    build.clone(),
                    invocation_id.clone().ok_or(())?,
                    sequence,
                )?;
                sequence = terminal_sequence;
                WorkerFrame::StepResult(result)
            }
            ParentFrame::VerifyArtifact(request) => {
                WorkerFrame::VerificationResult(execute_verification(request, limits))
            }
            ParentFrame::ContainmentProbe(probe) => {
                #[cfg(target_os = "windows")]
                {
                    if !crate::sandbox::worker::attest_windows_containment(&probe) {
                        return Err(());
                    }
                    WorkerFrame::ContainmentAttested(
                        super::protocol::ContainmentAttestation::Passed,
                    )
                }
                #[cfg(target_os = "macos")]
                {
                    let _ = probe;
                    if !crate::sandbox::worker::attest_macos_hosted_containment() {
                        return Err(());
                    }
                    WorkerFrame::ContainmentAttested(
                        super::protocol::ContainmentAttestation::Passed,
                    )
                }
                #[cfg(not(any(target_os = "windows", target_os = "macos")))]
                {
                    let _ = probe;
                    return Err(());
                }
            }
            ParentFrame::Shutdown => return Ok(()),
            ParentFrame::Hello(_) | ParentFrame::EffectResponse(_) => return Err(()),
            #[cfg(feature = "skills")]
            ParentFrame::SkillCallResponse(_) => return Err(()),
        };
        let response = WireFrame {
            protocol_version: super::protocol::PROTOCOL_VERSION,
            build_id: build.clone(),
            invocation_id,
            sequence,
            message,
        };
        let mut transport = transport.lock().map_err(|_| ())?;
        transport.protocol.on_send(&response).map_err(|_| ())?;
        write_terminal(&mut transport.output, &response)?;
    }
}

#[cfg(feature = "skills")]
fn write_skill_cache_failure<R: std::io::Read, W: Write>(
    transport: &Arc<Mutex<WorkerTransport<R, W>>>,
    build: &BuildIdentity,
    invocation_id: InvocationId,
    sequence: u64,
) -> Result<(), ()> {
    let response = WireFrame::invocation(
        build.clone(),
        invocation_id,
        sequence,
        WorkerFrame::StepResult(StepResult {
            outcome: StepOutcome::Error(JsErrorCode::Internal),
            console: Vec::new(),
            diagnostic: Some(Diagnostic {
                class: DiagnosticClass::Internal,
                stage: DiagnosticStage::Initialization,
                script_role: ScriptRole::SkillSource,
                exception_class: None,
                line: None,
                column: None,
            }),
            skill_events: Vec::new(),
            evidence_complete: false,
        }),
    );
    let mut transport = transport.lock().map_err(|_| ())?;
    transport.protocol.on_send(&response).map_err(|_| ())?;
    write_terminal(&mut transport.output, &response)
}

fn write_terminal(output: &mut impl Write, frame: &WorkerWireFrame) -> Result<(), ()> {
    write_frame(output, frame).map_err(|_| ())?;
    output.flush().map_err(|_| ())
}

#[cfg(test)]
mod bootstrap_handshake_tests {
    use std::io::{Cursor, Write};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::extras::js::protocol::ParentProtocol;

    #[derive(Clone, Default)]
    struct SharedOutput(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn build_mismatch_emits_authenticated_handshake_fault_before_exit() {
        let parent_build = BuildIdentity::new("1.8.0+in-place-upgrade").unwrap();
        let parent_protocol = ParentProtocol::new(parent_build.clone());
        let hello = WireFrame::connection(
            parent_build.clone(),
            0,
            ParentFrame::Hello(parent_protocol.hello()),
        );
        let mut input = Vec::new();
        write_frame(&mut input, &hello).unwrap();
        let sink = SharedOutput::default();

        assert!(bootstrap(Cursor::new(input), sink.clone()).is_err());
        let bytes = sink.0.lock().unwrap().clone();
        let fault: WorkerWireFrame = read_frame(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(fault.protocol_version, hello.protocol_version);
        assert_eq!(fault.build_id, parent_build);
        assert_eq!(fault.sequence, 1);
        assert_eq!(fault.invocation_id, None);
        assert_eq!(
            fault.message,
            WorkerFrame::ProtocolFault(ProtocolFault {
                code: ProtocolFaultCode::BuildMismatch,
                stage: ProtocolStage::Handshake,
            })
        );
    }
}

fn execute_brokered_run_step<R: std::io::Read + Send + 'static, W: Write + Send + 'static>(
    request: RunStep,
    #[cfg(feature = "skills")] artifacts: Vec<CachedSkillArtifact>,
    limits: ExecutionLimits,
    transport: Arc<Mutex<WorkerTransport<R, W>>>,
    build: BuildIdentity,
    invocation_id: super::protocol::InvocationId,
    sequence: u64,
) -> Result<(StepResult, u64), ()> {
    let model_grant_id = request.model_grant_id.clone();
    let ordinal = Arc::new(std::sync::atomic::AtomicU32::new(0));
    #[cfg(feature = "skills")]
    let skill_request_ordinal = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let sequence = Arc::new(Mutex::new(sequence));
    let protocol_failed = Arc::new(AtomicBool::new(false));
    let effect_limit_reached = Arc::new(AtomicBool::new(false));
    let terminal_requested = Arc::new(AtomicBool::new(false));
    let terminal_result = Arc::new(Mutex::new(None::<String>));
    let wire_dispatcher: WorkerEffectDispatcher = {
        let effect_build = build.clone();
        let effect_invocation_id = invocation_id.clone();
        let ordinal = ordinal.clone();
        let sequence = sequence.clone();
        let protocol_failed = protocol_failed.clone();
        let effect_limit_reached = effect_limit_reached.clone();
        let terminal_requested = terminal_requested.clone();
        let terminal_result = terminal_result.clone();
        let transport = transport.clone();
        Arc::new(move |grant_id, advisory, operation| {
            if protocol_failed.load(Ordering::Acquire) {
                return backend_failure();
            }
            if effect_limit_reached.load(Ordering::Acquire) {
                return backend_failure();
            }
            let effect_ordinal = ordinal.fetch_add(1, Ordering::AcqRel);
            if effect_ordinal >= super::protocol::MAX_EFFECTS_PER_STEP {
                // Quota exhaustion is an ordinary closed step result. Never emit a 257th effect
                // frame or poison the transport: the parent remains in the Running state and can
                // accept the terminal result with the first 256 effects durably accounted for.
                effect_limit_reached.store(true, Ordering::Release);
                return backend_failure();
            }
            let request = EffectRequest {
                effect_ordinal,
                grant_id,
                advisory,
                operation,
            };
            let requested_terminal_result =
                matches!(&request.operation, EffectOperation::Result { .. });
            let result = transport.lock().map_err(|_| ()).and_then(|mut transport| {
                transport.round_trip(request, &effect_build, &effect_invocation_id, &sequence)
            });
            match result {
                Ok(result) => {
                    if requested_terminal_result
                        && let EffectResult::ResultAccepted { json } = &result
                    {
                        let mut accepted = terminal_result
                            .lock()
                            .unwrap_or_else(|error| error.into_inner());
                        if accepted.is_none() {
                            *accepted = Some(json.clone());
                            terminal_requested.store(true, Ordering::Release);
                        }
                    }
                    if matches!(
                        &result,
                        EffectResult::Error(super::protocol::EffectError {
                            code: EffectErrorCode::OutcomeUnknown,
                        })
                    ) {
                        protocol_failed.store(true, Ordering::Release);
                    }
                    result
                }
                Err(()) => {
                    protocol_failed.store(true, Ordering::Release);
                    backend_failure()
                }
            }
        })
    };
    let model_dispatcher = model_grant_id.map(|grant_id| {
        let dispatcher = wire_dispatcher.clone();
        Rc::new(move |operation| {
            dispatcher(grant_id.clone(), AdvisoryAttribution::default(), operation)
        }) as ModelEffectDispatcher
    });
    #[cfg(feature = "skills")]
    let skill_call_authorizer = {
        let skill_request_ordinal = skill_request_ordinal.clone();
        let sequence = sequence.clone();
        let protocol_failed = protocol_failed.clone();
        let transport = transport.clone();
        let build = build.clone();
        let invocation_id = invocation_id.clone();
        Arc::new(
            move |artifact_id: String, export_name: String, call_ordinal: u32| {
                if protocol_failed.load(Ordering::Acquire) {
                    return Err(());
                }
                let request_ordinal = skill_request_ordinal.fetch_add(1, Ordering::AcqRel);
                if request_ordinal >= MAX_SKILL_CALLS_PER_STEP {
                    protocol_failed.store(true, Ordering::Release);
                    return Err(());
                }
                let request = SkillCallRequest {
                    request_ordinal,
                    artifact_id,
                    export_name,
                    call_ordinal,
                };
                let result = transport.lock().map_err(|_| ()).and_then(|mut transport| {
                    transport.skill_call_round_trip(request, &build, &invocation_id, &sequence)
                });
                if result.is_err() {
                    protocol_failed.store(true, Ordering::Release);
                }
                result
            },
        )
            as Arc<dyn Fn(String, String, u32) -> Result<SkillInvocationGrant, ()> + Send + Sync>
    };
    let mut terminal = execute_run_step(
        request,
        #[cfg(feature = "skills")]
        artifacts,
        limits,
        model_dispatcher,
        wire_dispatcher,
        terminal_requested,
        #[cfg(feature = "skills")]
        skill_call_authorizer,
    );
    if protocol_failed.load(Ordering::Acquire) {
        Err(())
    } else {
        if let Some(json) = terminal_result.lock().map_err(|_| ())?.take() {
            terminal.outcome =
                StepOutcome::Structured(serde_json::from_str(&json).map_err(|_| ())?);
            terminal.diagnostic = None;
        }
        if effect_limit_reached.load(Ordering::Acquire) {
            #[cfg(feature = "skills")]
            {
                terminal.evidence_complete = false;
            }
            if matches!(
                &terminal.outcome,
                StepOutcome::Value(_)
                    | StepOutcome::Structured(_)
                    | StepOutcome::Void
                    | StepOutcome::Error(JsErrorCode::Exception)
            ) {
                terminal.outcome = StepOutcome::Error(JsErrorCode::EffectLimit);
                terminal.diagnostic = Some(Diagnostic {
                    class: DiagnosticClass::ResourceLimit,
                    stage: DiagnosticStage::Evaluation,
                    script_role: ScriptRole::Model,
                    exception_class: None,
                    line: None,
                    column: None,
                });
            }
        }
        Ok((terminal, *sequence.lock().map_err(|_| ())?))
    }
}

struct WorkerTransport<R, W> {
    input: R,
    output: W,
    protocol: WorkerProtocol,
}

impl<R: std::io::Read, W: Write> WorkerTransport<R, W> {
    fn round_trip(
        &mut self,
        request: EffectRequest,
        build: &BuildIdentity,
        invocation_id: &super::protocol::InvocationId,
        sequence: &Mutex<u64>,
    ) -> Result<EffectResult, ()> {
        let frame = WireFrame::invocation(
            build.clone(),
            invocation_id.clone(),
            *sequence.lock().map_err(|_| ())?,
            WorkerFrame::EffectRequest(Box::new(request.clone())),
        );
        self.protocol.on_send(&frame).map_err(|_| ())?;
        write_terminal(&mut self.output, &frame)?;
        {
            let mut sequence = sequence.lock().map_err(|_| ())?;
            *sequence = sequence.checked_add(1).ok_or(())?;
        }
        let response: ParentWireFrame = read_frame(&mut self.input).map_err(|_| ())?;
        self.protocol.on_receive(&response).map_err(|_| ())?;
        {
            let mut sequence = sequence.lock().map_err(|_| ())?;
            *sequence = sequence.checked_add(1).ok_or(())?;
        }
        match response.message {
            ParentFrame::EffectResponse(EffectResponse {
                effect_ordinal,
                result,
            }) if effect_ordinal == request.effect_ordinal => Ok(result),
            _ => Err(()),
        }
    }

    #[cfg(feature = "skills")]
    fn skill_call_round_trip(
        &mut self,
        request: SkillCallRequest,
        build: &BuildIdentity,
        invocation_id: &super::protocol::InvocationId,
        sequence: &Mutex<u64>,
    ) -> Result<SkillInvocationGrant, ()> {
        let frame = WireFrame::invocation(
            build.clone(),
            invocation_id.clone(),
            *sequence.lock().map_err(|_| ())?,
            WorkerFrame::SkillCallRequest(request.clone()),
        );
        self.protocol.on_send(&frame).map_err(|_| ())?;
        write_terminal(&mut self.output, &frame)?;
        {
            let mut sequence = sequence.lock().map_err(|_| ())?;
            *sequence = sequence.checked_add(1).ok_or(())?;
        }
        let response: ParentWireFrame = read_frame(&mut self.input).map_err(|_| ())?;
        self.protocol.on_receive(&response).map_err(|_| ())?;
        {
            let mut sequence = sequence.lock().map_err(|_| ())?;
            *sequence = sequence.checked_add(1).ok_or(())?;
        }
        match response.message {
            ParentFrame::SkillCallResponse(SkillCallResponse {
                request_ordinal,
                authorization: Some(authorization),
            }) if request_ordinal == request.request_ordinal => Ok(authorization),
            _ => Err(()),
        }
    }
}

fn backend_failure() -> EffectResult {
    EffectResult::Error(super::protocol::EffectError {
        code: EffectErrorCode::BackendFailure,
    })
}

#[cfg(feature = "skills")]
fn prepare_bound_exports(
    request: &RunStep,
    artifacts: &[CachedSkillArtifact],
    capabilities: &InvocationCapabilityRuntime,
    events: Arc<Mutex<WorkerEventState>>,
    authorize_call: WorkerSkillCallAuthorizer,
) -> Result<
    std::collections::HashMap<
        String,
        std::collections::HashMap<String, super::realm::BoundExportInvocation>,
    >,
    (),
> {
    use std::collections::{HashMap, HashSet};

    validate_skill_cached_artifacts_bounds(artifacts)?;
    if artifacts.is_empty() {
        return Ok(HashMap::new());
    }
    if request.turn_id.is_empty() || request.tool_call_id.is_empty() {
        return Err(());
    }
    let mut prepared = HashMap::new();
    let mut seen_artifacts = HashSet::new();
    for cached in artifacts {
        let artifact = cached.artifact.as_ref();
        if !seen_artifacts.insert(artifact.id.clone()) {
            return Err(());
        }
        let mut exports = HashMap::new();
        for export in &artifact.exports {
            let metadata = WorkerEventMetadata {
                skill_id: artifact.id.clone(),
                export_name: export.name.clone(),
                turn_id: request.turn_id.clone(),
                tool_call_id: request.tool_call_id.clone(),
            };
            let call_authorizer = authorize_call.clone();
            let call_capabilities = capabilities.clone();
            let call_manifest = artifact.capability.clone();
            let call_artifact_id = artifact.id.clone();
            let call_export_name = export.name.clone();
            let call_turn_id = request.turn_id.clone();
            let call_tool_call_id = request.tool_call_id.clone();
            let authorize = Arc::new(move |call_ordinal: u32| {
                let issued = call_authorizer(
                    call_artifact_id.clone(),
                    call_export_name.clone(),
                    call_ordinal,
                )?;
                let expected_invocation = stable_invocation_id(
                    &call_turn_id,
                    &call_tool_call_id,
                    &call_artifact_id,
                    &call_export_name,
                    call_ordinal,
                );
                if issued.artifact_id != call_artifact_id
                    || issued.export_name != call_export_name
                    || issued.invocation_id.as_str() != expected_invocation
                {
                    return Err(());
                }
                let authorization = InvocationAuthorization::new(
                    issued.invocation_id,
                    call_artifact_id.clone(),
                    call_export_name.clone(),
                    call_manifest.clone(),
                    issued
                        .grants
                        .into_iter()
                        .map(|grant| (grant.capability, grant.grant_id)),
                )
                .map_err(|_| ())?;
                let invocation_id = expected_invocation;
                let handle = call_capabilities.prepare(authorization).map_err(|_| ())?;
                Ok((handle, invocation_id))
            });
            let start_events = events.clone();
            let start_metadata = metadata.clone();
            let on_start = Arc::new(move |id: String, shape: String| {
                let shape = if shape.len()
                    <= crate::extras::js::skills::telemetry::MAX_ARGUMENT_SHAPE_BYTES
                {
                    shape
                } else {
                    r#"{"truncated":true}"#.to_string()
                };
                start_events.lock().map_err(|_| ())?.start(
                    id.clone(),
                    start_metadata.clone(),
                    shape,
                );
                Ok(())
            });
            let terminal_events = events.clone();
            let on_terminal = Arc::new(move |invocation_id: String, success: bool| {
                terminal_events
                    .lock()
                    .map_err(|_| ())?
                    .terminal(&invocation_id, success);
                Ok(())
            });
            exports.insert(
                export.name.clone(),
                super::realm::BoundExportInvocation {
                    authorize,
                    on_start,
                    on_terminal,
                },
            );
        }
        prepared.insert(artifact.id.clone(), exports);
    }
    Ok(prepared)
}

#[cfg(feature = "skills")]
fn validate_skill_artifacts_bounds(artifacts: &[super::skills::SkillArtifact]) -> Result<(), ()> {
    validate_skill_artifact_refs(artifacts.len(), artifacts.iter())
}

#[cfg(feature = "skills")]
fn validate_skill_artifact_refs<'a>(
    count: usize,
    artifacts: impl IntoIterator<Item = &'a super::skills::SkillArtifact>,
) -> Result<(), ()> {
    if count > MAX_SKILL_ARTIFACTS_PER_STEP {
        return Err(());
    }
    let mut expected_grants = 0_usize;
    let mut source_bytes = 0_usize;
    for artifact in artifacts {
        source_bytes = source_bytes.checked_add(artifact.source.len()).ok_or(())?;
        if source_bytes > MAX_SKILL_SOURCE_BYTES_PER_STEP {
            return Err(());
        }
        if artifact.exports.len() > MAX_SKILL_EXPORTS_PER_ARTIFACT {
            return Err(());
        }
        expected_grants = expected_grants
            .checked_add(
                artifact
                    .exports
                    .len()
                    .checked_mul(artifact.capability.grants.len())
                    .ok_or(())?,
            )
            .ok_or(())?;
        if expected_grants > MAX_SKILL_CAPABILITY_GRANTS_PER_STEP {
            return Err(());
        }
    }
    Ok(())
}

#[cfg(feature = "skills")]
fn validate_skill_cached_artifacts_bounds(artifacts: &[CachedSkillArtifact]) -> Result<(), ()> {
    validate_skill_artifact_refs(
        artifacts.len(),
        artifacts.iter().map(|cached| cached.artifact.as_ref()),
    )
}

#[cfg(all(test, feature = "skills"))]
mod skill_authority_bound_tests {
    use super::*;
    use crate::extras::js::skills::{
        CapabilityManifest, CapabilityScope, CapabilityTier, SkillArtifact, SkillExport,
    };

    fn artifact(export_count: usize, capability: CapabilityManifest) -> SkillArtifact {
        SkillArtifact::new(
            "function unused() { return 0; }".into(),
            "worker cardinality fixture".into(),
            vec![],
            (0..export_count)
                .map(|index| SkillExport {
                    name: format!("export_{index}"),
                    signature: format!("export_{index}()"),
                })
                .collect(),
            vec!["true".into()],
            capability,
        )
        .unwrap()
    }

    #[test]
    fn worker_rejects_artifact_export_and_total_grant_overflow_before_preparation() {
        let pure = artifact(1, CapabilityManifest::pure());
        assert!(
            validate_skill_artifacts_bounds(&vec![pure; MAX_SKILL_ARTIFACTS_PER_STEP + 1]).is_err()
        );

        let too_many_exports = artifact(
            MAX_SKILL_EXPORTS_PER_ARTIFACT + 1,
            CapabilityManifest::pure(),
        );
        assert!(validate_skill_artifacts_bounds(&[too_many_exports]).is_err());

        let four_grants = CapabilityManifest::new(
            CapabilityTier::SideEffecting,
            vec![
                CapabilityScope::ReadFile {
                    workspace_prefixes: vec!["Cargo.toml".into()],
                },
                CapabilityScope::WriteFile {
                    workspace_prefixes: vec!["target".into()],
                },
                CapabilityScope::Fetch {
                    origins: vec!["https://example.test".into()],
                    methods: vec![crate::extras::js::skills::HttpMethod::Get],
                },
                CapabilityScope::Spawn {
                    programs: vec!["printf".into()],
                },
            ],
        )
        .unwrap();
        let grant_heavy = artifact(MAX_SKILL_EXPORTS_PER_ARTIFACT, four_grants);
        let artifact_count =
            MAX_SKILL_CAPABILITY_GRANTS_PER_STEP / (MAX_SKILL_EXPORTS_PER_ARTIFACT * 4) + 1;
        assert!(validate_skill_artifacts_bounds(&vec![grant_heavy; artifact_count]).is_err());
    }

    #[test]
    fn worker_cache_resolves_ordered_ids_only_within_the_same_turn() {
        let first = artifact(1, CapabilityManifest::pure());
        let first_id = first.id.clone();
        let mut cache = WorkerSkillCache::default();
        let mut initial = RunStep::new("1".into()).with_skills(
            vec![first],
            "cache-turn".into(),
            "cache-call-1".into(),
        );
        let compiled = cache.resolve(&mut initial).unwrap();
        assert_eq!(compiled.len(), 1);
        assert!(initial.artifacts.is_empty());
        assert!(!compiled[0].bytecode.is_empty());

        let mut reference = RunStep::new("1".into());
        reference.turn_id = "cache-turn".into();
        reference.tool_call_id = "cache-call-2".into();
        reference.cached_artifact_ids = vec![first_id.clone()];
        let reused = cache.resolve(&mut reference).unwrap();
        assert!(Arc::ptr_eq(&compiled[0].bytecode, &reused[0].bytecode));

        let mut stale = RunStep::new("1".into());
        stale.turn_id = "different-turn".into();
        stale.tool_call_id = "cache-call-3".into();
        stale.cached_artifact_ids = vec![first_id];
        assert!(cache.resolve(&mut stale).is_err());
    }
}

fn execute_run_step(
    request: RunStep,
    #[cfg(feature = "skills")] artifacts: Vec<CachedSkillArtifact>,
    limits: ExecutionLimits,
    effects: Option<ModelEffectDispatcher>,
    _wire_effects: WorkerEffectDispatcher,
    terminal_requested: Arc<AtomicBool>,
    #[cfg(feature = "skills")] authorize_skill_call: WorkerSkillCallAuthorizer,
) -> StepResult {
    let console = Arc::new(Mutex::new(Vec::new()));
    #[cfg(feature = "skills")]
    let event_state = Arc::new(Mutex::new(WorkerEventState::default()));
    #[cfg(feature = "skills")]
    let capability_runtime = {
        let effects = _wire_effects.clone();
        InvocationCapabilityRuntime::new(move |effect| {
            Ok(effects(
                effect.request.grant_id,
                effect.request.advisory,
                effect.request.operation,
            ))
        })
    };
    #[cfg(feature = "skills")]
    let _capability_lifecycle = WorkerCapabilityLifecycle::new(capability_runtime.clone());
    #[cfg(feature = "skills")]
    let bindings = match prepare_bound_exports(
        &request,
        &artifacts,
        &capability_runtime,
        event_state.clone(),
        authorize_skill_call,
    ) {
        Ok(bindings) => bindings,
        Err(()) => {
            return StepResult {
                outcome: StepOutcome::Error(JsErrorCode::Internal),
                console: Vec::new(),
                diagnostic: Some(Diagnostic {
                    class: DiagnosticClass::Internal,
                    stage: DiagnosticStage::Initialization,
                    script_role: ScriptRole::SkillSource,
                    exception_class: None,
                    line: None,
                    column: None,
                }),
                skill_events: Vec::new(),
                evidence_complete: false,
            };
        }
    };
    #[cfg(feature = "skills")]
    let proposal_effects = request.proposal_grant_id.clone().map(|grant_id| {
        let effects = _wire_effects;
        Rc::new(move |operation| {
            effects(grant_id.clone(), AdvisoryAttribution::default(), operation)
        }) as ModelEffectDispatcher
    });
    let execution = execute_fresh_step(
        &request.code,
        ScriptRole::Model,
        limits,
        console.clone(),
        effects,
        terminal_requested,
        request.spawn_available,
        request.model_effect_profile,
        #[cfg(feature = "skills")]
        proposal_effects,
        #[cfg(feature = "skills")]
        &artifacts,
        #[cfg(feature = "skills")]
        &bindings,
        #[cfg(feature = "skills")]
        &capability_runtime,
        #[cfg(feature = "skills")]
        event_state.clone(),
        #[cfg(feature = "skills")]
        &request.turn_id,
        #[cfg(feature = "skills")]
        &request.tool_call_id,
    );
    let console = console
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    #[cfg(feature = "skills")]
    {
        let outcome = match &execution {
            Ok(outcome) => outcome,
            Err(failure) => &failure.outcome,
        };
        if let Ok(mut state) = event_state.lock() {
            state.finalize_pending(outcome);
        }
    }
    #[cfg(feature = "skills")]
    let skill_events = event_state
        .lock()
        .map(|state| state.events.clone())
        .unwrap_or_default();
    match execution {
        Ok(outcome) => StepResult {
            outcome,
            console,
            diagnostic: None,
            #[cfg(feature = "skills")]
            skill_events,
            #[cfg(feature = "skills")]
            evidence_complete: true,
        },
        Err(failure) => StepResult {
            outcome: failure.outcome,
            console,
            diagnostic: Some(failure.diagnostic),
            #[cfg(feature = "skills")]
            skill_events,
            #[cfg(feature = "skills")]
            evidence_complete: true,
        },
    }
}

// These arguments are the complete per-request security context and keeping them
// explicit makes fresh-runtime construction and capability binding auditable.
#[allow(clippy::too_many_arguments)]
fn execute_fresh_step(
    source: &str,
    role: ScriptRole,
    limits: ExecutionLimits,
    console: Arc<Mutex<Vec<ConsoleRecord>>>,
    effects: Option<ModelEffectDispatcher>,
    terminal_requested: Arc<AtomicBool>,
    spawn_available: bool,
    model_effect_profile: ModelEffectProfile,
    #[cfg(feature = "skills")] proposal_effects: Option<ModelEffectDispatcher>,
    #[cfg(feature = "skills")] artifacts: &[CachedSkillArtifact],
    #[cfg(feature = "skills")] bindings: &std::collections::HashMap<
        String,
        std::collections::HashMap<String, super::realm::BoundExportInvocation>,
    >,
    #[cfg(feature = "skills")] capability_runtime: &InvocationCapabilityRuntime,
    #[cfg(feature = "skills")] event_state: Arc<Mutex<WorkerEventState>>,
    #[cfg(feature = "skills")] turn_id: &str,
    #[cfg(feature = "skills")] tool_call_id: &str,
) -> Result<StepOutcome, ClosedFailure> {
    let runtime = Runtime::new().map_err(|error| initialization_failure(error, role))?;
    runtime.set_memory_limit(MEMORY_LIMIT);
    runtime.set_max_stack_size(STACK_LIMIT);
    let deadline = Instant::now() + limits.timeout;
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupt_flag = interrupted.clone();
    let terminal_interrupt = terminal_requested.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || {
        let expired = Instant::now() >= deadline;
        if expired {
            interrupt_flag.store(true, Ordering::Relaxed);
        }
        expired || terminal_interrupt.load(Ordering::Acquire)
    })));

    let context = Context::full(&runtime).map_err(|error| initialization_failure(error, role))?;
    let bytecode = trusted_bootstrap_bytecode().ok_or_else(|| {
        ClosedFailure::error(JsErrorCode::Internal, DiagnosticStage::Initialization, role)
    })?;
    let functions = load_trusted_bootstrap_functions(&context, bytecode).map_err(|error| {
        classify_error(
            &context,
            error,
            deadline,
            &interrupted,
            DiagnosticStage::Initialization,
            role,
        )
    })?;
    install_console(&context, console, functions.console_wrapper.clone()).map_err(|error| {
        classify_error(
            &context,
            error,
            deadline,
            &interrupted,
            DiagnosticStage::Initialization,
            role,
        )
    })?;
    if let Some(effects) = effects {
        if model_effect_profile == ModelEffectProfile::Full {
            install_session_state_globals(
                &context,
                effects.clone(),
                functions.session_result_wrapper.clone(),
                functions.scratch_put_wrapper.clone(),
                functions.scratch_get_wrapper.clone(),
            )
            .map_err(|error| {
                classify_error(
                    &context,
                    error,
                    deadline,
                    &interrupted,
                    DiagnosticStage::Initialization,
                    role,
                )
            })?;
        }
        install_model_effect_globals(&context, effects, spawn_available, model_effect_profile)
            .map_err(|error| {
                classify_error(
                    &context,
                    error,
                    deadline,
                    &interrupted,
                    DiagnosticStage::Initialization,
                    role,
                )
            })?;
    }
    #[cfg(feature = "skills")]
    if let Some(proposal_effects) = proposal_effects {
        install_proposal_global(&context, proposal_effects).map_err(|error| {
            classify_error(
                &context,
                error,
                deadline,
                &interrupted,
                DiagnosticStage::Initialization,
                role,
            )
        })?;
    }
    let clone = functions.strict_clone;
    let string_gate = functions.string_gate;
    let exception_inspector = functions.exception_inspector;
    let async_completion_value = functions.async_completion_value;
    let exception_diagnostics = ExceptionDiagnostics {
        inspector: &exception_inspector,
        model_source: (role == ScriptRole::Model).then_some(source),
    };
    #[cfg(feature = "skills")]
    let mut loaded_artifacts = Vec::with_capacity(artifacts.len());
    #[cfg(feature = "skills")]
    for cached in artifacts {
        let artifact = cached.artifact.as_ref();
        let artifact_bindings = bindings.get(&artifact.id).cloned().ok_or_else(|| {
            ClosedFailure::error(
                JsErrorCode::Internal,
                DiagnosticStage::Initialization,
                ScriptRole::SkillSource,
            )
        })?;
        let loaded = super::realm::load_artifact_with_bound_exports_bytecode(
            &runtime,
            &context,
            artifact,
            &cached.bytecode,
            capability_runtime.clone(),
            artifact_bindings,
        )
        .map_err(|_| {
            ClosedFailure::error(
                JsErrorCode::Internal,
                DiagnosticStage::Initialization,
                ScriptRole::SkillSource,
            )
        })?;
        loaded_artifacts.push(loaded);
        event_state
            .lock()
            .map_err(|_| {
                ClosedFailure::error(
                    JsErrorCode::Internal,
                    DiagnosticStage::Initialization,
                    ScriptRole::SkillSource,
                )
            })?
            .injected(
                artifact.id.clone(),
                turn_id.to_string(),
                tool_call_id.to_string(),
            );
    }
    let value = evaluate(
        &context,
        source,
        &runtime,
        deadline,
        &interrupted,
        role,
        &exception_diagnostics,
    )?;
    let mut remaining_jobs = limits.max_pending_jobs;
    drain_jobs(
        &runtime,
        deadline,
        &interrupted,
        &mut remaining_jobs,
        role,
        &exception_diagnostics,
    )?;
    settle_and_convert(
        &runtime,
        &context,
        value,
        clone,
        string_gate,
        async_completion_value,
        deadline,
        &interrupted,
        role,
        &exception_diagnostics,
    )
}

fn install_console(
    context: &Context,
    records: Arc<Mutex<Vec<ConsoleRecord>>>,
    wrapper: Persistent<Function<'static>>,
) -> rquickjs::Result<()> {
    context.with(|ctx| {
        let console = Object::new(ctx.clone())?;
        let wrapper = wrapper.restore(&ctx)?;
        for (name, level) in [
            ("log", ConsoleLevel::Log),
            ("warn", ConsoleLevel::Warn),
            ("error", ConsoleLevel::Error),
        ] {
            let records = records.clone();
            let emit = Function::new(ctx.clone(), move |text: String, truncated: bool| {
                record_console(&records, level, text, truncated);
            })?;
            let function: Function = wrapper.clone().call((emit,))?;
            console.set(name, function)?;
        }
        ctx.globals().set("console", console)
    })
}

fn record_console(
    records: &Arc<Mutex<Vec<ConsoleRecord>>>,
    level: ConsoleLevel,
    text: String,
    already_truncated: bool,
) {
    let mut records = records
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if records.len() >= MAX_CONSOLE_RECORDS {
        if let Some(last) = records.last_mut() {
            last.truncated = true;
        }
        return;
    }
    let used = records
        .iter()
        .map(|record| record.text.len())
        .sum::<usize>();
    let available = MAX_CONSOLE_BYTES.saturating_sub(used);
    if available == 0 {
        if let Some(last) = records.last_mut() {
            last.truncated = true;
        }
        return;
    }
    let maximum = available.min(MAX_CONSOLE_RECORD_BYTES);
    let bounded = truncate_utf8(&text, maximum);
    records.push(ConsoleRecord {
        level,
        truncated: already_truncated || bounded.len() < text.len(),
        text: bounded,
    });
}

fn truncate_utf8(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn evaluate(
    context: &Context,
    source: &str,
    runtime: &Runtime,
    deadline: Instant,
    interrupted: &AtomicBool,
    role: ScriptRole,
    exception_diagnostics: &ExceptionDiagnostics<'_>,
) -> Result<Persistent<Value<'static>>, ClosedFailure> {
    context
        .with(|ctx| {
            let filename = if role == ScriptRole::Model {
                MODEL_SCRIPT_NAME
            } else {
                "mini-agent-verification.js"
            };
            let mut options = EvalOptions::default();
            options.filename = Some(filename.to_string());
            options.promise = role == ScriptRole::Model;
            ctx.eval_with_options::<Value, _>(source, options)
                .map(|value| Persistent::save(&ctx, value))
        })
        .map_err(|error| {
            classify_evaluation_error(
                context,
                runtime,
                error,
                deadline,
                interrupted,
                DiagnosticStage::Evaluation,
                role,
                exception_diagnostics,
            )
        })
}

fn drain_jobs(
    runtime: &Runtime,
    deadline: Instant,
    interrupted: &AtomicBool,
    remaining_jobs: &mut usize,
    role: ScriptRole,
    exception_diagnostics: &ExceptionDiagnostics<'_>,
) -> Result<(), ClosedFailure> {
    loop {
        if interrupted.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return Err(ClosedFailure::timeout(DiagnosticStage::JobDrain, role));
        }
        if *remaining_jobs == 0 {
            return if runtime.is_job_pending() {
                Err(ClosedFailure::error(
                    JsErrorCode::JobLimit,
                    DiagnosticStage::JobDrain,
                    role,
                ))
            } else {
                Ok(())
            };
        }
        match runtime.execute_pending_job() {
            Ok(true) => *remaining_jobs -= 1,
            Ok(false) => return Ok(()),
            Err(exception) => {
                let near_heap_limit = runtime_is_near_heap_limit(runtime);
                return Err(exception.0.with(|ctx| {
                    if interrupted.load(Ordering::Relaxed) || Instant::now() >= deadline {
                        let _ = ctx.catch();
                        ClosedFailure::timeout(DiagnosticStage::JobDrain, role)
                    } else if near_heap_limit {
                        let _ = ctx.catch();
                        ClosedFailure::out_of_memory(DiagnosticStage::JobDrain, role)
                    } else {
                        let thrown = ctx.catch();
                        classify_thrown_exception(
                            &ctx,
                            thrown,
                            deadline,
                            interrupted,
                            DiagnosticStage::JobDrain,
                            role,
                            exception_diagnostics,
                        )
                    }
                }));
            }
        }
    }
}

// Settlement receives the already-captured runtime guards as separate values so
// no reusable QuickJS state can be hidden in a context object.
#[allow(clippy::too_many_arguments)]
fn settle_and_convert(
    runtime: &Runtime,
    context: &Context,
    value: Persistent<Value<'static>>,
    clone: Persistent<Function<'static>>,
    string_gate: Persistent<Function<'static>>,
    async_completion_value: Persistent<Function<'static>>,
    deadline: Instant,
    interrupted: &AtomicBool,
    role: ScriptRole,
    exception_diagnostics: &ExceptionDiagnostics<'_>,
) -> Result<StepOutcome, ClosedFailure> {
    let near_heap_limit = runtime_is_near_heap_limit(runtime);
    context.with(|ctx| {
        let mut value = value.restore(&ctx).map_err(|error| {
            classify_ctx_error(
                &ctx,
                error,
                deadline,
                interrupted,
                DiagnosticStage::ResultConversion,
                role,
                Some(exception_diagnostics),
            )
        })?;
        let mut unwrap_async_completion = role == ScriptRole::Model;
        loop {
            if let Some(promise) = value.as_promise() {
                value = match promise.state() {
                    PromiseState::Resolved => promise
                        .result::<Value>()
                        .and_then(Result::ok)
                        .ok_or_else(|| {
                            ClosedFailure::error(
                                JsErrorCode::Internal,
                                DiagnosticStage::ResultConversion,
                                role,
                            )
                        })?,
                    PromiseState::Rejected => {
                        let rejected = promise.result::<Value>();
                        if near_heap_limit {
                            if matches!(rejected, Some(Err(Error::Exception))) {
                                let _ = ctx.catch();
                            }
                            return Err(ClosedFailure::out_of_memory(
                                DiagnosticStage::Evaluation,
                                role,
                            ));
                        }
                        let Some(Err(error)) = rejected else {
                            return Err(ClosedFailure::error(
                                JsErrorCode::Internal,
                                DiagnosticStage::Evaluation,
                                role,
                            ));
                        };
                        return Err(classify_ctx_error(
                            &ctx,
                            error,
                            deadline,
                            interrupted,
                            DiagnosticStage::Evaluation,
                            role,
                            Some(exception_diagnostics),
                        ));
                    }
                    PromiseState::Pending => {
                        return Err(ClosedFailure::error(
                            JsErrorCode::JobLimit,
                            DiagnosticStage::JobDrain,
                            role,
                        ));
                    }
                };
                continue;
            }
            if unwrap_async_completion {
                unwrap_async_completion = false;
                let extractor = async_completion_value.clone().restore(&ctx).map_err(|_| {
                    ClosedFailure::error(
                        JsErrorCode::Internal,
                        DiagnosticStage::ResultConversion,
                        role,
                    )
                })?;
                value = extractor.call((value,)).map_err(|error| {
                    if matches!(error, Error::Exception) {
                        let _ = ctx.catch();
                    }
                    if interrupted.load(Ordering::Relaxed) || Instant::now() >= deadline {
                        ClosedFailure::timeout(DiagnosticStage::ResultConversion, role)
                    } else if near_heap_limit {
                        ClosedFailure::out_of_memory(DiagnosticStage::ResultConversion, role)
                    } else {
                        ClosedFailure::error(
                            JsErrorCode::Internal,
                            DiagnosticStage::ResultConversion,
                            role,
                        )
                    }
                })?;
                continue;
            }
            break;
        }
        convert_value(
            &ctx,
            value,
            clone,
            string_gate,
            deadline,
            interrupted,
            role,
            exception_diagnostics,
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn convert_value<'js>(
    ctx: &Ctx<'js>,
    value: Value<'js>,
    clone: Persistent<Function<'static>>,
    string_gate: Persistent<Function<'static>>,
    deadline: Instant,
    interrupted: &AtomicBool,
    role: ScriptRole,
    exception_diagnostics: &ExceptionDiagnostics<'_>,
) -> Result<StepOutcome, ClosedFailure> {
    if value.is_undefined() || value.is_null() {
        return Ok(StepOutcome::Void);
    }
    if value.is_string() {
        let string_gate = string_gate.restore(ctx).map_err(|error| {
            classify_ctx_error(
                ctx,
                error,
                deadline,
                interrupted,
                DiagnosticStage::ResultConversion,
                role,
                Some(exception_diagnostics),
            )
        })?;
        let bounded = string_gate.call::<_, String>((value,)).map_err(|error| {
            let failure = classify_ctx_error(
                ctx,
                error,
                deadline,
                interrupted,
                DiagnosticStage::ResultConversion,
                role,
                Some(exception_diagnostics),
            );
            match failure.outcome {
                StepOutcome::Timeout | StepOutcome::OutOfMemory => failure,
                _ => ClosedFailure::error(
                    JsErrorCode::InvalidResult,
                    DiagnosticStage::ResultConversion,
                    role,
                ),
            }
        })?;
        return Ok(StepOutcome::Value(bounded));
    }
    let primitive = if let Some(value) = value.as_int() {
        Some(value.to_string())
    } else if let Some(value) = value.as_float() {
        value.is_finite().then(|| value.to_string())
    } else {
        value.as_bool().map(|value| value.to_string())
    };
    if let Some(primitive) = primitive {
        return if primitive.len() <= MAX_RESULT_BYTES {
            Ok(StepOutcome::Value(primitive))
        } else {
            Err(ClosedFailure::error(
                JsErrorCode::InvalidResult,
                DiagnosticStage::ResultConversion,
                role,
            ))
        };
    }
    if !value.is_object() {
        return Err(ClosedFailure::error(
            JsErrorCode::InvalidResult,
            DiagnosticStage::ResultConversion,
            role,
        ));
    }
    let clone = clone.restore(ctx).map_err(|error| {
        classify_ctx_error(
            ctx,
            error,
            deadline,
            interrupted,
            DiagnosticStage::ResultConversion,
            role,
            Some(exception_diagnostics),
        )
    })?;
    let encoded = clone.call::<_, String>((value,)).map_err(|error| {
        let failure = classify_ctx_error(
            ctx,
            error,
            deadline,
            interrupted,
            DiagnosticStage::ResultConversion,
            role,
            Some(exception_diagnostics),
        );
        match failure.outcome {
            StepOutcome::Timeout | StepOutcome::OutOfMemory => failure,
            _ => ClosedFailure::error(
                JsErrorCode::InvalidResult,
                DiagnosticStage::ResultConversion,
                role,
            ),
        }
    })?;
    if encoded.len() > MAX_RESULT_BYTES {
        return Err(ClosedFailure::error(
            JsErrorCode::InvalidResult,
            DiagnosticStage::ResultConversion,
            role,
        ));
    }
    Ok(StepOutcome::Value(encoded))
}

fn initialization_failure(error: Error, role: ScriptRole) -> ClosedFailure {
    if matches!(error, Error::Allocation) {
        ClosedFailure::out_of_memory(DiagnosticStage::Initialization, role)
    } else {
        ClosedFailure::error(JsErrorCode::Internal, DiagnosticStage::Initialization, role)
    }
}

fn classify_error(
    context: &Context,
    error: Error,
    deadline: Instant,
    interrupted: &AtomicBool,
    stage: DiagnosticStage,
    role: ScriptRole,
) -> ClosedFailure {
    context.with(|ctx| classify_ctx_error(&ctx, error, deadline, interrupted, stage, role, None))
}

fn classify_ctx_error(
    ctx: &Ctx<'_>,
    error: Error,
    deadline: Instant,
    interrupted: &AtomicBool,
    stage: DiagnosticStage,
    role: ScriptRole,
    exception_diagnostics: Option<&ExceptionDiagnostics<'_>>,
) -> ClosedFailure {
    if interrupted.load(Ordering::Relaxed) || Instant::now() >= deadline {
        if matches!(error, Error::Exception) {
            let _ = ctx.catch();
        }
        return ClosedFailure::timeout(stage, role);
    }
    if matches!(error, Error::Allocation) {
        return ClosedFailure::out_of_memory(stage, role);
    }
    if !matches!(error, Error::Exception) {
        return ClosedFailure::error(JsErrorCode::Internal, stage, role);
    }

    let thrown = ctx.catch();
    exception_diagnostics.map_or_else(
        || ClosedFailure::error(JsErrorCode::Exception, stage, role),
        |diagnostics| {
            classify_thrown_exception(ctx, thrown, deadline, interrupted, stage, role, diagnostics)
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn classify_evaluation_error(
    context: &Context,
    runtime: &Runtime,
    error: Error,
    deadline: Instant,
    interrupted: &AtomicBool,
    stage: DiagnosticStage,
    role: ScriptRole,
    exception_diagnostics: &ExceptionDiagnostics<'_>,
) -> ClosedFailure {
    let near_heap_limit = runtime_is_near_heap_limit(runtime);
    context.with(|ctx| {
        if interrupted.load(Ordering::Relaxed) || Instant::now() >= deadline {
            if matches!(error, Error::Exception) {
                let _ = ctx.catch();
            }
            return ClosedFailure::timeout(stage, role);
        }
        if matches!(error, Error::Allocation) || near_heap_limit {
            return ClosedFailure::out_of_memory(stage, role);
        }
        if !matches!(error, Error::Exception) {
            return ClosedFailure::error(JsErrorCode::Internal, stage, role);
        }
        let thrown = ctx.catch();
        classify_thrown_exception(
            &ctx,
            thrown,
            deadline,
            interrupted,
            stage,
            role,
            exception_diagnostics,
        )
    })
}

fn runtime_is_near_heap_limit(runtime: &Runtime) -> bool {
    let usage = runtime.memory_usage();
    usage.malloc_size >= (MEMORY_LIMIT.saturating_sub(1024 * 1024)) as i64
}

#[cfg(feature = "skills")]
fn execute_verification(request: VerifyArtifact, limits: ExecutionLimits) -> VerificationResult {
    if request.cases.is_empty()
        || request.cases.len() > MAX_VERIFICATION_CASES
        || request.cases.iter().any(|case| {
            case.case_id.is_empty()
                || case.case_id.len() > MAX_VERIFICATION_CASE_ID_BYTES
                || case.script.len() > MAX_RESULT_BYTES
        })
    {
        return VerificationResult {
            passed: false,
            cases: Vec::new(),
            loader_version: VERIFICATION_LOADER_VERSION,
        };
    }
    let runtime = match Runtime::new() {
        Ok(runtime) => runtime,
        Err(_) => return failed_skill_verification(&request, DiagnosticClass::Internal),
    };
    runtime.set_memory_limit(MEMORY_LIMIT);
    runtime.set_max_stack_size(STACK_LIMIT);
    let deadline = Instant::now() + limits.timeout;
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupt_flag = interrupted.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || {
        let expired = Instant::now() >= deadline;
        if expired {
            interrupt_flag.store(true, Ordering::Relaxed);
        }
        expired
    })));

    let mut results = Vec::with_capacity(request.cases.len());
    let transcript_budget = super::skills::fakes::VerificationTranscriptBudget::new();
    let mut transcript_calls_remaining = super::skills::fakes::VERIFICATION_TRANSCRIPT_MAX_CALLS;
    let mut terminal = None;
    for (case_index, case) in request.cases.iter().enumerate() {
        if let Some(diagnostic) = terminal.clone() {
            results.push(failed_case(case.case_id.clone(), diagnostic));
            continue;
        }
        if runtime.is_job_pending() {
            let diagnostic = diagnostic(
                DiagnosticClass::Contract,
                DiagnosticStage::JobDrain,
                verification_case_role(&case.kind),
            );
            terminal = Some(diagnostic.clone());
            results.push(failed_case(case.case_id.clone(), diagnostic));
            continue;
        }
        let mut result = execute_isolated_skill_verification_case(
            &runtime,
            &request.artifact,
            case,
            case_index,
            deadline,
            &interrupted,
            limits.max_pending_jobs,
            transcript_budget.clone(),
        );
        if transcript_budget.exceeded() {
            let limit_diagnostic = diagnostic(
                DiagnosticClass::Contract,
                DiagnosticStage::Verification,
                verification_case_role(&case.kind),
            );
            result = failed_case(case.case_id.clone(), limit_diagnostic.clone());
            terminal = Some(limit_diagnostic);
        }
        if result.transcript.call_count() > transcript_calls_remaining {
            if result.passed {
                result = failed_case(
                    case.case_id.clone(),
                    diagnostic(
                        DiagnosticClass::Contract,
                        DiagnosticStage::Verification,
                        verification_case_role(&case.kind),
                    ),
                );
            }
            result
                .transcript
                .limit_call_count(&mut transcript_calls_remaining);
        } else {
            transcript_calls_remaining -= result.transcript.call_count();
        }
        if result.diagnostic.as_ref().is_some_and(|diagnostic| {
            diagnostic.class == DiagnosticClass::ResourceLimit
                || diagnostic.stage == DiagnosticStage::JobDrain
        }) {
            terminal = result.diagnostic.clone();
        }
        results.push(result);
    }
    VerificationResult {
        passed: results.iter().all(|case| case.passed),
        cases: results,
        loader_version: VERIFICATION_LOADER_VERSION,
    }
}

#[cfg(feature = "skills")]
fn verification_case_role(kind: &super::protocol::VerificationCaseKind) -> ScriptRole {
    use super::protocol::VerificationCaseKind;
    match kind {
        VerificationCaseKind::Embedded => ScriptRole::EmbeddedTest,
        VerificationCaseKind::Mutation { .. } => ScriptRole::MutationTest,
        VerificationCaseKind::Inherited => ScriptRole::InheritedTest,
        VerificationCaseKind::HeldOut { .. } => ScriptRole::HeldOutTest,
    }
}

#[cfg(feature = "skills")]
#[allow(clippy::too_many_arguments)]
fn execute_isolated_skill_verification_case(
    runtime: &Runtime,
    artifact: &super::skills::SkillArtifact,
    case: &super::protocol::VerificationCase,
    case_index: usize,
    deadline: Instant,
    interrupted: &AtomicBool,
    max_pending_jobs: usize,
    transcript_budget: super::skills::fakes::VerificationTranscriptBudget,
) -> VerificationCaseResult {
    use super::protocol::VerificationCaseKind;
    use super::skills::fakes::{FakeHostGlobals, FakeTranscript};

    let role = verification_case_role(&case.kind);
    let fakes =
        FakeHostGlobals::with_transcript_budget(artifact.capability.clone(), transcript_budget);
    if let VerificationCaseKind::HeldOut {
        fake_files,
        fake_spawns,
        fake_fetches,
        ..
    } = &case.kind
        && (fake_files.len() > 32
            || fake_files.iter().any(|(path, contents)| {
                path.is_empty() || path.len() > 4 * 1024 || contents.len() > 64 * 1024
            })
            || fake_files
                .iter()
                .any(|(path, contents)| fakes.seed_file(path, contents).is_err())
            || fake_spawns
                .iter()
                .any(|fixture| fakes.seed_spawn(fixture).is_err())
            || fake_fetches
                .iter()
                .any(|fixture| fakes.seed_fetch(fixture).is_err()))
    {
        return VerificationCaseResult {
            case_id: case.case_id.clone(),
            passed: false,
            diagnostic: Some(diagnostic(
                DiagnosticClass::Contract,
                DiagnosticStage::Initialization,
                role,
            )),
            transcript: FakeTranscript::default(),
        };
    }
    let dispatch_fakes = fakes.clone();
    let manifest = artifact.capability.clone();
    let capabilities = InvocationCapabilityRuntime::new(move |effect| {
        execute_verification_fake(&manifest, &dispatch_fakes, effect.request.operation)
    });
    let bindings = match prepare_verification_bindings(artifact, &capabilities, case_index) {
        Ok(bindings) => bindings,
        Err(()) => {
            return VerificationCaseResult {
                case_id: case.case_id.clone(),
                passed: false,
                diagnostic: Some(diagnostic(
                    DiagnosticClass::Internal,
                    DiagnosticStage::Initialization,
                    role,
                )),
                transcript: fakes.transcript().bounded_for_wire(),
            };
        }
    };
    let context = match Context::full(runtime) {
        Ok(context) => context,
        Err(_) => {
            return VerificationCaseResult {
                case_id: case.case_id.clone(),
                passed: false,
                diagnostic: Some(diagnostic(
                    DiagnosticClass::Internal,
                    DiagnosticStage::Initialization,
                    role,
                )),
                transcript: fakes.transcript().bounded_for_wire(),
            };
        }
    };
    let Some(bytecode) = trusted_bootstrap_bytecode() else {
        return VerificationCaseResult {
            case_id: case.case_id.clone(),
            passed: false,
            diagnostic: Some(diagnostic(
                DiagnosticClass::Internal,
                DiagnosticStage::Initialization,
                role,
            )),
            transcript: fakes.transcript().bounded_for_wire(),
        };
    };
    let exception_inspector = match load_trusted_bootstrap_functions(&context, bytecode) {
        Ok(functions) => functions.exception_inspector,
        Err(_) => {
            return VerificationCaseResult {
                case_id: case.case_id.clone(),
                passed: false,
                diagnostic: Some(diagnostic(
                    DiagnosticClass::Internal,
                    DiagnosticStage::Initialization,
                    role,
                )),
                transcript: fakes.transcript().bounded_for_wire(),
            };
        }
    };
    let exception_diagnostics = ExceptionDiagnostics {
        inspector: &exception_inspector,
        model_source: None,
    };
    let mutation = match &case.kind {
        VerificationCaseKind::Mutation {
            export_name,
            mutation,
        } => Some((export_name.as_str(), *mutation)),
        _ => None,
    };
    let loaded = match super::realm::load_artifact_with_bound_exports_for_verification(
        runtime,
        &context,
        artifact,
        capabilities.clone(),
        bindings,
        mutation,
    ) {
        Ok(loaded) => loaded,
        Err(error) => {
            // Realm errors are deliberately closed, so preserve the worker-owned
            // interrupt flag before translating the loader's error category.
            let class = if interrupted.load(Ordering::Relaxed) {
                DiagnosticClass::ResourceLimit
            } else {
                match error {
                    super::realm::RealmError::Identity
                    | super::realm::RealmError::InvalidExport
                    | super::realm::RealmError::DuplicateExport
                    | super::realm::RealmError::ExportCollision
                    | super::realm::RealmError::MissingExport
                    | super::realm::RealmError::PendingInitializationJobs => {
                        DiagnosticClass::Contract
                    }
                    super::realm::RealmError::Initialization
                    | super::realm::RealmError::PrivateLibraryCompilation
                    | super::realm::RealmError::PrivateLibraryBytecodeLoad
                    | super::realm::RealmError::PrivateLibraryModuleEvaluation
                    | super::realm::RealmError::PrivateLibraryExportLookup
                    | super::realm::RealmError::PrivateLibraryFactoryExecution
                    | super::realm::RealmError::WrapperInstallation => DiagnosticClass::Exception,
                }
            };
            return VerificationCaseResult {
                case_id: case.case_id.clone(),
                passed: false,
                diagnostic: Some(diagnostic(
                    class,
                    DiagnosticStage::Initialization,
                    ScriptRole::SkillSource,
                )),
                transcript: fakes.transcript().bounded_for_wire(),
            };
        }
    };
    let mut remaining_jobs = max_pending_jobs;
    let mut result = match &case.kind {
        VerificationCaseKind::Embedded | VerificationCaseKind::Inherited => {
            execute_verification_case(
                runtime,
                &context,
                case.case_id.clone(),
                &case.script,
                role,
                deadline,
                interrupted,
                &mut remaining_jobs,
                &exception_diagnostics,
            )
            .0
        }
        VerificationCaseKind::HeldOut { expected, .. } => execute_held_out_verification_case(
            runtime,
            &context,
            case,
            expected,
            deadline,
            interrupted,
            &mut remaining_jobs,
            &exception_diagnostics,
        ),
        VerificationCaseKind::Mutation { .. } => execute_mutation_verification_case(
            runtime,
            &context,
            artifact,
            &case.case_id,
            deadline,
            interrupted,
            &mut remaining_jobs,
            &exception_diagnostics,
        ),
    };
    drop(loaded);
    drop(context);
    if runtime.is_job_pending() && result.passed {
        result = failed_case(
            case.case_id.clone(),
            diagnostic(DiagnosticClass::Contract, DiagnosticStage::JobDrain, role),
        );
    }
    let transcript = fakes.transcript();
    if transcript.exceeds_wire_call_limit() && result.passed {
        result = failed_case(
            case.case_id.clone(),
            diagnostic(
                DiagnosticClass::Contract,
                DiagnosticStage::Verification,
                role,
            ),
        );
    }
    result.transcript = transcript.bounded_for_wire();
    result
}

#[cfg(feature = "skills")]
fn prepare_verification_bindings(
    artifact: &super::skills::SkillArtifact,
    capabilities: &InvocationCapabilityRuntime,
    case_index: usize,
) -> Result<std::collections::HashMap<String, super::realm::BoundExportInvocation>, ()> {
    use std::collections::HashMap;

    let mut bindings = HashMap::with_capacity(artifact.exports.len());
    for (export_index, export) in artifact.exports.iter().enumerate() {
        let capabilities = capabilities.clone();
        let artifact_id = artifact.id.clone();
        let export_name = export.name.clone();
        let manifest = artifact.capability.clone();
        let authorize = Arc::new(move |call_ordinal: u32| {
            let invocation = format!("verify-{case_index}-{export_index}-{call_ordinal}");
            let invocation_id =
                super::protocol::InvocationId::new(invocation.clone()).map_err(|_| ())?;
            let authorization = InvocationAuthorization::new(
                invocation_id,
                artifact_id.clone(),
                export_name.clone(),
                manifest.clone(),
                manifest.grants.iter().map(|scope| {
                    (
                        scope.capability(),
                        super::protocol::GrantId::new(uuid::Uuid::new_v4())
                            .expect("random verification grant is non-nil"),
                    )
                }),
            )
            .map_err(|_| ())?;
            let handle = capabilities.prepare(authorization).map_err(|_| ())?;
            Ok((handle, invocation))
        });
        bindings.insert(
            export.name.clone(),
            super::realm::BoundExportInvocation {
                authorize,
                on_start: Arc::new(|_, _| Ok(())),
                on_terminal: Arc::new(|_, _| Ok(())),
            },
        );
    }
    Ok(bindings)
}

#[cfg(feature = "skills")]
fn execute_verification_fake(
    manifest: &super::skills::CapabilityManifest,
    fakes: &super::skills::fakes::FakeHostGlobals,
    operation: EffectOperation,
) -> Result<EffectResult, super::skills::capability::CapabilityError> {
    use super::skills::capability::CapabilityError;
    if !verification_scope_allows(manifest, &operation) {
        return Err(CapabilityError::DispatchDenied);
    }
    match operation {
        EffectOperation::ReadFile { path } => fakes
            .read_file(&path)
            .map(|content| EffectResult::ReadFile { content })
            .map_err(|_| CapabilityError::DispatchDenied),
        EffectOperation::ReadFiles { paths } => paths
            .iter()
            .map(|path| fakes.read_file(path))
            .collect::<Result<Vec<_>, _>>()
            .map(|contents| EffectResult::ReadFiles { contents })
            .map_err(|_| CapabilityError::DispatchDenied),
        EffectOperation::ListDir { .. }
        | EffectOperation::Glob { .. }
        | EffectOperation::Grep { .. } => Err(CapabilityError::DispatchDenied),
        EffectOperation::WriteFile { path, content } => fakes
            .write_file(&path, &content)
            .map(|()| EffectResult::WriteFile)
            .map_err(|_| CapabilityError::DispatchDenied),
        EffectOperation::Spawn { program, arguments } => fakes
            .spawn(&program, &arguments)
            .map(|response| EffectResult::Spawn {
                stdout: response.stdout,
                stderr: response.stderr,
                exit_code: response.code,
                timed_out: response.timed_out,
                stdout_truncated: response.stdout_truncated,
                stderr_truncated: response.stderr_truncated,
            })
            .map_err(|_| CapabilityError::DispatchDenied),
        EffectOperation::Fetch { url, method, .. } => {
            let method = match method {
                super::protocol::HttpMethod::Get => "GET",
                super::protocol::HttpMethod::Post => "POST",
            };
            fakes
                .fetch(&url, method)
                .map(|response| EffectResult::Fetch {
                    status: response.status,
                    body: response.body,
                })
                .map_err(|_| CapabilityError::DispatchDenied)
        }
        EffectOperation::Result { .. }
        | EffectOperation::ScratchPut { .. }
        | EffectOperation::ScratchGet { .. }
        | EffectOperation::ProposeSkill { .. } => Err(CapabilityError::DispatchDenied),
    }
}

#[cfg(feature = "skills")]
fn verification_scope_allows(
    manifest: &super::skills::CapabilityManifest,
    operation: &EffectOperation,
) -> bool {
    use super::skills::{CapabilityScope, HostCapability, HttpMethod as SkillHttpMethod};
    match operation {
        EffectOperation::ReadFile { path }
        | EffectOperation::ListDir { path }
        | EffectOperation::Glob { path, .. }
        | EffectOperation::Grep { path, .. } => manifest
            .scope(HostCapability::ReadFile)
            .and_then(|scope| match scope {
                CapabilityScope::ReadFile { workspace_prefixes } => Some(workspace_prefixes),
                _ => None,
            })
            .is_some_and(|prefixes| {
                prefixes
                    .iter()
                    .any(|prefix| virtual_path_in_scope(prefix, path))
            }),
        EffectOperation::ReadFiles { paths } => manifest
            .scope(HostCapability::ReadFile)
            .and_then(|scope| match scope {
                CapabilityScope::ReadFile { workspace_prefixes } => Some(workspace_prefixes),
                _ => None,
            })
            .is_some_and(|prefixes| {
                paths.iter().all(|path| {
                    prefixes
                        .iter()
                        .any(|prefix| virtual_path_in_scope(prefix, path))
                })
            }),
        EffectOperation::WriteFile { path, .. } => manifest
            .scope(HostCapability::WriteFile)
            .and_then(|scope| match scope {
                CapabilityScope::WriteFile { workspace_prefixes } => Some(workspace_prefixes),
                _ => None,
            })
            .is_some_and(|prefixes| {
                prefixes
                    .iter()
                    .any(|prefix| virtual_path_in_scope(prefix, path))
            }),
        EffectOperation::Spawn { program, .. } => manifest
            .scope(HostCapability::Spawn)
            .and_then(|scope| match scope {
                CapabilityScope::Spawn { programs } => Some(programs),
                _ => None,
            })
            .is_some_and(|programs| programs.contains(program)),
        EffectOperation::Fetch { url, method, .. } => {
            let Ok(url) = reqwest::Url::parse(url) else {
                return false;
            };
            let origin = url.origin().ascii_serialization();
            manifest
                .scope(HostCapability::Fetch)
                .and_then(|scope| match scope {
                    CapabilityScope::Fetch { origins, methods } => Some((origins, methods)),
                    _ => None,
                })
                .is_some_and(|(origins, methods)| {
                    origins.contains(&origin)
                        && methods.iter().any(|allowed| {
                            matches!(
                                (allowed, method),
                                (SkillHttpMethod::Get, super::protocol::HttpMethod::Get)
                                    | (SkillHttpMethod::Post, super::protocol::HttpMethod::Post)
                            )
                        })
                })
        }
        EffectOperation::Result { .. }
        | EffectOperation::ScratchPut { .. }
        | EffectOperation::ScratchGet { .. }
        | EffectOperation::ProposeSkill { .. } => false,
    }
}

#[cfg(feature = "skills")]
fn virtual_path_in_scope(prefix: &str, path: &str) -> bool {
    !path.starts_with('/')
        && !path.split('/').any(|component| component == "..")
        && (path == prefix
            || path
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with('/')))
}

#[cfg(feature = "skills")]
#[allow(clippy::too_many_arguments)]
fn execute_held_out_verification_case(
    runtime: &Runtime,
    context: &Context,
    case: &super::protocol::VerificationCase,
    expected: &super::protocol::VerificationExpectedValue,
    deadline: Instant,
    interrupted: &AtomicBool,
    remaining_jobs: &mut usize,
    exception_diagnostics: &ExceptionDiagnostics<'_>,
) -> VerificationCaseResult {
    let role = ScriptRole::HeldOutTest;
    let outcome = evaluate(
        context,
        &case.script,
        runtime,
        deadline,
        interrupted,
        role,
        exception_diagnostics,
    )
    .and_then(|value| {
        drain_jobs(
            runtime,
            deadline,
            interrupted,
            remaining_jobs,
            role,
            exception_diagnostics,
        )?;
        context.with(|ctx| {
            let value = value.restore(&ctx).map_err(|error| {
                classify_ctx_error(
                    &ctx,
                    error,
                    deadline,
                    interrupted,
                    DiagnosticStage::Verification,
                    role,
                    Some(exception_diagnostics),
                )
            })?;
            verification_expected_matches(expected, &value)
                .then_some(())
                .ok_or_else(|| {
                    ClosedFailure::error(
                        JsErrorCode::InvalidResult,
                        DiagnosticStage::Verification,
                        role,
                    )
                })
        })
    });
    match outcome {
        Ok(()) => VerificationCaseResult {
            case_id: case.case_id.clone(),
            passed: true,
            diagnostic: None,
            transcript: Default::default(),
        },
        Err(failure) => failed_case(case.case_id.clone(), failure.diagnostic),
    }
}

#[cfg(feature = "skills")]
fn verification_expected_matches(
    expected: &super::protocol::VerificationExpectedValue,
    actual: &Value<'_>,
) -> bool {
    use super::protocol::VerificationExpectedValue;
    match expected {
        VerificationExpectedValue::Boolean(expected) => actual.as_bool() == Some(*expected),
        VerificationExpectedValue::String(expected) => actual
            .as_string()
            .and_then(|value| value.to_string().ok())
            .is_some_and(|actual| actual == *expected),
        VerificationExpectedValue::Integer(expected) => actual
            .as_int()
            .is_some_and(|actual| i64::from(actual) == *expected),
        VerificationExpectedValue::Float(expected) => actual
            .as_float()
            .or_else(|| actual.as_int().map(f64::from))
            .is_some_and(|actual| actual == *expected),
        VerificationExpectedValue::Null => actual.is_null(),
    }
}

#[cfg(feature = "skills")]
#[allow(clippy::too_many_arguments)]
fn execute_mutation_verification_case(
    runtime: &Runtime,
    context: &Context,
    artifact: &super::skills::SkillArtifact,
    case_id: &str,
    deadline: Instant,
    interrupted: &AtomicBool,
    remaining_jobs: &mut usize,
    exception_diagnostics: &ExceptionDiagnostics<'_>,
) -> VerificationCaseResult {
    for test in &artifact.tests {
        let (result, terminal) = execute_verification_case(
            runtime,
            context,
            case_id.to_string(),
            test,
            ScriptRole::MutationTest,
            deadline,
            interrupted,
            remaining_jobs,
            exception_diagnostics,
        );
        if !result.passed {
            if terminal {
                return result;
            }
            return VerificationCaseResult {
                case_id: case_id.to_string(),
                passed: true,
                diagnostic: None,
                transcript: Default::default(),
            };
        }
    }
    failed_case(
        case_id.to_string(),
        diagnostic(
            DiagnosticClass::Contract,
            DiagnosticStage::Verification,
            ScriptRole::MutationTest,
        ),
    )
}

#[cfg(feature = "skills")]
fn failed_skill_verification(
    request: &VerifyArtifact,
    class: DiagnosticClass,
) -> VerificationResult {
    let diagnostic = diagnostic(
        class,
        DiagnosticStage::Initialization,
        ScriptRole::SkillSource,
    );
    VerificationResult {
        passed: false,
        cases: request
            .cases
            .iter()
            .map(|case| failed_case(case.case_id.clone(), diagnostic.clone()))
            .collect(),
        loader_version: VERIFICATION_LOADER_VERSION,
    }
}

#[cfg(not(feature = "skills"))]
fn execute_verification(request: VerifyArtifact, limits: ExecutionLimits) -> VerificationResult {
    let case_count = request
        .artifact
        .tests
        .len()
        .saturating_add(request.cases.len());
    if case_count > MAX_VERIFICATION_CASES
        || request
            .cases
            .iter()
            .any(|case| case.case_id.len() > MAX_VERIFICATION_CASE_ID_BYTES)
    {
        return VerificationResult {
            passed: false,
            cases: Vec::new(),
            loader_version: VERIFICATION_LOADER_VERSION,
        };
    }
    let runtime = match Runtime::new() {
        Ok(runtime) => runtime,
        Err(_) => return failed_verification(&request, DiagnosticClass::Internal),
    };
    runtime.set_memory_limit(MEMORY_LIMIT);
    runtime.set_max_stack_size(STACK_LIMIT);
    let deadline = Instant::now() + limits.timeout;
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupt_flag = interrupted.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || {
        let expired = Instant::now() >= deadline;
        if expired {
            interrupt_flag.store(true, Ordering::Relaxed);
        }
        expired
    })));
    let context = match Context::full(&runtime) {
        Ok(context) => context,
        Err(_) => return failed_verification(&request, DiagnosticClass::Internal),
    };
    if context
        .with(|ctx| {
            Object::new(ctx.clone()).and_then(|exports| ctx.globals().set("exports", exports))
        })
        .is_err()
    {
        return failed_verification(&request, DiagnosticClass::Internal);
    }
    let Some(bytecode) = trusted_bootstrap_bytecode() else {
        return failed_verification(&request, DiagnosticClass::Internal);
    };
    let exception_inspector = match load_trusted_bootstrap_functions(&context, bytecode) {
        Ok(functions) => functions.exception_inspector,
        Err(_) => return failed_verification(&request, DiagnosticClass::Internal),
    };
    let exception_diagnostics = ExceptionDiagnostics {
        inspector: &exception_inspector,
        model_source: None,
    };

    let mut remaining_jobs = limits.max_pending_jobs;
    let source = evaluate(
        &context,
        &request.artifact.source,
        &runtime,
        deadline,
        &interrupted,
        ScriptRole::SkillSource,
        &exception_diagnostics,
    )
    .and_then(|value| {
        drain_jobs(
            &runtime,
            deadline,
            &interrupted,
            &mut remaining_jobs,
            ScriptRole::SkillSource,
            &exception_diagnostics,
        )?;
        ensure_source_settled(
            &runtime,
            &context,
            value,
            deadline,
            &interrupted,
            &exception_diagnostics,
        )
    });
    if let Err(failure) = source {
        return failed_verification_with(&request, failure.diagnostic);
    }

    let mut cases = Vec::with_capacity(request.artifact.tests.len() + request.cases.len());
    let mut terminal_diagnostic: Option<Diagnostic> = None;
    for (index, script) in request.artifact.tests.iter().enumerate() {
        let case_id = format!("embedded-{index}");
        if let Some(diagnostic) = &terminal_diagnostic {
            cases.push(failed_case(case_id, diagnostic.clone()));
        } else {
            let (case, terminal) = execute_verification_case(
                &runtime,
                &context,
                case_id,
                script,
                ScriptRole::EmbeddedTest,
                deadline,
                &interrupted,
                &mut remaining_jobs,
                &exception_diagnostics,
            );
            if terminal {
                terminal_diagnostic = case.diagnostic.clone();
            }
            cases.push(case);
        }
    }
    for case in &request.cases {
        if let Some(diagnostic) = &terminal_diagnostic {
            cases.push(failed_case(case.case_id.clone(), diagnostic.clone()));
        } else {
            let (result, terminal) = execute_verification_case(
                &runtime,
                &context,
                case.case_id.clone(),
                &case.script,
                ScriptRole::HeldOutTest,
                deadline,
                &interrupted,
                &mut remaining_jobs,
                &exception_diagnostics,
            );
            if terminal {
                terminal_diagnostic = result.diagnostic.clone();
            }
            cases.push(result);
        }
    }
    VerificationResult {
        passed: cases.iter().all(|case| case.passed),
        cases,
        loader_version: VERIFICATION_LOADER_VERSION,
    }
}

#[cfg(not(feature = "skills"))]
fn ensure_source_settled(
    runtime: &Runtime,
    context: &Context,
    value: Persistent<Value<'static>>,
    deadline: Instant,
    interrupted: &AtomicBool,
    exception_diagnostics: &ExceptionDiagnostics<'_>,
) -> Result<(), ClosedFailure> {
    let near_heap_limit = runtime_is_near_heap_limit(runtime);
    context.with(|ctx| {
        let value = value.restore(&ctx).map_err(|error| {
            classify_ctx_error(
                &ctx,
                error,
                deadline,
                interrupted,
                DiagnosticStage::Verification,
                ScriptRole::SkillSource,
                Some(exception_diagnostics),
            )
        })?;
        let Some(promise) = value.as_promise() else {
            return Ok(());
        };
        match promise.state() {
            PromiseState::Resolved => match promise.result::<Value>() {
                Some(Ok(_)) => Ok(()),
                _ => Err(ClosedFailure::error(
                    JsErrorCode::Internal,
                    DiagnosticStage::Verification,
                    ScriptRole::SkillSource,
                )),
            },
            PromiseState::Rejected => {
                let rejected = promise.result::<Value>();
                if near_heap_limit {
                    if matches!(rejected, Some(Err(Error::Exception))) {
                        let _ = ctx.catch();
                    }
                    Err(ClosedFailure::out_of_memory(
                        DiagnosticStage::Verification,
                        ScriptRole::SkillSource,
                    ))
                } else if let Some(Err(error)) = rejected {
                    Err(classify_ctx_error(
                        &ctx,
                        error,
                        deadline,
                        interrupted,
                        DiagnosticStage::Verification,
                        ScriptRole::SkillSource,
                        Some(exception_diagnostics),
                    ))
                } else {
                    Err(ClosedFailure::error(
                        JsErrorCode::Internal,
                        DiagnosticStage::Verification,
                        ScriptRole::SkillSource,
                    ))
                }
            }
            PromiseState::Pending => Err(ClosedFailure::error(
                JsErrorCode::JobLimit,
                DiagnosticStage::JobDrain,
                ScriptRole::SkillSource,
            )),
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn execute_verification_case(
    runtime: &Runtime,
    context: &Context,
    case_id: String,
    script: &str,
    role: ScriptRole,
    deadline: Instant,
    interrupted: &AtomicBool,
    remaining_jobs: &mut usize,
    exception_diagnostics: &ExceptionDiagnostics<'_>,
) -> (VerificationCaseResult, bool) {
    let result = evaluate(
        context,
        script,
        runtime,
        deadline,
        interrupted,
        role,
        exception_diagnostics,
    )
    .and_then(|value| {
        drain_jobs(
            runtime,
            deadline,
            interrupted,
            remaining_jobs,
            role,
            exception_diagnostics,
        )?;
        let near_heap_limit = runtime_is_near_heap_limit(runtime);
        context.with(|ctx| {
            let mut value = value.restore(&ctx).map_err(|error| {
                classify_ctx_error(
                    &ctx,
                    error,
                    deadline,
                    interrupted,
                    DiagnosticStage::Verification,
                    role,
                    Some(exception_diagnostics),
                )
            })?;
            if let Some(promise) = value.as_promise() {
                value = match promise.state() {
                    PromiseState::Resolved => promise
                        .result::<Value>()
                        .and_then(Result::ok)
                        .ok_or_else(|| {
                            ClosedFailure::error(
                                JsErrorCode::Internal,
                                DiagnosticStage::Verification,
                                role,
                            )
                        })?,
                    PromiseState::Rejected => {
                        let rejected = promise.result::<Value>();
                        if near_heap_limit {
                            if matches!(rejected, Some(Err(Error::Exception))) {
                                let _ = ctx.catch();
                            }
                            return Err(ClosedFailure::out_of_memory(
                                DiagnosticStage::Verification,
                                role,
                            ));
                        }
                        let Some(Err(error)) = rejected else {
                            return Err(ClosedFailure::error(
                                JsErrorCode::Internal,
                                DiagnosticStage::Verification,
                                role,
                            ));
                        };
                        return Err(classify_ctx_error(
                            &ctx,
                            error,
                            deadline,
                            interrupted,
                            DiagnosticStage::Verification,
                            role,
                            Some(exception_diagnostics),
                        ));
                    }
                    PromiseState::Pending => {
                        return Err(ClosedFailure::error(
                            JsErrorCode::JobLimit,
                            DiagnosticStage::JobDrain,
                            role,
                        ));
                    }
                };
            }
            if value.as_bool() == Some(true) {
                Ok(())
            } else {
                Err(ClosedFailure::error(
                    JsErrorCode::InvalidResult,
                    DiagnosticStage::Verification,
                    role,
                ))
            }
        })
    });
    match result {
        Ok(()) => (
            VerificationCaseResult {
                case_id,
                passed: true,
                diagnostic: None,
                #[cfg(feature = "skills")]
                transcript: Default::default(),
            },
            false,
        ),
        Err(failure) => {
            let terminal = matches!(
                failure.outcome,
                StepOutcome::Timeout
                    | StepOutcome::OutOfMemory
                    | StepOutcome::Error(JsErrorCode::JobLimit)
            );
            (failed_case(case_id, failure.diagnostic), terminal)
        }
    }
}

fn failed_case(case_id: String, diagnostic: Diagnostic) -> VerificationCaseResult {
    VerificationCaseResult {
        case_id,
        passed: false,
        diagnostic: Some(diagnostic),
        #[cfg(feature = "skills")]
        transcript: Default::default(),
    }
}

#[cfg(not(feature = "skills"))]
fn failed_verification(request: &VerifyArtifact, class: DiagnosticClass) -> VerificationResult {
    failed_verification_with(
        request,
        diagnostic(
            class,
            DiagnosticStage::Initialization,
            ScriptRole::SkillSource,
        ),
    )
}

#[cfg(not(feature = "skills"))]
fn failed_verification_with(
    request: &VerifyArtifact,
    diagnostic: Diagnostic,
) -> VerificationResult {
    let mut cases = request
        .artifact
        .tests
        .iter()
        .enumerate()
        .map(|(index, _)| VerificationCaseResult {
            case_id: format!("embedded-{index}"),
            passed: false,
            diagnostic: Some(diagnostic.clone()),
        })
        .collect::<Vec<_>>();
    cases.extend(request.cases.iter().map(|case| VerificationCaseResult {
        case_id: case.case_id.clone(),
        passed: false,
        diagnostic: Some(diagnostic.clone()),
    }));
    VerificationResult {
        passed: false,
        cases,
        loader_version: VERIFICATION_LOADER_VERSION,
    }
}

#[cfg(test)]
pub(crate) fn exit_test_worker() -> ! {
    std::process::exit(run_marked_worker())
}
