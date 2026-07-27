# mini-agent — Agent Instructions

## Repository layout

```
mini-agent/
├── main.rs              # QuickJS PoC spike (research artifact, not production)
├── zerostack/           # The coding agent — ALL production work happens here
│   ├── src/
│   │   ├── agent/tools/ # Tool implementations (bash.rs, js.rs coming)
│   │   ├── extras/js/   # JS engine module (to be created — Phase 1)
│   │   └── sandbox.rs   # Process sandboxing
│   ├── docs/            # Architecture docs and specs
│   └── Cargo.toml
├── ARCHITECTURE.md      # JS engine integration architecture
├── SPEC.md              # Implementation specification
└── README.md            # Project overview
```

## Compilation rules (STRICT)

Working inside `zerostack/`:
- **ALWAYS** `cargo fmt` before committing
- **ALWAYS** use `cargo install --path . --debug` to build
- **ALWAYS** use `cargo test` for type checking and tests
- **NEVER** `cargo build`
- **NEVER** `cargo check`
- **NEVER** `--release` during development
- **ALWAYS** write tests for new non-TUI code
- **ALWAYS** update `docs/` when adding new modules

## JS engine integration — where to put things

| Concern | Location |
|---------|----------|
| Runtime lifecycle, JS thread | `src/extras/js/engine.rs` |
| JsTool (rig Tool impl) | `src/extras/js/tool.rs` |
| Host function implementations | `src/extras/js/host.rs` |
| Request/response channel types | `src/extras/js/types.rs` |
| Skill store (Phase 3) | `src/extras/js/skills/` |
| Unit tests | `src/extras/js/tests/` |

Register `JsTool` in `src/agent/builder.rs` under `#[cfg(feature = "js")]`, alongside the existing bash tool injection at lines 230–265.

## Invariants — never break these

1. `JsTool` struct fields must all be `Send + Sync`. QuickJS types (`Runtime`, `Context`) must never be fields.
2. One dedicated OS thread per `JsTool` instance. That thread owns the `Runtime`/`Context` lifecycle.
3. `Runtime` is dropped and recreated for **every** JS step. No exceptions.
4. `set_memory_limit(64 * 1024 * 1024)` and `set_max_stack_size(512 * 1024)` on every new `Runtime`.
5. `set_interrupt_handler` deadline must be set before `ctx.eval(...)` is called.
6. All `spawn()` calls from JS must go through `Sandbox::wrap_command` — same sandboxing as bash.

## Skill library (Phase 3) invariants

- Content-addressed by `sha256(source)` — the ID is the hash
- Skills ship with `tests: Vec<String>` (JS expressions evaluating to `true`)
- Mutating tests changes the hash → invalidates the skill (integrity enforced structurally)
- Retrieval via embedding cosine similarity on description field
- Auto-admission (Phase 4) requires a held-out Rust integration test to pass

## What NOT to do

- Do not add `final_answer` as a JS host global — the agent signals completion via the LLM response
- Do not expose `require()` or `import()` in the JS sandbox — no module system
- Do not use `.cargo/config.toml` link flags for stack size — not honored by `cargo install`
- Do not reuse `Runtime` across steps even if no OOM occurred — allocation state is unpredictable
- Do not add `fetch()` until Phase 2 permission routing is implemented
