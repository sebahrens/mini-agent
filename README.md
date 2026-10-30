<p align="center">
  <img src="assets/mini-agent-logo.png" alt="mini-agent" width="760">
</p>

# mini-agent

**A small Rust coding agent that uses JavaScript as a safe, portable action language—and, in a
skills-enabled build, learns better JavaScript tools for work it repeatedly encounters.**

mini-agent starts from [ZeroStack](https://github.com/gi-dellav/zerostack), whose design gets the
foundation right: a compact native agent, a fast terminal UI, strong permission controls, multiple
model providers, persistent sessions, MCP, worktrees, subagents, prompts, memory, and automation
without the runtime footprint of an Electron or Node application. mini-agent keeps that deliberately
small core and adds a brokered QuickJS engine, native Linux/macOS/Windows containment, and an
opt-in, verified library of reusable agent-authored JavaScript skills.

The result is still a practical coding agent, but its action space is no longer limited to a pile of
one-off shell commands.

## TL;DR — what users need to know

mini-agent is a small, local-first Rust coding agent for the terminal. Its defining capability is
**code as tool**: instead of choosing one host tool call at a time, the model can write a short,
bounded JavaScript program that parses, filters, branches, loops, and composes typed operations in a
single action. QuickJS handles the computation while Rust authorizes every external effect and the
OS contains the worker.

With the optional `skills` build, a successful recorded JavaScript step can be distilled into an
editable proposal, then verified, approved, activated, and retrieved for relevant work later.
Attributable task outcomes and human feedback let operators improve the library over time through
explicit promotion and retirement, while directly attributed faults can automatically quarantine a
revision. The [Skill Gym](docs/agent/GYM.md) adds isolated no-library versus library episodes for
deliberate training and evaluation; gym results never auto-author, approve, activate, or promote a
skill.

| Area | What it means for you |
|---|---|
| Interfaces | Use the interactive terminal UI for ongoing work, `mini-agent -p` for one-shot prompts, or `mini-agent -p --output json` for automation. ACP support lets compatible editors and clients drive the same agent. |
| Models | The GenAI Shared Service is supported through custom-provider configuration, alongside OpenRouter, OpenAI, Anthropic, Gemini, Ollama, and other custom providers. OpenRouter remains the upstream default. Model credentials stay parent-side and are never exposed to contained JavaScript workers. |
| Core workflow | The model reads and edits files, searches code, runs permission-controlled commands, uses structured local Git operations, fetches and searches the web through native permission-checked tools, and can delegate bounded work to subagents in the foreground or as background tasks. Sessions, compaction, prompts, memory, loops, MCP tools, and Git worktrees cover longer-running repository work; hooks and LSP integration are optional. |
| JavaScript actions | The model can filter, parse, loop, branch, and combine tool results inside one bounded QuickJS program. There is no Node.js sidecar, package manager, `require()`, `import()`, or direct OS API. A fresh JavaScript runtime is created for every request. |
| Permissions and effects | Rust owns every JavaScript external effect. Brokered file, fetch, process, and skill operations are typed, scope-narrowed, permission-checked, bounded, and audited by the parent process. Approval of one operation does not grant unrelated authority. |
| Sandboxing | Linux, macOS, and Windows use different native containment backends behind the same JavaScript contract. Startup includes live platform checks; if mini-agent cannot prove the required boundary, the affected capability fails closed instead of silently running uncontained. |
| Learning over time | The optional `skills` build can distil a recorded successful JavaScript step into an editable proposal and turn approved proposals into content-addressed local tools. It attributes verified outcomes and feedback to exact revisions; approval, activation, and promotion remain explicit human actions, while directly attributed faults can quarantine a revision automatically. |
| Skill Gym | The operator-run gym exercises the real skill store and lifecycle in isolated paired episodes, comparing the same task with and without a library. It supports repeatable training and evaluation while keeping gym outcomes non-production and unable to change lifecycle state automatically. |
| Default versus optional | Release binaries include JavaScript execution, sandboxing, MCP, ACP, subagents, worktrees, loops, memory, LSP diagnostics, lifecycle hooks, image attachments, and the other default features. Learned-skill storage is not included in release archives; install from source with `--features skills` to enable it. |
| Data and operation | Configuration, sessions, memory, and learned skills are local. The contained JavaScript worker has no direct network access; providers, MCP services, hooks, and shell or fetch operations follow their separately visible configuration, permission, and containment policies. |

The shortest mental model is: **the model decides, JavaScript coordinates tools, Rust authorizes and
executes effects, the OS contains the worker, and verified skills preserve what works**. Start with
`mini-agent --setup`, then run `mini-agent`; use the [quick start](#quick-start) for installation and
the [configuration guide](docs/agent/CONFIG.md) when you want to change providers, permissions,
sandboxing, or optional integrations.

## Why ZeroStack is the right foundation

ZeroStack proves that a capable coding agent does not need to be a large desktop application. Its
upstream project reports roughly **30k core lines of Rust**, a **26 MB binary**, about **16 MB average
RAM / 24 MB peak RAM**, **0% idle CPU**, and approximately **1.5% CPU while using tools** on its
reference machine. Those are upstream measurements rather than mini-agent benchmarks, but they
capture the design priority mini-agent preserves: spend resources on the model and the task, not on
the agent shell.

Just as importantly, ZeroStack already provides the everyday product around the agent loop:

- the GenAI Shared Service through custom-provider configuration, plus OpenRouter, OpenAI,
  Anthropic, Gemini, Ollama, and other custom providers
- a responsive terminal UI plus headless one-shot operation
- configurable permissions and session-scoped approvals
- saved sessions, compaction, prompts, memory, and context files
- MCP, subagents, Git worktrees, long-running loops, hooks, LSP, and ACP feature flags
- a native Rust binary with near-zero idle overhead

mini-agent is an extension of that philosophy, not a replacement for it: keep the excellent minimal
host, then give the agent a more expressive and portable way to act.

Upstream ZeroStack's README currently labels Windows support as untested. mini-agent's most important
architectural departure is to stop treating a Unix shell as the universal action substrate: the
portable JS contract is tested on Linux, macOS, and Windows, with a native containment adapter for
each.

## What mini-agent adds

| Addition | Why it matters |
|---|---|
| JavaScript as a tool | The model can filter, transform, branch, loop, and combine results in one bounded program instead of emitting many fragile tool calls. |
| QuickJS, not a Node sidecar | The JavaScript engine is small and embeddable; there is no package manager, module loader, or background Node service. |
| One cross-platform action contract | The same JavaScript and typed effects are available on Linux, macOS, and Windows. OS-specific code is confined to launch and containment. |
| Parent-owned capability broker | JavaScript cannot directly open the filesystem, network, or processes. Every real effect is typed, narrowed, permission-checked, audited, and executed by Rust. |
| Structured local Git | The agent can inspect history and safely stage, unstage, or commit through seven fixed permission-checked operations without raw shell arguments, remotes, or network access. |
| Fresh runtimes | Every step and every complete verification request receives a new bounded QuickJS runtime. No JavaScript heap survives between requests. |
| Curated skill library | The agent can propose useful JavaScript snippets, verify them, retrieve them by meaning, and improve the library from attributable evidence. |
| Evidence-based lifecycle | Skills progress through proposal, verification, human-gated canary, operator promotion or retirement, and automatic quarantine. Repair and rollback are implemented and tested as library contracts but are not reachable from the shipped binary. |

## Code as tool

Traditional agents choose one tool call at a time. That works for simple operations, but orchestration
quickly moves into the model's token stream: call a tool, inspect text, decide, call another tool,
repeat. Control flow is implicit, intermediate data is repeatedly serialized, and each round trip is
another opportunity to lose intent.

Code-as-tool gives the model a small program as its action:

```js
const manifest = JSON.parse(read_file("package.json"));
const internal = Object.entries(manifest.dependencies ?? {})
  .filter(([name]) => name.startsWith("@acme/"))
  .map(([name, version]) => ({ name, version }));

console.log(JSON.stringify(internal));
```

That program can express composition, conditionals, loops, parsing, aggregation, and retry decisions
directly. Apple's [CodeAct research](https://machinelearning.apple.com/research/codeact) describes
the same core advantage: executable code creates a unified action space in which an agent can compose
tools and revise its action from execution feedback; the paper reports gains of up to 20% on its
evaluated tasks.

In mini-agent, “code as tool” does **not** mean arbitrary ambient authority. The JavaScript language
handles computation; Rust retains effects. Model-authored code may receive the bounded
`read_file`, `read_files`, `list_dir`, `glob`, `grep`, `write_file`, `fetch`, `spawn`, `result`,
`scratch_put`, and `scratch_get` operations. A trusted opt-in can additionally expose
`propose_skill`; `skills_search` is a separate read-only discovery tool in skills-enabled builds.
Every operation is capability-brokered with explicit grants. There is no `require()`, `import()`,
or direct network/filesystem API.

## JavaScript as the cross-platform feature

Shell is not one language across operating systems. Bash, Command Prompt, PowerShell, executable lookup, quoting,
signals, path syntax, and sandbox primitives all differ. A Bash-first action design therefore makes
one platform canonical and the others compatibility ports.

JavaScript changes where that portability boundary sits:

```text
the same JS program
        │
        ▼
the same typed effect protocol
        │
        ├── Linux  → bubblewrap worker + Rust broker
        ├── macOS  → Seatbelt guardian + Rust broker
        └── Windows→ LPAC/AppContainer + Job + Rust broker
```

[QuickJS](https://bellard.org/quickjs/) is a particularly good fit: its official project describes a
small embeddable engine with modern ECMAScript support, fast runtime startup, and near-complete
Test262 conformance. The agent writes one standardized language; mini-agent implements the platform
differences below the effect contract.

Feature parity comes from four things together—not from JavaScript alone:

1. one JavaScript ABI and wire protocol;
2. one parent-side capability and permission model;
3. one behavioral test matrix for all three targets; and
4. platform-native containment that fails closed when its attestation is unavailable.

| Contract | Linux | macOS | Windows |
|---|---|---|---|
| JavaScript semantics and limits | QuickJS | QuickJS | QuickJS |
| Typed file/fetch/process/proposal effects | Same broker contract | Same broker contract | Same broker contract |
| Fresh runtime per request | Yes | Yes | Yes |
| No ambient worker workspace, credentials, or network | Enforced empty-root `bwrap` profile | Deny-default Seatbelt one-time image on validated macOS 26 hosts | LPAC/AppContainer token, handle, and Job attestation |
| Uncontained production fallback | Never | Never | Never |

The core JS feature contract is equal; high-authority operations remain available only when their
platform backend can prove the required guarantee. Linux's worker profile is enforced by namespaces
and seccomp. macOS uses Apple's deprecated Seatbelt interface and is therefore labeled
`DeprecatedBestEffort`, even though every launch runs the full live denial matrix. Windows uses
LPAC/AppContainer and Job objects, whose ambient filesystem visibility still depends partly on host
ACLs; learned-skill process spawning remains disabled there until an immutable-executable backend
exists. If a required backend or live proof fails, that capability—or JavaScript itself—is
unavailable instead of silently running with more authority.

One deliberate exception remains: checked replacement of an existing workspace file fails closed on
Windows because NT rename has no destination file-identity compare-and-swap operation. mini-agent
does not trade away the raced-edit protection used by Linux and macOS merely to claim parity.

See [the architecture overview](ARCHITECTURE.md) and
[brokered runtime specification](docs/specs/phase-6-brokered-js-runtime.md) for the exact threat
model.

### Getting the `js` tool used

Two things decide whether the model reaches for `js` on everyday work. First, brokered JS reads are
default-deny: set `js-read-roots = ["."]` (read-only, workspace) in `config.toml`, otherwise every
`read_file` in a script fails with `no js-read-roots configured`. Second, the model needs routing
guidance: a persona prompt needs a
"Built-in tool routing" section that sends cross-file questions (per-file counts, "which X has no
Y", cross-file references, JSON parsing) to a single `js` call and keeps single lookups on
`read`/`grep`. Several mini-agent processes can use `js` at the same time; each takes its own
effect-audit slot (see [platform paths](docs/specs/platform-paths.md)).

## An agent-curated library of JavaScript tools

The deeper idea is not merely “let the model run JavaScript.” It is to let the agent turn repeated
problem-solving work into a small, local, verified tool library.

Suppose the agent repeatedly needs to inspect Cargo metadata, normalize test output, compare API
schemas, or summarize dependency graphs. The first time, it writes task-specific JavaScript. When a
pattern is general enough to reuse, it may propose a named skill containing:

- source code and exported functions;
- a natural-language description and retrieval tags;
- exact capability scopes, such as read access limited to `src/`;
- ordered JavaScript tests that must evaluate to the boolean `true`; and
- ABI and lineage metadata.

The lifecycle is deliberately curatorial:

```text
repeated task
    → propose immutable JS skill
    → verify in fresh no-effect runtimes + held-out cases
    → human-approved, non-retrievable canary
    → explicit operator activation or promotion
    → retrieval for a relevant future prompt
    → attributed success or failure evidence
    → retain / quarantine / retire / supersede
```

Promotion of a replacement over its predecessor is an explicit local-owner command, not something
evidence performs on its own: nothing in the shipped binary promotes a canary because a threshold
was crossed. Quarantine is the one automatic transition, and it only ever removes a revision from
retrieval.

This follows the useful part of the
[Voyager skill-library idea](https://voyager.minedojo.org/): code represents temporally extended,
composable behavior; descriptions make skills retrievable; and new programs can build on simpler
ones. mini-agent adapts that pattern for coding work and adds stricter admission and authority
boundaries.

Every learned skill is content-addressed. Identity version 2 hashes the full canonical payload—not
just source—including tests, discovery metadata, ABI version, exports, and structured capability
scopes. Changing a test or widening a path therefore creates a different immutable artifact. The
worker never reads the skill database, and retrieved skills receive only their declared capabilities.

This is **bounded self-improvement**, not uncontrolled self-modification:

- proposing a skill cannot execute or activate it;
- verification has deterministic fakes and fresh runtime state;
- human approval is required before the initial canary;
- write, process, and network authority keep their human gates;
- production evidence must be directly attributable to the exact skill revision;
- failures can quarantine a revision without deleting its audit history; and
- every change of authority is a new immutable revision, so recovery never means guessing what
  changed.

Skills-enabled builds ship the local-owner lifecycle surface. Operators can import and
contained-verify packages, install the bundled seed library, inspect status and usage, list
proposals and read one proposal's admission outcome, approve or reject candidates, explicitly
activate an approved lineage root, promote an approved replacement over its active or quarantined
predecessor, retire an active revision without deleting it, submit attributable feedback, compact
retained events, and irreversibly purge a revision. Setting `enable_skill_proposals = true` in
trusted configuration also exposes `propose_skill` and starts the bounded proposal/admission
workers. Every approval, activation, and promotion gate above remains separate; importing or
proposing code never activates it automatically.

Two lifecycle capabilities described in the specifications are library contracts only and have no
command: **rollback** to a previous revision, and **repair**, which is compiled solely into the
verification suite. The recovery path for a defective active revision is therefore forward, not
backward: contain it (severe safety feedback quarantines it immediately; `--retire-learned-skill`
disables it administratively), get a corrected replacement approved, then promote the replacement
over the quarantined predecessor. See
[the Phase 5 spec](docs/specs/phase-5-evidence-learning.md) for the exact sequence and its
preconditions.

The local CLI's separate approval and root-activation actions use one `local-owner` identity; they
are two deliberate gates, not a claim that two independent people reviewed the skill.

Over time, the agent spends fewer tokens rediscovering reliable transformations and gains
domain-specific tools shaped by the repository it actually works in. The library becomes a compact
record of executable know-how rather than a growing prompt full of prose recipes.

Completion verification now records whether real work passed and which learned revisions were
actually invoked; operators can compare those task counts with matched no-skill baselines in
`--learned-skill-stats`.

## Skill Gym

Normal work and Gym training improve the same kind of learned JavaScript library, but they keep
their evidence separate. After the agent solves a real task with a successful JavaScript tool call,
an operator can turn that recorded step into an editable skill proposal:

```bash
mini-agent --distill-learned-skill <session-id> <tool-call-id>
```

The distiller either generalizes the recorded code with a configured model or emits an explicit
scaffold for the operator to finish. It never silently activates the result: the package must still
be reviewed, imported, verified, approved, and activated through the normal learned-skill
lifecycle.

The [Skill Gym](docs/agent/GYM.md) provides the deliberate training and evaluation loop. It runs the
same task in isolated `none` and `library` arms, gives each episode its own worktree and local state,
checks the result with a mechanical oracle, and writes comparable JSONL outcomes. A minimal run is:

```bash
scripts/gym/setup.sh
scripts/gym/train.sh \
  --tasks tasks.json \
  --output outcomes.jsonl \
  --binary "$PWD/.gym/bin/mini-agent" \
  --agent-arg=--yolo
```

This lets operators iterate on a candidate library and measure whether it helps across repeatable
tasks instead of judging it from anecdotes. Gym outcomes are always marked non-production and
cannot author, approve, activate, or promote a skill automatically; an operator decides what to
change and what enters the production lifecycle. The current Gym runs on Linux and macOS. See the
[full Gym guide](docs/agent/GYM.md) for task mining, provider/model selection, permission modes,
budgets, isolation, and result interpretation.

## Quick start

The Cargo package, installed CLI, and every binary release archive use the executable name
`mini-agent`.

Install the latest release on Linux or macOS:

```bash
curl -fsSL https://raw.githubusercontent.com/sebahrens/mini-agent/main/install.sh | bash
```

To install an exact release instead of the latest one, pass `--release`:

```bash
curl -fsSL https://raw.githubusercontent.com/sebahrens/mini-agent/main/install.sh | bash -s -- --release X.Y.Z
```

The installer refuses an archive that does not match the release's `SHA256SUMS`. That manifest is
served from the same release as the archive, so it catches a damaged or truncated download but not a
replaced release. Every release asset also carries SLSA build provenance signed by the release
workflow: when the GitHub CLI is installed and signed in, the installer runs
`gh attestation verify <archive> --repo sebahrens/mini-agent` and aborts if it fails. Without `gh` it
installs with a warning that prints the command; to verify a downloaded asset yourself, run:

```bash
gh attestation verify mini-agent-<arch>-<os>.tar.gz --repo sebahrens/mini-agent
```

`MINI_AGENT_SKIP_ATTESTATION=1` skips the provenance check (the `SHA256SUMS` check always runs).

If the anonymous download fails or returns a sign-in page (for example, a private fork), the
installer retries with credentials: first with `GITHUB_TOKEN` through the GitHub REST API (sent
as a header, never on the command line), then with `gh release download` when the GitHub CLI is
installed and `gh auth status` succeeds. Assets fetched either way pass the same `SHA256SUMS`
and provenance checks. Set `MINI_AGENT_INSTALL_NO_TOKEN=1` or `MINI_AGENT_INSTALL_NO_GH=1` to disable a
fallback.

On x86-64 Windows, the same release publishes
`mini-agent-windows-x64.msi` plus `MSI_SHA256SUMS`. After verifying the
checksum, open the MSI for a per-user, no-admin install; it also attempts to
side-load the bundled VSIX when VS Code is installed. Enterprise deployment
can use `msiexec /i mini-agent-windows-x64.msi ALLUSERS=1 /quiet /norestart`.
See the [Windows installation guide](docs/agent/WINDOWS_INSTALL.md) for the release link, exact
PowerShell checksum verification, VSIX-only installation, enterprise download alternatives, and
current Marketplace status.

Release binaries are not code-signed yet. The Windows MSI and `mini-agent.exe` carry no
Authenticode signature, and the macOS binaries are neither Developer ID signed nor notarized.
Opening a browser-downloaded MSI therefore shows a SmartScreen "Windows protected your PC"
prompt; choose **More info**, then **Run anyway**. On macOS, the shell installer's `curl`
download is not quarantined, but a `.tar.gz` downloaded with a browser is: after extracting it,
run `xattr -d com.apple.quarantine ./mini-agent` (or Control-click the binary in Finder and choose
**Open** once). Verify integrity before bypassing either prompt:

- compare the file's SHA-256 with the release's `SHA256SUMS` or `MSI_SHA256SUMS`
  (`Get-FileHash mini-agent-windows-x64.msi -Algorithm SHA256` on Windows,
  `shasum -a 256 <file>` on macOS); and
- check its build provenance with
  `gh attestation verify <file> --repo sebahrens/mini-agent`.

Because there is no publisher certificate, AppLocker and WDAC publisher rules cannot allow
mini-agent by signer identity. Managed fleets that enforce application control must use file-hash
rules for the verified MSI and `mini-agent.exe`, and refresh them on every release.

### Build an optimized binary from source

From a source checkout, `make install` is the production-style local installation. It enables every
optional feature, compiles with the optimized Cargo release profile, and replaces an existing
`mini-agent` installation in Cargo's binary directory:

```bash
git clone https://github.com/sebahrens/mini-agent.git
cd mini-agent
make install
```

Cargo installs with the release profile by default, so a successful build reports
`Finished release profile [optimized]`. The command run by `make install` is:

```bash
cargo install --locked --path . --features "hooks,lsp,skills,multimodal,pdf,advisor" --force
```

That is the default feature set plus every optional feature that builds on Linux, WSL, macOS, and
Windows without extra native libraries. It deliberately avoids `--all-features`, which would also
enable `skills-embed` and require a prebuilt ONNX Runtime (`ort-sys`). Override the list with
`make install FEATURES=...`.

`make install` also installs the bundled [ghagent-mcp](tools/ghagent-mcp/README.md) companion server
(`~/.local/bin/ghagent-mcp`, override with `PREFIX=`). It is a small stdlib-only Python MCP server that
runs outside the agent sandbox and gives a sandboxed agent two guarded GitHub operations,
`push_branch` and `wait_for_checks` (plus an opt-in `create_release`): fast-forward-only pushes to
non-protected branches of allowlisted repositories, and CI check-run status. It is not enabled
automatically; register it under `[mcp_servers.ghagent]` as described in its README.

Source builds also need Python 3 and the native C, curl and zlib development
toolchain for the embedded Git runtime:

| Platform | Packages |
|---|---|
| Debian/Ubuntu | `apt install build-essential libcurl4-openssl-dev zlib1g-dev python3` |
| Fedora/RHEL | `dnf install gcc make patch libcurl-devel zlib-devel python3` |
| macOS | `xcode-select --install` (system curl and zlib) |

Prebuilt release archives already contain the Git runtime payload and need none of these.

Windows and cross-target builds supply an
explicit prepared payload. See [Git runtime build prerequisites](docs/development/git-runtime-bootstrap.md)
and [static Linux builds](docs/development/git-runtime-musl.md). Prebuilt release
archives already include this payload.

`--locked` uses the dependency versions in the checked-in `Cargo.lock`. Do not run
`cargo install mini-agent`: crates.io currently assigns that name to an unrelated project. The
faster, unoptimized contributor build (which reports `Finished dev profile [unoptimized]`) is
documented in [CONTRIBUTING.md](CONTRIBUTING.md) together with the complete contributor workflow.

### Linux prerequisites

On Linux, both the default subprocess sandbox and the JavaScript runtime use
[bubblewrap](https://github.com/containers/bubblewrap) (`bwrap`). Release archives do not bundle
it; install it from your distribution as a root-owned system executable (mini-agent ignores a
`bwrap` in a user-writable directory):

```bash
sudo apt install bubblewrap      # Debian, Ubuntu
sudo dnf install bubblewrap      # Fedora
sudo pacman -S bubblewrap        # Arch (the AUR package lists it as an optional dependency)
```

Ubuntu 23.10 and later (including 24.04) restrict unprivileged user namespaces through AppArmor
(`kernel.apparmor_restrict_unprivileged_userns=1`), which bubblewrap needs. Check with
`bwrap --unshare-user --ro-bind / / true`; if it prints
`setting up uid map: Permission denied`, pick one of:

- **Per-binary AppArmor profile (recommended).** Allow user namespaces for `bwrap` only:

  ```bash
  sudo tee /etc/apparmor.d/bwrap <<'EOF'
  abi <abi/4.0>,
  include <tunables/global>

  profile bwrap /usr/bin/bwrap flags=(unconfined) {
    userns,
    include if exists <local/bwrap>
  }
  EOF
  sudo apparmor_parser -r /etc/apparmor.d/bwrap
  ```

  Any local program can then create user namespaces *through bwrap*, but the restriction stays in
  place for everything else.
- **Disable the restriction system-wide.** `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`
  (persist it in `/etc/sysctl.d/`). This is simpler but re-exposes the kernel's
  unprivileged-user-namespace attack surface to every program, which is what Ubuntu's default
  guards against.

Without a working `bwrap`, mini-agent cannot enforce its Linux sandbox. Its diagnostic (the
startup or launch error, and the logged preflight warning) names the cause: a missing `bwrap`, or
the AppArmor restriction above with both fixes. What mini-agent then does for ordinary
subprocesses follows the sandbox policy in
[Feature contract](docs/agent/GET_STARTED.md#6-feature-contract): an explicit `--sandbox` refuses
to start, while an implicit default warns. The `js` tool is always reported unavailable, since
it has no uncontained fallback. The shell installer warns when `bwrap` is not on `PATH`.

#### Tools outside the system directories (Homebrew, Cargo, `~/.local/bin`)

The sandbox mounts only the system runtime directories (`/usr`, `/bin`, `/sbin`, `/lib*`, `/nix`)
plus the workspace. CLIs installed elsewhere are invisible inside it, and the shell tool reports
`command not found` even though `PATH` names them. `sandbox-toolchain-roots` adds read-only binds for
such trees. Its default is:

```toml
sandbox-toolchain-roots = ["/home/linuxbrew", "~/.cargo/bin", "~/.local/bin", "~/.rustup"]
```

This covers Homebrew on Linux (for example `bd`), Cargo-installed binaries, user-local scripts,
and the rustup toolchains that Cargo's proxies and Debian's `/usr/bin/cargo` need. Missing entries
are skipped, mounted `bin` directories are appended to the sandbox `PATH`, and nothing is writable.
`~/.cargo` itself is not listed because it holds `credentials.toml`. Setting the key replaces the
default, so restate the defaults you want to keep, for example
`sandbox-toolchain-roots = ["/home/linuxbrew", "~/.rustup", "/opt/tools"]`. Use `[]` to bind nothing
extra. The `--sandbox-toolchain-root PATH` flag (repeatable) overrides both for one run. Run
`mini-agent --print-config` to see the configured and effective roots under **Sandbox
capabilities**. Entries must be absolute or start with `~/`; mini-agent refuses an entry that is `/`,
your home directory or one of its parents, contains the workspace or application cache, or overlaps
its private configuration, credential, or state directory. Commands that write to `~/.cargo`
(for example `cargo build` fetching crates) still cannot write there from inside the sandbox.

#### Dot folders, MCP servers, and network access

Making a CLI's binary visible is only half of it: most tools also keep data in HOME dot folders,
and the default `workspace` sandbox profile hides HOME. Three rules decide what a tool can reach.

1. **Shell commands are sandboxed; MCP stdio servers are not, unless you ask.** Bash and JS
   `spawn` run inside the general sandbox. An `[mcp_servers.NAME]` stdio entry without a
   `sandbox = "BACKEND"` key is launched as trusted code outside it, with a cleared environment
   plus its `env`/`inherit_env` values and the workspace as its working directory. Such a server
   can read its own dot folders and use the network. The usual way to give the agent a tool that
   needs HOME data or the network is therefore an MCP server rather than a shell command.
2. **HOME dot folders are hidden from shell commands until you mount them.**
   `sandbox-home-mounts` exposes HOME-relative dot files and folders, read-only by default or
   writable with a trailing `:rw` (directories only). For example, to run `json2pptx` through Bash
   as well as through its MCP server:

   ```toml
   sandbox-home-mounts = [
     ".json2pptx:rw",               # templates and wiki
     ".cache/json2pptx:rw",         # render cache
     ".cache/json2pptx-render:rw",  # thumbnail renders
     ".config/libreoffice:rw",      # soffice profile used for thumbnails
     ".gitconfig",                  # Git identity
     ".config/git",                 # Git ignore and attributes
   ]
   ```

   **1.12.0 release only (Linux):** with the bundled Git runtime in use, any non-empty
   `sandbox-home-mounts` list made every shell command fail with `sandbox profile unavailable:
   private mask overlaps a retained Git executable`. Builds from `main` after 1.12.0 fix this;
   the masked runtime stays hidden from contained commands. On the 1.12.0 release, leave the list
   unset and reach HOME data through MCP servers.

   Missing entries are skipped. Mounts are refused if they name, contain or lie inside a masked
   path. Built-in masks that can never be exposed are `.ssh`, `.aws`, `.gnupg`, `.kube`,
   `.azure`, `.config/gcloud`, `.config/gh`, and mini-agent's own configuration, credential, state
   and data directories. That is why `.config` cannot be mounted whole and must be listed per
   subfolder. Restart mini-agent after changing the list; `mini-agent --print-config` shows the
   effective grants.
3. **Shell commands never have network access.** The Linux bwrap sandbox always uses a separate
   network namespace, and macOS Seatbelt and Windows AppContainer deny network the same way. No
   mount changes this. A networked CLI such as `gh` therefore fails inside the sandbox for two
   reasons: no network, and `.config/gh`, which holds its login, is masked. Use a GitHub MCP
   server instead, as described below. The only other option is to start the session with
   `--no-sandbox`, which turns containment off for every shell command in that session.

##### GitHub access through the GitHub MCP server

GitHub's official MCP server runs outside the sandbox and gives the agent repository, issue,
pull-request and Actions tools without exposing your `gh` login to shell commands. Install it
(Homebrew shown; release binaries are also published by GitHub):

```bash
brew install github-mcp-server
```

Add the server to `~/.config/mini-agent/config.toml`:

```toml
[mcp_servers.github]
command = "/home/linuxbrew/.linuxbrew/bin/github-mcp-server"   # `command -v github-mcp-server`
args = ["stdio", "--toolsets=repos,issues,pull_requests,actions"]
inherit_env = ["HOME", "PATH", "GITHUB_PERSONAL_ACCESS_TOKEN"]
```

`--toolsets` keeps the tool list small (43 tools for these four groups); add `--read-only` to
remove every write tool. Never put the token in `config.toml`. Export it from the environment
mini-agent starts in; `inherit_env` passes only the listed names to the server. To reuse an
existing `gh auth login` without copying the token anywhere, add this to `~/.bashrc` (or your
shell's profile):

```bash
if command -v gh >/dev/null 2>&1; then
  GITHUB_PERSONAL_ACCESS_TOKEN="$(gh auth token 2>/dev/null)" && export GITHUB_PERSONAL_ACCESS_TOKEN
fi
```

Open a new shell, start mini-agent, and check that `mini-agent --doctor` counts the server under
**MCP configured servers**. The server can do whatever the token's scopes allow (`gh auth status`
lists them), so use `--read-only` or a fine-grained token when the agent should only read.

##### Example: code-indexer, json2pptx and GitHub together

| Tool | Configured as | Runs | Needs |
|---|---|---|---|
| code-indexer | `[mcp_servers.code-indexer]` (`code-indexer-server mcp`) | outside the sandbox | nothing extra; it writes its index to `.code-indexer/` in the workspace |
| json2pptx | `[mcp_servers.json2pptx]` (`json2pptx mcp`) | outside the sandbox | nothing extra; it reads `~/.json2pptx/templates` and writes `./output` |
| json2pptx through Bash | shell command | inside the sandbox | the `sandbox-home-mounts` entries shown above; on the 1.12.0 release use the MCP server instead (see the note above) |
| GitHub | `[mcp_servers.github]` | outside the sandbox | `GITHUB_PERSONAL_ACCESS_TOKEN` exported, listed in `inherit_env` |
| `gh` through Bash | shell command | inside the sandbox | not possible: no network, `.config/gh` masked |

Project-level `.mini-agent/config.toml` layers need trusted-project approval before their
`sandbox-home-mounts` take effect. They may add `sandbox-home-masks` without approval, because
masks can only narrow access.

Configure a provider interactively, then start a session:

```bash
mini-agent --setup
mini-agent
```

OpenRouter remains the upstream default and can also be configured directly:

```bash
export OPENROUTER_API_KEY="sk-or-v1-..."
mini-agent -p "Explain this repository's architecture"

# Emit one machine-readable result (final text, changed files, tool/usage stats, and cost)
mini-agent -p --output json "Update the README example"
```

See [Getting started](docs/agent/GET_STARTED.md),
[provider configuration](docs/agent/PROVIDERS.md), and
[the complete configuration reference](docs/agent/CONFIG.md). Contributors can
also run the [deterministic harness evaluation](docs/agent/HARNESS_EVAL.md) for
task-level regression coverage without a live provider.

### Configuration checklist

Everything below lives in the global config file (`mini-agent --print-config` prints its path;
on Linux/WSL it is `~/.config/mini-agent/config.toml`) or in your shell environment. Scalar keys
such as `provider` or `sandbox-home-mounts` must appear before the first `[table]` header.
Secrets never go into the file: providers and MCP servers read them from named environment
variables.

| # | What | Required? | How |
|---|---|---|---|
| 1 | Model provider | yes | `mini-agent --setup`, or set `provider`/`model` and export the provider's key variable (for example `OPENROUTER_API_KEY`). See [provider configuration](docs/agent/PROVIDERS.md). |
| 2 | Sandbox backend | Linux/WSL: yes | Install `bwrap` ([Linux prerequisites](#linux-prerequisites)). macOS and Windows use their built-in backends. |
| 3 | Tools outside system directories | only if a CLI reports `command not found` | Add its prefix to `sandbox-toolchain-roots` ([details](#tools-outside-the-system-directories-homebrew-cargo-localbin)). |
| 4 | MCP servers | for code-indexer, json2pptx, GitHub, etc. | One `[mcp_servers.NAME]` table per server (examples below). They run outside the sandbox. |
| 5 | Personas | optional | Install persona files into `~/.local/share/mini-agent/agents/`. Persona files may include a `code-indexer` specialist. |
| 6 | GitHub token | only with the GitHub server | Export `GITHUB_PERSONAL_ACCESS_TOKEN` before starting mini-agent ([details](#github-access-through-the-github-mcp-server)). |
| 7 | HOME dot folders for shell commands | optional | `sandbox-home-mounts` ([details](#dot-folders-mcp-servers-and-network-access)). Requires a build newer than 1.12.0. |

Minimal MCP entries for the code-navigation and slide tools used by the coding personas:

```toml
[mcp_servers.code-indexer]
command = "code-indexer-server"            # absolute path if it is not on PATH
args = ["mcp"]
inherit_env = ["HOME", "PATH"]
env = { RUST_LOG = "warn" }

[mcp_servers.json2pptx]
command = "json2pptx"
args = ["mcp", "--templates-dir", "/absolute/path/to/json2pptx/templates", "--output", "./output"]
inherit_env = ["HOME", "PATH"]
```

`code-indexer-server` indexes the directory mini-agent starts in and writes `.code-indexer/` there,
so start mini-agent from the repository root. Add `.code-indexer/` to the repository's
`.gitignore`. Agents load deferred MCP tools with `mcp_search`, so naming several tools in one
query (`ci_search_symbols ci_get_impl ci_find_callers`) enables them all at once.

To use the `code-indexer` specialist persona from another persona, delegate with
`task {"agent_type": "code-indexer", ...}`. The specialist needs both the `code-indexer` MCP server
above and a `code-indexer` persona file in
the agents directory. Its frontmatter `mcp_servers` allowlist names exactly the `ci_*` tools it
may call, at most 8 servers and 32 tools.

After any change, restart mini-agent and check:

```bash
mini-agent --print-config   # effective config, sandbox capabilities, load warnings
mini-agent --doctor         # provider, sandbox and "MCP configured servers" count
```

`load warnings: 0` means every key was recognised. An unknown key, for example one your build is
too old for, is reported there and otherwise ignored.

## Feature flags

The default build enables `loop`, `goal`, `git-worktree`, `mcp`, `acp`, `subagents`, `archmd`,
`status-signals`, `multithread`, `export`, `js`, `sandbox`, `memory`, `agent-skills`, `lsp`, `hooks`, `multimodal`, `bedrock`, and `vertex`. Learned-skill storage is
opt-in because it adds SQLite and retrieval dependencies.

Pre-built release archives include JavaScript execution but not learned-skill storage. To install a
build that can propose, verify, retrieve, and curate skills, build from source with:

```bash
cargo install --locked --git https://github.com/sebahrens/mini-agent --tag v1.10.4 --features skills
```

| Feature | Adds |
|---|---|
| `js` | Brokered QuickJS worker and parent-owned effect globals |
| `sandbox` | Linux/macOS/Windows process isolation and brokered `spawn`/`fetch` where supported |
| `agent-skills` | Default-on bounded instruction catalogue, `/skills`, one-turn pinning and lexical retrieval; no JS/store dependency |
| `skills` | Immutable learned-skill store, deterministic embeddings, retrieval, verification, and lifecycle; implies `js` |
| `skills-embed` | Local BGE embedding inference through ONNX Runtime |
| `skills-embed-dynamic` | Runtime ONNX linkage for hosts without a prebuilt `ort-sys` binary |
| `memory` | Persistent project and user memory |
| `mcp` | MCP client transports and tool discovery |
| `acp` | Stable Agent Client Protocol server over stdio or authenticated TCP; included by default |
| `loop`, `goal` | Autonomous rounds and their persistent objective/completion contract; `loop` implies `goal` |
| `git-worktree`, `subagents`, `archmd` | Isolated worktrees, delegated agents, and repository architecture context |
| `status-signals`, `multithread`, `export` | Local status endpoints, multi-threaded Tokio execution, and session export |
| `lsp` | Default-on automatic discovery of installed language servers; explicit `[lsp] enabled = false` disables it |
| `hooks` | Included by default; no commands run without hook configuration |
| `multimodal` | Default image/audio/document message support, subject to provider capabilities |
| `advisor` | Optional second-model advice |
| `pdf` | Explicit PDF document-attachment feature sent as raw provider messages; implies `multimodal` |

Full release archives use these defaults. Lite archives retain their explicit `--no-default-features` recipe. Automatic LSP discovery checks installed executables and starts each server only when an authorized file operation needs it; nothing is installed or downloaded. `/toggle` lists captured runtime availability.

For the full feature graph, see [Cargo.toml](Cargo.toml).

To compile and install the checked-out source with the default features plus `hooks`, `lsp`,
`skills`, `multimodal`, `pdf`, and `advisor`, with release optimizations, run:

```bash
make install
```

`make install` deliberately keeps Cargo's default release profile for installs. The equivalent Cargo command is
`cargo install --locked --path . --features "hooks,lsp,skills,multimodal,pdf,advisor" --force`.
`skills-embed` is left out because `ort-sys` has no prebuilt ONNX Runtime for every host; add it
with `make install FEATURES=hooks,lsp,skills-embed,multimodal,pdf,advisor` where it builds. When selecting features
individually, pass them as one comma-delimited argument, for example
`--features skills,js,sandbox,memory,mcp,acp,multimodal,pdf`; shell-separated feature names are
interpreted as additional package names.

The multithreaded runtime provisions an explicit 8 MiB stack for each Tokio worker. This keeps
full-feature installations—including tool-enabled turns on WSL2—independent of an ambient
`RUST_MIN_STACK` setting.

## Architecture and specifications

Production QuickJS never exists in the trusted parent process. The parent lazily supervises at most
one contained same-executable worker, creates an immutable grant table for each invocation, and
executes every external effect itself. The worker creates and drops a fresh 64 MiB-heap,
512 KiB-stack runtime for every step or complete verification request. Transport, protocol,
deadline, cancellation, resource, or containment faults recycle the worker and revoke the invocation.

The normative design is split by concern:

- [Specification index](docs/specs/00-index.md)
- [Core JavaScript engine](docs/specs/phase-1-js-engine.md)
- [Sandboxing and brokered effects](docs/specs/phase-2-sandbox.md)
- [Skill library and retrieval](docs/specs/phase-3-skill-library.md)
- [Proposal and admission](docs/specs/phase-4-auto-admission.md)
- [Evidence learning, quarantine, and rollback](docs/specs/phase-5-evidence-learning.md)
- [Contained worker and capability broker](docs/specs/phase-6-brokered-js-runtime.md)
- [Subprocess trust classes](docs/specs/subprocess-trust.md)

`ARCHITECTURE.md` and `SPEC.md` are maintained overviews; the indexed phase documents are normative.

## Development

Contributor build, test, and commit rules, including the debug development build, are in
[CONTRIBUTING.md](CONTRIBUTING.md). Run them from the repository root:

```bash
cargo fmt
cargo test
```

The separate `spike/` workspace is retained only for QuickJS research and is never a production
target. Do not use `cargo build`, `cargo check`, or development `--release` builds in this repository.

## License and upstream

mini-agent is licensed under [GPL-3.0-only](LICENSE). Binary releases include the dated
[upstream and modification notice](NOTICE) plus [exact Corresponding Source directions](SOURCE.md),
and a generated `THIRD_PARTY_LICENSES` inventory of the bundled Rust crates and their license texts.
It is built from the excellent
[ZeroStack](https://github.com/gi-dellav/zerostack) project and preserves its preference for a small,
fast, understandable native agent.

The public product name, command, project directory (`.mini-agent`), `MINI_AGENT_*` environment
variables, hook project envelope (`MINI_AGENT_PROJECT_DIR`), log file names, audit targets, and new
export file names use `mini-agent`. Unambiguous pre-rename `zerostack` user-global artifacts are
imported through the versioned legacy migration, and `/import` still accepts the legacy
`zerostack-session-jsonl` schema marker regardless of the file name.
HTML exports are standalone viewing documents; use JSONL to import a transcript.
See the [identity and package compatibility policy](docs/agent/PUBLISHING_RELEASES.md#product-identity-matrix).

## Security

Report vulnerabilities privately through
[GitHub private vulnerability reporting](https://github.com/sebahrens/mini-agent/security/advisories/new),
not public issues. [SECURITY.md](SECURITY.md) lists the supported versions, response targets and what
is in scope; [CONTRIBUTING.md](CONTRIBUTING.md) covers the development workflow.
