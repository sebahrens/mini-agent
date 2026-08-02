# Phase 3 — Skill Library

- **Document role**: normative phase specification
- **Specification version**: 1.1.0
- **Delivery status**: delivered
- **Owner**: mini-agent maintainers
- **Last reconciled**: 2026-07-31
- **Entry dependencies**: Foundation and Phase 1 complete; Phase 2 is optional
- **Exit dependency**: every acceptance criterion below and every Phase 3 blocker

**Delivers**: Open Agent Skills directory/ZIP import, an immutable content-addressed JS skill
store, prompt-time hybrid retrieval, a turn-scoped model manifest, and exact source binding for JS
execution.
**Target scale**: up to 100,000 local/shared skill revisions.

The corpus authority and conflict rules are defined in
[`00-index.md`](00-index.md). Phase 3 owns the delivered identity-v1/full-payload baseline, manual
admission, retrieval, and no-effect verification semantics. It does not own agent proposal
admission or evidence-based promotion.

[`phase-6-brokered-js-runtime.md`](phase-6-brokered-js-runtime.md) explicitly supersedes this
phase's identity-v1 flat capability shape, same-context runtime binding, and verifier runtime
ownership. Phase 3 remains authoritative for immutable full-payload identity, SQLite storage,
manual admission, frozen turn bundles, retrieval, declared exports, and deterministic verifier
semantics. The index maps the exact section-level boundary; Phase 6 is planned, so this notice does
not mark its identity-v2 or worker implementation delivered.

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
in the tokio runner/session layer. The delivered Phase 3 implementation sent the resolved bundle
to a dedicated JS thread; Phase 6 replaces that historical evaluator with a contained worker while
keeping retrieval and database access in the parent.

Phase 3 supports manual admission after verification. Agent proposals and human-gated canary
admission are Phase 4. Evidence-driven promotion, quarantine, repair, and rollback are Phase 5.

All persistent paths and storage classes follow
[`platform-paths.md`](platform-paths.md). A Phase 3 implementation is not portable if it merely
respects Linux XDG variables while placing SQLite under configuration storage.

---

## Feature gate

```toml
# Cargo.toml additions
[features]
skills = ["js", "dep:rusqlite", "dep:matrixmultiply", "dep:hnsw_rs"]
skills-embed = ["skills", "dep:fastembed"]

[dependencies]
rusqlite = { version = "0.40", features = ["bundled"], optional = true }
fastembed = { version = "5", optional = true }
matrixmultiply = { version = "0.3", optional = true, features = ["threading"] }
hnsw_rs = { version = "0.3", optional = true }
```

`skills` implies `js`; a selectable skills-without-JS state is invalid. Gate skill code behind
`#[cfg(feature = "skills")]`. Default and `js`-only builds must remain unchanged. `rusqlite` uses
bundled SQLite and must verify that FTS5 is enabled in the pinned build. If it is unavailable,
the lexical retriever must fail clearly at startup or use an explicitly tested fallback; it must
not silently claim hybrid retrieval. The existing mandatory `sha2` dependency is reused; do not
add a second optional declaration.

**`fastembed` is behind its own `skills-embed` feature, not `skills`.** `fastembed` pulls
`ort-sys` (ONNX Runtime), which ships no prebuilt binaries for every host — notably
`x86_64-apple-darwin`, where the build fails outright. Folding it into `skills` would make the
entire skill library unbuildable on those hosts. `skills` therefore carries only SQLite and
selects the offline deterministic embedding backend; `skills-embed` adds local BGE inference.
An external OpenAI-compatible embeddings API is the third option and needs no extra feature —
see the `[embedding]` section in `docs/agent/CONFIG.md`. Required CI covers the `skills` rows;
`skills-embed` runs as an optional, non-blocking job.

The original pins in this section (`fastembed = "3"`, `rusqlite = "0.31"`) were stale and did not
resolve against the current toolchain; the versions above are what is actually built.

---

## File placement

Learned JS files live in `src/extras/js/skills/`. Portable instruction-skill import
is a sibling feature so its resources are never mistaken for verified JS globals:

