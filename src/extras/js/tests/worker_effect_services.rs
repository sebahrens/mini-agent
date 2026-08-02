use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::extras::js::audit::EffectAudit;
use crate::extras::js::broker::{
    GrantPrincipal, HostCapability, InvocationBroker, InvocationGrant,
};
use crate::extras::js::host::{
    AllowConfig, FileEffectService, ParentHostEffectService, SpawnEffectService,
};
use crate::extras::js::protocol::{
    AdvisoryAttribution, EffectOperation, EffectRequest, EffectResult, InvocationId,
};
use crate::extras::js::tool::PermissionBridgeOwner;
use crate::extras::js::types::{
    EffectServiceError, PermCancellation, WRITE_FILE_MAX_BYTES, canonical_spawn_permission_subject,
};
use crate::paths::AppPaths;
use crate::permission::checker::{PermCheck, PermissionChecker};
use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};
use crate::sandbox::Sandbox;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("mini-agent-effects-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create effect-service test directory");
        Self(path)
    }

    fn audit(&self, tag: &str) -> EffectAudit {
        let root = self.0.join(format!("audit-{tag}"));
        EffectAudit::open(
            AppPaths {
                config_dir: root.join("config"),
                data_dir: root.join("data"),
                local_data_dir: root.join("local"),
                state_dir: root.join("state"),
                cache_dir: root.join("cache"),
                credentials_dir: root.join("credentials"),
                project_dir: None,
            }
            .effect_audit(),
        )
        .unwrap()
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn permission(base: PathBuf, action: Action) -> PermCheck {
    let config = PermissionConfig {
        bash: Some(ToolPerm::Simple(action)),
        read: Some(ToolPerm::Simple(action)),
        write: Some(ToolPerm::Simple(action)),
        doom_loop: Some(Action::Allow),
        ..PermissionConfig::default()
    };
    Arc::new(Mutex::new(PermissionChecker::new(
        &PermissionConfigs::from(config),
        SecurityMode::Standard,
        Some(base),
        Some(vec!["standard".to_string()]),
    )))
}

fn granular_permission(
    base: PathBuf,
    mode: SecurityMode,
    rules: HashMap<String, Action>,
) -> PermCheck {
    let config = PermissionConfig {
        bash: Some(ToolPerm::Granular(rules)),
        doom_loop: Some(Action::Allow),
        ..PermissionConfig::default()
    };
    Arc::new(Mutex::new(PermissionChecker::new(
        &PermissionConfigs::from(config),
        mode,
        Some(base),
        Some(vec![mode.to_string()]),
    )))
}

#[test]
fn worker_effect_services_spawn_subject_preserves_argument_boundaries() {
    let one_argument = canonical_spawn_permission_subject("tool", &["a b".to_string()]).unwrap();
    let two_arguments =
        canonical_spawn_permission_subject("tool", &["a".to_string(), "b".to_string()]).unwrap();

    assert_ne!(one_argument, two_arguments);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&one_argument).unwrap(),
        serde_json::json!({"version": 1, "program": "tool", "arguments": ["a b"]})
    );
}

#[tokio::test]
async fn worker_effect_services_spawn_policy_preserves_shape_and_yolo_deny() {
    let directory = TestDirectory::new();
    let exact_owner = PermissionBridgeOwner::new(
        Some(granular_permission(
            directory.0.clone(),
            SecurityMode::Standard,
            HashMap::from([("printf %s a b".to_string(), Action::Allow)]),
        )),
        None,
        Duration::from_millis(100),
    );
    let exact = SpawnEffectService::new(
        Sandbox::new(false, "bwrap"),
        exact_owner.bridge(),
        Duration::from_secs(1),
    );
    assert!(
        exact
            .execute(
                "printf",
                &["%s".to_string(), "a".to_string(), "b".to_string()],
                PermCancellation::new(),
            )
            .await
            .is_ok()
    );
    assert!(matches!(
        exact
            .execute(
                "printf",
                &["%s".to_string(), "a b".to_string()],
                PermCancellation::new(),
            )
            .await,
        Err(EffectServiceError::PermissionDenied)
    ));

    let target = directory.0.join("must-not-exist");
    let deny_owner = PermissionBridgeOwner::new(
        Some(granular_permission(
            directory.0.clone(),
            SecurityMode::Yolo,
            HashMap::from([("touch **".to_string(), Action::Deny)]),
        )),
        None,
        Duration::from_millis(100),
    );
    let denied = SpawnEffectService::new(
        Sandbox::new(false, "bwrap"),
        deny_owner.bridge(),
        Duration::from_secs(1),
    );
    assert!(matches!(
        denied
            .execute(
                "touch",
                &[target.to_string_lossy().into_owned()],
                PermCancellation::new(),
            )
            .await,
        Err(EffectServiceError::PermissionDenied)
    ));
    assert!(!target.exists());
}

