# Spec Index — mini-agent JS Engine

All specs derive from `SPEC.md` (single source of truth). Architecture decisions are in `ARCHITECTURE.md`.

| Phase | Spec file | Status | Delivers |
|-------|-----------|--------|---------|
| 1 | [phase-1-js-engine.md](phase-1-js-engine.md) | Pre-implementation | Core QuickJS integration, JsTool, host globals |
| 2 | [phase-2-sandbox.md](phase-2-sandbox.md) | Pre-implementation | `fetch()`, file allow-lists, birdcage process isolation |
| 3 | [phase-3-skill-library.md](phase-3-skill-library.md) | Pre-implementation | SQLite skill store, fastembed retrieval, preamble injection |
| 4 | [phase-4-auto-admission.md](phase-4-auto-admission.md) | Pre-implementation | `propose_skill()` host global, pending/promote lifecycle |

Prior research artifact (superseded by this index): `2026-07-27-js-engine-blueprint.md`

## Quick orientation

The monorepo was flattened in commit `7872f7b` (`zerostack/ → root`). All source lives under `src/` at the repo root — not under `zerostack/src/`. References to `zerostack/` in older docs mean `src/`.

The `js` feature gate and `rquickjs` dependency are **already present** in `Cargo.toml` (lines 37 and 80). No Cargo.toml edits are needed for Phase 1.

## Build commands (mandatory)

```bash
cargo fmt                         # before every commit
cargo test --features js          # type-check + run tests
cargo install --path . --debug    # install development binary
# Never: cargo build, cargo check, --release
```
