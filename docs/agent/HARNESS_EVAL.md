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
