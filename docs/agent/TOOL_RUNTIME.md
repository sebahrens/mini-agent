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

Lifecycle-only hook configurations select a small tool delegate before the
larger pre/post/failure pipeline is constructed or polled. The immutable event
configuration determines that choice for all three ToolDyn call forms. Inner
tool construction still starts on the delegate's first poll, so dropping an
unpolled call starts no inner work; arguments, extensions and structured
outcomes pass through unchanged. Configured tool hooks retain the existing
permission and post-hook behavior through the owned heap-future factory.

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

Linux general-process commands mount the retained workspace at its canonical real
path, after runtime readonly mounts and the temporary filesystem. Absolute paths,
`pwd`, and build artifacts therefore use the same workspace coordinates as the
file tools. Captured symlink ancestor spellings are recreated without mounting
their host parents; changed links refuse launch. Scripts and output are never
translated. Only explicit namespace/control-path collisions use the `/workspace`
fallback, which the system prompt and shell description announce. Filesystem
root `/` remains refused. Ordinary setup errors do not trigger a fallback.

`sandbox/workspace_layout.rs` owns this bounded general-process mount layout;
`paths/workspace_spelling.rs` retains Linux spelling/link metadata alongside the
existing canonical workspace capability. Neither changes file-tool permission
roots. Both writable `--bind-fd` and readonly `--ro-bind-fd` support are required:
bubblewrap checks the mounted workspace identity and consumes descriptor 197
before executing model code. The immutable brokered-executable path supplies the
same workspace descriptor and independently consumes its sealed fd 3 image.
Git metadata masks follow the canonical workspace bind. The broker-only JavaScript
worker still uses its separate empty-root profile and receives no workspace mount.

The Linux-only test module `sandbox/linux_workspace_tests.rs` owns native real-path,
spelling, fallback, replacement, cancellation, and build-artifact controls. Together
with the immutable-image fd197 collision control and an actual offline headless
shell/read parity test, these are seven lib tests plus one integration test. They
are explicitly ignored by ordinary test runs and must be run with `--ignored` on
a descriptor-capable native bubblewrap host; unavailable containment is a failure,
not a successful skip. Fixture setup stages the identical Rust toolchain under
the existing readonly `/usr` allowance and creates only owned UUID descendants.

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

### Instruction package discovery

`extras/skills/retrieval.rs` owns pure metadata and the shared bounded lexical
normalization; it has no learned-JS database, embedding backend, or worker
dependency. `extras/skills/discovery.rs` reads explicitly captured source roots
through retained no-follow directory/file capabilities, within one aggregate
entry/content budget. Project sources reuse the existing captured project config
trust and ordinary read authority. Discovered content-addressed snapshots use
private source-specific cache namespaces and cannot repoint the explicit imported
Agent Skills catalogue's `ACTIVE` pointers. Neither frontmatter `allowed-tools`
nor bundled resources grants authority or executes code.

`extras/skills/session.rs` owns captured instruction services and one captured pin/generation per ordinary
turn, including source revalidation. An intact successful host `skills_search` result can refreeze
that turn’s selection; redaction or spill rewriting does not install the candidate. Its single `InstructionContextHook` composes
plain instructions or the existing learned-JS manifest as an ephemeral system patch. The
instruction-only `extras/skills/search_tool.rs` exposes bounded metadata through the ordinary
mandatory tool wrappers; learned builds retain their existing same-turn search and lifecycle.

`extras/skills/selection.rs` owns the shared instruction/resource composition and post-escape budget;
`extras/skills/retrieval.rs` owns the same bounded whitespace-normalized query key used by the learned
embedder. Unknown or unavailable learned embedding services keep instruction discovery lexical.
The prepared-request observer rejects transcript/system overrides that would erase selected context;
it does not claim that a typed request attests arbitrary provider wire overrides.

For the actual native Anthropic adapter, the initial selected instruction/learned block stays fixed
for the captured turn. Later successful `skills_search` calls carry new bounded guidance in their
actual canonical tool result, under the static instruction policy; system and tool definitions stay
unchanged before retained signed thinking. This is not opaque replay across frontend turns (that
separate lifecycle is still governed by native history compatibility). Redaction, spill rewriting,
or removal of an expected native result stops locally before another model request and never
restores removed bytes. The initial block is at most 64 KiB, search metadata 8 KiB, a complete native
search result 80 KiB, and the exact runner retains at most 16 delivery receipts. These are consistency
checks, never permission grants or cryptographic signature verification. Combined learned/instruction
search builds a staged bundle without changing callable lookup, canary routing or attribution. Only
that captured turn's intact successful final host result commits both selections, once; redacted or
spilled results leave the previous live bundle unchanged. A cancelled or retired-turn candidate cannot
replace a later turn. Existing learned ABI, lease, scope and admission checks still apply; publication
is not a new script approval path.

### Windows general command family ownership

