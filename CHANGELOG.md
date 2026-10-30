# Changelog

Notable changes to mini-agent are documented in this file. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

**Fork release 1.14.2 (2026-10-28)**

### Security

- `pulldown-cmark` is updated to 0.13.5 for RUSTSEC-2026-0414 (exponential event growth with
  `ENABLE_WIKILINKS`).

### Fixed

- Skill Gym episodes now set `js-read-roots` and `js-write-roots` to the episode workspace, so the
  `js` tool's file effects work in gym runs instead of being denied.
- `propose_skill` guidance now documents the hidden `_cap` first parameter of stored exports, and a
  rejected proposal reports a bounded, path-free `detail` (the failing embedded test or rule) with
  its settled outcome instead of only `embedded verification failed`.
- `--learned-skill-stats` adds a `gym_invocations` column that breaks non-production
  (`MINI_AGENT_GYM`) invocations out of `invocations`.
- `docs/agent/SKILLS.md` shows `workspace_prefixes` without a trailing slash, the only spelling the
  importer accepts.
- An explicit `--model`, `--provider` or `--quick-model` (or `MINI_AGENT_MODEL`) now wins over the
  startup persona's `[prompt_to_model]` mapping; previously the mapped model silently replaced it.
- Headless JSON output adds `"cost_unpriced": true` when the model has no known token price (for
  example a custom OpenAI-compatible provider without `model_prices`), so a `cost` of 0 is no longer
  ambiguous. The field is absent for priced models. `docs/agent/CONFIG.md` notes that cached tokens
  stay 0 when the gateway does not report `prompt_tokens_details.cached_tokens`.
- A `fetch` deny rule written with a single `*` (for example `--deny 'fetch:*'` or
  `fetch:https://example.com/*`) now matches URLs with a path. `*` previously stopped at the first
  `/`, so the rule denied nothing; for `fetch` deny rules `*` now behaves like `**`.
- `mini-agent -p "prompt"` no longer hangs when stdin is an open pipe that never closes (a supervisor
  or subagent): with a prompt argument, a stdin that stays silent for 2 seconds is treated as
  empty. Piped input (`cat file | mini-agent -p 'summarize'`) is still read in full.
- Several mini-agent processes can now use the `js` tool at the same time. The JS effect audit has
  one exclusive writer per directory, so a second process (another pane, a headless run) used to
  fail every `js` call with `JS effect audit unavailable`, and the failed open stayed cached for the
  process lifetime. Each process now claims the first free audit slot (`js-effects`,
  `js-effects-1`, …, up to 64), reuses freed slots, and logs why an open failed instead of hiding
  it (mini-agent-6plxx, mini-agent-xsfo9).
- The structured `git` tool no longer reports an empty result when the sandboxed launch fails: a
  non-zero exit that wrote only to stderr is an error, not "no commits" (mini-agent-8rmk2).
- On Linux with the `bwrap` backend, the structured `git` tool no longer fails every call with
  `bwrap: execvp .../git-runtime/<hash>/git: No such file or directory`. The bundled Git runtime
  lives in the application data directory, which the workspace profile never mounted and which
  explicit-roots profiles mask. The git service launch now binds only the guarded runtime
  directory read-only, after the masks (mini-agent-j053j).

### Added

- `js-fetch-origins` accepts the strings `"all"` (no narrowing) and `"none"` (deny every JS `fetch()`) as
  well as an array of exact origins; other strings are rejected with an error naming the three forms.
  A project config cannot widen inherited origins to `"all"`. When the key is unset, a one-time
  startup notice (log, not model context) says fetch is unrestricted and how to set it.
- The `js` tool gains a lazily evaluated pack of pure helpers (both profiles) and a `help(name?)`
  global that lists them, so the always-sent description grows by one sentence (43 bytes) instead
  of one line per helper. First pack helpers: `date_parse`/`date_fmt` (ISO 8601, RFC 2822, nginx,
  `MM/DD/YYYY`, epoch; UTC only, never local time; impossible dates give `null`) and
  `glob_match`/`glob_filter` (`**`, `*`, `?`, `[..]`, `{a,b}`; matched by a bounded state search, so
  pathological patterns cannot backtrack catastrophically).
- `js` pack helpers `xml_parse(text)` / `xml_build(node)`: a strict, non-validating XML reader and
  writer with a plain `{name, attrs, children, text}` tree (namespaces kept as `prefix:name`, CDATA
  merged into text, comments and processing instructions checked and dropped). Any `<!DOCTYPE>`/DTD,
  external or custom entity is rejected (only the five predefined entities and numeric references are
  known), input is capped at 4 MiB, depth 256, 500k nodes and 1000 attributes per element, and
  malformed input fails with `... (line L, column C)`. Listed by `help()` only.
- `js` pack helpers `bpmn_validate(xml)` and `bpmn_summary(xml)`, built on `xml_parse`: validate
  returns `[{level, id, rule, msg}]` (unique ids; every sourceRef/targetRef/attachedToRef/processRef/
  bpmnElement/flowNodeRef/incoming/outgoing resolves; sequence flows stay inside one process;
  message flows cross participants; start/end event rules; exclusive/inclusive gateway conditions;
  every flow node and flow has a diagram shape or edge) and summary returns processes, participants,
  lanes, counts by element type, flows, message flows, orphans and diagram errors in about 1 KB.
  They read any prefix of the BPMN namespace and complement, not replace, the mcp-bpmn server and the
  bpmn-modeler skill.
- Opt-in `js` pack helper `bpmn_from_outline(steps)`: builds BPMN 2.0 XML with diagram interchange
  from a flat outline (`start`, `end`, `task`, `userTask`, `serviceTask`, `xor`, `and`, `or`,
  `boundary`, `subprocess`, `call`; lanes, pools with message flows, conditions with a default flow)
  using a deterministic left-to-right layered layout, and refuses to return a diagram that fails its
  own `bpmn_validate`. It is a headless first draft: prefer the mcp-bpmn server and the bpmn-modeler
  skill (which forbids generated XML) when they are available.
- Helper pack: `diff_lines(a, b, {context})` returns a `diff -u` style unified diff (Myers, minimal
  edit script, `\ No newline at end of file` markers); inputs are capped at 20000 lines and 8 MiB
  per side and at 2000 differing lines, beyond which it throws a `RangeError` instead of guessing.
- Helper pack: `dockerfile_parse(text)` returns `[{line, op, args}]` per instruction (line
  continuations, comments inside them, parser directives, `RUN`/`COPY`/`ADD` heredoc bodies,
  JSON-form arguments as arrays, `FROM ... AS` stage info, `ENV`/`ARG`/`LABEL` key/value pairs);
  unknown instructions and unterminated heredocs or quotes throw a `SyntaxError` with the line.
- Helper pack: `hcl_blocks(text)` lists Terraform/HCL blocks (`type`, `labels`, nested `blocks`,
  `attrs` as raw expression text plus decoded string/number/bool/null/heredoc literals, `line`,
  `end`) without evaluating anything; unterminated strings, heredocs or comments, unbalanced
  brackets and similar throw a `SyntaxError` with the line number.
- Helper pack: `cidr(spec, other?)` and `ip_in(cidr, ip)` do exact IPv4 and IPv6 subnet arithmetic
  on `BigInt` (network, broadcast, size, netmask, overlap, containment); input is parsed strictly
  (no leading-zero octets, no whitespace or zone ids) and anything else throws a `SyntaxError`.
