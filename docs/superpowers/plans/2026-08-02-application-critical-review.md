# Mini-agent Application Critical Review and Remediation Plan

**Date:** 2026-08-02
**Status:** Findings complete; implementation work to be tracked in Beads
**Normative authority:** `docs/specs/00-index.md` and the specifications it indexes
**Related live plan:** `docs/superpowers/plans/2026-08-01-brokered-js-runtime.md`

## 1. Executive assessment

Mini-agent's central design is coherent. The parent-brokered JavaScript worker, fresh QuickJS
runtime per request, typed effects, capability intersection, durable audit, learned-skill identity,
and platform containment are justified by the documented threat model. Removing those layers would
make the application simpler only by discarding its core security properties.

The critical weakness is not the Phase 6 architecture itself. It is authority and lifecycle drift
around the rest of the application:

1. interactive usage is charged twice and a positional interactive prompt is persisted without
   running;
2. ACP stores history and workspace state but does not use either, constructs policy differently
   from normal startup, and does not implement protocol cancellation;
3. model-visible LSP diagnostics bypass file permissions, malformed permission configuration fails
   late or is silently discarded, and repeated-read state leaks across sessions;
4. the release trigger, repository coordinates, and claimed packaging support disagree with the
   repository's actual identity and automation;
5. a research crate participates in the production workspace and a generic retry helper has no
   production caller; and
6. the current Phase 6 A20 branch makes a persisted export one-shot, contradicting the normative
   Phase 5 reusable-export contract.

The right response is a set of narrow correctness and deletion workstreams, not a general rewrite.

```text
CLI/TUI ───────────────┐
                      ├──> shared immutable run authority ──> agent + tools
ACP session/root ──────┘                 │
                                        ├──> permission policy
                                        ├──> workspace/root
                                        ├──> context and LSP
                                        └──> sandbox/subprocess cwd

Agent stream ──> one stream of UsageDelta events ──> budget + UI + persisted totals

Persisted skill export ──> reusable wrapper ──> fresh one-shot grant per call
```

## 2. Review method and evidence standard

The review covered the root application, feature modules, tests, release automation, package
metadata, architecture documents, indexed specifications, the live Phase 6 plan, all current Phase 6
worktrees, and all open and closed Beads. Multiple independent subagents audited architecture,
permissions, execution modes, release/package surfaces, dead code, and the ongoing Phase 6 branches.
Narsil MCP was used for indexed symbol, reference, dependency, call-path, dead-code, security,
dependency-vulnerability, and repository-structure checks. The final index contained 286 files and
roughly 104,000 lines. The dependency scan found no known vulnerable dependencies.

Automated findings were accepted only when source inspection established the runtime path and impact.
In particular, Rust exports reported as “unused” were not treated as dead merely because static
cross-crate analysis could not see feature/runtime registration.

`ARCHITECTURE.md` and `SPEC.md` were treated as overviews. They do not override the authority order in
`docs/specs/00-index.md`.

Before this plan was finalized, the Oracle reviewed the evidence, priorities, omissions, and live
Phase 6 overlap. Its main corrections were:

- treat the persisted-export one-shot behavior as a new P0 Phase 6 regression blocker;
- do not create a broad “remove all globals” refactor;
- do not create a TUI asynchronous git-status task;
- do not create a standalone archive-hardening epic;
- do not attack the learned-skills architecture as overengineering; and
- make Nix support truthful by either proving it continuously or removing it.

## 3. What should remain

The following complexity is core functionality, not belly fat:

- the same-executable, fail-closed, parent-supervised JavaScript worker;
- a fresh bounded QuickJS `Runtime`/`Context` for every request;
- typed, parent-executed effects rather than ambient worker authority;
- capability intersection across artifact scope, session policy, current permission, and durable
  intent/audit;
- immutable learned-skill identity and held-out verification;
- explicit platform containment evidence and truthful unavailable/degraded states; and
- the distinction between broker-only worker containment and workspace-capable subprocess trust.

These mechanisms may be simplified internally only when their invariants remain executable and
tested. They must not be replaced by an in-process or uncontained fallback.

## 4. Ranked findings

