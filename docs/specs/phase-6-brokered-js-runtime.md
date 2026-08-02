# Phase 6 — Brokered Cross-Platform JavaScript Runtime

- **Document role**: normative phase specification
- **Specification version**: 0.1.0
- **Delivery status**: planned
- **Owner**: mini-agent maintainers
- **Last reconciled**: 2026-08-01
- **Entry dependency**: the indexed Phase 1–5 contracts whose behavior Phase 6 preserves
- **Exit dependency**: every gate and acceptance requirement in this document

The corpus authority and conflict rules are defined in
[`00-index.md`](00-index.md). This document is the authority for JavaScript worker containment,
worker protocol, runtime ownership, brokered effects, effect audit, and production/verification
realm parity. It is a contract for future implementation, not a claim that Phase 6 is delivered.

Phase 1 remains the authority for the JavaScript language surface, resource limits, stable error
categories, and permission semantics that this phase preserves. Its exception text/stack
disclosure and in-parent, per-`JsTool` thread ownership are historical and superseded by this phase.
Phase 2 remains the authority for URL/path narrowing and
the general subprocess sandbox used by parent-brokered commands. Its workspace-visible process
profiles must never contain the JavaScript worker. Phase 3 remains authoritative for immutable
storage, manual admission, and retrieval, while Phase 6 supersedes its identity-v1 capability
shape, same-context runtime binding, and verifier runtime ownership. Phase 4's proposal and human
approval gates remain authoritative, while Phase 6 moves proposal transport and persistence behind
the broker. Phase 5's completed evidence policy and transactional lifecycle remain authoritative,
while Phase 6 adds the identity-v1 quarantine migration and forbids rollback to identity v1. The
index contains the exhaustive concern-level supersession map.

The following invariants are unconditional:

- One parent-created worker process is the native-code containment unit.
- One `RunStep` or whole `VerifyArtifact` request is the QuickJS `Runtime` lifetime unit.
- Stored-skill source initialization has no effects and no writer API.
- Parent policy is authoritative even when every worker-supplied attribution field is malicious.
- No supported production path launches an uncontained worker.

## Threat model

Generated, retrieved, proposed, inherited, mutation-test, and held-out JavaScript is untrusted.
The JavaScript worker itself is also untrusted native code: the parent must remain safe if the
worker exploits QuickJS, forges artifact/export identity, replays a grant identifier, violates the
wire state machine, or sends malformed output. A QuickJS `Context` is a source-level authority
boundary inside that untrusted process; it is not a native security boundary.

The trusted computing base is the mini-agent parent, its permission and narrowing policy, the
effect broker and audit writer, the platform launcher, and the operating-system containment
backend. The parent owns credentials, configuration, paths, databases, durable state, external
I/O, process creation, permission prompts, and audit. None of those resources or services is
initialized in the worker.

For an ordinary JavaScript invocation, per-artifact grants constrain the capabilities exposed by
the JavaScript realm. They are not a defense against a native-compromised worker. Such a worker
may try to borrow any parent-created grant handle provisioned for the current invocation, so the
maximum brokered authority is the union of those live handles. It cannot create authority: every
request is still intersected with parent session permissions and target-narrowing policy, and the
grant is bound to the parent-created invocation. Platform containment separately removes ambient
filesystem, network, credential, database, and parent-memory authority. Linux and Windows also
deny process creation; macOS carries the narrower, probed claim defined under Platform containment.

Out of scope are compromise of the parent, containment backend, operating system, or kernel, and
availability after a host-wide resource failure. Phase 6 does not claim that JavaScript realms
contain native compromise or that a permission-approved external effect is reversible.

## Worker lifecycle

The parent lazily keeps at most one interactive worker process live at a time. It launches the
current executable in an internal mode before Clap, Tokio, configuration, path discovery, logging,
hooks, MCP, providers, credentials, or the TUI initialize. The parent creates the containment unit
and owns all pipe and process handles. The worker never daemonizes and is not a pool, helper
package, VM service, or privileged system service.

