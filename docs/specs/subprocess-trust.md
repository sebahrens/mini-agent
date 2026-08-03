# Subprocess Trust Classes and Launch Contracts

- **Document role**: normative cross-cutting specification
- **Specification version**: 1.0.0
- **Delivery status**: contract delivered; hardening gaps remain tracked separately
- **Owner**: mini-agent maintainers
- **Last reconciled**: 2026-08-01

The corpus authority and conflict rules are defined in [`00-index.md`](00-index.md). This document
controls how mini-agent selects a subprocess boundary and records the authority crossing that
boundary. [`phase-2-sandbox.md`](phase-2-sandbox.md) continues to control the concrete `bwrap` and
Seatbelt capability matrices for model-generated actions.

## Core invariant

A subprocess profile is selected from **who authored the executable/arguments** and **what
authority the child needs**. The mere fact that code uses `Command`, `.spawn()`, `.output()`, or
`.status()` never determines the profile.

Every production launch must name exactly one trust class below before it can merge. Unknown or
ambiguous authorship, an unclassified launch site, invalid argv for the selected grammar, and an
unavailable requested containment backend all deny launch. A caller must not silently retry under
a more privileged profile.

## Required launch contract

The owner of a launch site must make all of these fields reviewable, even when the value is
"ambient", "not required", or "unsupported":

1. stable trust-class identifier and launch-site owner;
2. authoring principal for executable, arguments, and any interpreted script;
3. argv grammar, including which fields are opaque shell text and which are direct arguments;
4. working directory and how it is resolved;
5. environment construction and credential inheritance;
6. required filesystem and network authority;
7. sandbox/containment state and requested-backend behavior;
8. permission decision and a collision-free, durable audit identity;
9. wall-clock, stdout, stderr, and combined-output limits;
10. caller cancellation, direct-child reaping, and descendant/tree cleanup; and
11. supported platform behavior, including explicit gaps.

User-controlled values must remain data in a direct argv grammar. A profile that permits an opaque
shell program must say so explicitly; quoting a value does not convert that grammar to direct
exec. Configuration trust and command execution permission are separate decisions.

## Trust classes and normative contracts

"Current gap" text is an audit result, not permission to preserve the gap. Follow-up hardening may
narrow a class without changing its identity, but broadening authority requires this specification
to change first.

