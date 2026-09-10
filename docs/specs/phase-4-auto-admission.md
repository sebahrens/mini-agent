# Phase 4 — Agent Proposals and Human-Gated Admission

- **Document role**: normative phase specification
- **Specification version**: 1.6.0
- **Delivery status**: delivered
- **Owner**: mini-agent maintainers
- **Last reconciled**: 2026-09-08
- **Entry dependencies**: Foundation, Phase 1, and Phase 3 complete; Phase 2 is optional
- **Exit dependency**: every acceptance criterion below and every Phase 4 blocker

**Library contract**: a bounded `propose_skill()` host function, durable evaluation queue,
independent held-out cases, and human approval into canary state.

**Shipped-binary status**: a build with the `skills` feature exposes authenticated local-owner
commands for package/held-out-suite import, seed installation, stats, reevaluation, approval, rejection, and
explicit lineage-root activation. Trusted `enable_skill_proposals = true` configuration registers
`propose_skill` and starts the bounded proposal/admission workers; it is off by default. Proposals
stop at `awaiting_approval`, approval creates a non-retrievable canary, and activation remains a
distinct local-owner action.

The default CLI identifies both root-lifecycle approval actions as the same authenticated
`local-owner` principal. Distinct approval records preserve two explicit operator actions, but
they are not evidence of two independent human reviewers. Deployments that require separation of
duties must add an external identity-aware approval control.

The corpus authority and conflict rules are defined in
[`00-index.md`](00-index.md). The filename is retained for stable links, but Phase 4 does **not**
auto-admit or auto-activate agent-authored code. It owns proposal evaluation and explicit human
approval into a non-retrievable canary. Phase 5 alone owns evidence-based automatic transitions.

Phase 6 supersedes this phase's identity-v1 flat proposal capability payload, JS-thread host
placement, and verifier runtime ownership. Identity-v2 proposals carry complete structured scopes,
cross worker IPC as bounded drafts, and are canonicalized/persisted only by the parent. Phase 4's
field bounds, independent held-out evaluation, immutable reports, and human approval gates remain
authoritative. Brokered identity-v2 proposal transport and the local-owner operator workflow are
both exercised through production wiring and regression tests.

---

## Scope and safety boundary

Phase 4 lets the agent nominate successful JS code as an immutable reusable skill revision. It
adds the proposal and verification boundary, not autonomous learning. A proposal follows:

```text
agent proposal → pending → evaluating → verified → awaiting approval → canary
                         ├───────────────────────────────→ rejected (terminal)
                         └───────────────────────────────→ deferred (reopenable)
```

`deferred` parks a proposal whose *evaluation infrastructure* failed or whose claim budget was
spent. It is not a judgement about the artifact: the authenticated reevaluation transition can
return it to `pending`. The shipped `--reevaluate-learned-skill <SHA256>` command reopens a
proposal deferred for infrastructure failure or an exhausted claim budget, or a `verified` +
`held_out_suite_required` proposal. Importing the same package also requeues recoverable proposals
after restoring its matching baseline.
Explicit reevaluation also accepts `awaiting_approval`, allowing the operator to refresh an
outdated verifier report or changed suite selection before approving it. It clears the proposal's
report binding and preserves historical reports and their attempt numbering; approved and rejected
proposals remain ineligible. Reimport alone preserves an awaiting proposal's existing report.

Explicit reevaluation and rejection are authenticated database-only operations. They do not
initialize an embedding backend or execute the verifier, so missing embedding credentials or an
unavailable worker cannot block them. Rejection accepts only `awaiting_approval` proposals and
never replays an earlier approval; explicit approval retains its idempotent replay contract and
configured embedding/verification gates.

The evaluator:

1. recomputes and validates the full artifact identity under its owning identity version;
2. runs embedded tests through the owning fresh, bounded no-effect verifier contract;
3. runs inherited predecessor regressions for replacement proposals;
4. runs independent content-addressed held-out cases from trusted data storage;
5. verifies declared exports, capability, and duplicate policy;
6. persists an immutable structured report; and
7. requires human approval before atomically entering `canary`.

