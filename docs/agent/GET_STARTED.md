---
description: "Get started with mini-agent: install the minimal Rust coding agent, configure an LLM provider, and run your first session."
---

# Part 1. Get Started

Thanks for picking up mini-agent. This guide covers installation, model setup, and the basic commands.

This tutorial is designed to work on any Linux, macOS and WSL environment; if you are using Windows, we recommned using WSL, as Windows support is not currently mantained.

## 1. Installation

You can install via Cargo or the checksum-verified shell installer. The shell installer requires a
complete release in the canonical repository:
```
curl -fsSL https://raw.githubusercontent.com/sebahrens/mini-agent/main/install.sh | bash
```

For Cargo, use:
```
cargo install mini-agent
```

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

Then, you can change your configuration file (`~/.local/share/zerostack/config.toml` on Linux/WSL or `~/Library/Application Support/zerostack/` on macOS, unless overridden by `$ZS_CONFIG_DIR` or an existing `~/.config/zerostack/` file — see [CONFIG.md](CONFIG.md) for the full precedence) by adding `provider = [provider_name]` in order to change your default provider.

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
| `Ctrl+H` | Launches `lazygit`
| `Ctrl+S` | Force-save session |
| `Ctrl+C` | Interrupt the agent |
| `PgUp` / `PgDn` | Scroll chat |
| `Home` / `End` | Jump to top/bottom |

## 5. CLI flags

If you want to use mini-agent from scripts or other programs, these CLI flags are useful:

| Flag | Action |
| ---- | ------ |
| `-p <msg>` | Sends a message |
| `-c` | Continues from last open session |
| `--name <name>` | Set a name for the new session |
| `--session <id-or-name>` | Load session by ID prefix or name |
| `--read-only` | Only reads files |
| `--yolo` | No limitations given to the agent |
| `--sandbox` | Explicitly require the platform general-process sandbox; fail if unavailable |
| `--no-sandbox` | Disable the default-on general-process sandbox |
| `--worktree` | Run the agent inside a git worktree (Experimental) |
| `--parallel` | Run the agent inside a self-managed git worktree (Experimental) |
| `--load-prompt <prompt>` | Use a specific prompt |

## 6. Feature contract

Sandboxing and memory support are compiled into the default build. The general-process sandbox is
enabled by default: Linux uses `bwrap` when installed and trusted, supported macOS hosts use the
system-provided Seatbelt backend at `/usr/bin/sandbox-exec`, and Windows selects the AppContainer
candidate (`restricted-token` remains a compatibility alias). That Windows candidate currently
reports unavailable pending native hosted attestation, so startup fails closed unless
`--no-sandbox` explicitly opts out. Other implicitly selected unavailable defaults warn and start
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

On Windows, ordinary startup and `--print-config` evaluate worker status. That check creates or
reuses a persistent AppContainer profile and may add a persistent read/execute ACE for that
profile to a supported, user-owned installed executable. It has no automatic cleanup, ACL rollback,
or separate consent prompt. The local attestation does not test general host filesystem/network
denial; broader canaries are pending hosted reference-runner evidence.

Everything else above ships in the default build. A few extras are compiled in
only when you ask for them: lifecycle hooks (`--features hooks`), a
second-model advisor (`--features advisor`), image/PDF message attachments
(`--features multimodal,pdf`), and ACP editor integration
(`--features acp`). See the root [README](https://github.com/sebahrens/mini-agent) for what each one
does and how to enable it.

# Conclusions

Thanks for reading the *Get Started* guide until the end!

I hope that you enjoy mini-agent. You can discover more about its configuration and commands in the documentation, by asking the `autoconfig` prompt, or through the `/` interactive picker.

---

Cheers,
Giuseppe Della Vedova
