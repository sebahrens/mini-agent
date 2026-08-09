# Spec Index — mini-agent

- **Document role**: normative authority map
- **Specification version**: 1.3.0
- **Delivery status**: living specification
- **Owner**: mini-agent maintainers
- **Last reconciled**: 2026-08-09

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
| Foundation | [platform-paths.md](platform-paths.md) | In progress | Typed Linux/macOS/Windows roots, artifact ownership, secure migration |
| Cross-cutting | [subprocess-trust.md](subprocess-trust.md) | Contract delivered | Subprocess principals, trust classes, launch fields, fail-closed backend selection, checked launch inventory |
| 1 | [phase-1-js-engine.md](phase-1-js-engine.md) | Delivered | Core QuickJS integration, `JsTool`, primitive host globals |
| 2 | [phase-2-sandbox.md](phase-2-sandbox.md) | Delivered | `fetch()`, file allow-lists, Linux/macOS general-process isolation |
| 3 | [phase-3-skill-library.md](phase-3-skill-library.md) | Delivered | Agent Skills import, immutable JS skill store, prompt-time hybrid retrieval, turn-scoped injection |
| 4 | [phase-4-auto-admission.md](phase-4-auto-admission.md) | Delivered | Agent proposals, no-effect evaluation, held-out cases, human-gated canary admission |
| 5 | [phase-5-evidence-learning.md](phase-5-evidence-learning.md) | Delivered | Evidence-based promotion, telemetry, quarantine, repair, supersession, rollback |
| 6 | [phase-6-brokered-js-runtime.md](phase-6-brokered-js-runtime.md) | Delivered | JS worker containment and lifecycle, wire protocol, capability broker, realm/verification parity, effect audit |

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
| 5 | Phases 1–4 and Foundation | Evidence attribution, deterministic routing, permitted Tier 0/1 replacement promotion, quarantine, repair, rollback, and retention gates pass. |
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
delivered. Phase 6 is delivered with the dedicated cross-platform containment gates and platform
resource records from CI run 31319107422. The final validator at commit `9c6f164` independently
aggregated those records into the reviewed baseline in `docs/benchmarks/results/`. Source line
numbers are intentionally omitted here because they drift; tracker tasks must resolve current
symbols before editing.

## Build commands (mandatory)

```bash
cargo fmt                         # before every commit
cargo test --no-default-features --features js # isolated JS type-check + tests
cargo install --path . --debug    # install development binary
# Never: cargo build, cargo check, --release
```
