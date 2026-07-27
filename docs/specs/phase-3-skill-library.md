# Phase 3 — Skill Library

**Status**: Pre-implementation  
**Prerequisite**: Phase 1 complete and passing  
**Delivers**: A content-addressed SQLite skill store with embedding-based retrieval. Top-K skills are injected as a JS preamble before each agent step.

---

## Overview

Implements the Voyager model: the agent accumulates reusable JavaScript functions ("skills") that survive across sessions. Before each JS step the store retrieves the top-3 most relevant skills by cosine similarity and injects them as a preamble block. Skills are content-addressed by `sha256(source)` — mutating the source invalidates the skill structurally, not by policy.

This is the substrate for Phase 4 auto-admission. In Phase 3, skills are added manually (via CLI or direct store API). Auto-admission from successful agent steps is Phase 4.

---

## Feature gate

```toml
# Cargo.toml additions
[features]
skills = ["dep:fastembed", "dep:rusqlite"]

[dependencies]
fastembed  = { version = "3", optional = true }
rusqlite   = { version = "0.31", features = ["bundled"], optional = true }
```

Gate all skill code behind `#[cfg(feature = "skills")]`. The binary without `--features skills` must compile and all Phase 1/2 tests must pass.

---

## File placement

All new files in `src/extras/js/skills/` (to be created):

| File | Status | Purpose |
|------|--------|---------|
| `src/extras/js/skills/mod.rs` | TO BE CREATED | Module entry, `Skill` struct, `verify_skill` |
| `src/extras/js/skills/store.rs` | TO BE CREATED | SQLite store, content-addressed CRUD |
| `src/extras/js/skills/embed.rs` | TO BE CREATED | fastembed wrapper, cosine similarity, top-K retrieval |
| `src/extras/js/mod.rs` | Phase 1 creates | Add `#[cfg(feature = "skills")] pub mod skills;` |

---

## Skill struct — `src/extras/js/skills/mod.rs`

```rust
pub struct Skill {
    pub id:          String,   // sha256(source) hex — first 16 chars used as store key
    pub source:      String,   // JS function source (the actual code)
    pub description: String,   // embedded for retrieval; human-readable
    pub tests:       Vec<String>, // JS expressions each evaluating to true
    pub created_at:  u64,      // Unix timestamp
    pub usage_count: u64,
}
```

**Content-addressing invariant**: `id = sha256(source)[..16]`. Changing `source` changes `id` and the old skill record is unreachable. This is structurally enforced — there is no `UPDATE source` operation.

---

## SQL schema — `src/extras/js/skills/store.rs`

Database path: `~/.config/zerostack/skills.db` (respects `$XDG_CONFIG_HOME`). Use `rusqlite` with `features = ["bundled"]` so no system SQLite is required.

> **Schema note**: `SPEC.md §3.2` shows a `name TEXT NOT NULL` column that is absent from the `Skill` struct in `SPEC.md §3.1`. The schema below is the resolved version — it matches the struct exactly (no `name`, includes `created_at` and `usage_count`). Do not add a `name` column.

```sql
CREATE TABLE IF NOT EXISTS skills (
    id          TEXT    PRIMARY KEY,
    source      TEXT    NOT NULL,
    description TEXT    NOT NULL,
    tests       TEXT    NOT NULL,   -- JSON array of strings
    created_at  INTEGER NOT NULL,
    usage_count INTEGER NOT NULL DEFAULT 0
);
```

### CRUD operations

```rust
pub struct SkillStore {
    conn: rusqlite::Connection,
}

impl SkillStore {
    pub fn open() -> anyhow::Result<Self>;          // opens ~/.config/zerostack/skills.db

    /// Insert a skill. Returns Err if id already exists (content-addressed — no update).
    pub fn insert(&mut self, skill: &Skill) -> anyhow::Result<()>;

    pub fn get(&self, id: &str) -> anyhow::Result<Option<Skill>>;

    pub fn list(&self) -> anyhow::Result<Vec<Skill>>;

    pub fn increment_usage(&mut self, id: &str) -> anyhow::Result<()>;

    pub fn delete(&mut self, id: &str) -> anyhow::Result<()>;
}
```

The `embedding` column (Phase 3 extension) stores a `BLOB` of little-endian `f32` values. It is added to the schema after the skills table:

```sql
ALTER TABLE skills ADD COLUMN embedding BLOB;
```

Or define it in the initial schema:

```sql
CREATE TABLE IF NOT EXISTS skills (
    id          TEXT    PRIMARY KEY,
    source      TEXT    NOT NULL,
    description TEXT    NOT NULL,
    tests       TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    usage_count INTEGER NOT NULL DEFAULT 0,
    embedding   BLOB            -- NULL until embed() is called
);
```

---

## Embedding index — `src/extras/js/skills/embed.rs`

