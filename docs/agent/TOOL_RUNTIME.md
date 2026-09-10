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

Session restoration and import prepare every rebuilt tool from the incoming
session: read tracking, todos, JS scratch, spill registration, and the session ID
used for spill files. Preparation retains the active workspace and shell binding;
a serialized working directory cannot redirect tools. Provider setup must succeed
before publishing the replacement client, agent, or session. Import also saves the
prepared session before publication. A failed activation leaves the current agent
and its runtime state usable. `src/ui/slash/session_restore_tests.rs` exercises
replacement with a local provider through actual tool calls and spill persistence,
including unchanged and changed providers and failed provider setup. JS assertions
run when the platform permits JS registration; macOS additionally needs the
installed-binary path because libtest is not a production worker executable.

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

The TUI records tool calls and results as they stream. Both successful and failed
turns adopt provider call IDs and reasoning metadata from the runner's canonical
transcript before terminal presentation or cleanup. This preserves continuation
history after a provider failure without duplicating the live tool records.

Interactive teardown runs after both normal exit and event-loop errors. It stops
and joins the input reader, cancels the main runner and side questions, and waits
for their owned work before closing shared services. Reader shutdown drains queued
events into the UI's deferred queue while joining, so a saturated sender can finish
without closing the shared event channel. Terminal handoffs (including lazygit)
use that same path and restart only the reader; background completions retain
their destination and queued events retain their order. Resumption invalidates
the renderer so unchanged drafts and status lines are repainted. Ctrl-G keeps the reader
paused for the whole editor session and restores it even when the editor fails.
Editor launch, nonzero-exit, and draft-read failures are displayed after terminal
restoration. Drafts are limited to 4 MiB before launch and on readback. Only regular
UTF-8 files are accepted; symlinks, special files, and oversized content are refused.
The read itself stays bounded even if the file grows after its size check. Atomic
replacement saves are supported. Valid edits survive an editor failure; rejected
readback leaves the original input intact, and combined exit/read failures report
both causes. The private editor directory is bound to a retained handle; replacing
that directory makes readback and cleanup fail without accepting or deleting the
replacement's contents. Successful readback cleans up at most 128 sibling artifacts
(including backups), without following links or traversing subdirectories. Rejected
readback, excessive artifacts, or subdirectories retain recovery files and report
their location. If the directory moved, the notice identifies its original path
without claiming the original files are still there. Cleanup failures are surfaced
after terminal restoration; valid edits remain in the input buffer.
`src/ui/prebuild.rs` owns background agent construction and its work scope. It
lets cancelled MCP initialization finish process cleanup, explicitly closes MCP
managers in rejected or queued prebuild results, and waits for scoped children.
The result receiver stays with that owner. A memory refresh retires the stale
prebuild before constructing a replacement; a retirement failure stops the new
turn. Foreground agent builds invalidate the captured startup agent; prompt
selections also invalidate it when they clear the cache without rebuilding.
Consuming a late result retains its MCP services and rebuilds with the current
model, prompt, and session. A newer foreground MCP manager remains authoritative,
and duplicate startup connections are closed. Pending services are consumed even
when a foreground agent is already cached, so interruption cannot expose an old
startup agent later. Read-only commands leave the startup snapshot valid. Receiving
a result preserves its owner until retirement.
Retirement has a five-second bound per owner; a timeout is reported in the log
and remaining cleanup still runs.

Terminal `Done` and `Error` events precede the runner's final cleanup. The UI
waits for its lifecycle channel to close before starting compaction, loop
validation, or another turn. Successful response data is committed before that
wait, and normal settlement preserves the cached agent. A timeout retains the
channel and abort handle so error teardown can still cancel and settle the work.

Paused runners share one start barrier. The interactive path enables compaction
before releasing it; ACP can publish cancellation before release. Cancelling or
dropping a runner before start prevents provider work. ACP-only accessors and
feature-specific test adapters compile only with their consumers. The shared
bounded validation executor remains available without `loop`; loop defaults,
command display, and explicit validator cancellation are gated to loop callers
and their native tests.
MCP manager rebinding follows worktree transitions; ordinary MCP initialization
remains available without worktrees. Read-only JS constructors follow subagent
callers, while standalone skills tests retain child discovery and turn-isolation
coverage. The test-only task oracle adapter follows the sandboxed subagent
harness; production completion-verification evidence is unchanged.
ACP permission correlation claims a lifecycle ID when each registered tool
invocation enters the concurrency boundary, before waiting for its lease.
The ID stays local to that invocation through delayed and repeated approval
checks; an already-allowed sibling cannot donate its ID to another prompt.
Direct calls without runner context and independently brokered JS effects
retain ACP's synthetic-call fallback. Correlation state compiles only with ACP;
the approval reply channels remain available to every interactive frontend.

Live nested tool-call events compile with `subagents`, their only production
producer. ACP ignores those display-only events when subagents are enabled;
the outer tool call and result remain canonical. Persisted nested records stay
readable in all builds, including historical replay without subagents.
Brokered PreToolUse entry points compile with `js`, and subagent lifecycle
hook entry points compile with `subagents`. Their explicit-dispatcher cores
also compile for hook tests. The broker adapter rejects both Deny and Ask,
returns Allow/Defer input rewrites, and leaves grant-target validation to the
parent effect broker.

Loop caller regressions exercise real CLI output and transcript persistence,
and TUI completion handling with output limits, unavailable sandboxing, scoped
cancellation, and a stale result arriving during a replacement validation.
Headless signal cleanup remains covered by the existing CLI interrupt matrix.
Operator-configured loop and completion validators resolve the configured shell
independently of model tool eligibility. Their presence also preserves startup
sandbox requirements, including refusal of an explicitly unavailable backend.
Tool-free invocations without configured validation still skip shell lookup.
A shell resolved solely for validation stays unavailable to ordinary commands
and `!` interactions; only the validator's private sandbox clone may use it.
Workspace rebinding preserves that restriction and revalidates the shell identity.

JS-only audit ownership and permission-identity adapters compile with their
consumers; global path-ownership tests retain the audit namespace contract.
The macOS worker's volume/file identity helpers are separate from the general
filesystem identity checks, which remain available without JS. Atomic writes
retain their publication decision on every build; JS cancellation adapters and
Windows retry signals compile only where used. Publication tests cover both
sides of the cancellation decision in one matrix, release and join workers
before assertions, and bound checkpoint observation and release waits.

Workspace rebinding and its retained relative shell configuration compile for
`git-worktree` and native tests. The direct workspace service adapter follows
its MCP, LSP, and Git consumers; the Git capability probe and explicit-stdin
adapter follow their narrower callers. Complete-descendant authority inspection
and identity-only file replacement remain available to JS and native tests.
Status signal construction follows `status-signals`, while conflict messages
follow `git-worktree`; Unix protocol tests retain both adapters.

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
