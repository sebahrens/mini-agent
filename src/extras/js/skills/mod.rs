//! Immutable learned-JS skill artifacts, canonical identity, and capability types.
//!
//! This module is the shared foundation for the Phase 3 skill library. It owns the
//! artifact shape and the versioned canonical serialization that produces a skill's
//! identity. Persistence lives in [`store`], embedding in [`embed`], and no-effect
//! verification in [`verify`].
//!
//! Several admission and mutation entry points are intentionally unused by the
//! Phase 3 runtime. Phase 4 is their first production caller.

#![allow(dead_code)]
//!
//! Identity rules (see `docs/specs/phase-3-skill-library.md`):
//!
//! - `id` is the full 64-character lowercase hex SHA-256 of a versioned canonical
//!   serialization. Short/truncated IDs are never valid.
//! - Exact UTF-8 bytes are preserved for source, tests, and description. No implicit
//!   whitespace or newline normalization occurs.
//! - Every field and list item is length-prefixed so that no two distinct artifacts can
//!   serialize to the same byte string.
//! - Operational data (timestamps, status, lineage, row version, embeddings) is outside
//!   identity. There is no update operation for identity-bearing fields.

use std::fmt;

use reqwest::Url;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

pub mod admission;
mod admission_store;
pub mod capability;
pub mod coordinator;
pub mod embed;
pub mod fakes;
pub mod feedback;
pub mod held_out;
pub mod index;
pub mod lifecycle;
pub mod policy;
pub mod privacy;
pub mod proposal;
pub mod quarantine;
pub mod repair;
pub mod retention;
pub mod router;
pub mod scheduler;
pub mod store;
pub mod telemetry;
pub mod turn;
pub mod verify;
pub mod visibility;

/// Version of the canonical serialization scheme. Bumping this changes every identity.
pub const IDENTITY_VERSION: u32 = 2;

/// Version of the learned-skill invocation ABI included in every artifact identity.
pub const SKILL_ABI_VERSION: u16 = 2;

/// Harden the shared QuickJS realm before any untrusted skill source is evaluated.
/// Dynamic-code constructors and prototype mutation would otherwise recover the
/// ambient agent global from a lexical skill namespace.
pub const SKILL_REALM_HARDENING_JS: &str = r#"
(function () {
  const objects = [];
  const add = (value) => {
    if (value && !objects.includes(value)) objects.push(value);
  };
  for (const value of [
    Object.prototype, Function.prototype, Array.prototype, String.prototype,
    Number.prototype, Boolean.prototype, RegExp.prototype, Date.prototype,
    Error.prototype, TypeError.prototype, RangeError.prototype, ReferenceError.prototype,
    SyntaxError.prototype, EvalError.prototype, URIError.prototype,
    Map.prototype, Set.prototype, WeakMap.prototype, WeakSet.prototype,
    ArrayBuffer.prototype, DataView.prototype, Promise.prototype,
    Object.getPrototypeOf(Uint8Array.prototype),
    Object.getPrototypeOf(async function () {}),
    Object.getPrototypeOf(function* () {}),
    Object.getPrototypeOf(async function* () {}),
    JSON, Math, Reflect
  ]) add(value);
  for (const value of objects) {
    if (Object.prototype.hasOwnProperty.call(value, 'constructor')) {
      Object.defineProperty(value, 'constructor', {
        value: undefined, writable: false, configurable: false
      });
    }
  }
  for (const value of objects) Object.freeze(value);
  Object.defineProperty(globalThis, 'eval', {
    value: undefined, writable: false, configurable: false
  });
  Object.defineProperty(globalThis, 'Function', {
    value: undefined, writable: false, configurable: false
  });
})()
"#;

/// Return the immutable artifact source unchanged for evaluation as a QuickJS Script.
///
/// Export lookup and namespace construction are loader operations. In particular, this helper
/// must not wrap source in a function because doing so changes Script grammar.
pub(crate) fn private_skill_source(skill: &SkillArtifact) -> &str {
    &skill.source
}

