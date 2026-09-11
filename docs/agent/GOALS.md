---
description: "Goals: giving the agent an objective it keeps working toward, and deciding when it may stop."
---

# Goals

A goal is an objective the agent works toward across turns, together with the rules for deciding
when that work may stop. Set one with `/goal` in the TUI or `--goal` headless.

```
/goal make the migration idempotent
done when:
- running it twice is a no-op
- cargo test passes
```

```bash
mini-agent -p --goal "make the migration idempotent" \
  --goal-done "running it twice is a no-op" \
  --goal-check "cargo test"
```

## How it runs

A goal advances in **rounds**. One round is one agent run followed by one gate evaluation that
decides whether to keep going, and prints why:

```
goal: not yet (round 2) — Verification failed, so the goal is not met yet:
cargo test failed: test migrate::twice ... FAILED
```

The objective lives outside the conversation, so compaction cannot lose it, and it is restated into
every compaction summary. Each round starts with a fresh turn and token budget, so one runaway round
is still bounded the way any single turn is, while the goal's own bounds cover the objective.

You are not locked out while a goal runs. Typing a message is a normal turn; the gate simply
evaluates at its end.

## When the agent may stop

Claiming completion starts verification rather than ending the goal.

1. **Open todo items** veto a completion claim.
2. **Commands** decide next. `--goal-check` and `/goal check` commands must exit zero, and the
   configured `verify_command` also runs when the round did not trigger it. A completion claim from
   a round that touched nothing still runs it, because the edits it takes credit for may have landed
   earlier.
3. **A judge model** reviews last. It can withhold completion but can never overturn a command that
   exited zero.

With no checks configured the only evidence is the model's own account, and `/goal status` says so:

```
verification: self-report only (add /goal check <command> to verify)
```

A completion that no command proved is labelled:

```
note: completion was not proved by any command
```

## The judge

The judge is whatever model you name. Another family, the same one, smaller, larger, or identical to
the agent's own, on any provider. There is no assumed relationship and no built-in list.

```toml
goal_judge_model = "cheap_judge"

[quick_models.cheap_judge]
provider = "anthropic"
model = "claude-haiku-4-5-20251001"
```

Resolution order is `goal_judge_model`, then a `quick_models` entry named `goal_judge`, then your
session's own model in a fresh context. That last fallback means a single-model setup still gets a
second opinion from something that did not just do the work — weaker than a distinct model, and
`/goal status` labels it as such. `[goal] judge = "off"` turns the tier off.

The judge gets no tools and no workspace: only the objective and a bounded tail of the conversation,
fenced as untrusted data. If it cannot be reached, the goal keeps working rather than completing or
ending; three consecutive failures on completion claims park the goal so an outage cannot pass for
verification.

## Statuses

| Status | Meaning |
| ------ | ------- |
| `active` | Being worked on. |
| `awaiting user` | The agent asked you something. Answer it and the goal resumes. |
| `blocked` | The same blocker survived several rounds with no progress. |
| `paused` | Parked by the harness. `/goal status` says why. |
| `budget limited` | Rounds, tokens, or time ran out. Raise a bound and resume. |
| `met` | Reached, and verified as deeply as you configured. |
| `impossible` | Cannot be satisfied as written. `/goal reopen` to try again. |

Only `met` and `impossible` are final. Everything else resumes. Setting a new goal over an
unfinished one is refused rather than done silently: its rounds, verdicts, and reports are the
record of the work so far.

## Bounds

Bounds are enforced by the harness, not by asking the model to behave.

```toml
[goal]
max_rounds = 50
no_progress_rounds = 3
blocked_rounds = 3
```

Raise one mid-flight with `/goal bounds max_rounds=100`, which also re-arms a goal that stopped on
it. When a bound is exhausted the agent gets one short wrap-up round to land what it has, rather
than being cut off mid-edit or allowed to start something new.

Time spent waiting on a permission prompt does not count: the budget measures how long the agent
worked, not how long you took to answer.

A goal never changes your security mode, permission allowlist, or sandbox policy. It decides when
the agent may stop, never what it may do.

## Headless use

In `-p` mode the run continues until the goal stops, and the exit code says why: `0` met, `20`
impossible, `21` blocked, `22` awaiting you, `23` paused, `24` budget limited. `--output json` adds
a `goal` object with the status, rounds, tokens, and the evidence behind the verdict, so a CI job can
branch without parsing prose.

`--no-tools` and `--goal` are incompatible: the agent could never report progress, so every round
would look like a stall.

## Records

Each gate evaluation writes one JSON record under `state/goals/<goal-id>/round-NNNN.json` with what
was checked and what was concluded. Exports carry the objective too.

## Limitations

- A judge reading a transcript is not proof. Configure a check for anything that matters.
- Restart mode carries a harness-built summary, not the full conversation. Prefer the default
  `continue` unless you want each round to start clean.
- One goal per session.
