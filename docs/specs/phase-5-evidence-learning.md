# Phase 5 — Evidence-Based Self-Learning

- **Document role**: normative phase specification
- **Specification version**: 1.5.0
- **Delivery status**: delivered
- **Owner**: mini-agent maintainers
- **Last reconciled**: 2026-09-08
- **Entry dependencies**: Foundation and Phases 1–4 complete
- **Exit dependency**: every acceptance criterion below and every Phase 5 blocker
- **Target scale**: up to 100,000 local/shared skill revisions

**Delivers**: directly attributed skill telemetry, evidence-gated canary promotion, automatic
quarantine, immutable repair revisions, supersession, rollback, and bounded audit history.

**Shipped-binary status**: retrieval, directly attributed telemetry, and the proposal/admission
workers are wired in skills-enabled builds. The workers start only when trusted
`enable_skill_proposals = true` configuration opts in. Local-owner commands expose stats, targeted
feedback, retention compaction, privacy purge, import, approval/rejection, explicit root
activation (`--activate-learned-skill`), explicit replacement promotion
(`--promote-learned-skill`), and administrative retirement (`--retire-learned-skill`).

Not everything this specification describes is reachable from the shipped binary. Reading this
document as a description of running behaviour requires the following corrections:

- **Promotion** is reachable, but only as an *explicit operator* action. The automatic
  evidence-threshold promotion path (`LifecycleService::promote_replacement` under
  `PromotionPolicy`) has no production caller; `--promote-learned-skill` uses a separate
  local-owner authorization that records `evidence_threshold_promotion: false` in its evidence
  payload. No shipped code path promotes a canary because its evidence crossed a threshold.
  Its authority variant, transactional adapter, policy evaluator, and evidence readers compile
  only for tests; production task-outcome recording and validation remain enabled.
- **Rollback** (`rollback_replacement`) is a library operation with no production caller and no
  operator command. Its transactional adapters compile only for tests. The shared replacement
  transaction remains in production for explicit local-owner promotion.
- **Repair** (`skills/repair.rs`) is compiled `#[cfg(test)]`. Repair-record construction and
  repair-proposal submission exist only in the verification suite.
- **The evidence-decision scheduler** (`skills/scheduler.rs`) is also compiled `#[cfg(test)]`:
  nothing in production enqueues a held decision, so the module is deliberately kept out of the
  production build. The `skill_decision_jobs` table it leases is still created by the schema
  migrations and stays empty; a held automatic quarantine is logged and dropped rather than
  queued.
- **Automatic quarantine** *is* wired, on the telemetry ingestion path, together with the
  index-rebuild and retention-compaction background work.

The corpus authority and conflict rules are defined in
[`00-index.md`](00-index.md). Phase 5 owns evidence-based lifecycle automation. It cannot bypass
Phase 4 verification, held-out evaluation, immutable identity, or required human gates.

Phase 5 is complete. [`phase-6-brokered-js-runtime.md`](phase-6-brokered-js-runtime.md) extends only
its handling of identity-version migration: identity-v1 artifacts become ineligible and
quarantined before Phase 6 execution, rollback cannot reactivate them, and explicit reproposal is
required for identity version 2. Phase 5's evidence thresholds, transactional lifecycle/index
coordination, immutable lineage, repair, rollback mechanics for eligible artifacts, privacy, and
retention remain authoritative. Phase 6's identity-v1 quarantine and identity-v2 eligibility
extension is implemented on the current retrieval path. The bounded local-owner adapter exposes
only the authenticated operations listed above and does not bypass evidence or human gates.

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
The generic transaction primitive rejects entry into `canary` or `active`, and rejects
`active → superseded`; those edges require their dedicated admission, activation, or replacement
service. Production quarantine calls the primitive inside its own evidence transaction. The
standalone generic transaction adapter exists only for tests; there is no generic coordinated
publication wrapper.

