//! Versioned, bounded, in-memory record/replay host fakes for skill verification.
//!
//! This module provides deterministic fake implementations of file I/O, process spawning,
//! and network operations for use during skill verification. Fakes are:
//! - Versioned: updates change the version token
//! - Bounded: memory and operation counts are capped
//! - Record/replay: all I/O is recorded and can be inspected
//! - Virtual: they never access the real filesystem, network, or process launcher
//! - Scoped: state resets for each artifact verification
//! - Fixture-only: every effect a case exercises must be declared as a fixture. An
//!   undeclared read, spawn, or fetch fails and is recorded as a failed attempt; the
//!   fakes never invent a response, because a verification that passes against an
//!   invented response is not evidence.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::extras::js::skills::{CapabilityManifest, HostCapability};

/// Version of the fake host implementation. Bumping this invalidates existing
/// verification reports.
///
/// Version 4 made an unfixtured `spawn` and an unfixtured `fetch` fail the way an
/// unseeded `read_file` already did. Earlier versions answered an undeclared effect
/// with a synthetic success (`simulated <program> completed` with exit code 0, and an
/// HTTP 200 JSON body), so a candidate that shelled out to a real executable or
/// depended on a real HTTP response could pass on a response the harness invented.
/// Reports produced under those versions are not evidence and must not be reused.
pub const FAKES_VERSION: u32 = 4;

/// Maximum total size of all virtual files in bytes.
const FAKES_TOTAL_FILE_BYTES: usize = 10 * 1024 * 1024; // 10 MiB

/// Maximum number of spawn operations.
const FAKES_MAX_SPAWN_OPERATIONS: usize = 1_000;

/// Maximum number of fetch operations.
const FAKES_MAX_FETCH_OPERATIONS: usize = 1_000;

/// Worker results must remain bounded independently of the number or size of fake effects.
pub(crate) const VERIFICATION_TRANSCRIPT_MAX_CALLS: usize = 256;
pub(crate) const VERIFICATION_TRANSCRIPT_MAX_BYTES: usize = 512 * 1024;
const VERIFICATION_TRANSCRIPT_MAX_VALUE_BYTES: usize = 4 * 1024;
const TRANSCRIPT_RECORD_FIXED_WIRE_BYTES: usize = 256;
const TRANSCRIPT_LIMIT_ERROR: &str = "fake transcript limit exceeded";

#[derive(Debug, Default)]
struct VerificationTranscriptBudgetState {
    calls: usize,
    bytes: usize,
    exceeded: bool,
}

/// Whole-request budget shared by otherwise isolated per-case fake hosts.
///
/// Reservations use a conservative upper bound for JSON escaping and happen before transcript
/// values are cloned. This keeps the complete terminal verification frame comfortably below the
/// protocol frame limit even when an effect supplies adversarial strings.
#[derive(Clone, Debug)]
pub(crate) struct VerificationTranscriptBudget {
    state: Arc<Mutex<VerificationTranscriptBudgetState>>,
}

impl VerificationTranscriptBudget {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(VerificationTranscriptBudgetState::default())),
        }
    }

    fn reserve(&self, wire_bytes: usize) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        let next_calls = state.calls.saturating_add(1);
        let next_bytes = state.bytes.saturating_add(wire_bytes);
        if state.exceeded
            || next_calls > VERIFICATION_TRANSCRIPT_MAX_CALLS
            || next_bytes > VERIFICATION_TRANSCRIPT_MAX_BYTES
        {
            state.exceeded = true;
            return Err(TRANSCRIPT_LIMIT_ERROR.to_string());
        }
        state.calls = next_calls;
        state.bytes = next_bytes;
        Ok(())
    }

    pub(crate) fn exceeded(&self) -> bool {
        self.state.lock().unwrap().exceeded
    }
}

/// The failure an undeclared spawn produces, naming the exact command that had no fixture.
///
/// Verification has no real process launcher, so the only honest answer to a command the
/// verifier was never given a response for is "missing fixture". The program and its exact
/// argument vector are named so a skill author can declare the fixture the case needs.
fn missing_spawn_fixture(program: &str, args: &[String]) -> String {
    format!("Spawn fixture not found: {program} {args:?}")
}

