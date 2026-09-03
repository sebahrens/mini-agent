# VS Code ACP Setup Guide

mini-agent speaks the stable Agent Client Protocol (ACP) protocol version 1 over
stdio or TCP. The repository includes a native VS Code Chat Participant extension
in `editors/vscode`; other ACP-compatible clients can also drive the agent.

## Native VS Code extension

The native extension always talks to its bundled (or configured) binary over
**stdio**. It never opens a TCP connection, so no host, port, or API key
configuration exists or is needed.

### Install

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

### Use

Open a trusted local workspace and address `@mini-agent` in VS Code Chat. The
extension spawns `mini-agent --acp` lazily as a child process, keeps one ACP
session for subsequent prompts, streams assistant/tool/status updates, displays
permission requests as modal editor choices, and forwards Chat cancellation to
`session/cancel`. Stopping the extension, closing the session, revoking its
workspace context, or deactivating VS Code reaps the child process.

The extension fails closed in Restricted Mode and virtual workspaces. In a
Remote Development window, install it on the remote/workspace side so the ACP
process and selected `file:` workspace share the same authority.

### Commands

The Command Palette exposes six commands:

| Command | Effect |
|---|---|
| **Mini Agent: Start Session** | Spawn the binary and open an ACP session for the selected workspace folder. |
| **Mini Agent: Stop Session** | Close the session and reap the child process. |
| **Mini Agent: Restart Session** | Stop, then start a fresh session. |
| **Mini Agent: Select Workspace Folder** | Choose which trusted workspace folder the session is bound to. |
| **Mini Agent: Open Config** | Open the agent's global config, creating an inert owner-private `config.toml` when none exists. |
| **Mini Agent: Show Output** | Reveal the Mini Agent output channel without moving keyboard focus. |

Open Config follows the same platform-native global `zerostack` configuration
root and `ZS_CONFIG_DIR` override as the binary and opens an existing
TOML/YAML/JSON config when present.

### Settings

The extension contributes exactly two settings, both machine-scoped:

| Setting | Default | Meaning |
|---|---|---|
| `mini-agent.executablePath` | `""` | Path to the `mini-agent` executable. Leave empty to use the binary bundled in the VSIX. A custom path is accepted only from User/Remote machine settings; workspace settings cannot replace the executable. |
| `mini-agent.logLevel` | `info` | Verbosity of the Mini Agent output channel (`error`, `warn`, `info`, `debug`, `trace`). |

No API key is required: the bundled binary is started over stdio, and the model
provider key comes from the agent's own config or environment
(`--api-key` on the CLI is the provider key, not an ACP credential).

## Generic ACP clients over TCP

Other ACP clients can connect over TCP, which is useful when the agent runs on a
remote host or in a container. TCP always requires authentication.

### 1. Install the agent

```bash
cargo install --path . --debug
```

Make sure `mini-agent` is on the client's `PATH` if the client spawns it, or on
the host that will run the TCP listener.

### 2. Generate an API key

```bash
openssl rand -hex 32
```

### 3. Configure mini-agent

Add to your platform global config (for example
`~/.config/zerostack/config.toml` on Linux) or project-local
`.zerostack/config.toml`. The `type` key selects the transport; the `host`
and `port` must match the listener you start in the next step:

```toml
[acp_servers.default]
type = "tcp"
host = "127.0.0.1"
port = 7890
api_key = "<your-key>"
```

Alternatively, export `MINI_AGENT_ACP_API_KEY=<your-key>` instead of storing
the key in the config file. The environment variable takes precedence.

### 4. Start mini-agent in TCP mode

```bash
mini-agent --acp-port 7890
```

`--acp-host` overrides the bind address (default `127.0.0.1`); binding a
non-loopback address is logged as a warning and still requires the API key.
The default port when only `--acp-host` is given is `7243`.

### 5. Point your client at the agent

Configure the client with host `127.0.0.1`, port `7890`, and the API key you
generated. The listener performs a nonce/HMAC challenge before accepting ACP
traffic, so a client without the key is rejected immediately.

## Capabilities advertised to the client

| Capability | Supported |
|---|---|
| `session/new` | Yes |
| `session/close` | Yes |
| `session/prompt` | Yes |
| `session/cancel` | Yes |
| `session/request_permission` | Yes — prompts appear in the editor UI |
| Max concurrent sessions | 64 |

The machine-readable summary lives in [`docs/acp-registry.json`](acp-registry.json).

## Permission bridge

When a tool needs authorization, mini-agent sends a `session/request_permission` request to the connected client. The client (the native extension or another ACP client) displays the permission dialog; the user's choice (Allow once / Allow always / Deny) is forwarded back to the agent.

If no ACP client is connected, or if the session is non-interactive, tool calls that require a permission prompt are denied automatically.

## Troubleshooting

**Native extension cannot start the agent**: leave `mini-agent.executablePath`
empty to use the bundled binary, or point it at an executable from User/Remote
machine settings. The extension does not read `PATH`.

**TCP connection refused**: confirm the agent is running (`mini-agent --acp-port <port>`) and the port matches.

**TCP authentication failed**: the client must present the same key as
`[acp_servers.<name>].api_key` (or `MINI_AGENT_ACP_API_KEY`) for the bound
host and port.

**Permission prompts not appearing**: ensure the client implements
`session/request_permission` for ACP protocol version 1. Check the client's ACP
support in its documentation.
