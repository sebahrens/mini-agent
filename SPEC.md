# JavaScript Runtime — Implementation Overview

- **Document role**: non-normative implementation overview
- **Overview version**: 2.0.0
- **Delivery status**: Phase 6 delivered
- **Owner**: mini-agent maintainers
- **Last reconciled**: 2026-09-05

The sole normative JS corpus is
[`docs/specs/00-index.md`](docs/specs/00-index.md) and the specifications it indexes. This file maps
the current implementation; it cannot override those contracts.

## Pending amendments

See `docs/specs/00-index.md` → *Accepted amendments pending delivery (2026-09-05)* and
[the review plan](docs/plans/2026-09-05-001-harness-design-review.md). Current model-facing
limits worth knowing before they land: model script runs in strict script mode without top-level
`await`; all JavaScript failures render as `exception`; only `read_file`, `write_file`, `fetch`,
and (Linux/Windows) `spawn` effects exist; `propose_skill` is not registered.

## Foundation — paths and persistence

Normative specification: [`platform-paths.md`](docs/specs/platform-paths.md)

- Construct one typed `AppPaths` resolver at startup.
- Keep configuration, portable data, machine-local data, state, cache, credentials, and project
  roots distinct.
- Keep the learned-skill database and lifecycle evidence in machine-local data and the brokered
  effect audit in private state storage.
- Validate Agent Skill directories/ZIPs without execution; `allowed-tools` is metadata only.
- Keep platform storage/security claims qualified by their resolver, ACL, migration, archive, CI,
  and release gates.

## Phase 1 — preserved engine semantics

Normative specification: [`phase-1-js-engine.md`](docs/specs/phase-1-js-engine.md)

Phase 1's in-parent thread is historical. Phase 6 preserves these behavior requirements inside the
contained worker:

- one fresh runtime per step, a 64 MiB heap, 512 KiB JS stack, pre-eval interrupt deadline,
  bounded pending-job drain, and bounded values/output;
- no `require`, `import`, or `final_answer`;
- secure path identity and mandatory permission for real file operations;
- stable typed outcomes; and
- only closed diagnostic class/code plus validated source-free location metadata.

`src/extras/js/engine.rs` is test-only. Production QuickJS ownership is in `worker.rs` and
`realm.rs`; no QuickJS type exists in the parent `JsTool` or supervisor.

## Phase 2 — parent effect narrowing and general commands

Normative specification: [`phase-2-sandbox.md`](docs/specs/phase-2-sandbox.md)

- `fetch` remains HTTP(S)-only, origin/address/redirect validated, permission-gated, bounded, and
  deadline-limited, but now executes in the parent.
- File allow-lists narrow the exact securely resolved target and never grant permission.
- A parent-brokered model command reaches `Sandbox::wrap_command` with structural argv identity.
- The Linux/macOS/Windows general-process profiles remain workspace-visible and must never launch
  the JS worker.
- Windows model-authored `spawn` uses the separately attested regular-AppContainer command
  backend. LPAC worker containment never authorizes that command; learned-skill spawn remains
  disabled because Windows has no immutable-executable snapshot backend.

## Phase 3 — skill library and identity v2

Normative specification: [`phase-3-skill-library.md`](docs/specs/phase-3-skill-library.md)

- `skills` implies `js` and remains independent of the optional embedding backends.
- Keep Agent Skills separate from learned JavaScript artifacts.
- Identity v2 is the full SHA-256 of canonical source, ordered tests, ordered exports/signatures,
  discovery metadata, ABI version, and structured target scopes.
- Identity-v1 rows are quarantined and cannot execute, verify, receive evidence, promote, or be
  selected for rollback. Reproposal never infers scopes.
- Retrieve once from the user prompt before model generation and freeze one turn bundle.
- Production and verification share the contained private-realm loader and hidden-capability ABI.
  Verification gets only declared deterministic fakes, fresh case state, mutation coverage, and
  exact boolean-`true` tests.

## Phase 4 — parent-owned proposals

Normative specification: [`phase-4-auto-admission.md`](docs/specs/phase-4-auto-admission.md)

- Only model-authored code may receive the bounded `propose_skill` global.
- The worker serializes a complete identity-v2 draft; it never opens the store or owns durable
  enqueue.
- The parent validates/canonicalizes the draft, consumes the attempt budget, writes audit intent,
  and enqueues it.
- Independent held-out evaluation and explicit authenticated human approval are required before a
  revision enters non-retrievable canary state. Phase 4 never activates code automatically.

## Phase 5 — evidence-based lifecycle

Normative specification: [`phase-5-evidence-learning.md`](docs/specs/phase-5-evidence-learning.md)

- Validate worker-attributed terminal events against the parent invocation/grant table before
  durable evidence ingestion.
- Permit evidence-gated automation only for eligible pure/read-only replacements; write, process,
  and network capabilities retain a human gate.
- Keep quarantine, repair-as-new-identity, supersession, rollback, index publication, and retention
  transactional and generation-safe.
- An effect-audit `OutcomeUnknown` is ambiguous, never positive evidence, and never replayed.

## Phase 6 — brokered runtime

Normative specification: [`phase-6-brokered-js-runtime.md`](docs/specs/phase-6-brokered-js-runtime.md)

### Parent implementation

| Concern | Location |
|---------|----------|
| `JsTool`, permissions, per-call services | `src/extras/js/tool.rs` |
| Serialized process ownership/watchdog | `src/extras/js/supervisor.rs` |
| Closed JSON wire protocol | `src/extras/js/protocol.rs` |
| Invocation grants and effect authorization | `src/extras/js/broker.rs` |
| Durable intent/completion audit | `src/extras/js/audit.rs` |
| File/fetch/spawn effect services | `src/extras/js/host.rs` |
| Proposal service | `src/extras/js/skills/proposal.rs` |

