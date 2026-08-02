#!/usr/bin/env python3
"""Regression tests for the aggregate Phase 6 cross-platform CI gate."""

from __future__ import annotations

import re
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = REPOSITORY_ROOT / ".github" / "workflows" / "ci.yml"


def job_body(workflow: str, name: str) -> str:
    match = re.search(
        rf"(?ms)^  {re.escape(name)}:\n(?P<body>.*?)(?=^  [a-zA-Z0-9_-]+:\n|\Z)",
        workflow,
    )
    if match is None:
        raise AssertionError(f"workflow job {name!r} is missing")
    return match.group("body")


class Phase6CiWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.workflow = WORKFLOW.read_text(encoding="utf-8")

    def test_integration_branch_runs_the_required_delivery_gate(self) -> None:
        push_header = self.workflow.split("pull_request:", 1)[0]
        self.assertIn("      - main\n", push_header)
        self.assertIn("      - phase6-integration\n", push_header)

    def test_each_platform_gate_runs_real_probe_and_both_feature_rows(self) -> None:
        requirements = {
            "linux-sandbox-policy": (
                "linux_js_worker_containment",
                "ubuntu-latest",
            ),
            "macos-worker-containment-gate": (
                "macos_js_worker_containment",
                "macos-15",
            ),
            "windows-worker-containment-gate": (
                "windows_js_worker_containment",
                "windows-latest",
            ),
        }
        for job, (probe, runner) in requirements.items():
            with self.subTest(job=job):
                body = job_body(self.workflow, job)
                self.assertIn(runner, body)
                self.assertIn(probe, body)
                self.assertRegex(body, rf"(?s){probe}.+Count.+(?:-eq|-ne) 1|count.+-eq 1")
                self.assertIn(
                    "cargo test --locked --no-default-features --features js\n",
                    body,
                )
                self.assertIn(
                    "cargo test --locked --no-default-features --features skills\n",
                    body,
                )
                self.assertEqual(
                    0,
                    body.count("continue-on-error: true"),
                )

    def test_every_platform_rejects_zero_adversarial_suite_discovery(self) -> None:
        required_categories = (
            "worker_protocol",
            "worker_runtime",
            "worker_supervisor",
            "worker_broker",
            "js_effect_audit",
            "worker_fault_matrix",
            "skill_realm_isolation",
            "capability_manifest_v2",
            "worker_verifier",
            "skill_held_out_evaluator",
            "skill_admission_gate",
            "javascript_worker_status",
        )
        for job in (
            "linux-sandbox-policy",
            "macos-worker-containment-gate",
            "windows-worker-containment-gate",
        ):
            with self.subTest(job=job):
                body = job_body(self.workflow, job)
                for category in required_categories:
                    self.assertIn(category, body)
                self.assertIn("-- --list", body)
                self.assertIn("must execute at least one test", body)

    def test_each_platform_uploads_only_closed_source_free_phase6_evidence(self) -> None:
        for job in (
            "linux-sandbox-policy",
            "macos-worker-containment-gate",
            "windows-worker-containment-gate",
        ):
            with self.subTest(job=job):
                body = job_body(self.workflow, job)
                self.assertIn("name: Write source-free Phase 6 gate evidence", body)
                self.assertIn("if: always()", body)
                self.assertIn("raw_output = $false", body)
                self.assertIn("phase6-gate-evidence.json", body)
                self.assertIn("phase6-gate-evidence.log", body)
                self.assertIn("if-no-files-found: error", body)
                phase6_upload = body.split(
                    "name: Archive source-free Phase 6 gate evidence", 1
                )[1]
                self.assertNotIn("worker-containment.txt", phase6_upload)
                self.assertNotIn("print-config.txt", phase6_upload)

    def test_windows_gate_runs_and_archives_a_separate_non_admin_probe(self) -> None:
        body = job_body(self.workflow, "windows-worker-containment-gate")
        for required in (
            "New-LocalUser",
            "Get-LocalGroupMember -Group 'Administrators'",
            "Start-Process",
            "-Credential",
            "PHASE6_STANDARD_USER=passed",
            "phase6-standard-user-evidence.json",
            "phase6-standard-user-evidence.log",
            "new-allowlisted-non-admin",
            "raw_output = $false",
        ):
            self.assertIn(required, body)
        self.assertIn("windows_js_worker_containment", body)
        standard_user_step = body.split(
            "name: Validate the complete gate from a separate standard-user installation",
            1,
        )[1].split("name: Run Phase 6 adversarial suites", 1)[0]
        self.assertNotIn("continue-on-error", standard_user_step)
        self.assertIn(
            "[System.Security.Principal.WindowsIdentity]::GetCurrent()",
            standard_user_step,
        )
        self.assertIn("IsInRole", standard_user_step)
        self.assertIn("-ExpectedAccount", standard_user_step)
        self.assertIn("exit 10", standard_user_step)
        self.assertIn("exit 11", standard_user_step)
        self.assertIn("exit 12", standard_user_step)
        self.assertIn("exit 13", standard_user_step)
        self.assertIn("-UseNewEnvironment", standard_user_step)
        self.assertIn("-WorkingDirectory $standardRoot", standard_user_step)
        self.assertIn("[Parameter(Mandatory)][string] $UserHome", standard_user_step)
        self.assertIn("[Parameter(Mandatory)][string] $UserTemp", standard_user_step)
        for variable in ("HOME", "USERPROFILE", "TEMP", "TMP"):
            self.assertIn(
                f"[Environment]::SetEnvironmentVariable('{variable}'",
                standard_user_step,
            )
        for forbidden in ("ACTIONS_", "GITHUB_", "RUNNER_", "MINI_AGENT_"):
            self.assertIn(forbidden, standard_user_step)

    def test_each_platform_runs_and_uploads_the_a32_resource_hook(self) -> None:
        for job in (
            "linux-sandbox-policy",
            "macos-worker-containment-gate",
            "windows-worker-containment-gate",
        ):
            with self.subTest(job=job):
                body = job_body(self.workflow, job)
                self.assertIn("MINI_AGENT_JS_WORKER_BENCH", body)
                self.assertIn("MINI_AGENT_JS_WORKER_BENCH_EXE", body)
                self.assertIn("MINI_AGENT_JS_WORKER_BENCH_OUTPUT", body)
                self.assertIn("MINI_AGENT_JS_WORKER_BENCH_COMPARE", body)
                self.assertIn(
                    "cargo install --locked --path . --debug --no-default-features --features js",
                    body,
                )
                self.assertIn("js_worker_resource_benchmark", body)
                self.assertIn("-- --ignored --nocapture", body)
                self.assertRegex(
                    body,
                    r"js-worker-(?:\$\{RUNNER_OS\}|\$env:RUNNER_OS)-reference\.json",
                )
                self.assertIn("js-worker-${{ runner.os }}.json", body)
                self.assertIn("name: js-worker-resource-${{ runner.os }}", body)
                self.assertIn("if-no-files-found: error", body)
                self.assertNotIn("continue-on-error: true", body)
                self.assertIn("PHASE6_RESOURCE=recorded", body)

    def test_one_aggregate_job_requires_all_platform_results(self) -> None:
        body = job_body(self.workflow, "phase6-cross-platform-gate")
        self.assertIn("if: always()", body)
        for dependency in (
            "linux-sandbox-policy",
            "macos-worker-containment-gate",
            "windows-worker-containment-gate",
        ):
            self.assertIn(dependency, body)
        self.assertIn('"$result" != \'success\'', body)

    def test_aggregate_job_validates_exactly_three_resource_records(self) -> None:
        body = job_body(self.workflow, "phase6-cross-platform-gate")
        self.assertRegex(
            body,
            r"actions/download-artifact@[0-9a-f]{40}",
        )
        self.assertIn("pattern: js-worker-resource-*", body)
        self.assertIn("merge-multiple: true", body)
        for platform in ("Linux", "macOS", "Windows"):
            with self.subTest(platform=platform):
                path = f"js-worker-resources/js-worker-{platform}.json"
                self.assertIn(path, body)
                self.assertIn(f'test -f "${{RUNNER_TEMP}}/{path}"', body)
        self.assertIn("MINI_AGENT_JS_WORKER_BENCH_INPUTS", body)
        self.assertIn("js_worker_resource_aggregate", body)
        self.assertIn("name: js-worker-resource-baseline", body)
        self.assertIn("js-worker-baseline.json", body)
        self.assertIn("if-no-files-found: error", body)


if __name__ == "__main__":
    unittest.main()
