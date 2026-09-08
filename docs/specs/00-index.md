# Spec Index — mini-agent

- **Document role**: normative authority map
- **Specification version**: 1.7.0
- **Delivery status**: living specification
- **Owner**: mini-agent maintainers
- **Last reconciled**: 2026-09-08

## Authority and conflict resolution

The documents indexed below are the only normative design corpus. `ARCHITECTURE.md` is an
architecture overview and `SPEC.md` is an implementation overview; they explain the normative
corpus but cannot add, remove, or override requirements. Dated blueprints are superseded research
artifacts and must not be used for implementation.

Apply normative text in this order:

1. `platform-paths.md` controls storage, path, archive, and credential concerns in every phase.
2. `subprocess-trust.md` controls subprocess class selection and launch contracts; a phase spec
   controls the concrete boundary for the class it owns.
3. The phase spec that owns a concern controls that concern. A later phase changes an earlier
   contract only where it explicitly says that it extends or replaces that contract.
4. This index controls corpus authority, phase dependencies, feature relationships, and exit
   semantics.
5. If two normative passages still conflict, implementation stops until this index or the owning
   phase spec is corrected. Tracker text, overview text, examples, and current code do not break
   the tie.

Tracker issues must cite an indexed normative file and a named section. Existing issue text is
planning context only; it cannot override the cited section.

| Phase | Normative spec | Delivery status | Owns |
|-------|----------------|-----------------|------|
| Foundation | [platform-paths.md](platform-paths.md) | Delivered | Typed Linux/macOS/Windows roots, artifact ownership, secure migration |
| Cross-cutting | [subprocess-trust.md](subprocess-trust.md) | Contract delivered | Subprocess principals, trust classes, launch fields, fail-closed backend selection, checked launch inventory |
| 1 | [phase-1-js-engine.md](phase-1-js-engine.md) | Delivered | Core QuickJS integration, `JsTool`, primitive host globals |
| 2 | [phase-2-sandbox.md](phase-2-sandbox.md) | Delivered | `fetch()`, file allow-lists, Linux/macOS general-process isolation |
| 3 | [phase-3-skill-library.md](phase-3-skill-library.md) | Delivered | Agent Skills import, immutable JS skill store, prompt-time hybrid retrieval, turn-scoped injection |
| 4 | [phase-4-auto-admission.md](phase-4-auto-admission.md) | Delivered | Agent proposals, no-effect evaluation, held-out cases, human-gated canary admission |
| 5 | [phase-5-evidence-learning.md](phase-5-evidence-learning.md) | Delivered (see reachability note) | Evidence-based promotion, telemetry, quarantine, repair, supersession, rollback |
| 6 | [phase-6-brokered-js-runtime.md](phase-6-brokered-js-runtime.md) | Delivered | JS worker containment and lifecycle, wire protocol, capability broker, realm/verification parity, effect audit |

**Phase 5 reachability note.** "Delivered" in the row above means the phase owns those concerns and
their contracts are implemented and regression-tested. It does not mean every one of them is
reachable from the shipped binary. Reachable today: retrieval, attributed telemetry, automatic
quarantine, retention/compaction, privacy purge, and the local-owner commands for import, stats,
proposal listing, feedback, approval, rejection, root activation, **replacement promotion**, and
**retirement**. Not reachable: `rollback_replacement` (a tested library operation with no
production caller and no command), repair (`skills/repair.rs` is `#[cfg(test)]`), the evidence
decision scheduler (`skills/scheduler.rs` is `#[cfg(test)]`), and automatic evidence-threshold
promotion (`--promote-learned-skill` is an explicit human decision that records
`evidence_threshold_promotion: false`). The owning spec's **Shipped-binary status** section is
authoritative for this split.

### Skill Gym and task-outcome evidence

