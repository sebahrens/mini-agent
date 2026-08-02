use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::extras::js::broker::{
    AuthorizedEffect, EffectOperation, EffectResult, GrantPrincipal, HostCapability,
    HostEffectError, InvocationBroker, InvocationGrant, ParentEffectService,
};
use crate::extras::js::protocol::{
    AdvisoryAttribution, EffectErrorCode, EffectRequest, HttpHeader, HttpMethod, InvocationId,
    SkillProposalDraft,
};
use crate::extras::js::supervisor::InvocationEffectHandler;
use crate::extras::js::types::PermCancellation;

type ServiceFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, Default)]
struct ServiceFailures {
    target: Option<HostEffectError>,
    backend: Option<HostEffectError>,
    permission: Option<HostEffectError>,
}

#[derive(Clone, Debug, Default)]
struct ServiceRecord {
    executions: usize,
    authorized: Vec<AuthorizedEffect>,
}

struct RecordingService {
    failures: ServiceFailures,
    pending_permission: bool,
    record: Arc<Mutex<ServiceRecord>>,
}

impl ParentEffectService for RecordingService {
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

    fn authorize<'a>(
        &'a mut self,
        _authorized: &'a AuthorizedEffect,
        _operation: &'a EffectOperation,
        _cancellation: PermCancellation,
    ) -> ServiceFuture<'a, Result<(), HostEffectError>> {
        let result = self.failures.permission.map_or(Ok(()), Err);
        if self.pending_permission {
            Box::pin(std::future::pending())
        } else {
            Box::pin(async move { result })
        }
    }

    fn execute<'a>(
        &'a mut self,
        authorized: &'a AuthorizedEffect,
        operation: &'a EffectOperation,
        _cancellation: PermCancellation,
    ) -> ServiceFuture<'a, Result<EffectResult, HostEffectError>> {
        let result = success_for(operation);
        let authorized = authorized.clone();
        let record = Arc::clone(&self.record);
        Box::pin(async move {
            let mut record = record.lock().unwrap();
            record.executions += 1;
            record.authorized.push(authorized);
            Ok(result)
        })
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
                    exports: vec!["run".into()],
                    tests: vec!["run() === true".into()],
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
        EffectOperation::ProposeSkill { .. } => EffectResult::ProposalAccepted,
    }
}

fn grant(
    case: &OperationCase,
    invocation_id: &InvocationId,
    expires_at: Instant,
) -> InvocationGrant {
    InvocationGrant::issue(
        invocation_id.clone(),
        case.principal.clone(),
        BTreeSet::from([case.capability]),
        expires_at,
    )
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
) {
    let record = Arc::new(Mutex::new(ServiceRecord::default()));
    let service = RecordingService {
        failures,
        pending_permission: false,
        record: Arc::clone(&record),
    };
    (
        InvocationBroker::new(invocation_id, grants, session_allowed, service).unwrap(),
        record,
    )
}

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
    let (mut broker, record) = broker(broker_invocation, vec![grant], session_allowed, failures);

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

    let (mut broker, record) = broker(
        invocation_id.clone(),
        grants.clone(),
        HostCapability::all(),
        ServiceFailures::default(),
    );

    for (case, grant) in cases.iter().zip(&grants) {
        assert_eq!(
            broker
                .dispatch(request(case, grant), PermCancellation::new())
                .await,
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

        let (mut replay_broker, record) = broker(
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
        let (mut expired_broker, record) = broker(
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
        let (mut broker, record) = broker(
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
async fn worker_broker_grants_callback_returns_only_closed_wire_errors() {
    let invocation_id = invocation("inv-callback");
    let case = operation_cases(&invocation_id).remove(0);
    let grant = grant(
        &case,
        &invocation_id,
        Instant::now() + Duration::from_secs(30),
    );
    let effect = request(&case, &grant);
    let (mut broker, record) = broker(
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
async fn worker_broker_grants_cancel_a_pending_ask_before_execution() {
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
        pending_permission: true,
        record: Arc::clone(&record),
    };
    let mut broker =
        InvocationBroker::new(invocation_id, vec![grant], HostCapability::all(), service).unwrap();
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
    assert_eq!(record.lock().unwrap().executions, 0);
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
