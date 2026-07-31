use crate::extras::js::skills::held_out::{
    ExpectedJsValue, HeldOutCase, HeldOutError, HeldOutSelector, HeldOutSuiteDraft,
    TranscriptExpectation, evaluate, select_suites,
};
use crate::extras::js::skills::store::{AdminIdentity, SkillStore};
use crate::extras::js::skills::{
    CapabilityManifest, CapabilityTier, HostCapability, SkillArtifact, SkillExport,
};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn paths() -> (PathBuf, AppPaths) {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let root = std::env::temp_dir().join(format!(
        "held_out_{}_{}",
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
    (root, AppPaths::resolve(&environment).expect("paths"))
}

fn pure_artifact() -> SkillArtifact {
    SkillArtifact::new(
        "function normalize(v) { return String(v).trim(); }".to_string(),
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

fn pure_suite(expected: &str) -> HeldOutSuiteDraft {
    HeldOutSuiteDraft {
        selector: HeldOutSelector {
            tags: vec!["normalize".to_string()],
            exports: vec!["normalize".to_string()],
            capability_tier: Some("pure".to_string()),
        },
        cases: vec![HeldOutCase {
            expression: "normalize('\\tvalue\\n')".to_string(),
            expected: ExpectedJsValue::String(expected.to_string()),
            fake_files: BTreeMap::new(),
            transcript: TranscriptExpectation::default(),
        }],
    }
}

#[test]
fn skill_held_out_evaluator_import_selection_and_report_are_reproducible() {
    let (root, paths) = paths();
    let mut store = SkillStore::open_at(&paths).expect("store");
    let admin = AdminIdentity::authenticated("reviewer").expect("admin");
    let first_id = pure_suite("value")
        .import(&mut store, &admin, 10)
        .expect("import");
    let second_id = pure_suite("value")
        .import(&mut store, &admin, 11)
        .expect("idempotent import");
    assert_eq!(first_id, second_id);

    let artifact = pure_artifact();
    let selected = select_suites(&store, &artifact).expect("selection");
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].id, first_id);

    let first = evaluate(&store, &artifact, None).expect("evaluate");
    let second = evaluate(&store, &artifact, None).expect("repeat");
    assert_eq!(first, second);
    assert_eq!(first.suite_hashes, vec![first_id]);
    let serialized = serde_json::to_string(&first).expect("report");
    assert!(!serialized.contains("normalize('"));
    assert!(!serialized.contains("value"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_held_out_evaluator_missing_or_failing_suite_blocks_admission() {
    let (root, paths) = paths();
    let mut store = SkillStore::open_at(&paths).expect("store");
    let artifact = pure_artifact();
    assert!(matches!(
        evaluate(&store, &artifact, None),
        Err(HeldOutError::SuiteRequired)
    ));

    let admin = AdminIdentity::authenticated("reviewer").unwrap();
    pure_suite("wrong")
        .import(&mut store, &admin, 10)
        .expect("import");
    assert!(matches!(
        evaluate(&store, &artifact, None),
        Err(HeldOutError::CaseFailed { .. })
    ));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_no_effect_fakes_use_hidden_virtual_data_and_match_transcript() {
    let (root, paths) = paths();
    let mut store = SkillStore::open_at(&paths).expect("store");
    let artifact = SkillArtifact::new(
        "function fakeIsPresent() { return typeof read_file === 'function'; }".to_string(),
        "Prove the declared read fake is present.".to_string(),
        vec!["fake".to_string()],
        vec![SkillExport {
            name: "fakeIsPresent".to_string(),
            signature: "fakeIsPresent(): boolean".to_string(),
        }],
        vec!["fakeIsPresent() === true".to_string()],
        CapabilityManifest::new(CapabilityTier::ReadOnly, vec![HostCapability::ReadFile])
            .expect("manifest"),
    )
    .expect("artifact");
    let mut fake_files = BTreeMap::new();
    fake_files.insert("/hidden/input".to_string(), "held-out-secret".to_string());
    let suite = HeldOutSuiteDraft {
        selector: HeldOutSelector {
            tags: vec!["fake".to_string()],
            exports: vec![],
            capability_tier: Some("read_only".to_string()),
        },
        cases: vec![HeldOutCase {
            expression: "read_file('/hidden/input')".to_string(),
            expected: ExpectedJsValue::String("held-out-secret".to_string()),
            fake_files,
            transcript: TranscriptExpectation {
                reads: 1,
                read_paths: vec!["/hidden/input".to_string()],
                ..TranscriptExpectation::default()
            },
        }],
    };
    let admin = AdminIdentity::authenticated("reviewer").unwrap();
    suite.import(&mut store, &admin, 10).expect("import");
    let report = evaluate(&store, &artifact, None).expect("evaluate");
    let serialized = serde_json::to_string(&report).unwrap();
    assert!(!serialized.contains("held-out-secret"));
    assert!(!serialized.contains("/hidden/input"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_held_out_evaluator_inherits_predecessor_regressions() {
    let (root, paths) = paths();
    let mut store = SkillStore::open_at(&paths).expect("store");
    let admin = AdminIdentity::authenticated("reviewer").unwrap();
    pure_suite("value")
        .import(&mut store, &admin, 10)
        .expect("suite");
    let predecessor = pure_artifact();
    let candidate = SkillArtifact::new(
        "function normalize(v) { return String(v); }".to_string(),
        "Normalize a value differently.".to_string(),
        vec!["normalize".to_string()],
        predecessor.exports.clone(),
        vec!["normalize('x') === 'x'".to_string()],
        CapabilityManifest::pure(),
    )
    .expect("candidate");
    assert!(matches!(
        evaluate(&store, &candidate, Some(&predecessor)),
        Err(HeldOutError::Inherited(_))
    ));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_held_out_evaluator_inherits_predecessor_suite_selection() {
    let (root, paths) = paths();
    let mut store = SkillStore::open_at(&paths).expect("store");
    let admin = AdminIdentity::authenticated("reviewer").unwrap();
    pure_suite("value")
        .import(&mut store, &admin, 10)
        .expect("suite");
    let predecessor = pure_artifact();
    let candidate = SkillArtifact::new(
        predecessor.source.clone(),
        "Same contract with new retrieval metadata.".to_string(),
        vec!["replacement".to_string()],
        predecessor.exports.clone(),
        predecessor.tests.clone(),
        CapabilityManifest::pure(),
    )
    .expect("candidate");

    let report = evaluate(&store, &candidate, Some(&predecessor))
        .expect("predecessor-selected suite must still execute");
    assert_eq!(report.suite_hashes.len(), 1);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_held_out_secrecy_detects_persisted_suite_tamper() {
    let (root, paths) = paths();
    let mut store = SkillStore::open_at(&paths).expect("store");
    let admin = AdminIdentity::authenticated("reviewer").unwrap();
    let suite_id = pure_suite("value")
        .import(&mut store, &admin, 10)
        .expect("suite");
    store
        .conn_mut()
        .execute(
            "UPDATE held_out_suites SET cases_json = '[]' WHERE suite_id = ?1",
            [&suite_id],
        )
        .expect("tamper fixture");
    assert!(matches!(
        select_suites(&store, &pure_artifact()),
        Err(HeldOutError::TamperedSuite(_))
    ));
    let _ = std::fs::remove_dir_all(root);
}
