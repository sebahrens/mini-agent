# Spec Index — mini-agent JS Engine

**Authority**: the indexed phase specs in this directory are normative. `ARCHITECTURE.md`
summarizes resolved boundaries and `SPEC.md` is an implementation overview; neither may
override a phase spec. The dated JS blueprints are research artifacts and are superseded.
When documents conflict, the highest numbered applicable normative phase spec wins, then
this index. Tracker issues must link to a normative phase section.

| Phase | Spec file | Status | Delivers |
|-------|-----------|--------|---------|
| 1 | [phase-1-js-engine.md](phase-1-js-engine.md) | Pre-implementation | Core QuickJS integration, JsTool, host globals |
| 2 | [phase-2-sandbox.md](phase-2-sandbox.md) | Pre-implementation | `fetch()`, file allow-lists, birdcage process isolation |
| 3 | [phase-3-skill-library.md](phase-3-skill-library.md) | Pre-implementation | Immutable skill store, prompt-time hybrid retrieval, turn-scoped injection |
| 4 | [phase-4-auto-admission.md](phase-4-auto-admission.md) | Pre-implementation | No-effect candidate verification, held-out cases, human-gated canary admission |
| 5 | [phase-5-evidence-learning.md](phase-5-evidence-learning.md) | Pre-implementation | Evidence-based promotion, telemetry, quarantine, repair, supersession, rollback |

Prior research artifacts superseded by this index:

- `docs/specs/2026-07-27-js-engine-blueprint.md`
- `docs/superpowers/specs/2026-07-27-js-engine-blueprint.md`

## Cross-phase dependencies

| Dependency | Produces | Consumed by |
|-----------|---------|------------|
| Bounded QuickJS runtime builder with explicit host mode | Phase 1/2 | Phase 3 no-effect verification and turn execution |
| `SkillStore`, immutable `SkillArtifact`, and `SkillIndex` | Phase 3 | Phase 4 proposal/admission and Phase 5 lifecycle |
| `SkillTurnContext` + `TurnSkillBundle` | Phase 3 | Model manifest, exact runtime binding, Phase 5 attribution |
| Pending/verified/canary states and held-out evaluation cases | Phase 4 | Phase 5 evidence policy |
| Invocation events and lineage transitions | Phase 5 | Automatic quarantine, repair, promotion, and rollback |

## Phase entry and exit rules

- A phase may be decomposed while its prerequisite is open, but implementation cannot be
  marked delivered until every prerequisite delivery epic and every direct blocker is closed.
- Schema creation, compilation, or issue decomposition does not satisfy a phase exit gate.
- New defects discovered during implementation must be linked as blockers before closing the
  affected phase epic.
- Phase 3 retrieval must be driven by the current user prompt before model generation. Generated
  JavaScript is never the retrieval query.
- Phase 4 candidate code is untrusted and must execute in a no-effect verifier before approval:
  Tier 0 receives no host globals, while Tier 1/2 receive only declared deterministic in-memory
  fakes that can never touch the real filesystem, process table, or network.
- Phase 5 never mutates active source in place. Repair creates a new immutable revision and all
  automatic decisions retain evidence and a reversible predecessor link.

## Confirmed source locations (re-verified 2026-07-27)

The monorepo was flattened (old `zerostack/` → repo root). All source is under `src/` at the repo root.
References to `zerostack/src/` in older docs mean `src/`.

**Verification method**: `narsil-mcp` does not index this repo. Use `grep -n <pattern> <file>` directly to confirm line numbers before implementing.

| Symbol | Location | Line |
|--------|----------|------|
| `js = ["dep:rquickjs"]` feature | `Cargo.toml` | 37 |
| `rquickjs` dep | `Cargo.toml` | 80 |
| `reqwest = "0.13"` dep | `Cargo.toml` | 67 |
| `pub type AskSender` | `src/permission/ask.rs` | 5 |
| `pub type PermCheck` | `src/permission/checker.rs` | 10 |
| `pub enum ToolError` | `src/agent/tools/mod.rs` | 88 |
| `pub async fn check_perm` | `src/agent/tools/mod.rs` | 199 |
| `check_perm(...)` call in BashTool | `src/agent/tools/bash.rs` | 137 |
| `pub struct Sandbox` | `src/sandbox.rs` | 9 |
| `pub fn is_effectively_sandboxed` | `src/sandbox.rs` | 91 |
| `fn bwrap_exists` | `src/sandbox.rs` | 18 |
| `fn zerobox_exists` | `src/sandbox.rs` | 24 |
| `pub fn wrap_command` | `src/sandbox.rs` | 109 |
| `pub async fn output_command` | `src/sandbox.rs` | 205 |
| `pub(crate) fn kill_process_group` | `src/sandbox.rs` | 294 |
| `let mut all_tools` | `src/agent/builder.rs` | 279 |
| `filter_tools_by_allowlist` call | `src/agent/builder.rs` | 335 |
| `pub(crate) mod truncate` (last line) | `src/extras/mod.rs` | 40 |

**Current status**: the Phase 1 JS substrate exists under `src/extras/js/`, but the skill store,
retrieval index, admission pipeline, and evidence-learning lifecycle are not implemented.

**Import path note**: `AskSender` and `PermCheck` are private `use` items in `src/agent/tools/mod.rs` (lines 84–85) — not `pub use`. Child modules of `tools/` (like `bash.rs`) can reach them, but `src/extras/js/tool.rs` cannot. Use the direct paths: `crate::permission::ask::AskSender` and `crate::permission::checker::PermCheck`.

## Build commands (mandatory)

```bash
cargo fmt                         # before every commit
cargo test --features js          # type-check + run tests
cargo install --path . --debug    # install development binary
# Never: cargo build, cargo check, --release
```
