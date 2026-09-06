#!/usr/bin/env python3
"""Mine closed beads with explicit oracles into reproducible gym task.json files."""

from __future__ import annotations

import argparse
import json
import subprocess
import tempfile
from pathlib import Path


def run(argv: list[str], cwd: Path, *, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(argv, cwd=cwd, text=True, capture_output=True, check=check)


def changed_text_files(repo: Path, parent: str, commit: str) -> tuple[dict[str, str], dict[str, str]]:
    names = run(["git", "diff", "--name-only", "--diff-filter=AM", parent, commit], repo).stdout.splitlines()
    before: dict[str, str] = {}
    after: dict[str, str] = {}
    for name in names:
        if name.startswith(".") or ".." in Path(name).parts:
            continue
        old = run(["git", "show", f"{parent}:{name}"], repo, check=False)
        new = run(["git", "show", f"{commit}:{name}"], repo, check=False)
        if new.returncode or len(new.stdout.encode()) > 256_000:
            continue
        try:
            new.stdout.encode().decode("utf-8")
            old.stdout.encode().decode("utf-8")
        except UnicodeError:
            continue
        if old.returncode == 0:
            before[name] = old.stdout
        after[name] = new.stdout
    return before, after


def oracle_at(repo: Path, revision: str, command: str) -> bool:
    with tempfile.TemporaryDirectory(prefix="mini-agent-gym-mine-") as directory:
        worktree = Path(directory) / "worktree"
        run(["git", "worktree", "add", "--detach", str(worktree), revision], repo)
        try:
            result = subprocess.run(
                ["/bin/sh", "-lc", command],
                cwd=worktree,
                text=True,
                capture_output=True,
                timeout=300,
            )
            return result.returncode == 0
        finally:
            run(["git", "worktree", "remove", "--force", str(worktree)], repo, check=False)


def mine(repo: Path, oracle_map: dict[str, str], validate: bool, limit: int) -> list[dict[str, object]]:
    beads = json.loads(run(["bd", "list", "--status", "closed", "--json", "--limit", "0"], repo).stdout)
    tasks: list[dict[str, object]] = []
    for bead in beads:
        bead_id = bead.get("id", "")
        oracle = oracle_map.get(bead_id)
        if not oracle:
            continue
        commit = run(
            ["git", "log", "--all", "--format=%H", "-1", f"--grep={bead_id}"], repo
        ).stdout.strip()
        if not commit:
            continue
        parent = run(["git", "rev-parse", f"{commit}^"], repo).stdout.strip()
        if validate and (oracle_at(repo, parent, oracle) or not oracle_at(repo, commit, oracle)):
            continue
        initial, expected = changed_text_files(repo, parent, commit)
        if not expected:
            continue
        tasks.append(
            {
                "name": bead_id,
                "prompt": bead.get("title") or bead.get("description") or bead_id,
                "base_commit": parent,
                "initial_files": initial,
                "oracle": {"command": oracle, "id": f"bead:{bead_id}"},
                "expected_files": expected,
                "budgets": {"max_provider_turns": 12, "max_tool_calls": 24, "max_total_tokens": 16000},
                "tags": sorted(set(bead.get("labels") or []) | {"mined", "fail-to-pass"}),
                "scripted_provider_turns": [],
                "library": "seeds",
                "fix_commit": commit,
            }
        )
        if len(tasks) >= limit:
            break
    return tasks


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--oracle-map", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--limit", type=int, default=20)
    parser.add_argument("--no-validate", action="store_true")
    args = parser.parse_args()
    repo = args.repo.resolve()
    oracle_map = json.loads(args.oracle_map.read_text(encoding="utf-8"))
    tasks = mine(repo, oracle_map, not args.no_validate, max(1, args.limit))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(tasks, indent=2) + "\n", encoding="utf-8")
    print(f"mined {len(tasks)} validated task(s) into {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