| ID and current launch sites | Owner / principal and argv grammar | CWD | Environment / credentials | Filesystem / network | Containment and requested backend | Permission / audit | Resources, cancellation, and cleanup | Platform status |
|---|---|---|---|---|---|---|---|---|
| `TC-MODEL-ACTION` — `BashTool::call`; JS `make_spawn_with_timeout`; shared constructors in `Sandbox::wrap_command`; terminal launch in `Sandbox::run_built_output_command` | Model authors the Bash script or JS `cmd`/`args`. Bash uses the configured shell with opaque `-c` text. JS uses fixed `exec "$0" "$@"` shell text and appends program/arguments as distinct argv entries. | Canonical current process workspace at launch; `bwrap`/Seatbelt wrappers set it explicitly. | Requested `bwrap`/Seatbelt clears ambient credentials and restores only the Phase 2 allow-list. `zerobox` environment behavior is backend-defined and unknown to mini-agent. An explicit unsandboxed choice inherits ambient environment. | Requested `bwrap` and Seatbelt deny IP network and enforce their Phase 2 filesystem policies. `zerobox` requests workspace writes only; its filesystem reads, environment, process namespace, devices, and network authority are backend-defined/unknown. Unsandboxed mode has ambient host authority. | Shared `Sandbox`; never a raw JS launch. `requested-but-unavailable` starts no child and never retries unsandboxed. `zerobox` must not be reported as satisfying the `bwrap` or Seatbelt capability matrix. | Mandatory `bash` permission for the complete Bash script or structural JS program/argv identity; denial/cancellation has no effect. Sandbox capability state is user-visible. **Current gap:** JS joins argv with spaces for permission lookup, so distinct argv vectors can collide and must be replaced by a canonical structural key. | 30 s maximum; 1 MiB stdout, 1 MiB stderr, 1.5 MiB combined; cancellation kills and reaps the process group. | Linux `bwrap` and macOS Seatbelt guarantees are exactly those in Phase 2. `zerobox` has only backend-defined, capability-reported behavior on every platform. Windows has no isolation claim; requested isolation may not be reported as active without a defined backend. |
| `TC-EXPLICIT-USER-SHELL` — `Startup::dispatch_print`, `App::run_bang_command`, and `Sandbox::run_explicit_shell` | Human authors the opaque text after `!`; the complete suffix, including whitespace, is passed unchanged to the configured shell. This is not model-generated authority and never enters BashTool permission grammar. | Current process workspace is captured immediately before construction and applied explicitly to the child/wrapper. | A selected `bwrap`/Seatbelt boundary clears ambient credentials according to the general sandbox capability matrix. The explicit `user-trusted-bypass` and the distinct `unsandboxed-unavailable-default-fallback:<backend>` state inherit the parent environment and credentials. | Authority is exactly the selected general sandbox policy; either unsandboxed state inherits ambient host filesystem and network access, but only `user-trusted-bypass` claims a human-selected bypass. | Uses the configured general `Sandbox`, never the broker-only worker profile. `requested-but-unavailable` starts no child and never retries unsandboxed. Explicit `--no-sandbox` is reported as `user-trusted-bypass`; implicit fallback after an unavailable default is labeled separately and never misattributed to the operator. | Typing `!` is the authorization event. The shared audit event records the exact command string, class, captured cwd, selected boundary, and outcome; it records no environment values or output. Audit metadata is owned by the lifecycle worker so caller-drop cancellation is emitted only after cleanup. Both callers persist the original interaction in the ordinary session path when sessions are enabled. | 30 s; 1 MiB stdout, 1 MiB stderr, and 1.5 MiB combined. Concurrent drain plus operation cancellation kills and reaps the process group on timeout, cancellation, output cap, caller drop, and ordinary shell exit. Headless and TUI use the same rendered status policy. | Uses the configured shell contract. Unix process-group teardown and terminal caller-drop audit are tested, including TERM-ignoring descendants; Windows tree cleanup remains unsupported and must not be claimed. |
| `TC-LOOP-VALIDATION` — shared `loop::validation::start` called by headless and interactive loops | Human authors `--loop-run` opaque shell text. mini-agent selects the configured POSIX shell `-c` or PowerShell `-Command`; model output never becomes this command. | Current process workspace is captured by the selected `Sandbox` wrapper. | Requested `bwrap`/Seatbelt clears ambient credentials according to its capability matrix; an explicit unsandboxed choice inherits the project toolchain environment. | Workspace/project files and the network policy of the selected sandbox. | Uses the shared workspace-capable `Sandbox`, not broker-worker containment. A requested unavailable backend fails closed. | CLI configuration is the authorization event. Audit must identify the command, iteration, cwd, and outcome. **Current gap:** no shared audit event. | 30 s, 1 MiB stdout, 1 MiB stderr, and 1.5 MiB combined defaults; concurrent drain and operation-scoped cancellation kill and reap the Unix process group on timeout, cancellation, cap breach, parent drop, and ordinary exit. Headless and interactive paths share the same result rendering. | POSIX shell on Unix/macOS and PowerShell on Windows. Unix tree cleanup is tested; Windows descendant cleanup remains unverified. |
| `TC-PROJECT-AUTOMATION` — `run_hook_with_limits` and `run_shell_condition` | Managed/global hook owner or human-confirmed project binding authors it. Handler `command` + required `args` is direct exec. An `if` condition explicitly uses opaque `sh -c`/PowerShell text. | Inherited startup workspace; `$ZEROSTACK_PROJECT_DIR` identifies the resolved project. | Currently ambient environment plus `ZEROSTACK_PROJECT_DIR`; credentials therefore cross the boundary. Narrowing must be class-specific and must not silently break managed hooks. | Workspace/project access and hook-declared network needs; not assumed safe merely because the child is a hook. | Separate trusted automation runner, not `TC-MODEL-ACTION` and not `TC-BROKER-JS-WORKER`. Requested containment must fail closed. **Current state:** no OS sandbox. | Managed/global source policy plus hash-bound interactive trust for project executable argv and condition; headless unconfirmed project hooks are skipped. Dispatch result supplies the outcome audit channel. | Configured timeout; 1 MiB stdout, 1 MiB stderr, 1.5 MiB combined; output failure/timeout kills, waits, and cleans the Unix process group. **Current gap:** Windows descendant cleanup is not proven. | Direct exec is portable; condition shell differs by platform. Unix process groups are implemented; Windows tree semantics remain unsupported. |
| `TC-MCP-STDIO` — `McpClientHandle::connect` command arm, `stdio_command`, `Sandbox::wrap_workspace_service`, and `TokioChildProcess::builder(...).spawn()` | Human-trusted MCP configuration authors program and direct args. PATH resolution occurs once before spawn and the resolved absolute executable identity is passed without a shell. The remote model only chooses exposed MCP tools; it does not author the server launch. | Configured `cwd`, resolved relative to the connection workspace; omission captures and explicitly applies that workspace. | Ambient environment is cleared. `env` delegates explicit values; `inherit_env` names individual approved parent values, with explicit values winning. Neither values nor stderr beyond the diagnostic cap are logged. | Workspace-capable. Top-level `network = "inherit" | "deny"` records service authority separately from tool permission. Uncontained `inherit` uses the host network; `deny` requires an enforcing backend and otherwise starts no child. | Omitting `sandbox` is a truthful human-trusted bypass, not an active-boundary claim. A requested backend uses the dedicated direct-exec workspace-service profile and fails closed when missing, unsupported, or unable to enforce requested network denial. It never reuses broker-worker containment. | Configuration trust authorizes server start; MCP tool calls retain their own permission/audit identity. Built-in remote identities do not turn deserialized command servers into built-ins. Sandbox and network state never grant tool-call permission. | 10 s initialize deadline and at most 8 KiB rendered diagnostic stderr. The RMCP adapter owns `KillOnDrop` plus a Unix process group or Windows Job Object and explicitly terminates the group after graceful direct-child cleanup because a descendant may close protocol pipes first. Init error/timeout, cancelled connect, reconnect replacement, explicit shutdown, and handle drop cancel, kill, and reap the tree. | Unix process groups and Windows Job Objects are selected at compile time through `process-wrap`. Tests exercise Unix descendants; Windows Job Object parity is provided by the transport wrapper and remains a required platform CI row. |
| `TC-LSP-SERVICE` — `LspClient::spawn` | Built-in registry or trusted user LSP configuration authors program, direct args, and env overrides; binary resolves through PATH. Model edits trigger lazy startup but do not author argv. | Must be the resolved LSP project root. **Current gap:** launch inherits process cwd instead of setting `root`. | Ambient environment/credentials plus configured overrides, because language servers need project toolchains. Secret overrides must never be logged. | Full project/workspace reads and common build-cache writes; network may be needed for toolchains. | Dedicated workspace-service profile, never the broker-only worker profile. Requested service containment fails closed. **Current state:** no OS sandbox. | Enabling trusted LSP config authorizes launch; audit identifies server name, executable identity, root, and lifecycle outcome. **Current gap:** trace logging is not a complete audit record. | 15 s initialize request deadline and `kill_on_drop`/`start_kill`. **Current gaps:** stderr is unbounded, drop does not wait/reap, and descendant cleanup is not defined (`mini-agent-7r1a.4`). | Spawn is cross-platform; Unix/Windows tree cleanup and resource parity are unverified. |
| `TC-INTERNAL-GIT` — every `Command::new("git")` in `extras::git_worktree` (`detect`, `current_branch`, `default_branch`, `create`, merge/cleanup/conflict helpers, `run_git*`, status, and auto-commit); `Session::detect_git_status`; `/undo` stash | mini-agent authors a closed git subcommand grammar. User/worktree paths and branch names remain individual argv values, never shell text. | Current repository or explicit `git -C <path>`. | Ambient Git environment and credentials; remote fetch/pull may need user credential helpers. | Selected repositories/worktrees; network is needed only by explicit fetch/pull operations. | No model or broker containment. Optional containment must preserve selected repositories and credential-helper behavior and fail closed when requested. | UI/CLI operation is authorization; audit must include normalized subcommand class, repository, outcome, and whether a mutating operation was explicit/automatic. **Current gap:** no common audit or runner. | Must use operation-specific finite bounds and cancellation/tree cleanup. **Current gap:** synchronous `.output()` calls are unbounded and can freeze the TUI. | Git-dependent on all supported platforms; shell-free argv is portable. Cancellation/tree parity is not implemented. |
| `TC-SUPPORT-UTILITY` (direct argv) — `show_get_started` (`less`), Unix/macOS branches of `open_url` (`xdg-open`/`open`), `copy_to_clipboard` (`wl-copy`/`xclip`/`pbcopy`/`clip.exe`), and `App::run_lazygit` | mini-agent selects the executable and fixed options. Human-selected document/URL/action remains one direct argv value; clipboard content is written to stdin. No shell interprets these arguments. | Current process workspace is captured and explicitly applied by the hardened lazygit runner; `less` receives the resolved global documentation path. | Ambient desktop/session environment and credentials required by the selected utility. | Selected document/workspace plus desktop IPC; a browser opener may cause network access in the launched application. | No broker containment. Lazygit is explicitly classified as `user-trusted-bypass`; requested restriction never silently falls back. | The initiating UI action authorizes launch. Lazygit emits utility kind, captured cwd, bypass boundary, and every probe/launch outcome without output. The inherited-stdio worker owns its audit metadata and emits the terminal outcome only after cleanup, including caller-drop cancellation; the remaining direct utilities still lack shared audit. Never record clipboard contents. | Lazygit's captured version probe has a 5 s / 64 KiB bound. Its inherited-stdio launch has a finite 24 h bound, caller-drop cancellation, and process-group kill/reap; tests cover timeout, caller drop, TERM-ignoring descendants, terminal cancellation audit, and a subsequent launch. **Current gap:** the other direct utility launch sites still lack a common lifecycle policy. | Executable lists differ by platform and missing tools fall through where documented. `less`, `xdg-open`, and `open` are Unix/macOS paths; direct `clip.exe` is the Windows clipboard path. Unix lazygit tree teardown is tested; Windows parity remains unsupported. |
| `TC-SUPPORT-UTILITY` (opaque editor shell) — `InputEditor::open_in_editor` and `edit_memory_file_with_shell` | Human/configuration authors opaque editor shell text. mini-agent invokes `sh -c '<editor> "$1"' sh <temp-path>`; the temp path remains positional parameter `$1`, but the editor string is intentionally shell grammar. | Current process workspace; the selected temporary file is passed as `$1`. | Ambient shell/editor environment and credentials. | The selected temporary or memory file plus whatever desktop/project access the configured editor needs; network is editor-defined. | No broker containment. Any requested restriction is a support-utility policy and fails closed. | The editor action authorizes the configured shell program. Audit records editor identity, cwd, temporary-target class, and outcome, never file contents. **Current gap:** no shared audit. | Human-controlled interactive lifetime; requires cancellation and direct-child/tree reaping. **Current gap:** raw `.status()` has no common lifecycle policy. | Currently hard-coded to `sh`; this grammar is supported only where that shell contract exists. Windows editor launch is unsupported rather than portable direct argv. |
| `TC-SUPPORT-UTILITY` (Windows command interpreter) — `open_url` fallback `cmd /c start <url>` | mini-agent supplies `/c`, `start`, and the validated URL as separate process argv, but `cmd.exe` subsequently interprets command metacharacters. This is interpreter grammar, not direct argv safety. | Current process workspace. | Ambient Windows desktop/session environment and credentials. | Desktop shell/browser authority; opening the URL may cause browser network access. | No broker containment. Requested restriction fails closed. | The initiating UI action authorizes one HTTP(S) URL. **Current gap:** `is_safe_url` rejects whitespace/control characters but not all `cmd.exe` metacharacters, so a URL can change the interpreted command. Fix is tracked by P1 bead `mini-agent-x9tw`; F01 does not implement it. | The opener is spawned and waited synchronously with no common deadline or tree cleanup. | Intended for Windows. Until `mini-agent-x9tw` replaces or safely encodes the interpreter boundary, this path has an explicit command-injection gap and must not be described as direct argv-safe. |
| `TC-INTERNAL-VERIFICATION` — `verify_workflow_only_headless_relevance` | mini-agent authors fixed Bash harness text from the embedded checked-in policy; no user/model text enters argv. | Repository/startup workspace. | Needs only a minimal tool environment; **current gap:** ambient credentials are inherited. | Read-only repository policy inputs; no network required. | A minimal internal verifier, not model or broker authority. A requested backend fails closed. **Current state:** raw unsandboxed Bash. | CLI verification flag is the audit identity; record policy version and outcome. | Requires a short deadline, bounded output, cancellation, and reaping. **Current gap:** `.status()` is unbounded. | Unix-only by compile gate; there is no Windows claim. |
| `TC-LIFECYCLE-HELPER` — `Sandbox::run_built_output_command`, `Sandbox::run_built_status_command`, and `sandbox::kill_process_group` | The owning caller supplies an already-constructed command; the helper adds no argv grammar or authority. It spawns the child with either captured or inherited stdio, concurrently drains captured pipes, and uses safe `nix::killpg` calls with fixed `SIGTERM` then `SIGKILL` against the validated child process-group ID. | The owning trust-class constructor must set cwd before delegation. | The owning constructor defines environment policy; the lifecycle helper neither adds nor restores values. | The owning constructor defines filesystem/network authority. The helper needs host process-signalling authority so it can terminate the child group. | Runs at the host lifecycle layer outside the child sandbox so it can supervise and terminate the group. It is not callable as a command profile and does not change caller trust. | Audit belongs to the owning caller; audited captured and inherited-stdio callers pass metadata into the owned worker so terminal audit survives caller drop. Signalling failures other than an already-absent group are logged. | Finite caller-supplied deadline and, for captured commands, output caps; receiver-drop cancellation, concurrent captured-stream drains, process-group termination, and direct-child reap. | Unix uses process groups. Windows whole-tree parity is not implemented here and must not be claimed. |