| Rank | Priority | Finding | Confidence | Impact |
|---|---:|---|---|---|
| 1 | P0 | Persisted skill exports are documented/implemented as one-shot instead of reusable with a fresh one-shot grant per call | High | Normative Phase 5 contract regression; breaks valid repeated calls and loops |
| 2 | P0 | ACP session `cwd` is recorded but ignored by context, tools, LSP, JS roots, permission roots, and subprocesses | High | Cross-workspace authority confusion and session isolation failure |
| 3 | P1 | Interactive provider usage is charged as both per-call deltas and a final aggregate | High | Inflated cost/tokens and premature budget exhaustion |
| 4 | P1 | ACP prior messages are computed and discarded | High | ACP conversations are stateless after the first turn |
| 5 | P1 | ACP independently resolves execution authority and omits normal `read_only`/`guarded` behavior | High | Frontend-dependent permission semantics |
| 6 | P1 | ACP does not handle `session/cancel` and drops the runner abort handle | High | Cancelled client work can continue consuming models and executing tools |
| 7 | P1 | `lsp_diagnostics` reads/surveys files without the permission checker | High | Model-visible filesystem information bypasses read policy |
| 8 | P1 | Permission regexes compile lazily and malformed permission objects are silently defaulted | High | Runtime match-all surprises or silently weakened policy |
| 9 | P1 | Tag-based release documentation contradicts a manual-only workflow; branch dispatch derives an unsafe release identity | High | Tags do not release; manual branch runs can publish under the wrong identity |
| 10 | P1 | Installer, package metadata, docs, and model/provider identity contain the old repository coordinates | High | Installer and release-channel URLs target the wrong repository |
| 11 | P2 | Interactive positional input is inserted into history but never executed | High | Unmatched user turn and confusing no-op startup behavior |
| 12 | P2 | Repeated-read configuration and history are process-global | High | One agent/session changes another session's behavior |
| 13 | P2 | Release artifact actions use mutable major-version tags | High | Release build dependencies can change without a repository diff |
| 14 | P2 | Nix is presented as supported but is unpinned, untested in CI, stale, and structurally broken | High | False packaging claim and maintenance burden |
| 15 | P2 | `spike/` is a nonproduction research crate inside the production workspace | High | Lockfile, CI, lint, and build churn unrelated to the shipped crate |
| 16 | P2 | `retry::with_retry` has no production caller | High | Dead generic abstraction and private tests |

## 5. Phase 6 live-readiness assessment

The live Phase 6 epic is `mini-agent-xic0` with A01–A33. At review time A18, A20, A21, A23,
A24–A27, and A03 were actively being implemented or proven; A22 and A28–A33 remained open.
This plan does not duplicate:

- writer/runner separation in A18;
- capability intersection in A20;
- production-loader verification in A21;
- verification scheduling in A22;
- approval lifecycle in A23;
- platform containment/evidence in A24–A27;
- worker fault, cancellation, availability, CI, footprint, and final docs in A28–A33; or
- the aggregate CI gate in A31.

### P0 regression: reusable persisted exports

`docs/specs/phase-5-evidence-learning.md` requires every wrapper call to receive an invocation ID and
every genuinely new call to receive a new ordinal. Model code may call the same selected persisted
export more than once. The current A20 branch instead documents one parent-issued, one-shot handle for
the persisted export as a whole.

The corrected contract is:

```text
loaded persisted export (reusable)
    call #1 ──> invocation ordinal N   ──> one-shot grant N   ──> consume once
    call #2 ──> invocation ordinal N+1 ──> one-shot grant N+1 ──> consume once

replay grant N ──> denied
expired/revoked export authority ──> later calls denied
```

This must be tracked as a new Phase 6 child, depend on A18/A20/A21/A23, and block final CI/docs from
declaring the architecture complete. It must not weaken one-shot semantics for an individual grant.

## 6. Delivery graph

```text
Phase 6 A18/A20/A21/A23
          │
          ▼
  PH6-34 reusable exports ───────> A31 aggregate CI / A33 final docs

ACP-02 workspace binding ──┐
ACP-03 authority resolver ──┼──> ACP integration verification
ACP-01 history ─────────────┤
ACP-04 cancellation ────────┘

PERM-02 config validation ──────> PERM-01 LSP authorization

REL-01 release identity ───┐
REL-02 action pinning ─────┼──> release rehearsal
REL-03 repository identity ┤
REL-04 remove Nix claim ───┘

SIM-01 isolate spike ───────────> root production quality gates
SIM-02 remove dead retry helper ┘
```

Work in the same file should be serialized even when there is no semantic dependency. In
particular, release workflow tasks must not be assigned concurrently to the same checkout.

