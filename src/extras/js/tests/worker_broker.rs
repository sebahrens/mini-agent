use std::collections::BTreeSet;
use std::future::Future;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::pin::Pin;
#[cfg(feature = "skills")]
use std::sync::atomic::AtomicUsize;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering as AtomicOrdering},
};
use std::time::{Duration, Instant};

use crate::extras::js::audit::{
    AuditCapability, AuditDecision, AuditError, AuditFailurePoint, AuditOpenOptions,
    AuditResultCode, AuditState, EffectAudit, EffectCompletion, EffectIntent, SanitizedTarget,
};
use crate::extras::js::broker::{
    AuthorizedEffect, AuthorizedTarget, EffectOperation, EffectResult, ExecutableCopyError,
    ExecutablePreparationWaitError, GrantPrincipal, HostCapability, HostEffectError,
    InvocationBroker, InvocationGrant, MAX_SPAWN_EXECUTABLE_BYTES, NormalizedTarget,
    ParentEffectService, copy_and_hash_executable, copy_and_hash_executable_controlled,
    resolve_program_identity, run_executable_preparation,
};
#[cfg(feature = "skills")]
use crate::extras::js::broker::{SkillCallAuthority, SkillExportAuthoritySpec};
#[cfg(feature = "skills")]
use crate::extras::js::protocol::SkillCallRequest;
use crate::extras::js::protocol::{
    AdvisoryAttribution, EffectErrorCode, EffectRequest, HttpHeader, HttpMethod, InvocationId,
    RunStep, SkillProposalDraft, StepOutcome,
};
#[cfg(feature = "skills")]
use crate::extras::js::skills::{
    CapabilityManifest, CapabilityScope, CapabilityTier, HttpMethod as SkillHttpMethod,
};
use crate::extras::js::supervisor::{InvocationEffectHandler, JsWorkerSupervisor, WorkerError};
use crate::extras::js::types::PermCancellation;
use crate::paths::{AppPaths, EffectAuditPathOwner};
use crate::sandbox::worker::TestWorkerLauncher;

type ServiceFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

struct NoEffects;

impl InvocationEffectHandler for NoEffects {
    fn handle_effect(
        &mut self,
        _request: EffectRequest,
        _cancellation: PermCancellation,
    ) -> crate::extras::js::supervisor::EffectFuture<'_> {
        Box::pin(async {
            EffectResult::Error(crate::extras::js::protocol::EffectError {
                code: EffectErrorCode::Denied,
            })
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ServiceFailures {
    target: Option<HostEffectError>,
    backend: Option<HostEffectError>,
    permission: Option<HostEffectError>,
    execution: Option<HostEffectError>,
}

#[derive(Clone, Debug, Default)]
struct ServiceRecord {
    discards: usize,
    authorizations: usize,
    execute_calls: usize,
    executions: usize,
    authorized: Vec<AuthorizedEffect>,
}

struct RecordingService {
    failures: ServiceFailures,
    normalized_target: Option<Result<NormalizedTarget, HostEffectError>>,
    pending_permission: bool,
    permission_delay_until: Option<Instant>,
    record: Arc<Mutex<ServiceRecord>>,
}

impl ParentEffectService for RecordingService {
    fn discard_prepared(&mut self) {
        self.record.lock().unwrap().discards += 1;
    }

    fn validate_target(
        &mut self,
        _authorized: &AuthorizedEffect,
        _operation: &EffectOperation,
    ) -> Result<(), HostEffectError> {
        self.failures.target.map_or(Ok(()), Err)
    }

    fn ensure_backend(
        &mut self,
        _authorized: &AuthorizedEffect,
        _operation: &EffectOperation,
    ) -> Result<(), HostEffectError> {
        self.failures.backend.map_or(Ok(()), Err)
    }

    fn normalize_target<'a>(
        &'a mut self,
        _authorized: &'a AuthorizedEffect,
        operation: &'a EffectOperation,
        _cancellation: PermCancellation,
    ) -> ServiceFuture<'a, Result<NormalizedTarget, HostEffectError>> {
        let target = self
            .normalized_target
            .clone()
            .unwrap_or_else(|| normalized_target(operation));
        Box::pin(async move { target })
    }

    fn authorize<'a>(
        &'a mut self,
        _authorized: &'a AuthorizedEffect,
        _operation: &'a EffectOperation,
        _cancellation: PermCancellation,
    ) -> ServiceFuture<'a, Result<AuthorizedTarget, HostEffectError>> {
        let result = self.failures.permission.map_or(Ok(()), Err);
        let target = authorized_target(_operation);
        self.record.lock().unwrap().authorizations += 1;
        if self.pending_permission {
            Box::pin(std::future::pending())
        } else if let Some(delay_until) = self.permission_delay_until {
            Box::pin(async move {
                tokio::time::sleep_until(tokio::time::Instant::from_std(delay_until)).await;
                result.map(|()| target)
            })
        } else {
            Box::pin(async move { result.map(|()| target) })
        }
    }

    fn execute<'a>(
        &'a mut self,
        authorized: &'a AuthorizedEffect,
        operation: &'a EffectOperation,
        _cancellation: PermCancellation,
    ) -> ServiceFuture<'a, Result<EffectResult, HostEffectError>> {
        let failure = self.failures.execution;
        let result = success_for(operation);
        let authorized = authorized.clone();
        let record = Arc::clone(&self.record);
        Box::pin(async move {
            let mut record = record.lock().unwrap();
            record.execute_calls += 1;
            if let Some(error) = failure {
                return Err(error);
            }
            record.executions += 1;
            record.authorized.push(authorized);
            Ok(result)
        })
    }
}

fn normalized_target(operation: &EffectOperation) -> Result<NormalizedTarget, HostEffectError> {
    match operation {
        EffectOperation::ReadFile { path } => Ok(NormalizedTarget::ReadFile {
            workspace_relative: Some(path.clone()),
        }),
        EffectOperation::WriteFile { path, .. } => Ok(NormalizedTarget::WriteFile {
            workspace_relative: Some(path.clone()),
        }),
        EffectOperation::Fetch { url, method, .. } => {
            let url = reqwest::Url::parse(url).map_err(|_| HostEffectError::InvalidTarget)?;
            Ok(NormalizedTarget::Fetch {
                origin: url.origin().ascii_serialization(),
                method: match method {
                    HttpMethod::Get => "GET",
                    HttpMethod::Post => "POST",
                }
                .into(),
            })
        }
        EffectOperation::Spawn { program, .. } => Ok(NormalizedTarget::Spawn {
            program: program.clone(),
            resolved_executable: resolve_program_identity(program)?,
        }),
        EffectOperation::ProposeSkill { .. } => Ok(NormalizedTarget::ProposeSkill),
    }
}

fn authorized_target(operation: &EffectOperation) -> AuthorizedTarget {
    match operation {
        EffectOperation::ReadFile { path } => AuthorizedTarget::ReadFile {
            canonical_path: path.clone(),
        },
        EffectOperation::WriteFile { path, .. } => AuthorizedTarget::WriteFile {
            canonical_path: path.clone(),
        },
        EffectOperation::Fetch { url, method, .. } => AuthorizedTarget::Fetch {
            normalized_url: url.clone(),
            method: match method {
                HttpMethod::Get => "GET",
                HttpMethod::Post => "POST",
            }
            .to_string(),
        },
        EffectOperation::Spawn { program, .. } => AuthorizedTarget::Spawn {
            resolved_executable: program.clone(),
        },
        EffectOperation::ProposeSkill { .. } => AuthorizedTarget::ProposeSkill,
    }
}

#[derive(Clone)]
struct OperationCase {
    name: &'static str,
    operation: EffectOperation,
    capability: HostCapability,
    principal: GrantPrincipal,
    advisory: AdvisoryAttribution,
}

fn invocation(value: &str) -> InvocationId {
    InvocationId::new(value).unwrap()
}

fn operation_cases(invocation_id: &InvocationId) -> Vec<OperationCase> {
    let skill = |suffix: &str| GrantPrincipal::Skill {
        artifact_id: format!("artifact-{suffix}"),
        export: format!("export-{suffix}"),
        invocation_id: invocation_id.to_string(),
    };
    let advisory = |suffix: &str| AdvisoryAttribution {
        artifact_id: Some(format!("artifact-{suffix}")),
        export: Some(format!("export-{suffix}")),
    };

    vec![
        OperationCase {
            name: "read_file",
            operation: EffectOperation::ReadFile {
                path: "docs/input.txt".into(),
            },
            capability: HostCapability::ReadFile,
            principal: skill("read"),
            advisory: advisory("read"),
        },
        OperationCase {
            name: "write_file",
            operation: EffectOperation::WriteFile {
                path: "tmp/output.txt".into(),
                content: "output".into(),
            },
            capability: HostCapability::WriteFile,
            principal: skill("write"),
            advisory: advisory("write"),
        },
        OperationCase {
            name: "fetch",
            operation: EffectOperation::Fetch {
                url: "https://example.test/api".into(),
                method: HttpMethod::Get,
                headers: vec![],
                body: None,
            },
            capability: HostCapability::Fetch,
            principal: skill("fetch"),
            advisory: advisory("fetch"),
        },
        OperationCase {
            name: "spawn",
            operation: EffectOperation::Spawn {
                program: "printf".into(),
                arguments: vec!["%s".into(), "hello".into()],
            },
            capability: HostCapability::Spawn,
            principal: skill("spawn"),
            advisory: advisory("spawn"),
        },
        OperationCase {
            name: "propose_skill",
            operation: EffectOperation::ProposeSkill {
                draft: SkillProposalDraft {
                    source: "function run() { return true; }".into(),
                    description: "test proposal".into(),
                    exports: vec![crate::extras::js::protocol::SkillProposalExport {
                        name: "run".into(),
                        signature: "run(): boolean".into(),
                    }],
                    tests: vec!["run() === true".into()],
                    capability: crate::extras::js::protocol::SkillProposalCapability {
                        tier: "pure".into(),
                        grants: Vec::new(),
                    },
                    tags: Vec::new(),
                    predecessor_id: None,
                },
            },
            capability: HostCapability::ProposeSkill,
            principal: GrantPrincipal::ModelAuthored {
                tool_call_id: "tool-call-1".into(),
            },
            advisory: AdvisoryAttribution::default(),
        },
    ]
}

fn success_for(operation: &EffectOperation) -> EffectResult {
    match operation {
        EffectOperation::ReadFile { .. } => EffectResult::ReadFile {
            content: "contents".into(),
        },
        EffectOperation::WriteFile { .. } => EffectResult::WriteFile,
        EffectOperation::Fetch { .. } => EffectResult::Fetch {
            status: 200,
            headers: vec![HttpHeader {
                name: "content-type".into(),
                value: "text/plain".into(),
            }],
            body: "ok".into(),
            truncated: false,
        },
        EffectOperation::Spawn { .. } => EffectResult::Spawn {
            stdout: "hello".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
            stdout_truncated: false,
            stderr_truncated: false,
        },
        EffectOperation::ProposeSkill { .. } => EffectResult::ProposalAccepted {
            skill_id: "a".repeat(64),
            proposal_id: "proposal-test".into(),
            status: crate::extras::js::protocol::ProposalStatus::Pending,
            report_id: None,
        },
    }
}

