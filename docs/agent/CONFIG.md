---
description: "Full zerostack configuration reference: config file locations, providers, permissions, hooks, MCP, themes, and every available option."
---

# Configuration

zerostack reads an optional TOML, YAML, or JSON config from one canonical
configuration root. `ZS_CONFIG_DIR` overrides that root. Otherwise it is
`~/.config/zerostack` on Linux, `~/Library/Application Support/zerostack` on
macOS, and `%APPDATA%\zerostack` on Windows. Within that directory the filename
priority is `config.toml`, `config.yaml`, `config.yml`, then `config.json`.
If none exists, zerostack creates `config.toml` in that same directory.

`ZS_DATA_DIR` does not redirect configuration. Set both `ZS_CONFIG_DIR` and
`ZS_DATA_DIR` when a hermetic installation needs one physical root. A config
left under the former data-root location is migrated before config creation.
The source is retained for rollback.

**Project-local override**: if `.zerostack/config.toml` exists in the
current working directory, it is merged over the global config at startup.
Any subset of keys may be set — tables (e.g. `mcp_servers`, `quick_models`,
`api_keys`) merge per key, scalars and arrays replace the global value, and
keys absent from the local file keep their global values. If sensitive values
cannot be activated, startup prints a notice identifying the project config;
an ordinary benign merge is visible only in diagnostic logging.
Edits made by the setup UI and first-start prompts are applied as a structural
delta to the global file; unchanged project-local values are never copied into
global configuration.

```toml
# .zerostack/config.toml
model = "anthropic/claude-sonnet-4-5"
show_reasoning = true

[mcp_servers.local-fs]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
```

Project-local values are split by authority. Presentation, model, and bounded
resource settings such as `model`, `max_tokens`, `retry`, colors, and
compaction settings merge immediately. Every other top-level key—including
unknown future keys, executable/provider selections, MCP/LSP configuration,
and permission-mode changes—remains inert until the user approves the exact
canonical project path, config path, and SHA-256 of the file. Approval is
stored in the private state root and is invalidated by a content change or a
copied checkout. Headless and ACP startup never prompt; they apply only the
benign subset and print a notice for ignored sensitive settings.

Prompts and themes are loaded from multiple sources, with later sources
overriding earlier ones for same-named files:

**Prompts** (priority low to high):
1. Embedded at compile time
2. The platform data root's `zerostack/prompts/` (global, user-level)
3. `<startup-workspace>/.zerostack/prompts/` (project-level, highest priority)

**Themes** (priority low to high):
1. Embedded at compile time
2. The platform data root's `zerostack/themes/` (global, user-level)

`ZS_CONFIG_DIR` changes only the config root; prompts and themes continue to
use `ZS_DATA_DIR` or the platform data root.

Persistent storage uses separate platform roots:

| Content | Linux default | macOS default | Windows default |
| --- | --- | --- | --- |
| Config | `~/.config/zerostack` | `~/Library/Application Support/zerostack` | `%APPDATA%\zerostack` |
| Portable data | `~/.local/share/zerostack` | `~/Library/Application Support/zerostack` | `%APPDATA%\zerostack` |
| State, sessions, transcripts, logs | `~/.local/state/zerostack` | `~/Library/Application Support/zerostack/state` | `%LOCALAPPDATA%\zerostack\state` |
| Cache | `~/.cache/zerostack` | `~/Library/Caches/zerostack` | `%LOCALAPPDATA%\zerostack\cache` |
| Credentials | `<local-data>/credentials` | `<local-data>/credentials` | `%LOCALAPPDATA%\zerostack\credentials` |

The corresponding overrides are `ZS_CONFIG_DIR`, `ZS_DATA_DIR`,
`ZS_LOCAL_DATA_DIR`, `ZS_STATE_DIR`, `ZS_CACHE_DIR`, and
`ZS_CREDENTIALS_DIR`. Overrides must be absolute (a leading `~` is expanded).
zerostack never uses the current directory as a fallback for user-global
state.

The brokered JavaScript effect audit permits one active parent writer for its private state store
across the machine. A second parent cannot initialize JS auditing while that lock is held. The
first initialization success or failure is cached for the process lifetime; restart the blocked
parent to retry after the other process exits or the storage problem is repaired. Audit segments
rotate by size while retaining one fixed version-1 private target-correlation key. Key rotation is
not currently implemented.

On Windows, normal startup and `--print-config` evaluate JavaScript worker containment status only
when the `js` tool is eligible. `--no-tools` and a tool allowlist that omits `js` do not run this
preflight or initialize the learned-skill runtime. An eligible status preflight creates or reuses a
persistent AppContainer profile and may add that profile's
exact read/execute ACE to a supported, user-owned installed executable. These changes persist after
exit, and zerostack currently provides no automatic profile cleanup, ACL rollback, or separate
consent prompt. LPAC is not a filesystem namespace; host objects readable through applicable ACLs
can remain visible, while the broader filesystem/network/child canaries are hosted
reference-runner observations rather than local-attestation guarantees.

## Importing portable Agent Skills

Install one local Agent Skills directory containing `SKILL.md`, or one ZIP
archive containing a single skill tree:

```bash
mini-agent --import-agent-skill ./my-skill
mini-agent --import-agent-skill ./my-skill.zip
```

Imports are validated without executing bundled scripts. Trees are installed
by whole-tree digest below `<data-dir>/agent-skills/<name>/<digest>/`.
`allowed-tools` is retained as non-authoritative metadata and grants no tool
permission.

## Skill embeddings

The skill library ranks skills against your prompt using embedding vectors. The
`[embedding]` section selects where those vectors come from.

The `external` backend defaults to the **same OpenRouter endpoint and credential
the LLM already uses**, so enabling real embeddings needs only:

```toml
[embedding]
backend = "external"
```

That resolves to `https://openrouter.ai/api/v1/embeddings` with
`openai/text-embedding-3-small` (1536 dimensions), authenticated from
`OPENROUTER_API_KEY` — the same variable the OpenRouter LLM provider reads. Any
field can be overridden to point at a different OpenAI-compatible provider:

```toml
[embedding]
backend = "external"
base_url = "https://api.openai.com/v1"    # API root, not the full endpoint
model = "text-embedding-3-large"
api_key_env = "OPENAI_API_KEY"            # variable name, never the key itself
dimensions = 3072                         # required for any non-default model
timeout_secs = 30                         # optional, default 30
# model_revision = "2026-01-snapshot"     # optional; defaults to `model`

[embedding.headers]                       # optional extra request headers
X-Organization = "acme"
```

`dimensions` is inferred only for the default model. Any other model must state
its width, because a wrong value would silently mix incompatible vectors into an
index generation.

| Backend | Requires | Notes |
|---------|----------|-------|
| `deterministic` (default) | nothing | Offline hash projection. Builds and runs everywhere with no download and no network. Vectors are stable and well-formed but carry **no semantic meaning**, so retrieval quality is not comparable to a real model. |
| `external` | `base_url`, `model`, `api_key_env`, `dimensions` | Any OpenAI-compatible `/embeddings` endpoint. The practical choice for real embeddings without a local model. |
| `local` | the `skills-embed` build feature | Local ONNX inference of `BAAI/bge-small-en-v1.5` via `fastembed`, 384 dimensions. Fully offline after the first model download (~30 MiB). |

The API key is read from the environment variable named by `api_key_env`. It is
never written to config, never included in an error message, and redacted from
debug output; endpoint URLs are stripped from error text so a key passed as a
query parameter cannot reach a log.

Stored vectors are keyed by `(model_id, model_revision)`. Change `model_revision`
when the upstream model changes so existing vectors become ineligible instead of
being compared against incompatible new ones.

Model-authored learned-skill proposals are off by default. Enable the bounded
proposal and admission workers with a trusted global setting:

```toml
enable_skill_proposals = true
```

The setting is security-sensitive in project-local configuration and remains
inactive until that exact project config is content-bound and trusted. Enabling
it exposes `propose_skill` to model-authored JS only; stored skills never receive
proposal authority, and every proposal still needs held-out evaluation plus the
explicit local-owner approval and activation commands documented in
[Skills](SKILLS.md).

### Building the `local` backend

`skills-embed` pulls `ort-sys` (ONNX Runtime). On hosts it ships prebuilt
binaries for (Linux x86_64, arm64 macOS) plain `--features skills-embed` works.
On hosts it does not — notably `x86_64-apple-darwin` — use the
`skills-embed-dynamic` feature, which links ONNX Runtime at run time instead:

```bash
brew install onnxruntime
export ORT_DYLIB_PATH=$(brew --prefix onnxruntime)/lib/libonnxruntime.dylib
cargo test --features js,skills,skills-embed-dynamic
```

Selecting `local` without the `skills-embed` feature is a startup error rather
than a silent downgrade to `deterministic`.

## Config-file privacy

The global config can contain plaintext API keys, authorization headers, and
other credentials. Its containing configuration directory is therefore a
private persistence root:

- On Unix, zerostack creates configuration directories as `0700` and config
  files and atomic-write temporary files as `0600`, independent of the process
  umask. An existing current-user-owned real directory or regular file with
  broader mode bits is repaired through an opened handle before it is read or
  replaced.
- On Windows, Unix mode bits are not considered protection. The config
  directory, final file, and atomic-write temporary file receive a protected
  DACL with inheritance disabled. Full access is limited to the current user
  and `SYSTEM`; inherited `Everyone` and ordinary `Users` access is removed.
- A symbolic link, Windows reparse point, wrong file type, or path not owned by
  the current user is rejected. zerostack does not chmod or rewrite through
  such a path. Unsupported platforms fail closed instead of saving secrets
  with process-default permissions.

Atomic replacement publishes only a fully written private temporary file and
revalidates the resulting config file. Save and parse errors report the path
and error category but omit config source excerpts, which could contain stored
secrets. Config persistence currently creates no lock or backup file; any
future lock or backup artifact must use the same `0600`/protected-DACL policy.

## Session-storage privacy

Saved sessions and full tool outputs can contain prompts, source code, command
output, authorization data, and other secrets. zerostack applies the same
private-persistence policy to the state root's `sessions/` and
`tool-outputs/` trees:

- On Unix, newly created directories are `0700`; session, tool-output, lock,
  and atomic-write temporary files are `0600` from creation, independent of
  umask. Before reading or replacing an existing path, zerostack repairs a
  current-user-owned real directory or regular file to the exact private mode.
- On Windows, Unix mode bits do not provide protection. Directories and every
  final, lock, and temporary file receive a protected DACL from creation time,
  with full access limited to the current user and `SYSTEM`. Inherited access
  for `Everyone` and ordinary `Users` is removed.
- Symbolic links, Windows reparse points, wrong file types, and paths not owned
  by the current user are rejected without chmod, DACL repair, replacement, or
  deletion through the unsafe path. Unsupported platforms fail closed.

Session replacement publishes only a fully written private temporary file.
Failure cleanup removes a temporary file only after confirming that it is the
same file created by the failed write. Session saves are currently lock-free;
any lock artifact added later must continue to use the private storage helper.

On startup, known legacy content is copied through a private, no-follow,
content-verified migration and the original is retained. Identical candidates
are safe to converge. Differing candidates require an explicit numbered
selection in an interactive startup. Headless and ACP startup never select:
a config conflict stops startup, while an optional feature conflict disables
only that feature. To recover non-interactively, compare the reported files,
move the intended source to the reported canonical path, then restart; do not
delete the retained legacy tree until rollback is no longer needed.

All config keys are optional. CLI flags and their environment-backed values
(such as `ZS_PROVIDER` and `ZS_MODEL`) take precedence where both exist.

Example (YAML):

