---
description: "Default read-only and opt-in implementation subagents in mini-agent: the task tool, model and provider overrides, and per-agent tool limits."
---

# Subagents

## Overview

By default, subagents let the main agent delegate **precise read-only investigations** to a
**read-only child agent**. Each subagent receives a specific technical question
(e.g. "Where is MCP support implemented?") and returns a focused answer.
This keeps the main agent's context clean while enabling thorough lookups.

Subagents are designed for **highly specific questions**, not wide exploration.
Avoid broad instructions like "check all documentation" — instead ask precise
questions that can be answered with a few file reads and searches.

When the main agent calls the `task` tool, one subagent is scheduled per
prompt. Multiple prompts use **bounded parallelism**. By default, each subagent has access
only to read tools and returns a summary of findings, which the main agent then
incorporates into its response.

With the `skills` feature, each child also receives an isolated Agent Skill
retrieval context and the read-only `skills_search` discovery tool. Immutable
indexes are shared, but turn state and locks are not, so parallel children
cannot replace the parent or one another's selected context. When the `js`
feature is enabled, the main JS tool is eligible, and the worker containment
preflight succeeds, each child also receives a read-only `js` realm. Its only
brokered effect globals are `read_file`, `list_dir`, and `grep`; the parent
issues only read authority. Child retrieval may bind active pure learned-JS
exports, but read-only or side-effecting skills and canary substitutions are
excluded.

## Feature Gate

Subagents are gated by the `subagents` Cargo feature and are included in the
default build:

```toml
# Cargo.toml
[features]
default = ["loop", "goal", "git-worktree", "mcp", "acp", "subagents", "archmd",
           "status-signals", "multithread", "export", "js", "sandbox", "memory"]
```

Task-specific schema/provenance adapters and reasoning-budget accounting follow
the `subagents` feature; portable metadata and provider tests retain their helpers.
Main-agent definition loading remains available when subagents are disabled.

## Background tasks

