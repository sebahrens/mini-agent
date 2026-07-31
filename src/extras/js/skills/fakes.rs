//! Versioned, bounded, in-memory record/replay host fakes for skill verification.
//!
//! This module provides deterministic fake implementations of file I/O, process spawning,
//! and network operations for use during skill verification. Fakes are:
//! - Versioned: updates change the version token
//! - Bounded: memory and operation counts are capped
//! - Record/replay: all I/O is recorded and can be inspected
//! - Virtual: they never access the real filesystem, network, or process launcher
//! - Scoped: state resets for each artifact verification

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::extras::js::skills::{CapabilityManifest, HostCapability};

/// Version of the fake host implementation. Bumping this invalidates existing
/// verification reports.
pub const FAKES_VERSION: u32 = 1;

/// Maximum total size of all virtual files in bytes.
const FAKES_TOTAL_FILE_BYTES: usize = 10 * 1024 * 1024; // 10 MiB

/// Maximum number of spawn operations.
const FAKES_MAX_SPAWN_OPERATIONS: usize = 1_000;

/// Maximum number of fetch operations.
const FAKES_MAX_FETCH_OPERATIONS: usize = 1_000;

/// A record of a fake read_file operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeReadRecord {
    pub path: String,
    pub result: Result<String, String>,
}

/// A record of a fake write_file operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeWriteRecord {
    pub path: String,
    pub content: String,
    pub result: Result<(), String>,
}

/// A record of a fake spawn operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeSpawnRecord {
    pub program: String,
    pub args: Vec<String>,
    pub result: Result<String, String>, // JSON serialized SpawnResult
}

/// A record of a fake fetch operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeFetchRecord {
    pub url: String,
    pub method: String,
    pub result: Result<String, String>, // JSON response
}

/// Transcript of all I/O operations during verification.
#[derive(Debug, Clone, Default)]
pub struct FakeTranscript {
    pub reads: Vec<FakeReadRecord>,
    pub writes: Vec<FakeWriteRecord>,
    pub spawns: Vec<FakeSpawnRecord>,
    pub fetches: Vec<FakeFetchRecord>,
}

impl FakeTranscript {
    /// Whether this transcript is empty (no operations).
    pub fn is_empty(&self) -> bool {
        self.reads.is_empty()
            && self.writes.is_empty()
            && self.spawns.is_empty()
            && self.fetches.is_empty()
    }
}

/// Mutable state for fake host operations during one verification.
struct FakeState {
    /// Virtual file system: path -> content.
    files: HashMap<String, String>,
    /// Total bytes of all files.
    total_file_bytes: usize,
    /// Transcript of all operations.
    transcript: FakeTranscript,
}

impl FakeState {
    fn new() -> Self {
        Self {
            files: HashMap::new(),
            total_file_bytes: 0,
            transcript: FakeTranscript::default(),
        }
    }

    fn read_file(&mut self, path: &str) -> Result<String, String> {
        if let Some(content) = self.files.get(path) {
            let result = Ok(content.clone());
            self.transcript.reads.push(FakeReadRecord {
                path: path.to_string(),
                result: result.clone(),
            });
            result
        } else {
            let result = Err(format!("File not found: {}", path));
            self.transcript.reads.push(FakeReadRecord {
                path: path.to_string(),
                result: result.clone(),
            });
            result
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
    pub manifest: CapabilityManifest,
}

impl FakeHostGlobals {
    /// Create a new set of fake hosts for the given capability manifest.
    ///
    /// Tier 0 skills get no fakes (the builder will not register globals).
    /// Tier 1/2 skills get only the operations they declared.
    pub fn new(manifest: CapabilityManifest) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeState::new())),
            manifest,
        }
    }

    /// Whether `capability` is declared in the manifest.
    pub fn allows(&self, capability: HostCapability) -> bool {
        self.manifest.allows(capability)
    }

    /// Read the immutable transcript. Embedded tests cannot inspect this.
    pub fn transcript(&self) -> FakeTranscript {
        self.state.lock().unwrap().transcript.clone()
    }

    /// Read a virtual file. Fails if not declared in capability manifest.
    pub fn read_file(&self, path: &str) -> Result<String, String> {
        if !self.manifest.allows(HostCapability::ReadFile) {
            return Err("read_file not declared in capability manifest".to_string());
        }
        self.state.lock().unwrap().read_file(path)
    }

    /// Write a virtual file. Fails if not declared in capability manifest.
    pub fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        if !self.manifest.allows(HostCapability::WriteFile) {
            return Err("write_file not declared in capability manifest".to_string());
        }
        let mut state = self.state.lock().unwrap();
        let result = state.write_file(path, content);
        state.transcript.writes.push(FakeWriteRecord {
            path: path.to_string(),
            content: content.to_string(),
            result: result.clone(),
        });
        result
    }

    /// Simulate a spawn operation. Fails if not declared in capability manifest.
    /// Always succeeds with exit code 0 for determinism.
    pub fn spawn(&self, program: &str, args: &[String]) -> Result<String, String> {
        if !self.manifest.allows(HostCapability::Spawn) {
            return Err("spawn not declared in capability manifest".to_string());
        }

        let mut state = self.state.lock().unwrap();
        if state.transcript.spawns.len() >= FAKES_MAX_SPAWN_OPERATIONS {
            return Err("spawn limit exceeded".to_string());
        }

        // Simulated spawn result: always succeeds with empty output
        let result_json = r#"{"stdout":"","stderr":"","code":0,"timed_out":false,"stdout_truncated":false,"stderr_truncated":false}"#.to_string();
        let result = Ok(result_json.clone());
        state.transcript.spawns.push(FakeSpawnRecord {
            program: program.to_string(),
            args: args.to_vec(),
            result: result.clone(),
        });
        result
    }

    /// Simulate a fetch operation. Fails if not declared in capability manifest.
    pub fn fetch(&self, url: &str, _method: &str) -> Result<String, String> {
        if !self.manifest.allows(HostCapability::Fetch) {
            return Err("fetch not declared in capability manifest".to_string());
        }

        let mut state = self.state.lock().unwrap();
        if state.transcript.fetches.len() >= FAKES_MAX_FETCH_OPERATIONS {
            return Err("fetch limit exceeded".to_string());
        }

        // Simulated fetch result: empty JSON object
        let result_json = "{}".to_string();
        let result = Ok(result_json.clone());
        state.transcript.fetches.push(FakeFetchRecord {
            url: url.to_string(),
            method: _method.to_string(),
            result: result.clone(),
        });
        result
    }
}
