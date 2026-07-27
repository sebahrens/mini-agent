# Review: Architecture — mini-agent

You are evaluating the current mini-agent architecture against the target architecture
in ARCHITECTURE.md and the invariants in AGENTS.md.

## Setup

1. Read `CLAUDE.md`, `ARCHITECTURE.md`, and `AGENTS.md` fully.
2. Read `SPEC.md` §Threading model and §Host API.
3. Check existing beads: `bd list --limit 0 --status open && bd search "ARCH:"`.
4. Build the real code graph with narsil-mcp:
   ```
   mcp__narsil-mcp__get_import_graph()             # actual module dependency edges
   mcp__narsil-mcp__get_code_graph()               # symbol-level call graph
   mcp__narsil-mcp__find_circular_imports()        # any hidden cycles
   mcp__narsil-mcp__get_project_structure()        # filesystem + module layout
   mcp__narsil-mcp__get_export_map()               # measure public surface area
   mcp__narsil-mcp__find_callers("Tool")           # confirm Tool trait is the actual dispatch point
   ```

## Bead filing protocol

```bash
bd create --title="ARCH: <short summary>" --type=task --priority=<1-3> \
  --description="Location: <crate or module from narsil-mcp>
Description: <architectural concern>
Evidence: <dependency graph, call graph — from narsil-mcp output>
Spec reference: <ARCHITECTURE.md §section or docs/specs/phase-X.md §section>
Impact: <scalability, correctness, safety>
Fix: <concrete refactoring>
Verification: <how to validate the improvement>"
```

## Architecture vectors to investigate

### 1. Threading model correctness

The core invariant: QuickJS `Runtime`/`Context` are `!Send` and must live exclusively
on the dedicated JS thread. `JsTool` must hold only `Send + Sync` types.

```
mcp__narsil-mcp__find_symbols("JsTool")
mcp__narsil-mcp__get_symbol_definition("JsTool")   # check struct fields
mcp__narsil-mcp__find_references("Runtime")         # Runtime must only appear in js_thread_main
mcp__narsil-mcp__find_references("Context")         # same constraint
```

- Does `JsTool`'s struct definition contain any `!Send` field?
- Is `Runtime` ever constructed outside the dedicated JS thread?
- Is there any `unsafe` usage that overrides the `!Send` constraint?

### 2. Tool registration architecture

The architecture mandates `JsTool` is registered in `src/agent/builder.rs` under
`#[cfg(feature = "js")]` alongside BashTool.

```
mcp__narsil-mcp__find_symbols("AgentBuilder")
mcp__narsil-mcp__find_callers("register_tool")     # or equivalent builder method
mcp__narsil-mcp__get_call_graph("builder.rs")
mcp__narsil-mcp__find_symbols("BashTool")           # reference registration pattern
```

- Is `JsTool` registered the same way as `BashTool`?
- Is the feature gate `#[cfg(feature = "js")]` at the right level (builder, not deeper)?
- Is there a single tool registry or scattered dispatch?

### 3. Permission flow architecture

ARCHITECTURE.md: every tool call must route through `check_perm` before execution.

```
mcp__narsil-mcp__find_callers("check_perm")
mcp__narsil-mcp__find_call_path("JsTool::call", "check_perm")
mcp__narsil-mcp__find_call_path("BashTool::call", "check_perm")
```

- Is the permission check at a consistent abstraction level across all tools?
- Is there any tool that bypasses the check?
- Are `AskSender` and `PermCheck` types used consistently?

### 4. Feature gate architecture

The JS feature is gated on `features = ["js"]`. Verify gating is hermetic:

```
mcp__narsil-mcp__find_symbols("cfg(feature = \"js\")")
mcp__narsil-mcp__get_import_graph()
```

- Does `cargo build` (without `--features js`) compile cleanly without any JS symbols leaking?
- Is `rquickjs` an optional dependency? (`optional = true` in Cargo.toml)
- Does `src/extras/mod.rs` properly gate the `js` module?

### 5. Module boundary violations

Compare the actual import graph against the intended layering:

```
mcp__narsil-mcp__get_dependencies("src/extras/js")
mcp__narsil-mcp__find_circular_imports()
```

- Does `src/extras/js/` import from `src/agent/tools/` (wrong direction — should be reverse)?
- Does `src/agent/builder.rs` depend on `src/extras/js/` only through the feature gate?
- Any circular imports?

### 6. spike/ isolation

The `spike/` crate is a research artifact. No production code should depend on it.

```
mcp__narsil-mcp__get_dependencies("spike")
mcp__narsil-mcp__find_callers("spike")
```

- Does any module in `src/` import from `spike/`?
- Is `spike` listed as a workspace dependency anywhere it shouldn't be?

## Deduplication protocol

Before filing: `bd search "<keyword>"`. Comment on existing beads for duplicates.

## After completing

```bash
bd dolt push
```

Report: architectural health score (1-10), top 3 structural concerns, any `!Send` violations found.
