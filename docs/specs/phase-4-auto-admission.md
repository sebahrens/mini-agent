# Phase 4 — Agent Proposals and Human-Gated Admission

- **Document role**: normative phase specification
- **Specification version**: 1.0.0
- **Delivery status**: planned
- **Owner**: mini-agent maintainers
- **Last reconciled**: 2026-07-29
- **Entry dependencies**: Foundation, Phase 1, and Phase 3 complete; Phase 2 is optional
- **Exit dependency**: every acceptance criterion below and every Phase 4 blocker

**Delivers**: a bounded `propose_skill()` host function, durable evaluation queue, independent
held-out cases, and human approval into canary state.

The corpus authority and conflict rules are defined in
[`00-index.md`](00-index.md). The filename is retained for stable links, but Phase 4 does **not**
auto-admit or auto-activate agent-authored code. It owns proposal evaluation and explicit human
approval into a non-retrievable canary. Phase 5 alone owns evidence-based automatic transitions.

---

## Scope and safety boundary

Phase 4 lets the agent nominate successful JS code as an immutable reusable skill revision. It
adds the proposal and verification boundary, not autonomous learning. A proposal follows:

```text
agent proposal → pending → evaluating → verified → awaiting approval → canary
                         └───────────────────────────────→ rejected (terminal)
```

The evaluator:

1. recomputes and validates the full artifact identity from Phase 3;
2. runs embedded tests in a fresh, bounded no-effect QuickJS context;
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
| `src/extras/js/skills/admission.rs` | TO BE CREATED | Durable queue, evaluator, approval transaction |
| `src/extras/js/skills/held_out.rs` | TO BE CREATED | Trusted data-driven case store/loader |
| `src/extras/js/skills/verify.rs` | Phase 3 creates | Pure embedded/inherited/held-out execution |
| `src/extras/js/skills/store.rs` | Phase 3 creates | Proposal/report schema and lifecycle transactions |
| `src/extras/js/host.rs` | Phase 1 creates | Register bounded `propose_skill` |
| `src/extras/js/types.rs` | Phase 1 creates | Proposal request/response channel types |
| `src/extras/js/engine.rs` | Phase 1 creates | Register the host only in normal `skills` execution mode; verifier modes omit it |

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
    report_json      TEXT,
    reason_code      TEXT,
    CHECK (status IN (
        'pending','evaluating','verified','rejected','awaiting_approval','approved'
    ))
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

Identity-bearing fields remain in `skill_revisions`, initially with `status = 'pending'`.
`skill_proposals` stores queue and evaluation metadata. Foreign keys and uniqueness constraints
prevent a proposal from naming a different artifact after enqueue.

Claims use persisted leases and retries so a crash cannot strand a row in `evaluating`.
Evaluation reports bind proposal ID, artifact ID, verifier version, matched held-out suite hashes,
predecessor ID, attempt number, and timestamps. Reason codes are stable; human-readable messages
are supplementary.

Deterministic failures reject that immutable revision permanently. Retryable infrastructure errors
use bounded exponential backoff and preserve the row. A changed artifact always gets a new ID;
resetting queue status never changes content in place. Rejection atomically marks both the proposal
and `skill_revisions.status` as `rejected`. Re-proposing the same content-addressed ID returns the
existing rejection/report idempotently; only changed identity-bearing content creates a new chance.

---

## `propose_skill()` host global

