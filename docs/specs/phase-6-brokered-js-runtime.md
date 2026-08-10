# Phase 6 — Brokered Cross-Platform JavaScript Runtime

- **Document role**: normative phase specification
- **Specification version**: 1.0.0
- **Delivery status**: delivered
- **Owner**: mini-agent maintainers
- **Last reconciled**: 2026-08-09
- **Entry dependency**: the indexed Phase 1–5 contracts whose behavior Phase 6 preserves
- **Exit dependency**: every gate and acceptance requirement in this document

The corpus authority and conflict rules are defined in
[`00-index.md`](00-index.md). This document is the authority for JavaScript worker containment,
worker protocol, runtime ownership, brokered effects, effect audit, and production/verification
realm parity. The implementation and dedicated cross-platform containment gates and platform
records from CI run 31319107422 satisfy this contract. The checked-in resource baseline was
independently aggregated by the final validator at commit `9c6f164` and remains observational
rather than a security boundary.

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
may try to borrow any parent-created grant handle provisioned for the current request, so the
maximum brokered authority is the union of all live current-step handles rather than the scope of
one source-level realm. It cannot create authority: every
request is still intersected with parent session permissions and target-narrowing policy, and the
grant is bound to the parent-created invocation. Platform containment separately removes ambient
filesystem, network, credential, database, and parent-memory authority. An available Linux worker
and an attested Windows worker also deny process creation. On validated macOS 26 hosts, macOS
implements the explicitly weaker scoped boundary below and reports `DeprecatedBestEffort`; every
production status check still requires the complete production-binary live preflight. Other macOS
majors remain unavailable.

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
for the entire verification request; each verification case creates a fresh `Context` and reloads
source, so no source state, fake state, transcript, or pending job crosses a case boundary. The
runtime and every `Context`, function, promise, and value derived from it are dropped before the
terminal result is sent. Runtime reuse after success, timeout, cancellation, OOM, or any other
outcome is forbidden.

Verification fake transcripts have one aggregate call/serialized-byte reservation budget for the
whole request even though their contents remain isolated per case. Each fake effect reserves its
worst-case JSON wire size before cloning record values. A limit breach produces a bounded typed
verification result and terminates remaining cases, so a complete terminal frame always remains
below the protocol frame limit.

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

On Unix, a worker exit caused by `SIGXCPU` (or its shell-compatible `128 + SIGXCPU` status) is a
closed native-resource fault rather than a generic transport fault. The supervisor recycles the
process, and skill verification maps it to source evaluation failure. This keeps an infinite source
deterministic when the reused process reaches its cumulative 35-second native CPU ceiling before
the request-local QuickJS interrupt can serialize its normal timeout diagnostic.

Background skill verification enters one bounded FIFO queue owned by that same supervisor; it
never starts a verifier process or worker pool. Interactive `RunStep` callers have priority while
waiting or active. A whole `VerifyArtifact` already dispatched to the worker remains atomic, but
the dispatcher admits waiting interactive calls before dequeuing the next verification. A request
cancelled before dequeue never reaches the worker. Queue overflow or closure fails closed as a
retryable admission-infrastructure failure and cannot produce an admission success.

All full-agent rebuilds in the parent obtain this same lazy, authority-free supervisor. A rebuild
snapshots its own permission bridge, file/fetch policy, selected skill artifacts, invocation IDs,
grants, cancellation, and broker for each `JsTool::call`; none of those values is stored in the
warm process or supervisor. Model switches and network retries therefore reuse the existing tool
and worker generation, while dropping an agent closes only that build's permission receiver.
Subagents and `/btw` intentionally keep their exact restricted tool sets and do not receive JS.
Lifecycle regression tests assert stable process ID and generation across rebuilds plus denial
under a rebuilt policy, proving that the old policy did not leak into the reused worker.

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

Protocol version 2 binds startup to one process launch. The parent generates a fresh non-nil UUID
challenge when it constructs that launch's protocol state, sends it in `ParentHello`, and accepts
`WorkerReady` only when the worker echoes the exact challenge. The worker records only the
challenge received in its one accepted `ParentHello` and may send `WorkerReady` only with that
exact value. A nil, missing, replayed, or different challenge is a closed protocol failure and
does not advance either state machine.

After the parent state machine validates the exact `WorkerReady`, and before sequence 2 or any
containment probe, `RunStep`, or `VerifyArtifact`, the supervisor calls the worker process's
`finalize_authenticated_ready` platform hook. Hook failure aborts startup and tears down the
containment tree. The macOS hook removes the exact descriptor-pinned executable pathname after
challenge-bound Ready and before sequence 2. Linux and Windows remain no-ops at this hook.

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
The broker `SkillProposalDraft` carries the complete bounded identity-v2 proposal shape: source,
description, export names and signatures, tests, structured capability tier/scopes, tags, and an
optional predecessor identity. The parent converts that closed wire value into the existing
proposal validator, canonicalizes the complete artifact, writes the durable proposal audit intent,
and only then enqueues it. It never infers omitted scopes, signatures, tags, or identity fields.