`TEST-ONLY` identifies a launch expression inside a `#[cfg(test)]` block in a production source file;
it is checked for inventory drift but is not a production trust class. `NON-PROCESS` identifies a
lexical match that is a thread/task spawn, HTTP/tool status accessor, type reference, comment, or
the no-effect skill verifier's in-memory fake `spawn`; it never authorizes an OS process.

## Broker-only JS worker is a separate boundary

`TC-BROKER-JS-WORKER` is reserved for the Phase 6 brokered JavaScript evaluator. No current launch
site is assigned to it. Its minimum contract is a mini-agent-authored fixed worker executable and
protocol argv, an empty private cwd, a constructed non-credential environment, no workspace or
application-secret mounts, no direct network, and only bounded broker RPC capabilities. It must
have a hard lifetime/memory/output budget and whole-tree termination.

The broker profile is narrower than `TC-MODEL-ACTION`: it hosts untrusted evaluation and asks the
parent broker to perform separately authorized effects. It is categorically incompatible with
`TC-MCP-STDIO`, `TC-LSP-SERVICE`, `TC-EXPLICIT-USER-SHELL`, `TC-LOOP-VALIDATION`, and
`TC-SUPPORT-UTILITY`, all of which legitimately need workspace, desktop, toolchain, credential, or
long-lived service authority. Shared lifecycle utilities may be reused, but a trust profile may
not be selected merely to reuse implementation code.

