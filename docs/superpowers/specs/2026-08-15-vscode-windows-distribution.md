# VS Code Extension + Windows MSI Distribution

- **Status**: design approved, pre-implementation
- **Beads epic**: `mini-agent-dxkw`
- **Date**: 2026-08-15

## Goal

Deliver mini-agent to non-technical Windows users with no command-line steps required. A single
MSI installer or a VS Code marketplace click is the entire install experience. The Rust binary
build is unchanged; this is a purely additive, optional packaging layer.

## Constraints

- The standalone binary must remain buildable for all platforms without any extension toolchain.
- The extension packages are built on top of pre-compiled binary artifacts; they never trigger a
  Rust build.
- All backend and API key configuration is delegated to mini-agent's existing config system. The
  extension does not store or manage API credentials.

## Repository layout

```
mini-agent/
├── src/                          # Rust — unchanged
├── packaging/
│   ├── homebrew/                 # existing
│   ├── aur/                      # existing
│   ├── conda/                    # existing
│   └── windows/                  # NEW — WiX v4 MSI sources
│       ├── mini-agent.wxs        # component definitions
│       ├── installer.wixproj     # WiX project file
│       └── assets/               # license RTF, banner/dialog BMP
├── vscode-extension/             # NEW — TypeScript VS Code extension
│   ├── package.json
│   ├── tsconfig.json
│   ├── .vscodeignore
│   ├── config.template.toml      # bundled default config template
│   ├── src/
│   │   ├── extension.ts          # entry point — activate/deactivate
│   │   ├── binary.ts             # binary resolution
│   │   ├── client.ts             # ACP stdio client
│   │   ├── panel.ts              # chat webview sidebar panel
│   │   └── commands.ts           # Open Config, Restart Agent, Show Output
│   ├── media/
│   │   ├── panel.html
│   │   ├── panel.css
│   │   └── panel.js
│   └── bin/                      # gitignored — populated by CI at package time
│       ├── win32-x64/
│       ├── win32-arm64/
│       ├── darwin-x64/
│       ├── darwin-arm64/
│       ├── linux-x64/
│       └── linux-arm64/
└── .github/workflows/
    ├── rust.yml                  # existing — unchanged
    └── vscode-publish.yml        # NEW — build + publish extension + MSI
```

`bin/` is listed in `.gitignore` and `.vscodeignore` for all subdirectories except the target
being packaged in each `vsce package --target` invocation.

## VS Code extension

### Activation

Activation event: `"onStartupFinished"`. The extension activates once after VS Code is ready,
registers commands and the sidebar panel, then spawns the agent process.

### Binary resolution (`binary.ts`)

1. Check `PATH` for an existing `mini-agent` (or `mini-agent.exe`) install. If found and
   executable, use it — an existing developer install wins.
2. Fall back to `<extensionPath>/bin/<platform>/mini-agent[.exe]` where `<platform>` is derived
   from `process.platform` + `process.arch` (e.g. `win32-x64`, `darwin-arm64`).
3. If neither path resolves, show an error notification and do not spawn.

### ACP stdio client (`client.ts`)

Spawns `mini-agent --acp` as a `child_process` with `stdio: 'pipe'`. Implements the ACP framing
protocol over `stdin`/`stdout`. Exposes a typed async interface to the rest of the extension.
Handles:
- Process exit / crash → set status bar to error state, offer restart
- Spawn failure → surface error notification with path that was tried
- Cancellation / deactivate → kill child process cleanly

### Chat webview sidebar panel (`panel.ts` + `media/`)

A VS Code `WebviewView` registered in the `"explorer"` sidebar container. The webview HTML uses
only VS Code theme CSS variables (`--vscode-*`) — no external stylesheets, no bundler required.
Messages flow:
```
user input (webview) → postMessage → panel.ts → client.ts → ACP frame → mini-agent
mini-agent → ACP frame → client.ts → panel.ts → postMessage → message history (webview)
```

### Commands (`commands.ts`)

