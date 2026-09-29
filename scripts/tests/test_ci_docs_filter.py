#!/usr/bin/env python3
"""Regression tests for the CI documentation-only change filter.

The filter step's shell script is extracted from ci.yml and run against a
stubbed `git` so each case exercises the exact checked-in classification.
"""

from __future__ import annotations

import os
import re
import subprocess
import tempfile
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = REPOSITORY_ROOT / ".github" / "workflows" / "ci.yml"
BASE = "1" * 40
HEAD = "2" * 40


def filter_script() -> str:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    match = re.search(
        r"(?m)^        id: filter\n(?:^[^\n]*\n)*?^        run: \|\n(?P<body>(?:^          [^\n]*\n|^\n)+)",
        workflow,
    )
    if match is None:
        raise AssertionError("the changes job's filter step is missing")
    body = match.group("body")
    # Never run more than the filter step itself.
    if "code=false" not in body or "\n  " + "fmt:" in body or len(body.splitlines()) > 150:
        raise AssertionError("extracted filter script is not exactly the filter step")
    return "".join(
        line[10:] if line.startswith(" " * 10) else "\n"
        for line in match.group("body").splitlines(keepends=True)
    )


class CiDocsFilterTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.script = filter_script()

    def classify(self, *changed: str) -> str:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stub = root / "git"
            stub.write_text(
                "#!/bin/sh\n"
                'case "$1" in\n'
                "  cat-file) exit 0 ;;\n"
                '  diff) cat "$FILTER_CHANGED" ;;\n'
                "  *) exit 2 ;;\n"
                "esac\n",
                encoding="utf-8",
            )
            stub.chmod(0o755)
            changed_file = root / "changed"
            changed_file.write_text("".join(f"{path}\n" for path in changed), encoding="utf-8")
            output = root / "output"
            output.touch()
            env = os.environ.copy()
            env.update(
                PATH=f"{root}:{env['PATH']}",
                FILTER_CHANGED=str(changed_file),
                GITHUB_OUTPUT=str(output),
                GITHUB_REF="refs/pull/1/merge",
                EVENT="pull_request",
                BASE_SHA=BASE,
                HEAD_SHA=HEAD,
            )
            result = subprocess.run(
                # GitHub Actions runs `run:` steps as `bash -e -o pipefail`.
                ["bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", self.script],
                env=env,
                capture_output=True,
                text=True,
            )
            self.assertEqual(0, result.returncode, result.stderr)
            values = re.findall(r"^code=(true|false)$", output.read_text(), re.MULTILINE)
            self.assertEqual(1, len(values), output.read_text())
            return values[0]

    def test_plain_documentation_skips_the_matrix(self) -> None:
        self.assertEqual("false", self.classify("README.md", "docs/plans/idea.md"))
        self.assertEqual("false", self.classify(".beads/issues.jsonl", "editors/vscode/README.md"))

    def test_code_changes_run_the_matrix(self) -> None:
        self.assertEqual("true", self.classify("docs/plans/idea.md", "src/main.rs"))

    def test_documentation_consumed_by_the_build_or_tests_runs_the_matrix(self) -> None:
        for path in (
            "docs/specs/phase-6-brokered-js-runtime.md",
            "docs/specs/subprocess-trust.md",
            "docs/agent/CONFIG.md",
            "docs/vscode-acp-setup.md",
            "docs/benchmarks/results/js-worker-baseline.json",
            "docs/acp-registry.json",
            "SOURCE.md",
            "LICENSE",
        ):
            with self.subTest(path=path):
                self.assertEqual("true", self.classify("docs/plans/idea.md", path))

    def test_every_embedded_or_read_doc_is_covered(self) -> None:
        # Any docs path named by Rust source must be classified as code.
        referenced: set[str] = set()
        for source in (REPOSITORY_ROOT / "src").rglob("*.rs"):
            text = source.read_text(encoding="utf-8", errors="replace")
            for match in re.finditer(
                r'(?:include_str!|include_dir!|include_bytes!)\("(?:\$CARGO_MANIFEST_DIR/|(?:\.\./)+)(docs/[^"]+)"',
                text,
            ):
                referenced.add(match.group(1))
        self.assertTrue(referenced, "expected embedded documentation references")
        for path in sorted(referenced):
            candidate = path if "." in Path(path).name else f"{path}/EXAMPLE.md"
            with self.subTest(path=candidate):
                self.assertEqual("true", self.classify(candidate))


if __name__ == "__main__":
    unittest.main()