Every replacement stores `supersedes_id`. Successful promotion atomically sets the candidate to
`active` and the predecessor to `superseded`. A lineage must be acyclic and each active revision
may have at most one active successor. Multiple replacement canaries may collect evidence against
the same active predecessor. Candidate selection first favors the canary with the fewest canonical
production invocations, then breaks ties by creation time and immutable ID, so siblings cannot be
starved and scheduling remains deterministic.

Normal lifecycle operations never delete source, evidence, or predecessor links. An explicit
privacy purge may physically remove data after also removing dependent embeddings, events,
evaluation cases, and index entries.

### Phase 6 identity-v1 migration extension

Before JS can advertise Phase 6 availability, a parent-owned migration runs under the same
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

The installed export binding is reusable, but call authority is not. Each genuinely new call asks
the parent for the next ordinal and receives freshly minted, exactly attributed, one-shot
authority. Consuming one call handle cannot consume or lend another call's authority; replay,
expiry, or revocation denies before stored source executes.

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
Limits, policy faults, held-out regressions, and explicit targeted feedback drive automatic
quarantine. A wrapper-observed exception or rejection is telemetry, but it counts as a behavioral
fault only when active authenticated negative or severe feedback targets that exact invocation;
caller input mistakes therefore cannot quarantine a correct skill by themselves.

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
quarantine evidence. The asynchronous ingestion worker tracks incomplete turns independently
of the parent's enqueue-time snapshot. A failed event write invalidates earlier task outcomes
for that turn and marks subsequent outcomes and retries incomplete, without excluding healthy
turns elsewhere in the session. This also keeps missing invocation links from being interpreted
as a verified no-library baseline. Schema 15 persists these signals in `skill_turn_losses`,
keyed by turn and production context, so invocation promotion and behavioral windows also
exclude the entire incomplete turn after a restart. Original event rows remain immutable and
exact retries still match. Explicit incomplete events and outcomes record the same loss;
migration backfills existing incomplete rows and `observability_lost` events. Complete direct
safety faults retain their immediate-quarantine path.

Loss records contain only turn ID, production context, and last loss time. Raw-event compaction
removes an aged loss record only when no event or task outcome still references its turn.
Privacy purge removes losses owned exclusively by the purged revision and retains shared-turn
losses needed to keep surviving evidence excluded.

The brokered effect audit is separate from skill evidence. If an approved effect may have happened
but cannot be classified after cancellation, deadline, or transport failure, the audit records
`OutcomeUnknown`, revokes the invocation, and forces worker recycle. That ambiguous result never
counts as a successful skill outcome, positive evidence, or a reason to replay the effect.

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
so retries do not switch revisions unpredictably. The canary share is bounded by a fixed
constant, not configuration.
If no active lineage is selected, the model writes ordinary JS using the primitive host API.

Routing occurs after retrieval selects a logical lineage and before the model-visible manifest is
built. The active predecessor is the default revision. A local keyed hash of
`(lineage_root_id, candidate_id, turn_id, policy_version)` maps into the canary share; the same
turn therefore sees the same revision in its manifest and every JS call. Quarantined candidates
are ineligible regardless of hash. Candidate and predecessor are not both injected as competing
near-duplicates.

Conservative replacement-canary constants. These are compiled-in values, **not** configuration:
there is no configuration key for any of them. The only skill-related configuration keys are
`embedding` and `enable_skill_proposals` (`src/config/mod.rs`).

- maximum canary share: 10% of otherwise eligible turns
  (`turn.rs`, `CANARY_SHARE_BASIS_POINTS = 1_000`; `router.rs` additionally refuses any request
  above 1,000 basis points, so 10% is a hard ceiling and not merely a default);
- minimum qualified invocations before `active`: 25;
- evidence must span at least 25 distinct user turns because one revision receives at most one
  promotion evidence unit per turn;