The Skill Gym is a delivered operator/evaluation surface layered on Phases 3--5, not a new
authority-bearing runtime phase. [`../agent/GYM.md`](../agent/GYM.md) owns its operational task
format and scripts. Phase 5 owns durable task-outcome evidence and promotion semantics; Phase 3
owns retrieval; Phase 4 owns proposal, verification, and human admission; the deterministic
harness owns the paired `none`/`library` regression axis. Gym and harness sessions explicitly mark
their evidence non-production. The operator lifecycle episode uses only the shipped import,
approve, activate, feedback, and purge surface and verifies store/index invariants after each
step.

The offline successful-step distiller, nightly live-model library-level A/B, and evidence-derived
retrieval labels are deferred follow-ups (`mini-agent-4qtz`, `mini-agent-aa5i`, and
`mini-agent-n5j6`). They are not part of the delivered loop and must not be inferred from the
scripts or benchmark records.

Prior research artifacts superseded by this index:

- `docs/specs/2026-07-27-js-engine-blueprint.md`
- `docs/superpowers/specs/2026-07-27-js-engine-blueprint.md`

### Explicit Phase 6 supersession map

Phase 6 changes earlier contracts only in the rows below. Every unlisted concern remains owned by
its original phase.

| Earlier authority | Superseded or extended concern | Phase 6 authority | Preserved concern |
|-------------------|--------------------------------|-------------------|-------------------|
| Phase 1, `Threading model`, `Runtime lifecycle`, `Error surfacing`, and `Builder registration` | In-parent per-`JsTool` thread ownership, independent host-call deadline wording, and arbitrary exception message/stack disclosure | `Worker lifecycle`, `Failure semantics` | Language behavior, fresh-runtime rule, heap/stack/I/O limits, stable typed error distinctions |
| Phase 2, `General subprocess integration` | Using a workspace-visible general-process profile for the native JS worker; executing JS host effects in worker-owned closures | `Capability broker`, `Platform containment` | URL/path narrowing and the general command path reached through `Sandbox::wrap_command` |
| Phase 3, `Immutable skill artifact` | Identity-v1 flat host list as the current identity contract | `Persistence boundary` | Immutable full-payload identity, SQLite authority, manual admission, retrieval |
| Phase 3, `Runtime binding` and `No-effect skill verification` | Same-context source binding and parent/in-thread verifier runtime ownership | `Capability broker`, `Verification parity` | Frozen turn bundle, declared exports, deterministic fake semantics, exact-true tests |
| Phase 4, `propose_skill()` and proposal persistence | Identity-v1 flat capability payload, JS-thread host placement, and direct access to durable enqueue | `Capability broker`, `Persistence boundary` | Proposal field bounds, held-out evaluation, human approval gates |
| Phase 5, `Lifecycle and immutable lineage` and `Automatic quarantine` | Normal lifecycle treatment of identity-v1 artifacts during Phase 6 migration | `Persistence boundary`, `Failure semantics` | Evidence policy, transactional lifecycle/index coordination, repair/rollback for eligible identities, retention |

## Delivered amendments (2026-09-05)

The [2026-09-05 harness design review](../plans/2026-09-05-001-harness-design-review.md)
accepted the amendments below. Every named bead is closed with its required regression coverage,
so the owning phase specs now treat these additions as part of the delivered contract. No
amendment changes the Phase 6 canonical checklist.