| File | Status | Purpose |
|------|--------|---------|
| `src/extras/js/skills/mod.rs` | IMPLEMENTED | Immutable artifact, canonical identity, capability types |
| `src/extras/js/skills/store.rs` | IMPLEMENTED | SQLite schema, identity-validating persistence, lifecycle filters |
| `src/extras/js/skills/embed.rs` | IMPLEMENTED | Cached embedding backends and versioned embedding generation |
| `src/extras/js/skills/index.rs` | IMPLEMENTED | Immutable dense snapshot, FTS ranking, fusion, budgets |
| `src/extras/js/skills/verify.rs` | IMPLEMENTED | Fresh no-effect verifier used by Phases 3–5 |
| `src/extras/skills/import.rs` | IMPLEMENTED | Agent Skills directory/ZIP validation and content-addressed installation |
| `src/extras/skills/index.rs` | IMPLEMENTED | Progressive metadata discovery for instruction skills |
| `src/extras/js/mod.rs` | IMPLEMENTED | Declares the feature-gated skill modules |
| `src/extras/js/types.rs` | IMPLEMENTED | Carries the resolved skill bundle; Phase 6 supersedes runtime ownership |
| `src/extras/js/engine.rs` | IMPLEMENTED | Evaluates selected skills and agent code as separate scripts; Phase 6 supersedes execution ownership |
| `src/extras/js/tool.rs` | IMPLEMENTED | Snapshots the current bundle; Phase 6 delegates execution to the supervisor |
| `src/agent/runner.rs` | EXISTS | Retrieve from the user prompt before the first model call |

---

## Immutable skill artifact — `src/extras/js/skills/mod.rs`

The type shape below records delivered identity version 1. Its flat `allowed_hosts` list is
superseded for new Phase 6 artifacts by identity version 2 structured capability scopes. Identity
version 2 retains every other execution/discovery-bearing field, includes its ABI version and the
complete canonical structured scopes in the hash, and is governed by Phase 6's `Persistence
boundary`. No reader may interpret this version-1 example as permission to infer version-2 scopes.

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

For delivered identity version 1, `id` is the full 64-character SHA-256 of a versioned canonical
serialization containing source, ordered tests, ordered exports/signatures, description,
normalized ordered tags, and the full flat capability manifest. Identity version 2 applies the
same full-payload rule to its structured capability manifest and ABI version.
Exact UTF-8 bytes are preserved for source/tests/description; no implicit whitespace or newline
normalization occurs. Length-prefix every field and list item to avoid ambiguous concatenation.

Manifest validation enforces tier consistency: `Pure` has no allowed hosts; `ReadOnly` may declare
only read-only operations; `SideEffecting` may declare only the supported Tier 0–2 hosts. Unknown,
duplicate, or administrative/security-sensitive capabilities are rejected. Runtime and verifier
checks use the exact list, never a broad tier-wide ambient grant.

Changing any execution- or discovery-bearing field creates a new ID. Changing identity/ABI version
or any structured capability scope also creates a new ID. Timestamps, status,
telemetry, lineage, row version, and embedding bytes are operational data outside identity. There
is no update operation for identity-bearing columns. The store recomputes identity on insert and
every active read; caller-provided IDs are never trusted.

---

## Open Agent Skills interoperability

Phase 3 also loads the open Agent Skills format without conflating instruction packages with
learned JS functions. A valid portable skill is a directory containing `SKILL.md` with the
normative `name` and `description` frontmatter and optional scripts, references, assets, and other
resources. Installation accepts that directory or a `.zip` containing one such tree. The archive
filename is arbitrary; `skill.zip` is supported but is not itself the semantic standard.

Import, storage roots, tree identity, cross-platform filename validation, archive limits, and
zip-slip/symlink/reparse protections are defined in `platform-paths.md`. The importer validates
without executing any resource. The experimental `allowed-tools` field is retained as metadata but
does not grant permission or capability.

Discovery follows progressive disclosure:

1. Parse, validate, and pre-embed only the name/description and bounded discovery metadata.
2. Reuse the current turn's single query embedding to rank Agent Skills and learned JS skills in
   typed indexes with separate result/context budgets.
3. Load a selected `SKILL.md` body only when activated and load referenced resources on demand.
4. Keep bundled JavaScript as an ordinary Agent Skill resource. It is not inserted into
   `SkillStore`, injected as a global, or made self-learning without the normal immutable proposal,
   verification, capability, and evidence gates.

MCP remains composable. Agent Skill instructions may tell the model to use configured MCP tools,
but neither frontmatter nor bundled resources can forge built-in MCP trust or bypass the existing
`mcp_tool:{server}:{tool}` permission path. The `mcp,js,skills` feature row must preserve both MCP
tool discovery and prompt-time skill discovery.

---

## SQL schema — `src/extras/js/skills/store.rs`

Database path: `<AppPaths.local_data_dir>/skills/skills.db`. Model downloads and rebuildable index
snapshots use `AppPaths.cache_dir`; no skill database is stored under configuration or the current
directory.

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

### `SkillStore`

#### CRUD operations

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

### `embed.rs`

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
- Corpora below 10,000 rows use exact contiguous dot-product ranking. Larger corpora use an
  immutable HNSW generation; the exact path remains the recall and regression oracle.
- SQLite BLOBs are read only when building a new generation, never per query.
- FTS5/BM25 produces lexical candidates from exact identifiers, exports, descriptions, and tags.
- Dense and lexical ranks are combined with reciprocal-rank fusion.
- A dense similarity floor may reduce the result to zero; top-k is a maximum, not a quota.
- Semantic near-duplicates are collapsed before applying source/manifest budgets.
- Within one immutable generation, final ordering is deterministic by fused score then full skill
  ID. Independently rebuilt HNSW graphs must each satisfy the exact-oracle recall gate.

Initial policy defaults are `max_skills = 3`, a configurable dense score floor, a compact manifest
budget, and a separate JS source-byte budget. Threshold calibration must use checked-in retrieval
fixtures; a magic score may not be accepted only because one model happened to emit it.

At the 100,000-skill target, benchmark query embedding separately from index search. The production
hybrid index must meet a 5 ms p99 search target and at least 95% ANN recall@10 against the exact
oracle on a documented reference machine, excluding embedding inference. The checked-in benchmark
also reports memory, build, rebuild, concurrent-read, retirement/purge visibility, and relevance
measurements. Large generations build off the request path and publish atomically; lifecycle
removal uses an immutable visibility mask so revocation does not wait for a graph rebuild.

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

### Preamble injection

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

The JS tool snapshots the exact bundle when the tool call begins and includes it in the execution
request. Under the delivered Phase 3 implementation, the JS thread performed no database access,
embedding, or ranking. Phase 6 preserves that boundary: the contained worker receives a frozen
bundle and no database, while all retrieval and ranking remain parent-owned.

### Historical runtime binding (superseded by Phase 6)

Delivered Phase 3 evaluated selected skill sources as script 1, validated and wrapped the declared
exports, then evaluated model-authored code as script 2 in the same fresh context:

```javascript
// Script 1, historically generated by the mini-agent parent from the frozen bundle
function parseJson(s) { /* immutable selected source */ }

