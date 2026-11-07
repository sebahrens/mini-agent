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

Embedded agent documentation, normative specs, benchmark baselines, the ACP
registry, release source/license documents, unknown data files, build settings,
workflow actions, and production source remain inputs. Test-only changes retain
their existing test scope and platform rules. A mixed change retains the checks
required by its code or tests.

Agent configuration such as `.claude/settings.json` remains an input because
production hook compatibility can consume it from the workspace.

Git diffs disable rename detection and use NUL-delimited paths: moving source
into a review directory still removes a production input and runs the matrix.
Tags, scheduled drift checks, full manual runs, and unresolved history retain
their existing full/fail-open policy. The explicit Windows manual proof scopes
are unchanged. The formatting job executes the scope, fingerprint, and workflow
filter regression tests on every normal push, including records-only changes.

When adding a consumer of a previously exempt path, update the classifier and
its negative-control tests in the same change. Unknown non-Markdown documentation
continues to run the matrix until its role is established.
