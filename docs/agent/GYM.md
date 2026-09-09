---
title: "Skill Gym"
description: "Prepare an isolated gym host, mine task candidates, run paired library episodes, and interpret non-production reports."
---

# Skill Gym

The Skill Gym is an operator-run evaluation harness for learned JavaScript skills. It compares the
same task in a `none` arm and a `library` arm, runs a mechanical oracle, and writes one
`GYM_OUTCOME` JSON record per episode. It is not a release gate and it never turns evaluation data
into production evidence.

## Prerequisites

- Linux or macOS. `scripts/gym/setup.sh` exits 2 on any other host, and the runner shells out to
  `/bin/sh` and `git worktree`.
- `cargo` and `rustc` matching `rust-toolchain.toml` (setup compares the exact version), `git` 2.40
  or newer, Python 3.11 or newer, and `jq`.
- `bd` (beads) only for `scripts/gym/mine_tasks.py`; the miner falls back to an exported
  `.beads/issues.jsonl` or an explicit `--beads-json` file when `bd` is missing or fails.
- A workspace outside `/private/tmp`. Setup refuses that prefix because it is the Seatbelt test
  boundary.

## Prepare a host

```bash
scripts/gym/setup.sh [repository]
```

The optional first argument is the repository to build and evaluate; it defaults to the current
directory. The gym root defaults to `<repository>/.gym` and is overridden with
`MINI_AGENT_GYM_ROOT`. Episode worktrees and per-episode AppPaths trees are created under that
root, so ignore it in Git or point `MINI_AGENT_GYM_ROOT` outside the repository.

Setup checks the prerequisites, creates `<gym root>/worktrees` and `<gym root>/runs`, then runs:

```bash
cargo install --path . --debug --locked --features skills --root "$gym_root"
```

`cargo install --root DIR` installs into `DIR/bin`, so the gym binary is `<gym root>/bin/mini-agent`
and the operator's production `~/.cargo/bin/mini-agent` is left alone. Pass that absolute path to
`--binary`; a `PATH` lookup can resolve a different build. The `skills` feature is not a default
feature and the learned-skill operator commands the library arm calls do not exist without it, so
setup asserts the installed binary advertises `--install-learned-skill-seeds`. The platform
worker-containment test then runs under the **same** feature set as the install
(`cargo test --locked --features skills`), so the preflight exercises the build the episodes use.

Setup does not import or activate a library. Each `library` episode installs and activates its own
library into its own throwaway AppPaths tree, so no shared seeded state can leak between arms.

There is no runtime `gym.toml` configuration parser. Script options and the task JSON are the
configuration surface.

## Task schema

Both `scripts/gym/train.py` and the deterministic Rust harness fixture
(`tests/harness_eval/task.json`) use one `{defaults, tasks}` object in which a task entry shallowly
overrides any `defaults` field:

```json
{
  "schema_version": 1,
  "defaults": { "prompt": "...", "initial_files": {},
                "oracle": {"expected_files": {}, "id": "..."},
                "budgets": {"max_provider_turns": 12, "max_tool_calls": 24, "max_total_tokens": 16000},
                "scripted_provider_turns": {"none": [], "library": []},
                "expected_export": "parseJson",
                "library": "seeds" },
  "tasks": [ {"name": "...", "tags": ["..."], "base_commit": "...", "deleted_files": [],
              "oracle": {"command": "...", "id": "..."}, "...": "any default field"} ]
}
```

`scripts/gym/mine_tasks.py` writes exactly that `defaults` field set. The Rust harness deserializes
its fixture with `deny_unknown_fields`, so the checked-in fixture carries only the fields that
struct declares (no `schema_version`, no `oracle.command`, no `base_commit`); the Python runner
reads that file unchanged and additionally accepts the gym-only superset — `schema_version`,
`base_commit`, `deleted_files`, `timeout_secs`, `oracle.command`, `library`, and `fix_commit`.
`schema_version` is optional, defaults to `1`, and must be `1`. A bare JSON array is the
pre-convergence format and is rejected with a pointer at `mine_tasks.py`.

What the Python runner enforces at load time, before any episode runs:

| Field | Enforcement |
| --- | --- |
| `name` | required, non-empty, unique across the file |
| `prompt` | required, non-empty |
| `base_commit` | defaults to `HEAD`; a commit Git cannot materialize is a failed row, not an empty directory |
| `initial_files`, `deleted_files` | relative paths only; `..`, absolute paths, and root-only paths such as `.` are rejected |
| `oracle` | needs `command` or a non-empty `expected_files`; `id` defaults to a hash of the oracle |
| `budgets.max_provider_turns` | **required**, integer >= 1; passed to the binary as `--max-agent-turns` |
| `budgets.max_tool_calls`, `budgets.max_total_tokens` | optional, integer >= 1, **not enforced** (see below) |
| `timeout_secs` | optional per-task override of `--task-timeout` |

