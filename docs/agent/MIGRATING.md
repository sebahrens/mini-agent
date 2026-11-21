---
title: "Coming from Claude Code, Codex or opencode"
description: "Which of your existing Claude Code, Codex and opencode files mini-agent reads unchanged, which need an opt-in, and what is intentionally different."
---

# Coming from Claude Code, Codex or opencode

mini-agent reads many files other coding agents already keep. This page tells you which ones work
unchanged, which need a flag or config key, and which have no equivalent. In the tables below,
text in backticks is always something you type into mini-agent; names from other tools are written
in plain text.

Every claim here describes the current code. If a row says "not read", nothing in mini-agent
loads it, so copy the content over by hand.

Three rules hold for every tool:

- **Instruction text is data and executable assets need trust.** `AGENTS.md`, `CLAUDE.md` and
  `.claude/rules` load as inert text. A project's hooks, `.mcp.json`, agents, skills and commands
  stay inactive until you approve the project (`mini-agent trust`, see [CONFIG.md](CONFIG.md)),
  and ancestors of the launch directory never supply executable assets.
- **No subscription login for Claude.** mini-agent signs in to a ChatGPT or GitHub Copilot
  account (`--login openai`, `--login copilot`) but does not offer Claude Pro/Max sign-in; use an
  API key or a gateway for Claude models (see [PROVIDERS.md](PROVIDERS.md)).
- **Start with the guided setup.** `mini-agent --setup` picks a provider and key, and `--tutor`
  prints the getting-started guide. Run `mini-agent --doctor` when something does not behave as
  this page says; [TROUBLESHOOTING.md](TROUBLESHOOTING.md) explains each line.

## Coming from Claude Code

### What is read automatically

| Your Claude Code file | mini-agent behaviour |
| --- | --- |
| `CLAUDE.md` in the project and in parent directories up to the git root | Loaded as instructions beside `AGENTS.md` (`repo_root_instruction_files`, default on). Nested copies load when a file tool first touches that directory. |
| `CLAUDE.local.md` | Loaded as personal project guidance (needs project trust). |
| `.claude/rules/*.md` | Loaded as project rules. Symlinks that leave the workspace need `rule_external_symlinks = true`. |
| `.claude/commands/*.md` | Custom slash commands (project layer, needs project trust). Nested folders become `/team:frontend:review`. |
| `.claude/skills/<name>/SKILL.md` | Agent Skills, discovered in the project, in nested directories and in `~/.claude/skills`. |
| `.claude/agents/*.md` | Specialist definitions for the `task` tool (project layer, needs trust). |
| `.claude/settings.json` in the project | Only its `hooks` and `disableAllHooks` keys are read; each hook binding needs an exact approval (`mini-agent hooks trust`). |
| `.mcp.json` in the project | MCP servers; the file needs its own trust approval (see [MCP_CONFIGURATION.md](MCP_CONFIGURATION.md)). |

### What needs an opt-in

| Your Claude Code file or habit | Opt-in |
| --- | --- |
| `~/.claude/CLAUDE.md` and `~/.claude/rules` | `user_instruction_files = true` |
| `~/.claude/commands/*.md` | On by default; `claude_user_commands = false` turns it off. |
| `~/.claude/agents/*.md` | `claude_user_agents = true` |
| `--add-dir DIR` to give Claude access to another directory | `--add-dir DIR` only adds skill roots. Add `add_dir_tool_access = true` so the file tools may reach DIR under the normal permission rules. |
| Hooks run from a headless job | `--trust-project-hooks` (or `MINI_AGENT_TRUST_PROJECT_HOOKS`), or approve once with `mini-agent hooks trust`. |
| A project config in CI or a container | `--trust-project-config`, or `mini-agent trust --yes`. |

### What has no equivalent

| Claude Code | In mini-agent |
| --- | --- |
| Claude Pro/Max login | Not offered; use an API key (`ANTHROPIC_API_KEY`), Bedrock, Vertex, Azure or a gateway. |
| `~/.claude/settings.json` (permissions, env, model) | Never read. Translate it into `config.toml` and `permission-allow`/`permission-deny` rules by hand. |
| Plugins and marketplaces | None; `skills_plugin_dirs` points at plugin-shaped skill folders only. |
| SDKs, GitHub app, desktop app, web/mobile, voice | None. For CI see [CI.md](CI.md); for editors see [EDITORS.md](EDITORS.md). |
| Agent teams | None. Subagents (`task`) and `@agent:<name>` mentions exist. |
| Inline `` !`command` `` in headless or ACP runs | Works in the interactive session only (after the shell permission rules allow it). Headless `-p` and ACP fail closed on a command file that uses it. |

### Concept map