## Requested-backend state machine

Every class that offers optional containment represents state explicitly:

| State | Required behavior |
|---|---|
| Not requested | Launch only if the class permits ambient execution; report/audit that no backend is active. |
| Requested and available | Validate the backend identity and policy, construct the class-specific boundary, then launch once. |
| Requested but unavailable/invalid | Deny before child creation. Do not retry ambiently, choose another profile, or report configured capabilities as active. |

An explicit opt-out may select "not requested" only before launch and only for a class whose
contract permits it. A failed requested launch can never be reinterpreted as an opt-out.

## Exact launch-site audit

The checked inventory in `src/tests/subprocess_inventory_tests.rs` records the multiset of every
production-source line found by the required lexical audit:

```text
Command::new | tokio::process | .spawn( | .output( | .status(
```

The inventory excludes dedicated test directories, retains inline test and false-positive matches
as `TEST-ONLY`/`NON-PROCESS`, and assigns every remaining match to one current class above. An
explicit current-class allow-list excludes `TC-BROKER-JS-WORKER`. Every disposition and every site
in a file with multiple production classes has an exact fingerprint-and-occurrence ownership rule;
only files whose remaining launches share one production principal use a file-family rule. Thus a
launch cannot satisfy the check by borrowing another site or comment's class token. The inventory
counts identical fingerprints per file, so adding a second identical `Command::new("git")` still
fails. A new or changed match fails the default test until its owner audits the full launch contract,
updates this normative table when needed, and adds the required site or single-class-family ownership
reference. Removing a site also fails until the stale inventory and ownership rule are removed.

