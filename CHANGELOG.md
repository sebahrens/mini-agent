# Changelog

Notable changes to mini-agent are documented in this file. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Security

- The `js` tool's `list_dir`, `glob` and `grep` now honour per-file path deny rules like the built-in
  walkers: with `read = { "config/secrets/**" = "deny" }` they no longer list or search those files.
  JS file effects also treat `$HOME/...` like `~/...` (the home directory, checked by the ambient
  path policy) instead of a literal `$HOME` directory inside the workspace.
- `/share` no longer uploads on its own: it first states the export's size, how many tool outputs it
  contains and that a secret gist is readable by anyone with the link, and uploads only on
  `/share confirm`. The upload now gives up after 10 s connecting or 30 s in total instead of
  freezing the UI.
- The Linux JS worker's seccomp filter now also denies `ptrace`, `process_vm_readv`,
  `process_vm_writev`, `kcmp` and `pidfd_getfd`, so a compromised worker cannot attach to the
  unfiltered bubblewrap init in its PID namespace (on `ptrace_scope=0` hosts) and fork/exec through it.
- Worktree merges, auto-commits and `/undo stash` can no longer run a filter or driver configured in a
  repository the model nested inside the workspace: Git descends into such repositories, so their
  `.git/config` is now probed (recursively) and its filter/diff/merge commands neutralised, and an
  over-deep or unreadable nesting refuses the operation with an error naming the nested repository.
- The "sandbox backend is unavailable" warning now names the recorded cause, for example Ubuntu's
  AppArmor restriction on unprivileged user namespaces for bubblewrap.
- Misspelled or unknown top-level config keys are no longer silently dropped: startup (and
  `--print-config`) warns with the nearest known key, keys of builds with other Cargo features stay
  quiet, and every kebab-case key (`permission-deny`, `sandbox-backend`, `permission-modes`,
  `js-fetch-origins`, ...) now also accepts its snake_case spelling instead of ignoring it (fail-open).
  TOML parse errors now name the line and column, still without a source excerpt.
- The shell installer no longer trusts the release origin alone: after the `SHA256SUMS` check it
  runs `gh attestation verify <archive> --repo sebahrens/mini-agent` when a signed-in GitHub CLI is
  available and aborts if the build provenance does not verify (without `gh` it warns and prints the
  command; `MINI_AGENT_SKIP_ATTESTATION=1` opts out). `scripts/update-release-checksums.sh` now
  refuses to pin a package-recipe digest that disagrees with the release `SHA256SUMS` or fails
  attestation. The README shows pinned installs (`bash -s -- --release X.Y.Z`) and the verify command.
- URL (streamable-HTTP) MCP servers can no longer exhaust memory with oversized responses: JSON
  bodies and individual SSE events are read incrementally under the same 16 MiB cap as stdio
  protocol lines (OAuth servers included), an oversized SSE answer fails its request with an error
  naming the cap instead of hanging until the timeout, and at most 4 KiB of a non-2xx body reaches
  the error text (mini-agent-f2wne).
- Sandboxed commands, JS `spawn`, sandboxed hooks, and workspace services can no longer rewrite the
  workspace's `.git/config`, `hooks`, `info`, `modules`, `config.worktree`, `commondir`, or a
  linked worktree's gitfile (so they cannot plant `core.fsmonitor`, hooks, or filter drivers), nor
  rename `.git`; `git add`/`git commit` inside the sandbox still work. On Linux this needs
  bubblewrap 0.8.0 or later (older versions log a warning) and covers entries that exist at launch. In `standard` mode the write/edit tools now also ask before
  changing `.git/config` or anything under `.git/hooks` unless a configured rule decides.
- The permission prompt can no longer show a different command than the one that runs: bidi
  override/embedding/isolate controls, LRM/RLM/ALM, line/paragraph separators and zero-width format
  characters in a request are shown as visible `<U+XXXX>` markers ("Trojan Source"), and are removed
  from chat output and replaced with `�` in picker entries. ZWJ/ZWNJ and right-to-left text are kept.
