# VS Code ACP Setup Guide

mini-agent speaks the [Agent-Client Protocol (ACP)](https://github.com/anthropics/agent-client-protocol) v1.3.0 over stdio or TCP. Any ACP-compatible VS Code extension can drive it.

## Prerequisites

- mini-agent installed: `cargo install --path . --debug`
- An ACP-compatible VS Code extension (e.g. Claude Dev, Cline, or any extension that supports ACP)

## Stdio transport (recommended)

Stdio is zero-configuration. The extension spawns mini-agent as a child process and communicates over stdin/stdout.

In your extension's settings, set the agent command to:

```
mini-agent --acp
```

No API key or port configuration is required for stdio.

## TCP transport

TCP is useful when the agent runs on a remote host or in a container.

### 1. Generate an API key

```bash
openssl rand -hex 32
```

### 2. Configure mini-agent

Add to your `~/.config/mini-agent/config.toml` (or project-local `.mini-agent/config.toml`):

```toml
[acp_servers.default]
transport = "tcp"
host = "127.0.0.1"
port = 7890
api_key = "<your-key>"
```

### 3. Start mini-agent in TCP mode

```bash
mini-agent --acp-port 7890
```

### 4. Point the VS Code extension to the agent

In your extension's settings:

- **Host**: `127.0.0.1`
- **Port**: `7890`
- **API key**: the key you generated above

## Capabilities advertised to the client

| Capability | Supported |
|---|---|
| `session/new` | Yes |
| `session/close` | Yes |
| `session/prompt` | Yes |
| `session/request_permission` | Yes — prompts appear in the editor UI |
| Max concurrent sessions | 64 |

## Permission bridge

When a tool needs authorization, mini-agent sends a `session/request_permission` request to the connected client. The client (your VS Code extension) displays the permission dialog; the user's choice (Allow once / Allow always / Deny) is forwarded back to the agent.

If no ACP client is connected, or if the session is non-interactive, tool calls that require a permission prompt are denied automatically.

## Troubleshooting

**Agent not found**: make sure `mini-agent` is on your PATH (`which mini-agent`).

**TCP connection refused**: confirm the agent is running (`mini-agent --acp-port <port>`) and the port matches.

**Permission prompts not appearing**: ensure the extension supports `session/request_permission` (ACP schema ≥ 1.5.0). Check the extension's ACP version in its documentation.
