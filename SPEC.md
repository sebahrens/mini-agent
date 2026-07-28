# JS Engine — Implementation Specification

**Document status**: implementation overview. The indexed specifications under `docs/specs/` are
normative and override this summary. Detailed implementation spec for Phases 1–5.

## Foundation — platform paths and storage

[`docs/specs/platform-paths.md`](docs/specs/platform-paths.md) defines the mandatory Linux, macOS,
and Windows path contract. One typed `AppPaths` resolver owns config, portable data, local data,
state, cache, credentials, and project-local roots. Modules do not call `dirs::*` or fall back to
the current directory independently.

- Linux follows XDG config/data/state/cache roots.
- macOS uses Application Support for config/data/state and Library/Caches for rebuildable data.
- Windows uses Roaming AppData for user configuration/portable data and Local AppData for SQLite,
  state, cache, and ACL-protected credentials.
- The learned `skills.db` is local data, embedding models/index snapshots are cache, and MCP OAuth
  tokens live in the credential root.
- Legacy paths migrate explicitly and atomically; conflicting candidates never silently win.
- Phase 3 must support the standard Agent Skills `SKILL.md` directory format and validated ZIP
  transport, while keeping imported scripts separate from verified learned JS functions.

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

**Note on host globals:** `register_host_globals` is called inside `ctx.with()` scope in Phase 1. Host functions are synchronous from JavaScript's perspective and block only the dedicated JS thread. File operations and `spawn()` use `PermissionBridge` to route permission checks back to tokio; finite host-call and permission deadlines prevent the JS thread from waiting indefinitely.

### 1.4 Host functions — `src/extras/js/host.rs`

Phase 1 exposes four globals:

| Global | Implementation | Permission check |
|--------|----------------|-----------------|
| `read_file(path)` | stable bounded read, at most 1 MiB | `"js/read_file"` + canonical target |
| `write_file(path, content)` | descriptor-relative atomic create or replace, at most 1 MiB | `"js/write_file"` + resolved final target |
| `spawn(cmd, args)` | `Sandbox::wrap_command` | `"js/spawn"` + command |
| `console.log(...)` | `eprintln!` / `tracing::info!` | none |

`read_file` canonicalizes the target and captures its identity before requesting
permission for the exact canonical UTF-8 path. After approval, it rejects non-regular
or oversized files, opens without following a final symlink, verifies that the opened
file still has the approved identity, and performs a bounded UTF-8 read.

`write_file` rejects oversized content before mutation and rejects existing final
symlinks. For a new file, it canonicalizes the nearest existing parent and requires
the missing suffix to be one normal filename component. It requests permission for
the derived final UTF-8 path, then uses the descriptor-relative atomic helpers in
`src/fs.rs` to revalidate the approved parent identity and create or replace without
following symlinks.

The host closures enforce that ordering explicitly:

```rust
pub(crate) fn make_read_file(
    permission_bridge: PermissionBridge,
    runtime: tokio::runtime::Handle,
) -> impl Fn(String) -> rquickjs::Result<String> {
    move |path: String| {
        let target = block_on_host_call(
            &runtime,
            &permission_bridge,
            "js/read_file",
            STEP_TIMEOUT,
            resolve_read_target(&path),
        )?;
        let permission_path = permission_path("js/read_file", &target.path)?;
        permission_bridge
            .check_path("js/read_file", &permission_path)
            .map_err(|error| permission_error("js/read_file", error))?;
        block_on_host_call(
            &runtime,
            &permission_bridge,
            "js/read_file",
            STEP_TIMEOUT,
            read_approved_file(target),
        )
    }
}

pub(crate) fn make_write_file(
    permission_bridge: PermissionBridge,
    runtime: tokio::runtime::Handle,
) -> impl Fn(String, String) -> rquickjs::Result<()> {
    move |path: String, content: String| {
        if content.len() > WRITE_FILE_MAX_BYTES {
            return Err(file_error(
                "js/write_file",
                "resource limit",
                format!("content exceeds {WRITE_FILE_MAX_BYTES} byte write limit"),
            ));
        }
        let target = block_on_host_call(
            &runtime,
            &permission_bridge,
            "js/write_file",
            STEP_TIMEOUT,
            resolve_write_target(&path),
        )?;
        let permission_path = permission_path("js/write_file", &target.path)?;
        permission_bridge
            .check_path("js/write_file", &permission_path)
            .map_err(|error| permission_error("js/write_file", error))?;
        block_on_host_call(
            &runtime,
            &permission_bridge,
            "js/write_file",
            STEP_TIMEOUT,
            write_approved_file(target, content),
        )
    }
}
```

Denial, failed interactive approval, timeout, cancellation, or channel closure returns
a JS error and performs no content read or mutation. Permission is always required;
later filesystem allow-lists may only narrow an approved operation.

`spawn()` uses the same bridge rather than constructing an ad hoc permission channel:

```rust
pub(crate) fn make_spawn(
    sandbox: Sandbox,
    permission_bridge: PermissionBridge,
    runtime: tokio::runtime::Handle,
) -> impl Fn(String, Vec<String>) -> rquickjs::Result<SpawnResult> {
    move |cmd: String, args: Vec<String>| {
        let command = format_permission_command(&cmd, &args);
        permission_bridge.check("js/spawn", &command)?;
        let mut child = sandbox.wrap_command(r#"exec "$0" "$@""#);
        child.arg(cmd).args(args);
        runtime.block_on(run_with_timeout_and_cancellation(child))
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

Applied only after secure path resolution and before the mandatory permission request.
Allow-lists may narrow access but never bypass interactive authorization. Approved
reads still use the bounded stable-read path, and approved writes still use the
descriptor-relative atomic-write path. Violations return a typed JS error without
reading content or mutating the filesystem.

### 2.3 birdcage integration

```toml
[dependencies]
birdcage = { version = "0.7", optional = true }
```

Wrap `spawn()` subprocess with birdcage cage. Configuration mirrors existing `Sandbox` parameters. On Windows: `rappct` (Job Object + AppContainer) — gated behind `#[cfg(target_os = "windows")]`.

---

## Phase 3 — Skill library

Phase 3 is defined normatively in
[`docs/specs/phase-3-skill-library.md`](docs/specs/phase-3-skill-library.md).

- Skills are immutable, fully content-addressed artifacts. Identity covers source, ordered
  tests, public exports/signatures, retrieval description/tags, capability tier, and identity
  schema version.
- Embeddings are generated at admission, tagged with model/revision/dimension, normalized once,
  and loaded into an immutable in-memory `SkillIndex` snapshot.
- Retrieval runs once **before model generation**, using the current user prompt plus bounded
  deterministic task context. Generated JavaScript is never used as the search query.
- Dense exact cosine ranking and FTS5/BM25 lexical ranking are fused, thresholded, deduplicated,
  and constrained by an injection budget. The target scale is 100,000 local/shared skills.
- Portable Agent Skills load from a validated directory or ZIP using the open `SKILL.md` format.
  Their metadata participates in progressive prompt-time discovery, but bundled JS is not trusted
  or injected into the learned-function runtime without the normal admission gates.
- The LLM sees a compact manifest of selected IDs, descriptions, signatures, and capabilities.
  `JsTool` binds exactly the corresponding immutable source snapshot for that turn.
- Skill sources and model-authored code execute as separate scripts in one fresh bounded
  context so injected code does not shift reported agent-code line numbers.
- Candidate verification is no-effect: Tier 0 has no host globals; Tier 1/2 have only declared,
  deterministic in-memory fakes that cannot touch real files, processes, permissions, or networks.

---

## Phase 4 — Auto-evolution

Phase 4 is defined normatively in
[`docs/specs/phase-4-auto-admission.md`](docs/specs/phase-4-auto-admission.md).

- `propose_skill({...})` accepts one structured artifact proposal, including exports,
  capabilities, tests, and an optional predecessor.
- Pending source is untrusted. Verification has no real host effects, requires nonempty exact
  boolean tests, performs mutation checks, and runs immutable held-out cases with hidden fakes.
- Held-out cases are data, not a compile-time Rust registry, so a learned skill does not require
  rebuilding the binary.
- Near-duplicate proposals are redirected toward replacement rather than crowding retrieval.
- One public promotion service reloads current pending data, recomputes identity, reruns every
  gate, obtains explicit human approval, and performs an atomic lifecycle transition.
- Initial admission enters durable canary state. Without the Phase 5 deterministic router, canary
  revisions remain absent from model manifests and JS bundles.
- The proposal loop is bounded per session and by source/test/output sizes and evaluation time.

---

## Phase 5 — Evidence-based self-learning

Phase 5 is defined normatively in
[`docs/specs/phase-5-evidence-learning.md`](docs/specs/phase-5-evidence-learning.md).

- Instrumented wrappers distinguish selected, injected, invoked, succeeded, and directly failed
  skills. Overall task failure is not automatically blamed on every injected skill.
- Automatic promotion is evidence-gated and limited to Tier 0 pure and Tier 1 read-only
  replacements. Tier 2 write/process/network revisions always require human approval.
- New revisions begin with human-approved canaries. Mature replacements may enter canary
  automatically only after inherited regressions, held-out cases, anti-gaming checks, and
  capability checks pass.
- A lineage-root canary cannot be retrieved and requires a second human decision to become active;
  production canary evidence and predecessor comparisons apply only to replacements.
- Integrity/policy violations quarantine immediately. Behavioral quarantine requires directly
  attributed failures and a minimum evidence window.
- Repair creates an immutable candidate linked to its predecessor. Promotion atomically
  supersedes the predecessor; rollback atomically quarantines the replacement and reactivates
  the predecessor.
- Every automatic decision stores its policy version and evidence snapshot. Raw telemetry is
  bounded and compacted into durable aggregates without retaining raw prompts or arguments.

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
| Skill integrity | Full SHA-256 over versioned canonical execution and discovery fields; operational data excluded |
| Skill retrieval | Prompt-time dense + lexical fusion, in-memory exact index, thresholded turn bundle |
| Admission | No-effect verifier, mutation checks, data-driven held-out cases, human-gated initial canary |
| Self-learning | Evidence-gated Tier 0/1 replacement, automatic quarantine, immutable repair, transactional rollback |
| Persistent paths | Typed Linux/macOS/Windows roots with explicit Roaming/Local storage classes and migration |
| Agent Skills | Open `SKILL.md` directory format; validated ZIP transport; progressive disclosure |
| MCP composition | Existing MCP transport/tool support remains independent and is tested with `js,skills` |

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
