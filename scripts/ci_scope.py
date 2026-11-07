#!/usr/bin/env python3
"""Classify a CI diff as `docs`, `tests` or `code` (mini-agent-m7wn5).

The `changes` job and pass fingerprints share the non-build input rules here.
The workflow first uses --filter for the docs-only decision, then refines the
answer: a diff whose every non-documentation path is test-only *by path* gets
the `tests` scope, which runs fmt/policy, Linux clippy and the Linux test rows
and skips the macOS/Windows rows, platform smokes, packaging, install, VS Code
and the heavy gates. A test path that is platform specific also keeps its
platform's rows.

Production files with inline `#[cfg(test)]` modules are code: only the path
decides. The script fails open everywhere: an unresolvable base, a missing
history, a tag/schedule/dispatch event or any error reports `code` with every
platform enabled, which is the full matrix.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from typing import Callable, Iterable, NamedTuple

# Only known, non-build data is exempt. Unknown JSON and other data still run
# the matrix; extensions alone cannot distinguish evidence from fixtures.
DOCS_ONLY = re.compile(r"^(docs/|\.beads/|[^/]*\.md$|editors/vscode/.*\.md$)")
CONSUMED_DOCS = re.compile(
    r"^(docs/agent/|docs/specs/|docs/benchmarks/|docs/vscode-acp-setup\.md$|SOURCE\.md$|LICENSE)"
)
RECORDS = re.compile(r"^docs/(reviews|plans|decisions)/")
REPOSITORY_METADATA = re.compile(
    r"^(\.github/(ISSUE_TEMPLATE/|PULL_REQUEST_TEMPLATE/|PULL_REQUEST_TEMPLATE\.md$|"
    r"CODEOWNERS$|dependabot\.yml$))"
)
TEST_PATH = re.compile(r"^(src/tests/|src/.+/tests/|tests/)|_tests\.rs$")
PLATFORM_NAMES = {
    "windows": "windows",
    "appcontainer": "windows",
    "lpac": "windows",
    "macos": "macos",
    "seatbelt": "macos",
}
# Both whole-file and per-item attributes can introduce platform-only tests.
# This conservatively collects markers, rather than evaluating Rust cfg logic.
PLATFORM_CFG = re.compile(
    r"(?ms)^\s*#!?\s*\[\s*cfg(?:_attr)?\s*\((?P<body>.*?)\)\s*\]"
)

ALL_OS = ("ubuntu-latest", "macos-latest")


class Scope(NamedTuple):
    scope: str  # docs | tests | code
    macos: bool
    windows: bool


FULL = Scope("code", True, True)


def is_docs(path: str) -> bool:
    """Inputs that need only the unconditional formatting/repository policy job."""

    if CONSUMED_DOCS.match(path):
        return False
    if RECORDS.match(path) or REPOSITORY_METADATA.match(path):
        return True
    if not DOCS_ONLY.match(path):
        return False
    return not (path.startswith("docs/") and not path.endswith(".md"))


def is_test_path(path: str) -> bool:
    return TEST_PATH.search(path) is not None


def platforms(path: str, source: str | None = None) -> set[str]:
    """Platforms (`windows`, `macos`) a test path is specific to."""

    found = {p for token, p in PLATFORM_NAMES.items() if token in path.lower()}
    if source:
        for match in PLATFORM_CFG.finditer(source):
            body = match.group("body")
            if "windows" in body:
                found.add("windows")
            if "macos" in body or re.search(r"\bunix\b", body):
                found.add("macos")
            # Complemented platform predicates can select either native OS
            # without naming it, e.g. not(unix) selects Windows. Keep both
            # rather than pretending to evaluate arbitrary nested Rust cfg.
            if re.search(r"\bnot\s*\(", body) and re.search(r"\b(windows|macos|unix|linux)\b", body):
                found.update(("macos", "windows"))
    return found


def classify(paths: Iterable[str], read: Callable[[str], str | None] | None = None) -> Scope:
    """Classify changed paths. `read` returns a path's text, or None."""

    macos = windows = False
    saw_test = False
    for path in paths:
        if not path:
            continue
        if is_docs(path):
            continue
        if not is_test_path(path):
            return FULL
        saw_test = True
        source = read(path) if read else None
        found = platforms(path, source)
        macos = macos or "macos" in found
        windows = windows or "windows" in found
    if not saw_test:
        return Scope("docs", False, False)
    return Scope("tests", macos, windows)


