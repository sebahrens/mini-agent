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

Subagents are **opt-in** via the `subagents` Cargo feature:

```toml
# Cargo.toml
[features]
default = ["loop", "git-worktree", "mcp", "subagents"]
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
method, and output contract. Unknown names are silently ignored and fall back
to the default explore prompt.

| `agent_type` | Domain |
|--------------|--------|
| `rust-async-concurrency` | Tokio runtime, `Send`/`Sync` bounds, `Pin`/`Unpin`, cancel-safety |
| `rust-unsafe-code-audit` | UB categories, SAFETY comments, FFI soundness, Phase 6 invariants |
| `rust-security-review` | Trust boundaries, injection, secrets, supply chain, crypto, resource exhaustion |
| `vscode-extension-developer` | VS Code API, webview CSP, postMessage, ACP stdio, vsce packaging |
| `informatica-mapplet-to-fabric-sql` | PowerCenter/IDMC mapplet → Fabric T-SQL, order-dependence audit, reconciliation |
| `azure-cloud-architect` | Azure topology, identity, reliability, cost shape, IaC |

`rust-unsafe-code-audit` owns memory safety and `unsafe`; `rust-security-review`
owns everything that is safe Rust and still a vulnerability. They are
deliberately disjoint — use both when a review needs both.

`azure-cloud-architect` remains a read-only specialist for repository-backed
architecture investigations. Because a child receives no conversation history,
its report starts with constraints and marks unverified values as assumptions;
it makes a recommendation only when those stated constraints support one.

Definitions are plain markdown resolved by `src/context/agents.rs`, highest
priority first:

```
.zerostack/agents/<name>.md      # project override
data_dir/agents/<name>.md        # user global
data/agents/<name>.md            # compiled-in default
```

When a project definition wins, the host prefixes the task result with its
`.zerostack/agents/<name>.md` source. This makes a repository-controlled
replacement visible to the calling agent instead of silently presenting it as
the compiled-in specialist.

Adding a file to `data/agents/` is enough to register a new type; the filename
stem is the `agent_type` value. Update the `agent_type` description in
`src/extras/subagents/task_tool.rs` so the main agent knows the type exists.

## What the Subagent Can Do

### Read tools (always available)

| Tool       | Purpose                       |
|------------|-------------------------------|
| `read`     | Read file contents            |
| `grep`     | Regex search in files         |
| `find_files` | Find files by glob pattern |
| `list_dir` | List directory contents       |
| `todo`     | Track exploration steps       |

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
| `bash`         | Not needed — read tools cover exploration |
| `memory_write` | Subagent should not persist memory      |
| `mcp_tool`     | External, unpredictable — out of scope  |

## Security & Permissions

The subagent is built with **no permission system** (`permission: None` on all
its tools). This is safe because it only has read tools:

- **Read tools with `None` permission** will read any path without checks,
  but they cannot write, edit, or execute commands.
- The worst a subagent can do is read files, which is exactly what it is
  designed for.

The main agent's `task` tool itself goes through the normal permission check
(`check_perm("task", …)`), so users can allow/ask/deny it via their
`opencode.json` permission rules.

## Configuration

| Config field                | Type     | Default                  | Description |
|-----------------------------|----------|--------------------------|-------------|
| `task_max_turns`            | `usize`  | `20`                     | Max agent turns per subagent |
| `task_max_prompts`          | `usize`  | `8`                      | Max child prompts in one tool call |
| `task_max_concurrency`      | `usize`  | `4`                      | Max simultaneously running children |
| `task_max_output_bytes`     | `usize`  | `262144` (256 KiB)       | Hard cap on the complete returned tool output |
| `task_max_cost_units`       | `u64`    | `500000`                 | Aggregate provider token/cost-unit budget |
| `task_timeout_secs`         | `u64`    | `300`                    | Whole-call wall-clock deadline |
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

Example `opencode.json`:

```json
{
  "task_max_turns": 20,
  "task_max_prompts": 8,
  "task_max_concurrency": 4,
  "task_max_output_bytes": 262144,
  "task_max_cost_units": 500000,
  "task_timeout_secs": 300,
  "task_enabled": true,
  "subagent_model": "deepseek-v4-flash",
  "subagent_provider": "openrouter"
}
```

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
│ todo         │                         │ todo                │
│ task  ───────┤   spawns parallel       │ memory_read         │
│              │   subagents via         │ memory_search       │
│              │   tokio::spawn          │                     │
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
started/completed/cost accounting. A 128 KiB per-child response cap remains as
defense in depth, while `task_max_output_bytes` is a final hard cap over the
entire rendered tool result, including headings and status markers.
