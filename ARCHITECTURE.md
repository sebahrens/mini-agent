# JS Engine Integration — Architecture

**Document status**: architecture overview. Normative requirements live in `docs/specs/`.

QuickJS embedded in zerostack as a cross-platform action primitive, replacing bash on all platforms.

## 1. Problem statement

Bash is unavailable on Windows. A platform-specific bash/PowerShell split would double the tool surface and diverge behavior. Instead, zerostack embeds a **tiny JavaScript engine** as its primary action primitive. The agent writes JavaScript; the engine executes it natively with hard resource limits. The execution path is designed to be portable across Windows, Linux, and macOS; storage and sandbox support require the separate platform gates below before an unqualified Windows support claim.

Research basis: the CodeAct paradigm (executable code as actions) shows ~20% higher task success vs JSON tool calling. The engine doubles as a skill library runtime (Voyager model).

## 2. Engine choice — rquickjs 0.12.x / QuickJS-NG

All alternatives were evaluated:

| Engine | Binding | Hard mem limit | Windows | Verdict |
|--------|---------|---------------|---------|---------|
| **rquickjs 0.12 (QuickJS-NG)** | Rust-native | `Runtime::set_memory_limit` ✓ | MSVC pregenerated bindings ✓ | **Selected** |
| Boa 0.19 | Pure Rust | No hard limit ✗ | ✓ | Rejected |
| Brimstone | Pure Rust | No hard limit ✗ | Incomplete ✗ | Rejected |
| Nova | Pure Rust | No hard limit ✗ | Incomplete ✗ | Rejected |
| v8 (rusty_v8) | C++ | `Isolate` limit ✓ | ~50 MiB binary delta ✗ | Rejected |
| SpiderMonkey | C++ | ✓ | Build complexity ✗ | Rejected |

**rquickjs** is the only option with: hard memory cap, CPU interrupt handler, MIT license, <500 KiB binary delta, and pregenerated Windows MSVC bindings (no C toolchain at `cargo install` time).

## 3. Threading model

QuickJS `Context` and `Runtime` are `!Send`. The rig 0.40 `Tool::call` method returns `impl Future + WasmCompatSend`, which equals `Send` on native targets. A `JsTool` holding a `Context` will not compile.

**Solution:** Dedicated OS thread with mpsc channel.

```
[tokio runtime]                    [dedicated OS thread — "js-engine"]
JsTool::call()
  └─ tx.send(JsRequest) ─────────→ js_thread_main(rx)
  └─ await oneshot_rx ←─────────── handler spawns oneshot_tx per request
                                     ├─ for host calls needing async perm:
                                     │  sends back to tokio via ask_tx
                                     └─ returns JsResponse via oneshot_tx
```

`JsTool` holds only `Send + Sync` types:

```rust
pub struct JsTool {
    tx:         mpsc::Sender<JsRequest>,  // Send + Sync ✓
    permission: Option<PermCheck>,        // Send + Sync ✓
    ask_tx:     Option<AskSender>,        // Send + Sync ✓
}
```

Thread spawn (portable stack size):

```rust
std::thread::Builder::new()
    .name("js-engine".into())
    .stack_size(8 * 1024 * 1024)   // Windows=1MiB, musl=128KiB, glibc=8MiB — normalize all
    .spawn(move || js_thread_main(rx))
    .expect("failed to spawn JS thread");
```

`.cargo/config.toml` link flags are **not** honored by `cargo install`. Runtime stack_size is the only portable fix.

## 4. Runtime lifecycle — fresh per step (MANDATORY)

QuickJS's allocator is poisoned after an OOM kill. Cleanup code that runs after OOM itself requires allocation and panics. References: quickjs-emscripten #30, quickjs-ng discussion #823.

**Every JS step:**

```rust
let rt = Runtime::new().expect("failed to create QuickJS runtime");
rt.set_memory_limit(64 * 1024 * 1024);    // 64 MiB hard cap
rt.set_max_stack_size(512 * 1024);         // 512 KiB JS stack
let deadline = Instant::now() + STEP_TIMEOUT;
rt.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));
let ctx = Context::full(&rt).expect("failed to create context");
// ... register host globals ...
// ... eval code ...
// ... drain microtask queue ...
// rt drops here — Runtime dropped before Context per rquickjs docs
```

Overhead: ~500μs per step. Accepted.

## 5. Host API (Phase 1)

Five globals exposed to JS inside the sandbox:

| Global | Signature | Permission check |
|--------|-----------|-----------------|
| `read_file(path)` | `string → string` | `"js/read_file"` + path |
| `write_file(path, content)` | `(string, string) → void` | `"js/write_file"` + path |
| `spawn(cmd, args)` | `(string, string[]) → {stdout, stderr, code}` | `"js/spawn"` + cmd |
| `console.log(...)` | variadic | none |
| *(Phase 2)* `fetch(url, opts?)` | `(string, obj?) → {status, text}` | `"js/fetch"` + url |

