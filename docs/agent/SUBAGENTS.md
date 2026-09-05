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

## Feature Gate

Subagents are gated by the `subagents` Cargo feature and are included in the
default build:

```toml
# Cargo.toml
[features]
default = ["loop", "git-worktree", "mcp", "acp", "subagents", "archmd",
           "status-signals", "multithread", "export", "js", "sandbox", "memory"]
```

## The `task` Tool

The main agent has a new tool called `task`. It accepts:

```json
{
  "prompts": ["explore the auth module", "find all API route definitions"]
}
```

- **Single prompt**: one subagent explores, returns findings.
- **Multiple prompts**: up to `task_max_concurrency` subagents run at once.
  Each result appears under a `## Task N:` heading in original prompt order.
- The complete request is rejected before permission checking or execution if
  it is empty, contains a blank prompt, or exceeds `task_max_prompts`.

## Specialist Agent Types

`task` also accepts an optional `agent_type`. When set, the named definition is
prepended to the base explore prompt as the authoritative persona, scope,
method, and output contract. Unknown names are rejected with the valid names so
a misspelled specialist can never masquerade as a generic exploration.

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
first:

```
.zerostack/agents/<name>.md      # project override
data_dir/agents/<name>.md        # user global
data/agents/<name>.md            # compiled-in default
```

When a project definition wins, the host prefixes the task result with its
`.zerostack/agents/<name>.md` source. This makes a repository-controlled
replacement visible to the calling agent instead of silently presenting it as
the compiled-in specialist.

The filename stem is the `agent_type` value and must be 1–64 lowercase ASCII
letters, digits, or hyphens, without leading, trailing, or repeated hyphens.
An optional YAML frontmatter block may declare a matching `name`; the block is
validated and removed before the prompt is installed. Malformed metadata,
mismatched names, and empty prompt bodies are ignored silently and the
compiled-in definition of the same name is used instead (mini-agent-cflr). The
`agent_type` schema field is a plain string: the installed definitions are
resolved dynamically when a call is validated, and the valid names are listed
only in the error returned for an unknown name. The schema does not enumerate
them yet (mini-agent-kh1o). Frontmatter keys other than `name` (for example
`tools:` or `model:`) are ignored (mini-agent-6khf).

Project definitions are loaded without a project-trust check; a checkout can
therefore replace a specialist's prompt, and the base prompt tells the child
that the specialization overrides its defaults. The read-only tool set still
holds. Trust gating is planned (mini-agent-yb9w).

## What the Subagent Can Do

### Read tools (always available)

| Tool       | Purpose                       |
|------------|-------------------------------|
| `read`     | Read file contents            |
| `grep`     | Regex search in files         |
| `find_files` | Find files by glob pattern |
| `list_dir` | List directory contents       |

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
  `mcp_tool`, so it cannot modify files, run shell commands, or reach external
  MCP servers regardless of permissions.
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
| `task_timeout_secs`         | `u64`    | `300`                    | Whole-call wall-clock deadline. A fixed 300 s per-child cap also applies and is not configurable (mini-agent-166x) |
| `task_enabled`              | `bool`   | `true`                   | Whether the `task` tool is registered |
| `subagent_model`            | `string` | `none (uses main model)` | Model name or quick-model alias |
| `subagent_provider`         | `string` | (same as main)           | Provider for the subagent (optional) |

All numeric task limits must be greater than zero.
`task_max_output_bytes` must be at least 256, leaving room for an explicit
partial-status header, and `task_timeout_secs` cannot exceed 86400 (24 hours).
Cost units use the provider-reported aggregate token usage when present.
Because provider usage shapes differ, cached and cache-creation input are
conservatively included. If a provider reports no usage, the task tool falls
back to a text-size estimate so unknown usage is not treated as free.

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

## Known limits and planned changes (2026-09-05 review)

- A child that reaches `task_max_turns` currently fails with a provider `MaxTurnsError`; the
  failure cancels the other prompts in the same call and its partial text is discarded
  (mini-agent-ddno). Planned: return the partial report with a `[partial: turn budget
  exhausted]` marker and keep siblings running.
- `SubagentStart`/`SubagentStop` hooks receive the fixed agent type `explore` regardless of
  `agent_type` (mini-agent-abys).
- The child receives only the prompt string: no conversation history, files, or constraints,
  and it returns free text. Planned: a structured brief (`objective`, `files`, `constraints`,
  `expected_sections`) and a host-checked return skeleton (Findings, Unverified, Coverage)
  (mini-agent-nfd7, mini-agent-sux9).
- Planned: per-persona `model`, `tools` subset, and `description` frontmatter
  (mini-agent-6khf); persona names in the schema enum and in the orchestrator prompt
  (mini-agent-kh1o); trust-gated project definitions (mini-agent-yb9w); an append-only project
  notes layer so embedded personas can stay generic (mini-agent-7hjo).

See [the review plan](../plans/2026-09-05-001-harness-design-review.md) for the full list.

## Slash Commands

| Command                            | Description                                |
|------------------------------------|--------------------------------------------|
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

Partial returns begin with a summary containing the stop reason and aggregate
started/completed/cost accounting. Cost includes usage already observed from
started siblings before they were cancelled, so partial work cannot bypass the
aggregate budget. A 128 KiB per-child response cap remains as
defense in depth, while `task_max_output_bytes` is a final hard cap over the
entire rendered tool result, including headings and status markers.
