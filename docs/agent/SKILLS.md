# Skills

With the `skills` feature, mini-agent discovers two authority-separated skill types before the
first model request of each user turn:

- Agent Skills are content-addressed instruction packages under the portable data root. Only
  bounded metadata is indexed; a selected `SKILL.md` is loaded progressively, and resource content
  is included only for exact Markdown/code-path references within its independent budget.
  `allowed-tools` and bundled scripts are inert metadata/resources and never grant permissions.
  A bounded `learned-js` frontmatter list can associate exact, separately verified learned-JS
  identities with the instructions. Each turn compares a bounded catalog signature, so imports
  and ACTIVE-pointer changes become visible to already-running TUI and ACP sessions.
- Learned JavaScript skills are immutable, identity-checked, verified artifacts in
  `<local-data>/skills/skills.db`. Only active revisions enter a generation snapshot. Their source
  is never placed in the model prompt; the frozen source bundle is sent directly to the contained
  JavaScript worker. Identity-v1 rows are quarantined and cannot execute; current artifacts use
  identity v2 with ABI-bound structured target grants.

One cached query embedding feeds both typed indexes. The initial query is the bounded user prompt.
While working, the model can call the read-only `skills_search` tool with a more specific query;
the result exposes only Agent Skill name/description/digest and learned-JS
ID/description/export-signature metadata. That call re-freezes the learned-JS bundle at the tool
result boundary, so newly selected exports are available to later `js` calls in the same turn.
It does not reveal stored source or tests and cannot install, approve, activate, or widen a skill.
Queries are non-empty and limited to 8 KiB.

Selected instructions and manifests are appended to the system preamble through a per-completion,
non-sticky request patch. The user prompt remains a separate, byte-identical user message, so a
user-authored trusted-context delimiter cannot impersonate the system block. The patch is rebuilt
for internal tool-loop requests and is never added to the returned or persisted conversation
history; subsequent user turns therefore do not accumulate earlier 64 KiB skill blocks.

Learned-JS retrieval uses an immutable HNSW generation, the durable active-only `skill_search`
FTS5 index, RRF fusion, functional BM25 score floors, semantic/lineage dedupe, and independent
prompt/source budgets. The immutable generation's ID map filters every durable lexical result, so
new additions cannot enter an existing lease and removals disappear immediately. Small corpora use
the exact contiguous dense oracle. Startup hydration reuses the already-applied durable generation
without incrementing it; an exact/durable-FTS generation is published before an optional HNSW
graph, then the completed graph is atomically published while existing turn leases remain
unchanged. Rebuilds snapshot SQLite state under the coordinator mutex, release that mutex for every
potentially remote embedding batch, and reacquire it only to cache vectors and atomically validate
or publish the generation. Turn-time refresh and canary-routing queries run on blocking workers;
the canary secret is cached only for the exact published generation. Model or revision changes
backfill both active and canary vectors in the new generation, while canaries remain excluded from
independent prompt retrieval and are reachable only through routing.

Within a logical agent session, discovery storage and telemetry are initialized lazily once for
the canonical workspace and reused across model switches, compaction, and other full-agent
rebuilds. `--no-tools`, an ineligible JS tool, or unavailable worker containment starts none of
the discovery services. When
`enable_skill_proposals = true` is set in trusted configuration, one bounded proposal-store worker
and one contained-verification admission worker join the session bundle; otherwise the
`propose_skill` global remains absent. ACP
sessions retain separate service owners and turn contexts, so concurrent clients cannot replace
one another's selected-skill bundle.

Each read-only exploration subagent gets its own retrieval context and turn lock while sharing the
parent session's immutable Agent Skill index. The child receives initial Agent Skill instructions
and may use `skills_search` when its persona allows that tool. When contained JavaScript is
available, the child receives a read-only `js` realm and may discover and execute active pure
learned-JS exports. Read-only/side-effecting learned skills and canary replacements are excluded,
and the realm exposes only `read_file`, `list_dir`, and `grep` effects under a parent-issued read
grant. Child searches cannot replace the parent or a sibling's frozen bundle.