```yaml
provider: openrouter
model: deepseek/deepseek-v4-flash
max_tokens: 16384
temperature: 0.7
context_window: 128000
reserve_tokens: 8192
keep_recent_tokens: 10000
compact_enabled: true
mid_turn_compact_threshold: 0.80
deny_repeated_reads: false
default_prompt: code
default_permission_mode: standard
permission-modes: ["guarded", "standard", "yolo"]
show_tool_details: 3
sandbox: false

quick_models:
  fast:
    provider: openai
    model: gpt-4o-mini
custom_providers:
  local-vllm:
    provider_type: openai
    base_url: http://localhost:8000/v1
    api_key_env: VLLM_API_KEY
    model: gemma4
  company-gateway:
    provider_type: openai
    base_url: https://gateway.example.com/v1
    api_key_env: GATEWAY_API_KEY
    api_style: completions
    headers:
      cf-access-client-id: "${CF_ACCESS_CLIENT_ID}"
      cf-access-client-secret: "${CF_ACCESS_CLIENT_SECRET}"
    danger_accept_invalid_certs: false
    timeout_secs: 60
permission:
  "*": ask
  read: allow
  write:
    "**/*.rs": allow
    "**": ask
  bash:
    "cargo test": allow
    "rm **": deny
  external_directory:
    "/tmp/**": allow
    "/**": ask
  doom_loop: ask
```

The same config in TOML:

```toml
provider = "openrouter"
model = "deepseek-v4-flash"
max_tokens = 16384
temperature = 0.7
context_window = 128000
reserve_tokens = 8192
keep_recent_tokens = 10000
compact_enabled = true
mid_turn_compact_threshold = 0.80
edit_system = "similarity"
default_prompt = "code"
default_permission_mode = "standard"
permission-modes = ["guarded", "standard", "yolo"]
show_tool_details = 3

[quick_models.fast]
provider = "openai"
model = "gpt-4o-mini"

[custom_providers.local-vllm]
provider_type = "openai"
base_url = "http://localhost:8000/v1"
api_key_env = "VLLM_API_KEY"

[permission]
"*" = "ask"
read = "allow"

[permission.write]
"**/*.rs" = "allow"
"**" = "ask"

[permission.bash]
"cargo test" = "allow"
"rm **" = "deny"

[permission.external_directory]
"/tmp/**" = "allow"
"/**" = "ask"

permission.doom_loop = "ask"
```

### Completion verification

Set a trusted project quality command to prevent a tool-using turn from being
reported as complete before that command passes:

```toml
verify_command = "cargo test"
verify_timeout_secs = 600
verify_max_attempts = 3
```

After the agent invokes a potentially mutating tool, zerostack runs this command
in the startup workspace through the configured general sandbox. Known
read-only tools do not trigger the gate; unknown tools are treated
conservatively as potentially mutating. A failed attempt sends a sanitized,
bounded tail of stdout/stderr back to the model and allows it to repair the
problem within both `verify_max_attempts` and the remaining
`max_agent_turns`. Exhausting either bound fails the turn instead of emitting a
successful completion. Interactive TUI and ACP clients receive verification
status events; headless mode writes the status and diagnostics to stderr.

`verify_command` is executed by a shell and therefore carries the authority of
the user who configured it. It is a sensitive project-local setting: an
untrusted `.zerostack/config.toml` cannot activate it in TUI, headless, or ACP
mode.

Accepted top-level keys:

| Key                       | Type    | Description                                                                                                                                                                 |
| ------------------------- | ------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `provider`                | string  | Provider name. Built-ins are `openrouter`, `openai`, `anthropic`, `gemini`/`google`, and `ollama`; custom provider aliases are also accepted. Default: `openrouter`.        |
| `model`                   | string  | Model name. Default: `deepseek/deepseek-v4-flash`.                                                                                                                          |
| `max_tokens`              | integer | Maximum tokens for a single model response (the per-request output cap sent to the provider). Default: `16384`. This never limits a whole turn; see `turn_token_budget`.     |
| `turn_token_budget`       | integer | Optional cumulative fail-closed cap for one agentic turn: the sum of input+output tokens across every completion call the turn makes. Unset by default (no cap; turns are still bounded by `max_agent_turns`). Deliberately separate from `max_tokens` — a multi-tool-call turn legitimately accumulates many responses' worth of prompt tokens. |
| `max_agent_turns`         | integer | Maximum agent turns per response. Default: `200`.                                                                                                                           |
| `verify_command`          | string  | Optional trusted command that must pass before a turn which invoked a potentially mutating tool may complete. Unset or blank disables the gate. Project-local values are sensitive and require content-bound trust. |
| `verify_timeout_secs`     | integer | Wall-clock timeout for each `verify_command` attempt. Default: `300`; clamped to `1..=3600`. |
| `verify_max_attempts`     | integer | Maximum verification attempts in one agent turn, including the first check. Default: `3`; clamped to `1..=8` and also bounded by the remaining `max_agent_turns` budget. |
| `temperature`             | number  | Model temperature (`0.0` to `2.0`). Precedence is `--temperature`, then the active quick model's value, then this global value; values are clamped to the supported range. |
| `extra_body`              | object  | Provider-specific JSON shallow-merged into every completion request body as a global default (e.g. OpenRouter `plugins` routing presets). A matching `quick_models` entry's `extra_body` overrides this. See Provider-specific request body parameters below. |
| `retry`                   | object  | Retry policy with `max_attempts` (default `3`), `initial_backoff_ms` (`500`), and `max_backoff_ms` (`10000`). The bound applies independently to each provider completion call in a tool-using turn; transient failures resume from preserved interactions and do not replay completed tools. |
| `no_tools`                | boolean | Disable all tools. Default: `false`.                                                                                                                                        |
| `no_context_files`        | boolean | Disable loading global/project `AGENTS.md`, `CLAUDE.md`, and `ARCHITECTURE.md` (if `archmd` feature enabled) context files. Default: `false`.                               |
| `context_window`          | integer | Session context-window size used for status and auto-compaction. When unset, auto-detected from the selected model's catalog entry; falls back to `128000` if the model is not in the catalog. A value of `0` disables auto-compaction. |
| `reserve_tokens`          | integer | Tokens to reserve before compaction is triggered. When unset globally, falls back to the active quick model's `reserve_tokens` field, then to a default that scales with the context window: `window/10`, never below `16384` (so one maximal response cannot overshoot the window) and never above half the window. Examples: 128k window → 16384, 1M window → 100000. |
| `keep_recent_tokens`      | integer | Approximate recent-token budget kept verbatim during compaction. When unset, scales with the context window: `window/20` clamped to `[10000, 50000]` and at most a quarter of the window. Examples: 128k window → 10000, 1M window → 50000.                          |
| `max_text_file_size`      | integer | Maximum allowed file size in bytes for read/write tool operations. Default: `1048576` (1 MB).                                                                               |
| `max_read_lines`          | integer | Default maximum lines returned by one `read` call. Default: `2000`. |
| `max_bash_output_lines`   | integer | Line cap for shell tool output returned to the model, applied to successful output and to the partial output embedded in timeout/output-limit errors. Longer output keeps its head and tail around an `[... N lines omitted ...]` marker. Default: `2000`. Set `0` to disable line truncation (the 1 MiB per-stream / 1.5 MiB combined byte limits still apply). |
| `max_grep_results`        | integer | Maximum grep matches returned to the main agent. Default: `150`. |
| `max_find_results`        | integer | Maximum file matches returned to the main agent. Default: `150`. |
| `max_list_dir_entries`    | integer | Maximum directory entries returned to the main agent. Default: `150`. |
| `subagent_max_read_lines` | integer | Per-subagent read-line cap. Default: `2000`; requires `subagents`. |
| `subagent_max_grep_results` | integer | Per-subagent grep-result cap. Default: `200`; requires `subagents`. |
| `subagent_max_find_results` | integer | Per-subagent file-result cap. Default: `200`; requires `subagents`. |
| `subagent_max_list_dir_entries` | integer | Optional per-subagent directory-entry cap; requires `subagents`. |
| `deny_repeated_reads`     | boolean | Block repeated reads of the same canonical file section within one logical session until that session edits or writes the target. Agent rebuilds retain that session's history; concurrent UI, ACP, BTW, and subagent sessions keep independent settings and histories. Default: `true`. Set to `false` to allow re-reading. |
| `show_cost_always`        | boolean | Show the session cost in the status bar even when it is `$0.0000` (for example when the model has no per-token pricing configured). Default: `false`, which hides the cost until it is above zero. |
| `compact_enabled`         | boolean | Master switch for all automatic conversation compaction (between ordinary TUI turns, between `/loop` iterations, before an over-budget resumed headless `-p` request, and opt-in mid-turn compaction). Default: `false`. When `false`, nothing is ever compacted automatically.            |
| `mid_turn_compact_threshold` | number | Opt-in mid-turn compaction. Fraction of the context window (`0.0`–`1.0`) of real provider prompt pressure at which to compact *during* a turn, not just between turns. Unset by default, meaning no mid-turn compaction. Honored only when `compact_enabled` is `true`. Recommended starting value: `0.80`. See Mid-turn compaction below.            |
| `always_show_welcome`     | boolean | Always show the welcome banner on startup, bypassing the one-shot marker file. Default: `false`.                                                                               |
| `auto-update-prompts`     | boolean | When `true`, always regenerate prompts on version change without asking. When `false`, never regenerate. When unset, asks interactively.                                         |
| `auto-update-themes`      | boolean | When `true`, always regenerate themes on version change without asking. When `false`, never regenerate. When unset, asks interactively.                                         |
| `edit_system`             | string  | Edit system mode: `"similarity"` (SEARCH/REPLACE with fuzzy matching, default) or `"hashedit"` (CRC-32 tag-based CAS edits). See Edit System Modes below.                     |
| `custom_providers`        | object  | Map of provider aliases to `{ "provider_type", "base_url", "api_key_env", "api_style", "headers", "danger_accept_invalid_certs", "timeout_secs" }`. `provider_type` must resolve to a built-in provider type; `api_key_env` is optional. For OpenAI providers, `api_style` selects `"responses"` or `"completions"`, `headers` sets custom HTTP headers (values support `${ENV_VAR}` expansion), and `timeout_secs` overrides the HTTP timeout. `danger_accept_invalid_certs` disables TLS verification. See the OpenAI API styles section below. |
| `embedding`               | object  | Skill-retrieval embedding backend and model settings. See Skill embeddings above. |
| `enable_skill_proposals`  | boolean | Expose bounded `propose_skill` authority to model-authored JS and start the session proposal/admission workers. Default: `false`; project-local values require content-bound trust. |
| `permission`              | object  | Permission rules using glob patterns; see the permission config notes below.                                |
| `permission-regex`        | object  | Same structure as `permission` but patterns are interpreted as regex instead of glob.                       |
| `permission-allow`        | object  | Map of tool names to lists of glob patterns to allow. Works alongside the `permission` field. See below.    |
| `permission-ask`          | object  | Map of tool names to lists of glob patterns to prompt on. Works alongside the `permission` field. See below.|
| `permission-deny`         | object  | Map of tool names to lists of glob patterns to deny. Works alongside the `permission` field. See below.     |
| `restrictive`             | boolean | Select restrictive permission mode (ask for every operation). Overridden by `accept_all`/`yolo` if those are also true.                                                     |
| `accept_all`              | boolean | Select standard permission mode with auto-allow within CWD (equivalent to `default_permission_mode = "standard"`). Overridden by `yolo` if true.                            |
| `yolo`                    | boolean | Select yolo mode (allow all, ask for destructive bash commands).                                                                                                            |
| `permission-modes`        | array   | List of mode names that apply configured `allow` and `ask` rules. Default: `["guarded", "standard", "yolo"]`. Configured `deny` and `external_directory` deny rules are security baselines and remain active in every mode. |
| `sandbox`                 | boolean | Enforce the configured **general subprocess** sandbox for Bash and parent-brokered JS `spawn`. Default: `true`. Precedence is `--no-sandbox` (disable) > `--sandbox` (explicitly require) > this config value > the default. On non-Windows hosts, an unavailable backend inherited only from the default warns and runs unsandboxed. While sandboxing remains enabled, `--sandbox`, `sandbox = true`, or selecting a backend through the CLI/config fails closed if that backend is unavailable. This setting never disables the mandatory broker-only JS worker containment. |
| `sandbox-backend`         | string  | General-process backend. Defaults to `bwrap` on Linux, the system-provided `seatbelt` at `/usr/bin/sandbox-exec` on supported macOS hosts, and `appcontainer` on Windows (`restricted-token` is a compatibility alias). Setting this key or passing `--sandbox-backend` makes an enabled sandbox request explicit and fail-closed. Windows availability requires the cached native AppContainer production preflight. Before a new probe, a separate five-second bounded sweep recovers exact private roots preserved by interrupted earlier preflights. The new run phase is limited to five seconds, whole-tree reaping receives up to five seconds, and profile/ACL recovery then receives a fresh five-second ceiling. Failure remains closed unless `--no-sandbox` explicitly opts out. The backend adds package-SID workspace read/write plus read/execute grants for the application cache, exact selected executable, and explicitly configured AppContainer roots; ambient `PATH`, home, Cargo, and Rustup roots are never inferred. As a regular AppContainer it retains standard Windows system resources and any pre-existing object accessible to `ALL APPLICATION PACKAGES`; such an existing ACL can include write authority, so universal filesystem isolation is not claimed. It uses private profile storage, grants no network capability, and retains the private desktop plus bounded creation-time Job. Hosted observations describe the reference runner, not every host's ACL visibility; broader registry/device/session isolation is not claimed. `zerobox` is explicit and backend-defined. None of these profiles launches or describes the broker-only JS worker. |
| `windows-appcontainer-read-roots` | array of paths | Additional Windows AppContainer read/execute roots. Relative paths resolve from the workspace. Zero roots is the safe default. These values are ignored by non-AppContainer backends and rejected if they are remote, reparse-based, multiply linked, overlap a writable root, or contain the private AppContainer control sibling. Conflict diagnostics expose only fixed root roles and containment direction, never paths. CLI: repeat `--windows-appcontainer-read-root PATH`. |
| `windows-appcontainer-write-roots` | array of paths | Additional Windows AppContainer read/write roots. Relative paths resolve from the workspace. Zero roots is the safe default. These values are ignored by non-AppContainer backends and rejected if they overlap the read-only cache/configured roots, another writable root, or the private AppContainer control sibling. Deterministic conflicts are rejected before profile/journal creation. CLI: repeat `--windows-appcontainer-write-root PATH`. |
| `js-fetch-origins`        | array   | Exact origin narrowing list for the sandbox-gated JS `fetch()` global, for example `["https://docs.rs", "https://api.example.com:8443"]`. Absent leaves narrowing to permissions; empty or malformed denies all fetches. |
| `js-fetch-allow-http`     | boolean | Permit public-address HTTP origins for JS `fetch()` in addition to HTTPS. Default: `false`. Private, loopback, link-local, metadata, multicast, and reserved destinations remain denied. |
| `js-file-base-dir`        | path    | Base used to resolve relative JS file roots. Relative values resolve from the captured startup workspace; absent uses that workspace directly. |
| `js-read-roots`           | array   | Explicit read roots for brokered JS file effects, resolved from `js-file-base-dir`. |
| `js-write-roots`          | array   | Explicit write roots for brokered JS file effects, resolved from `js-file-base-dir`. |
| `js-read-unrestricted`    | boolean | Explicitly allow brokered JS reads outside configured roots; cannot be combined with `js-read-roots`. Default: `false`. |
| `js-write-unrestricted`   | boolean | Explicitly allow brokered JS writes outside configured roots; cannot be combined with `js-write-roots`. Default: `false`. |
| `default_permission_mode` | string  | Permission mode when no mode boolean/CLI flag is set. Accepts: `standard` (default), `restrictive`, `readonly`, `planwrite`, `guarded`, `yolo` (`accept` is an alias for `standard`). Any other value is rejected at startup with the list of accepted values. |
| `show_tool_details`       | boolean or integer | Show tool-result previews in the TUI. `false` hides output, `true` shows all lines, an integer limits to that many lines (e.g. `3`). Default: `3`. |
| `show_reasoning`          | boolean | Show streamed reasoning text in the TUI. Can still be toggled at runtime with `Ctrl+R` or `/reasoning`. Default: `false`. |
| `statusline`              | table   | Configurable status bar (up to 3 lines of colored segments). When absent, a built-in default layout is used. See Status bar below. |
| `chat_left_margin`        | integer | Left padding (columns) for the chat area only; input and status rows are unaffected. Default: `0`. |
| `default_prompt`          | string  | Prompt name to activate on startup. Default: `code`. If the prompt file has a `%%mode=<mode>` first-line directive, the security mode is set automatically (see Prompt directives below). |
| `wt-auto-merge`           | boolean | Automatically merge a CLI-created worktree on exit; requires `git-worktree`. Default: `false`. |
| `wt-base-dir`             | path    | Base directory for CLI-created worktrees; requires `git-worktree`. |
| `shell`                   | string  | Shell executable for the model-visible `shell` compatibility tool and explicit shell commands. Unix accepts Bash/sh; Windows also accepts PowerShell/pwsh. |
| `editor`                  | string  | Editor command for `Ctrl+G` (default: `$EDITOR` env var, then `editor`, then `nano`).                                                                                        |
| `api_keys`                | object  | Map of provider names to API keys (e.g. `"openai": "sk-..."`). Used as fallback when the corresponding env var is not set. Custom providers are isolated: an entry named `local-vllm` only consults `api_key_env` and `api_keys["local-vllm"]`, never `OPENAI_API_KEY` or `api_keys["openai"]`, so a vendor key is never sent to a third-party `base_url`. |
| `quick_models`            | object  | Map of quick-model names to `{ "provider", "model", "reserve_tokens"?, "input_token_cost"?, "output_token_cost"?, "temperature"?, "extra_body"? }`. Can be switched with `/models <name>` or `--quick-model=<name>`. See Provider-specific request body parameters below for `extra_body`. |
| `prompt_to_model`         | object  | Map of prompt names to quick-model names (e.g. `plan = "glm-52"`). When switching to a prompt, zerostack automatically switches to the corresponding quick model. Empty-string values are treated as "no change". See Prompt-to-model switching below. |
| `mcp_servers`             | object  | MCP server map when compiled with the `mcp` feature. When omitted, recommended MCPs are auto-configured (see below).                                                   |
| `enable-exa-mcp`          | boolean | Auto-configure the Exa Web Search MCP server. Default: `true`.                                                                                                         |
| `enable-context7-mcp`     | boolean | Auto-configure the Context7 MCP server. Default: `false`.                                                                                                              |
| `enable-grepapp-mcp`      | boolean | Auto-configure the Grep.app MCP server. Default: `false`.                                                                                                              |
| `allow_all_mcp_calls`     | boolean | When `true`, permission checks are skipped for all MCP tool calls. Default: `false`.                                                                                   |
| `mcp_tool_timeout_secs`   | integer | Bound on one MCP `tools/call` round trip, in seconds. A call that exceeds it is cancelled and reported to the model as a tool error. Default: `120` (minimum `1`).       |
| `acp_servers`             | object  | ACP server config map when compiled with the `acp` feature. See the ACP section below.                                                                                       |
| `acp_host`                | string  | TCP bind host for ACP server mode (equivalent to `--acp-host`).                                                                                                              |
| `acp_port`                | integer | TCP bind port for ACP server mode (equivalent to `--acp-port`, default: 7243).                                                                                               |
| `task_max_turns`          | integer | Maximum model turns per subagent. Default: `20`; requires `subagents`. |
| `task_max_prompts`        | integer | Maximum prompts in one `task` call. Default: `8`; requires `subagents`. |
| `task_max_concurrency`    | integer | Maximum concurrently polled child futures. Default: `4`; requires `subagents`. |
| `task_max_output_bytes`   | integer | Aggregate rendered output cap for a `task` call. Default: `262144`; requires `subagents`. |
| `task_max_cost_units`     | integer | Aggregate reported token/cost-unit cap for a `task` call. Default: `500000`; requires `subagents`. |
| `task_timeout_secs`       | integer | Whole-call subagent deadline. Default: `300`, maximum `86400`; requires `subagents`. |
| `task_enabled`            | boolean | Register the `task` tool when subagents are compiled in. Default: `true`. |
| `subagent_model`          | string  | Raw model id or quick-model alias used by subagents; absent inherits the main model. |
| `subagent_provider`       | string  | Provider used by subagents when not supplied by a quick-model alias; absent inherits the main provider. |
| `chain`                   | object  | Chain-of-prompts configuration. See Chain-of-Prompts below. |
| `lsp`                     | object  | Language-server configuration; requires `lsp`. See LSP below. |
| `advisor`                 | object  | Advisor configuration; requires `advisor`. See Advisor below. |
| `colors`                  | object  | Background color overrides for the TUI. See the colors section below.                                                                                                       |