- each turn retains its worst direct outcome and maximum invocation latency independently;
  a fast failing call cannot hide a slower successful call from the p95 latency gate;
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

Without the Phase 5 policy service, no canary can activate. The thresholds are the fixed fields of
`PromotionPolicy::conservative` (`skills/policy.rs`): `min_distinct_turns: 25`,
`max_observed_error_rate: 0.05`, `non_inferiority_margin: 0.05`,
`max_candidate_latency_ratio: 1.25`, `absolute_p95_latency_us: 5_000_000`, and
`min_verified_task_passes: None`. `PromotionPolicy` is a serializable versioned struct, so a
different policy *can* be constructed and persisted in code, but the shipped binary registers only
the conservative constants (from root activation and from quarantine) and exposes no configuration
key to change them. Every automatic decision still stores the effective values and the policy
version.

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
5. The route is frozen before any effect runs, so a canary *could* be abandoned for the
   predecessor without replaying completed side effects, and eligibility for that is restricted to
   Tier 0 and explicitly idempotent Tier 1 operations. **Automatic fallback is not implemented.**
   `router.rs` computes and freezes the route (`FrozenRoute`, including
   `fallback_before_effects`) and `turn.rs` emits a canary-exposure audit record for every turn
   that had an eligible candidate. This is eligibility metadata only: nothing in `JsTool` or
   the worker re-invokes the predecessor after a canary failure. A
   failed canary invocation fails the step; it does not silently retry on the active revision.

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
- authenticated `severe` user feedback, carrying one of the enumerated safety reason codes,
  targeted at an active or canary revision.

Behavioral quarantine of an active revision requires directly attributed faults and a minimum
sample window. Timeouts, OOMs, and capability denials are faults on their own. An invocation that
**threw or returned** counts as a fault when active authenticated `negative` *or* `severe`
feedback targets that exact invocation — so ordinary “wrong result” feedback on a successful
return does enter the behavioral window. The policy requires at least 20 qualified invocations and
at least 5 directly attributed faults in the window before rate-based quarantine. The window is the
raw-event retention window, and the counts are the fixed `QuarantinePolicy::conservative`
constants under policy version `phase5-quarantine-v1`; they are versioned but not configurable.

`severe` is a distinct feedback *kind*, not an escalation flag on wrong-result feedback. It is
restricted to the enumerated safety reason codes `integrity`, `permission_violation`, and
`unsafe_effect` (`skills/feedback.rs`, `SEVERE_FEEDBACK_REASON_CODES`), and submitting it against
an active or canary revision immediately enters the coordinator-backed quarantine transition. A
human therefore cannot mark wrong-result feedback severe and cannot use it to quarantine a
revision on the spot; the supported route for a revision that returns wrong data is `negative`
feedback attributed to the exact invocation, which counts toward the behavioral window above. If
the wrong result is itself a safety problem — corrupted or fabricated data, an effect outside the
declared grants — the matching safety reason code applies and quarantine is immediate.

Rate-based quarantine is evaluated on telemetry ingestion, over the skills named by terminal
production events in the arriving batch. Submitting feedback does not itself re-evaluate the
window: a revision that has crossed the fault threshold is quarantined the next time one of its
invocations reports a terminal event. Operators who need containment now should use
`--retire-learned-skill` (administrative disable, lineage preserved) or a severe safety report.

Model-generated feedback never triggers an automatic transition.

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

**Delivery status of this section**: `skills/repair.rs` is compiled `#[cfg(test)]`. Repair-record
construction and repair-proposal submission exist only in the verification suite; no shipped code
path builds a repair record from a quarantine, and no operator command emits one. The contract
above is the design this module implements and is regression-tested against; it is not something a
running binary does today.

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

### Delivery status and the operator surface

Supersession is reachable; rollback is not.

