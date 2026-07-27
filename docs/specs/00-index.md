# Spec Index — mini-agent JS Engine

All specs derive from `SPEC.md` (single source of truth). Architecture decisions are in `ARCHITECTURE.md`.

| Phase | Spec file | Status | Delivers |
|-------|-----------|--------|---------|
| 1 | [phase-1-js-engine.md](phase-1-js-engine.md) | Pre-implementation | Core QuickJS integration, JsTool, host globals |
| 2 | [phase-2-sandbox.md](phase-2-sandbox.md) | Pre-implementation | `fetch()`, file allow-lists, birdcage process isolation |
| 3 | [phase-3-skill-library.md](phase-3-skill-library.md) | Pre-implementation | SQLite skill store, fastembed retrieval, preamble injection |
| 4 | [phase-4-auto-admission.md](phase-4-auto-admission.md) | Pre-implementation | `propose_skill()` host global, pending/promote lifecycle |

Prior research artifact (superseded by this index): `2026-07-27-js-engine-blueprint.md`

## Cross-phase dependencies

| Dependency | Produces | Consumed by |
|-----------|---------|------------|
| `pub(crate) fn run_step` | Phase 1 must declare it with this visibility | Phase 3 `verify_skill()` calls it cross-module |
| `SkillStore` + `Skill` types | Phase 3 | Phase 4 `propose_skill()` host global |
| `skills_pending` table | Phase 4 extends Phase 3's store | Phase 4 promotion gate |

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

**Not yet implemented**: `src/extras/js/` directory and all contents.

**Import path note**: `AskSender` and `PermCheck` are private `use` items in `src/agent/tools/mod.rs` (lines 84–85) — not `pub use`. Child modules of `tools/` (like `bash.rs`) can reach them, but `src/extras/js/tool.rs` cannot. Use the direct paths: `crate::permission::ask::AskSender` and `crate::permission::checker::PermCheck`.

## Build commands (mandatory)

```bash
cargo fmt                         # before every commit
cargo test --features js          # type-check + run tests
cargo install --path . --debug    # install development binary
# Never: cargo build, cargo check, --release
```