Model-authored step code retains bounded file, fetch, and spawn effect globals. When the parent
provides a proposal service, it also issues a separate exact `ModelAuthored` proposal grant and the
worker installs the bounded `propose_skill` global. Without that service the global is absent and
unadvertised. The call sends only the full typed identity-v2 draft to the parent; validation,
durable intent/completion audit, queueing, admission, and attempt-budget ownership remain outside
the worker authority boundary. A proposal is never added to the current turn bundle, so its source
cannot execute in the proposing step. Telemetry treats every structured worker field as an untrusted
execution claim. The parent requires an exact match to the selected artifact/export and the
parent-derived turn, tool-call, deterministic invocation, and step outcome before rebuilding a
canonical event with its own retrieval metadata, index generation, production flag, timestamp,
and evidence status. Worker feedback, selection, observability, and capability-policy kinds are
rejected. Positive evidence exists only after a complete canonical batch is accepted by the
bounded dispatcher. Invalid, incomplete, saturated, disconnected, or failed dispatch records a
parent-owned `ObservabilityLost` signal and cannot trigger feedback or quarantine. Stored
learned-skill realms receive no effect or writer
globals. Learned-skill ABI v2 instead passes one hidden, immutable invocation capability object as
the first export argument. It contains only the methods declared by the stored artifact. Each
method closure embeds a parent-created grant ID and becomes unusable when the export promise
settles, the invocation is cancelled, or its runtime ends. A skill cannot inspect or manufacture a
grant ID, and retaining an object or method cannot transfer useful authority to a later invocation.
Only a parent-issued `ModelAuthored` grant may authorize `ProposeSkill`; the broker rejects every
stored-skill principal before target validation, audit, or enqueue. Direct, indirect, constructor,
prototype, initialization, export-body, and promise-continuation lookups in a stored realm therefore
have no writer binding and cannot create proposal traffic.

`src/extras/js/skills/capability.rs` owns the worker-local binding from an explicit invocation ID
and exact manifest to one opaque grant per declared method. `src/extras/js/realm.rs` constructs a
null-prototype frozen object immediately before calling the stored export, inserts it at hidden
argument zero, and keeps its token live only until synchronous return/throw or exact promise
fulfillment/rejection. Dispatch checks the captured token before constructing an `EffectRequest`;
a stale method therefore produces a closed denial and no protocol traffic. Cancellation removes
only the named invocation, while the worker runtime lifecycle clears every prepared and active
token on timeout, unwind, or recycle. Event attribution follows the request's explicit invocation
and captured grant; there is no ambient active-invocation map or map-order fallback.

Parent preparation retains only a reusable artifact/export binding inside the parent broker; it
does not yield reusable bearer authority. On each wrapper entry, the Rust-owned worker dispatcher
requests the next exact artifact/export call ordinal. The parent validates that ordinal against its
selected-export table, derives the invocation ID, and mints new scoped grants. The worker prepares
one opaque handle from that response and binds it immediately around the intended wrapper
`Function::call`; wrapper statement one consumes it before argument encoding or other
model-controlled work. Replaying that handle or requesting a stale, expired, revoked, unknown, or
out-of-order call denies before stored source executes. There is no FIFO, pool, metadata lookup, or
ambient fallback from which one export can borrow another export's authority. Pure and effectful
exports use the same dispatcher seam; only methods present in the authoritative manifest are
installed on the hidden capability object. Async results never expose a private-realm
promise to the model realm. A Rust-owned settlement registry
carries only the bounded encoded result string (or a closed rejection) into a promise created from
the model realm's captured intrinsic, so its prototype and continuation ownership remain
model-local even when private promise bindings are shadowed. Effect ordinals and the 256-effect
limit belong to the whole fresh worker runtime request, not to individual nested or overlapping
capability tokens, and reset only when that disposable runtime is recycled.

File, fetch, proposal, and command operations retain their owning Phase 1–4 validation and limits.
On Windows, model-authored JS `spawn` uses the separately attested regular-AppContainer general
command sandbox, which owns and verifies the complete descendant lifetime. LPAC worker containment
never serves as containment for a brokered command. Learned-skill spawn remains disabled on
Windows because its stronger executable-manifest contract requires an immutable-executable
snapshot backend that Windows does not provide.

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
One complete `VerifyArtifact` request owns one fresh runtime. Within it, every embedded, mutation,
inherited, and held-out case reloads the artifact through that production loader in a fresh context
with fresh grants, fake state, transcript, and job drain. Mutation substitutes only the selected
post-validation export bridge; it does not introduce a second source wrapper or loader.

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