```javascript
propose_skill({
  source,
  description,
  exports,
  tests,
  capability: { tier, allowed_hosts },
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
| `capability` | object | Tier plus exact allowed host operations; must be internally consistent and Tier 0–2 |
| `tags` | string[] | Optional normalized retrieval tags with count/length limits |
| `predecessor_id` | string | Optional full immutable revision ID for a replacement proposal |

The host canonicalizes the proposal and computes the Phase 3 full identity. Duplicate submissions
of the same artifact are idempotent. A predecessor must resolve to an eligible active/canary
revision. The complete canonical payload and per-session proposal count have fixed limits. Queue
backpressure returns a typed retryable error rather than growing memory without bound.

```rust
pub fn make_propose_skill(
    proposal_tx: ProposalSender,
    attempt_budget: Arc<AtomicUsize>,
) -> impl Fn(JsProposal) -> rquickjs::Result<String> {
    move |proposal: JsProposal| {
        consume_attempt_budget(&attempt_budget)?;
        let artifact = validate_and_canonicalize(proposal)?;
        let result = proposal_tx.enqueue(artifact)?;
        serde_json::to_string(&result).map_err(to_js_error)
    }
}
```

The host performs bounded shape and canonicalization checks only. It never evaluates untrusted
proposal code on the current JS runtime or blocks on model embedding. Durable enqueue follows the
same async request/response boundary as other host I/O. Evaluation happens in bounded blocking
workers after the current tool call.

Example response:

```json
{
  "id": "<64-character sha256>",
  "status": "pending"
}
```

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
        // 5. Verify exports/capability and exact/semantic duplicate policy.
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
| Exact or policy-disallowed near duplicate | `duplicate_skill` |
| Embedding unavailable after bounded retries | `embedding_unavailable` |

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

Selectors use deterministic trusted fields such as capability, declared exports, and
human-approved tags. Selection and suite IDs are recorded in the evaluation report before
execution. Cases run under the same fresh, bounded, no-effect contract as embedded tests. A
held-out case may supply hidden verifier-fake responses and assert the fake call transcript.
Expected values, fixture responses, and transcripts are never included in agent input or telemetry.

If no suitable suite matches, the proposal remains verified with
`held_out_suite_required`. It cannot enter canary until a human imports or approves a suite and
requests reevaluation. Agent-authored embedded tests alone never satisfy this gate.

## Promotion gate

### Promotion gate — held-out Rust integration test

At least one applicable trusted held-out suite must pass through the Rust-owned generic evaluator
before human approval can create a canary. Checked-in Rust integration tests exercise this full
loader/selector/no-effect execution path with trusted fixtures. There is no compiled per-skill ID
registry, and an empty registry or agent-authored tests alone never satisfies the gate.

---

## Human approval into canary

The reviewer receives artifact ID, description/tags, exports/signatures, capability tier, source,
embedded test summary, inherited regression summary, held-out suite IDs/results, duplicate report,
and verifier version. Approval is an explicit authenticated action, not an LLM response.

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

## Acceptance criteria

All must pass under `cargo test --features js,skills`:

- [ ] Host validation rejects empty/oversized/malformed payloads and Tier 3 capabilities without
      panicking or evaluating proposal code inline.
- [ ] Proposal identity covers source, ordered tests/exports, description/tags, capability, and
      identity version; duplicate proposals are idempotent.
- [ ] Durable queue claims recover after crash/lease expiry and retry infrastructure failures with
      bounded backoff and attempt counts.
- [ ] Evaluator reloads and rehashes the artifact before invoking the Phase 3 no-effect verifier.
- [ ] Agent-authored tests have no real host effects, only exact boolean `true` passes, Tier 1/2
      access only declared deterministic fakes, and mutation checks reject vacuous suites.
- [ ] Replacement proposals inherit every predecessor regression and cannot delete a failing case.
- [ ] Held-out suites are data-driven, content-addressed, human-approved, hidden from proposal APIs,
      and generic integration fixtures do not require a per-skill Rust registry.
- [ ] No matching held-out suite blocks approval; embedded-test success cannot activate a skill.
- [ ] Evaluator verifies exports/capability, semantic duplicates, and versioned embedding before
      human review.
- [ ] Human approval atomically transitions only an unchanged verified revision to canary, records
      audit data, and bumps index generation; stale rows and simulated failures fully roll back.
- [ ] Without Phase 5 routing, canary revisions remain absent from model manifests and JS bundles.
- [ ] Phase 4 has no path that automatically marks an agent proposal active.
- [ ] Proposal submission remains non-blocking from the JS thread and respects queue/session bounds.
- [ ] Logs and reports omit source, tests, held-out values, raw prompts, arguments, and secrets.
- [ ] `cargo test --features js` without `skills` passes unchanged.

---

## Out of scope for Phase 4

- Automatic promotion, quarantine, repair generation, supersession, rollback, evidence aggregation,
  or retention/privacy policy (Phase 5)
- Cross-agent/shared-library synchronization
- Human review UI beyond the minimum authenticated approval interface
