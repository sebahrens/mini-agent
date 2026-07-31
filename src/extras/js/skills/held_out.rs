//! Trusted, content-addressed held-out suites and generic no-effect evaluation.
//!
//! Suite contents are available only to Rust-owned import/evaluation paths. Public
//! reports bind suite hashes and pass/fail outcomes but never expose expressions,
//! expected values, fake responses, or transcripts.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

use super::fakes::{FAKES_VERSION, FakeTranscript};
use super::store::{AdminIdentity, HeldOutSuiteRecord, SkillStore, StoreError};
use super::verify::{VERIFIER_VERSION, VerificationError, verify_held_out_case, verify_skill};
use super::{CapabilityTier, SkillArtifact};

const SUITE_FORMAT_VERSION: u32 = 1;
const MAX_CASES: usize = 64;
const MAX_EXPRESSION_BYTES: usize = 4 * 1024;
const MAX_FAKE_FILES: usize = 32;
const MAX_FAKE_FILE_BYTES: usize = 64 * 1024;
const MAX_SELECTOR_VALUES: usize = 32;
const MAX_SELECTOR_VALUE_BYTES: usize = 128;
const MAX_EXPECTED_STRING_BYTES: usize = 64 * 1024;
const MAX_TRANSCRIPT_CALLS: usize = 256;
const MAX_TRANSCRIPT_VALUE_BYTES: usize = 4 * 1024;
const MAX_MATCHED_SUITES: usize = 32;
const MAX_MATCHED_CASES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(crate) struct HeldOutSelector {
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub exports: Vec<String>,
    #[serde(default)]
    pub capability_tier: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub(crate) enum ExpectedJsValue {
    Boolean(bool),
    String(String),
    Integer(i64),
    Float(f64),
    Null,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(crate) struct TranscriptExpectation {
    #[serde(default)]
    pub reads: usize,
    #[serde(default)]
    pub writes: usize,
    #[serde(default)]
    pub spawns: usize,
    #[serde(default)]
    pub fetches: usize,
    #[serde(default)]
    pub read_paths: Vec<String>,
    #[serde(default)]
    pub spawn_programs: Vec<String>,
    #[serde(default)]
    pub fetch_urls: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HeldOutCase {
    pub expression: String,
    pub expected: ExpectedJsValue,
    #[serde(default)]
    pub fake_files: BTreeMap<String, String>,
    #[serde(default)]
    pub transcript: TranscriptExpectation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HeldOutSuiteDraft {
    pub selector: HeldOutSelector,
    pub cases: Vec<HeldOutCase>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CanonicalSuitePayload {
    version: u32,
    selector: HeldOutSelector,
    cases: Vec<HeldOutCase>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HeldOutSuite {
    pub id: String,
    pub content_hash: String,
    pub selector: HeldOutSelector,
    pub cases: Vec<HeldOutCase>,
    pub approved_by: String,
    pub approved_at: i64,
}

impl HeldOutSuiteDraft {
    pub(crate) fn import(
        self,
        store: &mut SkillStore,
        admin: &AdminIdentity,
        now: i64,
    ) -> Result<String, HeldOutError> {
        validate_suite(&self)?;
        let canonical = CanonicalSuitePayload {
            version: SUITE_FORMAT_VERSION,
            selector: self.selector,
            cases: self.cases,
        };
        let canonical_payload = serde_json::to_string(&canonical)?;
        let content_hash = format!("{:x}", Sha256::digest(canonical_payload.as_bytes()));
        let record = HeldOutSuiteRecord {
            suite_id: content_hash.clone(),
            selector_json: serde_json::to_string(&canonical.selector)?,
            cases_json: serde_json::to_string(&canonical.cases)?,
            content_hash: content_hash.clone(),
            canonical_payload,
            approved_by: String::new(),
            approved_at: 0,
            enabled: true,
        };
        store.import_held_out_suite(Some(admin), &record, now)?;
        Ok(content_hash)
    }
}

pub(crate) fn select_suites(
    store: &SkillStore,
    artifact: &SkillArtifact,
) -> Result<Vec<HeldOutSuite>, HeldOutError> {
    let records = store.enabled_held_out_suites()?;
    let mut suites = Vec::new();
    for record in records {
        let suite = decode_record(record)?;
        if selector_matches(&suite.selector, artifact) {
            suites.push(suite);
        }
    }
    suites.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(suites)
}

fn decode_record(record: HeldOutSuiteRecord) -> Result<HeldOutSuite, HeldOutError> {
    let hash = format!("{:x}", Sha256::digest(record.canonical_payload.as_bytes()));
    if hash != record.suite_id || hash != record.content_hash {
        return Err(HeldOutError::TamperedSuite(record.suite_id));
    }
    let canonical: CanonicalSuitePayload = serde_json::from_str(&record.canonical_payload)?;
    if canonical.version != SUITE_FORMAT_VERSION {
        return Err(HeldOutError::UnsupportedVersion(canonical.version));
    }
    validate_suite(&HeldOutSuiteDraft {
        selector: canonical.selector.clone(),
        cases: canonical.cases.clone(),
    })?;
    if serde_json::to_string(&canonical.selector)? != record.selector_json
        || serde_json::to_string(&canonical.cases)? != record.cases_json
    {
        return Err(HeldOutError::TamperedSuite(record.suite_id));
    }
    Ok(HeldOutSuite {
        id: record.suite_id,
        content_hash: record.content_hash,
        selector: canonical.selector,
        cases: canonical.cases,
        approved_by: record.approved_by,
        approved_at: record.approved_at,
    })
}

fn selector_matches(selector: &HeldOutSelector, artifact: &SkillArtifact) -> bool {
    selector
        .tags
        .iter()
        .all(|tag| artifact.tags.contains(&tag.trim().to_lowercase()))
        && selector.exports.iter().all(|required| {
            artifact
                .exports
                .iter()
                .any(|export| export.name == *required)
        })
        && selector
            .capability_tier
            .as_deref()
            .is_none_or(|tier| artifact.capability.tier.as_token() == tier)
}

fn validate_suite(suite: &HeldOutSuiteDraft) -> Result<(), HeldOutError> {
    if suite.cases.is_empty() || suite.cases.len() > MAX_CASES {
        return Err(HeldOutError::InvalidSuite(
            "suite must contain between 1 and 64 cases".to_string(),
        ));
    }
    if suite.selector.tags.len() > MAX_SELECTOR_VALUES
        || suite.selector.exports.len() > MAX_SELECTOR_VALUES
        || suite
            .selector
            .tags
            .iter()
            .chain(&suite.selector.exports)
            .any(|value| {
                value.trim().is_empty()
                    || value.len() > MAX_SELECTOR_VALUE_BYTES
                    || value.contains('\0')
            })
    {
        return Err(HeldOutError::InvalidSuite(
            "selector contains too many values".to_string(),
        ));
    }
    if let Some(tier) = suite.selector.capability_tier.as_deref()
        && CapabilityTier::from_token(tier).is_none()
    {
        return Err(HeldOutError::InvalidSuite(
            "selector capability tier is invalid".to_string(),
        ));
    }
    for case in &suite.cases {
        if case.expression.trim().is_empty()
            || case.expression.len() > MAX_EXPRESSION_BYTES
            || case.expression.contains('\0')
        {
            return Err(HeldOutError::InvalidSuite(
                "held-out expression is empty or oversized".to_string(),
            ));
        }
        if case.fake_files.len() > MAX_FAKE_FILES
            || case.fake_files.iter().any(|(path, value)| {
                path.is_empty()
                    || path.len() > MAX_TRANSCRIPT_VALUE_BYTES
                    || path.contains('\0')
                    || value.len() > MAX_FAKE_FILE_BYTES
            })
        {
            return Err(HeldOutError::InvalidSuite(
                "held-out fake file fixture is invalid".to_string(),
            ));
        }
        if matches!(&case.expected, ExpectedJsValue::String(value) if value.len() > MAX_EXPECTED_STRING_BYTES)
            || matches!(&case.expected, ExpectedJsValue::Float(value) if !value.is_finite())
        {
            return Err(HeldOutError::InvalidSuite(
                "held-out expected value is invalid".to_string(),
            ));
        }
        let transcript = &case.transcript;
        if transcript.reads > MAX_TRANSCRIPT_CALLS
            || transcript.writes > MAX_TRANSCRIPT_CALLS
            || transcript.spawns > MAX_TRANSCRIPT_CALLS
            || transcript.fetches > MAX_TRANSCRIPT_CALLS
            || transcript.read_paths.len() > MAX_TRANSCRIPT_CALLS
            || transcript.spawn_programs.len() > MAX_TRANSCRIPT_CALLS
            || transcript.fetch_urls.len() > MAX_TRANSCRIPT_CALLS
            || transcript
                .read_paths
                .iter()
                .chain(&transcript.spawn_programs)
                .chain(&transcript.fetch_urls)
                .any(|value| value.len() > MAX_TRANSCRIPT_VALUE_BYTES || value.contains('\0'))
        {
            return Err(HeldOutError::InvalidSuite(
                "held-out transcript expectation is invalid".to_string(),
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct HeldOutCaseReport {
    pub suite_id: String,
    pub case_index: usize,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct HeldOutEvaluationReport {
    pub report_id: String,
    pub skill_id: String,
    pub predecessor_id: Option<String>,
    pub suite_hashes: Vec<String>,
    pub verifier_version: u32,
    pub fakes_version: u32,
    pub embedded_passed: bool,
    pub inherited_passed: bool,
    pub cases: Vec<HeldOutCaseReport>,
}

pub(crate) fn evaluate(
    store: &SkillStore,
    artifact: &SkillArtifact,
    predecessor: Option<&SkillArtifact>,
) -> Result<HeldOutEvaluationReport, HeldOutError> {
    artifact
        .verify_identity()
        .map_err(|error| HeldOutError::Identity(error.to_string()))?;
    verify_skill(artifact).map_err(HeldOutError::Embedded)?;

    if let Some(predecessor) = predecessor {
        predecessor
            .verify_identity()
            .map_err(|error| HeldOutError::Identity(error.to_string()))?;
        let mut inherited = artifact.clone();
        inherited.tests = predecessor.tests.clone();
        verify_skill(&inherited).map_err(HeldOutError::Inherited)?;
    }

    let mut suites = select_suites(store, artifact)?;
    if let Some(predecessor) = predecessor {
        for inherited_suite in select_suites(store, predecessor)? {
            if !suites.iter().any(|suite| suite.id == inherited_suite.id) {
                suites.push(inherited_suite);
            }
        }
        suites.sort_by(|left, right| left.id.cmp(&right.id));
    }
    if suites.len() > MAX_MATCHED_SUITES
        || suites
            .iter()
            .try_fold(0usize, |count, suite| count.checked_add(suite.cases.len()))
            .is_none_or(|count| count > MAX_MATCHED_CASES)
    {
        return Err(HeldOutError::InvalidSuite(
            "too many held-out suites or cases matched one proposal".to_string(),
        ));
    }
    if suites.is_empty() {
        return Err(HeldOutError::SuiteRequired);
    }
    let mut case_reports = Vec::new();
    let mut suite_hashes = Vec::with_capacity(suites.len());
    for suite in suites {
        suite_hashes.push(suite.content_hash.clone());
        for (case_index, case) in suite.cases.iter().enumerate() {
            let transcript =
                verify_held_out_case(artifact, &case.expression, &case.expected, &case.fake_files)
                    .map_err(|_| HeldOutError::CaseFailed {
                        suite_id: suite.id.clone(),
                        case_index,
                    })?;
            if !transcript_matches(&case.transcript, &transcript) {
                return Err(HeldOutError::TranscriptMismatch {
                    suite_id: suite.id,
                    case_index,
                });
            }
            case_reports.push(HeldOutCaseReport {
                suite_id: suite.id.clone(),
                case_index,
                passed: true,
            });
        }
    }

    let predecessor_id = predecessor.map(|value| value.id.clone());
    let identity_payload = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "skill_id": artifact.id,
        "predecessor_id": predecessor_id,
        "suite_hashes": suite_hashes,
        "verifier_version": VERIFIER_VERSION,
        "fakes_version": FAKES_VERSION,
        "case_count": case_reports.len(),
        "outcome": "passed"
    }))?;
    let report_id = format!("{:x}", Sha256::digest(identity_payload));
    Ok(HeldOutEvaluationReport {
        report_id,
        skill_id: artifact.id.clone(),
        predecessor_id,
        suite_hashes,
        verifier_version: VERIFIER_VERSION,
        fakes_version: FAKES_VERSION,
        embedded_passed: true,
        inherited_passed: true,
        cases: case_reports,
    })
}

fn transcript_matches(expected: &TranscriptExpectation, actual: &FakeTranscript) -> bool {
    expected.reads == actual.reads.len()
        && expected.writes == actual.writes.len()
        && expected.spawns == actual.spawns.len()
        && expected.fetches == actual.fetches.len()
        && expected.read_paths
            == actual
                .reads
                .iter()
                .map(|record| record.path.clone())
                .collect::<Vec<_>>()
        && expected.spawn_programs
            == actual
                .spawns
                .iter()
                .map(|record| record.program.clone())
                .collect::<Vec<_>>()
        && expected.fetch_urls
            == actual
                .fetches
                .iter()
                .map(|record| record.url.clone())
                .collect::<Vec<_>>()
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum HeldOutError {
    #[error("invalid held-out suite: {0}")]
    InvalidSuite(String),
    #[error("unsupported held-out suite version: {0}")]
    UnsupportedVersion(u32),
    #[error("held-out suite was tampered: {0}")]
    TamperedSuite(String),
    #[error("held-out_suite_required")]
    SuiteRequired,
    #[error("identity_invalid: {0}")]
    Identity(String),
    #[error("embedded_test_failed")]
    Embedded(VerificationError),
    #[error("inherited_regression_failed")]
    Inherited(VerificationError),
    #[error("held_out_failed for suite {suite_id} case {case_index}")]
    CaseFailed { suite_id: String, case_index: usize },
    #[error("held_out_failed transcript for suite {suite_id} case {case_index}")]
    TranscriptMismatch { suite_id: String, case_index: usize },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