`src/extras/js/realm.rs` is the worker-owned implementation point for this contract. Before stored
source is evaluated it validates the full identity-v2 artifact and ABI, rejects invalid or
colliding export names, creates a new private `Context`, captures the pristine clone/bridge
intrinsics, and applies realm hardening. The exact stored source is evaluated as a Script, without
a generated function wrapper that could change its grammar. Effect, proposal, and module-system
globals are absent from the private context. The loader resolves declared bindings into its own
null-prototype namespace with own data properties; source-created getters or Proxies never become
the namespace boundary. Any initialization exception or queued job—including a job queued while
extracting exports or constructing wrappers—rejects the artifact without publishing an export.
Duplicate exports and every deterministic publication failure are rejected before the first model
global is mutated; an engine resource failure rejects the disposable request/runtime. Exact
declared functions are exposed to the model only through frozen, non-writable, non-configurable
wrappers whose arguments and results travel as strict-cloned, 64-KiB-bounded JSON strings. No
QuickJS object,
closure, symbol, accessor, cycle, promise, or host value is a boundary value. Capability-object
injection and promise-lifetime revocation remain a separate invocation-binding layer and may not
weaken this pure loader contract.

## Effect audit

Before every real brokered call—including `read_file`—the parent appends and durably syncs an
authorization/intent record. Audit failure denies the effect. The record is parent-attributed and
contains the invocation ID, authoritative artifact/export identity when applicable, grant ID,
normalized operation and redacted target metadata, policy decision, sequence, timestamp, and
previous-record hash. It contains no source, prompt, argument or file content, response body,
environment value, credential, secret, raw filesystem path, URL user information, URL path/query,
or command argument.

The broker owns one closed ordering for every typed operation:

1. validate the worker request and parent invocation/grant identity;
2. intersect the live grant with session policy, validate and authorize the exact target, and
   retain the prepared parent target;
3. derive redacted metadata from that prepared target, append the intent, and successfully sync it;
4. execute the prepared effect exactly once;
5. append and sync a bounded completion or explicit `outcome_unknown` record; and
6. only then return the effect response to the worker.

Wire effect ordinals are zero-based. Audit sequences are the checked, one-based representation of
those ordinals, while replay identity is derived from the parent invocation identity plus the
original ordinal. A repeated invocation/ordinal pair is denied before execution. Validation,
grant/session/target authorization, audit append, or audit sync failure executes no effect. A
completion append/sync failure returns the closed audit operational error, retires the invocation,
and leaves the already-durable intent available for conservative `outcome_unknown` recovery; it
never retries the effect. All brokers in one parent share one mutex-protected writer. A private
lock file also enforces one active production parent writer for this audit store across the
machine: a second parent fails audit initialization and cannot advertise JS while the first holds
the lock. The first initialization result, including lock contention or recovery failure, is held
in a process-wide `OnceLock`; that process does not retry. A process restart creates a new
initialization attempt and can recover after the other writer exits or the storage fault is fixed.

Grant expiry remains a lease boundary while waiting for that shared writer. The broker rechecks
expiry after acquiring the audit lock and before appending intent, so a grant that expires under
writer contention cannot reach either the audit or the effect backend.

Target correlation uses HMAC-SHA-256 with a dedicated parent-only audit key and an explicit
metadata allow-list; plain unkeyed hashes are forbidden because paths and URLs are often guessable.
Each tag covers a length-prefixed, domain-separated tuple of operation, metadata kind, and canonical
target bytes, and the record stores the full 32-byte tag. A file record stores only its storage
class, operation, and target tag for the canonical UTF-8 path—never a basename or path component. A
fetch record stores only method, normalized scheme/effective port, one target tag for the
normalized host, and a separate target tag for the canonical path and query; host labels, query
names, and query values are never stored in plaintext.
A command record stores its operation and a keyed digest of the resolved executable, with no
arguments or environment. Redirect targets require independent records. Brokered fetch currently
fails closed after the first response and before a redirected second send until independent-hop
records are implemented; the direct legacy fetch mode may still follow its separately authorized
redirect policy. The target-correlation
digest is separate from the audit chain hash, is domain-separated by operation and metadata type,
and supports equality correlation only; it cannot be used as authorization or reversed to recover
the target.

The audit target-correlation key is one fixed version-1 key created in private parent-owned
storage. It never enters the worker or diagnostics. Each segment open record binds that same key
version and key digest; size-driven segment rotation writes hash-chained close/open anchors and
does not rotate the key. Key rotation, multiple retained keys, and recovery across a key change are
not implemented and remain explicitly out of Phase 6 scope. A missing, unreadable, corrupt, or
unexpected required version-1 key makes audit recovery fail and JS remain unavailable rather than
falling back to plaintext metadata or an unkeyed digest.

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
for tests. Until a target's real containment launcher is implemented and its required local
attestation succeeds, its production status and every production launch attempt remain
unavailable.

