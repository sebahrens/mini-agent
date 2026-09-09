"""Bounded Git worktree administration shared by Gym training and mining."""

from __future__ import annotations

import os
import shutil
import stat
import subprocess
from pathlib import Path

if __package__:
    from .process_capture import run_bounded
else:
    from process_capture import run_bounded

SETUP_TIMEOUT_SECS = 300
CLEANUP_TIMEOUT_SECS = 30


class WorktreeError(OSError):
    """Administrative execution failed; this is not an oracle verdict."""


def run_worktree(repo: Path, operation: str, *arguments: str) -> subprocess.CompletedProcess[bytes]:
    timeout = SETUP_TIMEOUT_SECS if operation == "add" else CLEANUP_TIMEOUT_SECS
    try:
        return run_bounded(["git", "worktree", operation, *arguments], repo, dict(os.environ), timeout)
    except subprocess.TimeoutExpired as error:
        detail = (error.stderr or b"").decode("utf-8", errors="replace").strip()
        raise WorktreeError(f"git worktree {operation} timed out after {timeout}s: {detail}") from error
    except OSError as error:
        raise WorktreeError(f"git worktree {operation} could not complete: {error}") from error


def remove_tree(path: Path) -> None:
    """Remove an owned entry without following root links; report incomplete removal."""
    try:
        if stat.S_ISDIR(path.lstat().st_mode):
            shutil.rmtree(path)
        else:
            path.unlink()
    except FileNotFoundError:
        pass
    if os.path.lexists(path):
        raise OSError(f"directory entry remains after removal: {path}")


def remove_workspace(repo: Path, workspace: Path) -> None:
    """Remove filesystem entries without following links, then clean registration."""
    try:
        # Canonicalize the parent only. Resolving a replaced root itself
        # would redirect cleanup to its symlink target.
        workspace = workspace.parent.resolve() / workspace.name
        remove_tree(workspace)
    except OSError as error:
        # Do not prune registration while filesystem cleanup is incomplete.
        raise WorktreeError(f"workspace filesystem cleanup failed: {error}") from error

    errors: list[WorktreeError] = []
    try:
        # Git canonicalizes its argument. Only give it the missing root,
        # after any symlink has been unlinked. A checkout hook may have locked
        # the Gym-owned registration, hence the repeated --force.
        run_worktree(repo, "remove", "--force", "--force", str(workspace))
    except WorktreeError as error:
        errors.append(error)
    finally:
        try:
            pruned = run_worktree(repo, "prune")
            if pruned.returncode:
                detail = pruned.stderr.decode("utf-8", errors="replace").strip()
                raise WorktreeError(f"git worktree prune exited {pruned.returncode}: {detail}")
        except WorktreeError as error:
            errors.append(error)
    if errors:
        raise WorktreeError("; ".join(str(error) for error in errors)) from errors[0]
