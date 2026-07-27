# mini-agent — Claude Instructions

This workspace contains a QuickJS PoC spike (`main.rs`) and the `zerostack/` coding agent.
The primary implementation target is `zerostack/`. All production code goes there.

## Build rules

```bash
# Spike (research only)
cargo run                      # runs main.rs directly

# zerostack (ALWAYS use these, never plain cargo build)
cd zerostack
cargo fmt                      # required before every commit
cargo test                     # unit tests — use instead of cargo check
cargo install --path . --debug # install development binary
```

**Never** run `cargo build` in zerostack. **Never** use `--release` during development.
**Never** run `cargo check` — `cargo test` catches type errors and tests in one pass.

## JS engine implementation

The integration lives at `zerostack/src/extras/js/`. Key invariants to uphold:

1. **Fresh `Runtime` per step** — never reuse across calls. OOM poisons the allocator.
2. **`JsTool` holds only `mpsc::Sender<JsRequest>`** — never a `Context` or `Runtime` (both `!Send`).
3. **Stack size via `std::thread::Builder::new().stack_size(8 * 1024 * 1024)`** — NOT `.cargo/config.toml`.
4. **Drain microtask queue** after every `eval`: `while rt.execute_pending_job() == Ok(true) {}`.
5. **Use `eval::<Value, _>`** — not `eval::<(), _>`. Extract `exception.stack()` for LLM self-correction.
6. **Interrupt handler fires only during JS bytecode** — add `tokio::time::timeout` on blocking host calls.

## Feature gate

The JS engine is gated behind `features = ["js"]` in `Cargo.toml`. Add `dep:rquickjs` as optional.
Do not enable by default until Phase 1 passes full test coverage.

## Testing new JS host functions

Host functions must be tested both at the Rust unit level (mock channel) and via integration tests
that actually spawn the JS thread. See `zerostack/src/extras/js/tests/` when it exists.

## Adding a new host global

1. Define handler in `js_thread_main` — receives from `oneshot` back to tokio for async permission checks
2. Register via `ctx.globals().set(name, Func::from(...))`
3. Add corresponding `JsRequest` variant
4. Route permission check through `check_perm` (same path as BashTool)
5. Write a unit test that exercises the handler in isolation

## Platform notes

- On Windows: bash feature is compiled out; `js` feature becomes the only action primitive
- Hook subprocess.rs uses `("sh", "-c")` on unix / `("powershell", "-Command")` on Windows — do not change this without updating the hooks module
- `sandbox.rs`: `kill_process_group` is `#[cfg(unix)]` with empty Windows arm — keep it that way

## Crate additions

When adding `rquickjs` to `zerostack/Cargo.toml`:
```toml
[features]
js = ["dep:rquickjs"]

[dependencies]
rquickjs = { version = "0.12", features = ["full"], optional = true }
```

Also add `birdcage` (Phase 2) and an embedding crate (Phase 3) as optional under their own feature gates.
