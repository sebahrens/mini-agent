# Phase 5 — Evidence-Based Self-Learning

- **Document role**: normative phase specification
- **Specification version**: 1.0.0
- **Delivery status**: complete
- **Owner**: mini-agent maintainers
- **Last reconciled**: 2026-08-01
- **Entry dependencies**: Foundation and Phases 1–4 complete
- **Exit dependency**: every acceptance criterion below and every Phase 5 blocker
- **Target scale**: up to 100,000 local/shared skill revisions

**Delivers**: directly attributed skill telemetry, evidence-gated canary promotion, automatic
quarantine, immutable repair revisions, supersession, rollback, and bounded audit history.

The corpus authority and conflict rules are defined in
[`00-index.md`](00-index.md). Phase 5 owns evidence-based lifecycle automation. It cannot bypass
Phase 4 verification, held-out evaluation, immutable identity, or required human gates.

Phase 5 is complete. [`phase-6-brokered-js-runtime.md`](phase-6-brokered-js-runtime.md) extends only
its handling of identity-version migration: identity-v1 artifacts become ineligible and
quarantined before Phase 6 execution, rollback cannot reactivate them, and explicit reproposal is
required for identity version 2. Phase 5's evidence thresholds, transactional lifecycle/index
coordination, immutable lineage, repair, rollback mechanics for eligible artifacts, privacy, and
retention remain authoritative. Phase 6 is planned; this extension is not marked delivered here.

---

## 1. Goal and safety boundary

Phase 5 makes the library improve from observed outcomes without allowing an agent-authored
candidate to activate itself. The system learns by creating and evaluating immutable revisions,
not by editing active source in place.

Automatic decisions are deliberately asymmetric:

- Removing a suspect revision from retrieval is reversible and may happen automatically.
- Increasing a revision's authority is higher risk and requires stronger evidence.
- Pure and read-only replacements may eventually promote automatically.
- Revisions that write files, start processes, or use the network always retain a human gate.

No model judgment alone counts as promotion evidence. The durable inputs are verifier results,
held-out cases, directly instrumented invocation outcomes, explicit user feedback, and measured
latency/resource use.

---

## 2. Lifecycle and immutable lineage

```text
pending → verified → canary → active → superseded
    │          │         │        │
    └──────────┴─────────┴────────┴──→ quarantined
                                          │
                                          └── repair proposal → pending

rejected: terminal evaluation failure for one immutable ID; never retrievable or re-evaluated
retired: explicit administrative disable, retained for audit
purged: explicit privacy operation, not a normal lifecycle transition
```

Allowed transitions are implemented in one service and persisted transactionally. Direct SQL or
raw store methods must not bypass verifier, approval, evidence, or rollback gates.

Every replacement stores `supersedes_id`. Successful promotion atomically sets the candidate to
`active` and the predecessor to `superseded`. A lineage must be acyclic and each active revision
may have at most one active successor.

Normal lifecycle operations never delete source, evidence, or predecessor links. An explicit
privacy purge may physically remove data after also removing dependent embeddings, events,
evaluation cases, and index entries.

### Phase 6 identity-v1 migration extension

Before Phase 6 can advertise JS availability, a parent-owned migration runs under the same
exclusive lifecycle/index-generation gate used for quarantine. It removes every identity-v1
artifact from retrievable and canary snapshots before any worker request can name it. A version-1
row in `pending`, `verified`, `canary`, `active`, or `superseded` transitions transactionally to
`quarantined` with an explicit `phase6_identity_v1` reason; an already quarantined row remains so.
`rejected` and `retired` retain their stricter terminal stored labels but are classified as
identity-quarantined by Phase 6 and denied by the same identity-version eligibility gate. No
identity-v1 row may execute, verify as a Phase 6 artifact, receive evidence, promote, or be selected
as a rollback target.

