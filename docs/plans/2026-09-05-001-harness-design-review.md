---
title: "review: harness design review — code as tool, personas, core loop"
type: review
status: accepted
date: 2026-09-05
epic: mini-agent-5ana
---

# 2026-09-05 harness design review and implementation plan

Six read-only reviewers were fanned out over disjoint areas at HEAD `b9277b3` (core loop and
context, JS code-as-tool surface, learned-skill library, native tools, personas and subagents,
provider/TUI/MCP/memory). Two research agents surveyed current harness practice (Anthropic
building-effective-agents, writing-tools-for-agents, effective-context-engineering, advanced tool
use and programmatic tool calling; CodeAct; smolagents; Voyager; Agent Skills; SWE-agent ACI
ablations; SWE-Skills-Bench; Harness-Bench; LangChain harness engineering). Every P1 and every
area's top claims were re-read in the source by the orchestrator before filing. Six reviewer
claims that did not survive were dropped or recorded as stale (`bd memories
review-claims-refuted-2026-09-05`).

Tracking: epic `mini-agent-5ana`, label `review-2026-09-05`, one bead per finding (98). Children
are deliberately not linked with parent-child edges so `bd ready` lists them. Filter with
`bd list -l review-2026-09-05`.

The Phase 6 security architecture was not the review target and is not weakened by anything
below. Every amendment is additive under the canonical checklist in
[`phase-6-brokered-js-runtime.md`](../specs/phase-6-brokered-js-runtime.md).

## Verdict

The containment story is mature. The performance gap is on the model's side of the boundary:

1. **Code-as-tool is secured but not sold to the model.** The `js` description lists five global
   names and nothing else; model code runs in strict script mode where top-level `await` is a
   syntax error; every failure renders as `exception`; on macOS there is no `spawn` and no file
   discovery effect; every effect costs one IPC round trip and two fsyncs.
2. **The learned-skill library is inert and its default retrieval is random.** The shipped binary
   does not register `propose_skill` and has no import/approve surface, so `skills.db` is empty in
   production; the default `Deterministic` embedding backend is a SHA-256 projection with a 0.20
   floor, and the lexical query ANDs every prompt word.
3. **Personas need separation of context and tools, not identity text.** A repo-controlled
   `.zerostack/agents/*.md` becomes the authoritative child prompt with no trust gate; children get
   a bare string and return free text; a child that hits `task_max_turns` fails and cancels its
   siblings.
4. **The loop drops turns and hides work from the next turn.** Only the first completion call of
   a turn is retried; 529 is not retryable; any later error rolls the whole turn back while edits
   stay on disk; cross-turn history is flattened to `[ToolCall]:` prose; batched tools run
   sequentially; shell is capped at 30 s so the model can never run `cargo test`.

## Design amendments

Normative wording lives in the owning spec under a section named **Accepted amendments
(2026-09-05, pending delivery)**. This table is the map.

