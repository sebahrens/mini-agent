---
description: "Complete reference of zerostack slash commands, keyboard shortcuts, and input prefixes for the terminal UI."
---

# Slash Commands

All slash commands are available from the TUI input prompt.

## Session

| Command | Description |
| ------- | ----------- |
| `/clear` | Clear the current session (all messages, tokens, compactions) and persist the change immediately. |
| `/new` | Alias for `/clear`. |
| `/undo` | Remove the last exchange (user message + assistant response), persisting both the shorter history and its redo point immediately. The working tree is never touched. |
| `/undo stash` | Same as `/undo`, then `git stash` the working tree. There is no interactive prompt; stashing only happens when asked for explicitly. |
| `/redo` | Restore whatever the most recent `/undo` or `/rewind` removed and persist the restored history immediately. |
| `/rewind` | Open a picker to jump the session back to an earlier point. |
| `/retry` | Load the last user message into the input editor for editing. |
| `/quit` | Exit zerostack. |
| `/exit` | Alias for `/quit`. |
| `/sessions` | List recent saved sessions (up to 20). |
| `/sessions <id-or-name>` | Load a session by its ID prefix or name. |
| `/sessions delete <id-or-name>` | Delete a session by its ID prefix or name. |
| `/rename <name>` | Rename the current session. |
| `/history` | Show global chat history (last 10 entries across sessions). |
| `/export [file]` | Export the current session to a standalone HTML page (default `zerostack-session-<id>.html`), or to JSONL when the file ends in `.jsonl`. Requires the `export` feature (default-on). |
| `/import <file>` | Import a session from a versioned zerostack JSONL export (or a native session JSON document), save it, and load it. Schema markers select the format deterministically; imports are limited to 16 MiB and 10,000 messages, and external native files cannot inject a hidden redo snapshot. Requires the `export` feature. |
| `/share` | Upload the HTML export as a secret GitHub gist and print the URL. Requires `GITHUB_TOKEN` or `GH_TOKEN` and the `export` feature. |
| `/queue` | List input queued while the agent is busy (same as `/queue ls`). |
| `/queue clear` | Empty the queue. |
| `/queue pop` | Remove the last queued input. |
| `/welcome` | Show the welcome/onboarding screen. |
| `/tutorial` | Alias for `/welcome`. |
| `/tutor` | Open the full getting-started guide in `less`, or print it when no pager is available. |

HTML exports treat every session field as untrusted. Raw HTML is displayed as escaped text;
assistant Markdown keeps normal formatting, HTTP(S)/`mailto:` or relative links, and HTTPS images,
while executable/active schemes, non-HTTPS images, and tags are removed. The page also carries a
restrictive Content Security Policy as defense in depth. JSONL exports preserve the original text
and, for newly recorded tool interactions, the correlated tool-call name, arguments, identifier,
and bounded tool result used to resume the model conversation. Older exports without those optional
fields remain importable and replay their tool records as labeled transcript text.

## Provider & Model

| Command | Description |
| ------- | ----------- |
| `/provider` | Show the current provider. |
| `/provider <name>` | Switch to a different provider. |
| `/model` | Show the current model. |
| `/model <name>` | Switch to a different model. |
| `/models` | List all quick models defined in config. |
| `/models <name>` | Switch to a named quick model. |
| `/models-add <name> <provider> <model>` | Save a new quick model to the config file. |

## Context Files

| Command | Description |
| ------- | ----------- |
| `/add` | List files currently added to context (with sizes). |
| `/add <path>` | Add a file to the agent's context (absolute or relative path). |
| `/drop <path>` | Remove a file from the agent's context. |
| `/drop-all` | Remove all added files from the agent's context. |

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
| `/mode` | Show the current security mode. |
| `/mode standard` | Allow path tools within CWD, ask for external paths. Config rules apply. |
| `/mode restrictive` | Ask for every operation. Config rules skipped. |
| `/mode readonly` | Allow reads only; deny writes, edits, shell, and everything else. |
| `/mode planwrite` | Read-only except for the built-in, workspace-contained plan-file write exception. |
| `/mode guarded` | Allow reads; ask for writes, edits, shell, and everything else. Config rules apply. |
| `/mode yolo` | Allow everything; ask for destructive shell commands. Config rules apply. |

Prompts can set the security mode automatically via `%%mode=<mode>` on
the first line. When a prompt with `%%mode=last_user_mode` is activated,
the mode reverts to whatever was last set explicitly by `/mode` or
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
| `/agent <name>` | Apply a persona to the main loop and activate its optional `mode:` prompt. |
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

## Conversation

