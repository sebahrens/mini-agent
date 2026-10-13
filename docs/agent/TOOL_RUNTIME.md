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

`src/permission/cli_rules.rs` parses and bounds invocation-only `--allow`, `--deny`,
and `--allow-prefix` seeds, with redacted diagnostics and Debug values. Frontends
compile the ordinary configured checker before merging this transient layer, so
new shell keys preserve built-in defaults. The layer never mutates the saved Config
or Session allowlist; no-check/no-tools modes still validate input. Headless, TUI,
and ACP use the same merge and retain their existing approval transports. See
[CLI seed precedence](CONFIG.md#rule-precedence) for mode and external-directory rules.

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

Foreground shell commands default to a 120 s deadline. `shell_timeout_secs`
sets it between 1 and 600 seconds; the tool definition publishes the effective
deadline. A call's millisecond `timeout` can only lower that deadline and must
be positive. Other sandbox command classes, including brokered JS `spawn`,
retain their existing limits. A shell call with
`background = true` instead returns a session-scoped job id immediately and
uses a 24-hour maximum (again lowerable with `timeout`).
The companion `job_status` tool polls bounded live output, stops and reaps
the job, or waits for completion. `action = "wait"` accepts `timeout_ms` from
1 to 60000 (default 10000); expiration or dropping the wait leaves the job
running. `action = "list"` needs no id and returns bounded command summaries,
ids, and statuses without copying captured output. The job registry belongs
to the session's sandbox, so list can recover ids after transcript compaction
and agent rebuilds. A session permits at most eight concurrent jobs and retains the newest
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
Fetch target construction, audit construction, and synchronous permission waits
follow the sandbox-enabled fetch service, with standalone broker and permission
tests retaining those contracts. Persisted fetch audit records remain readable
without that feature. Proposal body-limit errors also remain available to skills.
Permission correlation tests exercise synchronous and asynchronous replies,
including different decisions, crossed identities, and retries of the same key.
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

File tools (`read`, `write`, `edit`, `list_dir`, `grep`, `find_files`) resolve their path
argument once through `resolve_tool_target` in `src/agent/tools/mod.rs`. Home expansion (`~/`,
`$HOME/`) happens first, and the bound-versus-ambient decision is made from the expanded path: a
path that expands to an absolute path takes the ambient permission path and is reported as that
absolute path, while a path that is still relative is opened through the captured workspace
capability and reported as its logical location inside the workspace. A `$`-prefixed component
that does not expand (for example `$FOO/x`, or `$HOME/x` without a home directory) is a literal
workspace-relative name and is reported as such.

Workspace-relative opens do not follow symbolic links out of the component that holds them (each
component is opened without following links; Windows rejects reparse points). When `read`,
`edit`, or `write` is refused for that reason, the error names the link (for example
`'…/packages/x' is (or passes through) a symbolic link`), states where it currently resolves when that target is inside the workspace (an outside target
is not disclosed), and
tells the model to retry with the absolute path of the target, which is then checked by the
ambient permission policy. Other open errors are reported unchanged.

`todo_write` accepts at most 50 items, each with `content` of at most 500 characters and
`status`/`priority` of at most 32 characters. A list over any cap is rejected with a message
naming the limit, and the stored list is left unchanged. Open items are re-injected into every
compaction summary as a "Critical Context" block that is bounded to 32 KiB: fields are clipped
to the same caps (covering sessions restored from before the caps existed), and items that do not
fit are counted in an omission note instead of emitted.

`find_files` defaults to `mode: "auto"`: patterns containing `*`, `?`, `[`, `{`,
or `/` are parsed as relative-path globs first, falling back to filename regex
only if the glob is invalid. `src/*.rs` matches files directly in `src`, while
basename globs such as `*.rs` and `test_*.rs` match at any depth. Other patterns
retain filename-regex matching. Set `mode: "regex"` explicitly for patterns such
as `^test_.*\.rs$`, or `mode: "glob"` for literal glob semantics. An empty result
names the interpretation and suggests an alternative. Capped output is selected
with a bounded top-N heap: omitted `sort` or `sort: "name"` selects paths in
lexicographic order; `sort: "mtime"` selects the most recently modified files first.
Equal timestamps use ascending path order, and unavailable timestamps sort last.
Timestamps come from the already opened file's metadata, without an additional path
lookup. Counts and ordering cover the visited entries; a traversal limit notice
identifies incomplete searches.

`grep` accepts `max_results` from 1 through its configured result cap; omission
preserves that cap. It limits displayed source/context lines, or matching files in
`files_only` and `count` modes. Separators do not consume the cap. Invalid limits
fail explicitly. `multiline: true` searches each complete loaded file and enables
dot to match newlines (`(?-s:...)` can disable that flag locally). Output keeps the
existing numbered source-line format, showing each touched line once plus requested
context. In multiline mode `count` counts non-overlapping regex matches; ordinary
line mode continues counting matching source lines. Result limits still apply if
one match spans many lines.

Both recursive searches visit at most 100,000 directory entries and descend at
most 256 directories. Grep additionally reads at most 100 MiB per call, loads at
most 10 MiB per file (including files that grow during a read), and inspects at most
100,000 multiline matches and 100,000 touched source lines per file. A byte-budget-ending file is skipped rather
than searched as an incomplete prefix. Binary sniffing consumes the same byte
budget. Work limits produce a partial-result notice, including when no matches
were found; narrow the path or pattern to continue. These bounds leave path,
read-deny, external-directory approval, ignore, and no-follow traversal policy
unchanged.
`list_dir` keeps only actual directories in its directory-first sort group and
renders symbolic links as `name -> target`. Read windows size their line-number
column from the actual range and report an offset past EOF directly instead of
showing an inverted range. When edit's bounded fuzzy search is too expensive,
the error says that no closest-match suggestion was computed.

SEARCH/REPLACE edits require a unique match, including occurrences that overlap
in the original or whitespace-normalized text. For example, `aba` is ambiguous
in `ababa`; add context to select the intended occurrence. With `replace_all`,
exact replacements proceed left to right over non-overlapping occurrences, and
disjoint whitespace-normalized variants are included. Overlapping normalized
source ranges fail without writing. Exact-match ambiguity errors report the total
count and at most ten previews. Approximate matches provide a preview by default;
only `fuzzy: true` permits a unique match at 85% or higher similarity. It cannot
be combined with `replace_all` or hashedit, and all blocks remain atomic.

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
for `--continue`. `--no-session` suppresses these saves and the goal round records. If the iteration and
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


## Native mutation result metadata

[`src/agent/tools/mutation_diff.rs`](../../src/agent/tools/mutation_diff.rs) owns
bounded review rows, applied byte ranges, and before/after SHA-256 digests for
successful native edits and writes. Producers use their existing checked source
bytes and the bytes accepted by atomic publication, without a preview filesystem
read. A new-file create has no prior digest. Existing-file previews require the
side-effect-free read-policy query in `permission/checker.rs`: deny rules and
external-directory limits still apply, and an invocation's edit/write hook grant
cannot supply read authority. Failed, denied, oversized, and read-unapproved
mutations provide no extension.


Edit result disclosure uses existing read authority independently of edit permission.
An allowed edit with denied or unchecked `ask` read access still performs full matching
and compare-and-swap publication, but returns the applied-edit summary and bounded
caller-supplied replacement text instead of file excerpts. Unchanged neighboring lines,
unchanged portions of an edited line, approximate-match candidates, current checksum
values, and language-server diagnostic text are omitted. This display check never asks
for read access or consumes a one-shot read grant; an edit approval does not authorize
reading. Existing allowed read access retains the normal excerpts and typed mutation
metadata. An explicit read deny overrides earlier reads and session grants.


The native tools return Rig typed result extensions alongside their
model-visible strings. Definition memoization, hook decorators (including output
redaction), and concurrency wrappers forward these extensions. `MutationDiffHook`
observes successful results and transports a bounded snapshot with Rig's exact
internal call ID through a run-local event sink or headless observer. It uses no
name-keyed queue, global invocation registry, model-text parser, or path lookup.
The UI accepts a record only under the matching native tool-call ID and name.

`SessionMessage.recorded_diff` is display-only historical data, retained by
undo/redo, storage, JSONL, and HTML export. Import validates its bounds and labels
it as imported; it cannot confer permission, skill verification, or filesystem
identity. Model continuation history excludes this field. The feed sanitizes file
controls and uses semantic addition/deletion/hunk styles; HTML escapes the same
text. Configuration in [CONFIG.md](CONFIG.md#mutation-previews) controls context
and retained rows, with deterministic byte, input, and matching-work limits.

## Compaction history and file requests

[`src/session/compaction.rs`](../../src/session/compaction.rs) retains a bounded
historical file-request inventory across compaction, storage, and undo/redo. It
uses only structured `read`, `write`, and `edit` path inputs from the prefix
actually summarized, plus the previous structural inventory. Shell text and model
claims cannot add entries. These are requested operations, never proof of effects
or current file contents; nested requests retain their separate scope, and
imported metadata is explicitly untrusted. Up to 128
records and 16 KiB of names/paths are retained, with a 4096-byte path limit and
explicit omission counts. The rendered appendix shares the summarizer's existing
byte partition instead of increasing its context budget. Existing critical TODO
and goal restatements still append under their existing contracts.

The summarizer receives retained tool results before request-time context pruning.
Structured request identities/arguments accompany them inside data fences. It
never opens recorded spill paths, and typed mutation display rows remain excluded
from the model transcript. Rolling requests retain their existing byte/count/time
bounds, usage accounting, and partial-prefix drain contract. Summaries use Task,
Decisions, Files, Open Items, and Next Step sections. `/compact show` exposes the
active recap; manual compaction prints it and resume reports its line count.

## Headless answer schemas

The request-owned `src/output_schema.rs` module loads and compiles headless answer
schemas before provider/session initialization. It uses the existing pinned Rust
`jsonschema` dependency across platforms with retrieval features disabled, an
explicit denying retriever, linear regex limits, structural/work caps, and bounded
single-request tool-free repairs. It never executes QuickJS in the parent. The
owning CLI, envelope, limits, and cancellation contracts are documented in
[GET_STARTED.md](GET_STARTED.md#5-cli-flags). No production process-launch site
is added: formatting repairs use the existing owned native provider stream path.

## Custom slash-template loading

[`src/context/commands.rs`](../../src/context/commands.rs) separates authored
metadata and body segments before expansion. It uses the captured workspace and
private process configuration root, bounded no-follow regular-file opens, and
existing exact project/config/SHA trust bindings. Structured includes use the
current side-effect-free read authority before any one-shot or model mutation;
Ask remains fail-closed while slash dispatch owns the UI event loop. No read
tracker, doom-loop counter, hook one-shot or permission grant is consumed by
this check. Untrusted arguments and included data are never reparsed as
metadata, include references, slash dispatch or shell commands. The command
picker, help and real App submission share this loader. Project config trust
is shared with prompt and specialist discovery. Those consumers validate the
supplied retained workspace before and after reading a regular, non-link
config of at most 256 KiB; existing canonical project/config paths plus SHA-256
still define approval. Private trust state is limited to 1 MiB and retains owner,
private permissions/DACL, one-link and exact inode checks. Both Unix
private-open phases use `O_NONBLOCK` and validate the opened descriptor, so a
FIFO replacement fails instead of hanging. Global prompt/specialist aliases
and global theme loading keep their existing compatibility policy; this change
does not claim to harden every definition loader.


## Session catalogue and owned title work

[`src/session/catalogue.rs`](../../src/session/catalogue.rs) reads private,
no-follow, single-link UUID session files through bounded metadata discovery:
2,048 directory entries, 8 MiB per file, 64 MiB total, and 100 displayed rows.
Oversized or unsafe files are skipped with a visible catalogue notice. Stored
workspace strings are labels; they are never canonicalized or opened. Selection
reloads the exact bounded private record and verifies identity, unchanged scope,
and the retained current workspace before admission. Same-workspace private
resume retains its trusted history/grants/redo through claim-or-fork ownership;
an explicitly selected foreign record forks visible history into the current
workspace with a fresh ID, no old permission grants, and no hidden redo.

[`src/ui/pickers/sessions.rs`](../../src/ui/pickers/sessions.rs) owns the key and
filter state for the startup and in-app pickers. In-app drawing uses the
renderer-owned writer and checked geometry; the startup picker owns and restores
its terminal attachment before any provider is constructed.

[`src/session/title.rs`](../../src/session/title.rs) captures title model prices
and usage independently of conversation context calibration. Its provider
stream observer records reported usage before validating the text or rejecting
unexpected tool calls; there is no title tool dispatcher. The App retains a
join handle, session ID, generation, and captured workspace for each title task,
and cancels/joins it before replacement, worktree changes, or teardown. Late
results cannot title a successor session. Headless title work is awaited after
the actual turn is saved; observed usage survives empty answers, errors, and
interruption. Auxiliary titles use the shared passive provider-parameter filter:
raw tool/MCP/container capabilities, unknown gateway fields, retained server
state, messages, model/cap overrides, and native format injection never reach
the title wire; typed reasoning, generation, routing, and privacy settings remain.
Declared OpenRouter routing/privacy constraints are validated before any title
request; malformed or unsupported values, or use with another protocol, fail closed and keep the
first-message fallback without inventing usage.
See [CONFIG.md](CONFIG.md) for the default model and opt-out.

## Native Git command proposals

`src/ui/slash/git.rs` owns parsing, bounded request-path selection, proposal
validation and SDK-free native actors for `/diff`, `/commit` and `/pr`. The App
owns the preview, async event receiver and captured session/workspace identity.
Actors enter an `AgentWorkScope` before spawn and retain cleanup ownership through
receiver settlement. Cancellation denies pending requests and settles the exact
actor before auxiliary usage or a successor can be admitted. A cleanup failure
retains the work receiver and blocks a new run.

A per-invocation `FrozenTool` forwards structured calls through ordinary hooks and
captures the inner immutable result before post-hook display redaction. It rejects
pre-hook changes to the confirmed JSON operands; status guards and mutation
success checks consume that captured result. Selected staging uses nonrecursive
`git update-index --add --remove --` after the existing path, permission and
external-filter checks; selected commits use `git commit --only --`. Guards refuse
unrelated staged paths before and after staging. Concurrent external Git/filesystem
writers are not a transaction lock; users should inspect state after an interrupted
or refused command rather than assume rollback.

With `git-worktree` compiled, `ShellTool` adds an immutable `ForegroundResult` structured extension containing
actual completion status, exit code, descendant cleanup state and at most 64 KiB
of stdout with an explicit completeness bit. Text rendering and background job
behavior are unchanged. Resource-limit errors supply no successful foreground
extension. The PR consumer accepts only completed exit-zero results without escaped
descendants and with complete UTF-8 stdout containing exactly one HTTP(S) URL.
Hooks may redact model/display text while the captured inner extension remains
the authority for that decision. Metadata is invocation-local; no ambient result
registry or filename/model-text inference is used.

## Read-only context composition

`src/ui/slash/context.rs` implements `/context` from immutable agent-build
metadata and retained `Session` estimates. The production preamble composer
records contiguous UTF-8 byte ranges while constructing the actual text, then
captures its token estimates on `AnyAgent`. Capture does not change preamble
bytes, cache-prefix order or provider calibration. `Session.overhead_parts` is
runtime-only; imported JSON cannot supply these diagnostic labels.

The diagnostic reads static definitions through the existing SDK tool-server
handle with `get_tool_defs(None)`, which excludes vector retrieval and tool
execution. It neither rebuilds an agent nor connects MCP. A timeout bounds the
await; count, depth, node and final JSON-byte limits bound accepted metadata
encoding. The SDK may clone already-registered metadata before those encoding
checks. Unavailable or excessive schemas remain visibly unavailable.

The diagnostic emits no prompt, message or output content and opens no files
or spills. Hidden redo is not inspected. Projection reuses retained in-memory
metadata and the normal pruning estimator. The five largest live result estimates carry only bounded sanitized names and
IDs. A signed adjustment reconciles per-section local estimates with the
existing effective-context total. The command changes no history, calibration,
costs, permissions, pruning or persistence and bypasses the ordinary slash
command model-cache warming and session-save steps.

### Native Windows PR acceptance fixture

The guarded `windows-default-compile` CI step builds the test-only
`tests/fixtures/windows_gh_fixture.rs` image through Cargo's test target and exact
JSON artifact discovery. It never installs another production binary or contacts
GitHub. Ordinary test execution is inert; the native image acts as GH only under
an explicit owned fixture role. Five ignored Windows cases exercise literal
Unicode/quotes/metacharacters through the retained Windows PowerShell
`ProcessStartInfo` path, normal Shell Ask/deny, immutable foreground status and
bounded completeness, post-hook display redaction, and exact descendant Job
retirement before a controlled late effect is released. Fixture roots and actors
are owned and bounded.

These cases explicitly select the supported trusted `--no-sandbox` Shell policy;
there is no Git execution and no fallback from a required AppContainer boundary.
They establish native argv/result/cancellation authority, alongside the unchanged
standard-user AppContainer and worker-containment witnesses. The Windows proof
remains pending until the exact hosted step passes; macOS execution and pure
argument-encoder tests are separate evidence.
