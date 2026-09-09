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
`--learned-skill-stats` prints one TSV row per identity-v2 revision; see
[Reading `--learned-skill-stats`](#reading---learned-skill-stats) below for the exact columns and
for what the `success` column does and does not mean.
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
mini-agent --distill-learned-skill <session-id> <tool-call-id> \
  [--distill-learned-skill-out <path.json>]
mini-agent --import-learned-skill <package.json|directory>
mini-agent --install-learned-skill-seeds
mini-agent --list-learned-skill-proposals
mini-agent --learned-skill-proposal <full-sha256>
mini-agent --learned-skill-stats
mini-agent --approve-learned-skill <full-sha256>
mini-agent --reject-learned-skill <full-sha256>
mini-agent --activate-learned-skill <full-sha256>
mini-agent --promote-learned-skill <full-sha256>
mini-agent --retire-learned-skill <full-sha256>
mini-agent --reevaluate-learned-skill <full-sha256>
mini-agent --compact-learned-skill-events
mini-agent --purge-learned-skill <full-sha256> [--purge-learned-skill-force]
mini-agent --learned-skill-feedback <full-sha256> \
  --learned-skill-feedback-kind <positive|negative|severe> \
  --learned-skill-feedback-reason <code> \
  --learned-skill-feedback-key <idempotency-key> \
  [--learned-skill-feedback-invocation <full-sha256>]
```

Every command in that list is defined only in a `--features skills` build and runs before provider
initialization. Each one reports through a single shared line — `learned-skill <command>: name=value
…`, with `-` for an absent value — and `--learned-skill-json` (visible alias `--json`) prints the
same fields as one JSON object per line instead. The two tabular listings,
`--learned-skill-stats` and `--list-learned-skill-proposals`, are TSV in both modes.
See [COMMANDS.md](COMMANDS.md#learned-skill-and-agent-skill-cli-flags) for the one-line flag
reference.

A directory import reads 1–32 sorted regular `.json` files and ignores symlinks. Each file is
bounded to 256 KiB and contains exactly a `proposal` in the `propose_skill` wire shape plus a
non-empty `held_out_suites` array — the full format is in
[Package format](#package-format) below. Import canonicalizes
identity v2, registers the trusted baseline, enqueues the immutable artifact, and evaluates it in
the contained worker. A passing artifact stops at `awaiting_approval`; approval moves it to a
non-retrievable canary, and a distinct activation command publishes a lineage-root skill. Failed
or missing baselines cannot be approved. Worker/containment outages leave the proposal pending and
make the import command fail, rather than misclassifying infrastructure failure as skill failure.
Replacement activation continues to require the
evidence-based promotion path and is deliberately rejected by this root-activation command.

Reimporting the same validated package repairs damaged stored baseline data and re-enables its
held-out suites. Repairs retain each suite's canonical ID and record fresh local-owner approval.
Repeated imports of an intact enabled suite remain idempotent.

Use `--list-learned-skill-suites` to inspect suite IDs and enabled state without revealing hidden
cases or fixtures. To remove an obsolete or damaged suite from evaluation, run
`--disable-learned-skill-suite <SUITE_SHA256>`, then
`--reevaluate-learned-skill <PROPOSAL_SHA256>` for a parked proposal. Disabling preserves the
suite data and historical reports; approval still rechecks the enabled corpus. Both suite
commands support `--learned-skill-json` and require a separate invocation from other library
operations.

`--install-learned-skill-seeds` imports five bundled pure packages: JSON, bounded TOML, and CSV
parsing, whole-file unified-diff formatting, and aligned text-table formatting. The seeds use the
same held-out evaluation and two-action approval/activation route as external packages; they are
not silently trusted or activated.

`--compact-learned-skill-events` aggregates raw events older than the retention window (30 days)
before deleting them. `--purge-learned-skill` performs the coordinated, tombstoned privacy purge
described above; it refuses a non-terminal target, or one whose dependent revisions would be
re-rooted, unless `--purge-learned-skill-force` is also passed. `--retire-learned-skill` is the
administrative disable: it requires an `active` revision and preserves the revision, its lineage
and its audit, unlike a purge. `--promote-learned-skill` supersedes an active or quarantined
predecessor with its approved replacement canary and is the only route for a replacement; the
root-only `--activate-learned-skill` rejects one. The feedback contract is in
[Feedback](#feedback) below.

Completion verification records a task outcome after a turn, linked only to learned skills with a
durable invocation event in that turn. Sources are a hashed `verify_command`, an evaluator oracle
ID, or explicit `no_verify_command`. Production status comes from the session constructor;
`MINI_AGENT_GYM=1` only downgrades evidence. A promotion policy that opts into verified-task
evidence cannot fall back to invocation counts when the task threshold is unmet.

The operator Skill Gym is documented in [GYM.md](GYM.md). It exercises paired no-library/library
tasks and the real lifecycle/store boundary, but its evidence is non-production, and a gym report
never changes lifecycle state by itself. Converting recorded JavaScript into a proposal package is
`--distill-learned-skill`, described in
[Distilling a recorded JavaScript tool call](#distilling-a-recorded-javascript-tool-call); it
writes a file for the same human-gated import route and admits nothing on its own.

## The `_cap` calling convention

Every learned export is invoked by the worker as
`Reflect.apply(original, undefined, [capability, ...values])`, so **argument 0 is a hidden
capability object the parent injects** and the caller never writes. The consequence is a fixed
asymmetry an author has to get right in one direction only:

- **Stored source** declares the capability parameter first: `function f(_cap, ...args)`.
- **Everything that calls the export** — the package's own embedded `tests`, held-out suite
  `expression`s, and model-authored `agent.js` code — passes only the real arguments: `f(...args)`.
- The **`signature`** recorded for the export describes the caller's view, so it also omits the
  capability parameter.

The injected object is created with `Object.create(null)`, carries one frozen, non-writable,
non-configurable function per declared effect method, and is then frozen. A `pure` skill therefore
receives an empty frozen object: name the parameter `_cap` and ignore it. A skill that declared
effects calls them through it — `_cap.read_file(path)` — and never through an ambient global.

The bundled JSON seed is the smallest complete example
(`assets/learned-skills/json-parse.json`):

```json
{
  "source": "function parseJson(_cap, text) { return JSON.parse(text); }",
  "exports": [{ "name": "parseJson", "signature": "(text: string) => unknown" }],
  "tests": [
    "parseJson('{\"answer\":42}').answer === 42",
    "Array.isArray(parseJson('[1,2]'))"
  ]
}
```

`parseJson` is written with two parameters and called with one. Writing the source as
`function parseJson(text)` instead would bind `text` to the capability object and fail the
embedded tests.

## Package format

`--import-learned-skill` accepts one JSON file, or a directory of them. The top-level object is
exactly two keys and **rejects unknown fields**:

```json
{ "proposal": { ... }, "held_out_suites": [ { ... } ] }
```

`held_out_suites` must be non-empty: a package with no baseline is refused at load, before
anything is enqueued.

### `proposal`

The same wire shape the in-agent `propose_skill` global produces. Unknown fields are rejected here
too. The bundled seeds spell out all seven keys.

| Field | Type | Bound |
| --- | --- | --- |
| `source` | string | non-empty, ≤ 32 KiB. Written against the `_cap` convention above. |
| `description` | string | non-empty, ≤ 1 KiB |
| `exports` | array of `{name, signature}` | 1–32 entries; `name` ≤ 128 bytes, `signature` ≤ 512 bytes |
| `tests` | array of strings | 1–20 entries, each ≤ 4 KiB; each is an expression that must evaluate to the boolean `true` — a truthy non-boolean such as `1` or a non-empty string fails the case. A returned promise is settled first and its fulfilled value is checked the same way. |
| `capability` | `{tier, grants}` | `tier` ≤ 64 bytes; `grants` 0–4 entries |
| `tags` | array of strings | 0–32 entries, each ≤ 64 bytes. Canonicalization trims, lower-cases, drops empties, sorts and dedupes them, so tag spelling is part of the identity only in that normalized form |
| `predecessor_id` | string or `null` | `null` for a lineage root; the predecessor's full id for a replacement |

Each `grants` entry is a tagged object keyed by `kind`, and every list inside it holds 1–32 entries
(`methods` 1–2):

```json
{ "kind": "read_file",  "workspace_prefixes": ["src/"] }
{ "kind": "write_file", "workspace_prefixes": ["out/"] }
{ "kind": "fetch",      "origins": ["https://example.com"], "methods": ["GET"] }
{ "kind": "spawn",      "programs": ["rg"] }
```

`methods` accepts only the canonical `GET` and `POST`. A capability tier outside the closed set,
or a non-canonical method spelling, fails canonicalization before the artifact is enqueued.

### `held_out_suites`

A held-out suite is trusted, content-addressed evaluation data the proposal author does not get to
see: its `id` and `content_hash` are the SHA-256 of the canonical suite payload, and public reports
bind only that hash and pass/fail outcomes, never the expressions or expected values.

Each entry is `{selector, cases}`.

`selector` must constrain **at least one** of:

| Key | Meaning |
| --- | --- |
| `tags` | up to 32 values, ≤ 128 bytes each; every listed tag, trimmed and lower-cased, must appear in the artifact's normalized tags |
| `exports` | up to 32 `{name, signature}` pairs, ≤ 128 bytes each; every pair must match an artifact export exactly |
| `capability_tier` | a valid tier token that must equal the artifact's tier |

Selection is conjunctive across whichever keys are present, and an artifact runs *every* enabled
suite that matches. Matching suites are sorted by id and capped at 32; if their cases total more
than 64, the import is refused outright rather than sampling a prefix, because the report binds each
suite hash as a claim that the whole suite ran.

`cases` holds 1–64 entries:

| Key | Type | Bound |
| --- | --- | --- |
| `expression` | string | non-empty, ≤ 4 KiB; calls the export without the capability argument |
| `expected` | tagged value | `{"type": "boolean"\|"string"\|"integer"\|"float"\|"null", "value": …}`; a string value ≤ 64 KiB, a float must be finite, `null` carries no `value` |
| `fake_files` | object of path → contents | ≤ 32 entries; path ≤ 4 KiB, contents ≤ 64 KiB each |
| `fake_spawns` | array | ≤ 256 entries; `program` non-empty ≤ 4 KiB, ≤ 64 args, `stdout`/`stderr` ≤ 64 KiB |
| `fake_fetches` | array | ≤ 256 entries; `url` non-empty ≤ 4 KiB, `method` is `GET` or `POST`, `body` ≤ 64 KiB |
| `transcript` | object | expected effect accounting: `reads`, `writes`, `spawns`, `fetches` counts and `read_paths`, `spawn_programs`, `fetch_urls` lists, each ≤ 256 entries and each value ≤ 4 KiB |

`fake_files`, `fake_spawns`, `fake_fetches` and `transcript` all default to empty and can be
omitted for a pure skill.

### Directory import

A directory argument is read non-recursively: entries whose type is a regular file and whose
extension is `.json` are collected, sorted by path, and imported in that order. Symlinked entries
are not regular files and are skipped, and a symlinked import path itself is refused. The directory
must yield 1–32 such files. Each file is read with a hard 256 KiB ceiling. Every package is parsed
and validated **before** the first one is imported, so a malformed file in the set stops the run
rather than leaving a partial library.

### A complete worked package

```json
{
  "proposal": {
    "source": "function parseJson(_cap, text) { return JSON.parse(text); }",
    "description": "Parse JSON text into a JavaScript value with strict JSON semantics.",
    "exports": [{ "name": "parseJson", "signature": "(text: string) => unknown" }],
    "tests": [
      "parseJson('{\"answer\":42}').answer === 42",
      "Array.isArray(parseJson('[1,2]'))"
    ],
    "capability": { "tier": "pure", "grants": [] },
    "tags": ["seed", "json", "parse"],
    "predecessor_id": null
  },
  "held_out_suites": [
    {
      "selector": {
        "tags": ["seed", "json"],
        "exports": [{ "name": "parseJson", "signature": "(text: string) => unknown" }],
        "capability_tier": "pure"
      },
      "cases": [
        {
          "expression": "parseJson('{\"nested\":{\"ok\":true}}').nested.ok",
          "expected": { "type": "boolean", "value": true },
          "fake_files": {},
          "transcript": {}
        }
      ]
    }
  ]
}
```

## Distilling a recorded JavaScript tool call

Everything above assumes a package already exists. `--distill-learned-skill` is how one gets
written from work the agent already did:

```text
mini-agent --distill-learned-skill <session-id> <tool-call-id>
```

The two arguments name a persisted `js` tool call. `<session-id>` is a session under
`<state>/sessions`; `<tool-call-id>` is the `tool_call_id` of a message whose role is `tool_call`
and whose payload is a `js` call. A `tool_result` or `subagent_tool_call` record with the same id
is refused by name rather than silently distilled — a subagent record carries a different payload.

The command:

1. loads the session and extracts the `code` argument of that call;
2. asks the operator's configured provider **once** to rewrite the snippet as a named function with
   parameters instead of the values that were hard-coded for that one session, plus a description,
   embedded tests and hidden held-out expressions;
3. validates that draft against the real proposal and held-out types, canonicalizes it, and builds
   the held-out selector from the **canonicalized artifact** so the suite matches the artifact
   shipped beside it;
4. writes a `{proposal, held_out_suites}` file and prints the path and the next command.

Nothing is imported, verified, approved or activated. The output is a file; the human is still the
approver.

The default destination is `<local-data>/skills/distilled/<sanitized-tool-call-id>.json`;
`--distill-learned-skill-out <path>` chooses another. **An existing file is never overwritten** —
a distilled package is a draft meant to be edited, so a second run against an occupied destination
fails and names the two ways out (remove the file, or pass a different `--distill-learned-skill-out`).

### What the model does and does not decide

The model supplies text only: an export name and signature, a description, a source body, test
expressions, held-out expressions and tags. Everything with an admission or containment
consequence is built in Rust and is not negotiable:

- **`capability` is always `{"tier": "pure", "grants": []}`.** Verification hands a skill fakes
  rather than real effects, and an undeclared effect fails the case outright, so pure computation
  over the call's arguments is the only distillation target that can be honestly evaluated
  offline. If the recording called `read_file`, `spawn`, `fetch` or any other host global, the
  report names them on a `learned-skill distill note:` line and the operator must widen
  `capability` and add `fake_files`/`fake_spawns`/`fake_fetches` by hand before importing.
- **Every held-out case expects the boolean `true`**, never a value the model chose.
- **The selector** is the artifact's normalized tags, its exports and its tier — not text the model
  wrote.
- A generation whose `source` does not declare `function <name>(_cap, …)` is **rejected**, because
  argument 0 is the injected capability object (see
  [The `_cap` calling convention](#the-_cap-calling-convention)). So is a test or held-out
  expression that does not look like a boolean assertion: `run()` is refused, `run() === 2` is
  kept.
- Every distilled proposal carries the tag `distilled`.

### When the model is unavailable

If there is no usable provider, the request times out, the reply does not parse, or the draft
fails any check above, the command does **not** fail silently and does **not** write a broken file.
It writes a scaffold instead: the recorded snippet wrapped in `function distilledStep(_cap) { … }`,
with `TODO` comments naming every part a human must finish, and prints

```text
learned-skill distill: path=… id=… export=distilledStep tier=pure tests=1 held_out_cases=1 generalization=scaffold effects=- complete=false
learned-skill distill note: generalization unavailable (<reason>); complete every TODO marker in the file before importing.
learned-skill distill next: mini-agent --import-learned-skill …
```

A scaffold is a valid package — it loads through the same `deny_unknown_fields` shape — but its
test and its held-out case both reference an undefined identifier on purpose, so an unfinished
scaffold cannot pass verification and cannot be admitted by accident. `generalization=scaffold`
and `complete=false` say which path was taken.

### The whole loop

```text
$ mini-agent --distill-learned-skill 2026-09-08-a1b2c3 toolu_01XYZ
learned-skill distill: path=/…/skills/distilled/toolu_01XYZ.json id=9f… export=normalizeSemver tier=pure tests=2 held_out_cases=3 generalization=model effects=- complete=true
learned-skill distill next: mini-agent --import-learned-skill /…/skills/distilled/toolu_01XYZ.json
```

The emitted file is a package exactly like the worked example above: one `proposal` with
`capability.tier` `pure` and the `distilled` tag, and one `held_out_suites` entry whose selector
names that artifact's tags, exports and tier, with one boolean case per held-out expression.

Read it before importing — it is a draft, and the source is about to become an immutable identity.
Then run the ordinary route:

```text
mini-agent --import-learned-skill /…/skills/distilled/toolu_01XYZ.json
# -> learned-skill import: id=9f… status=awaiting_approval …
mini-agent --approve-learned-skill 9f…
mini-agent --activate-learned-skill 9f…
```

Import registers the trusted baseline, enqueues the immutable artifact and evaluates it in the
contained worker; approval moves it to a non-retrievable canary; activation publishes the
lineage-root skill. Editing the file changes its id, so an edit means importing a different
proposal, not amending this one.

## What verification can see

Admission verification runs the artifact in the broker-only worker with the effect handler that
rejects everything: any real effect request aborts the case as `external effect denied`. Concretely,
during verification there is:

- **no filesystem** — on Linux the worker gets an empty bubblewrap root with only `/proc`, `/dev`, a
  `/tmp` tmpfs, the worker binary read-only bound at `/mini-agent-worker/mini-agent`, and the
  trusted loader/shared-library closure it needs to start. On macOS the Seatbelt profile is
  `(deny default)` and allows reading only the worker image, `/System/Library` and `/usr/lib`.
  There is no `/workspace` mount and no access to the repository under evaluation;
- **no executables** — the environment is cleared, so there is no `PATH`, and no `rg`, `git`,
  interpreter or any other binary exists inside the root;
- **no network** — the Linux worker runs with `--unshare-net` and the macOS profile denies outbound
  connections;
- **no workspace, credential, database or persistence authority** of any kind.

Effects named by an artifact's `tests` and by held-out `expression`s therefore resolve only to
in-memory fakes. An effect the manifest does not declare fails immediately
(`read_file not declared in capability manifest`, and likewise for `write_file`, `spawn`, `fetch`).
For a declared effect the fakes behave as follows, and the difference matters:

| Effect | With a fixture | Without a fixture |
| --- | --- | --- |
| `read_file` | returns the `fake_files` contents | fails the call with `File not found: <path>` |
| `write_file` | writes into the virtual file map | — |
| `spawn` | returns the matching `fake_spawns` response | fails the call with `Spawn fixture not found: <program> <args>` |
| `fetch` | returns the matching `fake_fetches` response | fails the call with `Fetch fixture not found: <METHOD> <url>` |

The fakes never invent a response: a skill that depends on a real `rg` or on a specific HTTP
response body must pin that behaviour with `fake_spawns`/`fake_fetches`, or the case fails. Even
so, treat a green admission report as evidence about the code against the fixtures it was given,
not about the host.

## `verified` with `held_out_suite_required`

A proposal whose embedded tests and mutation gates passed, but for which **no enabled held-out
suite matched its selector**, does not reach `awaiting_approval`. It is completed as blocked, with
status `verified` and reason code `held_out_suite_required`.

The import itself fails with that state named:

```text
learned-skill verification did not reach awaiting approval: id=<sha256> status=verified reason=held_out_suite_required
```

`--list-learned-skill-proposals` keeps showing it (it is non-terminal), and
`--learned-skill-proposal <id>` reports it:

```text
learned-skill proposal: id=<sha256> proposal_id=<sha256> status=verified revision_status=<status> reason_code=held_out_suite_required report_id=- created_at=… updated_at=…
```

**It is not approvable.** `--approve-learned-skill` on it fails, because approval only accepts a
proposal in `awaiting_approval`:

```text
Error: learned-skill review failed

Caused by:
    proposal is not awaiting approval
```

There are two ways to unblock it. `--reevaluate-learned-skill <id>` returns the proposal to the
queue directly, and is the only route for a proposal that arrived through the in-agent
`propose_skill` global, whose exact source an operator does not have. It also recovers a proposal
parked as `deferred`, whether by a verification infrastructure outage or by an exhausted attempt
budget. Requeueing on its own does not supply the missing baseline, so import the matching suite
first unless the selector is what changed.

The other route is to **re-import the identical package with a matching held-out suite**: importing
the same proposal alongside a `held_out_suites` entry whose selector matches the artifact stores
that baseline first, then requeues the blocked proposal automatically. "Identical" is literal — the
proposal is content-addressed, so any change to `source`, `description`, `exports`, `tests`,
`capability` or `tags` produces a different id and a different proposal.

The usual cause is a selector that does not match: a tag the artifact does not carry (both sides are
trimmed and lower-cased, so case is not the problem — a missing or misspelled tag is), an `exports`
entry whose `name` or `signature` differs by a character, or a `capability_tier` that is not the
artifact's tier.

## Feedback

`--learned-skill-feedback <id>` submits one append-only, authenticated report as the local owner. It
requires `--learned-skill-feedback-kind`, `--learned-skill-feedback-reason` and
`--learned-skill-feedback-key`; `--learned-skill-feedback-invocation` is optional.

**Kinds** are `positive`, `negative` and `severe` — the flag rejects anything else at parse time.

**Reason codes** are closed-shape tokens, not free text, because the value is copied verbatim into
the append-only audit table. Any kind's reason code must be non-empty, at most 64 bytes, and
composed only of lowercase ASCII letters and `_`.

`severe` additionally accepts **only three** codes:

| Code | Use |
| --- | --- |
| `integrity` | the skill's behaviour or identity cannot be trusted |
| `permission_violation` | the skill attempted or achieved something outside its grants |
| `unsafe_effect` | a declared effect was used to cause harm |

Anything else submitted as `severe` is rejected and must be filed as `negative`:

```text
severe feedback requires reason_code to be one of `integrity`, `permission_violation` or `unsafe_effect`; `<code>` is not one of them, so submit it as negative feedback instead
```

**Idempotency key** (`--learned-skill-feedback-key`) is caller-chosen, non-empty, at most 128 bytes,
and restricted to ASCII letters, digits, `.`, `_`, `:` and `-` — it is stored in a UNIQUE column and
echoed in conflict reports. Re-submitting the same key with an identical payload returns the same
feedback id and changes nothing. Re-using it for anything else fails with
`idempotency key was reused for different feedback`.

**Ids.** Both the skill id and the optional invocation id must be exactly **64 lowercase hexadecimal
characters**, copied from telemetry. An uppercase or truncated copy is rejected on shape before any
lookup, with the offending value echoed back (truncated at 72 characters):

```text
feedback field `skill_id` must be 64 lowercase hexadecimal characters as copied from telemetry, not `<value>`
```

The skill id must equal a row in `skill_revisions` — that is, an exact revision id, not a lineage
root that has since been superseded.

**Invocation attribution.** When `--learned-skill-feedback-invocation` is given, the store must hold
an `invoked` event with that invocation id, and that event must name the same skill. A mismatch is
reported as such. A missing row is reported as a retention problem rather than a typo, because raw
events are compacted after **30 days**:

```text
invocation `<id>` has no recorded `invoked` event in raw skill telemetry; raw events are retained for 30 days and are compacted into daily aggregates afterwards, so an older invocation id can no longer be attributed
```

Attribute feedback while the invocation is still inside that window, or omit the flag.

**`reason_text`** is a separate 512-byte free-text field on the underlying record. The CLI does not
set it — feedback submitted from the command line always stores it empty.

**Effect.** Every accepted report increments the revision's cumulative `user_positive` or
`user_negative` counter (`severe` counts as negative); resolving or retracting a report later never
decrements them. A `severe` report additionally attempts containment through the normal coordinated
lifecycle path, and the command reports what happened in the same line:

```text
learned-skill feedback: id=<sha256> feedback_id=<sha256> kind=severe status=quarantined quarantine=applied quarantine_reason=-
```

`quarantine` is `applied`, `held` (a policy decision, with the hold reason in `quarantine_reason`),
`skipped` with `ineligible_status:<status>` when the target is neither `canary` nor `active`, or
`not_applicable` for a non-severe kind. Containment is attempted only for a retrievable or
about-to-be-retrievable revision; the feedback is recorded either way.

## Reading `--learned-skill-stats`

The command prints a TSV table with this header, one row per identity-v2 revision ordered by
invocation count, then a `total` row that sums only `invocations`, `gym_tasks`, `user_positive`,
`user_negative` and `est_round_trips_saved` (every other total cell is `-`):

```text
id  status  invocations  success  last_used_unix  tasks_with  passed_with  pass_rate_without  gym_tasks  user_positive  user_negative  declared_effect_methods  est_round_trips_saved
```

| Column | Meaning |
| --- | --- |
| `status` | live lifecycle status of the revision |
| `invocations` | recorded `invoked` events |
| `success` | **share of terminal calls that returned without a fault** — see below. `n/a` with no terminal calls |
| `last_used_unix` | timestamp of the most recent `invoked` event, or `never` |
| `tasks_with` | distinct production turns with a task outcome that used this skill |
| `passed_with` | of those, how many passed their verifier |
| `pass_rate_without` | pass rate of comparable production turns that did *not* use this skill; `n/a` when no such baseline exists |
| `gym_tasks` | task outcomes recorded outside production (`MINI_AGENT_GYM=1`), reported separately so gym traffic never inflates the utility columns |
| `user_positive` / `user_negative` | cumulative authenticated feedback reports received (`severe` counts as negative) |
| `declared_effect_methods` | number of grants in the capability manifest |
| `est_round_trips_saved` | lower-bound proxy: successes × (declared effect methods − 1). Actual effect counts and arguments are deliberately not retained in skill telemetry |

### `success` is not correctness

`success` counts the call outcome only. A call is a success when its terminal telemetry event is
`returned`; it is a failure when the event is `threw`, `timed_out`, `oom` or `capability_denied`.
Nothing inspects the returned value. **A skill that reliably returns semantically wrong data shows
100% success.** The default conservative promotion policy sets no verified-task threshold, so a
replacement's utility case there rests on distinct-turn counts and this same non-fault error rate —
alongside the regression, latency, severe-fault and unresolved-negative-feedback holds — and not on
any check that the output was right.

The correctness evidence is the separate task-outcome columns, `tasks_with`, `passed_with` and
`pass_rate_without`. Those come from completion verification, which records an outcome per turn from
a hashed `verify_command` or an evaluator oracle id. A session with no configured `verify_command`
records `no_verify_command` rows, which carry no pass/fail signal and are excluded — so for that
operator `tasks_with` and `passed_with` stay `0` and `pass_rate_without` stays `n/a`, leaving
`success` as the only non-trivial number on the row, which is precisely the number that cannot tell
you whether the skill is right. Set a top-level `verify_command` (see
[CONFIG.md](CONFIG.md#completion-verification)) if you want the correctness
columns to say anything.

`n/a` distinguishes "no observations" from a genuine `0.0%`: `success` is `n/a` with no terminal
calls, and `pass_rate_without` is `n/a` when no comparable baseline turn exists. An empty baseline is
not a failing baseline.

## Current limits

**Default retrieval is lexical only.** The built-in embedding backend is a deterministic hash
projection: identity-stable, but carrying no semantic meaning. Its vectors are barred from dense
retrieval, so on a default build the dense candidate limit is set to zero and every learned-JS
result comes from the FTS5 BM25 channel alone. The startup diagnostic
`semantic_retrieval_unavailable:deterministic_embedding_backend` records this.

That lexical channel builds its query by tokenizing the prompt on non-alphanumeric boundaries,
lower-casing, dropping a small stop-word list, ranking the surviving distinct terms by IDF over the
`skill_search` vocabulary, keeping the **top 16**, and joining them with `OR`. Two consequences
follow. Any prompt sharing a single indexed term with a skill — `parse`, `format`, `table`, `diff`,
`json`, `csv` and so on — will retrieve it, subject to the top-N and budget cuts. Any prompt with no
vocabulary overlap retrieves nothing at all, however obviously related it reads to a human.

The `skills_search` tool is subject to the same rule: on a default build it is a **lexical match**
against a model-written query, not a semantic search. Word the query with the vocabulary a skill's
description and export names would actually use.

Semantic retrieval requires a real embedding backend: build with the `skills-embed` feature and set
`[embedding] backend = "local"`, or point `[embedding] backend = "external"` at an
OpenAI-compatible embeddings API (see [CONFIG.md](CONFIG.md#skill-embeddings)). With either, dense
candidates rejoin the RRF fusion described above.

See [the Phase 3 specification](../specs/phase-3-skill-library.md),
[the Phase 6 brokered-runtime specification](../specs/phase-6-brokered-js-runtime.md), and
[the 100k benchmark](../benchmarks/skill-retrieval.md) for invariants and measured limits.
