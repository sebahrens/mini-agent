# Native replay checkpoint publication

Native replay dispatch requires a durable in-flight checkpoint before any
owned hook, model request or effect. The saved denial keeps the same replay
cutoff; the exact live actor retains its healthy binding. Imported data never
reconstructs a writer permit.

`session::native_publication` associates each snapshot with a value-carried
revision and an exact retained session writer/frontier. A stale clone cannot
refresh that revision from the shared cell. All native-bearing ordinary saves
must use this publisher; active-turn saves remain denied on disk. File work
runs inside an owned blocking child created after its scope is installed.

Only a validated payload from an exactly retired actor can become healthy.
Atomic namespace replacement is the logical publication point; file and
directory synchronization establish durability. If replacement succeeds but
later synchronization or forced exit prevents acknowledgment, another reader
may observe that complete retired payload. The live caller remains
conservative on an unacknowledged result and never rolls it back with an older
denied snapshot. A pre-retirement healthy payload is never eligible.

Normal session release revokes writer authority and defers unlock while a
publisher is active. Forced stop does the same for active native publishers
without waiting on filesystem work; ordinary legacy locks still unlock
explicitly. An inherited child descriptor can delay native recovery after
process exit. That delay must remain a visible unfinished-context refusal;
it cannot authorize replay or concurrent publication.

Ordinary headless print now transfers its startup ticket into the actual built
native agent, commits canonical progress and usage once, joins SessionEnd and
publishes its retired boundary. Immediate private reload retains the native
opaque blocks. A failed final write leaves the durable InFlight checkpoint; a
changed workspace, permission or instruction policy refuses replay.

Ordinary interactive turns now use the same strict writer and actual native
binding as plain print. ACP adoption, automatic goal/loop rounds, explicit
recovery, and native schema-repair continuations remain unfinished. Unadopted
loop lanes refuse before startup effects. Native automatic work refuses rather
than silently falling back to an unbound transcript. A valid primary schema answer works; an
invalid native answer cannot enter the generic schema repair path.

The writer retains the private sessions directory and exact lock inode.
Replacing either pathname invalidates its ownership; lexical equality and a
private replacement directory do not authorize a write into the new namespace.
A saved activation marker also restricts later legacy snapshots after the
original process lease is released. It supplies no replay or writer authority.

When session ownership cannot be established, an existing checkpoint is never
replaced. Only a new target may be published through an atomic create-only
operation under a validated parent. A concurrent creator wins unchanged. This
tightens the former warning-and-replace fallback; an unowned saved context must
be resumed under its owner or explicitly used as a fresh ephemeral context.

Current-actor progress saves require its exact mutable ticket. The actor seals
a value revision before queueing a bounded immutable checkpoint, including
canonical history, prices, accounting totals and unknownness. Later changes
to shared task/account owners cannot alter queued bytes. Staging rejects older
clones immediately. Both the awaiting caller and its scoped blocking writer
retain failure fences, including cancellation before child construction and
encoding failures. Only an acknowledged current snapshot can remain active;
ordinary saves cannot upgrade an older snapshot from the shared cell.

This primitive API does not yet establish adoption of every frontend save edge.

The startup carrier retains the preflight ticket and owns the full configured
hook task inside its scope. Transfer waits for that task and its registered
children; it moves the exact ticket into model work without claiming a model
completion. Idle startup and quit remain denied on disk. Capturing startup
ownership does not apply paid-request admission, so an exhausted cap does not
prevent read-only cost display. Fresh explicit ephemeral runs skip publication;
saved native-bearing contexts require persistence. These carrier controls are
not yet proof of adoption at every frontend entry and save edge.

The host workspace projection uses bounded borrowed path encoding and the
already-retained physical directory identity. It validates the live binding
before and after that projection; a same-path replacement refuses. Its fixed
digest is saved compatibility data only and cannot reopen a directory, grant
access, or restore a runtime owner. Controller admission and publication must
revalidate the actual retained owner; these isolated getter controls do not
enable saved opaque replay.

## Paired accepted-refusal publication checkpoint

A native binding can pair once with its exact mutable actor ticket before any
prepared request. The runtime pair retains an additional existing writer guard;
it is absent from serialized data. A cached or imported Session cannot create
that actor relationship. Each real model producer explicitly destroys its model
future before retiring the pair through its cleanup owner. The marker then waits
for the same registered children to join. Waiting on an unrelated empty I/O scope
is insufficient. The paired headless lane adds an auxiliary cleanup owner using
the existing settlement waiter on cancellation; it adds no model execution task
or registry. Generic unpaired headless behavior keeps its signal/grace contract.

Only an actual accepted binding refusal at the original epoch can record the
original static reason and its exact advanced cutoff. After exact model and
child retirement, the paired ticket can publish current canonical observations
as refused data. The publication validates the retained root/lock, ticket value
revision and pair, serializes bounded immutable bytes, and never heals the
boundary or adds a monetary receipt. Lost acknowledgment remains conservative;
an already published refused payload retains its original reason. This scoped
checkpoint is not full frontend replay/recovery or restored economic-completeness
delivery. Those controller and host-policy gates remain separate.