The process may remain warm between requests, but QuickJS state never does. `RunStep` creates one
fresh bounded `Runtime` for the agent step. `VerifyArtifact` creates one fresh bounded `Runtime`
for the entire verification request so source initialization and all tests share only the state
defined by the verification contract. The runtime and every `Context`, function, promise, and
value derived from it are dropped before the terminal result is sent. Runtime reuse after success,
timeout, cancellation, OOM, or any other outcome is forbidden.

Every runtime preserves the Phase 1 resource limits: a 30-second total request deadline, 64 MiB
QuickJS heap, and 512 KiB QuickJS stack. The one total deadline supersedes Phase 1's independent
per-host-call deadline wording; operation-specific timeouts may be shorter but never extend the
request budget. The runtime installs its memory limit, stack limit, and interrupt deadline before
source evaluation. Pending jobs, console output, sanitized typed diagnostics, file/fetch bodies,
and spawn output are bounded. Arbitrary exception text and stacks are never serialized. The parent
deadline includes worker IPC and brokered host calls rather than pausing while an effect is
serviced. Platform process limits are defense in depth and do not replace these runtime limits.

The worker handles exactly one invocation at a time. Parent cancellation, timeout, transport
failure, or shutdown kills and reaps the entire containment/process group; no unsolicited cancel
frame is sent. A later call may start a new worker, but the failed request is never replayed
automatically. Worker stdout is protocol-only. Console output is returned as bounded structured
data, while a bounded stderr pipe carries only sanitized diagnostics and never source, prompts,
arguments, file or response contents, environment values, or secrets.

`src/extras/js/supervisor.rs` owns the parent-side serialized transport lease shared by run and
verification requests. The shared state retains only process, protocol, generation, and bounded
stderr-drain data; per-invocation effect authority remains method-local. Dropping or cancelling an
in-flight lease invalidates that worker connection, so the next independent request launches a
new generation. One 30-second watchdog starts before lease acquisition and covers startup, IPC,
execution, and pending parent effects. The synchronous platform launch runs outside the async
lease task; cancellation or the watchdog can therefore win during launch, and any process returned
afterward is killed and reaped without becoming a generation. One supervisor-owned launch lease
spans the platform call and any late-result teardown, so subsequent callers wait within their own
deadlines without creating more launcher threads or workers. A worker exit is observed while reads and effects are
pending, and any startup fault, malformed frame, crash, cancellation, deadline, or caller-future
drop destroys the connection instead of attempting resynchronization. Cleanup closes/kills the
backend-owned containment tree and reaps its root within a fixed bound; the platform
`WorkerProcess` abstraction supplies direct native Unix process-group signalling or Windows Job
teardown. Tree-termination failure remains an error even when the root has already reaped. A graceful
shutdown sends the closed `Shutdown` frame, waits within the same bound, and still performs tree
cleanup. The next independent request always receives a new generation, so delayed output from
an old process cannot enter its protocol stream.

## Wire protocol

IPC uses inherited anonymous pipes and a strictly alternating, half-duplex protocol. Each frame is
an 8 MiB-or-smaller JSON payload preceded by a big-endian `u32` byte length. A receiver rejects an
over-limit prefix before allocating its payload buffer. Message enums are closed. Every frame
carries the protocol version, build identity, and monotonic sequence; every invocation frame also
carries a parent-created invocation ID.

The build identity combines the package version with a deterministic SHA-256 fingerprint of the
production compile inputs, dependency lockfile, enabled features, target/profile settings, and
Rust compiler version. Ordinary local and packaged builds therefore reject a same-version peer
built from different inputs without relying on Git metadata, network access, timestamps, or random
values.

After a versioned hello/ready exchange, the only invocation flow is:

```text
RunStep | VerifyArtifact
    -> (EffectRequest -> EffectResponse)*
    -> StepResult | VerificationResult
```

