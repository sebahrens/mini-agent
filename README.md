# mini-agent / JS Engine Integration

Minimal coding agent with a bounded, brokered JavaScript runtime. QuickJS executes only in a
contained same-executable worker; the parent retains permissions, effects, persistence, and audit.

## Repository layout

| Path | Purpose |
|------|---------|
| `src/` | Production mini-agent source |
| `src/extras/js/` | Brokered JS runtime, parent supervisor/effects, skills, and worker entry |
| `spike/` | QuickJS proof-of-concept research |
| `docs/specs/` | Normative phased JS specifications and superseded research |
| `ARCHITECTURE.md` | Non-normative architecture overview |
| `SPEC.md` | Non-normative implementation overview |

The old nested `zerostack/` layout was flattened. Production source and the workspace
`Cargo.toml` are at the repository root.

## Canonical executable

The Cargo package, installed CLI, and every release archive use the executable name
`mini-agent`. Target-specific full and lite archives are named
`mini-agent[-lite]-<target>.tar.gz`, and each archive contains exactly one executable at
`mini-agent`.

## Documentation authority

Start with the [normative specification index](docs/specs/00-index.md). It defines corpus
authority, feature relationships, phase dependencies, and exit semantics.

- [Foundation: paths and persistent storage](docs/specs/platform-paths.md)
- [Phase 1: core JS engine](docs/specs/phase-1-js-engine.md)
- [Phase 2: sandbox hardening](docs/specs/phase-2-sandbox.md)
- [Phase 3: skill library](docs/specs/phase-3-skill-library.md)
- [Phase 4: agent proposals and human-gated admission](docs/specs/phase-4-auto-admission.md)
- [Phase 5: evidence-based self-learning](docs/specs/phase-5-evidence-learning.md)
- [Phase 6: brokered JS runtime](docs/specs/phase-6-brokered-js-runtime.md)
- [Subprocess trust classes](docs/specs/subprocess-trust.md)

[ARCHITECTURE.md](ARCHITECTURE.md) and [SPEC.md](SPEC.md) are maintained overviews only. The
[dated JS blueprint](docs/specs/2026-07-27-js-engine-blueprint.md) is a superseded research
artifact retained for history and must not guide implementation.

## Core boundaries

- The parent lazily supervises at most one contained same-executable worker and serializes its JSON
  pipe protocol. QuickJS types never exist in the production parent.
- Every step and whole verification request creates and drops a fresh bounded `Runtime`; every
  verification case receives a fresh `Context`.
- File, fetch, proposal, and command effects execute only in parent services after exact grant,
  session permission, target narrowing, durable intent, and deadline checks.
- A native-compromised worker can attempt to borrow the union of current-step grants. Source-level
  private realms are not a native security boundary; platform containment removes ambient host
  authority and the parent remains the trusted computing base.
- An effect interrupted after dispatch may be recorded as `OutcomeUnknown`. It is never retried
  automatically, and the worker and all invocation authority are recycled.
- Parent-brokered process spawn uses `Sandbox::wrap_command`; the worker itself uses a separate,
  workspace-invisible, broker-only launcher.
- Learned-skill identity version 2 covers the full versioned execution/discovery payload, ABI,
  ordered tests, and structured target scopes; only exact JavaScript boolean `true` passes a test.
- Phase 4 requires human approval into non-retrievable canary state. Phase 5 owns the limited,
  evidence-based automatic lifecycle.

## General subprocess sandbox

Sandboxing and memory support are included in the default build, and the general subprocess
sandbox is on by default. `--no-sandbox` disables it. While sandboxing remains enabled,
`--sandbox`, `sandbox = true`, or selecting a backend through `--sandbox-backend` or
`sandbox-backend` makes the request explicit: if that backend is unavailable, startup fails
closed. On non-Windows hosts, when only the default requested sandbox is unavailable, mini-agent
warns and continues unsandboxed so hosts without the platform backend can still start. An
unsandboxed subprocess inherits the host filesystem, environment, devices, process namespaces,
and network.

The default general-process backend is platform-specific:

| Platform | Default backend |
|----------|-----------------|
| Linux | `bwrap`, when an installed trusted binary passes the real preflight |
| macOS | System-provided Seatbelt at `/usr/bin/sandbox-exec` on supported macOS hosts |
| Windows | `appcontainer` candidate; selected by default but production-unavailable pending native hosted attestation (`restricted-token` is a compatibility alias) |

With the Linux `bwrap` backend, mini-agent applies this capability matrix:

| Capability | Enforced Linux `bwrap` policy |
|------------|---------------------------------|
| Filesystem reads | Current workspace, mini-agent's application cache, explicit system runtime roots (`/usr`, `/bin`, `/sbin`, `/lib*`, `/nix`), `/etc/localtime`, `/etc/ld.so.cache`, and kernel/system metadata exposed by the new `/proc` |
| Filesystem writes | Current workspace, mini-agent's application cache, and a private ephemeral `/tmp`; the remaining sandbox root and runtime mounts are read-only |
| Processes | Separate user, PID, IPC, UTS, and cgroup namespaces |
| Devices | A minimal synthetic `/dev`; host devices are not mounted |
| Environment | Cleared and rebuilt from `PATH`, identity, shell, terminal, locale, and color variables; credential, agent-socket, display, and API-key variables are not forwarded |
| Network | Separate IP network namespace with no host or external IP connectivity, including no access to host loopback listeners; filesystem Unix-domain sockets placed in the workspace or application cache remain reachable |

Only the workspace path selected by the process working directory is exposed; running mini-agent
from a broad or sensitive directory broadens that boundary. The sandbox does not provide seccomp,
CPU/memory quotas, filtering of Unix-domain sockets inside writable bind mounts, or confidentiality
from host kernel metadata. Only a root-owned, non-group/world-writable `bwrap` executable beneath
equally protected parent directories is trusted. If it is missing or any namespace or mount setup
fails, the subprocess does not run. The optional `zerobox` backend has
backend-defined read/process/device/environment/network behavior and is not reported as providing
the Linux `bwrap` guarantee.

Use `mini-agent --sandbox-backend bwrap --print-config` to see the configured backend,
availability, and effective capability report. Parent-brokered, permission-gated HTTP fetches are
a separate boundary; subprocess network isolation does not bypass or replace fetch permissions.

On Windows, `appcontainer` is currently a hosted candidate, not an available production
backend. Startup fails closed unless the operator explicitly selects `--no-sandbox`. The native
probe exercises explicit package-SID access: the workspace is read/write, while the application
cache and exact selected executable are read/execute only. No ambient `PATH`, home, Cargo, or
Rustup root is inferred. Operators may add bounded AppContainer-only read or write roots explicitly;
relative values resolve from the workspace, and remote, reparse, hard-link, or permission-widening
overlaps fail closed. A unique
zero-capability AppContainer identity, handle-bound recursive ACL updates, bounded crash-stale
cleanup journal outside every granted tree, private OS-managed temporary storage, locked executable
proof, private desktop, and creation-time bounded Job all fail closed. ACL/profile removal starts
only after the exact Job reports zero active processes. Crash recovery records a unique parent-only
named Job and opens that exact object before stale cleanup; uncertain state retains the journal and
authority. The control directory has an owner-only protected DACL and is denied to the target. Any
cleanup failure retains the journal for stale recovery. No network capability is granted; hosted acceptance also requires zero token
capabilities, no loopback exemption, IPv4/IPv6 TCP/UDP loopback and external denial, outside
read/write denial, omitted-handle denial, and parent-owned descendant cleanup. Registry/device/
session isolation beyond the stated AppContainer and Job boundaries is not claimed. Use
`mini-agent --sandbox-backend appcontainer --print-config` for the fail-closed capability report.

## Broker-only JavaScript worker

The worker boundary is mandatory and independent of the optional general subprocess setting:

