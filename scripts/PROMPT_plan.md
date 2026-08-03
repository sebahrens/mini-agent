# Plan Mode — mini-agent

You are an AI agent creating detailed implementation spec files for the mini-agent project.
Your job is **documentation only** — you produce `docs/specs/*.md` files from `SPEC.md`.
You do **not** write Rust code and you do **not** create beads in this pass.

## Critical context

- `SPEC.md` at repo root is the single source of truth (four implementation phases).
- `ARCHITECTURE.md` has the threading model, host API, and all resolved design decisions.
- `CLAUDE.md` has the mandatory build rules (never `cargo build`, never `cargo check`).
- `AGENTS.md` lists invariants that must never be broken.
- Bead prefix is `mini-agent-`. Beads are created in decompose mode, not here.
- Production workspace: root package (`mini-agent`) only. `spike/` is a standalone research crate and never a production target. JS engine is feature-gated: `--features js`.

## Step 1 — Read the project

Read these files in order:
1. `SPEC.md` — four phases, exact types, function signatures
2. `ARCHITECTURE.md` — threading model, host API, permission flow, feature gate
3. `CLAUDE.md` — build rules and JS engine invariants
4. `AGENTS.md` — file placement table, invariants, prohibitions

Then survey the existing source tree with narsil-mcp to understand what already exists
before writing any spec (do not speculate about what's implemented — verify):

```
mcp__narsil-mcp__get_project_structure()          # full filesystem + module layout
mcp__narsil-mcp__find_symbols("JsTool")           # does JsTool already exist?
mcp__narsil-mcp__find_symbols("BashTool")         # locate the reference permission pattern
mcp__narsil-mcp__find_symbols("PermCheck")        # confirm permission types
mcp__narsil-mcp__get_symbol_definition("Tool")    # locate the rig Tool trait
mcp__narsil-mcp__find_symbols("AgentBuilder")     # find builder.rs entry point
mcp__narsil-mcp__find_dead_code()                 # surface stale code worth noting
```

Check what spec files already exist:
```bash
ls docs/specs/
```

## Step 2 — Create the spec index

Create or update `docs/specs/00-index.md` with a table linking to each phase spec file.

## Step 3 — Create one spec file per phase

Create (or fully rewrite) these four files. Each spec must be **implementation-ready** —
an agent can open it, read it, and implement without reading any other document.

### `docs/specs/phase-1-js-engine.md`

Cover the complete JS engine integration (Phase 1 from SPEC.md). Must include:

- **Overview**: what this phase delivers and why
- **Feature gate**: exactly `js = ["dep:rquickjs"]` in Cargo.toml; what compiles without it
- **File placement table** (from AGENTS.md):
  - `src/extras/js/types.rs` — channel types
  - `src/extras/js/engine.rs` — Runtime lifecycle + JS thread
  - `src/extras/js/tool.rs` — JsTool (rig Tool impl)
  - `src/extras/js/host.rs` — host globals
  - `src/extras/js/mod.rs` — module entry
  - `src/extras/mod.rs` — where `#[cfg(feature = "js")] pub mod js;` is added
  - `src/agent/builder.rs` — where JsTool is registered (lines 230-265)
- **Exact types** (copy verbatim from SPEC.md §Types):
  - `STEP_TIMEOUT`, `MEMORY_LIMIT`, `STACK_LIMIT`, `THREAD_STACK` constants
  - `JsRequest` struct with `code: String` and `reply: tokio::sync::oneshot::Sender<JsResponse>`
  - `JsOutcome` enum variants
  - `JsTool` struct fields (only `mpsc::Sender<JsRequest>`, `PermCheck`, `AskSender`)
- **Thread spawn pattern** (exact code from ARCHITECTURE.md)
- **Runtime lifecycle** (exact code: `set_memory_limit`, `set_max_stack_size`, `set_interrupt_handler`)
- **Host globals** and their semantics:
  - `read_file(path)` → `Result<string, string>`
  - `write_file(path, content)` → `Result<null, string>`
  - `spawn(cmd, args[])` → `Result<{stdout,stderr,status}, string>`
  - `console.log(...)` → void
- **Permission flow**: `JsRequest` → `check_perm` → same path as `BashTool` (reference `src/agent/tools/bash.rs` exact function name and line range found by narsil-mcp)
- **Microtask drain**: `while rt.execute_pending_job() == Ok(true) {}`
- **Error surfacing**: extract `exception.message()` + `exception.stack()` for LLM self-correction
- **Acceptance criteria** (≥ 5 testable bullets):
  - `cargo test --features js` passes
  - `JsTool` implements `rig::tool::Tool` and is `Send + Sync`
  - A fresh `Runtime` is created and dropped for every `js_run_step()` call
  - `set_memory_limit(64 * 1024 * 1024)` is called on every new Runtime
  - Interrupt handler deadline prevents infinite loops
  - host `spawn()` is sandboxed via `Sandbox::wrap_command` (same as BashTool)
- **Out of scope**: `fetch()`, `require()`, `import()`, `final_answer` host global

### `docs/specs/phase-2-sandbox.md`

Cover sandbox hardening with `birdcage` (Phase 2 from SPEC.md). Must include:

- **Feature gate**: `sandbox = ["dep:birdcage"]` — separate from `js`
- **Target files**: `src/sandbox.rs` (extend existing), `Cargo.toml` (add optional birdcage dep)
- **Current state**: what `src/sandbox.rs` already does (populate from narsil-mcp survey)
- **What birdcage adds**: Landlock (Linux) + Seatbelt (macOS) abstraction
- **Integration points**: `Sandbox::wrap_command` called by both `BashTool` and `JsTool`
- **Platform matrix**: Linux (Landlock), macOS (Seatbelt/sandbox-exec), Windows (empty arm — see `#[cfg(unix)]` pattern already in sandbox.rs)
- **Acceptance criteria** (≥ 3 testable bullets)
- **Out of scope**: Windows sandbox enforcement in Phase 2

### `docs/specs/phase-3-skill-library.md`

Cover the Voyager-model skill library (Phase 3 from SPEC.md). Must include:

- **Feature gate**: `skills = ["dep:fastembed", "dep:rusqlite"]`
- **Target files**: `src/extras/js/skills/mod.rs`, `src/extras/js/skills/store.rs`, `src/extras/js/skills/embed.rs`
- **SQL schema** (from SPEC.md — exact DDL):
  ```sql
  CREATE TABLE skills (
    id TEXT PRIMARY KEY,   -- sha256(source)[..16]
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    source TEXT NOT NULL,
    tests TEXT NOT NULL,   -- JSON array of JS expressions → bool
    embedding BLOB         -- f32 LE vector
  );
  ```
- **Content-addressing invariant**: `id = sha256(source)[..16]` — changing source invalidates the skill
- **Retrieval**: cosine similarity on `embedding` field using fastembed local model
- **Injection**: top-K skills prepended as preamble to JS step
- **Auto-admission gate** (Phase 4): skill must pass its own `tests` in a sandboxed Runtime before storage
- **Acceptance criteria** (≥ 3 testable bullets)
- **Out of scope**: UI for browsing/editing skills, cross-agent skill sharing

### `docs/specs/phase-4-auto-admission.md`

Cover auto-admission of skills from successful JS steps (Phase 4 from SPEC.md). Must include:

- **Trigger**: after a JS step produces a non-error `JsOutcome::Value` or `JsOutcome::Void`, the agent may nominate the code as a skill
- **Admission gate**: nominated code runs its `tests` in a fresh sandbox Runtime; all must return `true`
- **Integration test**: a held-out Rust `#[test]` must pass (not just the JS self-tests)
- **Target files**: extends `src/extras/js/engine.rs` (admission call after successful step), `src/extras/js/skills/store.rs`
- **Acceptance criteria** (≥ 3 testable bullets)
- **Out of scope**: LLM-assisted skill description generation (manual description required)

## Step 4 — Verify

After creating the files:
```bash
ls -la docs/specs/
```

Check that each file is non-empty and references the exact source locations narsil-mcp confirmed.

## Rules

- Write spec files only — no Rust code, no beads, no SPEC.md edits.
- Every file path in specs must match what narsil-mcp confirmed exists (or be clearly marked as "to be created").
- Every function/type reference must match the exact name returned by narsil-mcp.
- If narsil-mcp cannot find a symbol, note "NOT YET IMPLEMENTED" rather than inventing a location.

## Now begin

Read the project files and narsil-mcp survey, then create the four phase spec files and the index.