Exactly one frame is outstanding, and one invocation may issue at most 256 effect requests. The
worker cannot pipeline requests, send an effect before an
invocation, change invocation IDs, reuse or skip a sequence, substitute a terminal-result type, or
continue after a protocol fault. Artifact hash, artifact ID, export name, and other worker-supplied
attribution are advisory; the parent resolves authoritative identity from its own invocation and
grant tables.

Effect operations and their success/error responses are closed typed enums. Source, arguments,
capability declarations, and deterministic verifier fixtures enter only in the relevant request;
they are never smuggled through diagnostics. Oversized, truncated, malformed, unknown-version,
wrong-build, wrong-sequence, or state-invalid frames are fatal transport faults. The parent kills
and reaps the worker without attempting protocol resynchronization.

## Capability broker

All real effects execute in the parent. For each invocation the parent constructs an immutable
grant table from the authoritative artifact record, current turn selection, session permission
policy, and Phase 2 target-narrowing policy. Every worker effect request must name a live,
single-invocation grant whose closed operation and declared scope cover the requested target. The
parent re-parses and normalizes the target and arguments, obtains any required permission, applies
deadlines and output limits, writes the effect audit, and performs the effect. A worker assertion
of artifact identity, capability tier, scope, permission, or prior approval never authorizes it.

`src/extras/js/broker.rs` implements the parent-only invocation grant table and the narrow
supervisor callback. It issues opaque grant IDs, derives the effective principal only from its
table, intersects grant and session capabilities, validates invocation, expiry, attribution,
target, permission, and backend readiness, and erases all grant state on terminal, cancellation,
or worker recycle transitions. The authorized-effect seam is backed by parent-side file, fetch,
spawn, and proposal services whose callable contracts contain no QuickJS types. Worker closures
only decode JavaScript values, call those services, and encode their closed results or errors.
File services preserve stable path identity across authorization and I/O; fetch preserves exact
origin and public-address checks plus an outer deadline; spawn passes structured argv to the
general command sandbox. Spawn permission identity is versioned canonical JSON containing the
program and argument array, so argument boundaries are never collapsed into a shell-like string.
The provisional broker `SkillProposalDraft` omits identity-v2 capability and signature fields, so
that broker operation fails closed until the full typed proposal protocol lands; the direct parent
proposal service never infers omitted scopes or identity fields.

Model-authored step code retains bounded effect globals and a bounded `propose_skill` writer host.
Durable proposal enqueue is parent-owned. Stored learned-skill realms receive no effect or writer
globals. Learned-skill ABI v2 instead passes one hidden, immutable invocation capability object as
the first export argument. It contains only the methods declared by the stored artifact. Each
method closure embeds a parent-created grant ID and becomes unusable when the export promise
settles, the invocation is cancelled, or its runtime ends. A skill cannot inspect or manufacture a
grant ID, and retaining an object or method cannot transfer useful authority to a later invocation.

File, fetch, proposal, and command operations retain their owning Phase 1–4 validation and limits.
Parent-brokered JS `spawn` remains disabled on Windows until `mini-agent-uq5c` delivers and verifies
the separate general command sandbox. Worker containment never serves as containment for a
brokered command.

## Persistence boundary

The parent is the sole persistence authority. The worker receives no database handle, skill-store
path, workspace or cache mount, credential, configuration store, or general file API. It cannot
load an artifact by a claimed hash or commit a proposal, lifecycle transition, approval,
telemetry event, or audit record.

Existing `SkillArtifact` and SQLite storage remain authoritative; Phase 6 creates no parallel
manifest directory, writable skill folder, or source-only identity. Artifact identity version 2
includes structured capability scopes in the canonical manifest identity. Any identity-v1
artifact is quarantined until explicitly reproposed and reverified; scope must never be inferred
automatically.

Explicit identity migration reproposal is a parent-owned operation. It may preserve a quarantined
identity-v1 predecessor link for audit, but the old artifact contributes no grant, scope,
verification result, non-inferiority evidence, execution fallback, or rollback eligibility to the
identity-v2 revision. Model-authored proposals cannot invoke this migration exception.

