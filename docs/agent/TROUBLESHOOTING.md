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
the Windows directory. An executable anywhere else (for example a `cargo install` with a custom install root on another
drive) reports `<path> is not a supported install location for the JavaScript tool`; copy
`mini-agent.exe` to a supported location. See [WINDOWS_INSTALL.md](WINDOWS_INSTALL.md).

### Git tool unavailable on Windows

The structured `git` tool needs the AppContainer sandbox: with `--no-sandbox` it is not offered
(`Windows structured Git requires AppContainer`; run git through the shell tool instead).
`mini-agent --git-runtime-check` runs the bundled Git inside the container
(a `git rev-parse` that resolves the git directory, which succeeds or reports "not a git repository" when the executable, its
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

### Windows shell in the default sandbox: what works and what needs a key

The Windows AppContainer shell reads only the workspace, the cache and what `ALL APPLICATION PACKAGES` already reads. With the three opt-in keys below at their default (`false`) it is still usable:

- **PowerShell 7 (`pwsh.exe`, the default)** starts in the workspace as the PowerShell drive `ws:\` (a drive rooted at the workspace; `Get-Location` prints `ws:\`). PowerShell normalises provider paths by listing every parent directory, which the container may not do, so without the drive `Set-Location` and relative paths fail and the shell falls back to `C:\`. Relative paths, `Set-Location`, `Get-ChildItem`, `Get-Content`, `Set-Content`, redirections, `Remove-Item`, `Move-Item`, `Copy-Item`, `Rename-Item` and the `workdir` argument work; native programs receive the real directory. Use the `.ProviderPath` of a location (or `[IO.File]` methods) when a program needs a plain path. `Remove-Item`, `Move-Item`, `Copy-Item` and `Rename-Item` are wrapped by proxy functions (generated from the cmdlets' own metadata, so parameters, wildcards and `-Recurse` behave as usual) that first turn a relative or `ws:\` `-Path`, `-LiteralPath` or `-Destination` into a full provider path, because the stock cmdlets resolve a relative path against `C:\` here. A path piped in as a bare string is not rewritten; pipe the item (`Get-Item name | Remove-Item`) or give a full path.
- **`git` in PowerShell and in cmd.exe** runs the bundled, guarded Git (its directory is first on `PATH`, `git` is bound by alias in PowerShell, and for cmd.exe a one-line `git.cmd` shim in the read-only cache directory comes first on `PATH`, because cmd.exe finds programs by listing each `PATH` directory and cannot list the runtime directory): the host Git cannot open `/dev/null` in the container. `git ... > $null` and `| Out-Null` work. Local-path remotes work for fetch, ls-remote, clone and push when the remote directory is inside the workspace or a configured root: the patched Git starts `git upload-pack` and `git receive-pack` directly instead of through `sh`, and a local clone copies objects instead of hard-linking them (a hard-linked file would make the launcher refuse the workspace). A remote outside the workspace is refused with "does not appear to be a git repository ... Could not read from remote repository" and a `[sandbox: ...]` note naming `windows-appcontainer-read-roots` (fetch, ls-remote) and `windows-appcontainer-write-roots` (push); nothing is granted automatically. The `git` tool's own sub-commands (`stash push`, `cherry_pick`) resolve the bundled Git the same way.
- **`cmd.exe`** (`--shell cmd.exe`) is used only when `pwsh.exe` is not on `PATH`: `cd`, `type`, `copy`, `ren`, `move`, `mkdir`, `rmdir`, `git` and `for %f in (*) do` work; `dir` (every form), `del` and `>NUL` print "Access is denied". This is a platform limit of the default sandbox: they are cmd.exe built-ins that list the parents of the working directory or open the NUL device, and no `PATH` shim, prelude or `cmd /c` startup text can replace a built-in; `windows-appcontainer-ancestor-listing` and `windows-appcontainer-null-device` (the latter needs an elevated launcher) together lift all three (checked on a hosted runner: with both on, `dir /b`, `del` and `>NUL` work). Use `for %f in (*) do @echo %f` for listings, or PowerShell 7.
- **Windows PowerShell 5.1 (`--shell powershell.exe`) and Git Bash (`--shell bash`)** do not run inside the AppContainer (5.1 cannot load its core cmdlets; the MSYS runtime cannot initialise). The shell tool is then disabled with a startup warning that names the alternative; the default shell skips `powershell.exe` while the sandbox is on. Use PowerShell 7, `cmd.exe`, or `--no-sandbox`.
- **`--sandbox-profile host-read` and `host-full`** do not exist on Windows: startup fails with a message naming `windows-appcontainer-read-roots`, `windows-appcontainer-write-roots` and `--no-sandbox`.
- **Python, Node and other tools installed outside Program Files** cannot be started, by decision (no automatic grant): `py` runs but the interpreter it launches is unreadable ("Unable to create process ... Access is denied"; the only cause is the missing read grant). The `[sandbox: ...]` note on the failing command, and a `python grant` row in `mini-agent --doctor`, give the one-step recipe with the exact interpreter directory that `windows-appcontainer-toolchain-discovery` would add, for example `windows-appcontainer-read-roots = ['C:\\Python312']` in the global config (restart required; an administrator-owned install also needs an elevated launcher).

Three opt-in global-config keys (all default `false`; none is needed for the list above) widen the container; the `[sandbox: ...]` note on a failing command names the one that matches:

- `windows-appcontainer-ancestor-listing = true`: lets the container list the entries (names, sizes, timestamps) inside each parent of the workspace, so `cmd dir` and other tools that resolve the working directory by name work. System-owned parents (`C:\Users`, the drive root) need mini-agent started from an elevated launcher.
- `windows-appcontainer-null-device = true`: with an elevated launcher, lets the container open the NUL device (`>nul`, Python/Node `DEVNULL`, the host Git).
- `windows-appcontainer-toolchain-discovery = true`: read and execute on the install directory of `python`, `node`, ... when the command names them. An administrator-owned install needs an elevated launcher; or add the directory to `windows-appcontainer-read-roots`.

`mini-agent --doctor` shows the effective state of the three keys, whether the launcher is elevated, which `git` the contained shell resolves first, and a warning when an earlier elevated session left grants that an unelevated launch could not remove (start mini-agent elevated once to clear them). Settings are global-only and a managed policy can force each off. See `docs/agent/CONFIG.md`.

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
`permission_denials`. Choose a mode that allows it (`--yolo`; `--accept-all` still asks for shell and web), add `--allow TOOL:PATTERN`
rules for exactly what the job needs, or use `--read-only` for review jobs.

### Reading piped stdin (limit 1048576 bytes)

`-p` reads redirected stdin through EOF (up to 1 MiB) and appends it as untrusted reference text; the notice is printed only once stdin actually delivers data, so an empty or null stdin is silent. In CI,
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
