#!/usr/bin/env python3
"""Decide the `ci-success` aggregate result from the CI workflow's `needs` context.

`ci-success` is the single status check that branch protection and the release
workflow require. It must never turn green because a gating job silently did
not run, so a `skipped` result is accepted only where the workflow skips that
job by design:

* a documentation-only change (`changes` reported `code=false`) skips every
  build, lint and test job and the Corresponding Source assembly, and only
  `changes` and `fmt` must still succeed.

* a test-only change (`changes` reported `scope=tests`, see scripts/ci_scope.py)
  on a pull request or branch push additionally skips TESTS_SCOPE_SKIPPABLE:
  macOS/Windows rows, platform smokes, packaging, install and VS Code. A job
  tied to a platform stays required when `changes` reported that platform
  (`macos`/`windows` output `true`) because a changed test path is specific to
  it. Tags, the schedule and dispatch never accept these skips.

The heavy native Phase 6 gates (HEAVY_GATES) run only for tag pushes, manual
dispatch and the weekly schedule, so on pull requests and branch pushes their
`skipped` result is accepted by design; on a tag push or dispatch they must
succeed like every other job, which keeps the release's CI evidence complete.

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


# Slow Windows/macOS containment gates (and the aggregate that needs them),
# skipped by design outside tag pushes, workflow_dispatch and the schedule.
HEAVY_GATES = (
    "macos-worker-containment-gate",
    "windows-worker-containment-gate",
    "windows-general-sandbox-policy",
    "phase6-cross-platform-gate",
)


# Jobs a `scope=tests` run may skip. The value is the `changes` output that
# keeps the job required (a platform-specific test path), or None.
TESTS_SCOPE_SKIPPABLE: dict[str, str | None] = {
    "corresponding-source": None,
    "harness-regression": None,
    "package-compliance-smoke": None,
    "vscode": None,
    "platform-paths": None,
    "platform-path-negative-controls": None,
    "platform-path-install-smoke": None,
    "mcp-stdio": None,
    "lsp-lifecycle": None,
    "install": None,
    "windows-msi": None,
    "package": None,
    "dependency-policy": None,
    "windows-default-compile": "windows",
    "windows-hooks-compile": "windows",
    "windows-worker-launcher-unit": "windows",
    "macos-gym-ownership": "macos",
}


def heavy_gates_skipped_by_design(event: str, ref: str) -> bool:
    """True for the fast events: pull requests and pushes to branches."""

    if event == "pull_request":
        return True
    return event == "push" and not ref.startswith("refs/tags/")


def evaluate(needs: dict[str, Any], event: str, ref: str = "") -> list[str]:
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
    fast_event = heavy_gates_skipped_by_design(event, ref)
    scope = outputs.get("scope", "code")
    if changes.get("result") == "success" and scope not in ("code", "tests"):
        errors.append(f"changes reported an invalid scope output {scope!r}")
    # Fail closed: only a well-formed tests scope on a fast event relaxes anything.
    tests_scope = full_matrix and scope == "tests" and fast_event

    for job in sorted(needs):
        entry = needs[job]
        result = entry.get("result") if isinstance(entry, dict) else None
        if result == "success":
            continue
        if result == "skipped" and job not in ALWAYS_REQUIRED and not full_matrix:
            continue
        if result == "skipped" and job in HEAVY_GATES and fast_event:
            continue
        if result == "skipped" and tests_scope and job in TESTS_SCOPE_SKIPPABLE:
            platform = TESTS_SCOPE_SKIPPABLE[job]
            if platform is None or outputs.get(platform) == "false":
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
    parser.add_argument(
        "--ref",
        default=os.environ.get("GITHUB_REF", ""),
        help="the triggering git ref (refs/tags/... for a tag push)",
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

    errors = evaluate(needs, args.event, args.ref)
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