Migration never mutates source, tests, exports, or a flat capability list into version-2 identity
fields, and it never infers a structured scope. Explicit reproposal supplies the complete
identity-v2 ABI and structured capability scopes, creates a new immutable ID in `pending`, and
links the preserved version-1 predecessor through the trusted migration-only Phase 4 exception for
audit without reactivating it. That link provides no fallback or non-inferiority evidence. The new
revision then passes the ordinary Phase 4 verification/admission and Phase 5 evidence gates.

The database status changes, identity-version eligibility gate, desired index generation, and
removal-only snapshot publish are one fail-closed migration unit. Failure leaves Phase 6 JS
unavailable; startup must not expose an old snapshot or retry an identity-v1 artifact in the
historical runtime.

---

## 3. Capability tiers

| Tier | Allowed behavior | Automatic promotion |
|------|------------------|---------------------|
| 0 | Pure computation; no host globals | Eligible after evidence gates |
| 1 | Read-only approved host operations | Eligible after stronger evidence gates |
| 2 | File writes, process spawn, or network | Never; explicit human approval required |
| 3 | Administrative/security-sensitive effects | Not admitted as a learned reusable skill |

The delivered runtime tracked the currently executing skill wrapper. Phase 6 replaces that
attribution mechanism with parent-created invocation/grant bindings and identity-v2 structured
scopes. A brokered host call is allowed only when the normal session permission policy, target
narrowing, and the skill's immutable structured capability manifest all allow it.
Ambient session permission never upgrades a skill. Undeclared capability use is a directly
attributed policy fault and causes immediate quarantine.

The delivered identity-v1 manifest contained both tier and an exact flat allow-list of host
operations. Phase 6 identity v2 replaces that list with exact structured scopes. Tier still
validates the maximum kind of authority and never grants every operation in that tier. Unknown
operations, administrative/security-sensitive hosts, and tier/scope mismatches are unrepresentable
or rejected at admission.

A replacement that increases its capability tier cannot promote automatically, even if all
quality gates pass.

---

## 4. Invocation instrumentation

Each declared export is wrapped after skill source evaluation. The wrapper records:

- skill revision ID and export name;
- start/end monotonic timestamps;
- synchronous return or exception;
- asynchronous fulfillment or rejection when the export returns a Promise;
- timeout, OOM, and capability-policy faults;
- whether the containing model-authored JS step eventually succeeded.

Each wrapper call gets a stable `invocation_id` derived from the durable turn ID, tool-call ID,
skill ID, export, and call ordinal. A retry of the same acknowledged tool call reuses its IDs;
event insertion is idempotent. A genuinely new call gets a new ordinal. `turn_id` is allocated
once when a user prompt starts and survives model/tool retries until that turn settles.

Exception and rejection events persist only the Phase 6 sanitized class, stable code, and
source-free numeric location. They never persist a raw thrown value, message, stack, source
snippet, effect result, prompt/content field, or secret.

The runtime distinguishes these events:

```rust
pub enum SkillEventKind {
    Selected,
    Injected,
    Invoked,
    Returned,
    Threw,
    TimedOut,
    Oom,
    CapabilityDenied,
    UserPositive,
    UserNegative,
}
```

`Selected` and `Injected` do not increment invocation counts. A failure after a skill returned
successfully is recorded as a step failure but is not automatically attributed to that skill.
Only wrapper-observed exceptions, rejections, limits, policy faults, held-out regressions, and
explicit targeted feedback drive automatic quarantine.

A **qualified canary invocation** has one persisted `Invoked` event and exactly one persisted
terminal wrapper outcome for the canary revision, was executed rather than shadow-evaluated, and
has no observability-loss marker. Promotion policy gives at most one evidence unit per skill
revision per user turn, even if model code loops over an export; all calls remain available for
latency and debugging aggregates. Tests, benchmarks, replay, and evaluator runs never count as
production canary evidence.

Explicit feedback counts automatically only when an authenticated user action targets a recorded
invocation or revision. Sentiment inferred from conversation text and whole-turn failure do not
create `UserNegative` evidence.

