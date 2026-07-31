# Spec Index — mini-agent JS Engine

- **Document role**: normative authority map
- **Specification version**: 1.0.0
- **Delivery status**: living specification
- **Owner**: mini-agent maintainers
- **Last reconciled**: 2026-07-31

## Authority and conflict resolution

The documents indexed below are the only normative JS design corpus. `ARCHITECTURE.md` is an
architecture overview and `SPEC.md` is an implementation overview; they explain the normative
corpus but cannot add, remove, or override requirements. Dated blueprints are superseded research
artifacts and must not be used for implementation.

Apply normative text in this order:

1. `platform-paths.md` controls storage, path, archive, and credential concerns in every phase.
2. The phase spec that owns a concern controls that concern. A later phase changes an earlier
   contract only where it explicitly says that it extends or replaces that contract.
3. This index controls corpus authority, phase dependencies, feature relationships, and exit
   semantics.
4. If two normative passages still conflict, implementation stops until this index or the owning
   phase spec is corrected. Tracker text, overview text, examples, and current code do not break
   the tie.

Tracker issues must cite an indexed normative file and a named section. Existing issue text is
planning context only; it cannot override the cited section.

| Phase | Normative spec | Delivery status | Owns |
|-------|----------------|-----------------|------|
| Foundation | [platform-paths.md](platform-paths.md) | In progress | Typed Linux/macOS/Windows roots, artifact ownership, secure migration |
| 1 | [phase-1-js-engine.md](phase-1-js-engine.md) | Delivered | Core QuickJS integration, `JsTool`, primitive host globals |
| 2 | [phase-2-sandbox.md](phase-2-sandbox.md) | Delivered | `fetch()`, file allow-lists, Linux/macOS process isolation |
| 3 | [phase-3-skill-library.md](phase-3-skill-library.md) | Delivered | Agent Skills import, immutable JS skill store, prompt-time hybrid retrieval, turn-scoped injection |
| 4 | [phase-4-auto-admission.md](phase-4-auto-admission.md) | Planned | Agent proposals, no-effect evaluation, held-out cases, human-gated canary admission |
| 5 | [phase-5-evidence-learning.md](phase-5-evidence-learning.md) | Planned | Evidence-based promotion, telemetry, quarantine, repair, supersession, rollback |

Prior research artifacts superseded by this index:

- `docs/specs/2026-07-27-js-engine-blueprint.md`

## Feature relationships

Cargo features are not phase-completion claims:

- `js` enables the Phase 1 engine and primitive host API.
- `sandbox` is independent of `js` and extends the shared process sandbox. `js,sandbox` enables
  Phase 2 integrations; `js` alone still uses the existing `Sandbox::wrap_command` behavior.
- `skills` implies `js`; it does not imply `sandbox`. Phase 3 verification remains no-effect in
  either feature combination.
- `mcp` remains independent. Combining it with `js` or `skills` must not change MCP discovery or
  permission checks.
- Phases 4 and 5 extend the `skills` implementation; they do not grant a candidate a new Cargo
  feature or a path around lifecycle gates.

## Cross-phase dependencies

| Dependency | Produces | Consumed by |
|-----------|---------|------------|
| Typed `AppPaths`, storage-class ownership, and secure migration | Foundation | Every persistent feature and Phases 3–5 |
| Bounded QuickJS runtime builder with explicit host mode | Phase 1/2 | Phase 3 no-effect verification and turn execution |
| Agent Skills catalog, `SkillStore`, immutable `SkillArtifact`, and typed indexes | Phase 3 | Prompt-time discovery, Phase 4 proposal/admission, and Phase 5 lifecycle |
| `SkillTurnContext` + `TurnSkillBundle` | Phase 3 | Model manifest, exact runtime binding, Phase 5 attribution |
| Pending/verified/canary states and held-out evaluation cases | Phase 4 | Phase 5 evidence policy |
| Invocation events and lineage transitions | Phase 5 | Automatic quarantine, repair, promotion, and rollback |

## Phase entry and exit rules

| Phase | Entry dependency | Exit meaning |
|-------|------------------|--------------|
| Foundation | None | The resolver, ownership matrix, migration, secure creation, and platform tests in `platform-paths.md` pass. |
| 1 | None for the non-persistent engine; Foundation for any persistent artifact or unqualified platform-storage claim | The Phase 1 acceptance criteria pass, including mandatory permissions, bounded host calls, fresh runtimes, and `Sandbox::wrap_command` routing. |
| 2 | Phase 1 | Phase 2 acceptance criteria pass on Linux and macOS. Windows process isolation remains explicitly unsupported until a separate normative phase adds and verifies it. |
| 3 | Foundation and Phase 1; Phase 2 is optional | Manual admission, full artifact identity, no-effect verification, prompt-time retrieval, and turn binding pass. |
| 4 | Foundation, Phase 1, and Phase 3 | Proposals can reach human-approved, non-retrievable canary state; no proposal can become active automatically. |
| 5 | Phases 1–4 and Foundation | Evidence attribution, deterministic routing, permitted Tier 0/1 replacement promotion, quarantine, repair, rollback, and retention gates pass. |

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

## Current implementation note

The monorepo was flattened: production source and the workspace `Cargo.toml` are at the repository
root. Paths under `zerostack/` in a superseded artifact are historical, not aliases that new
tracker issues may cite.

Phase 1 and Phase 3 code exists under `src/extras/js/`, with the portable Agent Skills catalog in
`src/extras/skills/`. Phase 4 admission and Phase 5 evidence learning remain later-phase work.
Source line numbers are intentionally omitted here because they drift; tracker tasks must resolve
current symbols before editing.

## Build commands (mandatory)

```bash
cargo fmt                         # before every commit
cargo test --features js          # type-check + run tests
cargo install --path . --debug    # install development binary
# Never: cargo build, cargo check, --release
```