/// The failure an undeclared fetch produces, naming the exact request that had no fixture.
///
/// The method is the normalized (uppercase) one the fixture table is keyed by, so the message
/// names the request as the fixture must declare it.
fn missing_fetch_fixture(method: &str, url: &str) -> String {
    format!("Fetch fixture not found: {method} {url}")
}

fn string_wire_upper_bound(value: &str) -> usize {
    value.len().saturating_mul(6).saturating_add(2)
}

fn strings_wire_upper_bound<'a>(values: impl IntoIterator<Item = &'a str>) -> usize {
    values.into_iter().fold(0usize, |total, value| {
        total.saturating_add(string_wire_upper_bound(value))
    })
}

/// A record of a fake read_file operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FakeReadRecord {
    pub path: String,
    pub result: Result<String, String>,
}

/// A record of a fake write_file operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FakeWriteRecord {
    pub path: String,
    pub content: String,
    pub result: Result<(), String>,
}

/// A record of a fake spawn operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FakeSpawnRecord {
    pub program: String,
    pub args: Vec<String>,
    pub result: Result<String, String>, // JSON serialized SpawnResult
}

/// A record of a fake fetch operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FakeFetchRecord {
    pub url: String,
    pub method: String,
    pub result: Result<String, String>, // JSON response
}

/// Deterministic result returned by a verifier-owned spawn fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FakeSpawnResponse {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

/// Exact spawn request and its hidden held-out response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FakeSpawnFixture {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub response: FakeSpawnResponse,
}

/// Deterministic result returned by a verifier-owned fetch fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FakeFetchResponse {
    pub status: u16,
    pub body: String,
}

/// Exact fetch request and its hidden held-out response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FakeFetchFixture {
    pub url: String,
    pub method: String,
    pub response: FakeFetchResponse,
}

/// Transcript of all I/O operations during verification.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FakeTranscript {
    pub reads: Vec<FakeReadRecord>,
    pub writes: Vec<FakeWriteRecord>,
    pub spawns: Vec<FakeSpawnRecord>,
    pub fetches: Vec<FakeFetchRecord>,
}

impl FakeTranscript {
    /// Whether this transcript is empty (no operations).
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.reads.is_empty()
            && self.writes.is_empty()
            && self.spawns.is_empty()
            && self.fetches.is_empty()
    }

    /// Return a wire-safe transcript. Callers must separately reject a transcript whose
    /// operation counts exceed `VERIFICATION_TRANSCRIPT_MAX_CALLS` so truncation cannot turn
    /// an over-limit transcript into an apparent expectation match.
    pub(crate) fn bounded_for_wire(mut self) -> Self {
        let mut remaining = VERIFICATION_TRANSCRIPT_MAX_CALLS;
        self.limit_call_count(&mut remaining);

        for record in &mut self.reads {
            truncate_utf8(&mut record.path, VERIFICATION_TRANSCRIPT_MAX_VALUE_BYTES);
            truncate_result(&mut record.result);
        }
        for record in &mut self.writes {
            truncate_utf8(&mut record.path, VERIFICATION_TRANSCRIPT_MAX_VALUE_BYTES);
            truncate_utf8(&mut record.content, VERIFICATION_TRANSCRIPT_MAX_VALUE_BYTES);
            if let Err(error) = &mut record.result {
                truncate_utf8(error, VERIFICATION_TRANSCRIPT_MAX_VALUE_BYTES);
            }
        }
        for record in &mut self.spawns {
            truncate_utf8(&mut record.program, VERIFICATION_TRANSCRIPT_MAX_VALUE_BYTES);
            record.args.truncate(64);
            for arg in &mut record.args {
                truncate_utf8(arg, VERIFICATION_TRANSCRIPT_MAX_VALUE_BYTES);
            }
            truncate_result(&mut record.result);
        }
        for record in &mut self.fetches {
            truncate_utf8(&mut record.url, VERIFICATION_TRANSCRIPT_MAX_VALUE_BYTES);
            truncate_utf8(&mut record.method, VERIFICATION_TRANSCRIPT_MAX_VALUE_BYTES);
            truncate_result(&mut record.result);
        }
        self
    }

    pub(crate) fn exceeds_wire_call_limit(&self) -> bool {
        self.call_count() > VERIFICATION_TRANSCRIPT_MAX_CALLS
    }

    pub(crate) fn call_count(&self) -> usize {
        self.reads
            .len()
            .saturating_add(self.writes.len())
            .saturating_add(self.spawns.len())
            .saturating_add(self.fetches.len())
    }

    pub(crate) fn limit_call_count(&mut self, remaining: &mut usize) {
        limit_records(&mut self.reads, remaining);
        limit_records(&mut self.writes, remaining);
        limit_records(&mut self.spawns, remaining);
        limit_records(&mut self.fetches, remaining);
    }
}

