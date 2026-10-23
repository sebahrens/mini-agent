# Session storage size recovery

Session files remain bounded by `session::storage::MAX_SESSION_BYTES`. The
encoder counts JSON bytes, including escaping, and retains at most the bounded
payload. Storage errors remain static and do not quote session content. A typed
size error supplies the encoded byte count and fixed byte limit to frontends.

All ordinary persistence consumers use the shared recovery preparation path:
interactive settled turns and metadata saves, permissions, goal round boundaries,
headless startup/turn/schema saves, slash session mutations, and ACP checkpoints.
Only an encoded-size failure activates the bounded shrink ladder. If a rewind
undo snapshot exists, it is removed once and encoding is retried once. The
frontend emits exactly `undo snapshot dropped to stay within the session size
limit`. The visible conversation, tool history, accounting and compaction
records are never truncated by this policy. Validation and I/O errors do not
trigger undo removal.

If the retry is still oversized, or there was no undo snapshot, preparation
refuses before creating or replacing a session file. The previous private,
atomic snapshot remains available. The warning identifies the encoded size and
limit in bytes and directs the user to `/compact`. The same oversized state does
not produce a new warning on every turn. A successful save at or above 75% of
the limit warns once and suggests `/compact`; falling below that threshold arms
the warning for a future crossing. Notice state is process-local, excluded from
serialization and canonical digests. Deferred saves retain their existing
single-flight writer and report the size notice when a bounded snapshot is
accepted for publication; later I/O errors retain their existing reporting.

Native publication has an additional authority boundary. The exact startup,
progress, refusal or retirement owner may run the same shrink ladder before
sealing its new canonical digest. Removing undo from the live session immediately
queues its notice, even if publication or acknowledgment subsequently fails;
successful acknowledgment updates the size-threshold state. Immutable raw
native snapshots, replay hashes and ordinary saves of an already sealed native
snapshot remain strict: storage cannot remove undo and then bless a different
digest. Stale revisions, independently deserialized data and unowned snapshots
retain all existing rejection and lease checks. This distinction is covered by
the native-owner recovery regression.

The policy never increases the storage limit, starts automatic compaction,
contacts a provider or adds model requests. Explicit `/compact` uses the normal
compaction behavior; the next save can succeed with that smaller session.