## 7. Atomic implementation tasks

### PH6-34 — Restore reusable persisted exports with per-call one-shot authority (P0)

**Defect:** A20 binds one one-shot handle to the stored export, so the second valid invocation fails.
That contradicts the Phase 5 invocation-ordinal contract.

**Primary files/symbols:**

- `docs/specs/phase-5-evidence-learning.md`
- `docs/agent/SKILLS.md`
- `src/extras/js/skills/skill_runtime_binding.rs`
- `src/extras/js/skills/turn.rs`
- `src/extras/js/broker.rs`
- `src/extras/js/protocol.rs`
- `src/extras/js/tests/worker_broker.rs`
- current A18/A20/A21/A23 implementations in their Phase 6 worktrees

**TDD sequence:**

1. Add a broker/loader integration test that loads one verified ABI-v2 export and calls it twice.
2. Assert both calls succeed, use distinct monotonically new invocation IDs/ordinals, and retain exact
   artifact/export attribution.
3. Add replay, expiry, and revocation tests for each individual grant.
4. Move grant minting/consumption from wrapper lifetime to call lifetime.
5. Keep the wrapper/export binding reusable only while parent authority remains valid.
6. Update normative and user documentation to describe reusable exports plus one-shot calls.

**Constraints:** no reusable bearer token; no worker-minted authority; no broadening of artifact scope;
no fallback to ABI-v1 inference; no second runtime lifetime.

**Verification:** focused `worker_broker`/skill-loader tests, `cargo test --locked --all-features`,
cross-platform Phase 6 CI, then `cargo install --path . --debug`.

### INT-01 — Emit and charge one authoritative usage-delta stream (P1)

**Defect:** `src/agent/runner.rs` emits every Rig `CompletionCall` usage and then places Rig's final
aggregate usage in `AgentEvent::Done`. `src/ui/event_handler.rs` charges both. Rig 0.40's final response
already aggregates the run.

**Primary files/symbols:**

- `src/agent/runner.rs::spawn_agent_with_stream_policy`
- `src/event.rs::AgentEvent`
- `src/ui/event_handler.rs::handle_agent_event` and finalization helpers
- `src/session/mod.rs` token/cost fields
- runner and UI event-handler tests

**TDD sequence:**

1. Add a two-provider-call/tool-continuation fixture where final aggregate usage equals the sum of
   call usages; assert persisted/UI cost and token totals equal the aggregate once.
2. Add a provider-adapter fixture with final usage but no preceding `CompletionCall`; assert it is
   charged once.
3. Add a malformed/regressive aggregate fixture; field-wise reconciliation must never underflow or
   double charge and should emit a diagnostic.
4. Make the runner own reconciliation: emit chargeable `UsageDelta` events only. At terminal response,
   emit only any nonnegative difference between final aggregate and already emitted deltas.
5. Make budget enforcement, status/UI totals, cost, and persisted totals consume the same cumulative
   delta state. `Done` must not carry independently chargeable usage.

**Constraints:** preserve provider cache token semantics; do not hard-code Rig event ordering; do not
fix the test by ignoring tool continuations.

**Verification:** targeted runner/UI tests, `cargo test`, `cargo fmt`, and debug install.

### INT-02 — Execute positional interactive input as the first normal turn (P2)

**Defect:** `Startup::dispatch_interactive` adds `cli.message` to the session, then calls
`run_interactive(..., None, ...)`; the model never receives it.

**Primary files/symbols:**

- `src/startup.rs::dispatch_interactive`
- `src/ui/mod.rs::run_interactive`
- `src/ui/app.rs::App::new` auto-trigger path
- startup/UI tests

**TDD sequence:**

1. Add tests for interactive positional text, `-p`, empty input, and resumed sessions.
2. Pass nonempty positional text into the established auto-trigger/normal submission path.
3. Remove the eager history insertion; the turn path must insert the user message exactly once.
4. Assert history passed to the model excludes the current prompt while persisted history contains it
   exactly once after dispatch begins.

**Constraints:** do not change `-p`; do not create a second startup-only runner path; preserve empty
interactive startup and resume ordering.

**Verification:** targeted tests, `cargo test`, debug install, and tmux startup with a positional prompt.

### ACP-01 — Feed prior ACP turns into each prompt run (P1)