fn limit_records<T>(records: &mut Vec<T>, remaining: &mut usize) {
    records.truncate(*remaining);
    *remaining = remaining.saturating_sub(records.len());
}

fn truncate_result(result: &mut Result<String, String>) {
    match result {
        Ok(value) | Err(value) => truncate_utf8(value, VERIFICATION_TRANSCRIPT_MAX_VALUE_BYTES),
    }
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}

/// Mutable state for fake host operations during one verification.
struct FakeState {
    /// Virtual file system: path -> content.
    files: HashMap<String, String>,
    /// Total bytes of all files.
    total_file_bytes: usize,
    spawn_fixtures: HashMap<(String, Vec<String>), FakeSpawnResponse>,
    fetch_fixtures: HashMap<(String, String), FakeFetchResponse>,
    /// Transcript of all operations.
    transcript: FakeTranscript,
}

impl FakeState {
    fn new() -> Self {
        Self {
            files: HashMap::new(),
            total_file_bytes: 0,
            spawn_fixtures: HashMap::new(),
            fetch_fixtures: HashMap::new(),
            transcript: FakeTranscript::default(),
        }
    }

    fn write_file(&mut self, path: &str, content: &str) -> Result<(), String> {
        let new_size = content.len();
        let old_size = self.files.get(path).map(|c| c.len()).unwrap_or(0);

        // Use saturating arithmetic to avoid underflow
        let new_total = if self.total_file_bytes >= old_size {
            self.total_file_bytes - old_size + new_size
        } else {
            // If old_size somehow exceeds total, just use new_size
            new_size
        };

        if new_total > FAKES_TOTAL_FILE_BYTES {
            return Err(format!(
                "Write would exceed file limit: {} > {} bytes",
                new_total, FAKES_TOTAL_FILE_BYTES
            ));
        }

        self.total_file_bytes = new_total;
        self.files.insert(path.to_string(), content.to_string());
        Ok(())
    }
}

/// Verifier-owned deterministic fake hosts for Tier 1/2 skills.
///
/// Each fake is scoped to one artifact verification. State is fresh for each call
/// to `verify_skill()` and for each mutation pass. Unsupported capabilities fail
/// deterministically. Embedded tests cannot inspect, replace, or spy on fakes.
#[derive(Clone)]
pub struct FakeHostGlobals {
    state: Arc<Mutex<FakeState>>,
    transcript_budget: VerificationTranscriptBudget,
    pub manifest: CapabilityManifest,
}

