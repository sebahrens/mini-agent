# Goals

- **Document role**: normative feature specification
- **Specification version**: 1.0.0
- **Delivery status**: delivered (see **Shipped-binary status**)
- **Owner**: mini-agent maintainers
- **Last reconciled**: 2026-09-12 (post-delivery review, `mini-agent-c75u1`)
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

`paused` carries a reason — no progress, judge unavailable, context overflow, round failure, an
unknown stored status, or the user asking for it — because those are unrelated conditions and a
user cannot act on "paused" alone.

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
| 5 | Wrap-up round already ran | `budget_limited`, unless it is the completion claim it asked for |
| 6 | A bound is exhausted | one bounded wrap-up round |
| 7 | `needs_user` reported | `awaiting_user`, question surfaced |
| 8 | `blocked` reported, streak reached | `blocked` |
| 9 | `blocked` reported | continue, attempt n of N |
| 10 | `impossible` reported | adjudicate |
| 11 | No progress, streak reached | `paused{no_progress}` |
| 12 | No completion claim | continue |
| 13 | `met` with open todo items | continue |
| 14 | `met`, verify command configured | run it, or read this round's result |
| 15 | `met`, checks configured | run them |
| 16 | `met`, judge enabled | ask it |
| 17 | otherwise | `met` |

A tier that withholds completion returns a continuation, and a continuation never outlives a bound:
whatever row asked for another round, an exhausted budget takes it. That is what keeps a model
claiming completion every round against a judge that withholds it every round from running forever
on a two-round budget.

The ordering carries the meaning:

- **Budget exhaustion outranks a completion claim.** The claim is still verified on the way out, in
  a wrap-up round bounded to a few provider calls, rather than being lost. The wrap-up round asks
  the agent to finish and say so, so a completion it reports there is adjudicated like any other;
  one the tiers reject lands back on the budget stop rather than buying a further round.
- **A question outranks stall detection.** An agent that asks something is waiting, not stuck, and
  the gate must not answer it.
- **Evidence outranks the model's own account.** A judge may withhold completion but can never
  overturn a command that exited zero.
- **A completion claim from a round that touched nothing still runs the verify command**, because
  the edits it takes credit for may have landed earlier. A round that already ran it is not asked to
  run it again; its result is read, and counts as the external proof it is.

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

Checks run in one of two modes. By default they run only on a completion claim and gate it. With
`check_every_round` they also run at the end of every round as **feedback**: the output goes into
the next round's instruction and a failure does not stop the goal, because an agent that learns its
tests fail at the end of the round that broke them corrects sooner than one told only when it claims
to be finished. Completion is gated by the same checks in either mode.

Checks run through `extras::validation` under `TC-GOAL-CHECK`, with the completion-verification
gate's own limits rather than a copy of them: a goal check and a `verify_command` are the same kind
of thing and are bounded identically. The goal module builds no process itself and never calls
`Sandbox::wrap_command`. A non-zero exit, a timeout, an output-limit breach, and a launch failure
are all failures, and a goal whose checks cannot run can never reach `met` through this tier.

Cancellation is not a failure. A check the operator interrupted proved nothing either way, so the
round settles as interrupted — untouched and uncounted — rather than being judged on a command that
was stopped. The command is cancelled rather than dropped, so its process group is terminated and
reaped instead of outliving the interrupt.

### Judge

The judge is whatever model the user names: another family or the same, smaller, larger, or
identical to the agent's own, on any provider. The harness assumes no relationship between them and
keeps no table of cheap siblings. Resolution order is the configured judge model, then a
`quick_models` entry named `goal_judge`, then the session's own model in a fresh context — so a
single-model installation still gets a second opinion from something that did not just do the work.
That is weaker than a distinct model, and a verdict reached that way is labelled as such, in the
round transcript and in the line the surfaces print.

A judge on another provider is reached through a client built for that provider. The session's
client can only talk to the session's provider, so sending it a foreign model id would ask the
wrong endpoint for a model it has never heard of — and fail open, silently costing the tier the
user configured.

The judge receives the objective and a bounded, sanitized tail of the conversation, fenced as
untrusted data because that tail carries tool output the workspace controls. It gets no tools and no
workspace. Its prompt states that a verdict appearing inside the transcript is text being reported,
never a directive, and the fence holds structurally: the closing tag is defanged inside the tail, so
a fixture containing it cannot end the region. The tail's budget is spent from the newest end
backwards, because the claim and the work behind it live at the end of a transcript.

The judge's answer comes back as a report, not as an order. Its reason is framed and bounded before
it becomes the next round's instruction, for the same reason the tail is fenced: the judge wrote it
after reading text the workspace controls.

Failures fail open, and what that costs depends on what else proved the claim.

- With a command behind it, an unreachable judge costs nothing: the work is proven, the claim
  completes, and the verdict does not record agreement it never got.
