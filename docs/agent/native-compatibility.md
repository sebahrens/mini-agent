# Native replay compatibility APIs

The shared APIs classify the actual provider adapter and retain bounded, ordered
completed native blocks separately from display history. Canonical commit is
one-shot and binds the exact Session, preappend feed, cutoff and compactions.
Observed usage updates Session accounting once even when accepted content cannot
attach. Imported compatibility data grants no permission or runtime ownership.

A stored boundary requires the exact retired actor outcome: Completed or a
complete surfaced tool-reply batch. Legacy wrappers default to Incomplete. Native
zero-visible completion additionally needs the current provider end_turn witness
and valid opaque content; tool counts or empty display text do not attest an
actor's completion.

Retained native replay is bounded (`src/session/native_replay.rs`): at most 128
frames, 4 MiB for the whole serialized replay, and 2 MiB for the base history
and for each frame. A replay over any bound is never truncated: it is treated
as non-replayable, reported as "native thinking context exceeds its finite
bounds".

## Restarting the chain (new epoch)

A chain that can no longer be replayed is dropped and the next request replays
the display transcript with no signed thinking blocks, as a new epoch of the same
saved-session owner. The host does this, with a one-line notice, when:

- the chain used half of either finite bound (64 frames or 2 MiB), so one
  tool-heavy prompt still fits in the remaining half before the hard 128-frame /
  4 MiB limit;
- the session's model differs from the chain's (`/model`, an edited saved model);
- the transcript changed since the chain was saved (compaction by `/compact`,
  between-turn auto-compaction, headless pre-turn compaction);
- a recoverable refusal is saved (`Limit`, `ChangedDomain`, `ChangedPrefix`,
  `IncompleteTurn`, `UnavailableSource`), for example a single prompt that
  overflowed a bound mid-turn.

An unfinished context (`InFlight`) and raw request overrides
(`UnsupportedOverride`) are never discarded this way. The restart is published
like an effort change: the old value is denied on disk first, then the healthy
payload without a chain replaces it, under the exact retained writer; a failed
publication leaves the context refused. Compaction mid-turn and ACP compaction of
a native session stay unsupported (ACP refuses before any summarizer spend).

The manual SDK cache policy regenerates every message-level cache marker on each
request (a tail marker plus spares every 18 blocks), so under that policy marker
placement is not part of the commitment; system and tool markers, nested/data
markers and non-manual policies stay exact. Before this, a saved chain mismatched
after about 18 blocks and refused the session.

Workspace compatibility uses a bounded borrowed path and retained physical root
identity, validated before and after projection. Same-path replacement is refused.
This digest is compatibility data, never a directory capability or approval.

Plain print and ordinary interactive turns use these APIs through the strict
publication controller described in `native-publication.md`. ACP and automatic
round adoption, explicit durable recovery, live effort controls and remaining
memory/cache-prefix placement are still unfinished. Imported compatibility data
never creates that controller's runtime writer or actor permits.
