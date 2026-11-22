---
title: "Troubleshooting and error reference"
description: "Exact error and warning messages mini-agent prints at startup and during a run, what causes each, and how to fix it."
---

# Troubleshooting and error reference

Each heading below is the message mini-agent prints, so searching the site (or your terminal
scrollback) for the text lands on its entry. Start with the diagnostic report:

```bash
mini-agent --doctor          # one-shot, with bounded live checks (sandbox, git, JS worker)
mini-agent --print-config    # resolved configuration, layers, trust and managed policy
mini-agent --version         # build identity only
```

`--doctor` needs no API key, makes no provider call and exits `1` only when a config layer cannot
be parsed. Inside the TUI, `/doctor` prints the cached form of the same report. Review local
paths before pasting a report into an issue. Details of the report are in
[GET_STARTED.md](GET_STARTED.md).

## Install and first run

### mini-agent: command not found / 'mini-agent' is not recognized

The binary is not on `PATH`.

- Linux and macOS: the installer writes to `~/.local/bin` by default. Add it to `PATH`, or reinstall
  with `--dir`.
- Windows MSI: current MSIs add the install directory to the user `PATH` (or the system `PATH`
  for an `ALLUSERS=1` install). Open a new PowerShell window after installing; an already running
  shell keeps its old `PATH`. An MSI installed before that change needs a reinstall.
- Check with `Get-Command mini-agent` (PowerShell) or `command -v mini-agent` (POSIX shells).

### "Windows protected your PC" (SmartScreen)

Release binaries are not code-signed yet, so Windows shows this prompt for a browser-downloaded MSI.
Verify the file against `MSI_SHA256SUMS` first (exact PowerShell commands are in
[WINDOWS_INSTALL.md](WINDOWS_INSTALL.md)), then choose **More info**, then **Run anyway**. Managed
fleets that enforce application control need file-hash rules because there is no signer to trust.

### macOS: "mini-agent cannot be opened" / "cannot be verified" (Gatekeeper)

A `.tar.gz` downloaded with a browser carries the quarantine attribute. After extracting it, run
`xattr -d com.apple.quarantine ./mini-agent`, or Control-click the binary in Finder and choose
**Open** once. The shell installer's `curl` download is not quarantined. Verify the checksum and run
`gh attestation verify <file> --repo sebahrens/mini-agent` before bypassing the prompt.

### Error: SHA-256 mismatch / does not match SHA256SUMS

The installer refuses an archive whose digest differs from the release's `SHA256SUMS`. Retry the
download. If it repeats behind a proxy, the proxy may be rewriting content; download the archive in a
browser and verify it by hand. Never bypass this check.

### gh attestation verify failed

The installer aborts when the signed provenance does not match the archive. Update `gh`, check that
`--repo sebahrens/mini-agent` is the repository you intended, and retry. `MINI_AGENT_SKIP_ATTESTATION=1`
skips only this check (the checksum comparison always runs); use it only when you have verified the
asset another way.

## Credentials and sign-in

### No API key found. Set the ... environment variable, add it to config.api_keys under ..., pass --api-key, or run `mini-agent --setup` to configure interactively.

The selected provider has no credential. Export the variable named in the message (for example
`OPENROUTER_API_KEY`), add it under `api_keys` in `config.toml`, pass `--api-key`, or run
`mini-agent --setup`. If another provider's key is set, the message adds a hint to pass
`--provider <name>`. A custom provider on a loopback `base_url` (LM Studio, llama.cpp, vLLM) needs no
key. See [PROVIDERS.md](PROVIDERS.md).

### auth: ChatGPT sign-in required

Run `mini-agent --login openai` (or `/login openai` in the TUI). On a remote machine, forward a fixed
port with `MINI_AGENT_LOGIN_PORT` (1024 to 65535) or paste the final `http://127.0.0.1:PORT/auth/callback?...`
address on stdin. The sign-in waits 10 minutes. For another account use `--login-account new`.

### ChatGPT sign-in service temporarily unavailable

The provider's sign-in endpoint returned an error status. Wait and retry; nothing was stored.

### missing; run /login copilot or configure this route's credentials

`--doctor` line for a route without credentials. Run `/login copilot` or `mini-agent --login copilot`
for GitHub Copilot, or configure the API key of the route.

### A route reports a missing key although another provider's key is set

An Anthropic or Azure route never borrows `OPENAI_API_KEY` or another provider's key. Set the
variable that route names (see [PROVIDERS.md](PROVIDERS.md)).

## Sandbox

### warning: sandbox backend '...' is unavailable — shell commands run UNSANDBOXED with your full user privileges.

The default shell sandbox could not start, so the session fell back to running commands without
containment and withholds the built-in auto-allows for build and test commands. The message names the
cause and the fix. Common causes:

- **Linux, no bubblewrap.** Install it as a root-owned system executable: `sudo apt install bubblewrap`
  (Debian, Ubuntu), `sudo dnf install bubblewrap` or `sudo pacman -S bubblewrap`. A `bwrap` in a
  user-writable directory is ignored. The general-process sandbox needs bubblewrap 0.8.0 or later
  (`--bind-fd` and `--ro-bind-fd`).