Passing tests is not proof of production quality. Phase 4 never marks an agent proposal `active`
merely because verification succeeded. Evidence thresholds, automatic Tier 0–1 promotion,
quarantine, supersession, repair, and rollback belong to Phase 5.

Held-out cases are independent because the agent cannot write, modify, inspect, or select them.
They are data-driven and content-addressed so adding a learned skill does not require recompiling
the binary. Checked-in Rust integration tests exercise the generic evaluator and trusted fixture
loader; there is no compile-time registry of every future skill ID.

---

## Prerequisites from Phase 3

Phase 3's immutable `SkillArtifact`, no-effect verifier, versioned embedder, and store must be complete.
Phase 4 uses the same `skill_revisions` table and lifecycle statuses. It does not create a second
mutable copy of canonical source that could drift from the final artifact.

---

## Target files

| File | Status | Purpose |
|------|--------|---------|
| `src/extras/js/skills/proposal.rs` | IMPLEMENTED | Bounded validation, session budget, and durable queue handoff |
| `src/extras/js/skills/admission.rs` | IMPLEMENTED | Lease evaluator, review packet, retry classification, and approval orchestration |
| `src/extras/js/skills/admission_store.rs` | IMPLEMENTED | Private optimistic canary/denial transactions |
| `src/extras/js/skills/held_out.rs` | IMPLEMENTED | Trusted data-driven case store/loader and report binding |
| `src/extras/js/skills/turn.rs` | IMPLEMENTED | Active-only retrieval, prompt manifest, and frozen turn-bundle boundary (`TurnSkillBundle`, `SkillTurnContext`, `render_trusted_context`) |
| `src/extras/js/skills/visibility.rs` | TEST-ONLY | Historical visibility-snapshot helper; `#[cfg(test)]` and read by no production path |
| `src/extras/js/skills/verify.rs` | EXTENDED | Pure embedded/inherited/held-out execution |
| `src/extras/js/skills/store.rs` | EXTENDED | Proposal/report/suite/approval schema and lifecycle primitives |
| `src/extras/js/host.rs`, `skills/proposal.rs` | EXTENDED | Parent proposal effect service, bounds, durable queue handoff |
| `src/extras/js/worker.rs` | EXTENDED | Register the model-only wire global; verifier and stored-skill modes omit it |

The delivered Phase 4 implementation keeps proposal persistence on a dedicated bounded parent-side
admission worker, not the historical QuickJS thread. Phase 6 preserves that parent-owned
persistence boundary: the contained JS worker sends a bounded proposal draft over the protocol and
never opens the store. The admission worker opens its own store connection and advances durable
proposals off the JS execution path and async executor threads. `AdmissionEvaluator` is the sole
evaluator/reviewer service. Its private
`admission_store` dependency owns the only Phase 4 canary transaction; neither `SkillStore`'s
public surface nor a JS global exposes an active transition. Active-only visibility snapshots
exclude pending, verified, rejected, and canary revisions by construction.
The generic Phase 5 lifecycle transaction also refuses entry into canary, preserving the Phase 4
approval boundary. Admission and lifecycle tests assert the resulting stored status, retrieval
visibility, authorization requirements, and unchanged state after refused transitions.

---

## Durable proposal records

### Pending state

```sql
CREATE TABLE IF NOT EXISTS skill_proposals (
    proposal_id      TEXT PRIMARY KEY,
    skill_id         TEXT NOT NULL,
    predecessor_id   TEXT,
    proposed_at      INTEGER NOT NULL,
    status           TEXT NOT NULL DEFAULT 'pending',
    attempt_count    INTEGER NOT NULL DEFAULT 0,
    next_attempt_at  INTEGER,
    lease_owner      TEXT,
    lease_expires_at INTEGER,
    report_id        TEXT,
    reason_code      TEXT,
    infrastructure_attempt_count INTEGER NOT NULL DEFAULT 0,
    CHECK (status IN (
        'pending','evaluating','deferred','verified','rejected',
        'awaiting_approval','approved'
    )),
    CHECK ((status = 'deferred'
            AND reason_code IN ('evaluation_infrastructure_deferred',
                                'evaluation_attempts_exhausted'))
           OR status <> 'deferred')
);

CREATE TABLE IF NOT EXISTS held_out_suites (
    suite_id      TEXT PRIMARY KEY,
    selector_json TEXT NOT NULL,
    cases_json    TEXT NOT NULL,
    approved_by   TEXT NOT NULL,
    approved_at   INTEGER NOT NULL,
    content_hash  TEXT NOT NULL UNIQUE,
    enabled       INTEGER NOT NULL DEFAULT 1
);
```

