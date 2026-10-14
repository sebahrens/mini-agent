# File checkpoints and rewind

On empty idle input, press Escape twice within 500 ms, or run `/rewind`.
Select an earlier user turn, then choose conversation only, restore files only,
or restore files and conversation. The confirmation lists the affected paths.
Conversation rewind puts that turn's original input back in the editor.
`/redo` restores conversation history only; it does not replay filesystem effects
or rearm the abandoned branch's retired file checkpoints.

Compaction retires checkpoints for removed turns and reindexes surviving turns
with the retained user messages. A consumed steering message is anchored to its
actual canonical user row before the runner may request another completion or
perform later tools. A stale or unacknowledged handoff stops that continuation.

Native `write` and `edit` capture complete permitted preimages before their
checked publication. Checkpoints are independent of displayed diff limits and
post-tool output rewriting. The committed postimage is recorded before optional
formatter work yields; a known formatter result updates that postimage. The
ordinary request owns the capture, including native writes made by its child
agents. Shell commands and arbitrary external processes are not checkpointed.

Preimages are SHA-256 addressed private `.bin` files in the session's existing
`tool-outputs/<opaque-session-directory>/checkpoints` directory. The private
session JSON contains only the bounded turn/path/digest inventory. Each file is
limited to 1 MiB, the session to 16 MiB of preimages and 512 mutation records.
Oversized, unreadable, external-workspace or READ-disallowed preimages are not
captured; missing checkpoints visibly prevent file restoration for the affected
range. Content blobs may remain after an interrupted write, count against the
same recorded byte budget, and are never interpreted as executable instructions.
`--no-session` uses an in-memory preimage store and creates no checkpoint files.

Restoration is an explicit operator action. It never grants the agent new
permissions. Every checkpoint must match the current session and the retained
workspace's physical fingerprint. Every current postimage and preimage blob is
checked before the first publication, then each publication rechecks file
identity and content. An editor change, stale picker, corrupt blob, symlink or
replaced workspace is refused. There is no force bypass. Imported transcripts
and session forks do not inherit checkpoint restoration authority.

Existing files use the same checked atomic replacement as native tools. Created
files are moved into an exclusively-created private sibling retirement directory,
validated there, and deleted only if their identity and content still match.
A raced foreign object is retained and create-only rollback never overwrites a
newer file. Windows checked workspace replacement/retirement remains unsupported;
file rewind refuses that operation. Conversation-only rewind remains available.

Restoring several files is not a single filesystem transaction. All files are
preflighted together; if a race or I/O error occurs during publication, the
error reports the completed count, already-restored paths are retired from the
inventory, and the conversation is retained. The current save policy still
applies to later session persistence failures. Rewind does not refund usage,
restore old permission grants, execute commands, or call a provider.
