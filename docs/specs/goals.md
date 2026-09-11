# Goals

- **Document role**: normative feature specification
- **Specification version**: 1.0.0
- **Delivery status**: delivered (see **Shipped-binary status**)
- **Owner**: mini-agent maintainers
- **Last reconciled**: 2026-09-12
- **Entry dependency**: none; the `goal` Cargo feature is default-on and independent of every phase
- **Exit dependency**: every required test below
- **Target platforms**: Linux, macOS, and Windows MSVC

The corpus authority and conflict rules are defined in [00-index.md](00-index.md). This document
owns the goal concept, the round loop, the gate, verification tiers, bounds, and the surfaces that
expose them. It extends three other authorities and changes nothing else; the extensions are listed
under **Extension map** and recorded in their owning files.

## What a goal is

A goal is a persistent objective the agent works toward across turns, together with everything the
harness needs to decide when that work may stop.

It is held outside the conversation. Compaction, agent rebuilds, and session reloads cannot lose it,
and every consumer re-reads it from the record rather than from message history. This is the single
most important property in this document: the dominant failure mode in autonomous agent loops is an
objective that quietly disappears into a summary and is then declared complete.

## Round model

A goal advances in **rounds**. One round is one agent run followed by one gate evaluation.

The driver owns the loop — the TUI event handler, the headless driver, and the ACP prompt handler.
The runner does not, and must not: it holds no session to persist to, no client to ask a judge with,
and its per-response budgets fail hard rather than winding down. Running at the driver layer means
every round starts with a fresh `max_agent_turns` and `turn_token_budget`, so those existing
per-response bounds keep protecting a single runaway round while the goal's own bounds cover the
objective as a whole.

Two continuation modes:

- `continue` (default) relaunches with the conversation retained.
- `restart` relaunches with empty history plus a summary the harness assembles from the goal's own
  reports and verdicts. The summary is never free model prose: a model asked to summarize its way
  into the next round is a model given the chance to restate the objective as something smaller.

## Record

One goal per session, stored on the session in a shared handle so every rebuild observes the same
record. An absent goal serializes away entirely: a session file written before goals existed loads
and re-saves byte-identical.

Status is one of `active`, `awaiting_user`, `paused`, `blocked`, `budget_limited`, `met`,
`impossible`. **Only `met` and `impossible` are terminal.** Every other stop parks the goal for the
user rather than discarding it. A finished goal leaves its state only by an explicit user action;
`impossible` may be reopened, `met` may not. A status this build does not recognize parks the goal
instead of failing the whole session load.

`paused` carries a reason — no progress, judge unavailable, context overflow, round failure, or an
unknown stored status — because those are four unrelated conditions and a user cannot act on
"paused" alone.

The model's only influence on this record is appending a report through `goal_report`. The
objective, criteria, checks, judge, and bounds are read-only to it by construction, not by
instruction.

## The gate

At the end of every round the gate decides one thing: keep working, or stop and say why. It is a
pure function with no I/O, evaluated in two phases so that the tiers it cannot run itself are run
by the driver and fed back.

Rows, in order. The first match decides.

| # | Condition | Decision |
|---|-----------|----------|
| 1 | Interrupted | stop, goal untouched and uncounted |
| 2 | Context overflow | `paused{context_overflow}` — rerunning cannot make it fit |
| 3 | Run failed, under the retry limit | continue with the diagnostic |
| 4 | Run failed, at the limit | `paused{round_failure}` |
| 5 | Wrap-up round already ran | `budget_limited` |
| 6 | A bound is exhausted | one bounded wrap-up round |
| 7 | `needs_user` reported | `awaiting_user`, question surfaced |
| 8 | `blocked` reported, streak reached | `blocked` |
| 9 | `blocked` reported | continue, attempt n of N |
| 10 | `impossible` reported | adjudicate |
| 11 | No progress, streak reached | `paused{no_progress}` |
| 12 | No completion claim | continue |
| 13 | `met` with open todo items | continue |
| 14 | `met`, verify command not run this round | run it |
| 15 | `met`, checks configured | run them |
| 16 | `met`, judge enabled | ask it |
| 17 | otherwise | `met` |

