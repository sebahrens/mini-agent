# Review: Bug Hunter — mini-agent

For JavaScript runtime review, use **Phase 6 security invariants (canonical)** in
`docs/specs/phase-6-brokered-js-runtime.md` as the authority. Check current callers and behavior;
retired Phase 1 symbols are not missing implementation requirements.

You are conducting a focused bug-hunting review of the mini-agent Rust workspace.

## Setup

1. Read `CLAUDE.md` and `ARCHITECTURE.md` fully.
2. Check existing beads to avoid duplicates:
   ```bash
   bd list --limit 0 --status open
   bd search "bug"
   bd search "BUG:"
   ```
3. Survey the codebase with narsil-mcp before reading individual files:
   ```
   mcp__narsil-mcp__get_project_structure()         # full module layout
   mcp__narsil-mcp__find_uninitialized()            # use-before-init candidates
   mcp__narsil-mcp__find_dead_code()                # dead code that may mask bugs
   mcp__narsil-mcp__get_function_hotspots()         # largest/most complex functions first
   mcp__narsil-mcp__check_type_errors("src/")       # type-system sanity check
   ```

## Bead filing protocol

File every finding immediately — do not batch:

```bash
bd create --title="BUG: <short summary>" --type=bug --priority=<0-3> \
  --description="Location: <file:line from narsil-mcp>
Description: <what was found>
Evidence: <code snippet>
Impact: <what breaks or degrades>
Fix: <concrete code suggestion>
Verification: <how to confirm the fix — e.g. specific test command>"
```

Priority: 0=critical, 1=high, 2=medium, 3=low.

## Bug vectors to investigate

### 1. JS engine invariants (if `src/extras/js/` exists)

Use narsil-mcp to find the JS engine files first:
```
mcp__narsil-mcp__find_symbols("JsTool")
mcp__narsil-mcp__find_symbols("JsWorkerSupervisor")
mcp__narsil-mcp__find_symbols("Runtime")
```

Then check:
- Is `Runtime` ever reused across steps? (see the canonical fresh-runtime contract)
- Is `set_memory_limit` called on every new `Runtime`? (before source evaluation)
- Is `set_interrupt_handler` called before `ctx.eval(...)`? (before source evaluation)
- Is the microtask queue drained after every eval? (`execute_pending_job` loop)
- Does `JsTool` hold any `!Send` field (Runtime, Context, Rc, RefCell)?
- Does `spawn()` from JS go through `Sandbox::wrap_command`? (through the parent broker)
- Can a worker crash escape containment or leave the parent waiting indefinitely?

### 2. Channel and async safety

```
mcp__narsil-mcp__find_symbols("mpsc")
mcp__narsil-mcp__find_symbols("oneshot")
mcp__narsil-mcp__get_data_flow("RunStep")
```

- Does dropping a caller cancel its invocation, release transport ownership, and reap the worker?
- Are there `tokio::sync::Mutex` held across `.await` points?
- Do parent-brokered effects respect the invocation deadline and cancellation?
- Does worker termination reach `JsTool` as a closed error and allow a later call to recover?

### 3. Permission flow

```
mcp__narsil-mcp__find_callers("check_perm")
mcp__narsil-mcp__find_symbols("PermCheck")
mcp__narsil-mcp__find_call_path("spawn", "Sandbox::wrap_command")
```

- Is every call to `BashTool` (and `JsTool` if present) guarded by `check_perm`?
- TOCTOU: is there a race between permission check and file operation?
- Can a tool call bypass the permission enforcer by going through a different code path?

### 4. Sandbox and process lifecycle

```
mcp__narsil-mcp__find_symbols("kill_process_group")
mcp__narsil-mcp__find_symbols("Sandbox")
mcp__narsil-mcp__get_control_flow("wrap_command")
```

- Does `kill_process_group` have an empty Windows arm (it should — `#[cfg(unix)]`)?
- Are child process handles properly dropped on timeout?
- Can a subprocess outlive the agent process if the agent is killed?

### 5. General Rust bug patterns

```
mcp__narsil-mcp__find_semantic_clones()            # find duplicated buggy patterns
mcp__narsil-mcp__find_uninitialized()              # stale-state propagation
mcp__narsil-mcp__get_reaching_definitions()        # data flow to sensitive operations
```

Search the codebase for:
- `.unwrap()` and `.expect()` outside of `#[test]` blocks — especially in tool dispatch paths
- `Mutex` held across `.await`
- Integer overflow in iteration counters or token counts
- Missing error context (bare `?` losing the chain)
- `panic!()` or `unreachable!()` in non-test paths reachable from tool execution

## Deduplication protocol

Before filing, run: `bd search "<filename>"`. If a bead already covers the same file:line,
add a comment to the existing bead instead of creating a duplicate.

## After completing

Do NOT edit CLAUDE.md or ARCHITECTURE.md directly. File DOCFIX beads:

```bash
bd create --title="DOCFIX: <what to update>" --type=task --priority=3 \
  --description="File: CLAUDE.md|ARCHITECTURE.md
Section: <which section>
Change: <what to add/update/remove>"
```

```bash
bd dolt push
```

Report: total findings by severity, top 3 most critical, any invariant violations found.