- Helper pack: `scan_secrets(text)` reports likely secrets (AWS key ids, GitHub/GitLab/Slack/npm/
  PyPI/Stripe/Google/OpenAI/Anthropic tokens, JWTs, PEM private-key headers, `user:pass@` URLs,
  `password=`-style assignments) as `[{rule, line, preview}]`. It is report-only: the preview is
  the first few characters plus the length, never the value, so results are safe to log.
- Pack helpers `path` (`basename`, `dirname`, `extname`, `join`, `normalize`, `relative`; POSIX and
  dotfile-correct), `tally(items, key?)` (count descending, ties by key ascending) and `stats(nums)`
  (n, sum, mean, p50/p90/p95/p99, min, max; linear-interpolation percentiles, NaN rejected) form a
  second lazily loaded segment listed by `help()`; they add nothing to the always-sent description.
- Pack helpers `semver_cmp(a, b)` and `semver_sat(version, range, style?)`: SemVer 2.0 precedence
  (prerelease identifiers, big numbers, build metadata ignored) and npm `^ ~ >= <= > < =` x-range,
  hyphen and `||` ranges with a cargo-style default (a bare `1.2.3` means `^1.2.3`; `'npm'` makes it
  exact). Scanned by hand, with length and set caps, so hostile ranges cannot backtrack. Not PEP 440.
- Pack helpers `yaml_parse(text, {all?})` and `yaml_stringify(value)`: a YAML 1.2 core subset for
  Kubernetes, compose, GitHub Actions and Helm files (block and flow collections, plain and quoted
  scalars, `|` and `>` blocks, comments, anchors, aliases, `<<` merge keys, multi-document text with
  `{all: true}`; `yes`/`no` stay strings and integers beyond 2^53 become BigInt). Tags, complex keys,
  tab indentation, directives and duplicate keys throw with a line number instead of being guessed
  at; input size, nesting depth and alias expansion are capped, so an alias bomb is rejected quickly.
- `claude_project_agents = false` stops mini-agent from reading a repository's `.claude/agents`
  definitions, so Claude Code agents (for example with descriptions over the 160-character limit)
  no longer load or warn on every start. `.mini-agent/agents`, user-global and built-in
  definitions are unaffected. Default: `true`.
- `tools/ghagent-mcp/` bundles the `ghagent-mcp` companion server (stdlib-only Python MCP server that
  gives a sandboxed agent guarded `push_branch` and `wait_for_checks`, plus opt-in
  `create_release`). `make install` now also runs `make install-ghagent`, which installs it under
  `$(PREFIX)` (default `~/.local`); it never touches `~/.config/ghagent` or the mini-agent config.

**Fork release 1.14.1 (2026-10-26)**

### Fixed

- The `task` tool accepts `briefs` sent as a JSON-encoded string of the brief list, as some
  OpenAI-compatible gateways do, instead of failing with `invalid type: string`. A string that
  does not decode to a list of briefs is rejected with a message naming the expected form.
- A `/goal` round whose run fails in the TUI (an exhausted `max_agent_turns` budget, a provider
  error) is now settled through the goal gate, so it is retried and, after repeated failures,
  paused with a reason, as in headless mode. Previously the goal silently stopped, and the next
  ordinary turn inherited the failed round and was judged as failed itself.

**Fork release 1.14.0 (2026-10-24)**

Merges upstream `v1.10.4`. Version numbering stays on the fork line; the entries below are the
upstream changes that are new relative to this fork.

### Changed

- Add `gemini-pro-latest` to the embedded model catalog.
- Web fetches reuse up to two idle keep-alive connections per host for 30 s, and `grep` reuses
  one read buffer across files instead of allocating per file.
- Picker query editing (list and models pickers) shares one key handler. Backspace now places the
  caret where the removed character started, so editing a multi-byte query before trailing text
  no longer misplaces later keystrokes.

### Fixed

- Skip canonical storage paths that also appear as legacy migration candidates,
  avoiding unnecessary reads of active session lock files during Windows restart.
- Keep VS Code integration data separate from current session state, bind test
  prompts and commands to the same activated extension, and compare Windows editor
  paths through the VS Code URI representation.
- The Windows VS Code release fixture initializes explicit private ownership and ACLs
  before startup. A native Windows regression checks that its configuration remains
  readable after private-directory hardening, before release artifacts are built.
- GNU Linux release builds provision the native tools required to embed guarded Git.
  macOS builds explicitly select system libraries, keeping packaged Git independent
  of Homebrew, Fink, and MacPorts while preserving dependency validation.
- ARM64 musl release builds use a configurable, bounded Git compilation budget
  that accounts for emulation and retains process and container cleanup.
- Raise the shared persisted-session ceiling to 32 MiB for long-context and tool-heavy
  conversations, keeping save, catalogue and reload limits aligned. The catalogue scans
  session JSON with bounded memory, including files written with older field orders.
  Older binaries still refuse files larger than their previous 8 MiB ceiling without
  changing those files; use a current binary to reopen them (mini-agent-nqxsh,
  mini-agent-z3brj).
- The VS Code extension accepts up to 48 MiB of ACP session replay, including notification
  envelopes, alongside the server's 32 MiB session limit. The independent 100,000-update
  limit remains unchanged. Update the extension as well as the server to reopen larger
  conversations; an older extension continues to refuse oversized replay safely
  (mini-agent-y09gf).
- Oversized saves discard an optional rewind undo snapshot once before retrying, without
  truncating conversation history or starting model-driven compaction. Saves still over the
  limit retain the last good file and show a deduplicated `/compact` remedy; successful saves
  crossing 75% of the limit warn once. Native sessions apply undo recovery only through their
  authorized publication owner before sealing its digest (mini-agent-ldiyp).
- The scheduled harness-evaluation and JS worker stdout jobs provision the guarded Git runtime
  before Cargo again; since 2026-10-16 every nightly run had failed in the build script.

### Security

