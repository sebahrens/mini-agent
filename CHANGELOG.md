# Changelog

Notable changes to mini-agent are documented in this file. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Security

- Compaction is now injection-isolated: tool output, files or earlier summaries can no longer close
  the summarizer's `</message>`, `</transcript>`, `</previous_summary>` or `</user_instructions>`
  fences to forge user turns or contract updates, the summarizer is told fenced history is data, and
  a summary flushed to the daily memory log can no longer forge a separate log entry.

## [1.9.4] - 2026-09-29

Versions 1.9.0 to 1.9.2 were not published from this repository. The `v1.9.3` tag was created but
its release build was cancelled after CI found a Linux/Windows build break, so nothing was published
as 1.9.3; all of its changes ship in 1.9.4.

### Security

- Workspace hook scripts and executables are now bound to their content: a hook file the agent
  rewrites is blocked (not run) until it is re-approved on the next start, and file tools refuse to
  edit pinned hook files. Bare hook commands resolve only through absolute `PATH` entries.
- Permission rules are stricter and deterministic: `external_directory` rules no longer depend on
  hash-map order, relative allow rules such as `**/*.rs` no longer grant writes outside the
  workspace, deny rules also match case variants on macOS and Windows, and "always allow" grants
  match exactly. `"*": ask` now applies to unmatched in-workspace edits in standard mode, and
  `--yolo` asks before recognisably destructive shell commands (denied in headless runs).
- Sandboxed commands on macOS can no longer read stored API keys or OAuth tokens when the mini-agent
  home or config directory is reached through a symlink or `/tmp`/`/var`. The `zerobox` backend no
  longer passes provider API keys to model commands. The JS worker's macOS profile no longer allows
  opening arbitrary named FIFOs.
- Model and tool output can no longer inject terminal escape sequences: output, status-line values
  and picker entries are sanitised, and pasted text is normalised.
- ACP refuses non-loopback TCP binds unless `MINI_AGENT_ACP_ALLOW_INSECURE_REMOTE=1` is set. The
  advisor keeps `store=false`, zero-retention routing and other provider settings. MCP stdio output
  lines are capped at 16 MiB and an oversized line stops the server.
- Oversized `AGENTS.md`/`CLAUDE.md` files are truncated to the context budget instead of being loaded
  whole, and the completion judge only accepts an exact `met`, `not_yet` or `impossible` verdict.

### Fixed

- Global and managed hooks keep workspace-file digests across sessions (a changed file asks, or
  fails closed when headless), and `$ZEROSTACK_PROJECT_DIR` paths are content-bound. Each ACP turn
  runs hooks against its own workspace, so concurrent sessions in different repositories work with
  hooks. An `external_directory` allow is no longer overridden by `"*": ask` for external reads.
- ACP goal rounds count Gemini thinking tokens, and ACP no longer repeats its active recap in the
  memory block. Completion-judge calls are charged at the judge model's own prices (unpriced judge
  tokens are flagged), `/goal pause` stops a running check and judge, and a running `/loop` follows
  `/worktree` to the new workspace's `LOOP_PLAN.md`.
- The permission prompt keeps the status line, replayed sessions show each tool result under its
  call, a space after a full command such as `/model` opens its picker, and argument pickers follow
  monochrome mode.
- Terminal: exiting (including repeated Ctrl+C, `kill`, SIGHUP or closing the terminal) restores the
  terminal cleanly, shows the cursor and saves the session; no stray escape sequences are printed.
  Long streamed paragraphs repaint live.
- Permission prompts wrap long paths, summarise multi-line scripts, and the transcript can be
  scrolled while a prompt is waiting. Long scripts appear as one summarised line in the transcript.
- Input: long lines soft-wrap; Alt/Ctrl+arrows move by word and Alt+Backspace deletes a word;
  Ctrl+H is always backspace (lazygit moved to Ctrl+O); idle Ctrl+C clears a draft before quitting;
  whitespace-only input is never sent. Pickers no longer crash or desync when the caret moves, `@`
  completion edits the right mention, and backing out of an argument picker restores command
  completion.
