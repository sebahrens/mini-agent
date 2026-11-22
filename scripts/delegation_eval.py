#!/usr/bin/env python3
"""Delegated-versus-direct evaluation lane (mini-agent-964ao).

Runs each fixture task of ``tests/harness_eval/delegation/tasks.json`` twice with
a real mini-agent binary: once with the ``task`` tool enabled and once with
``task_enabled = false`` (a top-level config key). It reads each run's headless JSON result
(``usage``, ``total_cost_usd``, ``num_turns`` and the ``subagent_delegation``
telemetry), checks the task's expected files, and prints one JSON record per
arm plus a comparison.

This needs a configured provider (operator credentials in the environment) and
costs money, so nothing in CI runs it. ``--summarize FILE`` re-reads records
written by an earlier run and compares them without any provider.

    python3 scripts/delegation_eval.py --binary target/debug/mini-agent --out runs/
    python3 scripts/delegation_eval.py --summarize runs/records.jsonl
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_TASKS = ROOT / "tests" / "harness_eval" / "delegation" / "tasks.json"
ARMS = ("delegated", "direct")


def load_tasks(path: Path) -> list[dict[str, Any]]:
    tasks = json.loads(path.read_text(encoding="utf-8"))["tasks"]
    for task in tasks:
        for key in ("name", "prompt", "files", "expected_files"):
            if key not in task:
                raise ValueError(f"task {task.get('name', '?')} lacks {key!r}")
    return tasks


def files_match(workspace: Path, expected: dict[str, str]) -> bool:
    for relative, content in expected.items():
        target = workspace / relative
        if not target.is_file() or target.read_text(encoding="utf-8") != content:
            return False
    return True


def record_from_result(task: str, arm: str, result: dict[str, Any], success: bool) -> dict:
    """One flat record from a headless JSON result."""
    usage = result.get("usage") or {}
    delegation = result.get("subagent_delegation") or {}
    return {
        "task": task,
        "arm": arm,
        "success": success,
        "num_turns": result.get("num_turns"),
        "total_tokens": usage.get("total_tokens"),
        "cost_usd": result.get("total_cost_usd"),
        "children": delegation.get("children", 0),
        "contract_pass": delegation.get("contract_pass", 0),
        "repair_used": delegation.get("repair_used", 0),
        "host_replaced": delegation.get("host_replaced", 0),
        "child_tokens": delegation.get("total_tokens", 0),
        "child_cost_units": delegation.get("cost_units", 0),
        # The whole aggregate, so repaired / unstructured / normalized contracts
        # and the per-child rows survive into the record.
        "subagent_delegation": delegation,
    }


class ConfigRejected(RuntimeError):
    """The binary refused the generated configuration; the run would be an
    all-zero failure that looks like a result."""


CONFIG_ERROR_MARKERS = ("config is not valid", "unknown field", "invalid config")


def config_error(stderr: str, returncode: int) -> str | None:
    """The stderr tail when the binary exited because of its configuration."""
    if returncode == 0:
        return None
    lowered = stderr.lower()
    if any(marker in lowered for marker in CONFIG_ERROR_MARKERS):
        return stderr[-500:]
    return None


def scratch_root() -> str:
    """A temp root with no symlinked path component (macOS ``/var`` is one;
    the binary rejects private paths through symlinks)."""
    return os.path.realpath(tempfile.gettempdir())


def config_text(arm: str) -> str:
    """`task_enabled` is a top-level key, not a table member."""
    enabled = "true" if arm == "delegated" else "false"
    return f"task_enabled = {enabled}\n"


def run_arm(binary: Path, task: dict[str, Any], arm: str, extra: list[str]) -> dict:
    with tempfile.TemporaryDirectory(prefix="delegation-eval-", dir=scratch_root()) as scratch:
        scratch_path = Path(scratch)
        workspace = scratch_path / "work"
        config = scratch_path / "config"
        config.mkdir()
        for relative, content in task["files"].items():
            target = workspace / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content, encoding="utf-8")
        (config / "config.toml").write_text(config_text(arm), encoding="utf-8")
        env = dict(os.environ, MINI_AGENT_CONFIG_DIR=str(config))
        completed = subprocess.run(
            [str(binary), "-p", task["prompt"], "--output", "json", *extra],
            cwd=workspace,
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )
        rejected = config_error(completed.stderr, completed.returncode)
        if rejected is not None:
            raise ConfigRejected(f"{task['name']}/{arm}: the binary rejected its config: {rejected}")
        try:
            result = json.loads(completed.stdout.strip().splitlines()[-1])
        except (IndexError, json.JSONDecodeError):
            result = {"error": completed.stderr[-500:]}
        success = completed.returncode == 0 and files_match(workspace, task["expected_files"])
        return record_from_result(task["name"], arm, result, success)


def compare(records: list[dict]) -> list[dict]:
    """Per task: the two arms side by side with the delegated/direct ratios."""
    by_task: dict[str, dict[str, dict]] = {}
    for record in records:
        by_task.setdefault(record["task"], {})[record["arm"]] = record

    def ratio(delegated: Any, direct: Any) -> float | None:
        if not delegated or not direct:
            return None
        return round(delegated / direct, 3)

    rows = []
    for task, arms in sorted(by_task.items()):
        delegated, direct = arms.get("delegated"), arms.get("direct")
        if not delegated or not direct:
            continue
        rows.append(
            {
                "task": task,
                "delegated_success": delegated["success"],
                "direct_success": direct["success"],
                "token_ratio": ratio(delegated["total_tokens"], direct["total_tokens"]),
                "cost_ratio": ratio(delegated["cost_usd"], direct["cost_usd"]),
                "children": delegated["children"],
                "contract_pass": delegated["contract_pass"],
                "repair_used": delegated["repair_used"],
                "host_replaced": delegated["host_replaced"],
            }
        )
    return rows


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--binary", type=Path, help="mini-agent binary to run")
    parser.add_argument("--tasks", type=Path, default=DEFAULT_TASKS)
    parser.add_argument("--out", type=Path, help="directory for records.jsonl")
    parser.add_argument("--summarize", type=Path, help="compare records of an earlier run")
    parser.add_argument("extra", nargs="*", help="extra mini-agent arguments after --")
    args = parser.parse_args(argv)

    if args.summarize:
        records = [
            json.loads(line)
            for line in args.summarize.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]
    else:
        if not args.binary:
            parser.error("--binary is required unless --summarize is given")
        records = []
        for task in load_tasks(args.tasks):
            for arm in ARMS:
                record = run_arm(args.binary, task, arm, args.extra)
                print(json.dumps(record), flush=True)
                records.append(record)
        if args.out:
            args.out.mkdir(parents=True, exist_ok=True)
            (args.out / "records.jsonl").write_text(
                "".join(json.dumps(record) + "\n" for record in records), encoding="utf-8"
            )
    print(json.dumps({"comparison": compare(records)}, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
