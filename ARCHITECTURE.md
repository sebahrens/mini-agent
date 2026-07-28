# JS Engine Integration — Architecture Overview

- **Document role**: non-normative architecture overview
- **Overview version**: 1.0.0
- **Delivery status**: mixed; see the normative index
- **Owner**: mini-agent maintainers
- **Last reconciled**: 2026-07-29

The sole normative JS corpus is
[`docs/specs/00-index.md`](docs/specs/00-index.md) and the phase specifications it indexes. If this
overview differs from an indexed specification, the indexed specification controls and this file
must be corrected.

## Purpose

mini-agent embeds QuickJS through `rquickjs` as a bounded code-action primitive. The in-process
engine gives the model a compact JavaScript surface and later serves as the runtime for immutable,
retrieved skills.

This architecture supplements existing tools. It does not itself remove Bash, port shell hooks,
or prove full Windows readiness.

## Stable engine boundaries

- Each `JsTool` instance owns one dedicated OS thread with an 8 MiB thread stack.
- QuickJS `Runtime`, `Context`, and values remain on that thread; every `JsTool` field is
  `Send + Sync`.
- Every step creates and drops a fresh runtime.
- Every runtime receives the 64 MiB heap limit, 512 KiB JS stack limit, and an interrupt deadline
  before evaluation.
- Evaluation retains the returned `Value`, extracts bounded exception stack text, and drains the
  pending-job queue under deadline and job-count bounds.
- Blocking host calls have independent deadlines and cancellation because the QuickJS interrupt
  handler runs only during bytecode execution.

The normative runtime contract is
[`phase-1-js-engine.md § Runtime lifecycle`](docs/specs/phase-1-js-engine.md#runtime-lifecycle).

## Threading model

```text
[Tokio / agent]                         [dedicated js-engine thread]
JsTool::call(request)
  ├─ send bounded request ────────────→ create fresh Runtime + Context
  ├─ service permission bridge ←────── host global requests authorization/effect
  └─ await bounded reply ←──────────── eval, drain jobs, return outcome
```

The synchronous JS thread never owns the asynchronous permission policy. A bounded bridge carries
permission requests to Tokio and propagates denial, non-interactive `Ask`, backend failure,
timeout, cancellation, and shutdown distinctly.

## Primitive host surface

Phase 1 exposes `read_file`, `write_file`, `spawn`, and bounded `console.log`. It permanently omits
`require`, `import`, and `final_answer`; Phase 2 may add permission-gated `fetch`.

Both file globals always require permission for a securely resolved target:

- reads bind approval to canonical target identity and use a bounded stable no-follow read;
- writes bind approval to the resolved final target and use descriptor-relative, no-follow atomic
  publication; and
- a Phase 2 path allow-list can only narrow access, never grant it.

`spawn()` always follows the existing process permission policy and creates the command through
`Sandbox::wrap_command`. Permission and wrapper routing do not imply that an OS isolation backend
is effective.

The normative host contract is
[`phase-1-js-engine.md § Host globals`](docs/specs/phase-1-js-engine.md#host-globals).

## Isolation and platform boundary

The QuickJS VM and a spawned child are separate trust boundaries:

| Boundary | Phase 1 | Phase 2 |
|----------|---------|---------|
| QuickJS VM | hard heap/stack/time/job bounds; no ambient module system | unchanged |
| File host calls | secure resolution + mandatory permission | optional narrowing allow-lists; permission remains mandatory |
| Process creation | mandatory permission + shared wrapper; effective backend is runtime-dependent | verified effective isolation on Linux/macOS |
| Network | no global | bounded, allow-listed, permission-gated `fetch` |
| Windows process isolation | not delivered | not delivered |

Windows readiness requires separate verified process-isolation/lifecycle work plus the storage,
ACL, archive, migration, CI, and release gates in the normative corpus. The portable in-process
engine does not turn an unisolated child process into a sandbox.

The normative hardening contract is
[`phase-2-sandbox.md`](docs/specs/phase-2-sandbox.md).

## Feature relationships

- `js` enables the Phase 1 engine.
- `sandbox` is independent of `js` and extends the shared process sandbox.
- `skills` implies `js` but not `sandbox`.
- `mcp` remains independent and retains its own discovery and permission path.

A feature combination compiling does not mean its owning phase has passed its exit gate. See
[`00-index.md § Feature relationships`](docs/specs/00-index.md#feature-relationships).

## Skill architecture

Phase 3 introduces two deliberately separate forms of reusable capability:

1. Open Agent Skills are validated instruction/resource packages discovered progressively.
2. Learned JavaScript skills are immutable verified artifacts bound into a frozen turn bundle.

A learned artifact ID is the full SHA-256 of a versioned canonical payload covering source,
ordered embedded tests, ordered exports/signatures, retrieval description/tags, capability
manifest, and identity schema version. Operational status, timestamps, telemetry, embeddings, and
lineage are outside identity.

Retrieval occurs once before model generation using the current user prompt and bounded
deterministic context. The model sees a compact metadata manifest; every JS call in that turn
receives the same immutable source snapshot. The JS thread does not query SQLite or generate
embeddings.

The normative storage/retrieval contract is
[`phase-3-skill-library.md`](docs/specs/phase-3-skill-library.md).

## Verification and lifecycle ownership

All candidate verification runs in a fresh bounded no-effect context:

- Tier 0 receives no host globals.
- Tier 1/2 receive only declared deterministic in-memory fakes.
- At least one embedded test is required, and every test must evaluate to the exact JavaScript
  boolean `true`.
- Mutation checks require each declared export to affect at least one test.
- Trusted held-out suites remain hidden from proposing agents.

Phase ownership is intentionally asymmetric:

- Phase 3 owns immutable identity, no-effect verification, and manual admission.
- Phase 4 owns bounded agent proposals, independent evaluation, and explicit human approval into a
  non-retrievable canary. It has no automatic activation path.
- Phase 5 owns directly attributed evidence, deterministic replacement routing, allowed Tier 0/1
  automatic promotion, automatic quarantine, immutable repair, supersession, and rollback.
  Write/process/network capabilities always retain a human gate.

The normative lifecycle contracts are
[`phase-4-auto-admission.md`](docs/specs/phase-4-auto-admission.md) and
[`phase-5-evidence-learning.md`](docs/specs/phase-5-evidence-learning.md).

## Storage architecture

One typed `AppPaths` resolver owns configuration, portable data, local durable data, state, cache,
credentials, and project roots. Learned-skill databases and evidence are machine-local; model
downloads and rebuildable indexes are cache; OAuth material has a separate private credential
root. Persistent modules do not select roots from the current working directory.

Portable Agent Skills use the open `SKILL.md` directory format. ZIP is a validated transport,
`allowed-tools` grants no authority, and bundled scripts do not become learned JS without the
normal identity, verification, capability, admission, and evidence gates.

The normative storage contract is
[`platform-paths.md`](docs/specs/platform-paths.md).

## Current source map

Production source is at the repository root:

```text
src/extras/js/
├── engine.rs
├── host.rs
├── mod.rs
├── tool.rs
├── types.rs
├── tests/
└── skills/        # Phase 3 target
```

Tool registration lives in `src/agent/builder.rs`; the shared process wrapper lives in
`src/sandbox.rs`. Source line numbers are deliberately omitted because they are not specification
identifiers.
