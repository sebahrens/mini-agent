# Goal feature design — devil's-advocate review disposition (2026-09-11)

Two adversarial reviews of `docs/superpowers/specs/2026-09-11-goal-feature-design.md` (first draft): one against the codebase, one against the design's own research and UX. Findings and what the revised Part 4 does with each.

| # | Finding (severity) | Disposition |
|---|---|---|
| C1 | Runner has no `Session`; a plain `Option<Goal>` on `Session` is cloned by value so gate writes vanish (blocker) | Accepted. Gate moved to the driver; `GoalStore(Arc<Mutex<..>>)` with custom serde like `TodoStore`; driver is the only writer of status/progress. |
| C2 | In-run continuation collides with `max_agent_turns` / `turn_token_budget`, which hard-fail and cannot soft-stop (blocker) | Accepted. Rounds relaunch with fresh per-response budgets; bounds are per goal (`max_rounds`, tokens, active time); wrap-up is a bounded extra round. |
| C3 | No client, no structured output, no usage accounting for a runner-level judge (blocker) | Accepted. Judge runs in the driver on the compaction-summarizer path extended to return usage; fixed text grammar with fail-open parsing; cross-provider judge entries refused in slice 3. |
| C4 | `%%goal=` is not on the ACP path and would be an unauthenticated instruction channel (blocker) | Accepted. Directive dropped; ACP uses `_meta.goal` from the connected client. |
| C5 | Per-turn counters in the preamble break prompt caching; "agent rebuilt on goal change" false in headless (major) | Accepted. Preamble block is static; mutable facts go in the per-round instruction; rebuild points stated per driver. |
| C6 | ACP has no `Session`, no `Session::compress`, no self-initiated turns (major) | Accepted. ACP rounds run inside one `session/prompt`; compaction block added on the ACP history path in slice 4. |
| C7 | `TaskOutcomeSource::Goal` would count self-report as skill-promotion evidence (major) | Accepted. Only checks/verify-verified verdicts count; Phase 5 amendment required first. |
| C8 | Sensitivity is per top-level key; `[goal].checks` cannot be marked alone; headless drops untrusted sensitive keys; `subprocess-trust.md` must be amended first (major) | Accepted. Top-level `goal_checks` and `goal_judge_model` keys; `TC-GOAL-CHECK` amendment bead blocks slice 2. |
| C9 | `LoopState` deletion is entangled across 10 files and the loop tests encode behaviour the goal changes (major) | Accepted. Slice 5 becomes a preset plus deprecated shim with a parity table and a list of tests expected to change. |
| C10 | `--no-tools`, allowlists and subagents versus `goal_report` (major) | Accepted. `goal_report` is a reserved harness tool outside allowlists; goal refused under `--no-tools`; subagents excluded. |
| C11 | `estimate_overhead` call sites, transcripts under `loops/`, export, non-default `hooks`/`skills` (minor) | Accepted. Three overhead sites named; `goals_dir`; export bead; gate functional without optional features. |
| D1 | Same-model judge on self-authored evidence is not independence (blocker) | Partly accepted. The maintainer's decision to default the judge on with session-model fallback stands; verdicts are labelled `unverified by checks`, the judge never overrides passing checks, judge-only verdicts are excluded from promotion evidence. |
| D2 | `Impossible` unreachable because the judge only ran on `met` (blocker) | Accepted. `goal_report` gains `impossible`; judge adjudicates it and runs a cadence drift check. |
| D3 | Judge `Impossible` could terminally override passing checks (blocker) | Accepted. Coerced to `NotYet` when checks or verify passed; `/goal reopen` added. |
| D4 | `%%goal=` untrusted channel (blocker) | Accepted (see C4); goal block framed as data outside the rules text. |
| D5 | Promotion violates `00-index.md` authority rules (blocker) | Accepted. §4.11 extension map and a spec-amendments bead. |
| D6 | Soft stop unbounded; wall clock counts human latency; token unit undefined (blocker) | Accepted. Wrap-up round capped at `wrap_up_max_agent_turns`; active time excludes pending permission prompts; token unit defined. |
| D7 | No-progress definition gameable and collides with the run-sticky flag; questions to the user get auto-answered (major) | Accepted. Per-round mutation flag; `needs_user` report → `AwaitingUser`. |
| D8 | Jaccard blocker matching gameable both ways (major) | Accepted. Text similarity dropped; count blocked rounds without mutation. |
| D9 | `max_turns` overloads an existing term (major) | Accepted. Renamed `max_rounds`; relationship to `max_agent_turns` stated. |
| D10 | Row 8 was not a decision; read-only `met` rounds bypass verify; round failure recovery unspecified (major) | Accepted. Row 13 runs `verify_command` on `met` when the round skipped it; rows 2–3 absorb round failures. |
| D11 | Structured output not uniform across providers; malformed verdict paused the goal (major) | Accepted. Text grammar; fail open. |
| D12 | Judge input unbounded and injectable; sections disagreed on judge sensitivity (major) | Accepted. 8 messages / 512-byte tool results / 24 KiB cap, sanitizer named, transcript fenced, `goal_judge_model` sensitive. |
| D13 | Continue and Restart are two products; fold loses behaviour (major) | Accepted in scope (see C9). |
| D14 | Replace/resume asymmetric; no way to raise bounds (major) | Accepted. Uniform refuse-without-clear; `/goal bounds`; wrap-up once per exhaustion. |
| D15 | `Paused` overloaded with no reason field (major) | Accepted. `paused_reason: PauseReason`. |
| D16 | Exit codes and ACP visibility under-specified (major) | Accepted. Distinct exit codes; `_meta.goal` on ACP updates. |
| D17 | `goal_report` lacks `impossible`, untyped schema (minor) | Accepted. |
| D18 | Restart summary is a model-authored goal-shrinking channel (minor) | Accepted. Driver-built summary from reports and verdicts. |
| D19 | Scope: cut `history`, judge variants, token/time CLI flags; missing acceptance-criteria negotiation (minor) | Partly accepted. `history` replaced by bounded `reports`; token/time CLI flags deferred to slice 4; `criteria` ("done when") added; judge variants kept because the maintainer chose auto-selection. |
| D20 | Hard-coded judge retry count; check rules under `--no-tools` unstated (minor) | Accepted by citation of the `verify_command` rules. |

Both reviews found sound: reading the objective from the record rather than history, compaction survival on the `Session` path, cadence re-injection, the read-only split between objective and `goal_report`, the serde shape, reuse of the validation runner, the gate ordering after verification and before Stop hooks, and the slicing order.
