# Review: Compound (Cross-Cutting Concerns) — mini-agent

You are conducting a cross-cutting review that examines concerns spanning multiple
subsystems: error propagation, observability, configuration, and cross-feature interactions.

This is a Tier 2 domain — run after Tier 1 reviews (bugs, security, perf, orphans, missing,
quality) so you can reference their findings and identify compound problems.

## Setup

1. Read `CLAUDE.md`, `ARCHITECTURE.md`, and `SPEC.md`.
2. Read the open Tier 1 beads for context: `bd list --status open --limit 0`.
3. Survey with narsil-mcp:
   ```
   mcp__narsil-mcp__get_code_graph()               # full cross-module call graph
   mcp__narsil-mcp__get_data_flow("JsOutcome")     # trace error through the stack
   mcp__narsil-mcp__get_data_flow("JsRequest")     # trace request lifecycle
   mcp__narsil-mcp__find_callers("tracing")        # observability coverage
   mcp__narsil-mcp__get_control_flow("run_step")   # control flow through step execution
   ```

## Bead filing protocol

```bash
bd create --title="COMPOUND: <short summary>" --type=task --priority=<1-3> \
  --description="Subsystems: <comma-separated list of affected modules>
Description: <cross-cutting concern>
Evidence: <narsil-mcp output showing the cross-subsystem interaction>
Impact: <what breaks or degrades across subsystem boundaries>
Fix: <architectural change or cross-cutting refactor>
Verification: <how to test across the boundary>"
```

## Cross-cutting vectors

### 1. Error propagation consistency

```
mcp__narsil-mcp__get_data_flow("JsOutcome::Error")
mcp__narsil-mcp__find_references("anyhow")
mcp__narsil-mcp__find_references("thiserror")
```

- Does `JsOutcome::Error(String)` surface the full JS stack trace to the LLM?
- Is the error type consistent from JS thread → `JsTool::call` → agent loop → LLM prompt?
- Are there places where `anyhow::Error` is used in library code (should be `thiserror`)?
- Are errors swallowed silently at any cross-module boundary?

### 2. Timeout propagation

```
mcp__narsil-mcp__get_control_flow("set_interrupt_handler")
mcp__narsil-mcp__find_callers("tokio::time::timeout")
```

ARCHITECTURE.md: interrupt handler fires only during JS bytecode; blocking host calls
need `tokio::time::timeout`. Check:
- Is there a `tokio::time::timeout` wrapping the blocking `spawn()` call in the JS host?
- If JS times out via interrupt, does the `JsOutcome::Timeout` propagate cleanly?
- Is the same `STEP_TIMEOUT` constant used for both the interrupt deadline and the tokio timeout?

### 3. Feature gate interaction

```
mcp__narsil-mcp__find_symbols("cfg(feature")
mcp__narsil-mcp__get_import_graph()
```

- Can `--features js,sandbox` be combined without conflict?
- Can `--features js,skills` be combined (Phase 3 depends on Phase 1)?
- Are there undeclared feature dependencies (e.g. `skills` assumes `js` is active)?

### 4. Observability gaps

```
mcp__narsil-mcp__find_callers("tracing::info")
mcp__narsil-mcp__find_callers("tracing::error")
mcp__narsil-mcp__find_callers("tracing::warn")
```

- Is there a `tracing::info!` span around the JS step execution (start, duration, outcome)?
- Are JS errors logged with enough context (step number, code snippet, error message)?
- Is there a `tracing::warn!` for interrupt-triggered timeouts?
- Is there instrumentation at the permission check boundary?

### 5. Configuration and constants

```
mcp__narsil-mcp__find_symbols("STEP_TIMEOUT")
mcp__narsil-mcp__find_symbols("MEMORY_LIMIT")
mcp__narsil-mcp__find_symbols("STACK_LIMIT")
mcp__narsil-mcp__find_symbols("THREAD_STACK")
```

SPEC.md defines exact constants. Check:
- Are the constants defined in `src/extras/js/types.rs` (or `engine.rs`)?
- Are they used consistently — no magic numbers elsewhere in the JS engine?
- Are they overridable via CLI or config without modifying source code?

### 6. Skill library integration with JS engine (Phase 3 cross-cut)

```
mcp__narsil-mcp__find_symbols("SkillStore")
mcp__narsil-mcp__find_call_path("run_step", "SkillStore")
```

If Phase 3 is being implemented alongside Phase 1:
- Is the skill preamble injection happening before `ctx.eval(...)`, not after?
- Does skill retrieval happen on the Tokio thread (async) or the JS thread (sync)?
- Is there a performance risk if skill retrieval blocks the JS thread?

## Deduplication protocol

Before filing: `bd search "COMPOUND:"`. Check if the cross-cutting concern was already
captured by a Tier 1 review as a simpler single-subsystem bead.

## After completing

```bash
bd dolt push
```

Report: top 3 cross-cutting risks, any hidden feature-interaction bugs, observability coverage score.