The shipped store is at `PRAGMA user_version` 13. Migration 11 → 12 rebuilt `skill_proposals`
solely to widen the deferred reason-code CHECK so it also admits `evaluation_attempts_exhausted`,
and 12 → 13 rebuilt `skill_task_outcomes` to admit the `gate_skipped` source.

Identity-bearing fields remain in `skill_revisions`, initially with `status = 'pending'`.
`skill_proposals` stores queue and evaluation metadata. Foreign keys and uniqueness constraints
prevent a proposal from naming a different artifact after enqueue.

`deferred` is a non-terminal parking state, not a verdict about the candidate. A proposal is
parked there when the evaluation infrastructure — not the artifact — kept failing: the durable
`infrastructure_attempt_count` reaching its bound records
`evaluation_infrastructure_deferred`, and a proposal whose ordinary claim budget is spent while it
is still `pending` or holding an expired `evaluating` lease is swept to
`evaluation_attempts_exhausted`. Both reason codes are accepted by the same authenticated
reevaluation transition that reopens a `verified` + `held_out_suite_required` row; reopening an
exhausted row also clears its spent claim budget, and neither reopen alters identity-bearing bytes
or resets a deterministic rejection. The shipped `--reevaluate-learned-skill` command invokes
this transition directly for all three parking reasons. `--import-learned-skill` also requeues
recoverable proposals while importing their matching baselines.

Claims use persisted leases and retries so a crash cannot strand a row in `evaluating`.
The bounded claim counter and durable report attempt number are distinct. A claim allocates its
report number above every stored report for that proposal inside the claim transaction. Explicit
recovery resets a spent claim budget for every eligible proposal, including a final evaluation
waiting for a held-out suite or awaiting approval, without colliding with or replacing historical
reports. Restoring the suite and requesting reevaluation once must make the proposal claimable.
Evaluation reports bind proposal ID, artifact ID, verifier version, matched held-out suite hashes,
predecessor ID, attempt number, and timestamps. Reason codes are stable; human-readable messages
are supplementary, generated only from fixed templates and sanitized typed fields, and never embed
an arbitrary JS exception message/stack, thrown value, source snippet, effect result, prompt, or
content.

Deterministic failures reject that immutable revision permanently. Retryable infrastructure errors
use bounded exponential backoff and preserve the row. A changed artifact always gets a new ID;
resetting queue status never changes content in place. Rejection atomically marks both the proposal
and `skill_revisions.status` as `rejected`. Re-proposing the same content-addressed ID returns the
existing rejection/report idempotently; only changed identity-bearing content creates a new chance.

---

## `propose_skill()` host global

The current identity-v2 payload uses canonical structured `grants`. Omission, ambiguity, an
identity-v1 `allowed_hosts` list, or an unknown field is rejected rather than widened or inferred.

```javascript
propose_skill({
  source,
  description,
  exports,
  tests,
  capability: {
    tier,
    grants: [
      { kind: "read_file", workspace_prefixes: ["src"] },
      { kind: "fetch", origins: ["https://docs.rs"], methods: ["GET"] },
      { kind: "spawn", programs: ["cargo"] }
    ]
  },
  tags?,
  predecessor_id?
})
```

| Argument | Type | Constraint |
|----------|------|------------|
| `source` | string | Function source, nonempty, max 32 KiB |
| `description` | string | Retrieval description, nonempty, max 1 KiB |
| `exports` | object[] | Public names and signatures, min 1, bounded count/bytes |
| `tests` | string[] | Expressions required to return exact boolean `true`, 1–20, max 4 KiB each |
| `capability` | object | Tier plus exact structured target grants; must be internally consistent and Tier 0–2 |
| `tags` | string[] | Optional normalized retrieval tags with count/length limits |
| `predecessor_id` | string | Optional full immutable revision ID for a replacement proposal |

