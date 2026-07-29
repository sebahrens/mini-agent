# Loop-Enforced Feature-Specific Real-Binary Verification

**Date:** 2026-07-29  
**Status:** Approved design  
**Scope:** Require the build agent launched by `scripts/loop.sh` to test-drive each implemented feature through the installed application before the loop accepts the bead.

## Problem

The current bead-wide binary gate installs `mini-agent` and runs `mini-agent -p "say hello in one word"`. That command proves only that a basic OpenRouter request succeeds. It does not exercise the behavior implemented by most beads, so it can report success while the delivered CLI, TUI, permission, sandbox, persistence, packaging, or agent flow is broken.

Automated Rust checks remain necessary, but they are not sufficient evidence that users can reach and use the implemented feature through the production binary.

## Decision

The existing implementation agent will perform feature-specific real-binary verification before closing its selected bead. `loop.sh` will bind that evidence to the current iteration with an unpredictable token and will not accept or auto-close the bead without a matching passing evidence comment.

A separate test-driver agent is intentionally not added. Keeping implementation and feature test-driving in one invocation avoids doubling model calls while still requiring the agent that understands the change to exercise it end to end.

## Build-Agent Workflow

After implementation and before closing the bead, the build agent must:

1. Read the bead outcome and acceptance criteria and inspect its own change.
2. Define a concrete scenario that reaches the changed behavior through a supported public interface.
3. Run `cargo install --path . --debug` from the repository root.
4. Invoke the installed `mini-agent` resolved from `PATH`; `cargo run`, direct `target/debug/mini-agent` execution, and unit tests are not substitutes.
5. Use headless mode for noninteractive behavior, tmux for TUI behavior, and the exact produced artifact for packaging or release behavior.
6. Compare an observable result with an explicit expected result. Merely starting the process or receiving any model response is not sufficient.
7. Add a structured bead comment containing the iteration's evidence token and exact evidence.
8. Close the bead only when the feature-specific scenario passes.

The generic one-word prompt is valid only when the bead itself changes basic provider connectivity. Other beads must use a prompt, command sequence, fixture, configuration, or interaction that forces the changed feature to execute.

## Evidence Contract

`loop.sh` injects a unique token into the build prompt. The agent records one comment in this shape:

```text
[REAL-BINARY EVIDENCE]
Token: <injected token>
Scenario: <feature behavior exercised>
Interface: <headless | tmux-tui | packaged-artifact>
Commands: <exact commands and interaction>
Expected: <observable success condition>
Observed: <bounded output or state proving the condition>
Result: PASS
```

The evidence must not contain credentials, prompts with secrets, private file contents, or unbounded command output.

If the scenario cannot run because credentials, platform support, an external dependency, or a production integration path is unavailable, the agent records `Result: BLOCKED` with the reason and leaves the bead open. It must not replace the scenario with the generic hello prompt or claim that unit tests are equivalent.

## Loop Enforcement

For each build iteration, `loop.sh` will:

1. Generate a fresh evidence token before invoking the agent and inject it with the selected bead ID.
2. After the agent returns, query that bead's comments for the exact token and `Result: PASS` within the same structured evidence block.
3. Run the existing formatter, compiler/linter, and Rust test verification independently.
4. Reopen an agent-closed bead if either automated verification or current-iteration real-binary evidence failed.
5. Auto-close an open bead with verified code changes only when both gates passed.
6. Print the missing, blocked, or failed evidence state in the iteration summary.

Old comments and evidence from another iteration cannot satisfy the gate because their tokens differ. A bead cannot self-exempt by claiming that no binary path exists; that condition leaves the bead open for decomposition, integration work, or human review.

## Bead Instructions

The universal note on every open bead will be replaced rather than appended again. The replacement will require a feature-specific scenario and the structured evidence contract, while preserving any unrelated existing notes.

Bead-specific verification remains authoritative about what behavior to exercise. The universal note defines how the loop agent must convert those acceptance criteria into installed-application evidence.

## Failure Handling

- **Install failure:** evidence is not passing; existing automated verification may file its normal build-error bead.
- **Application failure or wrong result:** record bounded observed output as `Result: FAIL`; leave or reopen the bead.
- **Missing credentials/environment:** record `Result: BLOCKED`; leave the bead open.
- **TUI feature without a terminal:** use the repository's tmux procedure. Absence of tmux is blocked, not passed.
- **Packaging/release feature:** execute the binary from the produced package or extracted archive; the workspace-installed binary cannot satisfy the scenario.
- **No externally reachable behavior:** leave the bead open. Split or add the missing production wiring rather than declaring an internal-only implementation complete.

## Verification of the Enforcement Change

Add shell-level coverage or a sourceable helper test for evidence parsing and bead-state decisions. At minimum, cover:

- matching token plus `Result: PASS`;
- stale token;
- missing comment;
- `FAIL` and `BLOCKED` results;
- misleading text containing `PASS` outside the structured block;
- agent-closed bead reopened when evidence is absent;
- auto-close allowed only when automated checks, code-change detection, and evidence all pass.

Also run `bash -n scripts/loop.sh` and verify all open beads contain the replacement feature-specific note exactly once with no remaining mandatory one-word smoke note.

## Acceptance Criteria

- The build prompt requires installed, feature-specific application test-driving and structured evidence before closure.
- Every build iteration receives a unique evidence token that stale comments cannot satisfy.
- `loop.sh` requires both automated checks and matching `PASS` evidence before accepting or auto-closing a bead.
- Failure, blockage, missing evidence, or lack of a production path leaves the bead open.
- The generic hello prompt is described only as a connectivity-specific example, never as a universal acceptance test.
- Existing bead notes unrelated to the old universal gate are preserved.
