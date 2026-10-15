# Cost ceilings and observed usage

`--max-budget-usd AMOUNT` selects a finite, nonnegative USD ceiling for the
logical session. The flag overrides global `max_budget_usd` configuration;
project configuration cannot set, raise, lower or clear it, even after project
trust is granted. The ceiling is invocation policy and is not saved as a session
setting. A resumed session keeps its recorded spend. A value of zero admits no
model request or model-dispatched effect.

The ceiling uses provider-reported usage and captured configured or catalogue
prices. An admitted request can itself cross the ceiling. Requests and effects
already dispatched retain their cancellation and completion ownership; crossing
the ceiling does not undo their work. This is an estimated-spend admission bound,
not a vendor billing limit or a promise about unreported charges, provider fees,
routing changes or price accuracy.

After observed spend reaches the ceiling, required model continuations, format
repairs and new model-dispatched effects are refused. Headless mode exits nonzero
and reports `stop_reason: "budget"` in JSON/NDJSON. A completed valid primary
answer stays completed when an optional session title is skipped. An invalid
schema answer stays invalid if its repair cannot be admitted. Interactive mode
shows a budget notice and retains queued requests and any recoverable input draft.
Explicit operator commands keep their existing permission and confirmation policy.

`/cost` reads the private ledger without warming a provider, starting a model,
saving a session or changing history. It shows session spend by requested
provider/model, input, cached-input, cache-write and output tokens, today in UTC,
and all-time observations. Display identities are sanitized and bounded. At most
32 model detail rows are displayed; aggregate totals still cover every valid
stored receipt. Recorded spend includes newly observed receipts before disk
persistence. `/cost` labels pending receipts and observations made during its
scan; its ledger subtotals remain a read-only persistence snapshot, without
adding unpersisted charges to durable totals.

Omitted or malformed prices are unknown, rather than free. Explicit zero rates
are known free rates. Native Anthropic cache prices are captured separately, so a
free base price can still have a paid cache tier. Capped model admission requires
complete known base pricing. Missing metrics on a paid response, an unread SDK
response after an invalid tool, or an opened stream cancelled before its canonical
final response leaves tracking incomplete and blocks capped continuation. Usage
actually observed before validation failure or cancellation is retained. The SDK
normalizes absent metrics and explicitly reported all-zero metrics to the same
zero usage value. For positive or unknown rates both cases are conservatively
unreported: the row carries `usage_reported: false` and `cost: null`, and future
capped work is blocked. Explicit known free rates remain free; no paid zero-dollar
observation or unreported token quantity is inferred.

The financial account is monotonic across live clones, rewind and resume. A saved
checkpoint is reconciled with matching-session ledger observations without
charging an old receipt twice. Automatic session forks retain incomplete/unpriced
flags and the labeled estimate; rekeying never mints old receipts or clears uncertainty. Legacy or imported totals can seed only a labeled
historical estimate; they never become ledger rows. If that checkpoint is manually
removed, the legacy estimate and modern ledger rows are conservatively combined,
which can overlap. Imported history and user-edited token totals never mint
observations.

`--no-session` still records newly observed usage in the local usage ledger, but
saves no session or goal history. Its account is ephemeral: if an SDK response
cannot be observed, its in-memory incomplete flag disappears when that logical
session ends. A later ledger-only report cannot infer the missing invoice amount.
Ledger totals cover stored observations and are exact only for complete, priced
observations; they are not an invoice reconciliation.

## Runtime ownership

`src/spend.rs` owns immutable parent-minted receipts, captured pricing, and the
shared logical-session account. `src/spend/run.rs` carries an explicit request
profile through owned async tasks and exact SDK tool extensions. Primary, repair,
subagent, side-question, advisor, title, judge, compaction and generated Git-proposal
requests use the same account with their own captured requested model and prices.
No process-wide mutable request registry is used. Each SDK invocation retains a minted
request guard in its own hook scratchpad. The HTTP adapter captures that guard
before dispatch and marks only a successful completion response as opened. A
canonical accepted usage report is charged before tool/output waits; later SDK
projections reconcile against that same request's receipt and raw usage credit.
Large debug runner and headless title children use short `heap_future` factories
inside their original accounting and work scopes. These boundaries preserve the
native stack limit and the same request ownership.
Body teardown alone never signals missing usage. Retirement of an opened request
without a canonical report does. Concurrent child requests use distinct captured
profiles; an overlapping request on the exact same profile is refused rather than
matched by response arrival order. A mid-turn compaction restart reattaches the
logical session account and captured current model/prices before request and
effect admission; neither text relief nor a rebuilt agent resets recorded spend.