- Slash commands: `/add` accepts several and quoted paths, `/reasoning on|off` is explicit, `/toggle`
  validates its argument, `/rename` respects `--no-session`, failed provider or prompt-model switches
  leave the session consistent, and an unreachable gateway no longer freezes every command.
- Goals and loops: a `goal_report` followed by no closing text completes the round; Ctrl-C stops a
  pending completion judge; `/goal pause` during verification is no longer overwritten; the judge
  sees which checks passed; judge tokens count toward the goal budget; `--no-session` writes no goal
  records; headless runs never stop on the ARCHITECTURE.md offer and `--loop-max 0` exits at once;
  `/loop` reads `LOOP_PLAN.md` from the workspace (capped at 32 KiB); JSONL export/import keeps the
  goal and session totals.
- Headless: `--output json` prints one clean JSON line whose `result` is the final answer; piping into
  `head` no longer panics or loses the session; SessionEnd hooks run on every exit.
- Providers and sessions: `--provider X` uses X's default model, `OPENROUTER_MODEL` only applies to
  OpenRouter, Gemini thinking tokens are billed, resumed sessions respect pinned or quick-model
  context windows, `--session NAME` prefers an exact name, compaction and memory no longer send the
  same summary twice, and custom MCP servers named like a built-in survive config saves.
- ACP: the `task` tool works; `--quick-model`, `--api-key` and `[retry]` are honoured; internal
  failures return JSON-RPC errors and failed tools are reported as failed; editor-supplied stdio MCP
  servers are connected; `_meta.goal` is not applied to a rejected prompt; with hooks configured,
  concurrent prompts in different workspaces are refused rather than sharing one hook root.
- MCP and LSP: denying OAuth ends the wait immediately; binary results appear as placeholders; LSP
  evicts old documents with `didClose` instead of silently stopping, and answers string-id requests
  and `workspace/configuration` correctly. The advisor keeps the newest message when it is oversized.
- Subagents: a report that breaks the output contract gets one repair turn before the host fallback;
  task output accounting no longer cuts the closing fence or misreports partial results; `/agent`
  and the `task` schema state that personas do not change tools or permissions.
- The structured `git` tool's `log` returns correct fields, including root commits. `--worktree`
  creates worktrees next to the repository from subdirectories and refuses to nest them.
- Memory keeps the newest daily-log entries, truncates (rather than drops) a large `MEMORY.md`, and
  locks concurrent writes. Skill imports are crash-safe and clean up leftovers. Ctrl-C quits the
  setup wizard.
- Installer: upgrades replace the binary atomically, private-repo or authentication failures are
  reported as such, PATH entries are matched exactly with a warning for shadowing binaries, and `~`
  is expanded in the install directory.
- Windows: sandboxed hooks are reported unavailable with an accurate message instead of a misleading
  bwrap error, and cancelling or timing out a command no longer stalls the app.
- CI runs the full test matrix when documentation embedded in the binary or tests changes, compiles
  and runs the hooks rows on Windows, runs strict Clippy on every test row, and records named-FIFO
  denial on both macOS worker-gate runners.
- Windows trusted hooks run in a kill-on-close Job, so timeouts and cancellation end their whole
  process tree without the helper's cooperative wait. Starting an agent turn with the `acp` and
  `hooks` features no longer overflows the stack, and several load-sensitive process tests are
  fixed.

### Added

- ACP TCP can be served over TLS (`MINI_AGENT_ACP_TLS_CERT`/`MINI_AGENT_ACP_TLS_KEY`) with a
  certificate-bound handshake; editor-supplied stdio MCP servers run in the sandbox by default
  (`MINI_AGENT_ACP_TRUST_CLIENT_MCP=1` opts out).
- Opt-in `terminal_notify` (OSC 9/777) and `terminal_prompt_marks` (OSC 133), double-click word
  selection, `e` to expand a shortened permission request, and opt-in `session_title_model` for
  generated session titles.
