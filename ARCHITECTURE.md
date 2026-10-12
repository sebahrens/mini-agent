# Architecture Overview — mini-agent v1.9.4

mini-agent is one Rust binary with a terminal UI, headless and ACP entry points,
provider abstractions, native tools, and a brokered QuickJS worker. This overview
is injected as architecture context; detailed contracts belong in the linked
specifications and operator documentation.

## Directory layout

```text
src/
  main.rs                  Binary dispatch, CLI parsing and Tokio runtime
  cli.rs                   Flags and configuration resolution
  startup.rs               Workspace, session, permissions and provider setup
  provider.rs              AnyClient and AnyAgent provider dispatch
  config/                  Configuration types, loading and project trust
  paths.rs                 AppPaths and retained WorkspaceBinding capabilities
  fs.rs                    Stable filesystem access and atomic publication
  permission/              Permission modes, target checks and approval bridge
  context/                 AGENTS/CLAUDE context, prompts, themes and personas
  agent/
    builder.rs             Tool registration and system-prompt assembly
    runner.rs              Streaming runs, work scopes, cancellation and usage
    tools/                 Native file, shell, task-list and diagnostic tools
  session/                 Transcript projection, persistence and input history
  ui/
    app.rs                 Interactive application loop and input dispatch
    event_handler.rs       Agent events, rendering and transcript updates
    events.rs              Session replay and welcome output
    feed.rs                Bounded conversation blocks and rendered-row cache
    input/                 Input editor, cursor movement and picker input
    markdown.rs            Incremental markdown presentation
    permission_handler.rs  Interactive approval handling
    pickers/               Command, file, model, shell-history and rewind pickers
    prebuild.rs            Background agent preparation
    renderer.rs            Feed rendering, viewport and selection
    slash/                 Slash command dispatch and handlers
    state.rs               Shared application and turn state
    statusline.rs          Status-line presentation
    terminal.rs            Transactional terminal attach, suspend and restore
    utils.rs               Presentation helpers and output sanitization
  extras/
    acp/                   Agent Client Protocol server
    mcp/                   External MCP clients and tool registration
    lsp/                   Language-server registry, transport and diagnostics
    goal/                  Persistent objectives, pure gate, checks and driver
    loop/                  Loop preset and workflow validation
    subagents/             Bounded read-only child-agent tasks
    hooks/                 Lifecycle hooks and trust-controlled dispatch
    memory/                Persistent memory tools
    js/
      tool.rs              JS tool registration and parent-side services
      supervisor.rs        Worker lifetime, protocol and watchdog
      broker.rs            Invocation grants and effect authorization
      host.rs              Parent-owned file, network and process effects
      audit.rs             Durable effect intent/completion records
      protocol.rs          Wire types and state machine
      worker.rs            Internal worker bootstrap and request dispatch
      realm.rs             Model/skill realms and verification
      memory.rs            Bounded worker allocator
      pipe.rs              Worker transport
      session.rs           Invocation/session ownership
      types.rs             Parent-local JS result types
      vendor/              Vendored JavaScript validation assets
      skills/              Identity, store, retrieval, admission and lifecycle
      tests/               JS, worker and learned-skill regression tests
  sandbox.rs               General subprocess policy, limits and cleanup
  sandbox/
    git_metadata.rs        Filesystem authority for Git execution metadata
    windows.rs             General Windows AppContainer launcher
    worker.rs              Separate broker-only worker containment API
    worker/                Linux, macOS and Windows worker launchers
  tests/                   Cross-module tests compiled into the binary harness

tests/
  headless_json.rs          End-to-end headless output and provider fixtures
  macos_preflight_stderr.rs Production macOS preflight diagnostics
  worker_bootstrap.rs       Production worker dispatch before normal startup
  harness_eval/             Data-driven task and persona fixtures

docs/agent/                 Operator reference embedded into the binary
docs/specs/                 Normative architecture and security contracts
spike/                      Research crate excluded from production workspace
```

Other optional extras include advisor, architecture context, exports, attachments,
chains and Git worktrees; see `src/extras/mod.rs` and `Cargo.toml` for declarations.

## Main ownership boundaries

- `AnyClient` and `AnyAgent` in `src/provider.rs` dispatch provider-specific
  clients and Rig agents. `src/agent/builder.rs` registers tools and assembles
  the system prompt from context, selected prompt/persona and tool guidance.
- `AgentRunner` and `AgentWorkScope` in `src/agent/runner.rs` own streaming
  work, usage events, cancellation and child cleanup. The TUI consumes typed
  events; headless and ACP use the same underlying tools and permissions.
- `Session` in `src/session/mod.rs` retains canonical messages, structured
  tool calls/results, usage and compaction. Explicit `!command` output is one
  user message. Stored tool-call identities support model-context replay.