- Pin the VS Code packaging tool's Azure Identity dependency to patched MSAL Node
  7.1.0 for [GHSA-xcx6-jx8p-x3h5](https://github.com/advisories/GHSA-xcx6-jx8p-x3h5),
  with offline compatibility and authorization-code security controls.
- Update the VS Code Zod schema dependency to 4.6.7 to address collection-validation
  memory exhaustion ([GHSA-mrhx-c7xc-m3w5](https://github.com/advisories/GHSA-mrhx-c7xc-m3w5)).
- Refresh the audited Rust lockfile to ctutils 0.4.4, Tokio 1.53.3,
  rustls-webpki 0.103.16, and webpki-root-certs/webpki-roots 1.0.10. The VS Code
  tooling uses Vite 6.4.5 and Vitest 4.1.12, including its mocker package.
  These patch updates resolve the newly reported dependency audit findings
  without adding exceptions or weakening audit policy.

**Fork release 1.13.2 (2026-10-22)**

### Fixed

- Release builds: the musl Git runtime's native-source collection now falls back to an Alpine
  recipe's own literal upstream `source=` URL, still bound to the recipe's exact SHA512, when a
  freshly updated package's tarball is missing from the Alpine distfiles mirror (brotli 1.2.1-r0
  on v3.24 blocked both musl release targets).
- Linux JS workers could be SIGKILLed immediately after launch: bubblewrap's `--die-with-parent`
  installs `PR_SET_PDEATHSIG`, which the kernel ties to the parent OS *thread* rather than the
  parent process, so the transient thread that performed the production launch's
  `Command::spawn()` and then exited right away could race bwrap's own startup and get the freshly
  launched worker (and, inside its PID namespace, the contained worker with it) SIGKILLed — the
  root cause of the intermittent `linux-sandbox-policy:js_worker_resource_benchmark`
  `JS_TEST_WORKER_EXIT class=signal_kill` flake. The spawn syscall now runs on a single, lazily
  started, process-lifetime thread (`src/sandbox/spawner.rs`) that never exits, so every
  `--die-with-parent` launch keeps the same kernel-tracked parent thread regardless of which
  transient caller thread requested it (mini-agent-hdl3i).
- Linux JS worker: `RLIMIT_CPU` is armed with soft == hard (intentional hardening so the worker
  cannot raise its own soft limit), so the kernel sends a plain `SIGKILL` at the 35-second native
  CPU ceiling instead of `SIGXCPU`, leaving a reused worker that crossed the ceiling classified as
  an unclassified `Transport` failure instead of the documented closed `NativeCpuLimit` fault. The
  supervisor now reads a reused worker's cumulative CPU time before handing it another invocation
  (`utime`+`stime` from `/proc/<pid>/stat`, fail-closed on any read failure) and retires it once
  that time is within one step's worst-case CPU budget of the ceiling, so a step that starts can
  never cross the ceiling mid-flight; a kill is classified as `NativeCpuLimit` only when the
  worker's last CPU sample was already within that margin, leaving every other kill's
  classification unchanged (mini-agent-spjdd).

**Fork release 1.13.1 (2026-10-21)**

### Added

- `custom_providers.<name>.prompt_caching` (default `false`) opts an OpenAI-compatible Chat
  Completions custom provider into Anthropic-style `cache_control` breakpoints — system prompt,
  last tool definition, and conversation tail — for requests whose model id is in the Claude
  family (`bedrock.anthropic.*`, `vertex.anthropic.*`, `claude-*`), so a gateway that fronts a
  native Anthropic/Bedrock/Vertex backend over an OpenAI-shaped wire format can report prefix-cache
  hits instead of re-billing the same system prompt, tools, and settled history every turn. Honors
  the existing `cache_ttl = "1h"` setting on the markers it emits. Off by default and a no-op for
  non-Claude model ids and the Responses API style (mini-agent-u9jj3).
- Chat Completions requests now carry the same opaque, stable per-session `prompt_cache_key` the
  Responses path already sent, for OpenAI-family model ids (skipped for Claude-family model ids,
  which use the `cache_control` markers above instead), improving prefix-cache routing and hit rate
  on gateways that honor the key; an explicit `prompt_cache_key` in `extra_body` still wins
  (mini-agent-fb20s).
- `custom_providers.<name>.model_prices` prices a custom provider's models per id (USD per million
  tokens, mirroring `quick_models`' `input_token_cost`/`output_token_cost`/cache-tier fields), so a
  gateway configured only as a `custom_providers` entry reports `/cost` instead of `cost:?` and
  `max_budget_usd` can bound it, for both the main session and subagent/advisor/judge calls. Falls
  back to the embedded catalog by `provider_type` for a tier not set in the table; a `quick_models`
  alias pointed at the same provider/model still takes precedence for that alias
  (mini-agent-9osxv).

### Changed

- CI: a pull request that proves a fingerprinted job (`clippy`, `test`, `all-features`,
  `parallel-test-smoke`, `mcp-stdio`, `lsp-lifecycle`) now publishes that proof as a build artifact,
  and the push to `main` that lands its merge trusts a matching proof from that pull request's own
  successful run at its exact head SHA (failing open on any lookup error) instead of unconditionally
  re-running every job the pull request already passed (`scripts/ci_pr_trust.py`).

### Fixed

- `linux-sandbox-policy` CI: the JS worker resource benchmark now distinguishes a host-level
  out-of-memory kill of the native worker (corroborated by this process's own cgroup v2
  `memory.events` `oom_kill` counter) from a genuine containment regression, resamples the
  affected iteration up to twice on corroborated host exhaustion, and always writes its JSON
  output (a measurement, a recorded host-resource skip, or a failure record) so the archive
  step no longer depends on a lucky run to find a file; any other kill or transport error still
  fails the test.
**Fork release 1.13.0 (2026-10-20)**

### Added

- Persona trust: an interactive y/N prompt binds persona definitions in an otherwise-untrusted
  ("agents-only") project by content digest, so a reviewed project can use its own personas without
  trusting the whole project; headless and ACP sessions never prompt or trust, and an untrusted
  project instead gets a one-line notice naming the personas it is ignoring.
- `max_session_input_tokens` / `max_session_total_tokens` (global-only settings) cap cumulative
  session token usage through the shared spend account, the same way `max_budget_usd` already
  caps cost; subagents, repairs, and cron fires count toward the ceiling without needing per-model
  pricing, and a resumed session adds its ledger history.
- Subagents: a child that exhausts its turn budget now produces a tool-free write-up over its own
  retained history instead of stopping bare, and a report that fails the output contract gets one
  tool-free repair turn over that history before the host fallback; `/tasks <id>` shows a bounded
  tool log, and `/tasks result <id>` shows host-replaced raw text marked unverified.
- Subagents: opt-in `isolation: worktree` on a `mode: write` persona runs each child in its own Git
  worktree and branch, with tools, sandbox, and permissions bound to that worktree only (mutating
  the parent tree or `.git` is a hard denial); isolated children skip the shared write lane, and the
  host commits the child's work back and reports its branch and commits.
- UI: a cell-diffed scrollbar gutter and a "new output - End" hint on the chat feed; the plain mouse
  wheel now steps 3 lines by default (`mouse_scroll_lines`); Alt+Up/Down pages and Alt+Shift+Up/Down
  / Alt+</> jump (all rebindable); Shift/Ctrl+wheel pages; `/top` and `/bottom`; and `@agent`/`/agent`
  rows now label read-only versus write-capable sub-agents and main-agent personas.
- A one-line notice at startup and on `/model`/`/provider` switches names when a model falls back to
  its 128k context window, and a turn now warns once it ends past 100% of its context window.
- JS file-effect denials now carry a closed reason (`no_configured_roots`, `outside_roots`, or
  `permission_denied`) instead of an undifferentiated denial.
- Goal rounds: a blocked round with no edits now parks immediately as a named user gate (for example
  an untrusted-project-persona refusal) instead of retrying with nudges; the round collector and
  headless summaries carry the gated setting through to the goal report.

### Fixed

- On Linux, setting any `sandbox-home-mounts` entry (and the explicit `host-read`/`host-full`
  profiles) no longer makes every shell command fail with `private mask overlaps a retained Git
  executable`. The bundled Git runtime lives in mini-agent's masked data directory. Its readonly
  mask already hides it from contained commands, so it needs no separate readonly bind.
- The project-trust memo keys on `.mini-agent` again. The upstream merge had left its
  stat-fingerprint check pointed at `.zerostack`, so it never applied to mini-agent projects.
- The hook-event allowlist accepts the dispatched `PreCompact`, `Notification`, and
  `PermissionRequest` events, which it had been missing; any other unknown event now warns and is
  kept instead of being dropped, while near-miss typos still fail.
- The file-picker's cached-walk test and the export module's JSONL byte-pinning test are
  deterministic under the parallel test runner and across feature rows: the picker test no longer
  races the background cache walk, and the JSONL test builds its expected bytes from the real
  `ExportLine` struct instead of `serde_json::json!()`'s alphabetically-keyed map, so it passes
  under every feature row regardless of whether `serde_json/preserve_order` is unified into the
  build.

### Changed

- Numerous internal performance improvements landed with the upstream merge across the TUI
  renderer, file/grep walking, the JS worker and its effects, sandbox launch on Linux/macOS/Windows,
  MCP/LSP lifecycle, subagents, and session persistence (sharing validated state behind `Arc`,
  caching stat-fingerprinted digests and canonicalized paths, and cutting per-call clones and
  re-hashing). The 2026-10-19 JS/Phase 6 performance review additionally confirmed that none of its
  three findings change the Phase 6 canonical containment checklist
  (`docs/specs/phase-6-brokered-js-runtime.md`).
- Merged upstream's 186-commit `main`, reconciling both sides' independent work to skip the slow
  gates on ordinary pushes by keeping the fork's `gates`-output/`PLATFORM_GATES` mechanism (which
  includes `windows-worker-launcher-unit`) over upstream's ref-based `HEAVY_GATES` duplicate; Rust
  conflicts were resolved as unions of both sides' behaviour. A further 8-commit upstream merge
  then adopted upstream's CI test-impact scope and per-job pass fingerprints (`scripts/ci_scope.py`,
  `scripts/ci_fingerprint.py`, `.github/actions/ci-fingerprint`), replacing the fork's own
  `ci_test_impact.py`/`ci_proofs.py` mechanism. The fork's `cfg(test)` reachability checks were
  ported into `scripts/ci_scope.py` as two additional fail-safe scans: a balanced-paren scan of
  every `cfg(...)`/`cfg!(...)`/`cfg_attr(...)` call anywhere in a test file (not only a leading
  `#![cfg(...)]`), with `unix`/`target_family`/`target_vendor`/`target_env`/`target_arch` or an
  unresolved `target_os` conservatively keeping both platforms; and, for a `src/`-rooted test path,
  a requirement that its `mod`/`#[path]` declaration chain be reachable only through
  `cfg(test)`-gated declarations, or the diff falls back to the full `code` scan.
