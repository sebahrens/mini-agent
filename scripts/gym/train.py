#!/usr/bin/env python3
"""Run task.json episodes with paired no-library/library arms."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sqlite3
import subprocess
import tempfile
import time
from pathlib import Path


def run(argv: list[str], cwd: Path, env: dict[str, str], timeout: int = 900) -> subprocess.CompletedProcess[str]:
    return subprocess.run(argv, cwd=cwd, env=env, text=True, capture_output=True, timeout=timeout)


def install_library(binary: str, library: str, cwd: Path, env: dict[str, str]) -> None:
    command = [binary, "--install-learned-skill-seeds"] if library == "seeds" else [binary, "--import-learned-skill", library]
    result = run(command, cwd, env)
    if result.returncode:
        raise RuntimeError(result.stderr or result.stdout)
    databases = list(Path(env["ZS_LOCAL_DATA_DIR"]).rglob("skills.db"))
    if len(databases) != 1:
        raise RuntimeError("library import did not produce exactly one skills.db")
    with sqlite3.connect(databases[0]) as db:
        ids = [row[0] for row in db.execute("SELECT skill_id FROM skill_proposals WHERE status = 'awaiting_approval'")]
    for skill_id in ids:
        for flag in ("--approve-learned-skill", "--activate-learned-skill"):
            result = run([binary, flag, skill_id], cwd, env)
            if result.returncode:
                raise RuntimeError(result.stderr or result.stdout)


def prepare_workspace(repo: Path, task: dict[str, object], destination: Path) -> None:
    base = str(task.get("base_commit") or "HEAD")
    result = subprocess.run(["git", "worktree", "add", "--detach", str(destination), base], cwd=repo, text=True, capture_output=True)
    if result.returncode:
        destination.mkdir(parents=True)
    for relative, content in dict(task.get("initial_files") or {}).items():
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8", newline="")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--tasks", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--binary", default="mini-agent")
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--prepare-library")
    parser.add_argument("--state-root", type=Path)
    args = parser.parse_args()
    if args.prepare_library:
        if args.state_root is None:
            parser.error("--prepare-library requires --state-root")
        root = args.state_root.resolve()
        env = os.environ.copy()
        env.update({
            "MINI_AGENT_GYM": "1", "ZS_DATA_DIR": str(root / "data"),
            "ZS_LOCAL_DATA_DIR": str(root / "local"), "ZS_STATE_DIR": str(root / "state"),
            "ZS_CACHE_DIR": str(root / "cache"),
        })
        install_library(args.binary, args.prepare_library, args.repo.resolve(), env)
        print(f"prepared active library in {root}")
        return 0
    if args.tasks is None or args.output is None:
        parser.error("training requires --tasks and --output")
    tasks = json.loads(args.tasks.read_text(encoding="utf-8"))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    records: list[dict[str, object]] = []
    for task in tasks:
        for arm in ("none", "library"):
            with tempfile.TemporaryDirectory(prefix="mini-agent-gym-train-") as directory:
                root = Path(directory)
                workspace = root / "worktree"
                prepare_workspace(args.repo.resolve(), task, workspace)
                env = os.environ.copy()
                env.update({
                    "MINI_AGENT_GYM": "1",
                    "ZS_DATA_DIR": str(root / "data"),
                    "ZS_LOCAL_DATA_DIR": str(root / "local"),
                    "ZS_STATE_DIR": str(root / "state"),
                    "ZS_CACHE_DIR": str(root / "cache"),
                })
                if arm == "library":
                    install_library(args.binary, str(task.get("library") or "seeds"), workspace, env)
                started = time.monotonic()
                agent = run([args.binary, "-p", str(task["prompt"])], workspace, env)
                oracle = dict(task["oracle"])
                checked = run(["/bin/sh", "-lc", str(oracle["command"])], workspace, env, timeout=300)
                record = {
                    "task": task["name"], "arm": arm, "success": agent.returncode == 0 and checked.returncode == 0,
                    "oracle_id": oracle.get("id") or hashlib.sha256(str(oracle["command"]).encode()).hexdigest(),
                    "elapsed_ms": round((time.monotonic() - started) * 1000), "production": False,
                    "agent_exit": agent.returncode, "oracle_exit": checked.returncode,
                }
                print("GYM_OUTCOME " + json.dumps(record, sort_keys=True))
                records.append(record)
                subprocess.run(["git", "worktree", "remove", "--force", str(workspace)], cwd=args.repo, capture_output=True)
    args.output.write_text("".join(json.dumps(row, sort_keys=True) + "\n" for row in records), encoding="utf-8")
    return 0 if all(row["success"] for row in records) else 1


if __name__ == "__main__":
    raise SystemExit(main())