- `--promote-learned-skill <sha256>` performs the supersession transaction above as an explicit
  local-owner action. It requires an approved **canary** whose proposal names a predecessor and
  whose `supersedes_id` matches that predecessor. A lineage root is refused with a message pointing
  at `--activate-learned-skill`; a candidate already `active` is reported as an idempotent no-op;
  any other status is refused. The predecessor may be `active` (an ordinary replacement) or
  `quarantined` (the emergency path), and any other predecessor status is refused. Lineage is
  preserved rather than re-rooted, and the attempt is keyed on the observed candidate and
  predecessor row versions and the index generation, so a retry over the same observation replays
  exactly instead of colliding. It registers its own policy version
  (`local-owner-operator-promotion-v1`) and records `evidence_threshold_promotion: false`: it is an
  authenticated human decision, not an evidence-threshold promotion.
- `--activate-learned-skill <sha256>` accepts **only** an approved lineage-root canary. A candidate
  whose proposal carries a `predecessor_id` is refused with a message pointing at
  `--promote-learned-skill`; an already-`active` revision is an idempotent no-op; any other status
  is refused.
- `--retire-learned-skill <sha256>` is the administrative disable named in section 2. It requires an
  `active` revision, is idempotent once retired, keeps the revision, its lineage, and its audit, and
  publishes through the same coordinated gate. Unlike `--purge-learned-skill` it deletes nothing.
- **Rollback has no operator command and no production caller.** `rollback_replacement` is a tested
  library operation only.

Because rollback is unreachable, the emergency procedure for a defective **active** revision is
promotion over a quarantined predecessor, not reactivation of one:

1. Contain the defective revision. Either submit severe feedback with a safety reason code
   (`--learned-skill-feedback <id> --learned-skill-feedback-kind severe
   --learned-skill-feedback-reason integrity|permission_violation|unsafe_effect …`), which
   quarantines an active or canary revision immediately, or use `--retire-learned-skill` when the
   problem is not a safety fault. Note that quarantining the *predecessor* is what makes step 3
   legal; retirement is not an accepted predecessor state for promotion.
2. Get a corrected replacement to approved canary through the ordinary gates: import or propose it
   naming the defective revision as its predecessor, then `--approve-learned-skill`.
3. `--promote-learned-skill <replacement>`. The quarantined predecessor is superseded and the
   replacement becomes active with lineage intact.

If no replacement exists, the lineage stays unavailable: there is no shipped command that
reactivates an earlier revision. `--purge-learned-skill` is not a substitute — it deletes the
revision's bytes and re-roots any dependent replacement, which strips that replacement of its
replacement evidence.

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
  Feedback explanations and repair text redact bare and quoted credential assignments,
  including literal or Unicode-escaped JSON credential keys, spaces and escaped quotes inside
  values, YAML doubled single quotes (also after literal backslashes), and unfinished quoted values.
  Complete quoted assignments retain their surrounding syntax. Configured exact-secret
  substitutions run after shape detection so they cannot hide a credential label or split its
  value; UTF-8 truncation happens after all redaction.
- An explicit privacy purge removes the artifact and all dependent data and records a non-secret
  tombstone so stale indexes cannot resurrect the ID.

The shipped binary runs 30-day raw-event compaction automatically after successful telemetry
ingestion. Operators can also force the same idempotent pass with
`mini-agent --compact-learned-skill-events`. A local operator can irreversibly purge one exact
identity-v2 revision through `mini-agent --purge-learned-skill <64-character-sha256>`; this command
uses the lifecycle/index coordinator and publishes removal before reporting success. Neither
command initializes a model provider, and neither prints artifact source or telemetry payloads.

Targeted local-owner feedback is available without model initialization through
`--learned-skill-feedback <sha256>`, together with the required `--learned-skill-feedback-kind`,
`--learned-skill-feedback-reason`, and `--learned-skill-feedback-key` fields and the optional
`--learned-skill-feedback-invocation`. The idempotency key makes retries exact. Severe feedback is
restricted to the enumerated safety reason codes and immediately enters the coordinator-backed
quarantine transition for an active or canary revision.