- `[retry]` is now bounded: every policy is clamped at load to 1–10 attempts and backoffs of at most
  60 s (with a startup warning per clamped field), a retry never sleeps longer than `max_backoff_ms`,
  and an untrusted project `.zerostack/config.toml` may only tighten the user's global/default retry
  policy, so a cloned repository can no longer spin a zero-delay retry loop or hang headless runs.
- Replayed sessions (`--continue`/`--resume`, double-Esc rewind, session or worktree switch, `/memory`,
  `/init`) no longer paint stored escape sequences: user, assistant and system messages and the
  welcome line's directory name are sanitised, and the chat feed and painter now strip control
  sequences from all text so only the renderer's own colours and hyperlinks reach the terminal.
- Compaction is now injection-isolated: tool output, files or earlier summaries can no longer close
  the summarizer's `</message>`, `</transcript>`, `</previous_summary>` or `</user_instructions>`
  fences to forge user turns or contract updates, the summarizer is told fenced history is data, and
  a summary flushed to the daily memory log can no longer forge a separate log entry.
- The model-facing Git tool no longer runs workspace-defined filter drivers: `status`, `diff`,
  `stage`, `unstage` and `commit` (including their before/after snapshots) refuse with an error
  naming the `filter` attribute when a tracked path is bound to a repository-configured
  `filter.<name>.clean`/`.process` command, and run with every configured driver emptied.
  Previously an auto-allowed `status` could execute a clean filter the model had written into
  `.git/config` and `.gitattributes`.
- When the default sandbox backend is unavailable on Linux or macOS and the session falls back to
  running unsandboxed, the built-in auto-allows for commands that run workspace code (`cargo
  build`/`test`/`check`/`clippy`/`fmt`, `pip list`, `git status`) are withheld and ask first. The
  fallback is now always visible: a stderr notice in every mode (even with `RUST_LOG=off`), a chat
  notice before the first TUI turn, and a persistent red `sandbox:off` status-bar segment.
  Decision: Linux keeps starting in this degraded state rather than failing closed like Windows,
  because stock Ubuntu 24.04 blocks `bwrap` by default; use `--sandbox` to fail closed or
  `--no-sandbox` to opt out deliberately (mini-agent-cfib7).
- Releases are gated on CI: a new `ci-success` check aggregates every CI job, and the release
  workflow builds and publishes nothing until the tagged commit is on `main` and its tag CI run
  reports `ci-success`; publication runs in the `release` environment, and `just release` /
  `just add-tag` run `cargo test --locked` before tagging.
- `grep`, `find_files` and `list_dir` now honour per-file path `deny` rules while walking: files and
  directories denied to `read` (or to the walking tool itself, or by an `external_directory` deny)
  are skipped, so `grep` over `.` can no longer return the contents, and `find_files`/`list_dir` the
  names, of files the user denied to `read`. Previously only the search root was checked.
- A `custom_providers` entry named after a built-in alias (`custom`, `openai`, `google`, ...) with
  a third-party `base_url` no longer inherits the vendor key (`OPENAI_API_KEY`, `api_keys.openai`,
  ...); it uses only its own `api_key_env` or `api_keys` entry, unless it points at the vendor's
  own endpoint or sets the new opt-in `inherit_builtin_key = true`.
- Hooks and workspace services (MCP stdio and LSP servers, contained Git) now start in their own
  session with no controlling terminal, as sandboxed model commands already did, so they can no
  longer inject keystrokes into the TUI (`TIOCSTI`) or write escape sequences to your terminal.
- Language servers and stdio MCP servers now resolve bare executable names only through absolute
  `PATH` entries, as hooks already did: an empty or relative entry (`::`, `.`) can no longer make a
  workspace-planted `rust-analyzer`, `gopls` or MCP server binary run unsandboxed. A relative
  command with a directory component is rejected for LSP and needs an explicit `cwd` for MCP.
