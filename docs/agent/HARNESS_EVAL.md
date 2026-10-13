---
title: "Deterministic harness evaluation"
description: "Fixture repositories, scripted provider turns, regression budgets, and the nightly CI task-level evaluation."
---

# Deterministic harness evaluation

The task-level harness evaluation checks whether the agent loop can finish representative repository work without contacting a live model provider. It complements unit tests: a scripted `MockCompletionModel` drives the real headless runner and tool loop, and each case operates on a fresh repository materialized from a fixture in `tests/harness_eval/fixtures/`.

The five cases cover:

- editing a CRLF file without changing its line-ending convention;
- aggregating multiple files through the brokered JavaScript tool;
- ordered multi-prompt fan-out through the production task scheduler;
- continuing work with a compacted recap and retained conversation tail; and
- resolving and invoking the built-in `rust-security-review` persona.

A second fixture repository in `tests/harness_eval/personas/fixture.json`
contains one focused expected-finding case for each shipped persona. It checks
persona resolution and passes each scripted report through the production task
scheduler, which enforces the shared Findings/Unverified/Coverage contract.
Successful persona cases emit `PERSONA_EVAL` JSON records.

Each fixture declares upper bounds for provider turns, tool calls, and reported tokens, plus the exact expected repository contents. Any missing, rolled-back, or unexpected mutation fails the case. Successful cases emit one JSON metric record prefixed with `HARNESS_EVAL`.

Run the complete evaluation locally with:

```bash
cargo test --locked harness_regression_eval -- --ignored --nocapture --test-threads=1
cargo test --locked persona_regression_eval -- --ignored --nocapture --test-threads=1
```

The regular test suite runs a lightweight fixture-contract check. `.github/workflows/harness-eval.yml` runs the complete evaluation nightly and uploads the five JSON metric records as the `harness-eval-metrics` artifact. Provider responses remain scripted so thresholds are deterministic; changing a script or fixture requires reviewing and deliberately updating its bounds.
The same artifact also includes nine persona-evaluation records, one for every
compiled-in definition.

## Paired task.json library axis

`tests/harness_eval/task.json` adds twenty compact tasks: ten history-themed cases and ten Cargo
manifest/lockfile cases. The top-level `defaults` object supplies `prompt`, `initial_files`, an
exact-file `oracle`, `budgets`, per-arm `scripted_provider_turns`, and the library name; entries in
`tasks` provide a unique `name`, tags, and any overrides. Each task runs once without a learned
library and once with an active skill in a real `SkillStore`, using fresh workspace and turn
contexts. The library script must retrieve and invoke the export; both arms must satisfy the same
oracle and deterministic provider/tool/token/JS-round-trip budgets.

Run that contract with:

```bash
cargo test --no-default-features --features js,skills,sandbox,subagents \
  task_json_library_axis_uses_real_store_and_records_oracles -- --nocapture
```

The test records forty non-production oracle outcomes and links only the twenty library-arm rows
to a durably invoked skill. Its equal metric delta is a library-invocation regression check, not a
claim of real-model utility. See [GYM.md](GYM.md) for the separate operator runner and its limits.

The regular `five_native_reads_survive_unpruned_fake_provider_compaction_and_history_roundtrip`
regression reads five owned fixture files through the native read tool, sends the
retained results to an actual scripted Rig summarizer request with live projection
retention set to zero, serializes/restores and rewinds/redoes the session, and
verifies the next model request still contains all five historical read-request
paths and the retained tail. Provider usage is charged once. The scripted model
intentionally omits the names from its prose so the structural file inventory is
required for the continuation proof. No paid provider is contacted.

The process regression
`headless_compaction_instructions_and_retained_read_history_reach_actual_provider`
resumes a saved session against a bounded local HTTP fixture. It proves
`--compact-instructions` reaches every summarizer request, zero live-result
retention does not clear five retained details before summarization, the next
provider request includes all five paths, and summarizer plus turn usage is
persisted exactly once. These are context-delivery regressions, rather than an
assessment of real-model summary quality.