**Model**: `BAAI/bge-small-en-v1.5` via `fastembed` crate (~30 MiB download, cached locally). No API call required — fully local inference.

### Embedding a skill

```rust
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

pub fn embed_description(description: &str) -> anyhow::Result<Vec<f32>> {
    let model = TextEmbedding::try_new(
        InitOptions::new(EmbeddingModel::BGESmallENV15)
    )?;
    let embeddings = model.embed(vec![description], None)?;
    Ok(embeddings.into_iter().next().unwrap_or_default())
}
```

### Storing embeddings

After inserting a skill, compute its embedding and store it:

```rust
pub fn store_embedding(conn: &rusqlite::Connection, id: &str, embedding: &[f32]) -> anyhow::Result<()> {
    let blob: Vec<u8> = embedding.iter()
        .flat_map(|f| f.to_le_bytes())
        .collect();
    conn.execute("UPDATE skills SET embedding = ?1 WHERE id = ?2", (blob, id))?;
    Ok(())
}
```

### Retrieval — cosine similarity

```rust
pub fn top_k_skills(
    conn: &rusqlite::Connection,
    query_embedding: &[f32],
    k: usize,
) -> anyhow::Result<Vec<Skill>> {
    // Load all skills with non-null embeddings from SQLite
    // Compute cosine similarity in Rust for each
    // Return top-k sorted by similarity descending
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 { return 0.0; }
    dot / (norm_a * norm_b)
}
```

For Phase 3 scale (tens to hundreds of skills), linear scan is sufficient. A vector index (e.g., HNSW) is a Phase 3+ stretch concern.

---

## Preamble injection — `src/extras/js/engine.rs`

Before evaluating agent code, retrieve top-3 skills and prepend as a preamble:

```javascript
// === Skill library (auto-injected) ===
// skill:abc123def456 — parse JSON safely
function parseJson(s) { try { return JSON.parse(s); } catch(e) { return null; } }
// skill:789012345678 — read lines from file
function readLines(path) { return read_file(path).split('\n').filter(l => l.length > 0); }
// === End skill library ===

// Agent code:
<agent JS code here>
```

Implementation in `run_step` (or a wrapper):

```rust
fn build_full_code(agent_code: &str, skills: &[Skill]) -> String {
    if skills.is_empty() {
        return agent_code.to_string();
    }
    let mut preamble = String::from("// === Skill library (auto-injected) ===\n");
    for skill in skills {
        preamble.push_str(&format!("// skill:{} — {}\n", skill.id, skill.description));
        preamble.push_str(&skill.source);
        preamble.push('\n');
    }
    preamble.push_str("// === End skill library ===\n\n// Agent code:\n");
    preamble.push_str(agent_code);
    preamble
}
```

Retrieval uses the agent step's current context (last N tokens of the conversation) as the query for embedding. Top-3 is the default `k`.

---

## Skill verification — `src/extras/js/skills/mod.rs`

**Prerequisite**: Phase 1 must declare `run_step` as `pub(crate)` (not private `fn`). The Phase 1 spec already annotates this; verify before implementing Phase 3.

```rust
pub fn verify_skill(skill: &Skill) -> Result<(), String> {
    for test_expr in &skill.tests {
        // Run each test expression in a fresh sandbox Runtime (reuse run_step machinery)
        let result = crate::extras::js::engine::run_step(
            &format!("({}); {}", skill.source, test_expr)
        );
        match result {
            JsOutcome::Value(v) if v == "true" => {}
            JsOutcome::Void => {}
            other => {
                return Err(format!(
                    "test failed: `{}` → {:?}",
                    test_expr, other
                ));
            }
        }
    }
    Ok(())
}
```

Tests must pass before a skill is inserted into the store. Violation: `insert()` returns `Err`.

---

## Acceptance criteria

All must pass under `cargo test --features js,skills`:

- [ ] `SkillStore::insert` stores a skill and `SkillStore::get` retrieves it by id
- [ ] Inserting two skills with different `source` strings creates two distinct records (different ids)
- [ ] Inserting a skill with identical `source` returns `Err` (content-addressed — no duplicates)
- [ ] `embed_description` returns a non-empty `Vec<f32>` for any non-empty string
- [ ] `top_k_skills` returns at most `k` skills, sorted by cosine similarity descending
- [ ] `build_full_code` prepends the preamble and the agent's code follows after it
- [ ] `verify_skill` returns `Ok(())` when all test expressions evaluate to `true`
- [ ] `verify_skill` returns `Err` when any test expression evaluates to a non-true value
- [ ] `cargo test --features js` (without `skills`) passes unchanged

---

## Out of scope for Phase 3

- UI for browsing or editing skills
- Cross-agent skill sharing (single-user local store only)
- LLM-assisted skill description generation (manual description required)
- Auto-admission from successful agent steps (Phase 4)
- Vector index / ANN search (linear scan is sufficient at Phase 3 scale)
