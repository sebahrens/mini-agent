# Review: Compound (Cross-Cutting Concerns) — mini-agent

For JavaScript runtime review, use **Phase 6 security invariants (canonical)** in
`docs/specs/phase-6-brokered-js-runtime.md` as the authority. Check current callers and behavior;
retired Phase 1 symbols are not missing implementation requirements.

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
   mcp__narsil-mcp__get_data_flow("StepResult")     # trace error through the stack
   mcp__narsil-mcp__get_data_flow("RunStep")     # trace request lifecycle
   mcp__narsil-mcp__find_callers("tracing")        # observability coverage
   mcp__narsil-mcp__get_control_flow("execute_inner_request")   # control flow through step execution
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
mcp__narsil-mcp__get_data_flow("StepOutcome::Error")
mcp__narsil-mcp__find_references("anyhow")
mcp__narsil-mcp__find_references("thiserror")
```

- Do `StepResult` and `WorkerError` expose only closed codes and validated source-free metadata?
- Is the error type consistent from worker protocol → supervisor → `JsTool::call` → agent loop → LLM prompt?
- Are there places where `anyhow::Error` is used in library code (should be `thiserror`)?
- Are errors swallowed silently at any cross-module boundary?

### 2. Timeout propagation

```
mcp__narsil-mcp__get_control_flow("set_interrupt_handler")
mcp__narsil-mcp__find_callers("tokio::time::timeout")
```

The parent deadline includes launch, queueing, IPC, permission waits, and brokered effects.
Worker interrupts enforce the request-local JS budget; operation timeouts may only shorten it.

- Can any effect reset or extend the invocation deadline?
- Does a worker interrupt retain resource-limit classification and trigger process recycling?
- Are parent watchdog expiry and worker-reported source faults distinguished?

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
- Do logs stay source-free while retaining closed error classes and validated stage/role metadata?
- Is there a `tracing::warn!` for interrupt-triggered timeouts?
- Is there instrumentation at the permission check boundary?

### 5. Configuration and constants

```
mcp__narsil-mcp__find_symbols("STEP_TIMEOUT")
mcp__narsil-mcp__find_symbols("MEMORY_LIMIT")
mcp__narsil-mcp__find_symbols("STACK_LIMIT")
```

SPEC.md defines exact constants. Check:
- Are the constants defined in `src/extras/js/types.rs`, the wire protocol, and platform containment modules?
- Are they used consistently — no magic numbers elsewhere in the JS engine?
- Can configuration accidentally widen a fixed runtime or containment limit?

### 6. Skill library integration with JS engine (Phase 3 cross-cut)

```
mcp__narsil-mcp__find_symbols("SkillStore")
mcp__narsil-mcp__find_call_path("SkillRuntime", "SkillStore")
```

For the delivered skill pipeline:
- Is the frozen skill bundle available before model execution and installed before the model script?
- Do retrieval and store access stay in the parent, outside the contained worker?
- Can index hydration or retrieval leave the first prompt with an incorrectly empty skill bundle?

## Deduplication protocol

Before filing: `bd search "COMPOUND:"`. Check if the cross-cutting concern was already
captured by a Tier 1 review as a simpler single-subsystem bead.

## After completing

```bash
bd dolt push
```

Report: top 3 cross-cutting risks, any hidden feature-interaction bugs, observability coverage score.
