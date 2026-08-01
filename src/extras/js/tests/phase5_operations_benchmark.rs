use std::collections::BTreeMap;
use std::time::Instant;

use crate::extras::js::skills::CapabilityTier;
use crate::extras::js::skills::lifecycle::LifecycleStatus;
use crate::extras::js::skills::policy::{
    DirectOutcome, InvocationEvidence, PromotionContext, PromotionPolicy, evaluate_promotion,
};
use crate::extras::js::skills::retention::RetentionService;
use crate::extras::js::skills::router::{CanaryCandidate, RouteRequest, route};
use crate::extras::js::skills::store::SkillStore;
use crate::extras::js::skills::telemetry::{
    EventBatch, SkillEvent, SkillEventKind, TelemetryIngestor,
};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};

fn paths(root: &std::path::Path) -> AppPaths {
    AppPaths::resolve(&PathEnvironment {
        platform: if cfg!(target_os = "macos") {
            PathPlatform::MacOs
        } else if cfg!(target_os = "windows") {
            PathPlatform::Windows
        } else {
            PathPlatform::Linux
        },
        home_dir: None,
        config_base: Some(root.to_path_buf()),
        data_base: Some(root.to_path_buf()),
        local_data_base: Some(root.to_path_buf()),
        state_base: Some(root.to_path_buf()),
        cache_base: Some(root.to_path_buf()),
        workspace_root: None,
        overrides: Default::default(),
    })
    .unwrap()
}

fn evidence(skill_id: &str, prefix: &str) -> Vec<InvocationEvidence> {
    (0..25)
        .map(|index| InvocationEvidence {
            invocation_id: format!("{prefix}-invocation-{index}"),
            skill_id: skill_id.to_string(),
            turn_id: format!("{prefix}-turn-{index}"),
            outcome: DirectOutcome::Success,
            latency_us: 100,
            production: true,
            observability_complete: true,
            created_at: 100,
        })
        .collect()
}

#[test]
#[ignore = "set ZS_SKILL_BENCH_FULL=1 to run the 100,000-revision Phase 5 audit"]
fn phase5_operations_benchmark() {
    assert_eq!(std::env::var("ZS_SKILL_BENCH_FULL").as_deref(), Ok("1"));
    let root = std::env::temp_dir().join(format!("phase5-benchmark-{}", uuid::Uuid::new_v4()));
    let mut store = SkillStore::open_at(&paths(&root)).unwrap();
    let seed_started = Instant::now();
    {
        let tx = store.conn_mut().transaction().unwrap();
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO skill_revisions (
                        id, identity_version, source, description, tags_json,
                        exports_json, tests_json, capability_json, status,
                        lineage_root_id, row_version, created_at, updated_at
                     ) VALUES (?, 1, '', 'retained', '[]', '[]', '[]',
                               '{\"tier\":\"pure\",\"allowed_hosts\":[]}',
                               'retired', ?, 1, 0, 0)",
                )
                .unwrap();
            for index in 0..100_000u64 {
                let id = format!("{index:064x}");
                insert.execute(rusqlite::params![id, id]).unwrap();
            }
        }
        tx.commit().unwrap();
    }
    let seed_us = seed_started.elapsed().as_micros();
    let skill_id = format!("{:064x}", 1u64);

    let route_started = Instant::now();
    for index in 0..100_000u64 {
        let request = RouteRequest {
            active_id: "active".into(),
            active_lineage_root_id: "root".into(),
            turn_id: format!("turn-{index}"),
            policy_version: "phase5-v1".into(),
            canary_share_basis_points: 1_000,
            retrieval_score: 0.8,
            retrieval_rank: 0,
            index_generation: 1,
            candidate: Some(CanaryCandidate {
                candidate_id: "candidate".into(),
                lineage_root_id: "root".into(),
                status: LifecycleStatus::Canary,
                model_compatible: true,
                identity_valid: true,
                capability_tier: CapabilityTier::Pure,
                explicitly_idempotent: true,
            }),
        };
        route(b"phase5-benchmark-key", &request).unwrap();
    }
    let routing_us = route_started.elapsed().as_micros();

    let events = (0..256)
        .map(|index| SkillEvent {
            invocation_id: None,
            skill_id: skill_id.clone(),
            turn_id: format!("event-turn-{index}"),
            tool_call_id: None,
            kind: SkillEventKind::Selected,
            export_name: None,
            outcome: None,
            latency_us: None,
            retrieval_score: Some(0.8),
            retrieval_rank: Some(0),
            query_fingerprint: None,
            index_generation: 1,
            evidence_complete: true,
            production: true,
            argument_shape: None,
            created_at: 100,
        })
        .collect();
    let batch = EventBatch::new(events).unwrap();
    let ingest_started = Instant::now();
    TelemetryIngestor::new(&mut store).ingest(&batch).unwrap();
    let ingestion_us = ingest_started.elapsed().as_micros();

    let policy = PromotionPolicy::conservative("phase5-v1", 0, 200);
    let candidate_id = format!("{:064x}", 2u64);
    let candidate = evidence(&candidate_id, "candidate");
    let predecessor = evidence(&skill_id, "predecessor");
    let context = PromotionContext {
        candidate_id,
        predecessor_id: Some(skill_id.clone()),
        capability_tier: CapabilityTier::Pure,
        capability_increased: false,
        inherited_tests_passed: true,
        held_out_tests_passed: true,
        unresolved_negative_feedback: false,
        identity_valid: true,
        row_version_current: true,
        generation_current: true,
    };
    let policy_started = Instant::now();
    for _ in 0..10_000 {
        evaluate_promotion(&policy, &context, &candidate, &predecessor).unwrap();
    }
    let policy_us = policy_started.elapsed().as_micros();

    let compaction_started = Instant::now();
    RetentionService::new(&mut store)
        .compact_before(200, 1, 300)
        .unwrap();
    let compaction_us = compaction_started.elapsed().as_micros();
    let refresh_started = Instant::now();
    store.generation_state().unwrap();
    let generation_state_us = refresh_started.elapsed().as_micros();
    let purge_id = format!("{:064x}", 99_999u64);
    let purge_started = Instant::now();
    RetentionService::new(&mut store)
        .privacy_purge(&purge_id, "benchmark", 400)
        .unwrap();
    let purge_us = purge_started.elapsed().as_micros();

    let metrics = BTreeMap::from([
        ("compaction_256_us", compaction_us),
        ("generation_state_us", generation_state_us),
        ("ingestion_256_us", ingestion_us),
        ("policy_10000_us", policy_us),
        ("privacy_purge_us", purge_us),
        ("routing_100000_us", routing_us),
        ("seed_100000_us", seed_us),
    ]);
    println!("{}", serde_json::to_string(&metrics).unwrap());
    assert!(routing_us <= 2_000_000);
    assert!(ingestion_us <= 500_000);
    assert!(policy_us <= 5_000_000);
    assert!(compaction_us <= 1_000_000);
    assert!(generation_state_us <= 100_000);
    assert!(purge_us <= 1_000_000);
    let _ = std::fs::remove_dir_all(root);
}