No `require`, no `import`, no `final_answer`. Module system is intentionally absent.

### spawn() sandboxing

Routes through `Sandbox::wrap_command` — identical path to `BashTool`. On Linux: bubblewrap + Landlock. On macOS: `birdcage` (Seatbelt abstraction). On Windows: Job Objects (Phase 2).

### Interrupt handler limitation

`set_interrupt_handler` fires only during **JS bytecode execution**, not during blocking Rust host calls. Blocking `spawn()` needs per-call `tokio::time::timeout` on the tokio side:

```rust
// In the async permission/spawn handler running in tokio:
tokio::time::timeout(Duration::from_secs(30), sandbox.output_command(&cmd)).await
```

## 6. Permission flow

Every host function call routes through the same permission checker as bash:

```rust
// Pattern (same as check_perm in bash.rs):
check_perm(&self.permission, &self.ask_tx, "js/read_file", &path).await?
```

Permission config uses the existing allow/deny/ask system. Unknown paths fall to `Ask` — user approves interactively.

## 7. Error surfacing

Errors are returned verbatim to the LLM for self-correction:

```rust
match ctx.eval::<Value, _>(code) {
    Err(rquickjs::Error::Exception) => {
        let exc = ctx.catch().as_exception().unwrap();
        JsOutcome::Error(format!(
            "{}\n{}", exc.message().unwrap_or_default(),
                      exc.stack().unwrap_or_default()
        ))
    }
    Err(e) => JsOutcome::Error(e.to_string()),
    Ok(v)  => JsOutcome::Value(v.as_string().map(|s| s.to_string().unwrap_or_default())),
}
// Always drain microtask queue — do this even on error paths that permit it
while rt.execute_pending_job() == Ok(true) {}
```

`eval::<(), _>` loses the return value and the stack trace. Always use `eval::<Value, _>`.

## 8. Sandbox hardening (Phase 2)

Platform backends abstracted through the `birdcage` crate:

| Platform | Mechanism | Crate |
|----------|-----------|-------|
| Linux | Landlock + seccomp + bubblewrap | `birdcage` |
| macOS | Seatbelt (`sandbox-exec`) | `birdcage` |
| Windows | Job Objects + AppContainer | `rappct` (Phase 2) |

`birdcage` provides a single swap point if Apple removes `sandbox-exec`. File allow-list and network egress rules are enforced at the process level around `spawn()` calls.

`fetch()` in Phase 2: route through `PermissionChecker` with tool `"js/fetch"` and URL as `input_key`. Glob/regex allow rules in config. Unknown URLs fall to `Ask`.

## 9. Skill library (Phase 3)

Voyager-model substrate for self-evolving agent capabilities. The normative design is
[`docs/specs/phase-3-skill-library.md`](docs/specs/phase-3-skill-library.md).

```
[immutable skill artifact]
  key: sha256(versioned canonical execution + discovery payload)
  payload: { source, exports, description, tags, tests, capability tier }

[turn-time retrieval]
  query: current user prompt + bounded task context, before model generation
  dense: pre-normalized in-memory exact cosine index (up to 100,000 local skills)
  lexical: SQLite FTS5/BM25 over descriptions, signatures, tags, and identifiers
  fusion: reciprocal-rank fusion, similarity floor, dedupe, and source/token budget

[turn skill bundle]
  model sees: ids, descriptions, signatures, and capability tiers
  JS runtime receives: exactly the immutable sources selected for that turn
```

Retrieval occurs once per user turn in the runner/session layer, where the prompt exists. The
JS thread never embeds text or queries SQLite. The selected bundle is frozen for the turn and
is reused by every JS call. Skill source and model-authored code are evaluated as separate
scripts in one fresh context, preserving model-authored stack-trace line numbers.

The default index is an immutable contiguous in-memory exact scan. SQLite is authoritative
persistence, not a per-query vector reader. A replaceable `SkillIndex` boundary permits ANN
only after a 100,000-skill benchmark shows the exact implementation misses its p99 latency
budget. Query embeddings and active-index generations are cached; skill embeddings are
computed at admission or migration, never lazily on the request path.

## 10. Evaluator integrity

The canonical identity includes source, ordered tests, exports/signatures, retrieval metadata,
and declared capabilities. Mutating any execution- or discovery-bearing field changes the ID.
Operational status, telemetry, timestamps, and embedding bytes are outside the identity.