`src/sandbox/windows/general_owner.rs` owns ordinary Windows commands separately
from the AppContainer helper and broker-only JS worker launchers. The actual
constructor labels helper commands; a backend name (including disabled
`appcontainer`) does not identify a helper. Captured output, status, background
commands and contained Git commands retain this label through execution.

General commands are created suspended under the crate-wide creation lock,
assigned to a retained kill-on-close Job, and registered before their uniquely
identified initial thread resumes. Setup consumes the original command deadline
and rechecks cancellation and captured spend admission. These general constructors
author no extra Windows creation flags; the separately labelled AppContainer
helper retains its existing breakaway flags and launcher contract.

Retirement first freezes Job membership with the native-tested zero active-process
limit, retains up to 4,096 exact member handles, then terminates the Job. Success
requires the leader and every captured member to signal termination and Job
accounting to reach zero within the existing two-second family drain budget.
Query, capture or drain failure remains a failed settlement with the retained
owner; `kill_active` signals that same owner and cannot erase its accounting or
publish successful cleanup. This is process-family ownership, not a new sandbox
permission, and does not claim a crash-atomic boundary before Job association.

The hosted inert zero-limit probe passed on CI run 38509643944, job 115674985903.
Production adoption additionally requires the unchanged five native PR/Shell
cases and the exact constructor-failure, cancel/leader-exit, and status/background owner controls;
local macOS tests cannot establish that Windows acceptance.

Detached status/background workers retain the launch-admission `Run` solely for
captured spend checks, including changes while waiting for the creation lock.
They do not inherit a different `AgentWorkScope` or change the intentional
background lifetime/cancellation policy. The native held-lock negative uses a
real `WouldBlock` observation, then a controlled accounting receipt, to require
zero executed marker and exact suspended-owner cleanup before terminal failure.
It does not describe that controlled receipt as provider-observed usage.

The actual Tokio helper guard also rechecks this captured account after its
creation-lock wait and before process creation. The same native negative covers
that called guard primitive with an inert command; it does not substitute for
AppContainer runtime containment proofs, which retain their existing launcher
and broker contracts.

The native spend-wait test requires two Tokio runtime threads: one can wait on
actual process creation while the other records the controlled spend change.
The development Tokio dependency therefore enables `rt-multi-thread` alongside
`test-util`, including in minimal feature test rows. This supports the test
fixture only; production Tokio features, defaults and process ownership policy
remain unchanged. The fourth owner control is required in the exact native row.

The manual CI scope `windows-command-family` runs the existing Windows default
compilation job, including four physical App stack controls, three suspended Job
prototypes, four production owner controls and the unchanged five native PR/Shell
cases with their exact discovery guards. It also retains the three ledger
controls in that job. Other matrix jobs skip, and `ci-success` is not published:
a partial native proof cannot substitute for a full push/PR or manual `full` run.
Pushes, pull requests and unresolved diffs retain their existing full-matrix policy.

The learned search continuation regression binds the same production
`InstructionTurn` and `InstructionContextHook` as the builder, with retained
user skill packages. It checks the actual intact canonical tool receipt and
next-request replacement, rather than a test-only context hook or unbound
publication. Its roots are owned independently and retired on test unwinding.


`src/ui/slash/jobs.rs` exposes the existing session-owned background registry to
operators. `/jobs` uses bounded command metadata, without copying job output;
`/jobs stop <id>` requests the existing job token's cancellation and keeps the
registry and process owner intact. Native completion remains the sandbox's responsibility.

Busy `/help`, bare `/model`, and bare `/sessions` views bypass generic model
warming and session persistence. Session metadata is read by the existing tracked
blocking-work scope through retained regular-file handles and pure private-file/
directory validation; neither absent storage nor permissive modes are repaired.
Idle resume pickers retain their normal private reload behavior, and stale picker
selection/reload is rejected while a turn is active. Numeric todo/queue status comes
from the shared TodoStore and actual run queue, rather than parsing model output.
`/add` completion uses the existing bounded workspace FilePicker, inserts a literal
quoted operand, and never invokes the model or treats a selected name as a directive.

`/add` completion captures the actual `UiContext.workspace` on each input event
and refresh, independently of process cwd. Its bounded ignore-aware walk uses
that canonical root and validates the retained binding before walking, publishing,
and accepting batches; a workspace replacement or switch retires the old picker.
Only filesystem completion entries enter this operand picker, with no MCP resource
strings. Selection supplies a relative literal to the existing explicit operator
`/add` attachment path; it does not install a model read permission or change the
existing operator attachment policy.

Native Anthropic instruction composition uses an authored deterministic view before request preparation.
It retains ordered content-addressed package bodies, escaped resource bytes and inventory,
and learned callable IDs, exports, descriptions and capability tiers. Runtime catalogue
generations and retrieval scores/ranks stay in discovery metadata rather than the native
system block. Generic rendering keeps those decorations. The instruction prepared observer
still checks the composed system prefix and expected skill receipt before HTTP; it never
strips metadata from an already prepared request. A captured native turn keeps its initial
system context fixed; successful skill searches carry changed guidance in canonical tool
results. Full persisted native replay and cross-frontend system/tools/history compatibility
remain separate controller work.