The `workspace_prefixes` inside a `read_file` or `write_file` grant must already be in canonical
form: the structure is validated, never rewritten to repair a bad separator. A prefix is rejected
when it is empty, starts with `/`, **ends with `/`**, contains `\` or `:`, contains a control
character, or has an empty, `.` or `..` component. The example above therefore writes `"src"`, not
`"src/"`. The only rewriting applied is NFC normalization of each surviving path component.

The historical Phase 4 host canonicalized a flat identity-v1 proposal. The current worker only
decodes the bounded identity-v2 draft; the parent validates and canonicalizes it, writes durable
intent, and computes identity v2 including the ABI and complete structured grants. Duplicate
submissions of the same artifact/version are
idempotent. A predecessor must resolve to an eligible active/canary revision. The complete
canonical payload and per-session proposal count have fixed limits. Queue backpressure returns a
typed retryable error rather than growing memory without bound.

The sole exception to predecessor eligibility is Phase 6's explicit parent-owned identity
migration reproposal path. It may link a quarantined identity-v1 predecessor to the new identity-v2
revision for immutable audit lineage, but that predecessor supplies no execution, rollback,
non-inferiority, or capability-scope authority. Ordinary model-authored proposals cannot select
this exception.

The worker closure performs bounded JS-to-wire decoding only. It never evaluates proposed source,
opens the store, or owns the attempt budget. The parent effect service performs bounded shape and
canonicalization checks, appends audit intent before queue dispatch, and durably reconciles the
completion. Evaluation happens in bounded blocking workers after the current tool call.

Proposal cancellation coverage calls the same prepared-effect entry point as the parent
broker. It verifies an empty queue for pre-dispatch cancellation and holds a received queue
command's reply until the caller observes `OutcomeUnknown`; the background waiter must still
accept the late reply, and the next proposal succeeds. This uses explicit receipt/reply
coordination. The contained `JsTool` budget test covers missing tests and invalid capability
tiers; broker-level coverage retains wire-limit rejection and durable audit checks.

Example response:

```json
{
  "id": "<64-character sha256>",
  "proposal_id": "<proposal identifier>",
  "status": "pending",
  "report_id": null
}
```

`status` is the queue state observed at enqueue, not an admission decision. `pending` is only an
acknowledgement that the durable row exists; evaluation runs afterwards on the admission worker.
`report_id` is `null` until an evaluation report has been written, and an idempotent resubmission of
an already-settled artifact answers with that settled status and its `report_id` instead. A caller
that needs the outcome must read it back later — the session tracks its own enqueued proposals and
reports settled outcomes on a later call, and the operator surface exposes the same decision through
`--learned-skill-proposal`.

The proposal-attempt limit is defense in depth, not an evidence threshold. It is initialized per
session, applies before enqueue/evaluation work, and returns a non-panicking structured error when
exhausted.

---

## Evaluation pipeline

```rust
impl AdmissionEvaluator {
    pub async fn evaluate_next(&self) -> anyhow::Result<Option<EvaluationReport>> {
        // 1. Lease one due pending proposal and increment its attempt count.
        // 2. Reload artifact/predecessor and recompute canonical identity.
        // 3. Run embedded tests in the fresh no-effect verifier.
        // 4. Run all inherited predecessor regressions and matched held-out suites.
        // 5. Verify exports/capability and the exact contract-duplicate policy.
        // 6. Persist a structured report and mark verified or rejected.
        // 7. Generate a versioned embedding for verified artifacts off the request path.
        // 8. Move verified proposals to awaiting_approval; do not activate them.
    }
}
```

The evaluator always reloads persisted bytes; it does not trust the in-memory object used during
enqueue. The no-effect verifier gives Tier 0 no host globals. Tier 1/2 receive only the Phase 3
deterministic declared-capability fakes; they cannot touch real files, permissions, processes, or
networks. It requires at least one embedded test and exact JavaScript boolean `true` for every
expression. The Phase 3 mutation pass must prove each declared export affects at least one test.
Timeout, OOM, excessive pending jobs, syntax errors, Promise rejection, and undeclared fake-host
use fail.

Replacement proposals inherit all predecessor embedded tests, held-out cases, and later Phase 5
regression cases. A proposal may add tests but cannot omit inherited cases. A replacement must
name its predecessor and satisfy capability non-escalation unless the human explicitly approves
the new higher tier.

### Stable rejection and retry codes

| Failure | Reason code |
|---------|-------------|
| Canonical identity mismatch/corruption | `identity_invalid` |
| Embedded test is not exact `true` or throws | `embedded_test_failed` |
| Timeout, OOM, or job bound | `verification_resource_limit` |
| Missing or failed predecessor regression | `inherited_regression_failed` |
| Held-out case fails | `held_out_failed` |
| Export/capability mismatch | `contract_invalid` |
| Duplicate contract against an active, canary, or awaiting-approval revision | `duplicate_skill` |
| Verification infrastructure failed deterministically | `evaluation_infrastructure_unavailable` |

The duplicate gate is an exact contract comparison, not a similarity search: it matches on the
case-insensitive trimmed `description` together with the identical ordered export set, excluding the
candidate's own lineage. It runs before verification and before any embedding exists for the
candidate, so embedding- or vector-similarity near-duplicate detection is **out of scope for this
phase** and would require reordering the pipeline. Retrieval-time collapsing of semantic
near-duplicates is Phase 3's concern.

Deferred parking uses its own reason codes and is not a rejection:

| Parking outcome | Status | Reason code |
|-----------------|--------|-------------|
| No matching held-out suite | `verified` | `held_out_suite_required` |
| Repeated verification infrastructure failure | `deferred` | `evaluation_infrastructure_deferred` |
| Claim budget spent while pending or lease-expired | `deferred` | `evaluation_attempts_exhausted` |

---

## Independent held-out suites

```rust
pub struct HeldOutSuite {
    pub id: String,
    pub selector: HeldOutSelector,
    pub cases: Vec<HeldOutCase>,
}

