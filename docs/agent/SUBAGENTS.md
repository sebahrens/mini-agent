---
description: "Parallel read-only subagents in zerostack: the task tool, model and provider overrides, and per-agent tool limits."
---

# Subagents (read-only codebase exploration)

## Overview

Subagents let the main agent delegate **precise read-only investigations** to a
**read-only child agent**. Each subagent receives a specific technical question
(e.g. "Where is MCP support implemented?") and returns a focused answer.
This keeps the main agent's context clean while enabling thorough lookups.

Subagents are designed for **highly specific questions**, not wide exploration.
Avoid broad instructions like "check all documentation" — instead ask precise
questions that can be answered with a few file reads and searches.

When the main agent calls the `task` tool, one subagent is scheduled per
prompt. Multiple prompts use **bounded parallelism**. Each subagent has access
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
default = ["loop", "git-worktree", "mcp", "acp", "subagents", "archmd",
           "status-signals", "multithread", "export", "js", "sandbox", "memory"]
```

Task-specific schema/provenance adapters and reasoning-budget accounting follow
the `subagents` feature; portable metadata and provider tests retain their helpers.
Main-agent definition loading remains available when subagents are disabled.

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

`prompts` and `briefs` are mutually exclusive. Brief fields are rendered into
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
.zerostack/agents/<name>.md      # trusted project override
data_dir/agents/<name>.md        # user global
data/agents/<name>.md            # compiled-in default
```

A trusted project may also provide `.zerostack/agents/.notes.md`. Unlike a
same-named definition, this file replaces nothing: the host appends it under a
`## Project notes` boundary to every resolved persona after overrides have
been selected. The shared file is limited to 64 KiB and each combined persona
remains under the 256 KiB prompt cap. Its exact path is included in permission
and result provenance. Invalid, empty, unreadable, or oversized notes are
ignored with a warning and a visible task-result notice.

Project definitions participate only when the exact current
`.zerostack/config.toml` is bound in the private project-config trust store.
Project notes use the same gate. An untrusted checkout cannot add, replace, or
extend agent types; resolution falls back to user-global or compiled-in
definitions. Changing the project config content or copying the checkout
invalidates that content-and-path-bound trust.

Before reading a persona file, the task permission prompt identifies the
requested `agent_type` and the highest-precedence source path that would be
loaded, without embedding or parsing the specialist prompt body. Once approved,
the resolved provider/model, effort tier, and tool subset are applied. When a trusted
project definition wins, the host also prefixes the task result with its
`.zerostack/agents/<name>.md` source. This makes a repository-controlled
replacement visible to the calling agent instead of silently presenting it as
the compiled-in specialist. `SubagentStart` and `SubagentStop` hook envelopes
carry that resolved `agent_type` and definition source; unspecialized children
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
effort: medium
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
  Empty lists create a tool-free child; mutating or unknown tools reject the
  definition rather than widening authority.
- `model` is first resolved as a configured quick-model alias, including its
  provider and `extra_body`. Otherwise it is a raw model ID on the current
  subagent provider. A provider switch that cannot authenticate fails the task
  explicitly rather than falling back to another model.
- `effort` is `low`, `medium`, or `high`. It selects one-third, two-thirds, or
  all of the configured `task_max_turns`, rounded up, and can never widen that
  global cap.
