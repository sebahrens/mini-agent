# ADR: Structured Git mutations

Status: accepted (2026-08-14)

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
