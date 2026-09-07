use crate::extras::js::skills::fakes::{FakeSpawnFixture, FakeSpawnResponse};
use crate::extras::js::skills::held_out::{
    ExpectedJsValue, HeldOutCase, HeldOutError, HeldOutSelector, HeldOutSuiteDraft,
    TranscriptExpectation, evaluate, select_suites,
};
use crate::extras::js::skills::store::{AdminIdentity, SkillStore};
use crate::extras::js::skills::{
    CapabilityManifest, CapabilityTier, HostCapability, SkillArtifact, SkillExport, test_manifest,
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
        "function normalize(_cap, v) { return String(v).trim(); }".to_string(),
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
            exports: vec![SkillExport {
                name: "normalize".to_string(),
                signature: "normalize(value: unknown): string".to_string(),
            }],
            capability_tier: Some("pure".to_string()),
        },
        cases: vec![HeldOutCase {
            expression: "normalize('\\tvalue\\n')".to_string(),
            expected: ExpectedJsValue::String(expected.to_string()),
            fake_files: BTreeMap::new(),
            fake_spawns: vec![],
            fake_fetches: vec![],
            transcript: TranscriptExpectation::default(),
        }],
    }
}

fn pure_suite_with_cases(first_case: usize, count: usize) -> HeldOutSuiteDraft {
    HeldOutSuiteDraft {
        selector: HeldOutSelector {
            tags: vec!["normalize".to_string()],
            exports: vec![SkillExport {
                name: "normalize".to_string(),
                signature: "normalize(value: unknown): string".to_string(),
            }],
            capability_tier: Some("pure".to_string()),
        },
        cases: (first_case..first_case + count)
            .map(|index| HeldOutCase {
                expression: format!("normalize(' case{index} ')"),
                expected: ExpectedJsValue::String(format!("case{index}")),
                fake_files: BTreeMap::new(),
                fake_spawns: vec![],
                fake_fetches: vec![],
                transcript: TranscriptExpectation::default(),
            })
            .collect(),
    }
}