/// Compatibility wrapper for the Phase 3 in-process engine. The Phase 6 realm loader uses
/// [`private_skill_source`]; A15/A21 remove the remaining compatibility callers when production
/// registration and verification move onto the worker loader.
pub(crate) fn legacy_private_skill_source(skill: &SkillArtifact) -> String {
    let published = skill
        .exports
        .iter()
        .map(|export| {
            let key =
                serde_json::to_string(&export.name).unwrap_or_else(|_| "\"invalid\"".to_string());
            format!(
                "{key}: (typeof {name} === 'function' ? {name} : undefined)",
                name = export.name
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let host = |capability: HostCapability, name: &str| {
        if skill.capability.allows(capability) {
            format!("globalThis.{name}")
        } else {
            "undefined".to_string()
        }
    };
    let read_file = host(HostCapability::ReadFile, "read_file");
    let write_file = host(HostCapability::WriteFile, "write_file");
    let spawn = host(HostCapability::Spawn, "spawn");
    let fetch = host(HostCapability::Fetch, "fetch");
    let safe_global = format!(
        "Object.freeze({{read_file:{read_file},write_file:{write_file},spawn:{spawn},fetch:{fetch}}})"
    );
    format!(
        "(function(read_file,write_file,spawn,fetch,globalThis,self,window,global,Function,Promise,__zs_Object){{\n\
         'use strict';\n{}\n;return __zs_Object.freeze({{{published}}});\n\
         }})({read_file},{write_file},{spawn},{fetch},{safe_global},{safe_global},{safe_global},{safe_global},undefined,Promise,Object)",
        skill.source,
    )
}

/// Domain separator binding identities to this scheme, so a hash computed here can never
/// collide with a hash of the same bytes computed for another purpose.
const IDENTITY_DOMAIN: &[u8] = b"mini-agent/skill-identity";

/// Capability tier. Maps onto the verifier's Tier 0/1/2 host-exposure matrix.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityTier {
    /// Tier 0 — no host globals whatsoever.
    Pure,
    /// Tier 1 — read-only host operations only.
    ReadOnly,
    /// Tier 2 — may declare any supported host operation.
    SideEffecting,
}

impl CapabilityTier {
    /// Stable wire token. Used in canonical serialization and persisted JSON; never
    /// derive this from `Debug`, which is not a stability contract.
    pub fn as_token(self) -> &'static str {
        match self {
            Self::Pure => "pure",
            Self::ReadOnly => "read_only",
            Self::SideEffecting => "side_effecting",
        }
    }

    /// Parse a stable wire token.
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "pure" => Some(Self::Pure),
            "read_only" => Some(Self::ReadOnly),
            "side_effecting" => Some(Self::SideEffecting),
            _ => None,
        }
    }

    /// Whether this tier may declare `capability`.
    ///
    /// `Pure` declares nothing. `ReadOnly` is restricted to operations that cannot mutate
    /// local state or produce network egress — `Fetch` is deliberately excluded because an
    /// outbound request is an observable side effect regardless of HTTP method.
    pub fn permits(self, capability: HostCapability) -> bool {
        match self {
            Self::Pure => false,
            Self::ReadOnly => capability.is_read_only(),
            Self::SideEffecting => true,
        }
    }
}

impl fmt::Display for CapabilityTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_token())
    }
}

/// A single host operation a skill may declare.
///
/// This list is closed. Administrative and security-sensitive operations (permission
/// mutation, MCP trust, sandbox configuration) are intentionally absent and can never be
/// declared by a learned skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostCapability {
    ReadFile,
    WriteFile,
    Spawn,
    Fetch,
}

impl HostCapability {
    /// Stable wire token.
    pub fn as_token(self) -> &'static str {
        match self {
            Self::ReadFile => "read_file",
            Self::WriteFile => "write_file",
            Self::Spawn => "spawn",
            Self::Fetch => "fetch",
        }
    }

    /// Parse a stable wire token. Unknown tokens return `None` and must be rejected by the
    /// caller rather than silently dropped.
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "read_file" => Some(Self::ReadFile),
            "write_file" => Some(Self::WriteFile),
            "spawn" => Some(Self::Spawn),
            "fetch" => Some(Self::Fetch),
            _ => None,
        }
    }

    /// Whether the operation is free of local mutation and network egress.
    pub fn is_read_only(self) -> bool {
        matches!(self, Self::ReadFile)
    }
}

impl fmt::Display for HostCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_token())
    }
}

/// A declared export: the function name the skill publishes and its documented signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillExport {
    pub name: String,
    pub signature: String,
}

/// The closed set of HTTP methods a learned skill may request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum HttpMethod {
    #[serde(rename = "GET")]
    Get,
    #[serde(rename = "POST")]
    Post,
}

impl HttpMethod {
    pub fn as_token(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }
}

