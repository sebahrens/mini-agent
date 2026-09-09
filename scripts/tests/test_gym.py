from __future__ import annotations

import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import tracemalloc
import unittest
from pathlib import Path


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


def load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


MINE = load("gym_mine_tasks", ROOT / "scripts/gym/mine_tasks.py")
TRAIN_MODULE = load("gym_train", TRAIN)

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
  timeout)
    sleep 30
    exit 0
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


def run_training(root: Path, repo: Path, binary: Path, document: dict[str, object], *extra: str):
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
        env={**os.environ, "MINI_AGENT_GYM_ROOT": str(gym_root)},
    )
    rows = [json.loads(line) for line in output.read_text(encoding="utf-8").splitlines()] if output.is_file() else []
    return completed, rows, gym_root


class GymFileOracleTests(unittest.TestCase):
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
    def test_successful_run_records_rows_and_cleans_up(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
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
            completed, rows, gym_root = run_training(
                root, repo, binary, task_document([{"name": "fix", "tags": []}]), "--keep-run-dirs"
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
            self.assertTrue(sorted((gym_root / "runs").iterdir()))

    def test_elapsed_ms_times_the_agent_without_the_oracle(self) -> None:
        # A one-second oracle runs twice per episode (before and after the
        # agent). Folding that into elapsed_ms would make the number operators
        # compare across arms mostly oracle time.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            binary, _ = make_stub(root, "success")
            document = task_document([{"name": "fix", "tags": []}])
            document["defaults"]["oracle"] = {
                "command": "sleep 1; test -f fixed.txt",
                "id": "slow-oracle",
            }
            completed, rows, _ = run_training(root, repo, binary, document)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            for row in rows:
                self.assertTrue(row["success"], row)
                self.assertGreaterEqual(row["oracle_ms"], 1900, row)
                self.assertLess(row["elapsed_ms"], 900, row)
                self.assertGreaterEqual(
                    row["total_ms"], row["elapsed_ms"] + row["oracle_ms"], row
                )

    def test_agent_timeout_records_the_agent_clock_without_the_oracle(self) -> None:
        # The pre-agent oracle also takes a second here, so an elapsed_ms that
        # still spanned the whole episode would be about twice the timeout.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            binary, _ = make_stub(root, "timeout")
            document = task_document([{"name": "slow", "tags": []}])
            document["defaults"]["oracle"] = {
                "command": "sleep 1; test -f fixed.txt",
                "id": "slow-oracle",
            }
            completed, rows, _ = run_training(
                root, repo, binary, document, "--task-timeout", "1"
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            for row in rows:
                self.assertEqual(row["failure_reason"], "agent_timeout")
                self.assertGreaterEqual(row["elapsed_ms"], 900, row)
                self.assertLess(row["elapsed_ms"], 1900, row)
                self.assertGreaterEqual(row["oracle_ms"], 900, row)

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

    def test_agent_timeout_is_recorded_and_the_worktree_is_removed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            binary, _ = make_stub(root, "timeout")
            completed, rows, gym_root = run_training(
                root, repo, binary, task_document([{"name": "slow", "tags": []}]), "--task-timeout", "1"
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            for row in rows:
                self.assertFalse(row["success"])
                self.assertEqual(row["failure_reason"], "agent_timeout")
                self.assertEqual(row["agent_exit"], 124)
                self.assertEqual(row["timeout_secs"], 1)
            self.assertEqual(git(repo, "worktree", "list").strip().count("\n"), 0)
            self.assertEqual(sorted((gym_root / "worktrees").iterdir()), [])

    def test_library_install_failure_only_fails_the_library_arm(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            binary, _ = make_stub(root, "install_fail")
            completed, rows, _ = run_training(root, repo, binary, task_document([{"name": "fix", "tags": []}]))
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertTrue(rows[0]["success"], rows[0])
            self.assertFalse(rows[1]["success"])
            self.assertEqual(rows[1]["failure_reason"], "library_install_failed")
            self.assertIn("seed import exploded", rows[1]["failure_detail"])
            self.assertIsNone(rows[1]["agent_exit"])

    def test_unreachable_base_commit_is_a_failed_row_not_an_empty_workspace(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            binary, _ = make_stub(root, "success")
            document = task_document([{"name": "fix", "tags": [], "base_commit": "0" * 40}])
            completed, rows, _ = run_training(root, repo, binary, document)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            for row in rows:
                self.assertFalse(row["success"])
                self.assertEqual(row["failure_reason"], "workspace_unavailable")
                self.assertIn("git worktree add", row["failure_detail"])

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
            binary, _ = make_stub(root, "success")
            document = task_document([{"name": "delete", "tags": [], "deleted_files": ["value.txt"]}])
            document["defaults"]["oracle"] = {"command": "test ! -e value.txt && test -f fixed.txt", "id": "deleted"}
            completed, rows, _ = run_training(root, repo, binary, document)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            for row in rows:
                self.assertTrue(row["success"], row)

    def test_invalid_budget_fails_at_load_before_any_episode(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            binary, log = make_stub(root, "success")
            document = task_document([{"name": "fix", "tags": []}])
            document["defaults"]["budgets"]["max_provider_turns"] = 0
            completed, rows, _ = run_training(root, repo, binary, document)
            self.assertEqual(completed.returncode, 2)
            self.assertIn("max_provider_turns", completed.stderr)
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
    def test_miner_preserves_crlf_and_reports_deletions(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            git(repo, "config", "core.autocrlf", "false")
            (repo / "value.txt").write_bytes(b"broken\r\n")
            (repo / "gone.txt").write_text("bye\n", encoding="utf-8")
            (repo / "binary.bin").write_bytes(b"\x00\xff\xfe")
            nested = repo / ".github" / "workflows"
            nested.mkdir(parents=True)
            (nested / "ci.yml").write_text("on: push\n", encoding="utf-8")
            git(repo, "add", "-A")
            git(repo, "commit", "-qm", "setup")
            parent = git(repo, "rev-parse", "HEAD").strip()
            (repo / "value.txt").write_bytes(b"fixed\r\n")
            (repo / "gone.txt").unlink()
            (repo / "binary.bin").write_bytes(b"\x00\xff\xfd")
            (nested / "ci.yml").write_text("on: pull_request\n", encoding="utf-8")
            git(repo, "add", "-A")
            git(repo, "commit", "-qm", "fix mini-agent-test")
            commit = git(repo, "rev-parse", "HEAD").strip()

            before, after, deleted = MINE.changed_text_files(repo, parent, commit)
            self.assertEqual(before["value.txt"], "broken\r\n")
            self.assertEqual(after["value.txt"], "fixed\r\n")
            self.assertEqual(deleted, ["gone.txt"])
            self.assertEqual(before["gone.txt"], "bye\n")
            self.assertNotIn("gone.txt", after)
            self.assertNotIn("binary.bin", after)
            self.assertNotIn(".github/workflows/ci.yml", after)

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

    def test_commit_association_prefers_the_oldest_match_on_main(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            (repo / "value.txt").write_text("fixed\n", encoding="utf-8")
            git(repo, "commit", "-qam", "fix mini-agent-test")
            fix = git(repo, "rev-parse", "HEAD").strip()
            (repo / "value.txt").write_text("fixed again\n", encoding="utf-8")
            git(repo, "commit", "-qam", "docs: follow-up for mini-agent-test")
            git(repo, "checkout", "-q", "-b", "abandoned")
            (repo / "value.txt").write_text("abandoned\n", encoding="utf-8")
            git(repo, "commit", "-qam", "abandoned mini-agent-test")
            abandoned = git(repo, "rev-parse", "HEAD").strip()
            git(repo, "checkout", "-q", "main")

            commit, reason = MINE.resolve_commit(repo, "mini-agent-test", {}, "main")
            self.assertEqual(commit, fix)
            self.assertEqual(reason, "")
            explicit, reason = MINE.resolve_commit(repo, "mini-agent-test", {"fix_commit": abandoned}, "main")
            self.assertEqual(explicit, abandoned)
            missing, reason = MINE.resolve_commit(repo, "mini-agent-absent", {}, "main")
            self.assertEqual(missing, "")
            self.assertIn("no commit reachable from main", reason)
            bad, reason = MINE.resolve_commit(repo, "mini-agent-test", {"fix_commit": "deadbeef"}, "main")
            self.assertEqual(bad, "")
            self.assertIn("not a commit", reason)

    def test_beads_fall_back_to_the_exported_jsonl(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            beads = root / "beads.jsonl"
            beads.write_text(
                json.dumps({"id": "mini-agent-test", "title": "fix it", "status": "closed"}) + "\n",
                encoding="utf-8",
            )
            loaded = MINE.load_beads(repo, beads)
            self.assertEqual(loaded[0]["id"], "mini-agent-test")
            self.assertIn("dolt_mode", MINE.beads_hint(repo))

    def test_beads_are_returned_in_a_stable_id_order(self) -> None:
        # bd list documents no ordering, so --limit would otherwise select a
        # different subset from run to run.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            beads = root / "beads.jsonl"
            unordered = ["mini-agent-zzz", "mini-agent-aaa", "mini-agent-mmm"]
            beads.write_text(
                "".join(
                    json.dumps({"id": name, "title": name, "status": "closed"}) + "\n"
                    for name in unordered
                ),
                encoding="utf-8",
            )
            loaded = MINE.load_beads(repo, beads)
            self.assertEqual([bead["id"] for bead in loaded], sorted(unordered))

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

    def test_mined_tasks_use_the_composed_prompt(self) -> None:
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
                    }
                )
                + "\n",
                encoding="utf-8",
            )
            tasks, skipped = MINE.mine(
                repo, {"mini-agent-test": "grep -qx fixed value.txt"}, True, 10, "main", beads
            )
            self.assertEqual(skipped, [])
            prompt = str(tasks[0]["prompt"])
            self.assertIn("The import left value.txt reading broken.", prompt)
            self.assertIn("grep -qx fixed value.txt passes.", prompt)

    def test_miner_emits_a_loadable_document_for_a_real_fix_commit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = make_repo(root)
            (repo / "value.txt").write_text("fixed\n", encoding="utf-8")
            git(repo, "commit", "-qam", "fix mini-agent-test")
            beads = root / "beads.jsonl"
            beads.write_text(
                json.dumps({"id": "mini-agent-test", "title": "make value.txt say fixed", "status": "closed"}) + "\n",
                encoding="utf-8",
            )
            tasks, skipped = MINE.mine(
                repo, {"mini-agent-test": "grep -qx fixed value.txt"}, True, 10, "main", beads
            )
            self.assertEqual(skipped, [])
            self.assertEqual(len(tasks), 1)
            output = root / "tasks.json"
            output.write_text(json.dumps(MINE.document(tasks)), encoding="utf-8")
            loaded = TRAIN_MODULE.load_tasks(output, 900)
            self.assertEqual(loaded[0]["name"], "mini-agent-test")
            self.assertEqual(loaded[0]["initial_files"]["value.txt"], "broken\n")
            self.assertEqual(loaded[0]["oracle"]["expected_files"]["value.txt"], "fixed\n")


CARGO_STUB = """#!/bin/sh
set -u
printf '%s\\n' "$*" >> "$CARGO_LOG"
if [ "${1:-}" = install ]; then
  mkdir -p "$MINI_AGENT_GYM_ROOT/bin"
  cat > "$MINI_AGENT_GYM_ROOT/bin/mini-agent" <<'INNER'
#!/bin/sh
echo "usage: mini-agent [--install-learned-skill-seeds] [--import-learned-skill <path>]"
INNER
  chmod 755 "$MINI_AGENT_GYM_ROOT/bin/mini-agent"
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

    def test_setup_installs_and_preflights_with_the_same_feature_set(self) -> None:
        # Behavioural: the argv `setup.sh` really hands cargo, not a substring
        # of its own source text.
        with scratch_outside_tmp() as directory:
            root = Path(directory)
            repo, gym_root, env = make_setup_host(root)
            completed = subprocess.run(
                ["bash", str(SETUP), str(repo)], capture_output=True, text=True, env=env
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
        with scratch_outside_tmp() as directory:
            root = Path(directory)
            repo, _, env = make_setup_host(root)
            refused = Path(f"/tmp/mini-agent-gym-guard-{os.getpid()}")
            env["MINI_AGENT_GYM_ROOT"] = str(refused)
            completed = subprocess.run(
                ["bash", str(SETUP), str(repo)], capture_output=True, text=True, env=env
            )
            self.assertEqual(completed.returncode, 2, completed.stderr)
            self.assertIn("system temp root", completed.stderr)
            self.assertFalse(Path(env["CARGO_LOG"]).exists(), "the guard must run before cargo")
            self.assertFalse(refused.exists(), "the refused gym root must not be created")


if __name__ == "__main__":
    unittest.main()
