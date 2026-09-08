#!/usr/bin/env python3
"""Policy checks for the deterministic task-level harness evaluation."""

from __future__ import annotations

import importlib.util
import json
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
        # Metrics come from the validating extractor, never a line-anchored
        # grep that libtest's progress prefix can defeat.
        self.assertNotIn("grep '^HARNESS_EVAL '", workflow)
        self.assertNotIn("grep '^PERSONA_EVAL '", workflow)
        self.assertIn("scripts/harness_eval_metrics.py", workflow)
        self.assertIn("--marker HARNESS_EVAL", workflow)
        self.assertIn("--marker PERSONA_EVAL", workflow)
        self.assertIn("--expect 5", workflow)
        self.assertIn("--expect 9", workflow)
        self.assertIn("harness-eval.jsonl", workflow)
        self.assertIn("persona-eval.jsonl", workflow)
        self.assertIn("if-no-files-found: error", workflow)

    def test_nightly_workflow_keeps_the_library_axis_and_archives_diagnostics(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        self.assertIn("task_json_library_axis_uses_real_store_and_records_oracles", workflow)
        self.assertIn("--require library=none", workflow)
        self.assertIn("--require library=library", workflow)
        # Raw logs are exactly what a failed run needs.
        self.assertIn("name: harness-eval-logs", workflow)
        self.assertIn("if: always()", workflow)


def _load_extractor():
    path = REPOSITORY_ROOT / "scripts" / "harness_eval_metrics.py"
    spec = importlib.util.spec_from_file_location("harness_eval_metrics", path)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


class HarnessEvalExtractorTests(unittest.TestCase):
    """The extractor must survive the output libtest actually produces."""

    def setUp(self) -> None:
        self.extractor = _load_extractor()

    def persona_record(self, agent_type: str) -> str:
        return json.dumps(
            {
                "agent_type": agent_type,
                "success": True,
                "expected_finding": f"{agent_type} finding",
            }
        )

    def test_a_libtest_progress_prefix_does_not_hide_the_first_record(self) -> None:
        lines = [
            "running 1 test",
            # libtest emits the progress line and the first captured record
            # on one line, which is how the ninth persona went missing.
            "test tests::harness_eval_tests::persona_regression_eval ... PERSONA_EVAL "
            + self.persona_record("rust-maintainer"),
        ] + [
            "PERSONA_EVAL " + self.persona_record(f"persona-{index}") for index in range(8)
        ]
        records = self.extractor.extract_records(lines, "PERSONA_EVAL")
        self.assertEqual(len(records), 9)
        self.assertEqual(records[0]["agent_type"], "rust-maintainer")

    def test_unrelated_output_is_ignored(self) -> None:
        lines = [
            "warning: unused variable",
            "test result: ok. 1 passed",
            "HARNESS_EVAL "
            + json.dumps(
                {
                    "name": "crlf_edit",
                    "success": True,
                    "provider_turns": 2,
                    "tool_calls": 1,
                    "total_tokens": 220,
                }
            ),
        ]
        records = self.extractor.extract_records(lines, "HARNESS_EVAL")
        self.assertEqual(len(records), 1)
        self.assertEqual(records[0]["name"], "crlf_edit")

    def test_malformed_json_is_a_failure_not_a_silent_drop(self) -> None:
        with self.assertRaises(self.extractor.ExtractionError):
            self.extractor.extract_records(["PERSONA_EVAL {not json"], "PERSONA_EVAL")

    def test_a_record_missing_required_keys_is_rejected(self) -> None:
        with self.assertRaises(self.extractor.ExtractionError):
            self.extractor.extract_records(
                ['PERSONA_EVAL {"agent_type": "x"}'], "PERSONA_EVAL"
            )

    def test_the_command_line_enforces_counts_and_required_values(self) -> None:
        import tempfile

        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "eval.log"
            out = Path(directory) / "eval.jsonl"
            log.write_text(
                "test tests::x ... PERSONA_EVAL "
                + self.persona_record("a")
                + "\nPERSONA_EVAL "
                + self.persona_record("b")
                + "\n",
                encoding="utf-8",
            )
            argv = ["--marker", "PERSONA_EVAL", "--log", str(log), "--out", str(out)]
            self.assertEqual(self.extractor.main(argv + ["--expect", "2"]), 0)
            self.assertEqual(len(out.read_text(encoding="utf-8").splitlines()), 2)
            self.assertEqual(self.extractor.main(argv + ["--expect", "3"]), 1)
            self.assertEqual(
                self.extractor.main(argv + ["--require", "agent_type=a"]), 0
            )
            self.assertEqual(
                self.extractor.main(argv + ["--require", "agent_type=missing"]), 1
            )

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