At a JS call boundary, `JsTool` snapshots the current bundle. Each selected skill runs in a private
lexical namespace, its full SHA-256 identity and exports are revalidated, and only declared,
JSON-shaped function boundaries are published with the exact host-capability scope. Every selected
artifact/export receives a reusable Rust-owned binding, but no reusable bearer authority. On each
genuinely new wrapper call, that dispatcher asks the parent for the next exact call ordinal; the
parent derives the artifact/export-attributed invocation ID and returns a fresh one-shot handle
with newly minted scoped grants. The wrapper consumes that handle before stored source runs.
The warm worker compiles each identity-checked selected artifact once per turn and keeps only its
immutable artifact plus process-local bytecode. After the first call on that worker generation,
the parent sends ordered full identities instead of re-shipping source. Every call still creates a
fresh constrained runtime and private context, loads and evaluates the bytecode there, and mints
new invocation authority; cache misses or cross-turn references fail closed.
The trusted manifest identifies every selected export as a callable global in the `js` tool and
includes one direct-call example per skill. Routing policy, canary-share, and fingerprint metadata
remain parent-only and are not exposed to the model.
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
Use `--learned-skill-stats` to print per-revision status, invocation count, direct success rate,
last-use Unix timestamp, declared effect methods, and an estimated saved-round-trip lower bound.
The estimate credits only successful calls with more than one distinct declared effect method;
raw effect arguments and counts are intentionally not retained in skill telemetry.
Agent Skill instructions and learned capabilities never bypass the existing MCP, filesystem,
network, process, or sandbox permission paths.

An Agent Skill import resolves every `learned-js` identity through the normal learned-skill store
before installing the tree. Missing, corrupt, pending, rejected, retired, quarantined, or purged
identities fail the import; verified and canary identities may be referenced but remain unavailable
to execution. When an Agent Skill is selected for a turn, only its currently active declared
identities are placed ahead of ordinary retrieval results, with exact-ID deduplication and the same
learned-skill count, manifest-byte, and source-byte budgets. This association cannot approve,
activate, revive, or grant authority to code.

## Local-owner lifecycle

Learned-skill lifecycle commands run before provider initialization and use the private local data
directory as their OS-account authentication boundary:

```text
mini-agent --import-learned-skill <package.json|directory>
mini-agent --install-learned-skill-seeds
mini-agent --learned-skill-stats
mini-agent --approve-learned-skill <full-sha256>
mini-agent --reject-learned-skill <full-sha256>
mini-agent --activate-learned-skill <full-sha256>
mini-agent --compact-learned-skill-events
mini-agent --purge-learned-skill <full-sha256>
mini-agent --learned-skill-feedback <full-sha256> \
  --learned-skill-feedback-kind <positive|negative|severe> \
  --learned-skill-feedback-reason <code> \
  --learned-skill-feedback-key <idempotency-key> \
  [--learned-skill-feedback-invocation <full-sha256>]
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

`--compact-learned-skill-events` aggregates raw events older than the retention window before
deleting them. `--purge-learned-skill` performs the coordinated, tombstoned privacy purge described
above. Feedback commands require kind, reason, and caller-chosen idempotency key; the optional
invocation ID attributes feedback to one exact invocation. Severe authenticated feedback can
immediately quarantine an eligible canary or active revision through the normal coordinated
lifecycle path.

## Current limits

The default deterministic embedding backend has no semantic meaning, so dense retrieval is
disabled for that backend and the lexical OR/BM25 channel remains available. Configure a semantic
embedding backend to add dense candidates.

See [the Phase 3 specification](../specs/phase-3-skill-library.md),
[the Phase 6 brokered-runtime specification](../specs/phase-6-brokered-js-runtime.md), and
[the 100k benchmark](../benchmarks/skill-retrieval.md) for invariants and measured limits.
