# Brokered Cross-Platform JavaScript Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the in-process QuickJS thread with a small, fail-closed, cross-platform worker process whose only effects are typed, permission-checked, audited requests brokered by the mini-agent parent.

**Architecture:** One lazily started instance of the current executable runs in an internal worker mode. The process stays warm, but every agent step and every complete verification request creates a fresh bounded QuickJS `Runtime`; the parent owns policy, permissions, persistence, effect execution, and audit. Linux bubblewrap, macOS Seatbelt, and Windows LPAC/AppContainer plus a Job Object remove ambient worker authority; unavailable containment disables JS without an in-process fallback.

**Tech Stack:** Rust 1.96, rquickjs 0.12, serde/serde_json, Tokio, standard anonymous pipes, SQLite for the existing learned-skill lifecycle, platform Win32 APIs through target-specific `windows-sys`, bubblewrap, and macOS `sandbox-exec`/Seatbelt.

## Global Constraints

- Generated, retrieved, proposed, and held-out JavaScript is untrusted and never executes in the mini-agent parent process.
- There is one interactive worker process per mini-agent process, not a pool, daemon, helper package, VM, cgroup service, WFP driver, or mandatory privileged service.
- The same executable enters worker mode before Clap, Tokio, configuration, paths, logging, hooks, MCP, providers, credentials, or the TUI initialize.
- A warm process never implies a warm QuickJS heap: create and drop a fresh `Runtime` for every agent step and every whole-skill verification request.
- Preserve the existing limits: 30-second total step deadline, 64 MiB QuickJS heap, 512 KiB QuickJS stack, bounded pending jobs, 1 MiB file/fetch bodies, and bounded spawn output.
- IPC is strictly alternating and half-duplex: `RunStep -> (EffectRequest -> EffectResponse)* -> StepResult`; exactly one frame is outstanding.
- Frames use a big-endian `u32` length prefix plus JSON, an 8 MiB frame ceiling, protocol/build identity, invocation ID, monotonic sequence, and closed message enums.
- Parent cancellation kills and reaps the worker instead of sending an unsolicited cancellation frame; the next call starts a fresh worker.
- Parent enforcement never trusts a worker-claimed artifact hash. The parent always enforces session permission, path/fetch narrowing policy, deadlines, output limits, and effect audit. Per-artifact grants additionally constrain ordinary JS execution but cannot be claimed as protection against a native worker compromise.
- A native-compromised worker can at most borrow the union of grant handles provisioned for its current step. Platform containment removes ambient filesystem, network, credential, database, and parent-memory authority. Process creation is separately denied on Linux/Windows and probed under the weaker macOS backend; the parent always kills the whole containment/process group.
- Stored-skill initialization is pure. Skill realms do not contain `propose_skill` or effect hosts during source evaluation, and pending initialization jobs are rejected.
- Learned-skill ABI v2 receives one immutable hidden invocation capability object as the first export argument. It contains only declared methods, every closure embeds a parent-created grant ID, and every method becomes unusable after the export promise settles or the invocation is cancelled. No effect global is installed in a skill realm.
- Model-authored code retains a bounded `propose_skill` writer host; persisted skill realms never receive it. Durable enqueue remains parent-owned.
- Existing `SkillArtifact`/SQLite storage remains authoritative. Do not add a parallel `tools/*/manifest.toml`, writable skill folder, or source-only hash.
- Identity version 2 uses structured capability scopes. All identity-v1 artifacts are quarantined until explicitly reproposed/reverified; do not infer scopes automatically.
- A durable authorization/intent must be appended and synced before every brokered call, including `read_file`. Audit failure denies the effect.
- Production, embedded tests, mutation tests, inherited tests, and held-out tests use the same worker-owned artifact loader and realm contract. Verification receives deterministic fake capability objects through the same registration path; this is loader/realm-contract parity, not a claim that fake and real I/O, permissions, audit, or timing are identical.
- Worker stdout is protocol-only. Console output is bounded in `StepResult`; diagnostics use a bounded stderr pipe and never include source, prompts, arguments, contents, environment, or secrets.
- Linux uses a broker-only bwrap profile with no workspace/cache bind, an in-worker seccomp deny policy for process creation/exec, and validated OS resource limits. macOS uses a broker-only Seatbelt profile, explicit descriptor closing and rlimits, and reports that `sandbox-exec` is a deprecated/weaker best-effort MAC backstop. Windows uses zero-capability LPAC/AppContainer, creation-time Job assignment, child-process denial, compatible mitigations, and an exact handle list.
- If containment is unavailable or fails, JS is unavailable. Never retry in-process or unconfined outside tests.
- Parent-brokered JS `spawn` remains disabled on Windows until `mini-agent-uq5c` delivers and verifies the separate general command sandbox.
- Run `cargo fmt` before every commit, use `cargo test` for type checking/tests, and use `cargo install --path . --debug` for the development binary. Never run `cargo build`, `cargo check`, or `--release` during development.
- Do not use the repository's `just build`, `just build-all`, `just check`, or `just fmt` recipes for this work because they invoke prohibited development commands or mutate files outside the narrow verification step.
- Every commit message includes a body stating `Coauthored by Seb and Claude`.

---

## File Structure

### New production files

- `docs/specs/phase-6-brokered-js-runtime.md` — normative threat model, protocol, worker, broker, persistence, platform, rollout, and acceptance contract.
- `src/extras/js/protocol.rs` — wire-only IDs, request/result/effect enums, frame codec, limits, and protocol state validation; no policy or QuickJS imports.
- `src/extras/js/worker.rs` — internal worker entrypoint, fresh-runtime construction, worker-side host closures, execution state machine, and bounded diagnostics.
- `src/extras/js/supervisor.rs` — process-wide stateless worker supervisor, serialization, watchdog, crash/restart/recycle behavior, and transport ownership.
- `src/extras/js/broker.rs` — parent-owned invocation grants, typed effect dispatch, permission/policy intersection, and normalized audit metadata.
- `src/extras/js/audit.rs` — private append-only hash-chained effect intent/completion log, startup recovery, retention bounds, and failure policy.
- `src/extras/js/realm.rs` — one production/verification artifact loader, private skill contexts, pure initialization, declared exports, and JSON-safe boundaries.
- `src/sandbox/worker.rs` — worker-specific containment status plus platform-neutral owned process/pipe control; it never reuses the workspace-readable `Sandbox::wrap_command` profile.
- `src/sandbox/worker/linux.rs` — broker-only bubblewrap launcher.
- `src/sandbox/worker/macos.rs` — broker-only Seatbelt launcher.
- `src/sandbox/worker/windows.rs` — LPAC/AppContainer, Job Object, process mitigations, and exact inherited-handle launcher.
- `src/extras/js/tests/worker_protocol.rs` — hostile frame and state-machine tests.
- `src/extras/js/tests/worker_runtime.rs` — worker bootstrap, fresh-runtime, crash, cancellation, and parity tests.
- `src/extras/js/tests/worker_broker.rs` — grant, permission, audit, and broker target tests.
- `src/extras/js/tests/skill_realm_isolation.rs` — cross-context proof, pure initialization, ambient-global absence, and async attribution tests.
- `src/extras/js/tests/worker_containment.rs` — platform broker-only capability probes.

### Existing files with changed responsibility

- `src/main.rs` — synchronous preflight into internal worker mode before constructing Tokio or parsing normal CLI state.
- `src/extras/js/mod.rs` — declare the protocol, worker, supervisor, broker, audit, and realm modules.
- `src/extras/js/tool.rs` — retain the Rig `Tool`, permission owner, skill-turn snapshot, proposal/admission/telemetry handles; replace the JS thread with an `Arc<JsWorkerSupervisor>` call.
- `src/extras/js/engine.rs` — shrink to worker-owned evaluation helpers or move those helpers into `worker.rs`/`realm.rs`; no parent call site may create a QuickJS runtime.
- `src/extras/js/host.rs` — split QuickJS conversion from parent effect services so broker code returns typed Rust results/errors independent of rquickjs.
- `src/extras/js/types.rs` — retain parent-local cancellation/outcomes and share only deliberately serializable values with `protocol.rs`.
- `src/extras/js/skills/mod.rs` — capability identity v2, structured scopes, and versioned decoding.
- `src/extras/js/skills/capability.rs` — worker-side execution attribution plus parent-created grant bindings; remove global-map attribution ambiguity.
- `src/extras/js/skills/proposal.rs` — deserialize proposal drafts from the wire and keep canonicalization/enqueue parent-side.
- `src/extras/js/skills/verify.rs` — become a parent client of worker verification rather than constructing rquickjs runtimes.
- `src/extras/js/skills/held_out.rs` and `admission.rs` — submit exact-wrapper verifier requests and consume worker reports/transcripts.
- `src/extras/js/skills/store.rs` — identity-v2 quarantine migration and any lifecycle linkage for effect-audit evidence.
- `src/extras/js/skills/admission_store.rs` and `lifecycle.rs` — consume one-time parent-owned approval authorizations rather than caller-asserted identity/time.
- `src/agent/builder.rs` — construct per-build broker policy and reuse the stateless supervisor; do not spawn one worker for every rebuild.
- `src/provider.rs` and `src/ui/state.rs` — thread shared JS services through repeated full-agent rebuilds if explicit ownership proves simpler than a process-wide `OnceLock`.
- `src/sandbox.rs` — declare `mod worker`; leave the general subprocess sandbox behavior separate.
- `src/cli.rs`, `src/startup.rs`, `src/print.rs`, and `src/config/mod.rs` — expose worker-containment status and fail-closed JS availability without conflating it with the general subprocess sandbox.
- `.github/workflows/ci.yml` — required Linux/macOS/Windows worker rows, real backend probes, hostile protocol tests, and resource reports.
- `docs/specs/00-index.md`, phase specs 1–5, `ARCHITECTURE.md`, `SPEC.md`, and `README.md` — replace superseded in-process and capability claims after implementation proves them.

