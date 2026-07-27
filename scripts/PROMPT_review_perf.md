# Review: Performance — mini-agent

You are conducting a focused performance review of the mini-agent Rust workspace.

## Setup

1. Read `CLAUDE.md` and `ARCHITECTURE.md` (esp. the Runtime lifecycle section).
2. Check existing beads: `bd list --limit 0 --status open && bd search "PERF:"`.
3. Survey hotspots with narsil-mcp:
   ```
   mcp__narsil-mcp__get_function_hotspots()        # largest/most complex functions
   mcp__narsil-mcp__get_complexity()               # cyclomatic complexity by module
   mcp__narsil-mcp__get_hotspots()                 # combined churn + complexity signal
   mcp__narsil-mcp__get_project_structure()
   ```

## Bead filing protocol

```bash
bd create --title="PERF: <short summary>" --type=task --priority=<1-3> \
  --description="Location: <file:line from narsil-mcp>
Description: <performance concern>
Evidence: <code showing the issue — allocations, blocking, lock contention, etc.>
Impact: <latency, throughput, memory — quantify if possible>
Fix: <concrete optimization or architectural change>
Verification: <benchmark or test to confirm improvement>"
```

## Performance vectors to investigate

### 1. JS Runtime creation cost

The architecture mandates a fresh `Runtime` per step (~500μs overhead, accepted).
Verify the accepted cost has not crept beyond the design budget:

```
mcp__narsil-mcp__find_symbols("Runtime::new")
mcp__narsil-mcp__find_callers("js_run_step")
mcp__narsil-mcp__get_call_graph("js_run_step")
```

- Is `Runtime::new()` called exactly once per step (not more)?
- Is there any unnecessary work done between Runtime creation and the eval call?
- Are host functions registered efficiently or re-registered on every step?

### 2. Channel overhead

```
mcp__narsil-mcp__find_symbols("mpsc")
mcp__narsil-mcp__find_symbols("oneshot")
mcp__narsil-mcp__find_callers("JsTool::call")
```

- Is the `JsRequest` sent with unnecessary clones of large strings?
- Is the `mpsc::Sender` shared efficiently (Arc or per-call clone)?
- Can multiple JS requests queue up without backpressure, causing memory growth?

### 3. Async task scheduling

```
mcp__narsil-mcp__find_symbols("tokio::spawn")
mcp__narsil-mcp__find_symbols("spawn_blocking")
mcp__narsil-mcp__find_symbols("block_in_place")
```

- Are there blocking operations on the async runtime thread (file I/O, process wait)?
- Is `spawn_blocking` used where needed?
- Are there any unbounded async task spawns?

### 4. Memory allocation patterns

```
mcp__narsil-mcp__find_symbols("Vec::new")
mcp__narsil-mcp__find_symbols("String::from")
mcp__narsil-mcp__find_dead_code()
```

- Are large buffers preallocated where the size is known?
- Is `compact_str` used where appropriate (already a dependency)?
- Are `smallvec` opportunities missed for small collections?
- Any `collect::<Vec<_>>()` where an iterator would suffice?

### 5. Skill library retrieval (Phase 3, if implemented)

```
mcp__narsil-mcp__find_symbols("embedding")
mcp__narsil-mcp__find_symbols("cosine_similarity")
```

- Is embedding similarity computed linearly over all skills (O(n) per step)?
- Is there an index structure for faster nearest-neighbor search?
- Is the fastembed model loaded once at startup or per query?

## Deduplication protocol

Before filing: `bd search "<keyword>"`. Add comments to existing beads for duplicates.

## After completing

```bash
bd dolt push
```

Report: top 3 hotspots by impact, any Runtime creation overhead surprises, memory allocation concerns.