| Platform | Production JS status |
|----------|----------------------|
| Linux | Available only after a real empty-root `bwrap` preflight proves trusted runtime mounts, isolated namespaces/network, rlimits, non-dumpability, `no_new_privs`, and seccomp process/exec denial. The workspace, cache, configuration, credentials, and ambient environment are absent. |
| macOS | Unavailable. The real Seatbelt probe proves that allowing the stable image for initial execution also leaves it reusable for later exec; deprecated best-effort Seatbelt is not accepted as a fallback. |
| Windows | Available only after a process-wide cached minimal production attestation observes the LPAC/token shape, exact protocol handles, selected Job/mitigation state, protocol probe, fresh runtime, and clean shutdown. It does not test ambient filesystem/network/credential/actual-child denial or install roots; the full hosted canary records those observations only for its reference runner, and its final artifact remains pending. Model-authored JS `spawn` uses the separate general AppContainer backend; learned-skill spawn still requires an immutable-executable backend and remains disabled. |

Hooks, MCP servers, LSPs, loop validation, and the explicit interactive shell are separate trust
classes with different workspace, credential, and lifecycle needs. They never inherit the
broker-only worker profile merely because they also create processes.

On Windows, startup and `--print-config` status checks create or reuse a persistent AppContainer
profile and may add a persistent exact read/execute ACE to a supported, user-owned installed
executable. LPAC is not a filesystem namespace, and no automatic cleanup, ACL rollback, or consent
prompt is provided.

## Phase status

| Phase | Scope | Status |
|-------|-------|--------|
| Foundation | Typed paths, ownership, migration, platform security | In progress |
| 1 | Core JS engine, host globals, permissions, process wrapper | Delivered |
| 2 | `fetch`, narrowing allow-lists, Linux/macOS process isolation | Delivered |
| 3 | Agent Skills, immutable learned skills, and prompt-time retrieval | Delivered |
| 4 | Agent proposals, held-out evaluation, human-gated canary | Delivered |
| 5 | Evidence, promotion, quarantine, repair, rollback | Delivered |
| 6 | Contained worker, protocol, parent broker/audit, private realms | Implementation complete; final evidence pending |

## Cargo features

| Feature | Implies | Adds |
|---------|---------|------|
| `memory` | — | Project memory loading, editing, and context injection |
| `js` | — | Brokered QuickJS worker plus parent-owned effect globals (`rquickjs`) |
| `sandbox` | — | Shared Linux/macOS general-process isolation; with `js`, enables parent-brokered `spawn` where complete descendant containment exists and the permission-gated `fetch` global |
| `skills` | `js` | Agent Skills catalog plus learned-skill store, hybrid retrieval, and no-effect verifier |
| `skills-embed` | `skills` | Local BGE embedding inference (`fastembed` → ONNX Runtime) |
| `skills-embed-dynamic` | `skills-embed` | Links ONNX Runtime at run time via `ORT_DYLIB_PATH`, for hosts without prebuilt `ort-sys` binaries |
| `mcp` | — | MCP client transports and tool discovery |

Selecting `skills` automatically enables `js`; a skills-without-JS build is not
selectable. `skills` alone uses an offline deterministic embedding backend, so it
builds everywhere.

**Platform caveat:** `skills-embed` pulls `ort-sys` (ONNX Runtime), which ships no
prebuilt binaries for some hosts — notably `x86_64-apple-darwin`. On those hosts
build with `skills-embed-dynamic` and point `ORT_DYLIB_PATH` at a local ONNX
Runtime (`brew install onnxruntime`). Either way it stays opt-in and out of
required CI because of the native download. For real embeddings with no local
model at all, `[embedding] backend = "external"` reuses the OpenRouter endpoint
and key the LLM already uses; see [`docs/agent/CONFIG.md`](docs/agent/CONFIG.md).

Supported combinations exercised in CI are the default build and these focused
`--no-default-features` rows: no optional features, `memory`, `js`, `sandbox`,
`skills` (which proves `skills` implies `js`), `js,sandbox`, `mcp`, `js,skills`,
and the full supported core row `mcp,js,sandbox,skills,memory`. The feature-graph
gate also verifies that optional dependencies are absent whenever their owning
feature is disabled. `skills-embed` remains an explicitly non-blocking native
backend row for the platform reasons above.

## Development commands

From the repository root:

```bash
cargo fmt
cargo test --no-default-features --features js
cargo install --path . --debug
```

Do not use `cargo build`, `cargo check`, or development `--release` builds.
