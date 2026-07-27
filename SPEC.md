# JS Engine — Implementation Specification

Detailed implementation spec for Phases 1–4. All architecture decisions are resolved; this is the build guide.

## Phase 1 — Core engine

### 1.1 Cargo.toml changes

```toml
# zerostack/Cargo.toml

[features]
# existing features stay unchanged
js = ["dep:rquickjs"]

[dependencies]
# existing deps stay unchanged
rquickjs = { version = "0.12", features = ["full"], optional = true }
```

### 1.2 Types — `src/extras/js/types.rs`

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
```

### 1.3 Engine thread — `src/extras/js/engine.rs`

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

fn run_step(code: &str) -> JsOutcome {
    // Fresh Runtime EVERY step — OOM poisons allocator; never reuse
    let rt = match Runtime::new() {
        Ok(r) => r,
        Err(e) => return JsOutcome::Error(format!("Runtime::new failed: {e}")),
    };
    rt.set_memory_limit(MEMORY_LIMIT);
    rt.set_max_stack_size(STACK_LIMIT);

    let deadline = Instant::now() + STEP_TIMEOUT;
    rt.set_interrupt_handler(Some(Box::new(move || {
        if Instant::now() >= deadline {
            true  // interrupt JS execution
        } else {
            false
        }
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
                // Check if it was our interrupt (deadline exceeded)
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
                    // Serialize non-string return values as JSON
                    JsOutcome::Value(format!("{v:?}"))
                }
            }
        }
    });

    // Drain microtask queue after eval — required for Promise resolution
    while rt.execute_pending_job() == Ok(true) {}

    // rt drops here — Context must be dropped before Runtime
    // rquickjs handles this correctly via RAII if ctx is in scope above
    result
}
```

**Note on host globals:** `register_host_globals` is called inside `ctx.with()` scope in Phase 1. In Phase 1 the host functions are synchronous (file I/O is blocking on the JS thread, which is acceptable — it is not the tokio thread). The `spawn()` global requires a channel back to tokio for permission checks; see §1.4.

### 1.4 Host functions — `src/extras/js/host.rs`

Phase 1 host functions run synchronously on the JS thread. `spawn()` permission checks route back through a secondary channel to tokio (the `ask_tx` plumbing from `BashTool`).

```rust
// read_file: blocking std::fs::read_to_string on JS thread
// write_file: blocking std::fs::write on JS thread
// spawn: std::process::Command (blocking) — permission check via channel before exec
// console.log: eprintln! or tracing::info!
```

Full signatures:

```rust
pub fn make_read_file() -> impl Fn(String) -> rquickjs::Result<String> {
    move |path: String| {
        std::fs::read_to_string(&path)
            .map_err(|e| rquickjs::Error::new_from_js("read_file", &e.to_string()))
    }
}

pub fn make_write_file() -> impl Fn(String, String) -> rquickjs::Result<()> {
    move |path: String, content: String| {
        std::fs::write(&path, content)
            .map_err(|e| rquickjs::Error::new_from_js("write_file", &e.to_string()))
    }
}
```

`spawn()` implementation:

```rust
// JS thread receives a channel sender to tokio for permission checks.
// The JS thread blocks on a sync channel until tokio resolves the permission.
// This is acceptable because the JS thread is dedicated and not holding other work.
pub struct SpawnContext {
    pub sandbox: crate::sandbox::Sandbox,
    pub perm_sync_tx: std::sync::mpsc::SyncSender<PermRequest>,
}

pub fn make_spawn(ctx: SpawnContext) -> impl Fn(String, Vec<String>) -> rquickjs::Result<SpawnResult> {
    move |cmd: String, args: Vec<String>| {
        // 1. Send permission request to tokio via sync channel
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        ctx.perm_sync_tx.send(PermRequest { tool: "js/spawn".into(), key: cmd.clone(), reply: reply_tx }).ok();
        match reply_rx.recv() {
            Ok(PermResponse::Denied(msg)) => return Err(rquickjs::Error::new_from_js("spawn", &msg)),
            Ok(PermResponse::Allowed) => {}
            Err(_) => return Err(rquickjs::Error::new_from_js("spawn", "permission channel closed")),
        }
        // 2. Execute via Sandbox::wrap_command
        // ...
    }
}
```

### 1.5 JsTool — `src/extras/js/tool.rs`