#[test]
fn skill_held_out_evaluator_refuses_a_corpus_above_the_matched_case_cap() {
    let (root, paths) = paths();
    let mut store = SkillStore::open_at(&paths).expect("store");
    let admin = AdminIdentity::authenticated("reviewer").expect("admin");
    let artifact = pure_artifact();

    // Two matched suites of 40 cases each: 80 cases against a 64-case cap.
    let first_id = pure_suite_with_cases(0, 40)
        .import(&mut store, &admin, 10)
        .expect("import the first suite");
    let second_id = pure_suite_with_cases(40, 40)
        .import(&mut store, &admin, 11)
        .expect("import the second suite");
    assert_ne!(
        first_id, second_id,
        "the fixture must import two distinct suites"
    );

    let selected = select_suites(&store, &artifact).expect("selection");
    assert_eq!(selected.len(), 2);
    let matched_cases = selected
        .iter()
        .map(|suite| suite.cases.len())
        .sum::<usize>();
    assert_eq!(matched_cases, 80);

    match evaluate(&store, &artifact, None) {
        Err(HeldOutError::InvalidSuite(detail)) => {
            assert!(
                detail.contains("64-case evaluation cap"),
                "unexpected refusal detail: {detail}"
            );
        }
        other => panic!(
            "a corpus above the case cap must be refused instead of binding \
             suite hashes for cases that never ran, got {other:?}"
        ),
    }

    let _ = std::fs::remove_dir_all(root);
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
fn held_out_selectors_require_scope_and_exact_export_contracts() {
    let empty = HeldOutSuiteDraft {
        selector: HeldOutSelector::default(),
        cases: vec![HeldOutCase {
            expression: "true".to_string(),
            expected: ExpectedJsValue::Boolean(true),
            fake_files: BTreeMap::new(),
            fake_spawns: vec![],
            fake_fetches: vec![],
            transcript: TranscriptExpectation::default(),
        }],
    };
    assert!(matches!(
        empty.validate(),
        Err(HeldOutError::InvalidSuite(_))
    ));

    let (root, paths) = paths();
    let mut store = SkillStore::open_at(&paths).unwrap();
    let wrong_contract = HeldOutSuiteDraft {
        selector: HeldOutSelector {
            tags: vec!["normalize".to_string()],
            exports: vec![SkillExport {
                name: "normalize".to_string(),
                signature: "normalize(value: string): number".to_string(),
            }],
            capability_tier: Some("pure".to_string()),
        },
        cases: empty.cases,
    };
    wrong_contract
        .import(
            &mut store,
            &AdminIdentity::authenticated("reviewer").unwrap(),
            10,
        )
        .unwrap();
    assert!(select_suites(&store, &pure_artifact()).unwrap().is_empty());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_no_effect_fakes_use_hidden_virtual_data_and_match_transcript() {
    let (root, paths) = paths();
    let mut store = SkillStore::open_at(&paths).expect("store");
    let artifact = SkillArtifact::new(
        "function fakeIsPresent(cap, path) { return path === undefined ? typeof cap.read_file === 'function' : cap.read_file(path); }".to_string(),
        "Prove the declared read fake is present.".to_string(),
        vec!["fake".to_string()],
        vec![SkillExport {
            name: "fakeIsPresent".to_string(),
            signature: "fakeIsPresent(): boolean".to_string(),
        }],
        vec!["fakeIsPresent() === true".to_string()],
        test_manifest(CapabilityTier::ReadOnly, vec![HostCapability::ReadFile]).expect("manifest"),
    )
    .expect("artifact");
    let mut fake_files = BTreeMap::new();
    fake_files.insert(
        "fixtures/hidden/input".to_string(),
        "held-out-secret".to_string(),
    );
    let suite = HeldOutSuiteDraft {
        selector: HeldOutSelector {
            tags: vec!["fake".to_string()],
            exports: vec![],
            capability_tier: Some("read_only".to_string()),
        },
        cases: vec![HeldOutCase {
            expression: "fakeIsPresent('fixtures/hidden/input')".to_string(),
            expected: ExpectedJsValue::String("held-out-secret".to_string()),
            fake_files,
            fake_spawns: vec![],
            fake_fetches: vec![],
            transcript: TranscriptExpectation {
                reads: 1,
                read_paths: vec!["fixtures/hidden/input".to_string()],
                ..TranscriptExpectation::default()
            },
        }],
    };
    let admin = AdminIdentity::authenticated("reviewer").unwrap();
    suite.import(&mut store, &admin, 10).expect("import");
    let report = evaluate(&store, &artifact, None).expect("evaluate");
    let serialized = serde_json::to_string(&report).unwrap();
    assert!(!serialized.contains("held-out-secret"));
    assert!(!serialized.contains("fixtures/hidden/input"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn hidden_spawn_fixture_rejects_a_skill_that_fingerprints_default_fakes() {
    let (root, paths) = paths();
    let mut store = SkillStore::open_at(&paths).expect("store");
    let artifact = SkillArtifact::new(
        "function accepts(cap) { const response = cap.spawn('printf', ['check']); return response.code === 0 && response.stdout.startsWith('simulated '); }".to_string(),
        "Try to recognize the verifier's default spawn response.".to_string(),
        vec!["fake-probe".to_string()],
        vec![SkillExport {
            name: "accepts".to_string(),
            signature: "accepts(): boolean".to_string(),
        }],
        vec!["accepts() === true".to_string()],
        test_manifest(CapabilityTier::SideEffecting, vec![HostCapability::Spawn])
            .expect("manifest"),
    )
    .expect("artifact");
    let suite = HeldOutSuiteDraft {
        selector: HeldOutSelector {
            tags: vec!["fake-probe".to_string()],
            exports: vec![SkillExport {
                name: "accepts".to_string(),
                signature: "accepts(): boolean".to_string(),
            }],
            capability_tier: Some("side_effecting".to_string()),
        },
        cases: vec![HeldOutCase {
            expression: "accepts()".to_string(),
            expected: ExpectedJsValue::Boolean(true),
            fake_files: BTreeMap::new(),
            fake_spawns: vec![FakeSpawnFixture {
                program: "printf".to_string(),
                args: vec!["check".to_string()],
                response: FakeSpawnResponse {
                    stdout: String::new(),
                    stderr: "permission denied".to_string(),
                    code: 17,
                    timed_out: false,
                    stdout_truncated: false,
                    stderr_truncated: false,
                },
            }],
            fake_fetches: vec![],
            transcript: TranscriptExpectation {
                spawns: 1,
                spawn_programs: vec!["printf".to_string()],
                ..TranscriptExpectation::default()
            },
        }],
    };
    let admin = AdminIdentity::authenticated("reviewer").unwrap();
    suite.import(&mut store, &admin, 10).expect("import");

    let result = evaluate(&store, &artifact, None);
    assert!(
        matches!(result, Err(HeldOutError::CaseFailed { .. })),
        "fingerprinting skill must fail its hidden case, got {result:?}"
    );
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
        "function normalize(_cap, v) { return String(v); }".to_string(),
        "Normalize a value differently.".to_string(),
        vec!["normalize".to_string()],
        predecessor.exports.clone(),
        vec!["normalize('x') === 'x'".to_string()],
        CapabilityManifest::pure(),
    )
    .expect("candidate");
    assert!(matches!(
        evaluate(&store, &candidate, Some(&predecessor)),
        Err(HeldOutError::InheritedTestsRemoved)
    ));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_held_out_evaluator_runs_predecessor_tests_against_unchanged_candidate_identity() {
    let (root, paths) = paths();
    let mut store = SkillStore::open_at(&paths).expect("store");
    let admin = AdminIdentity::authenticated("reviewer").unwrap();
    pure_suite("value")
        .import(&mut store, &admin, 10)
        .expect("suite");
    let predecessor = pure_artifact();
    let candidate = SkillArtifact::new(
        predecessor.source.clone(),
        "Same implementation with a distinct embedded regression.".to_string(),
        vec!["normalize".to_string()],
        predecessor.exports.clone(),
        vec![
            predecessor.tests[0].clone(),
            "normalize(' y ') === 'y'".to_string(),
        ],
        CapabilityManifest::pure(),
    )
    .expect("candidate");
    assert_ne!(candidate.tests, predecessor.tests);

    evaluate(&store, &candidate, Some(&predecessor))
        .expect("predecessor scripts must run without rewriting candidate identity");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_held_out_evaluator_requires_all_ancestor_tests() {
    let (root, paths) = paths();
    let mut store = SkillStore::open_at(&paths).expect("store");
    let admin = AdminIdentity::authenticated("reviewer").unwrap();
    pure_suite("value")
        .import(&mut store, &admin, 10)
        .expect("suite");
    let root_artifact = pure_artifact();
    let child = SkillArtifact::new(
        root_artifact.source.clone(),
        "Normalize with a second regression.".to_string(),
        root_artifact.tags.clone(),
        root_artifact.exports.clone(),
        vec![
            root_artifact.tests[0].clone(),
            "normalize(' child ') === 'child'".to_string(),
        ],
        CapabilityManifest::pure(),
    )
    .unwrap();
    store.insert_verified(&root_artifact).unwrap();
    store.insert_verified(&child).unwrap();
    store
        .conn_mut()
        .execute(
            "UPDATE skill_revisions
             SET supersedes_id = ?1, lineage_root_id = ?1 WHERE id = ?2",
            rusqlite::params![root_artifact.id, child.id],
        )
        .unwrap();
    let grandchild = SkillArtifact::new(
        child.source.clone(),
        "Normalize after dropping the root regression.".to_string(),
        child.tags.clone(),
        child.exports.clone(),
        vec![child.tests[1].clone()],
        CapabilityManifest::pure(),
    )
    .unwrap();

    assert!(matches!(
        evaluate(&store, &grandchild, Some(&child)),
        Err(HeldOutError::InheritedTestsRemoved)
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
