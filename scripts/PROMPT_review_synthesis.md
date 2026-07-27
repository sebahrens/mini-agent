# Review: Synthesis — mini-agent

You are the final Tier 3 reviewer synthesizing all prior review findings into a coherent
picture: what's the overall health, what's the critical path, and what should happen next.

This domain runs last. It consumes outputs from all prior domains (bugs, security, perf,
orphans, missing, quality, arch, deps, compound, debate) and produces a prioritized action plan.

## Setup

1. Read `CLAUDE.md`, `ARCHITECTURE.md`, `SPEC.md` briefly for orientation.
2. Read ALL open beads — this is synthesis, so you need the full picture:
   ```bash
   bd list --limit 0 --status open
   bd stats
   ```
3. Survey the codebase state with narsil-mcp:
   ```
   mcp__narsil-mcp__get_project_structure()
   mcp__narsil-mcp__get_index_status()             # is the index current?
   mcp__narsil-mcp__get_metrics()                   # code metrics snapshot
   mcp__narsil-mcp__get_security_summary()         # security posture summary
   mcp__narsil-mcp__get_incremental_status()       # what changed recently
   ```

## Bead filing protocol

Synthesis files very few beads — only for gaps or contradictions the prior domains missed:

```bash
bd create --title="SYNTH: <cross-domain finding>" --type=task --priority=<0-2> \
  --description="Domains: <which prior review domains are involved>
Finding: <the cross-domain gap or contradiction>
Evidence: <bead IDs from prior domains + narsil-mcp confirmation>
Impact: <why this matters at the project level>
Fix: <what needs to happen>
Verification: <how to confirm resolution>"
```

## Synthesis tasks

### 1. Triage and prioritize all open beads

Group the open beads by urgency:

**Blockers** (must fix before Phase 1 can be considered complete):
- Any P0 beads from bugs or security domains
- Any `!Send` invariant violation (ARCH domain)
- Any missing spec files (MISSING domain)

**Phase 1 critical path**:
- Beads that block the first working `cargo test --features js` run

**Phase 2+ backlog**:
- Beads for sandbox, skill library, auto-admission that don't block Phase 1

Output a structured triage table in the beads comment or as a bd comment:
```bash
bd comments add <a-synthesis-bead-id> "Triage: P0=[...] P1=[...] P2=[...]"
```

### 2. Check for contradictions between review domains

- Do any two beads propose conflicting fixes to the same file:line?
- Does any ARCH bead conflict with an existing DEBATE bead's conclusion?
- Are there MISSING beads for things that QUALITY beads say are poorly implemented
  (i.e. partially implemented, not completely absent)?

Use `bd search` and `bd show` to cross-reference.

### 3. Validate the critical-path sequence

Based on SPEC.md's four phases, verify the bead dependency chain is correct:

```bash
bd list --status open --limit 0    # all open beads
```

- Can Phase 1 beads be executed in parallel, or are some sequentially blocked?
- Are Phase 2 beads correctly blocked on Phase 1 beads via `bd dep add`?
- Is there any Phase 3/4 bead that accidentally lacks a blocker on Phase 1?

### 4. Assess spec file completeness

```bash
ls -la docs/specs/
wc -l docs/specs/*.md
```

- Are all four phase spec files present and non-trivial?
- Do the spec files' acceptance criteria match the `:READY:` bead criteria in the decompose domain?
- File a SYNTH bead if the spec files are missing or stale.

### 5. Overall health assessment

Using narsil-mcp metrics and the bead landscape:
- What percentage of Phase 1 deliverables have a `:READY:` bead?
- What percentage of beads are P0 or P1?
- Is the project in a state where `./scripts/loop.sh build` can make meaningful progress?

### 6. Recommended next actions

File a single synthesis summary bead (or write it as a comment on an existing epic):

```bash
bd create --title="SYNTH: Review cycle complete — next actions" --type=task --priority=1 \
  --description="Review cycle summary.

Phase 1 readiness: X% of deliverables have :READY: beads
Blockers: [list bead IDs]
Critical path: [ordered sequence of bead IDs]
Recommended next command: ./scripts/loop.sh build|./scripts/loop.sh decompose

Top 3 actions:
1. <action + bead ID>
2. <action + bead ID>
3. <action + bead ID>"
```

## After completing

```bash
bd dolt push
```

Final report: overall health (1-10), number of beads by tier and priority, recommended next command,
and whether the project is ready for `./scripts/loop.sh build` to start implementing.
