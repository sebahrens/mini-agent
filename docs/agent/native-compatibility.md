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

Workspace compatibility uses a bounded borrowed path and retained physical root
identity, validated before and after projection. Same-path replacement is refused.
This digest is compatibility data, never a directory capability or approval.

Plain print and ordinary interactive turns use these APIs through the strict
publication controller described in `native-publication.md`. ACP and automatic
round adoption, explicit durable recovery, live effort controls and remaining
memory/cache-prefix placement are still unfinished. Imported compatibility data
never creates that controller's runtime writer or actor permits.