| Amendment | Owning spec | Bead |
|-----------|-------------|------|
| Async evaluation of model script; documented script semantics and limits | Phase 6 | mini-agent-ml1u, mini-agent-7w1l |
| Closed exception class and validated line/column in diagnostics (introduced in protocol v4; retained by v11) | Phase 6 | mini-agent-m2kw |
| Effect-count exhaustion as a bounded step error | Phase 6 | mini-agent-12cr |
| Read-only `list_dir`/`glob`/`grep` effects and batched `read_files` | Phase 6 (narrowing per Phase 2) | mini-agent-w2lv, mini-agent-ae65 |
| Distinct closed denial codes; permission-wait rendering | Phase 6 | mini-agent-dr93, mini-agent-osaj |
| Typed result channel and parent-owned JSON scratch store (design gate) | Phase 6 | mini-agent-yl18 |
| Deterministic backend disables dense retrieval; OR/BM25 lexical query | Phase 3 | mini-agent-bfsg, mini-agent-io7h |
| Skill context outside persisted user text; callable-export manifest | Phase 3 | mini-agent-rd89, mini-agent-4bqq |
| Bounded model-issued `skills_search` metadata query and tool-boundary refreeze | Phase 3 | mini-agent-a8a0 |
| Operator surface (import/approve/reject/stats) and seed library | Phase 4 | mini-agent-p0h1, mini-agent-vvud, mini-agent-i78t |
| Fault-only quarantine, canary ordering, WAL/immediate transactions, corrupt-row skipping | Phase 5 | mini-agent-lugc, mini-agent-840z, mini-agent-pwf2, mini-agent-jj8b |
| Task-outcome evidence, utility statistics, paired task fixtures, and operator Skill Gym | Phase 5 + evaluation tooling | mini-agent-lkff, mini-agent-t8p4, mini-agent-32hz, mini-agent-iwxi, mini-agent-b0b8, mini-agent-mna1, mini-agent-mtu2 |
| Round-2 process, hook, permission, lineage, and lifecycle hardening | Phase 2/4/5/6 + subprocess trust | mini-agent-qchz, mini-agent-agqn, mini-agent-r4sf, mini-agent-knzj, mini-agent-a97k, mini-agent-eqa8 |

**Accepted retrieval amendment.** The current user prompt remains the primary initial query and
generated JavaScript is never an implicit retrieval query. A bounded, explicit model-issued
`skills_search(query)` may return metadata only and refreeze the bundle at its tool-result boundary;
it never injects source or grants authority.

This retrieval amendment is delivered. The model-issued query is explicit, bounded, metadata-only,
and cannot install, approve, activate, or widen a skill.

## Delivered corrections (2026-09-07)

The 2026-09-07 review (and its round-2 adversarial pass) landed as code, not only as tracker text.
The beads below are closed with their regression coverage; the remaining `review-2026-09-07` beads
are still open and are mostly documentation work. No correction below changes the Phase 6 canonical
checklist.

| Correction | Owning spec | Bead |
|------------|-------------|------|
| Operator replacement promotion over an active or quarantined predecessor | Phase 5 | mini-agent-83k9 |
| Retirement exposed as an administrative disable that preserves lineage | Phase 5 | mini-agent-w0zp |
| Attributed negative/severe feedback on a `returned` invocation counts as a behavioural fault | Phase 5 | mini-agent-5mwn |
| Canary route persisted and audited; unimplemented automatic fallback identified | Phase 5 | mini-agent-sdt9 |
| Dead decision scheduler moved behind `#[cfg(test)]` | Phase 5 | mini-agent-fegd |
| Unused visibility-snapshot module moved behind `#[cfg(test)]` | Phase 4 | mini-agent-2u34 |
| Purge lifecycle/reference guard and named re-rooting | Phase 5 | mini-agent-zod2 |
| Proposal listing, id-addressed approve/reject, and model/CLI-visible admission outcome | Phase 4 | mini-agent-f1gy, mini-agent-ahqj, mini-agent-16om, mini-agent-o99c |
| Typed verification-failure classification instead of substring matching on diagnostics | Phase 4 | mini-agent-3ffo, mini-agent-lu0o, mini-agent-o3yr, mini-agent-tre6, mini-agent-u94h |
| Deferred parking for stranded proposals and bounded import evaluation | Phase 4 | mini-agent-z26b, mini-agent-dztq, mini-agent-afxm |
| Feedback validation, bounds, and quarantine reporting | Phase 5 | mini-agent-0rge, mini-agent-9ihc, mini-agent-8l2x, mini-agent-0fr1 |
| Proposal attempt budget consumed after validation | Phase 4/6 | mini-agent-0zxl |
| Learned-skill index hydration before the first prompt; no trusted skill block when no skill is selected | Phase 3 | mini-agent-kvdv, mini-agent-t0re |
| Agent Skill active-digest selection on import | Phase 3 | mini-agent-mg4r |
| Operator-visible reason when the containment gate removes JS and learned skills | Phase 6 | mini-agent-gdl0 |