Raw arguments, file contents, prompts, and model responses are not stored. When correlation is
needed, store a keyed fingerprint and a coarse argument shape after applying configured secret
redaction.

---

## 5. Evidence schema

SQLite remains the durable source of truth. Phase 5 adds append-only event and transition tables
plus compact aggregates.

```sql
CREATE TABLE skill_events (
    event_id          INTEGER PRIMARY KEY AUTOINCREMENT,
    invocation_id     TEXT,
    skill_id          TEXT NOT NULL,
    turn_id           TEXT NOT NULL,
    tool_call_id      TEXT,
    event_kind        TEXT NOT NULL,
    export_name       TEXT,
    outcome           TEXT,
    latency_us        INTEGER,
    retrieval_score  REAL,
    retrieval_rank   INTEGER,
    query_fingerprint TEXT,
    index_generation INTEGER NOT NULL,
    created_at        INTEGER NOT NULL,
    UNIQUE (invocation_id, event_kind)
);

CREATE TABLE skill_evidence (
    evidence_id       TEXT PRIMARY KEY,
    skill_id          TEXT NOT NULL,
    evidence_kind     TEXT NOT NULL,
    payload_json      TEXT NOT NULL,
    policy_version    TEXT NOT NULL,
    created_at        INTEGER NOT NULL
);

CREATE TABLE skill_transitions (
    transition_id     INTEGER PRIMARY KEY AUTOINCREMENT,
    skill_id          TEXT NOT NULL,
    from_status       TEXT NOT NULL,
    to_status         TEXT NOT NULL,
    reason            TEXT NOT NULL,
    evidence_snapshot TEXT NOT NULL,
    policy_version    TEXT NOT NULL,
    created_at        INTEGER NOT NULL
);

CREATE TABLE skill_stats (
    skill_id              TEXT PRIMARY KEY,
    selected_count        INTEGER NOT NULL DEFAULT 0,
    invoked_count         INTEGER NOT NULL DEFAULT 0,
    direct_success_count  INTEGER NOT NULL DEFAULT 0,
    direct_failure_count  INTEGER NOT NULL DEFAULT 0,
    timeout_count         INTEGER NOT NULL DEFAULT 0,
    oom_count             INTEGER NOT NULL DEFAULT 0,
    policy_fault_count    INTEGER NOT NULL DEFAULT 0,
    user_positive_count   INTEGER NOT NULL DEFAULT 0,
    user_negative_count   INTEGER NOT NULL DEFAULT 0,
    latency_total_us      INTEGER NOT NULL DEFAULT 0,
    updated_at            INTEGER NOT NULL
);
```

Foreign keys, lifecycle value checks, and schema versions are required. Evidence snapshots use
canonical JSON so the same decision inputs produce the same audit record.

The delivered Phase 5 implementation batched records off the historical JS thread through a
bounded channel. Phase 6 carries bounded worker-attributed events in the terminal protocol result
and validates them against the parent invocation/grant table before parent-side durable ingestion.
Queue overflow, attribution mismatch, or SQLite failure marks the turn's evidence incomplete; the
user-visible tool result remains valid, but the turn contributes no promotion or rate-based
quarantine evidence.

---

## 6. Initial revision policy

A brand-new skill has no trustworthy production history. It must pass Phase 4 verification and
receive explicit human approval before entering `canary`.

A lineage-root canary has no active representative, so prompt-time retrieval cannot select its
lineage and it cannot accumulate production canary evidence. It remains non-retrievable until a
second authenticated human decision activates it through the Phase 5 transition service. That
decision revalidates the artifact, evaluation report, held-out suites, capability, and row version
but does not fabricate predecessor telemetry or a non-inferiority comparison. The automatic
qualified-invocation gates below apply only to replacement canaries with an active predecessor.

Replacement canary eligibility is deterministic, based on a stable hash of `(skill_id, turn_id)`,
so retries do not switch revisions unpredictably. The canary share is bounded and configurable.
If no active lineage is selected, the model writes ordinary JS using the primitive host API.