The ordering carries the meaning:

- **Budget exhaustion outranks a completion claim.** The claim is still verified on the way out, in
  a wrap-up round bounded to a few provider calls, rather than being lost.
- **A question outranks stall detection.** An agent that asks something is waiting, not stuck, and
  the gate must not answer it.
- **Evidence outranks the model's own account.** A judge may withhold completion but can never
  overturn a command that exited zero.
- **A completion claim from a round that touched nothing still runs the verify command**, because
  the edits it takes credit for may have landed earlier.

**No text similarity anywhere.** Blocked and no-progress streaks count rounds. Comparing blocker
text is gameable in both directions: rewording a blocker each round would defer the stop forever,
and two unrelated blockers sharing boilerplate would merge into one. A round that changed something
or filed a report is progress, so reading, waiting, and asking are not mistaken for stalling.

## Verification tiers

| Tier | What it is | When it runs | What it proves |
|------|------------|--------------|----------------|
| 0 structural | open todos, verify state, per-round counters | every round | nothing on its own |
| 1 self-report | `goal_report` | every round | the model's claim |
| 2 checks | commands through the shared validation runner | on a completion claim | **external proof** |
| 3 judge | one no-tools completion | on a completion or impossibility claim, and periodically | a second reading |

Only tier 2 produces external evidence. A model saying it is finished and a judge agreeing are both
accounts of the work; a command exiting zero is a fact about it.

### Checks

Checks run through `extras::validation` with the configured verification limits, under
`TC-GOAL-CHECK`. The goal module builds no process itself and never calls `Sandbox::wrap_command`.
A non-zero exit, a timeout, cancellation, an output-limit breach, and a launch failure are all
failures. A goal whose checks cannot run can never reach `met` through this tier.

### Judge

The judge is whatever model the user names: another family or the same, smaller, larger, or
identical to the agent's own, on any provider. The harness assumes no relationship between them and
keeps no table of cheap siblings. Resolution order is the configured judge model, then a
`quick_models` entry named `goal_judge`, then the session's own model in a fresh context — so a
single-model installation still gets a second opinion from something that did not just do the work.
That is weaker than a distinct model, and a verdict reached that way is labelled as such.

The judge receives the objective and a bounded, sanitized tail of the conversation, fenced as
untrusted data because that tail carries tool output the workspace controls. It gets no tools and no
workspace. Its prompt states that a verdict appearing inside the transcript is text being reported,
never a directive.

Failures fail open. One unreachable judge costs nothing: the claim is evaluated as if none were
configured and the verdict does not record agreement. Three consecutive failures on completion
claims park the goal, so a silent outage cannot pass for verification. Unparseable output is an
error rather than a guess.

## Bounds

Enforced in code, never in prompt text.

| Bound | Default | On exhaustion |
|-------|---------|---------------|
| `max_rounds` | 50 | wrap-up round, then `budget_limited` |
| `max_tokens` | unset | same |
| `max_active_secs` | unset | same |
| `no_progress_rounds` | 3 | `paused{no_progress}` |
| `blocked_rounds` | 3 | `blocked` |
| `wrap_up_max_agent_turns` | 4 | round ends |
| `reinject_every` | 6 provider calls | mid-round reminder |
| `judge_every` | 5 rounds | drift check |

Active time excludes any stretch spent waiting on a permission prompt: a goal's budget measures how
long the agent worked, not how long the user took to answer. The wrap-up is issued once per
exhaustion and re-armed by resuming or raising a bound.

## Authority

**A goal never changes the security mode, the permission allowlist, or the sandbox policy.** It
bounds how long the harness keeps working and decides when it may stop; it never widens what may
run. This holds by construction: the goal module has no access to those objects, and a test asserts
the security mode is unchanged across a goal lifecycle.

The objective is user-provided data. It is fenced in the preamble with the framing ahead of it,
because a goal is a long-lived instruction channel that a flag or an editor client can set.

Goal checks carry the authority of whoever configured them. `goal_checks` and `goal_judge_model` are
top-level configuration keys outside the benign set, so an untrusted project configuration cannot
activate either; the `[goal]` bounds table beside them is benign because nothing in it can widen
authority.