The ordinary tool concurrency wrapper rechecks effect admission after obtaining
its lane. Permission replies are checked again before installing a session grant.
The JS broker captures the parent account and rechecks before preparation and after
approval/audit-lock waits, before durable effect intent. This adds no worker
financial authority and leaves the canonical Phase 6 containment checklist intact.

Tool-free judge and compaction requests consume one raw completion stream; canonical
final usage is observed before rejecting empty output or unexpected tools. Title
and Git-proposal streams use the same owned raw-response accounting contract.
Primary/repair SDK invalid-tool failures can reject a tool before consuming its
later usage chunk. That path persists incomplete tracking without inventing tokens
or a zero-dollar receipt. The original SDK hook and tool-ID behavior is retained.

`src/spend/ledger.rs` writes new receipts to `<state_dir>/usage.jsonl`. Rows include
UTC timestamp, receipt and session IDs, requested provider/model, request kind,
normalized usage, whether metrics were reported, captured effective prices and an
optional cost. No prompt, answer, tool arguments, source text or credential is
recorded. This is local accounting; it sends no analytics request.

Receipt charging is synchronous memory work; bounded scans, serialization locks,
appends and fsync run in blocking children owned by the request's work scope.
Run construction opens no ledger. Initial reconciliation is awaited before model
admission and before an implicit Git proposal collects context, including after a
saved session is restored; capped effects refuse outstanding persistence without inventing a
permanent error for a temporary wait. Cancellation can poll while a disk operation
is held. Released writers are joined; a forced headless stop uses the existing
grace/second-signal policy and saves observed history/usage with a conservative
incomplete checkpoint when persistence is still outstanding. Read-only `/cost`
scans have the same owned blocking boundary.

Existing ledger files are opened without permission repair. Validation requires
current ownership, a regular single-link file and Unix mode 0600 or a protected
Windows DACL granting only the current user and SYSTEM full access. A retained
directory capability and descriptor identity guard each read and append. Appends
use nonblocking exclusive lock attempts. Only typed lock contention is retried,
with a one-second total wait and at most ten milliseconds between attempts in
the existing owned blocking writer. This contention budget is below the existing
five-second actor retirement grace; it does not promise a filesystem I/O deadline.
Each attempt revalidates the retained file and directory. An expired wait or
other I/O/identity failure leaves the charge and pending receipt intact and
tracking incomplete. Summaries retain their one-shot nonblocking shared lock.
Read-only queries never create, repair, quarantine or rewrite storage.

Startup reconciliation of a resumed or cost-capped session reads a private
checkpoint sidecar, `<state_dir>/usage.checkpoint`, so it scans only the rows
appended since the previous startup. The checkpoint holds the covered byte
offset and record count, the ledger file's identity (device, inode, creation
time), the SHA-256 of the covered prefix, every covered receipt ID (for
duplicate detection), the all-time totals (overflow bounds) and each session's
accumulators, with costs stored as exact IEEE-754 bits, all sealed by a trailing
SHA-256. It is used only when the ledger's identity matches, its length is at
least the offset and the streamed SHA-256 of the prefix still equals the recorded
digest (one hashing pass instead of parsing and validating every row); a missing,
oversized, corrupt or mismatching checkpoint (rotation, replacement, truncation,
any in-place rewrite of the prefix) falls back to the full scan. The tail is then checked exactly as a full scan checks
every row, so the session totals, duplicate detection and overflow bounds equal
a full scan's. After a successful scan that read new rows, startup atomically
replaces the checkpoint while it still holds the shared ledger lock; this is the
only write a summary performs, it never touches `usage.jsonl`, and a failed write
only means the next startup scans more. Ledgers with more than 4096 sessions are
scanned exactly but not checkpointed. `/cost` summaries and appends still scan
the full file.

