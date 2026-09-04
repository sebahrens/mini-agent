---
description: "Get started with mini-agent: install the minimal Rust coding agent, configure an LLM provider, and run your first session."
---

# Part 1. Get Started

Thanks for picking up mini-agent. This guide covers installation, model setup, and the basic commands.

This tutorial applies to Linux, macOS, and Windows. The checksum-verified shell installer below
targets Linux and macOS; x86-64 Windows also has a release MSI. JavaScript actions expose the same brokered
feature contract on all three systems, while each platform uses and reports its own containment
assurance; see the repository README and architecture overview for those guarantees.

## 1. Installation

You can build from source or use the checksum-verified shell installer. The shell installer requires a
complete release in the canonical repository:
```
curl -fsSL https://raw.githubusercontent.com/sebahrens/mini-agent/main/install.sh | bash
```

On x86-64 Windows, download `mini-agent-windows-x64.msi` and
`MSI_SHA256SUMS` from the same GitHub release, verify the checksum, then open
the MSI. It installs per-user without elevation by default and side-loads the
bundled VSIX when VS Code is present. Managed machines can use:

```powershell
msiexec /i mini-agent-windows-x64.msi ALLUSERS=1 /quiet /norestart
```

For a source checkout, use:
```
git clone https://github.com/sebahrens/mini-agent.git
cd mini-agent
cargo install --path . --debug
```

Do not run `cargo install mini-agent`: the crates.io package with that name is an unrelated project.

The repository retains a Homebrew formula for package-channel compatibility, but no canonical
`sebahrens` tap is published yet, so this guide does not advertise a tap command that cannot work.

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
| OpenAI     | `OPENAI_API_KEY`      |
| Anthropic  | `ANTHROPIC_API_KEY`   |
| Gemini     | `GEMINI_API_KEY`      |
| Ollama     | (none — local)        |

Then, you can change your configuration file (`~/.config/zerostack/config.toml`
on Linux, `~/Library/Application Support/zerostack/config.toml` on macOS, or
`%APPDATA%\zerostack\config.toml` on Windows, unless overridden by
`ZS_CONFIG_DIR`; see [CONFIG.md](CONFIG.md)) by adding
`provider = "provider_name"`.

`ZS_MODEL` selects a model through the same CLI field as `--model`. For
compatibility, `OPENROUTER_MODEL` is also accepted as the next fallback—even
when another provider is selected—and takes precedence over the config-file
model. Prefer `ZS_MODEL` for provider-neutral configuration.

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
model = "deepseek/deepseek-v4-flash"
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

This is the short list; run `/prompt` in the agent for the full set of built-in prompts,
including `frontend-design`, `review-security`, `simplify`, `write-prompt`,
`autoconfig`, `orchestrator`, and `write-text`.

## 3. Autoconfig

There is one special prompt, called `autoconfig`, that has full access to your mini-agent configuration and the project's documentation. Load `autoconfig` when you want the agent to manage configuration for you.

## 4. Keybindings

Here is some keybindings to speed up your coding experience:

| Keys | Action |
| ---- | ------ |
| `Ctrl+R` | Toggle reasoning/thinking |
| `Ctrl+G` | Open input in `$EDITOR` |
| `Ctrl+H` | Launch `lazygit` |
| `Ctrl+C` | Interrupt the agent |
| `Ctrl+Shift+C` | Copy selected text (Windows) |
| `Ctrl+V` | Paste Unicode clipboard text (Windows) |
| `PgUp` / `PgDn` | Scroll chat |
| `Home` / `End` | Jump to top/bottom |
| `Shift+Enter` / `Alt+Enter` | Insert a newline |
| `@<query>` | Open the file picker; Tab or Enter selects |
| `Tab` | Insert two spaces when no picker is active |

## 5. CLI flags

If you want to use mini-agent from scripts or other programs, these CLI flags are useful:

| Flag | Action |
| ---- | ------ |
| `-p <msg>` | Sends a message |
| `--pure-stdout` | With `-p`, include tool calls and results on stdout rather than reserving stdout for the final answer. |
| `-c` | Continues from last open session |
| `-r`, `--resume` | List recent sessions for selection. |
| `--name <name>` | Set a name for the new session |
| `--session <id-or-name>` | Load session by ID prefix or name |
| `--resume-provider <name>` / `--resume-model <id>` | Explicitly change provider/model while resuming saved context; the provider change displays and audits a privacy warning. |
| `--no-session` | Run ephemerally without saving a session. |
| `--restrictive` | Ask for every operation. |
| `--read-only` | Only reads files |
| `--guarded` | Allow reads and ask for other operations. |
| `--accept-all` | Auto-allow operations inside the workspace while retaining the permission system. |
| `--yolo` | Allow operations except destructive shell commands, which still ask. |
| `--dangerously-skip-permissions` | Disable permission checks entirely. This is strictly broader than `--yolo`. |
| `--sandbox` | Explicitly require the platform general-process sandbox; fail if unavailable |
| `--no-sandbox` | Disable the default-on general-process sandbox |
| `--shell <path>` | Select Bash/sh, or PowerShell/pwsh on Windows, for explicit shell execution and the compatibility tool. |
| `--no-color` | Disable colored TUI output. |
| `--tutor` | Print the getting-started guide through the pager and exit. |
| `--loop`, `--loop-prompt`, `--loop-plan`, `--loop-max`, `--loop-run` | Configure the bounded headless iterative loop and optional validation command. |
| `--worktree <name>` | Run the agent inside a new git worktree. |
| `--parallel` | Run the agent inside a self-managed git worktree (Experimental) |
| `--wt-auto-merge`, `--wt-base-dir <path>` | Configure worktree merge-on-exit and its base directory. |
| `--status-socket <path>` | Send start/stop status messages to a Unix socket. |
| `--load-prompt <prompt>` | Use a specific prompt |

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
remains closed unless `--no-sandbox` explicitly opts out. Other implicitly selected unavailable defaults warn and start
unsandboxed. While sandboxing remains enabled, explicit `--sandbox`, `sandbox = true`, or selecting
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

On Windows, ordinary startup and `--print-config` evaluate worker status only when the `js` tool is
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
(`--features multimodal,pdf`). See the root [README](https://github.com/sebahrens/mini-agent) for what each one
does and how to enable it.

# Conclusions

Thanks for reading the *Get Started* guide until the end!

I hope that you enjoy mini-agent. You can discover more about its configuration and commands in the documentation, by asking the `autoconfig` prompt, or through the `/` interactive picker.

---

Cheers,
Giuseppe Della Vedova
