# Contributing to mini-agent

Thanks for helping. This file covers the rules every change must follow. By participating you
agree to the [Code of Conduct](CODE_OF_CONDUCT.md).

**Security problems are not reported as issues or pull requests.** Follow [SECURITY.md](SECURITY.md).

## Repository layout

The production Rust crate is the repository root (`Cargo.toml`, `src/`, `docs/`). The separate
`spike/` crate is a QuickJS research artifact and is never a production target.
[ARCHITECTURE.md](ARCHITECTURE.md) and [AGENTS.md](AGENTS.md) describe the design and its invariants;
the phase documents indexed in [docs/specs/00-index.md](docs/specs/00-index.md) are normative.

## Build and test rules

Run everything from the repository root:

```bash
cargo fmt                      # required before every commit
cargo test                     # type checking and tests in one pass
cargo install --path . --debug # the development build/install command
```

- The `--debug` install is the contributor build: faster to compile and keeps `debug_assert!`
  checks on. The README's `cargo install --path . --locked` is the end-user source install.
- **Never** run `cargo build` or `cargo check`; `cargo test` catches type errors and runs the tests.
- **Never** use `--release` during development.
- CI also runs `cargo clippy --locked --all-targets -- -D warnings` across a matrix of feature rows
  (see `.github/workflows/ci.yml`). Code must compile warning-free with default features and with
  `--no-default-features`; gate optional code behind its owning feature.
- Dependency changes go only in the root `Cargo.toml` and `Cargo.lock`. Reuse existing dependencies
  instead of adding a second version, and keep optional or platform-specific dependencies behind
  their feature or target.
- Changes under `scripts/`, `.github/workflows/`, `packaging/` or the release documents are covered by
  `python3 -m unittest` suites in `scripts/tests/` and by `python3 scripts/check-package-metadata.py`.

## Tests and documentation

- Write tests for new non-TUI production code. A bug fix needs a regression test that fails without
  the fix.
- Changes to the JS runtime, broker, protocol or sandbox must preserve the
  [Phase 6 security invariants](docs/specs/phase-6-brokered-js-runtime.md) and be tested both at the
  Rust unit level and through a contained-worker integration test (`src/extras/js/tests/`).
- Update `docs/` when behaviour or contracts change: `docs/agent/` for user documentation,
  `docs/specs/` for normative contracts, and add new modules to the relevant document.
- Add a line to `CHANGELOG.md` under `## [Unreleased]` for user-visible changes.

## Issue tracking with beads

Work is tracked with [beads](https://github.com/steveyegge/beads) (`bd`) in an embedded Dolt
database under `.beads/`. Do not create markdown TODO lists.

```bash
bd ready                  # find available work
bd show <id>              # read the issue (its description is the spec)
bd update <id> --claim    # claim it
bd close <id>             # close it once the change and its tests land
```

Run `bd prime` for the full workflow. Do not switch this repository to a shared or server Dolt mode.
GitHub issues are welcome for bugs and feature requests from people without tracker access.

## Commits and pull requests

- Run `cargo fmt` and `cargo test` before committing.
- Use a conventional prefix (`fix(scope):`, `feat(scope):`, `docs:`, `test:`, `ci:`) and reference
  the beads id or GitHub issue in the subject.
- Explain the root cause and the fix in the commit body.

mini-agent is licensed under [GPL-3.0-only](LICENSE); contributions are accepted under the same
license.