- `mode` remains the prompt default used when the persona is selected for the
  main loop through `/agent`. Child-only `tools`, `model`, and `effort`
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
honest-unknowns, read-only, and no-shell rules after the specialization,
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
If a model returns unstructured text, mini-agent retains it under `Raw child
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

### Explicitly excluded

| Tool           | Reason                                  |
|----------------|-----------------------------------------|
| `write`        | Subagent is read-only by design         |
| `edit`         | Subagent is read-only by design         |
| `bash`         | Children stay inside the read-only containment boundary; they cannot run builds or tests and must say so |
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
- **No mutation tools**: the child has no `write`, `edit`, `bash`, or
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
normal TOML/YAML/JSON zerostack configuration described in
[CONFIG.md](CONFIG.md#permission-config).

## Configuration

| Config field                | Type     | Default                  | Description |
|-----------------------------|----------|--------------------------|-------------|
| `task_max_turns`            | `usize`  | `20`                     | Max agent turns per subagent |
| `task_max_prompts`          | `usize`  | `8`                      | Max child prompts in one tool call |
| `task_max_concurrency`      | `usize`  | `4`                      | Max simultaneously running children |
| `task_max_output_bytes`     | `usize`  | `262144` (256 KiB)       | Hard cap on the complete returned tool output |
| `task_max_cost_units`       | `u64`    | `500000`                 | Aggregate provider token/cost-unit budget |
| `task_timeout_secs`         | `u64`    | `300`                    | Whole-call wall-clock deadline; completed sibling results are retained when it expires |
| `task_enabled`              | `bool`   | `true`                   | Whether the `task` tool is registered |
| `subagent_model`            | `string` | `none (uses main model)` | Model name or quick-model alias |
| `subagent_provider`         | `string` | (same as main)           | Provider for the subagent (optional) |

All numeric task limits must be greater than zero.
`task_max_output_bytes` must be at least 256, leaving room for an explicit
partial-status header, and `task_timeout_secs` cannot exceed 86400 (24 hours).
Cost units use the provider-reported aggregate token usage when present.
Cache-read input is charged at one tenth of its reported token count (rounded
up, so a non-empty hit is never free); cache-creation input remains fully
charged. If a provider omits the aggregate total, itemized usage is used. If it
reports no usage at all, the task tool falls back to a text-size estimate so
unknown usage is not treated as free.

### Model resolution (in order of precedence)

1. `subagent_model` is set and matches a **quick model name** (e.g. `"deepseek-v4-flash"`) → uses that quick model's provider + model.
2. `subagent_model` is set but does **not** match a quick model → uses the raw model string with `subagent_provider` (or the main provider as fallback).
3. `subagent_model` is **not** set but `subagent_provider` is → uses the main model with the specified provider.
4. Neither is set → falls back to the main agent's model (same provider + model).

When the subagent uses a different provider than the main agent, a separate
API client is created at startup. The subagent client is independent from the
main agent's client and can be switched at runtime.

Example `config.toml`:

```toml
task_max_turns = 20
task_max_prompts = 8
task_max_concurrency = 4
task_max_output_bytes = 262144
task_max_cost_units = 500000
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
  frontmatter controls per-persona descriptions, models, effort, and read-only
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
| `/model-subagent [name]`           | Show or switch the subagent's model        |
| `/models-subagent [name]`          | List quick models or switch subagent to one|

- **`/model-subagent`** with no arguments shows the current subagent provider
  and model. With a model name, it switches the subagent to that model (using
  the same provider).
- **`/models-subagent`** with no arguments lists quick models. With a quick
  model name, it switches the subagent to that quick model's provider + model.
  If the quick model uses a different provider, a new API client is created.

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

Exhausting `task_max_turns` is not a child failure. The child returns all text
accumulated so far followed by `[partial: turn budget exhausted]`; its queued
and in-flight siblings continue normally (mini-agent-ddno).

Partial returns begin with a summary containing the stop reason and aggregate
started/completed/cost accounting. Cost includes usage already observed from
started siblings before they were cancelled, so partial work cannot bypass the
aggregate budget. A 128 KiB per-child response cap remains as
defense in depth, while `task_max_output_bytes` is a final hard cap over the
entire rendered tool result, including headings and status markers.
Scheduling charges completed responses with their quotation prefixes and host
markers, plus any specialist notice, before starting queued work. Task labels
collapse prompt whitespace into a single line and keep at most 60 characters;
the full execution prompt is preserved.