Set `run_in_background: true` on an ordinary `task` call to return a task ID
immediately. The same persona, tools, permission checks, captured prices and
session cost cap apply. A background call has its own whole-call deadline,
`[subagents] task_background_timeout_secs` (default 3600 seconds, at most
86400), instead of the foreground `task_timeout_secs` (300 s); when it expires,
in-flight children return their narration so far as `[partial: cancelled ...]`
sections (see [Bounded execution](#bounded-execution-and-partial-results)). The session keeps at most 32 tasks, including
finished reports. When it is full, the next admission drops the oldest finished
task whose completion notice was already delivered (or that was restored from a
saved session); running tasks and undelivered results are never evicted, and
admission fails with an actionable error only when every entry is one of those.
`/tasks clear` forgets every finished, delivered entry at once.
Background batches share one lane of `task_max_concurrency` children, while the
parent turn's own foreground `task` call uses a separate lane of the same size,
so running background jobs cannot serialize it. Write-enabled children across
all batches share one write lane. These permits stay owned through actual
child-effect retirement, including cancellation, so a queued child cannot
overlap a still-retiring write. Admission checks captured spend and workspace
identity again after waiting for that lane. Time a call's children spend
waiting for a lane is not charged to its deadline (see
[Configuration](#configuration)).

`task_result {"id":"…","wait_secs":120}` blocks (1-600 seconds) until that task settles, so
the model does not spend a round trip per poll; expiry leaves the task running and the row says
so. A headless run (`-p`, stream-json) has no later turn: when the model replies while
background tasks are still running, the process waits up to
`[subagents] headless_exit_wait_secs` (default 300, at most 3600, `0` cancels at once) for them,
says so on stderr, and names any that were cancelled when the wait ran out.

Use `task_list`, `task_result {"id":"…"}`, `task_cancel {"id":"…"}` and `task_send` from an
agent (and `task_evidence`, see [Evidence handles](#evidence-handles)), or `/tasks`, `/tasks result <id>`, `/tasks cancel <id>` and `/tasks clear` from the TUI.
`task_send {"id":"…","message":"…","task":2}` sends a follow-up message
(at most 16 KiB, at most 8 waiting per child) to the running children of a
background task, or only to its `## Task N` child with `task`. Each running
child receives it as a user message, labelled as coming from the parent
agent, at its next model-call boundary; a child that finishes first never sees
it, and finished, cancelling, unknown or restored tasks are refused. The
message is data: it grants no permission and changes no tool, sandbox or
budget. Children do not message each other; there is no team mode.
Agent controls obey ordinary tool ceilings and permissions. Cancellation reports
`cancelling` until the actual model, approvals and effects settle. The default
`background_tasks` footer item shows active tasks and reports awaiting delivery.
The next admitted main turn receives one literal completion notice; an abandoned
preparation keeps it pending. Notices contain status/ID only and grant no authority.

Jobs outlive the foreground turn, including while a new native Anthropic prompt
runs. Their approvals remain ordinary TUI/ACP requests belonging to the exact
live session/job scope. Closing the session or process frontend cancels and joins
owned jobs before releasing services and workspace owners. Rewind/clear/undo stop
old jobs before changing their launch history. A saved entry is **data only**:
there is no deserialized task, grant or resumable handle, and entries last saved
as active are shown as `interrupted` after restart. The background result store
has a 256 KiB memory ceiling and a 16 KiB saved-entry ceiling; ordinary report
defaults remain 32 KiB per child and 128 KiB per batch. A report longer than
16 KiB is also written in full to the session's private tool-output directory
under a name derived only from the session and task IDs, and the saved copy
ends with `[background task result truncated in the saved session at 16384 of
N bytes; the full report is kept at <path>; ...]`. After a restart,
`task_result` and `/tasks result <id>` return that full report (read with the
private-file checks, at most 256 KiB); if the file is gone, the clipped copy
and its notice are returned. Saved data never supplies a path: nothing is
dereferenced on import, and the derived file is deleted when its entry is
evicted or cleared.

`subagents::background` owns the pool, and `session::background_observation`
tracks private runtime provenance for the shared account, file checkpoints and
status history. Native publication retains its full Session digest plus exact old
snapshots. Only same-store, same-pool, monotonic observations from admitted jobs
may enter the next ordinary publication. Transcript/pricing edits, replaced or
imported owners, expired jobs, and unattributed changes still refuse. Paid receipt
ACKs are attributed to their originating job; numerical cost growth alone is
never authority. One fixed Account → checkpoints → task-history lock order makes
saved shared snapshots coherent. Background checkpoint recorders keep their launch
row when foreground steering advances.

## The `task` Tool

The main agent has a new tool called `task`. It accepts:

```json
{
  "prompts": ["explore the auth module", "find all API route definitions"]
}
```

For work with explicit scope or deliverables, prefer structured briefs:

```json
{
  "briefs": [
    {
      "objective": "Audit authentication for session-fixation paths",
      "files": ["src/auth.rs", "src/session.rs"],
      "constraints": ["read only", "report only source-backed findings"],
      "expected_sections": ["attack path", "existing mitigations", "missing tests"]
    }
  ],
  "agent_type": "rust-security-review"
}
```

Send exactly one of `prompts` or `briefs`. Only a *populated* field counts as
a selection: a missing field, `null`, `[]`, a prompt list whose entries are all
blank, or a brief list whose objectives are all blank is treated as absent, so
`{"prompts": [], "briefs": [...]}` runs the briefs. The call is rejected only
when both fields are populated (the error tells the model to omit the unused
field) or when neither is present. The exclusivity rule lives in the tool and
property descriptions rather than a top-level JSON Schema `oneOf`, because
Anthropic rejects top-level combinators in tool input schemas and non-strict
OpenAI-compatible gateways ignore them. Brief fields are rendered into
a bounded, labelled child handoff; embedded newlines remain JSON-escaped so a
field value cannot forge another handoff heading. `files` are scope hints, not
permission grants. `constraints` bound the investigation, while
`expected_sections` name content that belongs inside the host-required return
sections rather than replacing them. Each list is limited to 64 non-empty
items, each item to 8 KiB, and the complete rendered brief to 64 KiB.

Optional `max_turns` and `timeout_secs` arguments set the child model-turn
budget and the whole-call deadline for one call (for example
`{"prompts": ["…"], "max_turns": 60, "timeout_secs": 1800}`). They override a
persona's frontmatter values and the configured defaults, and are rejected
when above `[subagents] task_max_turns_ceiling` (default 100) or
`task_timeout_ceiling_secs` (default 3600, at most 86400); the schema states
the current ceilings.

- **Single prompt**: one subagent explores, returns findings.
- **Multiple prompts or briefs**: up to `task_max_concurrency` subagents run at once.
  Each result appears under a `## Task N:` heading in original prompt order.
- The complete request is rejected before permission checking or execution if
  it is empty, contains a blank prompt, exceeds `task_max_prompts`, or has a
  prompt longer than 64 KiB (`MAX_BRIEF_BYTES`, the same bound as a rendered
  brief); the error says to put bulk content in a file and name it, or to
  split the work.

Display-only bounds (none of them changes what a child receives; source
constants in `task_tool.rs` and `background.rs`):

| What | Bound | Marker |
|---|---|---|
| `## Task N:` heading label | 60 characters of the whitespace-collapsed prompt (`task_heading`) | `…` when shortened |
| Foreground `/tasks` label | 200 characters, then 60 bytes | `...` |
| Tool log per child | 24 rows (`MAX_TOOL_LOG_ENTRIES`), names 48 / arguments 100 / errors 80 bytes | `...` per field, `... N more tool calls not logged` |
| Children with details per task | 8 (`MAX_DETAIL_CHILDREN`) | later children have no `/tasks` detail |
| Foreground calls kept for `/tasks` | 8 most recent (`MAX_RECENT_FOREGROUND`) | oldest dropped |

### Resuming a child (`resume`)

After a child returns a report, the host keeps its conversation (prompt, tool
calls, tool results and final text) and prints, after the child's closing
`[subagent output ends]` fence:

```text
[child id: <uuid>; continue this child's conversation with task {"resume": "<uuid>", "prompts": ["<follow-up>"]}]
```

A later `task {"resume": "<uuid>", "prompts": ["follow-up"]}` continues that
conversation with the single prompt as its next user turn, so the child keeps
what it already read. The call is otherwise ordinary: it passes the `task`
permission check again, runs as the stored `agent_type` (a different
`agent_type` is rejected; omit it), uses the current persona, tools,
deadlines and report contract, and its requests are charged to the call's
`task_max_cost_units` and the session spend account. A resumed child gets a
new child id. Exactly one prompt is accepted.

The conversations are bounded: the session keeps the 16 most recent (8 MiB of
serialized conversation each; larger ones get no id), owned by the session's task pool (or the
task tool without one). A session-owned store is also written, after every change, to a private
(owner-only) file inside the session's private tool-output directory, so it is deleted, pruned
and swept together with the session. The file holds the newest children whose serialized size
sums to at most `task_resume_max_bytes` (the newest is always kept) and at most
`task_resume_max_children` children; when a saved session is resumed (`--continue`, `--resume`)
the ids in that file work again with `task {"resume": id}`. A malformed, oversized or unsafe
file (symlink, wrong owner or mode, invalid ids) is ignored. Only ids that could not be restored
(dropped by the limits, removed artifacts, or a forked/imported session) are named in one system
notice as no longer resumable (ids from before the last such notice are not repeated), so the
model starts a new task instead of using a dead id. An unknown or expired id is rejected with an
explicit error. The task tool without a session pool keeps its conversations in memory only.
Worktree-isolated children are not resumable (their worktree ends with them); failed children get
no id.

## Specialist Agent Types

`task` also accepts an optional `agent_type`. When set, the named definition is
prepended to the base explore prompt as the persona, domain scope,
investigation method, and output contract. Host-owned safety and honesty rules
are appended after every configurable prompt layer and cannot be overridden by
the specialization. Unknown names are rejected with the valid names so a
misspelled specialist can never masquerade as a generic exploration.

| `agent_type` | Domain |
|--------------|--------|
| `rust-maintainer` | Broad Rust SDLC: toolchain, API/semver, ownership, tests, deps, CI, packaging |
| `rust-async-concurrency` | Tokio runtime, `Send`/`Sync` bounds, `Pin`/`Unpin`, cancel-safety |
| `rust-unsafe-code-audit` | UB categories, SAFETY comments, FFI soundness, Phase 6 invariants |
| `rust-security-review` | Trust boundaries, injection, secrets, supply chain, crypto, resource exhaustion |
| `python-maintainer` | Broad Python SDLC: interpreter compat, types, async, tests, deps, packaging, CI |
| `node-typescript-maintainer` | Broad Node.js/TS SDLC: module system, types, event loop, tests, deps, CI |
| `vscode-extension-developer` | VS Code API, webview CSP, postMessage, ACP stdio, vsce packaging |
| `informatica-mapplet-to-fabric-sql` | PowerCenter/IDMC mapplet → Fabric T-SQL, order-dependence audit, reconciliation |
| `azure-cloud-architect` | Azure topology, identity, reliability, cost shape, IaC |

`rust-maintainer` covers the full Rust SDLC as a broad first-pass reviewer.
`rust-unsafe-code-audit` owns memory safety and `unsafe`; `rust-security-review`
owns everything that is safe Rust and still a vulnerability. They are
deliberately disjoint — use both when a review needs both. `rust-maintainer`
delegates to all three specialists explicitly rather than absorbing their domains.

`python-maintainer` covers the full Python SDLC without assuming any particular
framework, package manager, or layout — it derives commands from the actual
project configuration. `node-typescript-maintainer` does the same for Node.js
and TypeScript projects; VS Code API and vsce packaging remain with
`vscode-extension-developer`.

`azure-cloud-architect` remains a read-only specialist for repository-backed
architecture investigations. Because a child receives no conversation history,
its report starts with constraints and marks unverified values as assumptions;
it makes a recommendation only when those stated constraints support one.

Task results are prefix-preserving when truncated at configured output limits.
Specialist output contracts therefore put unresolved risks, assumptions,
caveats, and required human confirmation before large payloads such as SQL or
architecture detail. New specialist definitions must preserve this
caveats-first ordering.

Definitions are markdown resolved by `src/context/agents.rs`, highest priority
first among eligible layers:

```
.mini-agent/agents/<name>.md     # trusted mini-agent project override (highest)
.claude/agents/<name>.md         # trusted Claude-style project definition
data_dir/agents/<name>.md        # user global
~/.claude/agents/<name>.md       # optional Claude Code user layer (claude_user_agents = true)
data/agents/<name>.md            # compiled-in default
```

Set `claude_user_agents = true` to also read your personal Claude Code agents from
`~/.claude/agents`. It is off by default (a personal agent can run in write mode), project config
needs trust to enable it, and the layer is treated like the user-global one: same validation,
no project trust involved, and mini-agent's own `data_dir/agents` wins on a name clash.

Set `claude_project_agents = false` to skip the `.claude/agents` layer when a
repository's Claude Code agents are not meant for mini-agent.

`src/context/mentions.rs` parses agent mentions in submitted text against the
same trust-gated resolution. The canonical token is `@agent:<name>`; Claude
Code's `@agent-<name>` alias is rewritten to it only when the name resolves and
otherwise stays plain text. Tokens are whitespace-delimited and must start with
`@` (so `x@agent:8080` and e-mail addresses are prose); surrounding punctuation
is trimmed. An unresolvable canonical mention fails with an error naming the
sorted valid personas, and untrusted-project personas never resolve.

The `task` schema describes specialists as read-only by default and documents
the explicit opt-in for implementation personas.

### Referencing a specialist with `@agent:`

In the TUI, a submitted message that contains `@agent:<name>` (the `@` picker
inserts it) is a routing hint, not a dispatch, matching Claude Code. The
message is sent as typed, with resolved `@agent-<name>` aliases rewritten to
`@agent:<name>`; that normalised text is what the echo line, the saved
transcript, rewind, hooks and the session title see. For that one request the
model also receives a non-persisted host routing note (the
`PromptInput.turn_directive` block) asking it to delegate through `task` with
`agent_type` set to each named persona and to write the brief itself; several
names may become parallel briefs. The note carries no persona content or
authority, and the `task` tool prompt describes the same convention.

An unknown canonical name is refused before anything is echoed or sent: the
error lists the valid personas and the draft returns to the editor. The same
resolution applies to queued input replayed after a turn and to input queued
behind a Git proposal. A message with `@mcp:` mentions takes the MCP resource
path first. Without the `subagents` feature the text is sent literally.

Headless `-p` and ACP prompts resolve mentions only when the operator opts in
with `headless_agent_mentions = true` (default off: scripts and editors that
send arbitrary text would otherwise start failing on an `@agent:` token). With
the key on, `-p` parses the typed prompt (never piped stdin, `!` commands or
`--loop` runs) at startup: an unknown name exits nonzero with the error on
stderr before any provider request; a resolved prompt is normalised and its
first model request carries the routing note. ACP parses only the client's
`text` content blocks of a non-command `session/prompt` (embedded resources and
resource links are reference material and never route or fail): an unknown name
is a JSON-RPC `-32602` invalid-params error carrying the same message, and a
resolved prompt is normalised and routed for that request only. In both modes
the note is never saved, replayed or shown to hooks. Without the `subagents`
feature the key is accepted but inert and prompts pass through literally.

#### Forcing the first call (`agent_mention_tool_choice = "force"`)

By default (`"hint"`) the routing note is the only steer and the model may
answer without delegating. The startup key `agent_mention_tool_choice =
"force"` additionally forces the turn's **first** model request to call
`task`: the submit path arms a typed marker
(`PromptInput::first_request_tool`, never inferred from the note text) next
to the directive, and `src/agent/first_tool.rs` turns it into a per-request
`tool_choice` patch (`ToolChoice::Specific(["task"])`, Anthropic
`{"type":"tool","name":"task"}`). The hook disarms as soon as the model
responds (an accepted turn, a tool call or a rejected call), so the request
carrying the `task` result and every later step or continuation in the same
turn use the baseline `auto` choice and the model can answer. A retried first
attempt that produced no model output stays forced. Non-mention turns are never
forced. Forcing is armed only by the TUI submit path: headless `-p` and ACP
mention turns carry the routing note alone.

Forcing falls back to the hint silently (debug log, never an error) when the
request path cannot carry it: Copilot and Ollama (rig sends or honours no
`tool_choice`), native Anthropic and OpenRouter `anthropic/` routes with
thinking on or of unknown state, and Claude models that reject forced tools
outright (Claude Opus 5.5, Sonnet 5.5, Fable 5.1, Mythos 5.1). It also falls back
when the agent's tool list for that request does not advertise `task` (for
example `task_enabled = false` or `--no-tools`). OpenAI Responses and Chat
Completions, Gemini and non-Claude OpenRouter routes carry it.

### Opt-in implementation personas

Enable workspace-writing children explicitly in the parent configuration:

```toml
[subagents]
allow_write = true
```

A definition from user-global configuration, compiled-in defaults, or a trusted
project must also select `mode: write`, for example `agents/implementer.md`:

```yaml
---
mode: write
tools: Read, Grep, Edit, Write, Shell
---
Implement the delegated change, verify it, and report the result.
```

On Unix, the installed binary regression can be replayed with
`python3 scripts/probes/write_subagents_stack.py --binary /path/to/mini-agent`.
It uses an owned local provider and a physical 1 MiB stack, checks default
refusal, and verifies two real read/edit/report children execute serially even
when configured concurrency is four. Artifacts remain in the printed temporary
directory; no external provider credentials or requests are used.

Call `task` with `agent_type: "implementer"`. A write persona is refused when
`allow_write` is absent or false. The task approval describes its write-capable
execution; each operation still uses the parent's exact permission checker and
approval channel. Approval prompts identify the child without altering tool
names, paths, or persisted grant patterns. Write/Edit share the child's read
tracker, captured workspace, formatter and LSP connection. Shell inherits the
parent's sandbox, output cap and deadline, and is absent when disabled for the
parent. A persona's `tools` can only narrow the eligible tool set.

Write children that share the parent workspace run one at a time within a task
call, regardless of `task_max_concurrency`; the parent's existing exclusive task
lane orders task calls with other parent mutations. For parallel implementation
use [worktree isolation](#worktree-isolated-write-children). This does not provide cross-process file
locking. Their edits count toward goal-round mutation accounting. They receive
neither nested task delegation nor memory writes, MCP, or additional JS effect
authority. Their JS realm remains read-only. The same report contract and
bounded scheduler cancellation apply. Existing personas without `mode: write`
keep their read-only tool set even when the global opt-in is enabled.

### Worktree-isolated write children

Add `isolation: worktree` to a `mode: write` persona to run each of its
children in a private git worktree and branch instead of the parent workspace,
so implementation steps can run in parallel:

```yaml
---
mode: write
isolation: worktree
tools: Read, Grep, Edit, Write, Shell
---
Implement the delegated change in your worktree and report the result.
```

The same isolation can be requested per call, without editing the persona,
by adding `"isolation": "worktree"` to a `task` call whose `agent_type` is a
`mode: write` persona; it is rejected for read-only children (and
`"isolation": "none"` is the default, leaving the persona's setting in
effect). An optional `"base_ref"` (a branch, tag or commit id, resolved to
one commit; no options, ranges or whitespace) makes isolated children branch
from that commit instead of the parent's `HEAD`; it requires isolation:

```json
{"agent_type": "implementer", "isolation": "worktree", "base_ref": "release/1.4",
 "briefs": [{"objective": "Backport the fix to the release branch"}]}
```

Requires the `git-worktree` and `subagents` features (both default), a parent
workspace that is a git repository, `[subagents] allow_write = true`, and active
permission checks (without a permission checker the parent-tree denial below
cannot be enforced, so the call is refused). `isolation: none` is the default;
any other value makes the definition invalid, never a silent shared-workspace
run.

For each isolated child the host:

1. Creates a worktree and branch `subagent-<id>` from the parent's current
   `HEAD` (or the call's `base_ref`) through the same `git_worktree::create`
   path as `/worktree`, next to
   the repository or under `wt_base_dir` / `--wt-base-dir`. Uncommitted parent
   changes are not visible to the child.
2. Binds the child's workspace capability, file tools, sandbox and permission
   checker to that worktree only. The checker is a snapshot of the parent's
   rules and mode with every session grant dropped, plus a hard denial (no rule,
   grant, hook verdict or approval can lift it) of any mutation under the parent
   workspace or main repository, and of any `.git` entry. Relative traversal and
   symlinks are stopped by the workspace capability. The shell tool keeps the
   parent's sandbox rebound to the worktree, so commands can write only there;
   if the sandbox is off, host-wide, or has an extra write root overlapping the
   parent tree, the child gets no shell tool. Parent-bound read-only JS and
   workspace instructions are not carried over.
3. Skips the shared write lane, so a batch is bounded only by the normal child
   concurrency (`task_max_concurrency`). Non-isolated write children keep the
   serialized lane.
4. When the child ends, the host (not the model) commits what it left in the
   worktree and appends an `## Isolated worktree` section to the report: branch,
   worktree path, base commit, commit list and diff stat. Nothing is applied to
   the parent. Merge explicitly, for example `git merge subagent-<id>` from the
   parent or `/worktree <path>` then `/wt-merge`.
5. Removes the worktree and branch when the child made no changes, and when the
   child is cancelled, times out or its task call is dropped. Worktrees with
   commits are kept until you merge and delete them.

Limits: concurrent children editing the same lines produce conflicting
branches; resolve at merge time. Committing needs a git identity (repository
or global `user.name`/`user.email`); if the commit fails the worktree is kept
and the report says so.

A trusted project may also provide `.mini-agent/agents/.notes.md`. Unlike a
same-named definition, this file replaces nothing: the host appends it under a
`## Project notes` boundary to every resolved persona after overrides have
been selected. The shared file is limited to 64 KiB and each combined persona
remains under the 256 KiB prompt cap. Its exact path is included in permission
and result provenance. Invalid, empty, unreadable, or oversized notes are
ignored with a warning and a visible task-result notice.

Project definitions participate only when the exact current
`.mini-agent/config.toml` is bound in the private project-config trust store.
All three project directories use this existing gate, even when the definitions
live under `.claude` or `.mini-agent`; those directories do not create an
independent trust grant. Project notes use the same gate and remain supported
only at `.mini-agent/agents/.notes.md`. Persona files themselves are not part of
the config fingerprint: an approved checkout may change its persona content.
Resolved persona maps are cached per user agents directory and workspace root.
Every lookup re-evaluates project trust and re-stamps each layer through the
same path discipline as the loader (directory identity plus each `.md` entry's
name, type, size, mtime, ctime, inode, device and link count); any change, a
layer that cannot be stamped, or a trust change reloads through the checked
loader, so an edited persona applies on the next `task` call.
Project reads use the session's captured workspace capability and refuse
symlinks (including linked directory components), reparse points, and nonregular
files. Each layer is enumerated before reading, with at most 1,024 directory
entries and 128 Markdown files; a layer with more than 128 Markdown files
loads the first 128 in sorted file-name order (never filesystem order) and the
task result carries a `[specialist source: ... skipped ...]` notice naming the
skipped files, while more than 1,024 directory entries still ignores the layer. Each file remains capped at 256 KiB,
so all three project layers read at most 96 MiB plus one sentinel byte per file
per resolution. An untrusted checkout cannot add, replace, or
extend agent types; resolution falls back to user-global or compiled-in
definitions. Changing the project config content or copying the checkout
invalidates that content-and-path-bound trust.

Before reading a persona file, the task permission prompt identifies the
requested `agent_type` and the highest-precedence source path that would be
loaded, without embedding or parsing the specialist prompt body. Once approved,
the resolved provider/model, turn-budget tier, and tool subset are applied. When a trusted
project definition wins, the host also prefixes the task result with its
winning project directory's `<name>.md` source. This makes a repository-controlled
replacement visible to the calling agent instead of silently presenting it as
the compiled-in specialist. `SubagentStart` and `SubagentStop` hook envelopes
carry that resolved `agent_type` and definition source. `SubagentStart` also
includes `agent_mode: "read"` or `"write"` for the actual child profile; unspecialized children
use `explore` and the compiled-in explorer source. The TUI also renders a
specialist-start line containing the same type and source before nested child
tool activity.

Returned specialist text is always rendered inside explicit
`[subagent output begins]`/`[subagent output ends]` markers with every line quoted. Text that looks
like a host failure, spill-file path, or specialist-source marker therefore remains untrusted child
content and cannot impersonate task-runner metadata. Hook permission mode is scoped to the child
dispatch and restored afterward; concurrent sessions do not share that mutable scope.

The filename stem is the `agent_type` value and must be 1–64 lowercase ASCII
letters, digits, or hyphens, without leading, trailing, or repeated hyphens.
An optional YAML frontmatter block may configure the persona:

```yaml
---
name: rust-review
description: Focused Rust API and test review
tools: [Read, Grep, Glob]
mcp_servers:
  Context7: [search_docs, get_context]
model: fast-review
turn_budget: medium
mode: review
---
```

- `description` is the delegation guidance shown in the task tool's `agent_type` schema. Up to
  1,000 characters of it are listed per agent (4,096 are kept); when all listed descriptions
  together exceed 12,000 characters the per-agent share shrinks (down to 160) and, last, trailing
  agents lose their description. The listing then ends with a note counting how many descriptions
  were shortened or omitted (also logged as a warning); the full text stays in the agent file.
  Without a description, the first sentence of the prompt (at most 160 characters) is used.
- `tools` narrows the installed child tools. It accepts a YAML list or a
  comma-separated string. Canonical names are `read`, `grep`, `find_files`,
  `list_dir`, `js` when JS support is compiled, and `skills_search` when skills support is compiled, plus
  `memory_read` and `memory_search` when memory support is compiled.
  Claude-style `Read`, `Grep`, and `Glob` spellings are accepted.
  `git`, `lsp_navigate` and `lsp_diagnostics` are granted only when the persona names them here
  (default closed) and the parent itself has the tool eligible and, for LSP, running. The child's
  `git` is the read-only subset (status, diff, log, show, blame, branches, stash_list; every
  mutation is refused and not even offered in the schema) bound to the child's workspace and
  sandbox; the LSP tools use the parent's language-server manager. Both run under the child's
  permission checker (the parent's rules, `git/<operation>` and read permission keys, same
  prompts). A worktree-isolated child gets neither, because both are bound to the parent tree.
  Empty lists create a tool-free child. Mutating, unknown, and feature-disabled
  tools are ignored while preserving the persona and its supported declared
  read tools. An all-unsupported list also creates a tool-free child; it never
  falls back to the default tools. `/agent` lists and task results include a
  one-line narrowing notice (up to eight ignored names, then an omitted count).
  Declarations accept at most 128 entries with nonempty printable ASCII names
  of at most 64 characters; malformed lists or names still reject the definition.
  `mode: write` also accepts `Write`, `Edit`, and `Bash`/`Shell` under the explicit global opt-in.
- `required_tools` declares necessary tools separately from the best-effort
  `tools` narrowing. It accepts a list or comma-separated ASCII tool identifiers
  (at most 128 entries, 64 bytes each), with the same supported aliases. Before
  selecting a child model, starting hooks, or reserving a background job, `task`
  compares these requirements with its actual child tool factory. A missing tool
  returns a structured `not started: missing required subagent capability` report
  with no child provider request. Requirements never grant tools or bypass the
  parent's selection, permissions, workspace binding, or sandbox.
  The legacy code-indexer 0.1.6 persona's explicit `## Tools` CLI-only contract is
  recognized as requiring `shell`; incidental mentions and examples elsewhere
  are not requirements. Read-only children have no shell or nested `Agent`
  route, and MCP only through an explicit `mcp_servers` allowlist. Use the parent's authorized indexer tools or a compatible read-only
  persona instead. New imported personas should declare `required_tools`.
  Task schema descriptions display declared requirements alongside resolved
  definitions; a requirement is not a promise that the tool is installed.
- `mcp_servers` is an exact map from configured server names to their allowed
  wire-level tool names (at most 8 servers and 32 tools). It is child-only and
  opt-in: omission grants no MCP capability, and the child never reuses or
  falls back to the outer agent's connections. Each child connects only the
  named server configurations, so credentials for other servers are absent,
  and installs only tools returned under the exact allowlist. An unknown
  server or missing tool fails the child instead of widening or silently
  degrading its capability.
- Claude Code camelCase frontmatter in `.claude/agents` files: `maxTurns` is read as `max_turns`
  (same 1 to 10,000 bound; setting both spellings is an error); `disallowedTools` removes the
  named child tools (`Read`, `Grep`, `Glob`, `Bash`, `Edit`, ... ) from the explicit `tools` list
  or, without one, from the default set, and names that are not child tools are reported in the
  result notice rather than ignored silently; `mcpServers` takes a list of configured server names
  (each grants all of that server's tools; a mapping to a tool-name list is read like
  `mcp_servers`), within the same 8-server limit, still child-only, trust-gated like any project
  persona and permission-checked per call. A server that is not configured in mini-agent fails the
  child, and an inline server definition is never started (it is reported as not applied).
  `skills` lists skill names: each is resolved against the current authorized skills catalogue
  when the child starts and its `SKILL.md` instructions are placed in the child's system prompt
  (resources are not inlined), within the instruction byte budget of `[skills]` selection (48 KiB
  by default) across all listed skills and at most 16 names. A name that is unknown, not
  authorized, invalid, or does not fit the budget is skipped with a
  `[skill preload: ...]` notice in the child's prompt (and a log warning); the child can still call
  `skills_search` when granted. Preloaded text never grants permissions.
  `background: true` makes every `task` delegation to the persona detach as a background task
  (as if `run_in_background` were set) whenever the session has a background pool; a resumed child
  stays foreground. `memory: user|project|local` and `hooks:` widen what a child can do, so they
  are opt-in (default closed, `[subagents]` needs project trust):
  - `[subagents] agent_memory = true` gives the persona a private notes directory (`user`:
    `<data dir>/agent-memory/<agent>/`, `project`: `<workspace>/.mini-agent/agent-memory/<agent>/`,
    `local`: `<workspace>/.mini-agent/agent-memory-local/<agent>/`; keep the last out of version
    control). The first 200 lines (16 KiB) of its `MEMORY.md` are injected into the child's system
    prompt as quoted data, and the child gets one extra tool, `agent_memory` (`read`, `write`,
    `append`, `list`), limited to `*.md` files in that directory (at most 64 files of 64 KiB, no
    link followed, owner-only directory, symlinked project notes directories refused). The child's
    other tools, permissions and sandbox are unchanged.
  - `[subagents] agent_hooks = true` runs the persona's Claude-format `hooks:` mapping around that
    child's tool calls (`PreToolUse`, `PostToolUseFailure`), inside the global hooks, and only for
    that child's lifetime. Hook commands run like user hooks; a mapping that does not validate is
    reported as `hooks not applied`.
  `permissionMode` is recognised but has no effect and produces a
  `[specialist metadata: permissionMode not applied: ...]` notice.
- `model` is first resolved as a configured quick-model alias, including its
  provider and `extra_body`. Otherwise it is a raw model ID on the current
  subagent provider. A provider switch that cannot authenticate fails the task
  explicitly rather than falling back to another model.
- `turn_budget` is `low`, `medium`, or `high`. It selects one-third,
  two-thirds, or all of the configured `task_max_turns`, rounded up, and can
  never widen that global cap. It is unrelated to the model's
  `[reasoning] effort`. The older `effort` key is still accepted as a
  deprecated alias and logs a warning; setting both `turn_budget` and `effort`
  rejects the definition.
- `max_turns` (1–10000) sets the child's model-turn budget, replacing the
  `turn_budget` fraction of `task_max_turns`; `timeout_secs` (1–86400) sets
  the whole-call deadline of calls using the persona. Both may exceed the
  global defaults but are checked against `[subagents] task_max_turns_ceiling`
  (default 100) and `task_timeout_ceiling_secs` (default 3600): a value above
  its ceiling rejects the `task` call with an error naming the key, never a
  silent clamp. A per-call `max_turns`/`timeout_secs` argument takes
  precedence over the persona.
- `contract: none` (default `sections`) returns the child's final text
  verbatim instead of requiring the Findings/Unverified/Coverage sections: the
  child is told to answer in plain text, the host skips section validation and
  the repair turn, and the text reaches the parent quoted inside the usual
  `[subagent output ...]` fence and cut (with a visible partial marker) at
  `task_max_report_bytes`. Use it for free-form delegation such as code
  generation or summaries. The safety and honesty rules still apply.
- `inline_images` is an integer from 0 to 3 that narrows
  `[subagents] task_inline_images` for this persona (it can never widen it);
  see [Inline image lane](#inline-image-lane).
- `isolation` is `worktree` or `none` (default). With `mode: write`,
  `worktree` runs each child in its own git worktree and branch; see
  [Worktree-isolated write children](#worktree-isolated-write-children).
- `mode: write` selects an implementation child only with the global opt-in.
  Other `mode` values remain the prompt default used when the persona is selected for the
  main loop through `/agent`. Child-only `tools`, `required_tools`, `mcp_servers`, `model`, and `turn_budget`
  settings do not alter main-loop authority.

Persona MCP startup uses the normal bounded initialization and catalog limits,
and calls use `mcp_tool_timeout_secs`. Each child receives a fresh permission
checker built from the configured rules and current mode; outer-session
allow-all state, one-shot approvals, and session grants are not copied. The
permission prompt transport may be shared, but decisions remain child-scoped.
The child future owns its MCP manager: normal completion performs graceful
shutdown, while cancellation drops the manager and immediately cancels its
services/process tree. MCP output still passes through the existing bounded
tool-result and `[subagent output ...]` report boundaries.

The block is validated and removed before the prompt is installed. Malformed
metadata, empty prompt bodies, invalid UTF-8, and oversized
files are rejected. Claude Code compatibility: a frontmatter `name` that differs from the file name is not fatal (the file name stays the agent type and a `[specialist metadata: ...]` notice is recorded), and a `description` over 4,096 characters is cut with a notice (the listing budget above applies separately). The loader emits a warning naming the file and reason. If the
rejected user or trusted-project file has the same valid stem as an available
lower-priority persona, the task result is prefixed with a
`[specialist source: ... definition ignored: ...]` notice and identifies the
fallback source. The loader reads at most 256 KiB plus one sentinel byte from
each user or project definition, including when project files are opened
through the captured workspace capability.

The `agent_type` schema is generated when the task tool is built. Its enum lists
the definitions resolved for the active workspace, and its description gives a
bounded one-line summary from `description` or, when absent, each persona's
first paragraph. Unknown names are still rejected with the current valid-name
list. A persona that pins `model:` or `mode: write` is annotated as
`name [mode: write, model: <value>]` (frontmatter text only, no resolved
credential), and the description states that the child model and write mode come
from the persona definition. Unknown frontmatter keys are ignored for forward compatibility, logged,
and surfaced in the task result so typos cannot silently change behavior.

The same resolved definitions can specialize the main loop. `/agent` lists
them, `/agent <name>` activates one, and `/agent default` clears it. Prompt
modes may select a persona with `%%agent=<name>`; persona frontmatter may select
a default prompt with `mode: <prompt-name>`. One-shot dot/review transitions
snapshot and restore both halves so a temporary mode cannot leak its persona
into the following turn. Project definitions and project prompt directives
participate only after the existing project-config trust check succeeds.

The host appends the repository-as-untrusted, prompt-injection reporting,
honest-unknowns and the selected read-only or write-mode rules after the specialization,
already-loaded architecture context, and user suffix. A specialization supplies
domain heuristics and its return contract, but the delegated objective remains
authoritative for scope: persona checklists do not mandate a whole-repository
audit. Neither the specialization nor any configurable context can override the
host rules (mini-agent-yb9w).

Every successful child response is required to use this host-owned structure:

```markdown
## Findings
- [confidence: high|medium|low] Evidence-backed finding or explicit no-finding statement.

## Unverified
- Missing evidence, caller-run checks, or None.

## Coverage
- Covered: files, paths, and checks actually inspected.
- Skipped: relevant scope not inspected and why, or None.
```

The host checks the section order, confidence label, and both coverage entries.
If a model returns unstructured text, the host first gives the same child one
bounded repair turn (a single model reply, tools disabled, charged to the task
budget) that continues the child's own conversation, tool results included, and
asks it to restate what it established in the required sections; a negative
result such as "no such capability exists" is a finding.
The restatement is used only if it passes validation. Otherwise mini-agent
withholds the original text from the parent model and returns a fixed
machine-checkable partial report with low confidence (the user reads the raw text
with `/tasks result <id>`, marked unverified). Malformed output never
masquerades as a complete specialist report.

A child that exhausts its model-turn budget gets one further tool-free turn over
its full history asking for the contract report. The result opens with
`[partial: turn budget exhausted after N model turns and M tool calls; ...]`.
If that turn yields nothing, the child's narration is returned under the same
header. Both continuations are charged to the repair allocation.

Each child also records a bounded tool log (tool name, truncated arguments,
result size including base64 image bytes, error; at most 24 rows per child,
extra calls only counted).
`/tasks <id>` shows it for background tasks (stored with the task result and
persisted with the session) and for the eight most recent foreground calls
(in memory, listed by `/tasks`). When the host replaces a child's text with a
repaired report, the child's raw text (capped at `[subagents]
task_unverified_bytes`, default 6 KiB, at most 16 KiB) is kept for the user:
`/tasks result <id>` prints it marked unverified. Neither the log nor the raw
text is part of `task_result` output or any other model-visible text.

### Evidence handles

With `task_evidence = true` (the default), the host also records every tool
result of each child, in tool-call order, as `E1`, `E2`, … (forced
`SubagentStop` continuations continue the numbering). A record holds the text
and validated image parts, the tool name, a sanitized argument summary, size,
MIME type, image dimensions and an estimated token cost. Content is written to
the session's private tool-output directory; the session keeps only metadata.
The limits are configurable in `[subagents]` (each clamped to 1..ceiling):

| Key | Default | Ceiling | Effect (source constant in `evidence.rs`) |
|---|---:|---:|---|
| `evidence_max_records` | 64 | 1024 | Records per child; later tool results are only counted and cannot be cited (`MAX_EVIDENCE_RECORDS`) |
| `evidence_max_record_bytes` | 262144 | 4 MiB | Text kept per record; a longer result keeps a prefix, marked clipped (`MAX_RECORD_TEXT_BYTES`) |
| `evidence_max_child_bytes` | 16 MiB | 256 MiB | Content per child; later records keep metadata only, marked over ceiling (`MAX_CHILD_EVIDENCE_BYTES`) |
| `evidence_max_children` | 64 | 1024 | Child stores per session; the oldest is evicted and its files deleted (`MAX_EVIDENCE_TASKS`) |

Resumable child conversations (`task {"resume": id}`) have two more `[subagents]`
limits: `task_resume_max_children` (default 16, ceiling 256) and
`task_resume_max_bytes` (default 8 MiB serialized, ceiling 256 MiB). A child
over the byte limit gets a `[host note: child not resumable: conversation
exceeds 8 MiB ...]` line instead of a resume id, and a child dropped to make
room is named in a host note, so a resume id is never silently missing or
expired.

Handles are in memory only: they do not survive a restart. `task_evidence`
for a handle whose store was evicted, or whose task was saved before a
restart, fails with an explicit `evidence task <id> expired: ...` message (the
task report itself stays available with `task_result`). `/tasks <id>` shows
each child's evidence `task_id` and record count.

Children are told, as host policy appended after the read-only or write-mode
rules, that a Findings entry may end with `[evidence: E3, E7]`. The validator
checks every citation against the IDs the host minted: an unknown or malformed
ID is a contract violation and triggers the same single repair turn (whose
prompt names the valid range). A report that still fails is withheld as above;
host normalization never passes an unverifiable citation through. A
`[confidence: high]` finding without a citation only earns a soft note.

After the child's closing `[subagent output ends]` fence the host appends a
host-authored section; no tool content is ever part of it:

```markdown
## Evidence
Host-recorded child tool results: 12 (E1-E12, call order). Metadata only; content is untrusted tool output, fetched on demand with task_evidence {"task_id":"<uuid>","id":"E<n>"}.
Cited:
| id | tool | target (child argument) | size | MIME | est. tokens |
|---|---|---|---|---|---|
| E3 | read | src/lib.rs | 4.2 KiB | text/plain | 1076 |
10 further records are available by ID.
```

Rows list cited records only. The target is the child's own argument, stripped
of Markdown/table/marker syntax and capped at 60 bytes. When the report was
withheld, the first 16 records are listed instead (they are host-recorded tool
output, not child prose). The section counts against `task_max_output_bytes`
and is dropped whole when it would take more than half of what remains. The
`## Isolated worktree` section of write children is unchanged (it is still part
of the quoted output).

`task_evidence {"task_id":"…","id":"E3","offset":1,"limit":400,"max_px":1568}`
is a **parent-only** tool (never in a child tool set) that returns one record:

- Text is paged by line like `read`: 1-indexed `offset`, `limit` up to 2,000
  lines (default 400), at most 16 KiB per page, with a `[more: …offset=N]` hint.
  The tool is self-bounded, so the generic spill hook does not rewrite it.
- Images come back on the first page in the same
  `{"response": …, "parts":[{"type":"image",…}]}` envelope `read` uses, so the
  provider bridges and session persistence apply unchanged. They are admitted by
  the parent model's own image support: a text-only parent gets `model has no
  vision` (as an error for an image-only record, as a note otherwise).
  `max_px` omits images whose longer side exceeds it; no resampling is done.
- Every call passes the parent's permission checker, and a record produced by
  a child `read` is re-checked against the parent's own read rule for that
  path: evidence never widens what the parent may see.
- The header marks the content as untrusted child tool output, not instructions.

`task_evidence = false` restores the text-only report: no recording, no
citation rules, no Evidence section and no `task_evidence` tool. Use it for
on/off comparisons. The rationale is recorded in
[docs/decisions/2026-11-06-subagent-return-channel.md](../decisions/2026-11-06-subagent-return-channel.md).

#### Evaluation: evidence handles on vs off (mini-agent-g442v)

`tests::harness_eval_tests::harness_regression_eval_evidence_axis` (ignored;
selected by the `harness_regression_eval` filter in ci.yml and the weekly
`harness-eval.yml`) runs every case of `tests/harness_eval/evidence/fixture.json`
through the **real** `TaskTool::call` executor twice, with `task_evidence` on
and off. The child is a real explore agent whose provider is a scripted
OpenAI-compatible loopback server, so its `read`/`grep` calls, evidence
recording, citation validation, repair turn and `## Evidence` section are the
production ones. File contents, answers and the child's wrong claims are drawn
from a seeded generator (`HARNESS_EVAL_SEED` overrides the default), so they
cannot be read from the fixture; the oracle is recomputed from the generated
workspace.

The parent is one fixed, deterministic policy for every case and both arms:
take the child's `ANSWER:` claim; if the host lists cited (or, for a withheld
report, indexed) records, dereference those about the asked subject through
the real `task_evidence` tool (probing other records in call order when the
citation points at a different target, at most 4 records and 4 pages each) and
prefer what the recorded output says. A count is trusted only from a record
read to its last page; a text-only parent cannot read images. Accuracy is the
verdict of the final answer against the oracle; cost is bytes entering the
parent context (task output plus every `task_evidence` call and result),
estimated as bytes/4 plus the patch-formula image tokens. The policy stands in
for a careful parent model, so the eval measures what each arm makes available
and what it costs, not a particular model's behaviour. Each case and arm emits
one `EVIDENCE_EVAL` record and the run one `EVIDENCE_EVAL_SUMMARY`
(`scripts/harness_eval_metrics.py`, `--expect 22`).

Results (default seed, 2026-11-06):

| case | child behaviour | off | on | off tokens | on tokens | derefs |
|---|---|---|---|---|---|---|
| stale_config_value | wrong value, cites the right record | wrong | right | 66 | 300 | 1 |
| incomplete_call_count | undercounts grep hits | wrong | right | 73 | 1,245 | 1 |
| correct_value_cited | right, cited | right | right | 65 | 289 | 1 |
| long_log_correct_count | right, 2-page evidence | right | right | 70 | 6,185 | 2 |
| distractor_citation | cites the wrong file's record | wrong | right | 66 | 291 | 1 (probe) |
| unknown_citation_repaired | cites E4 of 1, repair cites E1 | right | right | 61 | 278 | 1 |
| withheld_after_failed_repair | unknown citation twice, report withheld | wrong | right | 56 | 374 | 1 |
| image_dims_vision_parent | wrong image size, vision parent | wrong | right | 72 | 1,610 (1,323 image) | 1 |
| image_dims_text_only_parent | wrong image size, text-only parent | wrong | wrong | 69 | 298 | 1 |
| uncited_correct_claim | right, no citation | right | right | 61 | 116 | 0 |
| uncited_wrong_claim | wrong, no citation | wrong | wrong | 58 | 113 | 0 |
| **total** | | **4/11** | **9/11** | **717** | **11,099** | 10 |

Evidence handles raised parent accuracy from 4/11 to 9/11 at about 15x the
parent-context tokens, almost all of it spent where the parent chose to verify
(the one 2-page log is 56% of the total). An uncited report costs only the
Evidence header (about 55 tokens). Handles cannot fix an uncited wrong claim or
let a text-only parent check an image. Child cost rises only by the repair turn
of an unknown citation (2 to 3 requests in two cases).

The two image cases also run a third arm with the inline image lane
(`task_inline_images = 1`; the child marks `[attach: E1]`, which the other
arms treat as plain text):

| case | on (dereference) | inline | parent tool calls on / inline |
|---|---|---|---|
| image_dims_vision_parent | right, 1,610 tokens | right, 1,527 tokens | 1 / 0 |
| image_dims_text_only_parent | wrong, 298 tokens | wrong, 339 tokens | 1 / 1 |

**Inline image lane: no-go as a default.** Only one of the 11 cases (a vision
parent checking an image) benefits, and dereferencing already gets it right:
inlining saves one `task_evidence` round trip, about 83 tokens (5%) on that
turn. In exchange the ~1.3k image tokens enter the parent context whether or
not the parent would have checked the image, and they are re-billed on every
later parent turn. A text-only parent gains nothing and pays the notes. The
lane (mini-agent-6m0om) therefore ships opt-in with a default of zero.

#### Inline image lane

`[subagents] task_inline_images = N` (default 0, at most 3 per child; a
persona's `inline_images` may narrow it) lets a child return up to `N` of its
host-recorded **image** results inline. The lane needs `task_evidence = true`
and a foreground call; background results stay text. When it is on, the child
is told (host policy after the citation rules) that it may end a Findings
entry with `[attach: E5]`. After validation the host decides, per attached
record and in attachment order:

- The record must exist and hold images; text results are never inlined
  (raw text tool output only travels by handle).
- The parent model must accept the image (`read`'s captured image support).
  A text-only parent gets the record as an ordinary Evidence row with a note.
- The longer side must be at most 1,568 px. The build has no image decoder, so
  nothing is resampled: a larger image is omitted with a note pointing at
  `task_evidence`, using the dimensions recorded from its header.
- The parent's own read rule must allow a recorded `read` path without asking,
  as for `task_evidence`; otherwise the record is listed, not inlined.
- At most 5 images and 1.5 MiB of base64 per call (inside the 2 MiB frame and
  4 MiB aggregate ceilings of [IMAGE_READ.md](IMAGE_READ.md)), shared by all
  children of the call.
- Each image is charged to the call's `task_max_cost_units` at its
  patch-formula estimate (`ceil(w/28) * ceil(h/28)`, as in
  `image_cost.rs`), converted with the task's cost weights; an image the
  remaining budget cannot cover is omitted.

Every attached record is listed in the Evidence table, and an `Inline images`
block there states what was attached or why not.

The image limits are fixed constants, not configuration (only the count,
`task_inline_images` ≤ 3, is configurable):

| Limit | Value | Source constant |
|---|---:|---|
| Images one child may inline | 3 | `inline_images.rs` `MAX_PER_CHILD` |
| Images one call may inline (all children) | 5 | `inline_images.rs` `MAX_PER_CALL` |
| Longest side of an inlined image | 1,568 px | `inline_images.rs` `MAX_LONG_EDGE_PX` |
| Base64 bytes one call may inline | 1.5 MiB | `inline_images.rs` `MAX_ENCODED_BYTES_PER_CALL` |
| Base64 image bytes in one child's history | 4 MiB | `image_cost.rs` `MAX_CHILD_IMAGE_ENCODED_BYTES` |
| Token charge per image | ≤ 4,784 | `image_cost.rs` `MAX_IMAGE_TOKENS` |

Attachments beyond the per-child or per-call caps are listed in the Evidence
table with a note instead of inlined. When the 4 MiB history ceiling drops a
child's image tool result, the child is told in that result, and the parent's
report carries a host line after the child's output:
`[host note: N image result(s) were omitted from this child's history at its 4 MiB image ceiling ...]`. If at least one image was
admitted, the `task` output is the same `{"response": …, "parts": [...]}`
envelope `read` uses: `response` is the complete quoted text report and the
images follow it at the end of `parts`, so earlier prompt-cache prefixes are
undisturbed. Only the host builds the envelope; on the text path the host
also prefixes `[task output]` to any output that would parse as a JSON
object, so a child report can never be lifted into image content. A
withheld report's attachments are ignored. Images of a child whose Evidence
section was dropped for the output budget are not attached.

### Designer → implementer → reviewer pipeline

Three models, three profiles: the main loop designs on a strong model,
delegates edits to write-mode children on a cheaper model served by a different
OpenAI-compatible endpoint, then hands the result to a read-only reviewer on a
third profile. The complete example lives in
`tests/fixtures/persona-pipeline/` and its wiring is pinned by
`src/extras/subagents/task_tool/persona_pipeline_tests.rs` (no network).

**Config** (`config.toml`; endpoints and model ids are placeholders, only
environment variable names appear):

```toml
# Main loop starts on the designer's model (the `design` prompt switches to it).
provider = "anthropic"
model = "example-opus-model"

# Designer: strongest model, Anthropic-native.
[quick_models.opus]
provider = "anthropic"
model = "example-opus-model"

# Implementer: GLM served by a different OpenAI-compatible endpoint.
[quick_models.glm]
provider = "glm"
model = "example-glm-model"

# Reviewer: a third profile on a separate gateway.
[quick_models.review]
provider = "openrouter"
model = "example-vendor/example-reviewer-model"

[custom_providers.glm]
provider_type = "openai"
base_url = "https://glm.example.invalid/v1"
api_key_env = "EXAMPLE_GLM_API_KEY"
api_style = "completions"

# Selecting the `design` prompt switches the main loop to the opus alias.
[prompt_to_model]
design = "opus"

# Write-mode implementer children are refused without this opt-in.
[subagents]
allow_write = true
```

**Personas**, in the user agents directory (or a trusted project's
`.zerostack/agents/`):

`designer.md`, a main-loop persona (`mode:` names a prompt):

```markdown
---
description: Designs the change, delegates implementation, then requests review
mode: design
---
You are the designer. Study the code, decide the approach, then split the work
into small, self-contained implementation tasks. For each task call `task` with
`agent_type: "implementer"` and a prompt that names the files, the exact change
and how to verify it. When the implementer reports back, inspect the result and
call `task` with `agent_type: "reviewer"` to review it before you answer.
```

`implementer.md`, a task child (`mode: write` plus a quick-model alias):

```markdown
---
description: Implements one delegated change and verifies it
mode: write
model: glm
tools: Read, Grep, Glob, Edit, Write, Bash
turn_budget: high
---
Implement exactly the delegated change, run the stated verification, and report
the files you changed and the result. Do not widen the scope.
```

`reviewer.md`, a read-only task child on the third profile:

```markdown
---
description: Read-only review of the implemented change
model: review
tools: Read, Grep, Glob
turn_budget: medium
---
Review the delegated change read-only. Report defects first, ordered by
severity, each with file and line evidence. Do not propose edits you cannot
justify from the code.
```

**Design prompt**, `prompts/design.md` in the prompts directory:

```markdown
You are in design mode. Do not edit files yourself. Produce a short design,
delegate each implementation step to the `implementer` agent type through the
`task` tool, and finish by delegating a review to the `reviewer` agent type.
```

**Start it** with `export EXAMPLE_GLM_API_KEY=...` (plus the Anthropic and
OpenRouter keys, as environment variables) and run `/agent designer`. That
activates the `design` prompt, and `[prompt_to_model] design = "opus"` switches
the main loop to the opus alias. Each `task` call with
`agent_type: "implementer"` then runs on the `glm` provider and each
`agent_type: "reviewer"` call on the `review` alias. When a persona's alias
names a different provider than the parent's, the task tool builds a separate
client for it (`custom_providers` and `api_keys` from the same config); a raw
model id in `model:` stays on the parent's provider.

**Gotchas**

- `mode:` has two meanings. In a main-loop persona it is a prompt name
  (`design`); in a task child it is `write`. Designer and implementer must
  therefore be separate personas, and `mode: write` on the designer is not a
  way to make it write.
- The designer must stay in standard security mode and must not be read-only:
  write children inherit the parent's eligible tools, permissions and sandbox,
  so a read-only designer yields read-only implementers. Do not put
  `%%mode=readonly` in the design prompt.
- `[subagents] allow_write = true` is required; without it the implementer is
  refused. Each edit still goes through the parent's permission checker.
- Write children that share the parent workspace run one at a time even with
  `task_max_concurrency` above one, so independent implementation steps are
  serialized. Add `isolation: worktree` to the implementer to run them in
  parallel, each in its own git worktree and branch (see
  [Worktree-isolated write children](#worktree-isolated-write-children)).
- The task tool's `agent_type` description shows each persona's pinned
  `model:` and write mode, for example `implementer [mode: write, model: glm]`,
  so the model can see that a child model is chosen by the persona definition.
  The effective model of a finished child is still not echoed in the task
  result or `/agent` output.
- The `glm` provider is credential-isolated (see
  [Custom provider credentials](CONFIG.md#custom-provider-credentials)): it
  uses only `EXAMPLE_GLM_API_KEY` or `api_keys.glm`, never `OPENAI_API_KEY`.
  An explicit `--api-key` is scoped to the session provider it was given for:
  a persona child that switches to another provider, including `glm`, never
  receives it and resolves that provider's own credential instead. Supply the
  keys of the other endpoints through their environment variables or
  `api_keys`. If the key is missing the task fails with
  `could not initialize provider 'glm'` instead of falling back.

## What the Subagent Can Do

### Read tools (always available)

| Tool       | Purpose                       |
|------------|-------------------------------|
| `read`     | Read file contents            |
| `grep`     | Regex search in files         |
| `find_files` | Find files by glob pattern |
| `list_dir` | List directory contents       |

### Read-only JavaScript (when `js` is enabled and contained)

The child `js` tool supports local computation plus exactly four brokered
effects: `read_file`, `read_lines` (ranged/tail line reads of files larger than
the 1 MiB `read_file` cap), `list_dir`, and `grep`, and the pure helpers
`splitLines(text)` and `json_exact(text)`. `write_file`, `fetch`, `spawn`,
`scratch_put`, `scratch_get`, `result`, `read_files`, `glob`, and
`propose_skill` are absent from the realm. The parent broker independently
limits the invocation grant to `read_file`, so a malformed or compromised
worker cannot obtain mutation, network, process, or session-state authority.
Only active pure learned-JS exports can be installed in this realm.

### Memory tools (when `memory` feature is enabled)

| Tool            | Purpose                                |
|-----------------|----------------------------------------|
| `memory_read`   | Read memory files (long-term, notes…)  |
| `memory_search` | Keyword search across all memory       |

### Child shell jobs and output logs

A child that has `shell` (write-capable or read-only shell) also gets `job_status`, scoped to the
background jobs that child started: it cannot list, poll, wait for, stop or type into the parent's
or a sibling's jobs, and it does not receive the parent's job-completion notices. Jobs still
running when the child ends are stopped. Child shells honour the parent's `shell_auto_background`
and spool output beyond the in-memory limit to the parent session's private command-log directory,
so the omitted middle is recoverable with `read`/`grep` from the path named in the result.

### Opt-in read-only shell and web tools

Two `[subagents]` opt-ins (both default `false`) let a read-mode persona run
checks or fetch documentation without becoming a write child:

```toml
[subagents]
allow_read_only_shell = true   # a persona naming shell/Bash gets a read-only shell
allow_web = true               # a persona naming web_fetch/web_search gets them
```

```yaml
---
tools: Read, Grep, Bash, web_fetch, web_search
---
```

- **Read-only shell.** A read-mode persona whose `tools` names `shell` or
  `Bash` (without the opt-in the name is ignored with a notice, as before)
  receives the parent's shell rebound to a fresh private scratch directory
  under the system temporary directory. The scratch directory is the
  command's only writable workspace and is deleted when the child ends; the
  parent workspace is added as a read root, so commands can read but not
  write it (use absolute paths or `cd`, and point build outputs at the scratch
  directory, for example `CARGO_TARGET_DIR`). The child is told this in a host
  note. The shell is installed only when the rebound sandbox provably cannot
  write the workspace: an uncontained sandbox, the `host-full` profile or an
  extra write root overlapping the workspace removes it (the child is told
  why), and explicit read roots need the Seatbelt or bubblewrap backend. It
  needs the `git-worktree` feature (default) and a parent with an eligible
  shell tool. Permission rules, approvals, the network policy, output cap and
  deadline are the parent's. Such children are not write children: they run
  in parallel with other read-only children.
- **Web tools.** A persona whose `tools` names `web_fetch` and/or
  `web_search` receives the parent's eligible web tools with the parent's
  origin/provider settings and permission checks. Without the opt-in, or when
  the parent lacks the tool, they are not installed.

No default child (without a persona `tools` entry) ever receives a shell or
web tool. `required_tools` can name `shell`, `web_fetch` or `web_search` to
refuse a call before any child request when they are unavailable.

### Default profile exclusions

| Tool           | Reason                                  |
|----------------|-----------------------------------------|
| `write`        | Available only to an opted-in `mode: write` child         |
| `edit`         | Available only to an opted-in `mode: write` child         |
| `bash`         | Only opted-in write children receive the parent’s eligible `shell`; a read persona can opt into the sandboxed read-only shell (`allow_read_only_shell`) |
| `web_fetch`, `web_search` | Only a persona that names them under `[subagents] allow_web = true` |
| `memory_write` | Subagent should not persist memory      |
| `todo`         | Not registered — no planning tool in child context |
| `task`         | Nested subagents are deliberately unsupported |
| MCP tools      | Not inherited from the parent; a persona opts in per server and tool through its `mcp_servers` allowlist (persona frontmatter, described above: at most 8 servers and 32 tools, child-only, no fallback to the parent's connections) |

## Security & Permissions

The subagent **inherits** the parent's authorization context
(`SubagentAuthorization`), which carries the parent's `PermCheck`, approval
channel (`AskSender`), and workspace binding. Every child tool respects the
same path-containment and approval rules as the parent:

- **Path containment**: reads outside the workspace binding are denied or sent
  through the parent approval channel, exactly as they would be for the main
  agent.
- **Default profile has no mutation tools**: the child has no `write`, `edit`, `bash`, or
  MCP tools; a persona's exact `mcp_servers` allowlist is the only way a child
  gets MCP access. Its optional JS realm has no writer, network, process, or
  session-state globals and its broker grant contains only read authority, so
  it cannot modify files, run commands, or reach external services regardless
  of permissions.
- **No memory writes**: `memory_write` and `memory_edit` are deliberately
  absent; a subagent can only read persistent memory.
- **No nested tasks**: `task` itself is not registered for child agents, so
  nesting is impossible.

The main agent's `task` tool goes through the normal permission check
(`check_perm("task", …)`), so users can allow, ask, or deny it through the
normal TOML/YAML/JSON mini-agent configuration described in
[CONFIG.md](CONFIG.md#permission-config).

## Configuration

| Config field                | Type     | Default                  | Description |
|-----------------------------|----------|--------------------------|-------------|
| `task_max_turns`            | `usize`  | `20`                     | Max agent turns per subagent |
| `task_max_prompts`          | `usize`  | `8`                      | Max child prompts in one tool call |
| `task_max_concurrency`      | `usize`  | `4`                      | Max simultaneously running children |
| `task_max_output_bytes`     | `usize`  | `131072` (128 KiB)       | Hard cap on the complete returned tool output |
| `task_max_cost_units`       | `u64`    | `4000000`                 | Aggregate normalized work budget, independent of USD |
| `task_cost_weighting`       | `string` | `"auto"`                 | Model-aware ratios; `"fixed"` uses configured weights |
| `task_cost_weights`         | `object` | See below                | Token-category weights and automatic-mode fallbacks |
| `task_cost_overrides`       | `object` | `{}`                     | Weighting/weights by exact selected quick-model alias |
| `task_timeout_secs`         | `u64`    | `300`                    | Whole-call wall-clock deadline of foreground calls; completed sibling results and in-flight narration are retained when it expires |
| `[subagents] task_background_timeout_secs` | `u64` | `3600` | Whole-call deadline of `run_in_background` calls (at most 86400) |
| `[subagents] agent_memory` / `agent_hooks` | `bool` | `false` | Opt in to a persona's `memory:` notes directory and `hooks:` frontmatter (project config needs trust) |
| `[subagents] headless_exit_wait_secs` | `u64` | `300` | Seconds a headless run waits at exit for running background tasks before cancelling them (at most 3600; `0` cancels at once) |
| `[subagents] task_max_turns_ceiling` | `usize` | `100` | Highest per-call or persona `max_turns`; larger values are rejected |
| `[subagents] task_unverified_bytes` | `usize` | `6144` | Raw text of a host-replaced report kept for `/tasks result` (at most 16384) |
| `[subagents] task_timeout_ceiling_secs` | `u64` | `3600` | Highest per-call or persona `timeout_secs` (at most 86400); larger values are rejected |
| `task_enabled`              | `bool`   | `true`                   | Whether the `task` tool is registered |
| `task_evidence`             | `bool`   | `true`                   | Host-minted evidence handles, citations, the Evidence section and `task_evidence` ([details](#evidence-handles)); tool registration follows a restart |
| `subagent_model`            | `string` | `none (uses main model)` | Model name or quick-model alias |
| `subagent_provider`         | `string` | (same as main)           | Provider for the subagent (optional) |

All integer task limits must be greater than zero.
`task_max_output_bytes` must be at least 256, leaving room for an explicit
partial-status header, and `task_timeout_secs` cannot exceed 86400 (24 hours).

Session background batches share one lane of `task_max_concurrency` children;
the parent turn's own (foreground) task call uses a separate lane of the same
size, so running background jobs cannot serialize it. Write children of every
batch still share the session's single write lane. While every in-flight child
of a call is waiting for a lane, its `task_timeout_secs` deadline is paused,
with that credit capped at one further timeout; when the deadline stops a call
whose started children are still waiting, the partial-status header reports
them as `queued=N`.

### Model-aware task work accounting

`task_max_cost_units` remains **4,000,000** per task call. These are normalized
work units, not USD, provider credits, or measured GPU time. The captured policy
is implemented in `src/extras/subagents/cost.rs`; the session's existing monetary
account, prices, and dollar ceiling remain independent. A free model, missing
price, or an on-prem deployment does not disable its work budget.

`task_cost_weighting = "auto"` uses the selected quick-model's explicit pricing
and otherwise known catalogue metadata. With a finite positive input price,
output/cache prices are normalized against that input price and scaled by the
configured input weight. Missing components keep their fallbacks. Anthropic-native
cache-tier fallbacks are 0.1× input for reads, 1.25× for 5-minute writes, and 2×
for 1-hour writes when model metadata does not identify the tier. Without usable
positive input pricing, the configured weights apply; a zero price is never a
division-by-zero or a reason to make all work free.

`task_cost_weighting = "fixed"` uses the configured numbers directly, without
model-price ratios. Defaults, also used as general automatic-mode fallbacks, are:

| Token category | Weight |
|---|---:|
| Uncached input | `1.0` |
| Output, including separately reported reasoning where appropriate | `5.0` |
| Cache read | `1.0` |
| 5-minute cache write | `2.0` |
| 1-hour cache write | `2.0` |

All weights must be finite. Input/output weights must be positive; cache weights
may be zero. Partial weight objects fill omitted fields with these built-in
defaults. For example:

```toml
task_cost_weighting = "auto"
task_max_cost_units = 4000000

[task_cost_weights]
input = 1.0
output = 5.0
cache_read = 1.0
cache_write_5m = 2.0
cache_write_1h = 2.0

[task_cost_overrides.corporate_child]
weighting = "fixed"
weights = { input = 1.0, output = 6.0, cache_read = 1.0, cache_write_5m = 2.0, cache_write_1h = 2.0 }
```

An override is used only when `corporate_child` is the actual selected
quick-model alias and that entry matches the child's provider and model. It is
not inferred from a raw model id or another alias for the same model. An omitted
override `weighting` or `weights` inherits that global setting; an override's
partial **weights object** uses built-in field defaults, rather than merging
individual fields with the global object. Opaque corporate, LiteLLM and vLLM
aliases do not identify an upstream model or prove its prices. Configure exact
quick-model pricing, or select fixed weights explicitly.

The counter separates uncached input, cache reads, 5m/1h writes, output and
exclusive reasoning. Inclusive gateway input counters have cache tokens
subtracted only when consistent; reasoning already included in output is not
added again. Ambiguous write tiers use the greater applicable weight. A combined
usage total without categories uses the largest weight. When usage is missing,
the fallback weights the **complete prepared request estimate** and its reserved
output, without assuming a cache discount. Repair requests consume both their
separate allocation and the same aggregate work budget. Metadata and weights
are captured at the next delegation; refreshes and live edits do not reprice
already running children.

This applies to direct OpenAI, Anthropic and Gemini routes and OpenRouter, plus
OpenAI-compatible LiteLLM/vLLM deployments when their usage fields are available.
Provider documentation describes distinct input/output/cache categories:
[Anthropic pricing](https://platform.claude.com/docs/en/about-claude/pricing),
[OpenAI pricing](https://developers.openai.com/api/docs/pricing), and
[Gemini pricing](https://ai.google.dev/gemini-api/docs/pricing).
[OpenRouter usage accounting](https://openrouter.ai/docs/cookbook/administration/usage-accounting)
exposes usage and billing details; those dollar amounts do not replace this
local work counter. [LiteLLM custom pricing](https://docs.litellm.ai/docs/proxy/custom_pricing)
requires deployment-specific pricing metadata.
[vLLM's OpenAI-compatible server](https://docs.vllm.ai/en/latest/serving/online_serving/openai_compatible_server/)
provides protocol compatibility, which alone does not establish billing rates.
These sources inform the local policy; configured ratios are not a promise of
an exact provider invoice or hardware cost.

### Model resolution (in order of precedence)

1. `subagent_model` is set and matches a **quick model name** (e.g. `"deepseek-v4-flash"`) → uses that quick model's provider + model.
2. `subagent_model` is set but does **not** match a quick model → uses the raw model string with `subagent_provider` (or the main provider as fallback).
3. `subagent_model` is **not** set but `subagent_provider` is → uses the main model with the specified provider.
4. Neither is set → falls back to the main agent's model (same provider + model).

When the subagent uses a different provider than the main agent, a separate
API client is created at startup. The subagent client is independent from the
main agent's client and can be switched at runtime.

If that global provider client cannot initialize, subagents use the main
provider **and main model** instead. Startup displays the configured selection,
the initialization error, and the model actually in use; `/subagent-model`
shows the same fallback. Task calls then use the main model's pricing.
A successful runtime subagent model switch clears the fallback notice.
An unavailable model selected by a specialist persona still fails explicitly;
it does not inherit this global fallback.

### Provider request parameters

Every child request carries the same provider request-body parameters the main
agent would send for the child's model, built by the same helpers:

- the user's `extra_body`, resolved for the child's model exactly like the main
  agent (a matching quick model's `extra_body`, otherwise the global one);
- OpenAI Responses: the `[reasoning]` table mapped to `reasoning.effort`,
  `reasoning.summary`, `store` (so `store = false` zero-data-retention settings
  also apply to subagents), the encrypted-reasoning `include`, and a
  per-child `prompt_cache_key`;
- OpenAI Chat Completions: `[reasoning] effort` as `reasoning_effort`;
- OpenRouter: the Anthropic routing/cache pin plus `extra_body`;
- Anthropic, Gemini, and Ollama: `extra_body`.

A persona's quick-model `extra_body` is shallow-merged on top, so persona keys
win on collision.

Example `config.toml`:

```toml
task_max_turns = 20
task_max_prompts = 8
task_max_concurrency = 4
task_max_output_bytes = 131072
task_max_cost_units = 4000000
task_timeout_secs = 300
task_enabled = true
subagent_model = "deepseek-v4-flash"
subagent_provider = "openrouter"
```

## Current structured handoff contract

- The child receives no conversation history, except a `resume` call, which
  continues one earlier child's own conversation. Callers can provide a
  structured brief (`objective`, `files`, `constraints`, `expected_sections`),
  and every successful return is checked against the Findings, Unverified, and
  Coverage skeleton (mini-agent-nfd7, mini-agent-sux9).
- Persona names are present in the schema enum and orchestrator prompt
  (mini-agent-kh1o), project definitions are trust-gated (mini-agent-yb9w),
  frontmatter controls per-persona descriptions, models, turn budgets, and read-only
  tool subsets (mini-agent-6khf), and trusted `.notes.md` content extends
  generic embedded personas without replacing them (mini-agent-7hjo).

The deterministic harness also runs one compact fixture case for
every shipped persona. Each case resolves the production persona, passes a
structured response through the production task scheduler, and checks its
expected finding plus the host response contract. The fixture lives at
`tests/harness_eval/personas/fixture.json`; see
[HARNESS_EVAL.md](HARNESS_EVAL.md).

See the historical [review plan](../plans/2026-09-05-001-harness-design-review.md) for the design
and delivery record.

## Slash Commands

| Command                            | Description                                |
|------------------------------------|--------------------------------------------|
| `/agent [name]`                    | Show or switch the main-agent persona      |
| `/subagent-model [name]`           | Show or switch the subagent's model        |
| `/subagent-models [name]`          | List quick models or switch subagent to one|

- **`/subagent-model`** with no arguments shows the current subagent provider
  and model, including any startup fallback from an unavailable configured
  provider. With a model name, it switches the subagent to that model (using
  the same provider).
- **`/subagent-models`** with no arguments lists quick models. With a quick
  model name, it switches the subagent to that quick model's provider + model.
  If the quick model uses a different provider, a new API client is created.
- The earlier spellings `/model-subagent` and `/models-subagent` still work as
  hidden aliases; completion and `/help` show only the names above.
- In a build without the `subagents` feature these commands (either spelling)
  print that the feature is required instead of doing nothing.

These commands update the global `SubagentConfig` at runtime. The next call
to the `task` tool picks up the new settings automatically.

## Architecture

```
Main Agent                               Subagent(s)
┌──────────────┐                         ┌─────────────────────┐
│ read/write   │                         │ read                │
│ edit/bash    │  calls "task" tool      │ grep                │
│ grep/find_files│ ──────────────────────→│ find_files          │
│ list_dir     │   with prompt(s)        │ list_dir            │
│ todo         │                         │ memory_read         │
│ task  ───────┤   polls bounded child   │ memory_search       │
│              │                         │ skills_search       │
│              │                         │ read-only js        │
│              │   futures inline via    │                     │
│              │   FuturesUnordered      │                     │
│              │   ──────────────        │ runs ≤ max_turns    │
│              │   returns findings ────→│ returns summary     │
└──────────────┘                         └─────────────────────┘
```

Key files:

| File                                         | Role                                  |
|----------------------------------------------|---------------------------------------|
| `src/extras/subagents/mod.rs`                | Module root, static config            |
| `src/extras/subagents/task_tool.rs`          | `TaskTool` implementation             |
| `src/extras/subagents/builder.rs`            | Subagent construction (`build_explore_agent`) |
| `src/extras/subagents/isolation.rs`          | Worktree-isolated write children (`isolation: worktree`) |
| `src/extras/subagents/inline_images.rs`      | Opt-in inline image lane (`task_inline_images`) |
| `src/extras/subagents/prompt.rs`             | Subagent system prompt                |
| `src/agent/runner.rs` (`run_subagent`)       | Silent agent execution                |
| `src/agent/builder.rs`                       | Wires `TaskTool` into main agent      |
| `src/provider.rs` (`AnyAgent::run_subagent`) | Type-erased dispatch                  |
| `src/main.rs`                                | Initializes `SubagentConfig`          |

## Subagent System Prompt

The subagent receives its own system prompt focused on answering specific
technical questions (`src/extras/subagents/prompt.rs`). It instructs the
subagent to focus on the question given, use the available tools, and report
findings concisely without preamble or wandering.

## Bounded Execution and Partial Results

The task tool keeps at most `task_max_concurrency` child futures in flight.
Queued prompts are started only as slots become available. The whole call owns
all child futures, so dropping or cancelling the call drops every child before
returning; no detached subagent task is left running.

Before launching concurrent children, the scheduler deterministically reserves
the aggregate `task_max_cost_units` budget across all requested prompts. Each
child stops making further provider calls when its reservation is exhausted,
and both completed and cancelled children are charged at most their reserved
share. This prevents concurrent children from each spending the full aggregate
budget before the scheduler observes their usage. A provider response can
report usage beyond its reservation because usage arrives after that response;
such an overrun stops the call immediately and is charged only to the reserved
share in the bounded aggregate report. Images a child reads are charged too: through
provider-reported usage, or, when a route omits usage, through the local
estimate's patch-formula image charge. A child's history admits at most 4 MiB
of encoded image data; see [IMAGE_READ.md](IMAGE_READ.md) for the formula and
the omission behaviour.

A child failure (for example a provider error) is that child's outcome only:
it is reported as `[failed: ...]` and its queued and in-flight siblings run to
completion (mini-agent-xwsaa). Set `[subagents] task_fail_fast = true` to
restore the older behaviour where the first failure stops new launches and
cancels in-flight siblings. Aggregate output exhaustion, aggregate cost
exhaustion, and the whole-call deadline stop new launches and cancel in-flight
children. Every prompt still receives one deterministic status in original
prompt order:

- completed children contain their response;
- a failed child contains `[failed: ...]`;
- an in-flight child cancelled by the deadline or an output/cost limit that
  had already streamed text returns that narration, quoted like any child
  output, under the host header
  `[partial: cancelled (<reason>) after N tool calls; narration so far, not a validated report]`
  (at most 32 KiB per child, sharing the remaining output budget with the
  other cancelled children and marked `…[narration truncated]` when cut);
- other started children contain `[cancelled: ...]` (a fail-fast stop always
  uses this terse form for its siblings);
- queued prompts contain `[not started: ...]`.

A background task is recorded as `failed` only when the call stopped early or
every child failed; a batch with some failed children is `completed` and shows
their `[failed: ...]` sections.

The scheduler checks the absolute deadline before admitting each child.
A result already ready when the scheduler resumes is
retained, but it cannot cause queued work to start at or after the deadline.

Exhausting `task_max_turns` is not a child failure. The child returns all text
accumulated so far followed by `[partial: turn budget exhausted]`; its queued
and in-flight siblings continue normally (mini-agent-ddno).

Output accounting charges each response in its returned (quoted and fenced)
form: a response is cut so that form fits the remaining budget, and the cut is
marked inside that response. Output exhaustion makes the call partial only when
it abandons work that has not run; if every task completed, no partial header
is added. The partial header is reserved when the report is assembled, and a
final aggregate cut inside a quoted child output still closes its
`[subagent output ends]` fence.

Partial returns begin with a summary containing the stop reason and aggregate
started/completed/cost accounting. Cost includes usage already observed from
started siblings before they were cancelled, so partial work cannot bypass the
aggregate budget. The editable `task_max_report_bytes` per-child report cap
defaults to 32 KiB and is bounded by the aggregate limit, while
`task_max_output_bytes` is a final hard cap over the
entire rendered tool result, including headings and status markers.
Scheduling charges completed responses with their quotation prefixes and host
markers, plus any specialist notice, before starting queued work. Task labels
collapse prompt whitespace into a single line and keep at most 60 characters,
ending with `…` when shortened; the full execution prompt is preserved.

Children report their nested tool calls on the parent run's event channel.
Runs started while delegation is installed use a 512-slot channel instead of
the ordinary 32 slots, so several children plus the parent's token stream do
not stall child tool execution while the TUI repaints. It stays one bounded,
lossless, ordered channel; no subagent event is dropped or reordered.

A cancelled child's directory walk (`grep`, `find_files`) stops at the next
entry and the tool returns an error rather than a partial result that looks
complete. At TUI exit, closing background tasks cancels every job at once and
waits at most the teardown timeout (5 s) for their retirement; on expiry it
warns and exits, and a job that did not retire keeps a non-terminal saved
status. Session switches still wait for the full close.

### Editable child sizing

The main session's `context_window` (including a 1,000,000-token pin) remains
independent of a child's input capacity, response size, returned report, and
cumulative work budget. The following settings are available in `/config` and
through ordinary `--config key=value` overrides; changing them needs no recompile.

| Setting | Default | Unit and effect |
|---|---:|---|
| `subagent_context_window` | automatic | Known selected-model capacity, up to `1048576`; unknown models use `131072`. Includes the reserved response. An explicit value overrides automatic sizing, still capped by known capacity. |
| `subagent_max_tokens` | `32000` | Tokens in one child model response |
| `task_max_report_bytes` | `32768` | Bytes retained from one child's report; also capped by the aggregate limit |
| `task_max_output_bytes` | `131072` | Bytes in all reports returned by one task call |
| `task_max_cost_units` | `4000000` | Weighted usage units for the whole task, not dollars |
| `task_report_repair_max_tokens` | `8192` | Tokens in one text-only report restatement |
| `task_report_repair_max_cost_units` | `65536` | Separate weighted repair allocation, still charged to the same task total and USD account |

Concurrency remains 4, prompt count 8, model turns 20 per child, and the whole
call deadline 300 seconds. These are editable as `task_max_concurrency`,
`task_max_prompts`, `task_max_turns`, and `task_timeout_secs`. Concurrency is
editable without recompilation but requires restart: the session owns fixed
semaphores of that size, one shared by background batches and one for the
foreground task call. The other listed sizing
limits apply to the next delegation. The cost policy keys `task_cost_weighting`,
`task_cost_weights`, and `task_cost_overrides` also apply at that boundary;
`/config task_cost_weighting "fixed"` and
`/config task_cost_weights { input = 1.0, output = 5.0 }` persist validated
settings. Use object forms in JSON configuration files and inline tables in
`/config` or `--config` TOML literals. Reports are capped at 32 KiB per child and 128 KiB per batch, roughly 8K
and 32K tokens respectively (actual token counts vary). These caps are what the
parent sees inline: `task` is a self-bounded tool, so the parent's generic
12,000-character tool-result spill (head/tail plus spill path) never cuts a
rendered task result, live or on replay. Lower `task_max_report_bytes` or
`task_max_output_bytes` to make reports shorter. This keeps batch reports
near 3% of a 1M-token parent context by default; a large child context
is not permission to return an equally large report. Read/write authorization,
write serialization, cancellation, and the session spend ceiling are unchanged.

Except for the restart-required concurrency setting, a successfully saved
resource setting applies to the **next delegation**.
Running and background children retain their admission snapshots. Saved project
settings still require ordinary project trust, and higher local/CLI layers still
win. The settings picker shows captured next-delegation values. Model/provider,
tool permissions, and other restart-only settings retain their existing rules.
For example, `/config subagent_context_window 1000000` pins a child context
cap, while `/config subagent_max_tokens 64000` requests a larger response.
Removing the saved `subagent_context_window` key restores automatic model-based
sizing on the next startup.
The selected child's known capacity and an explicitly configured parent
`max_tokens`/`--max-tokens` ceiling still constrain it; the implicit parent default
(the model's catalog output ceiling up to 32K, else 16K) alone is not a ceiling on a separately configured child.
Generated config files leave that main response fallback unset. Existing
`max_tokens` entries remain explicit ceilings; remove or raise that entry when
you want a child response larger than its value.

Context admission uses the existing token estimator over the complete prepared
request (including history, system text and tool schemas) and reserves the
response. It refuses an oversized request locally; the refusal names
`subagent_context_window`. When `compact_enabled` is true (the default), a child
whose next request is refused compacts its own conversation and continues, at
most twice per child: its oldest tool-result payloads are replaced with an
omission notice until the history fits half the child window (call ids and
order are kept), the child writes a tool-free working summary, and it then
continues on the brief plus that summary. These relief requests are ordinary
child requests: they pass the same admission and are charged to the task's
weighted units. With `compact_enabled = false`, or when the summary fails, the
refusal is returned as before. This estimate is not a provider tokenizer;
the provider's actual context/output limits remain authoritative. Unknown model
capacity uses the independent configured/default child window, not the main
session's capacity. A failed format repair preserves the original findings in
the existing bounded host report rather than granting another exploration turn.

Task weighted-unit accounting runs at each accepted child model turn, before
its tools execute. Reaching the shared cap prevents further child requests,
forced continuations and report repair, and cancels owned siblings. Responses
already in flight can overshoot the cap; cancellation reports include their
known usage. If a provider omits usage entirely, admission conservatively
charges the prepared request estimate plus its reserved response allowance.
This does not replace the independent session USD accounting or ceiling.

Fenced code in a report is data: bullets and section names inside it do not
trigger format repair. Byte cuts retain a bounded host partial report with
Findings, Unverified and Coverage sections whenever those sections fit; smaller
allocations return an explicit partial/refusal. Aggregate truncation places its
status outside the quoted, untrusted child text.