Routing occurs after retrieval selects a logical lineage and before the model-visible manifest is
built. The active predecessor is the default revision. A local keyed hash of
`(lineage_root_id, candidate_id, turn_id, policy_version)` maps into the canary share; the same
turn therefore sees the same revision in its manifest and every JS call. Quarantined candidates
are ineligible regardless of hash. Candidate and predecessor are not both injected as competing
near-duplicates.

Conservative replacement-canary defaults:

- maximum canary share: 10% of otherwise eligible turns;
- minimum qualified invocations before `active`: 25;
- evidence must span at least 25 distinct user turns because one revision receives at most one
  promotion evidence unit per turn;
- zero integrity, capability, timeout, or OOM faults;
- no held-out or inherited regression failure;
- direct-call error rate below 5%;
- no unresolved explicit negative user feedback;
- p95 latency within the configured absolute budget.

`direct-call error rate` means terminal wrapper throws/rejections divided by qualified terminal
outcomes; selected-but-unused and later whole-step failures are excluded. p95 uses a documented
nearest-rank calculation over qualified production calls in the decision window. Promotion also
requires a one-sided 95% Wilson upper bound no worse than the predecessor's bound plus the
configured non-inferiority margin. Thus 25 is a floor, not a promise that 25 observations always
suffice. A replacement without enough predecessor data remains canary or requires human review.

Without the Phase 5 policy service, no canary can activate. Thresholds are configuration, but every
automatic decision stores the effective values and policy version.

---

## 7. Replacement policy

A repair proposal must identify its predecessor and add the observed failure as a regression
case. It inherits all predecessor embedded and held-out cases. Tests cannot be removed merely to
make a candidate pass.

An established Tier 0 or Tier 1 replacement may enter canary without a new human decision only
when all conditions hold:

1. The predecessor and evaluation suite have the configured minimum evidence history.
2. Canonical identity, no-effect verification, mutation checks, and held-out cases pass.
3. The replacement requests no additional capability.
4. Shadow evaluation is no worse than the predecessor on every inherited case.
5. A deterministic canary can fall back to the predecessor without replaying completed side
   effects. Automatic fallback is therefore restricted to Tier 0 and explicitly idempotent
   Tier 1 operations.

Promotion from canary to active requires at least 25 qualified candidate invocations, no severe
faults, no regression, a direct error rate below 5%, and p95 latency no worse than 125% of the
predecessor. Implementations should use a confidence-bound comparison once enough samples exist,
not promote from a single observed percentage.

The decision window, distinct-turn count, numerator/denominator, Wilson bounds, latency samples or
histogram version, predecessor comparison, and effective thresholds are stored in the canonical
evidence snapshot. Events outside the configured window or marked incomplete cannot contribute.

Tier 2 replacements always require human approval after all automated checks pass.

---

## 8. Automatic quarantine

Quarantine is immediate for:

- identity version 1 when Phase 6 migration/eligibility is active;
- canonical identity or stored-content mismatch;
- undeclared capability use;
- sandbox or permission-policy violation;
- held-out regression discovered after admission;
- any timeout/OOM during canary;
- corrupted embedding/model metadata that makes the revision unsafe to retrieve.
- explicit strong negative user feedback targeted at a canary invocation or revision.

Behavioral quarantine of an active revision requires directly attributed failures and a minimum
sample window. The initial policy requires at least 20 invocations and at least 5 directly
attributed failures before rate-based quarantine. Thresholds are configurable and versioned.

A targeted report of an integrity, permission, or unsafe-effect problem is severe and may
quarantine an active revision immediately. Ordinary “wrong result” feedback enters the behavioral
window unless a human explicitly marks it severe. Model-generated feedback never triggers an
automatic transition.

Quarantine uses the index coordinator's exclusive generation gate. It commits status, evidence,
and a desired index generation in one SQLite transaction, then publishes a new immutable snapshot
before releasing the gate. New-turn retrieval acquires the shared side of this gate and therefore
cannot observe a post-transition generation with a pre-transition candidate set. Already-frozen
turn bundles may finish.