- **Linux, AppArmor user-namespace restriction (Ubuntu 23.10 and later).** See the next entry.
- **macOS.** The Seatbelt backend needs `/usr/bin/sandbox-exec`. The cause line says it is missing
  or not a trusted system executable, that a profile could not be applied (mini-agent is probably
  already inside another Seatbelt sandbox), or that the preflight failed or timed out.

Pass `--sandbox` (or set `sandbox = true`) to refuse to start instead of falling back, or
`--no-sandbox` to run unsandboxed deliberately.

### sandbox backend '...' was not found — refusing to start with unsandboxed execution (use --no-sandbox to disable sandboxing explicitly)

You asked for `--sandbox` (or `sandbox = true`) and the backend is missing. Install the backend as
above, or drop the flag.

### setting up uid map: Permission denied

`bwrap --unshare-user --ro-bind / / true` fails on Ubuntu 23.10 and later because
`kernel.apparmor_restrict_unprivileged_userns=1`. The recommended fix is a per-binary AppArmor profile
for `/usr/bin/bwrap` that grants `userns`; the exact profile and the alternatives (a sysctl override)
are in [GET_STARTED.md](GET_STARTED.md) under "Linux prerequisites". Verify with the same `bwrap`
command, then run `mini-agent --doctor`.

### JavaScript runtime unavailable

A `[!] JavaScript runtime unavailable: <reason>` startup notice means the brokered `js` tool did not
start; the reason names the missing containment (bubblewrap on Linux, a validated host on macOS, the
LPAC gate on Windows). The rest of the agent works. Run `mini-agent --js-runtime-check` to see the
same check on its own.

On Windows the LPAC gate accepts only supported install locations: under `%USERPROFILE%` or
`%LOCALAPPDATA%` (the per-user MSI), `%CARGO_HOME%\bin` (default `%USERPROFILE%\.cargo\bin`), a cargo
`target\debug` or `target\release` directory, `%TEMP%`/`%TMP%`, and Program Files, ProgramData or
the Windows directory. An executable anywhere else (for example `cargo install --root` on another
drive) reports `<path> is not a supported install location for the JavaScript tool`; copy
`mini-agent.exe` to a supported location. See [WINDOWS_INSTALL.md](WINDOWS_INSTALL.md).

### Git tool unavailable on Windows

The structured `git` tool needs the AppContainer sandbox: with `--no-sandbox` it is not offered
(`Windows structured Git requires AppContainer`; run git through the shell tool instead).
`mini-agent --git-runtime-check` runs the bundled Git inside the container
(`git rev-parse --git-dir`, which succeeds or reports "not a git repository" when the executable, its
DLLs and the working directory all work). The runtime directory
(`<local data>\git-runtime\<id>`) holds `git.exe` and its DLL closure; every file is granted to the
container as a read/execute root, one grant per file (the files have protected private ACLs, so a
directory grant would not be inherited). Probe failures:

- Exit code `-1073741515` (0xC0000135) or `-1073741790` (0xC0000022): the container could not read a
  DLL beside `git.exe`.
- `cannot resolve its working directory inside the Windows AppContainer`: the bundled Git predates the
  runtime patch that falls back to the plain current directory when `GetFinalPathNameByHandleW` is
  denied (an AppContainer cannot resolve DOS volume names). The structured tool is withheld because
  every operation would fail; use git through the shell tool and rebuild the runtime from the current
  `scripts/git-runtime/policy.patch`.

The bundled Git needs no ACL change on the NUL device or on the drive root: where the container cannot
open `/dev/null` by name, the patched runtime keeps the launcher's standard handles (startup) and
substitutes a private delete-on-close temporary file for sub-command redirections.

### Windows shell: PowerShell starts in `C:\`, `dir` or `git` say "Access is denied" / "Unable to read current working directory", `python` or `node` is not recognized, `/dev/null` cannot be opened

The Windows AppContainer shell reads only the workspace, the cache and what `ALL APPLICATION PACKAGES` already reads. Three opt-in global-config keys (all default `false`) widen it; the `[sandbox: ...]` note on the failing command names the one that matches:

- `windows-appcontainer-ancestor-listing = true`: lets the container list the entries (names, sizes, timestamps) inside each parent of the workspace, which PowerShell, `cmd dir` and Git need to resolve the working directory. System-owned parents (`C:\Users`, the drive root) need mini-agent started from an elevated launcher.
- `windows-appcontainer-null-device = true`: with an elevated launcher, lets the container open the NUL device (Git, `>nul`, Python/Node `DEVNULL`). Otherwise use the `git` tool.
- `windows-appcontainer-toolchain-discovery = true`: read and execute on the install directory of `python`, `node`, ... when the command names them. An administrator-owned install needs an elevated launcher; or add the directory to `windows-appcontainer-read-roots`.

