# Phase 4 — Auto-Evolution (Skill Auto-Admission)

**Status**: Pre-implementation  
**Prerequisite**: Phase 1 and Phase 3 complete and passing  
**Delivers**: `propose_skill()` JS host global; pending/promote lifecycle; held-out Rust integration test harness; human approval gate.

---

## Overview

Phase 4 lets the agent nominate successful JS code as a reusable skill. The agent calls `propose_skill(source, description, tests)` from within a JS step. zerostack runs the `tests` in a fresh sandbox Runtime immediately. If all pass, the skill enters **pending** state. Promotion to the active store requires a held-out Rust integration test and human approval.

Auto-approval is **disabled** until evaluator gaming (agent learns to write tests that always pass regardless of skill quality) is studied.

---

## Admission trigger

After a JS step completes with `JsOutcome::Value` or `JsOutcome::Void`, the agent may call `propose_skill()`:

```javascript
// Agent JS:
function parseJson(s) { try { return JSON.parse(s); } catch(e) { return null; } }
parseJson('{"a":1}')  // → { a: 1 }

propose_skill(
  'function parseJson(s) { try { return JSON.parse(s); } catch(e) { return null; } }',
  'Parse JSON safely, returning null on error',
  ['parseJson("{\"a\":1}") !== null', 'parseJson("bad") === null']
)
```

`propose_skill` is a host global registered in Phase 4. It is **not** available in Phase 1–3.

---

## propose_skill() host global — `src/extras/js/host.rs`

Add `make_propose_skill` under `#[cfg(feature = "skills")]`:

```rust
pub fn make_propose_skill(
    store: Arc<Mutex<SkillStore>>,
    iteration_count: Arc<AtomicUsize>,
) -> impl Fn(String, String, Vec<String>) -> rquickjs::Result<String> {
    move |source: String, description: String, tests: Vec<String>| {
        // Guard: max 5 iterations per session
        let count = iteration_count.fetch_add(1, Ordering::SeqCst);
        if count >= 5 {
            return Ok("propose_skill: iteration limit reached (5 attempts per session)".into());
        }

        // 1. Verify all tests pass in a fresh sandbox Runtime
        let skill = Skill {
            id: sha256_hex(&source)[..16].to_string(),
            source: source.clone(),
            description: description.clone(),
            tests: tests.clone(),
            created_at: 0, // caller stamps with Unix timestamp
            usage_count: 0,
        };
        if let Err(e) = verify_skill(&skill) {
            return Ok(format!("propose_skill: test failed — {e}"));
        }

        // 2. Enter pending state (not yet in active store)
        let mut store = store.lock().unwrap_or_else(|e| e.into_inner());
        store.insert_pending(&skill)
            .map_err(|e| rquickjs::Error::new_from_js("propose_skill", &e.to_string()))?;
        Ok(format!("skill {} proposed (pending approval)", skill.id))
    }
}
```

Pending skills are stored in a separate `skills_pending` table (same database, same schema as `skills`). They are **not** injected as preamble until promoted.

---

## Pending state — `src/extras/js/skills/store.rs`

Add a parallel `skills_pending` table (extends Phase 3's store):

```sql
CREATE TABLE IF NOT EXISTS skills_pending (
    id          TEXT    PRIMARY KEY,
    source      TEXT    NOT NULL,
    description TEXT    NOT NULL,
    tests       TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    usage_count INTEGER NOT NULL DEFAULT 0,
    embedding   BLOB
);
```

New `SkillStore` methods (add to the existing impl block):

```rust
pub fn insert_pending(&mut self, skill: &Skill) -> anyhow::Result<()>;
pub fn list_pending(&self) -> anyhow::Result<Vec<Skill>>;
pub fn promote(&mut self, id: &str) -> anyhow::Result<()>;      // moves pending → active
pub fn reject_pending(&mut self, id: &str) -> anyhow::Result<()>;
```

`promote` copies the row from `skills_pending` to `skills`, then deletes it from `skills_pending`.

---

## Promotion gate

A pending skill is promoted when **all three** conditions are met:

### 1. All `tests` pass in a fresh sandbox Runtime

Already enforced at proposal time (`verify_skill`). Checked again at promotion to guard against Runtime or code changes between proposal and promotion.

### 2. Held-out Rust integration test passes

`src/extras/js/skills/verify.rs` provides a Rust test harness:

```rust
pub fn run_integration_tests(skill: &Skill) -> Result<(), String> {
    // Runs a fixed Rust-authored integration test for this skill.
    // The test is authored by a human (or future automation), not by the agent.
    // If no integration test exists for this skill id, returns Err("no integration test").
}
```

A registry maps skill `id` prefixes to Rust test functions. In Phase 4, this registry is empty by default — all skills without a matching test block at the "no integration test" error and cannot auto-promote. This is intentional: the harness is scaffolded; tests are added per-skill during human review.

### 3. Human approval via `Ask` prompt

```rust
// Promotion flow — runs in tokio
async fn promote_skill(skill: &Skill, ask_tx: &AskSender) -> anyhow::Result<()> {
    let approved = ask_user(
        ask_tx,
        &format!(
            "Approve skill `{}`?\n\nDescription: {}\n\nSource:\n{}",
            skill.id, skill.description, skill.source
        )
    ).await?;
    if !approved {
        return Err(anyhow::anyhow!("skill rejected by user"));
    }
    // proceed to promote
}
```

Auto-approval (`approved = true` without asking) is disabled until evaluator gaming is studied.

---

## Iteration loop

The agent may iterate on a skill proposal up to 5 times per session (tracked via `Arc<AtomicUsize>` initialized to 0 when the JS thread starts):

```
propose_skill(source_v1, ...) → test fails → error string returned to LLM
LLM revises JS
propose_skill(source_v2, ...) → tests pass → skill enters pending
... (up to 5 attempts total)
propose_skill(source_v6, ...) → "iteration limit reached (5 attempts per session)"
```

The counter is per-session (not persisted to disk). Resetting requires restarting the agent.

---

## Target files

| File | Status | Change |
|------|--------|--------|
| `src/extras/js/host.rs` | Phase 1 creates | Add `make_propose_skill()` under `#[cfg(feature = "skills")]` |
| `src/extras/js/skills/store.rs` | Phase 3 creates | Add `skills_pending` table, `insert_pending`, `list_pending`, `promote`, `reject_pending` |
| `src/extras/js/skills/verify.rs` | TO BE CREATED | Rust integration test harness and registry |
| `src/extras/js/engine.rs` | Phase 1 creates | Pass `store` and `iteration_count` to `js_thread_main` when `skills` feature is enabled |

---

## Acceptance criteria

All must pass under `cargo test --features js,skills`:

- [ ] `propose_skill(source, description, tests)` returns a success string when all tests pass
- [ ] `propose_skill()` returns an error string (not a panic) when any test fails
- [ ] Proposed skills appear in `store.list_pending()` and NOT in `store.list()` (active)
- [ ] `promote()` moves the skill from `skills_pending` to `skills`, making it available for preamble injection
- [ ] Promoting a skill without a matching Rust integration test returns `Err("no integration test")`
- [ ] The iteration counter prevents more than 5 `propose_skill` calls per session; the 6th returns the limit error string
- [ ] `cargo test --features js` (without `skills`) passes unchanged

---

## Out of scope for Phase 4

- LLM-assisted skill description generation (manual description required for now)
- Auto-approval without human confirmation (disabled until evaluator gaming studied)
- Cross-agent or cross-session skill collaboration
- Skill editing or source patching (content-addressed — edit = new skill with new id)