Skill Gym, harness, and provider/session corrections from the same review are owned by
[`../agent/GYM.md`](../agent/GYM.md) and the provider documentation, not by this corpus.

## Delivered corrections (2026-09-08)

The 2026-09-08 release code review (epic `mini-agent-wldr`) landed as code. All 27 findings are
closed with their regression coverage. No correction below changes the Phase 6 canonical checklist;
the macOS publisher still independently hashes both pinned descriptors on every publication.

| Correction | Owning spec | Bead |
|------------|-------------|------|
| Guarded file replacement carries the read's exact bytes into publication and rolls back a stale exchange | Phase 2 (workspace authority) | mini-agent-wldr.1 |
| Learned-skill export destinations keep an existing parent's permissions; application state stays private | Phase 4 | mini-agent-wldr.2 |
| Edit tool bounded by `max_text_file_size` on input, growth and result | Phase 2 | mini-agent-wldr.3 |
| Multi-session discovery isolates per-entry failures; exact loading stays fail-closed | — (session storage) | mini-agent-wldr.4 |
| LSP transport writes and notifications bounded; a timed-out partial frame reaps the server | — (LSP) | mini-agent-wldr.5 |
| MCP registered names allocated injectively against one used-name set | — (MCP) | mini-agent-wldr.6 |
| MCP `structuredContent` preserved; unsupported content kinds reported explicitly | — (MCP) | mini-agent-wldr.7 |
| Streamed Markdown publishes boundaries inside fences and bounds unbroken constructs | — (UI) | mini-agent-wldr.8 |
| Fence delimiter length, character, indentation and closing-line rules honored while streaming | — (UI) | mini-agent-wldr.9 |
| Nightly evaluation metrics extracted and schema-validated independently of libtest progress text | — (CI) | mini-agent-wldr.10 |
| Bounded line reading with incremental UTF-8 validation and a defined growth policy | Phase 2 | mini-agent-wldr.11 |
| Explicit provider connect and stream-inactivity deadlines for built-in and custom providers | — (provider) | mini-agent-wldr.12 |
| Hook decisions owned per invocation token | — (hooks) | mini-agent-wldr.13 |
| Every hook `ask` enforced before execution, independent of the inner permission key | — (hooks) | mini-agent-wldr.14 |
| Post-tool hooks receive the arguments that actually executed | — (hooks) | mini-agent-wldr.15 |
| Partial learned-skill startup reported and retried instead of cached as healthy | Phase 3/4 | mini-agent-wldr.16 |
| Observation and proposal startup moved to a tracked blocking worker | Phase 4 | mini-agent-wldr.17 |
| One embedding backend shared by retrieval, telemetry and admission | Phase 3 | mini-agent-wldr.18 |
| macOS fresh-worker phases profiled; cold-start target restated as a reviewed exception | Phase 6 | mini-agent-wldr.19 |
| macOS Phase 6 gate evidence derived from the validated probe result | Phase 6 | mini-agent-wldr.20 |
| Task outcomes attributed to every skill selected during the turn | Phase 5 | mini-agent-wldr.21 |
| Turn evidence completeness persisted; incomplete turns excluded from utility and baselines | Phase 5 | mini-agent-wldr.22 |
| Telemetry shutdown bounded and cancel-aware | Phase 5 | mini-agent-wldr.23 |
| Headless turns persist completed effects and usage before failing | — (runner) | mini-agent-wldr.24 |
| Safety-immediate quarantine applies despite pending index publication | Phase 5 | mini-agent-wldr.25 |
| ACP retains a protocol-valid partial turn on failure or cancellation | — (ACP) | mini-agent-wldr.26 |
| Skill utility statistics computed with grouped set queries | Phase 5 | mini-agent-wldr.27 |

## Feature relationships

Cargo features are not phase-completion claims:

- `js` enables the brokered Phase 6 architecture. The parent owns one lazy supervisor and all
  policy, persistence, and effects; the same executable enters a contained worker mode and creates
  a fresh QuickJS `Runtime` for every request. A missing or failed containment backend makes JS
  unavailable rather than selecting the historical Phase 1 in-process path.
- `sandbox` is independent of `js` and extends the shared process sandbox. `js,sandbox` enables
  Phase 2 integrations; `js` alone still uses the existing `Sandbox::wrap_command` behavior.
- `skills` implies `js`; it does not imply `sandbox`. Phase 3 verification remains no-effect in
  either feature combination.
- `mcp` remains independent. Combining it with `js` or `skills` must not change MCP discovery or
  permission checks.
- Phases 4 and 5 extend the `skills` implementation; they do not grant a candidate a new Cargo
  feature or a path around lifecycle gates.
- Phase 6 does not create a trust-bearing Cargo-feature relationship. Its contained worker is
  mandatory for every JavaScript feature combination; backend absence disables JS rather than
  selecting the Phase 1 execution path.

## Cross-phase dependencies

| Dependency | Produces | Consumed by |
|-----------|---------|------------|
| Typed `AppPaths`, storage-class ownership, and secure migration | Foundation | Every persistent feature and Phases 3–5 |
| Historical bounded QuickJS runtime builder with explicit host mode | Phase 1/2 | Phase 3 delivery baseline; Phase 6 supersedes its execution ownership |
| Agent Skills catalog, `SkillStore`, immutable `SkillArtifact`, and typed indexes | Phase 3 | Prompt-time discovery, Phase 4 proposal/admission, and Phase 5 lifecycle |
| `SkillTurnContext` + `TurnSkillBundle` | Phase 3 | Model manifest, exact runtime binding, Phase 5 attribution |
| Pending/verified/canary states and held-out evaluation cases | Phase 4 | Phase 5 evidence policy |
| Invocation events and lineage transitions | Phase 5 | Automatic quarantine, repair, promotion, and rollback |
| Historical fresh-runtime limits and host semantics | Phase 1/2 | Phase 6 worker runtime and parent capability broker |
| Broker-only worker containment, protocol, realm loader, and effect audit | Phase 6 | All production and verification JavaScript execution |

## Phase entry and exit rules

| Phase | Entry dependency | Exit meaning |
|-------|------------------|--------------|
| Foundation | None | The resolver, ownership matrix, migration, secure creation, and platform tests in `platform-paths.md` pass. |
| 1 | None for the non-persistent engine; Foundation for any persistent artifact or unqualified platform-storage claim | The Phase 1 acceptance criteria pass, including mandatory permissions, bounded host calls, fresh runtimes, and `Sandbox::wrap_command` routing. |
| 2 | Phase 1 | Phase 2 acceptance criteria pass on Linux and macOS; Windows general-process availability additionally requires its cached native AppContainer preflight. Hosted reference-runner evidence proves the explicit-root, zero-capability network, private-storage, and Job observations recorded by the gate, not universal host ACL visibility. |
| 3 | Foundation and Phase 1; Phase 2 is optional | Manual admission, full artifact identity, no-effect verification, prompt-time retrieval, and turn binding pass. |
| 4 | Foundation, Phase 1, and Phase 3 | Proposals can reach human-approved, non-retrievable canary state; no proposal can become active automatically. |
| 5 | Phases 1–4 and Foundation | Evidence attribution, deterministic routing, permitted Tier 0/1 replacement promotion, quarantine, repair, rollback, and retention gates pass *as library contracts under test*. Binary reachability is tracked separately by the Phase 5 reachability note above; promotion is reachable only as an explicit local-owner action, and rollback, repair, and the decision scheduler are not reachable at all. |
| 6 | Preserved Phase 1–5 contracts and both Phase 6 feasibility gates | The brokered runtime acceptance matrix passes on every enabled platform; no production JavaScript path runs in the parent or in an uncontained worker. |

