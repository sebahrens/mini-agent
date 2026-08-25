# Changelog

Notable changes to mini-agent are documented in this file. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.8.0] - 2026-08-25

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

### Changed

- Improved agent, JavaScript, skill-retrieval, session, Git, and terminal hot paths through cached
  immutable metadata, JSON Lines session persistence, incremental Markdown rendering, and reduced
  worker bootstrap overhead.
- Hardened release packaging with full and lite archives, vendored Corresponding Source, software
  bills of materials, checksum manifests, native VSIX candidates, and the Windows installer.
- Expanded continuous integration (CI) to lint every Rust target, test the VS Code extension, audit
  npm dependencies, and exercise isolated Cargo feature combinations.

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

### Security

- Updated the VS Code extension's Vitest, Vite, and esbuild development toolchain to patched
  versions; the CI high-severity npm audit now completes with zero known vulnerabilities.
- Vendored AJV 8.12.0 now has a byte-for-byte SHA-256 integrity test tied to its reviewed upstream
  artifact.
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