Stored-skill source initialization is pure. The worker installs neither effect hosts nor
`propose_skill` while evaluating stored source, and initialization must leave no pending jobs.
Declared exports are validated only after initialization completes. Proposal drafts and execution
evidence cross the wire as bounded data; the parent canonicalizes, validates, and persists them
under the existing lifecycle transaction rules.

## Verification parity

Production execution, embedded tests, inherited regression tests, mutation tests, and held-out
tests use one worker-owned artifact loader and the same private-realm, pure-initialization,
declared-export, hidden-capability-object, JSON-clone, and pending-job contract. No verifier may
construct a QuickJS runtime in the parent or use a source wrapper that production does not use.
One complete `VerifyArtifact` request owns one fresh runtime.

Verification supplies deterministic fake capability objects through the same capability
registration path used by production. Fakes are limited to the artifact's declared structured
scopes and produce a bounded transcript. This is parity of loader, realm, ABI, attribution, and
registration—not a claim that fake and real I/O, permissions, audit durability, or timing are
identical. Verification grant IDs resolve only to the deterministic fake responder and are rejected
by the production effect broker; verification cannot reach real effects or persistence services.

The realm delivery gate is mandatory: cross-`Context` function identity, promise continuation,
exception classification/location behavior, ambient-global absence, and declared JSON cloning
must pass the QuickJS proof before private realms or the verifier are implemented. The feasibility
test may inspect bounded QuickJS message/stack behavior entirely inside its test process, but no
production wire/result/log surface may preserve that arbitrary text. Failure stops Phase 6; a
shared-global wrapper is not an accepted fallback. Passing proves a source-level realm contract,
not native containment.

### Realm isolation

The locked realm gate passes with `rquickjs`, `rquickjs-core`, and `rquickjs-sys` 0.12.1, whose
vendored QuickJS reports version 0.15.1. Within one `Runtime`, a `Persistent<Function>` created in
one full `Context` restores and can be invoked while an agent `Context` is active, but resolves
`globalThis` from its defining skill context. A promise returned by that function remains pending
until the runtime executes its queued continuation, then settles with the defining context's
global. An exception thrown by the restored function remains available through the function's
context with its message, function name, and source filename intact so the test can apply bounded,
UTF-8-safe message and stack extraction.

The same gate installs mock `read_file`, `write_file`, `fetch`, `spawn`, and `propose_skill`
globals in the agent context and observes each as exactly `undefined` in the skill context. Plain
JSON-compatible objects and arrays cross only after strict validation, bounded JSON serialization,
and parsing in the receiving context; functions, symbols, BigInts, accessors, host objects, cycles,
sparse arrays, unsupported nested values, and oversized encodings are rejected. These results
establish the source-level operations required by the future loader. They do not make QuickJS
contexts a native security boundary, and they do not authorize arbitrary exception text on any
production result, wire, diagnostic, or log surface.

## Effect audit

Before every real brokered call—including `read_file`—the parent appends and durably syncs an
authorization/intent record. Audit failure denies the effect. The record is parent-attributed and
contains the invocation ID, authoritative artifact/export identity when applicable, grant ID,
normalized operation and redacted target metadata, policy decision, sequence, timestamp, and
previous-record hash. It contains no source, prompt, argument or file content, response body,
environment value, credential, secret, raw filesystem path, URL user information, URL path/query,
or command argument.

Target correlation uses HMAC-SHA-256 with a dedicated parent-only audit key and an explicit
metadata allow-list; plain unkeyed hashes are forbidden because paths and URLs are often guessable.
Each tag covers a length-prefixed, domain-separated tuple of operation, metadata kind, and canonical
target bytes, and the record stores the full 32-byte tag. A file record stores only its storage
class, operation, and target tag for the canonical UTF-8 path—never a basename or path component. A
fetch record stores only method, normalized scheme/effective port, one target tag for the
normalized host, and a separate target tag for the canonical path and query; host labels, query
names, and query values are never stored in plaintext.
A command record stores its operation and a keyed digest of the resolved executable, with no
arguments or environment. Redirect targets receive independent records. The target-correlation
digest is separate from the audit chain hash, is domain-separated by operation and metadata type,
and supports equality correlation only; it cannot be used as authorization or reversed to recover
the target.