Captured App test renderers scope all picker overlays through the same owned byte
sink and fixed geometry (`ui/pickers/test_surface.rs`, test-only). The synchronous
per-thread guard restores its predecessor on success or unwind. Captured cursor
and clear commands use Crossterm’s public ANSI serializer, without querying or
executing native console commands. Native picker geometry/stdout calls and errors
remain unchanged outside that test capture.

### Live model catalogue metadata

`src/provider_listing.rs` performs metadata GETs through the actual retained
provider client. `/models refresh` supports native Anthropic, both OpenAI API
styles, Gemini, OpenRouter and Ollama, including configured native aliases.
The complete operation has a ten-second deadline and limits of 32 pages,
8 MiB of response data and 10,000 entries. Invalid/repeated pagination, body
limits and failed responses preserve the previous complete cache. Errors omit
response bodies, credentials and endpoint values. Listing never issues a model
completion request or grants tool, image, permission or replay capabilities.

Cached results bind the actual endpoint, authentication headers and original
configured HTTP backend; client clones share this identity. New backends and
other routes cannot borrow its live prices. Inactive backend entries are
pruned; bounded cache admission refuses a new oversized result rather than
silently evicting a live route's prices. Startup retains the offline embedded
catalogue for baked providers. Explicit refresh changes picker metadata only,
not active session capacity, captured request prices or accounting.

Anthropic `max_input_tokens` and Gemini `inputTokenLimit` supply live input
capacity; Gemini removes the `models/` prefix once and keeps the precise
completion model ID, listing only `generateContent` models. OpenAI model-list
responses have no standardized capacity field, so catalogue and local pins
remain authoritative there. Published display names are escaped and limited to
4 KiB; oversized escaped names reject the complete refresh. Catalogue prices (including explicit zero) remain
available; IDs without known prices show `(no price known)`. Native listing
responses do not invent pricing. Raw model selection resolves actual-route
OpenRouter prices, then catalogue prices, configured known prices, then
unknown. Deliberate model selection uses a global context pin, a selected
alias pin, a deterministic matching provider/model alias pin, matching live
input capacity, catalogue capacity, then the existing 128k fallback. Offline
Doctor labels catalogue pricing as lacking a captured listing route; runtime
Doctor continues to show the session's captured rates.

## Optional post-commit formatters

`agent::tools::format_after_edit` implements `[format]` for native `write` and
`edit`. The existing permission/instruction gate and guarded atomic publication
remain the first mutation. After success, the target's read proof is invalidated
before any formatter approval or process work. The actual builder attaches this
service only when its registered shell capability is eligible; readonly child/BTW
tools and CLI-excluded shell do not acquire a hidden execution surface.

Templates tokenize literal argv before canonical `{file}` substitution. The
complete reconstructed script is both the normal shell permission key and the
executed script. Once approval and current effect admission finish, retained
workspace, parent, file identity and content digest are revalidated before
dispatch. The existing Sandbox output worker owns the family through termination
and reap, with 10 seconds and a combined 1 MiB output cap. No new worker/process
registry, grants, mount policy or CAS relaxation is introduced.

Formatting preserves the original workspace/path context. It is separately
authorized ordinary shell execution, so arbitrary formatter programs need not
be file-local or atomic. A legitimate formatter can replace the target inode;
a post-retirement nofollow regular-file observation validates the current target
through the retained root/parent and bounds UTF-8 content by the smaller of the
configured file-tool limit and an independent 8 MiB observation ceiling. This
ceiling does not change the ordinary edit limit.
Concurrent changes are reported only as observed final content. Missing, changed
parent/root, oversized or non-UTF-8 observations omit stale final metadata.

Success and modifying failure both use final content for replacement diffs and
bounded excerpts before LSP notification. Each disclosure requires fresh existing
READ-Allow, without a prompt, hook token, doom-loop count or read-tracker record.
Private source and formatter stdout/stderr never enter results. Without a valid
authorized snapshot, results retain caller-authored mutation success plus static
formatter status. Dropping a cancelled tool future cannot promise a reply, but the
original committed bytes are not rolled back and its scoped process owner retains
the existing retirement/forced-exit contract. An empty map preserves legacy tool
output exactly.

Native replay compatibility is recorded separately from the display feed in a
bounded, protocol-specific envelope. The shared canonical commit binds the actual
adapter, Session ID, preappend feed, cutoff, and compactions; after exact actor
retirement it projects observed usage at captured prices without creating another
receipt. Accepted native content that cannot attach advances a shared cutoff and
persists a closed static refusal. Same-session clones retain denial; forks inherit
an independent denied cell. Healthy legacy numeric cutoffs remain compatible.
These helpers confer no permissions, instruction receipts, or runtime ownership.
Frontend enablement and explicit durable fresh-context recovery are separate work.

