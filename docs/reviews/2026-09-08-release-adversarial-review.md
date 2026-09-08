# Release adversarial review — 2026-09-08

Review checkpoint at `6021ba1cee1725d52930e17d239b305d6ed9ad66` (v1.8.0). Tracking epic: **mini-agent-wldr**, in progress. No production fixes are included in this checkpoint.

Fifteen atomic findings are filed: **9 P1, 6 P2**. Each bead records the affected code, concrete failure, required change and verification criteria. The complete machine-readable findings are in [the review JSON](2026-09-08-release-adversarial-review.json).

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
cargo test --features skills,lsp,hooks release_review_probe -- --nocapture
```

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

## Scope and limits

Completed in this checkpoint: current-state inventory, canonical Phase 6 invariant read, baseline gates, targeted file-publication/session-discovery/MCP/LSP/streaming/distiller attacks, current CI failure analysis, and targeted hook concurrency/verdict/input-rewrite attacks. Broker cancellation, grant revalidation and runtime-limit ordering received an initial read with no additional confirmed finding; broader containment coverage remains pending. The prior closed reviews were treated as leads, not proof of current behavior.

This checkpoint does **not** establish completion of the full release review. Deep containment and broker/protocol validation, permission and hook interleavings, learned-skill admission/lifecycle and broader platform release behavior remain under the active epic. No claim is made that all possible bugs have been found. Findings .11 and .12 are code-confirmed rather than dynamically reproduced; their beads require bounded-memory and stalled-provider regression tests.