| Amendment | Owning spec | Beads |
|---|---|---|
| Model script evaluated in async (promise) mode; description documents script/strict semantics | Phase 6 `Worker lifecycle` | mini-agent-ml1u |
| Closed exception class plus validated line/column in diagnostics (protocol v4) | Phase 6 `Failure semantics` | mini-agent-m2kw |
| Effect-count exhaustion is a bounded step error, not a worker fault | Phase 6 `Failure semantics` | mini-agent-12cr |
| Additive read-only effects `list_dir`, `glob`, `grep`, and batched `read_files` under the same broker rules | Phase 6 `Capability broker`; Phase 2 narrowing | mini-agent-w2lv, mini-agent-ae65 |
| Distinct closed effect denial codes (`not_found`, `is_directory`, `denied`, `too_large`) | Phase 6 `Capability broker` | mini-agent-dr93 |
| Permission-wait rendered distinctly from compute timeout | Phase 6 `Failure semantics` | mini-agent-osaj |
| Typed result channel and JSON-only scratch store (design gate required) | Phase 6 `Capability broker` | mini-agent-yl18 |
| Deterministic embedding backend disables dense retrieval; lexical query is OR/BM25 | Phase 3 `SkillIndex and hybrid retrieval` | mini-agent-bfsg, mini-agent-io7h |
| Skill context delivered outside persisted user text; manifest names exports as callables | Phase 3 `Preamble injection` | mini-agent-rd89, mini-agent-4bqq |
| Bounded model-issued `skills_search` (metadata only) — decision needed against the index invariant | Phase 3 `Retrieval query`; `00-index.md` | mini-agent-a8a0 |
| Operator surface: import, approve, reject, stats; seed library | Phase 4 `Human approval into canary` | mini-agent-p0h1, mini-agent-vvud, mini-agent-i78t |
| Quarantine counts faults, not expected `threw`; canary selection by age; WAL + immediate transactions; skip corrupt embedding rows | Phase 5 `Automatic quarantine`, `Failure semantics` | mini-agent-lugc, mini-agent-840z, mini-agent-pwf2, mini-agent-jj8b |
| Project persona definitions trust-gated; non-overridable untrusted-content rules; structured brief and return skeleton | `docs/agent/SUBAGENTS.md` (no normative spec) | mini-agent-yb9w, mini-agent-nfd7, mini-agent-ddno |

## Implementation workstreams

Ordered by leverage. Each workstream is independently shippable; P1 beads first.

### W1 — Loop integrity (core-loop, provider)

- mini-agent-5xan retry between steps, keep partial transcripts; mini-agent-k90x 529;
  mini-agent-3ley context-length short-circuit; mini-agent-i160 empty-response check.
- mini-agent-0b6e structured tool history (reuse the ACP interaction store);
  mini-agent-9fzx read-block scoping; mini-agent-veu4 tool-result bound at result time;
  mini-agent-svmt tool-result pruning; mini-agent-gm16 compaction at a tool boundary.
- mini-agent-zlva tool concurrency for read-only tools.
- mini-agent-j946 system prompt sections; mini-agent-saje headless structured output.
- Acceptance: a scripted-provider test where the third completion call returns 529 must finish
  the turn with all tool records persisted; `convert_history` must round-trip a tool call with
  its arguments; a 1 MiB shell output must reach the model as head/tail plus a spill path.

### W2 — Verification and command deadline (tools, harness)

- mini-agent-3m0a configurable command deadline; mini-agent-9m0s background jobs;
  mini-agent-2g2z verification gate (`verify_command`).
- Planned config keys: `command_timeout_secs` (default 120), `command_timeout_max_secs`
  (default 600), `verify_command`, `verify_timeout_secs`, `verify_max_rounds` (default 3).
- Acceptance: `mini-agent -p "run cargo test --no-run"` completes in this repository; a failing
  `verify_command` feeds a bounded tail back and the turn ends with a visible verification status.

### W3 — Code-as-tool ergonomics (js)

- P1: mini-agent-ml1u, mini-agent-m2kw, mini-agent-b7xb, mini-agent-12cr.
- P2: mini-agent-w2lv discovery effects; mini-agent-ae65 batched effects and audit group
  commit; mini-agent-dr93 denial codes; mini-agent-tcug result conversion; mini-agent-im6v
  console; mini-agent-osaj permission wait; mini-agent-7w1l description; mini-agent-yl18 typed
  result and scratch (design gate); mini-agent-dw0z, mini-agent-7u1n, mini-agent-jqa5 nits.
- Acceptance: contained-worker integration tests for each new effect covering denial and bounded
  failure (per CLAUDE.md); a model script with 300 `read_file` calls returns a bounded error with
  its console records; `await fetch(u)` at top level succeeds; a `SyntaxError` renders its class
  and line.

### W4 — Skill library reachability and retrieval (skills)