| Platform | Required worker containment |
|----------|-----------------------------|
| Linux | Available only after a broker-only empty-root bubblewrap preflight proves isolated namespaces/environment, exact mounts, resource limits, and in-worker seccomp denial of process creation and execution. |
| macOS | Available on validated macOS 26 hosts with typed `DeprecatedBestEffort` assurance. The launcher descriptor-publishes a one-time image, unlinks it after authenticated Ready, gracefully shuts down and reaps the exact process, explicitly retires its lease/directory, and repeats the worker denial/readback plus guardian parent-death preflight. Other majors remain unavailable. |
| Windows | Available only after a cached minimal production attestation observes the LPAC/token shape, protocol handles, selected Job/mitigation state, closed protocol probe, fresh runtime, and clean shutdown for the same launcher used for real work. It does not establish filesystem, credential, network, actual-child, omitted-handle, or install-root denial. The hosted full canary/install-location gate is separate reference-runner evidence. |

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

Containment availability and the real backend are probed before JS is advertised. An available
Linux or Windows worker denies process creation/exec. Validated macOS 26 reports the weaker scoped
`DeprecatedBestEffort` assurance only after the complete live matrix below passes. A backend that
is absent, untrusted, misconfigured, unverifiable, or unable to apply every required restriction
makes JS unavailable.

### macOS standalone-CLI containment gate

The standalone macOS CLI reports available `Seatbelt` with typed `DeprecatedBestEffort` assurance
only on validated macOS 26 hosts after the complete live preflight passes. This boundary trusts the
parent-selected current executable and authenticated bootstrap, excludes hostile same-UID peer
processes, and treats request evaluation after authenticated Ready as untrusted. The earlier real
local probes on macOS 26 preserve the constraints that motivated the one-time-image transition:

- a deny-default profile without `process-exec` prevents `/usr/bin/sandbox-exec` from executing
  the initial worker image;
- allowing the exact initial image is not a one-time grant and remains usable for later exec; and
- macOS rejects applying a second, tighter Seatbelt profile after the first profile is active.

macOS 15 remains an explicit CI probe target rather than a validated runtime major. The production
allowlist must not classify it as validated until that runner has produced the same real-backend
evidence. Both CI rows first prove that the target-gated test exists, so an unsupported target or
an accidentally compiled-out test cannot pass as a zero-test success.
macOS 26 is allowlisted after the exact installed production binary emitted
`MACOS_CONTAINMENT_MATRIX_V1=passed` on macOS 26.5.2 for the complete worker denial/readback,
one-time-image lifecycle, and guardian parent-death matrix. Availability is still recomputed by
that same preflight, so a rejected profile or failed control returns typed unavailable.

The production launcher closes that scoped transition with a fresh one-time pathname. The public
`sandbox_init` API remains deprecated and the assurance therefore remains explicitly
`DeprecatedBestEffort`.

The production publisher creates an unguessable
`0700` directory under a caller-supplied private root using descriptor-relative operations, then
atomically creates a distinct single-link image from the pinned trusted source with Darwin
`fclonefileat(..., CLONE_NOOWNERCOPY)`. The APFS copy-on-write path avoids rewriting and durably
flushing the complete executable for every worker generation. If the pinned executable is on a
different or non-cloning volume, only `EXDEV` or `ENOTSUP` selects the descriptor-relative,
exclusive private-copy path; every other clone error fails closed. The publisher still independently
hashes both pinned descriptors, verifies source and destination metadata and distinct inodes,
changes the image to `0500`, and retains close-on-exec descriptors so cleanup and unlink remain
bound to the proven directory and inode. The destination must not exist, and a clone, reopen, ACL,
metadata, permission, synchronization, or hash failure cleans up or fails closed. Under the
approved scoped 2A boundary, the
parent-selected current executable and authenticated bootstrap are trusted; production therefore
uses that exact descriptor/metadata/SHA-256 proof and does not claim Security.framework identity as
an additional control. The stricter static-code test slice asks
Security.framework for the candidate CDHash set, constructs an exact CDHash requirement, and only
returns that identity after strict all-architecture validation succeeds. It checks the retained
descriptor, pathname metadata, and retained-descriptor SHA-256 both before and after the path-based
framework calls. The untrusted framework dictionary value is dynamically verified as an array of
one to eight distinct, exact 20-byte data values before any typed access; the generated requirement
is capped at 512 bytes before parsing. Candidate signing information is never accepted after a
validation failure.

Unit tests cover source/root rejection, exclusive copy-on-write cloning, secure cross-volume copy,
source/image mutation independence, publication permissions and identity, descriptor flags,
replacement detection, unlink refusal when an unexpected directory entry exists, and rejection of
tampered or malformed executables. Positive source/copy and distinct-system-image identity probes
exist but are ignored unless explicitly selected because macOS 26.5.2 on the development host
returns `CSSMERR_TP_NOT_TRUSTED` for pristine Apple system binaries, including `/bin/ls`, even with
an exact CDHash requirement. This is fail-closed evidence for that stricter test slice, not a
production signature claim and not permission to weaken a future boundary that does claim
Security.framework identity. The tests deliberately do not spawn `sandbox-exec` or claim
real-backend evidence.