The learned-skill proposal pipeline now ships, opt-in and off by default. Trusted
`enable_skill_proposals = true` configuration registers the `propose_skill` global and starts the
bounded proposal/admission workers; without it the global is absent and no worker runs. The
complete operator workflow that this was gated on now exists — listing, per-proposal outcome,
approval, rejection, root activation, replacement promotion, and retirement — so a proposal is no
longer written into a queue nothing can act on. Proposals still stop at `awaiting_approval`; the
flag grants no authority to activate.

Repair-record construction and repair-proposal submission remain the exception: `skills/repair.rs`
is compiled `#[cfg(test)]` and is exercised only by the verification suite. That surface stays
test-only until a shipped path produces repair records from quarantines.

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

## 12a. Delivered amendments (2026-09-05)

Accepted by the [2026-09-05 harness design review](../plans/2026-09-05-001-harness-design-review.md).

1. **Fault-only quarantine** (mini-agent-lugc, delivered). Behavioral quarantine counts `timed_out`, `oom`,
   and `capability_denied`. A `threw` event counts only when active authenticated negative or severe
   feedback targets that exact invocation; an uncorroborated exception caused by caller input is
   telemetry, not evidence against the revision. (Widened on 2026-09-07 — see section 12c — so a
   `returned` event with the same attributed feedback also counts.)
2. **Canary ordering** (mini-agent-840z, delivered). When several canaries supersede one active revision,
   routing selects by age and observed invocation count, never by lexicographic identity.
3. **Store concurrency** (mini-agent-pwf2, delivered). The store opens with WAL journaling and a busy
   timeout; every read-modify-write transaction begins `IMMEDIATE`. The telemetry worker disables
   SQLite's native busy timeout on its own connection and retries both event batches and task
   outcomes with capped backoff, preserving FIFO attribution. Shutdown gives the entire ingestion
   queue one 1.5-second flush budget, checked before each attempt; evidence discarded after that
   deadline increments the observability-loss counter. This bounds writer-contention retries, not
   the execution time of an already-running transaction or subsequent quarantine/index work.
   Contention tests use the production connection configuration and real competing writers to
   cover ordinary recovery, transient shutdown contention, and a locked queue tail.
4. **Corrupt rows** (mini-agent-jj8b, delivered). A row whose embedding cannot be decoded is
   skipped and reported like a malformed artifact row; it never darkens the index or triggers a
   rebuild on every turn, and repeated rebuild failures back off.
5. **Canary embeddings** (mini-agent-c8q6, delivered). Rebuilds backfill canary rows so an embedding model
   change cannot silently un-route them.

## 12b. Delivered task-outcome and lifecycle hardening (2026-09-06)

1. **Task outcomes.** Schema version 11 added one bounded outcome row for a completed turn with the
   exact invoked learned-skill IDs, pass/fail, attempt, timestamp, and a closed source:
   `verify_command` plus its SHA-256 identity, an evaluator `oracle` ID, explicit
   `no_verify_command`, or `gate_skipped` for a turn that ran under a configured command without
   touching the workspace. Production is derived by the session constructor; setting
   `MINI_AGENT_GYM=1` can only downgrade it. Gym and deterministic-evaluation evidence is therefore
   retained for analysis but cannot qualify for production promotion, and neither
   `no_verify_command` nor `gate_skipped` can become a pass or a failure. (The current store schema
   is version 15; migration 11 → 12 widened the deferred proposal reason-code CHECK, 12 → 13
   widened the task-outcome source CHECK to admit `gate_skipped`, and 13 → 14 added task-outcome
   completeness; 14 → 15 added durable turn-loss records and indexed turn lookups.)