A canonical restored unfinished checkpoint also has an uncertain economic
baseline: the old process may have opened a paid request after its preflight
save. Initializing its existing account marks tracking incomplete without
adding charges, quantities or receipts. Empty-ledger reconciliation cannot
clear that flag. This also conservatively covers quitting before any request,
since those saved bytes do not prove that no request was opened. Uncapped policy
and truly fresh new Session IDs retain their existing behavior. The denied idle
startup/recovery carrier is still a separate controller requirement; this
accounting restriction alone does not make that idle frontend path available.

## Permission compatibility data

The bounded permission-policy projection includes the configured ceiling, active
mode, compiled matching semantics, external path rules, literal shell prefixes
and retained configured root identity. It excludes session approvals, hook
decisions, spill grants and counters. A runtime-only monotonic watcher also
records transient policy changes, including change followed by restoration; an
exhausted watcher is unavailable. Restored compatibility data never installs an
approval. Native publication must still validate its captured permission owner
and retained workspace at admission and publication. These getters alone do not
enable native replay or authorize a metadata checkpoint.


## Actual headless ownership path

The actual built native adapter captures bounded compatibility data from its
retained workspace, configured permission policy and instruction source owners.
It revalidates those owners at Prepared, before a native tool dispatch and at
final publication. The saved projection cannot restore grants or runtime owners.
The Prepared guard separately checks actual system, tools, ordered native input
and opaque blocks, including raw request overrides and SDK cache policy.

Plain print runs the exact retired model producer while its writer remains
retained. Canonical Session recording preserves the literal input and observed
usage without another ledger receipt. Its current-value progress writes stay
InFlight through title work and the configured SessionEnd hook. Final publication
requires the paired retired producer and a still-matching feed/host guard. Error
paths settle the same IO scope; a dropped waiter transfers its retained ticket to
the existing cleanup owner. No extra model task or accounting observer is added.

The one-MiB owning control uses two real Startup/init_features/resolve_prompts/
dispatch cycles and reloads immediately between them. It verifies two actual
HTTP requests, two exact receipts and one occurrence of the prior signed block.
Separate controls retain observed accounting after a failed final publication
and reject a changed host before a second request. The fault injection is before
namespace replacement; the existing primitive post-rename/lost-ACK control
continues to distinguish a complete newly published payload from old InFlight
bytes. It does not claim that every post-rename error leaves denied disk data.

The ordinary non-native schema repair contract remains unchanged. Inactive goal
rounds and the final persistence phase are not polled around plain model or
schema-repair HTTP: measured short construction boundaries preserve the existing
1 MiB stack contract without changing signal ownership, effects or accounting.

At plain-print exit, configured asynchronous SessionEnd children are cancelled
and joined through their actual existing scope before final publication. The
unique writer ticket remains retained through that cleanup, including a dropped
awaiter; final file work then uses an owned IO scope. That new IO scope cannot
replace the binding's independent exact model-retirement proof.

Legacy Unix snapshots retain their existing owned-file permission repair behavior.
Under the retained session lock, the publisher first classifies bounded bytes
without changing permissions. Native markers and malformed classification remain
closed. Only a positively legacy target is rechecked for identity and unchanged
bytes, repaired through that exact descriptor, and privately replaced. Native
checkpoint imports and writes keep their nonrepairing validation.

## Additional actual publication controls

The plain-print acceptance fixture now saves a healthy first native turn,
accepts a second response, and fails its final replacement. Immediate reload
and a separately spawned process both retain InFlight refusal, 20 input and
4 output tokens, and the two canonical usage receipts. The new process uses
the bounded read-only loader and saved-history refusal gate; it cannot create
runtime writer authority.

Configured SessionStart has an actual successful handler control. Resuming
that saved native Session with `--no-session` refuses before a second handler
or HTTP request, while a fresh explicit `--no-session` invocation runs the
handler and model without creating a saved native checkpoint. The configured
UserPromptSubmit command has the same admitted/early-refused controls through
its actual provider gate. Neither startup hook can run when native preflight
fails. ACP native publication and automatic round ownership remain separate
controller work.

## Ordinary interactive turns

An ordinary TUI submission transfers its startup ticket, or durably opens its
next ticket, before building the native model. The actual paused runner shares
that exact scope and binding. Its literal or typed User row and current snapshot
are recorded before releasing the start barrier. Terminal event handling joins
the actual actor, projects usage once through the normal events, attaches the
complete canonical capture and publishes before settling input side effects.
Immediate read-only reload is checked after each accepted turn, both within one
App and after rebuilding from the saved Session.

SessionEnd uses a private, one-use runtime permit issued only by the already
retired model and its acknowledged publication. It retains the exact published
value and live host guard. The paired writer records that exact final revision,
digest and outcome at acknowledgment; an older actor cannot borrow the newest
value from its shared publisher. A changed value or permission/source owner refuses
before cleanup effects. A healthy boundary first becomes durably InFlight; the
configured hook then runs in its own existing cleanup scope. Only after all
admitted hook children are reaped may the unchanged original model boundary be
published again. Hook completion does not create another model-completion
witness. A previously refused boundary remains refused. Cancellation retains the
writer through hook or file cleanup.

Actual authored instruction controls pin the same content in two native TUI
turns while the catalogue generation increases. Their top-level system and tool
bytes remain identical. Changing the authored payload then refuses before any
third HTTP request; the first two accepted usage receipts remain unchanged.
Native pressure currently parks its complete bound tool reply rather than
starting an unbound compactor. Automatic round, explicit fresh-context/recovery,
and all metadata mutation paths still require their own controller adoption.
