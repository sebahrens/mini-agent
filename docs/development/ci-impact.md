# CI change impact

`scripts/ci_scope.py` owns both the workflow's change filter and the input
exclusions used by `scripts/ci_fingerprint.py`. A branch push or pull request
with only the following changes runs `changes`, the formatting/repository
policy job, and the required `ci-success` aggregate:

- Unconsumed Markdown documentation and `.beads/` issue records.
- Review evidence, plans, and decision records under `docs/reviews/`,
  `docs/plans/`, and `docs/decisions/`, including JSON, logs, and images.
- GitHub issue/PR templates, `CODEOWNERS`, and Dependabot configuration.
  Repository policy checks still validate
  security contact routing and dependency-update configuration.

These paths also leave production and test pass fingerprints unchanged, so
including a review report in a later source change does not invalidate extra
cached results. The exemption is based on the owning directory or exact metadata
path, not on a blanket JSON or log extension rule.
File modes are included in pass fingerprints: changing an executable bit on a
build/test input invalidates its pass, while modes on excluded evidence do not.

Embedded agent documentation, normative specs, benchmark baselines, the ACP
registry, release source/license documents, unknown data files, build settings,
workflow actions, and production source remain inputs. Test-only changes retain
native coverage selected by paths and Rust `cfg`/`cfg_attr` attributes, including
multiline and per-item attributes anywhere in either diff revision. Removed
tests and removed gates retain their previous platform coverage. Negated
platform conditions conservatively retain both native platforms. A mixed change retains the checks
required by its code or tests.

Agent configuration such as `.claude/settings.json` remains an input because
production hook compatibility can consume it from the workspace.

Git diffs disable rename detection and use NUL-delimited paths: moving source
into a review directory still removes a production input and runs the matrix.
Pull requests compare their head with a unique merge base, so unrelated changes
on the target branch do not turn a documentation PR into a full run. For changed
Rust tests, platform markers from the current target revision are retained too.
Pushes compare their exact before/after revisions. Missing or ambiguous PR
ancestry conservatively runs the full matrix.
Tags, scheduled drift checks, full manual runs, and unresolved history retain
their existing full/fail-open policy. The explicit Windows manual proof scopes
are unchanged. The formatting job executes the scope, fingerprint, and workflow
filter regression tests on every normal push, including records-only changes.
The classifier logs its decision, fallback reason, and at most 20 changed paths.
Paths are JSON-escaped so embedded newlines cannot create misleading log entries.

When adding a consumer of a previously exempt path, update the classifier and
its negative-control tests in the same change. Unknown non-Markdown documentation
continues to run the matrix until its role is established.
