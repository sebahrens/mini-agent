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
bounded completeness, post-hook display redaction, and required descendant
retirement before a controlled late effect is released. Fixture roots and actors
are owned and bounded.

These cases explicitly select the supported trusted `--no-sandbox` Shell policy;
there is no Git execution and no fallback from a required AppContainer boundary.
They require native argv/result/cancellation authority, alongside the unchanged
standard-user AppContainer and worker-containment witnesses. The Windows proof
remains pending until the exact hosted step passes; macOS execution and pure
argument-encoder tests are separate evidence.

The explicit disabled foreground path currently owns the direct child but does
not attach the Job owner used by AppContainer helpers and trusted hooks. Its
descendant retirement assertion remains a failing acceptance requirement; the
bypass is not documented as providing that Job guarantee. The hosted diagnostic reached only `NATIVE32_STAGE entered` before the first
`New-Object` initialization exceeded its unchanged 15-second command bound.
The PR command now constructs `ProcessStartInfo` and `Process` directly with
.NET `::new()` and obtains the current FileSystem provider location from
`$ExecutionContext.SessionState.Path.CurrentFileSystemLocation.ProviderPath`.
Literal argv encoding, normal Shell Ask, async stdout/stderr drains, exit status,
finally disposal and foreground ownership are unchanged. Microsoft documents
[direct constructors](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_object_creation)
and the [FileSystem location](https://learn.microsoft.com/en-us/dotnet/api/system.management.automation.pathintrinsics.currentfilesystemlocation)
/[provider path](https://learn.microsoft.com/en-us/dotnet/api/system.management.automation.pathinfo.providerpath)
used by this command.

The typed case first runs the separately asked literal preflight and an independent
inert-helper direct-constructor control. The archived cmdlet variant and exact
failed hosted transcript remain diagnostic evidence; its known timeout no longer
prevents the independent positive or five real PR cases from executing. The inert
control explicitly attaches its constructed `StartInfo` to `Process`, uses the
same FileSystem location as production, and invokes only the copied helper's
pure `--self-test`. Fixed stage and role-validation labels stay outside the real
PR script. Failed assertions retain only no-follow bounded owned marker files,
copied-image size and bounded actual results. All existing readiness, command,
settlement and cleanup bounds remain. The exact five native cases must still pass
on a fresh hosted Windows run; no native repair or descendant guarantee is claimed
from pure or macOS checks.

### Cached LSP diagnostic metadata

When `lsp` is compiled, the built `AnyAgent` retains the same `LspManager`
used by its registered LSP, edit and write tools. Its diagnostic snapshot reads
only configured names, retained client names and cached failure cooldowns.
It never opens documents, starts or reconnects a server, or sends an RPC.
Retained client entries describe cached state rather than a fresh health probe.
Both runtime caches use `try_lock`; an unavailable cache is reported separately
from an empty cache. Total counts remain visible while each name list is capped
at 32 entries and names at 128 Unicode characters with terminal controls removed.
Commands, arguments, environment values and diagnostic/document contents are
excluded. Tool-less or ineligible builds retain no LSP manager.

### Planning approval ownership

`src/ui/plan.rs` holds ephemeral planning-run capture and literal approval
state, owned by the App's `ChainState`. The planning prompt must explicitly
carry `%%mode=planwrite` and trusted `%%next=code` directives. Actual successful
`Done` handling settles its exact runner before offering approval. A response
larger than 64 KiB or relevant serialized history larger than 1 MiB is refused
rather than truncated. Approval binds the session ID, captured workspace object
and its validation, prompt/persona selection, plan and code prompt contents,
operator ceiling, and the relevant visible turn/compaction revision. Titles
and auxiliary charges are excluded from that revision.

Y selects the code prompt through the ordinary prompt/model path and submits
the exact approved text through ordinary main-turn admission. It never grants
permissions, bypasses spending limits, rereads a plan file, or accepts model
text as a user decision. N/cancel stay in planning; E edits bounded text and
requires a fresh Y after preview. Pending tool permission, unsettled native
work, or an unfinished goal blocks execution. Interrupted/failed turns clear
capture; imported/replayed sessions cannot reconstruct approval state. Other
chain and Git confirmation ownership and their heap-construction boundaries
remain separate. The headless and ACP surfaces keep their existing explicit
prompt submission semantics and do not acquire an implicit approval path.

### Operator diagnostics

`src/diagnostics.rs` assembles a 64 KiB report from captured state, with labels
limited to 192 Unicode characters and terminal controls removed.
`src/ui/slash/doctor.rs` reads the current session and retained managers. Its
read-only routing shares `/context`'s busy admission and bypasses ordinary
post-command settlement, cache warming, goal kickstart and session persistence.
Cached sandbox availability uses `OnceLock::get`, worker status uses `try_read`,
and hook/manager counters use nonblocking cache locks. Unknown or busy state is
reported explicitly, including the distinction between enabled protocols and
confirmed terminal support. No diagnostic getter launches a process or RPC.

The CLI branch runs before storage preparation and ordinary configuration
initialization. Its pure loader preserves the usual config candidate order,
benign project settings and content-bound trust while declining to create or
repair private files. Provenance stores only catalogue key names and layer
paths, is excluded from serialization, and never stores secret values. Actual
`AuthResolver` precedence chooses the reported source, including isolated custom
routes and typed Anthropic token handling. Optional bounded immediate-directory
and LSP executable metadata checks are CLI-only; the TUI performs neither.
## Recorded-spend admission

`spend::run::Run` is parent-owned request provenance carried through exact SDK
tool extensions and owned tasks. The ordinary concurrency wrapper checks the
logical-session USD ceiling after lane acquisition, and the shared permission
helper checks again after a reply before installing a session grant. A refusal
executes no tool and forwards no successful mutation extension. The JS broker
rechecks before preparation and after approval/audit-lock waits, before durable
intent. No worker financial authority or Phase 6 containment invariant changes.
Already dispatched work keeps its existing cancellation/reaping ownership;
completion and observed usage are retained. See [COST_TRACKING.md](COST_TRACKING.md)
for pricing, private-ledger bounds, failure handling and the reported-estimate
limit. Existing command classes, deadlines, URL policy and permission precedence
are preserved.
`src/tests/ledger_lock_tests.rs` exercises acquired ledger-lock retirement with
an owned Unix child held before exec. Explicit unlock ends the lock's ownership
before receipt acknowledgement; inherited descriptors cannot extend it after
the ledger operation completes. Corrupt-tail failures retain bytes and charges.

### Notebook cells

`src/agent/tools/notebook.rs` implements `notebook_edit` and the cell view used
by `read` for `.ipynb` files. Notebook source is literal data; these operations
never run a kernel, shell, or cell. The builtin name is reserved against MCP
shadowing and included in cold context schema estimates. Select exactly one
zero-based `cell_index`
or `cell_id`. `replace` updates source while preserving type, execution count,
outputs, attachments, metadata and unknown fields. `insert` adds before the
selected cell (index equal to the cell count appends), with an optional
`cell_type` of `code`, `markdown` or `raw`; `delete` removes that cell. Source
strings remain strings, and source arrays remain arrays. Inserts into notebook
4.0–4.4 do not mint cell IDs or change the format version; 4.5+ requires valid
unique IDs and receives a new ID on insertion.

Untouched JSON field payloads remain validated raw JSON, preserving large
integers and opaque metadata. Opaque nesting is not recursively traversed;
byte caps, rather than an assumed semantic nesting limit, bound its handling.
Notebook input and resulting serialization must fit `max_text_file_size`
(default 1 MiB), with an absolute 8 MiB ceiling. There are at most 4,096 cells,
256 interpreted fields per notebook/cell/output object, 1,024 bytes per field
name and 1 MiB of decoded source per cell. Duplicate interpreted object fields,
invalid UTF-8 and ambiguous cell IDs are rejected. Unselected unknown cell
types are preserved; mutation of an unknown type is refused.

The default `read` view shows virtual cell/source/output lines. Its existing
`offset`/`limit` arguments address those virtual lines, and the limit is capped
by `max_read_lines`. Returned text is capped at 64 KiB (or the configured text
cap, if smaller); text outputs have separate 4 KiB per-cell and 16 KiB overall
limits with explicit omissions, and at most 4,096 output objects per cell.
Opaque rich output payloads are omitted. This view provides no raw line hashes,
raw-file CRC or whole-file overwrite authorization. Set `raw_notebook: true`
to use the ordinary raw JSON line reader and its existing edit/write contract.

`notebook_edit` uses its own permission name, with matching configured `edit`
rules as fallback when no notebook-specific rule matches. Either tool's deny
rules apply before approvals, hooks and Yolo; prior runtime `edit` grants do
not become notebook grants. Unmatched Standard asks unless the configured
default denies; ReadOnly/PlanWrite refuse unmatched notebook edits. Explicit notebook approval uses the normal single-file
approval path and mandatory concurrency/spend wrappers. Mutation authority does
not confer read disclosure: unchecked Ask or denied reads suppress preimages,
neighbor previews and typed diffs, including equality hints for no-op source.

Workspace publication retains the captured regular-file identity, exact
content digest and parent capability, with the existing atomic exchange and
displaced-content rollback. External publication uses the existing file-edit
approved-parent and exact-content contract. Blocking preparation and publication remain owned jobs.
Cancellation wins a shared atomic decision before publication, preventing the
exchange; an already-started publication remains owned until it settles and may
have an ambiguous caller outcome. External aliases and approved parent identity
are revalidated; symlink/FIFO workspace targets are refused. No file creation,
implicit permission grant or notebook execution occurs.

## Terminal code presentation

`src/ui/highlight.rs` adds bounded lexical foreground spans to fenced Markdown.
The feed stores plain text plus ephemeral spans, preserving exact multiline state
at streamed fence boundaries and caching tokens independently of terminal width.
Unknown fence labels and oversized blocks use the ordinary code foreground;
monochrome mode skips tokenization. Theme changes invalidate token and layout caches.

Common Rust, C-family, JavaScript/TypeScript, Python, JSON, TOML, shell and diff/patch
labels are recognized. This is lexical presentation, with no code execution or
loaded grammar files. Model-authored diff fences do not become mutation metadata;
actual typed mutation rows retain their existing event path and colors. Limits and
the engine/size tradeoff are recorded in
[the syntax decision](../decisions/2026-10-13-bounded-terminal-syntax.md).

### Project note summaries

The memory feature appends a bounded project-note name/first-line index after the original escaped memory sections, within their remaining 32 KiB context budget. The directory scan and regular-file opens reuse captured filesystem capabilities; no links, FIFO blocking opens, note-body injection or new permissions are introduced. `append_facts` is an explicit note-only mode using long-term normalized line deduplication, while ordinary note appends retain Markdown/code formatting. See [MEMORY.md](MEMORY.md) for exact limits and the next-turn refresh behavior.

## Native output and opaque-thinking transport probes

`src/provider_effort_tests.rs` owns the bounded native Anthropic SDK wire
fixtures for output-schema/effort composition and completed empty signed
thinking. They use fake loopback responses, preserve owned token caps, reject
malformed effort before HTTP, and distinguish a completed opaque block from an
interrupted signature. The pinned SDK backports are documented in
`patched-deps/rig-core/PATCH.md`; they do not change permissions or tool dispatch.
These transport controls do not establish application history-prefix stability
or live effort support: frontend replay and mutable-context placement require
separate actual request-body proofs before those features are described as
delivered. Local reasoning display remains independent of provider computation.
The native cache-usage fixtures in `src/provider_cache_tests.rs` use the same
bounded owned HTTP fixture. They verify that message-start cache counts survive
output-only deltas, cumulative updates replace rather than add, and invalid
reported tier sums are rejected. The retained raw five-minute/one-hour split is
provider-reported data; it is never inferred from the requested cache TTL.
This transport correction does not enable a one-hour configuration or change
memory placement. Application tier pricing and durable receipt compatibility
are reviewed separately before that option is exposed.

`src/provider_effort_replay_probe.rs` owns the native completed-content order
fixtures. Actual SDK follow-up requests preserve completed native thinking,
redacted, text, and tool block order; partial thinking remains display-only.
Other provider aggregation and tool-batch validation remain unchanged. The
fixture also records the application's current history conversion separately;
SDK projection support alone does not imply saved-history replay support.

### Native completed-content capture (partial mini-agent-i7ro6.46)

`src/agent/native_capture.rs` owns a bounded, explicitly passed collection of
native Anthropic `ModelTurnFinished` content. The typed Anthropic `AnyAgent`
branch installs its observer on initial/retry streams and on runner-generated
continuations, before the financial admission hook; the observer always returns
`Continue` and adds no accounting observer, background job, or security grant.
Other typed provider branches use an empty observer. Frontends do not yet enable
this collection or replay it into saved sessions: prefix compatibility and
live/resumed replay remain separate unfinished work.

The collection preserves completed block order and exact SDK run/turn keys. A
canonical assistant `ToolCall.id`, the optional provider call ID, and Rig's
internal event ID remain distinct. Bijection checks use the canonical ID rather
than tool names, arguments, callback order, or an inferred provider ID. Limits
are 128 turns, 256 blocks per turn, 2 MiB of serialized content per turn, 16 MiB
of captured content/identity payload in total, and 512 bytes per identity.
Overflow, conflicting duplicate observations, recovered tools, or unavailable
call identity mark the collection unavailable without truncating it into a valid
replay prefix or suppressing ordinary effect/accounting settlement. An empty
collection or absence of an error marker does not prove a completed transport,
a compatible request prefix, or permission to reuse opaque content.

Native Anthropic reasoning display uses nonempty streamed summary deltas.
Completed signed and redacted blocks enter the transcript and optional canonical
capture independently of presentation; their signature and opaque payload bytes
are never rendered as reasoning. The typed native route selects this policy even
when canonical capture is disabled. Generic reasoning display is unchanged.
`src/provider_effort_replay_probe.rs` exercises omitted and nonempty native
summaries through the ordinary interactive runner with capture enabled and
disabled, checking one displayed summary and retained opaque blocks. This display
rule does not enable saved-session opaque replay or promise prefix compatibility.


### Windows foreground ownership experiment

The test-only module `src/tests/windows_foreground_job_tests.rs`, registered under
`sandbox::windows`, probes a general command created suspended, assigned to a retained
kill-on-close Job, and resumed only after an exact initial-thread and membership check.
Its owned inert GH image exercises setup failure, cancellation before resume, and family
retirement after both a waiting and an already-exited leader. The existing Windows CI
GH lane discovers exactly three prototype tests before running all five ordinary permission
and foreground-result acceptance cases.

The prototype retains the actual helper process handle after checking its Job
membership. Retirement requires both zero Job accounting and the descendant
handle's termination signal within the same five-second boundary. Job termination
initiates asynchronous process shutdown; zero accounting alone cannot replace
the retained process witness. Assertion unwinding retains those same capabilities
through bounded cleanup. This corrected prototype still requires hosted native
execution; it does not relax the ordinary Shell cancellation assertion.

The third test experiments with a retirement-only active-process limit of zero.
It requires the two existing members to remain alive and enumerable, rejects a
new actual .NET child launch with `ERROR_NOT_ENOUGH_QUOTA`, then checks both
retained process signals and zero Job accounting after retirement. This is a
candidate membership-snapshot fence, not a production policy. Until hosted
Windows proves every witness, its behavior remains unverified and production
must not assume zero is supported or that it prevents a new-child race.

This module is an experiment for mini-agent-i7ro6.108. Production disabled Shell does not
yet use it; the observed descendant-settlement gap remains open. A successful prototype
does not prove production integration or atomic recovery from a parent crash between
creation and assignment. The broker-only worker and AppContainer launcher are unchanged.


### ACP server idle exit

`--acp --idle-exit <positive-seconds>` exits successfully after the configured
quiet interval with no loaded sessions and no active requests or jobs. The
interval restarts when the final owned task settles. A dormant ACP session
retains its history and prevents this exit until `session/close`; sessions are
not silently expired. Admission and idle shutdown are ordered under one lock.

`src/extras/acp/idle.rs` tracks request and whole prompt-task ownership through
verification, receipts, history and terminal cleanup. Permission/question
bridges and both plaintext and TLS authentication tasks retain their own
activity until actual cleanup, including cancellation before first poll. The
listener waiting for its first peer is idle. Enabled transports drain these
owners after connection teardown. Without the option the existing lifecycle
is unchanged. Provider stream-stall timeouts remain independent.

`--loop` accepts this server option, but its existing finite goal driver exits
when it stops; it has no parked idle listener. Active rounds, validation and
judge requests are never terminated by `--idle-exit`, and their stop outcomes
and persistence keep their existing semantics.

### Nested workspace instruction delivery

`src/context/instructions.rs` owns captured instruction sources and private
native-tool notice metadata. Its delivery acknowledgement is runner-local in
Rig's existing hook scratchpad, carried explicitly to native tools. The
prepared-request hook is observe-only and runs after merged request patches;
accepted model turns acknowledge only notices present in that exact request.
A mutation denied pending instruction review has made no change and carries no
mutation diff. Read permission and target overwrite proof remain independent.
Reused canonical tool IDs retire earlier private receipts before the new
effective result is considered, so copied result bytes cannot restore a pruned
receipt. Canonical IDs are correlation data, not permission or replay authority.

Readonly native `/btw` and Task child agents use the same captured instruction
source service, with their actual provider protocol selected separately. Each
SDK invocation keeps its own delivery acknowledgements. Task-only parents still
capture the service; `--no-context-files` explicitly disables it for all native
file-tool consumers. Readonly children retain their existing narrowed tools and
permission, spend, cancellation and clarification ownership. Native child
requests also receive the configured response `max_tokens` (the normal 16384
default); a captured parent CLI ceiling and a narrower child configuration limit
both remain in force. This fixes native Anthropic child requests previously
rejected before HTTP for a missing required response limit.

Initial `CLAUDE.local.md` and authored `@path` / `@{path with spaces}` includes are
captured from each root instruction file's original retained directory. Includes
stay within that root, never follow symlinks or multiply-linked files, use at
most four include levels and 32 visited files, and reject missing explicit
includes or invalid/oversized UTF-8. A source is at most 16 KiB; newly captured
sources use at most 128 KiB and the remainder of the existing combined 512 KiB
context-file budget, measured after JSON escaping. Existing root context keeps
its earlier budget priority. Cycles and repeated canonical paths are deduplicated
without changing authored include or outer-to-inner scope order. Ambient parents
are not loaded. Global includes keep their captured global origin. Project
local/include sources require the existing exact project-config trust and
side-effect-free read Allow authority; unchecked Ask/Deny sources never prompt
for an implicit grant. Initial snapshots revalidate that authority before
provider dispatch without rereading instruction bodies.

Each lazy notice, including its JSON and framing, is capped at 16 KiB. An
oversized notice fails visibly without enabling a mutation. Ordinary result
redaction or spilling can also make a notice unavailable for acknowledgement;
the instruction hook never restores removed bytes or bypasses those limits.
Gemini's documented `systemInstruction` and `system_instruction` aliases are
both transcript overrides. Responses' closed request-parameter type discards
unknown `input`/`instructions` extras, and Ollama keeps extras within options;
the guard follows actual typed backend behavior rather than key-name guesses.

`/context` inspects only bounded metadata from the actual agent's source cache:
source paths, origin/scope, byte counts, fixed failure counts and notice count.
It never opens files, displays instruction bodies, or treats loaded snapshots or
notice counts as delivery acknowledgements. Busy metadata is unavailable rather
than blocking the UI. Snapshots remain stable until agent rebuild; changing or
revoking source authority cannot authorize a later mutation from stale delivery.


### MCP auxiliary requests and forms

The `mcp` feature also owns advertised prompt/resource catalogues and selected
text RPCs in `src/extras/mcp/catalogue.rs`. Operator selection uses the existing
MCP work actor and an exact session/workspace/generation permission origin.
Runtime resource snapshots enter new literal user messages; older request
prefixes are preserved. `src/extras/mcp/forms.rs` validates raw schema constraints
before rmcp decoding, and `elicitation.rs` binds callbacks to one live RPC's
captured Question/Run/work scope. Form answers never mint grants or a persisted
main question. Headless declines forms and ACP cancels unsupported forms; URL elicitation and sampling are
unsupported. Both transports keep bounded raw schema observations. See
[MCP prompts, resources and forms](MCP_INTERACTION.md) for limits and UI controls.
