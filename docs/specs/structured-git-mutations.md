# ADR: Structured Git mutations

- **Document role**: normative cross-cutting decision
- **Specification version**: 1.1.0
- **Delivery status**: delivered
- **Last reconciled**: 2026-11-21
- **Accepted**: 2026-08-14

## Decision

The model-visible Git surface is the closed `GitOperation` set below. Read-only:
`status`, `diff`, `log`, `show`, `blame`, `branches`, `stash_list`. Mutating: `stage`,
`unstage`, `commit` (optionally `amend`), `branch_create`, `switch`, `stash_push`,
`stash_pop`, `tag`, `restore`. Each operation is a typed request against the captured `WorkspaceBinding`; raw
argv, shell text, remotes, and network operations are not part of the API.

Permission keys: `git/status` (`status`, `branches`, `stash_list`), `git/diff`, `git/log`,
`git/show` (`show`, `blame`), `git/stage`, `git/unstage`, `git/commit`, `git/amend` (a second
check when `commit` sets `amend`), `git/branch` (`branch_create`), `git/switch`, `git/stash`
(`stash_push`, `stash_pop`), `git/tag`, `git/restore`.

Mutation calls require the exact permission verb for the operation and return before/after porcelain-v2 state,
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
No fetch, pull, push, reset-hard, clean, merge, or rebase operation is exposed.
`switch` and `restore` do rewrite the working tree, which can run checkout
conversion (smudge/process filters, eol and `ident` attributes); they run with
filters, hooks and helpers disabled and refuse paths or attributes that need an
external transform, exactly as staging does. `stash_push` and `stash_pop` create
commits and therefore need a committer identity; the identity is resolved by
the host from the captured global Git configuration (never model-supplied,
and `GIT_AUTHOR_*`/`GIT_COMMITTER_*` overrides from the parent environment are
removed), and the operation fails when none is configured. `amend` only
replaces a HEAD commit that is in no remote-tracking ref. Cancellation and output/timeout limits terminate the child and
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
truthful partial staging. Every `GitOperation::name()` must appear in the operation list
above (enforced by the `adr_lists_every_git_operation` test in `src/git/tool.rs`), and the
working-tree-rewriting operations (`switch`, `restore`, `stash_pop`) need rows for checkout
conversion refusal and stash identity. A platform where the executable or workspace cannot
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
