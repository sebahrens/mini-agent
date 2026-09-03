---
title: "review: v1.8.0 final pre-release review findings"
type: review
status: completed
date: 2026-09-03
epic: mini-agent-0yme
---

# v1.8.0 final pre-release review

Thirteen independent read-only reviewers were fanned out over the whole tree on 2026-09-03
(core loop, tools, permissions/config, unix sandbox, Windows sandbox, JS runtime core, JS skills,
MCP/ACP/LSP/hooks, extras, TUI, release engineering, dead-code sweep, VS Code extension).
Every finding below was verified against the code with `file:line` evidence. Where reviewers
disagreed (compaction) the orchestrator read the code and settled it; the macOS Seatbelt finding
was reproduced in-process before being ranked P0.

Tracking: epic `mini-agent-0yme`, label `release-1.8.0-review`. Each P0/P1 has its own issue;
P2/P3 are grouped per area.

## Baseline gates at HEAD 5ee52f1

| Gate | Result |
|---|---|
| `cargo test` (default features) | 1746 passed, 1 failed (flaky `stale_sweep` lock test, passes alone), 6 ignored |
| `cargo fmt --check` | pass |
| `cargo clippy --all-targets -D warnings -A dead-code` (default) | **fail** (5 lints locally, 6 in CI on toolchain 1.96) |
| `cargo test --features lsp,hooks,advisor,multimodal` | **does not compile** (`resumed_history_tests.rs:121`) |
| CI on `main` | **red for the last 5 runs** (clippy rows, fmt job's python gate, two Windows jobs, phase-6 gate) |
| `pages.yml` | red for the last 3 runs |
| Python checkers (`scripts/tests`) | 146 passed, 2 failed (`test_check_feature_graph`) |

## P0 — release blockers

| ID | Issue | Finding |
|---|---|---|
| R0-1 | mini-agent-8n35 | CI is red on `main`. Clippy 1.96 lints (`provider.rs:433`, `fs.rs:2469`, `session/mod.rs:774`, `ui/slash/mod.rs:491`, `sandbox/worker/linux.rs:1234`, `extras/js/tests/worker_containment.rs:488`; locally also `ui/app.rs:1003,1013`, `ui/mod.rs:301,306`). `scripts/tests/test_check_feature_graph.py:192,210` mutate a clippy command string that no longer matches `ci.yml:172`. Windows `config_persistence_permissions_tests.rs:282` panics; AppContainer runtime probe exits 1. `release.yml` has no CI status gate, so a tag would publish on a red commit. |
| R0-2 | mini-agent-rps3 | macOS Seatbelt with a workspace binding makes the workspace read-only. `sandbox.rs:900-904` returns `/dev/fd/197` as the workspace authority path; the Seatbelt profile (`sandbox.rs:1301-1339`) grants writes only under that literal path. Reproduced: `printf ws > file` fails with "Operation not permitted" under the default macOS config. The existing real-backend test (`sandbox.rs:4047`) sets no binding. |
| R0-3 | mini-agent-ekwd | Compaction deletes the wrong messages. `serialize_conversation_bounded` (`provider.rs:398-463`) counts included messages from the end of the slice; `Session::compress` drains from the front. The TUI (`ui/slash/mod.rs:463`) uses `cut_idx - messages_included`, deleting the oldest unsummarized messages and keeping the summarized ones; when everything fits it drains nothing and auto-compaction re-fires every turn. Headless (`startup.rs:443`) drains the whole slice and discards whatever the single-request budget excluded. Root cause: the rolling multi-request summarizer (`provider.rs:486-536`) is pre-truncated to one request at `provider.rs:284` and again in `bounded_recent_conversation`. |
| R0-4 | mini-agent-l80s | The edit tool's whitespace-normalized SEARCH match maps to the wrong byte range (`edit.rs:139-194` assumes 1:1 bytes, but `normalize.rs:5-16` trims trailing whitespace and collapses blank lines). File `"foo   \n    bar\n"`, SEARCH `"\tbar"`, REPLACE `"    baz"` becomes `"foo     bazar\n"` and the tool reports success. No test covers the normalized branch. |
| R0-5 | mini-agent-h41j | The TUI persists every tool call and result twice: live at `event_handler.rs:122/144`, then again from `Done.interactions` in `handle_agent_done` (`event_handler.rs:329-352`, commit b25ccfe). Resume doubles tool output; large outputs write a second untracked artifact file. |
| R0-6 | mini-agent-sxsm | An untrusted checkout escalates the permission mode. `context/prompts.rs:44-50` loads `.zerostack/prompts/*.md` without a trust check and `startup.rs:1244-1249` applies its `%%mode=yolo` directive even under `--guarded`/`--read-only`, bypassing the project-config trust gate (`config/load.rs:22-52`). `/prompt <name>` (`ui/mod.rs:66-98`) has the same hole. |

## P1 — fix before release

### Permissions and configuration
- R1-1 `mini-agent-wmpq` allow/ask precedence is nondeterministic (`permission/mod.rs:17-21` HashMap, `checker.rs:170-175`, `:381`, `:431` last-match-wins). The documented example flips per process.
- R1-2 `mini-agent-xhkz` relative deny patterns never match the absolute paths tools pass (`checker.rs:671-683`); `secrets/**` is bypassed by absolute spelling.
- R1-3 `mini-agent-sucq` bash deny regexes are defeated by a newline (`pattern.rs:161-190` no `(?s)`; yolo `checker.rs:409-412` allows); `rm -rf /\n` runs in yolo.
- R1-4 `mini-agent-dobf` `default_permission_mode = "planwrite"` or any typo silently resolves to Standard (`permission/mod.rs:124-131`).
- R1-5 `mini-agent-0pom` the `--setup` wizard discards every provider/model edit (`setup/mod.rs:820-823` and the handlers that mutate throwaway clones).
- R1-6 `mini-agent-afj7` custom-provider key resolution falls back to the built-in provider's key and sends it to a third-party `base_url` (`auth.rs:87-111`).

### Core loop and sessions
- R1-7 `mini-agent-xq32` `--verbose` log file filters on `zerostack=` but the crate target is `mini_agent` (`logging.rs:113`); the log captures none of the crate's events.
- R1-8 `mini-agent-ut1v` headless `-p` persists the answer before its tool calls (`startup.rs:1426-1461`); `run_print_with_stream_policy` (`runner.rs:2177`) returns only the last stream's interactions; empty terminal responses are persisted.
- R1-9 `mini-agent-i9rh` `&s.id[..8]` panics on short or non-ASCII session ids (`print.rs:247`, `startup.rs:658`, `ui/slash/session.rs:20,323`); `/import` accepts such ids.

### Tools
- R1-10 `mini-agent-wj43` hashedit ranges with descending line numbers panic (`edit.rs:503-513`); line 0 and overlapping ranges are accepted.
- R1-11 `mini-agent-ottz` `edit` rewrites non-UTF-8 files through `from_utf8_lossy` (`edit.rs:651-654`), corrupting untouched bytes.
- R1-12 `mini-agent-ibzr` sandboxed `git commit` runs with no `HOME`, so the global identity is invisible (`git/runner.rs:397-405`; `sandbox.rs:1670-1680`, `:1716`).
- R1-13 `mini-agent-el8c` shell output is uncapped by default and the resource-limit error embeds up to ~1.5 MiB (`bash.rs:96-99`, `:166-192`; `config/mod.rs:505-507`).

### MCP, ACP
- R1-14 `mini-agent-2jz5` MCP HTTP connect, `tools/list`, OAuth client build and `call_tool` have no timeouts (`mcp/client.rs:235-266`, `mcp/mod.rs:143`, `oauth.rs:1319-1337`, `mcp/tool.rs:79`); one stalled server hangs startup or a turn.
- R1-15 `mini-agent-e1vw` tool-name collisions between MCP servers silently drop one tool (`mcp/tool.rs:36-38`, `builder.rs:630-637`).
- R1-16 `mini-agent-7tw0` `docs/vscode-acp-setup.md:73-79` uses `transport = "tcp"` but the serde tag is `type` (`acp/config.rs:5-9`); the guide's config cannot parse. Also describes extension settings that do not exist.

### JS runtime and skills
- R1-17 `mini-agent-nq9p` `JsTool::call` drops the worker's `console` records and typed `diagnostic` (`js/tool.rs:1010-1017`); `console.log` is advertised but never returned. All seven Phase 6 invariants are enforced with tests; the vendored AJV hash was recomputed and matches.
- R1-18 `mini-agent-65d2` the learned-skill pipeline is write-only in the shipped binary: `propose_skill` proposals are evaluated then park forever because held-out suite import, human approval, activation, promotion, retention and purge exist only behind `cfg(test)` or have no production caller (`held_out.rs:330-332`, `admission.rs:944-960`, `lifecycle.rs:246-263`, `session.rs:293-340`). Decision needed: ship a minimal `/skills` operator surface, or stop registering `propose_skill` and mark the docs library-only.

### TUI
- R1-19 `mini-agent-r4ho` `/undo` reads raw stdin while the crossterm event thread polls the same tty (`ui/slash/session.rs:420-424`).
- R1-20 `mini-agent-y439` `/init`, `/tutor` and `/memory editor` run stdin consumers without rebinding the event thread; `docs.rs:28-30` calls `process::exit` inside the TUI.
- R1-21 `mini-agent-s6oj` Ctrl+U, Ctrl+K and Alt+Y use the byte cursor as a char index (`ui/input/mod.rs:317-343`, `:395-417`); the file picker has the same class of bug.
- R1-22 `mini-agent-d1rq` output written mid-stream (`/btw` answers, "queued:" lines) is erased and later tokens are mis-attached (`event_handler.rs:368-375`, `:859-863`).
- R1-23 `mini-agent-cuqo` `/memory write` and `/memory read daily|note` are unreachable because the dispatcher splits into at most three parts (`ui/slash/mod.rs:516` vs `memory.rs:167-176`).
- R1-24 `mini-agent-r4nu` scroll-indicator arithmetic underflows when the viewport grows while scrolled (`renderer.rs:709-714`); tiny terminals underflow u16 (`:1286`, `:1348`).

### VS Code extension
- R1-25 `mini-agent-r1oj` the permission modal shows only the tool name and a UUID; the command text is in `toolCall.content` and "Allow always" whitelists an unseen pattern (`extension.ts:218-221`).
- R1-26 `mini-agent-djxi` the Rust binary's stderr is dropped at the default log level, so startup failures have no reason (`session.ts:174-178`); `~` in `executablePath` is not expanded.

### Release engineering
- R1-27 `mini-agent-t6rk` Homebrew, AUR and conda recipes carry v1.7.2 digests under v1.8.0 URLs.
- R1-28 `mini-agent-acq2` CHANGELOG 1.8.0 omits 22 post-bump commits, including two security fixes; `release.yml` uses that section as the release body.
- R1-29 `mini-agent-9l0b` features `hooks`, `advisor`, `lsp`, `multimodal`, `pdf` are never compiled in CI; the hooks test target does not compile.
- R1-30 `mini-agent-s5wq` `pages.yml` references files deleted in the monorepo flatten.
- R1-31 `mini-agent-2tqf` release builds are not `--locked`, so shipped binaries may not correspond to the audited and vendored dependency graph.
- R1-32 `mini-agent-ij6h` version literals outside `sync-version.sh` (13 in `release.yml`, VS Code `package.json`, registry, `AGENT_VERSION = "1.0.5"`) make the next bump fail late.
- R1-33 `mini-agent-h7ij` `stale_sweep::HeldLock` releases its flock only on close; forked children keep it alive, so the lock test is flaky under a parallel `cargo test`.

### Windows (deferred: cannot be compiled or exercised on this machine)
- R1-34 `mini-agent-6qf6` granted workspace files are opened without `FILE_SHARE_DELETE` and the handles are retained for the command's lifetime (`windows.rs:2615-2695`); delete/rename of any pre-existing file fails (git commit, rm, mv, cargo build, npm install).
- R1-35 `mini-agent-0lg3` every launch rewrites explicit ACEs over the whole workspace and cache with `WRITE_DAC`, a 250k-entry cap and a single-link requirement (`windows.rs:150`, `:2625-2769`).
- R1-36 `mini-agent-xbpq` shells under Program Files cannot be granted, so the shell tool is silently disabled; a per-machine MSI refuses to start (`windows.rs:2571-2613`, `startup.rs:776-782`).
- R1-37 `mini-agent-vaei` Job limits (512 MiB per process, 1 GiB per job, 60 s CPU) kill ordinary builds (`windows.rs:164-167`, `:3662-3669`).
- R1-38 `mini-agent-7by6` a timed-out command leaves a stale ACL journal whose 5 s recovery wedges every later launch in large workspaces (`sandbox.rs:2689-2729`, `windows.rs:2957-3520`).
- R1-39 `mini-agent-ml0o` concurrent startup preflights race on the ACL mutex (JS holds up to 6 s, the general helper waits 5 s), turning a soft JS failure into a startup refusal (`startup.rs:170-186`).

No FFI or memory-safety defects were found in the ~12k lines of Windows code; struct sizes, buffer lifetimes, handle ownership and `windows-sys` feature coverage all check out.

## P2 and P3 (grouped issues)

| Issue | Area | Highlights |
|---|---|---|
| mini-agent-ksrh | Docs drift | CONFIG.md (about 30 undocumented keys, false local-trust claim, temperature note), MEMORY.md (four wrong claims), SUBAGENTS.md, HASHEDIT.md describes an unimplemented design, COMMANDS.md keybindings wrong (Ctrl+D quits, Ctrl+S/L absent, Tab is not the picker), `/help` drift, acp-registry tool names and `--api-key` note, `00-index.md` Phase 6 evidence claim, CLAUDE.md Windows spawn note, undocumented CLI flags. |
| mini-agent-fo49 | Tools | read tracker after shell/git, `list_dir` skips symlinks, fuzzy fallback cost, git stage directory-operand filter bypass, overlapping edits, mixed EOL normalization, new files 0600, git tool silently absent. |
| mini-agent-fccu | Permissions | unvalidated rule tool names, merged config saved to the global file, setup copies env secrets, Windows `check_bound_path`, deny not enforced in readonly modes, `accept_all` overrides CLI flags, dead `resources.rs`, unnormalized `check_path`. |
| mini-agent-17l0 | MCP/ACP/hooks | ACP echoes the client protocol version, ACP sessions get no MCP or hooks, `warn!` corrupts the TUI, hooks trust hash uses `DefaultHasher`, blob results uncapped, fail-open PreToolUse undocumented. |
| mini-agent-dtg5 | JS runtime | parent trusts worker result size, build mismatch reported as transport failure, pre-intent denials unaudited, spawn preparation before session check, watchdog and rquickjs spec drift. |
| mini-agent-1lri | Skills | quarantine on lifetime counters, `CapabilityDenied` unreachable, `SkillStore::purge` FK violation, retention with no surface, deterministic embedding hashes the batch index (default build is effectively FTS-only). |
| mini-agent-quv0 | Extras | `/export` cwd and overwrite, dead chain parser, `--loop --continue` ignores the session, `--continue` silent on error, `/loop` uncapped, `memory_edit` permission key, cancelled subagent cost. |
| mini-agent-jl9s | TUI | paste-burst counts mouse events, Ctrl+C ignored in permission prompts, tiny-terminal underflow, CJK width overflow, wide tables, orphan `docs.rs`, `/models-add` arguments, panic message wiped, silent worktree no-ops. |
| mini-agent-gkus | Sandbox | zerobox launched by bare name, seatbelt tests lack a binding, unsandboxed fallback only logged, Windows verbatim paths, WSL `bash` resolved first, `Global\` mutex. |
| mini-agent-tqvn | Release | conda `--all-features`, AJV notice not shipped, no artifact provenance, stale `test-install-checksums.sh`, VSIX packaging not in CI, `.vscodeignore`, justfile uses `cargo build`. |
| mini-agent-h89u | Dead code | stale `allow(dead_code)`, test-only helpers in the production API, orphan files, unused Feed helpers, 89 dead-code warnings in the off-by-default feature build. |
| mini-agent-tbxw | Tracker | 82 pre-existing open beads issues, many phase-gate items already delivered; needs triage. |

## Areas reviewed with no findings

Workspace boundary containment (capability-relative opens, `O_NOFOLLOW`, identity re-checks), git argument hygiene, bash process lifecycle, project-config trust store, path resolution, ACP TCP auth, headless ask fail-closed, stdio MCP lifecycle and env hygiene, OAuth storage and PKCE, LSP process reaping, hooks exec-form handlers, subagent containment and cancellation, worktree transactions, export HTML sanitization with CSP, JS framing/state machines/broker/supervisor/audit, all seven Phase 6 invariants, skills SQL/migrations/index consistency/feature gates, terminal restore on panic, markdown renderer termination, Windows FFI correctness, release workflow targets and checksum verification, dependency policy and action pinning.

## Fix status (2026-09-03, same session)

Eleven fix agents ran in parallel on disjoint file sets; the orchestrator integrated, formatted
and re-ran the full gate suite (default and off-by-default feature rows, clippy with
`-D warnings`, Python checkers, VS Code typecheck/lint/tests, `cargo install --debug`).

Fixed and verified in this session:

- All six P0s: R0-1 (local gates, the Python gate, and the new feature row; Windows jobs remain,
  see below), R0-2, R0-3, R0-4, R0-5, R0-6.
- P1s R1-1 through R1-17, R1-19 through R1-33 (everything except the Windows set and R1-18).
- New CI row `--no-default-features --features hooks,advisor,lsp,multimodal,pdf` for clippy and
  tests, plus the rotted tests it exposed (`resumed_history_tests.rs`, the LSP diagnostics
  byte-budget test).
- New config keys: `mcp_tool_timeout_secs` (default 120 s) and a default
  `max_bash_output_lines` of 2000 (0 disables).
- Behaviour changes worth knowing: `/undo` no longer prompts and never stashes; use
  `/undo stash`. Project-sourced prompts can only lower the permission mode. Custom providers
  no longer inherit a built-in provider's API key. `default_permission_mode` rejects unknown
  values at startup. Duplicate MCP tool names are namespaced `<server>__<tool>`.

Deferred (open issues):

- R1-18 `mini-agent-65d2`: whether to ship a minimal `/skills` operator surface or stop
  registering `propose_skill`. This is a product decision.
- R1-34 to R1-39 (Windows AppContainer helper) and the two red Windows CI jobs. These cannot be
  compiled or exercised on macOS; they need a Windows host or CI iteration.
- All P2/P3 grouped issues.
- The ACP server does not transmit `suggested_pattern` to clients, so the VS Code "Allow always"
  label can only say a rule for the exact input will persist; a small `_meta` addition on the
  Rust side would let the client show the real pattern.
