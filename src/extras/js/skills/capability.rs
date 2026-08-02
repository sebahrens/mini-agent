//! Runtime intersection of session authority and immutable skill manifests.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use crate::extras::js::protocol::{
    AdvisoryAttribution, EffectErrorCode, EffectOperation, EffectRequest, EffectResult, GrantId,
    HttpHeader, HttpMethod, InvocationId, MAX_EFFECTS_PER_STEP,
};

use super::{CapabilityManifest, CapabilityTier, HostCapability, IdentityError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillExecutionAttribution {
    pub skill_id: String,
    pub export_name: String,
    pub manifest: CapabilityManifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityDenied {
    pub skill_id: String,
    pub export_name: String,
    pub operation: HostCapability,
    pub reason: CapabilityDenialReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityDenialReason {
    Undeclared,
    SessionDenied,
    InvalidManifest,
}

#[derive(Debug, thiserror::Error)]
pub enum CapabilityError {
    #[error("skill attribution is missing an immutable full ID or export")]
    InvalidAttribution,
    #[error("invalid immutable capability manifest: {0}")]
    InvalidManifest(#[from] IdentityError),
    #[error("skill capability denied")]
    Denied(CapabilityDenied),
    #[error("invocation authorization is missing or invalid")]
    InvalidInvocation,
    #[error("invocation capability has been revoked")]
    Revoked,
    #[error("effect arguments are outside the invocation ABI")]
    InvalidArguments,
    #[error("effect dispatcher denied the request")]
    DispatchDenied,
}

/// Parent-issued authority for exactly one ABI-v2 export invocation.
#[derive(Debug, Clone)]
pub(crate) struct InvocationAuthorization {
    pub(crate) invocation_id: InvocationId,
    pub(crate) attribution: SkillExecutionAttribution,
    grants: BTreeMap<HostCapability, GrantId>,
}

impl InvocationAuthorization {
    pub(crate) fn new(
        invocation_id: InvocationId,
        skill_id: String,
        export_name: String,
        manifest: CapabilityManifest,
        grants: impl IntoIterator<Item = (HostCapability, GrantId)>,
    ) -> Result<Self, CapabilityError> {
        let attribution = SkillExecutionAttribution {
            skill_id,
            export_name,
            manifest,
        };
        validate_attribution(&attribution)?;
        let mut exact_grants = BTreeMap::new();
        for (capability, grant_id) in grants {
            if exact_grants.insert(capability, grant_id).is_some() {
                return Err(CapabilityError::InvalidInvocation);
            }
        }
        let declared = attribution
            .manifest
            .grants
            .iter()
            .map(|scope| scope.capability())
            .collect::<Vec<_>>();
        if exact_grants.len() != declared.len()
            || declared
                .iter()
                .any(|capability| !exact_grants.contains_key(capability))
            || exact_grants
                .values()
                .collect::<std::collections::HashSet<_>>()
                .len()
                != exact_grants.len()
        {
            return Err(CapabilityError::InvalidInvocation);
        }
        Ok(Self {
            invocation_id,
            attribution,
            grants: exact_grants,
        })
    }
}

/// An effect whose authoritative invocation identity travels separately from advisory fields.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct DispatchedEffect {
    pub(crate) invocation_id: InvocationId,
    pub(crate) request: EffectRequest,
}

/// Opaque one-shot binding between parent preparation and one wrapper entry.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PreparedInvocationHandle(u64);

type EffectDispatcher =
    dyn Fn(DispatchedEffect) -> Result<EffectResult, CapabilityError> + Send + Sync + 'static;

#[derive(Clone)]
pub(crate) struct InvocationCapabilityRuntime {
    state: Arc<Mutex<InvocationCapabilityState>>,
    dispatcher: Arc<EffectDispatcher>,
}

#[derive(Default)]
struct InvocationCapabilityState {
    next_prepared_handle: u64,
    next_token: u64,
    next_effect_ordinal: u32,
    prepared: VecDeque<PreparedInvocation>,
    active: HashMap<u64, ActiveInvocation>,
    bound_handle: Option<PreparedInvocationHandle>,
    seen_grants: HashSet<GrantId>,
}

struct PreparedInvocation {
    handle: PreparedInvocationHandle,
    authorization: InvocationAuthorization,
}

struct ActiveInvocation {
    authorization: InvocationAuthorization,
}

/// Exact, one-shot binding held only across the direct wrapper function call.
pub(crate) struct InvocationBindingGuard {
    runtime: InvocationCapabilityRuntime,
    handle: PreparedInvocationHandle,
}

impl Drop for InvocationBindingGuard {
    fn drop(&mut self) {
        if let Ok(mut state) = self.runtime.state.lock()
            && state.bound_handle == Some(self.handle)
        {
            state.bound_handle = None;
        }
    }
}

impl std::fmt::Debug for InvocationCapabilityRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (prepared, active) = self
            .state
            .lock()
            .map(|state| (state.prepared.len(), state.active.len()))
            .unwrap_or_default();
        formatter
            .debug_struct("InvocationCapabilityRuntime")
            .field("prepared", &prepared)
            .field("active", &active)
            .finish()
    }
}

impl InvocationCapabilityRuntime {
    pub(crate) fn new(
        dispatcher: impl Fn(DispatchedEffect) -> Result<EffectResult, CapabilityError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(InvocationCapabilityState::default())),
            dispatcher: Arc::new(dispatcher),
        }
    }

    pub(crate) fn deny_all() -> Self {
        Self::new(|_| Err(CapabilityError::DispatchDenied))
    }

    pub(crate) fn prepare(
        &self,
        authorization: InvocationAuthorization,
    ) -> Result<PreparedInvocationHandle, CapabilityError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| CapabilityError::InvalidInvocation)?;
        if state
            .prepared
            .iter()
            .any(|candidate| candidate.authorization.invocation_id == authorization.invocation_id)
            || state.active.values().any(|candidate| {
                candidate.authorization.invocation_id == authorization.invocation_id
            })
            || authorization
                .grants
                .values()
                .any(|grant_id| state.seen_grants.contains(grant_id))
        {
            return Err(CapabilityError::InvalidInvocation);
        }
        let next_prepared_handle = state
            .next_prepared_handle
            .checked_add(1)
            .ok_or(CapabilityError::InvalidInvocation)?;
        state
            .seen_grants
            .extend(authorization.grants.values().cloned());
        state.next_prepared_handle = next_prepared_handle;
        let handle = PreparedInvocationHandle(next_prepared_handle);
        state.prepared.push_back(PreparedInvocation {
            handle,
            authorization,
        });
        Ok(handle)
    }

    /// Activate exactly the parent-prepared handle; public artifact metadata is validation only.
    pub(crate) fn begin(
        &self,
        handle: PreparedInvocationHandle,
        skill_id: &str,
        export_name: &str,
        manifest: &CapabilityManifest,
    ) -> Result<u64, CapabilityError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| CapabilityError::InvalidInvocation)?;
        let position = state
            .prepared
            .iter()
            .position(|candidate| candidate.handle == handle)
            .ok_or(CapabilityError::InvalidInvocation)?;
        Self::activate(&mut state, position, skill_id, export_name, manifest)
    }

    /// Bind one exact prepared handle for the immediately following direct wrapper call.
    pub(crate) fn bind(
        &self,
        handle: PreparedInvocationHandle,
    ) -> Result<InvocationBindingGuard, CapabilityError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| CapabilityError::InvalidInvocation)?;
        if state.bound_handle.is_some()
            || !state
                .prepared
                .iter()
                .any(|candidate| candidate.handle == handle)
        {
            return Err(CapabilityError::InvalidInvocation);
        }
        state.bound_handle = Some(handle);
        Ok(InvocationBindingGuard {
            runtime: self.clone(),
            handle,
        })
    }

    /// Wrapper entry consumes only the explicitly bound opaque handle. Artifact metadata is
    /// validation, never an authority selector.
    pub(crate) fn claim_bound(
        &self,
        skill_id: &str,
        export_name: &str,
        manifest: &CapabilityManifest,
    ) -> Result<u64, CapabilityError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| CapabilityError::InvalidInvocation)?;
        let handle = state
            .bound_handle
            .take()
            .ok_or(CapabilityError::InvalidInvocation)?;
        let position = state
            .prepared
            .iter()
            .position(|candidate| candidate.handle == handle)
            .ok_or(CapabilityError::InvalidInvocation)?;
        Self::activate(&mut state, position, skill_id, export_name, manifest)
    }

    fn activate(
        state: &mut InvocationCapabilityState,
        position: usize,
        skill_id: &str,
        export_name: &str,
        manifest: &CapabilityManifest,
    ) -> Result<u64, CapabilityError> {
        let prepared = state
            .prepared
            .get(position)
            .ok_or(CapabilityError::InvalidInvocation)?;
        if prepared.authorization.attribution.skill_id != skill_id
            || prepared.authorization.attribution.export_name != export_name
            || prepared.authorization.attribution.manifest != *manifest
        {
            return Err(CapabilityError::InvalidInvocation);
        }
        let authorization = state
            .prepared
            .remove(position)
            .ok_or(CapabilityError::InvalidInvocation)?
            .authorization;
        state.next_token = state
            .next_token
            .checked_add(1)
            .ok_or(CapabilityError::InvalidInvocation)?;
        let token = state.next_token;
        state
            .active
            .insert(token, ActiveInvocation { authorization });
        Ok(token)
    }

    pub(crate) fn dispatch(
        &self,
        token: u64,
        operation: HostCapability,
        encoded_arguments: &str,
    ) -> Result<String, CapabilityError> {
        let effect = {
            let mut state = self.state.lock().map_err(|_| CapabilityError::Revoked)?;
            let (grant_id, invocation_id, artifact_id, export) = {
                let active = state.active.get(&token).ok_or(CapabilityError::Revoked)?;
                (
                    active
                        .authorization
                        .grants
                        .get(&operation)
                        .cloned()
                        .ok_or(CapabilityError::Revoked)?,
                    active.authorization.invocation_id.clone(),
                    active.authorization.attribution.skill_id.clone(),
                    active.authorization.attribution.export_name.clone(),
                )
            };
            let decoded_operation = decode_operation(operation, encoded_arguments)?;
            if state.next_effect_ordinal >= MAX_EFFECTS_PER_STEP {
                return Err(CapabilityError::DispatchDenied);
            }
            let effect_ordinal = state.next_effect_ordinal;
            state.next_effect_ordinal += 1;
            DispatchedEffect {
                invocation_id,
                request: EffectRequest {
                    effect_ordinal,
                    grant_id,
                    advisory: AdvisoryAttribution {
                        artifact_id: Some(artifact_id),
                        export: Some(export),
                    },
                    operation: decoded_operation,
                },
            }
        };
        encode_effect_result((self.dispatcher)(effect)?)
    }

    pub(crate) fn finish(&self, token: u64) {
        if let Ok(mut state) = self.state.lock() {
            state.active.remove(&token);
        }
    }

    pub(crate) fn cancel(&self, invocation_id: &InvocationId) {
        if let Ok(mut state) = self.state.lock() {
            if state.bound_handle.is_some_and(|handle| {
                state.prepared.iter().any(|candidate| {
                    candidate.handle == handle
                        && &candidate.authorization.invocation_id == invocation_id
                })
            }) {
                state.bound_handle = None;
            }
            state
                .prepared
                .retain(|candidate| &candidate.authorization.invocation_id != invocation_id);
            state
                .active
                .retain(|_, candidate| &candidate.authorization.invocation_id != invocation_id);
        }
    }

    pub(crate) fn recycle(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.prepared.clear();
            state.active.clear();
            state.bound_handle = None;
            state.next_effect_ordinal = 0;
            state.seen_grants.clear();
        }
    }

    #[cfg(test)]
    pub(crate) fn active_count(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.active.len())
            .unwrap_or_default()
    }
}

