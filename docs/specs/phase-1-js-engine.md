# Phase 1 — Core JS Engine Integration

**Status**: Pre-implementation  
**Prerequisite**: None  
**Delivers**: `JsTool` registered in the agent, executing JavaScript in a sandboxed QuickJS runtime with host globals for file I/O and process spawning.

---

## Overview

Phase 1 embeds a QuickJS JavaScript engine (via `rquickjs 0.12`) as a cross-platform action primitive. The agent writes JavaScript; the engine executes it natively with hard resource limits (64 MiB heap, 512 KiB JS stack, 30 s wall-clock timeout). This replaces the platform-specific bash/PowerShell split and lays the substrate for the Phase 3 skill library.

Without this phase: agent can only run bash, which is unavailable on Windows and sandboxed only on Linux.

---

## Feature gate

The feature and dependency are **already declared** in `Cargo.toml` (lines 37 and 80). No edits needed.

```toml
# Cargo.toml — already present, DO NOT duplicate
[features]
js = ["dep:rquickjs"]

[dependencies]
rquickjs = { version = "0.12", features = ["full"], optional = true }
```

Gate any new code behind `#[cfg(feature = "js")]`. The binary compiles and all existing tests pass without `--features js`.

---

## File placement

All new files go in `src/` at the repo root. The `zerostack/` directory no longer exists (monorepo flattened). The `src/extras/js/` directory does not exist yet and must be created.

| File | Status | Purpose |
|------|--------|---------|
| `src/extras/js/types.rs` | TO BE CREATED | Channel types, constants |
| `src/extras/js/engine.rs` | TO BE CREATED | Runtime lifecycle, JS thread main loop |
| `src/extras/js/tool.rs` | TO BE CREATED | `JsTool` — `rig::tool::Tool` impl |
| `src/extras/js/host.rs` | TO BE CREATED | Host global implementations |
| `src/extras/js/mod.rs` | TO BE CREATED | Module re-exports |
| `src/extras/mod.rs` | EXISTS (40 lines) | Append `#[cfg(feature = "js")] pub mod js;` after line 40 |
| `src/agent/builder.rs` | EXISTS | Add `#[cfg(feature = "js")]` block at lines 333–334 (see §Builder registration) |

---

## Exact types — `src/extras/js/types.rs`

Copy verbatim; do not alter the constants.

```rust
use std::time::Duration;

pub const STEP_TIMEOUT: Duration = Duration::from_secs(30);
pub const MEMORY_LIMIT: usize = 64 * 1024 * 1024;   // 64 MiB
pub const STACK_LIMIT: usize = 512 * 1024;            // 512 KiB JS stack
pub const THREAD_STACK: usize = 8 * 1024 * 1024;      // 8 MiB OS thread stack

#[derive(Debug)]
pub struct JsRequest {
    pub code: String,
    pub reply: tokio::sync::oneshot::Sender<JsResponse>,
}

#[derive(Debug)]
pub struct JsResponse {
    pub outcome: JsOutcome,
}

#[derive(Debug)]
pub enum JsOutcome {
    Value(String),
    Void,
    Error(String),
    Timeout,
    OomKilled,
}

/// Sent from the JS thread to tokio to request permission for a host call.
/// The JS thread blocks on `reply_rx.recv()` while tokio resolves the check.
#[derive(Debug)]
pub struct PermRequest {
    pub tool:  String,
    pub key:   String,
    pub reply: std::sync::mpsc::Sender<PermResponse>,
}

#[derive(Debug)]
pub enum PermResponse {
    Allowed,
    Denied(String),
}

/// Returned to JS by `spawn(cmd, args)`.
/// Visible in JS as `{ stdout: string, stderr: string, code: number }`.
#[derive(Debug, serde::Serialize)]
pub struct SpawnResult {
    pub stdout: String,
    pub stderr: String,
    pub code:   i32,
}
```

---

## Threading model

QuickJS `Context` and `Runtime` are `!Send`. The `rig::tool::Tool::call` method returns `impl Future + Send` on native targets. A `JsTool` that holds a `Context` will not compile.

