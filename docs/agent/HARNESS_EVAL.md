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

## Delegated versus direct lane

`scripts/delegation_eval.py` (mini-agent-964ao) is an operator-run lane, not a CI
check: it needs a configured provider and costs money. It runs each task of
`tests/harness_eval/delegation/tasks.json` twice with a real binary, once with the
`task` tool enabled and once with `[subagents] task_enabled = false`, checks the
expected files, and compares tokens and cost per task together with the
`subagent_delegation` telemetry of the headless result (contract pass, repair
used, host replacement, turns, cost units, stop reason). `--summarize FILE`
compares records of an earlier run offline. A baseline run is still to be
recorded next to the fixtures as `baseline.json` (first run: see Measured
2026-11-22 below); the published delegation numbers are ten-task samples by the authors of a competing method, so this lane
is how this harness gets its own. `scripts/tests/test_delegation_eval.py` checks
the script offline.

### Measured 2026-11-22

Live runs through OpenRouter with `anthropic/claude-sonnet-5.5` (mini-agent-zmhfp),
about 6.0 USD in total; raw records are in
`tests/harness_eval/delegation/baseline.json`. Every figure is a handful of runs
by one operator, so none carries a confidence interval and a difference of one
task or of a few cents is not evidence. Costs are the provider-reported cost and
move with prompt-cache warmth, which differed between runs. The second model
(`z-ai/glm-5.3`) was not run: the budget went to sonnet repeats.

Findings about the tooling (not patched here):

- `scripts/delegation_eval.py` writes `[subagents]\ntask_enabled = ...`, but
  `task_enabled` is a top-level key; the binary refuses the file (`config is not
  valid TOML ... unknown field`), so every record is an all-zero failure. The
  runs below used a local copy that writes `task_enabled = ...` at top level.
- The script creates its scratch directories with `tempfile`, which is under
  `/var/folders` on macOS; the binary rejects it (`private paths reject symlinked
  path components: /var`). Setting `TMPDIR` to a directory without symlinks
  works around it.
- The record keeps `contract_pass`, `repair_used` and `host_replaced` but drops
  `contract_repaired`, `contract_unstructured` and `contract_normalized`, so
  validated, repaired and unstructured cannot be told apart from its output; the
  local copy also stored the whole `subagent_delegation` aggregate.
- `tasks.json` has three tasks; `independent_questions` fails in both arms
  because its expected text `docs: 2` / `src: 1` is not what the prompt asks for.
  Two tasks were added for the runs (`find_definition_module`,
  `count_test_functions`).
- No run emitted a debug line naming a cache fork (see compaction below).

**Delegation lane** (5 tasks, both arms, one repetition each unless noted).

| Prompt | Arm | Tasks correct | Total tokens | Cost (USD) | Children |
|---|---|---:|---:|---:|---:|
| natural | delegated | 4/5 | 508,247 | 0.413 | 0 |
| natural | direct | 4/5 | 432,141 | 0.353 | 0 |
| told to delegate, run A | delegated | 5/5 | 551,641 | 0.594 | 5 |
| told to delegate, run A | direct | 5/5 | 434,016 | 0.364 | 0 |
| told to delegate, run B | delegated | 5/5 | 584,574 | 0.608 | 5 |
| told to delegate, run B | direct | 5/5 | 409,557 | 0.359 | 0 |

With the natural prompts the model never chose to call `task` on these small
tasks, so the delegated arm only pays for the extra tool schema (about +17%
tokens, +17% cost). Telling it to delegate gave 10 children: contract outcomes
(run B, from the aggregate) validated 2, repaired 2, unstructured 1 (host
replaced 1), normalized 0; run A recorded validated 3 and repair used 2 (the
script's own record cannot split the rest). Accuracy was equal in both arms, and
delegation raised cost per task by about 60-70% on these tasks that a single
agent solves in three or four turns. These tasks cannot show a benefit of
delegation; they bound its overhead and show that the repair turn ran for 5 of
the 10 children (2 of 5 in run A, 3 of 5 in run B) and one report was replaced
by the host as unstructured.

**Sibling prompt-cache reuse** (one `task` call, four children, three runs per
setting, fixture of four tiny files, each run with a unique `AGENTS.md` nonce).

| `task_stagger_ms` | Child cached/input per run | Cached input over four children | Run cost (USD) | `calls_with_differing_prefixes` |
|---:|---|---:|---:|---:|
| 0 | 0.49, 0.49, 0.49, 0.49 (all 3 runs) | 28,308 | 0.163, 0.164, 0.164 | 0, 0, 0 |
| 750 | 0.49 for the first, 0.97 for the other three (all 3 runs) | 48,861 / 47,993 / 48,861 | 0.114, 0.108, 0.114 | 0, 0, 0 |

Without stagger every child read only the same ~7,077 cached tokens; with it,
the later children also read the first child's prompt (~13,900 tokens). The
effect was identical in 3 of 3 runs per setting and cut the run cost by about
a third. One stagger run had all four children on the `plain` contract path
(smaller prompt, 14,077 input tokens each) with the same ratios. The fixture is
tiny (14k-token child prompts) and Anthropic via OpenRouter only.

**Context inheritance** (`experimental_inherit_context = true`; turn 1 states
facts with a correction, turn 2 forks the saved session and asks for one `task`
child; three tasks).

| Run | Arm | Correct | Cost (USD) | Child tokens |
|---|---|---:|---:|---:|
| A (inherit told to keep the objective to one sentence) | fresh brief | 3/3 | 0.099 | 22,208 |
| A | `inherit_context: true` | 2/3 | 0.079 | 40,172 |
| B (inherit told to state the question, not the facts), 2 reps | fresh brief | 6/6 | 0.120 | 47,260 |
| B | `inherit_context: true` | 6/6 | 0.158 | 63,086 |

In run A the one failure was a child that did not know the question because the
parent's objective omitted it (the current prompt is not in the inherited
transcript), a prompting error rather than a model error. Otherwise both ways
were correct. The inheriting child used 33% to 81% more tokens. Cost differences
between arms are dominated by cache warmth and are not interpretable at this n.
These facts are short enough for a fresh brief to carry; no accuracy gain from
inheritance was observed, so the data do not support shipping it.

**Compaction on OpenRouter** (`--config context_window=60000`, eight 41 KB
files read in one turn).

| Measure | Value |
|---|---|
| Run cost / compactions | 0.994 USD / 7 (all "mid-turn context relief") |
| Summariser requests | 362,813 input, 223,538 cached (62%), 139,247 cache-write, 6,282 output tokens |
| Summariser cost | 0.433 USD, 44% of the run |
| First compaction | 50,144 of 50,951 input cached, 0.015 USD |
| Compactions 2 to 7 | 28,899 cached (system prompt only), about 23,000 cache-write, 0.068-0.071 USD each |
| Between-turn compaction (resumed session, window 60000) | 1 compaction, 7,605 input, 0 cached, 861 cache-write, 0.026 USD |

No debug line mentioning a cache fork appeared in either run. The first
mid-turn compaction did reuse the previous prompt's cache; later ones did not
and cost about as much as an uncached request, and the next main-model request
after each also wrote about 22,000 tokens to the cache. At this small
window each compaction roughly doubled the cost per file read. One run per
setting; summariser behaviour at larger windows was not measured.
