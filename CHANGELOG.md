# Changelog

Notable changes to mini-agent are documented in this file. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Documented the 2026-09-05 harness design review (epic `mini-agent-5ana`): spec index 1.4.0,
  Phase 6 1.1.0, Phase 3 1.3.0, Phase 4 1.3.0, and Phase 5 1.2.0 gain *Accepted amendments
  (2026-09-05, pending delivery)* sections; agent docs now state the current limits of the
  `js` surface, learned-skill retrieval, subagent personas, memory refresh, and the shell
  deadline. No runtime behavior changed.

## [1.8.0] - 2026-09-03

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

[Unreleased]: https://github.com/sebahrens/mini-agent/compare/v1.8.0...HEAD
[1.8.0]: https://github.com/sebahrens/mini-agent/compare/v1.7.2...v1.8.0