The lower native replay checkpoint fingerprints the exact preappend Session rows and compactions. Same-cutoff accepted progress that cannot attach persists a closed local refusal; a stale foreign-session or newer-cutoff binding cannot fence its successor. Retirement and refusal share one coherent in-memory checkpoint, and dropping an old retirement guard cannot clear an accepted refusal. Native completed acceptance is recorded before finite capture limits, so an oversized completed frame remains progress even when its opaque payload cannot be retained. Imported frames require exhaustive ordered opaque-block witnesses; untracked opaque base/input history is refused before native HTTP. These are lower-core attachment checks. Actor-backed terminal/reply-boundary adoption and recovery after a failed on-disk replacement remain separate controller requirements; live-clone refusal is not evidence of cross-process failed-save fencing.


Native saved boundaries are attached only from the exact retired actor outcome:
`Done` permits a completed boundary after its zero-tool structural check;
`AwaitingUser` or an actual compaction pressure boundary permits a live reply
only with the complete correlated typed tool-result tail. Cancellation and
errors remain incomplete. Legacy wrapper calls default to incomplete, and an
imported missing or unknown boundary cannot attest retirement. A signed or
redacted-only native `end_turn` can truthfully produce `Done` with an empty
visible answer; truncated, paused, unsigned, interrupted or unbound responses
retain existing failure/continuation behavior. These lower shared helpers do
not enable ordinary frontend replay or provide cross-process fencing after a
failed disk replacement.

Canonical Session usage projection belongs to the exact first retired owner
claim. It preserves observed usage at captured prices even if a same-epoch
edited feed refuses attachment. Foreign sessions, newer cutoffs, and duplicate
retirement calls return before projection; lifecycle settlement uses the usage
already recorded by its event consumer. Neither path creates another ledger
receipt.


### Configuration layer ownership

`config/layers.rs` selects one bounded nearest project/local pair independently
from the retained execution workspace and parses literal repeated CLI overrides.
Each selected file uses the existing bounded no-follow regular-file reader and
independent path/SHA trust binding. Invalid nearest candidates do not cause
ancestor fallback. Project financial/cache ceilings and permission-cycle policy
remain global-only. `config/schema.rs` emits a side-effect-free structural editor
schema directly from `CONFIG_KEYS`; opaque/dynamic object semantics remain runtime
checks. `--print-config` reports canonical key names and layer participation only.

Configured MCP origins retain bounded immutable file contributors under CLI
composition: private global source, current-workspace `.mcp.json`, selected project
config, and selected local config. Each carries its exact retained source root,
regular-file identity, raw SHA and original bounded definition baseline. Admission
rechecks the actual execution workspace, ancestor membership, current source
bytes/identity and each file's current independent trust. CLI-only MCP declarations
come only from validated operator argv. Effective replacement does not erase file
revocation. Existing manager DNS/handshake/per-effect gates and ordinary MCP
permissions are unchanged. Diagnostic origin labels never confer launch authority.

`src/config/finite.rs` performs a bounded, allocation-free read-only serde traversal before CLI/project JSON normalization. Every serialized non-finite float is refused with a static value-redacted error; neither a base policy nor an overlay can turn into JSON null and erase a cap. It creates no second configuration/value, grant, writer, or effect owner.

`session::storage::load_session_exact_read_only` imports data through two
host-retained roots. It validates the application-private directory and file
without creating or repairing them, rejects links and changed identity, reads
at most 8 MiB plus one detection byte, and requires the requested portable ID
and current workspace. Saved labels and approvals do not create runtime grants;
the frontend constructs fresh permissions/instruction services separately.
All Session reads/serialization use the same 8 MiB ceiling. Saves encode once
through a bounded byte writer, so shared-owner changes cannot evade a separate
size-count pass. Malformed snapshots
produce a bounded static parse error; oversize saves refuse before creating or
replacing a session artifact. Native replay compatibility remains a separate
actual-model/controller check.

The shared `session::user` schema and `agent::input::PromptInput` retain ordered
bounded text/image data separately from literal instruction text. Typed rows
validate role and exact text projection; JSONL import remains tolerant of legacy
rows without token estimates. `print::persist_headless_turn_input` projects the
actual canonical delta, including intermediate empty Assistant boundaries and
non-initial User messages. The shared bounded encoder validates typed rows before
creating directories or claiming a writer. ACP list/load/media handlers use those
shared validators; importing data never imports approvals, instruction receipts or
replay authority.


### Installed Windows preflight helper observation

Only the explicit installed sandbox self-check attaches the unnamed fixed-size
`windows/preflight_witness.rs` observation cell. Its complete private request
transfers a noninheritable handle duplicated into the exact spawned helper; the
contained target retains its unchanged three-stdio handle list. Existing helper
stage stores mirror into one aligned scalar. The parent samples only after the
exact helper retires and emits a closed phase label, never handles, paths, SIDs,
argv, payloads, or credentials. The observation is not readiness, containment,
or availability authority and does not repair a timeout. Ordinary launches and
direct verification omit the optional field and retain their existing path.