**Solution**: dedicated OS thread with `std::sync::mpsc` channel.

```
[tokio runtime]                      [dedicated OS thread — "js-engine"]
JsTool::call()
  └─ tx.send(JsRequest) ──────────→  js_thread_main(rx)
  └─ await oneshot_rx  ←──────────   handler sends JsResponse via oneshot_tx
                                       ├─ for host calls needing async perm:
                                       │  sends back to tokio via ask_tx
                                       └─ returns JsResponse via oneshot_tx
```

`JsTool` holds only `Send + Sync` types — verified by the compiler:

```rust
pub struct JsTool {
    tx:         std::sync::mpsc::Sender<JsRequest>,  // Send + Sync ✓
    permission: Option<PermCheck>,                    // Send + Sync ✓
    ask_tx:     Option<AskSender>,                    // Send + Sync ✓
}
```

**Thread spawn** (in `src/agent/builder.rs` — see §Builder registration below):

```rust
std::thread::Builder::new()
    .name("js-engine".into())
    .stack_size(THREAD_STACK)   // 8 MiB — portable across Linux glibc/musl/Windows
    .spawn(move || js_thread_main(js_rx))
    .expect("failed to spawn JS thread");
```

`.cargo/config.toml` link flags are NOT honored by `cargo install`. `Builder::stack_size` is the only portable fix.

---

## Runtime lifecycle — `src/extras/js/engine.rs`

**Every JS step creates a fresh `Runtime` and drops it after eval.** OOM poisons the QuickJS allocator; cleanup code that runs post-OOM itself requires allocation and panics. This is not optional.

Exact implementation:

```rust
use rquickjs::{Context, Func, Runtime, Value};
use std::sync::mpsc;
use std::time::Instant;
use crate::extras::js::types::*;

pub fn js_thread_main(rx: mpsc::Receiver<JsRequest>) {
    while let Ok(req) = rx.recv() {
        let outcome = run_step(&req.code);
        let _ = req.reply.send(JsResponse { outcome });
    }
}

// pub(crate) required: Phase 3's verify_skill() calls this across modules
pub(crate) fn run_step(code: &str) -> JsOutcome {
    // Fresh Runtime EVERY step — OOM poisons allocator; never reuse
    let rt = match Runtime::new() {
        Ok(r) => r,
        Err(e) => return JsOutcome::Error(format!("Runtime::new failed: {e}")),
    };
    rt.set_memory_limit(MEMORY_LIMIT);
    rt.set_max_stack_size(STACK_LIMIT);

    let deadline = Instant::now() + STEP_TIMEOUT;
    rt.set_interrupt_handler(Some(Box::new(move || {
        Instant::now() >= deadline  // true = interrupt JS execution
    })));

    let ctx = match Context::full(&rt) {
        Ok(c) => c,
        Err(e) => return JsOutcome::Error(format!("Context::full failed: {e}")),
    };

    register_host_globals(&ctx);

    let result = ctx.with(|ctx| {
        match ctx.eval::<Value, _>(code) {
            Err(rquickjs::Error::Exception) => {
                let exc = ctx.catch();
                let exc = exc.as_exception().expect("exception type");
                let msg = exc.message().unwrap_or_default();
                let stack = exc.stack().unwrap_or_default();
                if msg.contains("interrupted") || Instant::now() >= deadline {
                    JsOutcome::Timeout
                } else {
                    JsOutcome::Error(format!("{msg}\n{stack}"))
                }
            }
            Err(e) => JsOutcome::Error(e.to_string()),
            Ok(v) => {
                if v.is_undefined() || v.is_null() {
                    JsOutcome::Void
                } else if let Some(s) = v.as_string() {
                    JsOutcome::Value(s.to_string().unwrap_or_default())
                } else {
                    JsOutcome::Value(format!("{v:?}"))
                }
            }
        }
    });

    // Drain microtask queue — required for Promise resolution
    while rt.execute_pending_job() == Ok(true) {}

    result
    // rt drops here — RAII; Context must be dropped before Runtime
}
```