| Command ID | Palette label | Action |
|---|---|---|
| `mini-agent.openConfig` | `mini-agent: Open Config` | Resolve `~/.config/mini-agent/config.toml`; if absent, copy `config.template.toml` then open in editor |
| `mini-agent.restart` | `mini-agent: Restart Agent` | Kill ACP child process, respawn |
| `mini-agent.showOutput` | `mini-agent: Show Output` | Reveal the extension's `OutputChannel` |

### Status bar item

Shows connection state: `$(check) mini-agent` (connected), `$(warning) mini-agent` (error).
Clicking opens the output channel.

### `package.json` targets

Published as separate per-platform `.vsix` files with the `--target` flag. Declared targets:
`win32-x64`, `win32-arm64`, `darwin-x64`, `darwin-arm64`, `linux-x64`, `linux-arm64`.

The `"files"` field in `package.json` uses `vsce`'s `${VSCE_TARGET}` substitution so each
packaged extension includes only its own platform's binary:
```json
"files": ["bin/${VSCE_TARGET}/**", "dist/**", "media/**", "config.template.toml"]
```

## Binary bundling strategy

The `vscode-publish.yml` workflow has a `needs` dependency on the matrix build job in `rust.yml`.
Once all platform binaries are uploaded as GitHub Actions artifacts, the publish workflow:

1. Downloads each platform artifact using `actions/download-artifact`
2. Places binaries into `vscode-extension/bin/<platform>/mini-agent[.exe]`
3. Sets executable bit on non-Windows binaries
4. Runs `npm run compile` to build TypeScript
5. For each target: `vsce package --target <target> --out mini-agent-<target>.vsix`
6. Runs `vsce publish` with all `.vsix` files using the `VSCE_PAT` secret
7. Uploads all `.vsix` files as release assets (enables VSIX side-loading for corporate installs)

The `bin/` directory is never committed. The publish workflow is the only place that populates it.

## Windows MSI installer

**Toolchain**: WiX Toolset v4 via the `wixtoolset/setup-wix@v4` GitHub Actions action.

**Installer behaviour**:
1. Installs `mini-agent.exe` to `%LOCALAPPDATA%\Programs\mini-agent\` — no elevation required.
2. Adds that directory to the current user's `PATH` via a registry `Environment` component.
3. Detects VS Code: checks for `code.cmd` on `PATH` and the VS Code install registry key.
4. If VS Code is found, extracts the bundled `mini-agent-win32-x64.vsix` (embedded as a binary
   resource) and runs `code --install-extension <vsix-path> --force` silently.
5. Creates an uninstaller entry in `Add/Remove Programs`.

**Silent install** (GPO / Intune / SCCM):
```
msiexec /i mini-agent-x64.msi /quiet /norestart
```

**MSI build** runs in the same `vscode-publish.yml` workflow after the `.vsix` files are produced,
so the MSI can embed the already-built `win32-x64.vsix`.

## CI/CD pipeline summary

```
release tag pushed
  │
  ├─► rust.yml matrix (existing)
  │     build win32-x64, darwin-arm64, linux-x64, ...
  │     upload binaries as artifacts
  │
  └─► vscode-publish.yml (new, needs: rust matrix)
        download all binary artifacts
        populate vscode-extension/bin/
        compile TypeScript
        vsce package (per target)
        vsce publish (all targets)
        wix build → mini-agent-x64.msi
        upload .vsix + .msi as release assets
```

## Publisher setup (one-time)

1. Create a free Azure DevOps organization (any Microsoft account).
2. Generate a PAT scoped to `Marketplace (Manage)` — store as `VSCE_PAT` in GitHub secrets.
3. Register publisher ID at `marketplace.visualstudio.com/manage`.
4. Verify a domain for the blue "Verified Publisher" badge (DNS TXT record).
5. Register the same extension on `open-vsx.org` for VS Code forks used in corporate environments.

## Out of scope for this spec

- Auto-update of the mini-agent binary independent of VS Code extension updates (VS Code's built-in
  update cycle is sufficient — each extension update re-bundles the latest binary).
- macOS `.pkg` or Linux `.deb`/`.rpm` installers (covered by existing homebrew/AUR/conda
  packaging).
- API key management — fully delegated to mini-agent's existing config system.
- Building a custom ACP protocol implementation — the extension uses mini-agent's existing
  `--acp` mode over stdio.