fn grant(
    case: &OperationCase,
    invocation_id: &InvocationId,
    expires_at: Instant,
) -> InvocationGrant {
    #[cfg(feature = "skills")]
    if case.capability != HostCapability::ProposeSkill
        && matches!(case.principal, GrantPrincipal::Skill { .. })
    {
        return scoped_skill_grant(case, invocation_id, manifest_for(case, true), expires_at);
    }
    InvocationGrant::issue(
        invocation_id.clone(),
        case.principal.clone(),
        BTreeSet::from([case.capability]),
        expires_at,
    )
}

#[cfg(feature = "skills")]
fn scoped_skill_grant(
    case: &OperationCase,
    invocation_id: &InvocationId,
    manifest: CapabilityManifest,
    expires_at: Instant,
) -> InvocationGrant {
    InvocationGrant::issue_scoped_skill_with_resolver(
        invocation_id.clone(),
        case.principal.clone(),
        manifest,
        expires_at,
        resolve_program_identity,
    )
    .unwrap()
}

#[cfg(feature = "skills")]
fn manifest_for(case: &OperationCase, allow_target: bool) -> CapabilityManifest {
    let grant = match case.capability {
        HostCapability::ReadFile => CapabilityScope::ReadFile {
            workspace_prefixes: vec![if allow_target { "docs" } else { "docs-private" }.into()],
        },
        HostCapability::WriteFile => CapabilityScope::WriteFile {
            workspace_prefixes: vec![if allow_target { "tmp" } else { "tmp-private" }.into()],
        },
        HostCapability::Fetch => CapabilityScope::Fetch {
            origins: vec![
                if allow_target {
                    "https://example.test"
                } else {
                    "https://other.test"
                }
                .into(),
            ],
            methods: vec![SkillHttpMethod::Get],
        },
        HostCapability::Spawn => CapabilityScope::Spawn {
            programs: vec![if allow_target { "printf" } else { "echo" }.into()],
        },
        HostCapability::ProposeSkill => unreachable!("skill manifests cannot propose skills"),
    };
    CapabilityManifest::new(CapabilityTier::SideEffecting, vec![grant]).unwrap()
}

fn request(case: &OperationCase, grant: &InvocationGrant) -> EffectRequest {
    EffectRequest {
        effect_ordinal: 1,
        grant_id: grant.grant_id().clone(),
        advisory: case.advisory.clone(),
        operation: case.operation.clone(),
    }
}

fn broker(
    invocation_id: InvocationId,
    grants: Vec<InvocationGrant>,
    session_allowed: BTreeSet<HostCapability>,
    failures: ServiceFailures,
) -> (
    InvocationBroker<RecordingService>,
    Arc<Mutex<ServiceRecord>>,
    AuditTempRoot,
) {
    broker_with_normalized_target(invocation_id, grants, session_allowed, failures, None)
}

fn broker_with_normalized_target(
    invocation_id: InvocationId,
    grants: Vec<InvocationGrant>,
    session_allowed: BTreeSet<HostCapability>,
    failures: ServiceFailures,
    normalized_target: Option<Result<NormalizedTarget, HostEffectError>>,
) -> (
    InvocationBroker<RecordingService>,
    Arc<Mutex<ServiceRecord>>,
    AuditTempRoot,
) {
    let record = Arc::new(Mutex::new(ServiceRecord::default()));
    let service = RecordingService {
        failures,
        normalized_target,
        pending_permission: false,
        permission_delay_until: None,
        record: Arc::clone(&record),
    };
    let root = AuditTempRoot::new("broker");
    let audit = EffectAudit::open(root.owner()).unwrap();
    (
        InvocationBroker::new(
            invocation_id,
            grants,
            session_allowed,
            service,
            Arc::new(Mutex::new(audit)),
        )
        .unwrap(),
        record,
        root,
    )
}

// Keeping each denial input explicit makes this cross-product test legible at its call sites.
#[allow(clippy::too_many_arguments)]
async fn assert_denied_before_execute(
    case: &OperationCase,
    expected: HostEffectError,
    grant: InvocationGrant,
    broker_invocation: InvocationId,
    session_allowed: BTreeSet<HostCapability>,
    failures: ServiceFailures,
    cancellation: PermCancellation,
    mutate_request: impl FnOnce(&mut EffectRequest),
) {
    let mut effect = request(case, &grant);
    mutate_request(&mut effect);
    let (mut broker, record, _audit_root) =
        broker(broker_invocation, vec![grant], session_allowed, failures);

    assert_eq!(
        broker.dispatch(effect, cancellation).await,
        Err(expected),
        "{} used the wrong denial",
        case.name
    );
    assert_eq!(
        record.lock().unwrap().executions,
        0,
        "{} reached the effect service",
        case.name
    );
}

#[cfg(feature = "skills")]
#[tokio::test]
async fn scoped_capability_intersection_enforces_manifest_before_session_permission_audit_and_effect()
 {
    let invocation_id = invocation("inv-scoped-cross-product");
    for case in operation_cases(&invocation_id)
        .into_iter()
        .filter(|case| case.capability != HostCapability::ProposeSkill)
    {
        let allowed = scoped_skill_grant(
            &case,
            &invocation_id,
            manifest_for(&case, true),
            Instant::now() + Duration::from_secs(10),
        );
        let (mut allowed_broker, allowed_record, _audit_root) = broker(
            invocation_id.clone(),
            vec![allowed.clone()],
            BTreeSet::from([case.capability]),
            ServiceFailures::default(),
        );
        assert_eq!(
            allowed_broker
                .dispatch(request(&case, &allowed), PermCancellation::new())
                .await,
            Ok(success_for(&case.operation)),
            "{} did not pass the full intersection",
            case.name
        );
        assert_eq!(allowed_record.lock().unwrap().authorizations, 1);
        assert_eq!(allowed_record.lock().unwrap().executions, 1);
        assert_eq!(allowed_broker.audit_records_for_test().len(), 2);

        let manifest_denied = scoped_skill_grant(
            &case,
            &invocation_id,
            manifest_for(&case, false),
            Instant::now() + Duration::from_secs(10),
        );
        let (mut denied_broker, denied_record, _audit_root) = broker(
            invocation_id.clone(),
            vec![manifest_denied.clone()],
            BTreeSet::from([case.capability]),
            ServiceFailures::default(),
        );
        assert_eq!(
            denied_broker
                .dispatch(request(&case, &manifest_denied), PermCancellation::new())
                .await,
            Err(HostEffectError::ManifestDenied),
            "{} did not identify the manifest layer",
            case.name
        );
        assert_eq!(denied_record.lock().unwrap().authorizations, 0);
        assert_eq!(denied_record.lock().unwrap().executions, 0);
        assert!(denied_broker.audit_records_for_test().is_empty());

        let session_denied = scoped_skill_grant(
            &case,
            &invocation_id,
            manifest_for(&case, true),
            Instant::now() + Duration::from_secs(10),
        );
        let (mut denied_broker, denied_record, _audit_root) = broker(
            invocation_id.clone(),
            vec![session_denied.clone()],
            BTreeSet::new(),
            ServiceFailures::default(),
        );
        assert_eq!(
            denied_broker
                .dispatch(request(&case, &session_denied), PermCancellation::new())
                .await,
            Err(HostEffectError::SessionDenied),
            "{} did not identify the session layer",
            case.name
        );
        assert_eq!(denied_record.lock().unwrap().authorizations, 0);
        assert_eq!(denied_record.lock().unwrap().executions, 0);
        assert!(denied_broker.audit_records_for_test().is_empty());
    }
}

#[cfg(feature = "skills")]
#[tokio::test]
async fn scoped_capability_intersection_keeps_model_authored_grants_session_only() {
    let invocation_id = invocation("inv-model-session-only");
    let case = operation_cases(&invocation_id)
        .into_iter()
        .find(|case| case.capability == HostCapability::ProposeSkill)
        .unwrap();
    let grant = grant(
        &case,
        &invocation_id,
        Instant::now() + Duration::from_secs(10),
    );
    let (mut broker, record, _audit_root) = broker(
        invocation_id,
        vec![grant.clone()],
        BTreeSet::from([HostCapability::ProposeSkill]),
        ServiceFailures {
            backend: Some(HostEffectError::BackendFailure),
            ..ServiceFailures::default()
        },
    );
    assert_eq!(
        broker
            .dispatch(request(&case, &grant), PermCancellation::new())
            .await,
        Err(HostEffectError::BackendFailure)
    );
    assert_eq!(record.lock().unwrap().authorizations, 0);
    assert!(broker.audit_records_for_test().is_empty());
}

#[cfg(feature = "skills")]
#[tokio::test]
async fn scoped_capability_intersection_normalizes_fetch_method_and_spawn_identity() {
    let invocation_id = invocation("inv-scoped-normalized-targets");
    let mut fetch_case = operation_cases(&invocation_id)
        .into_iter()
        .find(|case| case.capability == HostCapability::Fetch)
        .unwrap();
    fetch_case.operation = EffectOperation::Fetch {
        url: "https://example.test:443/api".into(),
        method: HttpMethod::Get,
        headers: vec![],
        body: None,
    };
    let grant = scoped_skill_grant(
        &fetch_case,
        &invocation_id,
        manifest_for(&fetch_case, true),
        Instant::now() + Duration::from_secs(10),
    );
    let (mut default_port_broker, record, _audit_root) = broker(
        invocation_id.clone(),
        vec![grant.clone()],
        BTreeSet::from([HostCapability::Fetch]),
        ServiceFailures::default(),
    );
    assert!(
        default_port_broker
            .dispatch(request(&fetch_case, &grant), PermCancellation::new())
            .await
            .is_ok()
    );
    assert_eq!(record.lock().unwrap().executions, 1);

    fetch_case.operation = EffectOperation::Fetch {
        url: "https://example.test/api".into(),
        method: HttpMethod::Post,
        headers: vec![],
        body: Some("body".into()),
    };
    let grant = scoped_skill_grant(
        &fetch_case,
        &invocation_id,
        CapabilityManifest::new(
            CapabilityTier::SideEffecting,
            vec![CapabilityScope::Fetch {
                origins: vec!["https://example.test".into()],
                methods: vec![SkillHttpMethod::Get],
            }],
        )
        .unwrap(),
        Instant::now() + Duration::from_secs(10),
    );
    let (mut method_broker, record, _audit_root) = broker(
        invocation_id.clone(),
        vec![grant.clone()],
        BTreeSet::from([HostCapability::Fetch]),
        ServiceFailures::default(),
    );
    assert_eq!(
        method_broker
            .dispatch(request(&fetch_case, &grant), PermCancellation::new())
            .await,
        Err(HostEffectError::ManifestDenied)
    );
    assert_eq!(record.lock().unwrap().authorizations, 0);
    assert!(method_broker.audit_records_for_test().is_empty());

    let spawn_case = operation_cases(&invocation_id)
        .into_iter()
        .find(|case| case.capability == HostCapability::Spawn)
        .unwrap();
    let grant = scoped_skill_grant(
        &spawn_case,
        &invocation_id,
        manifest_for(&spawn_case, true),
        Instant::now() + Duration::from_secs(10),
    );
    let (mut identity_broker, record, _audit_root) = broker_with_normalized_target(
        invocation_id,
        vec![grant.clone()],
        BTreeSet::from([HostCapability::Spawn]),
        ServiceFailures::default(),
        Some(Ok(NormalizedTarget::Spawn {
            program: "printf".into(),
            resolved_executable: resolve_program_identity("true").unwrap(),
        })),
    );
    assert_eq!(
        identity_broker
            .dispatch(request(&spawn_case, &grant), PermCancellation::new())
            .await,
        Err(HostEffectError::ManifestDenied)
    );
    assert_eq!(record.lock().unwrap().authorizations, 0);
    assert!(identity_broker.audit_records_for_test().is_empty());
}