**Defect:** `run_prompt` clones `SessionState.messages` into `_extra_messages`, then passes `vec![]` to
`spawn_runner`. ACP is stateless even though it stores turns.

**Primary files/symbols:**

- `src/extras/acp/mod.rs::{SessionState,handle_prompt,run_prompt}`
- `src/tests/acp_tests.rs`

**TDD sequence:**

1. Build a two-prompt ACP integration fixture whose second answer requires first-turn context.
2. Snapshot only prior committed messages before handling the current prompt.
3. Convert prior messages into Rig `Message` values and pass them to `spawn_runner`.
4. Record the current user turn once and record assistant output only with an explicit terminal outcome.
5. Preserve structured tool call/result messages where the runner/protocol exposes them; do not flatten
   them merely to fabricate textual history.
6. Define and test new-session/load-session behavior explicitly. If load is unsupported, advertise that
   truthfully rather than pretending persistence exists.

**Constraints:** no current-prompt duplication; no process-global history; no cross-session sharing.

**Verification:** `cargo test --features acp acp`, full `cargo test`, debug install with ACP feature.

### ACP-02 — Bind every ACP capability to the immutable session workspace (P0)

**Defect:** `NewSessionRequest.cwd` is logged and discarded. Context loading, preamble cwd, permission
root, relative file tools, Bash cwd, LSP root, JS roots, and application path derivation use process
CWD instead.

**Primary files/symbols:**

- `src/extras/acp/mod.rs::{SessionState,handle_new_session,run_prompt}`
- `src/agent/builder.rs::{build_preamble,build_agent_inner,register_js_tool}`
- `src/context/mod.rs` context traversal
- path-capable constructors in `src/agent/tools/`
- `src/extras/lsp/mod.rs::LspManager::new`
- `src/sandbox.rs`/`src/agent/tools/bash.rs` command cwd
- `src/tests/acp_tests.rs`

**TDD sequence:**

1. Create two simultaneous ACP sessions rooted in separate temporary workspaces with distinct sentinel
   files and context instructions.
2. Prove current code sees process-CWD state; then require each session to see only its own root.
3. Canonicalize and validate session cwd at session creation and store it immutably in `SessionState`.
4. Introduce the smallest shared workspace/root value needed by agent construction. Pass it explicitly
   to context loading, preamble generation, permission construction, file tools, LSP, JS allow roots,
   and subprocess `current_dir`.
5. Resolve relative paths against this root; never call `set_current_dir` for ACP.
6. Add concurrent read/write/Bash/LSP tests proving one session cannot resolve relative paths in the
   other workspace.

**Constraints:** do not absorb `mini-agent-8tbo`'s worktree CWD cleanup; do not use a global/TLS cwd;
do not weaken external-path permissions; reject an invalid/non-directory root.

**Verification:** ACP concurrency/isolation tests, all-feature tests, debug install, ACP editor smoke.

### ACP-03 — Share execution-authority resolution with normal startup (P1)

**Defect:** `startup.rs::resolve_mode` and `extras/acp/mod.rs::resolve_acp_mode` are separate. ACP omits
CLI `read_only` and `guarded`, and sandbox/no-tools/bypass precedence can drift.

**Primary files/symbols:**

- `src/startup.rs::{resolve_mode,build_permission_checker}`
- `src/extras/acp/mod.rs::resolve_acp_mode`
- `src/permission/mod.rs::build_noninteractive_permission`
- `src/cli.rs` mode/sandbox resolver methods
- startup, checker, and ACP tests

**TDD sequence:**

1. Write a table covering CLI/config precedence for yolo, accept-all, read-only, guarded,
   restrictive, default mode, no-tools, skip-permissions, and requested/unavailable sandbox.
2. Extract one pure authority resolver used by both startup and ACP.
3. Keep frontend approval behavior explicit: interactive mode may provide `AskSender`; ACP has no secure
   approval channel and therefore remains fail-closed on `Ask`.
4. Make explicitly requested unavailable containment fail identically before model/tool execution.
5. Delete the duplicate ACP resolver and its redundant tests after shared tests cover both callers.

**Constraints:** do not auto-approve ACP `Ask`; do not combine workspace/history/cancellation changes;
do not turn this into a general startup rewrite.

**Verification:** shared matrix tests, ACP tests, `cargo test`, debug install.

### ACP-04 — Implement race-safe `session/cancel` through runner abort (P1)