The audit key is created in private parent-owned storage, identified by a non-secret key version,
and never enters the worker or diagnostics. Rotation retains old keys only for the configured audit
retention window. A missing, unreadable, or corrupt required key makes audit recovery fail and JS
remain unavailable rather than falling back to plaintext metadata or an unkeyed digest.

The same metadata allow-list and target-tag rules apply to completion records, recovery messages,
and audit errors; none may reintroduce a raw target while describing a result.

After an attempted effect, the parent appends a bounded completion record describing success,
denial, timeout, cancellation, truncation, or an ambiguous outcome. The audit is append-only,
hash-chained, privately created, bounded by an explicit retention policy, and recovered before JS
becomes available at startup. Completion-record failure is surfaced as a terminal audit failure,
stops further invocation work, and does not cause an effect to be repeated. Because the effect may
already have happened, this fail-closed rule means no continuation or replay, not guaranteed
non-occurrence. Deterministic verification fakes write their verification transcript, not
production effect-audit records.

Authorization, intent, and completion records are evidence, not permission tokens. Recovery never
turns an old record into a live grant, silently fills a missing completion, or replays an effect.

## Platform containment

The worker launcher has a platform-neutral fail-closed contract and platform-specific trusted
backends. No backend may bind the workspace, application cache, skill store, credentials, sockets,
or parent configuration into the worker. Only the executable/runtime files required to start,
anonymous protocol and bounded-diagnostic handles, and narrowly required system resources are
visible. Inherited descriptors/handles are closed except for an exact allow-list. Parent teardown
kills the entire containment/process group.

The platform-neutral launcher owns its target-selected child control object and three anonymous
protocol/diagnostic pipes as ordinary files. Its common control surface is limited to process ID,
tree termination, nonblocking status, and reaping; it does not require a Windows process created
through Win32 APIs to masquerade as `std::process::Child`. The unconfined launcher is compiled only
for tests. Until a target's real containment launcher is delivered and probed, its production
status and every production launch attempt remain unavailable.

| Platform | Required worker containment |
|----------|-----------------------------|
| Linux | A broker-only bubblewrap profile with no workspace/cache bind; isolated namespaces and environment; validated OS resource limits; and an in-worker seccomp deny policy for process creation and execution. |
| macOS | A broker-only Seatbelt profile, explicit descriptor closing, sanitized environment, and validated rlimits. `/usr/bin/sandbox-exec` is a deprecated, weaker, best-effort MAC backstop, so real probes must report its exact denials and process-creation behavior and the parent must always kill the whole process group. |
| Windows | A zero-capability LPAC/AppContainer, compatible process mitigations, creation-time Job Object assignment, child-process denial, validated resource limits, and an exact inherited-handle list. |

The Linux launcher is a dedicated broker-only bubblewrap profile, not the general command
sandbox. It starts from an empty root, mounts only the exact worker executable and the exact
root-owned, non-group/other-writable regular system files already mapped into the parent runtime
(shared objects need not carry an executable mode bit), creates private
proc/dev/tmp views, clears the environment, closes non-protocol descriptors, and requests user, PID,
network, IPC, UTS, and (where supported) cgroup namespaces while dropping every capability. After
the authenticated `Hello` and before `Ready`, the already-exec'd worker applies
validated address-space, CPU, descriptor, core, and file-size ceilings, disables process
dumpability, sets `no_new_privs`, and
installs a seccomp filter denying fork, vfork, clone, clone3, execve, execveat, socket, and
socketpair, along with namespace and mount mutation. On x86_64 a preceding BPF range guard denies
every syscall number carrying the x32 ABI bit, so alternate-ABI numbers cannot bypass the exact
deny set. The post-handshake evaluator therefore cannot create private or external network
listeners even inside its isolated network namespace. Any trusted-path, namespace, mount, limit,
`no_new_privs`, or seccomp failure emits no `Ready`; there is no unconfined retry. Availability is
cached only after an actual namespace/limit/seccomp preflight succeeds.

