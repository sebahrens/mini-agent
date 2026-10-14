---
description: "Get started with mini-agent: install the minimal Rust coding agent, configure an LLM provider, and run your first session."
---

# Part 1. Get Started

Thanks for picking up mini-agent. This guide covers installation, model setup, and the basic commands.

This tutorial applies to Linux, macOS, and Windows. The shell installer below
targets Linux and macOS; x86-64 Windows also has a release MSI. JavaScript actions expose the same brokered
feature contract on all three systems, while each platform uses and reports its own containment
assurance; see the repository README and architecture overview for those guarantees.

Repository guidance can live in root `AGENTS.md` / `CLAUDE.md`, personal
`CLAUDE.local.md`, or nested copies loaded when native file tools first touch a
directory. An authored `@path` (or `@{path with spaces}`) includes a bounded file
inside the same captured root. Local, nested and included project guidance needs
project trust and read Allow authority. `/context` shows captured filenames and
sizes without their bodies. `--no-context-files` disables loading, including in
readonly BTW and Task agents. See [context limits and review](CONFIG.md)
for details. A mutation denied pending instruction review has made no change;
the agent must receive and review the native notice before deliberately retrying.

## 1. Installation

You can build from source or use the shell installer. The shell installer requires a
complete release in the canonical repository:
```
curl -fsSL https://raw.githubusercontent.com/sebahrens/mini-agent/main/install.sh | bash
```

Append `-s -- --release X.Y.Z` to `bash` to install an exact release. The installer checks the
archive against the release's `SHA256SUMS` (which detects a damaged download, not a replaced
release) and, when the GitHub CLI is installed and signed in, requires
`gh attestation verify <archive> --repo sebahrens/mini-agent` to accept the archive's build
provenance. Without `gh` it installs with a warning that prints that command.

On x86-64 Windows, download `mini-agent-windows-x64.msi` and
`MSI_SHA256SUMS` from the same GitHub release, verify the checksum, then open
the MSI. It installs per-user without elevation by default and side-loads the
bundled VSIX when VS Code is present. Managed machines can use:

```powershell
msiexec /i mini-agent-windows-x64.msi ALLUSERS=1 /quiet /norestart
```

Release binaries are not code-signed yet: the MSI and `mini-agent.exe` have no Authenticode
signature, and the macOS binaries are not notarized. Windows SmartScreen therefore asks before
opening a downloaded MSI (choose **More info**, then **Run anyway**), and macOS Gatekeeper blocks a
browser-downloaded archive's binary until you run `xattr -d com.apple.quarantine ./mini-agent` or
Control-click it and choose **Open**. The `curl` installer above is not affected. Before bypassing
either prompt, check the file against `SHA256SUMS`/`MSI_SHA256SUMS` and run
`gh attestation verify <file> --repo sebahrens/mini-agent`. Application-control policies
(AppLocker, WDAC) cannot use publisher rules for mini-agent yet; use file-hash rules instead.

For a source checkout, use:
```
git clone https://github.com/sebahrens/mini-agent.git
cd mini-agent
cargo install --path . --locked
```

