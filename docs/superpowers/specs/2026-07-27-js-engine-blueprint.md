# Blueprint: Embedded JavaScript Engine — zerostack

- **Document role**: superseded research artifact
- **Artifact version**: 2026-07-27
- **Status**: archival; not valid for implementation
- **Owner**: mini-agent maintainers
- **Superseded on**: 2026-07-29
- **Authoritative replacement**: [`../../specs/00-index.md`](../../specs/00-index.md) and its indexed normative phase specs

> **Historical content only.** The text below intentionally preserves the exploration that led to
> the current design. It contains obsolete host globals, identities, paths, feature relationships,
> and unsupported platform/sandbox claims. None of its examples or “decisions” are normative.

---

## 1. Problem Statement

`bash` does not exist on Windows. zerostack's current execution primitive (`BashTool` → `Sandbox::wrap_command`) therefore gates the entire agent on Unix, and its sandbox backend (bubblewrap) is Linux-only. On macOS the sandbox is also absent (`is_effectively_sandboxed()` returns false). The only harness that solves this cleanly is Codex-rs, which ships three platform-native sandbox backends (Landlock/seccomp, Seatbelt, Job Objects + AppContainer).

A built-in JavaScript engine gives one portable execution surface on all three platforms, eliminates the `sh -c` / `powershell -Command` fork on every hook invocation, and — because the engine runs in-process under hard resource limits — provides a sandbox that actually works on macOS and Windows today.

Secondary goal: establish the runtime substrate for Voyager-style accumulated skill libraries, where skills are JS functions retrieved by embedding similarity and verified before admission.

---

## 2. Engine Choice: rquickjs / QuickJS-NG

**Recommendation: rquickjs 0.12.x wrapping quickjs-ng.**

| Criterion | rquickjs | Boa (pure Rust) | deno_core (V8) |
|---|---|---|---|
| Binary delta (stripped) | ~500 KiB | ~3–5 MiB | ~30+ MiB |
| Fresh context startup | <300 μs | ~1.7 ms (5.7×) | ~55 ms |
| ES2024 (async/await, Proxy, BigInt) | ✓ full | ✓ 90% test262, limits WIP | ✓ full |
| Hard memory cap | ✓ `set_memory_limit` | Partial, WIP | Soft GC hint |
| Interrupt/timeout callback | ✓ (JS bytecode only) | Partial WIP | Epoch-based (preempts host calls) |
| `Send` Runtime | ✓ | ✓ | ✓ |
| C toolchain at build time | Pregenerated bindings — none needed | None (pure Rust) | Clang + Python (heavy) |
| Cross-compile musl/aarch64 | ✓ pregenerated | ✓ trivial | ✗ brittle |
| Windows MSVC build | ✓ pregenerated | ✓ | Possible but fiddly |
| License | MIT | MPL-2.0 | MIT |
| GPL-3.0 host compat | ✓ | ✓ | ✓ |

**Why not Boa**: startup is 5.7× slower (matters for per-step fresh contexts), and resource limits are immature — `set_memory_limit` and interrupt timeout are both in progress. We need hard limits today.

**Why not V8/deno_core**: 30 MiB binary delta is incompatible with zerostack's `opt-level = "z"` / strip philosophy. Build requires Clang + Python; cross-compilation to musl is brittle.

**Key rquickjs caveats (must mitigate)**:
- `set_interrupt_handler` fires only during JS bytecode execution, not during blocking Rust host function calls. A `spawn(cmd)` host call that hangs will hang forever regardless of the JS timeout.
- `Context` is `!Send` (thread-local). `Runtime` is `Send`.
- Stack size: QuickJS assumes ~8 MiB thread stack. Linux glibc default is 8 MiB (fine), musl is 128 KiB (crash), Windows main thread is 1 MiB (crash). Fix: spawn the JS thread explicitly with `std::thread::Builder::new().stack_size(8 * 1024 * 1024)`. This is portable and works at runtime — unlike the `.cargo/config.toml` link-flag approach, which is not honored by `cargo install`.

---

## 3. Integration Architecture

### 3.1 Feature Gate

```toml
# Cargo.toml
js = ["dep:rquickjs"]
```

