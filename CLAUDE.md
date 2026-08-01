# mini-agent — Claude Instructions

The production Rust crate is the repository root: `Cargo.toml`, `src/`, and `docs/`. The separate
`spike/` crate is a QuickJS research artifact and is never a production target.

## Build rules

```bash
# Run from the repository root
cargo fmt                      # required before every commit
cargo test                     # type checking and tests; use instead of cargo check
cargo install --path . --debug # required development build/install command
```

**Never** run `cargo build`. **Never** use `--release` during development.
**Never** run `cargo check` — `cargo test` catches type errors and tests in one pass.
Write tests for new non-TUI production code and update `docs/` when adding modules.

## JS engine implementation

The integration lives at `src/extras/js/`. Key Phase 6 invariants to uphold:

1. **Parent-owned contained worker** — lazily keep at most one JS worker process live at a time and reuse its supervisor across `JsTool`/agent rebuilds. Never execute untrusted JS in the parent or fall back to an uncontained worker.
2. **`JsTool` is `Send + Sync`** — never store a QuickJS `Context`, `Runtime`, or value in `JsTool` or anywhere in the production parent process.
3. **Fresh `Runtime` per request** — recreate it for every `RunStep` and every whole `VerifyArtifact`; never reuse after success, OOM, timeout, or cancellation.
4. **Install limits before eval** — every runtime gets the 64 MiB heap limit, 512 KiB JS stack limit, and interrupt deadline before `ctx.eval(...)`.
5. **Drain bounded pending jobs and sanitize errors** — use `eval::<Value, _>`, bound the microtask drain, and return only a closed error class/code plus validated source-free stage/script-role/line/column metadata. Never disclose arbitrary JS messages, stacks, thrown values, source snippets, effect results, prompts, contents, or secrets.
6. **Broker every effect in the parent** — worker hosts emit typed requests. Parent policy, permissions, target narrowing, deadlines, output limits, and audit are authoritative.
7. **Brokered spawn uses the general sandbox** — the parent creates a requested command through `Sandbox::wrap_command`. The JS worker itself uses the separate broker-only fail-closed launcher, never the workspace-readable general-process profile.

## Feature gate

The existing JS engine is gated by the root `Cargo.toml`'s `js` feature. Preserve the default build,
keep QuickJS optional, and do not treat a Cargo feature as proof that Phase 6 containment is
available or delivered.

## Testing new JS host functions

Host functions must be tested both at the Rust unit level (typed protocol/broker boundary) and via
integration tests that launch the contained worker. See `src/extras/js/tests/`.

## Adding a new host global

1. Add a closed typed effect operation to the worker protocol.
2. Register only the worker-side closure appropriate to the agent realm; stored-skill realms receive no effect or writer globals.
3. Implement parent broker validation and bind it to a parent-created invocation grant.
4. Route permission and target narrowing through the owning parent policy; for `spawn`, create the command through `Sandbox::wrap_command`.
5. Write broker unit tests plus a contained-worker integration test covering denial and bounded failure.

## Platform notes

- On Windows, JS remains disabled unless the LPAC supported-install-location gate passes; parent-brokered JS `spawn` remains disabled until the separate general Windows command sandbox is delivered
- Hook subprocess.rs uses `("sh", "-c")` on unix / `("powershell", "-Command")` on Windows — do not change this without updating the hooks module
- `sandbox.rs`: `kill_process_group` is `#[cfg(unix)]` with empty Windows arm — keep it that way

## Dependency changes

Edit only the root `Cargo.toml` and `Cargo.lock`. Reuse existing dependencies instead of adding a
second version, keep optional/platform-specific dependencies behind their owning feature or target,
and preserve the Phase 6 minimal QuickJS feature-surface requirement. Validate changes with
`cargo test` and `cargo install --path . --debug`, never `cargo build` or `cargo check`.


<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:ca08a54f -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Storage mode

- This repository uses **embedded Dolt**. The database lives in `.beads/embeddeddolt/` and requires no external SQL server.
- Do not run `bd dolt start`, `bd dolt stop`, or configure shared/server mode for this repository.
- Do not add `dolt.shared-server`, `dolt_server_host`, or `dolt_server_port` settings. `.beads/metadata.json` must keep `dolt_mode: embedded`.
- Use ordinary `bd` commands directly. Embedded Dolt is single-writer and manages its own file lock.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

## Session Completion

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   bd dolt push
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds
<!-- END BEADS INTEGRATION -->