impl FakeHostGlobals {
    pub(crate) fn with_transcript_budget(
        manifest: CapabilityManifest,
        transcript_budget: VerificationTranscriptBudget,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeState::new())),
            transcript_budget,
            manifest,
        }
    }

    /// Read the immutable transcript. Embedded tests cannot inspect this.
    pub fn transcript(&self) -> FakeTranscript {
        self.state.lock().unwrap().transcript.clone()
    }

    /// Seed verifier-owned virtual data without recording an operation.
    ///
    /// Held-out fixtures use this to provide hidden deterministic responses. The
    /// proposal runtime cannot access this handle or inspect the seeded bytes.
    pub(crate) fn seed_file(&self, path: &str, content: &str) -> Result<(), String> {
        if !self.manifest.allows(HostCapability::ReadFile) {
            return Err("read_file not declared in capability manifest".to_string());
        }
        self.state.lock().unwrap().write_file(path, content)
    }

    pub(crate) fn seed_spawn(&self, fixture: &FakeSpawnFixture) -> Result<(), String> {
        if !self.manifest.allows(HostCapability::Spawn) {
            return Err("spawn not declared in capability manifest".to_string());
        }
        let key = (fixture.program.clone(), fixture.args.clone());
        if self
            .state
            .lock()
            .unwrap()
            .spawn_fixtures
            .insert(key, fixture.response.clone())
            .is_some()
        {
            return Err("duplicate spawn fixture".to_string());
        }
        Ok(())
    }

    pub(crate) fn seed_fetch(&self, fixture: &FakeFetchFixture) -> Result<(), String> {
        if !self.manifest.allows(HostCapability::Fetch) {
            return Err("fetch not declared in capability manifest".to_string());
        }
        let key = (fixture.url.clone(), fixture.method.to_ascii_uppercase());
        if self
            .state
            .lock()
            .unwrap()
            .fetch_fixtures
            .insert(key, fixture.response.clone())
            .is_some()
        {
            return Err("duplicate fetch fixture".to_string());
        }
        Ok(())
    }

    /// Read a virtual file. Fails if not declared in capability manifest.
    pub fn read_file(&self, path: &str) -> Result<String, String> {
        if !self.manifest.allows(HostCapability::ReadFile) {
            return Err("read_file not declared in capability manifest".to_string());
        }
        let content_bytes = self
            .state
            .lock()
            .unwrap()
            .files
            .get(path)
            .map_or(0, String::len);
        self.transcript_budget.reserve(
            TRANSCRIPT_RECORD_FIXED_WIRE_BYTES
                .saturating_add(string_wire_upper_bound(path).saturating_mul(2))
                .saturating_add(content_bytes.saturating_mul(6)),
        )?;
        let mut state = self.state.lock().unwrap();
        let result = state
            .files
            .get(path)
            .cloned()
            .ok_or_else(|| format!("File not found: {path}"));
        state.transcript.reads.push(FakeReadRecord {
            path: path.to_string(),
            result: result.clone(),
        });
        result
    }

    /// Write a virtual file. Fails if not declared in capability manifest.
    pub fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        if !self.manifest.allows(HostCapability::WriteFile) {
            return Err("write_file not declared in capability manifest".to_string());
        }
        self.transcript_budget.reserve(
            TRANSCRIPT_RECORD_FIXED_WIRE_BYTES
                .saturating_add(string_wire_upper_bound(path))
                .saturating_add(string_wire_upper_bound(content)),
        )?;
        let mut state = self.state.lock().unwrap();
        let result = state.write_file(path, content);
        state.transcript.writes.push(FakeWriteRecord {
            path: path.to_string(),
            content: content.to_string(),
            result: result.clone(),
        });
        result
    }

    /// Replay a spawn fixture. Fails if `spawn` is not declared in the capability
    /// manifest, if the operation cap is reached, or if no fixture declares this exact
    /// program and argument vector.
    pub fn spawn(&self, program: &str, args: &[String]) -> Result<FakeSpawnResponse, String> {
        if !self.manifest.allows(HostCapability::Spawn) {
            return Err("spawn not declared in capability manifest".to_string());
        }

        self.transcript_budget.reserve(
            TRANSCRIPT_RECORD_FIXED_WIRE_BYTES
                .saturating_add(string_wire_upper_bound(program))
                .saturating_add(strings_wire_upper_bound(args.iter().map(String::as_str))),
        )?;

        let mut state = self.state.lock().unwrap();
        if state.transcript.spawns.len() >= FAKES_MAX_SPAWN_OPERATIONS {
            return Err("spawn limit exceeded".to_string());
        }

        // An unfixtured command has no honest answer: fabricating one would let a
        // candidate that shells out to a real executable pass on invented evidence.
        // The attempt is still recorded, exactly as an unseeded `read_file` is.
        let key = (program.to_string(), args.to_vec());
        let response = state.spawn_fixtures.get(&key).cloned();
        let recorded = match &response {
            Some(response) => {
                Ok(serde_json::to_string(response).map_err(|error| error.to_string())?)
            }
            None => Err(missing_spawn_fixture(program, args)),
        };
        state.transcript.spawns.push(FakeSpawnRecord {
            program: key.0,
            args: key.1,
            result: recorded,
        });
        response.ok_or_else(|| missing_spawn_fixture(program, args))
    }

    /// Replay a fetch fixture. Fails if `fetch` is not declared in the capability
    /// manifest, if the operation cap is reached, or if no fixture declares this exact
    /// method and URL.
    pub fn fetch(&self, url: &str, method: &str) -> Result<FakeFetchResponse, String> {
        if !self.manifest.allows(HostCapability::Fetch) {
            return Err("fetch not declared in capability manifest".to_string());
        }

        self.transcript_budget.reserve(
            TRANSCRIPT_RECORD_FIXED_WIRE_BYTES
                .saturating_add(string_wire_upper_bound(url))
                .saturating_add(string_wire_upper_bound(method)),
        )?;

        let mut state = self.state.lock().unwrap();
        if state.transcript.fetches.len() >= FAKES_MAX_FETCH_OPERATIONS {
            return Err("fetch limit exceeded".to_string());
        }

        // An unfixtured request has no honest answer: fabricating a 200 would let a
        // candidate that depends on a real HTTP response pass on invented evidence.
        // The attempt is still recorded, exactly as an unseeded `read_file` is.
        let normalized_method = method.to_ascii_uppercase();
        let response = state
            .fetch_fixtures
            .get(&(url.to_string(), normalized_method.clone()))
            .cloned();
        let recorded = match &response {
            Some(response) => {
                Ok(serde_json::to_string(response).map_err(|error| error.to_string())?)
            }
            None => Err(missing_fetch_fixture(&normalized_method, url)),
        };
        state.transcript.fetches.push(FakeFetchRecord {
            url: url.to_string(),
            method: method.to_string(),
            result: recorded,
        });
        response.ok_or_else(|| missing_fetch_fixture(&normalized_method, url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_transcripts_bound_counts_and_utf8_values() {
        let long = format!("{}é", "x".repeat(VERIFICATION_TRANSCRIPT_MAX_VALUE_BYTES));
        let transcript = FakeTranscript {
            reads: (0..=VERIFICATION_TRANSCRIPT_MAX_CALLS)
                .map(|_| FakeReadRecord {
                    path: long.clone(),
                    result: Ok(long.clone()),
                })
                .collect(),
            writes: vec![],
            spawns: vec![],
            fetches: vec![],
        };

        assert!(transcript.exceeds_wire_call_limit());
        let bounded = transcript.bounded_for_wire();
        assert_eq!(bounded.reads.len(), VERIFICATION_TRANSCRIPT_MAX_CALLS);
        assert_eq!(
            bounded.reads[0].path.len(),
            VERIFICATION_TRANSCRIPT_MAX_VALUE_BYTES
        );
        assert!(
            bounded.reads[0]
                .path
                .is_char_boundary(bounded.reads[0].path.len())
        );
        assert_eq!(
            bounded.reads[0].result.as_ref().unwrap().len(),
            VERIFICATION_TRANSCRIPT_MAX_VALUE_BYTES
        );

        let mut mixed = FakeTranscript {
            reads: vec![FakeReadRecord {
                path: "a".into(),
                result: Ok(String::new()),
            }],
            writes: vec![FakeWriteRecord {
                path: "b".into(),
                content: String::new(),
                result: Ok(()),
            }],
            spawns: vec![],
            fetches: vec![],
        };
        let mut budget = 1;
        mixed.limit_call_count(&mut budget);
        assert_eq!(mixed.call_count(), 1);
        assert_eq!(budget, 0);
    }
}