Cell version 2 packs the closed phase and capped phase-entry milliseconds into
that same single DWORD, preventing a killed helper from leaving a mixed
phase/time pair. The clock starts at validated helper attachment, before the
helper's full executable digest proof, and is explicitly separate from the
parent's elapsed clock. Setup observations distinguish parent/Job, profile
control and publication, root ACL grants, loopback attestation, token and desktop
work. They refine observation only: legacy helper failure stages, both complete
executable proofs, containment policy and deadlines are unchanged. A last phase
identifies the active region, not a proven blocking API or timeout cause.

The observed self-check owns its request writer thread through cancellation,
actual thread signal and join. Every parent pipe reader is closed after spawn.
Preparation, duplicate/encode/delivery, helper execution and retirement consume
the original five-second run plus five-second cleanup deadlines; recovery keeps
its established separate deadline. If exact helper/writer retirement cannot be
proved by that cleanup bound, the explicit self-check terminates fatally rather
than detaching an owner or declaring availability. Recovery state is preserved.
The scalar and feeder primitives have prior finite native Windows proof; an
actual observed installed check remains the owning integration witness.

Clipboard image input lives in `ui/clipboard_image.rs`. Only an explicit paste
in the idle ordinary editor requests an image; approval/question/picker keys
and background turns do not read it. Fixed macOS AppKit/JXA, Linux
`wl-paste`/`xclip`, and Windows STA PowerShell clipboard adapters run through the
existing owned support-command path with a three-second limit and bounded
stdout/stderr. Helper output never becomes a command or pathname. Complete PNG
bytes are capped before decoding base64, with bounded header dimensions; no
new image codec or process registry is introduced.

Clipboard images join `pending_media` with runtime display metadata. The input
footer shows their image number and PNG dimensions; `/drop 0` removes the first
pending attachment. The next ordinary turn carries the literal text and images
in one canonical user message. Its exact bytes persist inline in the private
session JSON and survive resume; `--no-session` stays ephemeral. Existing
canonical limits apply: 2 MiB per image, 4 MiB total and eight images per turn.
The transaction retains attachments for rollback when preparation or a
zero-progress turn fails. Active hooks, spend and permission admission use the
same typed runner path; a text hook cannot rewrite an ordered image prompt.

### Image results from `read`

The default multimodal build returns admitted PNG/JPEG/GIF/WebP files as an
image block plus dimensions/size summary, with a 5 MiB regular-file cap and
1..8000 pixel dimensions. The same captured file/read permission is used; image
reads grant no raw text overwrite proof. Known text-only models and unsupported
transport formats return an explicit error. Exact image data survives canonical
save/reload and backend tool follow-ups, within existing native/session envelope
limits. See [Image reads](IMAGE_READ.md) for protocol, capability and replay limits.


### Opt-in writing subagents