The ignored `linux_js_worker_containment` probe is the runtime evidence gate. It must run on a real
Linux host with trusted bubblewrap and must be listed explicitly before execution so a cfg error
cannot produce a zero-test success. macOS development can verify source ordering, owned teardown,
and fail-closed construction, but cannot claim this Linux runtime evidence. The real gate requires
an exact SIGXCPU outcome after an armed soft limit, a non-dumpable sacrificial SIGABRT child that
reports no core dump and leaves no artifact under `RLIMIT_CORE=0` (including on hosts whose
`core_pattern` pipes dumps to a handler), a malformed frame and explicit termination only after a
completed Hello/Ready handshake and valid contained `RunStep`,
and disappearance of the exact controlled sleeper PID/start-time after process-group teardown.

Containment availability and the real backend are probed before JS is advertised. Linux and
Windows separately deny process creation/exec. macOS must probe the weaker backend's process
behavior and report it without upgrading the claim. A backend that is absent, untrusted,
misconfigured, unverifiable, or unable to apply every required restriction makes JS unavailable.

### macOS standalone-CLI containment gate

The standalone macOS CLI currently reports `Seatbelt` with typed
`DeprecatedBestEffort` assurance and keeps production JavaScript unavailable. This is a deliberate
fail-closed result, not a delivered macOS containment backend. A real local probe on macOS 26
preserves the following platform evidence:

- a deny-default profile without `process-exec` prevents `/usr/bin/sandbox-exec` from executing
  the initial worker image;
- allowing the exact initial image is not a one-time grant and remains usable for later exec; and
- macOS rejects applying a second, tighter Seatbelt profile after the first profile is active.

macOS 15 remains an explicit CI probe target rather than a validated runtime major. The production
allowlist must not classify it as validated until that runner has produced the same real-backend
evidence. Both CI rows first prove that the target-gated test exists, so an unsupported target or
an accidentally compiled-out test cannot pass as a zero-test success.

Consequently the current stable-image design cannot both enter the worker through `sandbox-exec`
and deny a native-compromised worker from executing the allowed image again. The public
`sandbox_init` API is deprecated, accepts only named profiles, and documents re-sandboxing as an
error; the raw profile compile/apply functions used by `sandbox-exec` are private. A one-time
securely published image whose only pathname is removed before untrusted work is a possible future
design, not a delivered bypass: its code-signing behavior, publication race, descriptor/path
re-exec surface, crash cleanup, and full capability matrix remain unvalidated. Until a validated
standalone mechanism closes this transition, the launcher has no profile, descriptor, rlimit, or
process-group fallback: every production launch returns unavailable. The ignored real-backend test
`macos_js_worker_containment` is a blocker regression gate; it must not be represented as proof of
the complete workspace, skill-store, network, descriptor, resource, or descendant-denial matrix.

The Windows delivery gate is mandatory: an LPAC worker must load from every supported install
location and start with only the protocol handles. Failure for a location leaves Windows JS
disabled there. A restricted-token worker, unsafe broad ACL change, or unconfined fallback is
forbidden.

### Windows image-loading feasibility gate

The A03 research spike provides a Windows-only ignored real-backend test named
`windows_lpac_can_load_current_exe_with_only_protocol_handles`. Its containment matrix copies the
full-probe-capable Cargo-built libtest harness into each of the three supported destination
classes: Cargo build, Cargo install, and a user archive under `%LOCALAPPDATA%`. Every one of those
three rows runs the same token, capability, console, Job, handle, and workspace-denial readiness
probe. The source location and destination location are validated independently, so the archive
row correctly treats its harness source as Cargo build while requiring its copy to classify as a
user archive.