fn decode_operation(
    operation: HostCapability,
    encoded_arguments: &str,
) -> Result<EffectOperation, CapabilityError> {
    let arguments: Vec<serde_json::Value> =
        serde_json::from_str(encoded_arguments).map_err(|_| CapabilityError::InvalidArguments)?;
    let string = |index: usize| {
        arguments
            .get(index)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or(CapabilityError::InvalidArguments)
    };
    match operation {
        HostCapability::ReadFile if arguments.len() == 1 => {
            Ok(EffectOperation::ReadFile { path: string(0)? })
        }
        HostCapability::WriteFile if arguments.len() == 2 => Ok(EffectOperation::WriteFile {
            path: string(0)?,
            content: string(1)?,
        }),
        HostCapability::Spawn if arguments.len() == 2 => {
            let values = arguments
                .get(1)
                .and_then(serde_json::Value::as_array)
                .ok_or(CapabilityError::InvalidArguments)?;
            let command_arguments = values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .ok_or(CapabilityError::InvalidArguments)
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(EffectOperation::Spawn {
                program: string(0)?,
                arguments: command_arguments,
            })
        }
        HostCapability::Fetch if (1..=2).contains(&arguments.len()) => {
            let mut method = HttpMethod::Get;
            let mut headers = Vec::new();
            let mut body = None;
            if let Some(options) = arguments.get(1) {
                let options = options
                    .as_object()
                    .ok_or(CapabilityError::InvalidArguments)?;
                if options
                    .keys()
                    .any(|key| !matches!(key.as_str(), "method" | "headers" | "body"))
                {
                    return Err(CapabilityError::InvalidArguments);
                }
                if let Some(value) = options.get("method") {
                    method = match value.as_str() {
                        Some("GET") => HttpMethod::Get,
                        Some("POST") => HttpMethod::Post,
                        _ => return Err(CapabilityError::InvalidArguments),
                    };
                }
                if let Some(value) = options.get("headers") {
                    headers = value
                        .as_object()
                        .ok_or(CapabilityError::InvalidArguments)?
                        .iter()
                        .map(|(name, value)| {
                            Ok(HttpHeader {
                                name: name.clone(),
                                value: value
                                    .as_str()
                                    .map(str::to_owned)
                                    .ok_or(CapabilityError::InvalidArguments)?,
                            })
                        })
                        .collect::<Result<Vec<_>, CapabilityError>>()?;
                }
                if let Some(value) = options.get("body") {
                    body = Some(
                        value
                            .as_str()
                            .map(str::to_owned)
                            .ok_or(CapabilityError::InvalidArguments)?,
                    );
                }
            }
            Ok(EffectOperation::Fetch {
                url: string(0)?,
                method,
                headers,
                body,
            })
        }
        _ => Err(CapabilityError::InvalidArguments),
    }
}