def test_os_json(scope: Scope) -> str:
    rows = ALL_OS if scope.scope != "tests" or scope.macos else ALL_OS[:1]
    return "[" + ",".join(f'"{row}"' for row in rows) + "]"


def resolve(env: dict[str, str], run=subprocess.run, report: Callable[[str], None] | None = None) -> Scope:
    """Scope for a workflow event; anything doubtful is the full matrix."""

    def decided(scope: Scope, reason: str) -> Scope:
        if report:
            report(f"CI impact: {reason}")
        return scope

    event = env.get("EVENT", "")
    if event == "workflow_dispatch" and env.get("SCOPE") in (
        "windows-command-family", "windows-session-storage"
    ):
        return decided(Scope("docs", False, False), f"selected manual proof {env['SCOPE']}")
    if event not in ("push", "pull_request"):
        return decided(FULL, "full event policy (not a branch push or pull request)")
    if env.get("GITHUB_REF", "").startswith("refs/tags/"):
        return decided(FULL, "tag builds require the full matrix")
    base = env.get("BASE_SHA", "")
    head = env.get("HEAD_SHA", "")
    if not base or set(base) == {"0"} or not head:
        return decided(FULL, "missing base/head revision; running the full matrix")
    try:
        run(
            ["git", "cat-file", "-e", f"{base}^{{commit}}"],
            capture_output=True,
            text=True,
            check=True,
        )
        diff = run(
            ["git", "diff", "--no-renames", "--name-only", "-z", base, head, "--"],
            capture_output=True,
            text=True,
            check=True,
        )
    except (OSError, subprocess.CalledProcessError):
        return decided(FULL, "revision history or diff unavailable; running the full matrix")
    # Include both sides of moves: moving code into reviews still removes code.
    # NUL delimiters preserve filenames with newlines or Git quoting characters.
    paths = [path for path in diff.stdout.split("\0") if path]
    if report:
        omitted = f"; {len(paths) - 20} more omitted" if len(paths) > 20 else ""
        report(f"Changed paths ({len(paths)}): {json.dumps(paths[:20])}{omitted}")

    def read(path: str) -> str | None:
        if not path.endswith(".rs"):
            return None
        versions = []
        for revision in (base, head):
            entry = run(
                ["git", "--literal-pathspecs", "ls-tree", "-z", revision, "--", path],
                capture_output=True, text=True, encoding="utf-8", errors="replace", check=True,
            ).stdout
            if not entry:
                # An added or deleted file exists on only one side of the diff.
                continue
            mode = entry.split(" ", 1)[0]
            if mode not in ("100644", "100755"):
                raise OSError("changed Rust test is not a regular tracked file")
            versions.append(run(
                ["git", "show", f"{revision}:{path}"],
                capture_output=True, text=True, encoding="utf-8", errors="replace", check=True,
            ).stdout)
        if not versions:
            raise OSError("changed Rust test could not be read from either revision")
        return "\n".join(versions)

    try:
        scope = classify(paths, read)
    except (OSError, subprocess.CalledProcessError):
        return decided(FULL, "test source unavailable; running the full matrix")
    if scope.scope == "docs":
        reason = "only non-build records/documentation; skipping the expensive matrix"
    elif scope.scope == "tests":
        reason = f"test-only inputs; retain macOS={scope.macos}, Windows={scope.windows}"
    else:
        reason = "build or unknown inputs changed; running the full matrix"
    return decided(scope, reason)


def render(scope: Scope) -> str:
    full = scope.scope != "tests"
    values = {
        "scope": scope.scope,
        "full": str(full).lower(),
        "macos": str(full or scope.macos).lower(),
        "windows": str(full or scope.windows).lower(),
        "test_os": test_os_json(scope),
    }
    return "".join(f"{key}={value}\n" for key, value in values.items())


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--filter", action="store_true", help="Emit the workflow code filter")
    args = parser.parse_args()
    try:
        scope = resolve(dict(os.environ), report=print)
    except Exception as error:  # fail open on any doubt
        print(f"scope classification failed ({error}); running the full matrix")
        scope = FULL
    # The second workflow step runs only after code=true. Fail open if it gets
    # a contradictory docs result; docs-only runs leave its defaults untouched.
    text = (
        f"code={str(scope.scope != 'docs').lower()}\n"
        if args.filter else render(FULL if scope.scope == "docs" else scope)
    )
    print(text, end="")
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with open(output, "a", encoding="utf-8") as handle:
            handle.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