- Git commands mini-agent runs itself on the host (the statusline `git_status`/`git_changes`
  refresh, the headless JSON change list, worktree create/merge/auto-commit/cleanup, and
  `/undo stash`) no longer run anything the sandboxed agent can write into the repository: hooks,
  `core.fsmonitor`, repository-configured filter/diff/merge drivers, credential helpers, and
  `core.sshCommand` are neutralised, and Git gets an allow-listed environment without API keys
  (SSH agent and proxy variables only for fetch/pull). A fetch/pull is refused while the
  repository's own config sets a remote `uploadpack`/`receivepack` or `core.gitProxy`.
- A project `.zerostack/config.toml` statusline showing `git_status` or `git_changes` now needs
  project-config trust before it can turn on the background host `git status` refresh.

### Added

- `SECURITY.md` documents private vulnerability reporting through GitHub, supported versions,
  response targets and scope; the README and the new-issue page point security reports there, and
  `CONTRIBUTING.md` describes the build, test, documentation and beads workflow.
- Linux prerequisites are documented in the README and Getting started guide: installing
  bubblewrap, and the Ubuntu 23.10+/24.04 AppArmor user-namespace restriction with a per-binary
  profile or the `kernel.apparmor_restrict_unprivileged_userns=0` sysctl (and its trade-off). The
  AUR package lists `bubblewrap` in `optdepends`, and `install.sh` warns on Linux when `bwrap` is
  not on `PATH`.
- The README, Get Started guide, Windows packaging README and release guide now state that the
  Windows MSI/exe and macOS binaries are unsigned and not notarized, explain the SmartScreen and
  Gatekeeper prompts, show how to verify downloads with `SHA256SUMS` and `gh attestation verify`,
  and point AppLocker/WDAC users to hash rules until code signing exists.

### Fixed

- "Always allow" for `grep` and `find_files` now grants the approved search root and its literal
  subtree instead of a `first-word*` prefix glob, so approving `/a/other` no longer covers
  `/a/other-secrets` and a root containing a space is no longer cut at the space.
- The subprocess trust spec now states that unsandboxed Bash and explicit `!` shells clear the
  ambient environment and restore only the non-credential allow-list for every disabled-sandbox
  reason, matching the implementation.
- A workspace-relative `read`, `edit` or `write` through a symbolic link (for example
  `CLAUDE.md -> ../AGENTS.md` or `packages/x -> ../shared/x`) now names the link and suggests the
  absolute target path instead of reporting a raw "Too many levels of symbolic links".
- `todo_write` rejects lists over 50 items or items over 500 characters, and the todo block
  re-injected into every compaction summary is bounded to 32 KiB, so large todo content can no
  longer pin the context and force repeated compaction.
- A support utility such as lazygit now writes its terminal audit record before its process group
  leaves the active set, the same cleanup-then-audit-then-accounting order as the explicit shell,
  so a finished lifecycle always has its record. The caller-drop test no longer loses that record
  under a loaded parallel test runner.
- Hook, command and support-command cleanup no longer signals a process group after its leader has
  been reaped unless the group still has a live member, so a recycled pgid belonging to an unrelated
  process can no longer be sent SIGTERM/SIGKILL; lingering descendants are still terminated.
- A headless (`-p`, `--loop`, `--goal`) run no longer swallows a second Ctrl-C/SIGTERM while its
  cleanup stalls: the second signal, or 10 s after the first, saves what the turn returned and exits
  with status 130 without waiting for the stalled work.
- Sessions, config and other saved state are now `fsync`ed (file before the rename, directory after
  it), so a crash or power loss can no longer leave the session `--continue` would pick undecodable.
- A JS step that called `result(...)` and then kept issuing effects (for example a loop of more
  than 256 caught `scratch_get` calls) no longer fails with "JavaScript worker violated its
  protocol": effects after an accepted result are denied inside the worker without reaching the
  parent or its audit log, and the accepted value is returned. A `result(...)` step also keeps the
  warm JS worker instead of forcing a cold relaunch on the next call.
- `read`, `write`, `edit`, `list_dir`, `grep` and `find_files` now treat a `$HOME/...` path like
  `~/...`: it resolves to the home directory through the ambient permission check instead of being
  created as a literal `./$HOME/...` tree inside the workspace while the result named the home path.
  Reported paths are the paths actually touched on disk.