| Claude Code | mini-agent |
| --- | --- |
| default permission mode | `guarded` (reads allowed, everything else asks) |
| acceptEdits mode | `standard` (edits inside the workspace accepted; shell and web ask) |
| plan mode | `planwrite` (`/plan` or `/mode planwrite`); read-only except the plan file. It is not in the default Shift+Tab ring; add it with `permission_mode_cycle`. |
| bypassPermissions | `--dangerously-skip-permissions`; the milder `--yolo` still asks for destructive shell commands |
| Shift+Tab | Cycles `standard`, `guarded`, `yolo` (configurable with `permission_mode_cycle`) |
| `/resume` | `/sessions` (`/resume` is accepted as an alias); `--continue`, `--resume` and `--session` at launch |
| `/rewind`, checkpoints | `/rewind`, `/undo`, `/redo`; file checkpoints cover native edits only, not shell side effects (see [file-checkpoints.md](file-checkpoints.md)) |
| `/compact` | `/compact` (`/compact undo` restores the replaced transcript) |
| `/cost`, `/usage` | `/cost` (`/usage` is an alias) |
| `/context` | `/context` |
| `/status` | `/context` (alias) and `/doctor` |
| `/agents` | `/agent` (alias `/agents`) |
| `/init` | `/init` (creates `AGENTS.md`) |
| `/model` | `/model`, `/models`, `/effort` |
| `/mcp` | `/mcp` |
| `/hooks` | `/hooks` |
| `/review` | `/review` (local review of your working tree; it does not post to a pull request) |
| `claude -p "..."` | `mini-agent -p "..."` |
| `--output-format json` | `--output json` (`--output-format` is accepted as an alias) |
| `--output-format stream-json` | `--output stream-json --stream-json-schema claude` |
| `--max-turns` | `--max-turns` (alias of `--max-agent-turns`) |
| `--allowedTools`, `--disallowedTools` | `--allowed-tools`, `--disallowed-tools` (aliases of `--allow` and `--deny`, with `TOOL:PATTERN` rules) |
| `--append-system-prompt` | `--append-system-prompt` |
| `--mcp-config`, `--strict-mcp-config` | `--mcp-config`, `--strict-mcp-config` |
| `--continue` | `--continue` |
| auto-update | `mini-agent update` (manual); `update_check` controls the opt-in notice |

### Where settings live

| Purpose | Claude Code | mini-agent |
| --- | --- | --- |
| User settings | `~/.claude/settings.json` | `~/.mini-agent/config.toml` (Linux and macOS installs created earlier may use the platform config directory; `mini-agent --print-config` shows the resolved path) |
| Project settings | `.claude/settings.json` | `.mini-agent/config.toml` (needs project trust for sensitive keys) |
| Local, uncommitted settings | `.claude/settings.local.json` | `.mini-agent/config.local.toml` |
| Hooks | `settings.json` `hooks` | `settings.json` in the config directory, or the project's `.claude/settings.json` / `.mini-agent/settings.json` |
| Managed policy | managed settings file | `managed-config.toml` and `managed-settings.json` (see [CONFIG.md](CONFIG.md)) |

### Hooks written for Claude Code

A project `.claude/settings.json` hook usually works unchanged: `$CLAUDE_PROJECT_DIR` is expanded and
exported. mini-agent never passes `command` to a shell, so a command string that contains `;`, `|`,
`&`, `<` or `>` skips that one handler with a startup notice. Rewrite it as
`"command": "/bin/sh", "args": ["-c", "..."]`.

## Coming from Codex

| Codex | mini-agent |
| --- | --- |
| `AGENTS.md` at the repository root and below | Read natively, nested and `@path` includes included. |
| `~/.codex/AGENTS.md` | `user_instruction_files = true` (loads after mini-agent's own global `AGENTS.md`). |
| `~/.codex/config.toml` | Not read. mini-agent has its own `config.toml`; the keys differ. Run `mini-agent --setup`, or see [CONFIG.md](CONFIG.md). |
| Codex custom prompts and skills folders | Not read. A skill folder in the Agent Skills layout works from `.agents/skills`, `~/.agents/skills` or `mini-agent --import-agent-skill <path>`. |
| `codex exec "..."` | `mini-agent -p "..."`; add `--output json` or `--output stream-json` for machine output. |
| Approval policy and sandbox mode | Permission modes (`--restrictive`, `--guarded`, `--accept-all`, `--yolo`, `--read-only`) are separate from the sandbox (`--sandbox`, `--sandbox-profile`, `--sandbox-network`). |
| Resume | `--continue`, `--resume`, `--session`, and `/sessions` in the TUI. |
| ChatGPT sign-in | `mini-agent --login openai` (several registrations: `--login-account new`). |
| MCP servers in `config.toml` | Re-enter them with `/mcp add` or a project `.mcp.json`; the TOML tables are not imported. |
| Cloud tasks, IDE extension, SDK | None; see [CI.md](CI.md) and [EDITORS.md](EDITORS.md) for the closest paths. |

## Coming from opencode

| opencode | mini-agent |
| --- | --- |
| `AGENTS.md` | Read natively. |
| `opencode.json` and `.opencode/` folders (agents, commands, plugins, themes) | Not read. Convert agents to `.mini-agent/agents/<name>.md`, commands to `.mini-agent/commands/<name>.md` and MCP servers to `.mcp.json`. |
| Skills in `.claude/skills` or `.agents/skills` | Read as they are (both folders are discovered). |
| Providers and credentials from `opencode auth` | Not read. Use `mini-agent --setup`, environment variables such as `OPENROUTER_API_KEY`, or `custom_providers` for any OpenAI-compatible endpoint. |
| `opencode run "..."` | `mini-agent -p "..."`. |
| Plugins | None. |
| `opencode github install` | None; see the workflow recipe in [CI.md](CI.md). |

## Checking that your files are picked up

From the repository you are migrating:

1. Run `mini-agent --print-config` to see which config files and trust states apply.
2. Start `mini-agent` and run `/context` to list the instruction files that loaded (names and
   sizes only), then `/skills`, `/agent` and the command picker (`/`) to see skills, agents and
   commands. Anything missing is usually waiting for trust: run `mini-agent trust --list`.
3. Run `/hooks` to list approved hook bindings and `/mcp` for configured servers.