#[tokio::test]
async fn worker_effect_services_real_broker_prepares_then_executes_exact_target() {
    let directory = TestDirectory::new();
    let target = directory.0.join("broker.txt");
    std::fs::write(&target, "brokered").unwrap();
    let owner = PermissionBridgeOwner::new(None, None, Duration::from_millis(100));
    let service = ParentHostEffectService::new(
        FileEffectService::new(
            owner.bridge(),
            AllowConfig::unrestricted(&directory.0),
            Duration::from_secs(1),
        ),
        SpawnEffectService::new(
            Sandbox::new(false, "bwrap"),
            owner.bridge(),
            Duration::from_secs(1),
        ),
    );
    let invocation = InvocationId::new("effect-services-invocation").unwrap();
    let grant = InvocationGrant::issue(
        invocation.clone(),
        GrantPrincipal::ModelAuthored {
            tool_call_id: "call-1".to_string(),
        },
        BTreeSet::from([HostCapability::ReadFile]),
        Instant::now() + Duration::from_secs(10),
    );
    let request = EffectRequest {
        effect_ordinal: 1,
        grant_id: grant.grant_id().clone(),
        advisory: AdvisoryAttribution::default(),
        operation: EffectOperation::ReadFile {
            path: target.to_string_lossy().into_owned(),
        },
    };
    let mut broker = InvocationBroker::new(
        invocation,
        vec![grant],
        BTreeSet::from([HostCapability::ReadFile]),
        service,
        Arc::new(Mutex::new(directory.audit("read"))),
    )
    .unwrap();

    assert_eq!(
        broker.dispatch(request, PermCancellation::new()).await,
        Ok(EffectResult::ReadFile {
            content: "brokered".to_string(),
        })
    );

    let service = ParentHostEffectService::new(
        FileEffectService::new(
            owner.bridge(),
            AllowConfig::unrestricted(&directory.0),
            Duration::from_secs(1),
        ),
        SpawnEffectService::new(
            Sandbox::new(false, "bwrap"),
            owner.bridge(),
            Duration::from_secs(1),
        ),
    );
    let invocation = InvocationId::new("effect-services-write-limit").unwrap();
    let grant = InvocationGrant::issue(
        invocation.clone(),
        GrantPrincipal::ModelAuthored {
            tool_call_id: "call-2".to_string(),
        },
        BTreeSet::from([HostCapability::WriteFile]),
        Instant::now() + Duration::from_secs(10),
    );
    let request = EffectRequest {
        effect_ordinal: 1,
        grant_id: grant.grant_id().clone(),
        advisory: AdvisoryAttribution::default(),
        operation: EffectOperation::WriteFile {
            path: directory.0.join("too-large").to_string_lossy().into_owned(),
            content: "x".repeat(WRITE_FILE_MAX_BYTES + 1),
        },
    };
    let mut broker = InvocationBroker::new(
        invocation,
        vec![grant],
        BTreeSet::from([HostCapability::WriteFile]),
        service,
        Arc::new(Mutex::new(directory.audit("write"))),
    )
    .unwrap();
    assert_eq!(
        broker.dispatch(request, PermCancellation::new()).await,
        Err(crate::extras::js::broker::HostEffectError::OutputLimit)
    );
}

#[tokio::test]
async fn worker_effect_services_broker_rejects_unavailable_spawn_before_prompt() {
    let directory = TestDirectory::new();
    let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
    let owner = PermissionBridgeOwner::new(
        Some(permission(directory.0.clone(), Action::Ask)),
        Some(ask_tx),
        Duration::from_millis(100),
    );
    let unavailable_spawn = SpawnEffectService::new(
        Sandbox::new(true, "definitely-unavailable"),
        owner.bridge(),
        Duration::from_secs(1),
    );
    assert!(matches!(
        unavailable_spawn
            .execute("printf", &[], PermCancellation::new())
            .await,
        Err(EffectServiceError::BackendFailure)
    ));
    assert!(ask_rx.try_recv().is_err());
    let service = ParentHostEffectService::new(
        FileEffectService::new(
            owner.bridge(),
            AllowConfig::unrestricted(&directory.0),
            Duration::from_secs(1),
        ),
        unavailable_spawn,
    );
    let invocation = InvocationId::new("effect-services-backend").unwrap();
    let grant = InvocationGrant::issue(
        invocation.clone(),
        GrantPrincipal::ModelAuthored {
            tool_call_id: "call-backend".to_string(),
        },
        BTreeSet::from([HostCapability::Spawn]),
        Instant::now() + Duration::from_secs(10),
    );
    let request = EffectRequest {
        effect_ordinal: 1,
        grant_id: grant.grant_id().clone(),
        advisory: AdvisoryAttribution::default(),
        operation: EffectOperation::Spawn {
            program: "printf".to_string(),
            arguments: Vec::new(),
        },
    };
    let mut broker = InvocationBroker::new(
        invocation,
        vec![grant],
        BTreeSet::from([HostCapability::Spawn]),
        service,
        Arc::new(Mutex::new(directory.audit("spawn-backend"))),
    )
    .unwrap();
    assert_eq!(
        broker.dispatch(request, PermCancellation::new()).await,
        Err(crate::extras::js::broker::HostEffectError::BackendFailure)
    );
    assert!(ask_rx.try_recv().is_err());
}

