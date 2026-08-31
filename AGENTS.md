# mini-agent — Agent Instructions

## Repository layout

```
mini-agent/
├── Cargo.toml           # Production crate manifest
├── src/
│   ├── agent/tools/     # Agent tool implementations
│   ├── extras/js/       # JavaScript engine, skills, and Phase 6 worker modules
│   └── sandbox.rs       # General process sandbox; worker containment is a submodule
├── docs/                # Architecture docs, normative specs, and agent documentation
├── spike/               # QuickJS research crate; never a production target
├── ARCHITECTURE.md      # JS engine integration architecture
├── SPEC.md              # Implementation specification
└── README.md            # Project overview
```

## Compilation rules (STRICT)

Run production crate commands from the repository root:
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
| Worker lifecycle and fresh QuickJS runtimes (Phase 6) | `src/extras/js/worker.rs`, `src/extras/js/realm.rs` |
| Parent worker supervision and effect broker (Phase 6) | `src/extras/js/supervisor.rs`, `src/extras/js/broker.rs` |
| JsTool (rig Tool impl) | `src/extras/js/tool.rs` |
| Host conversion and parent effect services | `src/extras/js/host.rs` |
| Wire protocol and parent-local result types | `src/extras/js/protocol.rs`, `src/extras/js/types.rs` |
| Skill store (Phase 3) | `src/extras/js/skills/` |
| Unit tests | `src/extras/js/tests/` |

Register `JsTool` in `src/agent/builder.rs` under `#[cfg(feature = "js")]`, alongside the existing bash tool injection at lines 230–265.

## Invariants — never break these

The single authoritative Phase 6 containment checklist is **Phase 6 security invariants (canonical)** in `docs/specs/phase-6-brokered-js-runtime.md`. Read and preserve that checklist for every change to the JS runtime, broker, protocol, or sandbox; do not maintain a second copy here.

## Skill library (Phase 3) invariants

- Identity version 2 is content-addressed by SHA-256 of the full canonical execution/discovery payload, ABI version, and structured capability scopes — never source alone
- Skills ship with `tests: Vec<String>` (JS expressions evaluating to `true`)
- Mutating tests changes the hash → invalidates the skill (integrity enforced structurally)
- Identity-v1 artifacts are quarantined under Phase 6 until explicitly reproposed and reverified; never infer version-2 scopes
- Retrieval via embedding cosine similarity on description field
- Auto-admission (Phase 4) requires a held-out Rust integration test to pass

## What NOT to do

- Do not add `final_answer` as a JS host global — the agent signals completion via the LLM response
- Do not expose `require()` or `import()` in the JS sandbox — no module system
- Do not use `.cargo/config.toml` link flags for stack size — not honored by `cargo install`
- Do not reuse `Runtime` across steps even if no OOM occurred — allocation state is unpredictable
- Keep `fetch()` parent-brokered under the delivered Phase 2 URL, permission, and narrowing
  contract; never grant the worker direct network access

## Testing the binary

Build and install the debug binary from the repo root:

```bash
cargo install --path . --debug
```

**Connectivity smoke test** — use headless `-p` mode (no terminal required):

```bash
mini-agent -p "say hello in one word"
```

A one-word reply confirms the binary connects to OpenRouter using `OPENROUTER_API_KEY`.
Ignore any MCP teardown noise at exit (e.g. Exa session cleanup errors) — those are benign.

**TUI testing** — the interactive UI requires a real terminal; use tmux when running inside
an agent or CI environment:

```bash
tmux new-session -d -s test -x 220 -y 50
tmux send-keys -t test 'mini-agent' Enter
sleep 3
tmux capture-pane -t test -p   # inspect rendered output
tmux kill-session -t test
```

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