/// One target-narrowed host operation declared by an identity-v2 artifact.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapabilityScope {
    ReadFile {
        workspace_prefixes: Vec<String>,
    },
    WriteFile {
        workspace_prefixes: Vec<String>,
    },
    Fetch {
        origins: Vec<String>,
        methods: Vec<HttpMethod>,
    },
    Spawn {
        programs: Vec<String>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RawCapabilityScope {
    ReadFile {
        workspace_prefixes: Vec<String>,
    },
    WriteFile {
        workspace_prefixes: Vec<String>,
    },
    Fetch {
        origins: Vec<String>,
        methods: Vec<HttpMethod>,
    },
    Spawn {
        programs: Vec<String>,
    },
}

impl From<RawCapabilityScope> for CapabilityScope {
    fn from(scope: RawCapabilityScope) -> Self {
        match scope {
            RawCapabilityScope::ReadFile { workspace_prefixes } => {
                Self::ReadFile { workspace_prefixes }
            }
            RawCapabilityScope::WriteFile { workspace_prefixes } => {
                Self::WriteFile { workspace_prefixes }
            }
            RawCapabilityScope::Fetch { origins, methods } => Self::Fetch { origins, methods },
            RawCapabilityScope::Spawn { programs } => Self::Spawn { programs },
        }
    }
}

impl<'de> Deserialize<'de> for CapabilityScope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        RawCapabilityScope::deserialize(deserializer).map(Into::into)
    }
}

impl CapabilityScope {
    pub fn capability(&self) -> HostCapability {
        match self {
            Self::ReadFile { .. } => HostCapability::ReadFile,
            Self::WriteFile { .. } => HostCapability::WriteFile,
            Self::Fetch { .. } => HostCapability::Fetch,
            Self::Spawn { .. } => HostCapability::Spawn,
        }
    }

    fn canonicalize(self) -> Result<Self, IdentityError> {
        match self {
            Self::ReadFile { workspace_prefixes } => Ok(Self::ReadFile {
                workspace_prefixes: canonicalize_unique_strings(
                    workspace_prefixes,
                    "read_file.workspace_prefixes",
                    canonical_workspace_prefix,
                )?,
            }),
            Self::WriteFile { workspace_prefixes } => Ok(Self::WriteFile {
                workspace_prefixes: canonicalize_unique_strings(
                    workspace_prefixes,
                    "write_file.workspace_prefixes",
                    canonical_workspace_prefix,
                )?,
            }),
            Self::Fetch { origins, methods } => {
                let origins =
                    canonicalize_unique_strings(origins, "fetch.origins", canonical_fetch_origin)?;
                if methods.is_empty() {
                    return Err(IdentityError::EmptyCapabilityScope("fetch.methods"));
                }
                let mut canonical_methods = methods;
                canonical_methods.sort_unstable();
                if canonical_methods.windows(2).any(|pair| pair[0] == pair[1]) {
                    return Err(IdentityError::DuplicateScopeValue("fetch.methods"));
                }
                Ok(Self::Fetch {
                    origins,
                    methods: canonical_methods,
                })
            }
            Self::Spawn { programs } => Ok(Self::Spawn {
                programs: canonicalize_unique_strings(
                    programs,
                    "spawn.programs",
                    canonical_program,
                )?,
            }),
        }
    }
}

/// The exact set of host operations a skill is allowed to perform.
///
/// Runtime and verifier checks consult [`Self::grants`] directly. The tier is a
/// consistency constraint and a display aid — it never confers an ambient tier-wide grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapabilityManifest {
    pub tier: CapabilityTier,
    pub grants: Vec<CapabilityScope>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCapabilityManifest {
    tier: CapabilityTier,
    grants: Vec<CapabilityScope>,
}

impl<'de> Deserialize<'de> for CapabilityManifest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawCapabilityManifest::deserialize(deserializer)?;
        Self::new(raw.tier, raw.grants).map_err(serde::de::Error::custom)
    }
}

impl CapabilityManifest {
    /// A Tier 0 manifest declaring nothing.
    pub fn pure() -> Self {
        Self {
            tier: CapabilityTier::Pure,
            grants: Vec::new(),
        }
    }

