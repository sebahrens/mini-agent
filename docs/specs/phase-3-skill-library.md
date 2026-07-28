# Phase 3 — Skill Library

**Status**: Pre-implementation  
**Prerequisite**: Phase 1 complete and passing (Phase 2 is NOT required)  
**Delivers**: An immutable content-addressed skill store, prompt-time hybrid retrieval, a
turn-scoped model manifest, and exact source binding for JS execution.
**Target scale**: up to 100,000 local/shared skill revisions.

---

## Overview

Implements the Voyager model: the agent accumulates reusable JavaScript functions that survive
across sessions. Retrieval occurs once when the current user prompt arrives, before model
generation. The LLM receives a compact manifest containing selected IDs, descriptions, exports,
signatures, and capability tiers. Every JS tool invocation in that turn receives exactly the
same immutable source snapshot.

Retrieving inside `engine::run_step` from model-generated JavaScript is prohibited. At that point
the model has already written its code and cannot discover an injected function, and embedding
raw JS against English descriptions produces a cross-domain query. Embedding and retrieval live
in the tokio runner/session layer; the dedicated JS thread only evaluates a resolved bundle.

Phase 3 supports manual admission after verification. Agent proposals and human-gated canary
admission are Phase 4. Evidence-driven promotion, quarantine, repair, and rollback are Phase 5.

---

## Feature gate

```toml
# Cargo.toml additions
[features]
skills = ["js", "dep:fastembed", "dep:rusqlite", "dep:sha2"]

[dependencies]
fastembed = { version = "3", optional = true }
rusqlite = { version = "0.31", features = ["bundled"], optional = true }
sha2 = { version = "0.10", optional = true }
```

`skills` implies `js`; a selectable skills-without-JS state is invalid. Gate skill code behind
`#[cfg(feature = "skills")]`. Default and `js`-only builds must remain unchanged. `rusqlite` uses
bundled SQLite and must verify that FTS5 is enabled in the pinned build. If it is unavailable,
the lexical retriever must fail clearly at startup or use an explicitly tested fallback; it must
not silently claim hybrid retrieval.

---

## File placement

All new files go in `src/extras/js/skills/` (to be created):

| File | Status | Purpose |
|------|--------|---------|
| `src/extras/js/skills/mod.rs` | TO BE CREATED | Immutable artifact, canonical identity, capability types |
| `src/extras/js/skills/store.rs` | TO BE CREATED | SQLite schema, identity-validating persistence, lifecycle filters |
| `src/extras/js/skills/embed.rs` | TO BE CREATED | Cached fastembed model and versioned embedding generation |
| `src/extras/js/skills/index.rs` | TO BE CREATED | Immutable dense snapshot, FTS ranking, fusion, budgets |
| `src/extras/js/skills/verify.rs` | TO BE CREATED | Fresh no-effect verifier used by Phases 3–5 |
| `src/extras/js/mod.rs` | Phase 1 creates | Add `#[cfg(feature = "skills")] pub mod skills;` |
| `src/extras/js/types.rs` | Phase 1 creates | Add `ResolvedSkill`/`TurnSkillBundle` to `JsRequest` |
| `src/extras/js/engine.rs` | Phase 1 creates | Evaluate selected skills and agent code as separate scripts |
| `src/extras/js/tool.rs` | Phase 1 creates | Snapshot the current bundle when a JS call starts |
| `src/agent/runner.rs` | EXISTS | Retrieve from the user prompt before the first model call |

---

## Immutable skill artifact — `src/extras/js/skills/mod.rs`

```rust
pub struct SkillArtifact {
    pub id: String,
    pub identity_version: u32,
    pub source: String,
    pub description: String,
    pub tags: Vec<String>,
    pub exports: Vec<SkillExport>,
    pub tests: Vec<String>,
    pub capability: CapabilityManifest,
}

pub struct SkillExport {
    pub name: String,
    pub signature: String,
}

pub struct CapabilityManifest {
    pub tier: CapabilityTier,
    pub allowed_hosts: Vec<HostCapability>,
}

pub enum CapabilityTier {
    Pure,
    ReadOnly,
    SideEffecting,
}

pub enum HostCapability {
    ReadFile,
    WriteFile,
    Spawn,
    Fetch,
}
```

`id` is the full 64-character SHA-256 of a versioned canonical serialization containing source,
ordered tests, ordered exports/signatures, description, normalized ordered tags, and the full
capability manifest.
Exact UTF-8 bytes are preserved for source/tests/description; no implicit whitespace or newline
normalization occurs. Length-prefix every field and list item to avoid ambiguous concatenation.

