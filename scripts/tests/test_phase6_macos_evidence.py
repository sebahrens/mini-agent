#!/usr/bin/env python3
"""Behavior tests for the macOS Phase 6 gate-evidence generator."""

from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = REPOSITORY_ROOT / ".github" / "workflows" / "ci.yml"


def _load():
    path = REPOSITORY_ROOT / "scripts" / "phase6_macos_evidence.py"
    spec = importlib.util.spec_from_file_location("phase6_macos_evidence", path)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


class Phase6MacosEvidenceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.module = _load()

    def test_a_successful_macos_26_run_reports_an_available_worker(self) -> None:
        evidence = self.module.build_evidence(
            "macos-26", "success", "passed", "passed", "recorded"
        )
        containment = evidence["containment"]
        self.assertEqual(containment["worker_availability"], "available")
        self.assertEqual(containment["assurance"], "deprecated-best-effort")
        self.assertNotEqual(
            containment["native_resource_gate"], "unavailable-no-production-worker"
        )
        self.assertEqual(containment["resource_measurement"], "recorded")

    def test_a_runner_without_the_resource_step_is_marked_skipped_not_failed(self) -> None:
        evidence = self.module.build_evidence(
            "macos-15", "success", "passed", "passed", "skipped-unsupported-runner"
        )
        containment = evidence["containment"]
        self.assertEqual(containment["resource_measurement"], "skipped-unsupported-runner")
        self.assertEqual(containment["worker_availability"], "available")

    def test_an_unsupported_host_reports_no_production_worker(self) -> None:
        evidence = self.module.build_evidence(
            "macos-15", "success", "unsupported-host", "passed", "not-run"
        )
        containment = evidence["containment"]
        self.assertEqual(containment["worker_availability"], "unavailable")
        self.assertEqual(containment["assurance"], "fail-closed-unavailable")
        self.assertEqual(
            containment["native_resource_gate"], "unavailable-no-production-worker"
        )

    def test_a_refused_preflight_does_not_claim_containment_assurance(self) -> None:
        for probe in ("failed", "not-run"):
            with self.subTest(probe=probe):
                evidence = self.module.build_evidence(
                    "macos-26", "failure", probe, "not-run", "not-run"
                )
                containment = evidence["containment"]
                self.assertEqual(containment["worker_availability"], "unknown")
                self.assertNotIn("deprecated-best-effort", containment["assurance"])
                self.assertEqual(containment["production_binary_matrix"], probe)

    def test_measured_resources_cannot_coexist_with_no_production_worker(self) -> None:
        with self.assertRaises(self.module.InconsistentEvidence):
            self.module.validate(
                {
                    "resource_measurement": "recorded",
                    "native_resource_gate": "unavailable-no-production-worker",
                    "worker_availability": "unavailable",
                    "real_probe": "passed",
                    "assurance": "fail-closed-unavailable",
                }
            )

    def test_assurance_is_never_claimed_without_a_passing_probe(self) -> None:
        with self.assertRaises(self.module.InconsistentEvidence):
            self.module.validate(
                {
                    "resource_measurement": "not-run",
                    "native_resource_gate": "unavailable-no-native-mechanism",
                    "worker_availability": "unknown",
                    "real_probe": "failed",
                    "assurance": "deprecated-best-effort",
                }
            )

    def test_the_command_line_writes_both_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            json_out = Path(directory) / "phase6-gate-evidence.json"
            log_out = Path(directory) / "phase6-gate-evidence.log"
            code = self.module.main(
                [
                    "--runner",
                    "macos-26",
                    "--job-status",
                    "success",
                    "--probe",
                    "passed",
                    "--adversarial",
                    "passed",
                    "--resource",
                    "recorded",
                    "--json-out",
                    str(json_out),
                    "--log-out",
                    str(log_out),
                ]
            )
            self.assertEqual(code, 0)
            evidence = json.loads(json_out.read_text(encoding="utf-8"))
            self.assertEqual(evidence["platform"], "macos")
            self.assertFalse(evidence["raw_output"], "evidence must stay source-free")
            summary = log_out.read_text(encoding="utf-8")
            self.assertIn("worker=available", summary)
            self.assertIn("raw_output=false", summary)

    def test_the_workflow_uses_the_generator_instead_of_hard_coded_fields(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        macos = workflow.split("  macos-worker-containment-gate:", 1)[1]
        self.assertIn("scripts/phase6_macos_evidence.py", macos)
        self.assertNotIn("deprecated-best-effort-fail-closed", macos)
        self.assertNotIn(
            "native_resource_gate = 'unavailable-no-production-worker'", macos
        )
        self.assertIn("skipped-unsupported-runner", macos)


if __name__ == "__main__":
    unittest.main()