Acquired ledger locks are explicitly released before returning a summary or
acknowledging persisted receipts. Descriptor close alone can leave a Unix lock
held by an already-forked child until exec, even with CLOEXEC. Error and unwind
paths also attempt explicit release; an unlock failure cannot acknowledge a
receipt or return a successful summary. Append acknowledgement still follows
fsync and retained-identity validation. This changes neither the contention
budget nor the owned blocking-I/O boundary.

Reads are limited to 64 MiB, 65,536 records and 4 KiB per record. Pending receipts
are limited to 1,024; detailed model aggregates to 128 identities. Receipt token
counts are capped at 10^15 per field and cost/aggregate amounts at 10^18 USD.
Corrupt rows, duplicate IDs, crash tails, identity conflicts and capacity failures
fail closed for a capped session. Without a cap they produce a tracking warning;
observed spend and valid pending receipts remain in memory. Repeated saves and
receipt retries cannot charge or append the same receipt twice.

New receipts use version 2 to retain a reported native five-minute/one-hour
cache-write split and a separately captured one-hour rate. The one-hour fallback
inherits twice the selected base input price unless `cache_write_1h_token_cost`
explicitly overrides it; an explicit five-minute write override is
never scaled to infer it. Native input excludes cache tiers. Positive-base
equivalent-input rounding happens once over the combined tier pool; a free base
still charges explicitly paid cache tiers directly. Legacy version-1 rows retain
their original pricing interpretation and reject version-2 metadata. Historical
rows are not rewritten or repriced. A required split that is absent leaves the
observation unpriced and blocks subsequent capped work while retaining a valid
completed answer and its metrics. Tier metadata alone is not proof of reported
nonzero usage. Request TTL selection and cache-prefix placement are separate
controls. The owner global `cache_ttl="1h"` selects the native policy, captured
with every ordinary/auxiliary request. Positive writes missing a required split
add no fabricated Session/headless/judge cost; the account and ledger retain an
incomplete/unpriced observation. Default five-minute and version-1 estimates
keep their existing interpretation. Raw cache controls or transcript overrides
also require a split because the host cannot attest their effective write TTL.
Task proxy units use reported tiers or the larger captured write ratio when a
required split is missing; these conservative units are not dollar quotes.

`/cost` reports known write tiers and the latest recorded model request's
uncached/cached input. Old positive write totals cannot acquire a fabricated TTL
breakdown after reload or subsequent reported calls. Reads do not rewrite rows.

`src/spend/report.rs` owns the bounded read-only projection.
`src/spend/request_tests.rs` owns fake native HTTP fixtures for raw auxiliary
accounting, cancellation, SDK invalid-tool failures, captured prices and refusal
of subsequent requests. Production credentials and paid endpoints are not used.
`src/tests/ledger_lock_tests.rs` owns Unix pre-exec controls against the actual
private ledger: a child retains an inherited descriptor while successful and
corrupt-tail reader/writer operations retire. The controls check immediate lock
release, unchanged corrupt bytes, exact receipts and idempotent persistence.
The native Windows default CI job runs the existing idempotent persistence,
corrupt-tail and bounded contention controls before its process-lifecycle
fixtures. These exercise the platform's reader/writer lock contract separately
from the Unix inherited-descriptor witness.

The status bar's `calls` counter is display metadata on the same session account.
Only deduplicated accepted receipts with a canonical usage report increment it,
including free and auxiliary model requests. It does not change prices, admission,
receipt IDs, or token/cost arithmetic. Older checkpoints default to incomplete
call history; unreported requests or tracking failures do not manufacture an exact
count. Canonical unpriced reports still count while live; persisted tracking
uncertainty is restored conservatively as a lower bound. Existing
ledger reconciliation supplies a lower bound from reported rows, and a new
logical-session key retains the historical count and its completeness marker.
Neither rendering nor resizing opens the ledger or performs provider work.