`mini-agent --doctor` shows the effective state of the three keys, and a warning when an earlier elevated session left grants that an unelevated launch could not remove (start mini-agent elevated once to clear them). Settings are global-only and a managed policy can force each off. See `docs/agent/CONFIG.md`.

### [sandbox network: blocked host:port (reason)]

A contained command reached a host that `sandbox-network = "allowlist"` does not permit. Add the host
to `sandbox-network-hosts`, or switch policy for the session with `/sandbox network allow`. Recent
refusals are listed by `/sandbox`.

## Configuration and trust

### warning: N project hook(s) did not run (...): not trusted. Run `mini-agent hooks trust` once, or pass --trust-project-hooks for this run

Project hooks need an exact approval. Review them with `mini-agent hooks trust --list`, approve with
`mini-agent hooks trust --yes`, or pass `--trust-project-hooks` in a CI run you control.

### project config: ignored untrusted sensitive settings in headless mode

A project `.mini-agent/config.toml` can set only benign keys until you approve it; the log line above is the headless form. Run
`mini-agent trust --list` and `mini-agent trust`, or pass `--trust-project-config` for one run. The
ignored keys are named in the startup notice and by `--print-config`.

### private paths reject symlinked path components

mini-agent refuses to keep private state (config, data, sessions) under a path that contains a
symlink, because a link could redirect secrets. This is common on macOS where `/var` and `/tmp` are
links to `/private/...`. Point `MINI_AGENT_CONFIG_DIR` and `MINI_AGENT_DATA_DIR` at the resolved path
(`cd dir && pwd -P`). Also check that a symlinked launch path is not the cause; `mini-agent --doctor`
warns about a symlinked launch.

### invalid `default_permission_mode` value

The value must be one of `standard`, `restrictive`, `readonly`, `planwrite`, `guarded`, `yolo`
(`accept` is an alias for `standard`).

### unknown command: ... (try /help)

The slash command does not exist. mini-agent suggests the closest built-in or custom command name.
Custom commands from a project need project trust before they appear.

## Network, proxy and certificates

### invalid peer certificate / certificate verify failed

A corporate proxy is re-signing TLS and its root CA is not trusted. Point the platform TLS backend at
the bundle with `SSL_CERT_FILE` or `SSL_CERT_DIR` (where the backend supports it) and keep
`danger_accept_invalid_certs` off. The proxy variables `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and
`NO_PROXY` are honoured, including `socks5://` and `socks5h://` for `web_fetch`, `web_search` and JS
`fetch()`; a socks4 or unparsable value fails closed.

### Provider errors: overloaded, rate limited, 5xx

Transient provider errors retry with backoff (`retry` in [CONFIG.md](CONFIG.md): 6 attempts by
default). In headless runs `--fallback-model` names a same-provider model for a turn whose primary
fails. A stream that stalls ends after `stream_idle_timeout_secs` (300 by default).

## Headless runs

### Exit codes

| Exit | Meaning |
| ---- | ------- |
| `0` | Success (a met goal is also `0`). |
| `1` | Runtime failure: provider, tool or schema failure. |
| `2` | Usage error: bad or conflicting flags, empty prompt, unknown session. Retrying cannot succeed. |
| `3` | `--max-agent-turns` was exhausted (`stop_reason: "max_turns"`). |
| `4` | The spend budget was exhausted (`stop_reason: "budget"`). |
| `22` | `-p` is waiting for an answer to a question; send a new user message. |
| `130` | Interrupted. |

`-p --output json` always ends with one result record, even when startup fails: `is_error: true`,
`stop_reason: "failed"`, and the failure text in `result`.

### cost cap requires known model pricing; add model_prices for this model or switch

`--max-budget-usd` (or `max_budget_usd`) cannot bound spend on a model with no known price, so the run
stops with exit code `4`. Add `model_prices` to the custom provider (see [CONFIG.md](CONFIG.md)),
switch to a model the catalog prices, or cap the run with `--max-agent-turns` instead.

### a permission was refused in -p mode

Without a terminal there is nobody to approve, so an operation that would ask is refused and listed in
`permission_denials`. Choose a mode that allows it (`--permission-mode yolo`), add `--allow TOOL:PATTERN`
rules for exactly what the job needs, or use `--read-only` for review jobs.

### Reading piped stdin (limit 1048576 bytes)

`-p` reads redirected stdin through EOF (up to 1 MiB) and appends it as untrusted reference text. In CI,
redirect from `/dev/null` when you pass the prompt as an argument and have nothing to pipe, or the run
waits for the stream to close (`--stdin-idle-secs`).

## Crashes

### crash report written to ...

mini-agent writes a bounded crash report under `<data dir>/logs/crashes/`. It contains the panic
location and version, not your conversation. Attach it to a bug report with the output of
`mini-agent --doctor`.

## VS Code and other editors

See [vscode-acp-setup.md](../vscode-acp-setup.md) for the VS Code troubleshooting section and
[EDITORS.md](EDITORS.md) for Zed, JetBrains and Neovim.
