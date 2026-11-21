# ADR: Structured Git mutations

- **Document role**: normative cross-cutting decision
- **Specification version**: 1.2.0
- **Delivery status**: delivered
- **Last reconciled**: 2026-11-21 (history, LFS and checkpoint amendments)
- **Accepted**: 2026-08-14

## Decision

The model-visible Git surface is the closed `GitOperation` set below. Read-only:
`status`, `diff`, `log`, `show`, `blame`, `branches`, `stash_list`. Mutating: `stage`,
`unstage`, `commit` (optionally `amend`), `branch_create`, `switch`, `stash_push`,
`stash_pop`, `tag`, `restore`, and the local history operations `merge`, `merge_abort`,
`cherry_pick`, `cherry_pick_abort`, `revert`, `revert_abort`, `reset` (`soft` or `mixed` only),
`branch_delete`, `branch_rename`, `stash_apply`, `stash_drop` (plus `include_untracked` on
`stash_push`). Each operation is a typed request against the captured `WorkspaceBinding`; raw
argv, shell text, remotes, and network operations are not part of the API.

Permission keys: `git/status` (`status`, `branches`, `stash_list`), `git/diff`, `git/log`,
`git/show` (`show`, `blame`), `git/stage`, `git/unstage`, `git/commit`, `git/amend` (a second
check when `commit` sets `amend`), `git/branch` (`branch_create`), `git/switch`, `git/stash`
(`stash_push`, `stash_pop`, `stash_apply`, `stash_drop`), `git/tag`, `git/restore`, `git/merge`
(`merge`, `merge_abort`), `git/cherry_pick` (with its abort), `git/revert` (with its abort),
`git/reset`, `git/branch_delete` (`branch_delete`, `branch_rename`).

### Local history operations (mini-agent-nvnkx.50)

They run through the same hardened runner (hooks, filters, signing, editors and credential
helpers disabled; the per-repository mutation lock; before/after status snapshots) and take
their own permission key. `merge`, `cherry_pick` and `revert` use `--no-edit`, need a
revision (single commit for the latter two), create a commit with the host-resolved identity
and refuse to start while another operation is in progress. A conflict is reported, not hidden:
the result carries `conflicted_paths` and `in_progress`, and the matching `*_abort` operation
restores the pre-operation state. `reset` accepts `soft` and `mixed` only (never `--hard`:
the working tree is not touched) and refuses when HEAD is already contained in a
remote-tracking ref, like `amend`. `branch_delete` uses `branch -d` and `branch_rename` renames;
both refuse the checked-out branch. `stash_apply` and `stash_drop` address `stash@{index}`.
`rebase`, `fetch`, `pull`, `push` and `reset --hard` remain unavailable.

### In-progress state (mini-agent-nvnkx.316)

`status` adds `in_progress` (`merge`, `rebase`, `cherry-pick`, `revert` or `bisect`) and a note,
found by existence checks of the Git state files (`rev-parse --git-path`; no content is read).
`commit` refuses while a rebase or cherry-pick is in progress unless `allow_in_progress` is set.
The tool cannot continue or abort a rebase or bisect.

### Git LFS repositories (mini-agent-nvnkx.317)

The filter guard keeps refusing workspace-defined filter drivers. The stock `lfs` driver (only
its canonical `git-lfs` commands, or emptied values) is classified separately: every driver is
neutralised for the call, so nothing executes, and `status`, `diff`, `blame`, `unstage` and
`commit` proceed with `lfs_pointers_not_resolved`, `lfs_paths` and a notice in the result
(LFS-tracked paths compare as raw content against pointer files). `stage` and `commit` by path
refuse LFS-tracked paths with a message naming the shell, and `switch`, `restore`, stash and
the merge-family operations still refuse in a repository with LFS-bound paths because a
neutralised smudge filter would write pointer text over real files.

### File checkpoints (mini-agent-nvnkx.49)

`restore` reads each tracked file it may rewrite before the command and records the
preimage/postimage through the file-checkpoint recorder, so `/rewind` can bring back
uncommitted content. Operations that rewrite the working tree without a per-file list
(`switch`, stashes, `merge`, `cherry_pick`, `revert` and their aborts) register an uncaptured
mutation when the tree changed, and the rewind picker flags ranges that ran shell or git
commands.

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
No fetch, pull, push, reset-hard, clean, or rebase operation is exposed.
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