The audit currently resolves as follows:

| Classification | Exact source families covered |
|---|---|
| `TC-MODEL-ACTION` | `src/sandbox.rs` shell/zerobox/Seatbelt/bwrap constructors; Bash and JS callers are named in the normative table. The bounded output terminal is shared lifecycle infrastructure. |
| `TC-EXPLICIT-USER-SHELL` | Delegating calls in `src/startup.rs` and `src/ui/app.rs::run_bang_command`; shared construction, capture, audit, and rendering live in `src/sandbox.rs`. |
| `TC-LOOP-VALIDATION` | Shell constructors/output terminals in `src/extras/loop/headless.rs` and `src/ui/event_handler.rs`. |
| `TC-PROJECT-AUTOMATION` | Tokio process type/import, direct constructor, and terminal spawn in `src/extras/hooks/subprocess.rs`. |
| `TC-MCP-STDIO` | Tokio process import/type and RMCP terminal spawn in `src/extras/mcp/client.rs`. |
| `TC-LSP-SERVICE` | Tokio child/stdin types, constructor, and terminal spawn in `src/extras/lsp/client.rs`. |
| `TC-INTERNAL-GIT` | All constructors/output terminals in `src/extras/git_worktree/mod.rs`, `src/session/mod.rs`, and `src/ui/slash/session.rs`. |
| `TC-SUPPORT-UTILITY` | Direct-argv sites: pager in `src/docs.rs`, lazygit in `src/ui/app.rs`, and opener/clipboard constructors in `src/ui/renderer.rs`. Opaque editor-shell sites: `src/ui/input/mod.rs` and `src/ui/slash/memory.rs`. Windows command-interpreter site: the `cmd /c start` opener fallback in `src/ui/renderer.rs`. |
| `TC-INTERNAL-VERIFICATION` | Fixed embedded-policy Bash constructor/status in `src/extras/loop/mod.rs`. |
| `TC-LIFECYCLE-HELPER` | Shared bounded captured-process terminal and in-process Unix process-group signalling in `src/sandbox.rs`. Caller trust remains the caller's class; this helper grants no authority itself. |
| `TEST-ONLY` | Inline Bash-tool process-existence helper in `src/agent/tools/bash.rs`. |
| `NON-PROCESS` | Lexical exclusions in ACP, export/HTTP, JS runtime/skill thread tasks, fake verification, and source comments. |

