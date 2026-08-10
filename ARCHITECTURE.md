# JS Engine Integration — Architecture Overview

- **Document role**: non-normative architecture overview
- **Overview version**: 2.0.0
- **Delivery status**: Phase 6 delivered
- **Owner**: mini-agent maintainers
- **Last reconciled**: 2026-08-09

The sole normative JS corpus is
[`docs/specs/00-index.md`](docs/specs/00-index.md) and the phase specifications it indexes. If this
overview differs from an indexed specification, the indexed specification controls and this file
must be corrected.

## Purpose

mini-agent exposes QuickJS as a bounded code-action primitive without placing an interpreter in
the trusted parent. The parent launches the current executable in an internal, contained worker
mode, exchanges closed JSON frames over anonymous pipes, and performs every external effect through
its own policy services.

This architecture supplements Bash and other process-bearing features. It does not port shell
hooks or make hooks, MCP servers, LSPs, loop validation, or the explicit interactive shell part of
the JS worker boundary.

## Execution and trust boundary

```text
[trusted parent]
JsTool -> process-wide lazy supervisor -> platform worker launcher
   |              |                            |
   |              | JSON frames                v
   |              +-------------------> [contained same-exe worker]
   |                                      fresh Runtime per request
   |<-- typed effect request ------------ private skill/model realms
   |
   +-> invocation grant table -> permission/narrowing -> durable intent
       -> parent file/fetch/spawn/proposal service -> durable completion
```

- The parent lazily keeps at most one worker process alive and services one invocation at a time.
- The process may remain warm after a safe terminal result, but QuickJS state never does.
- Each `RunStep` and whole `VerifyArtifact` request creates and drops a fresh 64 MiB-heap,
  512 KiB-stack `Runtime`; each verification case receives a fresh `Context` and fake state.
- The interrupt deadline is installed before evaluation. The parent watchdog covers launch, IPC,
  evaluation, permission waits, effects, and pending-job drain.
- `JsTool`, the supervisor, broker, audit, and host services contain only `Send + Sync` parent
  state. No production parent field or value contains a QuickJS `Runtime`, `Context`, or value.
- A transport, protocol, timeout, cancellation, resource, or containment fault kills and reaps the
  complete worker boundary and erases every invocation grant.

The worker protocol is length-prefixed, bounded JSON with a closed version/build/sequence state
machine. Worker stdout is protocol-only. Diagnostics expose only closed class/code and validated
source-free stage/script-role/line/column metadata; exception messages, stacks, thrown values,
source, prompts, arguments, effect results, contents, environment, and secrets never cross the
production diagnostic boundary.

## Capability broker and residual authority

All real file, fetch, command, and proposal effects execute in the parent. For each request, the
parent builds an immutable table of opaque, invocation-bound grants from the authoritative selected
artifacts and model-authored host surface. It intersects each request with the session permission
policy, Phase 2 target narrowing, backend readiness, expiry, and exact target before performing an
effect.

Private realms and hidden capability objects prevent one JavaScript artifact from receiving
another artifact's source-level API. They are not a native security boundary. A worker that
compromises QuickJS may attempt to borrow the union of all live current-step grant handles. It
still cannot mint authority, escape the parent-created invocation, bypass parent permissions or
target scopes, or gain ambient filesystem/network/credential/database access removed by platform
containment.

Stored skills initialize without effect or writer globals. Each export call consumes a fresh
parent-issued handle and receives a null-prototype frozen capability object containing only its
declared identity-v2 methods. Model-authored code separately receives the bounded primitive effect
surface and, when configured, one model-only `propose_skill` grant. A proposal cannot execute in
the proposing step.

## Durable effects and cancellation

The private parent audit is a length-prefixed, hash-chained sequence. For every real effect the
parent validates authority, appends and syncs durable intent, performs the operation, and then
appends and syncs a closed completion. Audit failure before intent performs no effect.
One fixed private version-1 HMAC key correlates redacted targets across size-rotated, hash-linked
segments; key rotation is not implemented. A machine-wide file lock allows one active parent
writer for the audit store. Its process-wide `OnceLock` caches the first success or failure, so a
failed parent does not retry until process restart.

Cancellation is exact while waiting for a worker lease, target normalization, permission, or any
other pre-dispatch step. Once a write, fetch, subprocess, or queued proposal may have started, a
timeout, cancellation, or transport loss can make the result ambiguous. The parent records
`OutcomeUnknown`, drains or tears down the owned work within a fixed bound, revokes the invocation,
and recycles the worker. It never reports false success/failure or automatically retries an
ambiguous external effect.

Parent-brokered command effects still use the general `Sandbox::wrap_command` path and require a
backend that owns the complete descendant lifetime. The worker itself never uses that
workspace-visible profile; it has a dedicated broker-only launcher.

## Platform containment

