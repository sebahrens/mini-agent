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

Native `write`, `edit` and `notebook_edit` capture complete permitted preimages
before their checked publication (`notebook_edit` records the exact raw `.ipynb`
bytes, so restoring returns the original formatting byte for byte; notebooks
outside the bound workspace are counted as unavailable checkpoints, like
external `edit` targets). Checkpoints are independent of displayed diff limits and
post-tool output rewriting. The committed postimage is recorded before optional
formatter work yields; a known formatter result updates that postimage. The
ordinary request owns the capture, including native writes made by its child
agents. The `js` tool's `write_file` is captured the same way (it also appends
the one-time uncaptured notice) and replaces an existing file only after the `read`
tool returned its complete current contents, exactly like `write`; a refused
overwrite names the rule and leaves the file untouched. `read_file` inside a
script does not count as a read. One `js` result carries one mutation diff
(the first file written); further writes in the same call are counted in the
result, not previewed. The typed `git` tool's `restore` is checkpointed the same way (the content it discards is recorded per file); `switch`, stashes and the merge family rewrite the working tree without per-file preimages and are reported as uncaptured. Shell commands and arbitrary external processes are not file-checkpointed, and the rewind picker says when shell or git commands ran in the chosen range; the optional workspace snapshot tier below covers them.

Preimages are SHA-256 addressed private `.bin` files in the session's existing
`tool-outputs/<opaque-session-directory>/checkpoints` directory. The private
session JSON contains only the bounded turn/path/digest inventory. Limits are
configurable under `[file_checkpoints]`: `max_file_bytes` (largest pre- or
postimage; default follows the edit/write size limit), `max_total_bytes`
(preimage bytes per session, default 64 MiB) and `max_records` (mutation records
and distinct preimages per session, default 2048), clamped to 1 GiB, 4 GiB and
100000. Saved inventories are validated against those hard ceilings, so a
session written under larger limits still loads. Oversized, unreadable,
external-workspace or READ-disallowed preimages and mutations past a limit are
not captured. The first uncaptured mutation of a session appends a one-time
notice with the reason to that tool's result (so both the model and the
transcript see it at once); the `/rewind` picker then reports
`N edits not restorable (<reason>)` for the affected range, and missing
checkpoints prevent file restoration for it. Content blobs may remain after an interrupted write, count against the
same recorded byte budget, and are never interpreted as executable instructions.
`--no-session` uses an in-memory preimage store and creates no checkpoint files.

Retired records (turns removed by compaction, a conversation rewind or a completed restore) are dropped from the inventory together with the preimage blobs nothing else references, so they stop counting against the record and byte limits.

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
newer file. Windows has no atomic name exchange or compare-and-replace; it uses handle-bound
moves instead (`paths::exchange_relative`, `paths::retire_relative_expecting`,
`fs::atomic_write_resolved_expecting`): the old file is opened without following links and with
writers excluded, verified by identity and content on that handle, moved aside by the same
handle, and the replacement is published create-only (a failure puts the old file back
create-only; retirement deletes the verified object). The target name is briefly absent. File
rewind therefore works on Windows; whole-workspace snapshots stay Unix-only.

`/rewind` also offers **Restore unchanged files only**: every selected file whose
current bytes still match the recorded postimage is restored, files changed since
(a formatter, an editor, a shell command) or whose chain cannot be verified are left
untouched and named, and uncaptured edits in the range no longer refuse the rest.
The strict modes keep refusing everything on any drift.

Restoring several files is not a single filesystem transaction. All files are
preflighted together; if a race or I/O error occurs during publication, the
error reports the completed count, already-restored paths are retired from the
inventory, and the conversation is retained. The current save policy still
applies to later session persistence failures. Rewind does not refund usage,
restore old permission grants, execute commands, or call a provider.

## Workspace snapshots (shell changes)

Before the first shell command of a turn the whole workspace is recorded as a
content-addressed manifest in the session's private `tool-outputs/<dir>/workspace-snapshots`
directory: every regular file the ignore rules (`.gitignore`, `.ignore`,
`.git/info/exclude`) do not exclude, hashed and stored once per distinct content.
`.git`, ignored files and symbolic links are never recorded and never touched.
The user's own Git metadata is not used. Limits: 20000 files and 256 MiB per snapshot
(a larger workspace is refused as a whole, never recorded partially), 16 MiB per file
(a larger file is left out and left alone on restore), 1 GiB of new snapshot content per
session. `[file_checkpoints] workspace_snapshots` is `"auto"` (default), `true` (also
announces a refusal once) or `false`.

`/rewind` then offers **Restore workspace**: the confirmation lists how many files
would be restored and deleted (and the first names), and the restore rewrites changed
files, recreates deleted ones and removes files created since the snapshot, all through
the same checked primitives as file rewind (identity and content are re-verified for
every publication; a damaged snapshot blob changes nothing). It restores the state before
the first shell command at or after the chosen turn, so native edits made earlier in that
same turn are part of the restored state; it is a separate mode from the per-file restore
and is refused on Windows. Snapshots follow the conversation: a conversation rewind
discards the snapshots of the removed turns and compaction renumbers the kept ones.