A separate macOS-only module performs bounded crash-stale cleanup before publication. It validates
a private root, serializes sweeps by applying
an advisory `flock` directly to the validated, retained root directory descriptor without creating
a lock artifact, recognizes only canonical UUID publication directories, and uses a nonblocking
per-directory lease `flock` to skip live owners. Enumeration, candidate count, per-candidate
entries, and minimum age are bounded before any deletion. Only empty, lease-only, and
lease-plus-image (`0600` partial or `0500` sealed) states are recoverable; malformed names,
unexpected entries, metadata or ACL violations, hard links, and identity replacements fail closed.
The tests retain descriptors through identity revalidation, fsync removals, inject image and
directory replacements before the final revalidation, and exercise crash-state reclassification
and idempotence. They do not close the same-UID window between that validation and the final name
resolution by `unlinkat`; hostile same-UID peers are outside the approved scoped threat model.

Protocol v2 authenticates `WorkerReady` with a per-launch challenge. `WorkerChild` owns the exact
publication and lease, then unlinks and fsyncs the descriptor-proven image after that acknowledgement
and before sequence 2. Teardown removes the lease and directory. A trusted-current-binary guardian
owns the dedicated process group; parent-heartbeat EOF kills the group. The pre-exec boundary clears
the environment and installs the guardian heartbeat. Immediately after the trusted guardian exec,
before it starts a thread or launches the untrusted worker, the guardian closes every inherited
descriptor except protocol streams and the heartbeat and installs a 40 GiB virtual-address-space
ceiling, 35-second CPU, 64-descriptor, zero-core, and 1 MiB file-size limits. Darwin maps roughly
40 GiB of dyld shared-cache address space without making it resident, so smaller `RLIMIT_AS` values
are rejected on macOS 26; QuickJS retains its independent 64 MiB allocator cap. The deny-default
profile permits process execution only for the exact one-time image and denies network by omission.

The production-path lifecycle probe uses authenticated Ready, exact-image unlink, graceful
`Shutdown`, exact process reap, and fallible lease/directory retirement. Retirement propagates
unlink, `rmdir`, and directory/root `fsync` failures through ordinary supervisor teardown; `Drop`
is only last-resort cleanup and cannot turn a failed explicit retirement into availability.
Authenticated Ready alone is not containment evidence. The delivered non-libtest production-binary
matrix sends a typed post-unlink probe and emits one fixed source-free pass record only after the
contained worker attests workspace, skill-store, and credential sentinel read/write denial;
IPv4/IPv6 TCP and UDP denial; fork, alternate-exec, original-image, one-time-image, and `/dev/fd`
re-exec denial with exact error classes; the complete bounded pre-limit descriptor range;
resource-limit readback; and dedicated guardian process-group ownership. The parent additionally
requires graceful shutdown/reap/retirement, sentinel integrity, guardian parent-death whole-group
disappearance, and controlled one-day-policy stale recovery with the exact canary-owned publication
identity removed. Concurrent mini-agent publications are not misclassified as canary orphans.
The cached status path and hosted marker call the same full preflight. Only macOS 26 is currently
allowlisted; no other major is enabled until that matrix passes there. Enclosing sandboxes that
reject nested `sandbox-exec` remain a fail-closed environment-blocked observation. Every failure
remains typed unavailable; there is no uncontained fallback.

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

Windows handle inheritance is process-global state. The LPAC launcher and every production
standard-library, Tokio, or reviewed third-party process terminal in mini-agent use one crate-wide
creation lock. LPAC acquires it before the first handle is made inheritable and releases it only
after every intended child endpoint and excluded canary has had its inherit bit cleared or its
handle closed. Spawn/status command helpers acquire the same outer lock before entering Rust's
private Windows creation lock and release it immediately after synchronous spawn. The
standard-library output helper is the one synchronous exception: because
`Command` does not expose its stdio configuration for a faithful spawn-only reimplementation, it
holds the lock while `Command::output` completes, preserving explicit stdio and builder reuse. No
guard crosses an async suspension, and async/await/closure-deferred terminals cannot claim lexical
guard dominance; macro-contained raw terminals are likewise non-dominant because expansion can
defer execution. Inheritable-handle owners also clear their bit during drop on
every error path before the earlier-acquired lock guard is released. The checked subprocess
inventory recursively resolves parsed imports, type aliases, and local-module re-exports alongside
full-source tokens, treats glob imports and out-of-line modules as opaque, and inventories associated
terminal function-item references after normalizing raw identifiers. Terminal method identifiers in
macro inputs and locally defined `macro_rules!` bodies fail closed unless an exact inventory
identity proves the site non-process. The identity binds source path and occurrence to SHA-256 of the
unambiguously framed full macro-context chain. Each invocation structurally encodes exact path tokens
(including root qualification and raw identifier spelling), punctuation character and spacing,
token-tree kind, nested delimiter, and literal spelling without reconstructing a path or stringifying
the token stream. A matching terminal line, inner invocation, or macro name alone grants nothing;
macro-controlled terminals cannot inherit lexical guard dominance. It rejects
multiline, qualified-angle or renamed UFCS, and
ambiguous Windows-capable production terminals that bypass this boundary. A dedicated exact
multiset inventory for the creation helper itself requires
every raw standard-library, Tokio, and RMCP terminal to remain dominated by a retained crate guard.
Any future raw or third-party Windows launcher is inside the same boundary and must reuse it.

