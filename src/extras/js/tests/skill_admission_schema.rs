use crate::extras::js::skills::store::{
    AdminIdentity, EnqueueStatus, EvaluationReportRecord, HeldOutSuiteRecord, ProposalStatus,
    SkillStore, StoreError,
};
use crate::extras::js::skills::{CapabilityManifest, SkillArtifact, SkillExport};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn paths() -> (PathBuf, AppPaths) {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let root = std::env::temp_dir().join(format!(
        "phase4_schema_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let environment = PathEnvironment {
        platform: if cfg!(target_os = "macos") {
            PathPlatform::MacOs
        } else if cfg!(target_os = "windows") {
            PathPlatform::Windows
        } else {
            PathPlatform::Linux
        },
        home_dir: None,
        config_base: Some(root.join("config")),
        data_base: Some(root.join("data")),
        local_data_base: Some(root.join("local")),
        state_base: Some(root.join("state")),
        cache_base: Some(root.join("cache")),
        workspace_root: None,
        overrides: Default::default(),
    };
    let resolved = AppPaths::resolve(&environment).expect("test paths");
    (root, resolved)
}

fn artifact(source_suffix: &str) -> SkillArtifact {
    SkillArtifact::new(
        format!("function normalize(v) {{ return String(v).trim(); }}{source_suffix}"),
        "Normalize a value.".to_string(),
        vec!["normalize".to_string()],
        vec![SkillExport {
            name: "normalize".to_string(),
            signature: "normalize(value: unknown): string".to_string(),
        }],
        vec!["normalize(' x ') === 'x'".to_string()],
        CapabilityManifest::pure(),
    )
    .expect("artifact")
}

fn report(proposal_id: &str, skill_id: &str, attempt: u32) -> EvaluationReportRecord {
    let mut report = EvaluationReportRecord {
        report_id: String::new(),
        proposal_id: proposal_id.to_string(),
        skill_id: skill_id.to_string(),
        attempt,
        verifier_version: 1,
        fakes_version: 1,
        suite_hashes: vec!["suite-hash".to_string()],
        predecessor_id: None,
        embedding_model_id: Some("deterministic-hash".to_string()),
        embedding_model_revision: Some("deterministic-v1".to_string()),
        outcome: "passed".to_string(),
        reason_code: None,
        summary_json: r#"{"embedded":"passed","held_out":"passed"}"#.to_string(),
        created_at: 12,
    };
    report.report_id = report.recompute_id().expect("report identity");
    report
}

fn seed_v2(paths: &AppPaths, generations: &str) {
    let directory = paths.local_data_dir.join("skills");
    std::fs::create_dir_all(&directory).expect("skill directory");
    let connection = Connection::open(directory.join("skills.db")).expect("legacy database");
    connection
        .execute_batch(&format!(
            r#"
            PRAGMA foreign_keys = ON;
            CREATE TABLE skill_revisions (
                id TEXT PRIMARY KEY,
                identity_version INTEGER NOT NULL,
                source TEXT NOT NULL,
                description TEXT NOT NULL,
                tags_json TEXT NOT NULL,
                exports_json TEXT NOT NULL,
                tests_json TEXT NOT NULL,
                capability_json TEXT NOT NULL,
                status TEXT NOT NULL,
                supersedes_id TEXT,
                superseded_by_id TEXT,
                row_version INTEGER NOT NULL DEFAULT 1,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE skill_embeddings (
                skill_id TEXT NOT NULL,
                model_id TEXT NOT NULL,
                model_revision TEXT NOT NULL,
                dimensions INTEGER NOT NULL,
                normalized INTEGER NOT NULL,
                embedding BLOB NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (skill_id, model_id, model_revision),
                FOREIGN KEY (skill_id) REFERENCES skill_revisions(id) ON DELETE CASCADE
            );
            CREATE VIRTUAL TABLE skill_search USING fts5(
                identifier, description, tags, exports
            );
            {generations}
            PRAGMA user_version = 2;
            "#
        ))
        .expect("legacy v2 schema");
}

#[test]
fn skill_admission_schema_migrates_and_reopens() {
    let (root, paths) = paths();
    let store = SkillStore::open_at(&paths).expect("fresh store");
    assert_eq!(store.schema_version().expect("version"), 5);
    drop(store);
    assert_eq!(
        SkillStore::open_at(&paths)
            .expect("reopen")
            .schema_version()
            .expect("version"),
        5
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_admission_schema_upgrades_phase3_v2_without_losing_generation_shape() {
    let (root, paths) = paths();
    seed_v2(
        &paths,
        r#"
        CREATE TABLE skill_tombstones (id TEXT PRIMARY KEY, purged_at INTEGER NOT NULL);
        CREATE TABLE skill_generations (
            singleton INTEGER PRIMARY KEY,
            desired_generation INTEGER NOT NULL,
            applied_generation INTEGER NOT NULL,
            model_id TEXT NOT NULL,
            model_revision TEXT NOT NULL,
            dimensions INTEGER NOT NULL,
            normalized INTEGER NOT NULL
        );
        INSERT INTO skill_generations VALUES (1, 7, 6, 'model', 'revision', 4, 1);
        "#,
    );

    let store = SkillStore::open_at(&paths).expect("upgrade Phase 3 v2");
    assert_eq!(store.schema_version().unwrap(), 5);
    let state = store.generation_state().expect("generation state");
    assert_eq!(state.desired_generation, 7);
    assert_eq!(state.applied_generation, 6);
    assert_eq!(store.count_proposals().unwrap(), 0);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_admission_schema_upgrades_legacy_phase4_v2_collision() {
    let (root, paths) = paths();
    seed_v2(
        &paths,
        r#"
        CREATE TABLE skill_generations (
            singleton INTEGER PRIMARY KEY,
            desired_generation INTEGER NOT NULL,
            row_version INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );
        INSERT INTO skill_generations VALUES (1, 3, 2, 99);
        "#,
    );

    let store = SkillStore::open_at(&paths).expect("upgrade legacy Phase 4 v2");
    assert_eq!(store.schema_version().unwrap(), 5);
    let state = store
        .generation_state()
        .expect("normalized generation state");
    assert_eq!(state.desired_generation, 3);
    assert_eq!(state.applied_generation, 0);
    assert_eq!(store.count_proposals().unwrap(), 0);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_admission_schema_quarantines_all_identity_v1_tiers_without_inference() {
    let (root, paths) = paths();
    drop(SkillStore::open_at(&paths).expect("create current schema"));
    let database = paths.local_data_dir.join("skills/skills.db");
    let connection = Connection::open(&database).expect("open fixture database");
    connection
        .execute_batch(
            r#"
            PRAGMA user_version = 4;
            INSERT INTO skill_revisions (
                id, identity_version, source, description, tags_json, exports_json,
                tests_json, capability_json, status, lineage_root_id, supersedes_id,
                row_version, created_at, updated_at
            ) VALUES
                ('1111111111111111111111111111111111111111111111111111111111111111', 1,
                 'pure-v1-source', 'pure v1', '[]', '[]', '["true"]',
                 '{"tier":"pure","allowed_hosts":[]}', 'active',
                 '1111111111111111111111111111111111111111111111111111111111111111', NULL,
                 7, 10, 10),
                ('2222222222222222222222222222222222222222222222222222222222222222', 1,
                 'read-v1-source', 'read v1', '[]', '[]', '["true"]',
                 '{"tier":"read_only","allowed_hosts":["read_file"]}', 'canary',
                 '1111111111111111111111111111111111111111111111111111111111111111',
                 '1111111111111111111111111111111111111111111111111111111111111111',
                 8, 11, 11),
                ('3333333333333333333333333333333333333333333333333333333333333333', 1,
                 'effect-v1-source', 'effect v1', '[]', '[]', '["true"]',
                 '{"tier":"side_effecting","allowed_hosts":["write_file","fetch","spawn"]}',
                 'verified',
                 '1111111111111111111111111111111111111111111111111111111111111111',
                 '2222222222222222222222222222222222222222222222222222222222222222',
                 9, 12, 12);
            "#,
        )
        .expect("seed identity-v1 fixtures");
    drop(connection);

    let mut store = SkillStore::open_at(&paths).expect("migrate identity v1");
    assert_eq!(store.schema_version().unwrap(), 5);
    assert!(store.list_retrievable().unwrap().is_empty());

    let rows = store
        .conn_mut()
        .prepare(
            "SELECT id, source, capability_json, status, quarantine_reason,
                    lineage_root_id, supersedes_id, row_version
               FROM skill_revisions WHERE identity_version = 1 ORDER BY id",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].1, "pure-v1-source");
    assert_eq!(rows[1].1, "read-v1-source");
    assert_eq!(rows[2].1, "effect-v1-source");
    assert!(rows.iter().all(|row| row.3 == "quarantined"));
    assert!(
        rows.iter()
            .all(|row| row.4.as_deref() == Some("manifest_scope_required"))
    );
    assert!(rows[1].2.contains("allowed_hosts"));
    assert_eq!(rows[1].6.as_deref(), Some(rows[0].0.as_str()));
    assert_eq!(rows[2].6.as_deref(), Some(rows[1].0.as_str()));
    assert_eq!(
        rows.iter().map(|row| row.7).collect::<Vec<_>>(),
        vec![8, 9, 10]
    );
    assert_eq!(
        store
            .conn_mut()
            .query_row("SELECT COUNT(*) FROM skill_search", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_admission_schema_enqueue_is_idempotent_and_does_not_duplicate_bytes() {
    let (root, paths) = paths();
    let mut store = SkillStore::open_at(&paths).expect("store");
    let artifact = artifact("");

    let first = store
        .enqueue_proposal(&artifact, None, 10)
        .expect("first enqueue");
    let second = store
        .enqueue_proposal(&artifact, None, 11)
        .expect("duplicate enqueue");

    assert_eq!(first.status, EnqueueStatus::Pending);
    assert_eq!(second.proposal_id, first.proposal_id);
    assert_eq!(second.status, EnqueueStatus::Pending);
    assert_eq!(store.count_revisions().expect("revisions"), 1);
    assert_eq!(store.count_proposals().expect("proposals"), 1);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_admission_schema_cannot_repropose_a_privacy_purged_identity() {
    let (root, paths) = paths();
    let mut store = SkillStore::open_at(&paths).expect("store");
    let artifact = artifact("");
    store.insert_verified(&artifact).expect("verified artifact");
    store.purge(&artifact.id).expect("privacy purge");

    assert!(matches!(
        store.enqueue_proposal(&artifact, None, 10),
        Err(StoreError::Purged(id)) if id == artifact.id
    ));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_admission_schema_idempotence_cannot_rebind_predecessor() {
    let (root, paths) = paths();
    let mut store = SkillStore::open_at(&paths).expect("store");
    let predecessor_a = artifact("/* predecessor-a */");
    let predecessor_b = artifact("/* predecessor-b */");
    let candidate = artifact("/* candidate */");
    store
        .insert_verified(&predecessor_a)
        .expect("predecessor a");
    store
        .insert_verified(&predecessor_b)
        .expect("predecessor b");
    store
        .enqueue_proposal(&candidate, Some(&predecessor_a.id), 10)
        .expect("first enqueue");

    assert!(matches!(
        store.enqueue_proposal(&candidate, Some(&predecessor_b.id), 11),
        Err(StoreError::Constraint(message)) if message.contains("different predecessor")
    ));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_admission_leases_reclaim_expired_work_and_reject_stale_owner() {
    let (root, paths) = paths();
    let mut store = SkillStore::open_at(&paths).expect("store");
    let artifact = artifact("");
    let enqueued = store
        .enqueue_proposal(&artifact, None, 10)
        .expect("enqueue");

    let first = store
        .claim_due_proposal("worker-a", 20, 5)
        .expect("claim")
        .expect("work");
    assert_eq!(first.attempt, 1);
    assert!(
        store
            .claim_due_proposal("worker-b", 24, 5)
            .unwrap()
            .is_none()
    );

    let reclaimed = store
        .claim_due_proposal("worker-b", 25, 5)
        .expect("reclaim")
        .expect("expired work");
    assert_eq!(reclaimed.proposal_id, enqueued.proposal_id);
    assert_eq!(reclaimed.attempt, 2);
    assert!(matches!(
        store.renew_lease(&enqueued.proposal_id, "worker-a", 26, 5),
        Err(StoreError::LeaseLost(_))
    ));
    store
        .renew_lease(&enqueued.proposal_id, "worker-b", 26, 5)
        .expect("current owner renews");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_admission_leases_terminal_rejection_is_atomic_and_idempotent() {
    let (root, paths) = paths();
    let mut store = SkillStore::open_at(&paths).expect("store");
    let artifact = artifact("");
    let enqueued = store
        .enqueue_proposal(&artifact, None, 10)
        .expect("enqueue");
    let lease = store.claim_due_proposal("worker", 20, 5).unwrap().unwrap();
    let mut failed = EvaluationReportRecord {
        outcome: "rejected".to_string(),
        reason_code: Some("embedded_test_failed".to_string()),
        embedding_model_id: None,
        embedding_model_revision: None,
        ..report(&lease.proposal_id, &lease.skill_id, lease.attempt)
    };
    failed.report_id = failed.recompute_id().expect("rejection identity");

    store
        .reject_proposal(
            &lease.proposal_id,
            "worker",
            lease.row_version,
            &failed,
            "embedded_test_failed",
            22,
        )
        .expect("reject");
    store
        .reject_proposal(
            &lease.proposal_id,
            "worker",
            lease.row_version,
            &failed,
            "embedded_test_failed",
            23,
        )
        .expect("idempotent reject");

    let proposal = store.get_proposal(&enqueued.proposal_id).unwrap().unwrap();
    assert_eq!(proposal.status, ProposalStatus::Rejected);
    assert_eq!(
        store.revision_status(&artifact.id).expect("revision"),
        Some("rejected".to_string())
    );
    assert_eq!(
        store
            .enqueue_proposal(&artifact, None, 30)
            .expect("reproposal")
            .status,
        EnqueueStatus::Rejected
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn held_out_suite_schema_requires_authenticated_import_and_hashes_content() {
    let (root, paths) = paths();
    let mut store = SkillStore::open_at(&paths).expect("store");
    let suite_payload = r#"{"version":1,"selector":{"tags":["normalize"]},"cases":[{"expression":"normalize(' y ')", "expected":"y"}]}"#;
    let expected_hash = format!("{:x}", Sha256::digest(suite_payload.as_bytes()));
    let suite = HeldOutSuiteRecord {
        suite_id: expected_hash.clone(),
        selector_json: r#"{"tags":["normalize"]}"#.to_string(),
        cases_json: r#"[{"expression":"normalize(' y ')", "expected":"y"}]"#.to_string(),
        content_hash: expected_hash.clone(),
        canonical_payload: suite_payload.to_string(),
        approved_by: String::new(),
        approved_at: 0,
        enabled: true,
    };

    assert!(matches!(
        store.import_held_out_suite(None, &suite, 10),
        Err(StoreError::Unauthorized)
    ));
    let admin = AdminIdentity::authenticated("reviewer-1").expect("admin");
    store
        .import_held_out_suite(Some(&admin), &suite, 10)
        .expect("trusted import");
    let selected = store.enabled_held_out_suites().expect("trusted selection");
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].content_hash, expected_hash);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_admission_schema_awaiting_approval_requires_bound_report() {
    let (root, paths) = paths();
    let mut store = SkillStore::open_at(&paths).expect("store");
    let artifact = artifact("");
    let enqueued = store
        .enqueue_proposal(&artifact, None, 10)
        .expect("enqueue");
    let lease = store.claim_due_proposal("worker", 20, 5).unwrap().unwrap();
    let evaluation = report(&lease.proposal_id, &lease.skill_id, lease.attempt);

    store
        .complete_evaluation(
            &enqueued.proposal_id,
            "worker",
            lease.row_version,
            &evaluation,
            22,
        )
        .expect("complete");
    let proposal = store.get_proposal(&enqueued.proposal_id).unwrap().unwrap();
    assert_eq!(proposal.status, ProposalStatus::AwaitingApproval);
    assert_eq!(
        proposal.report_id.as_deref(),
        Some(evaluation.report_id.as_str())
    );
    assert_eq!(
        store.revision_status(&artifact.id).unwrap(),
        Some("verified".to_string())
    );
    let _ = std::fs::remove_dir_all(root);
}
