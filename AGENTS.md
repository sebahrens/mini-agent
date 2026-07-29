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