- P1: mini-agent-bfsg, mini-agent-io7h, mini-agent-p0h1, mini-agent-pwf2, mini-agent-jj8b.
- P2: mini-agent-rd89, mini-agent-4bqq, mini-agent-a8a0, mini-agent-lugc, mini-agent-840z,
  mini-agent-17x2, mini-agent-ml1a, mini-agent-i78t, mini-agent-vvud, mini-agent-a8gq;
  P3 mini-agent-xkbl, mini-agent-c8q6.
- Acceptance: with the deterministic backend and 20 unrelated skills, a random prompt selects
  zero skills; the NL benchmark case matches through the lexical channel; a skill imported through
  the operator command is retrievable after approval without a Rust change.

### W5 — Personas and subagents

- P1: mini-agent-ddno, mini-agent-yb9w. P2: mini-agent-kh1o, mini-agent-166x, mini-agent-ukp8,
  mini-agent-7hjo, mini-agent-nfd7, mini-agent-6khf, mini-agent-abys, mini-agent-cflr,
  mini-agent-s7s6, mini-agent-sux9, mini-agent-kwzu. P3: mini-agent-uwva, mini-agent-unbw.
- Acceptance: a child exhausting its turn budget returns `[partial: turn budget exhausted]` and
  siblings complete; an untrusted checkout's `.zerostack/agents/x.md` is ignored with a notice;
  the `task` schema enumerates installed personas.

### W6 — Native tools

- P1: mini-agent-fcer, mini-agent-j4kw, mini-agent-5gy8. P2: mini-agent-ry6o, mini-agent-0o7o,
  mini-agent-wvez, mini-agent-92ee, mini-agent-0mo1, mini-agent-c9en, mini-agent-qpe0,
  mini-agent-zexk. P3: mini-agent-dlzc, mini-agent-ucxe, mini-agent-f2vd, mini-agent-hk1y,
  mini-agent-4lyj, mini-agent-ra6k, mini-agent-y1l0.

### W7 — Provider, TUI, MCP, startup, eval

- mini-agent-hyxg catalog; mini-agent-v1lz OpenRouter caching; mini-agent-4jtx, mini-agent-7zvo.
- mini-agent-zjbn feed; mini-agent-xfzm git status; mini-agent-8qty, mini-agent-m7df MCP bounds;
  mini-agent-qs82; mini-agent-5nob; mini-agent-6oxh memory refresh; mini-agent-ngvc ACP history.
- mini-agent-fao6 harness regression eval; mini-agent-bsnl cache-stability test;
  mini-agent-cfqz loop detection; mini-agent-ma2l cached-token cost units.

## Documentation changes made with this review

- `docs/specs/00-index.md` 1.4.0: amendment map and the open decision on model-issued skill
  search.
- `docs/specs/phase-6-brokered-js-runtime.md` 1.1.0: amendments section; spawn-authority sentence
  corrected to include the Windows AppContainer backend.
- `docs/specs/phase-3-skill-library.md` 1.3.0, `phase-4-auto-admission.md` 1.3.0,
  `phase-5-evidence-learning.md` 1.2.0: amendments sections.
- `docs/agent/SUBAGENTS.md`: schema does not enumerate personas; per-child 300 s cap; bash
  exclusion reason; planned changes.
- `docs/agent/MEMORY.md`: memory block is captured at agent construction, not every turn.
- `docs/agent/TOOL_RUNTIME.md`, `docs/agent/SKILLS.md`, `docs/benchmarks/skill-retrieval.md`,
  `ARCHITECTURE.md`, `SPEC.md`, `README.md`, `CHANGELOG.md`, `AGENTS.md`, `CLAUDE.md`: current
  limitations and pointers to this plan.

## Non-goals

- No change to the Phase 6 canonical invariants, the fresh-runtime rule, or the parent-owned
  effect model.
- No in-parent or persistent JavaScript state; the scratch store is JSON-only and parent-owned.
- No automatic skill activation; every operator surface keeps the human gate.