fn encode_effect_result(result: EffectResult) -> Result<String, CapabilityError> {
    let value = match result {
        EffectResult::ReadFile { content } => serde_json::Value::String(content),
        EffectResult::WriteFile => serde_json::Value::Null,
        EffectResult::Fetch {
            status,
            headers,
            body,
            truncated,
        } => serde_json::json!({
            "status": status,
            "headers": headers.into_iter().map(|HttpHeader { name, value }| (name, value)).collect::<BTreeMap<_, _>>(),
            "body": body,
            "truncated": truncated,
        }),
        EffectResult::Spawn {
            stdout,
            stderr,
            exit_code,
            timed_out,
            stdout_truncated,
            stderr_truncated,
        } => serde_json::json!({
            "stdout": stdout,
            "stderr": stderr,
            "code": exit_code,
            "timed_out": timed_out,
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
        }),
        EffectResult::ProposalAccepted => return Err(CapabilityError::DispatchDenied),
        EffectResult::Error(error) => {
            let _closed_code = match error.code {
                EffectErrorCode::Denied
                | EffectErrorCode::InvalidTarget
                | EffectErrorCode::Cancelled
                | EffectErrorCode::TimedOut
                | EffectErrorCode::OutputLimit
                | EffectErrorCode::BackendFailure
                | EffectErrorCode::AuditFailure
                | EffectErrorCode::OutcomeUnknown => error.code,
            };
            return Err(CapabilityError::DispatchDenied);
        }
    };
    serde_json::to_string(&value).map_err(|_| CapabilityError::DispatchDenied)
}