**Defect:** the ACP protocol crate defines `CancelNotification` for `session/cancel`, but `serve`
registers no notification handler. `run_prompt` creates an `AgentRunner` and discards its
`abort_handle`; client cancellation cannot stop provider/tool work.

**Primary files/symbols:**

- `src/extras/acp/mod.rs::{serve,AcpState,handle_prompt,run_prompt}`
- `src/agent/runner.rs::AgentRunner`
- `src/tests/acp_tests.rs`

**TDD sequence:**

1. Add a blocking fake provider/tool ACP test and send `session/cancel` while it is active.
2. Register `on_receive_notification` for `CancelNotification`.
3. Store a generation-tagged in-flight abort handle per session before work can become uncancellable.
4. On cancel, atomically mark the matching generation cancelled, abort it, remove lifecycle state,
   return the protocol's cancelled stop reason, and do not commit an assistant response.
5. Test cancel-before-start, cancel-during-tool, duplicate cancel, stale cancel after completion, and a
   new prompt after cancellation. A stale notification must never abort a newer generation.

**Constraints:** cancellation must reach tool/model work, not merely drop the responder; no leaked task;
no history corruption; preserve fail-closed permission behavior.

**Verification:** ACP cancellation integration tests, all-feature tests, debug install.

### PERM-01 — Permission-gate explicit and project-wide LSP diagnostics (P1)

**Defect:** `LspTool` contains only `LspManager`. A path query synchronizes/reads a file and a no-path
query exposes project-wide diagnostics without `PermCheck` or `AskSender`.

**Primary files/symbols:**

- `src/agent/tools/lsp.rs::{LspTool,LspTool::new,Tool::call}`
- `src/agent/builder.rs` LSP registration
- `src/extras/lsp/mod.rs` root/diagnostic path accessors
- LSP/checker tests

**TDD sequence:**

1. Add allow/deny/ask tests for an explicit in-root path, an external path, and no-path project scope.
2. Inject permission and approval dependencies at construction.
3. Canonicalize an explicit file before checking the existing read/path policy and before any sync or
   server launch.
4. Authorize project-wide scope before returning diagnostics and ensure denied file entries cannot be
   leaked through the aggregate form. Prefer filtering/explicit scope authorization over bypassing
   per-file rules.
5. Keep operational LSP failures fail-open only after authorization succeeds.

**Constraints:** do not duplicate `mini-agent-7r1a.4` LSP subprocess lifecycle work; no server launch,
disk sync, or cached diagnostic disclosure before authorization.

**Verification:** LSP permission tests with `--features lsp`, checker tests, all-feature tests.

### PERM-02 — Validate permission objects and regexes before startup (P1)

**Defect:** `Config::build_permission_config` silently turns deserialization errors into defaults.
`Pattern` lazily compiles regexes and replaces invalid expressions with match-all at first use, which
is not safe for allow/ask actions and makes startup appear valid.

**Primary files/symbols:**

- `src/config/mod.rs::Config::build_permission_config`
- config resolution/loading callers in `src/config/load.rs` and `src/startup.rs`
- `src/permission/pattern.rs::Pattern`
- `src/permission/checker.rs::{compile_config,PermissionChecker::new}`
- config/checker tests

**TDD sequence:**

1. Add malformed structural permission JSON and malformed regex cases for each supported rule source,
   including granular tool and external-directory entries.
2. Make permission config construction return a contextual error naming config field, tool, and
   pattern. Do not silently default malformed configured values.
3. Compile configured regexes eagerly during startup/policy resolution; store the compiled regex in
   `Pattern`.
4. Keep valid glob expansion and default built-in rules behavior compatible.
5. Assert every execution mode, including ACP/headless, fails before agent construction on invalid
   policy.

**Constraints:** do not change standard mode's documented unmatched default; do not retain match-all
fallback for user-configured invalid expressions.

**Verification:** config and checker tests, `cargo test`, debug install with invalid-config smoke.

### PERM-03 — Scope repeated-read policy and tracking to one agent/session (P2)

**Defect:** `DENY_REPEATED_READS` and `READ_TRACKER` in `src/agent/tools/mod.rs` are process-global.
Concurrent agents and tests share policy and read history.

**Primary files/symbols:**

- `src/agent/tools/mod.rs::{DENY_REPEATED_READS,READ_TRACKER,track_read,untrack_read_path}`
- `src/agent/tools/read.rs::ReadTool`
- edit/write invalidation call sites
- `src/agent/builder.rs` tool construction
- `src/startup.rs` settings application
- `src/tests/tools_mod_tests.rs`