JavaScript worker containment is a runtime prerequisite, not a user-selected sandbox mode.
`--print-config` reports its backend, assurance, and availability separately from the general
subprocess backend. Linux becomes available only after the real empty-root `bwrap` preflight;
validated macOS 26 hosts require the one-time-image Seatbelt denial and guardian lifecycle
preflight and report typed `DeprecatedBestEffort` assurance; Windows requires a cached minimal
LPAC/Job production attestation. Other macOS majors remain unavailable. No unavailable worker
backend falls back to in-parent or uncontained JavaScript.

## System Prompt Suffix (`SUFFIX.md`)

You can append custom text to **every** system prompt by creating a
`SUFFIX.md` file in the config directory (same location as `config.toml`):

- Linux: `~/.config/zerostack/SUFFIX.md` (or `$ZS_CONFIG_DIR/SUFFIX.md`)
- macOS: `~/Library/Application Support/zerostack/SUFFIX.md`

If the file exists and contains non-whitespace content, its contents are
appended at the very end of the system prompt preamble — **after** AGENTS.md,
ARCHITECTURE.md, the active prompt, working directory, `/add`ed files,
memory, and everything else. A `---` separator is inserted automatically.

The suffix applies to **all** agent contexts:

- The main interactive agent
- Subagents (parallel task delegation)
- The advisor tool (second-model consultation, if the `advisor` feature is enabled)
- The conversation summarizer (compaction)
- `/btw` side questions

If the file is missing, empty, or whitespace-only, nothing is appended.

Use cases: inject persistent rules, style preferences, team-wide policies,
or provider-specific output formatting that should always be present
regardless of which prompt or mode is active.

## Hooks

Requires the `hooks` Cargo feature, which is **default-off** — a prebuilt
binary or package must have been compiled with `--features hooks` (or
`--all-features`) for any of this to apply. When the feature isn't compiled
in, none of the flags, files, or `/hooks`/`--hooks-test` commands below exist.

Hooks let external commands observe or gate agent behavior at defined points
(a tool call, a user prompt, the agent finishing a turn, a session
starting/ending, a subagent starting/stopping), using the same
`settings.json` shape, stdin envelope, and exit-code/stdout-JSON contract as
Claude Code, so an existing CC hooks setup is largely compatible (see the
`$CLAUDE_PROJECT_DIR` caveat below for the one script-level change some
setups need).

### Config file locations and precedence

Hook config lives in a `settings.json` (JSON, not `config.toml`/`.yaml`) at up
to three locations, loaded and merged in this order:

| Location | Trust |
| -------- | ----- |
| `~/.config/zerostack/settings.json` (global; on macOS `~/Library/Application Support/zerostack/settings.json`; on Windows `%APPDATA%\zerostack\settings.json`, experimental) | Trusted by default |
| `.zerostack/settings.json` (project, relative to CWD) | **Not** trusted by default — see Trust model below |
| `/etc/zerostack/managed-settings.json` (Linux) / `/Library/Application Support/zerostack/managed-settings.json` (macOS) / `C:\ProgramData\zerostack\managed-settings.json` (Windows, experimental) — admin-controlled | Always trusted; unaffected by `disableAllHooks` |

Each file may have a top-level `hooks` object (keyed by event name) and a
top-level `disableAllHooks: true` boolean. `disableAllHooks` (from the global
or project file) or the `--no-hooks` CLI flag suppresses every non-managed
hook; managed hooks still run regardless. A missing or invalid file is not an
error — it just contributes nothing.