#[tokio::test]
async fn worker_effect_services_file_errors_do_not_poison_next_call() {
    let directory = TestDirectory::new();
    let owner = PermissionBridgeOwner::new(None, None, Duration::from_millis(100));
    let service = FileEffectService::new(
        owner.bridge(),
        AllowConfig::unrestricted(&directory.0),
        Duration::from_secs(1),
    );

    assert_eq!(
        service
            .read(
                directory.0.join("missing").to_str().unwrap(),
                PermCancellation::new(),
            )
            .await,
        Err(EffectServiceError::InvalidTarget)
    );
    assert_eq!(
        service
            .write(
                directory.0.join("too-large").to_str().unwrap(),
                "x".repeat(WRITE_FILE_MAX_BYTES + 1),
                PermCancellation::new(),
            )
            .await,
        Err(EffectServiceError::BodyLimit)
    );

    let target = directory.0.join("ok.txt");
    service
        .write(
            target.to_str().unwrap(),
            "bounded".to_string(),
            PermCancellation::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        service
            .read(target.to_str().unwrap(), PermCancellation::new())
            .await
            .unwrap(),
        "bounded"
    );
}

#[tokio::test]
async fn worker_effect_services_permission_deny_timeout_and_cancellation_are_closed() {
    let directory = TestDirectory::new();
    let target = directory.0.join("input.txt");
    std::fs::write(&target, "secret").unwrap();

    let denied_owner = PermissionBridgeOwner::new(
        Some(permission(directory.0.clone(), Action::Deny)),
        None,
        Duration::from_millis(100),
    );
    let denied = FileEffectService::new(
        denied_owner.bridge(),
        AllowConfig::unrestricted(&directory.0),
        Duration::from_secs(1),
    );
    assert_eq!(
        denied
            .read(target.to_str().unwrap(), PermCancellation::new())
            .await,
        Err(EffectServiceError::PermissionDenied)
    );

    let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
    let ask_owner = PermissionBridgeOwner::new(
        Some(permission(directory.0.clone(), Action::Ask)),
        Some(ask_tx),
        Duration::from_millis(20),
    );
    let ask = FileEffectService::new(
        ask_owner.bridge(),
        AllowConfig::unrestricted(&directory.0),
        Duration::from_secs(1),
    );
    let pending = ask.read(target.to_str().unwrap(), PermCancellation::new());
    let request = tokio::spawn(async move { ask_rx.recv().await });
    assert_eq!(pending.await, Err(EffectServiceError::PermissionTimedOut));
    drop(request.await.unwrap());

    let cancellation = PermCancellation::new();
    cancellation.cancel();
    assert_eq!(
        denied.read(target.to_str().unwrap(), cancellation).await,
        Err(EffectServiceError::Cancelled)
    );
}

#[tokio::test]
async fn worker_effect_services_spawn_target_failure_then_success() {
    let owner = PermissionBridgeOwner::new(None, None, Duration::from_millis(100));
    let service = SpawnEffectService::new(
        Sandbox::new(false, "bwrap"),
        owner.bridge(),
        Duration::from_secs(1),
    );

    assert!(matches!(
        service
            .execute(
                "/definitely/missing/mini-agent-command",
                &[],
                PermCancellation::new(),
            )
            .await,
        Err(EffectServiceError::InvalidTarget)
    ));
    let result = service
        .execute(
            "printf",
            &["%s".to_string(), "a b".to_string()],
            PermCancellation::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.stdout, "a b");
    assert_eq!(result.code, 0);

    let cancellation = PermCancellation::new();
    cancellation.cancel();
    assert!(matches!(
        service.execute("printf", &[], cancellation).await,
        Err(EffectServiceError::Cancelled)
    ));

    let (ask_tx, _ask_rx) = tokio::sync::mpsc::channel(1);
    let ask_owner = PermissionBridgeOwner::new(
        Some(permission(std::env::current_dir().unwrap(), Action::Ask)),
        Some(ask_tx),
        Duration::from_millis(20),
    );
    let ask_service = SpawnEffectService::new(
        Sandbox::new(false, "bwrap"),
        ask_owner.bridge(),
        Duration::from_secs(1),
    );
    assert!(matches!(
        ask_service
            .execute("printf", &[], PermCancellation::new())
            .await,
        Err(EffectServiceError::PermissionTimedOut)
    ));

    let limited = service
        .execute("yes", &[], PermCancellation::new())
        .await
        .expect("bounded output result");
    assert!(limited.stdout_truncated || limited.stderr_truncated);
    assert_eq!(
        service
            .execute(
                "printf",
                &["recovered".to_string()],
                PermCancellation::new(),
            )
            .await
            .unwrap()
            .stdout,
        "recovered"
    );
}
