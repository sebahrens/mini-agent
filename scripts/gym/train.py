#!/usr/bin/env python3
"""Run task.json episodes with paired no-library/library arms.

The task file is the versioned `{schema_version, defaults, tasks}` object shared
with the deterministic Rust harness fixture (`tests/harness_eval/task.json`); a
task entry overrides any `defaults` field. Every episode runs in a gym-owned
AppPaths tree with a curated environment, so the operator's config, credentials
and skill database never take part.
"""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import json
import os
import re
import shutil
import sqlite3
import stat
import subprocess
import sys
import time
import uuid
from pathlib import Path

if __package__:
    from .process_capture import OUTPUT_TAIL_BYTES, run_bounded as run
    from .worktrees import WorktreeError, remove_tree, remove_workspace, run_worktree
else:
    from process_capture import OUTPUT_TAIL_BYTES, run_bounded as run
    from worktrees import WorktreeError, remove_tree, remove_workspace, run_worktree

SCHEMA_VERSION = 1
ARMS = ("none", "library")
DEFAULT_TASK_TIMEOUT_SECS = 900
ORACLE_TIMEOUT_SECS = 300
INSTALL_TIMEOUT_SECS = 900
TIMEOUT_EXIT = 124

# Budget keys the runner can actually hand to the binary, and the keys nothing
# enforces today. Unenforced keys are reported in every row so a comparison is
# never read as if a cap had applied.
ENFORCED_BUDGETS = ("max_provider_turns",)
UNENFORCED_BUDGETS = ("max_tool_calls", "max_total_tokens")

# Only these variables reach an episode. Everything else in the operator's
# environment stays out, so a run cannot inherit ambient authority or settings.
BASE_ENV_ALLOWLIST = (
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TZ",
    "SSL_CERT_DIR",
    "SSL_CERT_FILE",
)
PROVIDER_ENV_ALLOWLIST = (
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "OLLAMA_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
    "VLLM_API_KEY",
)
SAFE_NAME = re.compile(r"[^A-Za-z0-9._-]+")


class TaskError(RuntimeError):
    """A task-file problem the operator must fix before any episode runs."""


class EpisodeFailure(RuntimeError):
    """A bounded episode failure that is recorded as a failed row."""

    def __init__(self, reason: str, detail: str = "") -> None:
        super().__init__(detail or reason)
        self.reason = reason
        self.detail = detail


def tail_text(data: bytes | None, limit: int = OUTPUT_TAIL_BYTES) -> str:
    return data[-limit:].decode("utf-8", errors="replace") if data else ""


def elapsed_ms_since(started: float) -> int:
    """Whole milliseconds since a `time.monotonic()` mark."""
    return round((time.monotonic() - started) * 1000)