- `WorkspaceBinding` in `src/paths.rs` retains directory identity and file
  capabilities. Rebinding a workspace changes workspace-owned context without
  changing the process-wide configuration and storage roots.
- The general process sandbox in `src/sandbox.rs` serves shell and brokered
  subprocess effects. The JavaScript worker uses the distinct fail-closed
  containment launcher in `src/sandbox/worker.rs`.

The canonical JS containment checklist is in
[Phase 6](docs/specs/phase-6-brokered-js-runtime.md#phase-6-security-invariants-canonical).
The parent owns permissions, grants, effects and auditing; QuickJS state belongs
only to the contained worker. Platform guarantees and limitations are defined
there, rather than duplicated in this overview.

## Request flow

1. `src/main.rs` dispatches internal worker/platform helper modes before ordinary
   startup, then parses CLI arguments and creates the runtime.
2. `src/startup.rs` resolves configuration, workspace context, permission policy,
   session selection, provider and optional services.
3. Interactive mode enters `src/ui/app.rs`; `src/ui/slash/` handles slash
   commands. Headless mode enters the runner without attaching a terminal;
   ACP requests enter through `src/extras/acp/`.
4. The runner streams provider responses, dispatches tools under permission
   checks, records exact interactions and returns usage and terminal outcome.
5. The caller updates the session and presentation. Compaction summarizes a
   canonical transcript boundary; it does not silently discard in-flight tools.
6. Goal rounds in `src/extras/goal/` evaluate evidence before continuing. `--loop`
   is a goal preset and shares its driver.

The feed caches completed rendered rows and bounds retained display text.
Terminal attachment and suspension share one lifecycle, including Windows
console state. Persistent transcripts remain separate from display retention.

## Persistence and configuration

`AppPaths` in `src/paths.rs` is the storage authority. `--print-config` shows
resolved roots. Configuration uses `ZS_CONFIG_DIR`, then the selected global
home (`MINI_AGENT_HOME` or `~/.mini-agent`), or an existing platform-specific
legacy `zerostack` root. File priority is TOML, YAML, YML, then JSON; project
configuration lives under `.zerostack/config.toml` and has a separate trust gate.
See [configuration locations](docs/agent/CONFIG.md) and
[platform paths](docs/specs/platform-paths.md) for override/migration rules.

| Artifact | Resolved location |
| --- | --- |
| Configuration | `config_dir/config.toml` (or selected YAML/JSON file) |
| Session transcripts | `state_dir/sessions/` |
| Input history | `state_dir/chat_history.json` (JSONL content) |
| Saved tool output | `state_dir/tool-outputs/` with per-session opaque directories |
| Goal records | `state_dir/goals/` |
| Prompt/theme/agent files | `data_dir/prompts/`, `data_dir/themes/`, `data_dir/agents/` |
| Persistent memory | `data_dir/memory/` |
| Learned-skill database | `local_data_dir/skills/skills.db` |

## Features and checks

Default features are `loop`, `goal`, `git-worktree`, `mcp`, `acp`, `subagents`,
`archmd`, `status-signals`, `multithread`, `export`, `js`, `sandbox` and `memory`.
`loop` also requires `goal`. Opt-in features include `skills`, `skills-embed`,
`skills-embed-dynamic`, `lsp`, `multimodal`, `hooks`, `advisor` and `pdf`.
`Cargo.toml` owns the current feature graph and dependency versions.

Rust unit tests live beside their implementation and in `src/tests/`. Goal gate
and driver tests live in `src/extras/goal/`; MCP tests are
`src/tests/mcp_http_tests.rs`, `src/tests/mcp_oauth_tests.rs` and
`src/tests/mcp_stdio_tests.rs`. JS regression and resource measurements live in
`src/extras/js/tests/`. Workflow-policy tests live in `scripts/tests/`.
Production bootstrap/headless checks and harness fixtures live in `tests/`.
Native platform gates separately prove actual containment on hosted runners.

Use `cargo test` with filters for affected code, `cargo install --path . --debug`
for a production debug build, and `cargo fmt` before committing. Repository
instructions and CI define the required feature and native-platform checks.

## Further reading

- [Agent instructions](AGENTS.md): contribution workflow and repository invariants.
- [Commands](docs/agent/COMMANDS.md) and [configuration](docs/agent/CONFIG.md).
- [Tool runtime](docs/agent/TOOL_RUNTIME.md) and [subagents](docs/agent/SUBAGENTS.md).
- [Specification index](docs/specs/00-index.md): normative contracts and precedence.
- [Subprocess trust](docs/specs/subprocess-trust.md): distinct process trust classes.
