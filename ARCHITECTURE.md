# Architecture Overview — mini-agent v1.8.0

> **mini-agent** is a minimalistic coding agent with a built-in QuickJS engine, TUI, sandboxed workers, MCP/LSP integration, persistent skill learning, and autonomous loop/goal features. It is a single Rust binary using `rig-core` for LLM abstraction, `crossterm` for terminal UI, `tokio` for async I/O, and `rquickjs` for embedded JavaScript execution.

---

## Directory Layout

```
mini-agent/
├── src/
│   ├── main.rs                  # Binary entry: CLI parse, Tokio runtime, TUI start
│   ├── cli.rs                   # clap CLI argument parsing (~61 KB, all flags/subcommands)
│   ├── config/
│   │   ├── mod.rs               # Config struct, resolution methods, default values
│   │   ├── types.rs             # EmbeddingBackendKind, QuickModelConfig, ReasoningConfig, etc.
│   │   └── load.rs              # Config file loading from TOML, env overrides
│   ├── agent/
│   │   ├── mod.rs               # Agent public API re-exports
│   │   ├── runner.rs            # AgentRunner: spawn, event loop, tool dispatch, stream processing (~8794 lines)
│   │   ├── builder.rs           # Agent construction: system prompt assembly, model selection
│   │   └── tools/
│   │       ├── mod.rs           # Tool definitions, argument types, permission system, ReadTracker
│   │       ├── bash.rs          # Shell command execution (sandboxed + background jobs)
│   │       ├── concurrency.rs   # Concurrency tool (parallel tool dispatch)
│   │       ├── crc.rs           # File CRC32 computation (content hash verification)
│   │       ├── edit.rs          # SEARCH/REPLACE block editor (aider-style)
│   │       ├── find_files.rs    # Recursive file glob search
│   │       ├── grep.rs          # Regex file-content search
│   │       ├── list_dir.rs      # Directory listing
│   │       ├── lsp.rs           # LSP diagnostic/definition requests (feature: lsp)
│   │       ├── memoize.rs       # Knowledge memoization tool
│   │       ├── normalize.rs     # Computed tool-call IDs (NormalizedApiStyle)
│   │       ├── read.rs          # File read with line numbering
│   │       ├── todo.rs          # Task list tracking
│   │       └── write.rs         # File write (create-only, overwrite option)
│   ├── session/
│   │   ├── mod.rs               # Session struct, SessionMessage, compaction, undo (~2176 lines)
│   │   ├── chat_history.rs      # ChatHistoryEntry, JSONL persistence, legacy migration, compaction
│   │   └── storage.rs           # Session CRUD, atomic writes, discovery by prefix/name/workspace
│   ├── provider.rs              # AnyClient: type-erased LLM client, provider backends (~2666 lines)
│   ├── ui/
│   │   ├── mod.rs               # TUI module root
│   │   ├── tui.rs               # Terminal setup/teardown, crossterm lifecycle, transactional attach
│   │   ├── event_handler.rs     # Agent event dispatch → rendering, session save, compaction signals
│   │   ├── renderer.rs          # Incremental markdown rendering, feed blocks, scroll/viewport
│   │   ├── input.rs             # Input editor, slash commands, history, completion
│   │   ├── feed.rs              # Conversation feed: block cache, truncation, streaming
│   │   ├── theme.rs             # Theme engine, ANSI styles
│   │   ├── status.rs            # Status line rendering
│   │   └── welcome.rs           # Welcome screen
│   ├── extras/
│   │   ├── mod.rs               # Feature-gated module declarations
│   │   ├── export.rs            # Session export: JSONL, HTML, Gist sharing (~554 lines)
│   │   ├── status_signals.rs    # Unix-domain socket: start/stop/git-conflict notification
│   │   ├── truncate.rs          # UTF-8 safe truncation helpers (CJK-aware)
│   │   ├── loop/                # Autonomous loop: validation, restart, plan management (feature: loop)
│   │   ├── goal/                # Persistent goals: rounds, gate, judge, checks (feature: goal)
│   │   ├── subagents/           # Nested subagent spawn & dispatch (feature: subagents)
│   │   ├── mcp/                 # Model Context Protocol client (feature: mcp)
│   │   ├── acp/                 # Agent Communication Protocol server (feature: acp)
│   │   ├── js/                  # QuickJS engine, JS tool, sandbox policies (feature: js)
│   │   │   ├── tool.rs          # JsTool, per-call parent policy and services
│   │   │   ├── supervisor.rs    # One serialized contained process and watchdog
│   │   │   ├── broker.rs        # Invocation grants and effect authorization
│   │   │   ├── audit.rs         # Durable parent effect intent/completion chain
│   │   │   ├── protocol.rs      # Closed bounded wire types/state
│   │   │   ├── worker.rs        # Internal worker bootstrap and fresh runtimes
│   │   │   ├── realm.rs         # Private skill/model realms and verification loader
│   │   │   ├── host.rs          # Parent effect services
│   │   │   └── skills/          # Identity, storage, retrieval, admission, lifecycle
│   │   ├── skills/              # Learned skill library, vector search (feature: skills)
│   │   ├── hooks/               # Agent lifecycle hooks (feature: hooks)
│   │   ├── lsp/                 # LSP client integration (feature: lsp)
│   │   ├── memory/              # Persistent memory: long_term, scratchpad, daily, notes (feature: memory)
│   │   ├── multimodal/          # Image/audio/document attachments (feature: multimodal)
│   │   ├── advisor/             # Second-opinion advisor agent (feature: advisor)
│   │   ├── chain/               # Multi-step chain execution (always available)
│   │   ├── archmd/              # ARCHITECTURE.md handling (feature: archmd)
│   │   └── git_worktree/        # Git worktree sandbox integration (feature: git-worktree)
│   ├── sandbox/
│   │   ├── worker/
│   │   │   ├── linux.rs         # Empty-root broker-only bubblewrap
│   │   │   ├── macos.rs         # macOS 26 one-time image, Seatbelt, guardian
│   │   │   └── windows.rs       # LPAC, Job, attestation, full canary gate
│   │   └── mod.rs               # Sandbox infrastructure: rlimit, seccomp, platform launchers
│   ├── event.rs                 # UsageDelta, provider token/cost events
│   ├── auth.rs                  # API key resolution from env vars and config
│   ├── acp_auth.rs              # ACP OAuth2-style local TCP authentication (~10 KB)
│   ├── docs.rs                  # Embeds docs/agent/ markdown files into binary via include_dir
│   ├── fs.rs                    # Filesystem primitives: atomic write, symlink safety, private dirs
│   └── hex.rs                   # Stable lowercase hex encoding for digest consumers
├── docs/
│   ├── agent/                   # Core docs: COMMANDS.md, CONFIG.md, SKILLS.md, SUBAGENTS.md, etc.
│   ├── specs/                   # Phase specs (1-6), platform-paths, subprocess-trust, git-mutations
│   ├── benchmarks/              # Performance benchmark reports & results
│   ├── reviews/                 # Release reviews, adversarial reviews, probes
│   ├── decisions/               # Architecture Decision Records (ADR)
│   ├── plans/                   # Refactor/fix/review plans
│   └── superpowers/             # JS engine blueprint, loop evidence, VSCode distribution, goal design
├── Cargo.toml                   # Workspace + package metadata, 30+ features, dependencies
├── build.rs                     # Embedding build data, SHA2 hashing
├── README.md
├── ARCHITECTURE.md              # This file
└── spike/                       # Standalone spike/prototype (excluded from workspace)
```

