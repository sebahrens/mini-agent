# VS Code ACP Setup Guide

mini-agent speaks stable Agent Client Protocol (ACP) v1 over stdio or TCP. The
repository includes a native VS Code Chat Participant extension in
`editors/vscode`; other ACP-compatible clients can also drive the agent.

## Native Mini Agent extension

Install the platform-specific `mini-agent-<version>-<target>.vsix` from a GitHub
release. VS Code's **Extensions: Install from VSIX...** command installs the
candidate without a Marketplace connection. Each release includes SHA-256
checksums, a CycloneDX SBOM, the GPL notice, and corresponding-source directions.
Release packaging rejects wrong-architecture or ACP-disabled binaries, then drives
each native artifact through initialize/new/prompt/cancel/close over stdio before
assembling its platform VSIX.

On x86-64 Windows, `mini-agent-windows-x64.msi` provides a no-terminal alternative. It installs
per-user without elevation by default and attempts to side-load the bundled win32-x64 VSIX when
VS Code is detected. Enterprise deployment can use
`msiexec /i mini-agent-windows-x64.msi ALLUSERS=1 /quiet /norestart`; a service-account install
does not modify another user's VS Code profile. Verify the artifact against `MSI_SHA256SUMS`.

Open a trusted local workspace and address `@mini-agent` in VS Code Chat. The
extension starts its bundled native binary lazily, keeps one ACP session for
subsequent prompts, streams assistant/tool/status updates, displays permission
requests as modal editor choices, and forwards Chat cancellation to
`session/cancel`. Stopping the extension, closing the session, revoking its
workspace context, or deactivating VS Code reaps the child process.

The extension fails closed in Restricted Mode and virtual workspaces. In a
Remote Development window, install it on the remote/workspace side so the ACP
process and selected `file:` workspace share the same authority.

The Command Palette exposes **Mini Agent: Open Config**, **Mini Agent: Restart
Session**, and **Mini Agent: Show Output**. Open Config follows the same
platform-native global `zerostack` configuration root and `ZS_CONFIG_DIR`
override as the binary, opens an existing TOML/YAML/JSON config when present,
and otherwise creates an inert owner-private `config.toml`. Show Output reveals
the existing Mini Agent output channel without moving keyboard focus.

## Prerequisites

- mini-agent installed: `cargo install --path . --debug`
- The native Mini Agent extension or another stable ACP v1 client

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

Add to your platform global config (for example
`~/.config/zerostack/config.toml` on Linux) or project-local
`.zerostack/config.toml`:

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
| `session/cancel` | Yes |
| `session/request_permission` | Yes — prompts appear in the editor UI |
| Max concurrent sessions | 64 |

## Permission bridge

When a tool needs authorization, mini-agent sends a `session/request_permission` request to the connected client. The client (your VS Code extension) displays the permission dialog; the user's choice (Allow once / Allow always / Deny) is forwarded back to the agent.

If no ACP client is connected, or if the session is non-interactive, tool calls that require a permission prompt are denied automatically.

## Troubleshooting

**Agent not found**: make sure `mini-agent` is on your PATH (`which mini-agent`).

For the native extension, leave `mini-agent.executablePath` empty to use the
bundled binary. A custom path is accepted only from User/Remote machine settings;
workspace settings cannot replace the executable.

**TCP connection refused**: confirm the agent is running (`mini-agent --acp-port <port>`) and the port matches.

**Permission prompts not appearing**: ensure the extension supports `session/request_permission` (ACP schema ≥ 1.5.0). Check the extension's ACP version in its documentation.
