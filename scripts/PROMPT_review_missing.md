# Review: Missing Coverage — mini-agent

You are auditing what's specified in SPEC.md but not yet implemented, tested, or tracked as a bead.

## Setup

1. Read `SPEC.md` fully — all four phases.
2. Read `CLAUDE.md`, `ARCHITECTURE.md`, and all `docs/specs/*.md` files.
3. Check existing beads: `bd list --limit 0` and `bd stats`.
4. Survey what's actually implemented with narsil-mcp:
   ```
   mcp__narsil-mcp__get_project_structure()
   mcp__narsil-mcp__find_symbols("JsTool")
   mcp__narsil-mcp__find_symbols("js_thread_main")
   mcp__narsil-mcp__find_symbols("SkillStore")
   mcp__narsil-mcp__find_symbols("birdcage")
   mcp__narsil-mcp__workspace_symbol_search("phase")
   ```

## Bead filing protocol

```bash
bd create --title="MISSING: <short summary>" --type=task --priority=<1-3> \
  --description="Spec reference: <docs/specs/phase-X-*.md §section>
What's missing: <what SPEC.md requires that doesn't exist>
Confirmed absent via: narsil-mcp find_symbols returned no results for '<symbol>'
Expected location: <file path per docs/specs/ file>
Acceptance criteria:
- <concrete testable check>
- <concrete testable check>
Out of scope: <what this task does not do>"
```

## Missing coverage vectors to investigate

### 1. Phase 1 — JS engine

Spec requires these; check if each exists:
```
mcp__narsil-mcp__find_symbols("JsTool")            # src/extras/js/tool.rs
mcp__narsil-mcp__find_symbols("JsRequest")         # src/extras/js/types.rs
mcp__narsil-mcp__find_symbols("JsOutcome")         # src/extras/js/types.rs
mcp__narsil-mcp__find_symbols("js_thread_main")    # src/extras/js/engine.rs
mcp__narsil-mcp__find_symbols("read_file")         # src/extras/js/host.rs
mcp__narsil-mcp__find_symbols("write_file")        # src/extras/js/host.rs
```

For each missing symbol, file a MISSING bead pointing to the exact spec section in
`docs/specs/phase-1-js-engine.md` and the expected target file.

### 2. Phase 1 — Integration tests

SPEC.md requires tests in `src/extras/js/tests/`:
- Unit test: `JsTool` is `Send + Sync`
- Unit test: `Runtime` dropped between steps (use `Rc` sentinel pattern from SPEC.md)
- Unit test: `set_memory_limit` → allocation beyond limit returns error, not OOM
- Integration test: host `read_file` works with allowed path
- Integration test: host `spawn` is sandboxed

Are any of these tests missing?

### 3. Phase 2 — Sandbox hardening

```
mcp__narsil-mcp__find_symbols("birdcage")
mcp__narsil-mcp__find_symbols("Landlock")
mcp__narsil-mcp__find_symbols("Seatbelt")
```

If Phase 2 is not started, file one MISSING bead per deliverable:
- `sandbox = ["dep:birdcage"]` feature gate in Cargo.toml
- Landlock integration (Linux)
- Seatbelt integration (macOS)
- Windows empty arm preservation

### 4. Phase 3 — Skill library

```
mcp__narsil-mcp__find_symbols("SkillStore")
mcp__narsil-mcp__find_symbols("skills")
mcp__narsil-mcp__find_symbols("embedding")
```

### 5. Spec files themselves

Are `docs/specs/*.md` files missing or empty?
```bash
ls -la docs/specs/
wc -l docs/specs/*.md 2>/dev/null || echo "No spec files found"
```

If spec files are missing, that is the highest-priority finding — the plan mode (`./scripts/loop.sh plan`)
must be run first. File a P0 bead: "Run plan mode to generate docs/specs/ before decompose/build."

### 6. Build rule coverage

CLAUDE.md mandates `cargo install --path . --debug`. Is there a bead or CI check ensuring this works?
Is there a bead for `cargo test --features js` to cover the JS feature gate path?

## Deduplication protocol

Before filing: `bd search "MISSING"`. Check `bd list --limit 0` for existing coverage.

## After completing

```bash
bd dolt push
```

Report: count of missing items by phase, any critical blockers (missing spec files, missing feature gates).