#[cfg(feature = "skills")]
#[tokio::test]
async fn scoped_spawn_keeps_each_program_bound_to_its_own_executable_identity() {
    let invocation_id = invocation("inv-scoped-spawn-name-binding");
    let principal = GrantPrincipal::Skill {
        artifact_id: "artifact-spawn-binding".into(),
        export: "run".into(),
        invocation_id: invocation_id.to_string(),
    };
    let advisory = AdvisoryAttribution {
        artifact_id: Some("artifact-spawn-binding".into()),
        export: Some("run".into()),
    };
    let manifest = CapabilityManifest::new(
        CapabilityTier::SideEffecting,
        vec![CapabilityScope::Spawn {
            programs: vec!["bar".into(), "foo".into()],
        }],
    )
    .unwrap();
    let foo_identity = resolve_program_identity("printf").unwrap();
    let bar_identity = resolve_program_identity("true").unwrap();
    let grant = InvocationGrant::issue_scoped_skill_with_resolver(
        invocation_id.clone(),
        principal,
        manifest,
        Instant::now() + Duration::from_secs(10),
        |program| match program {
            "foo" => Ok(foo_identity.clone()),
            "bar" => Ok(bar_identity.clone()),
            _ => unreachable!(),
        },
    )
    .unwrap();
    let operation = EffectOperation::Spawn {
        program: "foo".into(),
        arguments: vec![],
    };
    let request = EffectRequest {
        effect_ordinal: 1,
        grant_id: grant.grant_id().clone(),
        advisory,
        operation,
    };
    let (mut broker, record, _audit_root) = broker_with_normalized_target(
        invocation_id,
        vec![grant],
        BTreeSet::from([HostCapability::Spawn]),
        ServiceFailures::default(),
        Some(Ok(NormalizedTarget::Spawn {
            program: "foo".into(),
            resolved_executable: bar_identity,
        })),
    );

    assert_eq!(
        broker.dispatch(request, PermCancellation::new()).await,
        Err(HostEffectError::ManifestDenied)
    );
    let record = record.lock().unwrap();
    assert_eq!(record.authorizations, 0);
    assert_eq!(record.execute_calls, 0);
    assert_eq!(record.discards, 1);
    assert!(broker.audit_records_for_test().is_empty());
}

#[cfg(feature = "skills")]
#[tokio::test]
async fn scoped_spawn_content_mismatch_denies_before_permission_audit_and_effect() {
    let invocation_id = invocation("inv-scoped-spawn-content-binding");
    let principal = GrantPrincipal::Skill {
        artifact_id: "artifact-spawn-content-binding".into(),
        export: "run".into(),
        invocation_id: invocation_id.to_string(),
    };
    let advisory = AdvisoryAttribution {
        artifact_id: Some("artifact-spawn-content-binding".into()),
        export: Some("run".into()),
    };
    let manifest = CapabilityManifest::new(
        CapabilityTier::SideEffecting,
        vec![CapabilityScope::Spawn {
            programs: vec!["printf".into()],
        }],
    )
    .unwrap();
    let approved_identity = resolve_program_identity("printf").unwrap();
    let changed_identity = approved_identity
        .clone()
        .with_content_sha256_for_test("00".repeat(32));
    let grant = InvocationGrant::issue_scoped_skill_with_resolver(
        invocation_id.clone(),
        principal,
        manifest,
        Instant::now() + Duration::from_secs(10),
        |_| Ok(approved_identity.clone()),
    )
    .unwrap();
    let request = EffectRequest {
        effect_ordinal: 1,
        grant_id: grant.grant_id().clone(),
        advisory,
        operation: EffectOperation::Spawn {
            program: "printf".into(),
            arguments: vec![],
        },
    };
    let (mut broker, record, _audit_root) = broker_with_normalized_target(
        invocation_id,
        vec![grant],
        BTreeSet::from([HostCapability::Spawn]),
        ServiceFailures::default(),
        Some(Ok(NormalizedTarget::Spawn {
            program: "printf".into(),
            resolved_executable: changed_identity,
        })),
    );

    assert_eq!(
        broker.dispatch(request, PermCancellation::new()).await,
        Err(HostEffectError::ManifestDenied)
    );
    let record = record.lock().unwrap();
    assert_eq!(record.authorizations, 0);
    assert_eq!(record.execute_calls, 0);
    assert_eq!(record.discards, 1);
    assert!(broker.audit_records_for_test().is_empty());
}

#[cfg(feature = "skills")]
#[test]
fn scoped_spawn_binding_map_is_deterministic_and_bounded() {
    let invocation_id = invocation("inv-scoped-spawn-map-bounds");
    let principal = GrantPrincipal::Skill {
        artifact_id: "artifact-spawn-map".into(),
        export: "run".into(),
        invocation_id: invocation_id.to_string(),
    };
    let identity = resolve_program_identity("printf").unwrap();
    let manifest = CapabilityManifest::new(
        CapabilityTier::SideEffecting,
        vec![CapabilityScope::Spawn {
            programs: vec!["bar".into(), "foo".into()],
        }],
    )
    .unwrap();
    let grant = InvocationGrant::issue_scoped_skill_with_resolver(
        invocation_id.clone(),
        principal.clone(),
        manifest,
        Instant::now() + Duration::from_secs(10),
        |_| Ok(identity.clone()),
    )
    .unwrap();
    let encoded = grant.spawn_program_bindings_json_for_test();
    let decoded: std::collections::BTreeMap<String, serde_json::Value> =
        serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded.keys().cloned().collect::<Vec<_>>(), ["bar", "foo"]);
    for binding in decoded.values() {
        let digest = binding["content_sha256"].as_str().unwrap();
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(binding["content_bytes"].as_u64().is_some());
    }

    let programs = (0..257).map(|index| format!("p{index:03}")).collect();
    let oversized = CapabilityManifest::new(
        CapabilityTier::SideEffecting,
        vec![CapabilityScope::Spawn { programs }],
    )
    .unwrap();
    assert_eq!(
        InvocationGrant::issue_scoped_skill_with_resolver(
            invocation_id,
            principal,
            oversized,
            Instant::now() + Duration::from_secs(10),
            |_| Ok(identity.clone()),
        ),
        Err(crate::extras::js::broker::BrokerBuildError::InvalidManifest)
    );
}

#[cfg(feature = "skills")]
#[test]
fn prepared_manifest_resolves_once_then_mints_distinct_singleton_grants_with_one_fresh_lease() {
    let invocation_id = invocation("inv-prepared-manifest-reuse");
    let identity = resolve_program_identity("printf").unwrap();
    let resolver_calls = AtomicUsize::new(0);
    let manifest = CapabilityManifest::new(
        CapabilityTier::SideEffecting,
        vec![
            CapabilityScope::ReadFile {
                workspace_prefixes: vec!["Cargo.toml".into()],
            },
            CapabilityScope::Spawn {
                programs: vec!["printf".into()],
            },
        ],
    )
    .unwrap();
    let preparation_started = Instant::now();
    let prepared = InvocationGrant::prepare_skill_manifest_with_resolver(manifest, |_| {
        resolver_calls.fetch_add(1, AtomicOrdering::Relaxed);
        Ok(identity.clone())
    })
    .unwrap();
    assert_eq!(resolver_calls.load(AtomicOrdering::Relaxed), 1);

    let expires_at = Instant::now() + Duration::from_secs(10);
    let mut grants = Vec::new();
    for export in ["first", "second"] {
        for capability in [HostCapability::ReadFile, HostCapability::Spawn] {
            grants.push(
                InvocationGrant::issue_prepared_scoped_skill(
                    invocation_id.clone(),
                    GrantPrincipal::Skill {
                        artifact_id: "artifact-prepared-reuse".into(),
                        export: export.into(),
                        invocation_id: format!("{export}-invocation"),
                    },
                    capability,
                    &prepared,
                    expires_at,
                )
                .unwrap(),
            );
        }
    }

    assert!(expires_at > preparation_started + Duration::from_secs(9));
    assert_eq!(resolver_calls.load(AtomicOrdering::Relaxed), 1);
    assert_eq!(
        grants
            .iter()
            .map(|grant| grant.grant_id().get())
            .collect::<BTreeSet<_>>()
            .len(),
        4
    );
    assert!(
        grants
            .iter()
            .all(|grant| grant.allowed_for_test().len() == 1)
    );
    assert!(
        grants
            .iter()
            .all(|grant| grant.expires_at_for_test() == expires_at)
    );
    assert!(grants.iter().all(|grant| {
        grant.spawn_program_bindings_json_for_test()
            == grants[0].spawn_program_bindings_json_for_test()
    }));
}

#[cfg(feature = "skills")]
#[test]
fn reusable_export_mints_fresh_exact_authority_and_denies_replay_revocation_and_expiry() {
    let outer = invocation("reusable-export-outer");
    let artifact_id = "a".repeat(64);
    let manifest = CapabilityManifest::new(
        CapabilityTier::ReadOnly,
        vec![CapabilityScope::ReadFile {
            workspace_prefixes: vec!["Cargo.toml".into()],
        }],
    )
    .unwrap();
    let prepared = InvocationGrant::prepare_skill_manifest_with_resolver(manifest, |_| {
        unreachable!("read-only manifest does not resolve executables")
    })
    .unwrap();
    let authority = SkillCallAuthority::new(
        "reusable-turn".into(),
        "reusable-tool-call".into(),
        Instant::now() + Duration::from_secs(10),
        vec![SkillExportAuthoritySpec {
            artifact_id: artifact_id.clone(),
            export_name: "read".into(),
            prepared_manifest: prepared.clone(),
        }],
    )
    .unwrap();
    let (active_broker, _record, _audit) = broker(
        outer,
        vec![],
        BTreeSet::from([HostCapability::ReadFile]),
        ServiceFailures::default(),
    );
    let mut active_broker = active_broker.with_skill_call_authority(authority);
    let request = |request_ordinal, call_ordinal| SkillCallRequest {
        request_ordinal,
        artifact_id: artifact_id.clone(),
        export_name: "read".into(),
        call_ordinal,
    };

    let first = active_broker
        .authorize_skill_call(request(0, 0))
        .authorization
        .unwrap();
    let second = active_broker
        .authorize_skill_call(request(1, 1))
        .authorization
        .unwrap();
    assert_ne!(first.invocation_id, second.invocation_id);
    assert_ne!(first.grants[0].grant_id, second.grants[0].grant_id);
    assert!(
        active_broker
            .authorize_skill_call(request(2, 0))
            .authorization
            .is_none()
    );
    assert!(active_broker.revoke_skill_export(&artifact_id, "read"));
    assert!(
        active_broker
            .authorize_skill_call(request(3, 2))
            .authorization
            .is_none()
    );

    let expired = SkillCallAuthority::new(
        "expired-turn".into(),
        "expired-tool-call".into(),
        Instant::now() - Duration::from_millis(1),
        vec![SkillExportAuthoritySpec {
            artifact_id: artifact_id.clone(),
            export_name: "read".into(),
            prepared_manifest: prepared,
        }],
    )
    .unwrap();
    let (expired_broker, _record, _audit) = broker(
        invocation("expired-export-outer"),
        vec![],
        BTreeSet::from([HostCapability::ReadFile]),
        ServiceFailures::default(),
    );
    assert!(
        expired_broker
            .with_skill_call_authority(expired)
            .authorize_skill_call(request(0, 0))
            .authorization
            .is_none()
    );
}

