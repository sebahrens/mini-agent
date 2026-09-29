#!/usr/bin/env python3
"""Refuse a release unless CI passed for the exact tagged commit.

The release workflow runs in parallel with the CI run that the same tag push
starts. This gate waits, with a bounded deadline, for that CI run's
`ci-success` aggregate check run and passes only when it concluded `success`.

The check run must belong to the CI workflow run started by the tag push itself
(`event=push`, `head_branch=<tag>`): tag builds always run the full matrix,
whereas a `main` push run can report `ci-success` after skipping the matrix for
a documentation-only diff. Binding to that run's check suite also ignores any
same-named check run created by another app.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from typing import Any, Callable
from urllib.parse import quote

CHECK_NAME = "ci-success"
CI_WORKFLOW = "ci.yml"

Api = Callable[[str], Any]


def gh_api(path: str) -> Any:
    result = subprocess.run(
        ["gh", "api", "-H", "Accept: application/vnd.github+json", path],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(f"gh api {path} failed: {result.stderr.strip()}")
    return json.loads(result.stdout)


def tag_ci_runs(api: Api, repository: str, sha: str, tag: str) -> list[dict[str, Any]]:
    """Return the CI workflow runs the tag push started for this commit."""

    runs = api(
        f"repos/{repository}/actions/workflows/{CI_WORKFLOW}/runs"
        f"?head_sha={sha}&event=push&per_page=100"
    ).get("workflow_runs", [])
    return [
        run
        for run in runs
        if run.get("head_sha") == sha
        and run.get("head_branch") == tag
        and run.get("event") == "push"
    ]


def ci_success_runs(
    api: Api, repository: str, sha: str, suite_ids: set[int]
) -> list[dict[str, Any]]:
    """Return ci-success check runs on this commit that belong to the given suites."""

    check_runs = api(
        f"repos/{repository}/commits/{sha}/check-runs"
        f"?check_name={quote(CHECK_NAME)}&filter=all&per_page=100"
    ).get("check_runs", [])
    return [
        check
        for check in check_runs
        if check.get("name") == CHECK_NAME
        and (check.get("check_suite") or {}).get("id") in suite_ids
    ]


def poll_once(api: Api, repository: str, sha: str, tag: str) -> tuple[str, str]:
    """Return ("pass" | "fail" | "wait", reason) for the current CI state."""

    runs = tag_ci_runs(api, repository, sha, tag)
    if not runs:
        return "wait", f"no CI run for tag {tag} at {sha} yet"
    suite_ids = {run["check_suite_id"] for run in runs if "check_suite_id" in run}
    checks = ci_success_runs(api, repository, sha, suite_ids)
    for check in checks:
        if check.get("status") == "completed" and check.get("conclusion") == "success":
            return "pass", f"{CHECK_NAME} succeeded: {check.get('html_url', check.get('id'))}"
    if all(run.get("status") == "completed" for run in runs):
        concluded = ", ".join(
            f"{check.get('conclusion')}" for check in checks
        ) or "no ci-success check run"
        return "fail", (
            f"CI for tag {tag} at {sha} completed without a successful "
            f"{CHECK_NAME} ({concluded}); fix CI and re-run the failed CI jobs, "
            "then re-run this release workflow"
        )
    return "wait", f"CI for tag {tag} at {sha} is still running"


def wait_for_ci(
    api: Api,
    repository: str,
    sha: str,
    tag: str,
    *,
    timeout: float,
    interval: float,
    clock: Callable[[], float] = time.monotonic,
    sleep: Callable[[float], None] = time.sleep,
    log: Callable[[str], None] = print,
) -> tuple[bool, str]:
    deadline = clock() + timeout
    while True:
        try:
            verdict, reason = poll_once(api, repository, sha, tag)
        except (RuntimeError, ValueError, KeyError, TypeError) as error:
            # A transient API failure is retried until the same deadline; it
            # can delay a release but never approve one.
            verdict, reason = "wait", f"GitHub API error: {error}"
        if verdict == "pass":
            return True, reason
        if verdict == "fail":
            return False, reason
        if clock() >= deadline:
            return False, f"timed out after {int(timeout)}s: {reason}"
        log(f"waiting: {reason}")
        sleep(interval)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", default=os.environ.get("GITHUB_REPOSITORY"))
    parser.add_argument("--sha", default=os.environ.get("GITHUB_SHA"))
    parser.add_argument("--tag", default=os.environ.get("GITHUB_REF_NAME"))
    parser.add_argument("--timeout-seconds", type=float, default=150 * 60)
    parser.add_argument("--interval-seconds", type=float, default=30)
    args = parser.parse_args(argv)
    if not args.repository or not args.sha or not args.tag:
        print("repository, sha, and tag are required", file=sys.stderr)
        return 2

    ok, reason = wait_for_ci(
        gh_api,
        args.repository,
        args.sha,
        args.tag,
        timeout=args.timeout_seconds,
        interval=args.interval_seconds,
    )
    if ok:
        print(reason)
        return 0
    print(f"ERROR: {reason}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