If a full rebuild fails after commit, the coordinator publishes an emergency snapshot derived
from the prior one with all newly ineligible IDs removed. Additions remain unavailable until a
verified full rebuild succeeds. The database records desired and applied generations so startup
and background repair can catch up without re-enabling removed code.

---

## 9. Repair protocol

Quarantine creates a repair record containing only the evidence needed to reproduce the fault:

- failing revision ID and export;
- sanitized argument shape or deterministic fixture;
- sanitized exception/rejection class, stable code, source-free location, timeout, or policy result;
- expected behavior when known;
- inherited regression case IDs;
- retrieval query fingerprint, score, and index generation.

The agent may use this record to call `propose_skill` with `supersedes_id`. The proposal is a new
immutable artifact and traverses the same Phase 4 gates. A failed repair leaves the predecessor
quarantined and preserved. It never reactivates broken code merely because a repair attempt failed.

Repair attempts have per-session and per-lineage limits. Repeated failures surface for human
review rather than looping indefinitely.

---

## 10. Supersession and rollback

Successful replacement promotion is one transaction:

1. Revalidate candidate and predecessor row versions and identities.
2. Set candidate to `active`.
3. Set predecessor to `superseded` and link both directions.
4. Persist the evidence snapshot and policy version.
5. Increment index generation.

Rollback is also one transaction:

1. Quarantine the replacement with the rollback reason.
2. Reactivate the exact predecessor revision only if it is eligible under the current identity
   version; identity v1 is never eligible under Phase 6.
3. Persist the transition/evidence record.
4. Increment index generation and atomically publish the rebuilt snapshot.

Missing or identity-ineligible predecessors, lineage cycles, stale row versions, or transaction
failures leave every status unchanged and return a typed error. When no eligible predecessor
exists, the lineage remains unavailable rather than falling back to identity v1. Rollback does not
delete the failed replacement.

---

## 11. Retention, privacy, and compaction

The target scale applies to active and retained revisions, not unlimited raw telemetry.

- Raw `skill_events` use a configurable retention window, initially 30 days.
- `skill_stats`, transition records, explicit feedback, evaluator results, and lineage are retained.
- Compaction is transactional and idempotent: aggregate raw events into versioned daily buckets
  before deleting them. A durable watermark prevents double counting after restart.
- Query fingerprints use a local keyed hash; no raw prompt is persisted.
- Argument values and file contents are never stored by default.
- Secret redaction runs before any repair fixture or evidence payload is persisted.
- An explicit privacy purge removes the artifact and all dependent data and records a non-secret
  tombstone so stale indexes cannot resurrect the ID.

Key rotation changes only future correlation fingerprints; evidence snapshots retain opaque old
fingerprints but cannot be reversed. Repair records derived from user data must pass redaction and
fixed size limits; uncertain or value-bearing fixtures require human approval before persistence.
Retention and purge workers share the lifecycle/index coordinator so compaction cannot race a
transition or leave referentially invalid snapshots.

Semantic duplicate compaction never deletes the rollback chain. Near-duplicates are either linked
as alternatives/supersession candidates or retired after review.

---

## 12. Failure semantics

- Telemetry write failure must not turn a successful JS result into a failed user task, but it
  disables automatic promotion/quarantine for the affected turn and emits an operational error.
- Lifecycle and index-generation changes are fail-closed and transactional.
- Identity-v1 migration or eligibility-check failure keeps Phase 6 JS unavailable and cannot
  restore a version-1 revision to retrieval.
- Embedding or index unavailability returns an empty learned-skill bundle plus an operational
  diagnostic; primitive JS remains available and no unscored skill is injected.
- A policy evaluator panic/error cannot promote a skill.
- Stale or missing evidence cannot promote a skill.
- Quarantine and rollback are idempotent.
- Automatic decisions never call `unwrap`, silently ignore a store error, or mutate source/tests.