#[test]
fn executable_snapshot_copy_is_bounded_and_reports_destination_failure() {
    let mut oversized = std::io::repeat(0).take(MAX_SPAWN_EXECUTABLE_BYTES + 1);
    assert_eq!(
        copy_and_hash_executable(&mut oversized, &mut std::io::sink()),
        Err(ExecutableCopyError::TooLarge)
    );

    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("injected snapshot write failure"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    assert_eq!(
        copy_and_hash_executable(&mut b"approved executable".as_slice(), &mut FailingWriter),
        Err(ExecutableCopyError::Write)
    );
}

struct ControllablyBlockingExecutableSource {
    gate: Arc<(Mutex<bool>, Condvar)>,
    started: Arc<tokio::sync::Semaphore>,
    announced: bool,
}

impl Read for ControllablyBlockingExecutableSource {
    fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
        if !self.announced {
            self.announced = true;
            self.started.add_permits(1);
        }
        let (released, wake) = &*self.gate;
        let mut released = released.lock().unwrap();
        while !*released {
            released = wake.wait(released).unwrap();
        }
        Ok(0)
    }
}

struct TrackedSnapshotResource {
    dropped: Arc<AtomicBool>,
}

impl Write for TrackedSnapshotResource {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for TrackedSnapshotResource {
    fn drop(&mut self) {
        self.dropped.store(true, AtomicOrdering::Release);
    }
}

async fn assert_blocked_executable_preparation_returns_promptly(cancel: bool) {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let started = Arc::new(tokio::sync::Semaphore::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let cancellation = PermCancellation::new();
    let deadline = Instant::now()
        + if cancel {
            Duration::from_secs(5)
        } else {
            Duration::from_millis(75)
        };
    let task_gate = gate.clone();
    let task_started = started.clone();
    let task_dropped = dropped.clone();
    let task_cancellation = cancellation.clone();
    let task = tokio::spawn(async move {
        run_executable_preparation(deadline, task_cancellation, move |control| {
            let mut source = ControllablyBlockingExecutableSource {
                gate: task_gate,
                started: task_started,
                announced: false,
            };
            let mut snapshot = TrackedSnapshotResource {
                dropped: task_dropped,
            };
            copy_and_hash_executable_controlled(&mut source, &mut snapshot, &control)
        })
        .await
    });

    started.acquire().await.unwrap().forget();
    let return_started = Instant::now();
    if cancel {
        cancellation.cancel();
    }
    let result = tokio::time::timeout(Duration::from_millis(300), task)
        .await
        .expect("blocked executable preparation did not return promptly")
        .unwrap();
    assert_eq!(
        result,
        Err(if cancel {
            ExecutablePreparationWaitError::Cancelled
        } else {
            ExecutablePreparationWaitError::TimedOut
        })
    );
    assert!(
        return_started.elapsed() < Duration::from_millis(300),
        "cancellation/deadline waited for a blocked source"
    );
    assert!(
        !dropped.load(AtomicOrdering::Acquire),
        "the source should still control when its worker unwinds"
    );

    let (released, wake) = &*gate;
    *released.lock().unwrap() = true;
    wake.notify_all();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !dropped.load(AtomicOrdering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("late snapshot resource was not closed after the source unwound");
}

#[tokio::test]
async fn executable_preparation_cancellation_returns_before_blocked_source_and_closes_snapshot() {
    assert_blocked_executable_preparation_returns_promptly(true).await;
}

#[tokio::test]
async fn executable_preparation_deadline_returns_before_blocked_source_and_closes_snapshot() {
    assert_blocked_executable_preparation_returns_promptly(false).await;
}

#[cfg(unix)]
#[test]
fn executable_identity_changes_when_bytes_are_overwritten_in_place() {
    use std::os::unix::fs::PermissionsExt;

    let directory = AuditTempRoot::new("executable-content-binding");
    let executable = directory.0.join("content-bound-command");
    std::fs::write(&executable, "#!/bin/sh\nprintf original").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let original = resolve_program_identity(executable.to_string_lossy().as_ref()).unwrap();

    std::fs::write(&executable, "#!/bin/sh\nprintf replaced").unwrap();
    let replaced = resolve_program_identity(executable.to_string_lossy().as_ref()).unwrap();
    assert_ne!(original, replaced, "identity must bind executable content");
}

#[tokio::test]
async fn worker_broker_grants_all_closed_operations_from_parent_identity() {
    let invocation_id = invocation("inv-success");
    let cases = operation_cases(&invocation_id);
    let expires_at = Instant::now() + Duration::from_secs(30);
    let grants: Vec<_> = cases
        .iter()
        .map(|case| grant(case, &invocation_id, expires_at))
        .collect();
    let grant_ids: BTreeSet<_> = grants.iter().map(|grant| grant.grant_id().get()).collect();
    assert_eq!(grant_ids.len(), cases.len(), "grant IDs must be unique");
    assert!(grant_ids.iter().all(|id| !id.is_nil()));

    let (mut broker, record, _audit_root) = broker(
        invocation_id.clone(),
        grants.clone(),
        HostCapability::all(),
        ServiceFailures::default(),
    );

    for (ordinal, (case, grant)) in cases.iter().zip(&grants).enumerate() {
        let mut effect = request(case, grant);
        effect.effect_ordinal = u32::try_from(ordinal).unwrap();
        assert_eq!(
            broker.dispatch(effect, PermCancellation::new()).await,
            Ok(success_for(&case.operation)),
            "{} was not dispatched",
            case.name
        );
    }

    let record = record.lock().unwrap();
    assert_eq!(record.executions, cases.len());
    for ((authorized, case), grant) in record.authorized.iter().zip(&cases).zip(&grants) {
        assert_eq!(authorized.invocation_id(), &invocation_id);
        assert_eq!(authorized.grant_id(), grant.grant_id());
        assert_eq!(authorized.principal(), &case.principal);
        assert_eq!(authorized.capability(), case.capability);
    }
}

#[tokio::test]
async fn worker_broker_grants_deny_forged_unknown_replayed_expired_and_wrong_invocation() {
    for case in operation_cases(&invocation("inv-denials")) {
        let current = invocation("inv-denials");
        let live = grant(&case, &current, Instant::now() + Duration::from_secs(30));
        let forged = grant(&case, &current, Instant::now() + Duration::from_secs(30));
        assert_denied_before_execute(
            &case,
            HostEffectError::UnknownGrant,
            live.clone(),
            current.clone(),
            HostCapability::all(),
            ServiceFailures::default(),
            PermCancellation::new(),
            |request| request.grant_id = forged.grant_id().clone(),
        )
        .await;

        let (mut replay_broker, record, _audit_root) = broker(
            current.clone(),
            vec![live.clone()],
            HostCapability::all(),
            ServiceFailures::default(),
        );
        assert!(replay_broker.revoke_grant(live.grant_id()));
        assert_eq!(
            replay_broker
                .dispatch(request(&case, &live), PermCancellation::new())
                .await,
            Err(HostEffectError::ReplayedGrant)
        );
        assert_eq!(record.lock().unwrap().executions, 0);

        let expired = grant(&case, &current, Instant::now() - Duration::from_secs(1));
        let expired_request = request(&case, &expired);
        let (mut expired_broker, record, _audit_root) = broker(
            current.clone(),
            vec![expired],
            HostCapability::all(),
            ServiceFailures::default(),
        );
        assert_eq!(
            expired_broker
                .dispatch(expired_request.clone(), PermCancellation::new())
                .await,
            Err(HostEffectError::ExpiredGrant)
        );
        assert_eq!(
            expired_broker
                .dispatch(expired_request, PermCancellation::new())
                .await,
            Err(HostEffectError::ReplayedGrant)
        );
        assert_eq!(record.lock().unwrap().executions, 0);

        let other = invocation("inv-other");
        let wrong = grant(&case, &other, Instant::now() + Duration::from_secs(30));
        assert_denied_before_execute(
            &case,
            HostEffectError::WrongInvocation,
            wrong,
            current,
            HostCapability::all(),
            ServiceFailures::default(),
            PermCancellation::new(),
            |_| {},
        )
        .await;
    }
}

#[tokio::test]
async fn worker_broker_grants_deny_attribution_cancel_session_capability_and_preflights() {
    let invocation_id = invocation("inv-policy");
    for case in operation_cases(&invocation_id) {
        let make_grant = || {
            grant(
                &case,
                &invocation_id,
                Instant::now() + Duration::from_secs(30),
            )
        };

        assert_denied_before_execute(
            &case,
            HostEffectError::AttributionMismatch,
            make_grant(),
            invocation_id.clone(),
            HostCapability::all(),
            ServiceFailures::default(),
            PermCancellation::new(),
            |request| request.advisory.artifact_id = Some("forged-artifact".into()),
        )
        .await;

        let cancellation = PermCancellation::new();
        cancellation.cancel();
        assert_denied_before_execute(
            &case,
            HostEffectError::InvocationCancelled,
            make_grant(),
            invocation_id.clone(),
            HostCapability::all(),
            ServiceFailures::default(),
            cancellation,
            |_| {},
        )
        .await;

        assert_denied_before_execute(
            &case,
            HostEffectError::SessionDenied,
            make_grant(),
            invocation_id.clone(),
            BTreeSet::new(),
            ServiceFailures::default(),
            PermCancellation::new(),
            |_| {},
        )
        .await;

        let no_capability = InvocationGrant::issue(
            invocation_id.clone(),
            case.principal.clone(),
            BTreeSet::new(),
            Instant::now() + Duration::from_secs(30),
        );
        assert_denied_before_execute(
            &case,
            HostEffectError::CapabilityDenied,
            no_capability,
            invocation_id.clone(),
            HostCapability::all(),
            ServiceFailures::default(),
            PermCancellation::new(),
            |_| {},
        )
        .await;

        for target_error in [
            HostEffectError::InvalidTarget,
            HostEffectError::TargetDenied,
        ] {
            assert_denied_before_execute(
                &case,
                target_error,
                make_grant(),
                invocation_id.clone(),
                HostCapability::all(),
                ServiceFailures {
                    target: Some(target_error),
                    ..ServiceFailures::default()
                },
                PermCancellation::new(),
                |_| {},
            )
            .await;
        }

        for permission_error in [
            HostEffectError::PermissionDenied,
            HostEffectError::AskTimedOut,
        ] {
            assert_denied_before_execute(
                &case,
                permission_error,
                make_grant(),
                invocation_id.clone(),
                HostCapability::all(),
                ServiceFailures {
                    permission: Some(permission_error),
                    ..ServiceFailures::default()
                },
                PermCancellation::new(),
                |_| {},
            )
            .await;
        }

        assert_denied_before_execute(
            &case,
            HostEffectError::BackendFailure,
            make_grant(),
            invocation_id.clone(),
            HostCapability::all(),
            ServiceFailures {
                backend: Some(HostEffectError::BackendFailure),
                ..ServiceFailures::default()
            },
            PermCancellation::new(),
            |_| {},
        )
        .await;
    }
}

#[tokio::test]
async fn worker_broker_grants_erase_authority_on_terminal_cancel_and_recycle() {
    for (transition, expected) in [
        ("terminal", HostEffectError::InvocationTerminal),
        ("cancel", HostEffectError::InvocationCancelled),
        ("recycle", HostEffectError::InvocationRecycled),
    ] {
        let invocation_id = invocation(&format!("inv-{transition}"));
        let case = operation_cases(&invocation_id).remove(0);
        let grant = grant(
            &case,
            &invocation_id,
            Instant::now() + Duration::from_secs(30),
        );
        let effect = request(&case, &grant);
        let (mut broker, record, _audit_root) = broker(
            invocation_id,
            vec![grant],
            HostCapability::all(),
            ServiceFailures::default(),
        );
        match transition {
            "terminal" => broker.finish(),
            "cancel" => broker.cancel_invocation(),
            "recycle" => broker.recycle(),
            _ => unreachable!(),
        }
        assert_eq!(broker.tracked_grant_count(), 0);
        assert_eq!(
            broker.dispatch(effect, PermCancellation::new()).await,
            Err(expected)
        );
        assert_eq!(record.lock().unwrap().executions, 0);
    }
}

#[tokio::test]
async fn worker_effect_cancellation_persists_unknown_and_stops_second_dispatch() {
    let invocation_id = invocation("inv-outcome-unknown-terminal");
    let case = OperationCase {
        name: "read_file",
        operation: EffectOperation::ReadFile {
            path: "docs/first.txt".into(),
        },
        capability: HostCapability::ReadFile,
        principal: GrantPrincipal::ModelAuthored {
            tool_call_id: "tool-call-outcome-unknown".into(),
        },
        advisory: AdvisoryAttribution::default(),
    };
    let grant = grant(
        &case,
        &invocation_id,
        Instant::now() + Duration::from_secs(30),
    );
    let grant_id = grant.grant_id().clone();
    let (broker, record, audit_root) = broker(
        invocation_id.clone(),
        vec![grant],
        BTreeSet::from([HostCapability::ReadFile]),
        ServiceFailures {
            execution: Some(HostEffectError::OutcomeUnknown),
            ..ServiceFailures::default()
        },
    );
    let supervisor = JsWorkerSupervisor::with_launcher_and_watchdog_for_test(
        TestWorkerLauncher::internal_worker_process_with_limits(500, 10_000),
        Duration::from_secs(2),
    );
    let request = RunStep::new(
        r#"
        try { read_file("docs/first.txt"); } catch (_) {}
        try { read_file("docs/second.txt"); } catch (_) {}
        "caught"
        "#
        .into(),
    )
    .with_model_grant(grant_id);

    assert_eq!(
        supervisor
            .execute_bound(invocation_id, request, broker, PermCancellation::new())
            .await,
        Err(WorkerError::EffectOutcomeUnknown)
    );
    let record = record.lock().unwrap().clone();
    assert_eq!(record.execute_calls, 1);
    assert_eq!(record.executions, 0);
    let recovered = EffectAudit::open(audit_root.owner()).unwrap();
    assert_eq!(recovered.records().len(), 2);
    assert_eq!(recovered.records()[0].state, AuditState::Intent);
    assert_eq!(recovered.records()[1].state, AuditState::OutcomeUnknown);
    assert_eq!(supervisor.generation_for_test().await, None);

    let next = supervisor
        .execute(
            RunStep::new("42".into()),
            NoEffects,
            PermCancellation::new(),
        )
        .await
        .unwrap();
    assert_eq!(next.outcome, StepOutcome::Value("42".into()));
    assert_eq!(supervisor.generation_for_test().await, Some(2));
    supervisor.shutdown_for_test().await.unwrap();
}

#[tokio::test]
async fn worker_broker_grants_callback_returns_only_closed_wire_errors() {
    let invocation_id = invocation("inv-callback");
    let case = operation_cases(&invocation_id).remove(0);
    let grant = grant(
        &case,
        &invocation_id,
        Instant::now() + Duration::from_secs(30),
    );
    let effect = request(&case, &grant);
    let (mut broker, record, _audit_root) = broker(
        invocation_id,
        vec![grant],
        HostCapability::all(),
        ServiceFailures {
            permission: Some(HostEffectError::AskTimedOut),
            ..ServiceFailures::default()
        },
    );

    let result =
        InvocationEffectHandler::handle_effect(&mut broker, effect, PermCancellation::new()).await;
    assert!(matches!(
        result,
        EffectResult::Error(error) if error.code == EffectErrorCode::TimedOut
    ));
    assert_eq!(record.lock().unwrap().executions, 0);
}

#[tokio::test]
async fn worker_effect_cancellation_cancels_pending_ask_before_execution() {
    let invocation_id = invocation("inv-pending-ask");
    let case = operation_cases(&invocation_id).remove(0);
    let grant = grant(
        &case,
        &invocation_id,
        Instant::now() + Duration::from_secs(30),
    );
    let effect = request(&case, &grant);
    let record = Arc::new(Mutex::new(ServiceRecord::default()));
    let service = RecordingService {
        failures: ServiceFailures::default(),
        normalized_target: None,
        pending_permission: true,
        permission_delay_until: None,
        record: Arc::clone(&record),
    };
    let audit_root = AuditTempRoot::new("pending-ask");
    let audit = EffectAudit::open(audit_root.owner()).unwrap();
    let mut broker = InvocationBroker::new(
        invocation_id,
        vec![grant],
        HostCapability::all(),
        service,
        Arc::new(Mutex::new(audit)),
    )
    .unwrap();
    let cancellation = PermCancellation::new();
    let canceller = cancellation.clone();
    tokio::spawn(async move {
        tokio::task::yield_now().await;
        canceller.cancel();
    });

    assert_eq!(
        broker.dispatch(effect, cancellation).await,
        Err(HostEffectError::InvocationCancelled)
    );
    assert_eq!(broker.tracked_grant_count(), 0);
    let record = record.lock().unwrap();
    assert_eq!(record.executions, 0);
    assert_eq!(record.discards, 1, "cancelled Ask retained prepared state");
}

#[tokio::test]
async fn worker_broker_grants_never_allow_a_skill_to_propose_another_skill() {
    let invocation_id = invocation("inv-skill-proposal");
    let mut case = operation_cases(&invocation_id).remove(4);
    case.principal = GrantPrincipal::Skill {
        artifact_id: "artifact-proposer".into(),
        export: "propose".into(),
        invocation_id: invocation_id.to_string(),
    };
    case.advisory = AdvisoryAttribution {
        artifact_id: Some("artifact-proposer".into()),
        export: Some("propose".into()),
    };
    let skill_grant = grant(
        &case,
        &invocation_id,
        Instant::now() + Duration::from_secs(30),
    );

    assert_denied_before_execute(
        &case,
        HostEffectError::CapabilityDenied,
        skill_grant,
        invocation_id,
        HostCapability::all(),
        ServiceFailures::default(),
        PermCancellation::new(),
        |_| {},
    )
    .await;
}

#[cfg(feature = "skills")]
#[tokio::test]
async fn worker_broker_records_skill_capability_denials_for_parent_telemetry() {
    let invocation_id = invocation("inv-skill-policy-telemetry");
    let mut case = operation_cases(&invocation_id).remove(4);
    let skill_invocation = "a".repeat(64);
    case.principal = GrantPrincipal::Skill {
        artifact_id: "artifact-proposer".into(),
        export: "propose".into(),
        invocation_id: skill_invocation.clone(),
    };
    case.advisory = AdvisoryAttribution {
        artifact_id: Some("artifact-proposer".into()),
        export: Some("propose".into()),
    };
    let grant = grant(
        &case,
        &invocation_id,
        Instant::now() + Duration::from_secs(30),
    );
    let effect = request(&case, &grant);
    let (mut broker, _record, _audit) = broker(
        invocation_id,
        vec![grant],
        HostCapability::all(),
        ServiceFailures::default(),
    );
    let tracker = broker.capability_denial_tracker();

    let result = broker.handle_effect(effect, PermCancellation::new()).await;
    assert!(matches!(
        result,
        EffectResult::Error(crate::extras::js::protocol::EffectError {
            code: EffectErrorCode::CapabilityDenied
        })
    ));
    assert_eq!(
        tracker.snapshot().unwrap(),
        BTreeSet::from([skill_invocation])
    );
}

#[tokio::test]
async fn worker_broker_grants_expiring_during_ask_never_execute() {
    let invocation_id = invocation("inv-expiring-ask");
    let case = operation_cases(&invocation_id).remove(0);
    // Full cross-platform suites can leave this task unscheduled for more than a second before
    // dispatch begins. Keep the expiry comfortably beyond setup so the test exercises expiry
    // during the Ask wait, rather than an unrelated pre-dispatch expiry.
    let expires_at = Instant::now() + Duration::from_secs(5);
    let grant = grant(&case, &invocation_id, expires_at);
    let effect = request(&case, &grant);
    let record = Arc::new(Mutex::new(ServiceRecord::default()));
    let service = RecordingService {
        failures: ServiceFailures::default(),
        normalized_target: None,
        pending_permission: false,
        permission_delay_until: Some(expires_at),
        record: Arc::clone(&record),
    };
    let audit_root = AuditTempRoot::new("expiring-ask");
    let audit = EffectAudit::open(audit_root.owner()).unwrap();
    let mut broker = InvocationBroker::new(
        invocation_id,
        vec![grant],
        HostCapability::all(),
        service,
        Arc::new(Mutex::new(audit)),
    )
    .unwrap();

    assert_eq!(
        broker
            .dispatch(effect.clone(), PermCancellation::new())
            .await,
        Err(HostEffectError::ExpiredGrant)
    );
    assert_eq!(
        broker.dispatch(effect, PermCancellation::new()).await,
        Err(HostEffectError::ReplayedGrant)
    );
    let record = record.lock().unwrap();
    assert_eq!(record.authorizations, 1, "the Ask preflight must run");
    assert_eq!(record.execute_calls, 0, "an expired grant reached execute");
    assert_eq!(record.executions, 0);
}

#[tokio::test]
async fn worker_broker_grants_expiring_while_waiting_for_audit_never_execute() {
    let invocation_id = invocation("inv-expiring-audit-lock");
    let case = operation_cases(&invocation_id).remove(0);
    let record = Arc::new(Mutex::new(ServiceRecord::default()));
    let service = RecordingService {
        failures: ServiceFailures::default(),
        normalized_target: None,
        pending_permission: false,
        permission_delay_until: None,
        record: Arc::clone(&record),
    };
    let audit_root = AuditTempRoot::new("expiring-audit-lock");
    let audit = Arc::new(Mutex::new(EffectAudit::open(audit_root.owner()).unwrap()));
    let (locked_tx, locked_rx) = std::sync::mpsc::sync_channel(1);
    let locked_audit = Arc::clone(&audit);
    let holder = std::thread::spawn(move || {
        let _guard = locked_audit.lock().unwrap();
        locked_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(150));
    });
    locked_rx.recv().unwrap();
    let grant = grant(
        &case,
        &invocation_id,
        Instant::now() + Duration::from_millis(40),
    );
    let effect = request(&case, &grant);
    let mut broker = InvocationBroker::new(
        invocation_id,
        vec![grant],
        HostCapability::all(),
        service,
        Arc::clone(&audit),
    )
    .unwrap();

    assert_eq!(
        broker.dispatch(effect, PermCancellation::new()).await,
        Err(HostEffectError::ExpiredGrant)
    );
    holder.join().unwrap();
    assert!(broker.audit_records_for_test().is_empty());
    let record = record.lock().unwrap();
    assert_eq!(record.authorizations, 1);
    assert_eq!(
        record.execute_calls, 0,
        "expired audit waiter reached execute"
    );
    assert_eq!(record.executions, 0);
}

#[tokio::test]
async fn worker_broker_grants_execute_cancellation_erases_authority_before_redispatch() {
    let invocation_id = invocation("inv-execute-cancel");
    let mut cases = operation_cases(&invocation_id);
    let first_case = cases.remove(0);
    let second_case = cases.remove(0);
    let first_grant = grant(
        &first_case,
        &invocation_id,
        Instant::now() + Duration::from_secs(30),
    );
    let second_grant = grant(
        &second_case,
        &invocation_id,
        Instant::now() + Duration::from_secs(30),
    );
    let first_effect = request(&first_case, &first_grant);
    let second_effect = request(&second_case, &second_grant);
    let (mut broker, record, _audit_root) = broker(
        invocation_id,
        vec![first_grant, second_grant],
        HostCapability::all(),
        ServiceFailures {
            execution: Some(HostEffectError::InvocationCancelled),
            ..ServiceFailures::default()
        },
    );

    assert_eq!(
        broker.dispatch(first_effect, PermCancellation::new()).await,
        Err(HostEffectError::InvocationCancelled)
    );
    assert_eq!(broker.tracked_grant_count(), 0);
    assert_eq!(
        broker
            .dispatch(second_effect, PermCancellation::new())
            .await,
        Err(HostEffectError::InvocationCancelled)
    );
    let record = record.lock().unwrap();
    assert_eq!(record.execute_calls, 1, "redispatch reached the service");
    assert_eq!(record.executions, 0);
}

#[tokio::test]
async fn js_effect_audit_ordering_requires_durable_intent_before_read_file() {
    let invocation_id = invocation("inv-audit-ordering-red");
    let case = operation_cases(&invocation_id).remove(0);
    let grant = grant(
        &case,
        &invocation_id,
        Instant::now() + Duration::from_secs(30),
    );
    let record = Arc::new(Mutex::new(ServiceRecord::default()));
    let service = RecordingService {
        failures: ServiceFailures::default(),
        normalized_target: None,
        pending_permission: false,
        permission_delay_until: None,
        record,
    };
    let root = AuditTempRoot::new("ordering-red");
    let audit = EffectAudit::open(root.owner()).unwrap();
    let mut broker = InvocationBroker::new(
        invocation_id,
        vec![grant.clone()],
        HostCapability::all(),
        service,
        Arc::new(Mutex::new(audit)),
    )
    .unwrap();

    assert_eq!(
        {
            let mut first = request(&case, &grant);
            first.effect_ordinal = 0;
            broker.dispatch(first, PermCancellation::new()).await
        },
        Ok(success_for(&case.operation))
    );
    assert_eq!(broker.audit_records_for_test().len(), 2);
}

struct AuditTempRoot(PathBuf);

impl AuditTempRoot {
    fn new(tag: &str) -> Self {
        // Hosted macOS 15 can reject the audit lock/open sequence beneath the shared
        // system temporary directory even though the repository's APFS volume supports
        // the durability contract. Keep this test-only state under Cargo's build tree.
        let parent = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("worker-broker-audits");
        let path = parent.join(format!(
            "mini-agent-js-effect-audit-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn owner(&self) -> EffectAuditPathOwner {
        AppPaths {
            config_dir: self.0.join("config"),
            data_dir: self.0.join("data"),
            local_data_dir: self.0.join("local"),
            state_dir: self.0.join("state"),
            cache_dir: self.0.join("cache"),
            credentials_dir: self.0.join("credentials"),
            project_dir: None,
        }
        .effect_audit()
    }
}

impl Drop for AuditTempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy, Debug, Default)]
enum OrderingExecutionFailure {
    #[default]
    None,
    BeforeEffect,
    AfterEffect,
    PendingAfterIntent,
}

#[derive(Debug, Default)]
struct OrderingRecord {
    validations: usize,
    backend_checks: usize,
    authorizations: usize,
    execute_calls: usize,
    effects: usize,
    saw_durable_intent: usize,
}

struct OrderingService {
    owner: EffectAuditPathOwner,
    failure: OrderingExecutionFailure,
    record: Arc<Mutex<OrderingRecord>>,
}

impl ParentEffectService for OrderingService {
    fn validate_target(
        &mut self,
        _authorized: &AuthorizedEffect,
        _operation: &EffectOperation,
    ) -> Result<(), HostEffectError> {
        self.record.lock().unwrap().validations += 1;
        Ok(())
    }

    fn ensure_backend(
        &mut self,
        _authorized: &AuthorizedEffect,
        _operation: &EffectOperation,
    ) -> Result<(), HostEffectError> {
        self.record.lock().unwrap().backend_checks += 1;
        Ok(())
    }

    fn normalize_target<'a>(
        &'a mut self,
        _authorized: &'a AuthorizedEffect,
        operation: &'a EffectOperation,
        _cancellation: PermCancellation,
    ) -> ServiceFuture<'a, Result<NormalizedTarget, HostEffectError>> {
        let target = normalized_target(operation);
        Box::pin(async move { target })
    }