A real binary produced by `cargo install --locked --no-default-features --features js --path .
--debug` supplies two additional, explicitly narrower image-loading rows: a disposable copy
under the active Cargo home and another under the user-archive root. Those rows execute
`--version` and prove only that `CreateProcessW` accepts the production image with the requested
attribute list and that the image reaches its version path. They do not prove the resulting token,
capability, console, handle, sentinel, or other child-side containment assertions and are never
reported as doing so. `MINI_AGENT_LPAC_CARGO_INSTALL_EXE` must name that installed binary. A
missing variable, a source outside the active Cargo home, or any omitted or failed row fails the
whole gate.

For every row the gate makes a private disposable copy within that row's real location class and
changes only the copy. It rejects NULL DACLs; inherited-deny ambiguity; broad executable access
for Everyone, Authenticated Users, Builtin Users, `ALL APPLICATION PACKAGES`, or `ALL RESTRICTED
APPLICATION PACKAGES`; and write, modify, ACL, ownership, or delete authority held by any
untrusted principal. It maps generic file rights before evaluation, rejects generic or specific
read/execute grants to every unexpected principal, counts every package-SID allow ACE, and
requires exactly one package allow ACE whose mapped mask is exactly non-inheriting read/execute.
It then checks effective access and holds the executable open without share-write or share-delete
until `CreateProcessW` returns. It never restores a stale whole DACL. Every ancestor through the
classified trust root must be local, fixed-drive,
non-reparse, non-NULL-DACL, and protected from other-principal mutation or deletion. UNC, remote,
protected machine-wide, and unknown roots fail closed.

The launch creates or derives one stable zero-capability AppContainer profile, opts out of `ALL
APPLICATION PACKAGES` for LPAC, uses `DETACHED_PROCESS`, supplies an exact three-handle anonymous
protocol/diagnostic list, and assigns an unnamed kill-on-close, single-process Job through the
creation-time attribute list. The parent verifies membership in that exact Job. The harness child
must verify `TokenIsAppContainer`, `TokenIsLessPrivilegedAppContainer`, zero token capabilities,
no console window or `CONIN$`/`CONOUT$`, the three distinct standard protocol handles, rejection of
an inheritable canary omitted from `HANDLE_LIST`, and access denial for a parent-created workspace
sentinel before emitting its fixed readiness frame. Profile and disposable-artifact cleanup errors
are part of the gate result rather than silently treated as success.

On a standard-user Windows checkout, prepare and run the complete gate with:

```powershell
cargo install --locked --no-default-features --features js --path . --debug
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$env:MINI_AGENT_LPAC_CARGO_INSTALL_EXE = Join-Path $cargoHome 'bin\mini-agent.exe'
cargo test --locked --no-default-features --features js windows_lpac_can_load_current_exe_with_only_protocol_handles -- --ignored --nocapture --exact
```

This source-level gate has not been executed on Windows as part of the macOS-authored change. Its
result remains unverified until the ignored test passes on `windows-latest` and a standard-user
Windows installation for every location that will be advertised. Production Windows worker status
therefore remains unavailable: this spike does not deliver the A26 launcher, does not permit a
restricted-token or unconfined fallback, and does not satisfy the separate `mini-agent-uq5c`
general-command sandbox.

## Failure semantics

Every production failure uses a closed sanitized diagnostic contract. `class` is one of `syntax`,
`javascript_exception`, `promise_rejection`, `host`, `permission`, `validation`, `timeout`,
`cancelled`, `out_of_memory`, `pending_job_limit`, `protocol`, `containment`, `audit`, or
`internal`. `code` comes from a versioned parent/worker allow-list; a worker-provided unknown code
is a protocol fault. Optional corrective metadata is limited to a source-free location: closed
`stage` and `script_role` enums plus validated one-based numeric line/column values within the
submitted script. It contains no filename, function name, property/key name, target, ordinal,
effect result, or other source-derived string.