The parent is the trusted computing base for credentials, configuration, permission prompts,
paths, databases, persistence, audit, and all external effects. It keeps one lazy process-wide
supervisor and one serialized worker. Invocation authority is method-local and never stored in the
warm process or shared supervisor.

The effect audit uses one fixed private version-1 target-correlation key across hash-chained,
size-rotated segments; key rotation is not delivered. One machine-wide file lock admits a single
active parent writer for the resolved audit store. A process-wide `OnceLock` caches initialization
success or failure, so recovery from contention or a repaired storage fault requires restarting
the parent before initialization is attempted again.

### Worker implementation

| Concern | Location |
|---------|----------|
| Same-executable bootstrap and fresh runtimes | `src/extras/js/worker.rs` |
| Private realms, export wrappers, verifier loader | `src/extras/js/realm.rs` |
| Linux empty-root containment | `src/sandbox/worker/linux.rs` |
| macOS one-time-image/Seatbelt/guardian launcher | `src/sandbox/worker/macos.rs` |
| Windows LPAC/Job and attestation | `src/sandbox/worker/windows.rs` |

The process may remain warm after a safe result, but every `RunStep` and whole `VerifyArtifact`
gets a fresh runtime and every verification case gets a fresh context. All protocol values are
owned, bounded Rust data before crossing the pipe.

### Authority ceiling and effects

Realm capabilities limit source-level access, not a native-compromised worker. The maximum
brokered authority of such a worker is the union of live current-step handles. Each effect still
requires an unexpired parent-created invocation/grant, exact declared target scope, session
permission, narrowing policy, backend readiness, and durable intent.

If an effect may have started but completion is uncertain, the parent records `OutcomeUnknown`,
revokes the invocation, recycles the complete worker boundary, and never automatically retries.

### Platform status

| Platform | Status |
|----------|--------|
| Linux | Enforced only after the real trusted empty-root `bwrap`/namespace/rlimit/seccomp preflight succeeds; otherwise unavailable. |
| macOS | Available only on validated macOS 26 hosts with typed `DeprecatedBestEffort` assurance after the one-time-image Seatbelt and guardian live preflight succeeds. Other majors, including macOS 15, remain unavailable. |
| Windows | A process-wide cached minimal production attestation observes the LPAC/token shape, exact protocol handles, selected Job/mitigation state, closed protocol probe, fresh runtime, and clean shutdown. The hosted full canary records ambient-denial and install-location observations only for its reference runner, not every host's ACL visibility. Model-authored `spawn` uses the separately attested general AppContainer backend; learned-skill spawn remains disabled without immutable executable snapshots. |

The Windows OS creation call itself is not cancellable. It runs on one owned helper thread behind a
five-second caller-side deadline; a late result is torn down, while a permanently blocked call is
an explicit availability residual and cannot cause a second launch helper to be created.
LPAC does not create a filesystem namespace, so host ACLs can retain visibility for the stable
package identity. Normal startup and `--print-config` status evaluation create or reuse a
persistent AppContainer profile and may add a persistent exact read/execute ACE to a supported,
user-owned installed executable. There is no automatic cleanup, ACL rollback, or consent prompt.

Run 31319107422 supplies the dedicated Linux, macOS 15/26, Windows worker, and Windows
general-sandbox gates plus the three platform resource records. The final validator at commit
`9c6f164` independently aggregated those records into the reviewed
[`js-worker` resource baseline](docs/benchmarks/js-worker.md), which records measured
reference-host behavior without converting noisy timing targets into security controls.

## Separate process trust classes

[`subprocess-trust.md`](docs/specs/subprocess-trust.md) remains authoritative for process classes
outside the worker. Project hooks, MCP servers, LSPs, loop validation, explicit interactive shell,
support utilities, and model-authored commands have distinct principals, workspace/credential
needs, and lifecycle guarantees. None inherits the broker-only worker profile.

## Phase and feature matrix

| Capability | Owning phase | Cargo relationship |
|------------|--------------|--------------------|
| Preserved bounded JavaScript semantics | Phase 1 | `js` |
| General Linux/macOS/Windows command hardening and parent fetch | Phase 2 | `sandbox` independent; integration uses `js,sandbox` |
| Manual learned-skill retrieval | Phase 3 + Foundation | `skills` implies `js` |
| Agent proposal to human-approved canary | Phase 4 | extends `skills` |
| Evidence-based lifecycle automation | Phase 5 | extends `skills` |
| Contained production and verification execution | Phase 6 | mandatory whenever `js` is enabled; backend absence disables JS |

Compilation alone is not phase completion. Entry dependencies and exit meaning remain normative in
the specification index.

## Permanent prohibitions

- No production QuickJS `Runtime`, `Context`, function, promise, or value in the parent.
- No runtime reuse across requests or context/state reuse across verification cases.
- No in-parent or uncontained JavaScript fallback.
- No arbitrary exception message, stack, thrown value, source, content, or secret in diagnostics.
- No real effect in the worker and no effect before parent authorization plus durable intent.
- No general model command that bypasses `Sandbox::wrap_command`; the broker-only worker uses its
  separate dedicated launcher and never the workspace-visible general profile.
- No permission-free file/network/process/proposal host operation.
- No `require`, `import`, or `final_answer`.
- No source-only/short identity, inferred identity-v2 scope, or identity-v1 execution.
- No truthy-value verifier semantics; only exact JavaScript boolean `true` passes.
- No claim that LPAC worker containment provides Windows general-command containment.
- No claim that reference-runner resource or ambient-denial observations prove every host has the
  same performance or Windows ACL visibility.