Candidate source and tests run in a fresh, bounded **no-effect** context. Tier 0 gets no host
globals; Tier 1/2 get only declared, deterministic in-memory record/replay fakes that cannot touch
real files, processes, permissions, or networks. Tests must be nonempty and each must evaluate to
exact JavaScript boolean `true`. Mutation checks replace exports with throwing stubs and reject
suites that still pass. Held-out golden/property cases and fake responses are content-addressed
data approved outside the proposing agent; adding a new case does not require recompiling zerostack.

[`docs/specs/phase-4-auto-admission.md`](docs/specs/phase-4-auto-admission.md) defines human-gated
candidate admission. [`docs/specs/phase-5-evidence-learning.md`](docs/specs/phase-5-evidence-learning.md)
defines the evidence-based lifecycle: deterministic canaries, directly attributed invocation
telemetry, automatic quarantine, immutable repair revisions, supersession, and transactional
rollback. Automatic promotion is limited to pure/read-only replacements with sufficient
held-out and canary evidence; write/process/network skills always require human approval.

## 11. Cross-platform paths and portable skills

[`docs/specs/platform-paths.md`](docs/specs/platform-paths.md) is the normative storage contract.
Startup constructs one typed resolver for configuration, roaming/portable data, machine-local
data, state, cache, credentials, and `.zerostack` project state. Linux uses XDG roots; macOS uses
Application Support and Caches; Windows deliberately splits Roaming AppData configuration from
Local AppData databases, cache, state, and credentials. No durable module falls back to CWD.

The learned JS database is machine-local. Embeddings are generated before request-time retrieval,
and immutable contiguous index snapshots keep exact cosine search independent of filesystem
latency. Model downloads and rebuildable snapshots are cache. MCP OAuth material is stored under a
separate private credential root with Unix modes or Windows ACLs.

Portable instruction skills follow the open Agent Skills directory format (`SKILL.md` plus optional
scripts/references/assets). A validated ZIP is accepted as transport, regardless of archive
filename. These packages use progressive disclosure and may compose with configured MCP tools, but
their `allowed-tools` metadata grants nothing and their bundled JS never becomes an injected
learned function without Phases 3–5 verification.

## 12. Platform capability summary

| Concern | Mechanism | Platform-specific? |
|---------|-----------|-------------------|
| JS execution | rquickjs (QuickJS-NG) | No |
| Thread stack | `Builder::stack_size(8MiB)` | No |
| Windows bindings | Pregenerated MSVC bindings in rquickjs | Windows only at build time |
| Process spawn | `Sandbox::wrap_command` | Abstracted |
| Filesystem sandbox | `birdcage` | Abstracted |
| Network sandbox | `birdcage` / Job Objects | Abstracted |
| Persistent storage | Typed `AppPaths` + explicit artifact classes | Linux XDG / macOS / Windows Known Folders |
| Credentials | Private files + platform protection | Unix modes / Windows ACL |
| Agent Skills transport | Validated directory or ZIP | Portable filename policy enforced everywhere |
| MCP | `rmcp` command/HTTP/OAuth transports and permission-wrapped tools | Path resolver owns OAuth credentials |
| Bash tool | `#[cfg(not(target_os = "windows"))]` | Yes — compiled out on Windows |

The table describes the selected architecture, not current delivery. Windows readiness requires
the resolver, ACL, filename/archive, migration, CI, and release-smoke Beads to be closed.

## 13. Feature gate

```toml
# zerostack/Cargo.toml
[features]
js = ["dep:rquickjs"]

[dependencies]
rquickjs = { version = "0.12", features = ["full"], optional = true }
```

Initially non-default. Graduates to `default = ["js"]` when Phase 1 passes full coverage. `bash` feature kept on non-Windows, compiled out on Windows via `#[cfg(not(target_os = "windows"))]`.

## 14. Module structure

```
zerostack/src/extras/js/
├── mod.rs        # pub use, feature gate
├── engine.rs     # js_thread_main, Runtime lifecycle, eval loop
├── tool.rs       # JsTool: Tool impl, mpsc channel setup
├── host.rs       # read_file, write_file, spawn, console implementations
├── types.rs      # JsRequest, JsResponse, JsOutcome, step limits
└── skills/       # Phase 3
    ├── store.rs  # content-addressed skill storage
    ├── index.rs  # embedding index + retrieval
    └── verify.rs # test runner (Rust, outside sandbox)
```

Registration in `src/agent/builder.rs` occurs after feature-gated tool collection and before the
allow-list filter (currently the `register_js_tool` call at line 361):

```rust
#[cfg(feature = "js")]
{
    let (js_tx, js_rx) = mpsc::channel(8);
    std::thread::Builder::new()
        .name("js-engine".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || js_thread_main(js_rx))
        .expect("failed to spawn JS thread");
    tools.push(JsTool::new(js_tx, permission.clone(), ask_tx.clone()).into_dyn());
}
```