- The installer retries private releases with `GITHUB_TOKEN` or an authenticated `gh`, still
  verifying checksums. Superseded Agent Skill versions are pruned once unused (newest two and
  anything from the last seven days are kept).
- Terminal title shows `mini-agent: working`, `waiting for approval` or `idle` for multiplexers such
  as herdr (`terminal_title = false` disables it).
- Tool results appear directly under their tool call; mouse selection copies exactly the dragged text
  with a transient notice; a `↑ N` marker shows history above the viewport; the status line can
  show the reasoning effort.
- `/model` is the single model selector (quick aliases, then provider models); compatibility aliases
  are hidden from completion. `--resume` and `/sessions` show a title for each session. `/goal status`
  lists the last ten rounds and attempts.

### Changed

- Delivered the 2026-09-05 harness review amendments across brokered JavaScript containment,
  learned-skill identity, admission, retrieval, telemetry, lifecycle operations, and
  publication. The owning specifications now record these as delivered amendments.
- Added background learned-skill work — index rebuild and publication, raw-telemetry retention
  compaction, automatic fault-based quarantine, and the opt-in proposal/admission workers — and
  explicit learned-skill operator commands, top-level `await` support, a completion-verification
  gate, structured history persistence, `skills_search`, and WAL-backed skill storage. There is no
  background decision scheduler: that module is compiled only into the test build.
- Hardened model-command containment, hook and permission scoping, replacement lineage,
  promotion evidence, and task-outcome attribution following the 2026-09-06 review.
- Added the deterministic paired `task.json` learned-library regression harness and operator Skill
  Gym scripts for isolated task mining, seed-library preparation, paired runs, and non-production
  outcome reporting. Added task utility columns to learned-skill statistics.
- Fixed LSP workspace rebinding and bounded diagnostics, ACP cancellation ownership, VS Code
  request cancellation, session/history persistence, and parallel-test shared-state races.
- Made the learned-skill operator surface usable end to end following the 2026-09-07 review: added
  `--promote-learned-skill` (promote an approved replacement canary over an active or quarantined
  predecessor, preserving lineage) and `--retire-learned-skill` (administrative disable that keeps
  the revision and its lineage), added proposal listing and per-proposal admission outcomes, guarded
  `--purge-learned-skill` against non-terminal targets and silent re-rooting, and counted attributed
  negative or severe feedback on an invocation that returned as a behavioural fault. Rollback and
  repair remain library-level contracts with no operator command; the unused decision-scheduler and
  visibility-snapshot modules were moved behind `#[cfg(test)]`.

## [1.8.0] - 2026-09-06

### Added

- Added a native Agent Client Protocol (ACP) extension for VS Code, including workspace-trust
  enforcement, chat participants, configuration commands, and five platform-specific VSIX release
  candidates.
- Added a structured Git tool with bounded, permission-checked `status`, `diff`, `log`, `show`,
  `stage`, `unstage`, and `commit` operations. Mutations never expose raw shell text, remotes, or
  network access.
- Added a dual-purpose Windows x86-64 MSI for per-user and managed installation, including the CLI,
  licensing materials, and the bundled VS Code extension.
- Added brokered QuickJS execution and the opt-in learned-skill library. Fresh contained workers,
  typed parent-owned effects, verification, retrieval, canary promotion, quarantine, repair, and
  rollback keep reusable agent-authored code within explicit capability boundaries.
- Added native JavaScript worker containment for Linux, macOS, and Windows. Production fails closed
  when a platform cannot prove the required boundary.
- Added Rust, Python, and Node/TypeScript lifecycle maintainer subagent personas with a
  caveats-first structure, a ten-step lifecycle investigation method, and contract tests.
- Added canonical provider interaction persistence: completed turns record structured tool calls
  and results with their call IDs so resumed sessions carry auditable, correlated tool transcripts.
- Added bounded `/add` context preloading (at most 20 files, 512 KiB per file, 8 MiB aggregate)
  that reads file content once at add time.
- Added an optional `turn_token_budget` setting that caps cumulative per-turn token usage
  independently of `max_tokens`.