```rust
use rig::tool::Tool;
use crate::agent::tools::{AskSender, PermCheck};
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
    type Error = crate::agent::tools::ToolError;
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

    async fn call(&self, args: JsArgs) -> Result<String, Self::Error> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tx.send(JsRequest { code: args.code, reply: reply_tx })
            .map_err(|_| ToolError::Msg("JS engine thread disconnected".into()))?;
        let response = reply_rx.await
            .map_err(|_| ToolError::Msg("JS engine reply channel closed".into()))?;
        match response.outcome {
            JsOutcome::Value(v) => Ok(v),
            JsOutcome::Void => Ok(String::new()),
            JsOutcome::Error(e) => Ok(format!("JS error:\n{e}")),  // return to LLM for self-correction
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

### 1.6 Builder registration — `src/agent/builder.rs`

Inside the tool-injection block (currently around line 230):

```rust
#[cfg(feature = "js")]
{
    use crate::extras::js::{engine::js_thread_main, tool::JsTool, types::THREAD_STACK};
    let (js_tx, js_rx) = std::sync::mpsc::channel::<crate::extras::js::types::JsRequest>();
    std::thread::Builder::new()
        .name("js-engine".into())
        .stack_size(THREAD_STACK)
        .spawn(move || js_thread_main(js_rx))
        .expect("failed to spawn JS thread");
    tools.push(JsTool::new(js_tx, permission.clone(), ask_tx.clone()).into_dyn());
}
```

### 1.7 Module declaration — `src/extras/mod.rs`

```rust
#[cfg(feature = "js")]
pub mod js;
```

### 1.8 Tests (Phase 1)

`src/extras/js/tests/` (integration):

```rust
#[tokio::test]
async fn test_return_value() { /* eval "1 + 1" → "2" */ }

#[tokio::test]
async fn test_read_write_roundtrip() { /* write_file then read_file */ }

#[tokio::test]
async fn test_timeout() { /* while(true){} → Timeout */ }

#[tokio::test]
async fn test_oom() { /* allocate until OOM → OomKilled, NOT panic */ }

#[tokio::test]
async fn test_syntax_error_includes_line() { /* "let x = ;" → error with line number */ }

#[tokio::test]
async fn test_fresh_runtime_after_oom() { /* two sequential steps: first OOMs, second succeeds */ }
```

---

## Phase 2 — Sandbox hardening

### 2.1 fetch() host global

Add to `Cargo.toml`:
```toml
[dependencies]
reqwest = { version = "0.12", features = ["blocking"], optional = true }
```

Permission routing: `check_perm(..., "js/fetch", &url)`. Glob rules in config:
```toml
[js.fetch.allow]
patterns = ["https://api.github.com/**", "https://*.openai.com/**"]
```

`fetch()` implementation uses `reqwest::blocking::Client` (on JS thread) — blocking is acceptable. Response: `{ status: number, text: string }`.

### 2.2 File allow-list

```toml
[js.file.allow]
read  = ["/home/**", "/tmp/**"]
write = ["/tmp/**"]
```

Checked in `read_file`/`write_file` host functions before `std::fs` call. Violations: `Err("path not in allow-list")` returned to JS.

### 2.3 birdcage integration

```toml
[dependencies]
birdcage = { version = "0.7", optional = true }
```

Wrap `spawn()` subprocess with birdcage cage. Configuration mirrors existing `Sandbox` parameters. On Windows: `rappct` (Job Object + AppContainer) — gated behind `#[cfg(target_os = "windows")]`.

---

## Phase 3 — Skill library

### 3.1 Skill schema

```rust
pub struct Skill {
    pub id:          String,          // sha256(source) hex
    pub source:      String,          // JS function source
    pub description: String,          // embedded for retrieval
    pub tests:       Vec<String>,     // JS expressions → true
    pub created_at:  u64,             // Unix timestamp
    pub usage_count: u64,
}
```

### 3.2 Skill store

SQLite via `rusqlite` (optional dep). One row per skill, indexed by `id`. Path: `~/.config/zerostack/skills.db` (respects `$XDG_CONFIG_HOME`).

```sql
CREATE TABLE skills (
    id          TEXT PRIMARY KEY,
    source      TEXT NOT NULL,
    description TEXT NOT NULL,
    tests       TEXT NOT NULL,  -- JSON array of strings
    created_at  INTEGER NOT NULL,
    usage_count INTEGER NOT NULL DEFAULT 0
);
```

### 3.3 Embedding index

Embedding model: `fastembed` crate (local, no API call required). Model: `BAAI/bge-small-en-v1.5` (~30 MiB). Embeddings stored in SQLite as BLOB (f32 array). Cosine similarity computed in Rust at retrieval time.

Add to `Cargo.toml`:
```toml
[dependencies]
fastembed = { version = "3", optional = true }
```

### 3.4 Retrieval at step start

Top-3 skills by cosine similarity are injected as a preamble block before agent JS code:

```javascript
// === Skill library (auto-injected) ===
// skill:abc123 — parse JSON safely
function parseJson(s) { try { return JSON.parse(s); } catch(e) { return null; } }
// skill:def456 — read lines from file
function readLines(path) { return read_file(path).split('\n').filter(l => l.length > 0); }
// === End skill library ===
\n// Agent code:
```

### 3.5 Skill verification

```rust
pub fn verify_skill(skill: &Skill) -> Result<(), String> {
    // Run each test expression in a fresh sandbox Runtime
    // Each must eval to true (JS boolean)
    // Failure: return Err with the failing test expression + actual value
}
```

---

## Phase 4 — Auto-evolution

### 4.1 Skill proposal protocol

Agent proposes a new skill by calling a `propose_skill(source, description, tests)` host global.
zerostack runs verification immediately. If all tests pass, the skill enters a **pending** state.

Pending skills are NOT injected into future steps until promoted.

### 4.2 Promotion gate

Promotion requires:
1. All `tests` pass in a fresh sandbox Runtime
2. A held-out Rust integration test in `src/extras/js/skills/verify.rs` passes (Phase 4 adds a harness)
3. Human approval (interactive `Ask` prompt) — auto-approval is disabled until evaluator gaming is studied

### 4.3 Loop-until-improvement

The agent can iterate on a skill proposal: propose → test fails → LLM receives failure → agent revises JS → proposes again. Max 5 iterations per skill per session.

---

## Resolved architecture decisions

| Decision | Resolution |
|----------|-----------|
| Engine | rquickjs 0.12 / QuickJS-NG — only option with hard limits + pregenerated Windows bindings |
| Runtime lifetime | Fresh per step — OOM poisons allocator, recovery impossible |
| Threading | Dedicated OS thread + mpsc — only way to satisfy `Send` bound while keeping `!Send` Context |
| Stack size | `std::thread::Builder::stack_size(8MiB)` — `.cargo/config.toml` not honored by `cargo install` |
| Interrupt scope | Fires only during JS bytecode — blocking host calls need per-call `tokio::time::timeout` |
| Sandbox abstraction | `birdcage` crate — single swap point for macOS Seatbelt deprecation |
| fetch() permissions | `PermissionChecker` with `"js/fetch"` + URL as `input_key`, glob allow rules in config |
| Skill integrity | Content-addressed by `sha256(source)` — mutating tests changes hash, structurally enforced |
| Skill retrieval | `fastembed` local model, cosine similarity, top-K preamble injection |
| Auto-admission | Phase 4 only, held-out Rust test required, human approval until evaluator gaming studied |

---

## Appendix A — Language alternatives survey

Eight alternatives were evaluated against three primary criteria: LLM writing proficiency, error self-correctability, and function library composability. The deciding factor in every case was the same: hard resource limits.

| Language | LLM proficiency | Hard mem cap | CPU interrupt | Rust bindings | Verdict |
|----------|-----------------|-------------|---------------|---------------|---------|
| **QuickJS (rquickjs 0.12)** | ~90% | `set_memory_limit` ✓ | `set_interrupt_handler` ✓ | Mature, pure-Rust bindings, pregenerated MSVC | **Selected** |
| Rhai 1.x | ~88% | ✗ (GH#327, open) | ✗ (stack depth only) | Pure Rust, good | Worse |
| Lua 5.4 (mlua 0.10) | ~72% | ✗ (manual hook) | ✗ (debug hook workaround) | Mature, requires C toolchain | Worse |
| Starlark (starlark-rust 0.12) | ~67%* | ✗ | ✗ | Pure Rust | Worse |
| Python (PyO3) | ~90% | ✗ | ✗ (GIL-dependent) | Mature, requires CPython (12–20 MiB delta) | Worse |
| Python (RustPython) | ~90% | ✗ | ✗ | Pure Rust, ~40% stdlib | Worse |
| Janet (janet-rs) | ~67% | ✗ | ✗ | Minimally maintained, C dep | Worse |
| Wren | ~80% | ✗ | ✗ | No maintained Rust bindings | Worse |
| Koto | ~78% | ✗ | ✗ | Pure Rust, moderate maturity | Worse |

\* Starlark's Python-like syntax is a **liability**: LLMs unlearn Python restrictions (no `while`, no `class`, no `import`) rather than learning a new language, yielding lower correctness than JavaScript despite surface familiarity.

**Pattern:** rquickjs is the unique intersection of (a) hard memory cap API, (b) CPU interrupt handler, (c) pregenerated Windows MSVC bindings, (d) ~500 KiB binary delta, (e) LLM training data prevalence (JavaScript is the most represented language in code corpora). No re-evaluation is warranted unless a new pure-Rust engine ships these APIs.