Non-default initially; graduates to default once stable. The existing `bash` tool remains on Unix; `js` adds a second action primitive. On Windows, `bash` is compiled out and `js` becomes the only tool.

```rust
// src/agent/builder.rs
#[cfg(feature = "js")]
all_tools.push(Box::new(JsTool::new(permission.clone(), ask_tx.clone(), js_bridge.clone())));

#[cfg(not(windows))]
all_tools.push(Box::new(BashTool::new(/* ... */)));
```

### 3.2 Threading Model (Dedicated JS Thread)

The recommended integration is a **dedicated OS thread with an explicit 8 MiB stack and a bi-directional mpsc channel**. This is the only approach that satisfies all constraints simultaneously:

- Does not block the `current_thread` tokio event loop.
- Allows host functions to do async work (including a permission `Ask` round-trip to the TUI) via a `oneshot` channel.
- Allows hard-killing a runaway script by dropping the sender / interrupting via the interrupt handler.
- Avoids the `!Send` problem — `Context` never crosses thread boundaries.

```
tokio current_thread
     │  JsTool::call(args)
     │  ──────────────────────────────────────► JS THREAD (8 MiB stack)
     │  JsRequest { code, timeout }             Runtime::new() per session
     │                                          Context::full() per step
     │                                          interrupt deadline set
     │                                          eval(code)
     │                                            │ console.log(...)  → logs
     │                                            │ read_file(path)   → std::fs
     │                                            │ write_file(path)  → std::fs
     │                                            │ spawn(cmd, args)  → oneshot ─►  tokio
     │                                            │                   ◄─ result  ──  (perm check, Sandbox)
     │                                            │ finish
     │  ◄─────────────────────────────────────── JsResponse { logs, return_val, error }
     │  feed back to LLM as next user message
```

```rust
// src/extras/js/mod.rs

pub struct JsBridge {
    tx: mpsc::Sender<JsRequest>,
}

struct JsRequest {
    code: String,
    timeout: Duration,
    reply: oneshot::Sender<JsResponse>,
    /// Channel back into tokio for host function async callbacks (perm checks etc.)
    host_tx: mpsc::Sender<HostCall>,
}

pub struct JsResponse {
    pub logs: Vec<String>,
    pub return_value: Option<String>,
    pub error: Option<String>,
}

impl JsBridge {
    pub fn spawn(host_tx: mpsc::Sender<HostCall>) -> Self {
        let (tx, rx) = mpsc::channel::<JsRequest>();
        std::thread::Builder::new()
            .name("js-engine".into())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || js_thread_main(rx))
            .expect("failed to spawn JS thread");
        Self { tx }
    }
}
```

### 3.3 Per-Step Resource Limits

| Limit | Mechanism | Default |
|---|---|---|
| Memory | `Runtime::set_memory_limit` | 64 MiB |
| JS interpreter stack | `Runtime::set_max_stack_size` | 512 KiB |
| Thread stack | `Builder::stack_size` | 8 MiB |
| Wall-clock (JS) | `set_interrupt_handler` checks `Instant` | 10 s |
| Wall-clock (host calls) | Per-host-call `tokio::time::timeout` on the caller side | 30 s per call |
| Output size | Truncate `logs` after N bytes | 64 KiB |

**Fresh `Runtime` per step (mandatory — not optional)**: QuickJS's internal allocator is poisoned after an OOM / memory-limit breach; cleanup code itself requires allocation, making recovery impossible. A second step using the same `Runtime` after an OOM will fail unpredictably. The ~500 μs overhead of creating a fresh `Runtime` per step is the accepted cost. Confirmed via [quickjs-emscripten #30](https://github.com/justjake/quickjs-emscripten/issues/30) and [quickjs-ng discussion #823](https://github.com/quickjs-ng/quickjs/discussions/823).

### 3.4 Host API Surface

The JS sandbox sees **exactly these globals** — nothing else:

```typescript
// Type declarations emitted into the prompt as context
function read_file(path: string): string;
function write_file(path: string, content: string): void;
function spawn(cmd: string, args: string[]): { code: number; stdout: string; stderr: string };
function final_answer(answer: string): void;
console.log(...args: any[]): void;
```

