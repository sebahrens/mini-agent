from __future__ import annotations

import contextlib
import io
import json
import os
import shlex
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
import tomllib
import tracemalloc
import unittest
from pathlib import Path
from unittest import mock

from scripts.gym import mine_tasks as MINE
from scripts.gym import train as TRAIN_MODULE
from scripts.gym import worktrees as WORKTREES


ROOT = Path(__file__).resolve().parents[2]
TRAIN = ROOT / "scripts/gym/train.py"
SETUP = ROOT / "scripts/gym/setup.sh"
TEMP_ROOTS = ("/tmp", "/private/tmp")


def scratch_outside_tmp() -> tempfile.TemporaryDirectory:
    """A scratch tree the gym host guard accepts.

    `setup.sh` refuses any workspace whose *resolved* path is under the system
    temp root, and on Linux `tempfile`'s default parent is exactly that root, so
    a full setup run has to be staged elsewhere. The checkout is the one
    directory guaranteed to exist and be writable here.
    """
    return tempfile.TemporaryDirectory(prefix=".gym-setup-", dir=ROOT)


DB_HELPER = """
import pathlib
import sqlite3
import sys

mode = sys.argv[1]
root = pathlib.Path(sys.argv[2]) / "skills"
root.mkdir(parents=True, exist_ok=True)
db = sqlite3.connect(root / "skills.db")
db.execute("CREATE TABLE IF NOT EXISTS skill_revisions (id TEXT PRIMARY KEY, status TEXT NOT NULL)")
db.execute(
    "CREATE TABLE IF NOT EXISTS skill_proposals ("
    "skill_id TEXT PRIMARY KEY, predecessor_id TEXT, status TEXT NOT NULL, proposed_at INTEGER NOT NULL)"
)
if mode == "seed":
    db.execute("INSERT OR REPLACE INTO skill_revisions VALUES ('skill-root', 'pending')")
    db.execute("INSERT OR REPLACE INTO skill_revisions VALUES ('skill-replacement', 'pending')")
    db.execute("INSERT OR REPLACE INTO skill_proposals VALUES ('skill-root', NULL, 'awaiting_approval', 1)")
    db.execute(
        "INSERT OR REPLACE INTO skill_proposals VALUES "
        "('skill-replacement', 'skill-root', 'awaiting_approval', 2)"
    )
elif mode == "activate":
    skill = sys.argv[3]
    if skill != "skill-root":
        raise SystemExit(f"refusing to activate non-root proposal {skill}")
    db.execute("UPDATE skill_revisions SET status = 'active' WHERE id = ?", (skill,))
db.commit()
db.close()
"""

STUB = """#!/bin/sh
set -u
mode={mode}
log="{log}"
helper="{helper}"
case "${{1:-}}" in
  --install-learned-skill-seeds|--import-learned-skill)
    if [ "$mode" = install_fail ]; then
      echo "seed import exploded" >&2
      exit 1
    fi
    python3 "$helper" seed "$ZS_LOCAL_DATA_DIR"
    exit 0
    ;;
  --approve-learned-skill)
    exit 0
    ;;
  --activate-learned-skill)
    python3 "$helper" activate "$ZS_LOCAL_DATA_DIR" "$2"
    exit $?
    ;;
esac
printf '%s\\n' "$*" >> "$log"
printf '%s\\t%s\\t%s\\n' "$PWD" "$TMPDIR" "$ZS_LOCAL_DATA_DIR" >> "$log.env"
case "$mode" in
  failure)
    echo "agent failed" >&2
    exit 3
    ;;
esac
printf 'fixed\\n' > fixed.txt
exit 0
"""


def git(repo: Path, *arguments: str) -> str:
    return subprocess.run(
        ["git", *arguments], cwd=repo, check=True, capture_output=True, text=True
    ).stdout


def make_repo(root: Path) -> Path:
    repo = root / "repo"
    repo.mkdir(parents=True)
    git(repo, "init", "-q", "-b", "main")
    git(repo, "config", "user.email", "gym@example.invalid")
    git(repo, "config", "user.name", "Gym Test")
    (repo / "value.txt").write_text("broken\n", encoding="utf-8")
    git(repo, "add", "value.txt")
    git(repo, "commit", "-qm", "broken")
    return repo


def make_stub(root: Path, mode: str) -> tuple[Path, Path]:
    helper = root / "stub_db.py"
    helper.write_text(DB_HELPER, encoding="utf-8")
    log = root / "agent-argv.log"
    binary = root / "fake-mini-agent"
    binary.write_text(STUB.format(mode=mode, log=log, helper=helper), encoding="utf-8")
    binary.chmod(0o755)
    return binary, log


def task_document(tasks: list[dict[str, object]]) -> dict[str, object]:
    return {
        "schema_version": 1,
        "defaults": {
            "prompt": "Create fixed.txt",
            "initial_files": {},
            "oracle": {"command": "test -f fixed.txt", "id": "fixed-file-v1"},
            "budgets": {"max_provider_turns": 3, "max_tool_calls": 5, "max_total_tokens": 900},
            "scripted_provider_turns": {"none": [], "library": []},
            "library": "seeds",
        },
        "tasks": tasks,
    }


def run_training(root: Path, repo: Path, binary: Path, document: dict[str, object], *extra: str, cwd: Path | None = None):
    tasks = root / "tasks.json"
    tasks.write_text(json.dumps(document), encoding="utf-8")
    output = root / "outcomes.jsonl"
    gym_root = root / "gym"
    completed = subprocess.run(
        [
            sys.executable,
            str(TRAIN),
            "--repo",
            str(repo),
            "--binary",
            str(binary),
            "--tasks",
            str(tasks),
            "--output",
            str(output),
            "--gym-root",
            str(gym_root),
            *extra,
        ],
        capture_output=True,
        text=True,
        cwd=cwd,
        env={**os.environ, "MINI_AGENT_GYM_ROOT": str(gym_root)},
    )
    rows = [json.loads(line) for line in output.read_text(encoding="utf-8").splitlines()] if output.is_file() else []
    return completed, rows, gym_root


class GymSubprocessTests(unittest.TestCase):
    def test_output_floods_retain_exact_tails_with_bounded_memory(self) -> None:
        limit = 2000
        for mode, exit_code in [("stdout", 0), ("stderr", 7), ("mixed", 0)]:
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                code = (
                    "import os,sys; mode=sys.argv[1]; "
                    "out=b'x'*65536; err=b'y'*65536\n"
                    "for _ in range(64):\n"
                    " if mode != 'stderr': os.write(1,out)\n"
                    " if mode != 'stdout': os.write(2,err)\n"
                    "if mode != 'stderr': os.write(1,b'\\xffstdout-end\\n')\n"
                    "if mode != 'stdout': os.write(2,b'\\xfestderr-end\\n')\n"
                    "sys.exit(int(sys.argv[2]))"
                )
                tracemalloc.start()
                try:
                    result = TRAIN_MODULE.run(
                        [sys.executable, "-c", code, mode, str(exit_code)],
                        Path(directory), dict(os.environ), 5,
                    )
                    peak = tracemalloc.get_traced_memory()[1]
                finally:
                    tracemalloc.stop()
                self.assertEqual(result.returncode, exit_code)
                self.assertLess(peak, 1024 * 1024, "output capture retained the flood")
                for actual, enabled, fill, marker in [
                    (result.stdout, mode != "stderr", b"x", b"\xffstdout-end\n"),
                    (result.stderr, mode != "stdout", b"y", b"\xfestderr-end\n"),
                ]:
                    self.assertEqual(len(actual), limit if enabled else 0)
                    self.assertEqual(actual, fill * (limit - len(marker)) + marker if enabled else b"")

    def test_timeout_retains_tails_and_reaps_child_with_open_or_closed_pipes(self) -> None:
        for close_pipes in [False, True]:
            with self.subTest(close_pipes=close_pipes), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                code = (
                    "import os,sys,time; from pathlib import Path; "
                    "Path('pid').write_text(str(os.getpid())); "
                    "os.write(1,b'x'*10000+b'OUT'); os.write(2,b'y'*10000+b'ERR')\n"
                    "if sys.argv[1]=='close': os.close(1); os.close(2)\n"
                    "time.sleep(30)"
                )
                started = time.monotonic()
                with self.assertRaises(subprocess.TimeoutExpired) as timed_out:
                    TRAIN_MODULE.run(
                        [sys.executable, "-c", code, "close" if close_pipes else "open"],
                        root, dict(os.environ), 1,
                    )
                self.assertLess(time.monotonic() - started, 3)
                self.assertEqual(timed_out.exception.timeout, 1)
                self.assertEqual(timed_out.exception.output, b"x" * 1997 + b"OUT")
                self.assertEqual(timed_out.exception.stderr, b"y" * 1997 + b"ERR")
                with self.assertRaises(ProcessLookupError):
                    os.kill(int((root / "pid").read_text()), 0)
                recovered = TRAIN_MODULE.run(
                    [sys.executable, "-c", "print('recovered')"], root, dict(os.environ), 2,
                )
                self.assertEqual((recovered.returncode, recovered.stdout, recovered.stderr), (0, b"recovered\n", b""))


