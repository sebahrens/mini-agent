use crate::extras::js::skills::scheduler::{PolicyScheduler, SchedulerError};
use crate::extras::js::skills::{
    CapabilityManifest, SkillArtifact, SkillExport, store::SkillStore,
};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};

#[test]
fn decision_leases_are_restart_safe_and_stale_workers_cannot_complete() {
    let root = std::env::temp_dir().join(format!("scheduler-{}", uuid::Uuid::new_v4()));
    let env = PathEnvironment {
        platform: if cfg!(target_os = "macos") {
            PathPlatform::MacOs
        } else if cfg!(target_os = "windows") {
            PathPlatform::Windows
        } else {
            PathPlatform::Linux
        },
        home_dir: None,
        config_base: Some(root.clone()),
        data_base: Some(root.clone()),
        local_data_base: Some(root.clone()),
        state_base: Some(root.clone()),
        cache_base: Some(root.clone()),
        workspace_root: None,
        overrides: Default::default(),
    };
    let mut store = SkillStore::open_at(&AppPaths::resolve(&env).unwrap()).unwrap();
    let skill = SkillArtifact::new(
        "function run() { return true; }".into(),
        "Scheduler fixture".into(),
        vec![],
        vec![SkillExport {
            name: "run".into(),
            signature: "() => bool".into(),
        }],
        vec!["run()".into()],
        CapabilityManifest::pure(),
    )
    .unwrap();
    store.insert_verified(&skill).unwrap();
    store
        .conn_mut()
        .execute(
            "INSERT INTO skill_policy_versions VALUES ('v1', '{}', 0)",
            [],
        )
        .unwrap();
    let mut scheduler = PolicyScheduler::new(&mut store);
    scheduler.enqueue("decision", &skill.id, "v1", 10).unwrap();
    scheduler.enqueue("decision", &skill.id, "v1", 10).unwrap();
    assert!(matches!(
        scheduler.enqueue("decision", &skill.id, "v1", 11),
        Err(SchedulerError::InvalidLease)
    ));
    let first = scheduler.lease_due("worker-a", 10, 5).unwrap().unwrap();
    assert_eq!(first.attempts, 1);
    assert!(scheduler.lease_due("worker-b", 12, 5).unwrap().is_none());
    let second = scheduler.lease_due("worker-b", 15, 5).unwrap().unwrap();
    assert_eq!(second.attempts, 2);
    assert!(matches!(
        scheduler.complete("decision", "worker-a", 16),
        Err(SchedulerError::StaleLease)
    ));
    scheduler.complete("decision", "worker-b", 16).unwrap();
    assert!(scheduler.lease_due("worker-c", 30, 5).unwrap().is_none());
    std::fs::remove_dir_all(root).unwrap();
}
