#!/usr/bin/env python3
"""Regression tests for ordinary VS Code extension CI coverage."""

from __future__ import annotations

import re
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = REPOSITORY_ROOT / ".github" / "workflows" / "ci.yml"


class VsCodeCiWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        match = re.search(
            r"(?ms)^  vscode:\n(?P<body>.*?)(?=^  [a-zA-Z0-9_-]+:\n|\Z)",
            workflow,
        )
        if match is None:
            raise AssertionError("workflow job 'vscode' is missing")
        cls.body = match.group("body")

    def test_job_uses_pinned_node_and_extension_working_directory(self) -> None:
        self.assertIn("node-version-file: editors/vscode/.nvmrc", self.body)
        self.assertIn("working-directory: editors/vscode", self.body)
        self.assertIn("cache-dependency-path: editors/vscode/package-lock.json", self.body)

    def test_job_runs_every_required_extension_gate(self) -> None:
        for command in (
            "npm ci --no-audit --no-fund",
            "npm run typecheck",
            "npm run lint",
            "npm test",
            "npm audit --audit-level=high",
            "cargo install --path . --debug --locked",
            "cp \"$RUNNER_TEMP/vscode-native/bin/mini-agent\" editors/vscode/bin/linux-x64/mini-agent",
            "npm run package:linux-x64",
            "npm run sbom",
        ):
            with self.subTest(command=command):
                self.assertIn(command, self.body)

    def test_job_uses_the_standard_non_scheduled_guard(self) -> None:
        condition = self.body.splitlines()[0].strip()
        self.assertIn("github.event_name != 'schedule'", condition)
        self.assertIn("inputs.scope != 'windows-general-sandbox'", condition)


if __name__ == "__main__":
    unittest.main()