---

## Key Types

### Core Abstractions

| Type | Location | Role |
|---|---|---|
| `AnyAgent` | `agent/mod.rs` | Type-erased agent handle, owns runner task and session |
| `AnyClient` | `provider.rs` | Type-erased LLM client (`Arc<dyn AnyClientTrait>`), abstracts OpenAI/Anthropic/Google/etc. |
| `AgentRunner` | `agent/runner.rs` | Public handle to spawned agent: `event_rx: mpsc::Receiver<AgentEvent>`, `abort_handle`, `compaction_decision_tx`, `compaction_enabled` |
| `BtwRunner` | `agent/runner.rs` | Handle to in-flight `/btw` side-question task: `abort_handle`, `task`, `work_scope` |
| `AgentWorkScope` | `agent/runner.rs` | Tracks child work that outlives originating future: cancelled flag, active-children counter, Cancellation/Idle Notify |
| `AgentEvent` | `agent/runner.rs` | Enum: `Token`, `Reasoning`, `ToolCall`, `SubagentToolCall`, `SubagentStarted`, `ToolResult`, `Done`, `Btw*` variants |
| `Session` | `session/mod.rs` | Conversation state: `id`, `name`, `messages: Vec<SessionMessage>`, `usage`, `working_dir`, `compaction: Option<Compaction>` |
| `SessionMessage` | `session/mod.rs` | Message union with `role: MessageRole` and variant-specific payloads |
| `MessageRole` | `session/mod.rs` | `User \| Assistant \| System \| ToolCall \| ToolResult \| SubagentToolCall` |
| `Compaction` | `session/mod.rs` | Compacted prefix summary: original message count, token count, compacted text |
| `ChatHistoryEntry` | `session/chat_history.rs` | `{ content: String, timestamp: CompactString }` — JSONL persistence atom (max 10,000 entries) |
| `ToolResultSpillStore` | `session/mod.rs` | `Arc<Mutex<…>>` handoff from pre-model hook to transcript persistence |
| `Config` | `config/mod.rs` | Central configuration (~70 fields): model, provider, tokens, permissions, features, goal, MCP, JS, shell |
| `PreservedConfig` | `config/mod.rs` | Opaque `BTreeMap<String, toml::Value>` for forward-compat unknown keys |
| `UsageDelta` | `event.rs` | Per-call token/cost increment for billing |

### Agent Event Lifecycle

```
User input → AgentRunner.spawn()
    → AgentEvent::Token          (streamed markdown chunks, throttled rendering)
    → AgentEvent::Reasoning      (dark magenta, gated by slash.show_reasoning)
    → AgentEvent::ToolCall       (rendered "◈" prefix, session saved)
    → AgentEvent::SubagentToolCall (rendered "⌥" prefix)
    → AgentEvent::SubagentStarted  (subagent lifecycle notification)
    → AgentEvent::ToolResult     (handles todo_write specially, show_tool_details gating)
    → AgentEvent::Done / Error / Cancelled
```