The `task` tool remains read-only by default. `[subagents] allow_write = true`
also permits trusted `mode: write` personas with the parent's eligible
Write/Edit/Shell implementations, checker, approval channel, workspace,
sandbox, formatter and LSP. Write children are serialized; each operation
retains its own ordinary permission check, and prompts identify the child.
Their JS profile remains read-only and they gain no MCP, nested delegation or
memory mutation. See [Subagents](SUBAGENTS.md#opt-in-implementation-personas).

### Mid-turn steering

In the interactive UI, `/steer <message>` holds one pending text message (up to
64 KiB) for the next settled tool batch. It does not interrupt tools or start a
second turn. The runner runs `UserPromptSubmit` at that boundary, appends accepted
context notices and the user message to the SDK's canonical transcript, and
reports delivery before the next completion request. Later requests and saved
sessions retain that message exactly once. A blocked hook reports its reason.
If the turn ends without another tool boundary, pending text returns to the draft.

Ordinary Enter still queues input by default. Set `running_input = "steer"` to
steer instead; `running_input = "queue"` preserves the default. Commands and
attachments keep their existing handling. Steering has no headless or ACP input
surface and grants no additional tool permission.

`src/agent/steering.rs` owns the bounded invocation-local slot and hook. The
patched SDK's `RequestPatch::append_input` adds ordinary text to the real run
state before request preparation; unlike `RequestPatch::history`, it is durable
canonical input rather than a one-request history replacement.
`session::user` stores ordered canonical text/image content directly in a User
row. Text-only legacy rows keep their existing serialization. Typed rows cannot
carry tool identities or mutations, and their text projection must match the
row. Importing media never reads a path, follows a URI, or installs a grant.
Images use strict base64 and a closed PNG/JPEG/GIF/WebP MIME set; they are not
locally decompressed. Independent limits bound block/image counts, text,
individual and aggregate decoded image bytes, and total encoded prompt bytes.
Shared bounded Session decode and save validate these rows before use.

`agent::input::PromptInput` separates the literal text used by host prompt gates
from the exact initial typed User message. Retries reuse that same message;
continuations retain it in history. Existing String entrypoints remain text-only
wrappers. A prompt hook that rewrites typed media text is declined visibly
instead of flattening images or changing their order.

`extras::acp::sessions` exposes bounded, read-only saved-session metadata without
creating or repairing storage. It uses the shared private catalogue predicate
and an explicitly owned blocking task. List cursors are bounded offsets, never
paths. At most 100 retained catalogue rows are paged in batches of 20; truncation
and skipped-entry counts are reported as metadata rather than claiming a full
scan. Stored workspace labels are returned as data and are never resolved.


### Canonical ACP session input and local commands

`session/user.rs` validates ordered inline text/image data. It caps blocks (128),
images (8), each decoded image (2 MiB), aggregate decoded images (4 MiB), text
(1 MiB), and the actual encoded JSON payload (6 MiB) independently. Whole session
storage remains capped at 8 MiB; combining individually valid maxima does not
promise that a session fits. Image MIME types are PNG, JPEG, GIF and WebP. URI
and annotation fields do not open files or fetch resources. Images remain data
on import, with the canonical role and literal text projection checked before
save or dispatch. A build without `multimodal` refuses image-bearing input or
loaded live history visibly; it never flattens or silently discards it. Known
transport support is checked without guessing a model's capabilities from its
name. Provider errors remain authoritative for unknown model capabilities.

`agent/input.rs` separates the exact initial typed User message from the literal
text host hooks use. Interrupted retry and continuation retain that typed
message. A hook that changes an ordered input's literal projection is refused
rather than flattening media. The canonical session feed, including ordered
payloads, participates in native replay consistency hashing.

`extras/acp/sessions.rs` lists bounded metadata and loads exact saved sessions
through retained private readers without creating or repairing import storage.
The requested workspace must match; stored workspace labels never select a
filesystem root. Loaded sessions receive fresh runtime grants, read tracking,
MCP and skill owners. Saved goals retain objective/progress data but rebuild
check, judge and context-file authority from current configuration. Checkpoints
run on owned blocking jobs after model, effect, accounting and MCP retirement;
the generation remains occupied through publication. Writer claim/publication
errors are visible and retain observed progress in memory. An unwritable older
disk checkpoint cannot be updated with an in-memory refusal; no cross-process
failed-save fence is claimed here.

`extras/acp/commands.rs` recognizes only one original Text block as a local
command. `/rename` and `/undo` change the canonical session without launching a
model. Undo removes conversation context, not filesystem effects or billed
usage. `/compact` summarizes completed context and records actual compaction
bounds; image pixels are explicitly absent from textual summaries. `/goal` is
available only with the goal feature. Rendered resource text, images and model
output never become local-command authority.


The editor's `protocolSession.ts` uses public ACP requests and exact session-ID
notification routing for both new and loaded conversations. Replay buffering
has independent count and encoded-byte bounds. User cancellation sends
`session/cancel` while continuing to await the server's terminal response;
close waits for that ownership boundary before releasing routing. A timed-out
peer cannot become a successfully switched conversation. The existing process
owner may then terminate the child; forced termination is not a writer-join
claim. `Mini Agent: Resume Saved Session` lists the current trusted workspace
and explicitly selects one saved ID. A busy prompt blocks switching, and no
saved permission grant or private SDK constructor is adopted.

The focused offline editor-host gate is
`npm run test:integration -- <candidate.vsix> <target> resume`. It uses the
existing private VSIX profile and scripted loopback provider, drives the real
saved-session picker after a process restart, and verifies public load/update
routing, the same saved ID, original history in the second wire request and
once-only cumulative usage. `tests/acp_session_parity.rs` separately exercises
actual stdio ordered image retry/save/load, imported grant removal and feature-off
refusal at the existing physical 1 MiB process stack budget on Unix.

ACP native turns use the same retained publication barrier, exact typed-model binding,
actor pairing, canonical ordered-input commit and owned retirement as the main
frontends, while all output remains ACP JSON-RPC/session updates. The barrier precedes
SessionStart, MCP and agent preparation. Native-bearing `/undo`, `/compact` and
`/rename` are visibly refused without changing saved bytes; native automatic compaction
also refuses before HTTP. Generic ACP sessions retain these local commands. Opaque
thinking/signatures remain persisted protocol data and are never editor replay text.
The focused native stdio fixture proves same-process/restart prefix retention and
closed refusal using inert signatures; it does not assert live-provider signature
acceptance.

The App event loop constructs measured large agent-event, key-handler, and
slash-command futures through the existing short `heap_future` factory. This
keeps their temporary storage out of parent polling frames while preserving
the same awaits, interruption cleanup, and permission ownership. Fixed 1 MiB
thread tests exercise a cold typed command, draft interruption, pending
approval denial, mode cycling, and session-picker selection. The Windows
default job discovers and runs both App stack consumers after compiling its
existing feature suite; compilation alone is insufficient evidence for this
stack budget. No production stack-size override is introduced.

### Native web retrieval and search

`src/agent/tools/web_http.rs` owns the parent-only URL/origin policy, public-address
checks, process-wide DNS budget, request/response caps and pinned HTTP transports
shared by native web tools and the JS broker. `web_fetch.rs` implements GET-only
page retrieval and minimal HTML-to-text conversion; `web_search.rs` implements
fixed-endpoint Exa search. These tools are available without the `js`, `sandbox`
or `mcp` Cargo features and use the same mandatory model-tool decorators as other
built-ins. `--no-tools` and named `--tools` selection apply normally.

Every requested destination must use HTTPS unless the operator enables native
HTTP explicitly. Normalized URLs, including every redirect, are limited to
8 KiB before DNS and permission approval. Embedded credentials, fragments, ambiguous trailing-dot hosts,
private/special addresses and mixed public/private DNS answers are refused. DNS
answers are checked before approval and pinned for the exchange; redirects are
never followed by reqwest. `web_fetch` independently validates and authorizes
at most five redirects. Search posts only to `https://api.exa.ai/search`, captures
`EXA_API_KEY` when the agent is built, and refuses every redirect. Keys never come
from a tool argument, are never sent to returned search URLs, and backend error
bodies are not exposed. Exact credential echoes in returned titles/snippets are
redacted within the same character caps; result URLs containing the captured
credential are omitted. A search can incur Exa charges; model-token/cash totals do
not estimate those external service charges.

Network approval uses `web_fetch` or `web_search`, the normalized URL and its
complete destination-address list. Unmatched network calls ask in standard mode;
headless requests fail closed without explicit permission. Readonly/planwrite
modes do not silently authorize network access. Host-wide interactive grants
still run all URL, origin and public-address checks on every request. An origin
allowlist restricts possible destinations; it does not grant permission.
The explicit operator `--dangerously-skip-permissions` mode disables ordinary
permission checks as for other native tools. It retains mandatory financial
admission, concurrency, destination checks, network caps and cancellation.

The parent limits request headers to 64/16 KiB, response headers to 128/64 KiB,
request bodies to 256 KiB and response bodies to 1 MiB. Encoded or non-UTF-8 bodies
are rejected. A native HTTP operation has one ten-second deadline spanning DNS,
approval, redirects and body consumption, with three-second DNS/connect/read
bounds. Cancellation drops native approval and asynchronous socket work. The
non-abortable OS resolver remains bounded by four process-wide permits; its
actual scoped blocking closure retains the permit and turn ownership until it
returns. Private spill writes also retain scoped ownership until the OS operation
finishes; the network deadline does not claim to interrupt filesystem sync.
No native operation retries a provider request.

Fetch returns up to 50,000 converted text characters, quoted line by line as
untrusted reference data. Script/style bodies are omitted; HTML parsing is a
small text extractor, not a browser or rendering engine. Optional `prompt` is a
retrieval-focus annotation for the caller; it performs no additional model call.
Returned page text, query text and snippets never become commands, prompt-mode
directives, file includes or permission grants. Truncated text is saved to the
current logical session's private output directory when storage is available,
with an explicit pointer or visible storage-failure notice. The exact opaque
pointer-bearing output hash transfers the artifact reference alongside bounded
replay text to existing session persistence; it does not invent SDK correlation ids. Canceled save waiters clean
up uncommitted output through owned RAII results. Both tools advertise their own
limits and retain them through live model-history and transcript persistence.

Search returns at most five provider-ranked titles/URLs and 2,000-character text
snippets per result. It requests bounded text rather than provider-generated
summaries, visits no returned URL, filters malformed/credential-bearing result
URLs, and quotes all content. Missing `EXA_API_KEY`, unknown providers, invalid
arguments and provider HTTP errors give bounded visible errors. The only current
provider is `exa`; `web-search-provider = "off"` removes search registration.
The API contract is documented in [Exa's Search reference](https://exa.ai/docs/reference/search).

## Native Linux workspace CI controls

The existing `linux-sandbox-policy` CI job uses the Ubuntu 22.04 runner with its
unchanged host security policy. It builds bubblewrap 0.11.0 from a checksum-pinned
upstream archive using `scripts/install-ci-bubblewrap.sh --pinned-native`. APT uses
only the selected Ubuntu distro source, and the installer copies only the compiled
root-owned non-setuid binary. It does not change AppArmor, seccomp or user-namespace
settings. The runner label is not containment evidence: existing worker probes,
rlimit/seccomp finalization, and both JS/skills adversarial gates still must pass.

Seven exact native workspace/snapshot tests and one actual offline headless
shell/read test are discovered individually and run explicitly with `--ignored`;
zero tests, a replaced identity, or a successful process reporting zero passes
fails the gate. Full native logs and kernel/bubblewrap/toolchain identities are
retained as CI artifacts. The identical Rust toolchain is staged under the existing
readonly `/usr` allowance only for fixture use; no sandbox mount is widened.
Owned fixture directories are retired after settlement. These controls do not
resolve the separately recorded whole-filesystem-root `/` refusal.

## Windows MCP exact-snapshot publication

The Windows adapter in `src/paths/windows_mcp_publication.rs` is specific to
MCP configuration edits. Ordinary workspace compare-and-exchange remains
unsupported on Windows. An existing `.mcp.json` or global MCP config is opened
as a transaction writer and compared against its retained file identity, bounded
original bytes, and captured owner/group/DACL fingerprint. Root and immediate
parent directories are acquired as transaction writers and compared against the
retained directory identities and current security observations. These data
fingerprints confer no filesystem or runtime authority.

The replacement uses the existing private-file initializer guards and validator.
Its already security-capable transacted descriptor is initialized directly;
ordinary temporary files retain their existing `ReOpenFile` entrypoint. Both
paths require an empty regular single-link image, set the explicit current-user
owner and private DACL, and revalidate that same object. All transacted file handles and metadata clones are closed before replacement; the
same owned KTM transaction performs replacement and commit. The normal snapshot
read handle remains retained throughout. A failure before commit rolls back the
transaction, without a nontransactional pathname replacement or restoration over
a newer file. Unsupported filesystems, conflicting writers, changed roots,
reparse points, extra hardlinks, and failed private initialization refuse the edit.
An absent destination still uses the existing create-only operation.

Publication remains inside the existing MCP scoped blocking job; dropping its
awaiter does not retire that job or release its transaction early. The owning
Windows controls cover the actual retained reader/private candidate, parent and
root metadata conflicts, pinned namespace rename refusal, a nested replacement
before publisher capture, rollback, and cancellation with a held blocking writer.
The separate native TxF diagnostic establishes commit,
rollback, stale-data refusal and transaction-lock survival on the recorded
Windows host; it is not a replacement for these consumer controls or evidence
for every filesystem or account privilege level.

## Ordinary command filesystem profiles

`--sandbox-profile workspace|host-read|host-full` and the trusted
`sandbox-profile` key select ordinary command filesystem reach; `/sandbox`
shows or changes the idle session selection. `sandbox-read-roots` and
`sandbox-write-roots` add explicit directory grants. Permission decisions and
per-caller IP-network isolation remain independent; it does not block filesystem
Unix sockets made visible by broader host binds. Host-full is explicit and
shows a persistent red indicator plus a startup warning. Private application
and common secret paths, retained Git executables/cache and Git metadata
remain protected; unsafe mask topologies refuse launch. Linux may prepare
missing empty private mountpoint directories as documented in
[CONFIG.md](CONFIG.md); existing entries/permissions are never repaired.

Host-read can read a home rustup/toolchain while Cargo's writable cache/target
stays in the workspace, dedicated command cache or an explicit write root.
The broker-only JavaScript worker retains its separate mandatory profile.
Independently configured MCP/LSP stdio and hook policies retain their own
workspace/trust selection instead of inheriting a global host-full grant.
Windows uses its existing AppContainer explicit root keys and rejects the two
host-wide levels.

The macOS Workspace profile includes read-only public runtime assets needed by
Homebrew and Apple command-line tools: `/opt/homebrew/Cellar`, its `opt` aliases,
`/Library/Developer/CommandLineTools`, Apple vendor frameworks under
`/Library/Developer/PrivateFrameworks`, and the `Contents/Developer`,
`Contents/Frameworks` and `Contents/SharedFrameworks` subtrees of system
`/Applications/Xcode*.app` bundles. These runtime frameworks include the
Xcode loader and its transitive framework dependencies. Each such bundle's
exact `Contents/Info.plist` metadata file is also readable.
Canonical path lookup may inspect metadata on `/Applications` and the exact
Xcode bundle and `Contents` ancestors; it does not grant their descendants
metadata access or directory contents. Sibling files, other bundle contents,
application bundles under user home directories, and the containing
`/Applications` and `/Library` directories receive no subtree grant. These are
read filters, not write roots.
The exact developer-selector links
at `/var/select/developer_dir` and `/var/select/sh`, with their `/private`
spellings, are readable.
Path normalization can inspect metadata on exact ancestors of the workspace,
command cache and explicit roots. These metadata grants preserve the later
private-path denials and do not expose parent-directory contents.
Private-path and Git protections still apply. Other toolchains outside these
locations need explicit read roots or HostRead.
The layout follows [Homebrew's package terminology](https://docs.brew.sh/Manpage#terminology)
and [Apple's command-line tools selection](https://developer.apple.com/documentation/xcode/configuring-command-line-tools-settings).

## Permission prompt attribution

Approval requests capture their effective permission mode, the user's selected
mode, and a closed cause at the actual checker decision while holding its lock.
The TUI and ACP show that snapshot; headless unanswered Ask denials include the
same non-secret classification. A later mode change cannot relabel a queued Ask.
When a prompt directive narrowed the selected mode, that difference is explicit.
Configured Ask rules, PreToolUse hook Ask decisions, destructive shell detection,
repeated-call policy, external-directory policy, and Git metadata protection remain
authoritative. These labels grant no permission and do not expose rule text or
hook output. Unmatched benign multiline shell/Python input sends no Ask in Yolo;
Standard still asks. This does not establish the cause of an older photographed
prompt without its matching runtime/version and policy evidence.
