#!/usr/bin/env python3
"""Regression tests for the aggregate Phase 6 cross-platform CI gate."""

from __future__ import annotations

import re
import json
import os
import subprocess
import sys
import tempfile
import unittest
import shlex
from pathlib import Path

from scripts import check_feature_graph


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
    SHARED_ADVERSARIAL_FILTERS = (
        "extras::js::tests::worker_protocol::",
        "extras::js::tests::worker_runtime::worker_runtime_",
        "extras::js::tests::worker_runtime::worker_supervisor_",
        "extras::js::tests::worker_broker::worker_broker_",
        "extras::js::tests::worker_broker::js_effect_audit_",
        "extras::js::tests::worker_fault_matrix::",
        "extras::js::supervisor::worker_exit_tests::",
        "agent::builder::js_tests::unavailable_worker_containment_",
        "print::tests::config_reports_",
    )
    SKILLS_ADVERSARIAL_FILTERS = (
        "extras::js::tests::skill_realm_isolation::",
        "extras::js::tests::capability_manifest_v2::",
        "extras::js::tests::worker_runtime::worker_runtime_verification_",
        "extras::js::tests::skill_held_out_evaluator::",
        "extras::js::tests::skill_admission_gate::",
    )

    @classmethod
    def setUpClass(cls) -> None:
        cls.workflow = WORKFLOW.read_text(encoding="utf-8")

    def test_integration_branch_runs_the_required_delivery_gate(self) -> None:
        push_header = self.workflow.split("pull_request:", 1)[0]
        self.assertIn("      - main\n", push_header)
        self.assertIn("      - phase6-integration\n", push_header)

    def test_manual_windows_general_probe_dispatch_skips_every_other_job(self) -> None:
        dispatch_header = self.workflow.split("  push:", 1)[0]
        self.assertIn("scope:", dispatch_header)
        self.assertIn("windows-general-sandbox", dispatch_header)

        jobs = re.findall(
            r"(?m)^  ([a-zA-Z0-9_-]+):$",
            self.workflow.split("jobs:\n", 1)[1],
        )
        self.assertIn("windows-general-sandbox-policy", jobs)
        for job in jobs:
            condition = job_body(self.workflow, job).splitlines()[0].strip()
            with self.subTest(job=job):
                if job == "changes":
                    # The change detector is infrastructure for every other
                    # job, including the Windows probe, which reads its output.
                    # Guarding it by scope would skip it on a probe dispatch and
                    # take the probe down with it, so it runs unconditionally
                    # and reports code=true for every dispatch.
                    self.assertNotIn("inputs.scope", condition)
                elif job == "windows-general-sandbox-policy":
                    self.assertIn("inputs.scope == 'windows-general-sandbox'", condition)
                else:
                    self.assertIn("inputs.scope != 'windows-general-sandbox'", condition)

        windows_job = job_body(self.workflow, "windows-general-sandbox-policy")
        source_step = windows_job.split(
            "name: Test AppContainer source and capability policy", 1
        )[1].split("- name:", 1)[0]
        self.assertIn("inputs.scope != 'windows-general-sandbox'", source_step)
        native_step = windows_job.split(
            "name: Test native general-sandbox policy and recovery", 1
        )[1].split("- name:", 1)[0]
        self.assertNotIn("if:", native_step)
        self.assertIn("$suite = 'sandbox::windows::tests::'", native_step)
        self.assertIn("general_preflight_cache_retains_success_and_failure_without_reprobing", native_step)
        self.assertIn("next_process_recovers_a_preserved_profile_job_acls_and_root", native_step)
        self.assertIn("timed_out_general_preflight_reaps_tree_and_removes_recovery_state", native_step)
        self.assertIn("-- --list", native_step)
        self.assertIn(".Count -ne 1", native_step)
        self.assertIn("cargo test --locked $suite -- --test-threads=1", native_step)
        self.assertNotIn("cargo test --locked $test --", native_step)
        self.assertLess(
            windows_job.index("name: Install the debug binary used by the native sandbox probe"),
            windows_job.index("name: Test native general-sandbox policy and recovery"),
        )
        self.assertIn("$env:MINI_AGENT_TEST_WINDOWS_SANDBOX_EXE = $env:WINDOWS_GENERAL_SANDBOX_EXE", native_step)

    def test_each_platform_gate_runs_real_probe_and_both_feature_rows(self) -> None:
        requirements = {
            "linux-sandbox-policy": (
                "linux_js_worker_containment",
                "ubuntu-latest",
            ),
            "macos-worker-containment-gate": (
                "MINI_AGENT_INTERNAL_MACOS_HOSTED_LIFECYCLE=production-binary-v1",
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
                if job == "macos-worker-containment-gate":
                    self.assertIn("MACOS_CONTAINMENT_MATRIX_V1=passed", body)
                else:
                    self.assertRegex(
                        body,
                        rf"(?s){probe}.+Count.+(?:-eq|-ne) 1|count.+-eq 1",
                    )
                self.assertRegex(body.lower(), r"shared_?suites")
                self.assertRegex(body.lower(), r"skills_?suites")
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
            "worker_exit",
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
                before_guard, after_guard = body.split(
                    "must execute at least one test", 1
                )
                discovery = (
                    before_guard.rsplit("- name:", 1)[1]
                    + "must execute at least one test"
                    + after_guard.split("- name:", 1)[0]
                )
                guard_calls = re.findall(
                    r"(?m)^\s+(?:require_suite|Assert-Suite)\s+(.+)$",
                    discovery,
                )
                self.assertEqual(len(required_categories), len(guard_calls))
                for category in required_categories:
                    matching_calls = [
                        call
                        for call in guard_calls
                        if re.search(rf"(?:^|[ '\"]){re.escape(category)}(?:$|[ '\"])", call)
                    ]
                    self.assertEqual(1, len(matching_calls), category)
                self.assertIn("-- --list", discovery)
                self.assertIn("must execute at least one test", discovery)

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
                # macOS derives its evidence from the actual probe results in
                # scripts/phase6_macos_evidence.py, which sets raw_output there;
                # the other platforms still inline the PowerShell writer.
                if job == "macos-worker-containment-gate":
                    self.assertIn("scripts/phase6_macos_evidence.py", body)
                else:
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
        self.assertIn("$installedBinary --print-config", standard_user_step)
        self.assertIn("$requiredStatus", standard_user_step)
        self.assertIn("public_cached_status = 'available-enforced'", standard_user_step)
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

    def test_phase6_jobs_clear_setup_rust_warning_injection(self) -> None:
        workflow_header = self.workflow.split("jobs:", 1)[0]
        self.assertIn("env:\n  RUSTFLAGS: ''", workflow_header)

        for job in (
            "linux-sandbox-policy",
            "macos-worker-containment-gate",
            "windows-worker-launcher-unit",
            "windows-worker-containment-gate",
            "phase6-cross-platform-gate",
        ):
            with self.subTest(job=job):
                body = job_body(self.workflow, job)
                install = body.split("name: Install Rust", 1)[1].split("- name:", 1)[0]
                self.assertIn("rustflags: ''", install)

        self.assertNotIn("rustflags: ''", job_body(self.workflow, "clippy"))
        commands = check_feature_graph._workflow_run_commands(self.workflow, "clippy")
        command = next(command for command in commands if "cargo clippy" in command)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            recorded = root / "arguments.json"
            cargo = root / "cargo"
            cargo.write_text(
                f"#!{sys.executable}\n"
                "import json,os,sys\n"
                "from pathlib import Path\n"
                "Path(os.environ['CARGO_TRACE']).write_text(json.dumps(sys.argv[1:]))\n"
                "sys.exit(int(os.environ['CARGO_STUB_EXIT']))\n"
            )
            cargo.chmod(0o755)
            for features in check_feature_graph.workflow_matrix_values(self.workflow, "clippy"):
                for exit_code in (0, 7):
                    with self.subTest(features=features, exit_code=exit_code):
                        result = subprocess.run(
                            ["bash", "-e", "-o", "pipefail", "-c",
                             command.replace("${{ matrix.features }}", features)],
                            env={**os.environ, "PATH": str(root) + os.pathsep + os.environ["PATH"],
                                 "CARGO_TRACE": str(recorded), "CARGO_STUB_EXIT": str(exit_code)},
                            capture_output=True, text=True, timeout=5,
                        )
                        self.assertEqual(result.returncode, exit_code, result.stderr)
                        expected = ["clippy", "--locked", "--all-targets", *shlex.split(features), "--", "-D", "warnings"]
                        self.assertEqual(json.loads(recorded.read_text()), expected)

    def test_phase6_adversarial_suites_use_isolated_serialized_processes(self) -> None:
        for job in (
            "linux-sandbox-policy",
            "macos-worker-containment-gate",
            "windows-worker-containment-gate",
        ):
            with self.subTest(job=job):
                body = job_body(self.workflow, job)
                adversarial = body.split(
                    "name: Run Phase 6 adversarial suites under js and skills", 1
                )[1].split("- name:", 1)[0]
                inventories = {}
                for inventory in ("shared", "skills"):
                    match = re.search(
                        rf"(?ms)(?:\${inventory}Suites|{inventory}_suites)\s*=\s*@?\((?P<body>.*?)^\s*\)",
                        adversarial,
                    )
                    self.assertIsNotNone(match, inventory)
                    inventories[inventory] = tuple(
                        re.findall(r"'([^']+)'", match.group("body"))
                    )
                self.assertEqual(
                    self.SHARED_ADVERSARIAL_FILTERS,
                    inventories["shared"],
                )
                self.assertEqual(
                    self.SKILLS_ADVERSARIAL_FILTERS,
                    inventories["skills"],
                )
                self.assertNotRegex(
                    adversarial,
                    r"--features (?:js|skills) -- --test-threads=1",
                )
                self.assertEqual(2, adversarial.count("cargo test"))
                self.assertEqual(2, adversarial.count("-- --test-threads=1"))
                self.assertRegex(
                    adversarial,
                    r"(?s)(?:for|foreach).+shared.+(?:for|foreach).+cargo test",
                )
                self.assertRegex(
                    adversarial,
                    r"(?s)(?:for|foreach).+skills.+cargo test",
                )

    def test_matrix_isolates_process_global_worker_and_skill_tests(self) -> None:
        body = job_body(self.workflow, "test")
        full_path = (
            "extras::js::tool::js_permission_bridge::"
            "js_supervisor_agent_rebuild_reuses_worker_and_stops_old_permission_receiver"
        )
        linux_step = body.split(
            "name: Test (${{ matrix.features }}) on Linux", 1
        )[1].split("- name:", 1)[0]
        self.assertIn("matrix.os != 'macos-latest'", linux_step)
        self.assertIn("-- --test-threads=1", linux_step)
        self.assertIn(
            "--skip tests::harness_eval_tests::"
            "task_json_library_axis_uses_real_store_and_records_oracles",
            linux_step,
        )
        self.assertIn(
            "--skip extras::js::tests::skill_runtime_binding",
            linux_step,
        )

        macos_step = body.split(
            "name: Test (${{ matrix.features }}) on macOS with process-global worker isolation",
            1,
        )[1].split("- name:", 1)[0]
        self.assertIn("matrix.os == 'macos-latest'", macos_step)
        self.assertIn("-- --test-threads=1", macos_step)
        self.assertIn(f"--skip {full_path}", macos_step)
        self.assertIn(
            "--skip extras::js::tests::skill_runtime_binding",
            macos_step,
        )
        self.assertIn(
            "--skip tests::harness_eval_tests::"
            "task_json_library_axis_uses_real_store_and_records_oracles",
            macos_step,
        )

        binding_step = body.split(
            "name: Test skill runtime binding in a fresh process", 1
        )[1].split("- name:", 1)[0]
        self.assertIn("contains(matrix.features, 'skills')", binding_step)
        self.assertIn("RUST_MIN_STACK: 8388608", binding_step)
        self.assertIn("extras::js::tests::skill_runtime_binding", binding_step)
        self.assertIn("-- --test-threads=1", binding_step)
        # The module filter must run all binding cases in one invocation. Per-case skips or
        # an exact filter would silently lose the identity/ABI/differential boundary coverage.
        self.assertEqual(binding_step.count("cargo test --locked"), 1)
        self.assertNotIn("--skip", binding_step)
        self.assertNotIn("--exact", binding_step)

        isolated_step = body.split(
            "name: Test macOS agent-rebuild worker reuse in a fresh process", 1
        )[1].split("- name:", 1)[0]
        self.assertIn("matrix.os == 'macos-latest'", isolated_step)
        self.assertIn("matrix.features == ''", isolated_step)
        self.assertIn("contains(matrix.features, 'js')", isolated_step)
        self.assertIn("contains(matrix.features, 'skills')", isolated_step)
        self.assertIn(full_path, isolated_step)
        self.assertIn("-- --exact --test-threads=1", isolated_step)

        task_step = body.split(
            "name: Test task-level skill eval in a fresh process", 1
        )[1].split("- name:", 1)[0]
        self.assertIn("contains(matrix.features, 'skills')", task_step)
        self.assertIn("contains(matrix.features, 'sandbox')", task_step)
        self.assertIn("contains(matrix.features, 'subagents')", task_step)
        self.assertIn(
            "tests::harness_eval_tests::"
            "task_json_library_axis_uses_real_store_and_records_oracles",
            task_step,
        )
        self.assertIn("-- --exact --test-threads=1", task_step)

    def test_large_async_test_futures_have_a_cross_platform_stack_budget(self) -> None:
        matrix_header = job_body(self.workflow, "test").split("    steps:\n", 1)[0]
        all_features_header = job_body(self.workflow, "all-features").split(
            "    steps:\n", 1
        )[0]
        for header in (matrix_header, all_features_header):
            self.assertIn("RUST_MIN_STACK: 8388608", header)

        all_features = job_body(self.workflow, "all-features")
        main_step = all_features.split("name: Test all features", 1)[1].split(
            "- name:", 1
        )[0]
        self.assertIn("-- --test-threads=1", main_step)
        self.assertIn(
            "--skip tests::harness_eval_tests::"
            "task_json_library_axis_uses_real_store_and_records_oracles",
            main_step,
        )
        self.assertIn(
            "--skip extras::acp::protocol_tests::"
            "cancellation_reaps_configured_async_user_prompt_hook",
            main_step,
        )
        hook_step = all_features.split(
            "name: Test all-features async hook cancellation in a fresh process", 1
        )[1].split("- name:", 1)[0]
        self.assertIn("--all-features", hook_step)
        self.assertIn(
            "extras::acp::protocol_tests::"
            "cancellation_reaps_configured_async_user_prompt_hook",
            hook_step,
        )
        self.assertIn("-- --exact --test-threads=1", hook_step)
        isolated_step = all_features.split(
            "name: Test all-features task-level skill eval in a fresh process", 1
        )[1].split("- name:", 1)[0]
        self.assertIn("--all-features", isolated_step)
        self.assertIn(
            "tests::harness_eval_tests::"
            "task_json_library_axis_uses_real_store_and_records_oracles",
            isolated_step,
        )
        self.assertIn("-- --exact --test-threads=1", isolated_step)

    def test_windows_default_suite_has_a_strict_compile_gate(self) -> None:
        body = job_body(self.workflow, "windows-default-compile")
        header = body.split("    steps:\n", 1)[0]
        self.assertIn("timeout-minutes: 30", header)
        self.assertNotIn("continue-on-error", header)
        step = body.split("name: Compile the Windows default feature suite", 1)[
            1
        ].split("- name:", 1)[0]
        self.assertIn("cargo test --locked --no-run", step)

    @staticmethod
    def matrix_features(body: str) -> list[str]:
        block = body.split("        features:\n", 1)[1]
        rows = []
        for line in block.splitlines():
            stripped = line.strip()
            if stripped.startswith("#"):
                continue
            if not stripped.startswith("- "):
                break
            rows.append(stripped[2:].strip().strip('"'))
        return rows

    def test_windows_compiles_and_runs_every_hooks_row(self) -> None:
        # mini-agent-2dp1m: a Windows-only hooks compile break was invisible
        # to CI because no Windows job enabled `hooks`.
        body = job_body(self.workflow, "windows-hooks-compile")
        header = body.split("    steps:\n", 1)[0]
        self.assertIn("runs-on: windows-latest", header)
        self.assertIn("timeout-minutes:", header)
        self.assertNotIn("continue-on-error", header)
        rows = self.matrix_features(body)
        test_rows = self.matrix_features(job_body(self.workflow, "test"))
        hook_rows = [row for row in test_rows if "hooks" in row.split("--features", 1)[-1]]
        self.assertTrue(hook_rows)
        self.assertEqual(sorted(rows), sorted(hook_rows))
        compile_step = body.split("name: Compile the Windows hooks feature suite", 1)[
            1
        ].split("- name:", 1)[0]
        self.assertIn("shell: bash", compile_step)
        self.assertIn("cargo test --locked ${{ matrix.features }} --no-run", compile_step)
        run_step = body.split("name: Test Windows trusted hook tree termination", 1)[
            1
        ].split("- name:", 1)[0]
        self.assertIn("shell: bash", run_step)
        self.assertIn("hook_subprocess_windows_", run_step)
        self.assertIn("-- --list", run_step)
        self.assertIn('-lt 1', run_step)

    def test_parallel_smoke_runs_the_default_suite_unserialised_on_linux_and_macos(
        self,
    ) -> None:
        # mini-agent-o55tm: every `test` row passes --test-threads=1, so a
        # readiness race that only appears under the parallel runner on macOS
        # was invisible to CI while local `cargo test` hit it.
        body = job_body(self.workflow, "parallel-test-smoke")
        header = body.split("    steps:\n", 1)[0]
        self.assertIn("fail-fast: false", header)
        self.assertIn("os: [ubuntu-latest, macos-latest]", header)
        self.assertIn("runs-on: ${{ matrix.os }}", header)
        self.assertNotIn("continue-on-error", header)
        step = body.split(
            "name: Test the default suite with the standard parallel runner", 1
        )[1].split("- name:", 1)[0]
        self.assertIn("run: cargo test --locked\n", step)
        self.assertNotIn("--test-threads", step)
        self.assertNotIn("--skip", step)
        self.assertLess(
            body.index("name: Use the durable macOS temporary root"),
            body.index("name: Test the default suite with the standard parallel runner"),
        )

    def test_every_test_row_has_a_strict_linux_clippy_row(self) -> None:
        # A test row compiles with warnings allowed, so a cfg-gated item that is
        # dead on Linux under that exact feature set only fails under Clippy.
        clippy = job_body(self.workflow, "clippy")
        self.assertIn("runs-on: ubuntu-latest", clippy)
        self.assertIn(
            "cargo clippy --locked --all-targets ${{ matrix.features }} -- -D warnings",
            clippy,
        )
        clippy_rows = set(self.matrix_features(clippy))
        test_rows = self.matrix_features(job_body(self.workflow, "test"))
        self.assertGreater(len(test_rows), 5)
        missing = [row for row in test_rows if row not in clippy_rows]
        self.assertEqual([], missing, "test rows without a strict Clippy row")

    def test_linux_sandbox_policy_proves_hooks_have_no_controlling_terminal(
        self,
    ) -> None:
        linux = job_body(self.workflow, "linux-sandbox-policy")
        # The ignored real-bwrap probe is only meaningful when the test process
        # owns a controlling terminal, which script(1) provides on the runner.
        self.assertIn(
            'script -qec "cargo test --locked --no-default-features '
            "--features hooks,js,subagents,skills,sandbox "
            "sandbox::sandbox_tests::bwrap_hook_has_no_controlling_terminal "
            '-- --exact --ignored --nocapture" /dev/null',
            linux,
        )
        self.assertLess(
            linux.index("bash scripts/install-ci-bubblewrap.sh"),
            linux.index("bwrap_hook_has_no_controlling_terminal"),
        )

    def test_hosted_platform_prerequisites_preserve_real_security_gates(self) -> None:
        linux = job_body(self.workflow, "linux-sandbox-policy")
        self.assertIn("bash scripts/install-ci-bubblewrap.sh", linux)
        self.assertIn("kernel.apparmor_restrict_unprivileged_userns", linux)
        self.assertIn(
            "sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0",
            linux,
        )

        macos = job_body(self.workflow, "macos-worker-containment-gate")
        self.assertIn("TMPDIR: /private/tmp", macos)
        for job in (
            "test",
            "parallel-test-smoke",
            "platform-paths",
            "platform-path-install-smoke",
            "mcp-stdio",
            "install",
        ):
            with self.subTest(job=job):
                body = job_body(self.workflow, job)
                self.assertIn("name: Use the durable macOS temporary root", body)
                self.assertIn("if: matrix.os == 'macos-latest'", body)
                self.assertIn(
                    "echo 'TMPDIR=/private/tmp' >> \"$GITHUB_ENV\"", body
                )
                self.assertNotIn("runner.os", body)

        windows = job_body(self.workflow, "windows-worker-containment-gate")
        install_step = windows.split(
            "name: Install the exact JS binary for Unicode-path containment cases",
            1,
        )[1].split("- name:", 1)[0]
        self.assertIn("phase6-cargo-home", install_step)
        self.assertNotIn("$env:CARGO_HOME = $installRoot", install_step)
        self.assertIn("MINI_AGENT_LPAC_CARGO_INSTALL_EXE", install_step)

        protected_step = windows.split(
            "name: Prepare the protected machine-wide negative control", 1
        )[1].split("- name:", 1)[0]
        self.assertIn("S-1-15-2-2", protected_step)
        self.assertIn("$packageReadRule", protected_step)
        self.assertIn("FileSystemRights]::ReadAndExecute", protected_step)

        standard_user_step = windows.split(
            "name: Validate the complete gate from a separate standard-user installation",
            1,
        )[1].split("name: Run Phase 6 adversarial suites", 1)[0]
        for path in (
            "phase6-standard-user-build",
            "source-checkout",
            "cargo-home",
            "user-home",
            "user-temp",
        ):
            self.assertIn(path, standard_user_step)
        self.assertIn("S-1-15-2-2", standard_user_step)
        self.assertIn("$restrictedPackagesSid", standard_user_step)
        self.assertIn("FileSystemRights]::ReadAndExecute", standard_user_step)
        self.assertNotIn("phase6 standard user λ", standard_user_step)

    def test_windows_hosted_administrator_status_is_fail_closed(self) -> None:
        body = job_body(self.workflow, "windows-worker-containment-gate")
        status_step = body.split(
            "name: Verify the hosted administrator path remains fail closed",
            1,
        )[1].split("name: Archive Windows containment evidence", 1)[0]
        self.assertIn("--print-config", status_step)
        for claim in (
            "windows-lpac",
            "unavailable",
            "enforced backend class; inactive",
            "disabled; no worker process starts",
            "not active; no worker process",
            "unavailable; no JS worker",
        ):
            with self.subTest(claim=claim):
                self.assertIn(claim, status_step)
        self.assertIn("$evidence -notmatch", status_step)
        self.assertNotIn("continue-on-error", status_step)

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
                    r"js-worker-(?:\$\{RUNNER_OS\}|\$env:RUNNER_OS|Windows)-reference\.json",
                )
                self.assertIn("js-worker-${{ runner.os }}.json", body)
                self.assertIn("name: js-worker-resource-${{ runner.os }}", body)
                self.assertIn("if-no-files-found: error", body)
                self.assertNotIn("continue-on-error: true", body)
                self.assertIn("PHASE6_RESOURCE=recorded", body)

        windows = job_body(self.workflow, "windows-worker-containment-gate")
        standard_user_step = windows.split(
            "name: Validate the complete gate from a separate standard-user installation",
            1,
        )[1].split("name: Archive Windows JS worker resource measurements", 1)[0]
        self.assertIn("MINI_AGENT_JS_WORKER_BENCH", standard_user_step)
        self.assertIn("js-worker-Windows-reference.json", standard_user_step)
        self.assertIn("js-worker-Windows.json", standard_user_step)
        self.assertIn("Copy-Item", standard_user_step)

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


class CiSuccessAggregateTests(unittest.TestCase):
    """`ci-success` is the one required check; it must cover every CI job."""

    @classmethod
    def setUpClass(cls) -> None:
        cls.workflow = WORKFLOW.read_text(encoding="utf-8")
        cls.jobs = re.findall(
            r"(?m)^  ([a-zA-Z0-9_-]+):$", cls.workflow.split("jobs:\n", 1)[1]
        )
        cls.body = job_body(cls.workflow, "ci-success")

    def needs(self) -> list[str]:
        block = self.body.split("    needs:\n", 1)[1].split("    runs-on:", 1)[0]
        return re.findall(r"(?m)^      - ([a-zA-Z0-9_-]+)$", block)

    def test_ci_success_needs_every_other_job(self) -> None:
        self.assertIn("ci-success", self.jobs)
        needs = self.needs()
        self.assertEqual(len(needs), len(set(needs)))
        self.assertEqual(set(self.jobs) - {"ci-success"}, set(needs))

    def test_ci_success_always_runs_on_push_and_pull_request(self) -> None:
        condition = self.body.splitlines()[0].strip()
        self.assertTrue(condition.startswith("if: always() && "), condition)
        self.assertNotIn("needs.changes.outputs.code", condition)
        self.assertNotIn("github.event_name == ", condition)
        self.assertNotIn("'push'", condition)
        self.assertNotIn("'pull_request'", condition)

    def test_ci_success_delegates_to_the_tested_result_policy(self) -> None:
        self.assertIn("CI_NEEDS_JSON: ${{ toJSON(needs) }}", self.body)
        self.assertIn('python3 scripts/ci_success.py --event "$GITHUB_EVENT_NAME"', self.body)
        self.assertNotIn("continue-on-error", self.body)


class CiSuccessPolicyTests(unittest.TestCase):
    def needs(self, code: str, **results: str) -> dict[str, object]:
        jobs = {
            "changes": {"result": "success", "outputs": {"code": code}},
            "fmt": {"result": "success", "outputs": {}},
        }
        default = "success" if code == "true" else "skipped"
        for job in ("test", "clippy", "windows-msi", "harness-regression"):
            jobs[job] = {"result": default, "outputs": {}}
        for job, result in results.items():
            jobs.setdefault(job.replace("_", "-"), {"outputs": {}})["result"] = result
        return jobs

    def evaluate(self, needs: dict[str, object], event: str = "push") -> list[str]:
        from scripts import ci_success

        return ci_success.evaluate(needs, event)

    def test_full_matrix_success_passes(self) -> None:
        for event in ("push", "pull_request", "workflow_dispatch"):
            with self.subTest(event=event):
                self.assertEqual([], self.evaluate(self.needs("true"), event))

    def test_documentation_only_change_accepts_skipped_matrix(self) -> None:
        self.assertEqual([], self.evaluate(self.needs("false")))
        self.assertEqual([], self.evaluate(self.needs("false"), "pull_request"))

    def test_skipped_job_with_code_changes_fails(self) -> None:
        errors = self.evaluate(self.needs("true", test="skipped"))
        self.assertEqual(["test finished with 'skipped'"], errors)

    def test_harness_regression_must_run_whenever_code_changed(self) -> None:
        # harness-regression (the deterministic eval and the only Gym
        # entrypoint smoke) runs on every event, so skipping it on a push is
        # a failure rather than a by-design skip (mini-agent-5casv).
        for event in ("push", "pull_request", "workflow_dispatch"):
            with self.subTest(event=event):
                errors = self.evaluate(
                    self.needs("true", harness_regression="skipped"), event
                )
                self.assertEqual(["harness-regression finished with 'skipped'"], errors)
        self.assertEqual(
            [], self.evaluate(self.needs("false", harness_regression="skipped"), "push")
        )

    def test_failed_or_cancelled_jobs_fail_even_for_documentation(self) -> None:
        for result in ("failure", "cancelled"):
            for code in ("true", "false"):
                with self.subTest(result=result, code=code):
                    errors = self.evaluate(self.needs(code, windows_msi=result))
                    self.assertEqual([f"windows-msi finished with {result!r}"], errors)

    def test_change_detector_and_fmt_must_succeed(self) -> None:
        errors = self.evaluate(self.needs("false", fmt="skipped"))
        self.assertIn("fmt finished with 'skipped'", errors)

        needs = self.needs("false")
        needs["changes"] = {"result": "failure", "outputs": {}}
        errors = self.evaluate(needs)
        self.assertIn("changes finished with 'failure'", errors)
        # Without a trusted docs-only verdict every skipped job is a failure.
        self.assertIn("test finished with 'skipped'", errors)

    def test_invalid_change_detector_output_fails_closed(self) -> None:
        needs = self.needs("true")
        needs["changes"]["outputs"]["code"] = ""
        errors = self.evaluate(needs)
        self.assertTrue(any("invalid code output" in error for error in errors))

    def test_missing_required_dependencies_are_rejected(self) -> None:
        self.assertEqual(["ci-success received no job results"], self.evaluate({}))
        needs = self.needs("true")
        del needs["fmt"]
        self.assertIn("ci-success must depend on fmt", self.evaluate(needs))

    def test_command_line_reads_needs_from_the_environment(self) -> None:
        script = REPOSITORY_ROOT / "scripts" / "ci_success.py"
        for needs, expected in ((self.needs("true"), 0), (self.needs("true", test="failure"), 1)):
            with self.subTest(expected=expected):
                result = subprocess.run(
                    [sys.executable, str(script), "--event", "push"],
                    env={**os.environ, "CI_NEEDS_JSON": json.dumps(needs)},
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(expected, result.returncode, result.stderr)
        result = subprocess.run(
            [sys.executable, str(script), "--event", "push"],
            env={key: value for key, value in os.environ.items() if key != "CI_NEEDS_JSON"},
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(2, result.returncode)


class BubblewrapInstallerTests(unittest.TestCase):
    def test_isolated_sources_and_failures_preserve_the_install_gate(self) -> None:
        for failure in ["none", "update", "install", "probe", "missing"]:
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                sources = root / "ubuntu sources.sources"
                if failure != "missing":
                    sources.write_text("Types: deb\nURIs: https://archive.ubuntu.com/ubuntu\nSuites: noble\nComponents: universe\n")
                log = root / "calls.jsonl"
                binaries = root / "bin"
                binaries.mkdir()
                sudo = binaries / "sudo"
                sudo.write_text('#!/bin/sh\nexec "$@"\n')
                sudo.chmod(0o755)
                command = (
                    f"#!{sys.executable}\nimport json,os,sys\nfrom pathlib import Path\n"
                    "name=Path(sys.argv[0]).name\nargs=sys.argv[1:]\n"
                    "options={}\n"
                    "while args and args[0]=='-o':\n"
                    " key,value=args[1].split('=',1); options[key]=value; args=args[2:]\n"
                    "phase=args[0] if name=='apt-get' else 'probe'\n"
                    f"with open({str(log)!r},'a') as handle: handle.write(json.dumps([phase,options,args])+'\\n')\n"
                    "if name=='apt-get':\n"
                    f" if options!={{'Dir::Etc::sourcelist':{str(sources)!r},'Dir::Etc::sourceparts':'-'}}:\n"
                    "  print('unrelated vendor repository hash mismatch',file=sys.stderr); sys.exit(100)\n"
                    f"if phase=={failure!r}: sys.exit(19)\n"
                    "if name=='bwrap': print('bubblewrap fixture')\n"
                )
                for name in ["apt-get", "bwrap"]:
                    stub = binaries / name
                    stub.write_text(command)
                    stub.chmod(0o755)
                completed = subprocess.run(
                    ["bash", str(REPOSITORY_ROOT / "scripts/install-ci-bubblewrap.sh"), str(sources)],
                    env={**os.environ, "PATH": str(binaries) + os.pathsep + os.environ.get("PATH", "")},
                    capture_output=True, text=True, timeout=5,
                )
                expected_phases = {"none": ["update", "install", "probe"], "update": ["update"],
                                   "install": ["update", "install"], "probe": ["update", "install", "probe"], "missing": []}
                calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
                self.assertEqual([call[0] for call in calls], expected_phases[failure], completed.stderr)
                self.assertEqual(completed.returncode, 0 if failure == "none" else 2 if failure == "missing" else 19,
                                 completed.stderr)
                if len(calls) >= 2:
                    self.assertEqual(calls[1][2], ["install", "-y", "bubblewrap"])
                if failure == "none":
                    self.assertIn("bubblewrap fixture", completed.stdout)


if __name__ == "__main__":
    unittest.main()