**Largely compatible with Claude Code's `.claude/settings.json`**: zerostack
does not read that file directly, but its own `settings.json` uses the same
basic `hooks` schema. For security, every command handler must add an `args`
array (use `[]` when the executable takes no arguments); zerostack never
implicitly passes `command` to a shell. Scripts may also need a change because
zerostack sets `$ZEROSTACK_PROJECT_DIR` rather than `$CLAUDE_PROJECT_DIR`.

### Handler schema

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash|Write",
        "hooks": [
          {
            "type": "command",
            "command": "./guard.sh",
            "args": [],
            "timeout": 30,
            "trust": "sandboxed",
            "env": { "GUARD_MODE": "strict" }
          }
        ]
      }
    ]
  }
}
```

| Field | Type | Description |
| ----- | ---- | ----------- |
| `type` | string | Only `"command"` is supported. |
| `command` | string | Executable to run directly. Relative paths such as `./guard.sh` resolve from the canonical selected workspace. Receives the stdin envelope as JSON; `$ZEROSTACK_PROJECT_DIR` is set to that same directory. To use a shell intentionally, set this to the shell executable and pass the script in `args`. |
| `args` | array of strings | Required, but may be empty. Passed directly as the executable's argv with no shell metacharacter expansion. |
| `timeout` | integer (seconds) | Per-hook timeout; the whole process group is killed on expiry. Default: 60. |
| `async` | boolean | When `true`, the hook's `if` condition and handler run in the background and its decision is ignored. Agent-turn work owns the background task, so turn cancellation still terminates and reaps it before the turn settles. Default: `false`. |
| `if` | string | A shell command evaluated (with the same stdin envelope) before the handler runs; the handler only runs if it exits `0`. Fails closed: a broken/unparseable/timed-out condition still runs the handler, with a warning. |
| `once` | boolean | Runs the handler at most once per event per session. A false `if` condition or a policy/preflight/spawn denial does not consume the binding; a successfully started child does. |
| `trust` | `"sandboxed"` or `"trusted"` | Subprocess authority. Default: `"sandboxed"`, which requires the configured general workspace sandbox and denies the hook and guarded action before child creation if the backend is unavailable. `"trusted"` is an explicit, audited containment bypass for reviewed automation; it does not restore the parent environment. |
| `env` | object of string values | Explicit environment additions. Values are literal (no shell expansion). The reserved `ZEROSTACK_PROJECT_DIR` key cannot be overridden in any ASCII case. Invalid names, NUL values, or case-insensitively colliding keys deny launch for portable Windows semantics. |

Project-hook approval remains bound to the immutable canonical startup project
root where the configuration was loaded. Worktree selection does not reload or
reapprove hook configuration; it separately rebinds execution to the canonical
selected workspace. That execution root is identity-pinned, revalidated for the
stdin envelope and immediately before every condition and handler child, and a
failed rebind or directory replacement denies subsequent launches. Conditions
and handlers use that same selected root, environment, trust, and sandbox
policy. Changing the parent process cwd cannot retarget them. Before every
launch zerostack clears the ambient environment, then
restores only `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `TERM`, `LANG`,
`LC_ALL`, `COLORTERM`, `NO_COLOR`, `TMPDIR`, and the Windows runtime names
`SYSTEMROOT`, `WINDIR`, `COMSPEC`, `PATHEXT`, `TEMP`, `TMP`, `USERNAME`, and
`USERPROFILE`, `SYSTEMDRIVE`, `ProgramFiles`, `ProgramFiles(x86)`, and
`ProgramW6432` when present, plus the handler's `env` values and
`ZEROSTACK_PROJECT_DIR`. API keys and other ambient credentials are therefore
absent unless the hook owner explicitly provides them in `env`.
Sandbox backends override `TMPDIR` with their confined temporary directory.

`"sandboxed"` selects the same backend named by `sandbox-backend`, independent
of the model-action `sandbox` on/off switch. On Linux, `bwrap` exposes the
workspace and application cache as writable, a minimal runtime filesystem,
and no IP network. On macOS, Seatbelt allows host-readable files, limits writes
to the workspace, application cache, temporary directory, and `/dev/null`, and
denies network. Other backends have only their reported backend-defined
guarantees. `"trusted"` has ambient filesystem and network access by explicit
configuration consent, while retaining direct argv, canonical cwd, minimal
environment, timeout/output bounds, cancellation, and tree cleanup. Audit logs
name the event, executable, trust choice, containment request/availability,
filesystem status, and network status; they never log argv, environment
values, stdin, or hook output.
Inside containment, a fixed trusted launcher verifies the executable is
visible and emits a private readiness record immediately before direct `exec`.
If the wrapper starts but never reaches readiness (setup or pre-exec failure),
zerostack classifies the outcome as policy denial rather than as a non-blocking
hook error; a `once` binding remains retryable.

Hook subprocess output has non-configurable hard limits: 1 MiB for stdout,
1 MiB for stderr, and 1.5 MiB combined. Stdout and stderr are drained
concurrently. Exceeding any limit is a hard hook failure: zerostack kills and
reaps the process group, retains at most the bounded prefixes for diagnostics,
and never interprets truncated stdout as a complete hook decision. The same
cleanup occurs on timeout or output-consumer failure.

`matcher` (on the handler group, not the handler) follows Claude Code
semantics: omitted, `""`, or `"*"` matches every tool; a bare name or a
`|`/`,`-separated list is an exact case-insensitive match after tool-name
normalization (e.g. `Bash` ↔ `bash`, `Glob` ↔ `find_files`, `Edit|Write`
matches zerostack's `write` tool); anything else is treated as a regex.
Invalid regexes are reported at load time.

### Events

`PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `UserPromptSubmit`,
`Stop`, `SessionStart`, `SessionEnd`, `SubagentStart`, `SubagentStop`.
`PreCompact` and `Notification` are not currently implemented.

Only `PreToolUse` is permission-blockable by default. A handler's stdout JSON
may set `"permissionDecision"` to `"deny"`, `"ask"`, `"allow"`, or omit it
(defer to the normal permission system). `deny` always blocks, holding even
under `--yolo`. `ask` forces an interactive confirmation regardless of
permission mode, and escalates to deny in non-interactive contexts (`-p`,
`--loop`) where no confirmation is possible. `allow` suppresses the
interactive prompt for that one call only — it can never override a deny
from a rule, security mode, managed policy, or another hook. `PreToolUse` may
also set `"updatedInput"` to rewrite the tool's arguments before it runs, and
`PostToolUse` may set `"redactions"` to an array of exact, non-empty strings.
Every occurrence is replaced with the fixed marker `[REDACTED]`. Hook-authored
`"result"` replacements are ignored so a hook cannot substitute arbitrary
model-visible output.

`SubagentStart` can set `"additionalContext"` to prepend text to the child
prompt. `UserPromptSubmit` cannot alter the submitted prompt; an
`"additionalContext"` field from that event is ignored. `Stop` and
`SubagentStop` can set
`"decision": "block"` with a `"reason"` to force the agent (or subagent) to
continue instead of finishing, using `reason` as the next instruction; `Stop`
gives up after 8 consecutive blocks without progress.

Any handler can also signal via **exit code** instead of JSON: exit `0` means
no objection, exit `2` blocks (for blockable events) with stderr as the
reason, and any other exit code is a non-blocking error. Exit `2` combined
with stdout JSON is a mixed-channel warning — the JSON is ignored. For
`PreToolUse`, an unexpected exit, timeout, policy-denied launch, or output-limit
failure denies the tool call: a configured guard must fail closed.

### Trust model

Project-level hook handlers (`.zerostack/settings.json` — global and managed
hooks are trusted automatically) require interactive confirmation the first
time they'd run, keyed by a hash of the handler's definition (event +
matcher + command/args/timeout/etc.); changing the definition changes the
hash and requires re-confirmation. Confirmations persist to
the state root at `hooks/trusted-hooks.json` (a user-level file, so child
processes/orchestrated subagents sharing it inherit trust automatically). In
headless contexts (`-p`, `--loop`) an unconfirmed project hook is skipped with
a warning rather than prompting.

Project confirmation and subprocess authority are separate decisions. The
confirmation hash includes `trust` and `env`, so either change requires new
consent. Global and managed provenance does not silently select the trusted
bypass: omitted `trust` still means `"sandboxed"` for every source.

### Global switches

| Flag | Effect |
| ---- | ------ |
| `--no-hooks` | Disables all non-managed hooks for this run. |
| `disableAllHooks: true` (in global or project `settings.json`) | Same effect, via config. |
| `--hooks-test <tool> [--hooks-test-input <json>]` | Dry-runs `PreToolUse` for `tool` against the loaded/trust-filtered dispatcher and prints the merged verdict/reason/`updatedInput`, then exits — no session, agent, or API key required. |

See [COMMANDS.md](COMMANDS.md#hooks) for the `/hooks` slash command.

## Mid-turn compaction

By default zerostack only compacts the conversation *between* turns, after a
response finishes, when the accumulated session history exceeds
`context_window - reserve_tokens`. A single long turn (many tool calls and large
tool results) can still blow past the model's real context limit before that
check ever runs, because the in-flight tool traffic never enters the session's
token estimate.

The summarizer processes oversized history as bounded recent chunks with a
rolling summary, at most 16 provider requests, and a five-minute aggregate
deadline. If a provider still rejects an ordinary request for exceeding its
context limit, the error points to `/compress` and `compact_enabled` recovery.

`mid_turn_compact_threshold` opts in to a second, *within-turn* check. On every
provider call zerostack compares the real provider-reported prompt size against
`context_window`; when the ratio crosses the threshold it aborts the current
run, compacts, and resumes the same task on the compacted history. The usage
event and abort cross an asynchronous channel, so a tool may already have
started and may be interrupted after partial effects. The continuation receives
a capped best-effort recap that labels this risk; inspect the working tree when
the last tool may have mutated files.

Provider usage is normalized without double-counting cache detail. Anthropic's
native Messages API reports uncached `input_tokens`, cache reads, and cache
writes separately, so all three count toward prompt pressure. OpenAI input
counts [include cached tokens](https://platform.openai.com/docs/api-reference/usage/audio_transcriptions_object),
OpenRouter returns cached tokens as a breakdown of
[`prompt_tokens`](https://openrouter.ai/docs/guides/best-practices/prompt-caching),
and Gemini says
[`promptTokenCount` includes cached content](https://ai.google.dev/api/generate-content).
Those routes therefore use the primary input count. For a non-native compatible
gateway that reports cache components separately, zerostack uses the larger
normalized `total_tokens - output_tokens` prompt count rather than adding cache
details blindly.

Before the first provider usage calibration, narrow text is estimated at a
conservative 3.25 characters per token instead of 4. This safety margin targets
code and JSON, which tokenize more densely; real usage replaces the estimate as
soon as a complete provider snapshot arrives.

- **Unset by default.** With no value set, behavior is unchanged: no mid-turn
  compaction. Setting a value is the opt-in.
- **Gated by `compact_enabled`.** `compact_enabled` is the master switch. If it
  is `false`, `mid_turn_compact_threshold` is ignored and nothing compacts.
- **Range.** A fraction in `(0.0, 1.0]`. An out-of-range value is ignored
  (mid-turn compaction stays off) and zerostack prints a warning at startup
  explaining the correct form, rather than failing silently. `0.80` is a
  reasonable starting value; it leaves headroom below the
  context window while still keeping the live prompt small enough to avoid the
  attention degradation ("context rot") that large, full context windows suffer.

```toml
compact_enabled = true            # master switch (default true)
context_window = 24576
mid_turn_compact_threshold = 0.80 # compact mid-turn at 80% real prompt pressure
```

## OpenAI API styles and custom headers

The `openai` provider (and any custom provider with `"provider_type": "openai"`)
can talk to either of rig's two OpenAI transports:

- **`responses`** — the Responses API (`/responses`). Default for
  `api.openai.com` (no `base_url`). Required for GPT-5-series models, which
  reject `max_tokens` on Chat Completions and expect `max_completion_tokens`.
- **`completions`** — the Chat Completions API (`/chat/completions`). Default
  when a custom `base_url` is set, because most OpenAI-compatible gateways
  (vLLM, LiteLLM, self-hosted) implement only this endpoint.

Set `api_style` to override the auto-detected default — for example, to force
`completions` against a gateway, or `responses` against an endpoint that
actually implements `/responses`.

Custom providers may also send arbitrary HTTP headers, which is useful for
gateways behind an auth proxy such as Cloudflare Access. Header values support
`${ENV_VAR}` expansion, so secrets stay in the environment rather than in the
config file:

```json
{
  "custom_providers": {
    "company-gateway": {
      "provider_type": "openai",
      "base_url": "https://gateway.example.com/v1",
      "api_key_env": "GATEWAY_API_KEY",
      "headers": {
        "cf-access-client-id": "${CF_ACCESS_CLIENT_ID}",
        "cf-access-client-secret": "${CF_ACCESS_CLIENT_SECRET}"
      }
    }
  }
}
```

The optional `timeout_secs` field overrides the default HTTP timeout for the
provider. TLS certificate verification can be disabled with
`"danger_accept_invalid_certs": true` (for self-signed or internal-CA
gateways) — use with care, as it makes the connection vulnerable to MITM.

When OpenRouter pricing or context metadata is missing, startup refreshes it
opportunistically while the remaining local initialization runs. Readiness
never waits for that network request: a refresh still pending at the dispatch
boundary is cancelled, and the existing session/catalog values are kept. The
foreground abort join has a 100 ms ceiling; a runtime reaper retains ownership
of any task still cancelling until it completes. A refresh that finishes in
time updates only fields that are still missing.
`custom_providers.openrouter.timeout_secs` continues to govern the request
while it is live.

## Provider-specific request body parameters

`headers` only touches HTTP headers. Some providers also accept parameters in
the JSON request *body* — for example OpenRouter's `plugins` presets that select
a routing strategy:

```json
{
  "model": "openrouter/fusion",
  "plugins": { "preset": "general-budget" }
}
```

`extra_body` injects arbitrary JSON into the completion request body. It is
shallow-merged (top-level keys win on collision) and works for **every**
provider — OpenAI, Anthropic, Gemini, Ollama, OpenRouter, and any custom
provider — not just OpenRouter. The same value is applied to the main agent and
the isolated `/btw` agent so they behave identically.

It can be set at two levels, resolved most-specific first:

1. **Per `quick_models` entry** — applies only when that model is active.
2. **Global top-level `extra_body`** — applies to every model, including the
   base `model`, unless a matching `quick_models` entry overrides it.

```toml
# Global default — applies to the base model and any model without its own value.
model = "openrouter/fusion"
provider = "openrouter"
extra_body = { plugins = { preset = "general-budget" } }

# A quick-model entry overrides the global value for that model.
[quick_models.quality]
provider = "openrouter"
model = "openrouter/fusion"
extra_body = { plugins = { preset = "quality" } }
```

In YAML:

```yaml
extra_body:
  plugins:
    preset: general-budget
quick_models:
  quality:
    provider: openrouter
    model: openrouter/fusion
    extra_body:
      plugins:
        preset: quality
```

Note that body parameters are **provider-specific**: a key one provider
understands may be ignored or rejected by another. Unlike `temperature`, a
global `extra_body` does not follow model switches, so prefer setting it per
`quick_models` entry — bundled with the matching `provider`/`model` — when the
parameter is tied to a specific provider.

## Status bar

The status bar at the bottom is configurable through `[statusline]`: up to 3
lines, each an ordered list of segments. When `[statusline]` is absent, a
built-in single-line layout is used.

```toml
# Line 1
[[statusline.lines]]
segments = [
  { item = "cwd", color = "blue" },
  { item = "separator", text = " " },
  { item = "git_branch", color = "magenta" },
  { item = "git_changes", color = "yellow" },
  { item = "flex_separator" },          # fills the row, pushing the rest right
  { item = "context_used", color = "green" },
  { item = "separator", text = "/" },
  { item = "context_max", color = "green" },
  { item = "separator", text = " " },
  { item = "context_percentage", color = "green" },
]

# Line 2 (optional)
[[statusline.lines]]
segments = [
  { item = "session_name" },
  { item = "separator", text = "  " },
  { item = "session_id", color = "dark_grey" },
  { item = "flex_separator" },
  { item = "tokens_input", color = "cyan" },
  { item = "separator", text = " " },
  { item = "tokens_output", color = "cyan" },
  { item = "separator", text = " " },
  { item = "cost", color = "green" },
]

# Line 3 (optional)
[[statusline.lines]]
segments = [{ item = "prompt", color = "white", bg = "#202020" }]
```

Each segment has:

| Field   | Description |
| ------- | ----------- |
| `item`  | The element to show (required). See the list below. |
| `color` | Foreground color: a name (`red`, `dark_cyan`, `light_blue`, ...) or `#rrggbb`. Optional. |
| `bg`    | Background color, same format. Optional. |
| `text`  | Literal text for the `separator` item. Optional (defaults to a space). |
| `left`  | Powerline cap glyph drawn before the item. A name (see below) or any literal string. Optional. |
| `right` | Powerline cap glyph drawn after the item. Optional. |
| `icon`  | Glyph shown before the value. `true` uses the item's built-in icon; a string sets a custom one (a named icon or a literal glyph). Optional. Needs a Nerd Font. |
| `always` | Force a numeric item (`tokens_input`, `tokens_output`, `cost`, `background_jobs`) to show even when its value is `0` (normally hidden until non-zero). Optional. |

Items with a built-in icon (used by `icon = true`): `git_branch`, `git_changes`,
`git_status`, `cwd`, `model`, `cost`, `context_used`/`context_max`/
`context_percentage`, `session_name`/`session_id`, `prompt`, `mode`, `loop`,
`btw`, `compaction`. Named custom icons for `icon = "<name>"`: `branch`,
`folder`, `chip`, `dollar`, `database`, `hash`, `terminal`, `lock`, `pencil`,
`sync`. Any other value is used literally, so a raw codepoint works too.

```toml
[[statusline.lines]]
segments = [
  { item = "git_branch", color = "magenta", icon = true },
  { item = "cwd", color = "light_blue", icon = "folder" },
]
```

`left`/`right` caps are drawn in the segment's `bg` color (falling back to its
`color`) over the status-bar background, so they read as the segment's edge.
They render only when the item is shown, and need a Nerd Font / Powerline font.
Named caps: `pl_right` (), `pl_left` (), `pl_right_thin` (),
`pl_left_thin` (), `pl_round_right` (), `pl_round_left` (),
`pl_flame_right`, `pl_flame_left`. Any other value is used as-is, so a raw
codepoint like `""` also works. Example:

```toml
[[statusline.lines]]
segments = [
  { item = "model", color = "white", bg = "#3b4252", left = "pl_round_left", right = "pl_round_right" },
]
```

Available items:

| Item                  | Shows |
| --------------------- | ----- |
| `session_name`        | The session name (hidden when empty). |
| `session_id`          | The first 8 characters of the session id. |
| `cwd`                 | The working directory name (folder only). |
| `cwd_full`            | The full working directory path, with `$HOME` shortened to `~`. |
| `worktree`            | Linked git worktree name (hidden when not in a linked worktree). |
| `git_branch`          | Current git branch (or short commit on detached HEAD). |
| `git_changes`         | Working-tree changes: `+staged ~modified -deleted ?untracked` (non-zero parts only; hidden when clean). |
| `git_status`          | Upstream sync and dirty marker: `↑ahead ↓behind *`, or `✓` when clean and in sync. |
| `model`               | The active model id. |
| `model_short`         | The model id without its provider prefix (e.g. `deepseek-v4-pro`). |
| `provider`            | The active provider name. |
| `tokens_input`        | Total input tokens this session. |
| `tokens_output`       | Total output tokens this session. |
| `context_used`        | Current context size in tokens. |
| `context_max`         | The model's context window. |
| `context_percentage`  | Context used as a percentage of the max. |
| `cost`                | Session cost (hidden at `$0.0000` unless `show_cost_always` is set). |
| `prompt`              | Active prompt (`prompt:<name>`). |
| `mode`                | Security mode when not `standard` (`mode:<name>`). |
| `loop`                | Active loop label. |
| `chain`               | Chain-of-prompts label. |
| `background_jobs`     | Running background shell jobs (`jobs:<n>`; hidden at zero). |
| `compaction`          | Number of compactions (`cmp:<n>`). |
| `btw`                 | `/btw` side-question token/cost usage. |
| `reasoning`           | Shows `reasoning` when reasoning is enabled (hidden when off). |
| `message_count`       | Number of messages in the session. |
| `session_age`         | Time since the session was created (e.g. `5m`, `2h10m`). |
| `session_updated`     | Time since the last message (same format). |
| `clock`               | Current local time (`HH:MM`). |
| `host`                | Machine hostname. |
| `user`                | Current username. |
| `separator`           | Literal text from `text` (default a space). Trimmed around hidden items. |
| `flex_separator`      | Expands to fill the remaining width; several split the space evenly. |

The `git_changes` and `git_status` items run `git status` once a second (only
when one of them is used). All other items are read from the session.

## Status signals

Requires the `status-signals` feature (included in the default build). Pass
`--status-socket <path>` to have zerostack emit `start`, `stop`, and
`git-conflict` events over a Unix domain socket at `<path>`, for external
status bars or tooling to watch. This is separate from the in-TUI status bar
above.

## Colors

The `colors` object accepts an optional `scheme_type` field and three optional
string fields for background colors, each of which can be a named color or hex
color (e.g. `"#1e1e2e"`). Named colors are case-insensitive.

### `scheme_type`

Controls the terminal color escape sequences emitted. Two values:

- `"full"` (default) — 24-bit true color via RGB escape sequences. Best for
  modern terminals.
- `"ansi"` — 256-color palette via ANSI escape sequences. Maps RGB colors to
  the nearest 256-color match (16 standard + 216 color cube + 24 grayscale).
  Use this for older terminals or multiplexers that don't support true color.

### Background fields

- `chat_background` — background color for the main conversation buffer.
- `input_background` — background color for the text input area.
- `status_background` — background color for the status bar (lowest line).

Supported named colors: `reset`, `black`, `red`, `green`, `yellow`, `blue`,
`magenta`, `cyan`, `white`, `grey`, `dark_grey`, `dark_red`, `dark_green`,
`dark_yellow`, `dark_blue`, `dark_magenta`, `dark_cyan`.

Example:
```json
{
  "colors": {
    "scheme_type": "full",
    "chat_background": "#1e1e2e",
    "input_background": "#181825",
    "status_background": "#11111b"
  }
}
```

Permission actions are lowercase strings: `allow`, `ask`, or `deny`. Each tool
rule can be a single action or an object mapping patterns to actions. Supported
permission tool keys are `shell` (`bash` is a compatibility alias), `js/fetch`, `read`, `write`, `edit`, `grep`,
`find_files`, `list_dir`, `todo_write`, `git/status`, `git/diff`, `git/log`,
`git/show`, `git/stage`, `git/unstage`, `git/commit`, and `mcp_tool`.
MCP-backed calls use `mcp_tool` as the tool key and
`{server_name}:{tool_name}` as the matched input. Use `"*"` for the default action,
`external_directory` for absolute-path rules outside the working directory, and
`doom_loop` for repeated identical tool calls (default: `ask`). If `bash` is
omitted, zerostack installs built-in exact-script allows (for commands such as
`pwd`, `git status`, and `cargo test`) plus pattern-based deny rules.
An `external_directory` deny is a security baseline: it takes precedence over
matching tool-specific allows and prior session AllowAlways scopes, including
inherited `read` access used by `lsp_diagnostics`.

The structured Git tool accepts only those seven fixed local operations. Read operations use a
canonical structured revision, path, and count identity for permission matching. `stage` and
`unstage` authorize each literal repository-relative path with their exact verb, while `commit`
uses the bounded commit message as its permission identity. The tool exposes no raw arguments,
shell commands, remotes, or network operations.

