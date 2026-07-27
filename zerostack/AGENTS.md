When compiling zerostack:
- Never run `cargo build`
- Don't use `--release` during development
- Never run `cargo check` (instead use `cargo test`)
- Always run `cargo fmt`
- Always run `cargo install --path . --debug`
- Run `cargo test` if you want to check all unit tests

Important notes:
- Always write tests when writing new non-TUI code.
- Always update docs/ files when needed.
- If adding or editing slash commands, edit the slash commands `/` picker in the TUI.

## JS engine (src/extras/js/)

The JS engine integration is gated behind `--features js`. Enable it:

```bash
cargo install --path . --debug --features js
cargo test --features js
```

Key invariants — never break:
1. `JsTool` fields must all be `Send + Sync`. Never put `Runtime` or `Context` in `JsTool`.
2. Create a **fresh `Runtime` per JS step** — OOM poisons the allocator, recovery is impossible.
3. Set `rt.set_memory_limit(64 * 1024 * 1024)` and `rt.set_max_stack_size(512 * 1024)` on every new `Runtime`.
4. Set the interrupt handler deadline BEFORE calling `ctx.eval(...)`.
5. Drain the microtask queue after every eval: `while rt.execute_pending_job() == Ok(true) {}`
6. Use `eval::<Value, _>`, never `eval::<(), _>` — the latter loses return values and stack traces.
7. All `spawn()` calls from JS must go through `Sandbox::wrap_command` — same path as `BashTool`.
8. `std::thread::Builder::new().stack_size(8 * 1024 * 1024)` for the JS thread — NOT `.cargo/config.toml`.

See `/Users/ahrens/projects/mini-agent/SPEC.md` for the full implementation specification.
