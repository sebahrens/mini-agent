---
title: "Editors: Zed, JetBrains, Neovim and VS Code (ACP)"
description: "Copy-paste Agent Client Protocol configuration for Zed, JetBrains IDEs, Neovim and VS Code, with a capability table and known limits."
---

# Editors: Zed, JetBrains, Neovim and VS Code (ACP)

mini-agent is an Agent Client Protocol (ACP) server. Any ACP client can start it as a child process
over stdio with one argument, `--acp`, and talk to it with newline-delimited JSON-RPC. This page gives
a configuration block per editor, what the server offers, and what to check when a client behaves
differently. The protocol surface is described in [vscode-acp-setup.md](../vscode-acp-setup.md) and
summarised in `docs/acp-registry.json` in the repository.

## What was verified

| Layer | Status |
| --- | --- |
| Server side: `mini-agent --acp` answers initialize, session/new and a prompt over stdio | Checked by `scripts/ci/acp_smoke.py`, which launches the binary with exactly the argv shown for each editor below against an offline stub provider, and by the `acp_session_parity` integration tests. The smoke script runs in the `Action self-test` workflow. |
| Client side: each editor's own configuration schema | Written from the editors' documented formats, **not** yet confirmed in a running editor. Treat a block as a starting point; when you confirm one, record the editor version in the "Confirmed with" line of its section. |

Every block uses the same launch contract: executable `mini-agent`, arguments `["--acp"]`, working
directory chosen by the client (the workspace). If the editor cannot find the executable, give it an
absolute path (`which mini-agent`, `Get-Command mini-agent` on Windows).

Credentials come from the usual places (environment variables, `config.toml`, `mini-agent --login`),
so run `mini-agent --setup` or sign in once in a terminal before first use. An editor launched from a
desktop menu may not inherit your shell's environment; put the key in `config.toml` (`api_keys`) or
start the editor from a terminal.

## Zed

Add the agent to `agent_servers` in Zed's `settings.json` (command palette: `zed: open settings`):

```json
{
  "agent_servers": {
    "mini-agent": {
      "command": "mini-agent",
      "args": ["--acp"],
      "env": {}
    }
  }
}
```

Open the agent panel, choose **New Thread**, and pick `mini-agent`. Newer Zed releases may want an
explicit `"type": "custom"` in the entry; add it if Zed reports the entry as invalid.

Confirmed with: not yet recorded.

## JetBrains IDEs (IntelliJ IDEA, PyCharm, WebStorm, ...)

The AI Assistant plugin reads custom ACP agents from an `acp.json` file (open it from
**Settings | Tools | AI Assistant | Agents | Add custom agent**):

```json
{
  "agent_servers": {
    "mini-agent": {
      "command": "mini-agent",
      "args": ["--acp"],
      "env": {}
    }
  }
}
```

Pick the agent in the AI Assistant chat. On Windows use the full path of `mini-agent.exe`, because
an IDE started from the Start menu may not see a freshly changed `PATH`.

Confirmed with: not yet recorded.

## Neovim

Neovim has no built-in ACP client; use a plugin. The repository ships a ready-made CodeCompanion.nvim
adapter: run `bash scripts/setup-neovim.sh`, or paste its adapter into your plugin spec. The part that
matters to mini-agent is the `commands` table:

```lua
adapters = {
  acp = {
    ["mini-agent"] = function()
      return {
        name = "mini-agent",
        formatted_name = "mini-agent",
        type = "acp",
        commands = { default = { "mini-agent", "--acp" } },
        -- roles, parameters and handlers as in scripts/setup-neovim.sh
      }
    end,
  },
},
interactions = { chat = { adapter = "mini-agent" } },
```

Other Neovim ACP plugins take the same command and arguments in their provider tables.

Confirmed with: not yet recorded.

## VS Code

Use the native extension (see [vscode-acp-setup.md](../vscode-acp-setup.md)); it launches `mini-agent --acp`
itself. It is distributed as a VSIX attached to each release, not through the Marketplace.

## What each client gets

mini-agent advertises these capabilities. Whether the editor shows them is up to the editor.

| Feature | Server behaviour | Needs from the client |
| --- | --- | --- |
| Sessions | `session/new`, `session/prompt`, `session/cancel`, `session/close`; up to 64 concurrent (`acp_max_sessions`) | Nothing special |
| Resume | `session/load` and paged `session/list` replay the saved transcript | A session picker that calls `session/list`/`session/load` |
| Edit diffs | Tool calls carry file locations and edit diffs, live and on replay | Diff rendering for tool-call content |
| Modes | `session/set_mode` switches the permission mode (modes a managed policy forbids are hidden) | A mode selector |
| Model, effort, persona | `session/set_config_option` for `model`, `effort` and persona | Config-option UI |
| Slash commands | `/compact`, `/undo`, `/rename`, `/cost`, `/model`, `/effort`, `/cron`, `/tasks` (and `/goal` with the goal feature), announced through `available_commands_update` | A slash-command picker, or type the command as the whole prompt |
| Images | PNG, JPEG, GIF and WebP content blocks; images over 2 MiB are re-encoded | The client must send image blocks |
| Permission prompts | `session/request_permission` with allow once, allow for this session and deny | A permission dialog. Without a connected client, calls that need approval are denied. |
| Editor buffers | With `acp_client_file_access = true`, the file tools use the client's `fs/read_text_file` and `fs/write_text_file` | Client advertises `fs` capabilities |
| MCP elicitation forms | Cannot be answered over ACP; the request is cancelled and an agent thought says so | None |

## Known limits

- A session that holds provider-native conversation state (Anthropic with session persistence, after
  its first turn) refuses `/compact`, `/undo`, `/rename` and `/goal <objective>`; start a new session.
- Inline `` !`command` `` in custom command files fails closed over ACP.
- Project hooks and sensitive project config need the same approvals as a headless run. Approve them
  once in a terminal (`mini-agent hooks trust`, `mini-agent trust`) before relying on them in an editor.
- A client that reads TCP instead of spawning a process needs the key and TLS setup in
  [vscode-acp-setup.md](../vscode-acp-setup.md#generic-acp-clients-over-tcp).

## Listing mini-agent in an ACP agent registry

`docs/acp-registry.json` is a machine-readable capability summary kept in this repository. Submitting
mini-agent to a public ACP agent registry is a manual release step, because it publishes to a
third-party service and each registry defines its own entry format. The release checklist in
[PUBLISHING_RELEASES.md](PUBLISHING_RELEASES.md#acp-agent-registry-submission-manual) lists what to
prepare and verify.