    /// Build and validate a manifest.
    ///
    /// Grants and their target lists are canonicalized and sorted. Duplicate operation
    /// scopes and duplicate normalized targets are rejected rather than silently merged.
    pub fn new(tier: CapabilityTier, grants: Vec<CapabilityScope>) -> Result<Self, IdentityError> {
        let mut canonical = grants
            .into_iter()
            .map(CapabilityScope::canonicalize)
            .collect::<Result<Vec<_>, _>>()?;
        canonical.sort_unstable();
        let manifest = Self {
            tier,
            grants: canonical,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    /// Enforce tier consistency and reject duplicates.
    pub fn validate(&self) -> Result<(), IdentityError> {
        let mut seen: Vec<HostCapability> = Vec::with_capacity(self.grants.len());
        for scope in &self.grants {
            let capability = scope.capability();
            if seen.contains(&capability) {
                return Err(IdentityError::DuplicateCapability(capability));
            }
            if !self.tier.permits(capability) {
                return Err(IdentityError::CapabilityExceedsTier {
                    tier: self.tier,
                    capability,
                });
            }
            seen.push(capability);
        }
        let mut canonical = self
            .grants
            .clone()
            .into_iter()
            .map(CapabilityScope::canonicalize)
            .collect::<Result<Vec<_>, _>>()?;
        canonical.sort_unstable();
        if canonical != self.grants {
            return Err(IdentityError::NonCanonicalManifest);
        }
        Ok(())
    }

    /// Whether `capability` is declared. This is the only authorization question the
    /// runtime and verifier ask.
    pub fn allows(&self, capability: HostCapability) -> bool {
        self.grants
            .iter()
            .any(|scope| scope.capability() == capability)
    }

    pub fn scope(&self, capability: HostCapability) -> Option<&CapabilityScope> {
        self.grants
            .iter()
            .find(|scope| scope.capability() == capability)
    }
}

#[cfg(test)]
pub(crate) fn test_manifest(
    tier: CapabilityTier,
    capabilities: Vec<HostCapability>,
) -> Result<CapabilityManifest, IdentityError> {
    let grants = capabilities
        .into_iter()
        .map(|capability| match capability {
            HostCapability::ReadFile => CapabilityScope::ReadFile {
                workspace_prefixes: vec!["fixtures".to_string()],
            },
            HostCapability::WriteFile => CapabilityScope::WriteFile {
                workspace_prefixes: vec!["tmp".to_string()],
            },
            HostCapability::Fetch => CapabilityScope::Fetch {
                origins: vec!["https://example.com".to_string()],
                methods: vec![HttpMethod::Get, HttpMethod::Post],
            },
            HostCapability::Spawn => CapabilityScope::Spawn {
                programs: vec!["printf".to_string()],
            },
        })
        .collect();
    CapabilityManifest::new(tier, grants)
}

/// An immutable learned-JS skill revision.
///
/// Construct through [`SkillArtifact::new`], which validates the manifest and computes the
/// canonical identity. A caller-supplied `id` is never trusted; use
/// [`SkillArtifact::verify_identity`] to check a value that came from storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillArtifact {
    pub id: String,
    pub identity_version: u32,
    pub abi_version: u16,
    pub source: String,
    pub description: String,
    pub tags: Vec<String>,
    pub exports: Vec<SkillExport>,
    pub tests: Vec<String>,
    pub capability: CapabilityManifest,
}

impl SkillArtifact {
    /// Validate the inputs and compute the canonical identity.
    ///
    /// Tags are normalized (trimmed, lowercased, deduplicated, sorted) before hashing, so
    /// reordering tags does not mint a new identity but changing one does. Source, tests,
    /// and description keep their exact bytes.
    pub fn new(
        source: String,
        description: String,
        tags: Vec<String>,
        exports: Vec<SkillExport>,
        tests: Vec<String>,
        capability: CapabilityManifest,
    ) -> Result<Self, IdentityError> {
        capability.validate()?;

        if description.trim().is_empty() {
            return Err(IdentityError::EmptyDescription);
        }

        let mut seen_exports: Vec<&str> = Vec::with_capacity(exports.len());
        for export in &exports {
            if export.name.trim().is_empty() {
                return Err(IdentityError::EmptyExportName);
            }
            if seen_exports.contains(&export.name.as_str()) {
                return Err(IdentityError::DuplicateExport(export.name.clone()));
            }
            seen_exports.push(&export.name);
        }

        let tags = normalize_tags(tags);

        let mut artifact = Self {
            id: String::new(),
            identity_version: IDENTITY_VERSION,
            abi_version: SKILL_ABI_VERSION,
            source,
            description,
            tags,
            exports,
            tests,
            capability,
        };
        artifact.id = artifact.compute_identity();
        Ok(artifact)
    }