| Platform | Broker-only worker guarantee |
|----------|------------------------------|
| Linux | Availability is cached only after a real preflight of a trusted empty-root `bwrap` profile. It maps the exact worker/runtime files, no workspace/cache/configuration, a private proc/dev/tmp, cleared environment, user/PID/network/IPC/UTS namespaces, dropped capabilities, rlimits, non-dumpability, `no_new_privs`, and seccomp process/exec/socket denial. Any failure is unavailable with no uncontained retry. |
| macOS | Available only on validated macOS 26 hosts, with typed `DeprecatedBestEffort` assurance. A trusted guardian launches an exact hash-proven one-time image under deny-default Seatbelt, using an APFS copy-on-write clone when the source volume supports it and a checked private copy otherwise; authenticated Ready unlinks that image, and every status check repeats the denial, limit, lifecycle, and guardian parent-death preflight. Other majors, including the macOS 15 CI probe, remain unavailable. |
| Windows | A process-wide `OnceLock` reports available only after one sacrificial same-launcher attestation observes the LPAC/token shape, exact protocol handles, selected Job/mitigation state, closed containment probe, fresh runtime, and clean shutdown. The hosted full canary records ambient-denial and install-location observations only for its reference runner; it does not prove identical ACL visibility on every Windows host. LPAC contains only the evaluator. Model-authored `spawn` uses the separately attested general AppContainer backend; learned-skill spawn remains disabled because Windows has no immutable-executable snapshot backend. |

Platform status is typed and source-free. Backend absence or failed attestation disables JS; it
never selects the historical in-parent engine or an uncontained worker. The reproducible resource
methodology and reviewed three-platform aggregate are in
[`docs/benchmarks/js-worker.md`](docs/benchmarks/js-worker.md). Measurements are observational;
the native memory and CPU ceilings remain independently enforced security controls.

On Windows the OS creation call runs on an owned helper thread behind a five-second caller-side
deadline; the call itself is not cancellable. A late return remains owned and is torn down, but a
permanently blocked call is an explicit availability residual and does not permit a second launcher.
LPAC is not a filesystem namespace, so host ACLs can still expose objects to the stable package
identity. Normal startup and `--print-config` containment-status evaluation create or reuse a
persistent AppContainer profile and may add a persistent exact read/execute ACE to a supported,
user-owned installed executable. No automatic cleanup, ACL rollback, or consent prompt exists.

## Skills, identity, and verification

Agent Skills and learned JavaScript skills remain separate:

1. Agent Skills are progressively disclosed instruction/resource packages. `allowed-tools` and
   bundled scripts grant no authority.
2. Learned JavaScript skills are immutable identity-v2 artifacts. Their SHA-256 identity covers
   the complete canonical source, ordered tests and exports, discovery metadata, ABI version, and
   structured capability scopes.

Identity-v1 rows are quarantined before JS availability and cannot execute, verify, receive
evidence, promote, or become rollback targets. Explicit reproposal creates a new identity-v2 row;
scopes are never inferred.

Retrieval occurs once before model generation from the user prompt. The parent freezes one bundle
for the turn and sends it directly to the worker; the worker never opens SQLite or computes
embeddings. Production and verification share the same private-realm loader and hidden-capability
ABI. Verification uses deterministic in-memory fakes only, fresh context/fake/transcript state per
case, and exact JavaScript boolean `true` semantics.

Phase 4 retains independent held-out evaluation and explicit human approval into non-retrievable
canary state. Phase 5 retains directly attributed evidence, limited Tier 0/1 automation,
quarantine, immutable repair, supersession, rollback, and retention. Write, process, and network
authority retain their human gates.

## Other process trust classes

[`docs/specs/subprocess-trust.md`](docs/specs/subprocess-trust.md) owns the separate contracts:

- project/global hooks are trusted automation and may need workspace state and credentials;
- MCP and LSP children are long-lived workspace services with their own configuration trust;
- loop validation is an explicit user-configured command;
- `!` is an explicit human shell with ambient authority; and
- model-authored Bash/JS commands use the general action sandbox.

These classes may reuse lifecycle utilities, but none may borrow the broker-only worker profile or
claim its containment guarantees.

## Current source map

```text
src/extras/js/
├── tool.rs           # JsTool, per-call parent policy and services
├── supervisor.rs     # one serialized contained process and watchdog
├── broker.rs         # invocation grants and effect authorization
├── audit.rs          # durable parent effect intent/completion chain
├── protocol.rs       # closed bounded wire types/state
├── worker.rs         # internal worker bootstrap and fresh runtimes
├── realm.rs          # private skill/model realms and verification loader
├── host.rs           # parent effect services; historical test globals
├── engine.rs         # test-only historical evaluator
└── skills/           # identity, storage, retrieval, admission, lifecycle

src/sandbox/worker/
├── linux.rs          # empty-root broker-only bubblewrap
├── macos.rs          # macOS 26 one-time image, Seatbelt, and guardian
└── windows.rs        # LPAC, Job, attestation, full canary gate
```

Tool registration lives in `src/agent/builder.rs`; general model-command isolation remains in
`src/sandbox.rs`. Shared digest consumers use `src/hex.rs` for stable lowercase encoding across
cryptographic crate upgrades.