### Permission System

| Type | Role |
|---|---|
| `PermCheck` | Four-outcome check: `Allowed`, `AllowedWithCoaching(msg)`, `Denied(reason)`, `Ask` |
| `AskSender` | Tokio oneshot channel for interactive permission prompts |
| Pattern checkers | `check_perm` (string key), `check_perm_path` (file path), `check_perm_canonical_path` (LSP), `check_perm_bound_path` (workspace-relative), `check_mcp_perm` (MCP with server identity) |
| `ReadTracker` | `Arc<Mutex<…>>` — denies repeated reads of unchanged file sections, tracks `(path, offset, limit, version)` where version = `(len, mtime, content_crc32)` |

### Config Types

| Type | Role |
|---|---|
| `Config` | Central config with ~70 fields: model, provider, max_tokens, turn_token_budget, context_window, reserve_tokens, keep_recent_tokens/tool_results, reasoning, retry, no_tools, max_agent_turns, verify_command, tool limits, compaction, permissions, custom_providers, prompt_to_model, api_style, extra_body, temperature, goal, MCP/ACP/LSP, JS sandbox, shell allowlist, embedding |
| `QuickModelConfig` | Predefined model profile: provider, model, costs, context window, reserve_tokens, temperature, extra_body |
| `CustomProviderConfig` | Custom backend: provider_type, base_url, api_key_env, model, context_window |
| `ReasoningConfig` | `effort: ReasoningEffort`, `summary: ReasoningSummary`, `encrypted_content`, `store` |
| `ReasoningEffort` | `None \| Minimal \| Low \| Medium \| High \| Xhigh` |
| `ShowToolDetails` | `Bool(bool) \| Lines(usize)` → resolves to `ResolvedShowToolDetails` (`Off \| Limited \| Unlimited`) |
| `EmbeddingBackendKind` | `Deterministic \| External \| Local` |
| `ApiStyle` | Controls responses vs. completions API path |
| `AdvisorConfig` | `enabled`, `model`, `max_uses`, `human_handoff`, `advisor_kilobytes_limit` |
| `GoalConfig` | `judge`, `continuation`, `max_rounds`, `max_tokens`, `max_active_secs`, `no_progress_rounds`, `blocked_rounds`, `wrap_up_max_agent_turns`, `reinject_every`, `judge_every` |

### Tool Argument Types

| Argument Type | Fields |
|---|---|
| `ReadArgs` | `path`, optional `offset`, `limit` |
| `WriteArgs` | `path`, `content`, `overwrite` (default false) |
| `EditArgs` | `path`, `replace_all`, optional `block`, `file_crc`, `edits: Vec<EditOp>` |
| `BashArgs` | `command`, optional `timeout`, `background` |
| `JobStatusArgs` | `id`, `action: JobAction` (`Poll \| Stop`) |
| `GrepArgs` | `pattern`, optional `path`/`include`/`context_lines`, `case_insensitive`, `files_only`, `count` |
| `FindFilesArgs` | `pattern`, optional `path` |
| `ListDirArgs` | optional `path` |

---

## Control Flow

### Startup Sequence

```
1. main() → CLI args parsed (clap) → all flags, subcommands, model selection
2. Config loaded: TOML files + env overrides + CLI overrides
3. Context files discovered recursively upward:
   - AGENTS.md, ARCHITECTURE.md (unless --no-context-files)
   - Custom prompts, themes
4. Session loaded (--continue by prefix/name/workspace) or created fresh
5. Provider client built: API key resolution via auth.rs, AnyClient construction
6. TUI attached (crossterm transactional terminal setup) or headless mode
7. Agent built via agent::builder → system prompt assembly
8. Event loop started: user input → agent run → streaming events → render
```

### Agent Run Lifecycle

```
1. User submits input (TUI InputEditor or headless stdin/ACP)
2. Input appended to Session as User message
3. Agent built/ensured (lazy, reconnects MCP if eligible):
   System prompt assembled from:
   ├── Base system prompt
   ├── AGENTS.md + ARCHITECTURE.md context
   ├── Tool definitions (JSON schema):
   │   ├── Rust-native tools (read, write, edit, bash, grep, find_files, list_dir, task, memory*, goal_report, js)
   │   ├── MCP tools (discovered at startup, prefixed mcp__)
   │   └── Feature-gated tools (lsp, advisor, etc.)
   └── Session context (compacted prefix + recent messages)
4. Overhead token estimate via agent::builder::estimate_overhead
5. AgentRunner::spawn() → Tokio task:
   a. LLM completion stream via AnyClient.completion_stream()
   b. Token parsing → AgentEvent::Token emission
   c. Tool call detection → JSON arg parsing
   d. Permission check (PermCheck lattice)
   e. Tool execution (bash sandbox, file I/O, JS, subagent, memory, skills)
   f. Tool result injection into completion context
   g. Loop until Done, max_agent_turns, or turn_token_budget exceeded
6. Events streamed via mpsc channel → UI event_handler
7. Round completes with AgentEvent::Done:
   a. UsageDelta applied to Session
   b. Session saved (atomic JSON write)
   c. Chat history appended (JSONL)
   d. Between-turn compaction evaluated
```