`spawn()` is the one capability that escapes the VM sandbox. It **must** route through `PermissionChecker` before execution and honor `Sandbox::wrap_command`. On Linux this is bubblewrap; on macOS Seatbelt (`sandbox-exec`); on Windows Job Objects + AppContainer (see §4.2). The host function blocks the JS thread synchronously via a `oneshot` channel to tokio while the permission check / subprocess runs asynchronously on the tokio side.

**What is deliberately omitted**:
- `fetch()` — raw outbound network with no permission model; add later behind its own gate.
- `require()` / `import()` — no module system; flat global API only.
- `final_answer()` from the spike — rig terminates the loop on its own; this is redundant.

### 3.5 Permission Flow for `spawn()`

```
JS: spawn("git", ["status"])
  │
  ▼ host function call (synchronous on JS thread)
  sends HostCall::Spawn { cmd: "git", args: ["status"], reply: oneshot }
  │
  ▼ tokio (async)
  check_perm_path(permission, ask_tx, "js/spawn", "git status")
    ├── Allowed → sandbox.output_command("git status").await → reply
    ├── Ask → ask_tx prompt → user approves → sandbox ... → reply
    └── Denied → reply(Err("permission denied"))
  │
  ▼ JS thread resumes with result or throws JS exception
```

This means the JS interrupt handler's wall-clock deadline is running while `spawn()` is awaiting user approval. Extend the timeout generously for interactive sessions or pause/resume the deadline around host calls.

### 3.6 Error Surfacing to the LLM

```rust
match ctx.eval::<Value, _>(code) {
    Err(rquickjs::Error::Exception) => {
        let exc = ctx.catch();
        let exc = exc.as_exception().expect("exception object");
        let msg = exc.message().unwrap_or_default();
        let stack = exc.stack().unwrap_or_default(); // includes line numbers
        outcome.error = Some(format!("{msg}\n{stack}"));
    }
    Err(e) => outcome.error = Some(e.to_string()), // syntax errors, OOM
    Ok(v) => outcome.return_value = v.as_string().map(|s| s.to_string().ok()).flatten(),
}
```

Use `eval::<Value, _>` not `eval::<(), _>` so a trailing expression is captured as the return value. The full `.stack` with line numbers is the key — models self-correct much faster when they see "TypeError: read_file is not a function (line 14)" than just "TypeError".

After `eval`, drain the microtask queue:

```rust
loop {
    match rt.execute_pending_job() {
        Ok(true) => continue,  // more jobs pending
        Ok(false) => break,    // done
        Err(e) => { outcome.error = Some(e.to_string()); break; }
    }
}
```

---

## 4. Platform Independence Analysis

### 4.1 JS Engine (in-process)

The engine itself is platform-independent after the stack-size fix. No `#[cfg(windows)]` needed in the JS integration code except the thread Builder call (which is cross-platform by definition).

### 4.2 `spawn()` Sandbox per Platform

This is the one piece that requires platform-specific code. Currently zerostack has bwrap (Linux only). To reach Codex-rs parity:

| Platform | Current | Target (Phase 2) | Crate |
|---|---|---|---|
| Linux | bubblewrap (bwrap) | bwrap + Landlock + seccomp | `extrasafe`, `birdcage` |
| macOS | none | `sandbox-exec` (Seatbelt) — deprecated but no CLI replacement exists | `birdcage` |
| Windows | none | Job Object + AppContainer restricted token | `rappct` |

`birdcage` (Phylum) abstracts Landlock on Linux and Seatbelt on macOS behind one Rust API. Windows support is not in birdcage today; use `rappct` separately. The existing `Sandbox` struct in `src/sandbox.rs` should grow platform backends rather than being replaced.

**Kill signal portability**: the existing `kill_process_group` is `#[cfg(unix)]` and empty on Windows. Add a `Job Object` handle path using `processkit` crate on Windows so runaway `spawn()` calls can actually be killed.

### 4.3 Hook Portability

Today hooks run as `sh -c` / `powershell -Command` subprocesses (`src/extras/hooks/subprocess.rs:33`). On Windows, any hook written as a shell script fails. With the JS engine, hooks can be written in JS and executed in-process:

```toml
# .zerostack/config.toml
[hooks.pre_tool_use]
type = "js"
code = """
  if (event.tool === "write_file" && event.args.path.endsWith(".rs")) {
    spawn("rustfmt", [event.args.path]);
  }
"""
```

The hook dispatcher (`src/extras/hooks/dispatcher.rs`) gains a JS execution path alongside the existing subprocess path. Hook JS runs in a separate, even more restricted context (no `spawn` by default, only `console.log` and a `Decision` return value).

---

## 5. Self-Evolving Skill Library (Substrate)

The JS engine is the natural runtime for accumulated skills. This section defines the substrate — not the evolution machinery (Phase 3+).

### 5.1 Skill Unit

```typescript
interface Skill {
  id: string;           // sha256(source) — content-addressed
  name: string;         // human-readable
  description: string;  // what it does (used for embedding retrieval)
  source: string;       // JS function body
  embedding: number[];  // from description, ~1536-dim
  verified_at: string;  // ISO timestamp of last successful eval
  provenance: string;   // "human" | "agent:<session-id>"
}
```

Skills are stored as immutable blobs in `~/.local/share/zerostack/skills/<id>.json`. The index (`skills/index.json`) maps `(name → id)` and `(embedding → id)` for retrieval.

### 5.2 Retrieval at Agent Start

On session start, the top-K skills most relevant to the user's task (by embedding cosine similarity against the initial user message) are injected into the JS global scope:

```javascript
// Injected before model-written code runs
const formatRustFile = (path) => { /* skill body */ };
const findTodos = (dir) => { /* skill body */ };
```

The model sees these as part of its global API surface, listed in the system prompt alongside `read_file`, `write_file`, etc.

### 5.3 Verification Gate (Phase 2)

Before a skill is admitted to the library:
1. It must execute without error in a fully restricted sandbox (no `spawn`, read-only FS).
2. If the skill ships with assertions, they must pass.
3. If the skill was proposed by the agent, a human approval prompt is shown.

No auto-admission of agent-proposed skills until Phase 3.

### 5.4 Archive Policy

Open-ended (Voyager model): skills accumulate monotonically. Compaction (deduplication by semantic similarity) runs periodically via `/skills compact`. Hill-climbing and auto-pruning are Phase 3 concerns.

---

## 6. Prompt Design

The system prompt for a JS-enabled session includes:

```
You have a JavaScript execution environment. Write self-contained JS scripts instead of shell commands.

Available globals:
- read_file(path: string): string
- write_file(path: string, content: string): void  
- spawn(cmd: string, args: string[]): { code: number, stdout: string, stderr: string }
- console.log(...args): void  // your observation channel back to me

Per-step limits: 10s CPU, 64 MiB heap, 64 KiB output.
Errors and console.log output are returned to you as the observation.
Use spawn() only for external tools (git, cargo, etc.) — prefer built-in JS for file/text operations.

[Injected skills: formatRustFile, findTodos, ...]
```

**API hallucination mitigation**: the global surface is small and typed. If the model calls `fs.readFileSync` (Node.js habit), it gets a `ReferenceError: fs is not defined` with the line number and can self-correct.

---

## 7. Roadmap

| Phase | Deliverable | Effort |
|---|---|---|
| **1 — Engine** | `js` feature gate; JS thread + bi-directional channel; `read_file`/`write_file`/`spawn` host globals; limits + interrupt; error surfacing with `.stack`; microtask drain; macOS/Windows stack fix | ~2 weeks |
| **1 — Tests** | State-leak test, OOM test, timeout test, permission-denied test, syntax-error surfacing test, Windows MSVC CI target | bundled |
| **2 — Platform sandbox** | Add birdcage (Linux Landlock + macOS Seatbelt) and rappct (Windows Job Object) backends to `Sandbox`; fix `kill_process_group` on Windows via processkit | ~3 weeks |
| **2 — Hook JS** | Hook dispatcher JS execution path; hooks can be `.js` files instead of shell scripts | ~1 week |
| **3 — Skill library** | Skill store (content-addressed JSON), embedding index, retrieval at session start, human approval gate, `/skills` TUI command | ~3 weeks |
| **4 — Guided evolution** | Agent proposes skill mutations via LLM; mutation testing harness; rollback by hash | ~4 weeks |
| **5 — Auto-evolution** | Automated admission when evaluator passes; open-ended archive compaction; provenance audit log | Future |

