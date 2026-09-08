# Review: Missing Coverage — mini-agent

For JavaScript runtime review, use **Phase 6 security invariants (canonical)** in
`docs/specs/phase-6-brokered-js-runtime.md` as the authority. Check current callers and behavior;
retired Phase 1 symbols are not missing implementation requirements.

You are auditing what's specified in SPEC.md but not yet implemented, tested, or tracked as a bead.

## Setup

1. Read `SPEC.md` fully and follow its current phase ownership references.
2. Read `CLAUDE.md`, `ARCHITECTURE.md`, and all `docs/specs/*.md` files.
3. Check existing beads: `bd list --limit 0` and `bd stats`.
4. Survey what's actually implemented with narsil-mcp:
   ```
   mcp__narsil-mcp__get_project_structure()
   mcp__narsil-mcp__find_symbols("JsTool")
   mcp__narsil-mcp__find_symbols("JsWorkerSupervisor")
   mcp__narsil-mcp__find_symbols("SkillStore")
   mcp__narsil-mcp__find_symbols("ProductionWorkerLauncher")
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
mcp__narsil-mcp__find_symbols("RunStep")           # src/extras/js/protocol.rs
mcp__narsil-mcp__find_symbols("StepResult")        # src/extras/js/protocol.rs
mcp__narsil-mcp__find_symbols("JsWorkerSupervisor") # src/extras/js/supervisor.rs
mcp__narsil-mcp__find_symbols("ParentHostEffectService") # src/extras/js/host.rs
mcp__narsil-mcp__find_symbols("EffectOperation")   # src/extras/js/protocol.rs
```

A missing symbol alone is not a finding: trace the current implementation for an equivalent
entry point, then file only a missing behavior required by the owning current spec.

### 2. Phase 1 — Integration tests

SPEC.md requires tests in `src/extras/js/tests/`:
- Unit test: `JsTool` is `Send + Sync`
- Unit test: fresh worker runtime and isolated state between requests
- Unit test: `set_memory_limit` → allocation beyond limit returns error, not OOM
- Integration test: host `read_file` works with allowed path
- Integration test: host `spawn` is sandboxed

Are any of these tests missing?

### 3. Phase 2 — Sandbox hardening

```
mcp__narsil-mcp__find_symbols("ProductionWorkerLauncher")
mcp__narsil-mcp__find_symbols("Sandbox")
mcp__narsil-mcp__find_symbols("Seatbelt")
```

Check the delivered platform contracts rather than proposing superseded dependencies:
- Linux general-command and broker-only worker isolation;
- macOS Seatbelt, one-time worker publication, and guardian lifecycle;
- Windows general-command containment and worker AppContainer/Job attestation;
- feature gating and fail-closed behavior when a required backend is unavailable.

Use the canonical Phase 6 checklist for worker requirements and the Phase 2 spec for general commands.

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