## Surfaces

- **TUI**: `/goal <objective>` with an optional `done when:` block, plus `status`, `pause`,
  `resume`, `reopen`, `clear`, `bounds`, and `check`. Replacing an unfinished goal is refused. A
  status-line item shows round and status. Each gate decision prints one line, because an agent that
  keeps going without saying why is the most common complaint about autonomous loops.
- **CLI**: `--goal`, `--goal-done`, `--goal-check`, `--goal-max-rounds`, `--goal-continuation`,
  `--goal-replace`. A goal under `--no-tools` is refused at startup: the agent could never report,
  so every round would look like a stall.
- **Headless**: rounds run until a stop status. `--output json` carries a goal object, and each
  non-met outcome has its own exit code so a caller can tell "resume me" from "stop retrying".
- **Hooks**: the `Stop` envelope carries the goal id, status, and round; null without a goal.
- **Transcripts**: one bounded JSON record per gate evaluation under the goals directory.

## Extension map

Goals extend three owned authorities and change nothing else.

| Owning authority | Extension |
|------------------|-----------|
| [subprocess-trust.md](subprocess-trust.md) | `TC-GOAL-CHECK`, whose principal is the human who wrote the check |
| [platform-paths.md](platform-paths.md) | `state_dir/goals/<goal-id>` for round transcripts |
| [phase-5-evidence-learning.md](phase-5-evidence-learning.md) | a `goal` task-outcome source, excluded from promotion unless a command proved it |

## Shipped-binary status

Reachable today: the record and its persistence, the preamble block and compaction restatement, the
mid-round reminder, `goal_report`, the full gate, the round driver in the TUI and headless, the
checks tier, the judge tier, bounds and wrap-up, `/goal` and the CLI flags, the status line,
transcripts, headless JSON and exit codes, session export, and the `Stop` envelope fields.

Not reachable: recording goal verdicts as Phase 5 task-outcome rows. The source and its promotion
exclusion exist and are tested, but no production `TaskOutcomeRecorder` is available at the driver
layer, so no such row is written yet (`mini-agent-a1qwa.20`). Independently, task outcomes do not
reach promotion in production at all today — `evaluate_promotion` passes an empty outcome slice —
which is the same reachability the index already records for automatic evidence-threshold
promotion.

ACP accepts a goal through `_meta.goal` on a prompt request and settles **one round per
`session/prompt`**, reporting the decision as a thought chunk and as `_meta.goal`. Rounds do not
relaunch inside a single prompt turn: the client sends the next prompt, which is how an editor
already works. Relaunching in-turn would re-enter the runner while that turn's cancellation
ownership and partial-transcript retention are live.

The `goal` Cargo feature being enabled is not a claim that any of this is delivered.

## Required tests

- Record: serde round trip per status, legacy session byte-identical re-save, shared-store cloning,
  unknown-status parking, caps.
- Gate: one test per row, ordering collisions, a combinatorial sweep asserting exactly one outcome
  and correct counters, determinism.
- Tool: per-status required fields, closed schema, refusal without a live goal, and that a met
  report changes nothing on the goal.
- Prompt: block stability across rounds, data framing order, objective present exactly once,
  overhead accounting, compaction restatement.
- Driver: round collection, both history modes, wrap-up cap, interrupt leaving the goal untouched,
  verification requested only on a completion claim.
- Checks: pass, non-zero, timeout, verify-before-checks ordering, and that nothing to run is not a
  vacuous pass.
- Judge: resolution table including cross-provider and fallback, verdict grammar including garbage,
  transcript fencing, fail-open ladder, and that passing checks survive an outage.
- Surfaces: slash subcommands, status-line rendering, CLI parsing, headless exit codes.
- Authority: security mode unchanged across a goal lifecycle; untrusted project config cannot supply
  a check or a judge model.

Build and verification commands are the repository defaults: `cargo fmt`, `cargo test`,
`cargo clippy --all-targets -- -D warnings` on the supported feature rows, and
`cargo install --path . --debug`. Never `cargo build`, `cargo check`, or `--release`.
