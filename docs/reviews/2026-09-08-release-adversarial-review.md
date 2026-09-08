# Release code review — 2026-09-08

Reviewed baseline: `6021ba1cee1725d52930e17d239b305d6ed9ad66` (v1.8.0). Tracking epic: **mini-agent-wldr**. Review and issue filing are complete; the epic remains open for its implementation findings. No production fixes are included.

Twenty-seven atomic findings are filed: **14 P1, 13 P2**. Each bead records the affected code, concrete failure, required change and verification criteria. The complete machine-readable findings are in [the review JSON](2026-09-08-release-adversarial-review.json).

| Bead | Priority | Finding | Evidence |
|---|---|---|---|
| mini-agent-wldr.1 | P1 | Guarded file replacement overwrites concurrent in-place user edits | Failing targeted regression |
| mini-agent-wldr.2 | P2 | Distilling to a user-selected file changes its existing parent directory permissions | Failing targeted regression |
| mini-agent-wldr.3 | P1 | Edit tool has no input/output file size bound and can exhaust the parent | Failing targeted regression |
| mini-agent-wldr.4 | P2 | One unsafe saved-session entry prevents lookup of unrelated healthy sessions | Failing targeted regression |
| mini-agent-wldr.5 | P1 | A language server that stops reading stdin hangs read/edit/write tools | Failing targeted regression |
| mini-agent-wldr.6 | P1 | MCP collision renaming can create duplicate registered tool names | Failing targeted regression |
| mini-agent-wldr.7 | P2 | MCP structured-only tool results are silently returned as empty output | Failing targeted regression |
| mini-agent-wldr.8 | P2 | Streaming a code fence repeatedly reparses the whole fence quadratically | Failing targeted regression |
| mini-agent-wldr.9 | P2 | Streaming Markdown mistakes a shorter code fence for a closing delimiter | Failing targeted regression |
| mini-agent-wldr.10 | P1 | Nightly evaluation drops the first persona metric and never runs the library axis | Current nightly CI log |
| mini-agent-wldr.11 | P1 | A bounded read can allocate an entire arbitrarily long source line | Code and locked dependency trace |
| mini-agent-wldr.12 | P1 | Default provider streams have no connection or inactivity deadline | Code and locked dependency trace |
| mini-agent-wldr.13 | P1 | Concurrent hook decisions overwrite another call's required approval | Failing targeted regression |
| mini-agent-wldr.14 | P1 | Hook approval decisions are ignored when tools use different permission keys or no inner check | Failing targeted regression |
| mini-agent-wldr.15 | P2 | Post-tool hooks receive original arguments after a pre-hook input rewrite | Failing targeted regression |
| mini-agent-wldr.16 | P1 | A transient skill-store startup failure is cached as healthy and never recovers | Failing targeted regression |
| mini-agent-wldr.17 | P2 | Skill observation and proposal startup block the async executor on SQLite and model initialization | Failing targeted regression |
| mini-agent-wldr.18 | P2 | Proposal-enabled learned-skill sessions load the local embedding model three times | Code trace |
| mini-agent-wldr.19 | P2 | macOS worker cold startup and cancellation recovery exceed the recorded latency targets | Current CI artifacts |
| mini-agent-wldr.20 | P2 | macOS 26 CI evidence incorrectly reports that no production worker is available | Current CI artifacts |
| mini-agent-wldr.21 | P2 | Mid-turn skills_search removes task-outcome credit for skills invoked before the search | Failing targeted regression |
| mini-agent-wldr.22 | P2 | Lost invocation telemetry is counted as a successful no-skill baseline | Failing targeted regression |
| mini-agent-wldr.23 | P1 | Telemetry retry on a busy SQLite writer can prevent process shutdown indefinitely | Failing targeted regression |
| mini-agent-wldr.24 | P1 | A failed headless turn discards completed tool history and usage after effects already ran | Code trace |
| mini-agent-wldr.25 | P1 | Immediate skill quarantine is lost when index publication is pending | Failing targeted regression |
| mini-agent-wldr.26 | P1 | ACP forgets completed tool effects when a turn fails or is cancelled | Failing targeted regression for the error path; cancellation code trace |
| mini-agent-wldr.27 | P2 | Skill statistics approach cubic query work as retained history grows | Exact production SQL on an indexed system-SQLite fixture |

## Verification evidence

