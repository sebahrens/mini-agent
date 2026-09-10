# ADR: Structured Git mutations

- **Document role**: normative cross-cutting decision
- **Specification version**: 1.0.1
- **Delivery status**: delivered
- **Last reconciled**: 2026-09-06
- **Accepted**: 2026-08-14

## Decision

The model-visible Git surface permits only `stage`, `unstage`, and `commit`.
Each operation is a typed request against the captured `WorkspaceBinding`; raw
argv, shell text, remotes, and network operations are not part of the API.
`status`, `diff`, `log`, and `show` remain read-only operations.

Mutation calls require the exact permission verb (`git/stage`,
`git/unstage`, or `git/commit`) and return before/after porcelain-v2 state,
exit status, bounded output, and the requested operation. A failed command is
never described as rolled back. A process-wide admission lock serializes
structured mutations with internal worktree mutations; Git's own index lock is
still authoritative and lock contention fails closed.

Paths are non-empty, repository-relative, literal components (no options,
absolute paths, traversal, globs, or symlinks), and are passed after `--`.
Commit messages are bounded UTF-8 values and are passed through stdin with
`--file=-`, avoiding command-line disclosure and argument-size limits.
The runner pins the discovered Git executable, uses `-C` with the captured
workspace, removes repository-redirection environment variables, disables
optional locks, hooks, external diff/textconv, credential helpers, signing,
submodule recursion, and protocol-based network/file helpers. Filters and
working-tree encodings are inspected before staging; paths with external
transforms are rejected.

The typed tool, mutation helpers, and contained execution helpers compile with `git-worktree`;
portable Git regression tests retain those helpers in test builds. Executable discovery
and bounded read-only queries remain available in every build for headless change
reporting and session status. The registration-time containment probe and internal
worktree stdin/network entry points follow their `git-worktree` callers.

## Threat model

Git can execute hooks, clean/smudge/process filters, signing programs,
credential helpers, editors, pagers, external diff/textconv, submodule
commands, and protocol helpers. The contract disables or rejects each surface.
No fetch, pull, push, checkout, reset-hard, clean, merge, or rebase operation
is exposed. Cancellation and output/timeout limits terminate the child and
preserve the truthful post-operation snapshot.

## Alternatives rejected

Keeping Git permanently read-only is safe but does not support agents recording
their own changes. Reusing the shell defeats typed permissions and Windows
parity. A new in-process Git library adds a large dependency and would still
need equivalent attribute, index-lock, and linked-worktree semantics. The
hardened direct executable boundary is selected for this narrow local subset.

## Verification matrix

Tests must cover Unix and Windows, linked worktrees, Unicode and option-like
paths, symlinks and submodules, hostile hooks/attributes/signing/editor config,
index lock contention, concurrent callers, cancellation, bounded output, and
truthful partial staging. A platform where the executable or workspace cannot
be verified is rejected before launch.

The Unix worktree concurrency tests hold real Git aliases and pre-commit hooks
behind owned FIFO release gates. Independent repositories must both reach their
held commands; same-root and linked-worktree cases share one exclusion matrix.
A caller-scoped test observer records the first poll of the actual mutation-lock
future after common-directory resolution, so exclusion requires a pending second
admission rather than a duration threshold. The hook controller must progress on
the same single-threaded runtime while the hook remains held, preserving process
cwd and relative file reads. Controller-assertion and caller-panic cases release all
commands and join their tasks and rescue threads before propagating the failure.
Emergency rescue is independently driven and makes the test fail; it is not an acceptance deadline.