### TUI Event Dispatch (`src/ui/event_handler.rs`)

```
AgentEvent received on mpsc receiver:
├── Token          → append to markdown buffer, throttle repaint
├── Reasoning      → render dark magenta (if slash.show_reasoning)
├── ToolCall       → save session, render "◈" prefix, track in-flight
├── SubagentToolCall → render "⌥" prefix
├── SubagentStarted  → render subagent lifecycle notification
├── ToolResult     → handle todo_write specially, apply show_tool_details policy
├── Done           → finalize response segment, apply usage, trigger between-turn compaction
├── Btw* variants  → side conversation event handling
└── Error/Cancelled → render diagnostic, clean up resources
```

### Headless Mode

```
--print / --goal / --loop flags:
  → No TUI, agent runs to completion
  → Events logged to stdout/stderr
  → Exit code reflects goal status (0=met, 1=failed, 2=error)
  → Session saved to disk unless --no-save
```

---

## Data Flow

### Primary Data Path

```
User Input (TUI/CLI/ACP)
    │
    ▼
InputEditor / CLI parse / ACP prompt handler
    │
    ▼
Session.messages.append(User)
    │
    ▼
agent::builder → system prompt assembly
    │   ├── AGENTS.md + ARCHITECTURE.md context
    │   ├── Tool definitions (JSON schema)
    │   ├── Session context projection (compacted prefix + recent)
    │   └── Custom prompt
    │
    ▼
AgentRunner::spawn()
    │
    ├──▶ AnyClient.completion_stream() ──▶ LLM API
    │         │
    │         ▼
    │    Token stream parsing
    │         │
    │         ├── Text → AgentEvent::Token → TUI renderer (feed blocks)
    │         ├── Tool call → parse JSON args → permission check
    │         │         │
    │         │         ▼
    │         │    Tool execution:
    │         │    ├── read / write / edit / bash (sandboxed)
    │         │    ├── grep / find_files / list_dir
    │         │    ├── js (QuickJS worker, capability-brokered)
    │         │    ├── task (subagent spawn, fresh context)
    │         │    ├── memory_read/write/edit/search (persistent memory)
    │         │    ├── goal_report (goal progress notification)
    │         │    ├── todo_write (task list tracking)
    │         │    └── mcp__* (external MCP tools)
    │         │         │
    │         │         ▼
    │         │    AgentEvent::ToolResult → UI renderer
    │         │         │
    │         │         ▼
    │         │    Tool result injected into completion loop
    │         │
    │         └── End turn → AgentEvent::Done
    │
    ▼
Session saved (storage::save_session → atomic JSON write)
Chat history appended (chat_history::append_entry → JSONL)
Usage recorded (event::UsageDelta → Session.usage)
```

### Data Persistence Model

| Data | Storage | Format | Location |
|---|---|---|---|
| Session state | `storage::save_session` | JSON | `state_dir/sessions/{id}.json` |
| Chat history | `chat_history::append_entry` | JSONL (max 10k entries, lazy 2× compaction) | `state_dir/chat_history.jsonl` |
| Goal transcripts | GoalStore | JSON | `state_dir/goals/{id}/round-NNNN.json` |
| Loop state | LoopStore | JSON | `state_dir/loops/{id}.json` |
| Memory (long_term) | memory system | Markdown | `state_dir/MEMORY.md` |
| Memory (notes) | memory system | Markdown | `state_dir/notes/{name}.md` |
| Memory (scratchpad) | memory system | Markdown | `state_dir/scratchpad.md` |
| Memory (daily) | memory system | Markdown | `state_dir/daily/YYYY-MM-DD.md` |
| Skills DB | rusqlite | SQLite + HNSW vector index | `state_dir/skills.db` |
| Skill embeddings | fastembed/ORT | ONNX vectors | `state_dir/skills_embedding/` |
| Tool outputs | `storage::save_tool_output` | Binary | `state_dir/tool_outputs/{hash}` |
| Config | TOML parser | TOML | `~/.config/mini-agent/config.toml` |
| Context files | file reader | Markdown | Project root + parent dirs (recursive upward) |
| JS audit log | `src/extras/js/audit.rs` | Hash-chained segments | `state_dir/js-audit/` |
| Themes | `storage::save_theme_name` | TOML | `state_dir/themes/` |
| Export | `src/extras/export.rs` | JSONL / HTML | User-specified path or Gist |

### Compaction (Two-Level)

**Session compaction** (`Session::compress`): Summarizes oldest messages into a `Compaction` prefix when the context window nears capacity. Frees space for the LLM. The compacted prefix includes message count, token count, and compressed text. Triggered by token threshold checks in the runner.

**Chat history compaction** (`chat_history.rs`): Independent of LLM context management. Flat JSONL file trimmed to 10,000 most recent entries. Uses lazy 2× threshold (compacts at 20,000 lines → rewrites to 10,000) to amortize full-file rewrites. Legacy JSON-array format auto-detected and migrated.