- Added build, rebuild, and resident-memory phase gates to the skill retrieval benchmark.
- Added a CI lint and test row for the opt-in `hooks`, `advisor`, `lsp`, `multimodal`, and `pdf`
  features, which no default or focused row compiled before.

### Changed

- Improved agent, JavaScript, skill-retrieval, session, Git, and terminal hot paths through cached
  immutable metadata, JSON Lines session persistence, incremental Markdown rendering, and reduced
  worker bootstrap overhead.
- Hardened release packaging with full and lite archives, vendored Corresponding Source, software
  bills of materials, checksum manifests, native VSIX candidates, and the Windows installer.
- Expanded continuous integration (CI) to lint every Rust target, test the VS Code extension, audit
  npm dependencies, and exercise isolated Cargo feature combinations.
- MCP servers now connect and discover tools concurrently (at most eight at a time) and report
  handles, tools, and notices in stable server-name order.
- Request preflight now counts the pending user prompt and attached media toward the compaction
  decision, so headless `-p` dispatch and the interactive path compact, or reject an irreducible
  request, before sending it to the provider.
- Compaction defaults now scale with the model's context window (`reserve_tokens` defaults to a
  tenth of the window with a 16384 floor; `keep_recent_tokens` to a twentieth, clamped to
  10k-50k) instead of fixed 128k-era constants.
- Automatic compaction now also runs at safe boundaries between loop iterations and before
  headless provider dispatch of resumed print sessions.
- Specialist subagent guidance now uses verifiable source-discovery procedures and one canonical
  Phase 6 security contract instead of duplicated inventories.
- Release builds pass `--locked`, the release workflow derives every VSIX and SBOM file name from
  the Cargo package version read once in its first job, and `just sync-version` now also covers the
  VS Code manifest and lockfile, `editors/vscode/SOURCE.md`, `packaging/windows/README.md`, and
  `docs/acp-registry.json`.
- `just sync-version` resets package recipe digests to an obvious placeholder when the version
  changes, and `check-package-metadata.py` rejects digests copied from the previous release tag;
  `just post-release` is the only step that records real digests.

### Fixed

- Fixed AArch64 Unix file opens to use target-specific `libc` flags instead of x86 constants, and
  prepared Bubblewrap/user namespaces before Linux release-archive runtime smokes.
- Fixed concurrent VS Code chat and command startup so they share one session creation, and made
  stop, workspace changes, and trust changes invalidate in-flight creation safely.
- Fixed status-bar ownership so each extension session disposes its item exactly once.
- Fixed permission precedence so explicit deny rules override built-in `todo_write` and plan-file
  conveniences.
- Fixed Git mutation advertising by implementing the previously declared `stage`, `unstage`, and
  `commit` operations with literal paths, symlink rejection, serialized index changes, and commit
  messages delivered through standard input.
- Fixed multiple Windows lifecycle, containment, Unicode clipboard, installer, and workspace
  authority edge cases.
- Fixed compaction so only messages actually included in the bounded summarizer input are deleted;
  a truncated summarizer input no longer discards older, unsummarized history.
- Fixed the compaction `first_kept_index` formula, bounded-serialization fallback for tiny budgets,
  prompt-pressure accounting, and bounded context recovery requests.
- Fixed the cumulative turn budget reusing `max_tokens` as its limit, which aborted multi-tool-call
  turns with a spurious exhaustion error.
- Fixed the JavaScript tool to enforce one 30-second absolute deadline across skill preparation,
  effect services, and supervisor execution instead of independent per-phase timeouts.
- Fixed the `btw` side-question path to bound in-flight concurrency and cancel tasks on teardown.
- Fixed specialist subagent contracts to fail closed, isolated project specialist overrides by
  workspace binding, and hardened specialist prompt contracts.
- Fixed tool result `call_id` extraction so persisted tool calls and results stay paired.
- Fixed the VS Code extension to report `clientInfo.version` from its manifest, and added a
  regression test proving a pathologically deep AJV schema fails closed with a sanitized keyword
  while the realm keeps validating.