---

## 8. Resolved Architecture Decisions

All open questions from the initial draft are resolved. The following are design decisions, not open items.

### Q1 — rig `Tool::call` Send bound ✅ RESOLVED

`Tool::call` in rig 0.40 returns `impl Future + WasmCompatSend`, which on all native targets equals `Send`. The `Tool` trait itself also requires `WasmCompatSend + WasmCompatSync`. This is enforced at trait-definition time regardless of whether the tokio runtime is `current_thread` or `rt-multi-thread`.

**Decision**: `JsTool` holds `mpsc::Sender<JsRequest>` (which is `Send + Sync`) — never a `Context` directly. A struct holding a `Context` will not compile. This is already correct in the design.

Source: [rig v0.40.0 Tool trait](https://github.com/0xPlaygrounds/rig/blob/v0.40.0/crates/rig-core/src/tool/mod.rs), [WasmCompatSend](https://raw.githubusercontent.com/0xPlaygrounds/rig/refs/tags/v0.40.0/crates/rig-core/src/wasm_compat.rs)

### Q2 — `set_memory_limit` recovery ✅ RESOLVED

QuickJS's internal allocator is poisoned after an OOM breach. Cleanup code requires allocation; the runtime cannot safely recover. A second step on the same `Runtime` after OOM fails unpredictably.

**Decision**: Fresh `Runtime` per step. ~500 μs overhead accepted. (See §3.3.)

Source: [quickjs-emscripten #30](https://github.com/justjake/quickjs-emscripten/issues/30), [quickjs-ng discussion #823](https://github.com/quickjs-ng/quickjs/discussions/823)

### Q3 — Seatbelt sunset on macOS ✅ RESOLVED

`sandbox-exec` is deprecated with no announced CLI replacement. The `birdcage` crate (Phylum) abstracts both Landlock (Linux) and Seatbelt (macOS) behind a single Rust API, providing a single swap point if Apple removes it.

**Decision**: Use `birdcage` for the Phase 2 sandbox layer on Linux and macOS. If Seatbelt is removed in a future macOS version, the swap is one crate update, not dozens of call sites.

### Q4 — `fetch()` capability ✅ RESOLVED

**Decision**: `fetch(url)` is added as a host global in Phase 2, gated through the existing `PermissionChecker` with tool name `"js/fetch"` and the URL as `input_key`. This gives users the full existing glob/regex rule system (`"*://docs.rs/*"`, `"*://crates.io/*"`, etc.). Unknown URLs fall to `Ask`. No new permission machinery is required.

```rust
// Config example
[tools.allow]
"js/fetch" = ["*://docs.rs/*", "*://crates.io/*", "*://api.github.com/*"]
```

### Q5 — Evaluator integrity for skill auto-admission ✅ RESOLVED

**Decision**: Skills ship with an embedded `tests` array of JS expressions that must evaluate to `true`. The test runner is Rust code outside the JS sandbox; skills cannot observe, modify, or influence it. The test execution context is fully restricted (no `spawn`, no FS writes). Auto-admission requires: (1) all embedded tests pass, (2) tests run in a fresh Runtime (not reused), (3) the test suite itself is immutable (stored as part of the skill's content-addressed blob — mutating tests changes the hash and invalidates the skill).

Phase 4 (guided evolution) adds a held-out integration check via a Rust test harness before any LLM-proposed skill is admitted.

### Engine alternatives ✅ SURVEYED — no change

Twelve alternative engines reviewed. Every pure-Rust option (Boa, Brimstone, Nova) fails on resource limits. SpiderMonkey/mozjs fails on Windows build complexity. JSC fails on cross-platform. Lua (mlua) is viable but is the wrong language for a developer-facing agent. **rquickjs / QuickJS-NG remains the correct choice.**