    fn authorize<'a>(
        &'a mut self,
        _authorized: &'a AuthorizedEffect,
        operation: &'a EffectOperation,
        _cancellation: PermCancellation,
    ) -> ServiceFuture<'a, Result<AuthorizedTarget, HostEffectError>> {
        self.record.lock().unwrap().authorizations += 1;
        let target = authorized_target(operation);
        Box::pin(async move { Ok(target) })
    }

    fn execute<'a>(
        &'a mut self,
        _authorized: &'a AuthorizedEffect,
        operation: &'a EffectOperation,
        _cancellation: PermCancellation,
    ) -> ServiceFuture<'a, Result<EffectResult, HostEffectError>> {
        let owner = self.owner.clone();
        let failure = self.failure;
        let record = Arc::clone(&self.record);
        let result = success_for(operation);
        Box::pin(async move {
            let bytes = audit_bytes(&owner);
            let durable_intents = String::from_utf8_lossy(&bytes)
                .matches("\"state\":\"intent\"")
                .count();
            {
                let mut record = record.lock().unwrap();
                record.execute_calls += 1;
                record.saw_durable_intent += usize::from(durable_intents > 0);
                if matches!(failure, OrderingExecutionFailure::BeforeEffect) {
                    return Err(HostEffectError::BackendFailure);
                }
                record.effects += 1;
                if matches!(failure, OrderingExecutionFailure::AfterEffect) {
                    return Err(HostEffectError::OutcomeUnknown);
                }
            }
            if matches!(failure, OrderingExecutionFailure::PendingAfterIntent) {
                return std::future::pending().await;
            }
            Ok(result)
        })
    }
}

