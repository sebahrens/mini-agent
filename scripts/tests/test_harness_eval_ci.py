#!/usr/bin/env python3
"""Policy checks for the deterministic task-level harness evaluation."""

from __future__ import annotations

import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = REPOSITORY_ROOT / ".github" / "workflows" / "harness-eval.yml"
PR_WORKFLOW = REPOSITORY_ROOT / ".github" / "workflows" / "ci.yml"
FIXTURES = REPOSITORY_ROOT / "tests" / "harness_eval" / "fixtures"


class HarnessEvalCiTests(unittest.TestCase):
    def test_pull_requests_gate_on_the_bounded_deterministic_eval(self) -> None:
        workflow = PR_WORKFLOW.read_text(encoding="utf-8")
        job = workflow.split("  harness-regression:", 1)[1].split(
            "\n  package-compliance-smoke:", 1
        )[0]
        self.assertIn("github.event_name == 'pull_request'", job)
        self.assertIn("cargo test --locked harness_regression_eval", job)
        self.assertIn("--ignored --nocapture", job)
        self.assertIn("task_json_library_axis_uses_real_store_and_records_oracles", job)

    def test_nightly_workflow_runs_and_archives_the_bounded_eval(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        self.assertIn("cron: '41 2 * * *'", workflow)
        self.assertIn("env:\n  RUSTFLAGS: ''", workflow)
        self.assertIn("cargo test --locked harness_regression_eval", workflow)
        self.assertIn("cargo test --locked persona_regression_eval", workflow)
        self.assertIn("--ignored --nocapture --test-threads=1", workflow)
        self.assertIn("set -o pipefail", workflow)
        self.assertIn("grep '^HARNESS_EVAL '", workflow)
        self.assertIn('test "$(wc -l <', workflow)
        self.assertIn(')" -eq 5', workflow)
        self.assertIn("harness-eval.jsonl", workflow)
        self.assertIn("persona-eval.jsonl", workflow)
        self.assertIn("-eq 9", workflow)
        self.assertIn("if-no-files-found: error", workflow)

    def test_the_five_core_fixture_repositories_are_present(self) -> None:
        names = sorted(path.parent.name for path in FIXTURES.glob("*/fixture.json"))
        self.assertEqual(
            names,
            [
                "compaction_mid_task",
                "crlf_edit",
                "js_aggregation",
                "persona_invocation",
                "subagent_fanout",
            ],
        )

    def test_persona_fixture_covers_every_shipped_persona(self) -> None:
        import json

        fixture_path = REPOSITORY_ROOT / "tests" / "harness_eval" / "personas" / "fixture.json"
        fixture = json.loads(fixture_path.read_text(encoding="utf-8"))
        actual = sorted(case["agent_type"] for case in fixture["cases"])
        shipped = sorted(path.stem for path in (REPOSITORY_ROOT / "data" / "agents").glob("*.md"))
        self.assertEqual(actual, shipped)
        self.assertTrue(fixture["repository_files"])
        for case in fixture["cases"]:
            self.assertTrue(case["expected_finding"])
            self.assertIn(case["expected_finding"], case["response"])


if __name__ == "__main__":
    unittest.main()
