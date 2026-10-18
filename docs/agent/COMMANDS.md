---
description: "Complete reference of mini-agent slash commands, keyboard shortcuts, and input prefixes for the terminal UI."
---

# Slash Commands

For ad hoc main-agent instructions, use `--system-prompt <text|@file>` or
`--append-system-prompt <text|@file>` at launch. See [system instructions](CONFIG.md#ad-hoc-system-instructions)
for retained safety/tool guidance, file limits and placement.

For shell flag completion outside the TUI, run `mini-agent --completions <shell>`
with `bash`, `zsh`, `fish`, or `powershell`. This exclusive local command prints the installed
binary's completion script and exits. See [installation instructions](GET_STARTED.md#shell-completions).

All slash commands are available from the TUI input prompt.

While the main agent is running, `/help` (including `/help keys`), bare `/model`,
`/sessions`, `/sessions --all`, bare `/sandbox`, `/jobs` and `/tasks` remain available as views. Busy session
listing is a bounded text catalogue; it does not open a resume picker, create storage,
or repair file permissions. Model switches and session load/delete selectors wait until
the active turn settles. These views do not warm a model, save the session, or advance a
queued goal. Existing `/queue`, `/btw`, and loop/goal stop controls retain their behavior.
Every [`/cron`](#cron) verb also works while busy; a schedule only fires once the TUI is idle.

The default footer keeps shared todo progress (`todo:<done>/<total>`) and queued inputs
(`queue:<n>`) visible when nonempty, subject to the usual whole-item width priorities.
Completed and cancelled todos count as done; a temporarily locked store displays `todo:?`.

## Session

| Command | Description |
| ------- | ----------- |
| `/clear` | Clear the current session (all messages, tokens, compactions) and persist the change immediately. |
| `/new` | Alias for `/clear`. |
| `/undo` | Remove the last user turn, including its tool calls/results and assistant responses, persisting both the shorter history and its redo point immediately. An explicit `!command` interaction is one user turn. The working tree is never touched. |
| `/undo stash` | Same as `/undo`, then `git stash` the working tree. There is no interactive prompt; stashing only happens when asked for explicitly. |
| `/redo` | Restore whatever the most recent `/undo` or `/rewind` removed and persist the restored history immediately. |
| `/rewind` | Pick an earlier user turn, confirm, and put its input back in the editor. Shell turns are labeled `[shell]` and restore only `!command`, without captured output. `/redo` restores the removed transcript. |
| `/retry` | Load the last user message into the input editor for editing. |
| `/quit` | Exit mini-agent. |
| `/exit` | Alias for `/quit`. |
| `/sessions` | Open the recent-session picker, filtered to the captured current workspace. The picker opens at once showing `loading sessions…` while the catalogue is read off the UI task; a query typed meanwhile filters the rows when they arrive, and a listing error is shown inside the picker. Type to filter; rows show title, relative time, workspace, and message count. Tab toggles All workspaces; Enter resumes the exact selected record after a fresh private reload, and Esc cancels. Up to 100 rows are shown within bounded directory/file budgets. Names win over generated titles and first-user-message fallback. |
| `/sessions --all` | Open the picker with All workspaces explicitly selected. Choosing foreign history creates a fresh session in the current workspace; foreign approvals and hidden redo are removed. |
| `/sessions <id-or-name>` | Load a session: an exact ID or exact case-insensitive name wins, otherwise a unique ID prefix or name substring. A same-workspace session another process owns is forked under a new ID. Foreign history always forks into the current workspace without foreign approvals or hidden redo. |
| `/sessions delete <id-or-name>` | Delete a saved session using the same selection rules. The active session cannot be deleted; load another session first. |
| `/fork [n]` | Branch into a new session named `<name> (fork)` and switch to it. Without `n`, keep the current history. With `n`, use the one-based user-turn numbering shown by `/rewind`, keeping that input (including ordered media) and dropping its answer and later turns. The source file stays unchanged. |
| `/rename <name>` | Rename the current session. |
| `/history` | Show global chat history (last 10 entries across sessions). |
| `/export [file]` | Export the current session to a standalone HTML page (default `mini-agent-session-<id>.html`), Markdown with `.md`, or JSONL with `.jsonl`. Quote a file name that contains spaces. Markdown preserves message bodies literally in fenced blocks and includes tool arguments; each tool result shows at most 16 KiB with an omission notice and any recorded full-output path. Export does not read that path and refuses to overwrite an existing file. Markdown is a readable transcript; use JSONL for `/import`. Requires the `export` feature (default-on). |
| `/import <file>` | Import a session from a versioned mini-agent JSONL export (or a native session JSON document), save it, and load it. Schema markers select the format deterministically; imports are limited to 16 MiB and 10,000 messages, and external native files cannot inject a hidden redo snapshot. Requires the `export` feature. |
| `/share` | Show what sharing would publish: the HTML export's size, how many tool outputs it contains (tool output can include file contents and secrets such as `.env` values), and that a secret gist is readable by anyone with the link. Nothing is uploaded. |
| `/share confirm` | Upload the HTML export as a secret GitHub gist and print the URL. The upload gives up after 10 seconds connecting or 30 seconds in total. Requires `GITHUB_TOKEN` or `GH_TOKEN` and the `export` feature. |
| `/queue` | List input queued while the agent is busy (same as `/queue ls`). Interrupting or failing the main run drops queued inputs and reports how many were dropped. |
| `/jobs` | List this session's retained background job IDs, command summaries, statuses and exit codes; captured output is not included. Works while the main agent runs. |
| `/jobs stop <job-id>` | Request cancellation of that exact owned job. `stopping` is a request, not proof of cleanup; the record stays owned until its process family settles. Works while busy. |
| `/queue clear` | Empty the queue. |
| `/queue pop` | Remove the last queued input. |
| `/welcome` | Show the welcome/onboarding screen. |
| `/tutorial` | Alias for `/welcome`. |
| `/tutor` | Open the full getting-started guide in `less`, or print it when no pager is available. |

Use `mini-agent -c --fork-session` to continue the latest saved session under a new
ID, or combine `--fork-session` with `--session <id-or-name>` or `--resume`.
The branch is saved even before a new model turn; `--name` overrides its generated
name. CLI forking requires a saved-session selection and conflicts with `--no-session`.
In an ephemeral TUI, `/fork` switches the in-memory branch without writing a file.

Forking requires a settled conversation. Branches retain recorded usage and budget
uncertainty, so branching cannot reset spending limits. Explicit branches discard
session approvals and hidden redo; runtime ownership is fresh. Forking does not
change files in the working tree.

HTML exports treat every session field as untrusted. Raw HTML is displayed as escaped text;
assistant Markdown keeps normal formatting, HTTP(S)/`mailto:` or relative links, and HTTPS images,
while executable/active schemes, non-HTTPS images, and tags are removed. The page also carries a
restrictive Content Security Policy as defense in depth. JSONL exports preserve the original text
and, for newly recorded tool interactions, the correlated tool-call name, arguments, identifier,
and bounded tool result used to resume the model conversation. Older exports without those optional
fields remain importable and replay their tool records as labeled transcript text. The JSONL header
also carries the session's token and cost totals and its context window. Version 2 additionally
preserves compaction records, generated titles and provider-native tool-call reasoning/provenance;
`/import` accepts older version-1 exports too. Replay metadata must correspond to the visible
transcript. Permission grants and concealed undo/redo history are never accepted from an import.
A goal travels as its objective, criteria, status and progress; `/import` restores it. An imported goal takes its
checks, judge and bounds from the importing installation, never from the file, and a status this
build does not know parks it.

## Provider & Model

| Command | Description |
| ------- | ----------- |
| `/provider` | Show the current provider. |
| `/provider <name>` | Switch to a different provider. |
| `/model` | Show the current model. From the command picker, Enter on `/model` opens the model picker: Tab switches between quick aliases and the provider's models, and the current model is marked `(current)`. |
| `/model <alias\|id>` | Switch model: a `[quick_models]` alias first (which may also switch provider and pricing), otherwise a raw model id on the current provider. Raw ids resolve pricing from the live listing, baked catalog, then a matching quick model, as at startup. Unknown prices reset both rates to zero and show a yellow note explaining that costs are not tracked. |
| `/models` | List quick models and the provider's available models. |
| `/models refresh` | Re-fetch the provider's model list (also retries a listing that failed recently). |
| `/models <name>` | Same as `/model <name>`, kept for compatibility. |
| `/models-add <name> <provider> <model>` | Save a new quick model to the config file. |
| `/login openai [new\|<number>]` | Sign in to a ChatGPT subscription in the browser. `new` registers another account or workspace; a number reauthorizes a registration listed by `/provider`. See [PROVIDERS.md](PROVIDERS.md). |
| `/login copilot` | Run the GitHub Copilot device authorization (one account per GitHub host). `/login anthropic` explains why no Anthropic subscription login exists. A running turn must finish first. |
| `/logout openai\|copilot` | Revoke (where supported) and clear the saved login for that provider. |

## Context Files

| Command | Description |
| ------- | ----------- |
| `/add` | List files currently added to context (with sizes). |
| `/add <path>...` | Add one or more files to the agent's context (absolute or relative paths). Quote a path that contains spaces (`/add "my notes.md" src/lib.rs`). |
| `/drop <path>...` | Remove one or more files (or pending media indexes) from the agent's context. |
| `/drop-all` | Remove all added files from the agent's context. |

After `/add `, Tab opens captured-workspace file completion for the current unquoted operand.
Enter selects one quoted literal path and leaves the draft in the editor; it does not submit
a prompt. `$`, `@`, and shell punctuation in a selected name remain literal. Existing quoted
operands are retained; an unfinished quote or Unix escape is left for manual editing.
This picker lists workspace files, rather than MCP resources.

Files added with `/add` are included alongside the conversation in each request,
useful for giving the agent reference documentation or code without cluttering
the chat directly.

## Initialization

| Command | Description |
| ------- | ----------- |
| `/init` | Create an AGENTS.md file for the current project by delegating to the agent. |
| `/init force` | Overwrite the existing AGENTS.md if one already exists. |

Requires a `code` prompt to be configured (run `/regen-prompts` to restore
built-in prompts, or create a custom `code.md` prompt).

## Security

| Command | Description |
| ------- | ----------- |
| `/mode` | Pick a security mode. In the TUI, accepting `/mode` from completion (Tab, or Enter on the full name) opens a picker listing every mode with a one-line description, the current mode highlighted and marked `(current)`; choosing one inserts `/mode <name>` for Enter to apply. Submitting a bare `/mode` (or running without a permission system) prints the current mode and the same described list as text. |
| `/mode standard` | Allow path tools within CWD, ask for external paths and for writes to `.git/config` or `.git/hooks`. Config rules apply. |
| `/mode restrictive` | Ask for every operation. Config rules skipped. |
| `/mode readonly` | Allow reads only; deny writes, edits, shell, and everything else. Approving a plan preserves this read-only ceiling. |
| `/mode planwrite` | Read-only except for the built-in, workspace-contained plan-file write exception. |
| `/mode guarded` | Allow reads; ask for writes, edits, shell, and everything else. Config rules apply. |
| `/mode yolo` | Allow everything; ask for destructive shell commands. Config rules apply. |
| `/mode accept` | Alias for `/mode standard`, matching `default_permission_mode`. |
| `/sandbox` | Show the current ordinary command filesystem profile and describe `workspace`, `host-read` and `host-full`. Works while the agent runs. |
| `/sandbox <profile>` | Switch this idle session's profile and rebuild its agent, without saving config. Active turns, background tasks and commands must settle first. Profiles never widen the JS worker; see [CONFIG.md](CONFIG.md) (`sandbox-profile`). |

Prompts can set the security mode automatically via `%%mode=<mode>` on
the first line. When a prompt with `%%mode=last_user_mode` is activated,
the mode reverts to whatever was last set explicitly by `/mode`, Shift+Tab, or
startup config. See Prompts & Themes below.

## Hooks

Requires the `hooks` feature (default-off; see [CONFIG.md](CONFIG.md#hooks)).

| Command | Description |
| ------- | ----------- |
| `/hooks` | Show whether a hook dispatcher is installed and, if so, each configured event with its handler count. |

Run `mini-agent --hooks-test <tool> [--hooks-test-input <json>]` from the
shell (not a slash command) to dry-run `PreToolUse` hooks for a tool without
starting a session or making a model call. See
[CONFIG.md](CONFIG.md#hooks) for the full hooks configuration reference.

## Prompts & Themes

| Command | Description |
| ------- | ----------- |
| `/prompt` | List available prompts. |
| `/prompt <name>` | Activate a named prompt. Also applies `%%mode=` and `%%agent=` header directives when present (see below). |
| `/prompt default` | Clear the active prompt. |
| `/agent` | List main-agent personas resolved for the active workspace. |
| `/agent <name>` | Apply a persona to the main loop and activate its optional `mode:` prompt. The confirmation states that only the persona prompt changes (and the security mode, when its prompt mode sets one); the tool set is unchanged. |
| `/agent default` | Clear the active main-agent persona. |

Prompts may start with contiguous `%%mode=<mode>` and `%%agent=<name>`
directives in either order. They automatically switch the security mode and
main-agent persona when activated. Valid security modes:
`standard`, `restrictive`, `readonly`, `planwrite`, `guarded`, `yolo`. Use
`%%mode=last_user_mode` to restore the mode the user last set via `/mode`
or startup config; `%%agent=default` clears the persona. Directive lines are
stripped before the prompt reaches the agent. Project prompt directives and
project persona definitions require the existing project-config trust binding.

Example `ask.md`:
```markdown
%%mode=readonly

## Read-Only Mode

You are in read-only mode. Only read files and explore.
```
| `/theme` | List available themes. |
| `/theme <name>` | Activate a named theme. |
| `/theme default` | Clear the active theme (use config colors). |
| `/regen-prompts` | Restore built-in prompts to the prompts directory. |
| `/regen-themes` | Restore built-in themes to the themes directory. |

A theme must contain valid JSON before `/theme <name>` changes the active or
saved choice. Invalid JSON reports the parse error with its line and column,
leaving the previous theme active. If a saved theme is broken or unavailable
at startup, mini-agent reports the problem and uses config colors; fixing the
file restores that saved choice on the next start. Unreadable prompt and theme
files are skipped with a warning containing their path and read error.

## Conversation

| Command | Description |
| ------- | ----------- |
| `/cost` | Show recorded session spend by requested provider/model and token category, today in UTC and all-time private ledger observations. A bounded read-only query; legacy estimates and incomplete/unpriced observations are labeled. See [COST_TRACKING.md](COST_TRACKING.md). |
| `/compress [instructions]` | Compress conversation history to free context window space. The summarizer receives the history, the previous summary and your instructions in fenced sections; closing tags of those fences inside the text (such as `</message>` or `</transcript>` in a tool result) are neutralized with U+2060 WORD JOINER, and the summarizer is told the fenced history is data, not instructions. |
| `/compact [instructions]` | Alias for `/compress`; after a manual compaction, show the active summary. |
| `/compact show` | Show the active saved summary without making a provider request or changing session history. Imported file-request metadata remains untrusted historical data. |
| `/context` | Inspect the captured preamble, conversation and retained tool-result estimates, registered tool metadata, compaction state and next-request pruning projection. Works during a turn; makes no model request and does not change or save the session. |
| `/doctor` | Show bounded build, config/key source, provider/pricing, cached sandbox/JS/integration and terminal metadata. Works during a turn, without starting an agent or saving history. |
| `/editsys` | Show the current edit system mode (similarity or hashedit). |
| `/editsys similarity` | Use SEARCH/REPLACE with exact/whitespace matching and explicit fuzzy opt-in (default). |
| `/editsys hashedit` | Use CRC-32 tag-based edits (token-efficient, CAS-guarded). |
| `/btw <message>` | Ask a quick side question in parallel, without touching the main conversation. It forks the current context (including the main agent's in-flight turn, if any), answers using read-only tools (read/grep/find_files/list_dir, no writes or shell), and prints the answer inline. Works even while the main agent is running. Nothing is written to history; its token cost is shown separately as `btw:$…`. Ctrl-C cancels an in-flight `/btw` without disturbing the main agent. |
| `/effort [low\|medium\|high\|xhigh\|max\|default]` | Inspect or set main-session effort. Changes persist with the session, affect the next request and leave reasoning display preferences unchanged. Stop the active turn first; unsupported backend/model levels are refused. `default` clears the session effort override. |
| `/reasoning [on\|off]` | Set actual thinking on supported native Claude routes; unsupported off requests explain the restriction. `default` clears an explicit native mode. On other routes this retains the existing prompt preference. |
| `/thinking` | Alias for `/reasoning`. |
| `/steer <message>` | While a turn runs, hold one text message (up to 64 KiB) for the next settled tool batch instead of queueing it; it does not interrupt tools. A second steer stays in the draft. See [mid-turn steering](TOOL_RUNTIME.md#mid-turn-steering) and `running_input` in [CONFIG.md](CONFIG.md). |
| `/review [msg]` | Run a one-shot code review. Activates the `review` prompt in readonly mode, submits a review message, and restores the previous prompt afterward. Without a message, auto-generates one based on session and worktree context. |
| `/toggle` | Show toggleable features, runtime availability, and current-workspace learned-skill service failures or degradation with retry status. |
| `/toggle todo [on\|off]` | Enable or disable todo-list tools. |

The startup banner reports JavaScript or learned-skill unavailability only after
the containment probe has made a refusal decision. While the initial agent is
still being built, the pending probe adds no warning; `/toggle` shows the
current runtime report once it is available.

`--print-config` reports an eligible JavaScript worker as `not probed`; configuration display
starts no JavaScript worker and does not attest containment or advertise worker capabilities.
Actual agent startup still requires the full containment probe and executable-content proofs.

## Git commands (feature-gated)

Requires `git-worktree`, included in the default build, a usable captured workspace
and the ordinary native Git/shell containment boundary.

| Command | Description |
| ------- | ----------- |
| `/diff [path\|turn]` | Show the native Git working-tree diff, optionally for one literal quoted path or the latest turn's edit/write request paths. Added, removed and hunk rows use the diff colors. |
| `/commit [msg]` | Preview a commit of the latest turn's native edit/write request paths. A supplied message is literal; otherwise a tool-free request drafts a message from the selected diff. |
| `/pr` | Draft a bounded title/body from recent local commits and the working-tree diff, preview them, then run `gh pr create` through the ordinary shell tool. Requires `gh` on the retained shell's PATH and existing GH authentication. |

A turn's request paths identify attempted native edits or writes; a failed request
or imported history does not prove a file changed. No shell/MCP output is parsed
as a file selection. Commit candidates are frozen before generation and cannot
expand to directories or descendants. Existing unrelated staged paths are refused;
the actual commit uses `--only` for the confirmed literal paths.

The preview asks for **Y** to execute, **N** or Esc to decline, or **B** to edit
the commit message / append a PR note. Editing returns to the preview and still
requires Y. Confirmation belongs to the same live session and captured workspace;
a stale proposal cannot run. Ordinary tool permissions and hooks still apply after
Y, and a hook cannot rewrite confirmed operands. Ctrl-C cancels owned work and
discards queued submissions; a cancelled stage can leave approved partial changes
in the index, so review them before retrying.

Proposal generation uses the current provider/model, no tools, a 1024-token output
cap, a 30-second deadline and bounded context/output. Hosted tool/state overrides
are removed, while declared provider privacy/routing constraints are validated.
Observed auxiliary usage is charged once at captured model prices to Session spend;
it does not recalibrate the main conversation context. Declining, malformed output
or cancellation retains usage already observed. `/pr` reports a URL only from a
successful actual foreground command with complete bounded stdout, never from
model text or a hook's rewritten display text. These controls do not add a
headless `--commit` flag; programmatic agents retain the native Git/shell tools.

## Memory (feature-gated)

Requires the `memory` feature, which is included in the default build.

| Command | Description |
| ------- | ----------- |
| `/memory` | Show memory status (MEMORY.md, scratchpad, daily log). |
| `/memory status` | Same as `/memory` (explicit status check). |
| `/memory search <query>` | Search all memory files with case-insensitive keyword matching. |
| `/memory read long_term` | Read the global MEMORY.md file. |
| `/memory read scratchpad` | Read the project scratchpad (open checklist items). |
| `/memory read daily [date]` | Read a daily log (defaults to today; use YYYY-MM-DD for past). |
| `/memory read note <name>` | Read a named note. |
| `/memory write long_term <content>` | Append to the global MEMORY.md. |
| `/memory write scratchpad <content>` | Append to the project scratchpad. |
| `/memory write daily <content>` | Append to today's daily log. |
| `/memory write note:<name> <content>` | Append to a named note. |
| `/memory editor` | Open MEMORY.md in your system `$EDITOR`. |
| `/memory clear scratchpad` | Clear all scratchpad items. |
| `/memory clear daily` | Clear all of today's entries. |

Long-term memory (MEMORY.md) and open scratchpad items are automatically injected
into every request. The two most recent non-empty daily logs are also included;
they are not assumed to be today and yesterday. Notes and older daily logs are
accessible via `/memory read` and `memory_search`.

## MCP (feature-gated)

| Command | Description |
| ------- | ----------- |
| `/mcp` | Show configured origins, inactive definitions, retained peers and observed connection/catalogue attempts; no probe. |
| `/mcp add <name> <command|url> [args…] [--project] [--allow-local]` | Save a named definition and reconnect. Project JSON requires explicit whole-file trust review. Arguments are literal, not shell expressions. |
| `/mcp remove <name> [--project]` | Remove the selected owning definition and close its retained peer. |
| `/mcp <server>` | List tools of a specific MCP server. |
| `/mcp login <server>` | Run the OAuth 2.0 login flow for a URL server, then reconnect it. |
| `/mcp logout <server>` | Remove a server's stored OAuth token. |
| `/mcp:<server>:<prompt> [JSON arguments]` | Expand a cached server prompt as literal user-turn data, then use ordinary model admission. |
| `/add mcp:<server>:<uri>` | Read bounded server text through ordinary MCP permission and add a runtime-only resource snapshot. |
| `/drop mcp:<server>:<uri>` | Remove a runtime resource snapshot; cancel a pending resource selection first. |

See [MCP prompts, resources and forms](MCP_INTERACTION.md) for cached picker operands, `@mcp:` mentions, bounds and form handling.

See [MCP configuration and live operations](MCP_CONFIGURATION.md) for source precedence, reviewed project trust, literal quoting and explicit loopback policy.

## Advisor (feature-gated)

| Command | Description |
| ------- | ----------- |
| `/advisor` | Show current advisor status (enabled, mode, model, max uses). |
| `/advisor on` | Enable the advisor tool. |
| `/advisor off` | Disable the advisor tool. |
| `/advisor handoff` | Toggle human handoff mode on. |
| `/advisor handoff on` | Enable human handoff mode (route calls to the user). |
| `/advisor handoff off` | Disable human handoff mode (use advisor model). |
| `/advisor model <name>` | Change the advisor model. |
| `/advisor max-uses <n>` | Set max advisor calls per request (0 = unlimited). |
| `/advisor context-limit <n>` | Set max kilobytes of conversation context sent to advisor. |

## Subagents (feature-gated)

Requires the `subagents` feature (default-on; see [SUBAGENTS.md](SUBAGENTS.md)).

| Command | Description |
| ------- | ----------- |
| `/subagent-model` | Show the model currently used for subagents. |
| `/subagent-model <name>` | Switch the subagent model. |
| `/subagent-models` | List quick models available for subagents. |
| `/subagent-models <name>` | Switch subagents to a named quick model. |
| `/tasks` | List this session's background `task` jobs (`run_in_background: true`) with their status. Works while the agent runs. |
| `/tasks result <id>` | Show the retained report of that task. |
| `/tasks cancel <id>` | Request cancellation; the task reports `cancelling` until its model, approvals and effects settle. |

`/model-subagent` and `/models-subagent` remain accepted as hidden aliases
for `/subagent-model` and `/subagent-models`. Without the `subagents` feature,
either spelling prints that the feature is required.

## Worktree (feature-gated)

| Command | Description |
| ------- | ----------- |
| `/worktree <name>` | Create a git worktree on a new branch and `cd` into it. |
| `/wt-merge [branch]` | Merge the worktree branch back into the target branch. |
| `/wt-exit` | Exit the worktree and return to the main repo. |

During a merge, a dirty worktree requires an explicit `c` to commit all changes;
a conflict offers `l` to leave it for manual resolution. Uppercase action keys
also work. `a`, Enter, Escape, Ctrl-C, or Ctrl-D selects abort. Closing the input
channel also aborts. Background events are queued for processing after the prompt.
The target branch is pulled before the merge when it has upstream configuration.
For a local-only target branch, the merge uses its local tip without requiring a remote.

Cleanup preserves a source worktree containing ignored files such as `target/`,
`node_modules/`, or `.env`, even after a successful merge. The message lists
untracked and ignored paths separately. For ignored-only contents it prints a
quoted `git -C <main-repo> worktree remove --force -- <worktree>` command
(PowerShell quoting on Windows, shell quoting elsewhere). Inspect those paths,
return to the main repository, and run it only if their contents may be deleted;
the source branch remains available. Untracked files must be committed or moved
before cleanup. A dirty main workspace with either kind of extra file also
requires a manual commit or stash before merging.

## Loop (feature-gated)

`--loop` is a preset over [goals](GOALS.md): it sets a goal from the loop prompt
and the goal driver runs its rounds. An iteration is a goal round in restart
mode, so each one starts from a clean conversation carrying the objective, the
plan file and a summary of the round before it. `--loop-max N` runs exactly N
iterations, and a validator that passes while the agent reports the objective
met can end the loop before the cap. `--loop-max 0` runs nothing at all.
Interrupting during a validator ends the loop cleanly and reports
`goal: interrupted during verification`; the round's work is saved first, and
the interrupted round is not counted against the goal. A loop that stops for a
reason of its own — blocked, waiting on you, or stalled — exits with that
reason's code rather than reporting success. `/goal status` describes a running loop, and round records
are written under the goal transcript directory rather than a separate one.

`--loop-plan <path>` selects the plan used for both reading progress and the
model's update instructions (default: `LOOP_PLAN.md`). When that file exists,
startup asks whether to resume if stdin is a terminal; unattended runs resume
automatically without reading stdin. To start fresh unattended, remove the plan
before launching or select a new path. `/loop` uses `LOOP_PLAN.md` in the
current workspace (the worktree, after `/worktree`). A loop that is already
running follows the session when `/worktree` switches workspace (in either
direction): later iterations read the new workspace's `LOOP_PLAN.md`, which is
seeded with a copy of the current plan when the new workspace has none; an
existing plan there is used as it is. Each iteration re-reads the plan and shows at most its first 32 KiB, so
a plan that keeps growing cannot overflow the context.

The optional `--loop-run <command>` validator uses the selected process sandbox
and the same captured shell contract as the model-visible shell tool (`-c` for
Bash/sh or `-Command` for PowerShell/pwsh). Loops and goals share
one bounded runner: each validation has a 30-second deadline, 1 MiB caps for stdout
and stderr, and a 1.5 MiB combined cap. Timeout, cancellation, output-limit,
launch-failure, and nonzero-exit results are recorded with explicit status and
separate sanitized stdout/stderr sections. Resource-limit cancellation reaps
the direct validator everywhere and, on Unix, kills its complete process group
before the next iteration. Cancellation is operation-scoped: headless Ctrl+C
or Unix SIGTERM waits for that validator's cleanup before exiting, while interactive
Ctrl-C/Ctrl-D is routed semantically (`/btw` first, then verification or the main
run) and never uses cancellation to stop unrelated sandbox commands.
Each verification carries a generation ID, so cancelling and starting a new round
immediately retires the old generation; a late completion cannot advance or
respawn the replacement round.

When a main run is interrupted after assistant or tool progress, the partial
transcript is retained so on-disk edits stay explainable on resume. Any tool call
whose result was not observed is recorded with an unknown-outcome marker. Only
a failure before any agent progress restores the original prompt to the editor
and rolls the transcript back.

| Command | Description |
| ------- | ----------- |
| `/loop [prompt]` | Start the iterative coding loop (bounded to 100 iterations). |
| `/loop stop` | Stop future rounds of the active loop, even during a round. The current agent run finishes; an in-flight verification is cancelled. |
| `/loop status` | Show current loop status, including during an active round. |

`/loop` is not a timer: it starts round 1 immediately. To repeat a prompt on a
schedule, use [`/cron`](#cron); a `/loop` whose first argument is an interval
such as `5m` is refused with a `did you mean /cron 5m ...?` hint rather than
starting a round.

## Cron

`/cron` repeats a plain prompt in the current session, either on a fixed
interval or self-paced (the model chooses each next fire). It is available in
every build and every verb works while the agent is running; arming, listing
and stopping only edit the schedule.

| Command | Description |
| ------- | ----------- |
| `/cron <interval> <prompt>` | Arm a schedule. The interval is digits plus `s`, `m` or `h` (`90s`, `5m`, `2h`). The minimum is `1m` (a shorter interval is raised to it with a notice) and the maximum is `24h`. The first fire is one interval from now. |
| `/cron <prompt>` | Arm a self-paced schedule (see below). Any first word that is not an interval and not `list`, `stop` or `run` starts the prompt; `resume` is a verb only on its own. The first fire runs at the next idle point. |
| `/cron` or `/cron list` | List schedules: id (`c1`, `c2`, ...), `every <interval>` or `self-paced`, time until the next fire (`due at next idle` for a parked fire, `running` while a self-paced fire's turn has not chosen one yet, `suspended` after the USD budget refused a fire, `disarmed` for a schedule restored with a resumed session), fires so far out of `cron_max_fires`, and the prompt. |
| `/cron stop <id>` / `/cron stop all` | Remove one schedule or every schedule. |
| `/cron run <id>` | Mark a schedule due now; it fires at the next idle point without waiting for its deadline. |
| `/cron resume` | Re-arm the schedules saved with a resumed session. Each keeps its id and fire count and gets a fresh deadline one interval from now; missed fires are never caught up. With nothing saved it prints a one-line notice. |

- **Prompts, plus a read-only slash allowlist.** A payload starting with `!`
  is always refused with a one-line reason. A payload starting with `/` is
  refused unless it is exactly one of `/goal status`, `/tasks`, `/jobs` or
  `/cost` (interval schedules only). Those commands only read and print: they
  never start a model run, edit the session or touch a running turn. Such a
  fire runs the command through the normal slash dispatcher at the idle point
  instead of starting a turn, so it needs no model request and no budget.
  Everything else (`/steer`, `/btw`, `/queue`, `/loop`, `/cron`, any other
  command or argument form) stays refused, so a schedule can never re-arm
  itself or steer a turn.
- **Self-paced schedules.** A `/cron <prompt>` schedule has no interval.
  Only during the turn of one of its fires, the model is offered the
  `schedule_wakeup` tool (`delay_secs` integer, `reason` string, `noop`
  boolean; see [TOOL_RUNTIME.md](TOOL_RUNTIME.md#turn-scoped-cron-tool)). The
  delay is clamped to `60..=3600` seconds (`5` becomes 60, `99999` becomes
  3600) and the next fire is set that long after the turn ends; the last call
  of a turn wins. A turn that ends (or is interrupted) without a call stops the
  schedule with a notice, so a self-paced schedule cannot run away. The next
  delay is never read from the response text. The chat shows
  `⟳ cron c1 (self-paced): <prompt>` for each fire and
  `⟳ cron c1: next wakeup in 5m · <reason>` after it. A self-paced prompt
  cannot be a slash command, and one whose second word is an interval
  (`/cron every 5m x`) is refused with a hint to put the interval first.
  `cron_max_fires` applies as for interval schedules.
- **Noop collapse.** A self-paced fire whose call sets `noop: true` ends with
  one line `⟳ cron c1: no change · next in 10m · <reason>`. When the next fire
  of the same schedule is also a noop and nothing else was written to the
  chat in between, that fire's marker and output are removed from the chat
  view and the line is rewritten as `⟳ cron c1: no change ×2 ...` (`×3`, ...).
  This is display-only: every fired prompt and response stays in the session
  as ordinary messages.
- **Idle-only firing.** A schedule fires only when no run, live goal
  (running, paused or waiting), permission or question prompt, unanswered
  model question, chain or plan prompt, pending Git proposal or Git command,
  MCP admin operation, background-task permission or question request, attached image, or queued or deferred input is active. A deadline
  that passes while busy parks **one** fire that runs at the next idle point,
  after any queued input; there is no catch-up, however many deadlines were
  missed. Each fire restarts the interval from the moment it starts, so a long
  turn cannot fire twice.
- **What a fire does.** The chat shows a host marker
  `⟳ cron c1 (every 5m): <prompt>` instead of the `> ` echo of typed input,
  and the prompt runs as a normal turn in the current session. The marker is
  display-only: the session records the prompt as an ordinary user message,
  so it appears in history and `/rewind`. `UserPromptSubmit` hooks see
  `"origin": "scheduled"` (see [CONFIG.md](CONFIG.md)), and usage is recorded
  with request kind `scheduled`. A permission prompt opened by a fired turn
  waits for your answer like any other; it is never auto-approved, and no
  other schedule fires until it is answered.
- **Interrupting.** Ctrl-C stops a running fire's turn but keeps its
  schedule (the prompt is not loaded into the editor); a self-paced schedule
  is kept only when the interrupted turn had already called
  `schedule_wakeup`. Esc at an idle, empty editor clears parked fires without
  removing interval schedules, and clears the pending wakeup of every
  self-paced schedule, which stops it (it has no interval to fall back to).
  Quitting stops every schedule; the session file keeps them disarmed (see
  below).
- **Bounded.** At most 16 schedules per session, and each is removed after
  `cron_max_fires` fires (default 24, see [CONFIG.md](CONFIG.md)).
- **Spend.** When `max_budget_usd` is unset, arming the first schedule prints a
  one-time warning that scheduled turns are unbounded in spend. When the USD
  budget refuses a fire, every schedule is suspended with one notice and kept
  (`/cron list` shows them; `/cron stop all` removes them).
- **Saved disarmed.** Schedules are saved with the session (id, interval,
  prompt, fires so far and the wall-clock time the next fire was expected),
  updated whenever one is armed, stopped, fired or retired. They only fire in
  the process that armed them: quitting, `/resume`, `/new`, `/clear` or any
  other session switch stops them. Resuming a session with saved schedules
  (`/resume`, `--continue`) never re-arms them on its own, to avoid surprise
  spend; it shows one notice such as `this session had 2 /cron schedules (c1
  every 5m: ...); /cron resume to re-arm`. Until then `/cron list` shows them
  as `disarmed`, `/cron stop` removes them, and new schedules take ids after
  the saved ones. A session without schedules is saved exactly as before, and
  older builds ignore the field. A self-paced schedule is saved with
  `"dynamic": true` (the key is omitted for interval schedules); one saved
  while its fire was running is saved as not running. `/cron list` shows it as
  `disarmed (self-paced)`, and `/cron resume` parks its next fire for the next
  idle point (no catch-up). While a self-paced fire's turn runs, the statusline
  `cron` item reads `running` until the turn chooses its wakeup, then shows
  that countdown.
- **Relative times.** Deadlines use a monotonic clock that does not advance
  while the machine is suspended, so times are always shown relative to now
  (`in 4m`).

## Goal (feature-gated)

A goal is a persistent objective the agent works toward across turns. It advances in **rounds**: one
agent run, then one gate evaluation that decides whether to keep going and says why. Every round
starts with a fresh turn and token budget, so a single runaway round is still bounded by
`max_agent_turns` while the goal's own bounds cover the objective as a whole.

The objective lives outside the conversation, so compaction cannot lose it, and it is restated into
every compaction summary.

A completion claim is verified before it is accepted. With `--goal-check` or `/goal check` the
command must exit zero; the configured `verify_command` also runs when the round did not trigger it.
A judge model then reviews the claim unless it is switched off. A judge can withhold completion but
can never overturn a command that exited zero, and `/goal status` labels a completion no command
proved.

A goal never changes the security mode, the permission allowlist, or the sandbox policy.

| Command | Description |
| ------- | ----------- |
| `/goal <objective>` | Set the objective. Lines after a `done when:` line become criteria. |
| `/goal status` | Objective, criteria, status, round, tokens, verification, judge, last check. |
| `/goal check <cmd>` | Require a command to exit zero before the goal may be reported complete. |
| `/goal bounds <k>=<v>` | Change `max_rounds`, `max_tokens`, `continuation`, and the rest. |
| `/goal pause` / `/goal resume` | Park the goal, or put it back to work. |
| `/goal reopen` | Reopen a goal that was declared impossible. |
| `/goal clear` | Discard the goal. Setting a new one over an unfinished goal is refused. |

Headless flags: `--goal`, `--goal-done` (repeatable), `--goal-check` (repeatable),
`--goal-max-rounds`, `--goal-continuation continue|restart`, `--goal-replace`. A goal with
`--no-tools` is refused: the agent could never report progress, so every round would look like a
stall.

In `-p` mode the run continues until the goal stops, and the exit code says why:

| Exit | Meaning |
| ---- | ------- |
| `0` | The objective was met. |
| `20` | Impossible as written. |
| `21` | Blocked by something the agent cannot remove. |
| `22` | Waiting for an answer from you. |
| `23` | Paused: no progress, a judge outage, a context overflow, or repeated run failures. |
| `24` | Budget limited: rounds, tokens, or time ran out. |

`--output json` adds a `goal` object with the status, rounds, tokens, the evidence that backed the
verdict, and the last reason. Goal round records are written under `state/goals/<goal-id>`.

## Shell Commands

Prefix a message with `!` to run it as a shell command instead of sending it to
the agent. The command and its captured output are stored in the session history
as one user message (``User ran `!cmd`:`` followed by the output in a fenced
block), so the model sees the output as something you shared, never as its own
reply. `/undo` removes that message, and `/retry` reloads the original `!cmd`.
After process cleanup, the command and captured result are saved immediately unless
`--no-session` is set. In the TUI, a save failure shows a warning and retains the live transcript.
While the command runs, the TUI shows a live tail of its output (the last 12
lines, refreshed at most every 100 ms) under the echoed command; when it exits
the tail is replaced by the saved final output.
Works in both TUI and `--print` mode.
In `--print` mode, a failed command or empty `!` exits nonzero. Captured output is still
rendered and saved before reporting a command failure; `--output json` marks that result
with `"stop_reason":"failed"`.

An empty or whitespace-only `-p` prompt is a local usage error (exit 2) before
provider and session initialization. Text mode reports `no prompt given` on
stderr; `--output json` (or the final `--output stream-json` record) emits a result with `stop_reason: "failed"`, zero usage
and cost, and the same message. No provider request or session message is created.

`@agent:<name>` mentions in a `-p` prompt are plain text unless the opt-in
`headless_agent_mentions = true` config key is set (default off, so scripts that
pipe arbitrary text keep working). With it on, and the `subagents` feature
compiled in, the typed prompt is resolved exactly as in the TUI: resolved
`@agent-<name>` aliases are rewritten to `@agent:<name>`, that normalised text is
what the session saves and hooks see, and the first model request carries the
non-persisted routing note. An unknown canonical name exits nonzero before any
provider request, with the error naming the valid personas on stderr (and in the
`--output json`/`stream-json` failure result). Piped stdin, `!` commands and
`--loop` runs are never parsed. See
[SUBAGENTS.md](SUBAGENTS.md#referencing-a-specialist-with-agent).

At startup, mini-agent resolves the configured shell once against the canonical
workspace and captured `PATH`. Supported Windows contracts are PowerShell/pwsh
with `-Command` and Bash/sh with `-c`; Unix uses Bash/sh with `-c`. The resolved
executable identity and argument contract are retained across agent rebuilds.
If the executable is missing or unsupported, the model-visible compatibility
tool named `shell` and its prompt guidance are omitted, and shell execution fails
closed. `--no-tools` performs no shell lookup.

Shell commands use the configured general sandbox when it is enabled. Running
with `--no-sandbox` is an explicit user-trusted bypass that inherits the parent
environment. If a sandbox enabled only by defaults is unavailable, startup may
continue unsandboxed after warning, but audit and failures label that condition
`unsandboxed-unavailable-default-fallback:<backend>` rather than claiming the
operator chose the bypass. An explicitly requested unavailable sandbox still
fails closed. Output is capped (1 MiB per stream, 1.5 MiB combined), commands
time out after 30 seconds, and TUI `Ctrl+C`/`Ctrl+D` cancels the whole process
tree before accepting the next command.
In `--print` mode, `Ctrl+C` or Unix `SIGTERM` cancels the explicit shell command
and waits for its owned process group to terminate, the direct child to be reaped,
and the command audit to finish before exiting with a nonzero interruption error.
That wait is bounded: a second `Ctrl+C`/`SIGTERM`, or 10 seconds without the cleanup
finishing, stops waiting and exits with status 130.

| Example | Description |
| ------- | ----------- |
| `!ls -la` | List files in the current directory. |
| `!git status` | Check git status without involving the agent. |
| `!cargo test` | Run tests and capture the output. |
| `!` | Empty command shows an error. |

If you want to run a command and then discuss the output with the agent, just
type `!<command>` first (it stores the command and output as one user message), then
follow up with a normal message asking the agent about it.

In the TUI, typing `!` into an empty input opens a picker of shell commands you
ran before (from the global chat history), newest first. Typing filters it, and
commands that start with the typed text rank first. Tab inserts the highlighted
command and Enter inserts it too, unless nothing matches or the highlight is
exactly what you typed, in which case Enter runs the command. Up/Down move the
highlight, Escape closes the picker and keeps what you typed, and Backspace on
the bare `!` removes it. With no earlier `!` commands the picker stays closed.

## Prompt Shortcut

Prefix a message with `.` to quickly switch prompts or run a one-shot query with
a different prompt.

| Example | Description |
| ------- | ----------- |
| `.` | Open the prompt picker (same as `/prompt` picker). |
| `.ask` | Switch to the `ask` prompt (same as `/prompt ask`). |
| `.plan what files changed?` | Temporarily use the `plan` prompt for this query, then restore the previous prompt and security mode. |

The `.[prompt] [msg]` syntax is a one-shot: it sets the prompt, submits the
message, and after the response restores the previous prompt and
`last_user_mode`. The submitted message is echoed once. Leading whitespace before a
dot shortcut is ignored both while idle and while a run is active.

An unknown dot name followed by a message is ordinary text: `.gitignore is wrong`
is sent to the model, and queues normally during an active run. Ellipses such as
`...continue` and dotted filenames such as `.config.toml` are ordinary text too.
An unknown bare name such as `.unknownprompt` reports a prompt-name error; `.`
and known prompt shortcuts remain commands and require the main run to be idle.

## Learned-skill and Agent Skill CLI flags

These are shell flags, not slash commands. Every one of them requires a build with the `skills`
feature; without it the flag does not exist and `clap` rejects it as an unexpected argument. Each
learned-skill flag runs before provider initialization and exits without starting a session, and
most of them conflict with one another so only one lifecycle action runs per invocation. The full
reference — package format, held-out suites, feedback semantics, stats columns — is in
[SKILLS.md](SKILLS.md).

| Flag | Description |
| ---- | ----------- |
| `--distill-learned-skill <SESSION_ID> <TOOL_CALL_ID>` | Turn the JavaScript of one persisted `js` tool call into a proposal package for `--import-learned-skill`, then exit. Asks the configured provider **once** to generalize the snippet; without a usable answer it writes a `TODO`-marked scaffold instead and says so. Imports, verifies, approves and activates nothing. See [SKILLS.md](SKILLS.md#distilling-a-recorded-javascript-tool-call). |
| `--distill-learned-skill-out <PATH>` | Write that package to `PATH` instead of `<local-data>/skills/distilled/<tool-call-id>.json`. Either way an existing file is never overwritten — a distilled package is an editable draft. Only valid with `--distill-learned-skill`. |
| `--import-learned-skill <DIR_OR_JSON>` | Import and contained-verify learned-skill JSON package(s) for approval. A directory imports 1–32 sorted `.json` files. |
| `--install-learned-skill-seeds` | Import and contained-verify the five bundled pure seed packages. Reports each seed independently and is idempotent. |
| `--list-learned-skill-proposals` | List proposals still awaiting an operator decision, as TSV, then exit. A `verified` proposal whose reason is `held_out_suite_required` is listed but is not approvable. |
| `--list-learned-skill-suites` | List trusted held-out suite IDs and enabled state, without hidden cases or fixtures. Supports `--learned-skill-json`. |
| `--disable-learned-skill-suite <SHA256>` | Disable one trusted suite as the local owner, preserving its data and historical reports. Repeated disabling is idempotent; validated reimport re-enables it. |
| `--learned-skill-proposal <SHA256>` | Print one proposal's admission outcome, including the `reason_code` and `report_id` of a rejected or deferred decision, then exit. Unlike the listing, this also finds terminal proposals. |
| `--learned-skill-stats` | Print the per-revision usage table (TSV) and exit. See [Reading `--learned-skill-stats`](SKILLS.md#reading---learned-skill-stats) before drawing conclusions from the `success` column. |
| `--approve-learned-skill <SHA256>` | Approve an evaluated proposal into the non-retrievable canary state. Requires `awaiting_approval`. |
| `--reject-learned-skill <SHA256>` | Reject a proposal awaiting approval as the authenticated local owner. Requires no embedding credentials or verifier; approved proposals are ineligible. |
| `--activate-learned-skill <SHA256>` | Activate an approved **lineage-root** skill after its held-out baseline. Deliberately refuses a replacement. |
| `--promote-learned-skill <SHA256>` | Promote an approved replacement canary over its active or quarantined predecessor, superseding it and preserving lineage. |
| `--retire-learned-skill <SHA256>` | Administratively disable an `active` revision, preserving the revision and its lineage. |
| `--reevaluate-learned-skill <SHA256>` | Requeue a proposal awaiting approval to refresh its evaluation, or one parked as `verified` with `held_out_suite_required` or `deferred` after an infrastructure outage or exhausted attempt budget. |
| `--compact-learned-skill-events` | Aggregate raw telemetry older than the 30-day retention window into daily rows, then delete it. |
| `--purge-learned-skill <SHA256>` | Coordinated, tombstoned privacy purge of one revision and its dependent records. |
| `--purge-learned-skill-force` | Permit that purge to delete a revision that is not in a terminal lifecycle status, or whose dependent revisions would be re-rooted. Only valid with `--purge-learned-skill`. |
| `--learned-skill-feedback <SHA256>` | Submit authenticated local-owner feedback for one learned skill. Requires the three flags below. |
| `--learned-skill-feedback-kind <KIND>` | `positive`, `negative` or `severe`; the flag itself rejects any other value. |
| `--learned-skill-feedback-reason <CODE>` | Reason code: 1–64 bytes of lowercase ASCII letters and `_`. For `severe`, only `integrity`, `permission_violation` or `unsafe_effect` are accepted. |
| `--learned-skill-feedback-key <KEY>` | Caller-chosen idempotency key: 1–128 bytes of ASCII letters, digits, `.`, `_`, `:` or `-`. Re-using it with a different payload is an error. |
| `--learned-skill-feedback-invocation <SHA256>` | Optional. Attribute the report to one exact invocation. Must be 64 lowercase hex characters naming a retained `invoked` event for that skill, within the 30-day raw-telemetry window. |
| `--learned-skill-json` (alias `--json`) | Emit learned-skill operator command results as one JSON object per line. The two tabular listings stay TSV. |
| `--import-agent-skill <PATH>` | Validate and install one local Agent Skills directory or ZIP archive, and point that skill's `ACTIVE` marker at the new digest. See [CONFIG.md](CONFIG.md#importing-portable-agent-skills). |

The four `--learned-skill-feedback-*` flags currently carry no `help` text of their own, so
`--help` shows them with a value name and no description. Until that is fixed, this table is the
reference for what each accepts.

Apart from the two TSV listings, every learned-skill command reports one line in a single shared
shape — `learned-skill <command>: name=value …`, with `-` for an absent value — so one parser
handles all of them. For example:

```text
learned-skill import: id=<sha256> proposal_id=<sha256> status=awaiting_approval reason_code=- report_id=<sha256> idempotent=false next_attempt_at=- blocked_by=-
learned-skill feedback: id=<sha256> feedback_id=<sha256> kind=severe status=quarantined quarantine=applied quarantine_reason=-
learned-skill distill: path=<path> id=<sha256> export=<name> tier=pure tests=2 held_out_cases=3 generalization=model effects=- complete=true
```

`--distill-learned-skill` adds one or two follow-up lines after that shared line: a
`learned-skill distill note:` line whenever it fell back to a scaffold or saw effect globals in the
recording, and always a `learned-skill distill next:` line carrying the exact
`--import-learned-skill` command to run. Under `--learned-skill-json` all of it collapses into the
one JSON object, which carries the extra `note` and `next_command` fields.

## General

| Command | Description |
| ------- | ----------- |
| `/help` | Show the full help message listing all commands and keybindings. |

## Keybindings

| Shortcut | Action |
| -------- | ------ |
| `Enter` | Send message. |
| `Shift+Enter` or `Alt+Enter` | Insert newline. |
| `Ctrl+C` | Cancel the current agent response, validation, or shell command. Typed follow-up drafts remain in the editor unless the failed prompt is restored; in that case Ctrl+Y can recover the displaced draft. Queued inputs are dropped with a count. When idle it clears a non-empty draft (Ctrl+Y brings it back) and quits on an empty input. |
| `Ctrl+D` | Same interrupt behavior as `Ctrl+C` while something runs. When idle it deletes the character under the cursor in a non-empty draft and quits on an empty input. |
| `Ctrl+Shift+C` | Copy selected text through the Unicode clipboard on Windows. |
| `Ctrl+V` | Paste Unicode clipboard text at the cursor on Windows. |
| `Ctrl+W` | Delete word backwards. |
| `Ctrl+U` | Delete everything before the cursor. |
| `Ctrl+K` | Delete everything after the cursor. |
| `Ctrl+A` / `Ctrl+E` | Move to the start/end of the current input line; pressing again at the edge stays put. |
| `Ctrl+B` / `Ctrl+F` | Move one character left/right. |
| `Alt+B` / `Alt+F`, `Alt+Left` / `Alt+Right`, `Ctrl+Left` / `Ctrl+Right` | Move one word left/right. Words are separated by any whitespace, including newlines. |
| `Alt+D` | Delete the next word. |
| `Alt+Backspace` | Delete the previous word (like `Ctrl+W`). |
| Other `Ctrl`/`Alt` letters | Ignored in the input and pickers; they never type the plain letter. With macOS Option-as-Meta enabled, Option-layer characters such as `@` on a German layout arrive as `Alt`+letter, so disable Option-as-Meta (or use the right Option key without it) to type them. |
| `Ctrl+Y` / `Alt+Y` | Yank the last deletion / rotate the kill ring. |
| `Ctrl+G` | Open the current input in the system editor (`$EDITOR`). |
| `Ctrl+O` | Launch `lazygit` (git TUI) in the project directory. (It was `Ctrl+H`, which many terminals send for Backspace; `Ctrl+H` is now always backspace.) tmux leaves `Ctrl+O` unbound by default; zellij's default keymap claims it for its session mode, so there switch zellij to locked mode (`Ctrl+G`) first or use zellij's `unlock-first` preset, as for `Ctrl+G` and the other editing keys. |
| `Ctrl+R` | Cycle reasoning display: collapsed → full → off → collapsed. Saves the preference globally. Click a reasoning header to expand or collapse that block. |
| `Shift+Tab` | With no picker open, cycle permission modes: standard → guarded → yolo → standard. Works while idle or running, including during an approval; the existing request remains unanswered. The status line flashes the new mode. Customize the ring with global `permission_mode_cycle`; in pickers Shift+Tab still moves the highlight. |
| `/` | Open the command picker at the start of the input. Typing filters it, and commands that start with the typed text rank first. Tab inserts the highlighted command. Enter runs a command typed in full and otherwise inserts the highlighted one. A space after a command typed in full closes completion and types the space; for a command with an argument picker (`/model `, `/models `, `/theme `, `/mode `, ...) it opens that picker instead. Argument pickers follow the monochrome setting like every other picker. Up/Down or Shift+Tab move the highlight, Backspace on the bare slash removes it, and Escape closes the picker. Backspace on an empty argument (for example after `/theme `) deletes the space and reopens completion on the command, as does deleting back to a bare `/command` word. Compatibility aliases (`/compact`, `/exit`, `/thinking`, `/tutorial`) are not suggested, but typing one in full and pressing Enter still runs it. |
| `@<query>` | Activate the file picker when `@` starts a word (input start, or after whitespace, a newline, `(` or a quote, including a pasted trailing `@`). When the `subagents` feature is compiled, the available specialist personas are listed first as `@agent:<name>` rows, ahead of `@mcp:` operands and files, from the same trusted persona source as the `task` tool and the `/agent` picker (refreshed when `@` or a leading `/` is typed). Each persona row shows the persona's one-line description dimmed after the name, cut to the terminal width and never wrapped; the query matches the name only, never the description; Tab or Enter inserts the highlighted path, or `@agent:<name>` followed by a space for a persona row, a space keeps the typed text and closes the picker, Ctrl+W deletes the query, and Escape closes it. On submit, `@agent:<name>` routes the request to that specialist through `task` and an unknown name is refused with the draft restored ([SUBAGENTS.md](SUBAGENTS.md#referencing-a-specialist-with-agent)). |
| `Tab` | Insert two spaces when no picker is active. |
| `Up / Down` | Move vertically in multiline input, one visual row at a time (long lines soft-wrap at the terminal width, breaking after whitespace where possible); at the first or last row, navigate command history (a recalled prompt puts the cursor at its end). |
| `Ctrl+P / Ctrl+N` | Move up/down exactly like `Up / Down`, including command history at the input boundaries. |
| `PageUp / PageDown` | Scroll viewport. |
| `Home / End` | Move to the start/end of the current input line. With an empty input (or while a picker is open) they jump to the top/bottom of chat history instead. |
| `Ctrl+Home / Ctrl+End` | Jump to the top/bottom of chat history. |
| `Escape` | Close active picker / cancel. |
| `Left / Right / Delete` in a picker | Close the picker (the typed text stays) and move or delete as in plain text. Clicking elsewhere in the input does the same. |
| Paste | Bracketed paste and `Ctrl+V` insert at the cursor. Pasted `\r` and `\r\n` line endings become newlines, tabs become four spaces, and escape sequences and other control characters are dropped. |
| Mouse drag | Select transcript text from the press point to the release point (partial first and last lines) and copy it on release; the result is shown briefly on the status line rather than added to the transcript. A plain click (no drag) never copies. |
| Mouse double-click | Select and copy the word under the pointer: letters, digits and `_ - . / ~ :`, so an identifier, a path or a `file.rs:12` location is taken whole (a trailing `.` or `:` is left out). Dragging after the second press extends the selection from the word. Set `mouse_capture = false` to leave the mouse to the terminal's (or multiplexer's) native selection. |
| Mouse scroll | Scroll chat history. |
| Permission prompt | While a `[permission]` prompt waits, the mouse wheel, `Up`/`Down`, `PageUp`/`PageDown` and `Home`/`End` scroll the transcript so the request's context can be reviewed; `y` allows once, `a` and `f` grant only the labelled session scopes, and `n` denies just this call. Native write/edit requests inside the captured workspace also offer `w`: allow writes and edits throughout that workspace for this session, including resume. This grants no read, shell, JavaScript, or MCP access, never overrides denied paths, and is unavailable for external or stale requests. Changing modes does not revoke an explicit session grant. `Esc`, `Ctrl-C`, and `Ctrl-D` deny the current request and stop its run, settling owned work and visibly discarding queued inputs. `s` also stops immediately, then accepts an optional one-line instruction: Enter puts it in the editor for review and a separate Enter sends it as the next user turn; Esc skips the note. The interrupted prompt and any displaced draft remain available through the kill ring (`Ctrl-Y`, then `Alt-Y` to cycle). Side-question answers, agent progress, session titles, diagnostics and MCP notices continue to render above the prompt before it is answered. A long path wraps (a path too long for the prompt area is shortened in the middle, keeping the file name); a multi-line command shows its first lines and a count of the rest, and its full text is in the transcript above. When the request was shortened the options end with `(e) show full request`: `e` expands the prompt to every row but one above the status line (a long path then wraps in full and a script shows as many lines as fit), and `e` again collapses it; `e` never answers the prompt. The status line stays visible while the prompt waits. Invisible Unicode characters that could make the request display differently from what runs (bidi embedding/override/isolate controls U+202A–U+202E and U+2066–U+2069, LRM/RLM/ALM, U+2028/U+2029, and the zero-width U+200B, U+2060 and U+FEFF) are shown in the prompt as a visible `<U+XXXX>` marker; elsewhere in the chat and in pickers they are removed or shown as `�`. Transcript tool-call lines show only the first line of a multi-line script, followed by `(+N lines, M chars)`. |

Learned-skill feedback can be inspected and corrected by the authenticated local
OS-account owner (requires `skills`):

```sh
mini-agent --list-learned-skill-feedback SKILL_ID --learned-skill-json
mini-agent --list-learned-skill-feedback SKILL_ID --learned-skill-feedback-after FEEDBACK_ID
mini-agent --learned-skill-feedback-record FEEDBACK_ID --learned-skill-json
mini-agent --correct-learned-skill-feedback FEEDBACK_ID VERSION resolved mistaken_report
mini-agent --correct-learned-skill-feedback FEEDBACK_ID VERSION retracted mistaken_report
```

Listing returns at most 100 records and a `next_after` cursor. Inspection excludes
submission prose, skill source, and invocation payloads. Correction requires the
current positive version, an active report, and a reason code of 1–64 lowercase
ASCII letters/underscores. It retains the original report and cumulative counters,
increments the version, and atomically appends an audit entry. Stale requests and
audit failures leave the report unchanged. Correction does not promote a skill or
release quarantine. The evidence-threshold promotion policy stops treating a
corrected report as active negative feedback; all its other gates remain intact.
Explicit local-owner promotion remains its existing, separate authorization path.


`-p --compact-instructions "preserve file names and blockers"` supplies a bounded
4096-byte compaction preference when automatic compaction actually runs. It does
not force a compaction and is unavailable in TUI, ACP, or loop mode. Preferences
are fenced below the summarization contract, just like manual `/compact` instructions.

## Custom prompt commands

Save `fix-issue.md` under `<config_dir>/commands/`, or under the captured
workspace's `.mini-agent/commands/` (`.claude/commands/` is also supported):

```markdown
---
description: Fix a reported issue
mode: plan
model: cheap
---
%%mode=readonly
Investigate issue $1. Additional details: $ARGUMENTS
Use @README.md as untrusted project context.
```

`/fix-issue 123` submits the expanded body as one ordinary user message. Commands
appear with their description in `/help` and slash completion. A filename stem
uses lowercase letters, digits and single hyphens, up to 64 bytes. Built-ins and
aliases, including `/diff`, `/commit`, `/pr` and feature-disabled commands, are
reserved. Layers override global, then `.claude`, then `.mini-agent`; invalid
files do not replace a valid lower layer.

Project command files are inactive until the existing project configuration is
trusted for its exact canonical paths and config bytes. This approval covers
later command-file contents, as with project agent definitions; changing the
bound config invalidates it. A moved/replaced workspace, symbolic links, special
files, invalid UTF-8 or oversized definitions are refused. Discovery reads at
most 1024 entries and 128 Markdown files per layer, at most 64 KiB per template.
No optional command directory is created merely to list commands.

`mode` names an existing prompt for one turn; prompt/persona selection and the
user's security mode restore on completion, failure or interruption. An authored
leading `%%mode=` uses the existing downgrade-only security policy. `model` uses
the usual `/model` quick-alias-first resolution, provider and pricing rules.
**Model switches persist**, just as with existing dot-prompt quick-model
switches; model restoration is not implied by one-shot prompt selection.
Templates cannot declare `%%grant` or `%%agent`, or arm an embedded grant offer.

`$ARGUMENTS` inserts the raw argument suffix; `$1` through `$128` insert positional
arguments using the normal slash quoting rules. Missing positions insert empty
text. Substitution is single-pass and literal: there is no shell, environment,
command or directive evaluation. Argument text beginning with `/`, `!`, `@` or
`%%mode` remains user-message text.

Only authored `@path` or `@{path with spaces}` tokens include a file. Paths use
forward slashes and stay relative to the captured workspace, without traversal
or links. Up to eight UTF-8 files, each at most 64 KiB, are included as fenced
untrusted data. Current read policy must already allow each include; deny or
approval-required reads fail visibly before changing the prompt or model.
Includes and supplied arguments cannot introduce another include or directive.
Arguments are limited to 64 KiB and the expanded message to 256 KiB, followed by
the normal context-budget check. Templates submit prompts through normal tools
and permissions; they never execute their body as shell or slash commands.
### Context composition

`/context` shows estimates by section and the five largest live retained tool
results, identified by bounded, sanitized tool names and call IDs. It prints no
prompt, message, result or spill-file content. Hidden redo history is excluded.
Inspecting context does not prune results, compact history, grant permissions,
connect MCP, warm the provider cache or charge usage.

An existing agent provides the exact preamble ranges captured at construction
and its already-registered tool definitions. Before an agent is built, the
report labels the last captured cold preamble estimate and unavailable schemas;
it does not initialize an agent to fill that gap. Metadata retrieval has a
100 ms timeout, 128-definition limit and bounded serialized estimate. Oversized
or unavailable metadata is reported explicitly, rather than counted as zero.

Sections use the local token estimator, not separately measured provider usage.
A signed calibration/tokenizer adjustment accounts for section rounding, schema
accounting and the current provider anchor so the total exactly matches the
context count in the status line. The pruning projection uses the same retained
history policy as the next request. The displayed compaction thresholds do not
trigger compaction.

## Agent Skills

`/skills` lists currently authorized instruction skills with their names, origins and descriptions,
and opens the command picker. `/<skill-name> [arguments]` pins that exact skill for the next ordinary
user turn, including its automatic continuations. The arguments are literal user text: they do not
expand shell variables, file includes or permission directives. A later explicit user prompt removes
the pin. Built-in commands and custom command templates take precedence over skill names.

Discovery checks explicitly imported packages first, then captured project `.zerostack/skills`,
`.mini-agent/skills` and `.claude/skills`, then the captured user's `~/.claude/skills`. First valid name
wins. Project sources use the existing project-config trust fingerprint; each original source also
requires current read authority. An Ask or Deny source is omitted without prompting or granting.
Links, special files, hard links and escaping resources are refused. Scripts and `allowed-tools`
remain inert. A pin that has changed, lost authority or cannot fit the shared context budget fails
visibly before adding the prompt to history or making a model request.

The default `agent-skills` feature enables instruction discovery without the learned-JS store.
`skills` additionally enables the existing learned-JS verification, ABI and capability lifecycle.
Automatic matching remains lexical on the deterministic backend; semantic ranking uses the
explicitly configured semantic embedder in learned-skills builds. `--no-context-files` suppresses
automatic directory discovery; explicit `/skills` and skill commands still apply normal trust and
read checks. While a turn is busy `/skills` displays only its already captured catalogue.
At startup the picker's skill entries are discovered in the background and appear in the `/`
picker once discovery finishes; the first frame does not wait for them.

For the actual native Anthropic adapter, the initial selected instruction/learned block stays fixed
for the captured turn. Later successful `skills_search` calls carry new bounded guidance in their
actual canonical tool result, under the static instruction policy; system and tool definitions stay
unchanged before retained signed thinking. This is not opaque replay across frontend turns (that
separate lifecycle is still governed by native history compatibility). Redaction, spill rewriting,
or removal of an expected native result stops locally before another model request and never
restores removed bytes. The initial block is at most 64 KiB, search metadata 8 KiB, a complete native
search result 80 KiB, and the exact runner retains at most 16 delivery receipts. These are consistency
checks, never permission grants or cryptographic signature verification. Combined learned/instruction
search stages its callable and instruction selections until the exact captured turn's intact final
host result commits them once. A rewritten or spilled result leaves live callable lookup, routing
and attribution unchanged; cancellation or a later user turn rejects stale candidates. Existing
learned ABI, lease, scope and admission checks remain in force.

Typing `?` into an empty input editor shows the keyboard help locally, including while busy.
A `?` in a nonempty draft or pasted text stays literal; help adds no chat-history entry.

`/models refresh` updates the current route's available models using bounded
native metadata GETs (ten seconds for the entire listing). `/models` and the
model picker then use the completed cache. Refresh does not change the active
model, session context window or captured prices. Live-only IDs with no known
catalogue/configured price show `(no price known)`; available IDs do not imply
image, tool or reasoning support. Startup for baked providers remains offline.

## Input editing preferences

`/keys` shows the effective configured keyboard actions. `/vim [on|off]` toggles
Vim editing for this UI session; `vim_mode = true` enables it at startup.
Escape switches Insert to Normal, `i` resumes insertion, `hjkl` moves, `w`/`b`
moves by word, `dd` deletes a line, `u` undoes input edits, and `/` searches
literally (`n` repeats, Escape cancels). The status bar shows the current mode
and search query. These commands affect the input only and remain available
while a model turn runs. Use `[keys]` in trusted config to replace shortcuts;
see [Input keyboard settings](input-keybindings.md).

`/config [--global|--project|--local] [key [TOML value]]` opens the known-setting
picker or validates and saves one setting. The picker shows current startup
values, types, sources and restart requirements. Sensitive project changes require
approval of the exact candidate; current permission/provider authority is unchanged.
See [interactive settings](CONFIG.md#interactive-settings-editor).

With the `multimodal` feature, Ctrl+V can paste a clipboard image into an idle
prompt (terminals may reserve Ctrl+V; an empty bracketed paste also requests an
image). The input footer shows `[image 1: WIDTHxHEIGHT png]`; type a question or
press Enter to send the image alone. `/drop 0` removes the first pending
attachment. Pasting stages data and makes no model request. Sent clipboard
images are saved inline with the session, rather than reopened from a file on
resume. Images over 2 MiB, more than eight images, or over 4 MiB total are
rejected. macOS uses its system clipboard service, Linux needs `wl-paste` or
`xclip`, and Windows uses its system PowerShell clipboard service.