- CI needs less compute to pass its gates:
  - A newer push to the same branch now cancels the run it supersedes. Tags, schedules and manual
    runs always finish.
  - The slow Windows and macOS Phase 6 containment gates now run only for release tags, the weekly
    schedule and manual runs. `ci-success` accepts them as skipped on ordinary branch pushes and
    pull requests. The release workflow still requires `ci-success` on the tagged commit, so every
    release runs them, and `linux-sandbox-policy` still runs on every code change.
  - macOS repeats only the feature rows whose code differs by platform: the default build, the
    minimal build, and every row with js, sandbox or skills.
  - Non-containment jobs restore a shared compiled-dependency cache keyed by runner image, rustc
    and `Cargo.lock`. One default-feature job per OS refreshes it from green pushes to `main`. The
    containment gates still build from a clean tree.

### Security

- `--api-key` is now scoped to the provider mini-agent started with: persona children, subagent
  configuration, the advisor, `/model`/`/provider` switches, prompt-to-model remaps, imported
  sessions, distillation, and ACP clients each resolve their own provider's credential (and fail
  closed) instead of receiving the CLI key that was supplied for a different provider.
- The pre-rename legacy migration that runs on every startup no longer reads the host's real
  default config/data directories when `MINI_AGENT_CONFIG_DIR`/`MINI_AGENT_DATA_DIR` (or their
  legacy `ZEROSTACK_*_DIR` aliases) explicitly redirect storage elsewhere. Previously it always
  scanned the OS-default `zerostack` siblings regardless of any override, so a container, test, or
  multi-profile install that redirects its data root could still have unrelated content (including
  another real install's sessions) migrated in from the host's actual default directories.
- The VS Code extension's dev-only `vite` 6.4.4 replaces 6.4.3, picking up the patched
  `launch-editor` (GHSA-587h-7wmj-h5pm, GHSA-698v-4r3x-3vcv, GHSA-r825-w8mm-5pxw). `vite` is
  a test-tooling dependency and is not bundled into the published VSIX.
- `rustls-webpki` 0.103.16 replaces 0.103.15, and `webpki-root-certs`/`webpki-roots` 1.0.10 replace
  1.0.9, rejecting certificate paths through the now-compromised `emSign` intermediates
  (RUSTSEC-2026-0382, RUSTSEC-2026-0383, RUSTSEC-2026-0381; rustls/webpki#546,
  rustls/webpki-roots#133) across `reqwest`, `hyper-rustls`, `tokio-rustls`, `google-cloud-auth`,
  `ureq`, and `rustls-platform-verifier`. A time-bounded exception
  (`dependency-exceptions.toml`, expires 2026-12-19) covers the one remaining `webpki-roots`
  0.26.11 pin: it is a compatibility shim that already re-exports the patched 1.0.10
  implementation, kept in `Cargo.lock` only for an optional `tokio-tungstenite` feature path of
  the vendored `rig-core` dependency that mini-agent never activates.

### Known Limitations

- The Phase 6 worker baseline remains explicitly `pending_external_runs`, with no platform
  evidence collected. This release changes only when the Windows and macOS containment gates run
  (release tags, the weekly schedule, and manual dispatch), not what they prove; maintainers must
  still collect and aggregate the Linux, macOS, and Windows CI artifacts before publishing measured
  results.

**Fork release 1.12.0 (2026-10-18)**

### Fixed

- `mcp_search` matches each query word independently, so one query can list several exact tool
  names; named tools rank first and partial matches follow by matched-word count. An empty result
  now carries a `hint`, so a query that matched nothing is no longer mistaken for a missing MCP
  server.
- Subagent report normalization accepts `**Covered**:` / `**Skipped**:` and a confidence label
  placed after a bold finding title, so such reports keep their findings instead of being replaced
  by the host repair notice.

### Changed

- Embedded coding personas (Rust, Python, Node/TypeScript, VS Code) prefer the code-indexer `ci_*`
  tools for symbol search, callers, tests, and impact when those tools are installed.
- CI provisions the Windows Git runtime's MSYS2 UCRT64 toolchain with the runner image's own
  pacman instead of the third-party `msys2/setup-msys2` action, which the organization's Actions
  policy does not allow.

### Security

- `zip` 9.0.5 replaces 8.6.0 (RUSTSEC-2026-0367, RUSTSEC-2026-0368, RUSTSEC-2026-0369).

**Fork release 1.11.0 (2026-10-16)**

### Added

- First-class Azure OpenAI v1, signed AWS Bedrock Claude Messages, Vertex AI Claude (client-owned
  ADC), and GitHub Copilot (app-owned device authorization) providers, plus documented ChatGPT
  sign-in (`--login openai`, `/login openai`) with protected provider credentials.
- Full/default builds include LSP, lifecycle hooks, and multimodal messages. LSP
  automatically selects available installed servers without starting or installing
  them; explicit off and the lite archive feature recipe remain supported. `/toggle`
  lists their captured runtime availability.
- Explicit sandbox filesystem profiles (`sandbox-profile`, `--sandbox-profile
  workspace|host-read|host-full`, `/sandbox`) and bounded `sandbox-read-roots` /
  `sandbox-write-roots` grants.
- Trusted project config layers (`.mini-agent/config.toml` plus `config.local.toml`), literal
  `--config key=value` overrides, per-key `--print-config` source labels, and `--config-schema`.
- Invocation-only `--allow`, `--deny`, and `--allow-prefix` permission seeds; `Shift+Tab` cycles
  permission modes (`permission_mode_cycle`).
- `--system-prompt` / `--append-system-prompt` and `--completions <shell>`.
- Session-owned background subagent tasks, opt-in `mode: write` implementation subagents,
  `required_tools` persona declarations, and configurable model-aware subagent budget weighting
  (`task_cost_weighting`, `task_cost_weights`, `task_cost_overrides`).
- Native web retrieval and search, clarifying-question and notebook-cell tools, clipboard image
  input, and bounded replayable image reads.
- File rewind, mid-turn steering, session effort controls, session forks, a settings editor, and
  persisted ACP sessions with VS Code resume (`Mini Agent: Resume Saved Session`).
- Configurable key bindings with optional Vim input editing; reasoning display modes
  (`reasoning_display`, `Ctrl+R` cycles collapsed/full/off); attributed mutation diffs
  (`diff_context_lines`, `max_diff_lines`); bounded syntax highlighting for code fences.
- Theme foreground roles, `default-light` / `default-dark` themes, and bounded terminal
  background detection (OSC 11, then `COLORFGBG`).
- Session spend tracking with a cache-tier `/cost` breakdown, Anthropic one-hour prompt caching
  (`cache_ttl`), configurable foreground shell deadlines (`shell_timeout_secs`), and streamed
  foreground shell progress.
- Read-only doctor diagnostics, explicit plan approval, approved Git workflows, bounded MCP
  prompts, resources and forms, owned MCP management, and bounded Agent Skills discovery.

### Changed

- The TUI render core repaints only changed rows, caches wrap layouts, paints resumed feeds
  tail-first, and keeps hooks, skills, update checks, and the OSC 11 query off the first frame.
- Subagents share one config snapshot per task call, cache persona maps per workspace, run
  foreground task calls in their own lane, and use a deeper ordered event channel.
- Startup spend reconciles from a verified checkpoint plus the ledger tail; MCP discovers prompts
  and resources off the connect path and caches `tools/list`.
- Source builds compile a pinned embedded Git runtime and need Python 3 plus the native C, curl,
  and zlib toolchain; prebuilt release archives already include it.
- The fork's Linux `bwrap` toolchain binds are now configured as `sandbox-toolchain-roots`
  (`--sandbox-toolchain-root`), because `sandbox-read-roots` now names the cross-platform
  profile grant.
- ACP personas are selected only through the `mode` session config option
  (`session/set_config_option`); ACP session modes (`session/set_mode`) now select the upstream
  permission modes. Clients that switched personas with `session/set_mode` must move to the config
  option.
- Project agents, skills, prompts, MCP and config layers that upstream reads from `.zerostack/`
  are read from the fork's `.mini-agent/` project directory; repository-controlled `.zerostack/`
  content is still not loaded.

### Fixed

- Four Unix-only Seatbelt/cache tests are correctly gated, restoring Windows test compilation.
- The embedded catalog includes current Claude 5.5 and GPT-6 models, and Anthropic defaults to
  Sonnet 5.5. Startup and `/provider` share default model selection.
- Bounded read, shell, search and JavaScript results retain their own output limits in live and
  replayed history. Saved output from unbounded tools can be read in headless mode through the
  current session's private directory capability, while explicit deny rules still apply.
- `find_files` interprets common patterns such as `src/*.rs` as globs; explicit regex mode is
  available. HTTP 408, 409 and 425 completion errors use bounded retry backoff.
- LSP selection is deterministic when servers share an extension. Empty, malformed and duplicate
  prompt directives are removed from the prompt header and reported to the user.
- Windows reliability: explicit private ownership for Git scratch, retained directory pins and
  exact-snapshot transactional MCP publication, and terminal EOF no longer spins during input
  reader shutdown.

### Security

- Update rustls to 0.23.46, including upstream fixes for unauthenticated key-exchange
  hints, session debug output, and early application data on ECH rejection.
- The pinned embedded Git runtime enforces executable configuration policy; repository-owned
  configuration cannot grant executable authority.

**Fork release 1.10.0 (2026-10-02)**

### Security

- Hook content pinning now matches `$MINI_AGENT_PROJECT_DIR`. The upstream merge had restored the
  legacy `$ZEROSTACK_PROJECT_DIR` name in the scanner, so hook scripts named through the real
  variable were not content-bound.
- Linux and macOS sandbox launches refuse existing symlink and hardlink aliases of protected Git
  metadata, including bounded inspection of protected-directory descendants and external linked
  worktree metadata. In-workspace gitfile target directories are pinned against rename while
  ordinary Git add and commit remain available. Metadata first created after launch and aliases
  introduced concurrently remain tracked limitations.
- Executable Git configuration from multiply linked global files is treated as untrusted,
  preserving ordinary single-link global helpers.
- macOS commands with a shared application home select a separate private runtime cache when the
  configured cache overlaps configuration or credentials, preserving those read denials while
  allowing temporary-file workflows.
  Git also treats executable configuration included from that selected cache as untrusted.
- Editor-supplied ACP MCP servers deny network by default and are capped at eight per session;
  operator-trusted servers retain their explicit bypass. VS Code rechecks workspace trust after
  permission and folder selection and across executable verification, ACP launch, and prompt
  dispatch. Audited VS Code tooling dependencies are patched.
- Internal Git commands classify executable configuration by its file origin as well as scope:
  a global include from a writable repository, linked Git directory, or sandbox cache cannot
  acquire user authority. The separate configuration probe-to-launch race remains tracked.

- macOS sandboxed commands, hooks, and workspace services use private temporary storage instead
  of granting writes throughout `/private/tmp`. Workspace service environment names cannot
  replace the declared executable through `env` options. Linux launches now fail closed when
  bubblewrap lacks descriptor-backed Git metadata protection (requires 0.8.0 or later).
- Standard-mode file tools ask before changing nested Git execution metadata, gitfiles, and
  linked-worktree administration, while preserving explicitly configured permission rules.

- Internal Git commands now refuse malformed or non-UTF8 executable configuration. The probe
  reads all configuration records instead of a locale-sensitive regex query, so a filter name
  containing invalid UTF-8 cannot escape the command overrides and execute on the host.

- URL MCP servers are pinned to the addresses that passed the public-address check: the HTTP
  transport, its SSE reconnects and the OAuth client no longer resolve the host again, and every
  other OAuth host, lookup and redirect must also be public, so DNS rebinding to `127.0.0.1` or
  `169.254.169.254` is refused. `/mcp login` now validates the server URL too.
- ACP "Allow always" (now labelled "Allow for this session") on a file tool without a narrower
  scope approves exactly the prompted path; a `*` or `?` in it no longer widens the grant to a
  glob. The option's `_meta.suggestedPattern` names the exact session rule, and the VS Code
  extension no longer says the rule is persisted.
- The VS Code extension resolves a bare `mini-agent.executablePath` to an absolute path using only
  the absolute `PATH` directories, so on Windows a `mini-agent.exe` planted in the workspace is
  never launched, and the version probe and the launch run the same file.
- An aborted, reset or interrupted connection, or a brief descriptor shortage, no longer shuts
  down the ACP TCP (plaintext or TLS) listener before a client authenticates; the plaintext
  listener no longer blocks an async worker while it waits.
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
  bubblewrap 0.8.0 or later (older versions refuse launch) and covers entries that exist at launch.
  In `standard` mode file tools also ask before changing Git execution metadata, including nested
  repositories, unless a configured rule decides.
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

- Tool results in the TUI fold into one-line summaries by default. `Ctrl+T`, clicking a summary,
  `/expand [n|all]` and `/collapse [n|all]` unfold them; errors keep their first line visible. The
  new `tool_output = "collapsed" | "preview" | "full"` setting selects the default.
- `sandbox-read-roots` (and repeatable `--sandbox-read-root`) configures extra read-only paths for
  the Linux shell and hook sandbox. The default mounts `/home/linuxbrew`, `~/.cargo/bin`,
  `~/.local/bin` and `~/.rustup`, so tools such as `bd` and `cargo` work in sandboxed shells
  (including WSL); `--print-config` shows the effective list.
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
- ACP sessions advertise the loaded prompts as session modes and as a `mode` session config
  option, each naming its routed model and permission mode. `session/set_mode` and
  `session/set_config_option` switch the session's persona and its `[prompt_to_model]` model from
  the next prompt; `mini-agent:none` selects no prompt.

### Fixed

- ACP mode now honours persona routing like the TUI and `-p`. A new session activates
  `--load-prompt` or `default_prompt` (its `%%agent=` persona and `%%mode=` narrowing) and runs
  each turn on the prompt's `[prompt_to_model]` route instead of always using the startup model.
  A routed provider that is not configured or lacks its credential is never replaced: the switch
  or the prompt is refused with a bounded, credential-free error, as is a route naming no quick
  model or a persona mode that cannot be enforced because permission checks are disabled.

- Windows: a `config.toml` (or other private file) created by another program, such as an editor
  or the VS Code release smoke, is readable again. Making its directory private re-propagated
  inheritance and left the file with an empty DACL that even its owner could not open
  (`Access is denied. (os error 5)`). Private directories now pass their owner-only SYSTEM and
  current-user grants to existing and new children.
- `grep` accepts a single regular file as `path`; `find_files` and `list_dir` explain when they are
  given a file instead of a directory.
- Log records no longer appear in the TUI transcript; while the TUI owns the terminal they go only
  to the `-v`/`--log-file` log, and errors already returned to the model are logged at debug.
- Windows sandbox traverse grants on workspace parent directories no longer re-propagate ACLs
  through the whole user profile, which had pushed preflight recovery past its deadline.
- `make install` builds an explicit supported feature set instead of `--all-features`, which
  required a prebuilt ONNX Runtime.
- Release, nightly harness and Windows release-smoke workflows use the repository's Rust setup
  action, which the organization's Actions allow-list permits.
- The supported lexical-only skill backend logs its diagnostic at Debug; genuine retrieval
  failures still warn. VS Code waits for bounded child termination after ACP startup fails.
- `/wt-merge` works with local-only target branches without requiring an upstream.
  Branches with upstream configuration still pull before merging.
- `/loop stop` and loop status are available during an active round; stopping prevents
  further rounds without rebuilding the running agent.
- Pending JavaScript startup checks no longer appear as false runtime-unavailability warnings.
- The VS Code extension now requires Stable 1.91, where chat participants became available,
  and release checks activate the installed VSIX against its bundled ACP binary.

- Background diagnostics now appear in the TUI chat feed without overwriting unsent input.
  Headless and ACP diagnostics continue to use stderr.
- Expected ACP permission responses no longer emit false unhandled-dispatch warnings.

- Fully typed slash-command arguments such as `/model org/model`, `/prompt default`, and
  `/models refresh` now submit on the first Enter. Partial arguments still complete the
  highlighted choice, and Tab keeps its existing completion behavior.

- The manual Windows LPAC image-loading feasibility test now builds its child environment with the
  same helper as the production worker launcher, adding the AppContainer profile's `LOCALAPPDATA`,
  `TEMP` and `TMP`; without them `CreateProcessW` failed on the hosted runner with os error 203. The
  test stays ignored and out of CI until a manual Windows run confirms the fix.
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
- Binary distributions now carry the license notices of their bundled Rust crates: every release
  archive ships a generated `THIRD_PARTY_LICENSES` naming each package of its target's locked
  dependency resolution with its license and verbatim license texts, the installer, Homebrew, AUR,
  Conda, MSI and VSIX install it, and `NOTICE` now credits QuickJS and rquickjs with their MIT texts.
  Releases before 1.9.5 predate the inventory: `install.sh` and the package recipes still install
  them, and the installer warns that the inventory is absent.
- CI: every job now has a timeout (no job can hold a runner for GitHub's 6-hour default), the
  dependency-policy job's condition is parenthesised so it no longer runs on documentation-only
  changes by operator precedence, the weekly scheduled run also re-runs the parallel test smoke and
  the Phase 6 containment gates for runner-image drift, the `--all-features` build (including
  `skills-embed`) is linted with `-D warnings`. (The Windows LPAC image-loading feasibility test
  stays ignored: it fails on the hosted runner and is tracked as mini-agent-6kdk2.) The macOS worker
  code-identity check now
  names the CDHash of every slice of a universal binary, so its real-signature tests run again
  instead of being ignored. Dead code hidden behind `#[allow(dead_code)]` (the pre-goal validation
  registry, the unused startup path field, test-only path helpers) was removed or compiled only
  where it is used, and `/loop` start/stop/usage gained slash-command tests.
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

### Changed

- The VS Code extension's CI audit runs through `scripts/npm_audit_gate.py`: production
  dependencies must audit clean, and the full tree may waive only a reviewed, expiring dev-only
  exception from `editors/vscode/audit-exceptions.json` (currently GHSA-vfj7-8cjw-p6xm in
  `braces`, which has no patched release and is reached only through build tooling).
- CI assembles the GPL Corresponding Source archive in its own code-gated job, so documentation-only
  pushes no longer spend minutes vendoring ~500 npm tarballs, and caches those tarballs by the
  `package-lock.json` hash. `package-corresponding-source.sh --npm-vendor-cache <dir>` reuses a cached
  tarball only when it matches the lockfile integrity, producing a byte-identical archive; release
  builds do not use the cache.
- The terminal UI is faster to type into, stream into and start. Each frame is written once,
  inside a synchronized-output update, and only changed chat rows are repainted (moved content is
  scrolled). Streamed text repaints at most every 40 ms. Queued keys, wheel and drag events are
  applied together before one paint. An open picker repaints only its own band. A resumed long
  session paints its tail first and shows ` ↑ earlier ` until the full layout is ready.
- The first frame no longer waits for `SessionStart` hooks, `/` skill discovery or the opt-in
  update check. The OSC 11 background query is skipped when `COLORFGBG` is valid and otherwise
  overlaps the rest of startup. Turn-end session saves are written off the UI task, and a failed
  write is reported on the next save.
  Exit retires its owners concurrently, closes MCP servers concurrently and bounds the background
  task close at 5 s.
- `!` commands show their output live while they run, and foreground model `shell` calls show a
  throttled preview of their last output lines under the tool call. The preview is display-only
  and is not saved.
- The statusline `git_status`/`git_changes` refresh adapts to activity: right after tool results,
  turn end and commands, otherwise every 10 s. It skips Git when the repository metadata is
  unchanged and still runs a full status at least every 30 s.
- A foreground `task` call has its own subagent concurrency lane, separate from background task
  batches. Time spent waiting for a lane is not charged to `task_timeout_secs` (up to one further
  timeout), and partial
  headers report waiting children as `queued=N`. `job_status` `poll`, `wait` and `list` no longer
  block parallel read-only tools.
- Startup spend reconciliation reads a private `<state_dir>/usage.checkpoint` and scans only the
  ledger rows appended since the last startup. MCP prompt and resource discovery no longer delays
  a server's connection, and each connection caches its `tools/list` result. The `/sessions`
  picker loads in the background.
- Development builds compile dependencies at opt-level 1. Release builds use opt-level 3
  (previously `z`), with thin LTO and one codegen unit.

## [1.10.4] - 2026-10-24

### Added

- Full/default builds include LSP, lifecycle hooks, and multimodal messages. LSP
  automatically selects available installed servers without starting or installing
  them; explicit off and the lite archive feature recipe remain supported. `/toggle`
  lists their captured runtime availability.
- `@agent:<name>` in a TUI message asks the model to delegate that request to the named
  specialist persona through `task`. The `@` picker lists the trusted personas first, with each
  persona's description dimmed after its name. Claude Code's `@agent-<name>` form is rewritten to
  the canonical token only when the name resolves; an unknown canonical name is refused with the
  valid names listed and the draft restored. The routing note is a non-persisted addition to that
  one request and names personas only: the saved transcript, rewind, compaction, titles and
  `UserPromptSubmit` see the typed text. The one exception is an Anthropic native thinking-replay
  chain, whose opaque replay frames keep the request as sent and are never shown in the
  transcript or hook payloads. Headless `-p` and ACP prompts resolve mentions only with the opt-in
  `headless_agent_mentions = true`; an unknown name then fails `-p` before any provider request
  and an ACP prompt with `-32602`. The opt-in `agent_mention_tool_choice = "force"` also forces the
  first model request of a TUI mention turn to call `task`, falling back to the routing note on
  providers and Claude models that cannot carry a forced `tool_choice`.
- `/cron <interval> <prompt>` repeats a plain prompt in the current session (`1m` to `24h`, at most
  16 schedules, each removed after `cron_max_fires` fires, default 24), and `/cron <prompt>` arms a
  self-paced schedule whose fired turns choose the next fire (60 s to 1 h) through a
  `schedule_wakeup` tool offered only during those turns; a turn without the call stops the
  schedule. `/cron list`, `stop <id>|all` and `run <id>` work while busy. A fire starts only when
  the TUI is idle; deadlines missed while busy park one fire, with no catch-up. Payloads starting
  with `!`, and slash commands other than `/goal status`, `/tasks`, `/jobs` and `/cost`, are
  refused. A fired turn is echoed with a `⟳ cron` marker, reaches `UserPromptSubmit` hooks with
  `"origin": "scheduled"` and is recorded as request kind `scheduled`; reaching the USD budget
  suspends every schedule, and arming without `max_budget_usd` warns once. Schedules are saved
  disarmed with the session and re-armed only by `/cron resume`. The statusline `cron` item shows
  the armed count and the next fire, or a parked, waiting, running or suspended state. A `/loop`
  whose first argument is an interval is refused with a `/cron` hint instead of starting a round.
  ACP sessions accept interval `/cron` schedules (not self-paced, `run`, `resume` or slash
  payloads, which return `-32602`); they are per session, never saved, fire only when the session
  has no active prompt, and keep `--idle-exit` waiting while any schedule is armed.
- `sandbox-home-mounts` exposes chosen HOME dot files and folders to contained commands (read-only,
  or a writable directory with `:rw`), and `sandbox-home-masks` hides further HOME dot paths in
  every profile in addition to the built-in masks. Entries are validated at startup and fail
  closed; the Windows AppContainer backend refuses mounts and any mask an explicit root would
  expose.

### Security

- Pin the VS Code packaging tool's Azure Identity dependency to patched MSAL Node
  7.1.0 for [GHSA-xcx6-jx8p-x3h5](https://github.com/advisories/GHSA-xcx6-jx8p-x3h5),
  with offline compatibility and authorization-code security controls.
- Update the VS Code Zod schema dependency to 4.6.7 to address collection-validation
  memory exhaustion ([GHSA-mrhx-c7xc-m3w5](https://github.com/advisories/GHSA-mrhx-c7xc-m3w5)).
- Refresh the audited Rust lockfile to ctutils 0.4.4, Tokio 1.53.3,
  rustls-webpki 0.103.16, and webpki-root-certs/webpki-roots 1.0.10. The VS Code
  tooling uses Vite 6.4.5 and Vitest 4.1.12, including its mocker package.
  These patch updates resolve the newly reported dependency audit findings
  without adding exceptions or weakening audit policy.

- Update rustls to 0.23.46, including upstream fixes for unauthenticated key-exchange
  hints, session debug output, and early application data on ECH rejection.

- Linux and macOS sandbox launches refuse existing symlink and hardlink aliases of protected Git
  metadata, including bounded inspection of protected-directory descendants and external linked
  worktree metadata. In-workspace gitfile target directories are pinned against rename while
  ordinary Git add and commit remain available. Metadata first created after launch and aliases
  introduced concurrently remain tracked limitations.
- Executable Git configuration from multiply linked global files is treated as untrusted,
  preserving ordinary single-link global helpers.
- macOS commands with a shared application home select a separate private runtime cache when the
  configured cache overlaps configuration or credentials, preserving those read denials while
  allowing temporary-file workflows.
  Git also treats executable configuration included from that selected cache as untrusted.
- Editor-supplied ACP MCP servers deny network by default and are capped at eight per session;
  operator-trusted servers retain their explicit bypass. VS Code rechecks workspace trust after
  permission and folder selection and across executable verification, ACP launch, and prompt
  dispatch. Audited VS Code tooling dependencies are patched.
- Internal Git commands classify executable configuration by its file origin as well as scope:
  a global include from a writable repository, linked Git directory, or sandbox cache cannot
  acquire user authority. The separate configuration probe-to-launch race remains tracked.

- macOS sandboxed commands, hooks, and workspace services use private temporary storage instead
  of granting writes throughout `/private/tmp`. Workspace service environment names cannot
  replace the declared executable through `env` options. Linux launches now fail closed when
  bubblewrap lacks descriptor-backed Git metadata protection (requires 0.8.0 or later).
- Standard-mode file tools ask before changing nested Git execution metadata, gitfiles, and
  linked-worktree administration, while preserving explicitly configured permission rules.

- Internal Git commands now refuse malformed or non-UTF8 executable configuration. The probe
  reads all configuration records instead of a locale-sensitive regex query, so a filter name
  containing invalid UTF-8 cannot escape the command overrides and execute on the host.

- URL MCP servers are pinned to the addresses that passed the public-address check: the HTTP
  transport, its SSE reconnects and the OAuth client no longer resolve the host again, and every
  other OAuth host, lookup and redirect must also be public, so DNS rebinding to `127.0.0.1` or
  `169.254.169.254` is refused. `/mcp login` now validates the server URL too.
- ACP "Allow always" (now labelled "Allow for this session") on a file tool without a narrower
  scope approves exactly the prompted path; a `*` or `?` in it no longer widens the grant to a
  glob. The option's `_meta.suggestedPattern` names the exact session rule, and the VS Code
  extension no longer says the rule is persisted.
- The VS Code extension resolves a bare `mini-agent.executablePath` to an absolute path using only
  the absolute `PATH` directories, so on Windows a `mini-agent.exe` planted in the workspace is
  never launched, and the version probe and the launch run the same file.
- An aborted, reset or interrupted connection, or a brief descriptor shortage, no longer shuts
  down the ACP TCP (plaintext or TLS) listener before a client authenticates; the plaintext
  listener no longer blocks an async worker while it waits.
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
  bubblewrap 0.8.0 or later (older versions refuse launch) and covers entries that exist at launch.
  In `standard` mode file tools also ask before changing Git execution metadata, including nested
  repositories, unless a configured rule decides.
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

- Skip canonical storage paths that also appear as legacy migration candidates,
  avoiding unnecessary reads of active session lock files during Windows restart.
- Keep VS Code integration data separate from current session state, bind test
  prompts and commands to the same activated extension, and compare Windows editor
  paths through the VS Code URI representation.
- The Windows VS Code release fixture initializes explicit private ownership and ACLs
  before startup. A native Windows regression checks that its configuration remains
  readable after private-directory hardening, before release artifacts are built.
- GNU Linux release builds provision the native tools required to embed guarded Git.
  macOS builds explicitly select system libraries, keeping packaged Git independent
  of Homebrew, Fink, and MacPorts while preserving dependency validation.
- ARM64 musl release builds use a configurable, bounded Git compilation budget
  that accounts for emulation and retains process and container cleanup.
- Raise the shared persisted-session ceiling to 32 MiB for long-context and tool-heavy
  conversations, keeping save, catalogue and reload limits aligned. The catalogue scans
  session JSON with bounded memory, including files written with older field orders.
  Older binaries still refuse files larger than their previous 8 MiB ceiling without
  changing those files; use a current binary to reopen them (mini-agent-nqxsh,
  mini-agent-z3brj).
- The VS Code extension accepts up to 48 MiB of ACP session replay, including notification
  envelopes, alongside the server's 32 MiB session limit. The independent 100,000-update
  limit remains unchanged. Update the extension as well as the server to reopen larger
  conversations; an older extension continues to refuse oversized replay safely
  (mini-agent-y09gf).
- Oversized saves discard an optional rewind undo snapshot once before retrying, without
  truncating conversation history or starting model-driven compaction. Saves still over the
  limit retain the last good file and show a deduplicated `/compact` remedy; successful saves
  crossing 75% of the limit warn once. Native sessions apply undo recovery only through their
  authorized publication owner before sealing its digest (mini-agent-ldiyp).

- Four Unix-only Seatbelt/cache tests are correctly gated, restoring Windows test compilation.
- The scheduled harness-evaluation and JS worker stdout jobs provision the guarded Git runtime
  before Cargo again; since 2026-10-16 every nightly run had failed in the build script.
- The embedded catalog includes current Claude 5.5 and GPT-6 models, and Anthropic defaults to
  Sonnet 5.5. Startup and `/provider` share default model selection.
- Bounded read, shell, search and JavaScript results retain their own output limits in live and
  replayed history. Saved output from unbounded tools can be read in headless mode through the
  current session's private directory capability, while explicit deny rules still apply.
- `find_files` interprets common patterns such as `src/*.rs` as globs; explicit regex mode is
  available. HTTP 408, 409 and 425 completion errors use bounded retry backoff.
- LSP selection is deterministic when servers share an extension. Empty, malformed and duplicate
  prompt directives are removed from the prompt header and reported to the user.

- The supported lexical-only skill backend logs its diagnostic at Debug; genuine retrieval
  failures still warn. VS Code waits for bounded child termination after ACP startup fails.
- `/wt-merge` works with local-only target branches without requiring an upstream.
  Branches with upstream configuration still pull before merging.
- `/loop stop` and loop status are available during an active round; stopping prevents
  further rounds without rebuilding the running agent.
- Pending JavaScript startup checks no longer appear as false runtime-unavailability warnings.
- The VS Code extension now requires Stable 1.91, where chat participants became available,
  and release checks activate the installed VSIX against its bundled ACP binary.

- Background diagnostics now appear in the TUI chat feed without overwriting unsent input.
  Headless and ACP diagnostics continue to use stderr.
- Expected ACP permission responses no longer emit false unhandled-dispatch warnings.

- Fully typed slash-command arguments such as `/model org/model`, `/prompt default`, and
  `/models refresh` now submit on the first Enter. Partial arguments still complete the
  highlighted choice, and Tab keeps its existing completion behavior.

- The manual Windows LPAC image-loading feasibility test now builds its child environment with the
  same helper as the production worker launcher, adding the AppContainer profile's `LOCALAPPDATA`,
  `TEMP` and `TMP`; without them `CreateProcessW` failed on the hosted runner with os error 203. The
  test stays ignored and out of CI until a manual Windows run confirms the fix.
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
- Binary distributions now carry the license notices of their bundled Rust crates: every release
  archive ships a generated `THIRD_PARTY_LICENSES` naming each package of its target's locked
  dependency resolution with its license and verbatim license texts, the installer, Homebrew, AUR,
  Conda, MSI and VSIX install it, and `NOTICE` now credits QuickJS and rquickjs with their MIT texts.
  Releases before 1.9.5 predate the inventory: `install.sh` and the package recipes still install
  them, and the installer warns that the inventory is absent.
- CI: every job now has a timeout (no job can hold a runner for GitHub's 6-hour default), the
  dependency-policy job's condition is parenthesised so it no longer runs on documentation-only
  changes by operator precedence, the weekly scheduled run also re-runs the parallel test smoke and
  the Phase 6 containment gates for runner-image drift, the `--all-features` build (including
  `skills-embed`) is linted with `-D warnings`. (The Windows LPAC image-loading feasibility test
  stays ignored: it fails on the hosted runner and is tracked as mini-agent-6kdk2.) The macOS worker
  code-identity check now
  names the CDHash of every slice of a universal binary, so its real-signature tests run again
  instead of being ignored. Dead code hidden behind `#[allow(dead_code)]` (the pre-goal validation
  registry, the unused startup path field, test-only path helpers) was removed or compiled only
  where it is used, and `/loop` start/stop/usage gained slash-command tests.
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

### Changed

- Refresh the embedded model catalog with current provider models and context limits;
  remove retired chat aliases and unavailable OpenRouter models.
- CI assembles the GPL Corresponding Source archive in its own code-gated job, so documentation-only
  pushes no longer spend minutes vendoring ~500 npm tarballs, and caches those tarballs by the
  `package-lock.json` hash. `package-corresponding-source.sh --npm-vendor-cache <dir>` reuses a cached
  tarball only when it matches the lockfile integrity, producing a byte-identical archive; release
  builds do not use the cache.
- The terminal UI is faster to type into, stream into and start. Each frame is written once,
  inside a synchronized-output update, and only changed chat rows are repainted (moved content is
  scrolled). Streamed text repaints at most every 40 ms. Queued keys, wheel and drag events are
  applied together before one paint. An open picker repaints only its own band. A resumed long
  session paints its tail first and shows ` ↑ earlier ` until the full layout is ready.
- The first frame no longer waits for `SessionStart` hooks, `/` skill discovery or the opt-in
  update check. The OSC 11 background query is skipped when `COLORFGBG` is valid and otherwise
  overlaps the rest of startup. Turn-end session saves are written off the UI task, and a failed
  write is reported on the next save.
  Exit retires its owners concurrently, closes MCP servers concurrently and bounds the background
  task close at 5 s.
- `!` commands show their output live while they run, and foreground model `shell` calls show a
  throttled preview of their last output lines under the tool call. The preview is display-only
  and is not saved.
- The statusline `git_status`/`git_changes` refresh adapts to activity: right after tool results,
  turn end and commands, otherwise every 10 s. It skips Git when the repository metadata is
  unchanged and still runs a full status at least every 30 s.
- A foreground `task` call has its own subagent concurrency lane, separate from background task
  batches. Time spent waiting for a lane is not charged to `task_timeout_secs` (up to one further
  timeout), and partial
  headers report waiting children as `queued=N`. `job_status` `poll`, `wait` and `list` no longer
  block parallel read-only tools.
- Startup spend reconciliation reads a private `<state_dir>/usage.checkpoint` and scans only the
  ledger rows appended since the last startup. MCP prompt and resource discovery no longer delays
  a server's connection, and each connection caches its `tools/list` result. The `/sessions`
  picker loads in the background.
- Development builds compile dependencies at opt-level 1. Release builds use opt-level 3
  (previously `z`), with thin LTO and one codegen unit.

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

[Unreleased]: https://github.com/sebahrens/mini-agent/compare/v1.10.4...HEAD
[1.10.4]: https://github.com/sebahrens/mini-agent/compare/v1.9.4...v1.10.4
[1.9.4]: https://github.com/sebahrens/mini-agent/compare/v1.8.0...v1.9.4
[1.8.0]: https://github.com/sebahrens/mini-agent/compare/v1.7.2...v1.8.0