---

### Task 1: Establish the normative Phase 6 security contract

**Files:**
- Create: `docs/specs/phase-6-brokered-js-runtime.md`
- Modify: `docs/specs/00-index.md` — phase index/status and authority map
- Modify: `docs/specs/phase-1-js-engine.md` — runtime lifecycle, limits, and supersession text
- Modify: `docs/specs/phase-2-sandbox.md` — JS-worker versus general subprocess containment

**Interfaces:**
- Consumes: approved architecture and this plan.
- Produces: indexed sections named `Threat model`, `Worker lifecycle`, `Wire protocol`, `Capability broker`, `Persistence boundary`, `Verification parity`, `Effect audit`, `Platform containment`, `Failure semantics`, and `Acceptance matrix` for every implementation Bead to cite.

- [ ] **Step 1: Write the normative spec and authority update**

The new spec must state these executable invariants, without implementation-status checkmarks:

```text
One parent-created worker process is the native-code containment unit.
One RunStep or VerifyArtifact request is the QuickJS Runtime lifetime unit.
Stored-skill source initialization has no effects and no writer API.
Parent policy is authoritative even when every worker-supplied attribution field is malicious.
No supported production path launches an uncontained worker.
```

Document the two delivery gates explicitly:

```text
Realm gate: cross-Context function/promise behavior must pass Task 2. Failure stops
Phase 6; the shared-global wrapper is not an accepted fallback.

Windows gate: LPAC image loading from every supported install location must pass
Task 3. Failure leaves Windows JS disabled; restricted-token or unconfined worker
fallback is forbidden.
```

- [ ] **Step 2: Mark earlier contracts as superseded only where Phase 6 owns them**

Phase 1 remains historical for host behavior and limits, but its per-`JsTool` in-process thread is replaced. Phase 2 remains authoritative for parent-brokered subprocesses; its workspace-visible bwrap/Seatbelt profiles are forbidden for the JS worker.

- [ ] **Step 3: Verify corpus consistency**

Run:

```bash
rg -n "in-process|dedicated OS thread|Windows.*unsupported|propose_skill|Runtime" \
  docs/specs/00-index.md \
  docs/specs/phase-1-js-engine.md \
  docs/specs/phase-2-sandbox.md \
  docs/specs/phase-3-skill-library.md \
  docs/specs/phase-4-auto-admission.md \
  docs/specs/phase-5-evidence-learning.md \
  docs/specs/phase-6-brokered-js-runtime.md
```

Expected: every surviving in-process/thread statement is explicitly historical or superseded; Phase 6 is indexed and owns the replacement behavior.

- [ ] **Step 4: Commit**

```bash
git add docs/specs
git commit -m "docs: specify brokered JavaScript runtime" \
  -m "Coauthored by Seb and Claude"
```

### Task 2: Prove QuickJS cross-context realm behavior before relying on it

**Files:**
- Create: `src/extras/js/tests/skill_realm_isolation.rs`
- Modify: `src/extras/js/tests/mod.rs`
- Modify after result: `docs/specs/phase-6-brokered-js-runtime.md` section `Realm isolation`

**Interfaces:**
- Consumes: rquickjs `Runtime`, two `Context::full` instances, `Persistent<Function>`, and pending-job draining.
- Produces: a tested yes/no delivery gate for Task 13; no production abstraction.

- [ ] **Step 1: Write the cross-context tests**

Tests must create one runtime and two contexts and prove all of the following:

```rust
#[test]
fn skill_function_keeps_its_own_global_realm_when_called_from_agent_context() {}

#[test]
fn skill_promise_settles_while_runtime_jobs_are_drained_from_the_agent_step() {}

#[test]
fn cross_context_exceptions_preserve_bounded_message_and_stack() {}

#[test]
fn skill_context_cannot_resolve_agent_effect_or_proposal_globals() {}

#[test]
fn values_cross_the_boundary_only_through_the_declared_json_clone_contract() {}
```

The first test sets conflicting sentinels in both globals, saves/restores a skill function across contexts, calls it from the agent context, and requires the skill sentinel. The promise test must perform a continuation after at least one pending job. The absence test checks `typeof read_file`, `write_file`, `fetch`, `spawn`, and `propose_skill` are all exactly `"undefined"` in the skill context.

- [ ] **Step 2: Run the gate**

Run:

```bash
cargo test --locked --features js,skills skill_realm_isolation -- --nocapture
```

Expected: PASS. Any failure blocks Tasks 13 and 16 and requires a new approved design; do not weaken the realm contract.

- [ ] **Step 3: Record the proven rquickjs behavior in the normative spec**

Record the exact rquickjs version and which cross-context operations passed. Do not claim the contexts are a native security boundary; they are a source-level authority boundary inside an already untrusted worker.

- [ ] **Step 4: Commit**

```bash
git add src/extras/js/tests docs/specs/phase-6-brokered-js-runtime.md
git commit -m "test: prove QuickJS skill realm isolation" \
  -m "Coauthored by Seb and Claude"
```

### Task 3: Prove Windows LPAC worker image-loading feasibility

**Files:**
- Create: `src/sandbox/worker.rs`
- Create: `src/sandbox/worker/windows.rs`
- Modify: `src/sandbox.rs` — declare `mod worker` and expose only narrowly required trusted-backend validation helpers
- Modify: `Cargo.toml` target-specific Windows dependencies
- Modify after result: `docs/specs/phase-6-brokered-js-runtime.md` section `Windows containment`

**Interfaces:**
- Consumes: current executable path, anonymous child pipe handles, Windows AppContainer profile APIs, and a temporary Job Object.
- Produces: a Windows-only test helper proving whether the supported installed binary can start with zero capabilities; production launch remains disabled.

- [ ] **Step 1: Add the target-specific dependency and isolated FFI module**

Use one target-specific `windows-sys` entry with only the Win32 feature groups needed by process creation, security capabilities, AppContainer profiles, Job Objects, ACLs, and handles. Because the crate has `#![deny(unsafe_code)]`, put a narrowly scoped `#[allow(unsafe_code)]` on the Windows worker module; never weaken the crate-wide lint. Keep every unsafe call in `src/sandbox/worker/windows.rs`, with `// SAFETY:` comments describing pointer lifetime, initialized lengths, ownership, and handle closure.

- [ ] **Step 2: Write a Windows-only ignored real-backend test**

```rust
#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires a real Windows AppContainer backend"]
fn windows_lpac_can_load_current_exe_with_only_protocol_handles() {}
```

The test must:

1. create or derive a stable zero-capability AppContainer profile;
2. determine whether the executable already has an applicable read/execute ACE;
3. add only a specific AppContainer/LPAC ACE when the current user owns and can safely modify the file ACL;
4. refuse broad `Everyone` or writable-directory ACL changes;
5. create the process with `SECURITY_CAPABILITIES`, LPAC opt-out, `HANDLE_LIST`, and a creation-time Job;
6. receive a fixed readiness frame; and
7. prove the worker cannot open a sentinel workspace file.

- [ ] **Step 3: Run on Windows and record the supported installation cases**

Run on `windows-latest` and a local standard-user Windows installation:

```powershell
cargo test --locked --no-default-features --features js windows_lpac_can_load_current_exe_with_only_protocol_handles -- --ignored --nocapture
```

Expected: PASS for user-owned Cargo/install/archive locations. If a machine-wide protected install cannot be granted safely, record it as unsupported and require JS-disabled status for that location.

- [ ] **Step 4: Commit the spike, not a production claim**

```bash
git add Cargo.toml Cargo.lock src/sandbox docs/specs/phase-6-brokered-js-runtime.md
git commit -m "test: prove Windows LPAC worker launch feasibility" \
  -m "Coauthored by Seb and Claude"
```

### Task 4: Minimize rquickjs features and prove module loading is absent

**Files:**
- Modify: `Cargo.toml` — optional `rquickjs` dependency declaration
- Modify: `Cargo.lock`
- Modify: `src/extras/js/tests/mod.rs`

**Interfaces:**
- Consumes: synchronous rquickjs core APIs only.
- Produces: `rquickjs = { version = "0.12", default-features = false, features = ["std"], optional = true }` unless the compiler proves one additional named feature is required.

- [ ] **Step 1: Add negative module-surface tests**

```rust
#[test]
fn runtime_has_no_require_or_dynamic_module_loader() {
    assert_eq!(run("typeof require"), value("undefined"));
    assert!(run("import('file:///tmp/native.so')").is_error());
}
```

Also verify an `import` declaration cannot be evaluated through the script entrypoint and no loader is configured.

- [ ] **Step 2: Run the test against the current dependency**

Run:

```bash
cargo test --locked --features js runtime_has_no_require_or_dynamic_module_loader
```

Expected: PASS before dependency minimization, proving the current runtime path does not install a loader.

- [ ] **Step 3: Replace `full` with the exact minimal feature set**

Start with `default-features = false, features = ["std"]`. Add another feature only when a concrete compiler error names an API that is required by existing production behavior.