- A failed Linux bubblewrap preflight now says why. A missing `bwrap`, and the AppArmor
  `setting up uid map: Permission denied` refusal on Ubuntu 23.10+/24.04, each produce a
  diagnostic that names the cause and the fix, both in sandbox startup and launch errors and in
  the JS runtime's "unavailable" reason. Previously the sandbox or `js` tool was reported
  unavailable with no explanation.
- Two processes can no longer silently lose each other's turns by resuming the same session: the
  owning process holds an advisory lock on `sessions/<id>.lock`, `--continue`/`--session`/`/sessions`
  on a session another running process owns continue in a forked copy with a notice, and a save of a
  session owned elsewhere is refused instead of overwriting it.
- Context compaction is now charged: the usage of every rolling summarizer request (up to 16
  full-context requests) is added to the session's token and cost totals, the headless JSON `usage`
  and `cost`, and a running goal's token count and `max_tokens` bound (mini-agent-i6q98).
- A JavaScript `spawn()` whose output is mostly control bytes (for example NUL) no longer turns an
  already-executed command into "JavaScript worker violated its protocol": each stream is truncated
  by its JSON-escaped size and flagged `*_truncated`, and any effect result too large to send is
  reported as `too_large` (read-only) or an unknown outcome (mutating) instead.
- CI now runs the `harness-regression` job (the bounded deterministic harness evaluation and the only
  Gym entrypoint smoke) on pushes as well as pull requests, and `ci-success` fails if it is skipped
  while code changed; it had been pull-request-only and so never ran for work landing on `main`.
- A JS call that passes an out-of-scope path, origin, method or program into an active learned skill
  no longer quarantines it immediately. The effect is still denied and audited, but the call is now
  recorded as `threw` (`scope_miss`) telemetry behind the behavioural threshold; only an operation
  outside the skill's declared capabilities is a capability-policy fault that quarantines at once.
- macOS CI now fails when the real Seatbelt tests for the general command sandbox and JS `spawn`
  skip because Seatbelt is unusable: they run with `MINI_AGENT_REQUIRE_REAL_SANDBOX=1`, which turns
  the skip into a failure, instead of silently passing.
- The MCP OAuth callback listener no longer drops a legitimate browser redirect on macOS. The accepted
  socket inherited the listener's non-blocking flag, so a redirect whose bytes had not yet arrived was
  answered with 400 and the login waited out its full timeout. The listener now blocks with a bounded
  read timeout and reassembles a split request line.
- The GPL Corresponding Source archive now includes every npm package tarball pinned by the VS Code
  extension's `package-lock.json` (whose runtime dependencies are bundled into the VSIX's
  `dist/extension.js`), verified against the lockfile integrity hashes, with offline rebuild steps
  in `SOURCE.md`; the archive is now written reproducibly (sorted entries, fixed timestamps).
- `cargo test` no longer fails intermittently on macOS in the ACP Bash cancellation test under the
  parallel runner: its readiness wait is split into bounded stages (tool announced, shell pid, child
  pid) that each report which one stalled, and CI now runs the unserialised default suite on macOS
  as well as Linux.
- The goal round-clock test that proves time waiting on a permission prompt does not count against a
  goal's time budget no longer flakes on loaded macOS CI runners: it now injects the instants instead
  of bounding real sleeps from above.
- On macOS, a failed JavaScript containment preflight (for example when mini-agent runs inside
  another sandbox) no longer prints CI tokens such as `MACOS_CONTAINMENT_MATRIX_FAILED=launch` on
  stderr during `--print-config`, `-p` or TUI startup; the worker's unavailable reason now names the
  failed stage in a sentence. Set `MINI_AGENT_CONTAINMENT_EVIDENCE=1` to get the tokens back.
- The README and Get Started source installs now use `cargo install --path . --locked` (the
  `--debug` build moved to CONTRIBUTING.md), and the learned-skills Git install is pinned with
  `--locked --tag vX.Y.Z`. The weekly model-catalog refresh runs with job-scoped permissions and no
  persisted checkout credentials, and dispatches CI for its pull request. Dependabot now also watches
  the VS Code extension's npm dependencies.

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