On a standard-user Windows checkout, prepare and run the complete gate with:

```powershell
cargo install --locked --no-default-features --features js --path . --debug
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$env:MINI_AGENT_LPAC_CARGO_INSTALL_EXE = Join-Path $cargoHome 'bin\mini-agent.exe'
cargo test --locked --no-default-features --features js windows_lpac_can_load_current_exe_with_only_protocol_handles -- --ignored --nocapture --exact
```

Construction of the production LPAC policy and attribute list is not runtime evidence: it cannot
prove that Windows accepts the combined token, Job, mitigation, handle, console, and executable
controls or that the resulting child observes them. Before advertising availability, the trusted
parent therefore performs one minimal sacrificial production attestation through the same raw
launcher, executable image, authenticated protocol, LPAC token, exact handle list, mitigation
attributes, and creation-time Job policy used by a real request. One process-wide `OnceLock` owns
the first status query and caches its result permanently. The parent verifies exact Job membership,
active-process limit one, 256 MiB per-process memory, CPU/UI/kill-on-close limits, selected
mitigations, and the queryable child-process restriction. After authenticated `Hello`/`Ready`, it
sends the closed connection-scoped `ContainmentProbe`; the child reports only `Passed` after
self-checking AppContainer plus less-privileged token state, zero capability groups, three distinct
valid pipe standard handles, and no console, window, or device authority. The parent then requires
`6 * 7` to evaluate to `42`, performs the closed shutdown, and requires successful exit. Any
failure or timeout becomes one fixed source-free unavailable reason. Later public launch never
retries under a weaker token or an uncontained launcher.

The caller's five-second wait starts before profile/ACL preparation and the global creation-lock
wait. The launcher propagates that absolute deadline and checks it immediately after acquiring the
creation lock and again immediately before `CreateProcessW`, so a lock released after caller
timeout cannot authorize a new launch. The preflight calls the raw launcher directly rather than
public launch/status or supervisor paths, and production never uses the executable overrides or
protocol children available to tests.

This bounded local attestation is the production availability decision, not evidence for
filesystem, credential, network, actual child-spawn, omitted-handle, or install-root denial. Those
ambient canaries belong only to the full hosted gate below and are intentionally absent from an
ordinary worker environment. LPAC is a token boundary, not a filesystem namespace: host objects
whose ACLs grant effective access to the stable AppContainer package identity can remain visible.
The broader hosted canaries record denial observations on the configured reference runner; they
do not prove that every Windows host has identical ACL visibility. While the
attestation is unavailable, normal launch returns a typed unavailable error and starts no
production worker. LPAC worker containment also does not satisfy the separate general-command
sandbox required by parent-brokered JS `spawn`.

The potentially blocking Windows creation call runs on one owned helper thread behind a five-second
caller-side deadline. Cancellation and timeout do not claim to interrupt the operating-system call.
If it returns late, the preflight helper retains sole ownership and tears down the result before it
exits. A permanently blocked creation call cannot be forcibly stopped and remains an explicit
availability residual; no second launch helper or worker may be created around it.

### Windows production containment and install-location gate

The production launcher constructs a zero-capability LPAC process with the exact protocol handle
list, child-process restriction, compatible mitigations, and creation-time Job limits. The cached
minimal attestation above is necessary for runtime availability. The ignored real-backend test
`windows_js_worker_containment` is the broader release gate and uses the same production launcher
to record denial-canary, supported-install-location, and lifecycle observations on the hosted
reference runner.

Evaluating Windows containment status during ordinary startup or `--print-config` performs
production preflight only when the `js` tool is eligible. Global tool suppression and allowlists
that omit `js` must not call the probe or initialize skill discovery. An eligible preflight creates
or reuses a stable AppContainer profile and, for a
supported current executable owned by the user, may add the package SID's exact read/execute ACE
to the installed executable. The production profile and committed ACE persist after process exit.
There is no automatic profile deletion, ACL rollback, explicit consent prompt, or cleanup command;
protected, other-user-owned, remote, reparse-point, and unsupported executable locations are
rejected instead of mutated. Merely querying status can therefore mutate supported Windows host
state even when no user JavaScript request is run.

The A27 test must observe all of the following on its hosted reference runner before emitting its
fixed, source-free pass frame:

- the child cannot read or write a workspace sentinel or a separately rooted skill-database
  sentinel and receives none of the parent's credential environment;
- zero-capability LPAC denies loopback TCP/UDP access and the creation-time child-process policy
  denies a second process;
- the three standard protocol handles are distinct anonymous pipes, while inheritable file and
  socket canaries omitted from `HANDLE_LIST` are invalid in the child;
- the process belongs to the exact creation-time Job, that Job reports active-process limit one,
  256 MiB per-process memory, the reviewed CPU-time ceiling, kill-on-close, and all UI
  restrictions, and closing it terminates and reaps the worker;