**Critical**: always use `eval::<Value, _>`, never `eval::<(), _>`. The `()` form loses the return value and the exception stack trace needed for LLM self-correction.

---

## Host globals — `src/extras/js/host.rs`

Five globals exposed to JS. Phase 1 includes four; `fetch()` is Phase 2.

| Global | JS signature | Rust impl | Permission check |
|--------|-------------|-----------|-----------------|
| `read_file(path)` | `string → string` | stable bounded read (blocking on JS thread) | `"js/read_file"` + canonical target |
| `write_file(path, content)` | `(string, string) → void` | descriptor-relative atomic write | `"js/write_file"` + resolved final target |
| `spawn(cmd, args)` | `(string, string[]) → {stdout, stderr, code}` | `std::process::Command` (blocking) | `"js/spawn"` + cmd |
| `console.log(...)` | variadic | `eprintln!` / `tracing::info!` | none |

**No** `require()`, `import()`, `final_answer`, or `fetch()` in Phase 1.

### read_file and write_file

Before reading content, `read_file` canonicalizes the target, captures its identity, and
requests `js/read_file` permission for the exact canonical UTF-8 path. After approval it
rejects non-regular or oversized files, opens the canonical path without following a
final symlink, verifies that the opened file has the pre-approval identity, and reads at
most 1 MiB. Invalid UTF-8 and content that grows beyond the cap are typed JS errors.

`write_file` rejects content over 1 MiB before mutation. Existing final symlinks are
rejected. For a new file, the nearest existing parent is canonicalized and the missing
suffix must be exactly one normal filename component. Permission is requested for
`js/write_file` and the derived final UTF-8 path. Creation and replacement use the
descriptor-relative atomic helpers in `src/fs.rs`, with the approved parent identity
revalidated immediately before mutation and no-follow traversal throughout. Resolved
parent symlinks are allowed only because permission is bound to their resulting target.

Both globals use finite 30-second host-call and permission deadlines. Denial, failed
interactive approval, timeout, cancellation, or channel closure returns a JS error and
performs no content read or mutation. Permission is always required even when later
phases add filesystem allow-lists; those lists may only narrow the approved operation.

### spawn() — permission routing

`spawn()` permission checks must route back to tokio before execution. The JS thread blocks on a `std::sync::mpsc` sync channel while tokio resolves the permission. This is acceptable because the JS thread is dedicated and holds no other work.

```rust
pub struct SpawnContext {
    pub sandbox: crate::sandbox::Sandbox,
    pub perm_sync_tx: std::sync::mpsc::SyncSender<PermRequest>,
}

pub fn make_spawn(ctx: SpawnContext) -> impl Fn(String, Vec<String>) -> rquickjs::Result<SpawnResult> {
    move |cmd: String, args: Vec<String>| {
        // 1. Request permission from tokio via sync channel
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        ctx.perm_sync_tx.send(PermRequest {
            tool: "js/spawn".into(),
            key: cmd.clone(),
            reply: reply_tx,
        }).ok();
        match reply_rx.recv() {
            Ok(PermResponse::Denied(msg)) =>
                return Err(rquickjs::Error::new_from_js("spawn", &msg)),
            Ok(PermResponse::Allowed) => {}
            Err(_) =>
                return Err(rquickjs::Error::new_from_js("spawn", "permission channel closed")),
        }
        // 2. Execute via std::process::Command (blocking — JS thread acceptable)
        //    Apply Sandbox parameters manually or call ctx.sandbox.wrap_command(&cmd)
        //    and convert the tokio::process::Command to blocking via .into_std().
        //    src/sandbox.rs:109 Sandbox::wrap_command returns tokio::process::Command.
        // ...
    }
}
```

**Reference pattern**: the `BashTool` permission call at `src/agent/tools/bash.rs:137` shows the exact idiom to mirror (from inside a tokio async context):

```rust
// src/agent/tools/bash.rs:137 — reference implementation (async path)
if let Some(msg) = check_perm(&self.permission, &self.ask_tx, "bash", cmd).await? {
    coaching = Some(msg);
}
```