class GymWorktreeCommandTests(unittest.TestCase):
    def test_cleanup_unlinks_replaced_roots_and_preserves_other_worktrees(self) -> None:
        for registered in [False, True]:
            with self.subTest(registered=registered), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                repo = make_repo(root)
                other = root / "other"
                git(repo, "worktree", "add", "--detach", str(other), "HEAD")
                git(repo, "worktree", "lock", "--reason", "unrelated", str(other))
                original = git(repo, "worktree", "list", "--porcelain")
                workspace = root / "owned"
                if registered:
                    git(repo, "worktree", "add", "--detach", str(workspace), "HEAD")
                    workspace.rename(root / "retained")
                workspace.symlink_to(other, target_is_directory=True)
                TRAIN_MODULE.remove_workspace(repo, workspace)
                self.assertEqual((other / "value.txt").read_text(), "broken\n")
                self.assertFalse(os.path.lexists(workspace))
                self.assertEqual(git(repo, "worktree", "list", "--porcelain"), original)
                if registered:
                    self.assertEqual((root / "retained/value.txt").read_text(), "broken\n")

    def test_filesystem_cleanup_failure_reports_data_and_preserves_registration(self) -> None:
        if os.geteuid() == 0:
            self.skipTest("permission fixture requires an unprivileged user")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            workspace = root / "worktree"
            git(repo, "worktree", "add", "--detach", str(workspace), "HEAD")
            original = git(repo, "worktree", "list", "--porcelain")
            protected = workspace / "protected"
            protected.mkdir()
            (protected / "file").write_text("retained")
            protected.chmod(0o500)
            try:
                with self.assertRaises(WORKTREES.WorktreeError) as failed:
                    TRAIN_MODULE.remove_workspace(repo, workspace)
                self.assertIn("filesystem cleanup", str(failed.exception))
                self.assertEqual((protected / "file").read_text(), "retained")
                # A partial removal may delete the .git pointer, but the
                # registration must remain available for cleanup retry.
                registrations = git(repo, "worktree", "list", "--porcelain")
                self.assertEqual([line for line in registrations.splitlines() if not line.startswith("prunable ")],
                                 original.splitlines())
            finally:
                protected.chmod(0o700)
                TRAIN_MODULE.remove_workspace(repo, workspace)
            self.assertFalse(workspace.exists())
            self.assertEqual(len(git(repo, "worktree", "list").splitlines()), 1)

    def test_stalled_checkout_is_setup_failure_and_bounds_output(self) -> None:
        for caller in ["trainer", "miner"]:
            with self.subTest(caller=caller), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                repo = make_repo(root)
                pid_file = root / "hook-pid"
                marker = root / "oracle-ran"
                code = (
                    "import os,time; from pathlib import Path\n"
                    f"Path({str(pid_file)!r}).write_text(str(os.getpid()))\n"
                    "for _ in range(32): os.write(1,b'x'*65536); os.write(2,b'y'*65536)\n"
                    "os.write(2,b'checkout-tail'); time.sleep(3)\n"
                )
                hook = repo / ".git/hooks/post-checkout"
                hook.write_text("#!/bin/sh\nexec " + shlex.join([sys.executable, "-c", code]) + "\n")
                hook.chmod(0o755)
                workspace = root / "worktree"
                tracemalloc.start()
                try:
                    with mock.patch.object(WORKTREES, "SETUP_TIMEOUT_SECS", 1):
                        if caller == "trainer":
                            task = {"base_commit": "HEAD", "initial_files": {}, "deleted_files": []}
                            with self.assertRaises(TRAIN_MODULE.EpisodeFailure) as failed:
                                TRAIN_MODULE.prepare_workspace(repo, task, workspace, True)
                            self.assertEqual(failed.exception.reason, "workspace_unavailable")
                        else:
                            with self.assertRaises(MINE.OracleSetupError) as failed:
                                MINE.oracle_at(repo, "HEAD", "touch " + shlex.quote(str(marker)))
                    peak = tracemalloc.get_traced_memory()[1]
                    self.assertIn("git worktree add timed out after 1s", str(failed.exception))
                    self.assertIn("checkout-tail", str(failed.exception))
                    self.assertLess(len(str(failed.exception)), 2200)
                    self.assertLess(peak, 1024 * 1024, "checkout diagnostics retained the flood")
                    self.assertFalse(marker.exists(), "an unavailable checkout cannot produce oracle evidence")
                    self.assertTrue(pid_file.exists(), "the checkout hook must actually start")
                finally:
                    tracemalloc.stop()
                    # Full descendant ownership remains m7bs. Explicitly terminate
                    # this fixture's surviving hook after the Git root is killed.
                    if pid_file.exists():
                        try:
                            os.kill(int(pid_file.read_text()), signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                    TRAIN_MODULE.remove_workspace(repo, workspace)
                self.assertEqual(len(git(repo, "worktree", "list").splitlines()), 1)

    def test_cleanup_failures_attempt_remaining_steps_and_reap_commands(self) -> None:
        real_git = shutil.which("git")
        self.assertIsNotNone(real_git)
        for caller, operation, outcome in [("trainer", "remove", "timeout"), ("trainer", "prune", "timeout"),
                                           ("miner", "prune", "error")]:
            with self.subTest(caller=caller, operation=operation, outcome=outcome), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                repo = make_repo(root)
                workspace = root / "worktree"
                if caller == "trainer":
                    git(repo, "worktree", "add", "--detach", str(workspace), "HEAD")
                shim_dir = root / "bin"
                shim_dir.mkdir()
                pid_file = root / "command-pid"
                calls = root / "calls.jsonl"
                shim = shim_dir / "git"
                shim.write_text(
                    f"#!{sys.executable}\nimport os,sys,time,json\nfrom pathlib import Path\n"
                    f"with open({str(calls)!r},'a') as log: log.write(json.dumps(sys.argv[1:])+'\\n')\n"
                    f"if sys.argv[1:3]==['worktree',{operation!r}]:\n"
                    f" Path({str(pid_file)!r}).write_text(str(os.getpid()))\n"
                    " for _ in range(32): os.write(1,b'x'*65536); os.write(2,b'y'*65536)\n"
                    " os.write(2,b'cleanup-tail')\n"
                    f" if {outcome!r}=='timeout': time.sleep(3)\n"
                    f" sys.exit(7 if {outcome!r}=='error' else 0)\n"
                    f"os.execv({real_git!r},[{real_git!r},*sys.argv[1:]])\n"
                )
                shim.chmod(0o755)
                tracemalloc.start()
                try:
                    with mock.patch.dict(os.environ, {"PATH": str(shim_dir) + os.pathsep + os.environ.get("PATH", "")}), \
                         mock.patch.object(WORKTREES, "CLEANUP_TIMEOUT_SECS", 1):
                        exception = WORKTREES.WorktreeError if caller == "trainer" else MINE.OracleSetupError
                        with self.assertRaises(exception) as failed:
                            if caller == "trainer":
                                TRAIN_MODULE.remove_workspace(repo, workspace)
                            else:
                                MINE.oracle_at(repo, "HEAD", "true")
                    peak = tracemalloc.get_traced_memory()[1]
                finally:
                    tracemalloc.stop()
                self.assertIn(f"git worktree {operation} " + ("timed out after 1s" if outcome == "timeout" else "exited 7"),
                              str(failed.exception))
                self.assertIn("cleanup-tail", str(failed.exception))
                self.assertLess(peak, 1024 * 1024, "cleanup diagnostics retained the flood")
                observed = [json.loads(line) for line in calls.read_text().splitlines()]
                self.assertEqual([args[1] for args in observed], ["remove", "prune"] if caller == "trainer"
                                 else ["add", "remove", "prune"])
                removed = next(args[-1] for args in observed if args[1] == "remove")
                self.assertFalse(Path(removed).exists())
                self.assertEqual(len(git(repo, "worktree", "list").splitlines()), 1)
                with self.assertRaises(ProcessLookupError):
                    os.kill(int(pid_file.read_text()), 0)

        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            workspace = repo / "leftover"
            workspace.mkdir()
            (workspace / "file").write_text("cleanup still required")
            with mock.patch.dict(os.environ, {"PATH": ""}):
                with self.assertRaises(WORKTREES.WorktreeError) as failed:
                    TRAIN_MODULE.remove_workspace(repo, workspace)
            self.assertIn("git worktree remove could not complete", str(failed.exception))
            self.assertIn("git worktree prune could not complete", str(failed.exception))
            self.assertFalse(workspace.exists(), "missing Git must not prevent filesystem cleanup")


class GymWorkspaceTests(unittest.TestCase):
    def test_overlay_rejects_symlink_ancestors_and_records_failed_rows(self) -> None:
        for operation in ["initial_files", "deleted_files"]:
            with self.subTest(operation=operation), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                repo = make_repo(root)
                outside = root / "outside"
                outside.mkdir()
                sentinel = outside / "value.txt"
                sentinel.write_text("untouched\n")
                (repo / "alias").symlink_to(outside, target_is_directory=True)
                git(repo, "add", "alias")
                git(repo, "commit", "-qm", "directory link")
                binary, log = make_stub(root, "success")
                entry = {"name": "escape", operation: {"alias/value.txt": "changed\n"}
                         if operation == "initial_files" else ["alias/value.txt"]}
                completed, rows, gym_root = run_training(root, repo, binary, task_document([entry]))
                self.assertEqual(sentinel.read_text(), "untouched\n")
                self.assertEqual(completed.returncode, 0, completed.stderr)
                self.assertEqual(len(rows), 2)
                for row in rows:
                    self.assertFalse(row["success"])
                    self.assertEqual(row["failure_reason"], "workspace_unavailable")
                    self.assertIsNone(row["agent_exit"])
                self.assertFalse(log.exists())
                self.assertEqual(list((gym_root / "worktrees").iterdir()), [])
                self.assertEqual(len(git(repo, "worktree", "list").splitlines()), 1)

    def test_overlay_replaces_or_unlinks_final_links_without_touching_targets(self) -> None:
        for operation in ["initial_files", "deleted_files"]:
            for kind in ["file", "directory", "dangling", "hardlink"]:
                with self.subTest(operation=operation, kind=kind), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    repo = make_repo(root)
                    outside = root / "outside"
                    outside.mkdir()
                    sentinel = outside / "value.txt"
                    sentinel.write_text("untouched\n")
                    target = {"file": sentinel, "directory": outside, "dangling": outside / "missing"}.get(kind)
                    if target is not None:
                        (repo / "alias").symlink_to(target, target_is_directory=kind == "directory")
                        git(repo, "add", "alias")
                        git(repo, "commit", "-qm", "final link")
                    else:
                        # Git does not preserve hardlinks. Create one in the
                        # real checkout hook, before the overlay is applied.
                        hook = repo / ".git/hooks/post-checkout"
                        hook.write_text("#!/bin/sh\nln " + shlex.quote(str(sentinel)) + " alias\n")
                        hook.chmod(0o755)
                    workspace = root / "worktree"
                    task = {"base_commit": "HEAD", "initial_files": {}, "deleted_files": []}
                    task[operation] = {"alias": "replacement\r\n"} if operation == "initial_files" else ["alias"]
                    TRAIN_MODULE.prepare_workspace(repo, task, workspace, False)
                    self.assertEqual(sentinel.read_text(), "untouched\n")
                    self.assertEqual(list(outside.iterdir()), [sentinel])
                    alias = workspace / "alias"
                    self.assertFalse(alias.is_symlink())
                    if operation == "initial_files":
                        self.assertEqual(alias.read_bytes(), b"replacement\r\n")
                    else:
                        self.assertFalse(alias.exists())

    def test_overlay_preserves_executable_mode_and_cleans_failed_publication(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            (repo / "script").write_text("old\n")
            (repo / "script").chmod(0o755)
            git(repo, "add", "script")
            git(repo, "commit", "-qm", "executable")
            workspace = root / "worktree"
            task = {"base_commit": "HEAD", "initial_files": {"script": "new\r\n", "nested/file": "added"},
                    "deleted_files": ["missing/child"]}
            TRAIN_MODULE.prepare_workspace(repo, task, workspace, False)
            self.assertEqual((workspace / "script").read_bytes(), b"new\r\n")
            self.assertTrue((workspace / "script").stat().st_mode & 0o111)
            self.assertEqual((workspace / "nested/file").read_text(), "added")
            TRAIN_MODULE.remove_workspace(repo, workspace)
            with mock.patch.object(TRAIN_MODULE.os, "replace", side_effect=OSError("publication failed")):
                with self.assertRaisesRegex(TRAIN_MODULE.EpisodeFailure, "publication failed"):
                    TRAIN_MODULE.prepare_workspace(repo, task, workspace, False)
            self.assertEqual((workspace / "script").read_text(), "old\n")
            self.assertEqual(sorted(p.name for p in workspace.iterdir()), [".git", "script", "value.txt"])

    def test_overlay_keeps_bound_parent_when_its_name_becomes_a_symlink(self) -> None:
        for operation in ["initial_files", "deleted_files"]:
            with self.subTest(operation=operation), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                repo = make_repo(root)
                (repo / "nested").mkdir()
                (repo / "nested/value").write_text("old")
                git(repo, "add", "nested")
                git(repo, "commit", "-qm", "nested file")
                outside = root / "outside"
                outside.mkdir()
                (outside / "value").write_text("untouched")
                workspace = root / "worktree"
                task = {"base_commit": "HEAD", "initial_files": {}, "deleted_files": []}
                task[operation] = {"nested/value": "new"} if operation == "initial_files" else ["nested/value"]
                original_open = os.open
                swapped = False

                def swap_after_open(path, flags, *args, **kwargs):
                    nonlocal swapped
                    descriptor = original_open(path, flags, *args, **kwargs)
                    if path == "nested" and flags & os.O_DIRECTORY and not swapped:
                        (workspace / "nested").rename(workspace / "retained")
                        (workspace / "nested").symlink_to(outside, target_is_directory=True)
                        swapped = True
                    return descriptor

                with mock.patch.object(TRAIN_MODULE.os, "open", side_effect=swap_after_open):
                    TRAIN_MODULE.prepare_workspace(repo, task, workspace, False)
                self.assertTrue(swapped, "fixture must replace the directory after it is opened")
                self.assertEqual((outside / "value").read_text(), "untouched")
                if operation == "initial_files":
                    self.assertEqual((workspace / "retained/value").read_text(), "new")
                else:
                    self.assertFalse((workspace / "retained/value").exists())


class GymFileOracleTests(unittest.TestCase):
    def test_file_oracle_confines_links_and_replaced_paths_to_the_workspace(self) -> None:
        cases = ["final_link", "ancestor_link", "root_link", "missing_root", "link_cycle", "internal_link", "internal_absolute_link",
                 "internal_directory_link", "replace_final", "replace_before_parent", "replace_parent", "replace_root"]
        for case in cases:
            with self.subTest(case=case), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                workspace = root / "workspace"
                outside = root / "outside"
                (workspace / "nested").mkdir(parents=True)
                (outside / "nested").mkdir(parents=True)
                (outside / "answer").write_text("expected")
                (outside / "nested/answer").write_text("expected")
                target = workspace / "nested/answer"
                target.write_text("wrong")
                relative = "nested/answer"
                outcome = (1, "unreadable: nested/answer")
                if case == "final_link":
                    target.unlink()
                    target.symlink_to(outside / "answer")
                elif case == "ancestor_link":
                    shutil.rmtree(workspace / "nested")
                    (workspace / "nested").symlink_to(outside, target_is_directory=True)
                elif case in ("root_link", "missing_root"):
                    shutil.rmtree(workspace)
                    if case == "root_link":
                        workspace.symlink_to(outside, target_is_directory=True)
                    outcome = (1, "unreadable workspace")
                elif case == "link_cycle":
                    target.unlink()
                    target.symlink_to("answer")
                elif case.startswith("internal"):
                    target.write_text("expected")
                    relative = "alias/answer" if case == "internal_directory_link" else "alias"
                    link_target = "nested" if case == "internal_directory_link" else (
                        target if case == "internal_absolute_link" else "nested/answer"
                    )
                    (workspace / "alias").symlink_to(link_target, target_is_directory=case == "internal_directory_link")
                    outcome = (0, "")
                elif case not in ("replace_final", "replace_before_parent"):
                    outcome = (1, "differs: nested/answer")

                original_open = os.open
                replaced = False

                def replace_before_open(path, flags, *args, **kwargs):
                    nonlocal replaced
                    trigger = "nested" if case == "replace_before_parent" else "answer"
                    if case.startswith("replace_") and Path(path).name == trigger and not replaced:
                        replaced = True
                        victim = target if case == "replace_final" else (
                            workspace / "nested" if case in ("replace_before_parent", "replace_parent") else workspace
                        )
                        victim.rename(root / "retained")
                        replacement = outside / "answer" if case == "replace_final" else outside
                        victim.symlink_to(replacement, target_is_directory=case != "replace_final")
                    return original_open(path, flags, *args, **kwargs)

                with mock.patch.object(os, "open", side_effect=replace_before_open):
                    self.assertEqual(
                        TRAIN_MODULE.run_oracle({"expected_files": {relative: "expected"}}, workspace, {}), outcome
                    )
                if case.startswith("replace_"):
                    self.assertTrue(replaced, "the replacement must occur during the real oracle read")

    def test_exact_file_comparison_handles_boundaries_and_invalid_content(self) -> None:
        cases = [
            ("exact", "snow ☃\r\n", "snow ☃\r\n".encode(), (0, "")),
            ("empty", "", b"", (0, "")),
            ("extra_byte", "ok", b"ok!", (1, "differs: result.txt")),
            ("empty_extra", "", b"x", (1, "differs: result.txt")),
            ("short", "ok", b"o", (1, "differs: result.txt")),
            ("newline", "ok\n", b"ok\r\n", (1, "differs: result.txt")),
            ("oversized", "ok", b"ok" + b"x" * (4 * 1024 * 1024), (1, "differs: result.txt")),
            ("invalid_utf8", "ok", b"\xff", (1, "unreadable: result.txt")),
            ("missing", "ok", None, (1, "unreadable: result.txt")),
            ("directory", "ok", None, (1, "unreadable: result.txt")),
        ]
        for name, expected, contents, outcome in cases:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                workspace = Path(directory)
                target = workspace / "result.txt"
                if name == "directory":
                    target.mkdir()
                elif contents is not None:
                    target.write_bytes(contents)
                if name == "oversized":
                    # Measure the real read, excluding fixture allocation. An
                    # unbounded read of this 4 MiB file exceeds this generous
                    # 1 MiB ceiling even though it correctly reports a mismatch.
                    tracemalloc.start()
                try:
                    self.assertEqual(
                        TRAIN_MODULE.run_oracle({"expected_files": {"result.txt": expected}}, workspace, {}),
                        outcome,
                    )
                    if name == "oversized":
                        self.assertLess(tracemalloc.get_traced_memory()[1], 1024 * 1024)
                finally:
                    if name == "oversized":
                        tracemalloc.stop()

    @unittest.skipUnless(hasattr(os, "mkfifo"), "requires POSIX FIFOs")
    def test_fifo_file_oracle_returns_failure_without_waiting_for_a_writer(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory)
            os.mkfifo(workspace / "result.txt")
            script = (
                "import importlib.util,json,sys; from pathlib import Path; "
                "sys.path.insert(0,str(Path(sys.argv[1]).parent)); "
                "spec=importlib.util.spec_from_file_location('train',sys.argv[1]); "
                "train=importlib.util.module_from_spec(spec); spec.loader.exec_module(train); "
                "print(json.dumps(train.run_oracle({'expected_files': {'result.txt':'ok'}},Path(sys.argv[2]),{})))"
            )
            completed = subprocess.run(
                [sys.executable, "-c", script, str(TRAIN), str(workspace)],
                capture_output=True, text=True, timeout=3,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(json.loads(completed.stdout), [1, "unreadable: result.txt"])


class GymTrainerTests(unittest.TestCase):
    def test_database_queries_preserve_literal_paths_and_read_only_access(self) -> None:
        for name in ["skills#probe.db", "skills?mode=rw.db", "skills%23probe.db", "space 雪.db"]:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                database = root / name
                with contextlib.closing(sqlite3.connect(database)) as connection, connection:
                    connection.execute("CREATE TABLE records (id TEXT)")
                    connection.execute("INSERT INTO records VALUES ('expected')")
                original_connect = sqlite3.connect
                readers = []

                def retain_reader(*args, **kwargs):
                    reader = original_connect(*args, **kwargs)
                    readers.append(reader)
                    return reader

                try:
                    with mock.patch.object(sqlite3, "connect", side_effect=retain_reader):
                        self.assertEqual(TRAIN_MODULE.query(database, "SELECT id FROM records"), ["expected"])
                        with self.assertRaisesRegex(sqlite3.OperationalError, "readonly"):
                            TRAIN_MODULE.query(database, "DELETE FROM records")
                        self.assertEqual(TRAIN_MODULE.query(database, "SELECT id FROM records"), ["expected"])
                        with self.assertRaises(sqlite3.OperationalError):
                            TRAIN_MODULE.query(root / ("missing-" + name), "SELECT 1")
                    # Retain actual connections so garbage collection cannot hide a leak.
                    for reader in readers:
                        with self.assertRaisesRegex(sqlite3.ProgrammingError, "closed"):
                            reader.execute("SELECT 1")
                finally:
                    for reader in readers:
                        reader.close()
                self.assertEqual(sorted(path.name for path in root.iterdir()), [name])

    def test_apppaths_setup_requires_a_fresh_root(self) -> None:
        for case in ["symlink", "file", "protected"]:
            if case == "protected" and os.geteuid() == 0:
                continue
            with self.subTest(case=case), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                repo = make_repo(root)
                binary, log = make_stub(root, "success")
                owned = root / "gym/runs/fix-none"
                owned.parent.mkdir(parents=True)
                outside = root / "outside"
                (outside / "config").mkdir(parents=True)
                sentinel = outside / "config/config.toml"
                sentinel.write_text("sentinel = true\n")
                protected = owned / "local/retained"
                if case == "symlink":
                    owned.symlink_to(outside, target_is_directory=True)
                elif case == "file":
                    owned.write_text("stale entry")
                else:
                    protected.mkdir(parents=True)
                    (protected / "stale").write_text("prior episode")
                    protected.chmod(0o500)
                try:
                    completed, rows, _ = run_training(
                        root, repo, binary, task_document([{"name": "fix"}]), "--keep-run-dirs"
                    )
                    self.assertEqual(sentinel.read_text(), "sentinel = true\n")
                    if case == "protected":
                        self.assertEqual(completed.returncode, 2, completed.stderr)
                        self.assertIn("AppPaths", completed.stderr)
                        self.assertEqual(rows, [])
                        self.assertFalse(log.exists(), "stale state must not reach the agent")
                        self.assertEqual((protected / "stale").read_text(), "prior episode")
                    else:
                        self.assertEqual(completed.returncode, 0, completed.stderr)
                        self.assertEqual(len(rows), 2)
                        self.assertTrue(all(row["success"] for row in rows), rows)
                        self.assertFalse(owned.is_symlink())
                        self.assertTrue((owned / "config/config.toml").is_file())
                finally:
                    if case == "protected":
                        protected.chmod(0o700)

    def test_apppaths_cleanup_reports_failures_and_runs_after_worktree_errors(self) -> None:
        real_git = shutil.which("git")
        self.assertIsNotNone(real_git)
        for case in ["filesystem", "worktree", "both", "keep"]:
            if case in ("filesystem", "both") and os.geteuid() == 0:
                continue
            with self.subTest(case=case), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                repo = make_repo(root)
                binary, log = make_stub(root, "success")
                marker = root / "agent-ran"
                suffix = "\ntouch " + shlex.quote(str(marker)) + "\n"
                if case in ("filesystem", "both"):
                    suffix += ('mkdir -p "$ZS_LOCAL_DATA_DIR/retained"\n'
                               'printf stale > "$ZS_LOCAL_DATA_DIR/retained/stale"\n'
                               'chmod 500 "$ZS_LOCAL_DATA_DIR/retained"\n')
                binary.write_text(binary.read_text().replace("printf 'fixed\\n' > fixed.txt", suffix + "printf 'fixed\\n' > fixed.txt"))
                shim_dir = root / "bin"
                shim_dir.mkdir()
                shim = shim_dir / "git"
                shim.write_text(
                    "#!/bin/sh\n"
                    f"if test -f {shlex.quote(str(marker))} && test \"$1 $2\" = 'worktree prune'; then\n"
                    " echo 'fixture prune failure' >&2; exit 7\nfi\n"
                    f"exec {shlex.quote(real_git)} \"$@\"\n"
                )
                shim.chmod(0o755)
                extra = ("--keep-run-dirs",) if case == "keep" else ()
                try:
                    with mock.patch.dict(os.environ, {"PATH": (str(shim_dir) + os.pathsep if case != "filesystem" else "")
                                                     + os.environ.get("PATH", "")}):
                        completed, rows, gym = run_training(root, repo, binary, task_document([{"name": "fix"}]), *extra)
                    self.assertEqual(completed.returncode, 2, completed.stderr)
                    self.assertEqual(rows, [], "cleanup failure cannot publish a successful episode")
                    self.assertTrue(log.exists(), "failure must happen after agent execution")
                    if case != "filesystem":
                        self.assertIn("fixture prune failure", completed.stderr)
                    owned = gym / "runs/fix-none"
                    if case in ("filesystem", "both"):
                        self.assertIn("AppPaths", completed.stderr)
                        self.assertEqual((owned / "local/retained/stale").read_text(), "stale")
                    else:
                        self.assertEqual(owned.exists(), case == "keep")
                    self.assertEqual(len(git(repo, "worktree", "list").splitlines()), 1)
                finally:
                    for protected in (root / "gym/runs").glob("*/local/retained"):
                        protected.chmod(0o700)

    def test_successful_run_records_rows_and_cleans_up(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            # Exercise the whole library arm with characters meaningful in a SQLite URI.
            root = Path(directory) / "gym #?%23 space"
            root.mkdir()
            repo = make_repo(root)
            binary, log = make_stub(root, "success")
            completed, rows, gym_root = run_training(
                root, repo, binary, task_document([{"name": "fix", "tags": ["unit"]}]), "--provider", "fake", "--model", "fake-1"
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual([row["arm"] for row in rows], ["none", "library"])
            for row in rows:
                self.assertTrue(row["success"], row)
                self.assertEqual(row["agent_exit"], 0)
                self.assertEqual(row["oracle_exit"], 0)
                self.assertEqual(row["oracle_pre_exit"], 1)
                self.assertIsNone(row["failure_reason"])
                self.assertFalse(row["production"])
                self.assertEqual(row["permission_mode"], "standard")
                self.assertEqual(row["provider"], "fake")
                self.assertEqual(row["model"], "fake-1")
                self.assertEqual(row["budgets_enforced"], {"max_agent_turns": 3})
                self.assertEqual(row["budgets_unenforced"], ["max_tool_calls", "max_total_tokens"])
            self.assertEqual(rows[0]["active_skill_ids"], [])
            self.assertEqual(rows[1]["active_skill_ids"], ["skill-root"])
            self.assertIn("GYM_SUMMARY", completed.stdout)
            self.assertIn("gym arm library: 1 passed, 0 failed of 1", completed.stdout)
            # Each episode passes the provider-turn budget to the binary.
            for line in log.read_text(encoding="utf-8").splitlines():
                self.assertTrue(line.startswith("--max-agent-turns 3 -p "), line)
            self.assertEqual(git(repo, "worktree", "list").strip().count("\n"), 0)
            self.assertEqual(sorted((gym_root / "worktrees").iterdir()), [])
            self.assertEqual(sorted((gym_root / "runs").iterdir()), [])

    def test_workspace_and_apppaths_stay_under_the_gym_root(self) -> None:
        # The episode workspace and every per-arm tree must live under the gym
        # root, not in the system temp dir: macOS Seatbelt write-allows
        # /private/tmp wholesale and Linux bwrap replaces /tmp with a tmpfs, so
        # a temp-dir arm is either outside the boundary under test or invisible
        # to the sandboxed child.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            binary, log = make_stub(root, "success")
            provider = 'gateway "quoted"\\route\nsecurity_mode = "yolo"'
            model = 'model-🚀\tfast\r\n\\edition\x7f'
            completed, rows, gym_root = run_training(
                root, repo, binary, task_document([{"name": "fix", "tags": []}]), "--keep-run-dirs",
                "--provider", provider, "--model", model,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            observed = [
                line.split("\t")
                for line in Path(f"{log}.env").read_text(encoding="utf-8").splitlines()
            ]
            self.assertEqual(len(observed), len(rows))
            # Compared resolved: the shell reports the physical cwd, which on
            # macOS differs from the symlinked temp path by /private.
            worktrees = (gym_root / "worktrees").resolve()
            runs = (gym_root / "runs").resolve()
            for cwd, tmpdir, local in observed:
                self.assertEqual(Path(cwd).resolve().parent, worktrees, cwd)
                for owned in (tmpdir, local):
                    self.assertEqual(Path(owned).resolve().parent.parent, runs, owned)
            # --keep-run-dirs leaves the AppPaths trees behind; they are the
            # gym's, so they are inside the gym root.
            run_dirs = sorted((gym_root / "runs").iterdir())
            self.assertEqual(len(run_dirs), 2)
            for run_dir in run_dirs:
                with (run_dir / "config/config.toml").open("rb") as config:
                    self.assertEqual(tomllib.load(config), {"provider": provider, "model": model})

    def test_phase_clocks_separate_agent_oracles_and_setup_on_success_failure_and_timeout(self) -> None:
        for arm in ("none", "library"):
            for outcome in ("success", "failure", "timeout"):
                with self.subTest(arm=arm, outcome=outcome), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    repo = make_repo(root)
                    gym_root = root / "gym"
                    task_path = root / "tasks.json"
                    task_path.write_text(json.dumps(task_document([{"name": "clock", "timeout_secs": 5}])))
                    task = TRAIN_MODULE.load_tasks(task_path, 5)[0]
                    args = mock.Mock(binary="agent", provider=None, model=None, forward_env=[], agent_arg=[],
                                     allow_empty_workspace=False, keep_run_dirs=False)
                    now = 100.0
                    oracle_calls = 0
                    original_prepare = TRAIN_MODULE.prepare_workspace
                    original_cleanup = TRAIN_MODULE.cleanup_episode

                    def prepare(*args, **kwargs):
                        nonlocal now
                        original_prepare(*args, **kwargs)
                        now += 7

                    def cleanup(*args, **kwargs):
                        nonlocal now
                        original_cleanup(*args, **kwargs)
                        # Initial stale-state cleanup is outside total_ms; final cleanup is inside.
                        now += 13

                    def install(*args):
                        nonlocal now
                        now += 11
                        return ["skill-root"]

                    def oracle(*args):
                        nonlocal now, oracle_calls
                        oracle_calls += 1
                        now += 3 if oracle_calls == 1 else 5
                        return (1, "before") if oracle_calls == 1 else (0, "")

                    def agent(argv, cwd, env, timeout):
                        nonlocal now
                        self.assertEqual(timeout, 5)
                        now += 5 if outcome == "timeout" else 2
                        if outcome == "timeout":
                            raise subprocess.TimeoutExpired(argv, timeout, stderr=b"timeout detail")
                        return subprocess.CompletedProcess(argv, 3 if outcome == "failure" else 0, b"", b"agent detail")

                    with mock.patch.object(TRAIN_MODULE.time, "monotonic", side_effect=lambda: now), \
                         mock.patch.object(TRAIN_MODULE, "prepare_workspace", side_effect=prepare), \
                         mock.patch.object(TRAIN_MODULE, "cleanup_episode", side_effect=cleanup), \
                         mock.patch.object(TRAIN_MODULE, "install_library", side_effect=install), \
                         mock.patch.object(TRAIN_MODULE, "run_oracle", side_effect=oracle), \
                         mock.patch.object(TRAIN_MODULE, "run", side_effect=agent):
                        row = TRAIN_MODULE.run_episode(task, arm, args, repo, gym_root)
                    timed_out = outcome == "timeout"
                    self.assertEqual(row["success"], outcome == "success")
                    self.assertEqual(row["elapsed_ms"], 5000 if timed_out else 2000)
                    self.assertEqual(row["oracle_ms"], 3000 if timed_out else 8000)
                    self.assertEqual(row["total_ms"], (28000 if timed_out else 30000) + (11000 if arm == "library" else 0))
                    self.assertEqual(row["agent_exit"], {"success": 0, "failure": 3, "timeout": 124}[outcome])
                    self.assertEqual(row["oracle_pre_exit"], 1)
                    self.assertEqual(row["oracle_exit"], None if timed_out else 0)
                    self.assertEqual(row["failure_reason"], {"success": None, "failure": "agent_exit_nonzero",
                                                            "timeout": "agent_timeout"}[outcome])
                    self.assertEqual(row["agent_stderr_tail"], "timeout detail" if timed_out else "agent detail")
                    self.assertEqual(row["active_skill_ids"], ["skill-root"] if arm == "library" else [])
                    self.assertEqual(sorted((gym_root / "worktrees").iterdir()), [])
                    self.assertEqual(sorted((gym_root / "runs").iterdir()), [])
                    self.assertEqual(git(repo, "worktree", "list").strip().count("\n"), 0)

    def test_agent_failure_is_a_failed_row_and_the_run_still_exits_zero(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            binary, _ = make_stub(root, "failure")
            completed, rows, gym_root = run_training(root, repo, binary, task_document([{"name": "fix", "tags": []}]))
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(len(rows), 2)
            for row in rows:
                self.assertFalse(row["success"])
                self.assertEqual(row["agent_exit"], 3)
                self.assertEqual(row["failure_reason"], "agent_exit_nonzero")
                self.assertIn("agent failed", row["agent_stderr_tail"])
            self.assertIn("gym arm none: 0 passed, 1 failed of 1", completed.stdout)
            self.assertEqual(sorted((gym_root / "worktrees").iterdir()), [])

    def test_library_install_failure_only_fails_the_library_arm(self) -> None:
        for failure in ["command", "corrupt_database", "incompatible_schema", "missing_database", "no_active_roots"]:
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                repo = make_repo(root)
                binary, _ = make_stub(root, "install_fail" if failure == "command" else "success")
                if failure in ("corrupt_database", "incompatible_schema", "missing_database"):
                    payload = {
                        "corrupt_database": "(root/'skills.db').write_bytes(b'not sqlite')",
                        "incompatible_schema": "sqlite3.connect(root/'skills.db').close()",
                        "missing_database": "pass",
                    }[failure]
                    (root / "stub_db.py").write_text(
                        "import pathlib,sqlite3,sys\n"
                        "root=pathlib.Path(sys.argv[2])/'skills'; root.mkdir(parents=True,exist_ok=True)\n" + payload + "\n"
                    )
                elif failure == "no_active_roots":
                    helper = root / "stub_db.py"
                    helper.write_text(helper.read_text().replace("status = 'active'", "status = 'pending'"))
                completed, rows, gym_root = run_training(
                    root, repo, binary, task_document([{"name": "first"}, {"name": "second"}])
                )
                self.assertEqual(completed.returncode, 0, completed.stderr)
                self.assertEqual([(row["task"], row["arm"]) for row in rows],
                                 [(name, arm) for name in ("first", "second") for arm in ("none", "library")])
                for row in rows:
                    if row["arm"] == "none":
                        self.assertTrue(row["success"], row)
                        continue
                    self.assertFalse(row["success"])
                    self.assertEqual(row["failure_reason"], "library_install_failed")
                    detail = {"command": "seed import exploded", "corrupt_database": "file is not a database",
                              "incompatible_schema": "no such table", "missing_database": "0 skills.db files",
                              "no_active_roots": "left no active skill revision"}[failure]
                    self.assertIn(detail, row["failure_detail"])
                    self.assertIsNone(row["agent_exit"])
                self.assertEqual(sorted((gym_root / "worktrees").iterdir()), [])
                self.assertEqual(sorted((gym_root / "runs").iterdir()), [])
                self.assertEqual(git(repo, "worktree", "list").strip().count("\n"), 0)

    def test_library_packages_resolve_from_invocation_before_neutral_import(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            repo = make_repo(root)
            invocation = root / "invocation"
            invocation.mkdir()
            bundle = invocation / "bundle file.json"
            bundle.write_text("expected\n")
            package_dir = invocation / "bundle directory"
            package_dir.mkdir()
            (package_dir / "package.json").write_text("expected\n")
            imports = root / "imports.log"
            binary, _ = make_stub(root, "success")
            binary.write_text(binary.read_text().replace("mode=success", "mode=success\n" + (
                'if [ "${1:-}" = --import-learned-skill ]; then\n'
                f'  printf "%s\\n" "$2" >> {shlex.quote(str(imports))}\n'
                '  bundle="$2"; if [ -d "$bundle" ]; then bundle="$bundle/package.json"; fi\n'
                '  grep -qx expected "$bundle" || exit 9\n'
                'fi\n'
            )))
            document = task_document([
                {"name": "relative-file", "library": bundle.name},
                {"name": "relative-directory", "library": package_dir.name},
                {"name": "absolute-file", "library": str(bundle)},
            ])
            completed, rows, gym_root = run_training(root, repo, binary, document, cwd=invocation)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(len(rows), 6)
            for row in rows:
                self.assertTrue(row["success"], row)
                self.assertEqual(row["active_skill_ids"], ["skill-root"] if row["arm"] == "library" else [])
            self.assertEqual(imports.read_text().splitlines(), [str(bundle), str(package_dir), str(bundle)])
            self.assertEqual(sorted((gym_root / "runs").iterdir()), [])

    def test_failed_checkouts_require_opt_in_and_a_fresh_empty_fallback(self) -> None:
        for failure in ["missing_revision", "checkout_hook", "locked_hook"]:
            for allow_empty in [False, True]:
                with self.subTest(failure=failure, allow_empty=allow_empty), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    repo = make_repo(root)
                    base = "0" * 40
                    if failure != "missing_revision":
                        base = "HEAD"
                        hook = repo / ".git/hooks/post-checkout"
                        lock = 'git worktree lock "$PWD"\n' if failure == "locked_hook" else ""
                        hook.write_text("#!/bin/sh\n" + lock + "printf stray > hook-artifact\nexit 1\n")
                        hook.chmod(0o755)
                    binary, log = make_stub(root, "success")
                    document = task_document([{"name": "fix", "base_commit": base, "initial_files": {"seed": "input"}}])
                    document["defaults"]["oracle"] = {
                        "command": "test -f fixed.txt && test -f seed && test ! -e .git && test ! -e value.txt && test ! -e hook-artifact"
                    }
                    completed, rows, gym_root = run_training(
                        root, repo, binary, document, *(["--allow-empty-workspace"] if allow_empty else [])
                    )
                    self.assertEqual(completed.returncode, 0, completed.stderr)
                    self.assertEqual(len(rows), 2)
                    self.assertEqual(log.exists(), allow_empty)
                    for row in rows:
                        self.assertEqual(row["success"], allow_empty, row)
                        if allow_empty:
                            self.assertEqual(row["oracle_pre_exit"], 1)
                            self.assertEqual(row["oracle_exit"], 0)
                        else:
                            self.assertEqual(row["failure_reason"], "workspace_unavailable")
                            self.assertIn("git worktree add", row["failure_detail"])
                            self.assertIsNone(row["agent_exit"])
                    self.assertEqual(list((gym_root / "worktrees").iterdir()), [])
                    self.assertEqual(len(git(repo, "worktree", "list").splitlines()), 1)
                    if failure == "checkout_hook" and allow_empty:
                        # A cleanup failure must not let the overlay modify a
                        # partial checkout while calling it an empty fallback.
                        destination = root / "cleanup-failed"
                        task = {"base_commit": "HEAD", "initial_files": {"value.txt": "changed"}, "deleted_files": []}
                        with mock.patch.object(TRAIN_MODULE, "remove_workspace"):
                            with self.assertRaises(TRAIN_MODULE.EpisodeFailure) as failed:
                                TRAIN_MODULE.prepare_workspace(repo, task, destination, True)
                        self.assertEqual(failed.exception.reason, "workspace_unavailable")
                        self.assertEqual((destination / "value.txt").read_text(), "broken\n")
                        TRAIN_MODULE.remove_workspace(repo, destination)

    def test_agent_args_are_forwarded_and_recorded(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            binary, log = make_stub(root, "success")
            completed, rows, _ = run_training(
                root, repo, binary, task_document([{"name": "fix", "tags": []}]), "--agent-arg=--yolo"
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            for row in rows:
                self.assertEqual(row["permission_mode"], "yolo")
                self.assertEqual(row["agent_args"], ["--yolo"])
            self.assertIn("--max-agent-turns 3 --yolo -p", log.read_text(encoding="utf-8"))

    def test_expected_files_are_the_fallback_oracle(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            binary, _ = make_stub(root, "success")
            document = task_document([{"name": "fix", "tags": []}])
            document["defaults"]["oracle"] = {"expected_files": {"fixed.txt": "fixed\n"}, "id": "expected-files"}
            completed, rows, _ = run_training(root, repo, binary, document)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            for row in rows:
                self.assertTrue(row["success"], row)
                self.assertEqual(row["oracle_pre_exit"], 1)
                self.assertEqual(row["oracle_id"], "expected-files")

    def test_oracle_that_passes_before_the_agent_marks_the_task_invalid(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            binary, log = make_stub(root, "success")
            document = task_document([{"name": "already-green", "tags": []}])
            document["defaults"]["oracle"] = {"command": "test -f value.txt", "id": "always"}
            completed, rows, _ = run_training(root, repo, binary, document)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            for row in rows:
                self.assertFalse(row["success"])
                self.assertEqual(row["oracle_pre_exit"], 0)
                self.assertEqual(row["failure_reason"], "task_invalid_oracle_passes_before_agent")
                self.assertIsNone(row["agent_exit"])
            self.assertFalse(log.exists(), "the agent must not run for an already-passing task")

    def test_deleted_files_are_removed_from_the_workspace(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            sentinel = root / "outside"
            sentinel.write_text("untouched")
            (repo / "discard/nested").mkdir(parents=True)
            (repo / "discard/nested/file").write_text("delete me")
            (repo / "discard/link").symlink_to(sentinel)
            git(repo, "add", "discard")
            git(repo, "commit", "-qm", "directory to delete")
            binary, _ = make_stub(root, "success")
            document = task_document([{"name": "delete", "tags": [], "deleted_files": ["value.txt", "discard"]}])
            document["defaults"]["oracle"] = {"command": "test ! -e value.txt && test ! -e discard && test -f fixed.txt", "id": "deleted"}
            completed, rows, _ = run_training(root, repo, binary, document)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            for row in rows:
                self.assertTrue(row["success"], row)
            self.assertEqual(sentinel.read_text(), "untouched")

    def test_invalid_budget_or_root_file_path_fails_before_any_episode(self) -> None:
        cases = [("budgets", 0)] + [(field, path) for field in ["initial_files", "deleted_files", "oracle"]
                                     for path in [".", "./"]]
        for field, value in cases:
            with self.subTest(field=field, value=value), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                repo = make_repo(root)
                binary, log = make_stub(root, "success")
                document = task_document([{"name": "fix", "tags": []}])
                if field == "budgets":
                    document["defaults"][field]["max_provider_turns"] = value
                elif field == "oracle":
                    document["defaults"][field] = {"expected_files": {value: "content"}}
                else:
                    document["defaults"][field] = {value: "content"} if field == "initial_files" else [value]
                completed, rows, _ = run_training(root, repo, binary, document)
                self.assertEqual(completed.returncode, 2)
                self.assertIn("max_provider_turns" if field == "budgets" else "must stay inside the workspace", completed.stderr)
                self.assertEqual(rows, [])
                self.assertFalse(log.exists())

    def test_legacy_task_arrays_are_rejected_with_a_migration_hint(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            binary, _ = make_stub(root, "success")
            completed, _, _ = run_training(root, repo, binary, [{"name": "fix"}])  # type: ignore[arg-type]
            self.assertEqual(completed.returncode, 2)
            self.assertIn("mine_tasks.py", completed.stderr)

    def test_repository_harness_fixture_loads_under_the_shared_schema(self) -> None:
        # The deterministic Rust fixture is the strict subset of the shared
        # `{defaults, tasks}` schema; the runner must read it unchanged.
        fixture = ROOT / "tests/harness_eval/task.json"
        raw = json.loads(fixture.read_text(encoding="utf-8"))
        tasks = TRAIN_MODULE.load_tasks(fixture, TRAIN_MODULE.DEFAULT_TASK_TIMEOUT_SECS)
        self.assertEqual(len(tasks), len(raw["tasks"]))
        for task, entry in zip(tasks, raw["tasks"]):
            self.assertEqual(task["name"], entry["name"])
            self.assertIsNone(task["oracle"]["command"])
            self.assertTrue(task["oracle"]["expected_files"])
            self.assertGreaterEqual(task["budgets"]["max_provider_turns"], 1)
        # A task that declares nothing but a name inherits every default.
        inherited = [task for task, entry in zip(tasks, raw["tasks"]) if set(entry) <= {"name", "tags"}]
        for task in inherited:
            self.assertEqual(task["prompt"], raw["defaults"]["prompt"])
            self.assertEqual(task["oracle"]["id"], raw["defaults"]["oracle"]["id"])

    def test_mined_document_matches_the_shared_schema(self) -> None:
        fixture = json.loads((ROOT / "tests/harness_eval/task.json").read_text(encoding="utf-8"))
        mined = MINE.document([{"name": "mini-agent-x", "tags": ["mined"]}])
        self.assertEqual(mined["schema_version"], TRAIN_MODULE.SCHEMA_VERSION)
        # Same `defaults` field set as the fixture the Rust harness deserializes.
        self.assertEqual(sorted(mined["defaults"]), sorted(fixture["defaults"]))
        self.assertEqual(sorted(mined["defaults"]["oracle"]), sorted(fixture["defaults"]["oracle"]))
        self.assertEqual(sorted(mined["defaults"]["budgets"]), sorted(fixture["defaults"]["budgets"]))
        self.assertEqual(sorted(mined["defaults"]["scripted_provider_turns"]), ["library", "none"])


class GymMinerTests(unittest.TestCase):
    def test_tracker_command_limits_preserve_fallback_and_complete_records(self) -> None:
        for outcome in ["timeout", "overflow", "exact"]:
            with self.subTest(outcome=outcome), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                repo = make_repo(root)
                (repo / ".beads").mkdir()
                (repo / ".beads/issues.jsonl").write_text('{"id":"fallback","status":"closed"}\n')
                shim_dir = root / "bin"
                shim_dir.mkdir()
                calls = root / "calls"
                payload = b'[{"id":"live","status":"closed"}]'
                limit = 65536
                shim = shim_dir / "bd"
                shim.write_text(
                    f"#!{sys.executable}\nimport os,sys,time,json\n"
                    f"with open({str(calls)!r},'a') as log: log.write(json.dumps([sys.argv[1],os.getpid()])+'\\n')\n"
                    "for _ in range(64): os.write(2,b'e'*65536)\n"
                    "os.write(2,b'tracker-tail')\n"
                    f"if {outcome!r}=='timeout': time.sleep(3)\n"
                    f"os.write(1,{payload!r})\n"
                    f"for _ in range(64 if {outcome!r}=='overflow' else 0): os.write(1,b' '*65536)\n"
                    f"if {outcome!r}=='exact': os.write(1,b' '*({limit}-len({payload!r})))\n"
                )
                shim.chmod(0o755)
                with mock.patch.dict(os.environ, {"PATH": str(shim_dir) + os.pathsep + os.environ.get("PATH", "")}), \
                     mock.patch.object(MINE, "METADATA_TIMEOUT_SECS", 1, create=True), \
                     mock.patch.object(MINE, "MAX_METADATA_BYTES", limit, create=True), \
                     contextlib.redirect_stderr(io.StringIO()) as diagnostics:
                    tracemalloc.start()
                    try:
                        loaded = MINE.load_beads(repo, None)
                        peak = tracemalloc.get_traced_memory()[1]
                    finally:
                        tracemalloc.stop()
                self.assertEqual([bead["id"] for bead in loaded], ["live" if outcome == "exact" else "fallback"])
                self.assertLess(peak, 1024 * 1024, "metadata retained the full stream flood")
                observed = [json.loads(line) for line in calls.read_text().splitlines()]
                self.assertEqual([call[0] for call in observed], ["list"] if outcome == "exact" else ["list", "version"])
                for _, pid in observed:
                    with self.assertRaises(ProcessLookupError):
                        os.kill(pid, 0)
                if outcome == "exact":
                    self.assertEqual(diagnostics.getvalue(), "")
                else:
                    self.assertIn("timed out" if outcome == "timeout" else "exceeded", diagnostics.getvalue())
                    self.assertIn("falling back to", diagnostics.getvalue())
                    self.assertLess(len(diagnostics.getvalue()), 3000)

    def test_git_metadata_failure_aborts_without_replacing_task_output(self) -> None:
        real_git = shutil.which("git")
        self.assertIsNotNone(real_git)
        for operation, outcome in [("log", "timeout"), ("rev-parse", "overflow"),
                                   ("diff", "overflow"), ("cat-file", "timeout")]:
            with self.subTest(operation=operation, outcome=outcome), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                repo = make_repo(root)
                (repo / "value.txt").write_text("fixed\n")
                git(repo, "commit", "-qam", "fix mini-agent-probe")
                beads = root / "beads.json"
                beads.write_text('[{"id":"mini-agent-probe","status":"closed"}]')
                oracle_map = root / "oracles.json"
                oracle_map.write_text('{"mini-agent-probe":"false"}')
                output = root / "tasks.json"
                output.write_text("existing task artifact\n")
                pid_file = root / "failed-command"
                shim_dir = root / "bin"
                shim_dir.mkdir()
                shim = shim_dir / "git"
                shim.write_text(
                    f"#!{sys.executable}\nimport os,sys,time\nfrom pathlib import Path\n"
                    f"if sys.argv[1]=={operation!r}:\n"
                    f" Path({str(pid_file)!r}).write_text(str(os.getpid()))\n"
                    " for _ in range(64): os.write(2,b'e'*65536)\n"
                    " os.write(2,b'metadata-tail')\n"
                    f" if {outcome!r}=='timeout': time.sleep(3)\n"
                    f" else:\n  for _ in range(64): os.write(1,b'x'*65536)\n"
                    f"os.execv({real_git!r},[{real_git!r},*sys.argv[1:]])\n"
                )
                shim.chmod(0o755)
                argv = [str(MINE.__file__), "--repo", str(repo), "--oracle-map", str(oracle_map),
                        "--beads-json", str(beads), "--output", str(output), "--no-validate"]
                with mock.patch.dict(os.environ, {"PATH": str(shim_dir) + os.pathsep + os.environ.get("PATH", "")}), \
                     mock.patch.object(MINE, "METADATA_TIMEOUT_SECS", 1, create=True), \
                     mock.patch.object(MINE, "MAX_METADATA_BYTES", 65536, create=True), \
                     mock.patch.object(sys, "argv", argv), contextlib.redirect_stderr(io.StringIO()) as diagnostics, \
                     contextlib.redirect_stdout(io.StringIO()) as stdout:
                    tracemalloc.start()
                    try:
                        result = MINE.main()
                        peak = tracemalloc.get_traced_memory()[1]
                    finally:
                        tracemalloc.stop()
                self.assertEqual(result, 2, diagnostics.getvalue())
                self.assertEqual(output.read_text(), "existing task artifact\n")
                self.assertEqual(stdout.getvalue(), "")
                self.assertIn("timed out" if outcome == "timeout" else "exceeded", diagnostics.getvalue())
                self.assertLess(len(diagnostics.getvalue()), 2200)
                self.assertLess(peak, 1024 * 1024)
                with self.assertRaises(ProcessLookupError):
                    os.kill(int(pid_file.read_text()), 0)

    def test_checkout_failures_skip_tasks_and_clean_partial_worktrees(self) -> None:
        for failed_content, locked in [(content, locked) for content in ["broken", "fixed"] for locked in [False, True]]:
            with self.subTest(failed_content=failed_content, locked=locked), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                repo = make_repo(root)
                parent = git(repo, "rev-parse", "HEAD").strip()
                (repo / "value.txt").write_text("fixed\n", encoding="utf-8")
                git(repo, "commit", "-qam", "fix mini-agent-test")
                commit = git(repo, "rev-parse", "HEAD").strip()
                unrelated = root / "unrelated"
                git(repo, "worktree", "add", "--detach", str(unrelated), commit)
                git(repo, "worktree", "lock", "--reason", "preserve this workspace", str(unrelated))
                original_registrations = git(repo, "worktree", "list", "--porcelain")
                hook = repo / ".git" / "hooks" / "post-checkout"
                lock = 'git worktree lock "$PWD"\n' if locked else ""
                hook.write_text("#!/bin/sh\n" + lock + f"! grep -qx {failed_content} value.txt\n", encoding="utf-8")
                hook.chmod(0o755)
                beads = root / "beads.jsonl"
                beads.write_text(json.dumps({"id": "mini-agent-test", "status": "closed"}) + "\n", encoding="utf-8")
                # At the base, even an always-green oracle must not be mined
                # when checkout fails. At the fix, exercise a real base failure
                # followed by an unavailable fix checkout.
                command = "true" if failed_content == "broken" else "grep -qx fixed value.txt"
                tasks, skipped = MINE.mine(repo, {"mini-agent-test": command}, True, 10, "main", beads)
                self.assertEqual(tasks, [])
                self.assertEqual(len(skipped), 1)
                failed_revision = parent if failed_content == "broken" else commit
                self.assertIn(f"worktree add {failed_revision} failed", skipped[0])
                self.assertEqual(git(repo, "worktree", "list", "--porcelain"), original_registrations)
                self.assertEqual((unrelated / "value.txt").read_text(), "fixed\n")
                self.assertEqual((repo / "value.txt").read_text(), "fixed\n")

    def test_miner_oracle_floods_are_bounded_and_worktrees_are_cleaned(self) -> None:
        for outcome in ["success", "failure", "timeout"]:
            with self.subTest(outcome=outcome), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                repo = make_repo(root)
                revision = git(repo, "rev-parse", "HEAD").strip()
                code = (
                    "import os,sys,time\n"
                    "for _ in range(64): os.write(1,b'x'*65536); os.write(2,b'y'*65536)\n"
                    "os.write(2,b'stderr-end\\n')\n"
                    "if sys.argv[1]=='timeout': time.sleep(30)\n"
                    "sys.exit(7 if sys.argv[1]=='failure' else 0)"
                )
                command = "exec " + shlex.join([sys.executable, "-c", code, outcome])
                with mock.patch.object(MINE, "ORACLE_TIMEOUT_SECS", 1 if outcome == "timeout" else 5):
                    with contextlib.redirect_stderr(io.StringIO()) as diagnostic:
                        tracemalloc.start()
                        try:
                            result = MINE.oracle_at(repo, revision, command)
                            peak = tracemalloc.get_traced_memory()[1]
                        finally:
                            tracemalloc.stop()
                self.assertEqual(result, outcome == "success")
                self.assertLess(peak, 1024 * 1024, "miner retained complete oracle output")
                self.assertEqual(len(git(repo, "worktree", "list").splitlines()), 1)
                if outcome == "success":
                    self.assertEqual(diagnostic.getvalue(), "")
                elif outcome == "failure":
                    self.assertIn("oracle exited 7", diagnostic.getvalue())
                    self.assertTrue(diagnostic.getvalue().endswith("stderr-end\n"))
                    self.assertLess(len(diagnostic.getvalue()), 2100)
                else:
                    self.assertIn("oracle timed out after 1s", diagnostic.getvalue())

    def test_miner_blob_limits_bound_memory_and_preserve_complete_text(self) -> None:
        limit = MINE.MAX_BLOB_BYTES
        with tempfile.TemporaryDirectory() as directory:
            repo = make_repo(Path(directory))
            before_files = {
                "exact.txt": b"a" * limit,
                "new_too_large.txt": b"small",
                "old_too_large.txt": b"b" * (limit + 1),
                "large_deleted.txt": b"d" * (4 * 1024 * 1024),
                "large_modified.txt": b"e" * (4 * 1024 * 1024),
                "at_limit_deleted.txt": b"g" * limit,
                "empty_old.txt": b"",
                "line\nname.txt": b"old",
            }
            after_files = {
                "exact.txt": b"z" * limit,
                "new_too_large.txt": b"x" * (limit + 1),
                "old_too_large.txt": b"small",
                "large_modified.txt": b"tiny",
                "large_added.txt": b"a" * (4 * 1024 * 1024),
                "at_limit_added.txt": b"+" * limit,
                "empty_new.txt": b"",
                "empty_old.txt": b"x",
                "line\nname.txt": b"new",
            }
            for name, data in before_files.items():
                (repo / name).write_bytes(data)
            git(repo, "add", "-A")
            git(repo, "commit", "-qm", "before")
            parent = git(repo, "rev-parse", "HEAD").strip()
            for name in before_files.keys() - after_files.keys():
                (repo / name).unlink()
            for name, data in after_files.items():
                (repo / name).write_bytes(data)
            git(repo, "add", "-A")
            git(repo, "commit", "-qm", "after")
            commit = git(repo, "rev-parse", "HEAD").strip()
            tracemalloc.start()
            try:
                before, after, deleted = MINE.changed_text_files(repo, parent, commit)
                peak = tracemalloc.get_traced_memory()[1]
            finally:
                tracemalloc.stop()
            self.assertLess(peak, 4 * 1024 * 1024, "oversized Git blobs were captured before rejection")
            self.assertEqual(before, {
                "exact.txt": "a" * limit,
                "at_limit_deleted.txt": "g" * limit,
                "empty_old.txt": "",
                "line\nname.txt": "old",
            })
            self.assertEqual(after, {
                "exact.txt": "z" * limit,
                "at_limit_added.txt": "+" * limit,
                "empty_new.txt": "",
                "empty_old.txt": "x",
                "line\nname.txt": "new",
            })
            self.assertEqual(deleted, ["at_limit_deleted.txt"])

    def test_miner_blob_read_rejects_ref_growth_after_size_query(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            repo = make_repo(Path(directory))
            small = git(repo, "rev-parse", "HEAD").strip()
            (repo / "value.txt").write_bytes(b"x" * (4 * 1024 * 1024))
            git(repo, "commit", "-qam", "large")
            large = git(repo, "rev-parse", "HEAD").strip()
            git(repo, "update-ref", "refs/heads/probe", small)
            original_run = MINE.run

            def replace_after_size(*args, **kwargs):
                result = original_run(*args, **kwargs)
                self.assertEqual(args[0], ["git", "cat-file", "-s", "probe:value.txt"])
                git(repo, "update-ref", "refs/heads/probe", large)
                return result

            with mock.patch.object(MINE, "run", side_effect=replace_after_size):
                tracemalloc.start()
                try:
                    data = MINE.read_blob(repo, "probe:value.txt")
                    peak = tracemalloc.get_traced_memory()[1]
                finally:
                    tracemalloc.stop()
            self.assertIsNone(data, "a capped prefix must never become oracle text")
            self.assertLess(peak, 1024 * 1024)

    def test_miner_text_delta_preserves_base_types_modes_and_line_endings(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            git(repo, "config", "core.autocrlf", "false")
            (repo / "value.txt").write_bytes(b"broken\r\n" * 512)
            (repo / "gone.txt").write_text("bye\n", encoding="utf-8")
            (repo / "binary.bin").write_bytes(b"\x00\xff\xfe")
            (repo / "modified-link").symlink_to("value.txt")
            (repo / "deleted-link").symlink_to("value.txt")
            (repo / "link-to-file").symlink_to("value.txt")
            (repo / "file-to-link").write_text("ordinary file\n")
            (repo / "executable").write_text("old\n")
            (repo / "executable").chmod(0o755)
            (repo / "mode-only").write_text("same\n")
            (repo / "mode-only").chmod(0o644)
            nested = repo / ".github" / "workflows"
            nested.mkdir(parents=True)
            (nested / "ci.yml").write_text("on: push\n", encoding="utf-8")
            git(repo, "add", "-A")
            git(repo, "commit", "-qm", "setup")
            parent = git(repo, "rev-parse", "HEAD").strip()
            (repo / "value.txt").write_bytes(b"fixed\r\n" * 512)
            (repo / "gone.txt").unlink()
            (repo / "binary.bin").write_bytes(b"\x00\xff\xfd")
            (repo / "modified-link").unlink()
            (repo / "modified-link").symlink_to("elsewhere")
            (repo / "deleted-link").unlink()
            (repo / "added-link").symlink_to("value.txt")
            (repo / "link-to-file").unlink()
            (repo / "link-to-file").write_text("now a file\n")
            (repo / "file-to-link").unlink()
            (repo / "file-to-link").symlink_to("value.txt")
            (repo / "executable").write_text("new\n")
            (repo / "mode-only").chmod(0o755)
            (nested / "ci.yml").write_text("on: pull_request\n", encoding="utf-8")
            git(repo, "add", "-A")
            git(repo, "commit", "-qm", "fix mini-agent-test")
            commit = git(repo, "rev-parse", "HEAD").strip()

            before, after, deleted = MINE.changed_text_files(repo, parent, commit)
            self.assertEqual(before["value.txt"], "broken\r\n" * 512)
            self.assertEqual(after["value.txt"], "fixed\r\n" * 512)
            self.assertEqual(deleted, ["gone.txt"])
            self.assertEqual(before["gone.txt"], "bye\n")
            self.assertNotIn("gone.txt", after)
            self.assertNotIn("binary.bin", after)
            self.assertNotIn(".github/workflows/ci.yml", after)
            for name in ["modified-link", "deleted-link", "added-link", "link-to-file", "file-to-link"]:
                self.assertNotIn(name, before)
                self.assertNotIn(name, after)
                self.assertNotIn(name, deleted)
            self.assertEqual(before["executable"], "old\n")
            self.assertEqual(after["executable"], "new\n")
            self.assertEqual((before["mode-only"], after["mode-only"]), ("same\n", "same\n"))
            workspace = root / "reconstructed"
            TRAIN_MODULE.prepare_workspace(repo, {"base_commit": parent, "initial_files": before,
                                                 "deleted_files": deleted}, workspace, False)
            self.assertEqual(git(workspace, "status", "--porcelain"), "", "mined overlays must preserve the validated base")
            self.assertEqual(os.readlink(workspace / "modified-link"), "value.txt")
            self.assertTrue((workspace / "deleted-link").is_symlink())
            self.assertTrue((workspace / "link-to-file").is_symlink())
            self.assertFalse((workspace / "added-link").is_symlink())
            self.assertEqual((workspace / "file-to-link").read_text(), "ordinary file\n")
            self.assertTrue((workspace / "executable").stat().st_mode & 0o111)
            self.assertFalse((workspace / "mode-only").stat().st_mode & 0o111)

    def test_miner_validates_fail_to_pass_under_an_isolated_environment(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            parent = git(repo, "rev-parse", "HEAD").strip()
            (repo / "value.txt").write_text("fixed\n", encoding="utf-8")
            git(repo, "commit", "-qam", "fix mini-agent-test")
            commit = git(repo, "rev-parse", "HEAD").strip()
            self.assertFalse(MINE.oracle_at(repo, parent, "grep -qx fixed value.txt"))
            self.assertTrue(MINE.oracle_at(repo, commit, "grep -qx fixed value.txt"))
            self.assertTrue(MINE.oracle_at(repo, commit, "test -n \"$MINI_AGENT_GYM\" && test -d \"$ZS_CONFIG_DIR\""))
            self.assertFalse(MINE.oracle_at(repo, commit, "test -n \"${GYM_MUST_NOT_LEAK:-}\""))

    def test_commit_association_prefers_the_oldest_exact_id_on_main(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            bead_id = "mini-agent-test.1"
            for misleading in ["mini-agent-test.10", "mini-agent-testx1", "prefix-mini-agent-test.1",
                               "mini-agent-test.1-suffix", "mini-agent-test.1_child"]:
                (repo / "value.txt").write_text(misleading + "\n", encoding="utf-8")
                git(repo, "commit", "-qam", f"unrelated fix {misleading}")
            (repo / "value.txt").write_text("fixed\n", encoding="utf-8")
            git(repo, "commit", "-qam", f"fix ({bead_id})")
            fix = git(repo, "rev-parse", "HEAD").strip()
            (repo / "value.txt").write_text("fixed again\n", encoding="utf-8")
            git(repo, "commit", "-qam", f"docs: follow-up for {bead_id}")
            git(repo, "checkout", "-q", "-b", "abandoned")
            (repo / "value.txt").write_text("abandoned\n", encoding="utf-8")
            git(repo, "commit", "-qam", f"abandoned {bead_id}")
            abandoned = git(repo, "rev-parse", "HEAD").strip()
            git(repo, "checkout", "-q", "main")

            commit, reason = MINE.resolve_commit(repo, bead_id, {}, "main")
            self.assertEqual(commit, fix)
            self.assertEqual(reason, "")
            explicit, reason = MINE.resolve_commit(repo, bead_id, {"fix_commit": abandoned}, "main")
            self.assertEqual(explicit, abandoned)
            missing, reason = MINE.resolve_commit(repo, "mini-agent-absent", {}, "main")
            self.assertEqual(missing, "")
            self.assertIn("no commit reachable from main", reason)
            bad, reason = MINE.resolve_commit(repo, bead_id, {"fix_commit": "deadbeef"}, "main")
            self.assertEqual(bad, "")
            self.assertIn("not a commit", reason)

    def test_bead_sources_filter_unfinished_issues_and_preserve_stable_order(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            rows = [{"id": "mini-agent-zzz", "status": "closed"}, {"id": "mini-agent-legacy"},
                    *[{"id": f"mini-agent-{status}", "status": status} for status in ["open", "in_progress", "deferred", None]],
                    {"id": "mini-agent-aaa", "status": "closed"}]
            jsonl = "".join(json.dumps(row) + "\n" for row in rows)
            (repo / ".beads").mkdir()
            metadata = repo / ".beads/metadata.json"
            (repo / ".beads/issues.jsonl").write_text(jsonl)
            for source in ["explicit-json", "explicit-jsonl", "bd", "fallback-missing", "fallback-error", "fallback-malformed",
                           "fallback-metadata-list", "fallback-metadata-null", "fallback-metadata-utf8"]:
                with self.subTest(source=source):
                    metadata.write_bytes({"fallback-metadata-list": b"[]", "fallback-metadata-null": b"null",
                                          "fallback-metadata-utf8": b"\xff"}.get(source, b'{"dolt_mode":"embedded"}'))
                    explicit = root / "export" if source.startswith("explicit") else None
                    if explicit is not None:
                        explicit.write_text(json.dumps(rows) if source == "explicit-json" else jsonl)

                    def command(argv, *args, **kwargs):
                        if explicit is not None:
                            self.fail("an explicit export must not invoke bd")
                        if argv[1] == "version":
                            return subprocess.CompletedProcess(argv, 0, b"bd fixture\n", b"")
                        if source == "fallback-missing" or source.startswith("fallback-metadata"):
                            raise FileNotFoundError("bd")
                        if source == "fallback-error":
                            raise subprocess.CalledProcessError(2, argv, stderr=b"tracker unavailable")
                        data = b"not json" if source == "fallback-malformed" else json.dumps(rows).encode()
                        return subprocess.CompletedProcess(argv, 0, data, b"")

                    with mock.patch.object(MINE, "run", side_effect=command), contextlib.redirect_stderr(io.StringIO()) as diagnostics:
                        loaded = MINE.load_beads(repo, explicit)
                    self.assertEqual([row["id"] for row in loaded], ["mini-agent-aaa", "mini-agent-legacy", "mini-agent-zzz"])
                    if source.startswith("fallback"):
                        self.assertIn("falling back to", diagnostics.getvalue())
                        if source.startswith("fallback-metadata"):
                            self.assertIn("dolt_mode: unreadable", diagnostics.getvalue())
                        elif source != "fallback-malformed":
                            self.assertIn("dolt_mode: embedded", diagnostics.getvalue())
                    else:
                        self.assertEqual(diagnostics.getvalue(), "")

    def test_mined_prompt_carries_the_description_and_acceptance_criteria(self) -> None:
        prompt = MINE.mined_prompt(
            {
                "id": "mini-agent-test",
                "title": "make value.txt say fixed",
                "description": "value.txt still reads broken after the import.",
                "acceptance_criteria": "grep -qx fixed value.txt passes.",
            }
        )
        self.assertIn("make value.txt say fixed", prompt)
        self.assertIn("value.txt still reads broken after the import.", prompt)
        self.assertIn("grep -qx fixed value.txt passes.", prompt)
        # A bead with nothing but a title contributes no empty sections.
        bare = MINE.mined_prompt({"id": "mini-agent-test", "title": "just a title"})
        self.assertEqual(bare, "just a title")
        self.assertEqual(MINE.mined_prompt({"id": "mini-agent-test"}), "mini-agent-test")

    def test_miner_cli_records_validation_provenance_in_loadable_tasks(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            (repo / "value.txt").write_text("fixed\n", encoding="utf-8")
            git(repo, "commit", "-qam", "fix mini-agent-test")
            beads = root / "beads.jsonl"
            beads.write_text(
                json.dumps(
                    {
                        "id": "mini-agent-test",
                        "title": "make value.txt say fixed",
                        "description": "The import left value.txt reading broken.",
                        "acceptance_criteria": "grep -qx fixed value.txt passes.",
                        "status": "closed",
                        "labels": ["domain:example", "fail-to-pass", "validation-skipped"],
                    }
                )
                + "\n",
                encoding="utf-8",
            )
            with beads.open("a", encoding="utf-8") as export:
                export.write(json.dumps({"id": "mini-agent-open", "status": "open", "title": "unfinished"}) + "\n")
            for validate, command, count in [(True, "grep -qx fixed value.txt", 1), (False, "true", 1), (True, "true", 0)]:
                with self.subTest(validate=validate, command=command):
                    output = root / "tasks.json"
                    oracle_map = root / "map.json"
                    marker = root / "oracle-ran"
                    oracle_command = "touch " + shlex.quote(str(marker)) + "; " + command
                    oracle_map.write_text(json.dumps({"mini-agent-test": oracle_command,
                                                     "mini-agent-open": {"command": oracle_command, "fix_commit": "HEAD"}}))
                    marker.unlink(missing_ok=True)
                    completed = subprocess.run(
                        [sys.executable, str(ROOT / "scripts/gym/mine_tasks.py"), "--repo", str(repo),
                         "--beads-json", str(beads), "--oracle-map", str(oracle_map), "--output", str(output),
                         *([] if validate else ["--no-validate"])],
                        capture_output=True, text=True, timeout=10,
                    )
                    self.assertEqual(completed.returncode, 0, completed.stderr)
                    self.assertEqual(marker.exists(), validate)
                    self.assertIn(f"mined {count} {'validated' if validate else 'unvalidated'} task(s)", completed.stdout)
                    tasks = json.loads(output.read_text())["tasks"]
                    self.assertEqual(len(tasks), count)
                    if not count:
                        self.assertIn("oracle already passes at base", completed.stderr)
                        continue
                    self.assertEqual(tasks[0]["tags"], ["domain:example", "fail-to-pass", "mined"] if validate
                                     else ["domain:example", "mined", "validation-skipped"])
                    self.assertIn("The import left value.txt reading broken.", tasks[0]["prompt"])
                    self.assertIn("grep -qx fixed value.txt passes.", tasks[0]["prompt"])
                    loaded = TRAIN_MODULE.load_tasks(output, 900)
                    self.assertEqual(loaded[0]["name"], "mini-agent-test")
                    self.assertEqual(loaded[0]["initial_files"]["value.txt"], "broken\n")
                    self.assertEqual(loaded[0]["oracle"]["expected_files"]["value.txt"], "fixed\n")


CARGO_STUB = """#!/bin/sh
set -u
printf '%s\\n' "$*" >> "$CARGO_LOG"
if [ "${1:-}" = install ]; then
  shift
  install_root=
  while [ "$#" -gt 0 ]; do
    if [ "$1" = --root ]; then shift; install_root=$1; fi
    shift
  done
  test -n "$install_root" || exit 2
  mkdir -p "$install_root/bin"
  cat > "$install_root/bin/mini-agent" <<'INNER'
#!/bin/sh
echo "usage: mini-agent [--install-learned-skill-seeds] [--import-learned-skill <path>]"
INNER
  chmod 755 "$install_root/bin/mini-agent"
fi
exit 0
"""

RUSTC_STUB = """#!/bin/sh
echo "rustc 1.90.0 (0000000000 2026-01-01)"
"""

NOOP_STUB = """#!/bin/sh
exit 0
"""


def make_setup_host(root: Path) -> tuple[Path, Path, dict[str, str]]:
    """A fake toolchain plus a repo `setup.sh` will accept.

    Only `cargo`, `rustc` and `jq` are stubbed; `git` and `python3` stay real
    because setup.sh checks their actual versions.
    """
    repo = root / "repo"
    repo.mkdir(parents=True)
    (repo / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "1.90.0"\n', encoding="utf-8")
    binaries = root / "bin"
    binaries.mkdir()
    for name, body in (("cargo", CARGO_STUB), ("rustc", RUSTC_STUB), ("jq", NOOP_STUB)):
        stub = binaries / name
        stub.write_text(body, encoding="utf-8")
        stub.chmod(0o755)
    gym_root = root / "gym"
    env = {
        **os.environ,
        "PATH": f"{binaries}{os.pathsep}{os.environ.get('PATH', '')}",
        "CARGO_LOG": str(root / "cargo.log"),
        "MINI_AGENT_GYM_ROOT": str(gym_root),
    }
    return repo, gym_root, env


class GymEntrypointTests(unittest.TestCase):
    def test_shell_entrypoints_are_syntax_valid(self) -> None:
        for script in (SETUP, ROOT / "scripts/gym/train.sh"):
            subprocess.run(["bash", "-n", str(script)], check=True)

    def test_python_entrypoints_load_shared_capture_outside_repository(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            for script, option in [(TRAIN, "--tasks"), (ROOT / "scripts/gym/mine_tasks.py", "--oracle-map")]:
                with self.subTest(script=script.name):
                    completed = subprocess.run(
                        [sys.executable, str(script), "--help"], cwd=directory,
                        capture_output=True, text=True, timeout=5,
                    )
                    self.assertEqual(completed.returncode, 0, completed.stderr)
                    self.assertIn(option, completed.stdout)

    def test_setup_installs_and_preflights_with_the_same_feature_set(self) -> None:
        # Behavioural: the argv `setup.sh` really hands cargo, not a substring
        # of its own source text.
        with scratch_outside_tmp() as directory:
            root = Path(directory)
            repo, gym_root, env = make_setup_host(root)
            # Resolve a nested relative destination before setup changes cwd;
            # Cargo must install into the same root the directory step created.
            env["MINI_AGENT_GYM_ROOT"] = "nested/gym"
            gym_root = root / "nested/gym"
            completed = subprocess.run(
                ["bash", str(SETUP), str(repo)], cwd=root, capture_output=True, text=True, env=env
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            invocations = (root / "cargo.log").read_text(encoding="utf-8").splitlines()
            self.assertEqual(
                invocations[0],
                f"install --path . --debug --locked --features skills --root {gym_root}",
            )
            self.assertTrue(
                invocations[1].startswith("test --locked --features skills "), invocations[1]
            )
            self.assertIn("_js_worker_containment", invocations[1])
            self.assertIn("--ignored", invocations[1])
            for invocation in invocations:
                self.assertNotIn("--no-default-features", invocation)
            self.assertTrue((gym_root / "bin/mini-agent").is_file())
            self.assertTrue((gym_root / "worktrees").is_dir())
            self.assertTrue((gym_root / "runs").is_dir())
            self.assertIn("gym host ready", completed.stdout)

            # Setup must reject an interpreter without descriptor-relative
            # rmtree before another install or containment test is launched.
            python_stub = root / "bin/python3"
            code = "import sys; sys.version_info=(3,10,0); exec(compile(sys.stdin.read(), '<setup>', 'exec'))"
            python_stub.write_text("#!/bin/sh\nshift\nexec " + shlex.join([sys.executable, "-c", code]) + ' "$@"\n')
            python_stub.chmod(0o755)
            rejected = subprocess.run(["bash", str(SETUP), str(repo)], cwd=root, capture_output=True, text=True, env=env)
            self.assertNotEqual(rejected.returncode, 0)
            self.assertIn("Python 3.11 or newer is required", rejected.stderr)
            self.assertEqual((root / "cargo.log").read_text().splitlines(), invocations)

    def test_setup_refuses_a_repo_that_only_resolves_into_the_temp_root(self) -> None:
        # macOS hands out `/tmp/...`, which resolves to `/private/tmp`; Linux
        # keeps `/tmp`. Either way the unresolved spelling must not get through.
        temp_root = next((path for path in TEMP_ROOTS if Path(path).is_dir()), None)
        if temp_root is None:
            self.skipTest("no system temp root to test against")
        repo = Path(tempfile.mkdtemp(prefix="gym-guard-", dir=temp_root))
        try:
            completed = subprocess.run(
                ["bash", str(SETUP), str(repo)], capture_output=True, text=True, env=os.environ
            )
            self.assertEqual(completed.returncode, 2, completed.stderr)
            self.assertIn("system temp root", completed.stderr)
        finally:
            repo.rmdir()

    def test_setup_refuses_a_gym_root_in_the_temp_root(self) -> None:
        with scratch_outside_tmp() as directory, tempfile.TemporaryDirectory(prefix="gym-guard-", dir="/tmp") as temporary:
            root = Path(directory)
            repo, _, env = make_setup_host(root)
            alias = root / "alias"
            alias.symlink_to(Path(temporary), target_is_directory=True)
            for case, refused in [("direct", Path(temporary) / "direct"), ("symlink", alias / "missing" / "nested"),
                                  ("relative", Path("alias/relative/nested"))]:
                with self.subTest(case=case):
                    env["MINI_AGENT_GYM_ROOT"] = str(refused)
                    completed = subprocess.run(
                        ["bash", str(SETUP), str(repo)], cwd=root, capture_output=True, text=True, env=env
                    )
                    self.assertEqual(completed.returncode, 2, completed.stderr)
                    self.assertIn("system temp root", completed.stderr)
                    self.assertFalse(Path(env["CARGO_LOG"]).exists(), "the guard must run before cargo")
                    self.assertFalse((root / refused).exists(), "the refused gym root must not be created")

    def test_setup_requires_supported_git_versions_with_vendor_suffixes(self) -> None:
        cases = [("git version 2.39.5 (Apple Git-155)", False), ("git version 2.39.9", False),
                 ("git version unknown build-155", False), ("git version 2.40.0", True),
                 ("git version 2.40.1 (Apple Git-155)", True), ("git version 2.40.1.windows.1", True)]
        for version, supported in cases:
            with self.subTest(version=version), scratch_outside_tmp() as directory:
                root = Path(directory)
                repo, _, env = make_setup_host(root)
                stub = root / "bin/git"
                stub.write_text("#!/bin/sh\nprintf '%s\\n' " + shlex.quote(version) + "\n")
                stub.chmod(0o755)
                completed = subprocess.run(["bash", str(SETUP), str(repo)], capture_output=True, text=True, env=env)
                self.assertEqual(completed.returncode == 0, supported, completed.stderr)
                self.assertEqual(Path(env["CARGO_LOG"]).exists(), supported)
                if not supported:
                    self.assertIn(version, completed.stderr)
                    self.assertIn("required 2.40", completed.stderr)


if __name__ == "__main__":
    unittest.main()