- the effective mitigation policy contains every queryable reviewed A26 mitigation and no
  unproved Win32k or dynamic-code prohibition. Heap-corruption termination remains a mandatory
  creation-time policy bit, but Windows exposes no post-creation query for that legacy bit, so the
  gate does not represent it as separately observed runtime state;
- a complete authenticated worker protocol exchange reaches `Ready` and evaluates a trivial
  request through a fresh runtime; and
- creation succeeds when the test parent is already in a compatible Job; when it is not, the probe
  first enters a private compatible outer Job. If Windows cannot apply the required nested
  creation-time Job list, the gate fails closed and worker status remains unavailable.

The install matrix uses the exact debug artifact produced by
`cargo install --locked --path . --debug --no-default-features --features js`. Cargo installation
and extracted-archive cases must include spaces and non-ASCII path components. Cargo-home and
user-owned local archive locations are only supported after the full matrix passes. A copy under a
protected machine-wide root is a mandatory negative control: the launcher must report the
location unsupported and must not add or widen an ACL. UNC, remote, reparse, other-user-owned, and
unknown roots remain unsupported.

After an administrator has staged an unchanged negative-control copy beneath `%ProgramFiles%`, a
standard user runs the complete A27 gate. When the test host is not already in a Job, the probe
first assigns itself to a private compatible outer Job so the production child exercises nested
creation-time Job assignment:

```powershell
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$env:CARGO_HOME = $cargoHome
$env:MINI_AGENT_LPAC_CARGO_INSTALL_EXE = Join-Path $cargoHome 'bin\mini-agent.exe'
$env:MINI_AGENT_LPAC_PROTECTED_EXE = 'C:\Program Files\mini-agent-a27-gate\mini-agent.exe'
cargo test --locked --no-default-features --features js windows_js_worker_containment -- --ignored --nocapture --test-threads=1
```

The user running the test must not own or be able to modify the protected copy or delete it through
its parent. The gate verifies owner and effective current-user rights, confirms that a write handle
is access-denied, snapshots the file owner and DACL before and after the rejected preparation, and
then exercises only private disposable copies for the supported user-owned rows.

The `windows-worker-containment-gate` CI job first lists tests and requires exactly one target-gated
`windows_js_worker_containment` test plus at least one `worker_runtime` test, so a cfg mistake or
filter drift cannot pass by running zero tests. It installs beneath a path containing spaces and
Unicode, prepares the protected-location negative control, runs the ignored real probe, exercises
the worker runtime, and invokes the installed binary's `--print-config`. Its separate standard-user
lane also installs the binary and requires that identity's own `--print-config` to report the same
cached available/enforced status before it may emit pass evidence. Raw standard-user output is
deleted rather than archived.
CI run 31319107422 records the cached production attestation, full canary, supported user-owned
install locations, protected-location negative control, and standard-user `--print-config` result.
Hosted reference-runner evidence does not prove every current host has identical ACL visibility.
No restricted-token or unconfined fallback is allowed, and this gate does not satisfy the separate
general-command sandbox, whose own native gate passed independently in the same run.

## Failure semantics

Every production failure uses a closed sanitized diagnostic contract. `class` is one of `syntax`,
`javascript_exception`, `promise_rejection`, `host`, `permission`, `validation`, `timeout`,
`cancelled`, `out_of_memory`, `pending_job_limit`, `protocol`, `containment`, `audit`, or
`internal`. `code` comes from a versioned parent/worker allow-list; a worker-provided unknown code
is a protocol fault. Optional corrective metadata is limited to a source-free location: closed
`stage` and `script_role` enums plus validated one-based numeric line/column values within the
submitted script. It contains no filename, function name, property/key name, target, ordinal,
effect result, or other source-derived string.

Worker reuse is a parent-owned, deterministic decision. A successful value or void result and the
explicitly allowlisted `syntax`, `exception`, and `invalid_result` JavaScript errors may leave the
contained process warm, but the worker still creates a fresh QuickJS `Runtime` for the next
request. Stack/job resource errors, internal errors, JavaScript timeout/OOM terminals, and
verification results containing a `resource_limit` or `internal` diagnostic poison the process
even though their terminal frames are well formed. A watchdog timeout, cancellation, malformed or
invalid-state frame, build/version mismatch, stale generation, unexpected verification effect,
transport failure, process exit/signal/panic, or shutdown fault also kills and reaps the complete
containment tree and erases all invocation grants before another request can launch. Warm
processes are retired after 256 completed requests or 15 minutes, whichever comes first. The age
deadline is enforced by an independent idle-retirement timer, so the parent reaps an expired idle
worker even when no later request arrives. Clean shutdown retires the idle process and a later
request starts a fresh generation.