Bash uses a fail-closed, opaque full-script permission model. The exact string
passed to `bash -c` is also the permission key. An `allow` entry authorizes Bash
only when the entry is byte-for-byte equal to the complete script; glob and
regex expansion never widens a Bash allow. For example, an allow entry
`echo *` authorizes the literal script `echo *`, but not `echo hello`, pipelines,
redirects, substitutions, command lists, subshells, or background jobs that
start with `echo`. Bash `ask` and `deny` entries remain pattern-based so broad
safeguards still work. An unmatched Bash script asks in `guarded` and
`standard`; `yolo` remains the explicit allow-all mode subject to deny rules.

`planwrite` is read-only except for the narrow built-in plan-file exception:
`write`, `edit`, and `js/write_file` may modify `PLAN*.md` only when the
canonical target is component-contained beneath the startup workspace. A
matching basename outside that workspace, a sibling-prefix path, `..` escape,
or a final/parent symlink escape receives no exception and follows the ordinary
configured permission policy. New plan files require an existing stable parent
directory; publication uses the same no-follow atomic-write checks as ordinary
file tools so a path replacement after authorization fails closed.

Existing-file workspace-relative `edit` and `js/write_file` operations bound to
the captured startup workspace additionally require an atomic compare-and-
replace primitive: the published replacement must displace the same file
identity that was approved. Linux and macOS provide an atomic name exchange for
this check. Windows does not expose an equivalent expected-identity condition
on its rename or replacement APIs, and opportunistic locks can be broken by a
competing rename. These checked workspace replacements therefore fail closed
on Windows without changing the target; create-only `write` and `js/write_file`
operations remain available.

Bash commands have a mandatory 30-second deadline. A tool call's optional
`timeout` value is milliseconds and can only lower that deadline. Captured raw
output is limited to 1 MiB of stdout, 1 MiB of stderr, and 1.5 MiB combined;
both pipes are drained concurrently. Timeout, cancellation, or any output cap
kills and reaps the command's process group and returns an explicit non-success
status, with any retained prefix labelled as partial.

Set the shell tool's `background` argument to `true` for builds, test suites,
or servers that must outlive one tool call. The call returns a session-scoped
job id; `job_status` polls its state and rolling output or stops it with
`action = "stop"`. Background commands have a 24-hour maximum, accept a lower
`timeout`, keep 64 KiB of head/tail output per stream, allow eight concurrent
jobs, and retain the newest 32 job records. Cancellation and session shutdown
kill and reap every owned background process tree.

For a completed command, stdout and stderr preserve byte order within their own
streams. They are decoded independently using UTF-8 replacement, then rendered
as all stdout followed by all stderr with a newline separator. Interleaving
between the two OS pipes is intentionally not reconstructed.

There are two config fields for controlling permissions by pattern:

- **`permission`** — patterns are treated as globs (e.g. `**/*.rs`, `src/**`).
- **`permission-regex`** — same structure as `permission`, but patterns are
  treated as regular expressions (e.g. `.*\.rs$`, `^src/`). Regex patterns are
  unanchored — use `^` and `$` to match the full input.

Glob rules match the complete input. `*` matches zero or more characters other
than `/`; `**` also crosses `/`; `**/` matches zero or more directory levels;
and `?` matches exactly one character. All other characters—including brackets,
braces, and backslashes—are literal. A leading `~` is expanded. Filesystem glob
rules accept `/` on every platform and normalize Windows input separators;
raw `permission-regex` expressions retain their authored separator semantics.

Permission policy configuration is validated before any provider, model,
tool, UI, loop runner, or ACP server is constructed. Malformed permission
objects and invalid `permission-regex` expressions stop startup with the
configuration field, tool, and pattern path in the error. Invalid expressions
never degrade to a match-all rule.

Both fields can be used together; rules from both are merged. If both define a
default action (`"*"`), the glob default takes precedence.

### Rule precedence

When several rules for one tool match the same input, the outcome is
deterministic and independent of the order in which the rules are written:

1. Any matching `deny` wins. Deny rules are also evaluated before session
   `AllowAlways` grants and hook one-shot verdicts.
2. Otherwise the most specific matching pattern wins. Specificity is the
   number of literal characters in the pattern: everything except `*`, `**`,
   and `?` for globs, and everything except regex metacharacters for
   `permission-regex` rules. A single action (`read = "allow"`) counts as `**`
   with specificity `0`, so any pattern beats it.
3. On equal specificity `ask` beats `allow`.

In the example above, `src/main.rs` is allowed because `**/*.rs` (four literal
characters) is more specific than `**` (none), while `README.md` falls through
to the `**` ask rule. Path rules are matched against the absolute path, the
path as the tool received it, and the workspace-relative spelling, so a
relative rule such as `secrets/**` also applies when a tool passes the
canonical absolute path. Bash scripts are checked as a whole and line by line
against deny rules: a deny that matches any line denies the entire script.

As a TOML-friendly alternative to the nested `permission` object, you can use
`permission-allow`, `permission-ask`, and `permission-deny` at the top level.
Each is a map from tool name to a list of glob patterns. These work side by
side with the `permission` field and are especially convenient in TOML configs:

```toml
permission-allow = { read = ["src/**", "tests/**"] }
permission-ask = { bash = ["rm **"] }
permission-deny = { write = ["/etc/**", "/usr/**"] }
```

In YAML:
```yaml
permission-allow:
  read: ["src/**", "tests/**"]
permission-ask:
  bash: ["rm **"]
permission-deny:
  write: ["/etc/**", "/usr/**"]
```

A `permission-regex` example in YAML:

```yaml
permission-regex:
  "*": ask
  read:
    "\\.md$": allow
    "\\.rs$": ask
  bash:
    "^cargo (test|check|build)$": ask
    "^rm ": deny
```

Because Bash allows are exact-script only, use the non-regex permission field
for explicit allowed scripts:

```yaml
permission:
  bash:
    "cargo test": allow
    "git status": allow
```

When compiled with MCP support, `mcp_servers` accepts local stdio and remote
URL-based servers. A local stdio entry launches `command` directly and passes
`args` without shell parsing. `command` may be an executable available on
`PATH` or an absolute executable path; zerostack resolves platform shims such
as Windows `.cmd`/`.exe` launchers to an absolute identity before spawn.

MCP connections are capability-driven. `--no-tools` never connects configured
servers. An exact `--tools` allowlist containing only built-in tool names also
skips MCP; an unrecognized name keeps MCP eligible because it may be a server
tool. This avoids launching workspace services for runs that cannot expose any
of their tools.

The child environment is empty by default. `env` supplies explicit values and
`inherit_env` names individual parent variables that the server is allowed to
receive; an `env` value wins if the same name appears in both. `cwd` selects an
explicit working directory. When omitted, zerostack captures the connection
workspace and still applies it explicitly rather than inheriting mutable
process-global state.

Configured command servers are human-trusted workspace services. Omitting
`sandbox` is an explicit trusted-code bypass with inherited host filesystem and
network access; it is not reported as sandboxed. Set `sandbox` to a supported
backend name to require the dedicated workspace-service profile. A missing or
unsupported requested backend denies launch. `network` is `inherit` by default;
`network = "deny"` requires a selected backend that can enforce denial and
otherwise also denies launch. Server launch trust, service sandbox/network
authority, and permission to call each exposed MCP tool are independent.

The server must reserve stdout for MCP protocol messages and write diagnostics
to stderr. Every server must complete the MCP initialization handshake within
10 seconds; for URL servers that budget also covers the TCP/TLS connect and,
with OAuth, restoring (and refreshing) the stored token. Each server's
`tools/list` enumeration is bounded to 30 seconds and 64 pages. A server that
exceeds either budget is reported in a startup notice and skipped without
delaying the others. Every `tools/call` is bounded by `mcp_tool_timeout_secs`
(default 120); on expiry zerostack cancels the request and returns a tool error
the model can act on instead of stalling the turn. Malformed JSON arguments are
rejected rather than silently converted to an argument-less call. Text, image,
and embedded resource data from one result share a hard 1 MiB cumulative bound.

When two servers expose a tool with the same name, both are registered under
`<server>__<tool>` (for example `alpha__search` and `beta__search`) and a
startup notice lists the renames. The permission key stays
`mcp_tool:{server_name}:{tool_name}` with the bare tool name.

Resolution, spawn, handshake, malformed-output, and early-exit
failures identify the configured server and include at most 8 KiB of captured
stderr (including invalid UTF-8 replacement) so startup diagnostics stay useful
and bounded. The transport owns a Unix process group or Windows Job Object;
initialization failure, cancellation, reconnect replacement, shutdown, and
handle drop terminate and reap the service tree, including descendants that
close inherited protocol pipes.

Servers can also be added per project via `.zerostack/config.toml` (see
*Project-local override* above); project servers merge with — and can override
— global ones by name.

```json
{
  "mcp_servers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "."],
      "cwd": ".",
      "env": {},
      "inherit_env": ["PATH", "HOME"],
      "sandbox": "bwrap",
      "network": "deny"
    },
    "remote-search": {
      "url": "https://example.com/mcp",
      "headers": {
        "authorization": "Bearer token"
      }
    }
  }
}
```

URL servers must use HTTP(S), must not embed user information, and must resolve
exclusively to public IP addresses. `localhost`, loopback, private, link-local,
metadata, multicast, documentation, reserved, and other non-public destinations
are rejected before the connection is created. Use a stdio server for a local
service; the OAuth redirect listener is a separate product-owned loopback flow.

### OAuth for URL servers

URL-based servers can authenticate with OAuth 2.0 (authorization code + PKCE).
Add an `oauth` field. Use `true` for defaults (dynamic client registration, no
extra scopes), or an object for explicit settings:

```json
{
  "mcp_servers": {
    "my-oauth-server": {
      "url": "https://example.com/mcp",
      "oauth": true
    },
    "scoped-server": {
      "url": "https://api.example.com/mcp",
      "oauth": {
        "scopes": ["read", "write"],
        "client_id": "pre-registered-client-id",
        "redirect_port": 8970
      }
    }
  }
}
```

OAuth fields (all optional):

| Field           | Default                        | Description                                                              |
| --------------- | ------------------------------ | ------------------------------------------------------------------------ |
| `scopes`        | none                           | Scopes to request during authorization.                                  |
| `client_id`     | dynamic registration           | Pre-registered client id. When omitted, the client registers on the fly. |
| `redirect_port` | `8970`                         | Loopback port for the redirect URI `http://127.0.0.1:<port>/callback`.   |

The first time you connect, run `/mcp login <server>` inside the TUI. zerostack
prints an authorization URL and attempts to copy it to your clipboard; the TUI
distinguishes a confirmed clipboard write from an unacknowledged OSC 52
terminal request. Open the URL in a browser, approve access, and the redirect
is caught on the loopback port. The browser wait runs in the background, so the
TUI stays responsive (you can keep working or select the URL with the mouse to
copy it). The token is saved to
`<credentials_dir>/mcp-oauth/<opaque-server-identity>.json`; server display
names never become filenames. `credentials_dir` defaults to
`<local_data_dir>/credentials` (including `%LOCALAPPDATA%\zerostack\credentials`
on Windows) and can be set explicitly with the absolute `ZS_CREDENTIALS_DIR`
override.

The credential directory and every final, temporary, and lock file are private
from creation: `0700`/`0600` on Unix and a protected current-user/SYSTEM DACL
on Windows. Existing owned regular paths are repaired to that policy; symlinks,
reparse points, wrong-type paths, oversized records, and ambiguous legacy names
fail closed. zerostack imports an unambiguous legacy
`<data_dir>/mcp-oauth/<server>.json` only when the canonical record is absent,
leaves the legacy source in place for recovery, and can safely retry after an
interruption. A private, non-secret migration marker prevents an explicit logout
from re-importing that retained source. Resolve a migration conflict by moving
aside the unrelated legacy candidate, then reconnect or log in again.

Later sessions reuse the stored refresh token and reconnect without a browser.
Use `/mcp logout <server>` to remove only that canonical identity's stored
token. A server with OAuth enabled but no stored token fails to connect until
you log in.

### Recommended MCP servers

When `mcp_servers` is not explicitly set, three recommended MCP servers are
available. Each can be toggled with a boolean config key (all default to the
listed API key environment variable when that variable is set):