Manifest validation enforces tier consistency: `Pure` has no allowed hosts; `ReadOnly` may declare
only read-only operations; `SideEffecting` may declare only the supported Tier 0–2 hosts. Unknown,
duplicate, or administrative/security-sensitive capabilities are rejected. Runtime and verifier
checks use the exact list, never a broad tier-wide ambient grant.

Changing any execution- or discovery-bearing field creates a new ID. Timestamps, status,
telemetry, lineage, row version, and embedding bytes are operational data outside identity. There
is no update operation for identity-bearing columns. The store recomputes identity on insert and
every active read; caller-provided IDs are never trusted.

---

## SQL schema — `src/extras/js/skills/store.rs`

Database path: `~/.config/zerostack/skills.db` (respects `$XDG_CONFIG_HOME`).

```sql
CREATE TABLE IF NOT EXISTS skill_revisions (
    id               TEXT PRIMARY KEY,
    identity_version INTEGER NOT NULL,
    source           TEXT NOT NULL,
    description      TEXT NOT NULL,
    tags_json        TEXT NOT NULL,
    exports_json     TEXT NOT NULL,
    tests_json       TEXT NOT NULL,
    capability_json  TEXT NOT NULL,
    status           TEXT NOT NULL,
    supersedes_id    TEXT,
    superseded_by_id TEXT,
    row_version      INTEGER NOT NULL DEFAULT 1,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    CHECK (status IN (
        'pending','verified','canary','active','quarantined','superseded','retired','rejected'
    ))
);

CREATE TABLE IF NOT EXISTS skill_embeddings (
    skill_id       TEXT NOT NULL,
    model_id       TEXT NOT NULL,
    model_revision TEXT NOT NULL,
    dimensions     INTEGER NOT NULL,
    normalized     INTEGER NOT NULL,
    embedding      BLOB NOT NULL,
    created_at     INTEGER NOT NULL,
    PRIMARY KEY (skill_id, model_id, model_revision)
);
```

FTS5 indexes description, tags, export signatures, and identifiers extracted from source. The
source itself is not copied wholesale into model-visible retrieval output.

### CRUD operations

```rust
impl SkillStore {
    pub fn open() -> anyhow::Result<Self>;
    pub fn insert_verified(&mut self, artifact: &SkillArtifact) -> anyhow::Result<()>;
    pub fn get(&self, id: &str) -> anyhow::Result<Option<SkillArtifact>>;
    pub fn list_retrievable(&self) -> anyhow::Result<Vec<SkillArtifact>>;
    pub fn store_embedding(&mut self, record: &EmbeddingRecord) -> anyhow::Result<()>;
    pub fn retire(&mut self, id: &str, expected_version: u64) -> anyhow::Result<()>;
    pub fn purge(&mut self, id: &str) -> anyhow::Result<()>;
}
```

In Phase 3, manual insertion verifies the artifact first and stores it as active. `get`, index
loading, and later promotion paths recompute canonical identity and reject or quarantine invalid
rows before returning source. In Phase 3, `list_retrievable` returns only `active` rows. The schema
reserves `canary` for Phase 4, but canaries remain non-retrievable until the Phase 5 router can
attach them as alternatives to an active lineage. They are never independent search competitors.
Pending, quarantined, superseded, retired, and rejected rows never enter an index snapshot.

`retire` is the normal reversible removal path. `purge` is an explicit privacy operation and must
delete dependent embeddings/index data transactionally. Not-found, collision, stale-version,
corruption, and database errors return typed errors and never panic.

---

## Embedding generation — `src/extras/js/skills/embed.rs`

**Model**: `BAAI/bge-small-en-v1.5` via `fastembed` crate (~30 MiB download, cached locally). No API call required — fully local inference.

The fastembed model is initialized once and reused. The retrieval document is deterministic:

```text
<description>\nExports: <signature>; ...\nTags: <tag>, ...\nIdentifiers: <bounded sorted identifiers>
```

Persist model ID, model revision, dimensions, normalization flag, and bytes. A model or dimension
change never mixes vectors: build a new generation and atomically switch after all retrievable
skills have embeddings. Skill embeddings are computed at insert/promotion or migration, never
lazily in the request path.