def relative_path(value: object, label: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise TaskError(f"{label} must be a non-empty relative path")
    candidate = Path(value)
    if not candidate.parts or candidate.is_absolute() or candidate.drive or any(part in ("..", "") for part in candidate.parts):
        raise TaskError(f"{label} must stay inside the workspace: {value!r}")
    return value


def positive_int(value: object, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 1:
        raise TaskError(f"{label} must be an integer of at least 1, not {value!r}")
    return value


def validated_budgets(budgets: object, label: str) -> dict[str, int]:
    if not isinstance(budgets, dict):
        raise TaskError(f"{label}.budgets must be an object")
    checked: dict[str, int] = {}
    for key in ENFORCED_BUDGETS:
        if key not in budgets:
            raise TaskError(f"{label}.budgets.{key} is required")
        checked[key] = positive_int(budgets[key], f"{label}.budgets.{key}")
    for key in UNENFORCED_BUDGETS:
        if key in budgets:
            checked[key] = positive_int(budgets[key], f"{label}.budgets.{key}")
    return checked


def validated_oracle(oracle: object, label: str) -> dict[str, object]:
    if not isinstance(oracle, dict):
        raise TaskError(f"{label}.oracle must be an object")
    command = oracle.get("command")
    expected = oracle.get("expected_files") or {}
    if not isinstance(expected, dict):
        raise TaskError(f"{label}.oracle.expected_files must be an object")
    expected_files = {relative_path(name, f"{label}.oracle.expected_files") : content for name, content in expected.items()}
    if command is not None and (not isinstance(command, str) or not command.strip()):
        raise TaskError(f"{label}.oracle.command must be a non-empty string when present")
    if not command and not expected_files:
        raise TaskError(f"{label}.oracle needs either 'command' or a non-empty 'expected_files'")
    for name, content in expected_files.items():
        if not isinstance(content, str):
            raise TaskError(f"{label}.oracle.expected_files[{name!r}] must be a string")
    identifier = oracle.get("id")
    if identifier is None:
        seed = command if command else json.dumps(expected_files, sort_keys=True)
        identifier = hashlib.sha256(seed.encode("utf-8")).hexdigest()
    return {"command": command, "expected_files": expected_files, "id": str(identifier)}


def load_tasks(path: Path, default_timeout: int) -> list[dict[str, object]]:
    document = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(document, dict):
        raise TaskError(
            f"{path}: expected the versioned {{defaults, tasks}} object; "
            "regenerate legacy task arrays with scripts/gym/mine_tasks.py"
        )
    version = document.get("schema_version", SCHEMA_VERSION)
    if version != SCHEMA_VERSION:
        raise TaskError(f"{path}: unsupported schema_version {version!r} (this runner speaks {SCHEMA_VERSION})")
    defaults = document.get("defaults", {})
    entries = document.get("tasks")
    if not isinstance(defaults, dict):
        raise TaskError(f"{path}: 'defaults' must be an object")
    if not isinstance(entries, list) or not entries:
        raise TaskError(f"{path}: 'tasks' must be a non-empty array")
    tasks: list[dict[str, object]] = []
    seen: set[str] = set()
    for index, entry in enumerate(entries):
        if not isinstance(entry, dict):
            raise TaskError(f"{path}: tasks[{index}] must be an object")
        merged = {**defaults, **entry}
        label = f"{path}: tasks[{index}]"
        name = merged.get("name")
        if not isinstance(name, str) or not name.strip():
            raise TaskError(f"{label} needs a non-empty 'name'")
        if name in seen:
            raise TaskError(f"{path}: duplicate task name {name!r}")
        seen.add(name)
        label = f"{path}: task {name!r}"
        prompt = merged.get("prompt")
        if not isinstance(prompt, str) or not prompt.strip():
            raise TaskError(f"{label} needs a non-empty 'prompt'")
        initial = merged.get("initial_files") or {}
        if not isinstance(initial, dict):
            raise TaskError(f"{label}.initial_files must be an object")
        deleted = merged.get("deleted_files") or []
        if not isinstance(deleted, list):
            raise TaskError(f"{label}.deleted_files must be an array")
        tasks.append(
            {
                "name": name,
                "prompt": prompt,
                "base_commit": str(merged.get("base_commit") or "HEAD"),
                "initial_files": {
                    relative_path(key, f"{label}.initial_files"): str(value) for key, value in initial.items()
                },
                "deleted_files": [relative_path(value, f"{label}.deleted_files") for value in deleted],
                "oracle": validated_oracle(merged.get("oracle"), label),
                "budgets": validated_budgets(merged.get("budgets"), label),
                "library": str(merged.get("library") or "seeds"),
                "timeout_secs": positive_int(merged.get("timeout_secs", default_timeout), f"{label}.timeout_secs"),
                "tags": list(merged.get("tags") or []),
            }
        )
    return tasks


def episode_env(root: Path, provider: str | None, model: str | None, forwarded: list[str]) -> dict[str, str]:
    """Build the curated environment and gym-owned config/credentials tree."""
    names = list(BASE_ENV_ALLOWLIST) + list(PROVIDER_ENV_ALLOWLIST) + list(forwarded)
    env = {name: os.environ[name] for name in names if name in os.environ}
    for directory in ("data", "local", "state", "cache", "config", "credentials", "neutral", "tmp"):
        (root / directory).mkdir(parents=True, exist_ok=True)
    settings = ["# Generated by scripts/gym/train.py; the operator's config.toml is never read.\n"]
    for key, value in (("provider", provider), ("model", model)):
        if value:
            # JSON escaping matches TOML basic strings except that TOML also
            # forbids a literal DEL. Keep non-BMP Unicode out of surrogate escapes.
            quoted = json.dumps(value, ensure_ascii=False).replace("\x7f", r"\u007f")
            settings.append(f"{key} = {quoted}\n")
    (root / "config" / "config.toml").write_text("".join(settings), encoding="utf-8")
    env.update(
        {
            "MINI_AGENT_GYM": "1",
            "ZS_CONFIG_DIR": str(root / "config"),
            "ZS_CREDENTIALS_DIR": str(root / "credentials"),
            "ZS_DATA_DIR": str(root / "data"),
            "ZS_LOCAL_DATA_DIR": str(root / "local"),
            "ZS_STATE_DIR": str(root / "state"),
            "ZS_CACHE_DIR": str(root / "cache"),
            "TMPDIR": str(root / "tmp"),
        }
    )
    return env


def skills_database(env: dict[str, str]) -> Path:
    databases = sorted(Path(env["ZS_LOCAL_DATA_DIR"]).rglob("skills.db"))
    if len(databases) != 1:
        raise EpisodeFailure(
            "library_install_failed",
            f"library import produced {len(databases)} skills.db files, expected exactly one",
        )
    return databases[0]


def query(database: Path, sql: str) -> list[str]:
    with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as db:
        return [str(row[0]) for row in db.execute(sql)]


def install_library(binary: str, library: str, env: dict[str, str]) -> list[str]:
    """Import a library, approve/activate its lineage roots, return active ids.

    Runs from a gym-owned neutral directory so no project-local config.toml in a
    checked-out workspace can influence the import.
    """
    neutral = Path(env["ZS_CONFIG_DIR"]).parent / "neutral"
    command = [binary, "--install-learned-skill-seeds"] if library == "seeds" else [binary, "--import-learned-skill", library]
    try:
        result = run(command, neutral, env, INSTALL_TIMEOUT_SECS)
    except subprocess.TimeoutExpired:
        raise EpisodeFailure("library_install_failed", f"{library} import timed out after {INSTALL_TIMEOUT_SECS}s") from None
    if result.returncode:
        raise EpisodeFailure("library_install_failed", tail_text(result.stderr) or tail_text(result.stdout))
    database = skills_database(env)
    # Replacement proposals cannot be activated standalone, so only lineage
    # roots are approved; anything else is left alone deliberately.
    roots = query(
        database,
        "SELECT skill_id FROM skill_proposals "
        "WHERE status = 'awaiting_approval' AND predecessor_id IS NULL ORDER BY proposed_at, skill_id",
    )
    for skill_id in roots:
        for flag in ("--approve-learned-skill", "--activate-learned-skill"):
            try:
                result = run([binary, flag, skill_id], neutral, env, INSTALL_TIMEOUT_SECS)
            except subprocess.TimeoutExpired:
                raise EpisodeFailure("library_install_failed", f"{flag} {skill_id} timed out") from None
            if result.returncode:
                raise EpisodeFailure("library_install_failed", f"{flag} {skill_id}: {tail_text(result.stderr)}")
    active = query(database, "SELECT id FROM skill_revisions WHERE status = 'active' ORDER BY id")
    if not active:
        raise EpisodeFailure(
            "library_install_failed",
            f"library {library!r} left no active skill revision; the arm would be labelled 'library' with nothing installed",
        )
    return active


@contextlib.contextmanager
def workspace_parent(root_fd: int, relative: str, *, create: bool):
    """Walk below the checkout without following any directory symlinks."""
    parts = Path(relative_path(relative, "workspace overlay")).parts
    descriptor = os.dup(root_fd)
    try:
        for part in parts[:-1]:
            if create:
                try:
                    os.mkdir(part, dir_fd=descriptor)
                except FileExistsError:
                    pass
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = child
        yield descriptor, parts[-1]
    finally:
        os.close(descriptor)


def publish_workspace_file(parent_fd: int, name: str, content: str) -> None:
    """Replace the entry, preserving regular-file mode without writing through links."""
    mode = None
    try:
        previous = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
        if stat.S_ISREG(previous.st_mode):
            mode = stat.S_IMODE(previous.st_mode) & 0o777
    except FileNotFoundError:
        pass
    temporary = f".gym-overlay-{uuid.uuid4().hex}"
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o666, dir_fd=parent_fd)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8", newline="") as handle:
            handle.write(content)
            if mode is not None:
                os.fchmod(handle.fileno(), mode)
        os.replace(temporary, name, src_dir_fd=parent_fd, dst_dir_fd=parent_fd)
    finally:
        try:
            os.unlink(temporary, dir_fd=parent_fd)
        except FileNotFoundError:
            pass


def prepare_workspace(repo: Path, task: dict[str, object], destination: Path, allow_empty: bool) -> None:
    base = str(task["base_commit"])
    try:
        result = run_worktree(repo, "add", "--detach", str(destination), base)
    except WorktreeError as error:
        raise EpisodeFailure("workspace_unavailable", str(error)) from error
    if result.returncode:
        if not allow_empty:
            raise EpisodeFailure(
                "workspace_unavailable",
                f"git worktree add {base} failed: {tail_text(result.stderr) or tail_text(result.stdout)}",
            )
    try:
        if result.returncode:
            # A failed checkout hook can leave a complete, registered checkout.
            # An empty fallback starts fresh or fails before applying overlays.
            remove_workspace(repo, destination)
            destination.mkdir(parents=True)
        root_fd = os.open(destination, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            for relative in list(task["deleted_files"]):  # type: ignore[arg-type]
                try:
                    with workspace_parent(root_fd, str(relative), create=False) as (parent_fd, name):
                        entry = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
                        if stat.S_ISDIR(entry.st_mode):
                            shutil.rmtree(name, dir_fd=parent_fd)
                        else:
                            os.unlink(name, dir_fd=parent_fd)
                except FileNotFoundError:
                    pass
            for relative, content in dict(task["initial_files"]).items():  # type: ignore[arg-type]
                with workspace_parent(root_fd, relative, create=True) as (parent_fd, name):
                    publish_workspace_file(parent_fd, name, content)
        finally:
            os.close(root_fd)
    except (OSError, TaskError) as error:
        raise EpisodeFailure("workspace_unavailable", f"workspace preparation failed: {error}") from error


def open_regular_oracle_file(path: str, flags: int) -> int:
    """Open without waiting on a FIFO, then validate the descriptor we read."""
    descriptor = os.open(path, flags | getattr(os, "O_NONBLOCK", 0))
    try:
        if not stat.S_ISREG(os.fstat(descriptor).st_mode):
            raise OSError("oracle output is not a regular file")
    except BaseException:
        os.close(descriptor)
        raise
    return descriptor


def run_oracle(oracle: dict[str, object], workspace: Path, env: dict[str, str]) -> tuple[int, str]:
    command = oracle.get("command")
    if command:
        try:
            result = run(["/bin/sh", "-c", str(command)], workspace, env, ORACLE_TIMEOUT_SECS)
        except subprocess.TimeoutExpired:
            return TIMEOUT_EXIT, f"oracle timed out after {ORACLE_TIMEOUT_SECS}s"
        return result.returncode, tail_text(result.stderr)
    mismatches: list[str] = []
    for relative, expected in dict(oracle["expected_files"]).items():  # type: ignore[arg-type]
        target = workspace / relative
        try:
            with open(target, "r", encoding="utf-8", newline="", opener=open_regular_oracle_file) as handle:
                # One extra character distinguishes an exact match from a
                # matching prefix, without retaining an agent-sized output.
                actual = handle.read(len(expected) + 1)
        except (OSError, UnicodeDecodeError):
            mismatches.append(f"unreadable: {relative}")
            continue
        if actual != expected:
            mismatches.append(f"differs: {relative}")
    return (1 if mismatches else 0), "; ".join(mismatches)


def permission_mode(agent_args: list[str]) -> str:
    return "yolo" if "--yolo" in agent_args else "standard"


def cleanup_episode(repo: Path, workspace: Path, root: Path, keep_run_dirs: bool) -> None:
    """Attempt both independent cleanups, retaining all filesystem/admin failures."""
    errors: list[OSError] = []
    try:
        remove_workspace(repo, workspace)
    except OSError as error:
        errors.append(error)
    if not keep_run_dirs:
        try:
            remove_tree(root)
        except OSError as error:
            errors.append(OSError(f"AppPaths cleanup failed: {error}"))
    if errors:
        raise OSError("; ".join(str(error) for error in errors)) from errors[0]


def run_episode(task: dict[str, object], arm: str, args: argparse.Namespace, repo: Path, gym_root: Path) -> dict[str, object]:
    name = str(task["name"])
    budgets = dict(task["budgets"])  # type: ignore[arg-type]
    oracle = dict(task["oracle"])  # type: ignore[arg-type]
    slug = SAFE_NAME.sub("-", f"{name}-{arm}")
    record: dict[str, object] = {
        "task": name,
        "arm": arm,
        "success": False,
        "production": False,
        "schema_version": SCHEMA_VERSION,
        "oracle_id": oracle["id"],
        # Three separate clocks. `elapsed_ms` is the agent's own wall clock,
        # because that is the number operators compare across arms; folding the
        # oracle (and, in the library arm, the seed import) into it would make
        # the library arm look slower for work the agent never did.
        "elapsed_ms": 0,
        "oracle_ms": 0,
        "total_ms": 0,
        "agent_exit": None,
        "oracle_exit": None,
        "oracle_pre_exit": None,
        "failure_reason": None,
        "failure_detail": "",
        "agent_stderr_tail": "",
        "permission_mode": permission_mode(args.agent_arg),
        "agent_args": list(args.agent_arg),
        "provider": args.provider,
        "model": args.model,
        "active_skill_ids": [],
        "timeout_secs": task["timeout_secs"],
        "budgets_enforced": {"max_agent_turns": budgets["max_provider_turns"]},
        "budgets_unenforced": sorted(key for key in UNENFORCED_BUDGETS if key in budgets),
    }
    workspace = gym_root / "worktrees" / slug
    root = gym_root / "runs" / slug
    cleanup_episode(repo, workspace, root, keep_run_dirs=False)
    # Refuse reuse if another entry appears after cleanup. Child directories
    # may use exist_ok only once this episode owns a freshly created root.
    root.mkdir(parents=True)
    started = time.monotonic()
    oracle_ms = 0
    try:
        prepare_workspace(repo, task, workspace, args.allow_empty_workspace)
        env = episode_env(root, args.provider, args.model, args.forward_env)
        if arm == "library":
            record["active_skill_ids"] = install_library(args.binary, str(task["library"]), env)
        oracle_started = time.monotonic()
        pre_exit, pre_detail = run_oracle(oracle, workspace, env)
        oracle_ms += elapsed_ms_since(oracle_started)
        record["oracle_pre_exit"] = pre_exit
        if pre_exit == 0:
            raise EpisodeFailure("task_invalid_oracle_passes_before_agent", pre_detail)
        argv = [args.binary, "--max-agent-turns", str(budgets["max_provider_turns"]), *args.agent_arg, "-p", str(task["prompt"])]
        agent_started = time.monotonic()
        try:
            agent = run(argv, workspace, env, int(task["timeout_secs"]))
        except subprocess.TimeoutExpired as expired:
            record["elapsed_ms"] = elapsed_ms_since(agent_started)
            record["agent_exit"] = TIMEOUT_EXIT
            record["agent_stderr_tail"] = tail_text(expired.stderr)
            raise EpisodeFailure("agent_timeout", f"agent exceeded {task['timeout_secs']}s") from None
        record["elapsed_ms"] = elapsed_ms_since(agent_started)
        record["agent_exit"] = agent.returncode
        record["agent_stderr_tail"] = tail_text(agent.stderr)
        oracle_started = time.monotonic()
        post_exit, post_detail = run_oracle(oracle, workspace, env)
        oracle_ms += elapsed_ms_since(oracle_started)
        record["oracle_exit"] = post_exit
        if agent.returncode:
            record["failure_reason"] = "agent_exit_nonzero"
            record["failure_detail"] = record["agent_stderr_tail"]
        elif post_exit:
            record["failure_reason"] = "oracle_failed"
            record["failure_detail"] = post_detail
        else:
            record["success"] = True
    except EpisodeFailure as failure:
        record["failure_reason"] = failure.reason
        record["failure_detail"] = failure.detail
    finally:
        cleanup_episode(repo, workspace, root, args.keep_run_dirs)
        record["oracle_ms"] = oracle_ms
        # Everything the episode cost, so the library arm's seed-import and
        # workspace setup overhead stays visible instead of hiding inside the
        # agent's number.
        record["total_ms"] = elapsed_ms_since(started)
    return record


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tasks", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--binary", default="mini-agent", help="absolute path to a binary built with --features skills")
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--gym-root", type=Path, help="gym-owned root (default: $MINI_AGENT_GYM_ROOT or <repo>/.gym)")
    parser.add_argument(
        "--agent-arg",
        action="append",
        default=[],
        help="extra argument for every agent invocation, e.g. --agent-arg=--yolo (repeatable)",
    )
    parser.add_argument("--forward-env", action="append", default=[], help="extra environment variable to forward (repeatable)")
    parser.add_argument("--provider", help="provider written into the gym-owned config.toml")
    parser.add_argument("--model", help="model written into the gym-owned config.toml")
    parser.add_argument(
        "--task-timeout",
        type=int,
        default=DEFAULT_TASK_TIMEOUT_SECS,
        help=f"per-episode agent wall-clock bound in seconds (default {DEFAULT_TASK_TIMEOUT_SECS})",
    )
    parser.add_argument("--allow-empty-workspace", action="store_true", help="use an empty directory when the worktree cannot be created")
    parser.add_argument("--keep-run-dirs", action="store_true", help="keep each episode's AppPaths tree for debugging")
    args = parser.parse_args()
    repo = args.repo.resolve()
    gym_root = (args.gym_root or Path(os.environ.get("MINI_AGENT_GYM_ROOT") or repo / ".gym")).resolve()
    try:
        if args.task_timeout < 1:
            raise TaskError("--task-timeout must be at least 1 second")
        tasks = load_tasks(args.tasks, args.task_timeout)
        (gym_root / "worktrees").mkdir(parents=True, exist_ok=True)
        (gym_root / "runs").mkdir(parents=True, exist_ok=True)
        args.output.parent.mkdir(parents=True, exist_ok=True)
    except (TaskError, OSError, json.JSONDecodeError) as error:
        print(f"gym train: {error}", file=sys.stderr)
        return 2
    summary = {arm: {"passed": 0, "failed": 0} for arm in ARMS}
    try:
        with args.output.open("w", encoding="utf-8") as handle:
            for task in tasks:
                for arm in ARMS:
                    record = run_episode(task, arm, args, repo, gym_root)
                    handle.write(json.dumps(record, sort_keys=True) + "\n")
                    handle.flush()
                    print("GYM_OUTCOME " + json.dumps(record, sort_keys=True))
                    summary[arm]["passed" if record["success"] else "failed"] += 1
    except OSError as error:
        print(f"gym train: {error}", file=sys.stderr)
        return 2
    print("GYM_SUMMARY " + json.dumps({"tasks": len(tasks), "arms": summary}, sort_keys=True))
    for arm in ARMS:
        counts = summary[arm]
        print(f"gym arm {arm}: {counts['passed']} passed, {counts['failed']} failed of {len(tasks)}")
    # A completed run exits 0 whatever the rows say: the none arm is expected to
    # fail the tasks the library helps with. Non-zero means the runner failed.
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