#[tokio::test]
async fn worker_effect_cancellation_reconciles_dropped_post_intent_future_as_unknown() {
    let invocation_id = invocation("inv-interrupted-post-intent");
    let case = operation_cases(&invocation_id).remove(1);
    let grant = grant(
        &case,
        &invocation_id,
        Instant::now() + Duration::from_secs(30),
    );
    let request = request(&case, &grant);
    let root = AuditTempRoot::new("interrupted-post-intent");
    let owner = root.owner();
    let audit = EffectAudit::open(owner.clone()).unwrap();
    let (service, record) = ordering_service(owner, OrderingExecutionFailure::PendingAfterIntent);
    let mut broker = InvocationBroker::new(
        invocation_id,
        vec![grant],
        HostCapability::all(),
        service,
        Arc::new(Mutex::new(audit)),
    )
    .unwrap();

    let mut dispatch = Box::pin(broker.dispatch(request, PermCancellation::new()));
    tokio::select! {
        biased;
        result = &mut dispatch => panic!("post-intent service unexpectedly completed: {result:?}"),
        _ = tokio::task::yield_now() => {}
    }
    drop(dispatch);
    assert_eq!(record.lock().unwrap().saw_durable_intent, 1);

    assert!(matches!(
        InvocationEffectHandler::reconcile_interrupted_effect(&mut broker),
        EffectResult::Error(error) if error.code == EffectErrorCode::OutcomeUnknown
    ));
    let records = broker.audit_records_for_test();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].state, AuditState::Intent);
    assert_eq!(records[1].state, AuditState::OutcomeUnknown);
    broker.recycle();
    assert_eq!(broker.tracked_grant_count(), 0);
}

fn ordering_service(
    owner: EffectAuditPathOwner,
    failure: OrderingExecutionFailure,
) -> (OrderingService, Arc<Mutex<OrderingRecord>>) {
    let record = Arc::new(Mutex::new(OrderingRecord::default()));
    (
        OrderingService {
            owner,
            failure,
            record: Arc::clone(&record),
        },
        record,
    )
}

fn audit_intent(effect_id: &str, target: SanitizedTarget) -> EffectIntent {
    EffectIntent {
        effect_id: effect_id.into(),
        invocation_id: "inv-audit".into(),
        grant_id: "grant-audit".into(),
        sequence: 1,
        timestamp_ms: 1_800_000_000_000,
        artifact_id: Some("artifact-audit".into()),
        export: Some("run".into()),
        capability: AuditCapability::ReadFile,
        normalized_target: target,
        decision: AuditDecision::Authorized,
    }
}

fn audit_segments(owner: &EffectAuditPathOwner) -> Vec<PathBuf> {
    let mut segments = std::fs::read_dir(owner.directory())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("segment-") && name.ends_with(".audit"))
        })
        .collect::<Vec<_>>();
    segments.sort();
    segments
}

fn audit_bytes(owner: &EffectAuditPathOwner) -> Vec<u8> {
    let mut bytes = Vec::new();
    for segment in audit_segments(owner) {
        bytes.extend(std::fs::read(segment).unwrap());
    }
    bytes
}

fn raw_effect_fragments(operation: &EffectOperation) -> Vec<&str> {
    match operation {
        EffectOperation::ReadFile { path } => vec![path],
        EffectOperation::WriteFile { path, content } => vec![path, content],
        EffectOperation::Fetch { .. } => vec!["example.test", "/api"],
        EffectOperation::Spawn { program, arguments } => {
            let mut fragments = vec![program.as_str()];
            fragments.extend(arguments.iter().map(String::as_str));
            fragments
        }
        EffectOperation::ProposeSkill { draft } => {
            vec![draft.source.as_str(), draft.description.as_str()]
        }
    }
}