`scripted_provider_turns` and `expected_export` are consumed by the Rust harness only; the Python
runner ignores them. `tags`, `fix_commit`, and a mined `oracle.expected_files` sitting alongside a
`command` are provenance.

### Budgets that are recorded but not enforced

The binary caps agent turns (`--max-agent-turns`) and per-response tokens, but has no tool-call cap
and no per-task total-token or dollar cap. `max_tool_calls` and `max_total_tokens` are therefore
validated, echoed in each row's `budgets_unenforced` list, and enforced by nothing. Cap cost outside
the gym (for example with a provider account limit) before a live-model run.

## Mine tasks from closed beads

```bash
python3 scripts/gym/mine_tasks.py --oracle-map map.json --output tasks.json [--main-ref main] \
  [--beads-json export.jsonl] [--limit 20] [--no-validate]
```

Each `map.json` value is either an oracle command string or an object with `command` plus optional
`fix_commit`, `base_commit`, `id`, and `library`. Without an explicit `fix_commit` the miner takes
the **oldest** commit reachable from `--main-ref` whose message mentions the bead id, so a
follow-up mention or an abandoned branch cannot be selected. Every bead that cannot be turned into
a task is reported on stderr with the reason (no matching commit, unresolvable base, oracle already
green at base, oracle not green at the fix, checkout unavailable, or no bounded text delta).
Checkout failures provide no oracle evidence: the candidate is skipped if either revision cannot
be checked out. Cleanup also removes a partially created worktree when a post-checkout hook fails,
including its Git registration. Command timeouts remain reported oracle failures, allowing tasks
that fix hangs or excessive runtime; the fix revision must complete successfully within the limit.
Validated tasks receive the `fail-to-pass` tag. With `--no-validate`, neither oracle runs: tasks
receive `validation-skipped` and the CLI reports them as unvalidated. These two provenance tags
are derived from the current mining run rather than inherited from bead labels.

Diffs are captured as bytes and decoded as strict UTF-8, so CRLF files survive verbatim and binary
blobs are skipped rather than raising. Both the parent and child blob are bounded at 256000 bytes:
Git reports their size before content is captured, and the subsequent read independently enforces
the same limit and a 30-second deadline. A failed or oversized required blob skips the whole file;
truncated prefixes are never used as oracle text. Exact-limit and empty blobs are accepted.
Renames are recorded as a delete plus an add (`--no-renames`), deletions become `deleted_files`, and
any path with a dot-prefixed component (`.github/workflows/...` included) is skipped. Validation
runs each oracle with `/bin/sh -c` under the same curated environment and gym-owned AppPaths as
training, never a login shell and never the operator's environment.

## Run paired episodes

```bash
scripts/gym/train.sh \
  --tasks tasks.json \
  --output outcomes.jsonl \
  --binary "$PWD/.gym/bin/mini-agent" \
  --agent-arg=--yolo
```

`train.sh` only fixes `--repo` to the repository root; every other option belongs to `train.py`:
`--gym-root` (default `$MINI_AGENT_GYM_ROOT` or `<repo>/.gym`), `--agent-arg` (repeatable),
`--forward-env` (repeatable), `--provider`, `--model`, `--task-timeout`, `--allow-empty-workspace`,
and `--keep-run-dirs`.

For each task and each arm the runner:

1. creates a detached worktree at `base_commit` under `<gym root>/worktrees/`, removes
   `deleted_files`, and overlays `initial_files`. A worktree that cannot be created is a failed row
   with `failure_reason=workspace_unavailable`; `--allow-empty-workspace` restores the old silent
   empty-directory behaviour;
2. builds a fresh AppPaths tree under `<gym root>/runs/` and a curated environment (below);
3. for the `library` arm, installs the library from a neutral directory, approves and activates only
   the **lineage-root** proposals (`predecessor_id IS NULL`), and fails the episode unless at least
   one revision ends up `active`. The runner reads `skills.db` directly with read-only SQLite to
   pick the roots and read back the active set; the active ids land in `active_skill_ids`. (The
   binary now has `--learned-skill-json`, but the runner does not use it.)
4. runs the oracle **before** the agent and records `oracle_pre_exit`. An oracle that already passes
   makes the task invalid: the row fails with `task_invalid_oracle_passes_before_agent` and the
   agent is never launched;
5. runs `mini-agent --max-agent-turns <budget> [agent args] -p <prompt>` bounded by
   `--task-timeout` (default 900s);
6. runs the oracle again: `oracle.command` through `/bin/sh -c` with a 300-second bound, or, when no
   command is given, an exact UTF-8 comparison of `oracle.expected_files` against the workspace.
   File comparisons preserve line endings, accept only regular files (including symlink targets),
   and read at most the expected character count plus one. POSIX FIFO opens are nonblocking;
7. removes the worktree, runs `git worktree prune`, and deletes the run tree in a `finally`, so
   these cleanup steps also run after a timeout or install failure.