// Script 2, exactly the model-authored code
parseJson(input)
```

Phase 6 replaces the shared context with its private skill realm, agent realm, hidden immutable
invocation capability object, and declared JSON-clone boundary. The separate-script line-number
and frozen-bundle requirements remain. Skill-source failures identify the full skill ID
and never rewrite the agent script. Missing/duplicate exports, source exceptions, capability
violations, and bundle identity mismatches fail closed. If the bundle is empty, evaluate only the
agent script and add no manifest.

---

## No-effect skill verification — `src/extras/js/skills/verify.rs`

The no-real-effect, exact-true, deterministic-fake, mutation, and error semantics in this section
remain authoritative. Its delivered parent/thread runtime ownership and same-context loader are
superseded by Phase 6 `Verification parity`, which requires the contained worker and the production
private-realm loader/ABI path.

```rust
pub fn verify_skill(skill: &SkillArtifact) -> Result<VerificationReport, VerificationError> {
    // Send the complete bounded artifact and cases to the contained JS worker.
    // The worker uses the production realm loader and hidden capability ABI for every case.
    // Require at least one test and exact JavaScript boolean true for every embedded test.
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

Delivered Phase 3 verification errors included the stage/test index and bounded stack information
but never activated or persisted the artifact. Phase 6 supersedes stack/message disclosure with its
stable sanitized error code, closed stage/script role, and source-free numeric location metadata.
In delivered Phase 3, each skill verification got a new runtime and fake state, and tests within one
run executed in declared order in the same fresh context. Phase 6 moves all QuickJS ownership out
of `skills/{verify,held_out,admission}.rs`: the parent sends a bounded `VerifyArtifact` request to
the contained supervisor. The worker creates one bounded runtime for that request, then reloads the
exact production private-realm loader and hidden-capability ABI into a fresh context for every
embedded, mutation, inherited, and held-out case. Each case also gets fresh deterministic fake
state, a fresh transcript, fresh one-invocation grants, and an independent pending-job drain. Tests
therefore cannot observe source state, fake effects, hidden fixtures, or jobs from another case.
Inherited predecessor scripts are sent as typed cases against the unchanged candidate artifact;
the verifier never rewrites the candidate's identity-bearing embedded tests. Although fake state
remains isolated per case, transcript accounting is shared across the whole request. Fake calls
reserve a conservative serialized-byte bound before cloning effect values, and a call or byte
limit breach returns a closed verification failure rather than overflowing the protocol frame.

The verifier also performs an anti-vacuity mutation pass. For each declared export, rerun the suite
with that export replaced by a throwing stub after the ordinary production loader has validated the
original artifact; at least one embedded test must then fail for the
expected reason. This proves that every public export is exercised, not that its behavior is fully
correct. Mutation runs use fresh contexts and the same resource bounds. An empty, always-true, or
unrelated suite cannot verify an artifact.

Manual Phase 3 insertion calls the verifier before identity-validating persistence. Store APIs do
not accept an “already verified” caller assertion without a corresponding trusted verification
report tied to the full artifact ID.

---

## Acceptance criteria

All must pass under `cargo test --features js,skills`:

- [x] Identity changes when source, test/order, export/signature, description/tag, capability, or
      identity version changes; operational metadata does not affect identity.
- [x] Agent Skills directories and ZIPs load according to the open `SKILL.md` format, preserve
      progressive disclosure, and reject traversal, links, archive bombs, reserved filenames, and
      cross-platform normalized collisions without executing resources.
- [x] `allowed-tools` and Agent Skill scripts never bypass permission/capability checks or enter the
      learned JS store without independent proposal and verification.
- [x] The skill database, model cache, import tree, and index snapshots resolve to the storage
      classes in `platform-paths.md` on Linux, macOS, and Windows.
- [x] Caller ID mismatch, row tampering, legacy short IDs, dimensions/model mismatch, and a
      simulated collision are rejected without returning source to retrieval.
- [x] No-effect verification requires nonempty exact-boolean tests, gives Tier 0 no host globals,
      gives Tier 1/2 only declared deterministic in-memory fakes, and mutation checks prove every
      declared export affects at least one test.
- [x] Embeddings are generated at admission/migration and tagged with model revision/dimensions.
- [x] The fastembed model and bounded query cache are reused; request-time retrieval never lazily
      embeds a stored skill.
- [x] The current user prompt is the primary query and retrieval completes before the first model
      output; generated JS is not present in the query.
- [x] Exact/ANN dense + FTS ranking, RRF fusion, threshold, dedupe, deterministic order, and manifest/
      source budgets have fixture tests.
- [x] The model-visible manifest contains only the frozen selected bundle's metadata and is present
      before the model emits a JS tool call.
- [x] Every JS call in one turn receives the same immutable bundle and retries do not re-embed.
- [x] Skill source and agent source run as separate scripts; an agent error on line N reports line N
      with zero, one, or three selected skills.
- [x] Pending/canary/quarantined/superseded/retired/rejected rows never appear as independent Phase
      3 search results; only Phase 5 may route an eligible canary after selecting its active lineage.
- [x] A 100,000-skill benchmark reports query-embedding and index-search latency separately and
      production ANN search meets the documented p99 and recall gates.
- [x] One query embedding is reused for typed Agent Skill and learned-JS indexes, with separate
      injection budgets; `mcp,js,skills` retains normal MCP discovery and permission checks.
- [x] `cargo test --features js` without `skills` passes unchanged.

---

## Out of scope for Phase 3

- UI for browsing or editing skills
- Cross-agent skill sharing (single-user local store only)
- Agent proposals and human-gated canary admission (Phase 4)
- Evidence-driven promotion, quarantine, repair, and rollback (Phase 5)
- Additional ANN backends or distributed/shared indexes