- [ ] **Step 4: Verify dependency features and behavior**

Run:

```bash
cargo tree -e features -i rquickjs
cargo tree -e features -i rquickjs-core
cargo test --locked --features js
cargo test --locked --features js,skills
```

Expected: neither tree contains `loader`, `dyn-load`, `macro`, `chrono`, `either`, `indexmap`, or `phf`; both test suites pass.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/extras/js/tests/mod.rs
git commit -m "build: minimize QuickJS feature surface" \
  -m "Coauthored by Seb and Claude"
```

### Task 5: Implement the bounded alternating wire protocol

**Files:**
- Create: `src/extras/js/protocol.rs`
- Create: `src/extras/js/tests/worker_protocol.rs`
- Modify: `src/extras/js/mod.rs`
- Modify: `src/extras/js/tests/mod.rs`

**Interfaces:**
- Produces:

```rust
pub(crate) const PROTOCOL_VERSION: u16 = 1;
pub(crate) const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_EFFECTS_PER_STEP: u32 = 256;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct InvocationId(String);

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub(crate) struct GrantId(uuid::Uuid);

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum ParentFrame {
    Hello(ParentHello),
    RunStep(RunStep),
    VerifyArtifact(VerifyArtifact),
    EffectResponse(EffectResponse),
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum WorkerFrame {
    Ready(WorkerReady),
    EffectRequest(EffectRequest),
    StepResult(StepResult),
    VerificationResult(VerificationResult),
    ProtocolFault(ProtocolFault),
}
```

Every frame contains protocol version, build identity, invocation ID when applicable, and a `u64` sequence. `EffectRequest` carries one closed typed operation and `GrantId`; artifact/export identity is advisory metadata only.

- [ ] **Step 1: Write hostile codec and state-machine tests**

Cover zero-length, oversized, truncated header/body, invalid JSON, unknown version/build, sequence replay/gap/wrap, wrong invocation, wrong direction, duplicate terminal result, result before run, more than 256 effects, oversized nested strings, and EOF at every byte offset. Add complete transition-table tests for both `RunStep` and `VerifyArtifact`.

- [ ] **Step 2: Run tests and observe missing types**

Run:

```bash
cargo test --locked --features js worker_protocol
```

Expected: FAIL because `protocol` does not exist.

- [ ] **Step 3: Implement codec and explicit protocol states**

```rust
enum ParentState {
    AwaitReady,
    Idle,
    AwaitWorker { invocation: InvocationId, next_effect: u32 },
    AwaitEffectResponseSent { invocation: InvocationId, effect: u32 },
    Closed,
}
enum WorkerState {
    AwaitHello,
    Idle,
    Running { invocation: InvocationId, next_effect: u32 },
    AwaitParentEffect { invocation: InvocationId, effect: u32 },
    Closed,
}
```

Use `read_exact` for the header/body, reject before allocation when length exceeds 8 MiB, serialize to a bounded `Vec`, and never log payload bodies. Exactly one effect request may await its matching response; no second effect or terminal result may be emitted meanwhile. The worker suspends JS/job draining while waiting. Per-invocation effect ordinals are monotonic. Nested/reentrant protocol use is forbidden. A response after terminal state or for another effect kills/recycles the worker.

- [ ] **Step 4: Verify**

Run:

```bash
cargo test --locked --features js worker_protocol
cargo test --locked --features js,skills worker_protocol
```

Expected: all codec, hostile-input, and state-machine tests pass in both feature combinations.

- [ ] **Step 5: Commit**

```bash
git add src/extras/js
git commit -m "feat: add bounded JavaScript worker protocol" \
  -m "Coauthored by Seb and Claude"
```

### Task 6: Add a fail-closed worker launcher abstraction and test-only launcher

**Files:**
- Modify: `src/sandbox/worker.rs`
- Create: `src/sandbox/worker/linux.rs`
- Create: `src/sandbox/worker/macos.rs`
- Modify: `src/sandbox/worker/windows.rs`
- Modify: `src/sandbox.rs`

**Interfaces:**
- Produces:

```rust
pub(crate) enum WorkerBackend { Bubblewrap, Seatbelt, WindowsLpac }
pub(crate) enum WorkerContainmentStatus {
    Available(WorkerBackend),
    Unavailable { backend: WorkerBackend, reason: String },
}
pub(crate) struct WorkerProcess {
    pub process: platform::WorkerChild,
    pub input: std::fs::File,
    pub output: std::fs::File,
    pub stderr: std::fs::File,
    pub backend: WorkerBackend,
}
pub(crate) fn containment_status() -> WorkerContainmentStatus;
pub(crate) fn launch() -> Result<WorkerProcess, WorkerLaunchError>;
```

`platform::WorkerChild` is a target-selected owned control type with common `id`, `terminate_tree`, `wait`, and bounded `try_wait` methods. Unix may wrap `std::process::Child` plus a process-group ID; Windows owns the process and Job `OwnedHandle`s returned by direct `CreateProcessW`. Do not require conversion from direct Win32 process creation into `std::process::Child`.

- [ ] **Step 1: Write fake-launch and unavailable tests**

Production `launch()` must return `Unavailable` until the relevant real platform task lands. Under `#[cfg(test)]`, inject `TestWorkerLauncher` that starts the current executable with cleared environment, piped stdio, and no security claim.

- [ ] **Step 2: Implement the abstraction without copying the general subprocess profile**

The module may reuse trusted-backend path validation from `sandbox.rs`, but it must not call `Sandbox::wrap_command`, because that profile intentionally exposes the workspace and cache.

- [ ] **Step 3: Verify**

Run:

```bash
cargo test --locked --features js worker_launcher
cargo test --locked sandbox::
```

Expected: tests can inject the unconfined launcher; production status is unavailable until a real backend exists; existing Bash/JS-spawn sandbox tests are unchanged.

- [ ] **Step 4: Commit**

```bash
git add src/sandbox.rs src/sandbox
git commit -m "feat: define fail-closed JS worker launcher" \
  -m "Coauthored by Seb and Claude"
```

### Task 7: Implement the hidden worker bootstrap and fresh-runtime execution

**Files:**
- Create: `src/extras/js/worker.rs`
- Create: `src/extras/js/tests/worker_runtime.rs`
- Modify: `src/extras/js/tests/mod.rs` — declare `worker_runtime`
- Modify: `src/main.rs` — crate lint scope, synchronous `main`, Tokio runtime construction, and existing async `run`
- Modify: `src/extras/js/mod.rs`
- Modify: `src/extras/js/engine.rs`
- Modify: `src/extras/js/types.rs`

**Interfaces:**
- Consumes: `ParentFrame`/`WorkerFrame`, protocol codec, and `TestWorkerLauncher`.
- Produces: `pub(crate) fn maybe_run_internal_worker() -> Option<std::process::ExitCode>` and worker-owned `execute_step`.

- [ ] **Step 1: Write bootstrap isolation tests**

Spawn the test worker with canary environment variables and a workspace sentinel. Require a `Ready` frame, then run `typeof process`, `typeof require`, `typeof fetch`, and `typeof read_file`; all authority globals are absent until explicitly provisioned. Assert stderr is bounded and stdout contains only frames.

- [ ] **Step 2: Restructure `main` so worker detection precedes Tokio**

Use a reserved environment marker set only by the parent launcher plus valid inherited pipes. The worker path must reject a manually supplied marker without a valid `Hello` frame. Remove the `#[tokio::main]` entrypoint: synchronous `main` checks worker mode first; only the normal path constructs the configured Tokio runtime and calls the existing async `run()`.

- [ ] **Step 3: Move runtime ownership into the worker**

Every `RunStep` creates a fresh runtime, applies limits before evaluation, creates a context, installs only the requested worker closures, evaluates a `Value`, drains bounded jobs, converts to a serializable `StepResult`, then drops all QuickJS values/context/runtime before reading the next frame.

- [ ] **Step 4: Test state reset and limits through real pipes**

Cover success, void, syntax/stack errors, promise fulfillment/rejection, endless jobs, timeout, OOM, stack exhaustion, console bounds, and a second successful request after every nonfatal outcome.

- [ ] **Step 5: Verify**

Run:

```bash
cargo test --locked --features js worker_runtime
cargo test --locked --features js extras::js::tests::
```

Expected: protocol-based tests pass and the existing outcome behavior remains green.

- [ ] **Step 6: Commit**

```bash
git add src/main.rs src/extras/js
git commit -m "feat: run QuickJS in an internal worker process" \
  -m "Coauthored by Seb and Claude"
```

### Task 8: Implement the stateless worker supervisor and recovery contract

**Files:**
- Create: `src/extras/js/supervisor.rs`
- Modify: `src/extras/js/mod.rs`
- Extend: `src/extras/js/tests/worker_runtime.rs`

**Interfaces:**
- Produces:

```rust
#[derive(Clone)]
pub(crate) struct JsWorkerSupervisor(Arc<SupervisorInner>);
impl JsWorkerSupervisor {
    pub(crate) fn shared() -> Arc<Self>;
    pub(crate) async fn execute(
        &self,
        request: RunStep,
        effects: impl InvocationEffectHandler,
        cancellation: PermCancellation,
    ) -> Result<StepResult, WorkerError>;
    pub(crate) fn verify_blocking(
        &self,
        request: VerifyArtifact,
    ) -> Result<VerificationResult, WorkerError>;
}
```

Task 8 defines the narrow `InvocationEffectHandler` callback contract; Task 9 implements it with `InvocationBroker`. The shared instance stores only process/transport state. It must not retain `Sandbox`, `AllowConfig`, permissions, approvals, skill bundles, proposal hosts, audit handles, or grants between invocations.

- [ ] **Step 1: Write crash/cancel/restart tests**

Cover worker exit before ready, malformed ready, crash during pure JS, crash while a fake effect handler is pending, dropped caller, parent deadline during JS, stale response from an old process, and successful next call after each fault. Permission-Ask/effect cancellation belongs to Task 21 after the real broker exists.

- [ ] **Step 2: Implement one serialized transport owner**

Use an async mutex or one supervisor task so exactly one invocation owns the pipes. The parent-side 30-second deadline covers JS, effects, and Ask waits. Cancellation closes/kills the worker and cancels the current broker future; no cancellation frame is sent.

- [ ] **Step 3: Implement deterministic process teardown**

On Unix terminate the containment process group and reap it. On Windows close/terminate the Job and wait for the process handle. Bound stderr draining and never wait indefinitely in `Drop`.

- [ ] **Step 4: Verify**

Run:

```bash
cargo test --locked --features js worker_runtime
cargo test --locked --features js test_js_reply_receiver_drop_is_non_fatal
```

Expected: no test leaves a child; each recovery test proves a subsequent request succeeds.

- [ ] **Step 5: Commit**

```bash
git add src/extras/js
git commit -m "feat: supervise and recover the JavaScript worker" \
  -m "Coauthored by Seb and Claude"
```

### Task 9: Refactor host effects into a parent-side typed broker

**Files:**
- Create: `src/extras/js/broker.rs`
- Create: `src/extras/js/tests/worker_broker.rs`
- Modify: `src/extras/js/tests/mod.rs` — declare `worker_broker`
- Modify: `src/extras/js/host.rs`
- Modify: `src/extras/js/tool.rs`
- Modify: `src/extras/js/types.rs`
- Modify: `src/extras/js/skills/proposal.rs`

**Interfaces:**
- Produces `InvocationBroker`, `InvocationGrant`, `EffectOperation`, `EffectResult`, and rquickjs-independent `HostEffectError`.

```rust
pub(crate) struct InvocationGrant {
    pub grant_id: GrantId,
    pub principal: GrantPrincipal,
    pub allowed: BTreeSet<HostCapability>,
}

pub(crate) enum GrantPrincipal {
    ModelAuthored { tool_call_id: String },
    Skill { artifact_id: String, export: String, invocation_id: String },
}
```

- [ ] **Step 1: Write broker authorization tests**

For every effect, prove permission denial/Ask timeout, path/origin denial, malformed target, expired invocation, unknown/replayed grant, artifact mismatch, cancellation, and backend failure execute no effect. Also prove model grants and skill grants are distinct and parent logs identity from its grant table rather than worker metadata.

- [ ] **Step 2: Split secure effect services from QuickJS conversion**

Move path resolution, secure reads/writes, SSRF-safe fetch, bounded process execution, and proposal canonicalization into Rust functions returning typed Rust results. Worker closures only convert JS values to protocol requests and protocol results to JS values/errors.

- [ ] **Step 3: Preserve the honest-runtime manifest intersection**

The parent grants only operations declared for a skill and allowed by session policy. Document/test that this is source-level containment: the hard parent floor against a native worker compromise is session policy and the union of current-step provisioned grants.

- [ ] **Step 4: Fix structured argv permission fidelity**

Replace the lossy space join with a structured permission subject or a canonical reversible rendering. Assert `spawn("echo", ["a b"])` and `spawn("echo", ["a", "b"])` cannot share an approval.

- [ ] **Step 5: Verify**

Run:

```bash
cargo test --locked --no-default-features --features js worker_broker
cargo test --locked --no-default-features --features js js_file_host_permissions
cargo test --locked --no-default-features --features js,sandbox js_fetch
cargo test --locked --no-default-features --features js spawn
```

Expected: all broker and legacy host behavior tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/extras/js
git commit -m "refactor: broker JavaScript effects in the parent" \
  -m "Coauthored by Seb and Claude"
```

### Task 10: Add durable intent-before-effect audit

**Files:**
- Create: `src/extras/js/audit.rs`
- Extend: `src/extras/js/tests/worker_broker.rs`
- Modify: `src/extras/js/broker.rs`
- Modify: `src/paths.rs` — add the typed effect-audit artifact owner/path
- Modify: `src/tests/platform_paths_tests.rs` — extend artifact ownership, permission, and migration acceptance
- Modify: `docs/specs/platform-paths.md`

**Interfaces:**
- Produces `EffectAudit`, `EffectIntent`, `EffectCompletion`, and startup recovery of unknown outcomes.

```rust
pub(crate) enum AuditState { Intent, Completed, OutcomeUnknown }
pub(crate) struct EffectAuditRecord {
    pub effect_id: String,
    pub invocation_id: String,
    pub artifact_id: Option<String>,
    pub export: Option<String>,
    pub capability: String,
    pub normalized_target: SanitizedTarget,
    pub state: AuditState,
    pub decision: String,
    pub result_code: Option<String>,
    pub previous_hash: String,
    pub record_hash: String,
}
```

- [ ] **Step 1: Write corruption, crash-window, privacy, and failure tests**

Cover audit path permissions, truncated last frame, interior corruption, hash mismatch, failure before intent sync, parent crash after intent/before effect completion, duplicate completion, replayed effect ID, secret-bearing URLs/paths/argv, retention rotation/anchors, missing prior segments, and concurrent open.

- [ ] **Step 2: Implement a bounded private append-only log**

Use one parent-owned writer with an exclusive lock and length-prefixed canonical JSON records with a SHA-256 chain. Append and sync an authorization/intent before every brokered operation, including `read_file`. Rotation emits linked close/open anchor records, syncs the file and parent directory around create/rename, and fails closed when a required prior segment is missing or corrupt. On startup, convert unmatched intents to `OutcomeUnknown`; never claim the effect did or did not happen.

- [ ] **Step 3: Integrate broker ordering**

Required order:

```text
validate request -> authorize grant/session/target -> append+sync intent
-> execute effect -> append completion -> return response
```

Audit append/sync failure returns denial and executes nothing. Completion failure returns an operational error and preserves the durable intent for recovery.

- [ ] **Step 4: Verify**

Run:

```bash
cargo test --locked --features js js_effect_audit
cargo test --locked --features js,skills js_effect_audit
cargo test --locked platform_paths_acceptance
```

Expected: all crash/failure cases preserve truthful states and no fixture secret appears in the audit file.

- [ ] **Step 5: Commit**

```bash
git add src/extras/js docs/specs/platform-paths.md src/paths.rs
git commit -m "feat: durably audit brokered JavaScript effects" \
  -m "Coauthored by Seb and Claude"
```

### Task 11: Migrate `JsTool` from its thread to the shared supervisor

**Files:**
- Modify: `src/extras/js/tool.rs` — `JsTool` constructors, fields, `Tool::call`, and thread/channel teardown
- Modify: `src/extras/js/engine.rs` — `js_thread_main`, `run_step`, and parent runtime creation paths
- Modify: `src/agent/builder.rs` — `register_js_tool`
- Modify if explicit ownership is chosen: `src/provider.rs` — `build_agent`
- Modify if explicit ownership is chosen: `src/ui/state.rs` — `UiContext::agent_build_ctx` and `AgentBuildCtx`
- Modify: `src/extras/js/tests/mod.rs`

**Interfaces:**
- Consumes: `JsWorkerSupervisor`, `InvocationBroker`, current per-tool permission/proposal/telemetry state.
- Produces: `JsTool` with no mpsc engine channel, JS thread, join handle, or parent QuickJS runtime.

- [ ] **Step 1: Add lifecycle-count and rebuild tests**

Build and drop multiple full agents, switch models, rebuild through `AgentBuildCtx`, and run JS after each. Assert only one worker PID exists at a time, no grant/config leaks between builds, and subagents/`/btw` still omit JS as before.

- [ ] **Step 2: Replace thread send/reply with supervisor execution**

`JsTool::call` still obtains top-level `js` permission, snapshots the exact skill bundle, creates cancellation and invocation IDs, constructs a fresh parent broker/grant table, and awaits the supervisor. Proposal/admission/telemetry workers remain parent-side.

- [ ] **Step 3: Remove parent runtime creation paths**

After migration this command must find rquickjs imports only in worker/realm code and narrowly scoped tests:

```bash
rg -n "rquickjs|Runtime::new|Context::full" src/extras/js
```

- [ ] **Step 4: Verify behavioral parity**

Run:

```bash
cargo test --locked --features js
cargo test --locked --features js,skills skill_runtime_binding
cargo test --locked --features js,skills skill_event_attribution
```

Expected: the existing JS behavior suite passes through the test worker; agent rebuild tests see one stateless shared supervisor.

- [ ] **Step 5: Commit**

```bash
git add src/agent/builder.rs src/provider.rs src/ui/state.rs src/extras/js
git commit -m "refactor: route JsTool through the worker supervisor" \
  -m "Coauthored by Seb and Claude"
```

### Task 12: Enforce the proposal writer/runner split

**Files:**
- Modify: `src/extras/js/worker.rs`
- Modify: `src/extras/js/broker.rs`
- Modify: `src/extras/js/skills/proposal.rs`
- Modify: `src/extras/js/tests/propose_skill_host.rs`
- Modify: `src/extras/js/tests/skill_runtime_binding.rs`

**Interfaces:**
- Consumes: `GrantPrincipal::ModelAuthored`, parent `ProposalHost`, worker realm mode.
- Produces: model-realm-only `propose_skill`; runner realms have no lexical, global, indirect, initialization, or asynchronous path to it.

- [ ] **Step 1: Write persistence-implant regressions**

Create pure and effectful stored artifacts whose top-level source, export body, promise continuation, constructor/prototype chain, and indirect global lookup attempt `propose_skill`. Add an exact regression named `stored_skill_cannot_propose_descendant` to `src/extras/js/tests/propose_skill_host.rs`. Selection/loading must enqueue zero proposals. Model-authored code invoking the same host must enqueue exactly one bounded proposal through the parent.

- [ ] **Step 2: Register writer API only in the model realm**

The worker sends `EffectOperation::ProposeSkill` using a model-only grant. The parent canonicalizes and enqueues. Stored-skill realm construction has no proposal grant or global.

- [ ] **Step 3: Audit and verify**

Run:

```bash
cargo test --locked --features js,skills propose_skill_host
cargo test --locked --features js,skills stored_skill_cannot_propose_descendant
```

Expected: every stored-skill attempt fails without queue or audit side effects; model proposal creates one audited intent/completion.

- [ ] **Step 4: Commit**

```bash
git add src/extras/js
git commit -m "fix: separate skill writer and runner authority" \
  -m "Coauthored by Seb and Claude"
```

### Task 13: Implement private skill contexts and pure initialization

**Files:**
- Create: `src/extras/js/realm.rs`
- Modify: `src/extras/js/worker.rs`
- Modify: `src/extras/js/skills/mod.rs` — `private_skill_source` and loader-facing artifact helpers
- Modify: `src/extras/js/skills/capability.rs`
- Extend: `src/extras/js/tests/skill_realm_isolation.rs`
- Modify: `src/extras/js/tests/skill_runtime_binding.rs`

**Interfaces:**
- Consumes: successful Task 2 cross-context proof, `GrantId`, immutable `SkillArtifact`.
- Produces one model context and one private context per selected skill in the same fresh runtime, with declared callable exports bridged into the model context.

- [ ] **Step 1: Write pure-initialization and ambient-authority tests**

Top-level source must not read/write/fetch/spawn/propose, schedule jobs, mutate model globals, replace host descriptors, access another skill's globals, or export undeclared names. Selection of an effectful artifact must produce no effect audit record until an exported function is explicitly called.

- [ ] **Step 2: Implement one shared loader**

`load_artifact` validates full identity and ABI version, creates a private context, applies hardened intrinsics, evaluates source with no effect or proposal globals, rejects any pending initialization job, extracts exactly declared functions, and publishes frozen wrappers in the model context. Arguments/results cross using a bounded JSON-compatible clone contract; functions, symbols, host objects, and cyclic values are rejected.

- [ ] **Step 3: Bind grants only for explicit export invocation**

Skill ABI v2 defines every stored export as receiving an immutable hidden capability object as argument zero; model-visible wrappers hide that argument. The object contains only methods declared by the artifact manifest, and each method closes over one opaque parent-created `GrantId`. Source initialization receives no capability object and no effect global. The wrapper creates the object immediately before explicit invocation, keeps it valid through the exact returned promise's settlement, then irrevocably revokes it. Cancellation, timeout, exception, worker recycle, and nested-call completion also revoke it. Calls through retained methods after revocation return a stable denial without sending an `EffectRequest`. Multiple overlapping promises keep distinct grant IDs and cannot borrow each other's methods. Replace the process-global `active_invocations.values().next_back()` attribution with explicit invocation/grant identity.

- [ ] **Step 4: Verify**

Run:

```bash
cargo test --locked --features js,skills skill_realm_isolation
cargo test --locked --features js,skills skill_runtime_binding
cargo test --locked --features js,skills skill_event_attribution
```

Expected: initialization is pure, no effect global exists, retained methods fail after settlement, async effects retain the exact invocation, and overlapping promises do not intersect unrelated manifests.

- [ ] **Step 5: Commit**

```bash
git add src/extras/js
git commit -m "feat: isolate learned skills in private JS contexts" \
  -m "Coauthored by Seb and Claude"
```

### Task 14: Introduce structured capability manifest identity version 2

**Files:**
- Modify: `src/extras/js/skills/mod.rs` — `IDENTITY_VERSION`, ABI version, capability types, manifest, artifact, and canonical identity validation
- Modify: `src/extras/js/skills/proposal.rs`
- Modify: `src/extras/js/skills/store.rs`
- Modify: `src/extras/js/skills/visibility.rs`
- Modify: `src/extras/js/tests/skill_admission_schema.rs`
- Modify: `src/extras/js/tests/skill_runtime_binding.rs`

**Interfaces:**
- Produces:

```rust
pub enum CapabilityScope {
    ReadFile { workspace_prefixes: Vec<String> },
    WriteFile { workspace_prefixes: Vec<String> },
    Fetch { origins: Vec<String>, methods: Vec<HttpMethod> },
    Spawn { programs: Vec<String> },
}
pub struct CapabilityManifest {
    pub tier: CapabilityTier,
    pub grants: Vec<CapabilityScope>,
}
pub const IDENTITY_VERSION: u32 = 2;
pub const SKILL_ABI_VERSION: u16 = 2;
```

- [ ] **Step 1: Write canonicalization and migration tests**

Cover ordering, duplicates, `.`/`..`, absolute paths, separators, Unicode normalization, exact origins/default ports, HTTP opt-in, duplicate methods/programs, unknown fields, tier mismatch, ABI mismatch, hidden-capability-argument export validation, canonical ID changes, and a v1 store containing pure/read-only/side-effecting artifacts.

- [ ] **Step 2: Implement version-2 validation and identity**

File prefixes are portable workspace-relative normal-component paths. Fetch origins use the existing exact normalization parser. Spawn programs are exact executable names without separators; the parent separately resolves/authorizes execution. Manifest scopes can narrow session policy but never broaden it.

- [ ] **Step 3: Quarantine all v1 artifacts**

On migration/retrieval, preserve v1 rows and lineage for audit but mark them non-retrievable with reason `manifest_scope_required`. Do not infer v2 grants or mutate/re-hash source in place. Reproposal creates a new v2 artifact and passes every normal gate.

- [ ] **Step 4: Verify**

Run:

```bash
cargo test --locked --features js,skills skill_store_identity
cargo test --locked --features js,skills skill_admission_schema
cargo test --locked --features js,skills capability_manifest_v2
```

Expected: malformed scopes/ABI fail identity validation; all v1 fixtures remain stored but none is retrievable.

- [ ] **Step 5: Commit**

```bash
git add src/extras/js/skills src/extras/js/tests
git commit -m "feat: scope learned-skill capabilities in identity v2" \
  -m "Coauthored by Seb and Claude"
```

### Task 15: Enforce structured scopes in broker grants

**Files:**
- Modify: `src/extras/js/broker.rs`
- Modify: `src/extras/js/host.rs`
- Extend: `src/extras/js/tests/worker_broker.rs`
- Modify: `src/extras/js/tests/skill_runtime_binding.rs`

**Interfaces:**
- Consumes: version-2 `CapabilityManifest`, session `AllowConfig`, permission service.
- Produces target-specific intersection enforcement for each skill grant.

- [ ] **Step 1: Write cross-product tests**

For each operation, test manifest allow/session allow, manifest deny/session allow, manifest allow/session deny, malformed target, scope sibling-prefix, symlink/race, redirected origin, method mismatch, program mismatch, and model-authored session-only behavior.

- [ ] **Step 2: Implement deterministic intersection**

Normalize once using the existing secure path/URL/program parser, then evaluate manifest scope, session narrowing policy, mandatory permission, audit intent, and effect in that order. Return a typed denial reason identifying the layer without exposing secret target content.

- [ ] **Step 3: Verify**

Run:

```bash
cargo test --locked --features js,skills scoped_capability_intersection
cargo test --locked --features js,skills skill_runtime_binding
cargo test --locked --features js,sandbox js_fetch
```

Expected: no layer broadens another and every denial executes no effect.

- [ ] **Step 4: Commit**

```bash
git add src/extras/js
git commit -m "feat: enforce scoped skill grants in the broker" \
  -m "Coauthored by Seb and Claude"
```

### Task 16: Move verification and held-out evaluation onto the production loader contract

**Files:**
- Modify: `src/extras/js/skills/verify.rs`
- Modify: `src/extras/js/skills/held_out.rs`
- Modify: `src/extras/js/skills/admission.rs`
- Modify: `src/extras/js/worker.rs`
- Modify: `src/extras/js/realm.rs`
- Modify: `src/extras/js/protocol.rs`
- Modify: `src/extras/js/tests/skill_held_out_evaluator.rs`
- Modify: `src/extras/js/tests/skill_admission_gate.rs`

**Interfaces:**
- Consumes: `VerifyArtifact` wire request, exact `load_artifact`, deterministic fake fixtures.
- Produces a worker `VerificationResult` containing bounded embedded/inherited/held-out outcomes, mutation results, fake transcript, resource classification, and loader version; parent computes/persists report identity. This guarantees loader/realm-contract parity, not real-I/O, permission-wait, audit, cancellation-timing, or latency parity.

- [ ] **Step 1: Add production/verifier differential tests**

Create artifacts that behave differently under raw-source versus private-wrapper evaluation: ambient globals, top-level effects, undeclared exports, async export, fake host access, constructor escape, and line-number/stack behavior. Require admission and production loaders to make identical contract decisions.

- [ ] **Step 2: Define one verification request lifetime**

One complete skill verification request owns one fresh Runtime and creates a fresh Context for every embedded, mutation, inherited, and held-out case. Every case begins and ends with zero pending jobs and its own fake transcript; any pending job after a case fails that case and prevents state from reaching another context. The worker receives artifact plus bounded Rust-owned fixtures; it never opens SQLite or fixture files.

- [ ] **Step 3: Replace parent rquickjs verification**

`verify_skill` and `verify_held_out_case` become typed supervisor clients. Remove direct `Runtime::new`, `Context::full`, and raw `ctx.eval(skill.source)` from parent verification modules.

- [ ] **Step 4: Prevent background verifier starvation**

Use a low-priority bounded verification queue. Interactive `RunStep` requests take priority between complete verification requests; a running verification remains atomic. Queue overflow is a retryable admission infrastructure error, not a reason to execute in-process.

- [ ] **Step 5: Verify**

Run:

```bash
cargo test --locked --features js,skills skill_held_out_evaluator
cargo test --locked --features js,skills skill_admission_gate
cargo test --locked --features js,skills auto_admission_end_to_end
rg -n "Runtime::new|Context::full|ctx\.eval.*skill\.source" src/extras/js/skills
```

Expected: all evaluation tests pass; the final search finds no parent verification runtime or raw-source execution.

- [ ] **Step 6: Commit**

```bash
git add src/extras/js
git commit -m "refactor: verify skills through the production worker loader" \
  -m "Coauthored by Seb and Claude"
```

### Task 17: Replace caller-asserted approvals with one-time parent authorization

**Files:**
- Modify: `src/extras/js/skills/admission.rs`
- Modify: `src/extras/js/skills/admission_store.rs`
- Modify: `src/extras/js/skills/lifecycle.rs`
- Modify: `src/extras/js/skills/store.rs`
- Modify: `src/extras/js/tests/skill_admission_gate.rs`
- Modify: `src/extras/js/tests/skill_lifecycle_schema.rs`
- Modify: `src/extras/js/tests/skill_repair_and_rollback.rs`
- Modify as needed: `src/extras/js/tests/self_learning_end_to_end.rs`

**Interfaces:**
- Produces a private `ApprovalAuthorization` created only by a parent-owned authenticated interaction and consumed transactionally once.

```rust
pub(crate) struct ApprovalAuthorization {
    authorization_id: String,
    principal: String,
    artifact_id: String,
    report_id: String,
    manifest_digest: String,
    transition: ApprovalTransition,
    issued_at: i64,
    expires_at: i64,
}
```

- [ ] **Step 1: Write forgery/replay/staleness tests**

Add exact tests prefixed `authenticated_approval_authorization_`. Reject arbitrary principal/timestamp construction, wrong artifact/report/manifest/transition, expiry, reused authorization, stale row version, first-approval token reused for root activation, and transaction failure after token consumption.

- [ ] **Step 2: Make construction private and consumption transactional**

Persist the authorization before review completion, bind it to the exact review packet digest, and consume it in the same `BEGIN IMMEDIATE` transaction as canary/activation. Rollback must leave both transition and token consumption unchanged.

- [ ] **Step 3: Verify**

Run:

```bash
cargo test --locked --features js,skills skill_admission_gate
cargo test --locked --features js,skills authenticated_approval_authorization
```

Expected: only exact, fresh, one-time authorizations transition lifecycle state.

- [ ] **Step 4: Commit**

```bash
git add src/extras/js/skills src/extras/js/tests
git commit -m "fix: bind skill approval to one-time authorization" \
  -m "Coauthored by Seb and Claude"
```

### Task 18: Implement the Linux broker-only bubblewrap launcher

**Files:**
- Modify: `src/sandbox/worker/linux.rs`
- Modify: `src/sandbox/worker.rs`
- Create/extend: `src/extras/js/tests/worker_containment.rs`
- Modify: `src/extras/js/tests/mod.rs` — declare target-gated `worker_containment`
- Modify: `Cargo.toml` and `Cargo.lock` — add only a target-specific safe seccomp/rlimit dependency if standard APIs are insufficient
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: trusted bwrap path validation and current executable path.
- Produces `WorkerBackend::Bubblewrap` with no workspace/cache/network/device/environment authority, no process creation after readiness, and enforced OS resource ceilings.

- [ ] **Step 1: Write real-backend capability probes**

The worker must fail to read workspace and skill DB sentinels, write anywhere except an internal private temp if required, inspect credential canary environment, connect TCP/UDP/Unix sockets, access host devices, fork/clone/exec after readiness, exceed native memory/CPU/file-descriptor limits, or create a surviving child. It must still load the current executable, system runtime libraries, QuickJS, and protocol pipes.

- [ ] **Step 2: Build the minimal bwrap root**

Use new user/PID/network/IPC/UTS/cgroup namespaces where supported, `--die-with-parent`, `--new-session`, `--clearenv`, synthetic `/dev`, fresh `/proc`, private `/tmp`, read-only exact executable/system runtime mounts, and no workspace/application-cache bind. Before reading untrusted frames, the worker applies `no_new_privs`, a seccomp policy denying `fork`, `vfork`, `clone`, `clone3`, `execve`, and `execveat`, and validated address-space/CPU/file-descriptor/core/file-size rlimits. The initial targets are a 256 MiB process address-space ceiling and 35-second CPU ceiling; Task 23 records any reviewed platform adjustment before release. Backend, seccomp, or required-limit setup failure runs no worker and is never retried outside bwrap. Cgroup v2 remains optional defense-in-depth, not a prerequisite.

- [ ] **Step 3: Verify locally and in CI**

Run:

```bash
cargo test --locked --no-default-features --features js linux_js_worker_containment -- --ignored --nocapture
cargo test --locked --no-default-features --features js worker_runtime
```

Run only on Linux with bubblewrap installed; the target-specific CI job must fail rather than accept a zero-test result. Expected: containment and native resource probes pass; unavailable bwrap/seccomp/required limits report JS unavailable.

- [ ] **Step 4: Commit**

```bash
git add src/sandbox src/extras/js/tests .github/workflows/ci.yml
git commit -m "feat: confine the JS worker with bubblewrap" \
  -m "Coauthored by Seb and Claude"
```

### Task 19: Implement the macOS broker-only Seatbelt launcher

**Files:**
- Modify: `src/sandbox/worker/macos.rs`
- Modify: `src/sandbox/worker.rs`
- Extend: `src/extras/js/tests/worker_containment.rs`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Produces `WorkerBackend::Seatbelt` with a deny-default profile, explicit descriptor closure, process-group teardown, validated rlimits where effective, and explicit deprecated/weaker status. It is not claimed equivalent to Linux namespaces/seccomp or Windows LPAC/Job containment.

- [ ] **Step 1: Write real-backend capability probes**

Probe workspace/skill DB reads and writes, environment canaries, outbound/listening network, process creation, inherited file descriptors, process-group cleanup, address-space/CPU limits where the OS version enforces them, and an unknown/unvalidated macOS-version status. Also prove the profile permits the exact executable, dyld/system libraries, protocol pipes, and bounded stderr.

- [ ] **Step 2: Implement a dedicated profile**

Do not reuse the general subprocess profile that allows host-readable files. Deny by default; allow only system/runtime reads required to load, exact executable execution, inherited protocol descriptors, and the minimum process/mach services proven necessary by the probe. Use `/usr/bin/env -i` or equivalent; close every non-protocol descriptor before exec; establish a new process group; apply address-space/CPU/file-descriptor/core/file-size rlimits before untrusted work; and deny process fork/exec in the profile. The parent kills the whole group on any fault.

- [ ] **Step 3: Surface limitations**

Status must state `sandbox-exec` is an undocumented/deprecated best-effort MAC policy and not a complete native-compromise boundary. Validate the exact profile and limit probes for each supported macOS major version; unknown or failed versions disable JS rather than run unconfined.

- [ ] **Step 4: Verify**

Run:

```bash
cargo test --locked --no-default-features --features js macos_js_worker_containment -- --ignored --nocapture
cargo test --locked --no-default-features --features js worker_runtime
```

Run only on macOS with `/usr/bin/sandbox-exec`; the target-specific CI job must fail rather than accept a zero-test result. Expected: probes pass on an explicitly supported macOS major version; missing/profile-rejected/unvalidated Seatbelt disables JS.

- [ ] **Step 5: Commit**

```bash
git add src/sandbox src/extras/js/tests .github/workflows/ci.yml
git commit -m "feat: confine the JS worker with Seatbelt" \
  -m "Coauthored by Seb and Claude"
```

### Task 20: Implement the Windows LPAC/AppContainer and Job launcher

**Files:**
- Modify: `src/sandbox/worker/windows.rs`
- Modify: `src/sandbox/worker.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Extend: `src/extras/js/tests/worker_containment.rs`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: successful Task 3 image-load gate.
- Produces `WorkerBackend::WindowsLpac` using zero capabilities, creation-time Job assignment, exact pipe handle inheritance, and compatible mitigations.

- [ ] **Step 1: Add Windows containment tests**

Prove no workspace/skill DB read or write, no credential environment, no network, no child process, no broad inherited standard/file/socket handles, kill-on-parent/job-close, active-process limit one, and a 256 MiB process-memory limit. Test under a parent that is already in a Job and record the minimum supported Windows version/nested-Job behavior.

- [ ] **Step 2: Implement race-free process creation**

Create/configure the Job before process creation, including kill-on-close, active-process one, CPU/time, memory, and UI restrictions. Build one `STARTUPINFOEX` attribute list containing `SECURITY_CAPABILITIES`, LPAC all-application-packages opt-out, `JOB_LIST`, `CHILD_PROCESS_POLICY`, compatible `MITIGATION_POLICY`, and `HANDLE_LIST`. Mark only the child pipe endpoints inheritable, pass `bInheritHandles = TRUE`, include exactly those handles, then immediately clear parent-side inheritance. Use `EXTENDED_STARTUPINFO_PRESENT`; the Job is effective before the initial thread runs. Own and close every `PROCESS_INFORMATION`, pipe, attribute-list, AppContainer SID, process, thread, and Job resource on every success/failure path. If nested Job assignment or any required attribute is unsupported/rejected, fail closed.

- [ ] **Step 3: Apply conservative mitigations**

Enable heap termination, ASLR, extension-point disable, remote/low-label image-load denial, and prefer-System32 when verified. Add Win32k or dynamic-code prohibition only if the exact release binary passes a dedicated launch/evaluation test.

- [ ] **Step 4: Preserve the scope fence**

This launcher confines only the broker-only QuickJS worker. It does not satisfy `mini-agent-uq5c`; parent-brokered `spawn` remains denied on Windows.

- [ ] **Step 5: Verify on Windows**

Run:

```powershell
cargo test --locked --no-default-features --features js windows_js_worker_containment -- --ignored --nocapture
cargo test --locked --no-default-features --features js worker_runtime
cargo install --locked --path . --debug
& (Join-Path $env:CARGO_HOME 'bin/mini-agent.exe') --print-config
```

Expected: worker status is available only when every containment attribute applies; all probes pass and no spawned descendant survives.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/sandbox src/extras/js/tests .github/workflows/ci.yml
git commit -m "feat: confine the Windows JS worker with LPAC" \
  -m "Coauthored by Seb and Claude"
```

### Task 21: Finalize cancellation, recycling, and fault classification

**Files:**
- Modify: `src/extras/js/supervisor.rs`
- Modify: `src/extras/js/worker.rs`
- Modify: `src/extras/js/protocol.rs`
- Extend: `src/extras/js/tests/worker_runtime.rs`
- Extend: `src/extras/js/tests/worker_broker.rs`

**Interfaces:**
- Produces exhaustive `WorkerError`/`JsOutcome` mapping and recycle policy.

- [ ] **Step 1: Write the fault matrix as tests**

Cover normal error, JS timeout, QuickJS OOM, protocol fault, worker panic, signal/abnormal exit, OS memory kill, audit failure, effect completion unknown, caller cancellation during JS/effect/Ask, and clean shutdown. Each test asserts user outcome, audit state, process recycle decision, and successful next invocation.

- [ ] **Step 2: Implement recycling policy**

Always recycle after timeout, OOM, malformed protocol, abnormal exit, cancellation, stuck/failed effect response, build mismatch, and configurable maximum process age/call count. Ordinary JS exceptions and successful calls reuse the process but never the Runtime.

- [ ] **Step 3: Bound parent effects on cancellation**

Cancel permission Ask, fetch, file work before mutation where possible, and the complete spawned command process group/tree. If an effect may have occurred after durable intent, record `OutcomeUnknown` rather than a false failure/success.

- [ ] **Step 4: Verify**

Run:

```bash
cargo test --locked --features js worker_fault_matrix
cargo test --locked --features js,skills worker_fault_matrix
```

Expected: every row passes and leaves no child/process/grant active.

- [ ] **Step 5: Commit**

```bash
git add src/extras/js
git commit -m "fix: make JS worker failure recovery exhaustive" \
  -m "Coauthored by Seb and Claude"
```

### Task 22: Roll out fail-closed availability and truthful status

**Files:**
- Modify: `src/agent/builder.rs`
- Modify: `src/cli.rs` — sandbox/internal-worker/print-config fields
- Modify: `src/startup.rs` — startup feature initialization and JS availability
- Modify: `src/print.rs` — `print_config` and sandbox/feature reporting
- Modify: `src/config/mod.rs` and/or `src/config/types.rs` — whichever type owns persisted JS policy
- Modify: `src/extras/js/tool.rs`
- Modify: `src/sandbox/worker.rs`
- Modify: tests colocated with those modules

**Interfaces:**
- Produces separate user-visible statuses for general subprocess sandbox and JS worker containment.

- [ ] **Step 1: Write configuration/status tests**

Cover supported backend, missing backend, backend setup failure, `--no-sandbox`, default sandbox degradation for general commands, manual internal-worker flag, Windows worker available but spawn unavailable, and builds without `js`. A sentinel proves unavailable worker status registers or calls no JS tool.

- [ ] **Step 2: Implement strict JS availability**

General subprocess `--no-sandbox` does not authorize an uncontained QuickJS worker. When containment is unavailable, either omit the JS tool or return one stable unavailable diagnostic without launching. Never call the old in-process engine.

- [ ] **Step 3: Extend `--print-config`**

Add a `JavaScript worker` section with compiled status, backend, containment status/reason, process policy, network/filesystem/process authority claims, protocol version, and Windows spawn support. Do not infer capability from Cargo features alone.

- [ ] **Step 4: Verify installed binary**

Run:

```bash
cargo test --locked
cargo test --locked --no-default-features
cargo test --locked --no-default-features --features js
cargo test --locked --no-default-features --features skills
cargo install --locked --path . --debug
mini-agent --print-config
mini-agent -p "use JavaScript to compute 6 * 7 and return only the number"
```

Expected: config truthfully reports the real backend; the smoke returns `42` when contained or a stable JS-unavailable diagnostic when no backend exists. Provider connectivity smoke may be recorded blocked if credentials/network are unavailable.

- [ ] **Step 5: Commit**

```bash
git add src/agent/builder.rs src/cli.rs src/startup.rs src/print.rs src/config src/extras/js src/sandbox
git commit -m "feat: fail closed when JS worker containment is unavailable" \
  -m "Coauthored by Seb and Claude"
```

### Task 23: Add adversarial cross-platform CI and measured resource evidence

**Files:**
- Modify: `.github/workflows/ci.yml`
- Create: `docs/benchmarks/js-worker.md`
- Create: `docs/benchmarks/results/js-worker-baseline.json`
- Add or modify benchmark/test modules under `src/extras/js/tests/`

**Interfaces:**
- Produces required machine-readable Linux/macOS/Windows containment and resource artifacts.

- [ ] **Step 1: Add required CI rows**

Linux installs bwrap and runs real containment. macOS runs the real Seatbelt probe. Windows runs LPAC/Job/handle probes. All platforms run protocol fuzz-like hostile cases, runtime parity, crash recovery, realm isolation, verifier parity, and status tests under `js` and `js,skills` where dependencies support them.

- [ ] **Step 2: Add resource measurements**

First record the reference host, OS/kernel, CPU, RAM, debug binary profile, sample count (at least 100 after 10 warmups), percentile calculation, and variance. Then measure:

```text
cold worker Ready p95: Linux <= 250 ms, macOS <= 300 ms, Windows <= 750 ms
warm pure-expression p95 added overhead: <= 10 ms
4 KiB brokered read p95 IPC overhead excluding permission wait: <= 10 ms
idle worker private RSS: <= 32 MiB on the documented reference hosts
post-cancel successful trivial call: <= 1 second
steady call count: one worker process, zero retained QuickJS runtimes while idle
```

These are provisional performance targets, not security limits. The 256 MiB native-process and 35-second CPU ceilings from Tasks 18–20 (or a reviewed, measured replacement documented for an individual platform) are enforced security gates. If a reference host misses a performance target, optimize first; otherwise amend the target only with captured evidence and rationale. Do not use timing-sensitive shared-runner p95 values as flaky CI assertions: CI gates functionality and uploads measurements for regression review.

- [ ] **Step 3: Run the full local quality gate**

Run:

```bash
cargo fmt --check
cargo test --locked
cargo test --locked --no-default-features
cargo test --locked --no-default-features --features js
cargo test --locked --no-default-features --features skills
cargo test --locked --no-default-features --features js,sandbox
cargo test --locked --no-default-features --features mcp,js,skills
cargo install --locked --path . --debug
```

Expected: all commands pass. Do not substitute `cargo build` or `cargo check`.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/ci.yml docs/benchmarks src/extras/js/tests
git commit -m "ci: gate brokered JS runtime across platforms" \
  -m "Coauthored by Seb and Claude"
```

### Task 24: Reconcile all overview and normative documentation

**Files:**
- Modify: `docs/specs/00-index.md`
- Modify: `docs/specs/phase-1-js-engine.md`
- Modify: `docs/specs/phase-2-sandbox.md`
- Modify: `docs/specs/phase-3-skill-library.md`
- Modify: `docs/specs/phase-4-auto-admission.md`
- Modify: `docs/specs/phase-5-evidence-learning.md`
- Modify: `docs/specs/phase-6-brokered-js-runtime.md`
- Modify: `ARCHITECTURE.md`
- Modify: `SPEC.md`
- Modify: `README.md`
- Modify any affected user docs under `docs/`

**Interfaces:**
- Consumes: completed implementation and CI/resource evidence.
- Produces one conflict-free description of actual Linux/macOS/Windows guarantees and residual risks.

- [ ] **Step 1: Update delivery statuses and authority map**

Mark Phase 6 delivered only after every direct implementation Bead and platform gate is closed. Reconcile the stale Phase 5 status in `00-index.md`. Preserve historical rationale while removing current-tense in-process claims.

- [ ] **Step 2: Document residual risks explicitly**

Include native-worker access to the union of current-step grants, macOS Seatbelt deprecation/asymmetry, platform-backend absence behavior, Windows general-spawn separation, audit `OutcomeUnknown`, and the fact that hooks/MCP/LSP/loop/interactive shell have separate trust semantics.

- [ ] **Step 3: Scan for contradictions and stale paths**

Run:

```bash
rg -n "in-process|one dedicated OS thread|warm Runtime|Windows.*unsandboxed|bwrap.*workspace|propose_skill" \
  README.md ARCHITECTURE.md SPEC.md docs
rg -n "zerostack/" README.md ARCHITECTURE.md SPEC.md docs/specs
```

Expected: matches are historical warnings or accurate current statements; no production path points to the old `zerostack/` layout.

- [ ] **Step 4: Final verification**

Run:

```bash
cargo fmt --check
cargo test --locked --no-default-features --features js
cargo test --locked --no-default-features --features skills
cargo install --locked --path . --debug
git status --short
```

Expected: tests/install pass and only intended documentation/code changes are present.

- [ ] **Step 5: Commit**

```bash
git add README.md ARCHITECTURE.md SPEC.md docs
git commit -m "docs: reconcile brokered JS runtime guarantees" \
  -m "Coauthored by Seb and Claude"
```

---

## Atomic Bead Decomposition

The Tasks above are readable implementation workstreams. File and execute them as the following 33 atomic Beads so a single worker can finish, verify, and commit one responsibility without mixing security boundaries:

Filed epic: `mini-agent-xic0`. Atomic labels A01–A33 map directly to hierarchical IDs `mini-agent-xic0.1` through `mini-agent-xic0.33`.

| Bead | Source | Atomic outcome |
|---|---|---|
| A01 | Task 1 | Normative Phase 6 contract and supersession map |
| A02 | Task 2 | QuickJS cross-context feasibility gate |
| A03 | Task 3 | Windows LPAC image-loading/ACL feasibility gate |
| A04 | Task 4 | Minimal rquickjs feature surface |
| A05 | Task 5 | Bounded frame codec and complete protocol transition table |
| A06 | Task 6 | Platform-neutral fail-closed launcher interface and test launcher |
| A07 | Task 7 steps 1–2 | Pre-Clap/pre-Tokio authenticated internal-worker bootstrap |
| A08 | Task 7 steps 3–5 | Fresh-runtime execution, outcomes, jobs, console, and limits |
| A09 | Task 8 interface/step 2 | One serialized supervisor transport owner using `InvocationEffectHandler` |
| A10 | Task 8 steps 1,3–4 | Basic watchdog, crash/cancel teardown, and clean restart |
| A11 | Task 9 steps 1,3 | Parent grant table and typed broker authorization |
| A12 | Task 9 steps 2,4–5 | rquickjs-independent effect services; absorbs `mini-agent-04n3`, links `mini-agent-6qry` |
| A13 | Task 10 steps 1–2 | Private hash-chained audit storage, recovery, rotation, and privacy |
| A14 | Task 10 steps 3–4 | Broker intent-before-every-effect integration and failure semantics |
| A15 | Task 11 | `JsTool`/builder migration and one-supervisor lifecycle |
| A16 | Task 13 steps 1–2 | Private realm loader, pure initialization, JSON clone boundary |
| A17 | Task 13 steps 3–4 | ABI-v2 hidden invocation capability lifetime and async attribution |
| A18 | Task 12 | Model writer versus persisted runner split |
| A19 | Task 14 | Manifest/ABI identity v2 and v1 quarantine migration |
| A20 | Task 15 | Scoped manifest/session/permission/audit intersection |
| A21 | Task 16 steps 1–3,5 | Production loader-contract verification migration |
| A22 | Task 16 step 4 | Bounded interactive-priority verification scheduling |
| A23 | Task 17 | One-time parent-owned approval authorization |
| A24 | Task 18 | Linux bwrap/seccomp/rlimit backend and probes |
| A25 | Task 19 | macOS Seatbelt/descriptor/rlimit backend and version probes |
| A26 | Task 20 steps 2–4 | Windows LPAC/Job/handle-list launcher |
| A27 | Task 20 steps 1,5 | Windows containment/resource/install-location probe matrix |
| A28 | Task 21 steps 1–2,4 | Exhaustive fault classification and recycle matrix |
| A29 | Task 21 step 3 | Parent-effect cancellation and truthful unknown outcomes |
| A30 | Task 22 | Fail-closed registration, config, and status rollout |
| A31 | Task 23 steps 1,3 | Cross-platform adversarial CI matrix and artifacts |
| A32 | Task 23 step 2 | Reproducible resource baseline and reviewed targets |
| A33 | Task 24 | Final normative/overview reconciliation and delivery gate |

Every filed Bead must repeat: the invariant it protects; exact owned files/symbols; consumed/produced interface; failing tests first; implementation steps; focused commands and expected result; acceptance criteria; migration/failure behavior; and task-specific non-goals. It links to A01 and this plan. A research-gate Bead may close with a negative result only as `design blocked`; its consumers remain blocked and may not weaken the contract.

## Corrected Dependency Order

`X -> Y` means Y is blocked by X.

```text
A01 -> A02,A03,A04,A05,A06,A19
A04,A05,A06,A07 -> A08
A05,A06 -> A07
A05,A06,A08 -> A09 -> A10
A05,A08,A09 -> A11
A11,mini-agent-6qry -> A12 (absorbs the related `mini-agent-04n3` acceptance)
A11 -> A13 -> A14
A09,A10,A11,A12,A14 -> A15

A02,A05,A08,A11,A19 -> A16 -> A17
A12,A14,A17 -> A18
A11,A14,A19 -> A20
A05,A09,A16,A17,A19 -> A21 -> A22
A19,A21 -> A23

A05,A06,A07,A08 -> A24
A05,A06,A07,A08 -> A25
A03,A05,A06,A07,A08 -> A26 -> A27
A10,A11,A14,A24,A25,A27 -> A28
A10,A12,A14,A24,A25,A27 -> A29

A15,A17,A18,A20,A21,A22,A23,A24,A25,A27,A28,A29 -> A30
A02,A03,A04,A05,A10,A14,A17,A18,A20,A21,A22,A23,A24,A25,A27,A28,A29,A30,mini-agent-gqq,mini-agent-jygu -> A31
A24,A25,A27,A28,A30 -> A32
A01..A32 -> A33
```

A24, A25, and A26 can run in parallel after their common prerequisites. A23 can run in parallel with worker/platform implementation after A19. A13 can begin after the broker interface exists, but A14 blocks every production migration. No Bead may bypass A02 or A03 by weakening the approved design.

## Out-of-Scope but Required Follow-up Tracking

The Phase 6 worker does not make all mini-agent subprocesses sandboxed. Related non-blocking epic `mini-agent-7r1a` classifies and hardens these separately; its F01–F05 atomic children are `mini-agent-7r1a.1` through `.5`:

- hooks in `src/extras/hooks/subprocess.rs`;
- MCP stdio in `src/extras/mcp/client.rs`;
- LSP launch in `src/extras/lsp/client.rs`;
- loop validation in `src/extras/loop/headless.rs` and `src/ui/event_handler.rs` (reuse existing `mini-agent-qmrn`);
- interactive/print `!` shell paths in `src/ui/app.rs` and `src/startup.rs`;
- general Windows command containment (reuse existing `mini-agent-uq5c`).

That epic must classify user-trusted, project-config-trusted, and model-generated commands separately rather than forcing every subprocess into the broker-only worker profile.

Existing issue reconciliation:

- `mini-agent-04n3` argv approval fidelity is absorbed by A12 and linked rather than duplicated.
- `mini-agent-6qry` remains the canonical fetch outer-timeout issue and blocks A12.
- `mini-agent-gqq` and `mini-agent-jygu` remain canonical feature/CI gates and block A31.
- `mini-agent-uq5c` remains the general workspace-capable Windows sandbox; it is related to A26/A27/A30 but does not block broker-only Windows JS.
- `mini-agent-z2mh` remains the general sandbox/default-on documentation issue and is related to A33.
- `mini-agent-n6ct` remains a separate imported-Agent-Skill prompt-boundary vulnerability and is not duplicated here.

## Self-Review Results

- **Spec coverage:** The plan covers native isolation, capability brokerage, persistence, approval, verification, effect audit, Linux/macOS/Windows backstops, failure policy, resource footprint, rollout, CI, and documentation. General non-JS subprocesses are explicitly tracked separately.
- **Placeholder scan:** No `TBD`, `TODO`, unspecified “appropriate handling,” or unnamed test steps remain. The two research uncertainties are explicit fail-closed delivery gates with no weaker fallback.
- **Type consistency:** `InvocationId`, `GrantId`, frame enums, `JsWorkerSupervisor`, `InvocationBroker`, `CapabilityManifest` v2, worker launch types, and audit types have one producing task and are consumed only after that task in the dependency graph.
