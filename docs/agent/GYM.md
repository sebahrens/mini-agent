---
title: "Skill Gym"
description: "Prepare isolated learned-skill state, mine task candidates, run paired library episodes, and interpret non-production reports."
---

# Skill Gym

The Skill Gym is an operator-run evaluation harness for learned JavaScript skills. It compares the
same task in a `none` arm and a `library` arm, runs a mechanical oracle, and writes one
`GYM_OUTCOME` JSON record per episode. It is not a release gate and it never turns evaluation data
into production evidence.

## Prepare a host

From the repository root:

```bash
scripts/gym/setup.sh
```

The setup script checks the required Rust/Python tools, installs the debug binary with
`cargo install --path . --debug`, exercises the platform worker-containment test, creates a private
gym root, and imports, approves, and activates the seed library through the shipped local-owner
commands. Override the default repository-local `.gym` root with `MINI_AGENT_GYM_ROOT`.

The root contains separate `data`, `local`, `state`, and `cache` directories. Every gym process
sets the corresponding `ZS_*_DIR` variables and `MINI_AGENT_GYM=1`; the operator's normal skill
database, sessions, memory, and effect audit are not selected. Treat the directory as sensitive
test state anyway: an explicitly configured external command or an unsandboxed model can still use
the authority of the account running it.

There is no runtime `gym.toml` configuration parser. Script options and the task JSON are the
configuration surface; normal mini-agent settings, including `verify_command`, keep their ordinary
documented defaults.

## Task files

`scripts/gym/mine_tasks.py` reads an explicit JSON map from bead ID to oracle command, finds fix
commits naming those beads, and optionally verifies that each oracle fails at the parent and passes
at the fix. It emits a JSON array whose entries contain:

- `name`, `prompt`, `base_commit`, and classification `tags`;
- bounded `initial_files` and expected post-fix files;
- `oracle.command` and a stable `oracle.id`;
- `budgets` for provider turns, tool calls, and tokens;
- optional scripted provider turns, the selected `library`, and the fix commit used only for
  provenance.

The deterministic CI fixture at `tests/harness_eval/task.json` uses a compact object with
`defaults` plus a `tasks` array. A task overrides any default field. Its oracle uses
`expected_files`; `scripted_provider_turns` supplies separate `none` and `library` scripts. The
Rust harness materializes a fresh workspace and real `SkillStore` for every arm, checks exact files
and budgets, and records evaluator-oracle outcomes with `production=false`.

## Run paired episodes

```bash
scripts/gym/train.sh \
  --tasks path/to/tasks.json \
  --output path/to/outcomes.jsonl \
  --binary mini-agent
```

For each array entry, the runner creates a fresh detached worktree at `base_commit` (or an empty
isolated directory if Git cannot materialize it), overlays `initial_files`, and runs both arms. The
library arm installs either the shipped seeds or the bundle named by `library`, then approves and
root-activates every newly awaiting revision. Each arm has its own AppPaths tree, so it cannot
observe the other arm's library or session state.

The runner invokes `mini-agent -p`, then executes the task's oracle with a 300-second bound. A row
passes only when both processes exit successfully. The JSONL fields are `task`, `arm`, `success`,
`oracle_id`, `elapsed_ms`, `production`, `agent_exit`, and `oracle_exit`; `production` is always
false. Standard output repeats each row with a `GYM_OUTCOME ` prefix for streaming collection.

Compare success counts and elapsed time only across matching task/oracle IDs and always report the
sample size. These records do not currently include reliable provider-token or dollar-cost fields,
so enforce cost outside the script (for example with a provider account limit) before a live-model
run. The deterministic task-json test is the supported no-provider regression path.

The optional successful-step distiller, resumable live-model experiment manager, built-in dollar
cost cutoff, aggregate Wilson report, and evidence-derived retrieval labels are deferred. No gym
output should be presented as production utility evidence or used to promote a skill automatically.
