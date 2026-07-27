# mini-agent / JS Engine Integration

Research and implementation workspace for integrating QuickJS into **zerostack** as a cross-platform action primitive replacing bash.

## What lives here

| Path | Purpose |
|------|---------|
| `main.rs` | Proof-of-concept QuickJS spike — 5 host globals, fresh Runtime per step, timeout via interrupt handler |
| `zerostack/` | The coding agent itself — where the production integration goes |
| `zerostack/docs/superpowers/specs/2026-07-27-js-engine-blueprint.md` | Full architecture blueprint with all resolved decisions |

## The core idea

Bash is unavailable on Windows. Rather than shipping two tool implementations, zerostack gets a **tiny built-in JavaScript engine** (QuickJS-NG via rquickjs 0.12.x) as its primary action primitive. The agent writes JS instead of shell commands; the engine runs it natively with hard memory + CPU limits.

CodeAct research shows ~20% higher task success vs JSON tool calling. The JS engine also doubles as a **skill library runtime** — agents accumulate reusable JS functions that future agent steps retrieve via embedding search (Voyager model).

## Quick start

```bash
# Build the PoC spike
cd /Users/ahrens/projects/mini-agent
rustup target add x86_64-pc-windows-msvc  # if you need Windows testing
cargo run --manifest-path Cargo.toml 2>/dev/null

# Build zerostack (development mode)
cd zerostack
cargo fmt
cargo test
cargo install --path . --debug
```

## Key constraints

- **Fresh Runtime per step** — QuickJS allocator is poisoned after OOM; recovery requires allocation. ~500μs overhead, accepted.
- **Dedicated OS thread** — QuickJS `Context` is `!Send`. `JsTool` holds only `mpsc::Sender<JsRequest>`.
- **8 MiB stack via `std::thread::Builder::stack_size`** — not `.cargo/config.toml` which `cargo install` ignores.
- **rquickjs only** — Boa/Brimstone/Nova lack hard resource limits. No alternative survives the audit.

## Documentation

- [ARCHITECTURE.md](ARCHITECTURE.md) — threading model, host API, sandbox backends, skill library
- [SPEC.md](SPEC.md) — module layout, exact types, phase-by-phase implementation
- [Blueprint (HTML artifact)](https://claude.ai/code/artifact/9904fa51-2b70-41e5-8c7f-a449842b407c) — full design doc with all resolved decisions

## Phases

| Phase | Scope | Status |
|-------|-------|--------|
| 1 | Core JS engine: JsTool, threading, host globals, permissions | Planned |
| 2 | Sandbox hardening: birdcage, fetch(), file allow-lists | Planned |
| 3 | Skill library: content-addressed store, embedding retrieval | Planned |
| 4 | Auto-evolution: held-out test gate, Voyager loop | Planned |