| Command | Description |
| ------- | ----------- |
| `/compress [instructions]` | Compress conversation history to free context window space. |
| `/compact` | Alias for `/compress`. |
| `/editsys` | Show the current edit system mode (similarity or hashedit). |
| `/editsys similarity` | Use SEARCH/REPLACE with fuzzy matching for edits (default). |
| `/editsys hashedit` | Use CRC-32 tag-based edits (token-efficient, CAS-guarded). |
| `/btw <message>` | Ask a quick side question in parallel, without touching the main conversation. It forks the current context (including the main agent's in-flight turn, if any), answers using read-only tools (read/grep/find_files/list_dir, no writes or shell), and prints the answer inline. Works even while the main agent is running. Nothing is written to history; its token cost is shown separately as `btw:$…`. Ctrl-C cancels an in-flight `/btw` without disturbing the main agent. |
| `/reasoning` | Toggle LLM reasoning on/off (requires model support). |
| `/thinking` | Alias for `/reasoning`. |
| `/review [msg]` | Run a one-shot code review. Activates the `review` prompt in readonly mode, submits a review message, and restores the previous prompt afterward. Without a message, auto-generates one based on session and worktree context. |
| `/toggle` | Show toggleable features, runtime availability, and current-workspace learned-skill service failures or degradation with retry status. |
| `/toggle todo [on\|off]` | Enable or disable todo-list tools. |

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
| `/mcp` | List connected MCP servers and their tool counts. |
| `/mcp <server>` | List tools of a specific MCP server. |
| `/mcp login <server>` | Run the OAuth 2.0 login flow for a URL server, then reconnect it. |
| `/mcp logout <server>` | Remove a server's stored OAuth token. |

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
| `/model-subagent` | Show the model currently used for subagents. |
| `/model-subagent <name>` | Switch the subagent model. |
| `/models-subagent` | List quick models available for subagents. |
| `/models-subagent <name>` | Switch subagents to a named quick model. |

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

## Loop (feature-gated)

`--loop-plan <path>` selects the plan used for both reading progress and the
model's update instructions (default: `LOOP_PLAN.md`). When that file exists,
startup asks whether to resume if stdin is a terminal; unattended runs resume
automatically without reading stdin. To start fresh unattended, remove the plan
before launching or select a new path.

The optional `--loop-run <command>` validator uses the selected process sandbox
and the same captured shell contract as the model-visible shell tool (`-c` for
Bash/sh or `-Command` for PowerShell/pwsh). Headless and interactive loops share
one bounded runner: each validation has a 30-second deadline, 1 MiB caps for stdout
and stderr, and a 1.5 MiB combined cap. Timeout, cancellation, output-limit,
launch-failure, and nonzero-exit results are recorded with explicit status and
separate sanitized stdout/stderr sections. Resource-limit cancellation reaps
the direct validator everywhere and, on Unix, kills its complete process group
before the next iteration. Cancellation is operation-scoped: headless Ctrl+C
or Unix SIGTERM waits for that validator's cleanup before exiting, while interactive
Ctrl-C/Ctrl-D is routed semantically (`/btw` first, then validation or the main
run) and never uses validation cancellation to stop unrelated sandbox commands.
Interactive validators carry generation IDs, so cancelling and starting a new
run or loop immediately retires the old generation; a late cleanup completion
cannot advance or respawn the replacement loop.

When a main run is interrupted after assistant or tool progress, the partial
transcript is retained so on-disk edits stay explainable on resume. Any tool call
whose result was not observed is recorded with an unknown-outcome marker. Only
a failure before any agent progress restores the original prompt to the editor
and rolls the transcript back.

| Command | Description |
| ------- | ----------- |
| `/loop [prompt]` | Start the iterative coding loop (bounded to 100 iterations). |
| `/loop stop` | Stop the active loop. |
| `/loop status` | Show current loop status. |

## Shell Commands

Prefix a message with `!` to run it as a shell command instead of sending it to
the agent. The command's output is captured and stored in the session history as
an Assistant message. Works in both TUI and `--print` mode.
In `--print` mode, a failed command or empty `!` exits nonzero. Captured output is still
rendered and saved before reporting a command failure; `--output json` marks that result
with `"stop_reason":"failed"`.

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

| Example | Description |
| ------- | ----------- |
| `!ls -la` | List files in the current directory. |
| `!git status` | Check git status without involving the agent. |
| `!cargo test` | Run tests and capture the output. |
| `!` | Empty command shows an error. |

If you want to run a command and then discuss the output with the agent, just
type `!<command>` first (it stores the output as an Assistant message), then
follow up with a normal message asking the agent about it.

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
`last_user_mode`.

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
| `Ctrl+C` | Cancel the current agent response, validation, or shell command; quit when idle. |
| `Ctrl+D` | Same interrupt/quit behavior as `Ctrl+C`. |
| `Ctrl+Shift+C` | Copy selected text through the Unicode clipboard on Windows. |
| `Ctrl+V` | Paste Unicode clipboard text at the cursor on Windows. |
| `Ctrl+W` | Delete word backwards. |
| `Ctrl+U` | Delete everything before the cursor. |
| `Ctrl+K` | Delete everything after the cursor. |
| `Ctrl+A` / `Ctrl+E` | Move to the start/end of the current input line. |
| `Ctrl+B` / `Ctrl+F` | Move one character left/right. |
| `Alt+B` / `Alt+F` | Move one word left/right. |
| `Alt+D` | Delete the next word. |
| `Ctrl+Y` / `Alt+Y` | Yank the last deletion / rotate the kill ring. |
| `Ctrl+G` | Open the current input in the system editor (`$EDITOR`). |
| `Ctrl+H` | Launch `lazygit` (git TUI) in the project directory. |
| `Ctrl+R` | Toggle reasoning visibility. |
| `@<query>` | Activate the file picker; Tab/Enter selects and Escape closes it. |
| `Tab` | Insert two spaces when no picker is active. |
| `Up / Down` | Move vertically in multiline input; at an edge, navigate command history. |
| `PageUp / PageDown` | Scroll viewport. |
| `Home / End` | Jump to the top/bottom of chat history. |
| `Escape` | Close active picker / cancel. |
| Mouse drag | Select text and copy it on release. |
| Mouse scroll | Scroll chat history. |

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