Query embedding runs in a bounded blocking worker because local model inference is CPU-bound. A
cache keyed by `(model_revision, sha256(retrieval_query))` avoids repeated inference for retries
and tool continuations. Cache entries are bounded by count/bytes and have explicit eviction.

### Embedding API

```rust
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

pub struct Embedder { /* lazily initialized model + immutable metadata */ }

impl Embedder {
    pub fn embed_query(&self, query: &str) -> anyhow::Result<Vec<f32>>;
    pub fn embed_documents(&self, documents: &[String]) -> anyhow::Result<Vec<Vec<f32>>>;
}
```

Empty output, non-finite values, dimension mismatch, or model initialization/download failure is a
normal error. Retrieval then returns no learned skills and surfaces a diagnostic; it never injects
an unscored fallback.

---

## `SkillIndex` and hybrid retrieval — `src/extras/js/skills/index.rs`

```rust
pub trait SkillIndex: Send + Sync {
    fn search(
        &self,
        query_text: &str,
        query_embedding: &[f32],
        policy: &RetrievalPolicy,
    ) -> anyhow::Result<Vec<ScoredSkill>>;
}
```

The default implementation loads active, identity-valid rows into one immutable snapshot. Phase 5
extends a logical lineage entry with eligible canary alternatives after retrieval; it does not add
candidate/predecessor near-duplicates to the search corpus.

- IDs/metadata and pre-normalized equal-dimension f32 vectors are contiguous and deterministic.
- Exact cosine ranking is a dot product; a bounded heap selects candidates without sorting every
  row.
- SQLite BLOBs are read only when building a new generation, never per query.
- FTS5/BM25 produces lexical candidates from exact identifiers, exports, descriptions, and tags.
- Dense and lexical ranks are combined with reciprocal-rank fusion.
- A dense similarity floor may reduce the result to zero; top-k is a maximum, not a quota.
- Semantic near-duplicates are collapsed before applying source/manifest budgets.
- Final ordering is deterministic by fused score then full skill ID.

Initial policy defaults are `max_skills = 3`, a configurable dense score floor, a compact manifest
budget, and a separate JS source-byte budget. Threshold calibration must use checked-in retrieval
fixtures; a magic score may not be accepted only because one model happened to emit it.

At the 100,000-skill target, benchmark query embedding separately from index search. The exact
in-memory implementation must meet a 5 ms p99 search target on a documented reference machine,
excluding embedding inference. ANN/HNSW is not a Phase 3 dependency. It may replace the trait
implementation only when a checked-in 100,000-skill benchmark demonstrates a missed p99 budget
and includes recall, memory, build, update, quarantine, and deletion measurements.

---

## Prompt-time retrieval and turn-scoped binding

### Retrieval query

The current user prompt is always the primary query. To resolve references such as “do the same
for the other file,” append a deterministic bounded suffix from recent task context. Do not call a
second LLM to summarize the query. Do not include unbounded conversation history, tool output,
timestamps, random IDs, or generated JS.

### Turn context

```rust
pub struct TurnSkillBundle {
    pub query_fingerprint: String,
    pub embedding_model_revision: String,
    pub index_generation: u64,
    pub skills: Vec<ResolvedSkill>,
}

pub struct ResolvedSkill {
    pub id: String,
    pub score: f32,
    pub rank: usize,
    pub description: String,
    pub exports: Vec<SkillExport>,
    pub capability: CapabilityManifest,
    pub source: String,
}
```

The runner resolves one bundle before its initial `stream_chat(prompt, history)` call and stores
it in a per-agent `SkillTurnContext`. Retries reuse the same bundle. Continuations within the same
user turn do not re-embed or rerank. A new user prompt creates a new generation-stamped bundle.

The model-visible prompt is prefixed with a compact non-user-spoofable manifest when the provider
supports such a channel; otherwise use a clearly delimited trusted context block inserted by the
runner. The manifest contains no source:

```text
<available_js_skills index_generation="42">
- id: <full id>
  exports: parseJson(text: string): unknown | null
  capability: pure
  description: Parse JSON safely and return null on syntax error.
</available_js_skills>
```

The JS tool snapshots the exact bundle when the tool call begins and includes it in `JsRequest`.
The JS thread performs no database access, embedding, or ranking.

### Runtime binding

Evaluate selected skill sources as script 1, validate and wrap the declared exports, then evaluate
model-authored code as script 2 in the same fresh context:

```javascript
// Script 1, generated by zerostack from the frozen bundle
function parseJson(s) { /* immutable selected source */ }

// Script 2, exactly the model-authored code
parseJson(input)
```

Separate scripts preserve agent-code line numbers. Skill-source failures identify the full skill ID
and never rewrite the agent script. Missing/duplicate exports, source exceptions, capability
violations, and bundle identity mismatches fail closed. If the bundle is empty, evaluate only the
agent script and add no manifest.

---

## No-effect skill verification — `src/extras/js/skills/verify.rs`

```rust
pub fn verify_skill(skill: &SkillArtifact) -> Result<VerificationReport, VerificationError> {
    // One fresh bounded Runtime/Context for this verification.
    // Register no real-effect host globals. Tier 0 gets none; Tier 1/2 get only declared fakes.
    // Evaluate source as one script, then each test as a separate script in the same context.
    // Require at least one test and exact JavaScript boolean true for every test.
}
```

Reject empty tests, false, undefined/void, numeric/string/object truthy values, source/test syntax or
runtime errors, Promise rejection, timeout, OOM, endless jobs, undeclared exports, and any host
access outside the declared capability. Tier 0 receives no host symbols. Tier 1 and Tier 2 receive
only verifier-owned deterministic record/replay fakes for declared operations. The fakes are
versioned, bounded, and backed by in-memory virtual state; they never call the real filesystem,
permission service, process launcher, or network. Unconfigured operations fail deterministically.
Trusted held-out cases may supply hidden fake responses and assert the recorded call transcript.
Embedded tests cannot inspect hidden fixtures or replace fake implementations.

Verification errors include the stage/test index and bounded stack information but never activate
or persist the artifact. Each skill verification gets a new runtime and new fake state; tests
within that verification may see the source and prior test/fake effects only if the implementation
documents and tests that ordering.

The verifier also performs an anti-vacuity mutation pass. For each declared export, rerun the suite
with that export replaced by a throwing stub; at least one embedded test must then fail for the
expected reason. This proves that every public export is exercised, not that its behavior is fully
correct. Mutation runs use fresh contexts and the same resource bounds. An empty, always-true, or
unrelated suite cannot verify an artifact.

Manual Phase 3 insertion calls the verifier before identity-validating persistence. Store APIs do
not accept an “already verified” caller assertion without a corresponding trusted verification
report tied to the full artifact ID.

---

## Acceptance criteria

All must pass under `cargo test --features js,skills`:

- [ ] Identity changes when source, test/order, export/signature, description/tag, capability, or
      identity version changes; operational metadata does not affect identity.
- [ ] Caller ID mismatch, row tampering, legacy short IDs, dimensions/model mismatch, and a
      simulated collision are rejected without returning source to retrieval.
- [ ] No-effect verification requires nonempty exact-boolean tests, gives Tier 0 no host globals,
      gives Tier 1/2 only declared deterministic in-memory fakes, and mutation checks prove every
      declared export affects at least one test.
- [ ] Embeddings are generated at admission/migration and tagged with model revision/dimensions.
- [ ] The fastembed model and bounded query cache are reused; request-time retrieval never lazily
      embeds a stored skill.
- [ ] The current user prompt is the primary query and retrieval completes before the first model
      output; generated JS is not present in the query.
- [ ] Exact dense + FTS ranking, RRF fusion, threshold, dedupe, deterministic order, and manifest/
      source budgets have fixture tests.
- [ ] The model-visible manifest contains only the frozen selected bundle's metadata and is present
      before the model emits a JS tool call.
- [ ] Every JS call in one turn receives the same immutable bundle and retries do not re-embed.
- [ ] Skill source and agent source run as separate scripts; an agent error on line N reports line N
      with zero, one, or three selected skills.
- [ ] Pending/canary/quarantined/superseded/retired/rejected rows never appear as independent Phase
      3 search results; only Phase 5 may route an eligible canary after selecting its active lineage.
- [ ] A 100,000-skill benchmark reports query-embedding and index-search latency separately and
      exact search meets the documented p99 target or blocks Phase 3 closure.
- [ ] `cargo test --features js` without `skills` passes unchanged.

---

## Out of scope for Phase 3

- UI for browsing or editing skills
- Cross-agent skill sharing (single-user local store only)
- Auto-admission from successful agent steps (Phase 4)
- Evidence-driven promotion, quarantine, repair, and rollback (Phase 5)
- ANN/HNSW unless the 100,000-skill exact-index benchmark misses its p99 budget