pub struct HeldOutCase {
    pub expression: String,
    pub expected: ExpectedJsValue,
}
```

Suite IDs are SHA-256 hashes of a versioned canonical payload. Human/admin-only import validates
bounds and records approval. The proposal API cannot list suite inputs or expected outputs, write
the suite database, or choose which suite runs.

Authenticated reimport of the same canonical suite restores damaged stored representations and
re-enables the suite. A repair records fresh approval metadata and advances the row version;
an unchanged enabled suite is an idempotent no-op. Changing suite content produces a new ID.

The local-owner `--list-learned-skill-suites` command lists canonical suite IDs and enabled state
without exposing selectors, cases, fixture responses, or transcripts. The authenticated
`--disable-learned-skill-suite <SHA256>` command disables one suite without deleting its immutable
data or historical report bindings. Repeated disabling is idempotent; an unknown ID fails.
After correcting the corpus, `--reevaluate-learned-skill` requeues a parked proposal. Explicit
validated reimport re-enables a disabled suite. These commands are unavailable to proposal APIs;
approval continues to revalidate the currently enabled corpus.

Within each case, spawn fixtures must have distinct `(program, args)` keys and fetch fixtures
must have distinct `(url, method)` keys. Duplicate keys are rejected at import, even when their
responses agree. Different cases own independent fixture maps and may reuse the same keys.

Selectors use deterministic trusted fields such as capability, declared exports, and
human-approved tags. Selection and suite IDs are recorded in the evaluation report before
execution. Cases run under the same fresh, bounded, no-effect contract as embedded tests. A
held-out case may supply hidden verifier-fake responses and assert the fake call transcript.
Expected values, fixture responses, and transcripts are never included in agent input or telemetry.

Held-out integer expectations must fit signed 64-bit storage and be exactly representable as a
JavaScript Number. Verifier version 6 compares both QuickJS numeric representations by exact
integer value; it rejects fractions, non-finite numbers, non-numeric types, and rounded neighbors.
Invalid expected integers are a trusted-corpus validation error, not a candidate test failure.

The evaluator permits at most 32 distinct matched suites and 64 total held-out cases across the
candidate and its complete predecessor lineage. Shared suites count once. If either bound is
exceeded, evaluation refuses the corpus before running any held-out case; it must never truncate
the suite union or execute a partial suite. Every successful report covers the complete union.

Corpus capacity failures and malformed, tampered, invalid, or unsupported stored suites belong to
trusted evaluation infrastructure, not candidate source. They follow bounded infrastructure retry
and then `evaluation_infrastructure_deferred`; correcting the corpus and resubmitting the unchanged
artifact permits reevaluation. During human approval, these failures report infrastructure
unavailability and leave the reviewed proposal intact. Corruption diagnostics use a fixed message
without parser details or hidden fixture values. Actual failed cases remain deterministic rejections.

If no suitable suite matches, the proposal remains verified with
`held_out_suite_required`. It cannot enter canary until a human imports or approves a suite and
requests reevaluation. Agent-authored embedded tests alone never satisfy this gate.
The authenticated reevaluation transition atomically returns only the selected proposal and its
revision to `pending`; it cannot reset deterministic rejection or alter identity-bearing bytes. It
also accepts the two `deferred` reason codes above and unapproved `awaiting_approval` proposals.
In the shipped binary this transition is
available through `--reevaluate-learned-skill <SHA256>`. Importing a package whose held-out
baseline is bundled with it also imports the baseline and requeues a recoverable proposal.

## Promotion gate

### Promotion gate — trusted held-out suite

At least one applicable trusted held-out suite must pass through the Rust-owned generic evaluator
before human approval can create a canary. Checked-in Rust integration tests exercise this full
loader/selector/no-effect execution path with trusted fixtures. There is no compiled per-skill ID
registry, and an empty registry or agent-authored tests alone never satisfies the gate.

---

## Human approval into canary

The reviewer receives artifact ID, description/tags, exports/signatures, capability tier, source,
embedded test summary, inherited regression summary, held-out suite IDs/results, duplicate report,
and verifier version. Approval is an explicit authenticated action, not an LLM response.

The synchronous approval callback returns an authenticated human decision and
the admission service returns the canary transaction result directly. Rejection
uses the separate authenticated database-only operation, so damaged evaluation
reports, unavailable embeddings, or disabled held-out suites cannot prevent it.
Cancelling or abandoning a review before explicit approval leaves the proposal
awaiting approval; it does not invoke the approval callback. Authentication
freshness is checked before verification or publication, and the reviewed
artifact/report versions are checked again before the contained gate runs.

```sql
BEGIN IMMEDIATE;
-- Recheck artifact/report identity and optimistic versions.
UPDATE skill_revisions
SET status = 'canary', row_version = row_version + 1, updated_at = ?1
WHERE id = ?2 AND status = 'verified' AND row_version = ?3;
UPDATE skill_proposals
SET status = 'approved'
WHERE proposal_id = ?4 AND skill_id = ?2 AND status = 'awaiting_approval';
-- Record approver/audit data and increment the active index generation.
COMMIT;
```

Any stale row or statement failure aborts the transaction. Retrying an already successful approval
is idempotent. Tier 2 side-effecting skills remain human-gated permanently. Tier 3 security/admin
capabilities are rejected as reusable learned skills. Tier 0–1 may become automatically promotable
only under the Phase 5 evidence policy. A Phase 4 canary is durable but non-retrievable; Phase 5
adds deterministic bounded routing and evidence collection. Phase 4 never exposes all users to a
candidate merely because it was approved for future canary evaluation.

---

## Logging and privacy

Lifecycle logs use structured fields for artifact/proposal IDs, state transitions, reason codes,
durations, verifier version, and capability. They never include source, tests, held-out inputs or
outputs, raw prompts, tool arguments, file contents, environment values, or secrets. Human review
may display source and sanitized metadata only through an explicitly authorized interface.

---

## Delivered amendments (2026-09-05)

Accepted by the [2026-09-05 harness design review](../plans/2026-09-05-001-harness-design-review.md).

1. **Operator surface** (mini-agent-p0h1, delivered). The shipped binary provides authenticated local-owner
   commands to import a skill draft (verified in the contained worker and inserted as awaiting
   approval), and approve or reject an awaiting revision by identity into or out of canary. The
   stats command reports stored lifecycle statuses; there is no separate awaiting-only list
   command. `propose_skill` may be re-registered behind an explicit configuration flag so
   proposals land in the same queue. Every human gate in this phase is preserved; no command
   activates code automatically.
2. **Seed library and Agent-Skill bridge** (mini-agent-vvud, delivered). A shipped set of pure
   learned skills is verified at build/test time through the normal gate. An Agent Skill may
   declare up to 32 exact learned-JS identities; import resolves them only after verification and
   turn selection attaches only active revisions, so declaration never bypasses approval,
   activation, containment, or learned-skill budgets.
3. **Stats surface** (mini-agent-i78t, delivered). Per-skill selections, invocations, the `success`
   column, and last-use are readable by the operator. `success` is a **return rate**, not a
   correctness rate: its numerator counts terminal `returned` events and its denominator counts
   `returned` plus `threw`/`timed_out`/`oom`/`capability_denied`. A revision that returns
   semantically wrong data without throwing shows 100%. Correctness evidence lives in the separate
   `tasks_with`, `passed_with`, and `pass_rate_without` columns, which are derived from
   verifier/oracle task outcomes.
4. **Replacement integrity** (2026-09-06, delivered). Approval derives and persists a replacement's
   `supersedes_id` and lineage root from the reviewed predecessor, so a real approved canary is
   routable and remains rollback-capable. Contract-duplicate evaluation excludes that exact
   predecessor's lineage while still rejecting every unrelated duplicate. Capability comparison is a
   structural subset test over each scoped grant, never tier-only equivalence. The proposal attempt
   budget is consumed only after bounded payload and predecessor validation, and the admission
   worker renews its lease before the worst-case contained verification window.

## Acceptance criteria

All must pass under `cargo test --features js,skills`:

- [x] Host validation rejects empty/oversized/malformed payloads and Tier 3 capabilities without
      panicking or evaluating proposal code inline.
- [x] Proposal identity covers source, ordered tests/exports, description/tags, capability, and
      identity version; duplicate proposals are idempotent.
- [x] Durable queue claims recover after crash/lease expiry and retry infrastructure failures with
      bounded backoff and attempt counts.
- [x] Evaluator reloads and rehashes the artifact before invoking the Phase 3 no-effect verifier.
- [x] Agent-authored tests have no real host effects, only exact boolean `true` passes, Tier 1/2
      access only declared deterministic fakes, and mutation checks reject vacuous suites.
- [x] Replacement proposals inherit every predecessor regression and cannot delete a failing case.
- [x] Held-out suites are data-driven, content-addressed, human-approved, hidden from proposal APIs,
      and generic integration fixtures do not require a per-skill Rust registry.
- [x] No matching held-out suite blocks approval; embedded-test success cannot activate a skill.
- [x] Evaluator verifies exports/capability and the exact contract-duplicate policy before human
      review, and generates the versioned embedding for a verified artifact off the request path.
- [x] Human approval atomically transitions only an unchanged verified revision to canary, records
      audit data, and bumps index generation; stale rows and simulated failures fully roll back.
- [x] Without Phase 5 routing, canary revisions remain absent from model manifests and JS bundles.
- [x] Phase 4 has no path that automatically marks an agent proposal active.
- [x] Proposal submission was non-blocking from the delivered Phase 4 JS thread and respected
      queue/session bounds; Phase 6 preserves those bounds through worker IPC and parent-owned
      durable enqueue.
- [x] Logs and reports omit source, tests, held-out values, raw prompts, arguments, and secrets.
- [x] `cargo test --features js` without `skills` passes unchanged.

---

## Out of scope for Phase 4

- Automatic promotion, quarantine, repair generation, supersession, rollback, evidence aggregation,
  or retention/privacy policy (Phase 5)
- Cross-agent/shared-library synchronization
- Human review UI beyond the minimum authenticated approval interface