For `JsTool`'s async `call()` method (before sending to the JS thread), replace `"bash"` with the JS-specific tool key (e.g. `"js/spawn"`, `"js/read_file"`, `"js/write_file"`) and `cmd` with the relevant input string. The sync-channel `SpawnContext` approach is used when the permission check must be made from the non-async JS thread side.

### Interrupt handler scope

`set_interrupt_handler` fires only during **JS bytecode execution**, not during blocking Rust host calls. A `spawn()` call that hangs will not be interrupted by the JS timeout. Mitigation is a per-call timeout on the tokio side (Phase 2 concern; see `ARCHITECTURE.md §5`).

---

## Error surfacing

Errors are returned verbatim to the LLM for self-correction (not surfaced as `Err`):

```rust
JsOutcome::Error(e) => Ok(format!("JS error:\n{e}")),  // LLM self-corrects
JsOutcome::Timeout => Ok("JS error: execution timed out (30s limit exceeded)".into()),
JsOutcome::OomKilled => Ok("JS error: out of memory (64 MiB limit exceeded)".into()),
```

Exception format includes the stack trace:

```
ReferenceError: 'foo' is not defined
    at <eval> (eval_script):3:5
```

The LLM uses this to revise its JS on the next step.

---

## JsTool — `src/extras/js/tool.rs`

### Import paths

`AskSender` and `PermCheck` are **NOT** re-exported via `pub use` from `crate::agent::tools`. They appear in `src/agent/tools/mod.rs` as private `use` items (lines 84–85), accessible to child modules of `tools` (like `bash.rs`) but NOT from `crate::extras::js::tool`. Use the direct module paths:

| Type | Direct path | Declared at |
|------|-------------|-------------|
| `AskSender` | `crate::permission::ask::AskSender` | `src/permission/ask.rs:5` |
| `PermCheck` | `crate::permission::checker::PermCheck` | `src/permission/checker.rs:10` |
| `ToolError` | `crate::agent::tools::ToolError` | `src/agent/tools/mod.rs:88` |
| `check_perm` | `crate::agent::tools::check_perm` | `src/agent/tools/mod.rs:199` |

### Full implementation

```rust
use rig::tool::Tool;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::agent::tools::{ToolError, check_perm};
use crate::extras::js::types::*;

pub struct JsTool {
    tx:         std::sync::mpsc::Sender<JsRequest>,
    permission: Option<PermCheck>,
    ask_tx:     Option<AskSender>,
}

impl JsTool {
    pub fn new(
        tx: std::sync::mpsc::Sender<JsRequest>,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Self {
        Self { tx, permission, ask_tx }
    }
}

impl Tool for JsTool {
    const NAME: &'static str = "js";
    type Error = ToolError;
    type Args = JsArgs;
    type Output = String;

    fn description(&self) -> String {
        "Execute JavaScript code. Available globals: read_file(path), write_file(path, content), \
         spawn(cmd, args), console.log(...). Returns the last expression value as a string. \
         Errors include the stack trace for self-correction.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "JavaScript code to execute" }
            },
            "required": ["code"]
        })
    }

    async fn call(&self, args: JsArgs) -> Result<String, ToolError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tx.send(JsRequest { code: args.code, reply: reply_tx })
            .map_err(|_| ToolError::Msg("JS engine thread disconnected".into()))?;
        let response = reply_rx.await
            .map_err(|_| ToolError::Msg("JS engine reply channel closed".into()))?;
        match response.outcome {
            JsOutcome::Value(v) => Ok(v),
            JsOutcome::Void => Ok(String::new()),
            JsOutcome::Error(e) => Ok(format!("JS error:\n{e}")),
            JsOutcome::Timeout => Ok("JS error: execution timed out (30s limit exceeded)".into()),
            JsOutcome::OomKilled => Ok("JS error: out of memory (64 MiB limit exceeded)".into()),
        }
    }
}

#[derive(serde::Deserialize)]
pub struct JsArgs {
    pub code: String,
}
```

---

## Module entry — `src/extras/js/mod.rs`