**TDD sequence:**

1. Add two concurrent `ReadTool`/agent fixtures with different settings and the same path.
2. Introduce a small owned `ReadTracker` shared only by tools in one agent/session.
3. Put the deny flag and tracked ranges on that object; inject it into read/edit/write tools.
4. Delete global setters/state and rewrite tests to avoid serial global cleanup.
5. Assert an edit invalidates only the owning session's tracked path.

**Constraints:** do not create a broad globals epic; edit-system/TODO/advisor/subagent globals remain
outside this task absent a source-proven production defect.

**Verification:** concurrent tracker tests, tool tests, `cargo test`.

### REL-01 — Restore tag-derived release identity and reject mismatches (P1)

**Defect:** `just release` and publishing docs say a pushed `v*` tag triggers release, but
`.github/workflows/release.yml` is manual-only. Manual branch dispatch feeds branch names into
`gh release create` without proving a matching Cargo version.

**Primary files/symbols:**

- `.github/workflows/release.yml`
- `justfile::{add-tag,release}`
- `docs/agent/PUBLISHING_RELEASES.md`
- `scripts/check-package-metadata.py` and tests

**TDD sequence:**

1. Add static workflow tests for the trigger and release-ref contract.
2. Restore `push.tags: ["v*"]`; remove unconstrained manual dispatch, or require an explicit validated
   tag input if manual recovery is retained.
3. Before builds, assert `GITHUB_REF_TYPE=tag`, parse `vX.Y.Z[-pre]`, and require it to equal
   `Cargo.toml` package version.
4. Keep prerelease detection and final publication ordering.
5. Rehearse one mismatch (must publish nothing) and one valid test tag.

**Constraints:** no branch-derived public release; no partial release; preserve full artifact matrix.

**Verification:** workflow YAML/static tests, package metadata check, nonpublic Actions rehearsal.

### REL-02 — Pin release artifact actions to immutable commits (P2)

**Defect:** every `upload-artifact`/`download-artifact` use in the release workflow references mutable
`@v4`, unlike most other actions and the CI workflow.

**Primary files:** `.github/workflows/release.yml` and workflow policy/static tests.

**TDD sequence:**

1. Add a static check that every `uses:` in release automation is a full commit SHA with a version
   comment unless explicitly allowlisted.
2. Pin all upload/download sites to reviewed immutable v4 commits.
3. Ensure Dependabot/Renovate can update commit and version comment together.

**Constraints:** no workflow behavior change; do not replace a mutable tag with an unverified SHA.

**Verification:** static workflow test and Actions artifact rehearsal.

### REL-03 — Establish canonical repository/product coordinates without breaking data compatibility (P1)

**Defect:** the actual repository is `sebahrens/mini-agent`, but installer, package URLs, docs,
OpenRouter identity, and welcome links target `sebahrens/mini-agent`. The public binary is
`mini-agent`, while `zerostack` is also used as a persisted compatibility namespace and package brand.
Unclassified global replacement would be destructive; leaving wrong network coordinates is broken.

**Primary files:**

- `install.sh`
- `justfile`
- `packaging/aur/{PKGBUILD,.SRCINFO}`
- `packaging/homebrew/zerostack.rb`
- `packaging/conda/zerostack*/meta.yaml`
- `scripts/{sync-version.sh,check-package-metadata.py}` and script tests
- `docs/agent/{GET_STARTED.md,PUBLISHING_RELEASES.md}`
- `src/provider.rs::build_openrouter_client`
- `src/ui/events.rs::show_welcome`
- `src/extras/{acp/mod.rs,lsp/client.rs}` protocol implementation/client identity

**TDD sequence:**

1. Write and test an identity matrix: public crate/binary `mini-agent`; repository
   `sebahrens/mini-agent`; release assets `mini-agent-*`; legacy persisted `.zerostack`, app-data, and
   `ZEROSTACK_*` names remain compatibility API until a separate migration is approved.
2. Change every network/download/homepage coordinate to the canonical repository.
3. Make model/provider/protocol client identity report the chosen public product identity consistently;
   preserve an alias only when a compatibility test demonstrates it is required.
4. Keep package-channel names only where registry compatibility requires them; document the binary
   installed by each.