#[tokio::test]
async fn js_effect_audit_ordering_covers_every_operation_and_parent_identity() {
    let invocation_id = invocation("inv-ordering-success");
    for case in operation_cases(&invocation_id) {
        let root = AuditTempRoot::new(case.name);
        let owner = root.owner();
        let audit = EffectAudit::open(owner.clone()).unwrap();
        let grant = grant(
            &case,
            &invocation_id,
            Instant::now() + Duration::from_secs(30),
        );
        let (service, service_record) =
            ordering_service(owner.clone(), OrderingExecutionFailure::None);
        let mut broker = InvocationBroker::new(
            invocation_id.clone(),
            vec![grant.clone()],
            HostCapability::all(),
            service,
            Arc::new(Mutex::new(audit)),
        )
        .unwrap();
        let mut effect = request(&case, &grant);
        effect.effect_ordinal = 0;

        assert_eq!(
            broker.dispatch(effect, PermCancellation::new()).await,
            Ok(success_for(&case.operation)),
            "{} did not complete",
            case.name
        );
        let service_record = service_record.lock().unwrap();
        assert_eq!(service_record.validations, 1, "{} validation", case.name);
        assert_eq!(service_record.backend_checks, 1, "{} backend", case.name);
        assert_eq!(
            service_record.authorizations, 1,
            "{} authorization",
            case.name
        );
        assert_eq!(service_record.execute_calls, 1, "{} execution", case.name);
        assert_eq!(service_record.effects, 1, "{} effect", case.name);
        assert_eq!(
            service_record.saw_durable_intent, 1,
            "{} executed before its durable intent",
            case.name
        );
        drop(service_record);

        let records = broker.audit_records_for_test();
        assert_eq!(records.len(), 2, "{} audit count", case.name);
        assert_eq!(records[0].state, AuditState::Intent, "{} intent", case.name);
        assert_eq!(
            records[1].state,
            AuditState::Completed,
            "{} completion",
            case.name
        );
        assert_eq!(records[0].invocation_id, invocation_id.as_str());
        assert_eq!(records[0].grant_id, grant.grant_id().get().to_string());
        assert_eq!(
            records[0].sequence, 1,
            "zero-based wire ordinal maps safely"
        );
        let expected_identity = match &case.principal {
            GrantPrincipal::ModelAuthored { .. } => (None, None),
            GrantPrincipal::Skill {
                artifact_id,
                export,
                ..
            } => (Some(artifact_id.as_str()), Some(export.as_str())),
        };
        assert_eq!(records[0].artifact_id.as_deref(), expected_identity.0);
        assert_eq!(records[0].export.as_deref(), expected_identity.1);

        let bytes = String::from_utf8_lossy(&audit_bytes(&owner)).into_owned();
        for fragment in raw_effect_fragments(&case.operation) {
            assert!(
                !bytes.contains(fragment),
                "{} persisted raw target/content fragment {fragment:?}",
                case.name
            );
        }
    }
}

#[tokio::test]
async fn js_effect_audit_ordering_pre_intent_failures_execute_nothing() {
    let invocation_id = invocation("inv-ordering-pre-intent");
    for case in operation_cases(&invocation_id) {
        for failure in [AuditFailurePoint::Append, AuditFailurePoint::FileSync] {
            let root = AuditTempRoot::new(case.name);
            let owner = root.owner();
            let audit = EffectAudit::open(owner.clone()).unwrap();
            let grant = grant(
                &case,
                &invocation_id,
                Instant::now() + Duration::from_secs(30),
            );
            let (service, service_record) = ordering_service(owner, OrderingExecutionFailure::None);
            let mut broker = InvocationBroker::new(
                invocation_id.clone(),
                vec![grant.clone()],
                HostCapability::all(),
                service,
                Arc::new(Mutex::new(audit)),
            )
            .unwrap();
            broker.fail_next_audit_durability_for_test(failure);

            assert_eq!(
                broker
                    .dispatch(request(&case, &grant), PermCancellation::new())
                    .await,
                Err(HostEffectError::AuditFailure),
                "{} {failure:?}",
                case.name
            );
            let service_record = service_record.lock().unwrap();
            assert_eq!(service_record.execute_calls, 0, "{} {failure:?}", case.name);
            assert_eq!(service_record.effects, 0, "{} {failure:?}", case.name);
        }

        let target_grant = grant(
            &case,
            &invocation_id,
            Instant::now() + Duration::from_secs(30),
        );
        let (mut target_broker, target_record, _target_root) = broker(
            invocation_id.clone(),
            vec![target_grant.clone()],
            HostCapability::all(),
            ServiceFailures {
                target: Some(HostEffectError::TargetDenied),
                ..ServiceFailures::default()
            },
        );
        assert_eq!(
            target_broker
                .dispatch(request(&case, &target_grant), PermCancellation::new())
                .await,
            Err(HostEffectError::TargetDenied)
        );
        assert_eq!(target_record.lock().unwrap().execute_calls, 0);
        assert!(target_broker.audit_records_for_test().is_empty());

        let session_grant = grant(
            &case,
            &invocation_id,
            Instant::now() + Duration::from_secs(30),
        );
        let (mut session_broker, session_record, _session_root) = broker(
            invocation_id.clone(),
            vec![session_grant.clone()],
            BTreeSet::new(),
            ServiceFailures::default(),
        );
        assert_eq!(
            session_broker
                .dispatch(request(&case, &session_grant), PermCancellation::new())
                .await,
            Err(HostEffectError::SessionDenied)
        );
        assert_eq!(session_record.lock().unwrap().execute_calls, 0);
        assert!(session_broker.audit_records_for_test().is_empty());
    }
}

#[tokio::test]
async fn js_effect_audit_ordering_records_attempt_outcomes_and_denies_replay() {
    let invocation_id = invocation("inv-ordering-outcomes");
    for case in operation_cases(&invocation_id) {
        for (failure, expected, state, code, expected_effects) in [
            (
                OrderingExecutionFailure::BeforeEffect,
                HostEffectError::BackendFailure,
                AuditState::Completed,
                "backend_failure",
                0,
            ),
            (
                OrderingExecutionFailure::AfterEffect,
                HostEffectError::OutcomeUnknown,
                AuditState::OutcomeUnknown,
                "outcome_unknown",
                1,
            ),
        ] {
            let root = AuditTempRoot::new(case.name);
            let owner = root.owner();
            let audit = EffectAudit::open(owner.clone()).unwrap();
            let grant = grant(
                &case,
                &invocation_id,
                Instant::now() + Duration::from_secs(30),
            );
            let (service, service_record) = ordering_service(owner, failure);
            let mut broker = InvocationBroker::new(
                invocation_id.clone(),
                vec![grant.clone()],
                HostCapability::all(),
                service,
                Arc::new(Mutex::new(audit)),
            )
            .unwrap();
            assert_eq!(
                broker
                    .dispatch(request(&case, &grant), PermCancellation::new())
                    .await,
                Err(expected),
                "{} {failure:?}",
                case.name
            );
            assert_eq!(service_record.lock().unwrap().effects, expected_effects);
            let records = broker.audit_records_for_test();
            assert_eq!(records.len(), 2);
            assert_eq!(records[1].state, state);
            assert_eq!(records[1].result_code.as_deref(), Some(code));
        }

        let root = AuditTempRoot::new(case.name);
        let owner = root.owner();
        let audit = EffectAudit::open(owner.clone()).unwrap();
        let grant = grant(
            &case,
            &invocation_id,
            Instant::now() + Duration::from_secs(30),
        );
        let effect = request(&case, &grant);
        let (service, service_record) = ordering_service(owner, OrderingExecutionFailure::None);
        let mut broker = InvocationBroker::new(
            invocation_id.clone(),
            vec![grant],
            HostCapability::all(),
            service,
            Arc::new(Mutex::new(audit)),
        )
        .unwrap();
        assert!(
            broker
                .dispatch(effect.clone(), PermCancellation::new())
                .await
                .is_ok()
        );
        assert_eq!(
            broker.dispatch(effect, PermCancellation::new()).await,
            Err(HostEffectError::AuditFailure),
            "{} duplicate was accepted",
            case.name
        );
        assert_eq!(service_record.lock().unwrap().effects, 1);
    }
}

#[tokio::test]
async fn js_effect_audit_ordering_completion_append_failure_recovers_unknown() {
    let invocation_id = invocation("inv-ordering-completion-failure");
    for case in operation_cases(&invocation_id) {
        let root = AuditTempRoot::new(case.name);
        let owner = root.owner();
        let audit = EffectAudit::open(owner.clone()).unwrap();
        let grant = grant(
            &case,
            &invocation_id,
            Instant::now() + Duration::from_secs(30),
        );
        let (service, service_record) =
            ordering_service(owner.clone(), OrderingExecutionFailure::None);
        let mut broker = InvocationBroker::new(
            invocation_id.clone(),
            vec![grant.clone()],
            HostCapability::all(),
            service,
            Arc::new(Mutex::new(audit)),
        )
        .unwrap();
        broker.fail_next_completion_durability_for_test(AuditFailurePoint::Append);

        assert_eq!(
            broker
                .dispatch(request(&case, &grant), PermCancellation::new())
                .await,
            Err(HostEffectError::AuditFailure),
            "{} did not surface completion append failure",
            case.name
        );
        assert_eq!(service_record.lock().unwrap().effects, 1);
        assert_eq!(broker.audit_records_for_test().len(), 1);
        assert_eq!(broker.audit_records_for_test()[0].state, AuditState::Intent);
        drop(broker);

        let recovered = EffectAudit::open(owner).unwrap();
        assert_eq!(recovered.records().len(), 2);
        assert_eq!(recovered.records()[1].state, AuditState::OutcomeUnknown);
        assert_eq!(
            recovered.records()[1].result_code.as_deref(),
            Some("outcome_unknown")
        );
    }
}