| Key                    | Default | Description                                     | Env var              |
| ---------------------- | ------- | ----------------------------------------------- | -------------------- |
| `enable-exa-mcp`       | `true`  | Exa web search (mcp.exa.ai)                     | `EXA_API_KEY`        |
| `enable-context7-mcp`  | `false` | Context7 documentation lookup (mcp.context7.com) | `CONTEXT7_API_KEY`   |
| `enable-grepapp-mcp`   | `false` | Grep.app semantic code search (mcp.grep.app)     | `GREP_APP_API_KEY`   |

Set `enable-exa-mcp = false` to disable the Exa default without touching
`mcp_servers`. Set `"mcp_servers": {}` to disable all MCP auto-configuration.

In `readonly` and `planwrite` modes, approval-free MCP access is limited to
immutable built-in registrations and these exact read-only tool names:
Exa `websearch` and `webfetch`; Context7 `get_context` and `search_docs`; and
Grep.app `search_code` and `search_repos`. They are exempt because they only
retrieve public web, documentation, or source-search results. The exemption is
bound to zerostack's built-in registration identity, not the configured server
name or URL. Custom servers—including servers using the same name or
endpoint—follow normal MCP permission rules, and any unlisted tool on a
built-in server does too.

## ACP (Agent Communication Protocol) configuration

When compiled with the `acp` feature, zerostack can act as an ACP agent server.
The following config keys are available:

| Key           | Type    | Description                                            |
| ------------- | ------- | ------------------------------------------------------ |
| `acp_servers` | object  | Named ACP server configurations (see below)            |
| `acp_host`    | string  | TCP bind host for ACP server (default: loopback when TCP is selected) |
| `acp_port`    | integer | TCP bind port for ACP server (default: 7243)           |

ACP server configs (in `acp_servers`) support two transport types:

```json
{
  "acp_servers": {
    "tcp-server": {
      "type": "tcp",
      "host": "127.0.0.1",
      "port": 7243,
      "api_key": "replace-with-a-long-random-secret"
    }
  }
}
```

When `--acp` is passed without `--acp-host`, zerostack runs in stdio mode
(the editor spawns it as a subprocess). Supplying `--acp-host`, `--acp-port`,
`acp_host`, or `acp_port` selects TCP. If only a port is supplied, the bind
host defaults to `127.0.0.1`. A non-loopback `acp_host` is an explicit remote
exposure choice and emits a startup warning.

Every `session/new` request must provide an existing directory as `cwd`.
zerostack canonicalizes that directory before creating the session and binds
the session to that directory's filesystem identity. The binding retains an
open directory capability and opens relative context, prompt, read, write,
edit, search, list, and JavaScript paths through it. Each operation first
checks that the canonical pathname still names the captured directory. A
rename, removal, or replacement visible before that check fails closed; a
replacement racing after the check cannot redirect the operation because its
filesystem access is already relative to the retained handle. Relative file
operations walk held directory descriptors and reject symlink or reparse-point
components and targets.

ACP advertises protocol V1, connects MCP services for each tool-enabled prompt,
and runs the same process-wide lifecycle and tool hooks as other frontends.
Permission prompts reuse the corresponding ACP tool-call ID, so clients can
attach the decision to the call they already rendered.

Each new ACP session also owns an independent in-memory conversation history.
Only completed turns are committed: the user prompt, correlated structured tool
call/result messages, and the terminal assistant response are retained together.
The next prompt receives that committed history before its current user message.
History is bounded to 128 complete turns and 2 MiB of serialized Rig messages;
the oldest complete turns are evicted first. The process retains at most 64 ACP
sessions. Because ACP exposes no session-close notification, creating a 65th
session is rejected rather than silently invalidating any live session. Each
session accepts one active prompt; a concurrent prompt for the same session is
rejected instead of queued. Protocol dispatch and work in other sessions remain
independent. History is not persisted across server restarts, and the agent
therefore neither advertises nor handles ACP `session/load`.

ACP `session/cancel` stops the active turn for that session and
returns `cancelled` from its original `session/prompt` request. Cancellation is
generation-tagged: duplicate notifications are idempotent, a notification after
completion cannot abort a later prompt, and a runner attached after an early
cancellation is aborted immediately. Cancelled turns commit no user, tool, or
assistant history. Dropping the original prompt request has the same effect.
Concurrent prompts for one session are rejected while its turn is active, so an
untagged duplicate cancellation cannot be redirected to queued work. Other
sessions remain independent and continue normally.

ACP context includes managed global files and context files in the captured
workspace root. It intentionally does not load `AGENTS.md`, `CLAUDE.md`, or
`ARCHITECTURE.md` from ambient parent directories, because those parents are
outside the session capability and can be reparented independently.

On Unix, Bash/JavaScript children enter the retained directory with `fchdir`,
and contained wrappers receive a fixed inherited descriptor instead of
re-resolving the host pathname. LSP services likewise use a descriptor-backed
root URI and stable, no-follow file reads. On Windows, the session holds a
directory handle that deliberately excludes delete sharing, pinning the root
name for the binding lifetime. A requested `zerobox` sandbox for an ACP
workspace fails closed on Unix because that backend cannot consume the
directory-handle authority.

Permission containment, LSP services, and delegated read-only agents use the
same binding. Concurrent ACP sessions may therefore use different roots
without changing or inheriting the server process working directory. Missing
paths and non-directories are rejected before an agent is built. LSP file
requests are strictly contained; other absolute, `..`, symlink, and reparse-point
escapes are rejected.

### ACP TCP peer authentication

TCP mode always fails closed unless an authentication key is available. The
key is resolved in this order:

1. The non-empty `MINI_AGENT_ACP_API_KEY` environment variable.
2. The non-empty `api_key` from a TCP entry in `acp_servers` whose `host` and
   `port` exactly match the resolved listener endpoint.

Environment configuration is recommended so the secret is not stored in the
configuration file. Restart the server after rotating the key. Stdio mode does
not use this authentication handshake.

Authentication happens before ACP framing, initialization, or session
allocation. For each connection the server sends:

```text
MINI-AGENT-ACP-AUTH/1 CHALLENGE <32-lowercase-hex-nonce>\n
```

The client responds with:

```text
MINI-AGENT-ACP-AUTH/1 RESPONSE <hmac-sha256-hex>\n
```

`hmac-sha256-hex` is lowercase HMAC-SHA-256 using `api_key` as the key and
the following byte sequence as the message:

```text
"MINI-AGENT-ACP-AUTH/1" || 0x00 || nonce
```

Each connection receives a fresh nonce, so a captured response cannot be
replayed. The comparison is timing-safe; authentication has a five-second
total deadline, responses are limited to 128 bytes, and at most 16 peers may
authenticate concurrently. Missing, malformed, oversized, timed-out, invalid,
and replayed responses are disconnected without entering the ACP parser.
Authentication errors and configuration debug output never include the key.

The TCP transport does not encrypt ACP traffic or authenticate the server.
Keep the loopback default where possible. For remote access, use a high-entropy
key and place the connection inside a trusted TLS, VPN, or SSH tunnel.

`mini-agent --acp-authentication-check` runs a headless loopback check that
mechanically verifies valid, missing, and replayed credential behavior.

## TOML configuration

Within each search directory, zerostack picks the first existing file in
this priority order: `config.toml`, `config.yaml`, `config.yml`, then
`config.json`. `config.json` is kept for backwards compatibility — since YAML
is a superset of JSON, legacy JSON configs parse transparently through the
YAML reader. If none exists, a default `config.toml` is created automatically.

TOML is especially well suited for zerostack's permission rules and structured
settings. Hyphenated keys such as `permission-regex`, `permission-allow`,
`permission-ask`, and `permission-deny` are idiomatic in TOML and avoid deeply
nested tables:

```toml
permission-allow = { read = ["src/**", "tests/**"] }
permission-ask = { bash = ["rm **"] }
permission-deny = { write = ["/etc/**", "/usr/**"] }
```

For more complex configurations, explicit TOML tables provide clear structure:

```toml
[permission]
"*" = "ask"

[permission.bash]
"cargo test" = "allow"
"rm **" = "deny"

[permission.write]
"**/*.rs" = "allow"
"**" = "ask"
```

### Key naming in TOML

All top-level keys use kebab-case when they contain hyphens (e.g.
`permission-allow`, `allow-all-mcp-calls`). Simple keys use the same name as
their YAML counterpart. Quoted keys (`"*"`, `"**"`) are required when the key
contains special characters like `*` or `/`.

## Edit System Modes

zerostack supports two edit systems, selectable via `edit_system` config key,
`--edit-system` CLI flag, or `/editsys` slash command:

### `similarity` (default)

The classic aider-style SEARCH/REPLACE format. The LLM copies exact text from
read output into `<<<<<<< SEARCH` blocks and provides replacements in
`>>>>>>> REPLACE` blocks. Falls back to whitespace normalization and fuzzy
matching when the exact text doesn't match.

```
edit_system = "similarity"
```

### `hashedit`

Tag-based edits using CRC-32 line hashes and file-level CAS (check-and-set)
tokens. The read tool annotates each line with an 8-char hex CRC-32 tag (e.g.
`"  10|f1e2d3c4 int count = 10;"`) and a file-level CRC header. The edit tool
receives tagged lines from the read output and provides only the replacement
text — no old-text reproduction needed.

Key advantages:
- **Token-efficient**: No old-text reproduction (significant savings for
  deletions and large edits)
- **CAS-guarded**: File-level CRC prevents applying edits to stale content
- **Reliable**: Per-line tag validation catches content mismatches

```
edit_system = "hashedit"
```

Switching between modes is immediate and does not require agent restart.
The `/editsys` `similarity` and `/editsys` `hashedit` slash commands
provide the same functionality at runtime.

## Prompt directives

Custom prompt `.md` files may include a `%%mode=<mode>` directive on the
**first line** to automatically switch the security mode when the prompt
is activated (via `/prompt <name>` or as the `default_prompt`).

Valid modes: `standard`, `restrictive`, `readonly`, `planwrite`, `guarded`, `yolo`.

A directive can only narrow the mode. The user's own selection (a CLI flag,
`default_permission_mode`, or `/mode`) is the ceiling: a prompt asking for a
more permissive mode (for example `%%mode=yolo` while running `--guarded`) is
ignored and the current mode is kept. Ranking from least to most permissive:
`readonly`, `planwrite`, `restrictive`, `guarded`, `standard`, `yolo`.

Prompts are loaded from three sources and the source decides whether a
directive is honored at all. Embedded prompts and the user's own prompts
directory are trusted like the global config. Prompts from the project's
`.zerostack/prompts/` are repository content: their `%%mode=` directive is
dropped (with a warning) unless the project's `.zerostack/config.toml` has
been explicitly trusted through the project-config trust store described
under Trust model; `%%mode=last_user_mode` is always kept because it can
only restore the user's selection.

Use `%%mode=last_user_mode` to keep (or restore) the mode the user last
set explicitly via `/mode` or startup config — useful when a prompt wants
to avoid overriding the user's chosen mode.

The directive line is stripped from the prompt content before it reaches
the agent.

Example `ask.md`:

```markdown
%%mode=readonly

## Read-Only Mode

You are in read-only mode. Only read files and explore.
```

Example `code.md` that defers to the user's mode:

```markdown
%%mode=last_user_mode

## Coding Mode

Write well-tested code. Follow project conventions.
```

The mode change is applied when the prompt is activated and persists
until changed again by `/mode`, another prompt directive, or a restart.
The status bar shows `| mode:<name>` when the mode is not `standard`.

## Prompt-to-model switching

The `[prompt_to_model]` table maps prompt names to quick-model names. When
you switch to a prompt (via `/prompt`, `.name`, `/review`, chain transitions,
or `default_prompt` at startup), zerostack looks up the mapping and
automatically switches the active model to the corresponding quick model.

Values are quick-model names — the same names defined in `[quick_models]`.
An empty string (`""`) means "no change", so the current model stays active.

```toml
[prompt_to_model]
plan = "glm-52"
code = "deepseek-v4-pro"
review = "qwen37-plus"
brainstorm = ""
```

With this config:
- `/prompt plan` or `.plan` switches to the `glm-52` quick model.
- `/prompt code` or `.code` switches to `deepseek-v4-pro`.
- `/review` switches to `qwen37-plus`.
- `/prompt brainstorm` or `.brainstorm` does **not** change the current model.

