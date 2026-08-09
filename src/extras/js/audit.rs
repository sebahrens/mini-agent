//! Private, append-only persistence for parent-authorized JavaScript effects.
//!
//! This module owns storage integrity only. The broker owns effect ordering; keeping the writer
//! independent here prevents persistence code from executing or authorizing an effect.

use std::collections::{BTreeMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::paths::EffectAuditPathOwner;

const FORMAT_VERSION: u16 = 1;
const MAX_RECORD_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_SEGMENT_BYTES: u64 = 4 * 1024 * 1024;
const DEFAULT_MAX_SEGMENTS: u64 = 256;
const MAX_IDENTIFIER_BYTES: usize = 256;
const TARGET_KEY_BYTES: usize = 32;
const TARGET_KEY_VERSION: u16 = 1;
const INITIALIZATION_MARKER: &[u8] = b"mini-agent-js-effect-audit-v1\n";
const ZERO_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuditState {
    Intent,
    Completed,
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuditCapability {
    ReadFile,
    WriteFile,
    Fetch,
    Spawn,
    ProposeSkill,
}

impl AuditCapability {
    fn as_str(self) -> &'static str {
        match self {
            Self::ReadFile => "read_file",
            Self::WriteFile => "write_file",
            Self::Fetch => "fetch",
            Self::Spawn => "spawn",
            Self::ProposeSkill => "propose_skill",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuditDecision {
    Authorized,
}

impl AuditDecision {
    fn as_str(self) -> &'static str {
        match self {
            Self::Authorized => "authorized",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuditResultCode {
    Succeeded,
    Denied,
    Cancelled,
    TimedOut,
    OutputLimit,
    BackendFailure,
    OutcomeUnknown,
}

impl AuditResultCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Denied => "denied",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
            Self::OutputLimit => "output_limit",
            Self::BackendFailure => "backend_failure",
            Self::OutcomeUnknown => "outcome_unknown",
        }
    }
}

/// Metadata safe to persist. Raw paths, URLs, query strings, argv, bodies, source, prompts,
/// headers, credentials, and environment values have no representable field in this type.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct SanitizedTarget {
    #[serde(flatten)]
    kind: SanitizedTargetKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SanitizedTargetKind {
    File {
        key_version: u16,
        storage_class: String,
        target_tag: String,
    },
    Fetch {
        key_version: u16,
        scheme: String,
        effective_port: u16,
        host_tag: String,
        path_query_tag: String,
        method: String,
    },
    Spawn {
        key_version: u16,
        executable_tag: String,
    },
    Proposal,
}

impl SanitizedTarget {
    fn file(key: &[u8; TARGET_KEY_BYTES], operation: &str, path: &str) -> Self {
        Self {
            kind: SanitizedTargetKind::File {
                key_version: TARGET_KEY_VERSION,
                storage_class: "workspace".into(),
                target_tag: target_tag(key, operation, "canonical_path", path.as_bytes()),
            },
        }
    }

    fn fetch(key: &[u8; TARGET_KEY_BYTES], url: &str, method: &str) -> Result<Self, AuditError> {
        let (scheme, remainder) = url.split_once("://").ok_or(AuditError::InvalidMetadata)?;
        let scheme = scheme.to_ascii_lowercase();
        if !matches!(scheme.as_str(), "http" | "https") {
            return Err(AuditError::InvalidMetadata);
        }
        let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
        let authority = &remainder[..authority_end];
        let authority_without_credentials = authority.rsplit('@').next().unwrap_or_default();
        if authority_without_credentials.is_empty()
            || authority_without_credentials
                .chars()
                .any(char::is_whitespace)
        {
            return Err(AuditError::InvalidMetadata);
        }
        let suffix = &remainder[authority_end..];
        let path_query = suffix.split('#').next().unwrap_or_default();
        let (host, explicit_port) = split_host_port(authority_without_credentials)?;
        let effective_port = explicit_port.unwrap_or(if scheme == "https" { 443 } else { 80 });
        let method = method.to_ascii_lowercase();
        if !matches!(method.as_str(), "get" | "post") {
            return Err(AuditError::InvalidMetadata);
        }
        Ok(Self {
            kind: SanitizedTargetKind::Fetch {
                key_version: TARGET_KEY_VERSION,
                scheme,
                effective_port,
                host_tag: target_tag(key, "fetch", "normalized_host", host.as_bytes()),
                path_query_tag: target_tag(
                    key,
                    "fetch",
                    "canonical_path_query",
                    path_query.as_bytes(),
                ),
                method,
            },
        })
    }

    fn spawn(key: &[u8; TARGET_KEY_BYTES], program: &str) -> Self {
        Self {
            kind: SanitizedTargetKind::Spawn {
                key_version: TARGET_KEY_VERSION,
                executable_tag: target_tag(key, "spawn", "resolved_executable", program.as_bytes()),
            },
        }
    }

    const fn proposal() -> Self {
        Self {
            kind: SanitizedTargetKind::Proposal,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EffectIntent {
    pub(crate) effect_id: String,
    pub(crate) invocation_id: String,
    pub(crate) grant_id: String,
    pub(crate) sequence: u64,
    pub(crate) timestamp_ms: i64,
    pub(crate) artifact_id: Option<String>,
    pub(crate) export: Option<String>,
    pub(crate) capability: AuditCapability,
    pub(crate) normalized_target: SanitizedTarget,
    pub(crate) decision: AuditDecision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EffectCompletion {
    pub(crate) effect_id: String,
    pub(crate) result_code: AuditResultCode,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EffectAuditRecord {
    pub(crate) effect_id: String,
    pub(crate) invocation_id: String,
    pub(crate) grant_id: String,
    pub(crate) sequence: u64,
    pub(crate) timestamp_ms: i64,
    pub(crate) artifact_id: Option<String>,
    pub(crate) export: Option<String>,
    pub(crate) capability: String,
    pub(crate) normalized_target: SanitizedTarget,
    pub(crate) state: AuditState,
    pub(crate) decision: String,
    pub(crate) result_code: Option<String>,
    pub(crate) previous_hash: String,
    pub(crate) record_hash: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuditFailurePoint {
    Append,
    FileSync,
    DirectorySync,
}

#[derive(Clone, Debug)]
pub(crate) struct AuditOpenOptions {
    max_segment_bytes: u64,
    max_segments: u64,
    failure: Option<AuditFailurePoint>,
}

impl Default for AuditOpenOptions {
    fn default() -> Self {
        Self {
            max_segment_bytes: DEFAULT_MAX_SEGMENT_BYTES,
            max_segments: DEFAULT_MAX_SEGMENTS,
            failure: None,
        }
    }
}

impl AuditOpenOptions {
    #[cfg(test)]
    pub(crate) fn for_test(max_segment_bytes: u64) -> Self {
        Self {
            max_segment_bytes,
            max_segments: DEFAULT_MAX_SEGMENTS,
            failure: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_max_segments(mut self, max_segments: u64) -> Self {
        self.max_segments = max_segments;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_failure(mut self, failure: AuditFailurePoint) -> Self {
        self.failure = Some(failure);
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum AuditError {
    #[error("effect audit path is unavailable")]
    PathUnavailable,
    #[error("effect audit already has a writer")]
    WriterLocked,
    #[error("effect audit durability operation failed")]
    SyncFailed,
    #[error("effect audit record is corrupt")]
    CorruptRecord,
    #[error("effect audit hash chain does not match")]
    HashMismatch,
    #[error("effect audit segment is missing")]
    MissingSegment,
    #[error("effect audit ID was replayed")]
    ReplayedEffect,
    #[error("effect audit completion has no durable intent")]
    UnknownEffect,
    #[error("effect audit metadata is invalid")]
    InvalidMetadata,
    #[error("effect audit record exceeds its bound")]
    RecordTooLarge,
    #[error("effect audit writer is unavailable after a durability failure")]
    Unavailable,
    #[error("effect audit target-correlation key is unavailable")]
    KeyUnavailable,
    #[error("effect audit retention limit has been reached")]
    RetentionLimit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
// The effect body is serialized immediately and preserving the direct value keeps
// the authenticated on-disk representation and replay code straightforward.
#[allow(clippy::large_enum_variant)]
enum StoredKind {
    Open {
        segment_index: u64,
        previous_segment_hash: String,
        target_key_version: u16,
        target_key_digest: String,
    },
    Close {
        segment_index: u64,
        next_segment_index: u64,
    },
    Effect(EffectBody),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EffectBody {
    effect_id: String,
    invocation_id: String,
    grant_id: String,
    sequence: u64,
    timestamp_ms: i64,
    artifact_id: Option<String>,
    export: Option<String>,
    capability: String,
    normalized_target: SanitizedTarget,
    state: AuditState,
    decision: String,
    result_code: Option<String>,
}

#[derive(Serialize)]
struct UnsignedRecord<'a> {
    version: u16,
    kind: &'a StoredKind,
    previous_hash: &'a str,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRecord {
    version: u16,
    kind: StoredKind,
    previous_hash: String,
    record_hash: String,
}

impl StoredRecord {
    fn new(kind: StoredKind, previous_hash: String) -> Result<Self, AuditError> {
        let unsigned = UnsignedRecord {
            version: FORMAT_VERSION,
            kind: &kind,
            previous_hash: &previous_hash,
        };
        let canonical = serde_json::to_vec(&unsigned).map_err(|_| AuditError::InvalidMetadata)?;
        Ok(Self {
            version: FORMAT_VERSION,
            kind,
            previous_hash,
            record_hash: sha256_hex(&canonical),
        })
    }

    fn validate_hash(&self) -> Result<(), AuditError> {
        if self.version != FORMAT_VERSION
            || !valid_hash(&self.previous_hash)
            || !valid_hash(&self.record_hash)
        {
            return Err(AuditError::CorruptRecord);
        }
        let unsigned = UnsignedRecord {
            version: self.version,
            kind: &self.kind,
            previous_hash: &self.previous_hash,
        };
        let canonical = serde_json::to_vec(&unsigned).map_err(|_| AuditError::CorruptRecord)?;
        if sha256_hex(&canonical) != self.record_hash {
            return Err(AuditError::HashMismatch);
        }
        Ok(())
    }
}

struct AuditLock {
    file: File,
}

impl AuditLock {
    fn acquire(path: &Path) -> Result<Self, AuditError> {
        let file = open_private_rw(path, true, false).map_err(|_| AuditError::PathUnavailable)?;
        try_lock_exclusive(&file)?;
        Ok(Self { file })
    }
}

impl Drop for AuditLock {
    fn drop(&mut self) {
        unlock(&self.file);
    }
}

pub(crate) struct EffectAudit {
    owner: EffectAuditPathOwner,
    options: AuditOpenOptions,
    _lock: AuditLock,
    current_file: File,
    current_segment: u64,
    current_length: u64,
    current_effect_records: u64,
    previous_hash: String,
    records: Vec<EffectAuditRecord>,
    active: BTreeMap<String, EffectAuditRecord>,
    terminal: HashSet<String>,
    rotation_anchors: usize,
    poisoned: bool,
    target_key: [u8; TARGET_KEY_BYTES],
}

impl EffectAudit {
    pub(crate) fn open(owner: EffectAuditPathOwner) -> Result<Self, AuditError> {
        Self::open_with_options(owner, AuditOpenOptions::default())
    }

    pub(crate) fn open_with_options(
        owner: EffectAuditPathOwner,
        options: AuditOpenOptions,
    ) -> Result<Self, AuditError> {
        if options.max_segment_bytes < 512 || options.max_segments == 0 {
            return Err(AuditError::InvalidMetadata);
        }
        prepare_owner_directory(&owner, options.failure)
            .map_err(|error| report_open_stage("prepare_owner_directory", error))?;
        let lock = AuditLock::acquire(&owner.lock_file())
            .map_err(|error| report_open_stage("acquire_lock", error))?;

        let indices =
            segment_indices(&owner).map_err(|error| report_open_stage("segment_indices", error))?;
        let initialized = initialization_marker_exists(&owner)
            .map_err(|error| report_open_stage("initialization_marker_exists", error))?;
        if initialized && indices.is_empty() {
            return Err(AuditError::MissingSegment);
        }
        if indices.len() as u64 > options.max_segments {
            return Err(AuditError::RetentionLimit);
        }
        let target_key = load_or_create_target_key(&owner, indices.is_empty(), options.failure)
            .map_err(|error| report_open_stage("target_key", error))?;
        let mut audit = if indices.is_empty() {
            let file = create_private_segment(&owner.segment_file(0))
                .map_err(|error| report_open_stage("create_segment", error))?;
            let mut audit = Self {
                owner,
                options,
                _lock: lock,
                current_file: file,
                current_segment: 0,
                current_length: 0,
                current_effect_records: 0,
                previous_hash: ZERO_HASH.into(),
                records: Vec::new(),
                active: BTreeMap::new(),
                terminal: HashSet::new(),
                rotation_anchors: 0,
                poisoned: false,
                target_key,
            };
            audit.sync_directory()?;
            audit.append_stored(StoredKind::Open {
                segment_index: 0,
                previous_segment_hash: ZERO_HASH.into(),
                target_key_version: TARGET_KEY_VERSION,
                target_key_digest: sha256_hex(&audit.target_key),
            })?;
            audit.sync_file()?;
            audit.rotation_anchors = 1;
            audit
        } else {
            validate_contiguous_indices(&indices)?;
            let last = *indices.last().ok_or(AuditError::MissingSegment)?;
            let file = open_private_rw(&owner.segment_file(last), false, false)
                .map_err(|_| AuditError::PathUnavailable)?;
            Self {
                owner,
                options,
                _lock: lock,
                current_file: file,
                current_segment: last,
                current_length: 0,
                current_effect_records: 0,
                previous_hash: ZERO_HASH.into(),
                records: Vec::new(),
                active: BTreeMap::new(),
                terminal: HashSet::new(),
                rotation_anchors: 0,
                poisoned: false,
                target_key,
            }
        };

        if !indices.is_empty() {
            audit.validate_existing(&indices)?;
        }
        audit
            .ensure_initialization_marker()
            .map_err(|error| report_open_stage("ensure_initialization_marker", error))?;
        audit
            .recover_unknown_outcomes()
            .map_err(|error| report_open_stage("recover_unknown_outcomes", error))?;
        Ok(audit)
    }

    pub(crate) fn append_intent(
        &mut self,
        intent: EffectIntent,
    ) -> Result<EffectAuditRecord, AuditError> {
        if self.poisoned {
            return Err(AuditError::Unavailable);
        }
        validate_identifier(&intent.effect_id)?;
        validate_identifier(&intent.invocation_id)?;
        validate_optional_identifier(intent.artifact_id.as_deref())?;
        validate_optional_identifier(intent.export.as_deref())?;
        if self.active.contains_key(&intent.effect_id) || self.terminal.contains(&intent.effect_id)
        {
            return Err(AuditError::ReplayedEffect);
        }
        let body = EffectBody {
            effect_id: intent.effect_id,
            invocation_id: intent.invocation_id,
            grant_id: intent.grant_id,
            sequence: intent.sequence,
            timestamp_ms: intent.timestamp_ms,
            artifact_id: intent.artifact_id,
            export: intent.export,
            capability: intent.capability.as_str().into(),
            normalized_target: intent.normalized_target,
            state: AuditState::Intent,
            decision: intent.decision.as_str().into(),
            result_code: None,
        };
        let record = self.append_effect(body)?;
        self.active.insert(record.effect_id.clone(), record.clone());
        Ok(record)
    }

    pub(crate) fn append_completion(
        &mut self,
        completion: EffectCompletion,
    ) -> Result<EffectAuditRecord, AuditError> {
        if self.poisoned {
            return Err(AuditError::Unavailable);
        }
        validate_identifier(&completion.effect_id)?;
        let Some(intent) = self.active.get(&completion.effect_id).cloned() else {
            return if self.terminal.contains(&completion.effect_id) {
                Err(AuditError::ReplayedEffect)
            } else {
                Err(AuditError::UnknownEffect)
            };
        };
        let state = if completion.result_code == AuditResultCode::OutcomeUnknown {
            AuditState::OutcomeUnknown
        } else {
            AuditState::Completed
        };
        let body = body_from_record(&intent, state, Some(completion.result_code.as_str().into()));
        let record = self.append_effect(body)?;
        self.active.remove(&completion.effect_id);
        self.terminal.insert(completion.effect_id);
        Ok(record)
    }

    pub(crate) fn records(&self) -> &[EffectAuditRecord] {
        &self.records
    }

    pub(crate) fn file_target(&self, canonical_path: &str) -> SanitizedTarget {
        SanitizedTarget::file(&self.target_key, "read_file", canonical_path)
    }

    pub(crate) fn write_file_target(&self, canonical_path: &str) -> SanitizedTarget {
        SanitizedTarget::file(&self.target_key, "write_file", canonical_path)
    }

    pub(crate) fn fetch_target(
        &self,
        normalized_url: &str,
        method: &str,
    ) -> Result<SanitizedTarget, AuditError> {
        SanitizedTarget::fetch(&self.target_key, normalized_url, method)
    }

    pub(crate) fn spawn_target(&self, resolved_executable: &str) -> SanitizedTarget {
        SanitizedTarget::spawn(&self.target_key, resolved_executable)
    }

    pub(crate) const fn proposal_target(&self) -> SanitizedTarget {
        SanitizedTarget::proposal()
    }

    #[cfg(test)]
    pub(crate) fn rotation_anchor_count(&self) -> usize {
        self.rotation_anchors
    }

    fn validate_existing(&mut self, indices: &[u64]) -> Result<(), AuditError> {
        let mut expected_previous = ZERO_HASH.to_string();
        let mut prior_was_close = false;
        let mut expected_open_previous = ZERO_HASH.to_string();

        for (position, index) in indices.iter().copied().enumerate() {
            let is_last = position + 1 == indices.len();
            let path = self.owner.segment_file(index);
            let mut file =
                open_private_rw(&path, false, false).map_err(|_| AuditError::PathUnavailable)?;
            let (entries, valid_length, truncated) = read_segment(
                &mut file,
                is_last,
                self.options
                    .max_segment_bytes
                    .saturating_add((MAX_RECORD_BYTES * 2) as u64),
            )?;
            if truncated {
                file.set_len(valid_length)
                    .map_err(|_| AuditError::SyncFailed)?;
                sync_file(&file, self.options.failure)?;
            }
            if entries.is_empty() {
                return Err(AuditError::CorruptRecord);
            }

            let mut effect_records = 0_u64;
            let entry_count = entries.len();
            for (entry_position, stored) in entries.into_iter().enumerate() {
                stored.validate_hash()?;
                if stored.previous_hash != expected_previous {
                    return Err(AuditError::HashMismatch);
                }
                match &stored.kind {
                    StoredKind::Open {
                        segment_index,
                        previous_segment_hash,
                        target_key_version,
                        target_key_digest,
                    } if entry_position == 0
                        && *segment_index == index
                        && previous_segment_hash == &expected_open_previous
                        && *target_key_version == TARGET_KEY_VERSION
                        && target_key_digest == &sha256_hex(&self.target_key) =>
                    {
                        prior_was_close = false;
                        self.rotation_anchors += 1;
                    }
                    StoredKind::Open {
                        target_key_digest, ..
                    } if target_key_digest != &sha256_hex(&self.target_key) => {
                        return Err(AuditError::KeyUnavailable);
                    }
                    StoredKind::Open { .. } => return Err(AuditError::CorruptRecord),
                    StoredKind::Close {
                        segment_index,
                        next_segment_index,
                    } if !prior_was_close
                        && entry_position + 1 == entry_count
                        && *segment_index == index
                        && *next_segment_index == index + 1 =>
                    {
                        prior_was_close = true;
                        expected_open_previous = stored.record_hash.clone();
                        self.rotation_anchors += 1;
                    }
                    StoredKind::Close { .. } => return Err(AuditError::CorruptRecord),
                    StoredKind::Effect(body) if !prior_was_close => {
                        self.accept_loaded_effect(body, &stored)?;
                        effect_records += 1;
                    }
                    StoredKind::Effect(_) => return Err(AuditError::CorruptRecord),
                }
                expected_previous = stored.record_hash;
            }
            if !is_last && !prior_was_close {
                return Err(AuditError::MissingSegment);
            }
            if is_last && prior_was_close {
                return Err(AuditError::MissingSegment);
            }
            if is_last {
                self.current_file = file;
                self.current_length = valid_length;
                self.current_effect_records = effect_records;
            }
        }
        self.previous_hash = expected_previous;
        self.current_segment = *indices.last().ok_or(AuditError::MissingSegment)?;
        self.current_file
            .seek(SeekFrom::End(0))
            .map_err(|_| AuditError::PathUnavailable)?;
        Ok(())
    }

    fn accept_loaded_effect(
        &mut self,
        body: &EffectBody,
        stored: &StoredRecord,
    ) -> Result<(), AuditError> {
        validate_effect_body(body).map_err(|_| AuditError::CorruptRecord)?;
        let record = record_from_body(
            body,
            stored.previous_hash.clone(),
            stored.record_hash.clone(),
        );
        match body.state {
            AuditState::Intent => {
                if self.active.contains_key(&body.effect_id)
                    || self.terminal.contains(&body.effect_id)
                {
                    return Err(AuditError::ReplayedEffect);
                }
                self.active.insert(body.effect_id.clone(), record.clone());
            }
            AuditState::Completed | AuditState::OutcomeUnknown => {
                let intent = self
                    .active
                    .remove(&body.effect_id)
                    .ok_or(AuditError::CorruptRecord)?;
                if self.terminal.contains(&body.effect_id)
                    || !same_effect_metadata(&intent, &record)
                {
                    return Err(AuditError::ReplayedEffect);
                }
                self.terminal.insert(body.effect_id.clone());
            }
        }
        self.records.push(record);
        Ok(())
    }

    fn recover_unknown_outcomes(&mut self) -> Result<(), AuditError> {
        let pending = self.active.values().cloned().collect::<Vec<_>>();
        for intent in pending {
            let effect_id = intent.effect_id.clone();
            let body = body_from_record(
                &intent,
                AuditState::OutcomeUnknown,
                Some("outcome_unknown".into()),
            );
            self.append_effect(body)?;
            self.active.remove(&effect_id);
            self.terminal.insert(effect_id);
        }
        Ok(())
    }

    fn append_effect(&mut self, body: EffectBody) -> Result<EffectAuditRecord, AuditError> {
        if self.poisoned {
            return Err(AuditError::Unavailable);
        }
        let result = self.append_effect_unpoisoned(body);
        if matches!(
            result,
            Err(AuditError::PathUnavailable | AuditError::SyncFailed | AuditError::RetentionLimit)
        ) {
            self.poisoned = true;
        }
        result
    }

    fn append_effect_unpoisoned(
        &mut self,
        body: EffectBody,
    ) -> Result<EffectAuditRecord, AuditError> {
        validate_effect_body(&body)?;
        let candidate =
            StoredRecord::new(StoredKind::Effect(body.clone()), self.previous_hash.clone())?;
        let encoded = encode_record(&candidate)?;
        if self.current_effect_records > 0
            && self.current_length.saturating_add(encoded.len() as u64)
                > self.options.max_segment_bytes
        {
            self.rotate()?;
        }
        let stored =
            StoredRecord::new(StoredKind::Effect(body.clone()), self.previous_hash.clone())?;
        self.append_encoded(&stored)?;
        self.sync_file()?;
        self.current_effect_records += 1;
        let record = record_from_body(&body, stored.previous_hash, stored.record_hash);
        self.records.push(record.clone());
        Ok(record)
    }

    #[cfg(test)]
    pub(crate) fn fail_next_durability_for_test(&mut self, failure: AuditFailurePoint) {
        self.options.failure = Some(failure);
    }

    fn rotate(&mut self) -> Result<(), AuditError> {
        let next = self
            .current_segment
            .checked_add(1)
            .ok_or(AuditError::RecordTooLarge)?;
        if next >= self.options.max_segments {
            return Err(AuditError::RetentionLimit);
        }
        self.append_stored(StoredKind::Close {
            segment_index: self.current_segment,
            next_segment_index: next,
        })?;
        self.sync_file()?;
        self.rotation_anchors += 1;

        let next_path = self.owner.segment_file(next);
        let file = create_private_segment(&next_path)?;
        self.sync_directory()?;
        self.current_file = file;
        self.current_segment = next;
        self.current_length = 0;
        self.current_effect_records = 0;
        self.append_stored(StoredKind::Open {
            segment_index: next,
            previous_segment_hash: self.previous_hash.clone(),
            target_key_version: TARGET_KEY_VERSION,
            target_key_digest: sha256_hex(&self.target_key),
        })?;
        self.sync_file()?;
        self.rotation_anchors += 1;
        Ok(())
    }

    fn append_stored(&mut self, kind: StoredKind) -> Result<(), AuditError> {
        let stored = StoredRecord::new(kind, self.previous_hash.clone())?;
        self.append_encoded(&stored)
    }

    fn append_encoded(&mut self, stored: &StoredRecord) -> Result<(), AuditError> {
        if self.options.failure == Some(AuditFailurePoint::Append) {
            self.options.failure = None;
            return Err(AuditError::PathUnavailable);
        }
        let encoded = encode_record(stored)?;
        self.current_file
            .write_all(&encoded)
            .map_err(|_| AuditError::PathUnavailable)?;
        self.current_length = self.current_length.saturating_add(encoded.len() as u64);
        self.previous_hash = stored.record_hash.clone();
        Ok(())
    }

    fn sync_file(&self) -> Result<(), AuditError> {
        sync_file(&self.current_file, self.options.failure)
    }

    fn sync_directory(&self) -> Result<(), AuditError> {
        sync_directory(self.owner.directory(), self.options.failure)
    }

    fn ensure_initialization_marker(&self) -> Result<(), AuditError> {
        let path = self.owner.initialization_marker();
        if !initialization_marker_exists(&self.owner)? {
            crate::fs::private_atomic_create_sync(&path, INITIALIZATION_MARKER)
                .map_err(|_| AuditError::PathUnavailable)?;
        }
        let marker =
            open_private_rw(&path, false, false).map_err(|_| AuditError::PathUnavailable)?;
        sync_file(&marker, self.options.failure)?;
        sync_directory(&self.owner.state_root(), self.options.failure)
    }
}

fn report_open_stage(stage: &'static str, error: AuditError) -> AuditError {
    #[cfg(test)]
    eprintln!("EFFECT_AUDIT_OPEN_FAILED={stage}");
    #[cfg(not(test))]
    let _ = stage;
    error
}

fn body_from_record(
    record: &EffectAuditRecord,
    state: AuditState,
    result_code: Option<String>,
) -> EffectBody {
    EffectBody {
        effect_id: record.effect_id.clone(),
        invocation_id: record.invocation_id.clone(),
        grant_id: record.grant_id.clone(),
        sequence: record.sequence,
        timestamp_ms: record.timestamp_ms,
        artifact_id: record.artifact_id.clone(),
        export: record.export.clone(),
        capability: record.capability.clone(),
        normalized_target: record.normalized_target.clone(),
        state,
        decision: record.decision.clone(),
        result_code,
    }
}

fn record_from_body(
    body: &EffectBody,
    previous_hash: String,
    record_hash: String,
) -> EffectAuditRecord {
    EffectAuditRecord {
        effect_id: body.effect_id.clone(),
        invocation_id: body.invocation_id.clone(),
        grant_id: body.grant_id.clone(),
        sequence: body.sequence,
        timestamp_ms: body.timestamp_ms,
        artifact_id: body.artifact_id.clone(),
        export: body.export.clone(),
        capability: body.capability.clone(),
        normalized_target: body.normalized_target.clone(),
        state: body.state,
        decision: body.decision.clone(),
        result_code: body.result_code.clone(),
        previous_hash,
        record_hash,
    }
}

fn same_effect_metadata(left: &EffectAuditRecord, right: &EffectAuditRecord) -> bool {
    left.effect_id == right.effect_id
        && left.invocation_id == right.invocation_id
        && left.grant_id == right.grant_id
        && left.sequence == right.sequence
        && left.timestamp_ms == right.timestamp_ms
        && left.artifact_id == right.artifact_id
        && left.export == right.export
        && left.capability == right.capability
        && left.normalized_target == right.normalized_target
        && left.decision == right.decision
}

fn validate_effect_body(body: &EffectBody) -> Result<(), AuditError> {
    validate_identifier(&body.effect_id)?;
    validate_identifier(&body.invocation_id)?;
    validate_identifier(&body.grant_id)?;
    if body.sequence == 0 || body.timestamp_ms < 0 {
        return Err(AuditError::InvalidMetadata);
    }
    validate_optional_identifier(body.artifact_id.as_deref())?;
    validate_optional_identifier(body.export.as_deref())?;
    if !matches!(
        body.capability.as_str(),
        "read_file" | "write_file" | "fetch" | "spawn" | "propose_skill"
    ) || body.decision != "authorized"
    {
        return Err(AuditError::InvalidMetadata);
    }
    validate_target(&body.capability, &body.normalized_target)?;
    match body.state {
        AuditState::Intent if body.result_code.is_none() => Ok(()),
        AuditState::Completed
            if matches!(
                body.result_code.as_deref(),
                Some(
                    "succeeded"
                        | "denied"
                        | "cancelled"
                        | "timed_out"
                        | "output_limit"
                        | "backend_failure"
                )
            ) =>
        {
            Ok(())
        }
        AuditState::OutcomeUnknown if body.result_code.as_deref() == Some("outcome_unknown") => {
            Ok(())
        }
        _ => Err(AuditError::InvalidMetadata),
    }
}

fn validate_target(capability: &str, target: &SanitizedTarget) -> Result<(), AuditError> {
    match (&target.kind, capability) {
        (
            SanitizedTargetKind::File {
                key_version,
                storage_class,
                target_tag,
            },
            "read_file" | "write_file",
        ) if *key_version == TARGET_KEY_VERSION
            && storage_class == "workspace"
            && valid_hash(target_tag) =>
        {
            Ok(())
        }
        (
            SanitizedTargetKind::Fetch {
                key_version,
                scheme,
                effective_port,
                host_tag,
                path_query_tag,
                method,
            },
            "fetch",
        ) if *key_version == TARGET_KEY_VERSION
            && matches!(scheme.as_str(), "http" | "https")
            && *effective_port != 0
            && valid_hash(host_tag)
            && valid_hash(path_query_tag)
            && matches!(method.as_str(), "get" | "post") =>
        {
            Ok(())
        }
        (
            SanitizedTargetKind::Spawn {
                key_version,
                executable_tag,
            },
            "spawn",
        ) if *key_version == TARGET_KEY_VERSION && valid_hash(executable_tag) => Ok(()),
        (SanitizedTargetKind::Proposal, "propose_skill") => Ok(()),
        _ => Err(AuditError::InvalidMetadata),
    }
}

fn validate_identifier(value: &str) -> Result<(), AuditError> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+' | b':')
        })
    {
        return Err(AuditError::InvalidMetadata);
    }
    Ok(())
}

fn validate_optional_identifier(value: Option<&str>) -> Result<(), AuditError> {
    value.map_or(Ok(()), validate_identifier)
}

fn encode_record(record: &StoredRecord) -> Result<Vec<u8>, AuditError> {
    let body = serde_json::to_vec(record).map_err(|_| AuditError::InvalidMetadata)?;
    if body.len() > MAX_RECORD_BYTES {
        return Err(AuditError::RecordTooLarge);
    }
    let length = u32::try_from(body.len()).map_err(|_| AuditError::RecordTooLarge)?;
    let mut encoded = Vec::with_capacity(body.len() + 8);
    encoded.extend_from_slice(&length.to_be_bytes());
    encoded.extend_from_slice(&body);
    encoded.extend_from_slice(&length.to_be_bytes());
    Ok(encoded)
}

fn read_segment(
    file: &mut File,
    is_last: bool,
    maximum_bytes: u64,
) -> Result<(Vec<StoredRecord>, u64, bool), AuditError> {
    if file
        .metadata()
        .map_err(|_| AuditError::PathUnavailable)?
        .len()
        > maximum_bytes
    {
        return Err(AuditError::CorruptRecord);
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|_| AuditError::PathUnavailable)?;
    let mut bytes = Vec::new();
    file.take(maximum_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| AuditError::PathUnavailable)?;
    let mut offset = 0_usize;
    let mut entries = Vec::new();
    while offset < bytes.len() {
        if bytes.len() - offset < 4 {
            return if is_last {
                Ok((entries, offset as u64, true))
            } else {
                Err(AuditError::CorruptRecord)
            };
        }
        let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        if length == 0 || length > MAX_RECORD_BYTES {
            return Err(AuditError::CorruptRecord);
        }
        let body_end = offset
            .checked_add(4)
            .and_then(|value| value.checked_add(length))
            .ok_or(AuditError::CorruptRecord)?;
        let frame_end = body_end.checked_add(4).ok_or(AuditError::CorruptRecord)?;
        if frame_end > bytes.len() {
            return if is_last {
                if complete_frame_has_mismatched_prefix(&bytes[offset..]) {
                    Err(AuditError::CorruptRecord)
                } else {
                    Ok((entries, offset as u64, true))
                }
            } else {
                Err(AuditError::CorruptRecord)
            };
        }
        let trailing_length = u32::from_be_bytes(
            bytes[body_end..frame_end]
                .try_into()
                .map_err(|_| AuditError::CorruptRecord)?,
        ) as usize;
        if trailing_length != length {
            return Err(AuditError::CorruptRecord);
        }
        let record = serde_json::from_slice::<StoredRecord>(&bytes[offset + 4..body_end])
            .map_err(|_| AuditError::CorruptRecord)?;
        entries.push(record);
        offset = frame_end;
    }
    Ok((entries, offset as u64, false))
}

fn complete_frame_has_mismatched_prefix(bytes: &[u8]) -> bool {
    if bytes.len() < 8 {
        return false;
    }
    let Ok(trailer) = <[u8; 4]>::try_from(&bytes[bytes.len() - 4..]) else {
        return false;
    };
    let length = u32::from_be_bytes(trailer) as usize;
    if length == 0 || length > MAX_RECORD_BYTES || length.saturating_add(8) != bytes.len() {
        return false;
    }
    serde_json::from_slice::<StoredRecord>(&bytes[4..bytes.len() - 4])
        .is_ok_and(|record| record.validate_hash().is_ok())
}

fn segment_indices(owner: &EffectAuditPathOwner) -> Result<Vec<u64>, AuditError> {
    let mut indices = Vec::new();
    for entry in std::fs::read_dir(owner.directory()).map_err(|_| AuditError::PathUnavailable)? {
        let entry = entry.map_err(|_| AuditError::PathUnavailable)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(AuditError::CorruptRecord);
        };
        if matches!(name, "writer.lock" | "target-hmac-v1.key") {
            continue;
        }
        let Some(index) = name
            .strip_prefix("segment-")
            .and_then(|value| value.strip_suffix(".audit"))
            .and_then(|value| value.parse::<u64>().ok())
        else {
            return Err(AuditError::CorruptRecord);
        };
        if name != format!("segment-{index:020}.audit") {
            return Err(AuditError::CorruptRecord);
        }
        if entry
            .file_type()
            .map_err(|_| AuditError::PathUnavailable)?
            .is_symlink()
        {
            return Err(AuditError::PathUnavailable);
        }
        indices.push(index);
    }
    indices.sort_unstable();
    Ok(indices)
}

fn validate_contiguous_indices(indices: &[u64]) -> Result<(), AuditError> {
    if indices.first() != Some(&0)
        || indices
            .iter()
            .copied()
            .enumerate()
            .any(|(expected, actual)| actual != expected as u64)
    {
        return Err(AuditError::MissingSegment);
    }
    Ok(())
}

fn create_private_segment(path: &Path) -> Result<File, AuditError> {
    open_private_rw(path, true, true).map_err(|_| AuditError::PathUnavailable)
}

fn load_or_create_target_key(
    owner: &EffectAuditPathOwner,
    may_create: bool,
    failure: Option<AuditFailurePoint>,
) -> Result<[u8; TARGET_KEY_BYTES], AuditError> {
    let path = owner.target_key_file();
    match std::fs::symlink_metadata(&path) {
        Ok(_) => read_target_key(&path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && may_create => {
            let mut key = [0_u8; TARGET_KEY_BYTES];
            key[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
            key[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
            crate::fs::private_atomic_create_sync(&path, &key)
                .map_err(|_| AuditError::KeyUnavailable)?;
            let key_file =
                open_private_rw(&path, false, false).map_err(|_| AuditError::KeyUnavailable)?;
            sync_file(&key_file, failure)?;
            sync_directory(owner.directory(), failure)?;
            read_target_key(&path)
        }
        Err(_) => Err(AuditError::KeyUnavailable),
    }
}

fn read_target_key(path: &Path) -> Result<[u8; TARGET_KEY_BYTES], AuditError> {
    let mut file = crate::fs::open_private_file(path).map_err(|_| AuditError::KeyUnavailable)?;
    let mut key = [0_u8; TARGET_KEY_BYTES];
    file.read_exact(&mut key)
        .map_err(|_| AuditError::KeyUnavailable)?;
    let mut extra = [0_u8; 1];
    if file
        .read(&mut extra)
        .map_err(|_| AuditError::KeyUnavailable)?
        != 0
    {
        return Err(AuditError::KeyUnavailable);
    }
    Ok(key)
}

fn initialization_marker_exists(owner: &EffectAuditPathOwner) -> Result<bool, AuditError> {
    let path = owner.initialization_marker();
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            let marker =
                crate::fs::open_private_file(&path).map_err(|_| AuditError::PathUnavailable)?;
            let mut bytes = Vec::new();
            marker
                .take((INITIALIZATION_MARKER.len() + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(|_| AuditError::PathUnavailable)?;
            if bytes != INITIALIZATION_MARKER {
                return Err(AuditError::CorruptRecord);
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(AuditError::PathUnavailable),
    }
}

fn prepare_owner_directory(
    owner: &EffectAuditPathOwner,
    failure: Option<AuditFailurePoint>,
) -> Result<(), AuditError> {
    crate::fs::ensure_private_directory(&owner.state_root())
        .map_err(|_| AuditError::PathUnavailable)?;
    let state_parent = owner
        .state_root()
        .parent()
        .map(Path::to_path_buf)
        .ok_or(AuditError::PathUnavailable)?;
    let audit_parent = owner
        .directory()
        .parent()
        .ok_or(AuditError::PathUnavailable)?;
    crate::fs::ensure_private_directory(audit_parent).map_err(|_| AuditError::PathUnavailable)?;
    crate::paths::ensure_no_link_traversal(&owner.state_root(), owner.directory())
        .map_err(|_| AuditError::PathUnavailable)?;
    crate::fs::ensure_private_directory(owner.directory())
        .map_err(|_| AuditError::PathUnavailable)?;
    crate::paths::ensure_no_link_traversal(&owner.state_root(), owner.directory())
        .map_err(|_| AuditError::PathUnavailable)?;
    sync_directory(&state_parent, failure)?;
    sync_directory(audit_parent, failure)?;
    sync_directory(&owner.state_root(), failure)
}

fn open_private_rw(path: &Path, create: bool, create_new: bool) -> std::io::Result<File> {
    if create
        && !create_new
        && std::fs::symlink_metadata(path).is_err()
        && let Err(error) = crate::fs::private_atomic_create_sync(path, b"")
        && error.kind() != std::io::ErrorKind::AlreadyExists
    {
        return Err(error);
    }
    if create_new {
        crate::fs::private_atomic_create_sync(path, b"")?;
    } else if path.exists() {
        drop(crate::fs::open_private_file(path)?);
    }
    #[cfg(target_os = "macos")]
    let before = crate::fs::open_private_file(path)?;
    #[cfg(not(target_os = "macos"))]
    let before = crate::fs::checked_path_metadata(path)?;
    #[cfg(target_os = "macos")]
    let before_is_file = before.metadata()?.is_file();
    #[cfg(not(target_os = "macos"))]
    let before_is_file = !before.file_type().is_symlink() && before.is_file();
    if !before_is_file {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "audit file is not a regular file",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        const NOFOLLOW: i32 = if cfg!(target_os = "macos") {
            0x100
        } else {
            0x2_0000
        };
        options.custom_flags(NOFOLLOW);
    }
    let file = options.open(path)?;
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::MetadataExt;

        let before = before.metadata()?;
        let opened = file.metadata()?;
        let after = crate::fs::open_private_file(path)?.metadata()?;
        if before.dev() != opened.dev()
            || before.ino() != opened.ino()
            || opened.dev() != after.dev()
            || opened.ino() != after.ino()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "audit path changed during private open",
            ));
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let opened = crate::fs::checked_file_metadata(&file)?;
        let after = crate::fs::checked_path_metadata(path)?;
        crate::fs::ensure_same_file(path, &before, &opened)?;
        crate::fs::ensure_same_file(path, &opened, &after)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn sync_file(file: &File, failure: Option<AuditFailurePoint>) -> Result<(), AuditError> {
    if failure == Some(AuditFailurePoint::FileSync) {
        return Err(AuditError::SyncFailed);
    }
    file.sync_all().map_err(|_| AuditError::SyncFailed)
}

#[cfg(unix)]
fn sync_directory(path: &Path, failure: Option<AuditFailurePoint>) -> Result<(), AuditError> {
    if failure == Some(AuditFailurePoint::DirectorySync) {
        return Err(AuditError::SyncFailed);
    }
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| AuditError::SyncFailed)
}

#[cfg(windows)]
fn sync_directory(path: &Path, failure: Option<AuditFailurePoint>) -> Result<(), AuditError> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    if failure == Some(AuditFailurePoint::DirectorySync) {
        return Err(AuditError::SyncFailed);
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| AuditError::SyncFailed)
}

#[cfg(not(any(unix, windows)))]
fn sync_directory(_path: &Path, _failure: Option<AuditFailurePoint>) -> Result<(), AuditError> {
    Err(AuditError::SyncFailed)
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn try_lock_exclusive(file: &File) -> Result<(), AuditError> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn flock(fd: std::os::raw::c_int, operation: std::os::raw::c_int) -> std::os::raw::c_int;
    }
    loop {
        // SAFETY: the descriptor is valid and remains owned by `file` for the call.
        if unsafe { flock(file.as_raw_fd(), 2 | 4) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return if error.kind() == std::io::ErrorKind::WouldBlock {
            Err(AuditError::WriterLocked)
        } else {
            Err(AuditError::PathUnavailable)
        };
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn unlock(file: &File) {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn flock(fd: std::os::raw::c_int, operation: std::os::raw::c_int) -> std::os::raw::c_int;
    }
    // SAFETY: the descriptor remains valid while its advisory lock is released.
    let _ = unsafe { flock(file.as_raw_fd(), 8) };
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn try_lock_exclusive(file: &File) -> Result<(), AuditError> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: *mut c_void,
    }
    unsafe extern "system" {
        fn LockFileEx(
            file: *mut c_void,
            flags: u32,
            reserved: u32,
            low: u32,
            high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
    }
    let mut overlapped = Overlapped {
        internal: 0,
        internal_high: 0,
        offset: 0,
        offset_high: 0,
        event: std::ptr::null_mut(),
    };
    // SAFETY: the handle remains valid and `overlapped` lives for the synchronous call.
    if unsafe {
        LockFileEx(
            file.as_raw_handle(),
            0x2 | 0x1,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    } != 0
    {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(33 | 158)) {
            Err(AuditError::WriterLocked)
        } else {
            Err(AuditError::PathUnavailable)
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn unlock(file: &File) {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: *mut c_void,
    }
    unsafe extern "system" {
        fn UnlockFileEx(
            file: *mut c_void,
            reserved: u32,
            low: u32,
            high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
    }
    let mut overlapped = Overlapped {
        internal: 0,
        internal_high: 0,
        offset: 0,
        offset_high: 0,
        event: std::ptr::null_mut(),
    };
    // SAFETY: the handle and `overlapped` remain valid for the synchronous unlock call.
    let _ = unsafe { UnlockFileEx(file.as_raw_handle(), 0, u32::MAX, u32::MAX, &mut overlapped) };
}

#[cfg(not(any(unix, windows)))]
fn try_lock_exclusive(_file: &File) -> Result<(), AuditError> {
    Err(AuditError::PathUnavailable)
}

#[cfg(not(any(unix, windows)))]
fn unlock(_file: &File) {}

fn sha256_hex(bytes: &[u8]) -> String {
    hex_digest(Sha256::digest(bytes))
}

fn split_host_port(authority: &str) -> Result<(String, Option<u16>), AuditError> {
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let close = bracketed.find(']').ok_or(AuditError::InvalidMetadata)?;
        let host = &bracketed[..close];
        let suffix = &bracketed[close + 1..];
        let port = if suffix.is_empty() {
            None
        } else {
            Some(
                suffix
                    .strip_prefix(':')
                    .ok_or(AuditError::InvalidMetadata)?
                    .parse::<u16>()
                    .map_err(|_| AuditError::InvalidMetadata)?,
            )
        };
        (host, port)
    } else if let Some((host, possible_port)) = authority.rsplit_once(':') {
        if possible_port.bytes().all(|byte| byte.is_ascii_digit()) {
            (
                host,
                Some(
                    possible_port
                        .parse::<u16>()
                        .map_err(|_| AuditError::InvalidMetadata)?,
                ),
            )
        } else {
            (authority, None)
        }
    } else {
        (authority, None)
    };
    if host.is_empty() || host.chars().any(char::is_whitespace) {
        return Err(AuditError::InvalidMetadata);
    }
    Ok((host.to_ascii_lowercase(), port))
}

fn target_tag(
    key: &[u8; TARGET_KEY_BYTES],
    operation: &str,
    metadata_kind: &str,
    canonical_target: &[u8],
) -> String {
    let mut message =
        Vec::with_capacity(operation.len() + metadata_kind.len() + canonical_target.len() + 24);
    for value in [
        operation.as_bytes(),
        metadata_kind.as_bytes(),
        canonical_target,
    ] {
        message.extend_from_slice(&(value.len() as u64).to_be_bytes());
        message.extend_from_slice(value);
    }
    hex_digest(hmac_sha256(key, &message))
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK_BYTES: usize = 64;
    let mut padded_key = [0_u8; BLOCK_BYTES];
    if key.len() > BLOCK_BYTES {
        padded_key[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        padded_key[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36_u8; BLOCK_BYTES];
    let mut outer_pad = [0x5c_u8; BLOCK_BYTES];
    for index in 0..BLOCK_BYTES {
        inner_pad[index] ^= padded_key[index];
        outer_pad[index] ^= padded_key[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_digest);
    outer.finalize().into()
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    use std::fmt::Write as _;
    let bytes = digest.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::{hex_digest, hmac_sha256};

    #[test]
    fn js_effect_audit_storage_hmac_sha256_matches_rfc_4231() {
        let key = [0x0b_u8; 20];
        assert_eq!(
            hex_digest(hmac_sha256(&key, b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }
}