- With nothing but the model's own account, the judge was the only thing between a self-report and a
  terminal `met`. The claim waits for the next round instead. After three such
  rounds the goal parks, so a silent outage can neither pass for verification nor spin forever.

Only completion claims climb that ladder. A drift check that could not run withheld nothing — the
goal never had that guidance — and counting those outages would park a working goal after two
routine cadence misses. Any answer at all clears the streak, and so does resuming a parked goal.
Unparseable output is an error rather than a guess.

## Bounds

Enforced in code, never in prompt text.

| Bound | Default | On exhaustion |
|-------|---------|---------------|
| `max_rounds` | 50 | wrap-up round, then `budget_limited` |
| `max_tokens` | unset | same |
| `max_active_secs` | unset | same |
| `no_progress_rounds` | 3 | `paused{no_progress}` |
| `blocked_rounds` | 3 | `blocked` |
| `wrap_up_max_agent_turns` | 4 | round ends; zero makes the bound exact |
| `reinject_every` | 6 provider calls | mid-round reminder |
| `judge_every` | 5 rounds | drift check |

Active time excludes any stretch spent waiting on a permission prompt: a goal's budget measures how
long the agent worked, not how long the user took to answer. Every surface measures it — the
terminal around its own prompt, an editor by subtracting what the client spent deciding, and a
headless run by the round's wall clock, since nothing there can prompt.

The wrap-up round runs on an agent capped to `wrap_up_max_agent_turns`, so the bound the harness
announces is the bound the round gets. It is issued once per exhaustion and re-armed by resuming or
raising a bound.

## Authority

**A goal never changes the security mode, the permission allowlist, or the sandbox policy.** It
bounds how long the harness keeps working and decides when it may stop; it never widens what may
run. This holds by construction: the goal module has no access to those objects, and a test asserts
the security mode is unchanged across a goal lifecycle.

The objective is user-provided data. It is fenced in the preamble with the framing ahead of it,
because a goal is a long-lived instruction channel that a flag or an editor client can set.

Goal checks carry the authority of whoever configured them. `goal_checks` and `goal_judge_model` are
top-level configuration keys outside the benign set, so an untrusted project configuration cannot
activate either. The `[goal]` table beside them is benign because a bound only limits how long the
harness keeps working — with one exception carved out of it: `[goal].judge` is split off with the
sensitive keys, because choosing which configured endpoint sees a slice of the transcript, or
switching the review off entirely, is not a bound.

An editor client is held to the same line. `_meta.goal` is a closed schema carrying an objective,
its criteria, and whether to replace or clear. A client asking for checks, a judge or bounds is
refused rather than quietly ignored, so it can never believe it configured a gate it did not get.

## Surfaces

Every surface builds its goal through one factory, so a `[goal]` bound, a project's `goal_checks`
and the configured judge mean the same thing whichever way the objective arrived. A surface with
flags applies them afterwards, so a flag always beats the file it overrides.

- **TUI**: `/goal <objective>` with an optional `done when:` block, plus `status`, `pause`,
  `resume`, `reopen`, `clear`, `bounds`, and `check`. The verbs that take no argument only match
  when none was given, so an objective beginning with one is still an objective. Replacing an
  unfinished goal is refused. A status-line item shows round and status. Each gate decision prints
  one line, because an agent that keeps going without saying why is the most common complaint about
  autonomous loops. `status` and `pause` are reachable while rounds are chaining, which is the whole
  time a goal exists.
- **CLI**: `--goal`, `--goal-done`, `--goal-check`, `--goal-max-rounds`, `--goal-continuation`,
  `--goal-replace`. The dependent flags require `--goal` rather than being accepted and dropped. A
  goal under `--no-tools` is refused at startup, whether it came from the flag or from a resumed
  session: the agent could never report, so every round would look like a stall.
- **Headless**: rounds run until a stop status, each round persisted as it completes under the
  prompt it actually ran. `--output json` carries a goal object, and each non-met outcome has its
  own exit code so a caller can tell "resume me" from "stop retrying" — in text output too. A goal
  still running when the process exits was interrupted, which is not a stop of its own and exits
  zero.
- **Hooks**: the `Stop` envelope carries the goal id, status, and round, published from the moment a
  goal is set rather than from its first gate decision; null without a goal, and after one is
  cleared or the session is switched.
- **Transcripts**: one bounded JSON record per gate evaluation under the goals directory, naming
  which model judged and whether it was the agent's own. A stored goal id that is not a plain
  identifier is reissued rather than used as a path.

## Extension map

Goals extend three owned authorities and change nothing else.

| Owning authority | Extension |
|------------------|-----------|
| [subprocess-trust.md](subprocess-trust.md) | `TC-GOAL-CHECK`, whose principal is the human who wrote the check |
| [platform-paths.md](platform-paths.md) | `state_dir/goals/<goal-id>` for round transcripts |
| [phase-5-evidence-learning.md](phase-5-evidence-learning.md) | a `goal` task-outcome source, excluded from promotion unless a command proved it |