#[test]
fn js_effect_audit_storage_private_path_and_exclusive_writer_fail_closed() {
    let root = AuditTempRoot::new("private-lock");
    let owner = root.owner();
    let audit = EffectAudit::open(owner.clone()).unwrap();

    assert!(owner.directory().starts_with(owner.state_root()));
    assert!(owner.directory().is_dir());
    assert!(owner.lock_file().is_file());
    assert!(matches!(
        EffectAudit::open(owner.clone()),
        Err(AuditError::WriterLocked)
    ));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(owner.directory())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(owner.lock_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(owner.target_key_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(owner.initialization_marker())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        for segment in audit_segments(&owner) {
            assert_eq!(
                std::fs::metadata(segment).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    drop(audit);
    EffectAudit::open(owner).unwrap();
}

#[test]
fn js_effect_audit_storage_hash_chain_privacy_and_crash_recovery_are_truthful() {
    let root = AuditTempRoot::new("privacy-recovery");
    let owner = root.owner();
    let path_secret = "workspace/credential-super-secret.txt?token=path-secret";
    let url_secret = "https://user:password@example.test/private?token=query-secret";
    let argv_secret = "--password=argv-secret";
    let file_target_tag;

    {
        let mut audit = EffectAudit::open(owner.clone()).unwrap();
        let file_target = audit.file_target(path_secret);
        file_target_tag = file_target.clone();
        assert_ne!(file_target, audit.file_target("workspace/other.txt"));
        audit
            .append_intent(audit_intent("effect-file", file_target))
            .unwrap();
        audit
            .append_completion(EffectCompletion {
                effect_id: "effect-file".into(),
                result_code: AuditResultCode::Succeeded,
            })
            .unwrap();
        let write_target = audit.write_file_target(path_secret);
        let mut write = audit_intent("effect-write", write_target);
        write.capability = AuditCapability::WriteFile;
        audit.append_intent(write).unwrap();
        audit
            .append_completion(EffectCompletion {
                effect_id: "effect-write".into(),
                result_code: AuditResultCode::Succeeded,
            })
            .unwrap();
        let fetch_target = audit.fetch_target(url_secret, "post").unwrap();
        let mut fetch = audit_intent("effect-fetch", fetch_target);
        fetch.capability = AuditCapability::Fetch;
        audit.append_intent(fetch).unwrap();
        let spawn_target = audit.spawn_target("helper");
        let mut spawn = audit_intent("effect-spawn", spawn_target);
        spawn.capability = AuditCapability::Spawn;
        audit.append_intent(spawn).unwrap();
        let mut proposal = audit_intent("effect-proposal", audit.proposal_target());
        proposal.capability = AuditCapability::ProposeSkill;
        audit.append_intent(proposal).unwrap();
        audit
            .append_completion(EffectCompletion {
                effect_id: "effect-proposal".into(),
                result_code: AuditResultCode::Succeeded,
            })
            .unwrap();
    }

    let stored = audit_bytes(&owner);
    for secret in [
        path_secret,
        url_secret,
        "password",
        "query-secret",
        argv_secret,
    ] {
        assert!(
            !stored
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "audit persisted secret fixture {secret:?}"
        );
    }

    let recovered = EffectAudit::open(owner.clone()).unwrap();
    assert_eq!(recovered.file_target(path_secret), file_target_tag);
    for effect_id in ["effect-fetch", "effect-spawn"] {
        assert!(recovered.records().iter().any(|record| {
            record.effect_id == effect_id && record.state == AuditState::OutcomeUnknown
        }));
    }
    let unknown_count = recovered
        .records()
        .iter()
        .filter(|record| record.state == AuditState::OutcomeUnknown)
        .count();
    drop(recovered);
    let reopened = EffectAudit::open(owner).unwrap();
    assert_eq!(
        reopened
            .records()
            .iter()
            .filter(|record| record.state == AuditState::OutcomeUnknown)
            .count(),
        unknown_count,
        "restart duplicated recovered unknown outcomes"
    );
}

#[test]
fn js_effect_audit_storage_truncated_tail_recovers_but_interior_corruption_fails() {
    let root = AuditTempRoot::new("truncation");
    let owner = root.owner();
    {
        let mut audit = EffectAudit::open(owner.clone()).unwrap();
        let target = audit.file_target("safe/path");
        audit
            .append_intent(audit_intent("effect-tail", target))
            .unwrap();
    }
    let segment = audit_segments(&owner).pop().unwrap();
    let valid_length = std::fs::metadata(&segment).unwrap().len();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&segment)
        .unwrap();
    file.write_all(&128_u32.to_be_bytes()).unwrap();
    file.write_all(br#"{"partial":true"#).unwrap();
    drop(file);

    let recovered = EffectAudit::open(owner.clone()).unwrap();
    assert!(std::fs::metadata(&segment).unwrap().len() >= valid_length);
    assert!(recovered.records().iter().any(|record| {
        record.effect_id == "effect-tail" && record.state == AuditState::OutcomeUnknown
    }));
    drop(recovered);

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&segment)
        .unwrap();
    file.seek(SeekFrom::Start(8)).unwrap();
    let mut byte = [0_u8; 1];
    file.read_exact(&mut byte).unwrap();
    file.seek(SeekFrom::Current(-1)).unwrap();
    byte[0] ^= 1;
    file.write_all(&byte).unwrap();
    file.sync_all().unwrap();
    assert!(matches!(
        EffectAudit::open(owner),
        Err(AuditError::CorruptRecord) | Err(AuditError::HashMismatch)
    ));
}

#[test]
fn js_effect_audit_storage_final_prefix_corruption_is_not_truncated_as_a_crash_tail() {
    let root = AuditTempRoot::new("prefix-corruption");
    let owner = root.owner();
    {
        let mut audit = EffectAudit::open(owner.clone()).unwrap();
        let target = audit.file_target("safe/path");
        audit
            .append_intent(audit_intent("effect-prefix", target))
            .unwrap();
    }
    let segment = audit_segments(&owner).pop().unwrap();
    let mut bytes = std::fs::read(&segment).unwrap();
    let mut offset = 0_usize;
    let mut final_offset = 0_usize;
    while offset < bytes.len() {
        final_offset = offset;
        let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        offset += length + 8;
    }
    let length = u32::from_be_bytes(bytes[final_offset..final_offset + 4].try_into().unwrap());
    bytes[final_offset..final_offset + 4].copy_from_slice(&(length + 1).to_be_bytes());
    std::fs::write(segment, bytes).unwrap();

    assert!(matches!(
        EffectAudit::open(owner),
        Err(AuditError::CorruptRecord)
    ));
}

#[test]
fn js_effect_audit_storage_rotation_missing_segments_replay_and_hash_mismatch_fail_closed() {
    let root = AuditTempRoot::new("rotation");
    let owner = root.owner();
    let options = AuditOpenOptions::for_test(1_024);
    {
        let mut audit = EffectAudit::open_with_options(owner.clone(), options.clone()).unwrap();
        for index in 0..12 {
            let effect_id = format!("effect-{index}");
            let target = audit.file_target(&format!("safe/path/{index}"));
            let mut intent = audit_intent(&effect_id, target);
            intent.sequence = index + 1;
            audit.append_intent(intent.clone()).unwrap();
            assert!(matches!(
                audit.append_intent(intent),
                Err(AuditError::ReplayedEffect)
            ));
            let completion = EffectCompletion {
                effect_id,
                result_code: AuditResultCode::Succeeded,
            };
            audit.append_completion(completion.clone()).unwrap();
            assert!(matches!(
                audit.append_completion(completion),
                Err(AuditError::ReplayedEffect)
            ));
        }
        assert!(audit.rotation_anchor_count() >= 2);
    }
    let segments = audit_segments(&owner);
    assert!(
        segments.len() >= 3,
        "rotation did not create linked segments"
    );

    let missing_root = AuditTempRoot::new("missing-segment");
    let missing_owner = missing_root.owner();
    // Initialize the copied audit through the production path. In particular,
    // Windows requires the state directory, marker, and private files to have
    // the same ownership setup as a real audit before replay reaches segment
    // continuity validation.
    EffectAudit::open_with_options(missing_owner.clone(), options.clone()).unwrap();
    for segment in audit_segments(&missing_owner) {
        std::fs::remove_file(segment).unwrap();
    }
    std::fs::copy(owner.target_key_file(), missing_owner.target_key_file()).unwrap();
    for segment in &segments {
        std::fs::copy(
            segment,
            missing_owner.directory().join(segment.file_name().unwrap()),
        )
        .unwrap();
    }
    std::fs::remove_file(
        missing_owner
            .directory()
            .join(segments[1].file_name().unwrap()),
    )
    .unwrap();
    assert!(matches!(
        EffectAudit::open_with_options(missing_owner, options.clone()),
        Err(AuditError::MissingSegment)
    ));

    let hash_root = AuditTempRoot::new("hash-mismatch");
    let hash_owner = hash_root.owner();
    {
        let mut audit =
            EffectAudit::open_with_options(hash_owner.clone(), options.clone()).unwrap();
        for index in 0..12 {
            let effect_id = format!("hash-effect-{index}");
            let target = audit.file_target(&format!("safe/hash/{index}"));
            let mut intent = audit_intent(&effect_id, target);
            intent.sequence = index + 1;
            audit.append_intent(intent).unwrap();
            audit
                .append_completion(EffectCompletion {
                    effect_id,
                    result_code: AuditResultCode::Succeeded,
                })
                .unwrap();
        }
        assert!(audit.rotation_anchor_count() >= 2);
    }
    let hash_segments = audit_segments(&hash_owner);
    assert!(
        hash_segments.len() >= 3,
        "hash fixture did not create linked segments"
    );
    let last = hash_segments.last().unwrap();
    let mut bytes = std::fs::read(last).unwrap();
    let marker = b"record_hash";
    let offset = bytes
        .windows(marker.len())
        .position(|window| window == marker)
        .unwrap();
    let hash_start = bytes[offset..]
        .windows(2)
        .position(|window| window == b":\"")
        .unwrap()
        + offset
        + 2;
    bytes[hash_start] = if bytes[hash_start] == b'a' {
        b'b'
    } else {
        b'a'
    };
    std::fs::write(last, bytes).unwrap();
    assert!(matches!(
        EffectAudit::open_with_options(hash_owner, options.clone()),
        Err(AuditError::HashMismatch)
    ));

    let key = std::fs::read(owner.target_key_file()).unwrap();
    let mut corrupted_key = key.clone();
    corrupted_key[0] ^= 1;
    std::fs::write(owner.target_key_file(), &corrupted_key).unwrap();
    assert!(matches!(
        EffectAudit::open_with_options(owner.clone(), options.clone()),
        Err(AuditError::KeyUnavailable)
    ));
    std::fs::write(owner.target_key_file(), key).unwrap();
    std::fs::remove_file(owner.target_key_file()).unwrap();
    assert!(matches!(
        EffectAudit::open(owner.clone()),
        Err(AuditError::KeyUnavailable)
    ));

    let initialized_root = AuditTempRoot::new("initialized-missing-chain");
    let initialized_owner = initialized_root.owner();
    {
        EffectAudit::open(initialized_owner.clone()).unwrap();
    }
    for segment in audit_segments(&initialized_owner) {
        std::fs::remove_file(segment).unwrap();
    }
    assert!(matches!(
        EffectAudit::open(initialized_owner),
        Err(AuditError::MissingSegment)
    ));
}

#[test]
fn js_effect_audit_storage_retention_limit_poisoning_is_fail_closed() {
    let root = AuditTempRoot::new("retention-limit");
    let owner = root.owner();
    let options = AuditOpenOptions::for_test(2_048).with_max_segments(1);
    let mut audit = EffectAudit::open_with_options(owner, options).unwrap();

    let first_target = audit.file_target("safe/first");
    audit
        .append_intent(audit_intent("effect-first", first_target))
        .unwrap();
    audit
        .append_completion(EffectCompletion {
            effect_id: "effect-first".into(),
            result_code: AuditResultCode::Succeeded,
        })
        .unwrap();

    let second_target = audit.file_target("safe/second");
    assert!(matches!(
        audit.append_intent(audit_intent("effect-second", second_target)),
        Err(AuditError::RetentionLimit)
    ));
    let later_target = audit.file_target("safe/later");
    assert!(matches!(
        audit.append_intent(audit_intent("effect-later", later_target)),
        Err(AuditError::Unavailable)
    ));
}

#[test]
fn js_effect_audit_storage_required_path_and_sync_failures_are_typed() {
    for failure in [
        AuditFailurePoint::FileSync,
        AuditFailurePoint::DirectorySync,
    ] {
        let root = AuditTempRoot::new("sync-failure");
        let owner = root.owner();
        let options = AuditOpenOptions::for_test(4_096).with_failure(failure);
        assert!(matches!(
            EffectAudit::open_with_options(owner, options),
            Err(AuditError::SyncFailed)
        ));
    }

    let root = AuditTempRoot::new("bad-path");
    let owner = root.owner();
    std::fs::create_dir_all(owner.directory().parent().unwrap()).unwrap();
    std::fs::write(owner.directory(), b"not a directory").unwrap();
    assert!(matches!(
        EffectAudit::open(owner),
        Err(AuditError::PathUnavailable)
    ));

    let root = AuditTempRoot::new("poison-after-sync");
    let owner = root.owner();
    let mut audit = EffectAudit::open(owner).unwrap();
    audit.fail_next_durability_for_test(AuditFailurePoint::FileSync);
    let target = audit.file_target("safe/path");
    assert!(matches!(
        audit.append_intent(audit_intent("effect-sync-failed", target)),
        Err(AuditError::SyncFailed)
    ));
    let target = audit.file_target("safe/other");
    assert!(matches!(
        audit.append_intent(audit_intent("effect-after-failure", target)),
        Err(AuditError::Unavailable)
    ));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let root = AuditTempRoot::new("linked-component");
        let owner = root.owner();
        let outside = root.0.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(owner.state_root()).unwrap();
        symlink(&outside, owner.state_root().join("audit")).unwrap();
        assert!(matches!(
            EffectAudit::open(owner),
            Err(AuditError::PathUnavailable)
        ));
        assert!(!outside.join("js-effects").exists());
    }
}
