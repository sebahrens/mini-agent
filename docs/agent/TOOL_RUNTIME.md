# Tool Runtime

Built-in, optional, and MCP tools are collected as `ToolDyn` instances before
the main agent is built. After command-line filtering, each registered tool is
wrapped by `src/agent/tools/memoize.rs`. The wrapper snapshots the tool name,
description, and JSON parameter schema once, then returns owned clones when Rig
requests provider definitions on later completion turns. Calls and structured
results are delegated unchanged to the original tool.

MCP collection rejects names reserved for built-in tools, including optional tools
whose features are disabled. The reserved-name policy and its test matrix compile
with `mcp`, independently of `js`. The JS directory-discovery adapter compiles
with `js`; ordinary `find_files` and `list_dir` retain their shared directory access.

`/editsys` rebuilds the agent immediately after changing the edit system, so the memoized
`read`/`edit` definitions and their runtime behavior always switch together.

Foreground shell commands run under a fixed 30 s deadline that a call can only
lower; an explicit timeout must be at least one millisecond. A shell call with
`background = true` instead returns a session-scoped job id immediately and
uses a 24-hour maximum (again lowerable with `timeout`).
The companion `job_status` tool polls bounded live output or stops and reaps
the job. A session permits at most eight concurrent jobs and retains the newest
32 job records; each stream keeps a 64 KiB rolling head/tail view. Background
commands use the same shell, workspace, sandbox, and permission decision as
foreground commands, and all are cancelled on turn cancellation or session
shutdown.

Interactive teardown runs after both normal exit and event-loop errors. It stops
and joins the input reader, cancels the main runner and side questions, and waits
for their owned work before closing shared services. Reader shutdown drains queued
events into the UI's deferred queue while joining, so a saturated sender can finish
without closing the shared event channel. Terminal handoffs (including lazygit)
use that same path and restart only the reader; background completions retain
their destination and queued events retain their order. Ctrl-G keeps the reader
paused for the whole editor session and restores it even when the editor fails.
Editor launch and nonzero-exit failures are displayed after terminal restoration;
unchanged drafts and edits from a failed editor are retained.
`src/ui/prebuild.rs` owns background agent construction and its work scope. It
lets cancelled MCP initialization finish process cleanup, explicitly closes MCP
managers in rejected or queued prebuild results, and waits for scoped children.
The result receiver stays with that owner. A memory refresh retires the stale
prebuild before constructing a replacement; a retirement failure stops the new
turn. Receiving a result preserves its owner until retirement.
Retirement has a five-second bound per owner; a timeout is reported in the log
and remaining cleanup still runs.

Terminal `Done` and `Error` events precede the runner's final cleanup. The UI
waits for its lifecycle channel to close before starting compaction, loop
validation, or another turn. Successful response data is committed before that
wait, and normal settlement preserves the cached agent. A timeout retains the
channel and abort handle so error teardown can still cancel and settle the work.

Captured/model-authored commands receive null stdin when the caller supplies no explicit input;
they cannot inherit the TTY. On Unix they start in a fresh session. The Linux `bwrap` launcher
closes its temporary workspace-authority descriptor inside the namespace before the model's shell
runs. General-sandbox availability is based on a bounded real launch probe, and model commands
receive only the non-credential environment even when the operator explicitly disables OS
containment. The dedicated sandbox cache is `<cache_dir>/sandbox-runtime`; the full application
cache is not mounted.

Captured stdout and stderr are returned in separately labelled sections. A
non-zero exit is retained at the tail even under very small line caps; Unix
signal deaths report the signal name and number instead of a synthetic `-1`
exit code.

Tool calls issued together in one assistant message may execute up to four at
once. Every registered tool shares a fair reader/writer lane: `read`, `grep`,
`find_files`, `list_dir`, `todo_read`, and other explicitly classified
read-only tools use shared access. Mutating tools (`write`, `edit`, `shell`,
`git`, `js`, `task`, and state writers), MCP tools, and unknown extension tools
use exclusive access. Consequently independent reads overlap, results remain
in the model's original call order, mutations remain serialized, and reads do
not observe a concurrent mutation. Tool hooks are inside the same lane so a
guard-rail command cannot race another tool operation.

`find_files` preserves filename-regex matching for valid regular expressions.
If the supplied pattern is not valid regex, it is parsed as a workspace-relative
path glob, so common forms such as `**/*.rs` search recursively. Capped output
is selected with a bounded lexicographic top-N heap, making “first N” stable
across filesystem traversal orders while still reporting the exact match count.
`list_dir` keeps only actual directories in its directory-first sort group and
renders symbolic links as `name -> target`. Read windows size their line-number
column from the actual range and report an offset past EOF directly instead of
showing an inverted range. When edit's bounded fuzzy search is too expensive,
the error says that no closest-match suggestion was computed.

SEARCH/REPLACE edits require a unique match, including occurrences that overlap
in the original or whitespace-normalized text. For example, `aba` is ambiguous
in `ababa`; add context to select the intended occurrence. With `replace_all`,
exact replacements proceed left to right over non-overlapping occurrences.
Exact-match ambiguity errors report the total count and at most ten previews.

Recursive `grep` and `find_files` traversal, plus `list_dir` directory reads,
run on the bounded blocking pool rather than occupying an async runtime worker.
Ignore rules use a metadata-invalidated, per-workspace parent-chain cache and a
persistent chain for nested directories, so descent does not clone every prior
matcher. `grep` reads only an 8 KiB prefix before rejecting a binary file and
does not load the remainder of that file.

`write` creates files by default. For a deliberate full-file replacement, the
caller must first read the current file completely and then set
`overwrite=true`; the shared read tracker checks the current metadata and
content fingerprint before the atomic replacement, so an intervening edit
invalidates the authorization.

Headless loops save each iteration's prompt, completed tool calls and results,
response, and token usage to the resumable session before validation or error
propagation. A provider failure after a tool effect therefore leaves a record
for `--continue`. `--no-session` suppresses these saves. If the iteration and
the save both fail, the error reports both failures.

The same wrapper is used for read-only `/btw` and exploration-subagent tool
sets. Definition metadata that genuinely needs to vary while an agent is live
must not be placed behind this wrapper; current tool metadata is fixed when its
agent instance is constructed.

JavaScript calls run in a fresh QuickJS runtime, but model-authored code can pass
validated plain JSON between calls with `scratch_put(key, value)` and
`scratch_get(key)`. Scratch keys are restricted to 1–128 ASCII letters, digits,
`.`, `_`, `-`, or `:`; values are capped at 1 MiB each, 4 MiB and 128 entries per
logical session/workspace. Scratch is process-local, omitted from persisted
sessions, and cleared when the session changes workspace. Stored skills cannot
access these globals.

Exploration subagents use a distinct read-only JS profile when worker
containment is available. That realm exposes only `read_file`, `list_dir`, and
`grep` as brokered effects, and its parent-issued grant contains only read
authority. It has no scratch, structured-result, writer, network, process,
proposal, batch-read, or glob global. Active pure learned-JS exports may be
bound; every effectful tier and canary replacement is excluded.

`result(value)` is the terminal structured-return channel. It accepts
descriptor-only plain JSON up to 256 KiB, is parent-validated and durably
audited, and stops later JavaScript/effects after acknowledgement. The tool
returns that JSON directly without another model round trip. Ordinary final
expressions remain limited to strings, finite scalars, or plain JSON and use the
smaller 64 KiB result channel.
