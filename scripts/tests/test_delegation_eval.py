#!/usr/bin/env python3
"""Offline checks for the delegated-versus-direct lane (mini-agent-964ao)."""

from __future__ import annotations

import importlib.util
import os
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "delegation_eval", ROOT / "scripts" / "delegation_eval.py"
)
delegation_eval = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(delegation_eval)


class DelegationEvalTests(unittest.TestCase):
    def test_fixture_tasks_are_well_formed(self) -> None:
        tasks = delegation_eval.load_tasks(delegation_eval.DEFAULT_TASKS)
        self.assertGreaterEqual(len(tasks), 3)
        self.assertEqual(len({task["name"] for task in tasks}), len(tasks))

    def test_record_flattens_usage_and_telemetry(self) -> None:
        record = delegation_eval.record_from_result(
            "t",
            "delegated",
            {
                "num_turns": 4,
                "usage": {"total_tokens": 900},
                "total_cost_usd": 0.02,
                "subagent_delegation": {
                    "children": 2,
                    "contract_pass": 1,
                    "repair_used": 1,
                    "host_replaced": 0,
                    "total_tokens": 500,
                    "cost_units": 30,
                },
            },
            True,
        )
        self.assertEqual(record["children"], 2)
        self.assertEqual(record["child_tokens"], 500)
        direct = delegation_eval.record_from_result("t", "direct", {}, True)
        self.assertEqual(direct["children"], 0)

    def test_config_puts_task_enabled_at_the_top_level(self) -> None:
        for arm, value in (("delegated", "true"), ("direct", "false")):
            text = delegation_eval.config_text(arm)
            self.assertEqual(text, f"task_enabled = {value}\n")
            self.assertNotIn("[", text)

    def test_a_rejected_config_fails_loudly(self) -> None:
        stderr = "Error: config is not valid TOML: unknown field `task_enabled`"
        self.assertIsNotNone(delegation_eval.config_error(stderr, 2))
        self.assertIsNone(delegation_eval.config_error(stderr, 0))
        self.assertIsNone(delegation_eval.config_error("provider failed", 1))

    def test_scratch_root_has_no_symlinked_component(self) -> None:
        root = delegation_eval.scratch_root()
        self.assertEqual(root, os.path.realpath(root))

    def test_record_keeps_the_whole_delegation_aggregate(self) -> None:
        aggregate = {
            "children": 1,
            "contract_pass": 0,
            "contract_repaired": 1,
            "contract_unstructured": 2,
            "contract_normalized": 3,
        }
        record = delegation_eval.record_from_result(
            "t", "delegated", {"subagent_delegation": aggregate}, True
        )
        self.assertEqual(record["subagent_delegation"], aggregate)

    def test_the_independent_questions_prompt_asks_for_the_oracle_text(self) -> None:
        task = next(
            t
            for t in delegation_eval.load_tasks(delegation_eval.DEFAULT_TASKS)
            if t["name"] == "independent_questions"
        )
        for line in task["expected_files"]["answer.txt"].splitlines():
            label = line.split(":")[0]
            self.assertIn(f"`{label}: <count>`", task["prompt"])

    def test_compare_reports_ratios_for_complete_pairs_only(self) -> None:
        delegated = {
            "task": "a", "arm": "delegated", "success": True, "total_tokens": 300,
            "cost_usd": 0.03, "children": 1, "contract_pass": 1, "repair_used": 0,
            "host_replaced": 0,
        }
        direct = dict(delegated, arm="direct", total_tokens=100, cost_usd=0.01, children=0)
        lonely = dict(delegated, task="b")
        rows = delegation_eval.compare([delegated, direct, lonely])
        self.assertEqual([row["task"] for row in rows], ["a"])
        self.assertEqual(rows[0]["token_ratio"], 3.0)

    def test_files_match_checks_exact_content(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            workspace = Path(scratch)
            (workspace / "answer.txt").write_text("42\n", encoding="utf-8")
            self.assertTrue(delegation_eval.files_match(workspace, {"answer.txt": "42\n"}))
            self.assertFalse(delegation_eval.files_match(workspace, {"answer.txt": "43\n"}))
            self.assertFalse(delegation_eval.files_match(workspace, {"missing.txt": "x"}))


if __name__ == "__main__":
    unittest.main()