/// Cloneable execution context shared by the JS wrappers and host globals for
/// one dedicated JS thread. Nested scopes intersect manifests.
#[derive(Clone, Default)]
pub struct CapabilityContext {
    stack: Arc<Mutex<Vec<SkillExecutionAttribution>>>,
    denials: Arc<Mutex<Vec<CapabilityDenied>>>,
}

impl std::fmt::Debug for CapabilityContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let depth = self.stack.lock().map(|stack| stack.len()).unwrap_or(0);
        formatter
            .debug_struct("CapabilityContext")
            .field("depth", &depth)
            .finish()
    }
}

impl CapabilityContext {
    pub fn enter(
        &self,
        attribution: SkillExecutionAttribution,
    ) -> Result<CapabilityGuard, CapabilityError> {
        self.push(attribution)?;
        Ok(CapabilityGuard {
            context: self.clone(),
            active: true,
        })
    }

    pub(crate) fn push(
        &self,
        attribution: SkillExecutionAttribution,
    ) -> Result<(), CapabilityError> {
        validate_attribution(&attribution)?;
        self.stack
            .lock()
            .map_err(|_| CapabilityError::InvalidAttribution)?
            .push(attribution);
        Ok(())
    }

    pub(crate) fn pop(&self) -> Result<(), CapabilityError> {
        self.stack
            .lock()
            .map_err(|_| CapabilityError::InvalidAttribution)?
            .pop()
            .map(|_| ())
            .ok_or(CapabilityError::InvalidAttribution)
    }