5. Add a static check that rejects legacy repository coordinates in active source, scripts, packaging, and user
   docs while allowlisting explicitly historical superseded specs.
6. Smoke the installer against a test release from the canonical repository.

**Constraints:** no blind rename of data directories, project `.zerostack`, environment variables,
logs, or stored identities; no release before canonical assets exist.

**Verification:** package metadata/script tests, installer checksum/archive smoke, docs link check,
`cargo test`, debug install.

### REL-04 — Remove the unproved Nix support surface (P2)

**Decision:** carve Nix support out now. There is no Nix CI gate or current root install path, the
expressions are impure and stale, `release.nix` exports old names, package metadata expects a missing
Cargo homepage, and `postInstall` uses invalid quote characters. Repairing this honestly would require
a pinned flake plus Linux/macOS package builds and exact-output smoke, which is a separate product
commitment rather than a cleanup.

**Primary files:**

- `default.nix`, `release.nix`, `shell.nix`
- `nix/overlay/*.nix`, `nix/package/*.nix`
- `scripts/check-package-metadata.py` and tests
- `docs/agent/PUBLISHING_RELEASES.md`
- other active user-facing Nix support claims found by scoped search

**TDD/deletion sequence:**

1. Add/adjust metadata tests so supported channels are explicit rather than inferred from file
   presence.
2. Remove the Nix expressions and overlay/package/dev-shell files.
3. Remove Nix from release-channel claims and metadata validation.
4. Keep runtime references to `/nix` where they are platform sandbox compatibility, not packaging
   support.
5. Document that future Nix restoration requires pinned inputs, CI on claimed platforms, default
   feature parity, and smoke of the exact store output.

**Constraints:** do not remove `/nix` sandbox runtime roots; do not disturb Cargo packaging.

**Verification:** stale file/reference search, metadata script tests, `cargo test`, debug install.

### SIM-01 — Isolate `spike/` from the production Cargo workspace (P2)

**Defect:** `spike/` is explicitly “research artifact, not production,” yet root workspace membership
causes root metadata, lockfile, lint, loop verification, and workspace tests to include it.

**Primary files:**

- root `Cargo.toml [workspace]`
- `spike/Cargo.toml`
- `scripts/loop.sh` workspace crate selection
- repository contributor guidance that claims workspace membership

**TDD sequence:**

1. Record root and spike `cargo metadata --locked --no-deps` package sets.
2. Exclude `spike` from the root workspace and make it an explicit standalone workspace/package so it
   remains runnable for research.
3. Remove production loop/CI logic that conditionally validates spike as a shipped crate.
4. Preserve source and research instructions; do not delete the spike.
5. Assert root metadata contains only `mini-agent` and spike-local metadata contains only `spike`.

**Constraints:** no production imports from spike; no dependency consolidation; no spike behavior work.

**Verification:** both metadata commands, root `cargo test`, spike-local test/run if documented, debug
install from root.

### SIM-02 — Delete the unused generic `with_retry` helper (P2)

**Defect:** `src/retry.rs::with_retry` is referenced only by tests written for itself. Production uses
the streaming/runner-specific retry paths.

**Primary file:** `src/retry.rs`.

**Deletion sequence:**

1. Re-run exact reference search for `with_retry` immediately before editing.
2. Delete the helper and its helper-only tests.
3. Keep `RetryConfig`, `simple_jitter`, `is_retryable`, and `retry_stream_chat` unchanged.
4. Do not consolidate or redesign remaining retry loops in this task.

**Verification:** `rg '\bwith_retry\b' src`, retry tests, `cargo test`, `cargo fmt`.

## 8. Beads structure and dependencies

The final all-status duplicate search covered 274 pre-existing Beads. The review created these five
epics, fifteen children, and one new child under the existing Phase 6 epic:

1. **`mini-agent-1hq5` — Interactive turn and accounting correctness**
   - `mini-agent-1hq5.1` — INT-01 usage deltas (P1)
   - `mini-agent-1hq5.2` — INT-02 positional initial turn (P2)
2. **`mini-agent-2gsc` — ACP session correctness and authority**
   - `mini-agent-2gsc.1` — ACP-01 history (P1)
   - `mini-agent-2gsc.2` — ACP-02 workspace binding (P0)
   - `mini-agent-2gsc.3` — ACP-03 shared authority resolution (P1)
   - `mini-agent-2gsc.4` — ACP-04 cancellation (P1)
