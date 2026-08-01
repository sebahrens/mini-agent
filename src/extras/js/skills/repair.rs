//! Bounded, sanitized, content-addressed repair records.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::SkillArtifact;
use super::privacy::Redactor;
use super::store::{SkillStore, current_timestamp};

pub const MAX_REPAIR_PAYLOAD_BYTES: usize = 2_048;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpectedBehavior {
    Unknown,
    Shape(String),
    ApprovedFixture(String),
}

#[derive(Debug, Clone)]
pub struct RepairInput {
    pub failing_skill_id: String,
    pub export_name: String,
    pub argument_shape: Option<String>,
    pub deterministic_fixture: Option<String>,
    pub fixture_human_approved: bool,
    pub direct_outcome: String,
    pub expected_behavior: ExpectedBehavior,
    pub inherited_case_ids: Vec<String>,
    pub query_fingerprint: Option<String>,
    pub retrieval_score: Option<f64>,
    pub index_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepairRecord {
    pub schema_version: u32,
    pub repair_id: String,
    pub failing_skill_id: String,
    pub export_name: String,
    pub sanitized_argument_shape: Option<String>,
    pub approved_fixture: Option<String>,
    pub direct_outcome: String,
    pub expected_behavior: ExpectedBehavior,
    pub inherited_case_ids: Vec<String>,
    pub query_fingerprint: Option<String>,
    pub retrieval_score: Option<f64>,
    pub index_generation: u64,
}

#[derive(Serialize)]
struct RepairIdentity<'a> {
    schema_version: u32,
    failing_skill_id: &'a str,
    export_name: &'a str,
    sanitized_argument_shape: &'a Option<String>,
    approved_fixture: &'a Option<String>,
    direct_outcome: &'a str,
    expected_behavior: &'a ExpectedBehavior,
    inherited_case_ids: &'a [String],
    query_fingerprint: &'a Option<String>,
    retrieval_score: Option<f64>,
    index_generation: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum RepairError {
    #[error("repair input is invalid or exceeds bounds")]
    InvalidInput,
    #[error("value-bearing fixture requires authenticated human approval")]
    FixtureApprovalRequired,
    #[error("repair payload still contains a configured secret")]
    SecretRemaining,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("repair proposal must mint a distinct immutable compatible artifact")]
    InvalidProposal,
    #[error("Phase 4 proposal enqueue failed without changing the predecessor")]
    ProposalRejected,
    #[error(transparent)]
    Store(#[from] super::store::StoreError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// Persist the sanitized content-addressed record before any repair proposal is
/// submitted. Replays are idempotent and cannot change record contents.
pub fn persist_record(
    store: &mut SkillStore,
    record: &RepairRecord,
    created_at: i64,
) -> Result<(), RepairError> {
    if created_at < 0 {
        return Err(RepairError::InvalidInput);
    }
    let identity = repair_identity_bytes(record)?;
    if identity.len() > MAX_REPAIR_PAYLOAD_BYTES
        || format!("{:x}", Sha256::digest(&identity)) != record.repair_id
    {
        return Err(RepairError::InvalidInput);
    }
    let payload = serde_json::to_string(&serde_json::json!({
        "schema_version": record.schema_version,
        "argument_shape": record.sanitized_argument_shape,
        "approved_fixture": record.approved_fixture,
        "expected_behavior": record.expected_behavior,
    }))?;
    let inherited = serde_json::to_string(&record.inherited_case_ids)?;
    let human_approved = i64::from(
        record.approved_fixture.is_some()
            || matches!(
                record.expected_behavior,
                ExpectedBehavior::ApprovedFixture(_)
            ),
    );
    let changed = store.connection_mut().execute(
        "INSERT OR IGNORE INTO skill_repair_records (
            repair_id, skill_id, export_name, outcome_kind,
            sanitized_payload, inherited_cases_json, query_fingerprint,
            retrieval_score, index_generation, human_approved, created_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        rusqlite::params![
            record.repair_id,
            record.failing_skill_id,
            record.export_name,
            record.direct_outcome,
            payload,
            inherited,
            record.query_fingerprint,
            record.retrieval_score,
            record.index_generation as i64,
            human_approved,
            created_at,
        ],
    )?;
    if changed == 1 {
        return Ok(());
    }
    let exact: bool = store.connection().query_row(
        "SELECT EXISTS(
            SELECT 1 FROM skill_repair_records
             WHERE repair_id = ? AND skill_id = ? AND export_name = ?
               AND outcome_kind = ? AND sanitized_payload = ?
               AND inherited_cases_json = ?
               AND query_fingerprint IS ? AND retrieval_score IS ?
               AND index_generation = ? AND human_approved = ?
         )",
        rusqlite::params![
            record.repair_id,
            record.failing_skill_id,
            record.export_name,
            record.direct_outcome,
            payload,
            inherited,
            record.query_fingerprint,
            record.retrieval_score,
            record.index_generation as i64,
            human_approved,
        ],
        |row| row.get(0),
    )?;
    if exact {
        Ok(())
    } else {
        Err(RepairError::InvalidProposal)
    }
}

pub trait RepairProposalSink {
    fn enqueue(
        &mut self,
        artifact: &SkillArtifact,
        supersedes_id: &str,
        repair_id: &str,
    ) -> Result<(), String>;
}

impl RepairProposalSink for SkillStore {
    fn enqueue(
        &mut self,
        artifact: &SkillArtifact,
        supersedes_id: &str,
        _repair_id: &str,
    ) -> Result<(), String> {
        let now = current_timestamp().map_err(|error| error.to_string())?;
        self.enqueue_proposal(artifact, Some(supersedes_id), now)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

/// Thin adapter into Phase 4's bounded proposal path. Evaluation and admission
/// remain exclusively owned by that phase.
pub fn submit_repair_proposal(
    sink: &mut impl RepairProposalSink,
    predecessor: &SkillArtifact,
    candidate: &SkillArtifact,
    record: &RepairRecord,
) -> Result<(), RepairError> {
    predecessor
        .verify_identity()
        .map_err(|_| RepairError::InvalidProposal)?;
    candidate
        .verify_identity()
        .map_err(|_| RepairError::InvalidProposal)?;
    if record.failing_skill_id != predecessor.id
        || candidate.id == predecessor.id
        || candidate.capability.tier > predecessor.capability.tier
        || candidate
            .capability
            .grants
            .iter()
            .any(|grant| !predecessor.capability.grants.contains(grant))
    {
        return Err(RepairError::InvalidProposal);
    }
    sink.enqueue(candidate, &predecessor.id, &record.repair_id)
        .map_err(|_| RepairError::ProposalRejected)
}

pub fn create_record(input: RepairInput, redactor: &Redactor) -> Result<RepairRecord, RepairError> {
    if input.failing_skill_id.len() != 64
        || !input
            .failing_skill_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || input.export_name.trim().is_empty()
        || input.direct_outcome.trim().is_empty()
        || input
            .retrieval_score
            .is_some_and(|score| !score.is_finite())
    {
        return Err(RepairError::InvalidInput);
    }
    if input.deterministic_fixture.is_some() && !input.fixture_human_approved {
        return Err(RepairError::FixtureApprovalRequired);
    }
    if matches!(
        input.expected_behavior,
        ExpectedBehavior::ApprovedFixture(_)
    ) && !input.fixture_human_approved
    {
        return Err(RepairError::FixtureApprovalRequired);
    }
    let shape = input
        .argument_shape
        .as_deref()
        .map(|value| redactor.redact(value));
    let fixture = input
        .deterministic_fixture
        .as_deref()
        .map(|value| redactor.redact(value));
    let expected = match input.expected_behavior {
        ExpectedBehavior::Shape(value) => ExpectedBehavior::Shape(redactor.redact(&value)),
        ExpectedBehavior::ApprovedFixture(value) => {
            ExpectedBehavior::ApprovedFixture(redactor.redact(&value))
        }
        ExpectedBehavior::Unknown => ExpectedBehavior::Unknown,
    };
    let direct_outcome = redactor.redact(&input.direct_outcome);
    let query_fingerprint = input
        .query_fingerprint
        .as_deref()
        .map(|value| redactor.redact(value));
    let mut inherited_case_ids: Vec<String> = input
        .inherited_case_ids
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    inherited_case_ids.sort();

    let draft = RepairRecord {
        schema_version: 1,
        repair_id: String::new(),
        failing_skill_id: input.failing_skill_id,
        export_name: input.export_name,
        sanitized_argument_shape: shape,
        approved_fixture: fixture,
        direct_outcome,
        expected_behavior: expected,
        inherited_case_ids,
        query_fingerprint,
        retrieval_score: input.retrieval_score,
        index_generation: input.index_generation,
    };
    let bytes = repair_identity_bytes(&draft)?;
    if bytes.len() > MAX_REPAIR_PAYLOAD_BYTES
        || redactor.contains_configured_secret(std::str::from_utf8(&bytes).unwrap_or(""))
    {
        return Err(if bytes.len() > MAX_REPAIR_PAYLOAD_BYTES {
            RepairError::InvalidInput
        } else {
            RepairError::SecretRemaining
        });
    }
    let repair_id = format!("{:x}", Sha256::digest(&bytes));
    Ok(RepairRecord { repair_id, ..draft })
}

fn repair_identity_bytes(record: &RepairRecord) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&RepairIdentity {
        schema_version: record.schema_version,
        failing_skill_id: &record.failing_skill_id,
        export_name: &record.export_name,
        sanitized_argument_shape: &record.sanitized_argument_shape,
        approved_fixture: &record.approved_fixture,
        direct_outcome: &record.direct_outcome,
        expected_behavior: &record.expected_behavior,
        inherited_case_ids: &record.inherited_case_ids,
        query_fingerprint: &record.query_fingerprint,
        retrieval_score: record.retrieval_score,
        index_generation: record.index_generation,
    })
}

#[derive(Debug, Clone)]
pub struct RepairAttemptPolicy {
    pub max_per_session: usize,
    pub max_per_lineage: usize,
    pub base_backoff_seconds: u64,
    pub max_backoff_seconds: u64,
}

impl RepairAttemptPolicy {
    pub fn next_allowed_at(
        &self,
        session_attempts: usize,
        lineage_attempts: usize,
        last_attempt_at: i64,
    ) -> Option<i64> {
        if last_attempt_at < 0
            || session_attempts >= self.max_per_session
            || lineage_attempts >= self.max_per_lineage
        {
            return None;
        }
        let exponent = u32::try_from(lineage_attempts.min(31)).ok()?;
        let delay = self
            .base_backoff_seconds
            .saturating_mul(2u64.saturating_pow(exponent))
            .min(self.max_backoff_seconds);
        last_attempt_at.checked_add(i64::try_from(delay).ok()?)
    }
}