2. **Promotion gate.** When a policy version sets `min_verified_task_passes`, promotion counts
   distinct production turns in the window where the candidate was actually invoked, task
   evidence is complete, and a real verifier/oracle passed. Durable promotion loading preserves
   the stored completeness flag; incomplete task passes and failures never qualify. That policy
   cannot fall through to the historical 25-invocation path.
   Verification failure is a signal for review and statistics, never an automatic quarantine
   trigger; behavioral quarantine remains fault-only.
3. **Operator utility.** `--learned-skill-stats` reports `tasks_with`, `passed_with`, and
   `pass_rate_without`. The baseline is matched by verifier-command hash or oracle ID and includes
   only outcomes where the compared skill was absent, so unlike arms are not silently combined.
4. **Lifecycle safety.** Rollback clears the reactivated predecessor's stale
   `superseded_by_id`; replacement capability scopes must be a true subset of predecessor scopes;
   and long admission evaluation renews its durable lease before executing the contained suite.

## 12c. Delivered corrections (2026-09-07)

From the 2026-09-07 review. Each bead below is closed with its regression coverage.

1. **Attributed feedback on a successful return** (mini-agent-5mwn). The behavioral-window join in
   `skills/telemetry.rs` credits active `negative` or `severe` feedback on a terminal `returned`
   event, not only on `threw`. Wrong-result feedback against an active revision is therefore no
   longer inert. Section 8 is written against this behaviour.
2. **Operator replacement promotion** (mini-agent-83k9). `--promote-learned-skill` promotes an
   approved replacement canary over an `active` **or** `quarantined` predecessor as an explicit
   local-owner action, preserving lineage. See section 10.
3. **Retirement exposed** (mini-agent-w0zp). `--retire-learned-skill` reaches the previously
   uncalled `retire_and_publish`, giving the administrative disable of section 2 an operator
   command.
4. **Decision scheduler gated out** (mini-agent-fegd). `skills/scheduler.rs` had no production
   caller and was moved behind `#[cfg(test)]` rather than left as apparently-live machinery.
5. **Canary route audited, fallback not implemented** (mini-agent-sdt9). The frozen route is
   recorded on a per-turn canary-exposure audit record; section 7 rule 5 describes eligibility
   metadata only. The unused `may_fallback` predicate was subsequently removed (mini-agent-3l4w).
6. **Purge guard** (mini-agent-zod2). `--purge-learned-skill` refuses a non-terminal target or one
   with dependent revisions unless `--purge-learned-skill-force` is passed, and names every
   revision a forced purge re-roots.

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
  The pair test fails the second transition insert after both revision updates and the generation
  write, then verifies unchanged durable state and exact rollback replay. An overlapping reader
  retains the complete prior snapshot until its read transaction ends. The operator test uses
  that failure point through the production command and verifies owner authority is not consumed.
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
| Worker-attributed terminal events and parent ingestion | `src/extras/js/worker.rs`, `protocol.rs`, `types.rs`, `skills/telemetry.rs` |
| Capability intersection and authoritative attribution | `src/extras/js/skills/capability.rs`, `src/extras/js/broker.rs`, `src/extras/js/tool.rs` |
| Canary routing and per-turn bundle | `src/extras/js/skills/router.rs`, `turn.rs` |
| Promotion evidence | `src/extras/js/skills/policy.rs` |
| Decision leases (**test-only**, `#[cfg(test)]`; no production enqueue) | `src/extras/js/skills/scheduler.rs` |
| Generation publication | `src/extras/js/skills/coordinator.rs` |
| Feedback and quarantine | `src/extras/js/skills/feedback.rs`, `quarantine.rs` |
| Repair (**test-only**, `#[cfg(test)]`) | `src/extras/js/skills/repair.rs` |
| Redaction, retention, and purge | `src/extras/js/skills/privacy.rs`, `retention.rs` |

Modules marked **test-only** are compiled under `#[cfg(test)]` and are not part of the shipped
binary; see the shipped-binary status at the top of this document.

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