When you run `/prompt default` (clearing the active prompt), the model
reverts to the session's default model (from `model` / `provider` config
or `--quick-model`).

The model switch writes a line to chat:
`switched to model: glm-52 (from prompt 'plan')`

## Chain-of-Prompts

When enabled, after the agent finishes responding with a `brainstorm`, `plan`,
or `code` prompt, the status bar shows `Continue to <next>? [Yes/But/No]`.
The user's next input is interpreted as a chain decision:

- **Yes** (`y`/`yes`) — switch to the next prompt and auto-submit a transition message.
- **But** (`but <msg>` / `b <msg>` / `yes but <msg>`) — same as yes, but prepend
  `<msg>` as an additional instruction to the transition message.
- **No** (`n`/`no`) — decline the chain, continue normally.

Typing anything that doesn't match these patterns clears the chain and
processes the input as a normal message.

### Phases

| Transition | Default | Description |
|-----------|---------|-------------|
| `brainstorm-to-plan` | `true` | After brainstorming, prompt to move to planning |
| `plan-to-code` | `true` | After planning, prompt to start coding |
| `code-to-review` | `false` | After coding, prompt to run a review |

### TOML

```toml
[chain]
brainstorm-to-plan = true
plan-to-code = true
code-to-review = false
```

### YAML

```yaml
chain:
  brainstorm-to-plan: true
  plan-to-code: true
  code-to-review: false
```

## Advisor

The advisor tool lets the agent consult a stronger reviewer model (or the
user, in human-handoff mode) for strategic guidance before making important
decisions. This follows the [advisor strategy](https://claude.com/blog/the-advisor-strategy):
a cheaper "executor" model drives the task and escalates to a more capable
model only when needed.

### TOML

```toml
[advisor]
enabled = true
model = "deepseek-v4-pro"
# max_uses = 3                    # max advisor calls per request (nil = unlimited)
# human_handoff = true            # struct default is true, but currently has no effect from config alone; see the note below
# advisor_kilobytes_limit = 256   # max KB of conversation context (split half head / half tail)
```

### YAML

```yaml
advisor:
  enabled: true
  model: deepseek-v4-pro
  max_uses: 3
  human_handoff: true
  advisor_kilobytes_limit: 256
```

### CLI flags

| Flag | Description |
|------|-------------|
| `--advisor` | Enable the advisor tool |
| `--advisor-model <name>` | Advisor model name |
| `--advisor-max-uses <n>` | Max advisor calls per request |
| `--advisor-human-handoff[=<bool>]` | Route advisor calls to the user instead of a model. Bare flag or `=true` enables it; CLI default is `false` unless passed |
| `--advisor-kilobytes-limit <n>` | Max KB of conversation context sent to advisor (default: 256) |

**Known quirk:** the CLI flag always supplies a value (`Some(false)` when not
passed), so `resolve_advisor_human_handoff()` never falls through to the
config file's `human_handoff` key in practice. Use `--advisor-human-handoff`
or the `/advisor handoff on` runtime command to actually enable it; setting
`human_handoff` in the config file alone currently has no effect.

### Human handoff mode

When enabled, the agent's advisor calls are redirected to the
user instead of a second model. The agent pauses, shows its question, and the
user types a response. This is useful for:

- Reviewing the agent's approach before it writes code
- Stepping in when the agent is stuck or uncertain
- Teaching the agent your preferences interactively

### Runtime control

The `/advisor` slash command provides runtime control:

```
/advisor                    Show current advisor status
/advisor on|off             Enable or disable the advisor
/advisor handoff [on|off]   Toggle human handoff mode
/advisor model <name>       Change the advisor model
/advisor max-uses <n>       Set max advisor calls per request (0 = unlimited)
/advisor context-limit <n>  Set max kilobytes of conversation context
```

## LSP

zerostack can run language servers for the files the agent edits and feed
diagnostics (errors/warnings) back into `edit`/`write` tool results — the
agent sees type errors immediately instead of discovering them on the next
build. An `lsp_diagnostics` tool also lets the agent query one file or the
whole project on demand.

`lsp_diagnostics` uses the existing `read` permission policy. A file query
checks the canonical file path before synchronizing it or starting a language
server. Canonical paths must be UTF-8 and regular files; other filesystem node
types fail closed before permission evaluation or server access. A whole-project
query checks the canonical project root before reading
the diagnostic cache, then applies the same path policy to every cached file;
denied files are omitted from the aggregate result. External files therefore
also follow `permission.external_directory`. Aggregate checks keep each cached
file's exact regular-file handle open across permission prompts and bind result
formatting to that identity, so pathname swaps cannot change which diagnostic
is authorized. Choosing AllowAlways for the project prompt grants only that
project tree; path patterns use normalized separators on Windows.

The root AllowAlways decision persists both the exact project root and its
descendant scope, preventing repeat aggregate prompts without granting sibling
trees. Generated scopes escape literal glob metacharacters in project directory
names through a separate literal-path encoding, leaving user-authored glob and
regex syntax unchanged. Windows external-directory policy evaluates both
verbatim drive/UNC and prefix-stripped spellings, combining matches fail-closed,
so anchored raw regexes written for either canonical form remain effective.
Aggregate authorization binds and snapshots one relevant cached file at a time, formats
only the remaining capped lines while the binding is live, and releases its
descriptor before opening the next candidate. Large or high-volume workspaces
therefore cannot cause unbounded diagnostic cloning or file-handle use.

Cache entries are accepted only for canonical file URIs that still resolve to
regular files; symlink aliases and alternate URI spellings are discarded. The
client also accepts a publish only when its document version matches the most
recent full-document sync, preventing delayed pre-edit results from satisfying
a post-edit diagnostics wait. Versioned diagnostic support is advertised.
Versionless initial publishes and clears (including an explicit JSON
`version: null`) remain accepted while the sync epoch is unchanged; after an
edit they fail closed until an exact versioned publish anchors the new epoch.
The diagnostic cache retains at most 256 distinct file entries, at most 256
diagnostics per entry, and at most 2 MiB of accounted retained diagnostic data
in aggregate. Messages are truncated to 1,024 UTF-8 bytes and unused extension
payloads such as arbitrary JSON `data` are discarded before cache commit.
Updating an entry replaces its contribution to the aggregate byte budget.
If a valid newer update cannot fit, its older cached diagnostics are replaced
by an empty version tombstone so waits complete without exposing stale data.
These limits also bound aggregate candidate allocation and sorting. Retained
diagnostic message bodies are supplied by the configured language server and
can mention other files, so language-server commands and configuration must
still be treated as trusted project code.

This integration is behind the non-default `lsp` Cargo feature — build with
`--features lsp` to enable it.

### TOML

```toml
[lsp]
enabled = true

[lsp.servers.rust]          # override a built-in default
command = "rust-analyzer"
extensions = [".rs"]
inherit_env = ["PATH"]      # named parent values only

[lsp.servers.myserver]      # fully custom server
command = "my-ls"
args = ["--stdio"]
extensions = [".my"]
# env = { TOOLCHAIN_HOME = "/opt/toolchain" } # explicit values win
# sandbox = "bwrap"          # or "seatbelt" / "zerobox"
# network = "deny"           # requires a sandbox that enforces denial
# initialization = { ... }   # server-specific initializationOptions
# disabled = false           # true removes a same-named built-in
```

### YAML

```yaml
lsp:
  enabled: true
  servers:
    rust:
      command: rust-analyzer
      extensions: [".rs"]
      inherit_env: [PATH]
```

Built-in server defaults (used only when the binary is on PATH):
rust-analyzer, gopls, typescript-language-server, pyright-langserver,
clangd, bash-language-server, lua-language-server.

Behavior notes:

- Executables are resolved once through the launcher's PATH (or an absolute
  configured path); zerostack never auto-installs a language server. A missing
  binary is skipped with a debug log.
- Servers start lazily on the first edit touching one of their extensions
  using the canonical session cwd for both process cwd and `rootUri`.
- The child environment is cleared. `inherit_env` delegates named parent
  values and `env` supplies explicit values; explicit values win. Built-ins
  delegate only `PATH` so they can find project toolchains.
- Omitting `sandbox` explicitly trusts the configured workspace service. A
  requested backend fails closed when unavailable. `network = "deny"` starts
  only when that backend can enforce network denial; it never falls back to an
  uncontained launch.
- Initialization is bounded to 15 seconds. Malformed/oversized protocol input,
  server exit, and shutdown terminate and reap the complete Unix process group
  or Windows Job Object. Stderr is drained in fixed-size chunks, never logged,
  and terminates the server after 64 KiB of cumulative output.
  A stopped server is replaced on the next matching edit.
- Document synchronization skips files larger than 4 MiB without advancing
  protocol state. Published diagnostics are accepted only for workspace URIs,
  capped at 50 entries for each of at most 128 files per server, and stripped
  of unneeded related/data payloads before retention; exceeding a storage cap
  terminates that server.
- Everything is fail-open: a hung or crashed server only means "no
  diagnostics", never a failed edit.
- Post-edit diagnostics are capped (errors first, ~20 lines); nothing is
  appended when the file is clean.

## Logging

zerostack uses the `tracing` framework for structured logging. By default, only
warnings and errors are printed to stderr at the `warn` level (with the `rig`
crate silenced). Full debug and trace output is available via CLI flags.

### Verbose mode (`-v` / `--verbose`)

```bash
zerostack -v
```

Enables full trace-level logging to a timestamped log file below the state
root's `logs/` directory (`ZS_STATE_DIR` overrides the state root). The log
file is named `zerostack-YYYY-MM-DD_HH-MM-SS_<pid>.log`. A new file is created
per instance — previous runs are never overwritten.

With `-v`, stderr output stays at the default `warn` level so the TUI remains
clean. The log file captures everything at `trace` level for all zerostack
modules.

### Custom log file (`--log-file`)

```bash
mini-agent --log-file /tmp/debug.log
```

Writes full trace-level logs to the specified path instead of the default
location. Implies `-v` for the file output. Can be combined with `-v` (no
effect on the path, since `--log-file` takes precedence).

### Custom stderr log level (`--log-level`)

```bash
mini-agent --log-level debug
```

Sets the minimum level for stderr output. Accepted values: `trace`, `debug`,
`info`, `warn`, `error`. This overrides the `RUST_LOG` environment variable.

### Environment variable (`RUST_LOG`)

The standard `RUST_LOG` environment variable is still supported for backward
compatibility:

```bash
RUST_LOG=mini_agent=debug mini-agent          # debug level for this crate
RUST_LOG=debug,rig=off mini-agent             # debug for everything except rig
RUST_LOG=mini_agent::agent::tools=trace mini-agent  # trace only tool execution
```

The crate's tracing target is the crate name, `mini_agent` (the `mini-agent`
package name with `-` mapped to `_`), not `zerostack`; only the explicit audit
events use `zerostack::audit::*` targets.

Priority (highest wins): `--log-level` > `RUST_LOG` env > default `warn,rig=off`.

The `-v` / `--log-file` file layer is not affected by `RUST_LOG`: it always
uses `mini_agent=trace,zerostack=trace,rig=off`, so every event from this
crate plus the `zerostack::audit::*` targets reaches the file.

### Logged subsystems

With `-v`, the following subsystems produce debug/trace output:

- **Agent lifecycle**: prompt sizes, retry events, token usage, tool call dispatch
- **LLM-exposed tools**: every tool invocation with start/end, arguments, and results (bash, read, write, edit, grep, find_files, list_dir, todo_write)
- **Config loading**: first startup detection, config file path, quick model and provider counts
- **Session management**: save, delete, and find operations with message counts
- **Permission checker**: every permission check result, doom-loop detection, mode changes
- **MCP**: connection attempts, transport details, per-server tool counts, reconnects
- **ACP**: server start, session creation, prompt execution with provider/model info
- **Memory**: store open, write operations (target/bytes), searches, tool entry points
- **Advisor**: initialization (model, enabled, max uses), tool call prompts
- **Filesystem**: atomic write paths and byte counts