Contributors use the debug development build described in
[CONTRIBUTING.md](https://github.com/sebahrens/mini-agent/blob/main/CONTRIBUTING.md).

Do not run `cargo install mini-agent`: the crates.io package with that name is an unrelated project.

### Shell completions

`mini-agent --completions <shell>` prints a script for the flags in your installed
binary and exits before loading configuration, connecting to providers, or
starting the runtime. Regenerate it after updating the binary.

For Bash, save the script and source it from `~/.bashrc`:

```bash
mkdir -p ~/.local/share/bash-completion/completions
mini-agent --completions bash > ~/.local/share/bash-completion/completions/mini-agent
source ~/.local/share/bash-completion/completions/mini-agent
```

For Zsh, put `~/.zfunc` in `fpath` before `compinit` in `~/.zshrc`:

```zsh
mkdir -p ~/.zfunc
mini-agent --completions zsh > ~/.zfunc/_mini-agent
fpath=(~/.zfunc $fpath)
autoload -Uz compinit
compinit
```

Fish loads its completion directory automatically:

```fish
mkdir -p ~/.config/fish/completions
mini-agent --completions fish > ~/.config/fish/completions/mini-agent.fish
```

For PowerShell, save and dot-source the script; add the dot-source line to
your `$PROFILE` to load it in future sessions:

```powershell
mini-agent --completions powershell > "$HOME/mini-agent-completions.ps1"
. "$HOME/mini-agent-completions.ps1"
```

The repository retains a Homebrew formula for package-channel compatibility, but no canonical
`sebahrens` tap is published yet, so this guide does not advertise a tap command that cannot work.

### Linux prerequisites

On Linux, the default subprocess sandbox and the JavaScript runtime both use bubblewrap (`bwrap`),
which release archives do not bundle. Install it as a root-owned system executable (a `bwrap` in a
user-writable directory is ignored):

```
sudo apt install bubblewrap      # Debian, Ubuntu
sudo dnf install bubblewrap      # Fedora
sudo pacman -S bubblewrap        # Arch
```

Ubuntu 23.10 and later (including 24.04) set `kernel.apparmor_restrict_unprivileged_userns=1`,
which stops bubblewrap from creating its user namespace. `bwrap --unshare-user --ro-bind / / true`
then fails with `setting up uid map: Permission denied`. Choose one fix:

- Per-binary AppArmor profile (recommended): write `/etc/apparmor.d/bwrap` containing

  ```
  abi <abi/4.0>,
  include <tunables/global>

  profile bwrap /usr/bin/bwrap flags=(unconfined) {
    userns,
    include if exists <local/bwrap>
  }
  ```

  and load it with `sudo apparmor_parser -r /etc/apparmor.d/bwrap`. Only `bwrap` gains user
  namespaces; the restriction still applies to every other program.
- System-wide sysctl: `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`, persisted in
  `/etc/sysctl.d/`. Simpler, but it re-exposes the kernel's unprivileged-user-namespace attack
  surface to every program, which is what Ubuntu's default guards against.

When `bwrap` is missing or blocked, mini-agent's diagnostic (the startup or launch error, and the
logged preflight warning) names the cause, including the AppArmor restriction and its fixes. General subprocesses then follow the sandbox policy in
[Feature contract](#6-feature-contract), and the `js` tool is reported unavailable because it has
no uncontained fallback. The shell installer prints a warning when `bwrap` is not on `PATH`.

## 2. Setting up the provider

mini-agent defaults to **OpenRouter**, which gives access to hundreds of models through a single API key, without needing per-provider signup; OpenRouter also provides free models that can be used to complete this setup.

### Get an OpenRouter API key

1. Go to [openrouter.ai/keys](https://openrouter.ai/keys)
2. Create a key
3. Set it as an environment variable:

```bash
export OPENROUTER_API_KEY="sk-or-v1-..."
```

Add that line to `~/.bashrc` or `~/.zshrc` to make it permanent.

### Or use another provider

While we showed OpenRouter, you can set up all mainstream provider, local inference engines, and all OpenAI-compatible servers.

You can just set the matching env var with :

| Provider   | Env var               |
| ---------- | --------------------- |
| OpenRouter | `OPENROUTER_API_KEY`  |
| OpenAI     | `OPENAI_API_KEY`      |
| Anthropic  | `ANTHROPIC_API_KEY` (or `ANTHROPIC_AUTH_TOKEN` bearer) |
| Gemini     | `GEMINI_API_KEY`      |
| Ollama     | (none — local)        |

See the [canonical environment reference](CONFIG.md#environment-variables) for precedence,
endpoint fallbacks, application paths, ACP credentials and OS/internal controls.

Then, you can change your configuration file (`~/.mini-agent/config.toml`
on a fresh install; an existing legacy install keeps
`~/.config/zerostack/config.toml` on Linux,
`~/Library/Application Support/zerostack/config.toml` on macOS, or
`%APPDATA%\zerostack\config.toml` on Windows; `ZS_CONFIG_DIR` or
`MINI_AGENT_HOME` override it; see [CONFIG.md](CONFIG.md)) by adding
`provider = "provider_name"`.

`ZS_MODEL` selects a model through the same CLI field as `--model`. For
compatibility, `OPENROUTER_MODEL` is also accepted as the next fallback when
OpenRouter is the provider in effect, and then takes precedence over the
config-file model; it is ignored for other providers. Prefer `ZS_MODEL` for
provider-neutral configuration.

The config-file `model` is used only with the provider it belongs to (a quick
model's own provider, otherwise the configured `provider`). When `--provider`
or the config's `provider` selects a different one and no model is given,
that provider's default model is used instead of another provider's model id.

If you are using a provider that's not your default one, use the `--provider` CLI flag:

```bash
mini-agent --provider anthropic
```

See [Providers](PROVIDERS.md) for custom endpoints, header configuration, and prompt caching details.

## 3. Pick a Model

OpenRouter models use the format `provider/model-name`: [here](https://openrouter.ai/models?order=top-weekly) you can find the currently most used models, and [here](https://openrouter.ai/models?order=top-weekly&max_price=0) you can list only free models sorted by usage.

Models can be changed using the provider's model name via the `/model` command.

## 4. How to use Quick Models

Using model strings is cumbersome because they can be long and provider-specific. mini-agent therefore provides Quick Models, aliases that select both the model and provider.

A quick model can be added in the configuration by doing something like:
```
[quick_models.fast]
provider = "openrouter"
model = "deepseek/deepseek-v4-pro"
```

From there, you can use the `model` field in the configuration file to set the default model, or use `/models` to use an interactive picker directly in the agent.

## 5. Start a Session

You are now ready to launch mini-agent. Run `mini-agent`, type a message in the TUI, and press Enter for a streaming response.

It can read, write, edit, and search your codebase, while giving full control to the user on what it's allowed to do.

# Part 2. Useful commands

Now that you are in, you might want to be able to control the agent, and here's how:

## 1. Essential commands

By pressing `/` on an empty message, you can select any command to send the agent; here are the most useful commands:

| Command | What it does |
| ------- | ------------ |
| `/help` | List all commands |
| `/models <name>` | Switch model mid-session using Quick Models |
| `/clear` | Start with a fresh context |
| `/mode readonly` | Lock down to read-only |
| `/undo` | Undo the last exchange |
| `/redo` | Redo the last undo action |
| `/btw` | Ask a question to the agent without changing the context |
| `/review` | Ask the agent to review the last changes made |
| `/sessions` | List older sessions |
| `/rename <name>` | Rename the current session |
| `/quit` | Exit |

## 2. Prompts

Prompts change *how* the agent behaves. Type `.` at the start of a message to one: if it's followed by some text, the prompt will be used only for that message; if not, it will be set as the default for the rest of the session.

| Prompt | Use for |
| ------ | ------- |
| `code` | Writing and editing code (default) |
| `plan` | Designing before writing |
| `review` | Reviewing changes |
| `ask` | Q&A — no tools, just answers |
| `brainstorm` | Ideation, exploring options |
| `debug` | Systematic debugging |
| `refactor` | Restructuring existing code |

Try `.plan add a flag`. After a successful planning turn, mini-agent asks
`Execute this plan?`: **Y** selects the code prompt and starts an ordinary
turn with the exact approved plan; **N** keeps the plan prompt active;
**E** puts the plan in the editor. Enter previews an edited plan, and **Y**
is still required to execute it. Escape or Ctrl-C cancels approval. Planning
errors and interrupted turns never offer execution.

Approval preserves your last selected permission mode and configured deny
rules. In a read-only session, approving a plan keeps execution read-only.
Any tool approval and spending limit still apply. Queued follow-ups are held
while approval is pending and discarded with a visible notice when you decide;
typed drafts remain recoverable through Ctrl-Y. Approval is local to the
current session and workspace and is never restored from a saved session.

This is the short list; run `/prompt` in the agent for the full set of built-in prompts,
including `frontend-design`, `review-security`, `simplify`, `write-prompt`,
`autoconfig`, `orchestrator`, and `write-text`.

## 3. Autoconfig

Load the built-in `autoconfig` prompt (`.autoconfig`) when you want the agent to manage configuration for you. It is given the exact paths of your global config file, your project config file, and the installed documentation, and it proposes each change as a diff for your approval before editing.

`autoconfig` has no standing access to your configuration. It runs in the security mode you already chose, and the first time it needs your config directory mini-agent asks once whether to allow reading, editing, and listing that directory for this session. Decline and each access is approved individually instead.

## 4. Keybindings

Here is some keybindings to speed up your coding experience:

| Keys | Action |
| ---- | ------ |
| `Ctrl+R` | Cycle collapsed / full / off reasoning display; click a header to expand one block |
| `Ctrl+G` | Open input in `$EDITOR` |
| `Ctrl+O` | Launch `lazygit` |
| `Ctrl+P` / `Ctrl+N` | Move up/down in input; at its boundaries, navigate command history |
| `Ctrl+C` | Interrupt the agent |
| `Ctrl+Shift+C` | Copy selected text (Windows) |
| `Ctrl+V` | Paste Unicode clipboard text (Windows) |
| `PgUp` / `PgDn` | Scroll chat |
| `Home` / `End` | Start/end of the input line (empty input: jump to top/bottom) |
| `Ctrl+Home` / `Ctrl+End` | Jump to top/bottom of chat |
| `Shift+Enter` / `Alt+Enter` | Insert a newline |
| `/` | Open the command picker; Tab inserts the highlight, Enter runs a command or argument typed in full |
| `@<query>` | Open the file picker; Tab or Enter inserts the highlighted path, a space closes it |
| `Tab` | Insert two spaces when no picker is active |

## 5. CLI flags

If you want to use mini-agent from scripts or other programs, these CLI flags are useful:

| Flag | Action |
| ---- | ------ |
| `-p <msg>` | Sends a message |
| `--pure-stdout` | With `-p`, include tool calls and results on stdout rather than reserving stdout for the final answer. |
| `--max-budget-usd AMOUNT` | Finite nonnegative recorded-spend ceiling for the logical session, including auxiliary requests. Refused required continuation reports `budget` and exits nonzero; admitted work can exceed the estimate. See [COST_TRACKING.md](COST_TRACKING.md). |
| `--output-schema FILE` | With plain `-p`, validate an answer against a bounded offline JSON schema; default JSON envelope, at most two tool-free repairs. |
| `--output text\|json\|stream-json` | With `-p`, select the final answer (default), one JSON result, or streamed NDJSON events. Machine output conflicts with `--pure-stdout`. |
| `--quiet` | With `-p`, silence reasoning, stdin notices, and tool progress on stderr. Errors and operational warnings remain visible. Conflicts with `--pure-stdout`. |
| `-c`, `--continue` | Continue the most recent session saved for the current workspace. Use `--session` to select one from another workspace. If another running mini-agent process owns that session, continue in a forked copy under a new ID (with a notice) instead of overwriting its turns. |
| `-r`, `--resume` | Open a workspace-scoped session picker in a terminal. Tab explicitly toggles All workspaces, Enter resumes, and Esc cancels. Without a terminal, list bounded current-workspace rows and exit without provider work. |
| `--resume-all` | With `--resume`, include foreign workspaces. Selecting foreign history forks it into the current workspace without foreign approvals or hidden redo. |
| `--name <name>` | Name a new session or rename the selected session with `--continue`/`--session`. Renames are saved unless `--no-session` is set. |
| `--session <id-or-name>` | Load a session: an exact ID or exact (case-insensitive) name wins, otherwise a unique ID prefix or name substring. A session another running process owns is continued in a forked copy. |
| `--resume-provider <name>` / `--resume-model <id>` | Explicitly change provider/model while resuming saved context; the provider change displays and audits a privacy warning. |
| `--no-session` | Run ephemerally without saving a session or goal round records. |
| `--restrictive` | Ask for every operation. |
| `--read-only` | Only reads files |
| `--guarded` | Allow reads and ask for other operations. |
| `--allow TOOL:PATTERN` / `--deny TOOL:PATTERN` | Repeatable invocation-only permission seeds. Denies win; modes and external-directory safeguards still apply. Shell allows are exact scripts; other tools use permission globs. |
| `--allow-prefix SHELL:ARGV` | Repeatable simple shell argv-prefix grants, with the same restricted syntax as `permission-allow-prefix`. |
| `--accept-all` | Auto-allow operations inside the workspace while retaining the permission system. |
| `--yolo` | Allow operations except destructive shell commands, which still ask. |
| `--dangerously-skip-permissions` | Disable permission checks entirely. This is strictly broader than `--yolo`. |
| `--sandbox` | Explicitly require the platform general-process sandbox; fail if unavailable |
| `--no-sandbox` | Disable the default-on general-process sandbox |
| `--shell <path>` | Select Bash/sh, or PowerShell/pwsh on Windows, for explicit shell execution and the compatibility tool. |
| `--no-color` | Disable colored TUI output. |
| `--tutor` | Print the getting-started guide through the pager and exit. |
| `--loop`, `--loop-prompt`, `--loop-plan`, `--loop-max`, `--loop-run` | Configure the bounded headless iterative loop and optional validation command. |
| `--goal <objective>`, `--goal-done`, `--goal-check`, `--goal-max-rounds`, `--goal-continuation`, `--goal-replace` | Work toward a persistent objective across turns, with completion criteria and commands that must pass before it counts as met. See [GOALS.md](GOALS.md). |
| `--worktree <name>` | Run the agent inside a new git worktree. |
| `--parallel` | Run the agent inside a self-managed git worktree (Experimental) |
| `--wt-auto-merge`, `--wt-base-dir <path>` | Configure worktree merge-on-exit and its base directory. |
| `--status-socket <path>` | Send start/stop status messages to a Unix socket. |
| `--load-prompt <prompt>` | Use a specific prompt |

In default text mode, stdout contains the canonical final answer after the turn and its session
save settle. Intermediate assistant text and tool transcripts remain in the saved session;
bounded, terminal-safe tool summaries and reasoning go to stderr. `--pure-stdout` retains the
streamed assistant/tool transcript on stdout, with bounded tool previews.

For ordinary `-p` model prompts, redirected stdin is read through EOF before provider work,
up to 1 MiB of UTF-8. It is appended as explicitly untrusted reference material, with quoted
lines and fences that cannot be closed by the supplied text. For example:

```bash
git diff | mini-agent -p --quiet "Review this diff"
cat report.txt | mini-agent -p --output json "Summarize this report"
mini-agent -p --output stream-json "Update the README example"
```

Invalid UTF-8 and oversized input fail locally. Empty prompts fail before reading stdin;
`--loop`, ACP, interactive mode, and explicit `-p '!command'` do not consume it. A stalled stdin
reader uses the same interrupt and 10-second forced-stop grace as an active headless turn.

`--output stream-json` writes one JSON object per line: `system` (logical session ID/name,
provider/model, and `ephemeral`), `assistant_text`, `tool_call`, `tool_result`, `usage`, optional
`goal` updates, optional `file_mutation` records, and a final `result`. Calls and results share `id`; `provider_call_id` records
the provider identifier separately (its preview is capped at 512 bytes). Tool result events include `is_error`; text/arguments and
tool previews are limited to 8 KiB and mark truncation. JSON escapes terminal control and deceptive
Unicode characters while preserving decoded argument data. These events reflect observed SDK
ordering, including intermediate assistant text. Usage events contain observed per-request
`usage` deltas and per-round `cumulative` totals; the final result is authoritative for normalized
usage, compaction, cost, and persistence. Native edit/write mutation records retain the exact
invocation `id`, affected path, byte counts, and SHA-256 identities from the successful typed SDK
result; no path is reopened for output. Their final escaped `diff` JSON preview is capped at 8 KiB; larger diffs retain
identity/counts, omit rows, and set `truncated`; their path preview is capped at 1024 UTF-8 bytes
and marks `path_truncated` when shortened. Already-observed callbacks retain order even if the SDK result batch later fails; hooks that
abort before the mutation observer runs cannot provide a record. At most 32 records wait in the run-local queue; `file_mutation_gap`
reports `omitted_events` explicitly if a larger atomic batch overflows it. Text and final JSON
retain their existing schemas. Diagnostics stay on stderr. A terminal failure still
emits a final result and retains the existing nonzero exit status. Closing stdout stops output
while allowing owned work cleanup and session persistence to finish.

Runtime stdout and stderr writes preserve backpressure and event order. Consumers should drain
both pipes. If a consumer holds either sink open but stops reading, the pending write remains
owned; an interrupt closes admission to that sink, saves observed turn progress, and waits for
that write's cleanup. This includes tool/reasoning progress, verification and goal notices, and
runtime error reporting. Default tracing diagnostics inside an active headless turn use a
nonblocking queue capped at 32 records of 4 KiB, with one turn-owned drain worker; overflow is
reported as dropped diagnostics when delivery is possible, and cancellation drops pending logs.
Pre-provider startup/config/MCP notices, diagnostics outside an active turn, pre-runtime Clap,
and explicit file logging retain their existing I/O behavior. A second signal or the 10-second grace ends
the process with status 130. A kernel-blocked write cannot be canceled portably; it remains owned
until its exact join or forced process exit. Forced transport shutdown can leave a partial NDJSON
line. An interrupted output transport cannot promise delivery of a final record. This output path
does not change inherited blocking, append, async, or access modes.

Final JSON and NDJSON results include `session_id`, `session_name`, and `session_persisted`.
IDs identify the actual selected or forked session; `--no-session` still has a logical ID but
sets persistence false. Early input errors use null identity. The initial stream event describes
logical identity only; it makes no claim that the session has been saved.

`-p --output json` reserves stdout for exactly one JSON object when a turn result is available:

```json
{"session_id":"saved-session-id","session_name":"docs update","session_persisted":true,"result":"done","files_changed":["src/main.rs"],"tool_calls":{"total":2,"by_name":{"edit":1,"read":1}},"usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"cached_input_tokens":0,"cache_creation_input_tokens":0,"tool_use_prompt_tokens":0,"reasoning_tokens":0},"cost":0.00042,"stop_reason":"completed"}
```

The object is the first and only line on stdout. `usage` and `cost` cover the turn's requests and
an automatic compaction that ran before the turn. `result` is the text of the turn's final
completion: text the agent wrote before a tool call is saved as its own message in the session but
is not prepended to `result`, matching the interactive transcript. A goal round that ends with an
accepted `goal_report` and no closing text completes with an empty `result` rather than being
retried as an empty response.

`--output-schema FILE` enables a typed JSON answer in plain `-p` mode:

```bash
mini-agent -p --output-schema answer.schema.json "Extract the requested fields"
```

Without `--output`, this selects the JSON envelope. `--output stream-json` keeps
candidate text/tool/usage events in observed order; candidates may be invalid,
so consume only the final `result_json` as the validated value. The existing
`result` string, usage, cost, effects, and session identity remain present.
A valid answer adds `result_json`; exhausted validation emits `result_json: null`,
`stop_reason: "schema_failed"`, and a nonzero exit. Provider, cancellation,
persistence, and token-budget failures keep `stop_reason: "failed"`.
The initial agent retains its ordinary tools. Up to two additional, single-request
format repairs have no tools, skills, verification commands, child launchers, or
executable prompt/stop lifecycle hooks. The original turn keeps all its hooks.
Repairs preserve known generation/reasoning/privacy controls and owned OpenRouter
routing. OpenRouter `provider.data_collection` (`allow`/`deny`), boolean
`allow_fallbacks`/`require_parameters`, and `order`/`ignore`/`only` lists also survive;
lists are capped at 16 nonempty, control-free names of at most 128 UTF-8 bytes.
Malformed declared constraints, or known constraints on another protocol, fail
before an auxiliary request; unknown routing keys are dropped. Arbitrary
`extra_body` extensions, hosted tools, state/container
handles, request-message replacements, and raw format/token-cap overrides are
not forwarded. The repair token cap and any native schema format are host-owned.
Their requests consume the same remaining turn-token budget, including observed
auto-compaction usage. Automatic transport
or SDK follow-ups are disabled for repairs; a repair provider failure ends the run. Each rejected
answer and its observed usage are saved before a repair starts, preserving
original effects on cancellation; repairs do not repeat tools. `--no-session`
still disables every session save.

This initial scope rejects ACP, loops, operator `!commands`, explicit text or
pure-stdout output, and active goals (including a resumed goal), before any
model/effect work. Schema files compile before provider or session initialization.
Schema files must be regular UTF-8 JSON, at most 64 KiB. Validation supports the
validator's bundled standard drafts, bounded acyclic local JSON-pointer `$ref`s,
and linear regex syntax. External/file references, resource IDs, named/encoded
anchors, dynamic/recursive refs, and backtracking-only regex features are rejected;
no schema resource can be fetched. Schemas and answers have depth 32 caps;
schema nodes/reference expansion are capped at 512, answer nodes at 4,096,
and their validation-work product at 1,000,000. Answers must be exactly one JSON
value, at most 256 KiB; no Markdown stripping or coercion occurs. Format checks
are enabled, with unknown formats rejected. Over-limit answers cannot enter repair.

Native Anthropic formatting is enabled only at the built-in official endpoint,
without custom-provider entries, for `claude-opus-5-5`, `claude-sonnet-5-5`,
`claude-haiku-5-5`, `claude-opus-4-6`, and `claude-sonnet-4-6`. Its conservative
schema subset permits closed objects, basic scalar/array types, scalar enums and
constants, with at most 24 optional fields; other constraints use local validation
and tool-free repairs. Custom gateways receive no automatic native fields.
All responses are still validated locally, including native output.
The [Anthropic format contract](https://platform.claude.com/docs/en/build-with-claude/structured-outputs),
[OpenRouter routing/privacy contract](https://openrouter.ai/docs/guides/routing/provider-selection),
and [pinned Rust validator options](https://docs.rs/jsonschema/0.49.9/jsonschema/struct.ValidationOptions.html)
define the supported wire shape and offline/linear validation controls.
Auxiliary requests preserve typed OpenRouter `zdr` and `enforce_distillable_text`
constraints alongside the documented routing allowlist. Unsupported or malformed
provider preferences stop the auxiliary request; they are never silently removed.

In text mode, closing stdout early (for example `mini-agent -p ... | head -5`) no longer aborts
the run: output stops, the turn finishes, and the session is still saved.

`stop_reason` is `"completed"` on success and `"failed"` when the agent, explicit shell command,
or session save fails. Failed turns retain observed output, tool counts, changed files, usage,
and cost, and exit nonzero; completed tool records are saved unless `--no-session` is set or
persistence itself fails. Errors before a turn result is available can exit without JSON.
Runtime errors retain their specific cause on stderr; missing credentials and unknown
providers include setup guidance from provider initialization.

During headless agent turns (`-p` and `--loop`), Ctrl+C or Unix SIGTERM cancels provider
waits and active work, waits for registered tool and hook cleanup, then saves completed
tool records and observed usage before exiting nonzero. An interrupted `-p --output json`
turn reports `"stop_reason":"failed"` with its partial result. Completion verification
is interrupted through the same cleanup path. The wait after the first signal is bounded:
a second Ctrl+C/SIGTERM, or 10 seconds without the turn and its cleanup settling, saves
whatever the turn returned and exits with status 130 without
waiting for the stalled work.

`files_changed` combines workspace paths whose bounded Git status changed during the invocation
with `write` and `edit` targets visible in the provider transcript. It is empty when no
change can be observed (for example, a non-Git workspace changed only by an opaque shell command).
`cost` is the estimated cost of this invocation in US dollars using the resolved model prices; it
is zero when prices are unavailable. `usage.output_tokens` includes reasoning tokens for every
provider: Gemini reports its thinking tokens separately, so they are added to it (and still shown
in `usage.reasoning_tokens`), and the session totals and cost charge them the same way. Status and
diagnostic messages remain on stderr so scripts can
parse stdout directly. Headless runs use a non-interactive prompt: the agent states reasonable
assumptions and proceeds within its granted authority instead of waiting for clarification.

During every user turn, mini-agent also detects repetitive tool loops. The third identical failed
call is replaced with corrective feedback, later calls with the same tool and arguments are skipped
before execution, and a sixth alternating A/B edit is stopped before it can undo the prior edit
again. Interactive sessions show a yellow `loop guard` event; headless and subagent runs receive the
same corrective tool result. These attempts still consume the configured model-turn budget.

Offline policy probes—`--config-preservation-check`,
`--project-config-trust-check`, `--js-runtime-check`,
`--memory-editor-preservation-check`, `--resume-provider-safety-check`,
`--acp-authentication-check`, `--acp-permission-policy-check`, and
`--loop-verification-policy-check`—run one named self-check and exit. They are
intended for packaging and CI diagnostics rather than ordinary sessions.

## 6. Feature contract

Sandboxing and memory support are compiled into the default build. The general-process sandbox is
enabled by default: Linux uses `bwrap` when installed and trusted, supported macOS hosts use the
system-provided Seatbelt backend at `/usr/bin/sandbox-exec`, and Windows selects the attested
AppContainer backend (`restricted-token` remains a compatibility alias). Windows availability
requires its cached native production preflight. That probe has a five-second run deadline followed
by up to five seconds for whole-tree reaping and a fresh five-second profile/ACL recovery window.
Before a new probe, a separate five-second bounded sweep recovers exact private roots preserved by
an interrupted earlier process; malformed or active roots fail closed without deletion. Failure is cached and
remains closed unless `--no-sandbox` explicitly opts out. On Linux and macOS an implicitly selected
default backend that is unavailable (for example `bwrap` missing, or blocked by Ubuntu 24.04's
AppArmor user-namespace restriction) still starts, but visibly and with reduced trust: a notice is
printed on stderr (even with `RUST_LOG=off`), the TUI repeats it in the chat before the first turn
and keeps a red `sandbox:off` status-bar segment for the whole session, and the built-in auto-allows
for commands that run workspace code (`cargo build`/`test`/`check`/`clippy`/`fmt`, `pip list`,
`git status`) are withheld so those commands ask first. Install bubblewrap to restore containment,
pass `--sandbox` (or set `sandbox = true`) to refuse to start instead, or pass `--no-sandbox` to run
unsandboxed deliberately (which keeps the built-in allows). While sandboxing remains enabled, explicit `--sandbox`, `sandbox = true`, or selecting
a backend through `--sandbox-backend` or `sandbox-backend` remains fail-closed. This general
subprocess policy is distinct from the mandatory, stricter JavaScript worker containment below.

The default build and every pre-built release archive include the brokered
JavaScript engine (`js` feature). QuickJS runs only in a contained same-executable worker; the
parent retains permissions, external effects, persistence, and audit. Linux requires its real
empty-root `bwrap` preflight, validated macOS 26 hosts require the one-time-image Seatbelt denial
and guardian lifecycle preflight with typed `DeprecatedBestEffort` assurance, and Windows requires
a cached minimal LPAC/Job production attestation. Other macOS majors remain unavailable.
There is no in-parent or uncontained fallback. The `mini-agent-lite-*` release archives
are built with `--no-default-features` and omit JS and other default features
— use those only when you need a minimal binary without the JS runtime.

Each `js` call gets a fresh strict-mode runtime—variables and imports do not persist—and may use
top-level `await` even though the brokered host globals are synchronous. The tool description lists
their exact signatures and examples. Current per-call limits are 30 seconds, a 64 MiB JavaScript
heap, a 512 KiB JavaScript stack, a 64 KiB result, 256 effects, 1 MiB per file read/write, and 256
console records capped at 8 KiB each and 256 KiB total. Fetch request bodies are POST-only and
capped at 256 KiB; fetch responses and each spawn output stream are capped at 1 MiB. Use `read` or
`grep` for direct lookup and the shell for repository toolchains; use `js` to combine bounded reads
with parsing or aggregation in one call. Return strings, primitives, or accessor-free plain JSON
objects/arrays. For Date, Map, Set, class instances, accessors, sparse arrays, or objects containing
`undefined` or non-finite numbers, return `JSON.stringify(value)` (or another explicit string) so
result conversion stays predictable and does not implicitly execute object hooks.

For a support report, run `mini-agent --doctor` or enter `/doctor` in the TUI.
The report includes build identity, captured config-layer key sources (values
omitted), credential source, provider/model/context and pricing source, cached
sandbox/worker/integration status, application paths and terminal metadata.
`--doctor` works with missing or invalid configuration and requires no API key;
it creates no default config, repairs no permissions, makes no network request,
and runs no sandbox or worker preflight. Invalid configuration is reported by
error class without parser excerpts. `/doctor` also works during an agent turn
without queuing input, warming a model cache, or saving session history.

Fresh CLI status is explicitly unprobed. The TUI shows recorded manager state,
including cached LSP failures, rather than making liveness requests. CLI disk
sizes include only up to 128 immediate entries per directory, so nested files
or unreadable entries make them partial; the TUI does not scan disk. When LSP
is enabled, the CLI checks executable metadata for at most 32 effective servers
through 128 absolute PATH entries, without launching them or showing arguments
and environment values. Terminal protocol emission is reported separately from
confirmed terminal support. The embedded model snapshot currently carries no
catalogue date; the report identifies that limitation and its build fingerprint.
Review local paths and custom server/model names before sharing the report.
For build identity alone, `mini-agent --version` prints the version, fingerprint
and compilation target without initializing application storage.

`--print-config` displays JavaScript worker status as `not probed` when the tool is eligible;
it starts no worker and advertises no worker capabilities. This speeds up configuration display,
without changing executable digest checks or containment preflight for actual JavaScript use.

On Windows, ordinary startup evaluates worker status only when the `js` tool is
eligible. `--no-tools` and allowlists that omit `js` skip the worker check and learned-skill startup.
An eligible check creates or reuses a persistent AppContainer profile and may add a persistent
read/execute ACE for that
profile to a supported, user-owned installed executable. It has no automatic cleanup, ACL rollback,
or separate consent prompt. The local attestation does not test general host filesystem/network
denial; the delivered hosted canaries record those broader observations only for their reference
runner and do not prove identical ACL visibility on every Windows host.

Everything else above, including ACP editor integration, ships in the default build. A few extras are compiled in
only when you ask for them: lifecycle hooks (`--features hooks`), a
second-model advisor (`--features advisor`), image/PDF message attachments
(`--features multimodal`, or the equivalent `--features pdf` alias). PDF attachments
are sent as raw document messages to a provider that supports them; mini-agent
does not parse PDFs locally. Attachments must be regular files (or symlinks to
them), and the 20 MiB limit is enforced while reading, including if a file grows.
See the root [README](https://github.com/sebahrens/mini-agent) for what each one
does and how to enable it.

# Conclusions

Thanks for reading the *Get Started* guide until the end!

I hope that you enjoy mini-agent. You can discover more about its configuration and commands in the documentation, by asking the `autoconfig` prompt, or through the `/` interactive picker.

---

Cheers,
Giuseppe Della Vedova
