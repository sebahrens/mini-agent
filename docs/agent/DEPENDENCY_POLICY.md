# Dependency security and license policy

The committed `Cargo.lock` is the dependency-review artifact. CI rejects a
manifest that cannot resolve with `--locked`, audits that exact file, applies
license and source rules from `deny.toml`, and fails if any policy command
changes the lockfile.

## Enforced gates

- `cargo audit` fails for RustSec vulnerabilities rated medium or higher,
  unsound advisories, and yanked releases. It fetches a current, non-stale
  advisory database.
- `cargo deny --locked check bans licenses sources` rejects wildcard version
  requirements, unapproved licenses, unknown registries, and unknown git
  sources. Git exceptions must pin a full revision.
- `cargo deny check advisories` refreshes advisory and yank data after the
  locked metadata preflight. A final `git diff -- Cargo.lock` prevents this
  networked scan from silently changing the reviewed dependency snapshot.
- Direct unmaintained dependencies fail. Transitive unmaintained notices are
  visible warnings and must be assessed when their parent dependency is
  updated.
- `python3 scripts/check_feature_graph.py` enforces the supported feature
  relationships, verifies every optional dependency through its owning
  feature's semantic closure, and runs focused `cargo tree
  --no-default-features` checks. It also normalizes the required test and
  Clippy matrices in `.github/workflows/ci.yml`, so optional JS, skill,
  embedding, MCP, ACP, and LSP dependencies cannot leak into disabled rows and
  required CI rows cannot silently disappear.

The Monday scheduled CI run executes the dependency gate even when no source
or lockfile changed. Pull requests and pushes run it as part of normal CI.

`cargo audit` does not provide a `--locked` option: it reads a lockfile directly.
CI therefore uses `cargo audit --file Cargo.lock`, preceded by
`cargo metadata --locked --all-features`. The `--locked` flag on the
`cargo install` commands locks the policy tools themselves, not the project.

## Exceptions

Fix or replace the dependency first. If a temporary exception is unavoidable:

1. Add one entry to `dependency-exceptions.toml` with an exact advisory or
   SPDX ID, exact `crate@version` (or exact HTTPS source URL), accountable
   `@github-handle`, rationale and removal plan, creation date, and expiry.
2. Project the same exception into both advisory ignore lists, the exact
   `licenses.exceptions` entry, or the exact source allow-list entry as
   appropriate.
3. Keep the duration at 90 days or less. Renewals require a new review and
   updated rationale; expired entries fail CI.
4. Assign the owner to remove the exception and dependency projection before
   expiry.

`python3 scripts/check_dependency_policy.py` rejects orphaned projections,
missing projections, malformed ownership, overlong durations, and expired
entries. `python3 scripts/tests/test_check_dependency_policy.py` proves that a
synthetic denied license, high-severity advisory, unknown git source, and
expired exception are rejected.

## Updates

Dependabot opens weekly Cargo and GitHub Actions updates. Minor and patch
updates may be grouped; major updates remain separate pull requests so their
security and compatibility impact is reviewed independently. No dependency
updates are auto-merged.

Tool versions are pinned under `[workspace.metadata.dependency-policy]` in
`Cargo.toml`. Update the pin and CI install command together, review the
tooling changelog, then run:

```bash
python3 scripts/tests/test_check_dependency_policy.py
python3 scripts/check_dependency_policy.py
cargo metadata --locked --all-features --format-version 1 > /dev/null
cargo audit --file Cargo.lock
cargo deny --locked check bans licenses sources
cargo deny check advisories
git diff --exit-code -- Cargo.lock
```