- `cargo test --features skills`, run with local socket and nested sandbox access: **2,773 unit tests passed, 13 ignored; eight integration tests passed**. The initial restricted run had 29 failures, including denied local sockets/nested containment and one timing outlier. It is not used to claim product failures; the unrestricted suite passed.
- `python3 -m unittest discover -s scripts/tests`: **235 passed**.
- VS Code `npm run typecheck`, `npm run lint`, `npm test`: passed, **57 tests across nine files**.
- Package metadata for `v1.8.0`, feature graph, workspace boundary, Windows MSI source/wiring and dependency-policy checkers: passed.
- `cargo install --path . --debug --locked --offline --root /tmp/mini-agent-release-review`: passed. Installed binary reports `mini-agent 1.8.0`; `--help` succeeded. The first non-offline install attempt failed because the restricted environment could not resolve crates.io; no compilation defect was inferred.
- [Main CI 34190987872](https://github.com/sebahrens/mini-agent/actions/runs/34190987872): success at the reviewed SHA.
- [Nightly evaluation 34199175478](https://github.com/sebahrens/mini-agent/actions/runs/34199175478): failure at the reviewed SHA. Its first persona metric shares libtest's progress line, so the anchored extractor finds eight of nine records. The library-axis step is skipped (finding .10).

## Reproducing the experimental findings

The temporary tests were removed after execution. Their exact patches are preserved for an isolated checkout at the reviewed SHA; they deliberately fail until the findings are fixed:

```sh
git apply docs/reviews/2026-09-08-probes-core.patch
git apply docs/reviews/2026-09-08-probes-integrations.patch
git apply docs/reviews/2026-09-08-probes-hooks.patch
git apply docs/reviews/2026-09-08-probes-skill-startup.patch
git apply docs/reviews/2026-09-08-probes-telemetry.patch
git apply docs/reviews/2026-09-08-probes-acp.patch
cargo test --features acp,skills,lsp,hooks release_review_probe -- --nocapture
```

The ACP patch covers .26. Its in-memory protocol test wrote a real temporary file, emitted a completed tool call/result, then returned a terminal error. The next prompt received an empty history despite the completed effect. The test failed under `cargo test --features acp,skills release_review_probe_acp -- --nocapture`; its temporary source edit was removed. Cancellation follows the same source omission but was not dynamically reproduced. Across the six patches, 20 Rust probes cover 19 findings.

The telemetry patch covers .21–.23 and .25. All four probes failed using local fixtures under `cargo test --features skills`: an earlier invocation lost its outcome link after an actual mid-turn search, an observability-lost turn became a no-skill baseline, runtime shutdown waited 879 ms for a separate SQLite writer, and a pending index generation caused immediate quarantine to be skipped permanently. The temporary source edits were removed after execution.

The skill-startup patch covers .16–.17. Both probes failed under `cargo test --features skills release_review_probe -- --nocapture`: a real SQLite write lock caused permanently empty learned retrieval after unlock, and observation startup blocked a current-thread timer for 416 ms. The recovery test exercises the actual workspace cache and full service opener; the responsiveness probe targets the synchronous startup helper invoked directly by the full opener.

The hooks patch covers .13–.15 with four probes; `cargo test --features hooks,skills release_review_probe -- --nocapture` reproduced all four failures. Its permission probes use fixture tools against the production decorator/checker and, for concurrency, the production scheduling wrapper. The input-rewrite probe captures an actual post-hook subprocess envelope.

The core patch covers .1–.4. The integration patch covers .5–.9 and adds modes to the existing repository-owned MCP/LSP fixture programs. Each test cleans up its temporary data or child process before asserting, except the session test whose existing RAII fixture cleans up on unwind. The patches contain regression probes, not implementations of the proposed fixes.

Observed failures:

- Guarded replacement returned success and persisted an agent edit over a concurrent same-inode user change.
- Export changed the existing directory from `0755` to `0700`.
- Edit accepted and rewrote a 2 MiB file despite the 1 MiB default text-file budget.
- An unrelated broken session symlink made exact-prefix session discovery return an error.
- A language server that stopped reading after initialization held document sync until external cancellation. The probe's 500 ms limit demonstrates the blocked boundary; it is not a prescribed production timeout.
- Three MCP servers produced registered names `[alpha__probe, beta__probe, alpha__probe]`.
- A structured MCP response containing `answer: 42` became empty output delimiters.
- A 19,008-byte, 1,000-line code fence caused **9,517,500 bytes** of Markdown parsing.
- A four-backtick fence containing a triple-backtick line rendered a literal heading differently during streaming and after finalization.
- Concurrent read calls with two hook ask verdicts produced one denied call and one successful call without an approval channel.
- Hook ask verdicts fell through for a Git-shaped permission key and for a tool without an inner permission check.
- A pre-hook changed the executed path to `actual-target`, while the post-hook envelope still identified `original-target`.
- Releasing a SQLite writer after degraded startup left one installed skill undiscoverable on the next turn, with no cache failure diagnostic.
- A 400 ms SQLite writer delayed the observation-startup probe’s 20 ms async timer by 416 ms.
- Mid-turn search left a previously invoked skill with zero task-outcome links.
- An explicit observability-lost turn counted as one no-skill baseline task.
- Runtime teardown waited 879 ms for a writer whose external watchdog released it after 700 ms; the retry loop has no shutdown bound.
- A capability-denied skill remained active and visible after the pending index generation rebuilt.

## Statistics scaling evidence

Run `python3 docs/reviews/2026-09-08-stats-scaling.py` from the repository root. The [recorded results and query plan](2026-09-08-stats-scaling.json) use the exact SQL extracted from `load_skill_stats`, with the relevant production indexes and one observed task plus one baseline per revision under the same oracle.

| Revisions | Outcomes | SQLite VM steps (rounded to 100) | Illustrative elapsed time |
|---:|---:|---:|---:|
| 20 | 40 | 177,700 | 3 ms |
| 40 | 80 | 1,215,400 | 21 ms |
| 80 | 160 | 8,942,800 | 174 ms |

Doubling the fixture multiplied work by 6.84 and 7.36. The correlated baseline scans and nested source lookups explain the growth (.27). This uses system SQLite 3.53.2 and a reduced schema, not the application’s bundled SQLite; that distinction is part of the bead’s acceptance criteria. Each sample has a 50-million-VM-step abort bound. An initial larger run was explicitly interrupted and is not used as completed evidence.

## Platform evidence

The baseline run’s actual containment summaries and aggregate measurements are preserved in [platform evidence](2026-09-08-platform-evidence.json), with original run and artifact identifiers. The dedicated gates reported success on Linux, macOS 15/26 and Windows; Windows also passed under a separate non-admin user. The macOS 26 summary mislabels availability despite recording a running worker (.20).

| Recorded runner | Cold Ready p95 | Warm call p95 | Cancel/recover p95 | Maximum idle private memory |
|---|---:|---:|---:|---:|
| Linux x86_64 | 12.931 ms | 1.103 ms | 23.735 ms | 20,684,800 bytes |
| macOS 26 ARM64 | 1,879.391 ms | 1.232 ms | 1,900.945 ms | 4,211,712 bytes |
| Windows x86_64 | 29.392 ms | 1.939 ms | 39.499 ms | 2,211,840 bytes |

Each record uses an installed v1.8.0 debug production binary, 10 warmups and 100 measured samples. All recorded platforms observed one worker and zero idle runtimes. macOS missed its 300 ms cold and 1,000 ms recovery targets in both matched-runner runs (.19); these targets are informational and the results are not generalized to other hosts or release-profile binaries. Warm calls met the target. The checked-in benchmark manifest remains a pending manifest; these downloaded CI records supply the current evidence.

## Scope and limits

Review coverage: current-state inventory, canonical Phase 6 invariant read, baseline gates, targeted file-publication/session-discovery/MCP/LSP/streaming/distiller attacks, current CI failure analysis, and targeted hook concurrency/verdict/input-rewrite attacks, skill-startup contention/recovery probes, inspection of the actual cross-platform containment/resource artifacts, task-outcome attribution and completeness, telemetry teardown, quarantine/publication interleavings, and headless terminal-failure persistence. Packaging source checks confirmed that all-zero pre-release recipe checksums are intentional and rejected by the post-release validation gate; no recipe-checksum defect was inferred. The containment pass traced descriptor closure and empty-root Linux launch, seccomp finalization, macOS one-time-image/guardian launch, Windows LPAC creation attributes and Job limits, bounded wire framing, supervisor I/O cancellation, and grant/audit revalidation. Existing hostile tests were inspected for denied execution, durable audit ordering, fresh runtimes, bounded jobs and redaction; the baseline suite and dedicated platform gates provide execution evidence. No additional authority-expansion finding was confirmed in that pass. The prior closed reviews were treated as leads, not proof of current behavior.

The final local pass also inspected admission authorization and optimistic commits, lifecycle replacement state validation, retention watermark and purge behavior, runner cancellation settlement, TUI partial-progress persistence, ACP terminal paths, and installer/archive/MSI contracts. Existing admission tests cover stale reviews, exact authorization binding, rollback and concurrent consumption; retention tests cover idempotent compaction and ineligible-prefix boundaries. No additional defect was confirmed beyond the filed findings in that pass.

Review and issue filing are complete for this scope. The 27 findings remain open for implementation and acceptance testing; this report does not approve the release. Cross-platform installer execution and exhaustive concurrency exploration were not performed locally. Containment findings and measurements apply to the reviewed code and recorded reference runners; they do not establish safety against every possible native exploit. Findings .11, .12, .18 and .24 are code-confirmed rather than dynamically reproduced; their beads require bounded-memory, stalled-provider, embedding-construction and headless-failure persistence tests. ACP cancellation and statistics performance on the actual bundled database remain explicit acceptance work under .26 and .27.