An unavailable audit prevents broker construction and therefore sends no request to a worker. An
effect whose durable completion is `outcome_unknown` immediately erases invocation authority,
closes that invocation in both protocol state machines, and forces process recycle without retry.
JavaScript cannot catch that error and dispatch another effect: a caught second call fails locally
inside the closing worker and never reaches the parent broker. Caller cancellation and the shared
absolute deadline signal the active parent effect and give it a separate bounded drain window
before authority is erased. That drain lets the service kill and reap an owned subprocess tree,
stop fetch work, hand off a bounded proposal waiter, and append the truthful durable completion. Protocol
continuation is never attempted after an interrupted effect; the worker is recycled and the next
invocation starts with fresh grants and process state.

Cancellation while waiting for the serialized worker lease, target normalization, or permission
`Ask` aborts before mutation and returns `cancelled` (or `timed_out`). Read cancellation is likewise
exact because it is non-mutating. A write cancelled before its mutation future is polled is exact;
after open/write dispatch, cancellation or deadline is `outcome_unknown` because atomic replacement
does not imply transactional rollback. The blocking atomic writer is drained; cancellation and the
final rename serialize through one publication gate, so cancellation returning proves a later
publication cannot begin. If the writer wins that atomic start decision, cancellation remains
bounded while the already-approved syscall may finish and the result stays `outcome_unknown`. JS
receives `spawn` authority only when the configured process sandbox
owns the complete descendant lifetime independently of process-group membership (currently the
Linux bwrap PID namespace). Elsewhere spawn fails closed before intent. Within that boundary, spawn
cancellation signals the command-specific token, kills the process group and containing namespace,
reaps the direct child, and then records `outcome_unknown` because the program may already have
changed external state. Fetch preserves its one outer wall-clock
deadline across permission, DNS, connect, response headers, and body; cancellation after HTTP
dispatch is ambiguous. These rules do not extend cancellation to MCP, LSP, or the general agent
loop and do not promise exactly-once execution.

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
drains the response; the broker durably reconciles that ambiguous outcome before recycling the
invocation. Callers must not replay that proposal automatically.

## Acceptance matrix

The matrix defines required evidence. Phase 6 is delivered under the index exit rule using the
dedicated containment gates and three platform records from CI run 31319107422 plus the checked-in
aggregate independently revalidated at commit `9c6f164`. Reference-runner observations are
recorded with their platform and residual-risk qualifications; they are never generalized into
unmeasured host guarantees.

| Contract area | Required acceptance evidence |
|---------------|------------------------------|
| Threat model | Hostile worker tests prove forged identity, artifact hash, sequence, and grant data cannot expand parent-owned authority. |
| Worker lifecycle | Tests prove one contained process, one fresh runtime per `RunStep`/whole `VerifyArtifact`, limits installed before eval, bounded jobs/output, and kill-and-reap cancellation. |
| Wire protocol | Codec/state-machine tests reject oversized, malformed, unknown, replayed, out-of-order, cross-invocation, and wrong-terminal frames. |
| Capability broker | Permission/policy/grant intersection tests cover every typed effect, expiry, scope, Windows spawn denial, and malicious attribution. |
| Persistence boundary | Tests prove no worker database/path authority, pure initialization, no writer in stored realms, identity-v1 quarantine, and parent-only canonical persistence. |
| Verification parity | The QuickJS realm gate passes; production and all verifier modes use one loader/ABI path with only declared deterministic fake capabilities and the same sanitized typed diagnostic contract. |
| Effect audit | Recovery and failure-injection tests prove durable intent before every real effect, bounded completion, hash-chain integrity, fixed version-1 HMAC target correlation/redaction, version-1 key failure, segment rotation/anchors, bounded retention, machine-wide single-writer exclusion, process-restart retry semantics, and no replay. Key rotation remains out of scope. |
| Platform containment | CI run 31319107422 passes the dedicated real empty-root Linux probe, the macOS 15 fail-closed probe, the validated macOS 26 production-binary denial/guardian live matrix, the Windows cached-attestation/full-canary/supported-install-location matrix, and the separate Windows general-AppContainer gate. The macOS 26 and Windows hosted results apply to their reference runners only. |
| Resource baseline | The three platform records from CI run 31319107422 were independently aggregated and schema-validated by the final validator at commit `9c6f164`; the reviewed result is checked in at [`../benchmarks/results/js-worker-baseline.json`](../benchmarks/results/js-worker-baseline.json) under the method in [`../benchmarks/js-worker.md`](../benchmarks/js-worker.md). It records one worker and zero idle runtimes per measured platform. Timing and memory target booleans remain informational unless a matched-host repeat is explicitly promoted; the enforced native ceilings are verified separately. |
| Failure semantics | Crash, OOM, timeout, cancellation, audit failure, backend absence, parent death, ambiguous-effect, and secret-in-thrown-value tests all fail closed with only stable class/code and source-free location metadata. |
| Corpus consistency | The exact Phase 1–6 documentation scan shows all surviving in-process/thread claims as historical or superseded, removes stale platform and path claims from current documentation, and records delivered status consistently. Superseded dated blueprints and implementation plans remain explicitly historical. |