3. **`mini-agent-nivz` — Permission boundary consistency**
   - `mini-agent-nivz.1` — PERM-01 LSP authorization (P1)
   - `mini-agent-nivz.2` — PERM-02 eager config/regex validation (P1)
   - `mini-agent-nivz.3` — PERM-03 session-scoped repeated reads (P2)
4. **`mini-agent-jj7m` — Release and distribution reliability**
   - `mini-agent-jj7m.1` — REL-01 trigger/ref identity (P1)
   - `mini-agent-jj7m.2` — REL-02 immutable action pins (P2)
   - `mini-agent-jj7m.3` — REL-03 canonical coordinates (P1)
   - `mini-agent-jj7m.4` — REL-04 remove Nix support claim (P2)
5. **`mini-agent-1gr9` — Production workspace simplification**
   - `mini-agent-1gr9.1` — SIM-01 isolate spike (P2)
   - `mini-agent-1gr9.2` — SIM-02 remove dead retry helper (P2)
6. **Existing Phase 6 epic `mini-agent-xic0`**
   - `mini-agent-xic0.34` — PH6-34 reusable persisted exports (P0), depending on A18/A20/A21/A23
     and blocking A31/A33.

The dependency graph serializes tasks that own the same high-conflict files:

- ACP workspace → shared authority → history → cancellation;
- permission validation → LSP authorization → repeated-read state;
- release trigger → release action pins; and
- canonical coordinates → Nix removal.

Independent interactive and simplification children may proceed in parallel with file reservations.
`mini-agent-xic0.31` and `.33` explicitly depend on `.34`, so aggregate CI and final documentation
cannot declare Phase 6 complete before the new regression is fixed.

Every Bead description must include the defect/impact, exact files and symbols, required behavior,
constraints/non-goals, acceptance criteria, verification commands, parent, dependencies, and the
duplicate-search result.

## 9. Compatibility and migration risks

- **Usage totals:** existing persisted totals may already be inflated. Do not silently rewrite them
  without a trustworthy event ledger. Fix future charging and document that historical totals are not
  automatically repairable.
- **ACP workspace:** rejecting invalid roots or no longer inheriting server CWD is an intentional
  fail-closed behavior change. Clients must send a valid directory.
- **ACP history:** the current unmatched/partial history may exist only in process memory. Do not infer
  structured tool calls from flattened strings.
- **Permission validation:** configurations that previously “worked” by being ignored will now fail
  startup. Error messages must make repair direct.
- **Product identity:** `.zerostack` data roots and environment contracts remain stable. Repository URL
  correction must not become an accidental data migration.
- **Nix:** removal is a support-policy correction. Announce it in release docs; restoring Nix is a new
  tracked feature with reproducibility/CI gates.
- **Spike isolation:** the research crate keeps its source and can be run explicitly, but root workspace
  commands no longer validate it.

## 10. Findings deliberately not filed

The review does not recommend Beads for:

- asynchronous TUI git-status refresh without measured latency evidence;
- a broad removal of every process-global setting;
- learned-skills/Phase 6 architecture removal;
- optional Cargo features merely because they are not default-enabled;
- `status-signals` or `visibility.rs` as dead code;
- Narsil's blanket Rust unused-export or circular-import reports;
- generic orchestration consolidation;
- session-prefix optimization without scale evidence;
- unproved prompt/context budgeting or resume-role claims;
- a standalone archive extraction epic without a concrete exploit path; or
- defects already owned by `mini-agent-4gom`, `mini-agent-7r1a.*`, `mini-agent-8tbo`,
  `mini-agent-9g1i`, `mini-agent-9zt0`, `mini-agent-r2cu`, or existing `mini-agent-xic0.*` tasks.

Recent validation, transcript chronology, EOF, feature-matrix, and cancellation-scope fixes were also
treated as completed rather than refiled.

## 11. Completion gates

Each implementation Bead uses the narrow focused tests specified above, then the repository gates
appropriate to its blast radius:

```bash
cargo fmt
cargo test
cargo install --path . --debug
```

Feature-specific changes also run their feature/all-feature rows. Release changes require static
workflow/package checks and a nonpublic rehearsal. Interactive changes require a tmux smoke. Phase 6
changes require the cross-platform containment/CI gates already owned by A24–A31.

No epic closes merely because it was decomposed. It closes only when every child is implemented,
verified, and its compatibility/documentation obligations are satisfied.