A phase may be decomposed while a prerequisite is open, but it cannot be marked delivered until
every entry dependency, acceptance criterion, delivery epic, and direct blocker is closed.
Schema creation, compilation, or issue decomposition alone never satisfies an exit gate. New
defects discovered during implementation must be linked as blockers before the affected phase
closes.

The following rules remain cross-phase invariants:

- Persistent modules may not define independent `dirs::*`, environment-variable, current-directory,
  or Windows Roaming/Local policy. They must consume the foundation resolver.
- Phase 3 retrieval must be driven by the current user prompt before model generation. Generated
  JavaScript is never the retrieval query.
- Standard Agent Skills use the open `SKILL.md` directory format; ZIP is a validated transport.
  Imported scripts do not become trusted learned JS and `allowed-tools` never bypasses permissions.
- Phase 4 candidate code is untrusted and must execute in a no-effect verifier before approval:
  Tier 0 receives no host globals, while Tier 1/2 receive only declared deterministic in-memory
  fakes that can never touch the real filesystem, process table, or network.
- Phase 5 never mutates active source in place. Repair creates a new immutable revision and all
  automatic decisions retain evidence and a reversible predecessor link.
- Phase 6 keeps credentials, persistence, permissions, external effects, and audit in the parent.
  Stored-skill initialization has no effect or writer authority, and no unavailable containment
  backend may fall back to parent-process or uncontained JavaScript execution.

## Current implementation note

The monorepo was flattened: production source and the workspace `Cargo.toml` are at the repository
root. Paths under `zerostack/` in a superseded artifact are historical, not aliases that new
tracker issues may cite.

Phase 1–5 behavior remains implemented under `src/extras/js/`, with the portable Agent Skills
catalog in `src/extras/skills/`. Phase 6 moved production and verification QuickJS ownership into
the contained same-executable worker in `worker.rs`/`realm.rs`; `engine.rs` is retained only for
historical regression tests. `tool.rs`, `supervisor.rs`, `broker.rs`, and `audit.rs` remain in the
trusted parent and own invocation policy, transport, effects, and durable audit. Phase 5 is
delivered. Phase 6 has dedicated cross-platform containment gates, but the committed
`docs/benchmarks/results/js-worker-baseline.json` still declares
`evidence_state: pending_external_runs` and contains no platform evidence records. It is therefore
a target-and-schema document, not proof that the external platform matrix has been aggregated or
reviewed. Source line numbers are intentionally omitted here because they drift; tracker tasks must
resolve current symbols before editing.

Identity-v2 private learned-skill realms also include the vendored AJV 8.12.0 validator. Stored
skill source can call `Ajv.validate(schema, data)` and inspect the frozen `Ajv.errors` array after
a false result. The facade is absent from the model-authored realm, and neither AJV's mutable
instance nor its dynamic-code constructor is exposed. The trusted bundle is compiled to
process-local bytecode once and instantiated by the trusted loader before hardening only when
canonical source or tests use the exact `Ajv` global, preserving the existing resource envelope for
other skills. A frozen lexical facade is the only reference to the mutable instance. AJV's internal
`Function` calls use a lexically captured native-eval shim that stored source cannot reach; realm
hardening still removes every ambient dynamic-code capability before source runs.
Schemas use AJV's bundled default draft-07 vocabulary. Runtime schema meta-validation, messages,
and optimizer passes are disabled, and AJV emits its equivalent ES5 validator form. Windows
QuickJS exceeds the normative 512 KiB stack even for that minimal generated validator, so the same
frozen facade uses `jsonschema` 0.49.9's non-codegen Draft 7 validator there; its default network
and file resolvers are disabled. Non-meta AJV schemas are removed after every call so compiler
caches cannot grow across invocations; invalid or unsupported schemas return `false` with a closed
schema-stage keyword. The committed AJV bundle and MIT notice live in `src/extras/js/vendor/`.

## Build commands (mandatory)

```bash
cargo fmt                         # before every commit
cargo test --no-default-features --features js # isolated JS type-check + tests
cargo install --path . --debug    # install development binary
# Never: cargo build, cargo check, --release
```
