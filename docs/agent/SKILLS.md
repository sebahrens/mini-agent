# Skills

With the `skills` feature, mini-agent discovers two authority-separated skill types before the
first model request of each user turn:

- Agent Skills are content-addressed instruction packages under the portable data root. Only
  bounded metadata is indexed; a selected `SKILL.md` is loaded progressively, and resource content
  is included only for exact Markdown/code-path references within its independent budget.
  `allowed-tools` and bundled scripts are inert metadata/resources and never grant permissions.
- Learned JavaScript skills are immutable, identity-checked, verified artifacts in
  `<local-data>/skills/skills.db`. Only active revisions enter a generation snapshot. Their source
  is never placed in the model prompt; the frozen source bundle is sent directly to the JS thread.

One cached query embedding feeds both typed indexes. Learned-JS retrieval uses an immutable HNSW
generation, a generation-local FTS5 snapshot, RRF fusion, score floors, semantic/lineage dedupe,
and independent prompt/source budgets. Small corpora use the exact contiguous dense oracle. Large
startup rebuilds run once per process in a dedicated background thread; an exact/FTS generation is
published before the HNSW graph, then the completed graph is atomically published while existing
turn leases remain unchanged.

At a JS call boundary, `JsTool` snapshots the current bundle. Each selected skill runs in a private
lexical namespace, its full SHA-256 identity and exports are revalidated, and only declared,
JSON-shaped function boundaries are published with the exact host-capability scope. Protected host
globals cannot be replaced. Model-authored code then runs separately as `agent.js`, preserving its
line numbers. Identity, collision, export, source, or capability errors fail before agent code
executes.

Normal removal is optimistic, versioned retirement. Retirement and privacy purge publish an
immediate immutable visibility mask without rebuilding the graph; purge also deletes persistent
vectors, and a purged identity is tombstoned so it cannot be resurrected.
Agent Skill instructions and learned capabilities never bypass the existing MCP, filesystem,
network, process, or sandbox permission paths.

See [the Phase 3 specification](../specs/phase-3-skill-library.md) and
[the 100k benchmark](../benchmarks/skill-retrieval.md) for invariants and measured limits.
