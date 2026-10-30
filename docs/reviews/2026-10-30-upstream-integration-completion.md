# Upstream integration completion evidence

The delivered code and workflow revision is `12b85985584277e2bfa67efc0e12a12910b28509`.
This record completes the pending hosted-validation entries in the earlier
[upstream integration review](2026-10-30-upstream-integration.json) and
[evidence fixes review](2026-10-30-evidence-fixes.json), which retain their pre-push observations.

[Main CI run 41028632185](https://github.com/sebahrens/mini-agent/actions/runs/41028632185)
completed successfully for that exact revision: 78 successful jobs, four policy-permitted skips,
and a successful required `ci-success` gate. Native Linux sandbox policy and the all-features
suite passed. The four heavy-gate skips follow the existing branch-push policy; they are not
claims of additional native worker evidence.

[Scoped native Windows run 41028658528](https://github.com/sebahrens/mini-agent/actions/runs/41028658528)
also completed successfully for the same revision. It required all 46 general-sandbox policy and
recovery regressions, the exact installed-preflight observer controls, and the installed-binary
write, authority, desktop, network, and parent-owned Job boundary probes.

Coordinated local validation passed: 93 affected Rust tests, 22 minimal-feature tests, 12 final
image/pressure tests, the strengthened terminal-receipt regression, six ACP parity tests and 53
headless contract tests. Strict all-targets Clippy passed with skills enabled and with no default
features. The debug install passed its native macOS 26.4.1 JavaScript self-check. The final
macOS worker-reuse test also passed separately with default features and the CI temporary-root
setting. Python policy and affected suites, and VS Code typecheck/lint/tests, passed as recorded
in the earlier reviews. Cargo execution was coordinated in one checkout; no full local Rust
suite or duplicate feature matrix was run.

The supplied 321-path patch was accounted for against its snapshot, with containment, legacy
path compatibility, release-documentation, and CI-contract corrections recorded in the reviews.
The five follow-up issues are closed: `mini-agent-tmryb`, `mini-agent-h2bz4`, `mini-agent-8tpau`,
`mini-agent-r0uzf`, and `mini-agent-bygyx`. Integration tracking is `mini-agent-wc4n4`.

Finite historical-result retention remains an explicit opt-in setting; an existing configured
value of eight must be removed to use the new default. Retained compacted images remain bounded
to eight distinct images and 7 MiB of encoded pixels, with explicit historical omission metadata.
Character-window recovery and image replay preserve existing permissions, import boundaries,
model capability checks, and feature availability.