    /// Model-authored code outside a skill wrapper has no skill constraint and
    /// is governed solely by normal session permissions.
    pub fn authorize(
        &self,
        operation: HostCapability,
        session_allowed: bool,
    ) -> Result<(), CapabilityError> {
        let stack = self
            .stack
            .lock()
            .map_err(|_| CapabilityError::InvalidAttribution)?;
        let current = stack.last();
        let Some(current) = current else {
            return if session_allowed {
                Ok(())
            } else {
                Err(CapabilityError::Denied(CapabilityDenied {
                    skill_id: String::new(),
                    export_name: String::new(),
                    operation,
                    reason: CapabilityDenialReason::SessionDenied,
                }))
            };
        };
        if !session_allowed {
            return self.deny(CapabilityDenied {
                skill_id: current.skill_id.clone(),
                export_name: current.export_name.clone(),
                operation,
                reason: CapabilityDenialReason::SessionDenied,
            });
        }
        // Every nested frame must allow the operation; callees cannot borrow
        // authority from either callers or the ambient session.
        if stack
            .iter()
            .all(|attribution| attribution.manifest.allows(operation))
        {
            Ok(())
        } else {
            let denied_attribution = stack
                .iter()
                .rev()
                .find(|attribution| !attribution.manifest.allows(operation))
                .unwrap_or(current);
            self.deny(CapabilityDenied {
                skill_id: denied_attribution.skill_id.clone(),
                export_name: denied_attribution.export_name.clone(),
                operation,
                reason: CapabilityDenialReason::Undeclared,
            })
        }
    }

    fn deny(&self, denial: CapabilityDenied) -> Result<(), CapabilityError> {
        if let Ok(mut denials) = self.denials.lock() {
            // A single JS step has a bounded event response. Retain the first
            // direct policy faults and drop repeated loop noise.
            if denials.len() < 256 {
                denials.push(denial.clone());
            }
        }
        Err(CapabilityError::Denied(denial))
    }

    pub fn take_denials(&self) -> Vec<CapabilityDenied> {
        self.denials
            .lock()
            .map(|mut denials| std::mem::take(&mut *denials))
            .unwrap_or_default()
    }

    pub fn current(&self) -> Option<SkillExecutionAttribution> {
        self.stack.lock().ok()?.last().cloned()
    }

    pub fn clear(&self) {
        if let Ok(mut stack) = self.stack.lock() {
            stack.clear();
        }
    }
}

fn validate_attribution(attribution: &SkillExecutionAttribution) -> Result<(), CapabilityError> {
    if attribution.skill_id.len() != 64
        || !attribution
            .skill_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || attribution.export_name.trim().is_empty()
    {
        return Err(CapabilityError::InvalidAttribution);
    }
    attribution.manifest.validate()?;
    Ok(())
}

pub struct CapabilityGuard {
    context: CapabilityContext,
    active: bool,
}

impl Drop for CapabilityGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = self.context.pop();
            self.active = false;
        }
    }
}

pub fn tier_may_automate(tier: CapabilityTier) -> bool {
    matches!(tier, CapabilityTier::Pure | CapabilityTier::ReadOnly)
}
