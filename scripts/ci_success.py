#!/usr/bin/env python3
"""Decide the `ci-success` aggregate result from the CI workflow's `needs` context.

`ci-success` is the single status check that branch protection and the release
workflow require. It must never turn green because a gating job silently did
not run, so a `skipped` result is accepted only where the workflow skips that
job by design:

* a documentation-only change (`changes` reported `code=false`) skips every
  build, lint and test job, and only `changes` and `fmt` must still succeed.

Whenever `changes` reported `code=true` every other gating job must succeed on
every event, including `harness-regression` on a push (it used to run on pull
requests only, which left it unexercised because work lands directly on main).
`failure` and `cancelled` are never accepted.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from typing import Any

# Jobs that must succeed on every event ci-success runs for.
ALWAYS_REQUIRED = ("changes", "fmt")


def evaluate(needs: dict[str, Any], event: str) -> list[str]:
    """Return one error per job whose result does not satisfy the gate."""

    errors: list[str] = []
    if not isinstance(needs, dict) or not needs:
        return ["ci-success received no job results"]

    for job in ALWAYS_REQUIRED:
        if job not in needs:
            errors.append(f"ci-success must depend on {job}")

    changes = needs.get("changes") or {}
    outputs = changes.get("outputs") or {}
    code = outputs.get("code")
    if changes.get("result") == "success" and code not in ("true", "false"):
        errors.append(f"changes reported an invalid code output {code!r}")
    full_matrix = code != "false"

    for job in sorted(needs):
        entry = needs[job]
        result = entry.get("result") if isinstance(entry, dict) else None
        if result == "success":
            continue
        if result == "skipped" and job not in ALWAYS_REQUIRED and not full_matrix:
            continue
        errors.append(f"{job} finished with {result!r}")
    return errors


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--event",
        default=os.environ.get("GITHUB_EVENT_NAME", ""),
        help="the triggering GitHub event name",
    )
    args = parser.parse_args(argv)
    raw = os.environ.get("CI_NEEDS_JSON")
    if not raw:
        print("CI_NEEDS_JSON is not set", file=sys.stderr)
        return 2
    try:
        needs = json.loads(raw)
    except json.JSONDecodeError as error:
        print(f"CI_NEEDS_JSON is not valid JSON: {error}", file=sys.stderr)
        return 2

    errors = evaluate(needs, args.event)
    for job in sorted(needs):
        entry = needs[job] if isinstance(needs[job], dict) else {}
        print(f"{job}: {entry.get('result')}")
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1
    print("ci-success: every gating job succeeded or was skipped by design")
    return 0


if __name__ == "__main__":
    sys.exit(main())