```rust
pub mod engine;
pub mod host;
pub mod tool;
pub mod types;
```

---

## Module declaration — `src/extras/mod.rs`

Append after the last existing line (line 40, which is `pub(crate) mod truncate;`):

```rust
#[cfg(feature = "js")]
pub mod js;
```

---

## Builder registration — `src/agent/builder.rs`

`all_tools` is created at **line 279**. Existing feature-gated tool blocks occupy lines 281–333 (subagents, memory, mcp, advisor, lsp). The JS block must be inserted **after the `#[cfg(feature = "lsp")]` block (which ends at line 333) and before `filter_tools_by_allowlist` (line 335)**.

Existing `BashTool` injection is at lines 242–247 inside `base_tools` (a fixed-size `SmallVec`). JS goes into `all_tools` after construction because it is feature-gated and cannot be included in the fixed-size array.

```rust
// Insert at lines 333–334, before: let all_tools = filter_tools_by_allowlist(all_tools, &cli.tools);

#[cfg(feature = "js")]
{
    use crate::extras::js::{engine::js_thread_main, tool::JsTool, types::THREAD_STACK};
    let (js_tx, js_rx) = std::sync::mpsc::channel::<crate::extras::js::types::JsRequest>();
    std::thread::Builder::new()
        .name("js-engine".into())
        .stack_size(THREAD_STACK)
        .spawn(move || js_thread_main(js_rx))
        .expect("failed to spawn JS thread");
    all_tools.push(Box::new(JsTool::new(js_tx, permission.clone(), ask_tx.clone())));
}
```

---

## Tests — `src/extras/js/tests/`

Create `src/extras/js/tests/mod.rs` with the following integration tests. All are `#[tokio::test]`, all require `--features js`.

```rust
#[tokio::test]
async fn test_return_value() {
    // eval "1 + 1" via JsTool → Ok("2")
}

#[tokio::test]
async fn test_read_write_roundtrip() {
    // write_file("/tmp/zs_test.txt", "hello") then read_file("/tmp/zs_test.txt") → "hello"
}

#[tokio::test]
async fn test_timeout() {
    // eval "while(true){}" → response contains "timed out"
}

#[tokio::test]
async fn test_oom() {
    // allocate until OOM → response contains "out of memory"; NOT a panic
}

#[tokio::test]
async fn test_syntax_error_includes_line() {
    // eval "let x = ;" → error message contains a line number
}

#[tokio::test]
async fn test_fresh_runtime_after_oom() {
    // step 1: OOM → error; step 2: eval "1+1" → "2"; proves Runtime recreation works
}
```

---

## Acceptance criteria

All must pass under `cargo test --features js`:

- [ ] `cargo test --features js` compiles and all tests pass
- [ ] `JsTool` implements `rig::tool::Tool` and is `Send + Sync` (verified by compiler)
- [ ] A fresh `Runtime` is created and dropped for every `run_step()` call — never reused
- [ ] `set_memory_limit(64 * 1024 * 1024)` called on every new `Runtime`
- [ ] `set_max_stack_size(512 * 1024)` called on every new `Runtime`
- [ ] `set_interrupt_handler` deadline is set before `ctx.eval(...)` is called
- [ ] Interrupt fires and returns `JsOutcome::Timeout` for an infinite JS loop
- [ ] OOM returns `JsOutcome::OomKilled`, does not panic the engine thread
- [ ] `spawn()` calls route through `Sandbox::wrap_command` (`src/sandbox.rs:109`)
- [ ] Microtask queue is drained after every eval: `while rt.execute_pending_job() == Ok(true) {}`
- [ ] Exception messages include JS stack traces (line numbers visible to LLM)
- [ ] Binary compiled without `--features js` passes all existing tests unchanged

---

## Out of scope for Phase 1

- `fetch()` host global (Phase 2)
- File allow-list enforcement (Phase 2)
- birdcage process isolation (Phase 2)
- Skill library (Phase 3)
- Auto-admission (Phase 4)
- `require()`, `import()`, `final_answer` host global — forbidden permanently
