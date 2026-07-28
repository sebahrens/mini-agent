# JS Engine — Implementation Overview

- **Document role**: non-normative implementation overview
- **Overview version**: 1.0.0
- **Delivery status**: mixed; see the normative index
- **Owner**: mini-agent maintainers
- **Last reconciled**: 2026-07-29

This document is a maintained overview, not a generated contract. The sole normative JS corpus is
[`docs/specs/00-index.md`](docs/specs/00-index.md) and the phase specifications it indexes.
Implementers must cite and follow the owning normative section. Code snippets, tracker text, dated
blueprints, and this overview cannot override it.

## Foundation — paths and persistence

Normative specification:
[`docs/specs/platform-paths.md`](docs/specs/platform-paths.md)

- Construct one typed `AppPaths` resolver at startup.
- Keep configuration, portable data, machine-local data, state, cache, credentials, and project
  roots distinct.
- Store the learned-skill database and lifecycle evidence under machine-local data.
- Validate directory/ZIP Agent Skills without execution and without trusting archive names.
- Treat `allowed-tools` as metadata only.
- Qualify Windows storage/security support until resolver, ACL, migration, archive, CI, and release
  gates pass.

## Phase 1 — core JS engine

Normative specification:
[`docs/specs/phase-1-js-engine.md`](docs/specs/phase-1-js-engine.md)

Implementation areas:

| Concern | Location |
|---------|----------|
| Runtime lifecycle, eval, and pending jobs | `src/extras/js/engine.rs` |
| Host globals, secure file effects, and spawn | `src/extras/js/host.rs` |
| Tool lifecycle and permission bridge | `src/extras/js/tool.rs` |
| Request/response types and limits | `src/extras/js/types.rs` |
| Registration | `src/agent/builder.rs`, `src/extras/mod.rs` |

Stable requirements:

- one dedicated 8 MiB OS thread per `JsTool`;
- only `Send + Sync` fields in `JsTool`, with no QuickJS state crossing threads;
- one fresh runtime per step with the 64 MiB heap limit, 512 KiB JS stack limit, interrupt
  deadline, `Value` evaluation, bounded stack extraction, and pending-job drain;
- independent timeout/cancellation for every host call;
- mandatory permission on securely resolved reads and writes;
- mandatory process permission plus `Sandbox::wrap_command` for every spawn; and
- no `require`, `import`, `fetch`, or `final_answer` in Phase 1.

VM isolation and child-process isolation are distinct. Phase 1 does not promise an effective
macOS or Windows process sandbox.

## Phase 2 — sandbox hardening

Normative specification:
[`docs/specs/phase-2-sandbox.md`](docs/specs/phase-2-sandbox.md)

- Keep `sandbox` independent from `js`; combined features enable the JS integrations.
- Add bounded HTTP(S)-only `fetch` with URL/redirect validation, narrowing allow-lists, mandatory
  `js/fetch` permission, and finite deadlines.
- Apply file allow-lists only to resolved targets and only as a restriction before the mandatory
  Phase 1 permission.
- Extend the shared process wrapper and verify effective isolation on Linux and macOS.
- Do not claim Windows process isolation; it is outside Phase 2.

## Phase 3 — skill library

Normative specification:
[`docs/specs/phase-3-skill-library.md`](docs/specs/phase-3-skill-library.md)

- `skills` implies `js` and does not imply `sandbox`.
- Keep portable Agent Skills separate from learned JavaScript artifacts.
- Use the full 64-character SHA-256 of the versioned canonical execution/discovery payload,
  including source, ordered tests, ordered exports/signatures, description/tags, capability, and
  identity version.
- Verify in a fresh no-effect context. Tests are nonempty and every expression must return exact
  JavaScript boolean `true`; mutation checks cover every export.
- Precompute embeddings, combine exact dense and lexical retrieval, and freeze one bundle before
  model generation for the whole user turn.
- Admit manually verified artifacts as active in Phase 3; reserve proposal/canary states for later
  phases.

## Phase 4 — agent proposals and human-gated admission

Normative specification:
[`docs/specs/phase-4-auto-admission.md`](docs/specs/phase-4-auto-admission.md)

The stable filename predates the final ownership split; Phase 4 does not auto-activate code.

- Bound and durably enqueue structured proposals without evaluating them on the submitting
  runtime.
- Reload persisted bytes, recompute full identity, and run embedded, inherited, mutation, and
  independent held-out gates in no-effect verification.
- Keep held-out fixtures outside proposing-agent visibility and control.
- Require an explicit authenticated human action to enter non-retrievable canary state.
- Never mark an agent proposal active in Phase 4.

## Phase 5 — evidence-based lifecycle

Normative specification:
[`docs/specs/phase-5-evidence-learning.md`](docs/specs/phase-5-evidence-learning.md)

- Attribute selected, injected, invoked, returned, thrown, timeout, OOM, policy, and targeted
  feedback events without retaining raw prompts, arguments, or file content.
- Route replacement canaries deterministically after lineage retrieval and before manifest
  construction.
- Permit evidence-gated automatic promotion only for eligible pure/read-only replacements.
- Keep write/process/network revisions human-gated.
- Quarantine severe integrity/capability failures immediately and behavioral failures only from
  direct evidence with a minimum sample.
- Create repair as a new immutable revision; make promotion, supersession, rollback, evidence, and
  index-generation changes transactional.

## Phase and feature matrix

| Capability | Required completed phase | Cargo relationship |
|------------|--------------------------|--------------------|
| Primitive bounded JS | Phase 1 | `js` |
| Linux/macOS process hardening and `fetch` | Phase 2 | `sandbox` independent; integration uses `js,sandbox` |
| Manual learned-skill retrieval | Phase 3 + Foundation | `skills` implies `js` |
| Agent proposal to human-approved canary | Phase 4 | extends `skills` |
| Evidence-based lifecycle automation | Phase 5 | extends `skills` |

Compilation alone is not phase completion. Entry dependencies and exit meaning are normative in
[`docs/specs/00-index.md § Phase entry and exit rules`](docs/specs/00-index.md#phase-entry-and-exit-rules).

## Permanent prohibitions

- No QuickJS `Runtime`, `Context`, `Rc`, or `RefCell` in `JsTool`.
- No runtime reuse across steps.
- No direct process path that bypasses `Sandbox::wrap_command`.
- No permission-free file or network host global.
- No `require`, `import`, or `final_answer`.
- No short or source-only learned-skill identity.
- No truthy-value test semantics; only exact JavaScript boolean `true` passes.
- No Phase 4 automatic activation.
- No unqualified Windows sandbox/storage/security claim before its normative gates pass.