### Mid-Turn Compaction

An interactive runner can request mid-turn compaction. The UI sends a `CompactionBoundaryDecision` over a dedicated channel. The current tool batch finishes, the runner emits exact structured interactions up to the boundary, and the UI compacts that canonical prefix before respawning the continuation. Tool work is never cancelled for compaction.

---

## Design Decisions

### 1. Type-Erased Client and Agent (Trait Objects)
`AnyClient = Arc<dyn AnyClientTrait>`, `AnyAgent` wraps a type-erased agent. Adding a new LLM backend requires only implementing `AnyClientTrait`, not changing consumer code. Dynamic dispatch overhead is negligible compared to network I/O.

### 2. mpsc Channels for Event Streaming
Agent events flow through `tokio::sync::mpsc` channels from the runner task to the UI handler. Decouples async stream processing from synchronous terminal rendering. Separate channels for agent events, user events, and permission requests.

### 3. Sandboxed Tool Execution
Shell commands run in sandboxed worker processes with platform-specific containment:
- **Linux**: `bwrap` empty-root profile, user/PID/network/IPC/UTS namespaces, dropped capabilities, rlimits, `no_new_privs`, seccomp BPF filter denying process/exec/socket syscalls
- **macOS**: Trusted guardian + Seatbelt deny-default sandbox + APFS copy-on-write clone
- **Windows**: LPAC (Least-Privilege AppContainer), Job objects, mitigation policies