    /// Compute the canonical identity of this artifact's identity-bearing fields.
    ///
    /// Does not consult `self.id`, so it is safe to call on a row read from storage in
    /// order to detect tampering.
    pub fn compute_identity(&self) -> String {
        let mut hasher = Sha256::new();
        let mut canonical = Vec::new();

        push_field(&mut canonical, IDENTITY_DOMAIN);
        push_u64(&mut canonical, u64::from(self.identity_version));
        push_u64(&mut canonical, u64::from(self.abi_version));
        push_field(&mut canonical, self.source.as_bytes());
        push_field(&mut canonical, self.description.as_bytes());

        push_u64(&mut canonical, self.tags.len() as u64);
        for tag in &self.tags {
            push_field(&mut canonical, tag.as_bytes());
        }

        push_u64(&mut canonical, self.exports.len() as u64);
        for export in &self.exports {
            push_field(&mut canonical, export.name.as_bytes());
            push_field(&mut canonical, export.signature.as_bytes());
        }

        push_u64(&mut canonical, self.tests.len() as u64);
        for test in &self.tests {
            push_field(&mut canonical, test.as_bytes());
        }

        push_field(&mut canonical, self.capability.tier.as_token().as_bytes());
        push_u64(&mut canonical, self.capability.grants.len() as u64);
        for scope in &self.capability.grants {
            push_field(&mut canonical, scope.capability().as_token().as_bytes());
            match scope {
                CapabilityScope::ReadFile { workspace_prefixes }
                | CapabilityScope::WriteFile { workspace_prefixes } => {
                    push_string_list(&mut canonical, workspace_prefixes);
                }
                CapabilityScope::Fetch { origins, methods } => {
                    push_string_list(&mut canonical, origins);
                    push_u64(&mut canonical, methods.len() as u64);
                    for method in methods {
                        push_field(&mut canonical, method.as_token().as_bytes());
                    }
                }
                CapabilityScope::Spawn { programs } => {
                    push_string_list(&mut canonical, programs);
                }
            }
        }

        hasher.update(&canonical);
        hex_lower(&hasher.finalize())
    }

    /// Recompute identity and compare against the stored `id`.
    ///
    /// Storage calls this on every active read so a tampered row cannot return source to
    /// retrieval.
    pub fn verify_identity(&self) -> Result<(), IdentityError> {
        if self.identity_version != IDENTITY_VERSION {
            return Err(IdentityError::UnsupportedIdentityVersion(
                self.identity_version,
            ));
        }
        if self.abi_version != SKILL_ABI_VERSION {
            return Err(IdentityError::UnsupportedAbiVersion(self.abi_version));
        }
        if self.id.len() != 64
            || !self
                .id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(IdentityError::MalformedId(self.id.clone()));
        }
        let expected = self.compute_identity();
        if expected != self.id {
            return Err(IdentityError::IdentityMismatch {
                claimed: self.id.clone(),
                computed: expected,
            });
        }
        self.capability.validate()
    }
}

fn canonicalize_unique_strings(
    values: Vec<String>,
    field: &'static str,
    canonicalize: fn(&str) -> Result<String, IdentityError>,
) -> Result<Vec<String>, IdentityError> {
    if values.is_empty() {
        return Err(IdentityError::EmptyCapabilityScope(field));
    }
    let mut values = values
        .iter()
        .map(|value| canonicalize(value))
        .collect::<Result<Vec<_>, _>>()?;
    values.sort();
    if values.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(IdentityError::DuplicateScopeValue(field));
    }
    Ok(values)
}

fn canonical_workspace_prefix(raw: &str) -> Result<String, IdentityError> {
    if raw.is_empty()
        || raw.starts_with('/')
        || raw.ends_with('/')
        || raw.contains('\\')
        || raw.contains(':')
        || raw.chars().any(char::is_control)
    {
        return Err(IdentityError::InvalidWorkspacePrefix(raw.to_string()));
    }
    let components = raw.split('/').collect::<Vec<_>>();
    if components
        .iter()
        .any(|component| component.is_empty() || matches!(*component, "." | ".."))
    {
        return Err(IdentityError::InvalidWorkspacePrefix(raw.to_string()));
    }
    Ok(components
        .into_iter()
        .map(|component| component.nfc().collect::<String>())
        .collect::<Vec<_>>()
        .join("/"))
}

fn canonical_program(raw: &str) -> Result<String, IdentityError> {
    if raw.is_empty()
        || matches!(raw, "." | "..")
        || raw.contains(['/', '\\', ':'])
        || raw.chars().any(char::is_control)
    {
        return Err(IdentityError::InvalidProgram(raw.to_string()));
    }
    Ok(raw.nfc().collect())
}

fn canonical_fetch_origin(raw: &str) -> Result<String, IdentityError> {
    let url = Url::parse(raw).map_err(|_| IdentityError::InvalidFetchOrigin(raw.to_string()))?;
    if !matches!(url.scheme(), "https" | "http")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
        || url.path() != "/"
    {
        return Err(IdentityError::InvalidFetchOrigin(raw.to_string()));
    }
    let host = url
        .host_str()
        .ok_or_else(|| IdentityError::InvalidFetchOrigin(raw.to_string()))?;
    if host.is_empty() || host.ends_with('.') {
        return Err(IdentityError::InvalidFetchOrigin(raw.to_string()));
    }
    Ok(url.origin().ascii_serialization())
}

fn push_string_list(out: &mut Vec<u8>, values: &[String]) {
    push_u64(out, values.len() as u64);
    for value in values {
        push_field(out, value.as_bytes());
    }
}