- Fixed the skills benchmark's optional RSS assertion, which broke compilation of every
  `--features skills` CI job.
- Fixed the ACP `initialize` response to report the Cargo package version instead of a stale
  literal.
- Fixed the GitHub Pages workflow, which still built the pre-flattening `docs/` layout, so it
  publishes `docs/agent` again.
- Fixed `docs/vscode-acp-setup.md` and `docs/acp-registry.json`: the TCP config key is `type`,
  the native extension exposes six commands and two settings over stdio only, TCP authentication
  is `[acp_servers.<name>].api_key`, and tool names are `read`, `write`, `edit`, and `list_dir`.

#### 2026-09-03 final review

- Fixed macOS Seatbelt profiles denying workspace writes when a workspace binding is active.
- Fixed compaction deleting unsummarized messages and re-firing after a completed pass.
- Fixed the edit tool corrupting files when a whitespace-normalized match was applied.
- Fixed duplicate tool-call persistence in the TUI.
- Fixed untrusted project prompts escalating the permission mode.
- Fixed permission precedence to be deterministic across rule sources.
- Fixed relative deny patterns not matching absolute paths.
- Fixed a multi-line bash command bypassing deny rules.
- Fixed planwrite mode.
- Fixed the `--setup` wizard discarding edits.
- Fixed custom-provider API key fallback.
- Fixed the verbose log filter.
- Fixed headless persistence order.
- Fixed session-id panics.
- Fixed hashedit range validation.
- Fixed edits of non-UTF-8 files to fail closed.
- Fixed the sandboxed git identity.
- Fixed bash output caps.
- Fixed MCP timeouts and tool-name collisions.
- Fixed JavaScript console output not being surfaced.
- Fixed TUI stdin contention in `/undo`, `/init`, `/tutor`, and the memory editor.
- Fixed non-ASCII line editing.
- Fixed mid-stream output handling.
- Fixed `/memory` write reachability.
- Fixed a scroll underflow.
- Fixed VS Code permission detail and stderr surfacing.

### Security

- Updated the VS Code extension's Vitest, Vite, and esbuild development toolchain to patched
  versions; the CI high-severity npm audit now completes with zero known vulnerabilities.
- Vendored AJV 8.12.0 now has a byte-for-byte SHA-256 integrity test tied to its reviewed upstream
  artifact, and its MIT notice now ships in `NOTICE` (binary archives, MSI) and the VSIX
  third-party inventory.
- Compaction now isolates untrusted transcript data from summarizer instructions with XML-element
  encapsulation and a system-prompt contract, so injected role labels, delimiters, or prompt
  placeholders cannot escape the data section.
- Bumped `h2` to 0.4.18 to resolve RUSTSEC-2026-0258 (unbounded empty DATA frames denial of
  service).
- Replaced the yanked `chacha20` 0.10.1 with 0.10.2 in `Cargo.lock`, restoring the cargo-audit
  yanked-crate gate.
- The temporary `RUSTSEC-2026-0187` exception for `lopdf` 0.41.0 is renewed through November 23,
  2026. `rig-core` 0.42 splits and removes runtime APIs used throughout mini-agent, so the migration
  remains tracked separately; untrusted PDF ingestion must remain disabled until it lands.

### Known Limitations

- The Phase 6 worker baseline remains explicitly `pending_external_runs` with no platform evidence.
  Version 1.8.0 therefore makes no cross-platform worker-performance claim. Maintainers must collect
  and aggregate the Linux, macOS, and Windows CI artifacts before publishing measured results.
- Marketplace and Open VSX publication of the native extension remains a separate manual step.

Thanks to sebahrens and platon2001 for the release work.

[Unreleased]: https://github.com/sebahrens/mini-agent/compare/v1.9.4...HEAD
[1.9.4]: https://github.com/sebahrens/mini-agent/compare/v1.8.0...v1.9.4
[1.8.0]: https://github.com/sebahrens/mini-agent/compare/v1.7.2...v1.8.0