## Review and change rules

- A new launch site must update the checked inventory in the same change.
- Every current launch classification must be in the current-class allow-list. `TEST-ONLY`,
  `NON-PROCESS`, and sites in mixed-production-class files require exact fingerprint-and-occurrence
  ownership; file-family ownership is valid only when every remaining launch has one production
  principal. `TC-BROKER-JS-WORKER` remains forbidden until an actual Phase 6 worker boundary exists
  and its contract is reviewed.
- A new trust class or broader authority must update this file before implementation.
- Moving an existing launch to a shared runner must preserve its class; sharing code never merges
  principals.
- Logs and errors must report requested/active containment distinctly and must not expose inherited
  environment values, clipboard data, editor contents, hook stdin, or credentials.
- Platform support is a verified runtime property. Compilation alone cannot upgrade an
  "unsupported" or "unverified" contract field.

## Acceptance criteria

- [x] Every current production lexical launch match maps to a stable trust class or an explicit
      non-process/test-only exclusion.
- [x] Every class records principal, argv, cwd, environment/credentials, filesystem/network,
      containment, permission/audit, resource/cancellation/tree, and platform semantics.
- [x] Requested containment failure is fail-closed and cannot silently downgrade authority.
- [x] The broker-only JavaScript worker is distinct from workspace services, explicit user
      commands, and the current shared model-action sandbox; no current launch may claim it.
- [x] The default test suite contains a drift check for newly added, changed, duplicated, or
      removed launch expressions.
