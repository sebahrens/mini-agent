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
rebuilds. `--no-tools`, an ineligible JS tool, or unavailable worker containment starts none of
the discovery services. When
`enable_skill_proposals = true` is set in trusted configuration, one bounded proposal-store worker
and one contained-verification admission worker join the session bundle; otherwise the
`propose_skill` global remains absent. ACP
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
exposes `propose_skill` only under the trusted opt-in above. It enters the same durable queue as
operator imports and never makes a proposal retrievable. Stored skill initialization and exports
never receive proposal authority.

Private realms prevent one skill from receiving another skill's source-level capability object;
they do not contain native compromise. The parent treats the union of all live current-step grants
as the worker's maximum brokered authority and still applies exact scope, session permission,
target narrowing, durable audit, and deadline checks to every effect. The worker has no ambient
workspace, network, credential, database, or persistence authority.

The library's removal contract is optimistic, versioned retirement. Retirement and privacy purge
publish an immediate immutable visibility mask without rebuilding the graph; purge also deletes
persistent vectors, and a purged identity is tombstoned so it cannot be resurrected. The
`--purge-learned-skill` operator command invokes the coordinated privacy-purge path.
Agent Skill instructions and learned capabilities never bypass the existing MCP, filesystem,
network, process, or sandbox permission paths.

## Local-owner lifecycle

Learned-skill lifecycle commands run before provider initialization and use the private local data
directory as their OS-account authentication boundary:

```text
mini-agent --import-learned-skill <package.json|directory>
mini-agent --install-learned-skill-seeds
mini-agent --approve-learned-skill <full-sha256>
mini-agent --reject-learned-skill <full-sha256>
mini-agent --activate-learned-skill <full-sha256>
```

A directory import reads 1–32 sorted regular `.json` files and ignores symlinks. Each file is
bounded to 256 KiB and contains exactly a `proposal` in the `propose_skill` wire shape plus a
non-empty `held_out_suites` array in the Phase 4 held-out-suite shape. Import canonicalizes
identity v2, registers the trusted baseline, enqueues the immutable artifact, and evaluates it in
the contained worker. A passing artifact stops at `awaiting_approval`; approval moves it to a
non-retrievable canary, and a distinct activation command publishes a lineage-root skill. Failed
or missing baselines cannot be approved. Worker/containment outages leave the proposal pending and
make the import command fail, rather than misclassifying infrastructure failure as skill failure.
Replacement activation continues to require the
evidence-based promotion path and is deliberately rejected by this root-activation command.

`--install-learned-skill-seeds` imports five bundled pure packages: JSON, bounded TOML, and CSV
parsing, whole-file unified-diff formatting, and aligned text-table formatting. The seeds use the
same held-out evaluation and two-action approval/activation route as external packages; they are
not silently trusted or activated.

## Current limits and planned changes (2026-09-05 review)

- The default `Deterministic` embedding backend is a hash projection with no semantic meaning;
  with the default score floor it can select unrelated skills at random. Configure a real
  embedding backend for meaningful dense retrieval. Planned: dense retrieval is disabled under
  the deterministic backend (mini-agent-bfsg).
- The lexical channel requires every prompt word to match, so natural-language prompts rarely
  match; an OR/BM25 query is planned (mini-agent-io7h).
- A stats/list view remains planned (mini-agent-vvud, mini-agent-i78t); lifecycle commands print
  the full identity and resulting state for scripting in the meantime.
- The model manifest does not yet say that exports are callable globals (mini-agent-4bqq).

See [the Phase 3 specification](../specs/phase-3-skill-library.md),
[the Phase 6 brokered-runtime specification](../specs/phase-6-brokered-js-runtime.md), and
[the 100k benchmark](../benchmarks/skill-retrieval.md) for invariants and measured limits.
