# Phase 1 — Core JS Engine Integration

- **Document role**: normative phase specification
- **Specification version**: 1.0.0
- **Delivery status**: delivered
- **Owner**: mini-agent maintainers
- **Last reconciled**: 2026-07-31
- **Entry dependency**: none for the non-persistent engine
- **Exit dependency**: every acceptance criterion below and every Phase 1 blocker

The corpus authority and conflict rules are defined in
[`00-index.md`](00-index.md). Overview documents and dated blueprints cannot override this file.

## Overview

Phase 1 adds a bounded in-process QuickJS action primitive. It delivers portable JavaScript
evaluation, file globals, process spawning through the existing `Sandbox` abstraction, and the
permission bridge needed by synchronous host functions.

“Sandbox” has two distinct meanings:

- The QuickJS VM is isolated from Node.js and ambient Rust APIs and is bounded by memory, stack,
  wall-clock, and pending-job limits.
- A child process is isolated only when `Sandbox::wrap_command` reports an effective backend.
  Phase 1 does not claim process isolation on macOS or Windows, and it must surface a configured
  backend that is unavailable rather than describing the child as sandboxed.

Phase 1 supplements the existing action tools. It does not remove Bash, make hooks portable, or
establish an unqualified Windows release claim.

## Feature gate

`js = ["dep:rquickjs"]` enables the engine and remains non-default until this phase exits.
All JS-specific production code is gated by `#[cfg(feature = "js")]`; the default build remains
unchanged.

The `sandbox` feature belongs to Phase 2 and is independent. A `js`-only build still routes
`spawn()` through the existing `Sandbox::wrap_command`; whether that wrapper provides effective
OS isolation is a runtime/platform fact, not a Cargo-feature inference.

## File placement

Production files live at the repository root:

| Concern | Location |
|---------|----------|
| Runtime lifecycle and JS thread | `src/extras/js/engine.rs` |
| `JsTool` implementation | `src/extras/js/tool.rs` |
| Host globals and secure file operations | `src/extras/js/host.rs` |
| Request/response and permission-bridge types | `src/extras/js/types.rs` |
| Module registration | `src/extras/js/mod.rs`, `src/extras/mod.rs` |
| Agent tool registration | `src/agent/builder.rs` |
| JS integration tests | `src/extras/js/tests/` |

Paths under `zerostack/` are historical and must not be used by new tracker tasks.

## Exact types

The exact Rust representation may evolve without changing this contract, but the type boundary is
fixed:

- requests own source text, cancellation, a one-shot reply, and any frozen turn bundle;
- responses contain one bounded `JsOutcome`;
- permission requests carry a JS-facing operation, exact key/path, deadline, cancellation, and a
  reply channel;
- process results contain bounded stdout/stderr, exit status, and truncation/timeout metadata; and
- all channel payloads are `Send`; no payload contains a QuickJS value or context.

The Phase 1 limits are 30 seconds per step/host call, 64 MiB heap, 512 KiB JS stack, 8 MiB OS thread
stack, and 1 MiB per file read/write. One shared constants module owns these values.

## Threading model

Each `JsTool` instance owns exactly one dedicated OS thread. That thread owns every QuickJS
`Runtime`, `Context`, and value derived from them. QuickJS types, `Rc`, and `RefCell` never cross
the channel and are never fields of `JsTool`.

```rust
std::thread::Builder::new()
    .name("js-engine".into())
    .stack_size(8 * 1024 * 1024)
```

## `JsTool`

Every `JsTool` field must be `Send + Sync`. The tool may own channel endpoints, permission-bridge
state, a Tokio handle, cancellation state, and the JS thread join handle.

The tool shuts down its permission bridge, closes the request channel, and joins a finished thread
without leaving new work able to enter a shutting-down instance.

### Import paths

JS code imports permission types directly from `crate::permission`, tool errors from
`crate::agent::tools`, and engine/host/types through `crate::extras::js`. It must not depend on a
private sibling-module import that happens to be visible to `BashTool`.

### `JsTool` — Full implementation

The implementation is authoritative only as code under test; this specification defines its
required boundary and behavior. Do not copy a second complete `JsTool` implementation into a
tracker issue or overview document.

## Runtime lifecycle

Every JS step creates a new `Runtime` and drops it after evaluation. Runtime reuse is forbidden,
including after a successful step, because an OOM can poison allocator state.

Every new runtime applies these limits before evaluation:

```rust
rt.set_memory_limit(64 * 1024 * 1024);
rt.set_max_stack_size(512 * 1024);
rt.set_interrupt_handler(/* deadline/cancellation handler */);
```

The interrupt deadline is installed before `ctx.eval(...)`. Evaluation uses
`eval::<Value, _>`, never `eval::<(), _>`.

After every evaluation attempt, the engine drains pending jobs. The baseline rule is:

```rust
while rt.execute_pending_job() == Ok(true) {}
```

An implementation may add a deadline and finite job-count guard, but it may not skip the drain or
allow a self-replenishing microtask chain to monopolize the JS thread. Promise rejection, job
errors, timeout, cancellation, and OOM are returned as bounded outcomes.

## Host globals

Phase 1 exposes only:

| Global | Contract | Authorization/effect boundary |
|--------|----------|-------------------------------|
| `read_file(path)` | UTF-8 regular-file read, at most 1 MiB | mandatory `js/read_file` permission for the canonical target |
| `write_file(path, content)` | atomic no-follow create/replace, at most 1 MiB | mandatory `js/write_file` permission for the resolved final target |
| `spawn(cmd, args)` | bounded child result | mandatory process permission and `Sandbox::wrap_command` |
| `console.log(...)` | bounded diagnostic output | no external effect permission |

There is no `require()`, `import()`, `fetch()`, or `final_answer()` global.

### `read_file` and `write_file`

Permission is mandatory on every file operation. A configured Phase 2 allow-list may narrow an
operation but can never authorize it or bypass the permission service.

`read_file`:

1. resolves and canonicalizes the target without reading content;
2. captures target identity and derives an exact UTF-8 permission path;
3. obtains `js/read_file` permission with a finite deadline;
4. opens without following a final symlink, revalidates identity and regular-file type; and
5. reads no more than 1 MiB.

`write_file`:

1. rejects oversized content before mutation;
2. rejects an existing final symlink;
3. resolves the nearest existing parent and permits only one missing normal component;
4. obtains `js/write_file` permission for the resolved final UTF-8 target; and
5. revalidates the approved parent before descriptor-relative, no-follow atomic publication.

Denial, non-interactive `Ask`, approval-channel failure, timeout, cancellation, path race, invalid
UTF-8, or I/O failure returns a typed JS error and performs no content read or mutation.

### `spawn()` permission routing and sandbox boundary

The synchronous host closure routes authorization through the same permission policy used by the
Bash process primitive. It must preserve the JS-facing operation identity in errors and audit
events. Approval alone is insufficient: the command must then be created through
`Sandbox::wrap_command`, with the executable and arguments passed without interpolation.

`Sandbox::wrap_command` is the single Phase 1 process path. Direct
`std::process::Command::new(user_cmd)` execution is forbidden. The wrapper may run without an
effective isolation backend when sandboxing is disabled or unavailable; that state must remain
observable. Phase 2 adds Linux/macOS process hardening and does not retroactively make Phase 1
Windows-isolated.

### Interrupt handler scope and host-call deadlines

The QuickJS interrupt handler runs only during JS bytecode. Every blocking host operation,
permission wait, child wait, and shutdown wait therefore has its own finite timeout and
cancellation path in Phase 1. Host calls do not inherit safety merely from the eval deadline.
Timeout/cancellation kills the child process group where the platform supports it and returns a
bounded JS error.

## Error surfacing

Exceptions are returned to the model for self-correction and include bounded message and stack
text, including line information when QuickJS supplies it. Syntax errors, host errors, Promise
rejections, timeout, cancellation, OOM, and pending-job exhaustion are distinguished. Error
formatting never panics because an exception lacks a message or stack.

## Builder registration

`src/agent/builder.rs` registers one `JsTool` under `#[cfg(feature = "js")]` before the tool
allow-list is applied. `JsTool::new` owns creation of its dedicated thread so one tool cannot
accidentally share a QuickJS runtime with another.

Registration does not remove or silently replace another action tool. Windows availability is
controlled by verified platform support in code and CI, not by an overview-document claim.

## Module entry

`src/extras/js/mod.rs` declares and exposes the Phase 1 engine, host, tool, types, and test modules.
Later-phase modules remain behind their own feature gates.

## Module declaration

`src/extras/mod.rs` declares `pub mod js` only under `#[cfg(feature = "js")]`.

## Tests

Tests cover, at minimum:

- return values, void, syntax errors, and stack-bearing runtime errors;
- fresh runtime state after success, timeout, and OOM;
- exact memory/stack/deadline setup and bounded pending jobs;
- mandatory allow/deny/ask behavior for both file globals and process spawn;
- canonical permission paths, symlink races, non-regular files, UTF-8, and 1 MiB boundaries;
- atomic/no-follow write behavior and “no effect on denial/failure”;
- process argument safety, wrapper use, output bounds, timeout, cancellation, and shutdown;
- one dedicated 8 MiB thread per `JsTool` and clean tool drop; and
- default and `js` feature builds, plus every platform configuration for which support is claimed.

## Acceptance criteria

- [x] All threading, runtime, limit, eval, microtask, and exception invariants above are tested.
- [x] `JsTool` and all of its fields satisfy `Send + Sync`; QuickJS state stays on its thread.
- [x] Every file operation is permission-gated on the resolved target and fails without effects.
- [x] Every process spawn is permission-gated, argument-safe, bounded, and created by
      `Sandbox::wrap_command`.
- [x] Documentation and runtime diagnostics distinguish VM isolation from effective child-process
      isolation.
- [x] No Phase 2 or later host global is registered in a `js`-only Phase 1 build.
- [x] The default build remains unchanged and the `js` feature test suite passes.

## Out of scope for Phase 1

- `fetch()` and file allow-lists (Phase 2)
- Linux/macOS process-isolation hardening (Phase 2)
- Windows process isolation (not delivered by Phase 2)
- portable Agent Skills and learned JS skills (Phase 3)
- agent proposals and human-gated canaries (Phase 4)
- evidence-based promotion and lifecycle automation (Phase 5)
- hook-language migration or removal of Bash
