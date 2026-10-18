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
immediately. The same persona, tools, permission checks, deadlines, captured
prices and session cost cap apply. The session keeps at most 32 tasks, including
finished reports; start a new session when that bounded history is full.
Background batches share one lane of `task_max_concurrency` children, while the
parent turn's own foreground `task` call uses a separate lane of the same size,
so running background jobs cannot serialize it. Write-enabled children across
all batches share one write lane. These permits stay owned through actual
child-effect retirement, including cancellation, so a queued child cannot
overlap a still-retiring write. Admission checks captured spend and workspace
identity again after waiting for that lane. Time a call's children spend
waiting for a lane is not charged to its deadline (see
[Configuration](#configuration)).

Use `task_list`, `task_result {"id":"…"}` and `task_cancel {"id":"…"}` from an
agent, or `/tasks`, `/tasks result <id>` and `/tasks cancel <id>` from the TUI.
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
defaults remain 32 KiB per child and 128 KiB per batch. No report path is
dereferenced on import.

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

- **Single prompt**: one subagent explores, returns findings.
- **Multiple prompts or briefs**: up to `task_max_concurrency` subagents run at once.
  Each result appears under a `## Task N:` heading in original prompt order.
- The complete request is rejected before permission checking or execution if
  it is empty, contains a blank prompt, or exceeds `task_max_prompts`.

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
.zerostack/agents/<name>.md      # trusted legacy project override (highest)
.mini-agent/agents/<name>.md     # trusted mini-agent project definition
.claude/agents/<name>.md         # trusted Claude-style project definition
data_dir/agents/<name>.md        # user global
data/agents/<name>.md            # compiled-in default
```

`src/context/mentions.rs` parses agent mentions in submitted text against the
same trust-gated resolution. The canonical token is `@agent:<name>`; Claude
Code's `@agent-<name>` alias is rewritten to it only when the name resolves and
otherwise stays plain text. Tokens are whitespace-delimited and must start with
`@` (so `x@agent:8080` and e-mail addresses are prose); surrounding punctuation
is trimmed. An unresolvable canonical mention fails with an error naming the
sorted valid personas, and untrusted-project personas never resolve.

The `task` schema describes specialists as read-only by default and documents
the explicit opt-in for implementation personas.

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

Write children run one at a time within a task call, regardless of
`task_max_concurrency`; the parent's existing exclusive task lane orders task
calls with other parent mutations. This does not provide cross-process file
locking. Their edits count toward goal-round mutation accounting. They receive
neither nested task delegation nor memory writes, MCP, or additional JS effect
authority. Their JS realm remains read-only. The same report contract and
bounded scheduler cancellation apply. Existing personas without `mode: write`
keep their read-only tool set even when the global opt-in is enabled.

A trusted project may also provide `.zerostack/agents/.notes.md`. Unlike a
same-named definition, this file replaces nothing: the host appends it under a
`## Project notes` boundary to every resolved persona after overrides have
been selected. The shared file is limited to 64 KiB and each combined persona
remains under the 256 KiB prompt cap. Its exact path is included in permission
and result provenance. Invalid, empty, unreadable, or oversized notes are
ignored with a warning and a visible task-result notice.

Project definitions participate only when the exact current
`.zerostack/config.toml` is bound in the private project-config trust store.
All three project directories use this existing gate, even when the definitions
live under `.claude` or `.mini-agent`; those directories do not create an
independent trust grant. Project notes use the same gate and remain supported
only at `.zerostack/agents/.notes.md`. Persona files themselves are not part of
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
entries and 128 Markdown files; an over-limit layer is ignored in full rather
than selecting files in filesystem order. Each file remains capped at 256 KiB,
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
model: fast-review
turn_budget: medium
mode: review
---
```

- `description` is the bounded (160-character) summary shown in the task
  tool's `agent_type` schema. Without it, the first prompt paragraph is used.
- `tools` narrows the installed child tools. It accepts a YAML list or a
  comma-separated string. Canonical names are `read`, `grep`, `find_files`,
  `list_dir`, `js` when JS support is compiled, and `skills_search` when skills support is compiled, plus
  `memory_read` and `memory_search` when memory support is compiled.
  Claude-style `Read`, `Grep`, and `Glob` spellings are accepted.
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
  are not requirements. Read-only children have no shell, MCP, or nested `Agent`
  route. Use the parent's authorized indexer tools or a compatible read-only
  persona instead. New imported personas should declare `required_tools`.
  Task schema descriptions display declared requirements alongside resolved
  definitions; a requirement is not a promise that the tool is installed.
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
- `mode: write` selects an implementation child only with the global opt-in.
  Other `mode` values remain the prompt default used when the persona is selected for the
  main loop through `/agent`. Child-only `tools`, `required_tools`, `model`, and `turn_budget`
  settings do not alter main-loop authority.

The block is validated and removed before the prompt is installed. Malformed
metadata, mismatched names, empty prompt bodies, invalid UTF-8, and oversized
files are rejected. The loader emits a warning naming the file and reason. If the
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
list. Unknown frontmatter keys are ignored for forward compatibility, logged,
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
bounded repair turn (a single model reply, no tools, charged to the task
budget) asking it to restate its own previous response in the required
sections; a negative result such as "no such capability exists" is a finding.
The restatement is used only if it passes validation. Otherwise mini-agent
retains the original text under `Raw child
response` but wraps it in a machine-checkable partial report with low
confidence. This keeps malformed output useful without letting it masquerade as
a complete specialist report.

## What the Subagent Can Do

### Read tools (always available)

| Tool       | Purpose                       |
|------------|-------------------------------|
| `read`     | Read file contents            |
| `grep`     | Regex search in files         |
| `find_files` | Find files by glob pattern |
| `list_dir` | List directory contents       |

### Read-only JavaScript (when `js` is enabled and contained)

The child `js` tool supports local computation plus exactly three brokered
effects: `read_file`, `list_dir`, and `grep`. `write_file`, `fetch`, `spawn`,
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

### Default profile exclusions

| Tool           | Reason                                  |
|----------------|-----------------------------------------|
| `write`        | Available only to an opted-in `mode: write` child         |
| `edit`         | Available only to an opted-in `mode: write` child         |
| `bash`         | Only opted-in write children receive the parent’s eligible `shell`; default children cannot run builds or tests |
| `memory_write` | Subagent should not persist memory      |
| `todo`         | Not registered — no planning tool in child context |
| `task`         | Nested subagents are deliberately unsupported |
| `mcp_tool`     | External, unpredictable — out of scope  |

## Security & Permissions

The subagent **inherits** the parent's authorization context
(`SubagentAuthorization`), which carries the parent's `PermCheck`, approval
channel (`AskSender`), and workspace binding. Every child tool respects the
same path-containment and approval rules as the parent:

- **Path containment**: reads outside the workspace binding are denied or sent
  through the parent approval channel, exactly as they would be for the main
  agent.
- **Default profile has no mutation tools**: the child has no `write`, `edit`, `bash`, or
  `mcp_tool`. Its optional JS realm has no writer, network, process, or
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
| `task_timeout_secs`         | `u64`    | `300`                    | Whole-call wall-clock deadline; completed sibling results are retained when it expires |
| `task_enabled`              | `bool`   | `true`                   | Whether the `task` tool is registered |
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

- The child receives no conversation history. Callers can provide a
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

The first child failure stops new launches and cancels in-flight siblings.
Aggregate output exhaustion, aggregate cost exhaustion, and the whole-call
deadline do the same. Every prompt still receives one deterministic status in
original prompt order:

- completed children contain their response;
- the triggering failure contains `[failed: ...]`;
- started siblings contain `[cancelled: ...]`;
- queued prompts contain `[not started: ...]`.

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
collapse prompt whitespace into a single line and keep at most 60 characters;
the full execution prompt is preserved.

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
and 32K tokens respectively (actual token counts vary). This keeps batch reports
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
`max_tokens`/`--max-tokens` ceiling still constrain it; the implicit parent 16K
fallback alone is not a ceiling on a separately configured child.
Generated config files leave that main response fallback unset. Existing
`max_tokens` entries remain explicit ceilings; remove or raise that entry when
you want a child response larger than its value.

Context admission uses the existing token estimator over the complete prepared
request (including history, system text and tool schemas) and reserves the
response. It refuses an oversized request locally without trimming signed
history or silently compacting it. This estimate is not a provider tokenizer;
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