If QuickJS cannot be classified without trusting exception-controlled text, the worker returns the
generic `javascript_exception` or `promise_rejection` code. Arbitrary exception `name`, message,
stack, thrown value, source line/snippet, cause, aggregate members, effect result, console-derived
text, prompt, argument, file/fetch content, and secret never cross the production worker protocol
as a diagnostic and never enter model output, stderr, logs, audit, telemetry, evaluation reports,
or repair records. The worker discards those values after deriving the closed class and validated
source-free location. Parent-generated fixed templates may render the stable code and safe metadata
for correction, but must not interpolate worker-controlled text.

The system fails closed at every trust boundary:

- unavailable or failed containment makes JS unavailable; production never retries in-process or
  uncontained;
- failed realm or Windows delivery gates block the affected implementation/status claims and do
  not select weaker designs;
- launch, handshake, version/build mismatch, malformed frame, state violation, worker crash, OOM,
  timeout, cancellation, or transport failure kills and reaps the worker group and returns one
  bounded typed failure;
- an unknown, expired, wrong-invocation, wrong-operation, or out-of-scope grant is denied without
  performing the requested effect;
- permission, target-narrowing, validation, deadline, output-limit, or pre-effect audit failure
  denies the effect;
- parent shutdown or a closed protocol pipe causes worker exit, while parent process control
  remains responsible for forced cleanup; and
- worker errors and diagnostics disclose no arbitrary message/stack, thrown value, source snippet,
  effect result, target, prompt, argument, content, environment, credential, or secret.

An external effect can complete immediately before the worker or transport fails. Such an outcome
is recorded as completed or ambiguous and returned without automatic retry. Phase 6 promises
neither exactly-once external effects nor rollback of an approved effect; it promises durable
pre-effect intent, bounded completion evidence, no automatic replay, and fresh state on the next
independent call.

Proposal cancellation is exact before queue dispatch and returns `cancelled`. After a proposal has
entered the bounded queue, cancellation returns `outcome_unknown` while a detached blocking waiter
drains the response; callers must not replay that proposal automatically. A29 owns durable
reconciliation of this ambiguous outcome.

## Acceptance matrix

The matrix defines required evidence. It deliberately records no completed status; Phase 6 remains
planned until the index exit rule is satisfied.

| Contract area | Required acceptance evidence |
|---------------|------------------------------|
| Threat model | Hostile worker tests prove forged identity, artifact hash, sequence, and grant data cannot expand parent-owned authority. |
| Worker lifecycle | Tests prove one contained process, one fresh runtime per `RunStep`/whole `VerifyArtifact`, limits installed before eval, bounded jobs/output, and kill-and-reap cancellation. |
| Wire protocol | Codec/state-machine tests reject oversized, malformed, unknown, replayed, out-of-order, cross-invocation, and wrong-terminal frames. |
| Capability broker | Permission/policy/grant intersection tests cover every typed effect, expiry, scope, Windows spawn denial, and malicious attribution. |
| Persistence boundary | Tests prove no worker database/path authority, pure initialization, no writer in stored realms, identity-v1 quarantine, and parent-only canonical persistence. |
| Verification parity | The QuickJS realm gate passes; production and all verifier modes use one loader/ABI path with only declared deterministic fake capabilities and the same sanitized typed diagnostic contract. |
| Effect audit | Recovery and failure-injection tests prove durable intent before every real effect, bounded completion, hash-chain integrity, HMAC target correlation/redaction, key rotation/failure, retention, and no replay. |
| Platform containment | Real Linux/macOS/Windows probes prove the broker-only capability matrix; the LPAC install-location gate passes wherever Windows JS is enabled. |
| Failure semantics | Crash, OOM, timeout, cancellation, audit failure, backend absence, parent death, ambiguous-effect, and secret-in-thrown-value tests all fail closed with only stable class/code and source-free location metadata. |
| Corpus consistency | The exact Phase 1–6 documentation scan shows all surviving in-process/thread claims as historical or superseded, indexes this planned spec, and marks no Phase 6 feature delivered. |