/// Normalize tags for identity: trim, lowercase, drop empties, deduplicate, sort.
pub fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut normalized: Vec<String> = tags
        .into_iter()
        .map(|tag| tag.trim().to_lowercase())
        .filter(|tag| !tag.is_empty())
        .collect();
    normalized.sort();
    normalized.dedup();
    normalized
}

/// Append an 8-byte little-endian length followed by the bytes themselves.
fn push_field(out: &mut Vec<u8>, bytes: &[u8]) {
    push_u64(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn hex_lower(bytes: &[u8]) -> String {
    use fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a String is infallible.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Errors from artifact construction and identity validation.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdentityError {
    #[error("capability {capability} is not permitted at tier {tier}")]
    CapabilityExceedsTier {
        tier: CapabilityTier,
        capability: HostCapability,
    },

    #[error("capability {0} declared more than once")]
    DuplicateCapability(HostCapability),

    #[error("capability scope {0} must contain at least one target")]
    EmptyCapabilityScope(&'static str),

    #[error("capability scope {0} contains a duplicate normalized target")]
    DuplicateScopeValue(&'static str),

    #[error("invalid portable workspace prefix: {0}")]
    InvalidWorkspacePrefix(String),

    #[error("invalid exact fetch origin: {0}")]
    InvalidFetchOrigin(String),

    #[error("invalid separator-free executable name: {0}")]
    InvalidProgram(String),

    #[error("capability manifest is not in canonical form")]
    NonCanonicalManifest,

    #[error("export {0} declared more than once")]
    DuplicateExport(String),

    #[error("export name must not be empty")]
    EmptyExportName,

    #[error("description must not be empty")]
    EmptyDescription,

    #[error("malformed skill id: {0}")]
    MalformedId(String),

    #[error("identity mismatch: row claims {claimed} but content hashes to {computed}")]
    IdentityMismatch { claimed: String, computed: String },

    #[error("unsupported identity version: {0}")]
    UnsupportedIdentityVersion(u32),

    #[error("unsupported skill ABI version: {0}")]
    UnsupportedAbiVersion(u16),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn export(name: &str, signature: &str) -> SkillExport {
        SkillExport {
            name: name.to_string(),
            signature: signature.to_string(),
        }
    }

    fn scope(capability: HostCapability) -> CapabilityScope {
        match capability {
            HostCapability::ReadFile => CapabilityScope::ReadFile {
                workspace_prefixes: vec!["fixtures".to_string()],
            },
            HostCapability::WriteFile => CapabilityScope::WriteFile {
                workspace_prefixes: vec!["tmp".to_string()],
            },
            HostCapability::Spawn => CapabilityScope::Spawn {
                programs: vec!["program".to_string()],
            },
            HostCapability::Fetch => CapabilityScope::Fetch {
                origins: vec!["https://example.com".to_string()],
                methods: vec![HttpMethod::Get],
            },
        }
    }

    fn manifest(
        tier: CapabilityTier,
        capabilities: Vec<HostCapability>,
    ) -> Result<CapabilityManifest, IdentityError> {
        CapabilityManifest::new(tier, capabilities.into_iter().map(scope).collect())
    }

    /// A representative artifact. Each test perturbs exactly one identity-bearing field.
    fn artifact() -> SkillArtifact {
        SkillArtifact::new(
            "function parseJson(s) { return JSON.parse(s); }".to_string(),
            "Parse JSON safely.".to_string(),
            vec!["json".to_string(), "parse".to_string()],
            vec![export("parseJson", "parseJson(text: string): unknown")],
            vec!["parseJson('{}') !== null".to_string()],
            CapabilityManifest::pure(),
        )
        .expect("representative artifact must be valid")
    }

    #[test]
    fn id_is_full_64_char_lowercase_hex() {
        let id = artifact().id;
        assert_eq!(id.len(), 64, "identity must be a full SHA-256, got {id}");
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "identity must be lowercase hex: {id}"
        );
    }

    #[test]
    fn identity_is_deterministic() {
        assert_eq!(artifact().id, artifact().id);
    }

    #[test]
    fn source_change_changes_identity() {
        let mut changed = artifact();
        changed.source.push(' ');
        assert_ne!(changed.compute_identity(), artifact().id);
    }

    #[test]
    fn description_change_changes_identity() {
        let mut changed = artifact();
        changed.description = "Parse JSON unsafely.".to_string();
        assert_ne!(changed.compute_identity(), artifact().id);
    }

    #[test]
    fn test_content_change_changes_identity() {
        let mut changed = artifact();
        changed.tests = vec!["parseJson('[]') !== null".to_string()];
        assert_ne!(changed.compute_identity(), artifact().id);
    }

    #[test]
    fn test_order_changes_identity() {
        let two = |tests: Vec<String>| {
            SkillArtifact::new(
                "function f() {}".to_string(),
                "d".to_string(),
                vec![],
                vec![export("f", "f()")],
                tests,
                CapabilityManifest::pure(),
            )
            .expect("valid")
            .id
        };
        let forward = two(vec!["a".to_string(), "b".to_string()]);
        let reversed = two(vec!["b".to_string(), "a".to_string()]);
        assert_ne!(forward, reversed, "test order is identity-bearing");
    }

    #[test]
    fn export_signature_change_changes_identity() {
        let mut changed = artifact();
        changed.exports = vec![export(
            "parseJson",
            "parseJson(text: string): unknown | null",
        )];
        assert_ne!(changed.compute_identity(), artifact().id);
    }

    #[test]
    fn export_name_change_changes_identity() {
        let mut changed = artifact();
        changed.exports = vec![export("parseJSON", "parseJson(text: string): unknown")];
        assert_ne!(changed.compute_identity(), artifact().id);
    }

    #[test]
    fn tag_change_changes_identity() {
        let mut changed = artifact();
        changed.tags = normalize_tags(vec!["json".to_string(), "decode".to_string()]);
        assert_ne!(changed.compute_identity(), artifact().id);
    }

    #[test]
    fn tag_reordering_does_not_change_identity() {
        let forward = SkillArtifact::new(
            "function f() {}".to_string(),
            "d".to_string(),
            vec!["alpha".to_string(), "beta".to_string()],
            vec![export("f", "f()")],
            vec!["true".to_string()],
            CapabilityManifest::pure(),
        )
        .expect("valid");
        let reversed = SkillArtifact::new(
            "function f() {}".to_string(),
            "d".to_string(),
            vec!["beta".to_string(), "ALPHA  ".to_string()],
            vec![export("f", "f()")],
            vec!["true".to_string()],
            CapabilityManifest::pure(),
        )
        .expect("valid");
        assert_eq!(
            forward.id, reversed.id,
            "tags are normalized and sorted, so order and case must not matter"
        );
    }

    #[test]
    fn capability_change_changes_identity() {
        let pure = artifact();
        let effectful = SkillArtifact::new(
            pure.source.clone(),
            pure.description.clone(),
            pure.tags.clone(),
            pure.exports.clone(),
            pure.tests.clone(),
            manifest(CapabilityTier::ReadOnly, vec![HostCapability::ReadFile])
                .expect("valid manifest"),
        )
        .expect("valid");
        assert_ne!(pure.id, effectful.id);
    }

    #[test]
    fn identity_version_is_bound_into_the_hash() {
        let mut changed = artifact();
        changed.identity_version = IDENTITY_VERSION + 1;
        assert_ne!(changed.compute_identity(), artifact().id);
    }

    #[test]
    fn length_prefixing_prevents_field_boundary_collisions() {
        // Without length prefixes, "ab" + "c" and "a" + "bc" would concatenate identically.
        let build = |description: &str, tag: &str| {
            SkillArtifact::new(
                "function f() {}".to_string(),
                description.to_string(),
                vec![tag.to_string()],
                vec![export("f", "f()")],
                vec!["true".to_string()],
                CapabilityManifest::pure(),
            )
            .expect("valid")
            .id
        };
        assert_ne!(build("ab", "c"), build("a", "bc"));
    }

    #[test]
    fn verify_identity_accepts_an_untampered_artifact() {
        artifact()
            .verify_identity()
            .expect("freshly built artifact must verify");
    }

    #[test]
    fn verify_identity_rejects_a_tampered_row() {
        let mut tampered = artifact();
        // Simulates a row edited in the database without recomputing the hash.
        tampered.source = "function parseJson(s) { return exfiltrate(s); }".to_string();
        let error = tampered
            .verify_identity()
            .expect_err("tampered source must be rejected");
        assert!(
            matches!(error, IdentityError::IdentityMismatch { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn verify_identity_rejects_legacy_short_ids() {
        let mut short = artifact();
        short.id.truncate(16);
        let error = short
            .verify_identity()
            .expect_err("short id must be rejected");
        assert!(
            matches!(error, IdentityError::MalformedId(_)),
            "got {error:?}"
        );
    }

    #[test]
    fn verify_identity_rejects_non_hex_ids() {
        let mut bad = artifact();
        bad.id = "z".repeat(64);
        let error = bad
            .verify_identity()
            .expect_err("non-hex id must be rejected");
        assert!(
            matches!(error, IdentityError::MalformedId(_)),
            "got {error:?}"
        );
    }

    #[test]
    fn verify_identity_rejects_unsupported_identity_version() {
        let mut future = artifact();
        future.identity_version = IDENTITY_VERSION + 1;
        future.id = future.compute_identity();
        let error = future
            .verify_identity()
            .expect_err("future version must be rejected");
        assert!(
            matches!(error, IdentityError::UnsupportedIdentityVersion(_)),
            "got {error:?}"
        );
    }

    #[test]
    fn pure_tier_cannot_declare_any_host() {
        let error = manifest(CapabilityTier::Pure, vec![HostCapability::ReadFile])
            .expect_err("Tier 0 must declare nothing");
        assert!(
            matches!(error, IdentityError::CapabilityExceedsTier { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn read_only_tier_rejects_mutating_and_egress_capabilities() {
        for capability in [
            HostCapability::WriteFile,
            HostCapability::Spawn,
            HostCapability::Fetch,
        ] {
            let error = manifest(CapabilityTier::ReadOnly, vec![capability])
                .expect_err("Tier 1 must reject non-read-only capabilities");
            assert!(
                matches!(error, IdentityError::CapabilityExceedsTier { .. }),
                "{capability} rejected for the wrong reason: {error:?}"
            );
        }
        // The one capability Tier 1 does permit.
        manifest(CapabilityTier::ReadOnly, vec![HostCapability::ReadFile])
            .expect("Tier 1 must permit read_file");
    }

    #[test]
    fn duplicate_capability_is_rejected() {
        let error = manifest(
            CapabilityTier::SideEffecting,
            vec![HostCapability::ReadFile, HostCapability::ReadFile],
        )
        .expect_err("duplicates must be rejected");
        assert!(
            matches!(error, IdentityError::DuplicateCapability(_)),
            "got {error:?}"
        );
    }

    #[test]
    fn manifest_declaration_order_does_not_change_identity() {
        let build = |hosts: Vec<HostCapability>| {
            manifest(CapabilityTier::SideEffecting, hosts).expect("valid")
        };
        let forward = build(vec![HostCapability::ReadFile, HostCapability::Spawn]);
        let reversed = build(vec![HostCapability::Spawn, HostCapability::ReadFile]);
        assert_eq!(forward.grants, reversed.grants);
    }

    #[test]
    fn allows_consults_the_exact_list_not_the_tier() {
        let manifest = manifest(
            CapabilityTier::SideEffecting,
            vec![HostCapability::ReadFile],
        )
        .expect("valid");
        assert!(manifest.allows(HostCapability::ReadFile));
        assert!(
            !manifest.allows(HostCapability::Spawn),
            "SideEffecting must not confer an ambient tier-wide grant"
        );
    }

    #[test]
    fn duplicate_exports_are_rejected() {
        let error = SkillArtifact::new(
            "function f() {}".to_string(),
            "d".to_string(),
            vec![],
            vec![export("f", "f()"), export("f", "f(x)")],
            vec!["true".to_string()],
            CapabilityManifest::pure(),
        )
        .expect_err("duplicate export names must be rejected");
        assert!(
            matches!(error, IdentityError::DuplicateExport(_)),
            "got {error:?}"
        );
    }

    #[test]
    fn empty_description_is_rejected() {
        let error = SkillArtifact::new(
            "function f() {}".to_string(),
            "   ".to_string(),
            vec![],
            vec![export("f", "f()")],
            vec!["true".to_string()],
            CapabilityManifest::pure(),
        )
        .expect_err("empty description must be rejected");
        assert!(
            matches!(error, IdentityError::EmptyDescription),
            "got {error:?}"
        );
    }

    #[test]
    fn capability_tokens_round_trip() {
        for tier in [
            CapabilityTier::Pure,
            CapabilityTier::ReadOnly,
            CapabilityTier::SideEffecting,
        ] {
            assert_eq!(CapabilityTier::from_token(tier.as_token()), Some(tier));
        }
        for capability in [
            HostCapability::ReadFile,
            HostCapability::WriteFile,
            HostCapability::Spawn,
            HostCapability::Fetch,
        ] {
            assert_eq!(
                HostCapability::from_token(capability.as_token()),
                Some(capability)
            );
        }
        assert_eq!(HostCapability::from_token("delete_everything"), None);
        assert_eq!(CapabilityTier::from_token("admin"), None);
    }

    #[test]
    fn source_bytes_are_not_whitespace_normalized() {
        let build = |source: &str| {
            SkillArtifact::new(
                source.to_string(),
                "d".to_string(),
                vec![],
                vec![export("f", "f()")],
                vec!["true".to_string()],
                CapabilityManifest::pure(),
            )
            .expect("valid")
            .id
        };
        assert_ne!(build("function f() {}"), build("function f() {}\n"));
        assert_ne!(build("function f() {}"), build("function  f() {}"));
    }
}
