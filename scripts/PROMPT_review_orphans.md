# Review: Orphans & Dead Code — mini-agent

You are auditing the mini-agent workspace for dead code, unused features, and orphaned beads.

## Setup

1. Read `CLAUDE.md` and `AGENTS.md`.
2. Check existing beads: `bd list --limit 0 --status open && bd search "ORPHAN:"`.
3. Run the automated dead-code analysis with narsil-mcp:
   ```
   mcp__narsil-mcp__find_dead_code()               # unreachable functions/structs
   mcp__narsil-mcp__find_unused_exports()           # pub items with no external callers
   mcp__narsil-mcp__find_semantic_clones()          # duplicated logic worth consolidating
   mcp__narsil-mcp__get_project_structure()         # full layout to spot orphaned files
   mcp__narsil-mcp__find_circular_imports()         # import cycles
   ```

## Bead filing protocol

```bash
bd create --title="ORPHAN: <short summary>" --type=task --priority=<2-4> \
  --description="Location: <file:line from narsil-mcp>
Description: <what's orphaned or dead>
Evidence: <narsil-mcp output showing no callers / unused status>
Impact: <confusion, maintenance burden, binary size>
Fix: <remove, consolidate, or document as intentional>
Verification: <cargo test still passes after removal>"
```

## Orphan vectors to investigate

### 1. Unused feature flags

```
mcp__narsil-mcp__find_symbols("cfg(feature")       # find all feature-gated code
mcp__narsil-mcp__find_unused_exports()
```

Check `Cargo.toml` features:
- Are all declared features (`loop`, `git-worktree`, `mcp`, `subagents`, `archmd`, etc.) actually used in `src/`?
- Is there `#[cfg(feature = "X")]` code with no corresponding `Cargo.toml` feature entry?
- Are there features in `[features]` with no code gated on them?

### 2. Dead source code

```
mcp__narsil-mcp__find_dead_code()
mcp__narsil-mcp__find_symbol_usages("pub fn")      # pub functions with zero external callers
```

- Functions declared `pub` with no external callers (should be `pub(crate)` or removed).
- Modules declared in `mod.rs` but never imported.
- Trait implementations that no code path invokes.

### 3. The spike crate

```
mcp__narsil-mcp__get_file("spike/src/main.rs")
mcp__narsil-mcp__find_symbols("spike")
```

`spike/` is a research artifact. Check:
- Does any production code in `src/` depend on `spike/`?
- Are there concepts in `spike/src/main.rs` that have been absorbed into `src/extras/js/` (if it exists)? File a task to remove the absorbed code from spike.

### 4. Orphaned bead dependencies

```bash
bd orphans          # beads with broken dep chains
bd stale            # beads with no recent activity
```

For each orphaned or stale bead:
- Is it still relevant?
- If superseded, close it with `bd close <id> --reason "Superseded by mini-agent-xx"`.
- If blocked, update its status to `blocked` with the blocking bead ID noted.

### 5. Duplicate logic

```
mcp__narsil-mcp__find_semantic_clones()
mcp__narsil-mcp__find_similar_code("fn run_step")
```

- Is there similar step-execution logic in both `spike/` and `src/`?
- Are there multiple implementations of timeout or interrupt handling?

## Deduplication protocol

Before filing: `bd search "<keyword>"`. Comment on existing beads for duplicates.

## After completing

```bash
bd dolt push
```

Report: count of dead functions, unused exports, duplicate code blocks, and orphaned beads found.