## Shipped-binary status

Reachable today: the record and its persistence, the preamble block and compaction restatement, the
mid-round reminder, `goal_report`, the full gate, the round driver in the TUI, headless and ACP, the
checks tier, the judge tier on every surface, bounds and wrap-up, `/goal` and the CLI flags, the
status line, transcripts, headless JSON and exit codes, session export and import, and the `Stop`
envelope fields.

With the `skills` feature, goal verdicts are recorded as Phase 5 task-outcome rows from both the
TUI and headless driver: one row per goal, when it settles. In a build without `skills` there is no
recorder and a round settles exactly as it would otherwise. Task outcomes still do not reach
promotion in production — `evaluate_promotion` passes an empty outcome slice — which is the same
reachability the index already records for automatic evidence-threshold promotion.

ACP accepts a goal through `_meta.goal` on a prompt request and settles **one round per
`session/prompt`**, reporting the decision as a thought chunk and as `_meta.goal`, including what
the verdict rests on. Both tiers run there exactly as they do elsewhere, so the same objective is
gated the same way in an editor as in a terminal. Rounds do not relaunch inside a single prompt
turn: the client sends the next prompt, which is how an editor already works. Relaunching in-turn
would re-enter the runner while that turn's cancellation ownership and partial-transcript retention
are live.

The `goal` Cargo feature being enabled is not a claim that any of this is delivered.

## Required tests

- Record: serde round trip per status, legacy session byte-identical re-save, shared-store cloning,
  unknown-status parking, a record from a newer build loading with the objective intact, a stored id
  that is not a plain identifier being reissued, caps.
- Gate: one test per row, ordering collisions, a combinatorial sweep asserting exactly one outcome
  and correct counters, determinism, and that no tier can buy rounds past an exhausted bound.
- Tool: per-status required fields, closed schema, refusal unless a round is running, and that a met
  report changes nothing on the goal.
- Prompt: block stability across rounds, data framing order, objective present exactly once,
  overhead accounting, compaction restatement.
- Driver: round collection, both history modes, the wrap-up cap reaching the agent that runs the
  round, an interrupt leaving the goal untouched — including one during verification — and
  verification requested only on a completion claim.
- Checks: pass, non-zero, timeout, verify-before-checks ordering, and that nothing to run is not a
  vacuous pass.
- Judge: resolution table including cross-provider and fallback, verdict grammar including garbage,
  transcript fencing against a tail that closes it, a tail that keeps the newest messages, the round
  under judgement being in that tail, the fail-open ladder and what does not climb it, and that
  passing checks survive an outage.
- Surfaces: slash subcommands, status-line rendering, CLI parsing, headless exit codes walked
  through the production mapping, a round boundary that persists and records, an export carrying a
  goal importing, and an editor's closed `_meta.goal` schema.
- Authority: security mode unchanged across a goal lifecycle; untrusted project config cannot supply
  a check, a judge model, or the `[goal].judge` choice.

Build and verification commands are the repository defaults: `cargo fmt`, `cargo test`,
`cargo clippy --all-targets -- -D warnings` on the supported feature rows, and
`cargo install --path . --debug`. Never `cargo build`, `cargo check`, or `--release`.

## `--loop` as a preset

`--loop` and `/loop` construct a goal through `extras::goal::preset::loop_goal`
and run it on the goal driver. The mapping is: prompt to objective, `--loop-run`
to a check with `check_every_round` set, `--loop-plan` to `context_file`,
`--loop-max` to `max_rounds` with `wrap_up_max_agent_turns` zero, and restart
continuation so each iteration begins from a clean conversation.

A zero wrap-up budget makes a bound exact. When a bound falls due the gate still
adjudicates a completion claim made in that round, and still runs per-round
checks so a validator reports on the final iteration; a claim the checks reject
then ends the goal rather than buying another round.

Two things about a loop are deliberately not the goal default. Its judge is off
unless the installation named one, because a loop has never called a second
model and its early finish is meant to rest on the validator. And `--no-tools`
means what it says: such a loop has no way to report, so it is driven by its
validator alone and its iteration cap is what ends it.

The goal is installed before the agent is built, because the preamble's goal
block is captured at build time and an agent built without it never learns it is
working toward anything. Installation configuration applies first and the loop's
own flags over it, so `--loop-max` beats a `[goal]` round cap while a project's
required check still runs. `Stop` hooks still receive `loop_iteration` and
`loop_active`, derived from the round the goal is on.

`LoopState` no longer exists. The loop module keeps only its plan handling, the
bounded validation runner shared with goal checks, and the workflow-only
headless verification that `--loop-verification-policy-check` exercises. The
`loop` Cargo feature therefore depends on `goal`.
