---
title: "Feature parity matrix"
description: "How mini-agent compares with Claude Code, Codex and opencode, per feature, with the date each cell was last verified."
---

# Feature parity matrix

Snapshot of 2026-11-21, refreshed against the code after that day's changes (see the
`[Unreleased]` section of CHANGELOG.md). Legend: **Y** yes, **P** partial, **N** no, **?** not verified.

- The **mini-agent** column is verified against this repository: `mini-agent --help`, the slash
  command registry, `docs/agent` and the source.
- A competitor cell marked `*` comes from a vendor page fetched on 2026-11-21: Claude Code from
  `code.claude.com/docs/llms.txt`, Codex from `learn.chatgpt.com/docs/codex/cli`, opencode from
  `opencode.ai/docs` and `/docs/config`. An unmarked competitor cell is the reviewers' prior
  knowledge. Re-check every `?` and unmarked cell against the vendor's documentation before quoting
  this table outside the project.
- "nvnkx review" in the last column means the gap is tracked in the 2026-11-21 harness review epic
  (`mini-agent-nvnkx`, label `review-2026-11-21-harness`); a bead id is named where one owns the row.

| Feature | mini-agent | Claude Code | Codex | opencode | Tracking |
| --- | --- | --- | --- | --- | --- |
| Subscription login | P (ChatGPT, Copilot; Claude Pro/Max deliberately not offered, PROVIDERS.md) | Y* | Y* | Y |  |
| API-key providers / breadth | P (9 built-in + OpenAI-compatible custom) | P (Anthropic via 1P, Bedrock, Vertex, Foundry, gateways*) | P | Y* |  |
| Setup wizard / tutor | Y (`--setup`, `--tutor`; `D`/`T` keys in the wizard) | Y | Y | Y |  |
| Model switch, effort, thinking controls | Y | Y* | Y* | Y |  |
| Auto-compaction, /compact undo | Y | Y | Y | Y* |  |
| /context breakdown | Y (share of window, free space, until compaction) | Y* | P | P |  |
| Instruction files (AGENTS.md/CLAUDE.md, nested, @include) | Y (repo-root and nested files load by default) | Y* | Y* | Y* |  |
| Persistent memory | Y | Y* | Y* | ? |  |
| Plan mode with approve-and-execute | Y (as `/mode planwrite`; not in the default Shift+Tab ring, add it with `permission_mode_cycle`) | Y* | P | Y |  |
| Todo list tool | Y | Y* | Y | Y* |  |
| Inline diffs / mutation previews | Y (line-number gutter; intra-line highlighting not yet) | Y | Y* | Y |  |
| Per-hunk accept/reject in terminal | N | N | N | N |  |
| File checkpoints / rewind | P (native edits only; shell changes not captured) | P* | Y* (git checkpoints) | Y* (snapshots) |  |
| Background shell jobs | Y | Y | Y | Y |  |
| OS sandbox for shell | Y (bwrap, Seatbelt, AppContainer) | Y* | Y* | N? |  |
| Web search + fetch | Y | Y | Y* | Y* |  |
| LSP diagnostics/navigation | Y | Y* (plugins) | N? | Y |  |
| Post-edit formatters | Y | P (hooks) | ? | Y* |  |
| Subagents (foreground + background) | Y | Y* | Y* | Y* |  |
| Custom agent definitions (.claude/agents) | Y (project layer; user layer with `claude_user_agents`) | Y* | Y* | Y* |  |
| Git worktree sessions | Y | Y* | Y* | Y* |  |
| Agent teams / cross-session messaging | N | Y* | ? | N? | nvnkx review |
| Cloud / remote-hosted agents | N | Y* | Y* | P | nvnkx review |
| Custom slash commands | Y (inline !command only in the interactive session; headless and ACP fail closed) | Y | Y | Y* | nvnkx.144 |
| Agent Skills | Y | Y* | Y* | Y* |  |
| Hooks | Y (broad event parity incl. PermissionRequest; Claude-shaped PreToolUse decisions) | Y* | Y* | P (via plugins) |  |
| MCP client (stdio/HTTP/legacy SSE, OAuth, resources, prompts, elicitation) | Y (`--mcp-config`, `--strict-mcp-config`) | Y* | Y* | Y* |  |
| Run as MCP server | Y (`--mcp-server`) | Y | Y | ? |  |
| Plugins + marketplace | N (`skills_plugin_dirs` only) | Y* | Y* | Y* (plugins; marketplace ?) | nvnkx review |
| Output styles / personas | P (/prompt, /agent, `--system-prompt`) | Y* | P | Y (agents) |  |
| Configurable status bar | P (up to 3 lines of fixed items, no command segment) | Y* | P | P |  |
| Themes, keybindings, vim mode | Y (vim Normal-mode operators, counts, motions and `.` repeat) | Y* | P* | Y* |  |
| Headless -p, JSON, stream-json, structured output | Y | Y* | Y* (codex exec) | Y |  |
| Budget and turn caps | Y (`--max-budget-usd`, `--max-agent-turns`, typed exit codes 3 and 4) | Y | P | ? |  |
| SDK (TypeScript/Python) | N | Y* | Y* | Y | nvnkx review |
| GitHub Action / CI integration | P (`action.yml` in the repository, used with `uses: sebahrens/mini-agent@<tag>`; not listed in a marketplace) | Y* | Y* | Y | nvnkx.104; [CI.md](CI.md) |
| PR review in CI / PR comments | P (example workflows in CI.md post a comment; `/pr` creates a PR, `/review` is local) | Y* | Y* | P | nvnkx.104 |
| Scheduled tasks | P (/cron in-session only; a CI `schedule` job works with the action) | Y* | Y* | N? | nvnkx review |
| Goal / loop until verified | Y | Y* | P* (long-running work) | N? |  |
| IDE | P (ACP config for Zed, JetBrains, Neovim in EDITORS.md, server side smoke-tested, editor side unconfirmed; VS Code extension unpublished) | Y* | Y* | Y | nvnkx.136; [EDITORS.md](EDITORS.md) |
| Desktop app | N | Y* | Y* | Y* | nvnkx review |
| Web / mobile / remote control | N | Y* | Y* | P* (opencode pair) | nvnkx review |
| Session share link | P (secret GitHub gist of HTML export) | P* (artifacts) | ? | Y |  |
| Session export/import, fork, resume picker | Y | Y | Y* | Y |  |
| Cost view | Y (/cost, ledger, caps, OpenRouter billed cost) | Y* | P | Y |  |
| Plan-limit / quota view | P (`/cost` prints a Rate limits section from response headers) | Y | Y | ? |  |
| Managed (admin) policy | Y | Y* | Y* | Y* |  |
| Proxy / custom CA | Y (including socks5/socks5h) | Y* | Y | Y |  |
| Bedrock / Vertex / Azure | Y / Y / Y (Azure OpenAI; no Claude-on-Foundry) | Y* / Y* / Y* | P* | Y |  |
| OpenTelemetry export | N | Y* | Y | ? | nvnkx review |
| Screen-reader mode | N | Y* | ? | ? | nvnkx review |
| Voice dictation | N | Y* | Y* | N? | nvnkx review |
| UI / docs localisation | N | P* (docs in 11 languages) | ? | ? |  |
| Images, PDF, notebooks | Y (notebook cell images, PDF page images) | Y | P* (images) | P* |  |
| Clipboard image paste | Y (also a pasted or dragged image path) | Y | Y* | Y |  |
| Desktop notifications (OSC 9/777), OSC 133 marks | Y (opt-in; `terminal_notify`) | Y | Y* | ? |  |
| Install channels | P (curl script, MSI with PATH entry, cargo; in-repo templates for Homebrew, npm, winget and scoop are not published) | Y | Y* | Y* | nvnkx.127; [PUBLISHING_RELEASES.md](PUBLISHING_RELEASES.md) |
| Self-update | P (manual `mini-agent update`, opt-in notice; managed installs print their channel's command) | Y (auto) | P* | Y* (notify/auto) |  |
| Signed binaries | N (deferred bead 9bwjr) | Y | Y | ? | 9bwjr |
| Windows ARM64 binary | N | ? | ? | ? | nvnkx review |
| Docs site with search + troubleshooting | Y (sidebar, client-side search, generated config-key reference, [TROUBLESHOOTING.md](TROUBLESHOOTING.md)) | Y* | Y* | Y* | nvnkx.135 |
| Migration guide from other agents | Y ([MIGRATING.md](MIGRATING.md)) | ? | Y* | ? | nvnkx.128 |

Unique to mini-agent: brokered QuickJS code-as-tool, the learned-skill library and Skill Gym,
structured Git operations, an advisor model, and chain-of-prompts.

## Changes since the first snapshot

Rows that moved the same day, because the work landed on main after the review base:

- Custom slash commands and instruction files: inline `` !`command` `` now runs in the interactive
  session; `~/.claude/commands` loads; repository-root `AGENTS.md`/`CLAUDE.md` load by default.
- Hooks: Claude-shaped `hookSpecificOutput` decisions and `$CLAUDE_PROJECT_DIR` work.
- MCP: legacy SSE servers, `--mcp-config` and `--strict-mcp-config`.
- Quota: `/cost` prints rate-limit windows from response headers.
- Docs, migration, editors, CI and Windows `PATH`: this package (nvnkx.128, .135, .136, .104, .301, .127).

## How to refresh this matrix

At each competitive review, rerun the inventory the mini-agent column was built from:

```bash
mini-agent --help
mini-agent --doctor
```

plus the slash-command tables in [COMMANDS.md](COMMANDS.md) and the headings of `docs/agent`. Update the
date at the top, move rows that changed, and keep one bead per N or P row that matters.
