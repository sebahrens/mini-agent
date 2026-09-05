# Skills

With the `skills` feature, mini-agent discovers two authority-separated skill types before the
first model request of each user turn:

- Agent Skills are content-addressed instruction packages under the portable data root. Only
  bounded metadata is indexed; a selected `SKILL.md` is loaded progressively, and resource content
  is included only for exact Markdown/code-path references within its independent budget.
  `allowed-tools` and bundled scripts are inert metadata/resources and never grant permissions.
- Learned JavaScript skills are immutable, identity-checked, verified artifacts in
  `<local-data>/skills/skills.db`. Only active revisions enter a generation snapshot. Their source
  is never placed in the model prompt; the frozen source bundle is sent directly to the contained
  JavaScript worker. Identity-v1 rows are quarantined and cannot execute; current artifacts use
  identity v2 with ABI-bound structured target grants.

One cached query embedding feeds both typed indexes. Learned-JS retrieval uses an immutable HNSW
generation, a generation-local FTS5 snapshot, RRF fusion, score floors, semantic/lineage dedupe,
and independent prompt/source budgets. Small corpora use the exact contiguous dense oracle. Large
startup rebuilds run once per process in a dedicated background thread; an exact/FTS generation is
published before the HNSW graph, then the completed graph is atomically published while existing
turn leases remain unchanged.

Within a logical agent session, discovery storage and telemetry are initialized lazily once for
the canonical workspace and reused across model switches, compaction, and other full-agent
rebuilds. Proposal and admission workers are not started by the shipped binary. `--no-tools`, an
ineligible JS tool, or unavailable worker containment starts none of the discovery services. ACP
sessions retain separate service owners and turn contexts, so concurrent clients cannot replace
one another's selected-skill bundle.

At a JS call boundary, `JsTool` snapshots the current bundle. Each selected skill runs in a private
lexical namespace, its full SHA-256 identity and exports are revalidated, and only declared,
JSON-shaped function boundaries are published with the exact host-capability scope. Every selected
artifact/export receives a reusable Rust-owned binding, but no reusable bearer authority. On each
genuinely new wrapper call, that dispatcher asks the parent for the next exact call ordinal; the
parent derives the artifact/export-attributed invocation ID and returns a fresh one-shot handle
with newly minted scoped grants. The wrapper consumes that handle before stored source runs.
Replaying a consumed handle or calling after parent expiry/revocation fails closed, and no ambient,
FIFO, or metadata fallback exists. Protected host globals cannot be replaced.
Model-authored code then runs separately as `agent.js`, preserving its line numbers. Identity,
collision, export, source, or capability errors fail before agent code executes. The shipped agent
does not expose `propose_skill`: there is no authenticated operator adapter for held-out suite
import, approval/denial, activation, or purge. Proposal and admission APIs remain test/library
infrastructure. Stored skill initialization and exports never receive proposal authority.

Private realms prevent one skill from receiving another skill's source-level capability object;
they do not contain native compromise. The parent treats the union of all live current-step grants
as the worker's maximum brokered authority and still applies exact scope, session permission,
target narrowing, durable audit, and deadline checks to every effect. The worker has no ambient
workspace, network, credential, database, or persistence authority.

The library's removal contract is optimistic, versioned retirement. Retirement and privacy purge
publish an immediate immutable visibility mask without rebuilding the graph; purge also deletes
persistent vectors, and a purged identity is tombstoned so it cannot be resurrected. No shipped
operator command currently invokes those lifecycle mutations.
Agent Skill instructions and learned capabilities never bypass the existing MCP, filesystem,
network, process, or sandbox permission paths.

## Current limits and planned changes (2026-09-05 review)

- The default `Deterministic` embedding backend is a hash projection with no semantic meaning;
  with the default score floor it can select unrelated skills at random. Configure a real
  embedding backend for meaningful dense retrieval. Planned: dense retrieval is disabled under
  the deterministic backend (mini-agent-bfsg).
- The lexical channel requires every prompt word to match, so natural-language prompts rarely
  match; an OR/BM25 query is planned (mini-agent-io7h).
- The shipped binary has no command to import, approve, or list learned skills, so the store
  stays empty in production. An operator surface, a seed library, and a stats view are planned
  (mini-agent-p0h1, mini-agent-vvud, mini-agent-i78t).
- The model manifest does not yet say that exports are callable globals (mini-agent-4bqq).

See [the Phase 3 specification](../specs/phase-3-skill-library.md),
[the Phase 6 brokered-runtime specification](../specs/phase-6-brokered-js-runtime.md), and
[the 100k benchmark](../benchmarks/skill-retrieval.md) for invariants and measured limits.