Workspace overlays traverse directories through retained descriptors and reject symlinked
ancestors. Initial files replace the destination entry without writing through a symlink or
hardlink; regular-file executable permissions and exact line endings are preserved. Deletions
unlink final symlinks (including dangling ones) and remove directories without following links
inside them. An overlay error produces `workspace_unavailable` and prevents the agent from
running for that episode. Failed file publication removes its temporary file.

Agent, library-install, and command-oracle output in both training and task mining is drained
concurrently by [process_capture.py](../../scripts/gym/process_capture.py), retaining only the last
2,000 bytes of each stream. Git blobs use its capped complete-stdout mode; other Git data queries
retain their complete output. Timeout diagnostics use
those same tails. The exit deadline still applies if a process closes its output pipes early.
Timeout cleanup currently terminates and reaps the immediate child; descendants in other process
groups can survive. Complete descendant cleanup is tracked in `mini-agent-m7bs`.

Each row is appended and flushed as it is produced, so an interrupted run keeps everything already
finished.

### Isolation

Episodes never read the operator's configuration. `ZS_CONFIG_DIR` and `ZS_CREDENTIALS_DIR` point at
gym-owned directories seeded with a minimal `config.toml` holding only `--provider`/`--model` when
given, alongside gym-owned `ZS_DATA_DIR`, `ZS_LOCAL_DATA_DIR`, `ZS_STATE_DIR`, `ZS_CACHE_DIR`,
`TMPDIR`, and `MINI_AGENT_GYM=1`. The environment is an allowlist, not a copy: `PATH`, `HOME`,
`USER`, `LOGNAME`, `LANG`, `LC_ALL`, `LC_CTYPE`, `TERM`, `TZ`, `SSL_CERT_DIR`, `SSL_CERT_FILE`, the
known provider key variables (`ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `OLLAMA_API_KEY`,
`OPENAI_API_KEY`, `OPENROUTER_API_KEY`, `VLLM_API_KEY`), and anything added with `--forward-env`.
Library installs run from a neutral gym-owned directory so a project-local `config.toml` in a
checked-out workspace cannot influence the import. Treat the gym root as sensitive test state
anyway: an explicitly configured external command or an unsandboxed model still runs with the
authority of the account.

### Permission mode

`-p` runs headless. In the default `standard` mode the `shell`, `js`, `fetch`, and `memory_write`
tools resolve to *ask*, and a non-interactive run turns *ask* into "Permission denied
(non-interactive mode)" — so an agent cannot run `cargo test` or `grep` unless you say otherwise.
Pass `--agent-arg=--yolo` (or any other flag the binary accepts) to change that. Every row records
the `agent_args` used and a `permission_mode` of `yolo` or `standard`.

### Output rows and exit codes

Each JSONL row and each `GYM_OUTCOME` line carries: `task`, `arm`, `success`, `production` (always
false), `schema_version`, `oracle_id`, `elapsed_ms`, `oracle_ms`, `total_ms`, `timeout_secs`,
`agent_exit` (124 on timeout), `oracle_exit`, `oracle_pre_exit`, `failure_reason`, `failure_detail`,
`agent_stderr_tail`, `permission_mode`, `agent_args`, `provider`, `model`, `active_skill_ids`,
`budgets_enforced`, and `budgets_unenforced`. `failure_reason` is one of `workspace_unavailable`,
`library_install_failed`, `task_invalid_oracle_passes_before_agent`, `agent_timeout`,
`agent_exit_nonzero`, or `oracle_failed`.

The three clocks are separate on purpose:

- `elapsed_ms` times **the agent alone** — from launching `mini-agent` to its exit, or to the
  timeout. That is the number to compare across arms; folding in the oracle, and in the library arm
  the seed import, would make the library arm look slower for work the agent never did.
- `oracle_ms` is the summed wall clock of the pre- and post-agent oracle runs.
- `total_ms` is the whole episode including workspace setup and, for the library arm, the library
  install — so that overhead stays visible instead of hiding inside the agent's number.

All three are present on every row, including failed ones: `elapsed_ms` stays `0` when the episode
failed before the agent launched, and `oracle_ms`/`total_ms` are written in a `finally` so they are
recorded whatever went wrong.

The run ends with a `GYM_SUMMARY` line and one `gym arm <arm>: N passed, M failed of T` line per
arm. **Exit 0 means the run completed**, whatever the rows say — the `none` arm is expected to fail
the tasks the library helps with. Non-zero is reserved for runner errors: exit 2 covers an
unreadable or invalid task file, an invalid budget, and output-path failures.

Compare success counts and elapsed time only across matching task/oracle ids and always report the
sample size. These records do not include provider-token or dollar-cost fields. The deterministic
task-json test is the supported no-provider regression path.

The optional successful-step distiller, resumable live-model experiment manager, built-in dollar
cost cutoff, aggregate Wilson report, and evidence-derived retrieval labels are deferred. No gym
output should be presented as production utility evidence or used to promote a skill automatically.