JavaScript executes in a separate contained same-exe worker process with a fresh QuickJS runtime per invocation. See [JS Engine Integration](#js-engine-integration) below.

### 4. Transactional TUI Attachment
Terminal setup is transactional: partial setup and every exit restore prior terminal state. On Windows, UTF-8 codepages are active only while the TUI is active; redirected/headless streams are never treated as console handles. External editors, pagers, and support utilities suspend/resume the same lifecycle.

### 5. Incremental Rendering with Feed Block Cache
The TUI conversation feed caches immutable rendered-row segments per semantic block. Streaming invalidates only the active block. Completed history is bounded by 4,096 blocks and 16 MiB of display text; live blocks are bounded to 2 MiB. Truncation is explicit; retention eviction resets scroll/selection before next draw.

### 6. Permission Lattice
Every tool invocation passes through a four-outcome permission check: `Allowed`, `AllowedWithCoaching`, `Denied`, `Ask`. The `Ask` outcome pauses the agent and prompts the user via a oneshot channel. Users can `AllowOnce` or `AllowAlways` (adding a pattern to the session allowlist). Permission patterns support string key, path, canonical-path (LSP), workspace-bound, and MCP-server-identity scoping.

### 7. ReadTracker Deduplication
Prevents the LLM from re-reading unchanged file sections in a single conversation, saving tokens and API costs. Tracks `(path, offset, limit, version)` where version = `(len, mtime, content_crc32)`. Can be disabled entirely.

### 8. Feature Gating (~30 Cargo Features)
Optional subsystems gated behind compile-time flags. Default features: `loop`, `goal`, `git-worktree`, `mcp`, `acp`, `subagents`, `archmd`, `status-signals`, `multithread`, `export`, `js`, `sandbox`, `memory`. Opt-in features: `skills`, `skills-embed`, `skills-embed-dynamic`, `lsp`, `multimodal`, `hooks`, `advisor`, `pdf`. Keeps the default binary lean.

### 9. Forward-Compatible Config
`PreservedConfig` is an opaque `BTreeMap<String, toml::Value>` preserving unknown config keys across version upgrades. Older binaries preserve newer keys on round-trip writes.

### 10. Atomic Writes Everywhere
All persistent writes go through `fs::private_atomic_write_sync`: write to temp file in same directory → sync → rename. Crash safety: previous version always survives. Symlink rejection (`O_NOFOLLOW` / `FILE_FLAG_OPEN_REPARSE_POINT`) and private-directory enforcement on every I/O path.

### 11. ACP Protocol
Local TCP server with OAuth2-style bearer token authentication. Editor plugins connect as agent backends. Goals set via `_meta.goal` on `PromptRequest` (not text directives). Each gate decision emits both an `AgentThoughtChunk` and a `_meta.goal` session update.

### 12. Goal System — Driver-Level Round Relaunch
Goals advance in rounds: one runner run, then one gate evaluation. The **driver** (TUI event handler, headless loop, ACP prompt handler) relaunches rounds — the runner is not goal-aware beyond a cadence reminder. Both `Continue` (retain history) and `Restart` (fresh history + summary) modes ship together. See [Goal System](#goal-system-feature-goal) below.

---

## Feature Modules (Extras)

### Goal System (feature: `goal`)
Persistent multi-round objective tracking. The driver owns the goal lifecycle:
- **Round**: One agent runner invocation
- **Gate**: Post-round evaluation against criteria and tier-2 shell checks
- **Judge**: Optional LLM-based evaluation (auto-selects model, fail-open)
- **Continuation**: `Continue` (retained history) or `Restart` (fresh history + driver-built summary)
- **Bounds**: `max_rounds`, `max_tokens`, `max_active_secs`, `no_progress_rounds`, `blocked_rounds`
- **Wrap-up**: Final round with reduced `max_agent_turns`
- **Terminal statuses**: `Met`, `Impossible` (reopenable)
- **Surface**: `/goal` slash command, `--goal` CLI, `_meta.goal` ACP object, status line, feed line, hooks envelope, headless JSON exit codes

Architecture: `src/extras/goal/` (8 files) — `Goal` struct, `GoalStore`, `Gate` evaluation table, judge resolution, check execution, cadence reminder injection into the system prompt.

### Loop System (feature: `loop`)
Autonomous validation loops. Runs the agent, validates output via `verify_command`, restarts with feedback on failure. Tracks plan artifacts (`LOOP_PLAN.md`). Slated for deprecation in favor of the goal system (slice 5 of goal delivery: reimplements `--loop` as a `Restart` preset).

### Subagent System (feature: `subagents`)
`task` tool spawns read-only exploration subagents with fresh context. Subagents receive the same system prompt preamble (including `AGENTS.md` and `ARCHITECTURE.md`). Configurable tool set, token budget, and turn limit. Results returned as tool results. Uses shared `AgentWorkScope` for cancellation propagation. Tool-call IDs use `subagent_call_` prefix namespace to avoid collisions with provider IDs.

### JavaScript Engine (feature: `js`)

**Execution model**: The parent launches the current executable in an internal worker mode, exchanges closed JSON frames over anonymous pipes, and performs every external effect through policy services. No interpreter in the trusted parent.

```
[trusted parent]
JsTool → process-wide lazy supervisor → platform worker launcher
    |              |                            |
    |              | JSON frames (length-prefixed, bounded)
    |              +--------------------------> [contained same-exe worker]
    |                                           fresh Runtime per request
    |<-- typed effect request ------------------ private skill/model realms
    |
    +→ invocation grant table → permission/narrowing → durable intent
       → parent file/fetch/spawn/proposal service → durable completion
```

- One worker process at a time; lazy creation; may stay warm but QuickJS state is per-request fresh
- Each invocation creates a fresh `Runtime` (64 MiB heap, 512 KiB stack)
- Interrupt deadline installed before evaluation; parent watchdog covers launch, IPC, evaluation, permission waits, effects, and drain
- Worker stdout is protocol-only; diagnostics expose only closed class/code and validated source-free metadata

**Capability broker**: All real effects (file, fetch, command, proposal) execute in the parent. The parent builds an immutable table of opaque invocation-bound grants, intersects each request with session permission policy, target narrowing, backend readiness, and expiry before performing effects. Private realms prevent one artifact from receiving another's source-level API.

**Durable effects**: Hash-chained audit log: validate authority → append intent → perform → append completion. Audit failure before intent performs no effect. Cancellation may produce `OutcomeUnknown` for effects already started; never reports false success/failure or auto-retries.

**Platform containment**: Linux (`bwrap` empty-root, namespaces, seccomp), macOS 26 (Seatbelt + guardian + APFS clone), Windows (LPAC + Job + attestation gate). Platform status is typed and source-free. Backend absence disables JS entirely — never falls back to in-process interpreter.

**Skills & verification**: Agent Skills (instruction/resource packages) and Learned JavaScript Skills (immutable identity-v2 artifacts with SHA-256 identity covering source, tests, exports, metadata, ABI version, capability scopes) are separate. Identity-v1 rows are quarantined. Retrieval occurs once before model generation; the worker never opens SQLite or computes embeddings. Verification uses deterministic in-memory fakes and exact JavaScript boolean `true` semantics.

Source map: `src/extras/js/` (9 files) + `src/sandbox/worker/{linux,macos,windows}.rs`.

### MCP Integration (feature: `mcp`)
Model Context Protocol client for external tool servers. Supports stdio and HTTP transports (via `rmcp`). Tools discovered at startup and merged into agent's tool palette with `mcp__` prefix. Configurable per-server timeout overrides. Permission checks include MCP-server-identity scoping.

### ACP Server (feature: `acp`)
Agent Communication Protocol server: exposes mini-agent over a local TCP socket for editor plugin integration. OAuth2-style bearer token authentication (`src/acp_auth.rs`). Session management, prompt handling with `_meta.goal` support, streaming response emission, session update with `_meta` objects.

### Memory System (feature: `memory`)
Persistent memory across sessions with four targets:
- `long_term`: `MEMORY.md` — deduplicated facts (whitespace-insensitive), always loaded into context
- `scratchpad`: Per-project `[ ]` checklists, auto-injected open items
- `daily`: `daily/YYYY-MM-DD.md` — running log, one file per day
- `note`: `notes/{name}.md` — named reference material

Tools: `memory_read`, `memory_write`, `memory_edit`, `memory_search` (case-insensitive keyword search across all memory files).

### Skill System (feature: `skills`)
Learned skill library with vector search. Skills are markdown procedures stored in SQLite with ONNX embeddings (or deterministic fallback when `skills-embed` is not enabled). HNSW index for nearest-neighbor retrieval. Skills automatically proposed when context matches stored embeddings. Configurable proposal threshold and max skills per suggestion. Separated from JavaScript learned skills (which are immutable identity-v2 artifacts with their own storage and verification regime).

### Hooks (feature: `hooks`)
Agent lifecycle hooks: pre/post tool execution, session start/stop. Hook scripts receive structured JSON on stdin with agent event context. Configurable approval model with `ask` verdict decoupled from the tool's own permission check.

### LSP Integration (feature: `lsp`)
Language Server Protocol client: connects to language servers for diagnostics, definitions, references, hover. Provides `lsp` tool to the agent. Configurable per-langauge server settings. Uses `lsp-types` crate.

### Export (`src/extras/export.rs`)
Session export to JSONL and standalone HTML. GitHub Gist sharing support. Preserves full message history, tool calls, usage data.

### Multimodal (feature: `multimodal`)
Image, audio, and document attachments sent to models supporting multimodal inputs. Uses Rig's image support (`rig/image`). PDFs via raw document messages (no local PDF parser). Feature flag: `multimodal` (enables `rig/image`); `pdf` is an alias for `multimodal`.

### Advisor (feature: `advisor`)
Second-opinion advisor agent: runs a separate model in parallel to review the main agent's decisions. Configurable: `enabled`, `model`, `max_uses`, `human_handoff`, `advisor_kilobytes_limit`.

### Chain (`src/extras/chain/`)
Multi-step chain execution (always available, not feature-gated). Sequences multiple agent turns with intermediate output routing.

### Archmd (feature: `archmd`)
Handles `ARCHITECTURE.md` file discovery, loading, and injection into system prompt preamble for both main agent and subagents.

### Git Worktree (feature: `git-worktree`)
Git worktree sandbox integration: creates isolated worktrees for agent operations, keeping the main working tree clean.

---

## Dependencies

| Crate | Version | Role |
|---|---|---|
| `rig-core` | 0.40 | LLM client abstraction, completion streaming, usage tracking |
| `crossterm` | 0.29 | Cross-platform terminal manipulation, raw mode, styling |
| `tokio` | 1 | Async runtime: I/O, timers, signals, process, sync primitives |
| `clap` | 4 | CLI argument parsing with derive macros and env var support |
| `serde` / `serde_json` / `toml` / `serde_yaml_ng` | — | Serilization: config (TOML), sessions (JSON), chat history (JSONL), YAML |
| `rquickjs` | 0.12 | Embedded QuickJS JavaScript engine (optional, feature: `js`) |
| `rmcp` | 2.0 | Model Context Protocol client (optional, feature: `mcp`) |
| `agent-client-protocol` | 2.0 | ACP server protocol (optional, feature: `acp`) |
| `rusqlite` | 0.40 | SQLite for skill storage (optional, feature: `skills`) |
| `hnsw_rs` | 0.3 | HNSW vector index for skill retrieval (optional) |
| `fastembed` | 5 | ONNX embedding for skills (optional, feature: `skills-embed`) |
| `ort` | 2.0.0-rc.13 | ONNX Runtime bindings (optional, feature: `skills-embed`) |
| `cap-std` | 4.0 | Capability-based filesystem sandboxing |
| `pulldown-cmark` | 0.13 | Markdown parsing for TUI rendering |
| `include_dir` | 0.7 | Embeds `docs/agent/` into binary |
| `reqwest` | 0.13 | HTTP client: LLM APIs, MCP transports, fetch JS effect |
| `chrono` | 0.4 | Timestamps in sessions, chat history, memory |
| `uuid` | 1 | Session and goal identifiers (v4) |
| `sha2` | 0.11 | Content hashing: build fingerprint, CRC alternative |
| `regex` | 1 | Permission patterns, grep tool search |
| `ignore` / `globset` | 0.4 | `.gitignore`-aware file traversal |
| `unicode-width` | 0.2 | CJK text width calculation |
| `compact_str` | 0.10 | Small-string optimization for identifiers |
| `smallvec` | 1 | Inline vectors for small collections |
| `zip` | 8.6 | Session export packaging (deflate) |
| `thiserror` | 2 | Derive `Error` trait for error types |
| `anyhow` | 1 | Flexible error handling with context |
| `tracing` / `tracing-subscriber` | 0.1/0.3 | Structured logging with env-filter |
| `futures` | 0.3 | Async stream combinators |
| `dirs` | 6 | Platform config/data/cache directories |
| `unicase` | 2 | Case-insensitive string comparison |
| `unicode-normalization` | 0.1 | Unicode normalization for matching |
| `http` | 1 | HTTP types for ACP |
| `blocking` | 1 | Synchronous blocking calls (ACP, optional) |
| `nix` (Unix) | 0.31 | Signal handling, process control, terminal, I/O |
| `libc` (Unix) | 0.2 | C library bindings |
| `socket2` (Unix) | 0.6 | Socket configuration for ACP |
| `seccompiler` (Linux) | 0.5 | Seccomp BPF filter compilation for shell sandbox |
| `rustix` (Linux) | 1.1 | Process and thread control |
| `windows-sys` (Windows) | 0.61 | Windows job objects, security tokens, AppContainers, console, file system, pipes |
| `jsonschema` (Windows) | 0.49 | JSON Schema validation (Windows build only) |
| `openssl` (musl) | 0.10 | Vendored OpenSSL for static musl builds |

---

## Entry Points

| Entry Point | Location | Trigger |
|---|---|---|
| `main()` | `src/main.rs` | Binary invocation |
| `run_interactive()` | `src/main.rs` | TUI mode (default) |
| `run_headless()` | `src/main.rs` | `--print`, `--goal`, `--loop` flags |
| ACP server | `src/extras/acp/` | `--acp` flag → local TCP listener |
| MCP client connect | `src/extras/mcp/` | On agent build when MCP servers configured |
| Slash commands | `src/ui/input.rs` | `/` prefix in TUI input |
| Goal gate | `src/extras/goal/` | After each agent run round, before continuation |
| Loop driver | `src/extras/loop/` | `--loop` / `/loop` autonomous iteration |
| Subagent spawn | `src/extras/subagents/` | `task` tool call from agent |
| JS tool | `src/extras/js/tool.rs` | `js` tool call from agent |
| Skills retrieval | `src/extras/skills/` | Automatic: skill proposal on context match |
| Memory tools | `src/extras/memory/` | `memory_read/write/edit/search` tool calls |

---

## Subprocess Trust Classes

Per `docs/specs/subprocess-trust.md`, distinct trust classes govern different process boundaries:

| Class | Trust | Containment |
|---|---|---|
| Project/global hooks | Trusted automation; may need workspace state and credentials | Standard process |
| MCP/LSP children | Long-lived workspace services; own configuration trust | Standard process |
| Loop validation (`verify_command`) | Explicit user-configred command | `Sandbox::wrap_command` |
| `!` explicit shell | Human shell with ambient authority | None |
| Model-authored Bash | Untrusted model output | `Sandbox::wrap_command` with limits |
| Model-authored JS | Untrusted model output | Broker-only worker profile (see JS Engine Integration) |
| JS worker (broker-only) | Contained same-exe worker | Platform-specfic max containment |

---

## Spec Framework

`docs/specs/` contains a phase-based specification system:

| File | Content |
|---|---|
| `00-index.md` | Spec index, extension rules, promotion criteria |
| Phase specs 1-6 | Core agent capabilities: basics, tool exec, integration, verification, skills, evaluation |
| `platform-paths.md` | Directory ownership: state dirs, goals dir, cache dirs, loops dir |
| `subprocess-trust.md` | Trust model for shell execution, verification commands, and worker boundaries |
| `structured-git-mutations.md` | Git operation atomicity and safety invariants |

Specs carry normative front matter and extension maps. New features (like goals) amend existing specs before promotion.

---

## Testing Strategy

| Layer | Location | Scope |
|---|---|---|
| Unit tests | `src/**/*.rs` (inline `#[cfg(test)]`) | Individual functions, types, status transitions |
| Integration tests | `src/tests/` | Agent lifecycle, tool execution, session persistence |
| Headless tests | `tests/headless_json.rs` | End-to-end with mocked LLM, exit code verification |
| Loop tests | `src/tests/loop_tests.rs` | Validation cycles, restart behavior |
| Goal tests | `src/tests/goal_tests.rs` | Goal lifecycle, gate table, judge integration |
| Provider tests | `src/provider.rs` (test module) | Mock provider streams, error handling |
| MCP tests | `tests/mcp.rs` | MCP server integration |
| Benchmark tests | `tests/bench/` | Performance benchmarks for critical paths |
| Spec tests | `tests/spec/` | Spec compliance verification |

---

## Build Configuration

| Profile | Opt-level | Debug | LTO | Codegen Units | Strip |
|---|---|---|---|---|---|
| `dev` | 0 | false | — | 16 (rig-core: 16) | — |
| `release` | "z" | false | thin | 1 | true |

Special dev overrides:
- `rig-core`: overflow-checks disabled (upstream aggregates Usage with uncheked arithmetic in debug; mini-agent accounts from exact per-call events)
- `matrixmultiply`, `hnsw_rs`, `anndists`: opt-evel 3 for skill retrieval performance in debug builds
- Static musl builds: `openssl` with `vendored` feature

Minimum Rust version: edition 2024. Workspace resolver 2.

---

## Relationship to Other Documents

| Document | Role |
|---|---|
| `AGENTS.md` | Coding conventions, instructions, project-specfic procedures ("how to work in this codebase") |
| `ARCHITECTURE.md` (this file) | High-evel design: structure, relationships, rationale ("how this codebase is built") |
| `docs/agent/COMANDS.md` | Slash command reference |
| `docs/agent/CONFIG.md` | Configuration reference (all keys, default values, sensitive keys) |
| `docs/agent/SKILLS.md` | Skill system documentation |
| `docs/agent/SUBAGENTS.md` | Subagent usage and configuration |
| `docs/agent/MEMORY.md` | Memory system documentation |
| `docs/agent/TOOL_RUNTIME.md` | Tool execution runtime details |
| `docs/specs/00-index.md` | Normative specifications (overrides this file where they conflict) |
| `docs/decisions/` | Architecture Decision Records (ADR) for significant choices |