---

## 13. Acceptance criteria

All must pass under `cargo test --features skills` and `cargo test --features js,skills`:

- [x] Selected-but-unused, invoked-success, invoked-throw, Promise rejection, timeout, OOM, and
      capability denial produce distinguishable events and aggregates.
- [x] Invocation/event retries are idempotent; looping calls cannot contribute more than one
      promotion evidence unit per revision and user turn.
- [x] Raw prompts, arguments, file contents, and known secret fixtures never appear in telemetry.
- [x] Initial revisions require human approval before canary.
- [x] A lineage-root canary is non-retrievable and reaches active only through a second explicit
      human decision; predecessor/non-inferiority evidence is never fabricated for it.
- [x] Tier 0/1 replacements auto-enter canary only with sufficient inherited and held-out evidence.
- [x] Canary routing is deterministic per turn/lineage/policy, occurs before the manifest, never
      injects both candidate and predecessor, and excludes quarantined revisions.
- [x] Tier 2 replacements cannot auto-promote under any evidence configuration.
- [x] Integrity, capability, held-out, canary timeout, and canary OOM faults quarantine immediately.
- [x] Rate-based quarantine respects minimum sample and directly attributed failure requirements.
- [x] Quarantined/superseded/retired revisions are absent from new retrieval snapshots.
- [x] Repair creates a new ID linked to the predecessor and cannot mutate the predecessor.
- [x] Promotion and rollback are atomic under injected transaction failure and concurrent readers.
- [x] A lifecycle transition cannot let any newly starting turn read an older eligibility snapshot;
      failed rebuilds publish a removal-only emergency snapshot and recover by generation.
- [x] Every automatic transition records the exact policy version and evidence snapshot.
- [x] Retention compaction is idempotent and preserves aggregates, lineage, and rollback.
- [x] At 100,000 retained revisions, lifecycle refresh, canary routing, event batching, compaction,
      and retrieval remain within separately documented latency/memory budgets.

Named validation targets:

```bash
cargo test --features js,skills skill_event_attribution
cargo test --features js,skills evidence_promotion_policy
cargo test --features js,skills skill_quarantine_policy
cargo test --features js,skills skill_repair_and_rollback
cargo test --features js,skills skill_telemetry_retention
cargo test --features js,skills
cargo test --features js
```

Before committing, run `cargo fmt`. Do not use `cargo build`, `cargo check`, or `--release`.

---

## 14. Implementation map

Phase 5 production code is split by policy boundary rather than collected into one mutable
manager:

| Boundary | Module |
|----------|--------|
| Schema, typed rows, tombstones | `src/extras/js/skills/store.rs` |
| Lifecycle, root activation, supersession, rollback | `src/extras/js/skills/lifecycle.rs` |
| Wrapper events and off-thread ingestion | `src/extras/js/engine.rs`, `types.rs`, `skills/telemetry.rs` |
| Capability intersection | `src/extras/js/skills/capability.rs`, `src/extras/js/host.rs` |
| Canary routing | `src/extras/js/skills/router.rs` |
| Promotion evidence and leases | `src/extras/js/skills/policy.rs`, `scheduler.rs` |
| Generation publication | `src/extras/js/skills/coordinator.rs` |
| Feedback, quarantine, and repair | `src/extras/js/skills/feedback.rs`, `quarantine.rs`, `repair.rs` |
| Redaction, retention, and purge | `src/extras/js/skills/privacy.rs`, `retention.rs` |

This map is descriptive only. Phase completion still requires every acceptance criterion, entry
dependency, named test, real-binary smoke, performance gate, and Beads child/audit closure.

---

## 15. Out of scope

- Fleet-wide or Internet-shared skill synchronization.
- Fully autonomous promotion for write/process/network capabilities.
- Model-only quality judgments as durable evidence.
- Online mutation of active source.
- Additional ANN backends or distributed/shared indexes beyond Phase 3's immutable local HNSW
  generation.
