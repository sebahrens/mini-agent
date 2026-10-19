#!/usr/bin/env python3
"""Regression tests for the aggregate Phase 6 cross-platform CI gate."""

from __future__ import annotations

import re
import json
import os
import subprocess
import sys
import tempfile
import textwrap
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


def workflow_jobs(workflow: str) -> list[str]:
    return re.findall(r"(?m)^  ([a-zA-Z0-9_-]+):$", workflow.split("jobs:\n", 1)[1])


def top_level_split(expression: str, operator: str) -> list[str]:
    """Split a GitHub Actions expression on `operator` outside parentheses and quotes.

    Operands are stripped. A term that is one fully parenthesised group keeps its
    parentheses, so `a && (b || c)` splits on `&&` into `a` and `(b || c)`.
    """

    terms: list[str] = []
    depth = 0
    quoted = False
    start = 0
    index = 0
    while index < len(expression):
        character = expression[index]
        if character == "'":
            quoted = not quoted
        elif not quoted and character == "(":
            depth += 1
        elif not quoted and character == ")":
            depth -= 1
            if depth < 0:
                raise AssertionError(f"unbalanced parentheses in {expression!r}")
        elif not quoted and depth == 0 and expression.startswith(operator, index):
            terms.append(expression[start:index].strip())
            index += len(operator)
            start = index
            continue
        index += 1
    if depth != 0 or quoted:
        raise AssertionError(f"unbalanced expression {expression!r}")
    terms.append(expression[start:].strip())
    return terms


class ConditionParserTests(unittest.TestCase):
    def test_precedence_bug_is_visible_at_top_level(self) -> None:
        buggy = (
            "needs.changes.outputs.code == 'true' && github.event_name != "
            "'workflow_dispatch' || inputs.scope != 'windows-general-sandbox'"
        )
        self.assertEqual(2, len(top_level_split(buggy, "||")))
        fixed = (
            "needs.changes.outputs.code == 'true' && (github.event_name != "
            "'workflow_dispatch' || inputs.scope != 'windows-general-sandbox')"
        )
        self.assertEqual([fixed], top_level_split(fixed, "||"))
        self.assertEqual(
            [
                "needs.changes.outputs.code == 'true'",
                "(github.event_name != 'workflow_dispatch' || "
                "inputs.scope != 'windows-general-sandbox')",
            ],
            top_level_split(fixed, "&&"),
        )

    def test_quoted_operators_and_parentheses_are_ignored(self) -> None:
        self.assertEqual(
            ["a == '(x || y'", "b"], top_level_split("a == '(x || y' && b", "&&")
        )


class Phase6CiWorkflowTests(unittest.TestCase):
    def test_spawn_parity_gate_batches_only_its_feature_consumers(self) -> None:
        linux = job_body(WORKFLOW.read_text(encoding="utf-8"), "linux-sandbox-policy")
        step = linux.split("- name: Test JS spawn parity through the shared sandbox", 1)[1].split("- name:", 1)[0]
        body = step.split("run: >-\n", 1)[1]
        command = shlex.split(" ".join(line.strip() for line in body.splitlines()))
        self.assertEqual(command[:5], ["cargo", "test", "--locked", "--features", "js,sandbox"])
        self.assertEqual(command[5], "--")
        self.assertEqual(command[6:], [
            "sandbox::", "extras::js::", "agent::builder::js_tests::",
            "tests::sandbox_platform_tests::", "--test-threads=1",
        ])

    def test_native_workspace_gate_keeps_host_policy_and_exact_discovery(self) -> None:
        linux = job_body(self.workflow, "linux-sandbox-policy")
        self.assertIn("runs-on: ubuntu-22.04", linux)
        self.assertIn("scripts/install-ci-bubblewrap.sh --pinned-native", linux)
        self.assertNotIn("sysctl -w", linux)
        self.assertNotIn("--privileged", linux)
        self.assertNotIn("seccomp=unconfined", linux)
        self.assertIn("sysctl kernel.apparmor_restrict_unprivileged_userns", linux)
        self.assertIn('test "${#names[@]}" -eq 7', linux)
        self.assertIn('--exact --ignored --nocapture --test-threads=1', linux)
        self.assertIn("test result: ok. 1 passed;", linux)
        self.assertIn("headless-list.txt", linux)
        self.assertIn("linux_native_shell_and_read_share_the_actual_headless_workspace", linux)
        self.assertIn("name: Archive native Linux workspace proof\n        if: always()", linux)
        self.assertIn("name: Retire owned native workspace fixtures\n        if: always()", linux)
        # Existing worker checks remain enforced on the new native runner.
        self.assertIn("linux_js_worker_containment -- --ignored --nocapture", linux)
        self.assertIn("Run Phase 6 adversarial suites under js and skills", linux)

    def test_native_linux_compiled_cache_is_partitioned_by_pinned_runner_abi(self) -> None:
        linux = job_body(self.workflow, "linux-sandbox-policy")
        runner = re.search(r"(?m)^    runs-on: (\S+)$", linux).group(1)
        self.assertEqual(runner, "ubuntu-22.04")
        rust = linux.split("- name: Install Rust\n", 1)[1].split("- name:", 1)[0]
        self.assertIn("uses: actions-rust-lang/setup-rust-toolchain@", rust)
        cache_key = re.search(r"(?m)^          cache-key: (\S+)$", rust)
        self.assertIsNotNone(cache_key, "compiled cache must not reuse Ubuntu 24 artifacts")
        # The pinned action puts cache-key into BOTH primary and restore prefixes.
        # cache-shared-key would override it and revive cross-image restoration.
        self.assertEqual(cache_key.group(1), runner)
        self.assertNotIn("cache-shared-key:", rust)

    CACHED_COMPILE_JOBS = (
        "clippy", "test", "all-features", "parallel-test-smoke",
        "windows-default-compile", "windows-hooks-compile", "mcp-stdio", "lsp-lifecycle",
    )
    CLEAN_ROOM_CONTAINMENT_JOBS = (
        "macos-worker-containment-gate", "windows-worker-containment-gate",
        "windows-general-sandbox-policy", "phase6-cross-platform-gate",
    )

    def test_compile_jobs_cache_dependencies_partitioned_by_image_and_row(self) -> None:
        for name in self.CACHED_COMPILE_JOBS:
            with self.subTest(job=name):
                body = job_body(self.workflow, name)
                derive = body.split("- name: Derive the compiled-dependency cache key\n", 1)[1]
                derive = derive.split("\n      - name:", 1)[0]
                self.assertIn("id: cache-key", derive)
                # OS, runner image (ImageOS) and the feature row are all in the key.
                self.assertIn("${RUNNER_OS}-${ImageOS:-unknown}-", derive)
                self.assertIn("CACHE_ROW: ${{ matrix.features }}", derive)
                self.assertLess(
                    body.index("Derive the compiled-dependency cache key"),
                    body.index("- name: Install Rust"),
                )
                rust = body.split("- name: Install Rust\n", 1)[1].split("\n      - name:", 1)[0]
                self.assertIn("cache-key: ${{ steps.cache-key.outputs.key }}", rust)
                self.assertNotIn("cache-shared-key:", rust)
                self.assertNotRegex(rust, r"(?m)^          cache: false")

    def test_containment_gates_stay_uncached_for_clean_room_evidence(self) -> None:
        for name in self.CLEAN_ROOM_CONTAINMENT_JOBS:
            with self.subTest(job=name):
                body = job_body(self.workflow, name)
                rust = body.split("- name: Install Rust\n", 1)[1].split("\n      - name:", 1)[0]
                self.assertRegex(rust, r"(?m)^          cache: false$")
                self.assertNotIn("cache-key:", rust)
                self.assertNotIn("actions/cache@", body)

    def test_native_workspace_names_match_the_actual_defining_sources(self) -> None:
        linux = job_body(self.workflow, "linux-sandbox-policy")
        step = linux.split("- name: Test native Linux workspace real paths and snapshot authority", 1)[1].split("- name:", 1)[0]
        names = re.findall(r"^\s+'([^']+::linux_native[^']+)'$", step, re.M)
        actual = set()
        for filename, prefix in [
            ("src/sandbox/linux_workspace_tests.rs", "sandbox::linux_workspace_tests::"),
            ("src/extras/js/host.rs", "extras::js::host::tests::"),
        ]:
            source = (REPOSITORY_ROOT / filename).read_text()
            actual.update(prefix + name for name in re.findall(r"async fn (linux_native_\w+)\(", source))
        self.assertEqual(len(actual), 7)
        self.assertEqual(set(names), actual)
        self.assertEqual(len(names), 7)
        integration = "linux_native_shell_and_read_share_the_actual_headless_workspace"
        source = (REPOSITORY_ROOT / "tests/headless_json.rs").read_text()
        self.assertEqual(len(re.findall(r"(?:async )?fn " + integration + r"\(", source)), 1)
        self.assertIn(integration, step)

    def test_native_workspace_step_rejects_zero_tests_and_false_success(self) -> None:
        linux = job_body(self.workflow, "linux-sandbox-policy")
        step = linux.split("- name: Test native Linux workspace real paths and snapshot authority", 1)[1].split("- name:", 1)[0]
        script = textwrap.dedent(step.split("run: |\n", 1)[1])
        required_names = re.findall(r"^\s+'([^']+::linux_native[^']+)'$", script, re.M)
        self.assertEqual(len(required_names), 7)
        for count, pass_count, wrong_identity, succeeds in [(7, 1, False, True), (0, 1, False, False), (6, 1, False, False), (7, 0, False, False), (7, 1, True, False)]:
            with self.subTest(count=count, pass_count=pass_count), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                binary = root / "bin"
                binary.mkdir()
                cargo = binary / "cargo"
                discovered = required_names[:count]
                if wrong_identity:
                    discovered[-1] = "sandbox::linux_native_wrong_identity"
                cargo.write_text(
                    f"#!{sys.executable}\nimport sys\n"
                    "args=sys.argv[1:]\n"
                    "if '--list' in args:\n"
                    " if '--test' in args: print('linux_native_shell_and_read_share_the_actual_headless_workspace: test')\n"
                    f" else:\n  for name in {discovered!r}: print(name+': test')\n"
                    f"else: print('test result: ok. {pass_count} passed; 0 failed; 0 ignored;')\n"
                )
                cargo.chmod(0o755)
                for name in ["bwrap", "rustc"]:
                    stub = binary / name
                    stub.write_text("#!/bin/sh\nprintf 'fixture version\n'\n")
                    stub.chmod(0o755)
                # This simulator starts eight separate test subprocesses;
                # allow bounded scheduling slack on a busy developer host.
                result = subprocess.run(["bash", "-c", script], cwd=root,
                    env={**os.environ, "PATH": str(binary) + os.pathsep + os.environ.get("PATH", "")},
                    capture_output=True, text=True, timeout=15)
                self.assertEqual(result.returncode == 0, succeeds, result.stderr)
                if succeeds:
                    logs = list((root / "target/native-workspace-evidence").glob("*.log"))
                    self.assertEqual(len(logs), 8)
                    self.assertTrue(all("test result: ok. 1 passed;" in log.read_text() for log in logs))

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

        jobs = workflow_jobs(self.workflow)
        self.assertIn("windows-general-sandbox-policy", jobs)
        skip_probe_scope = (
            "(github.event_name != 'workflow_dispatch' || "
            "inputs.scope != 'windows-general-sandbox')"
        )
        probe_scope = (
            "(github.event_name != 'workflow_dispatch' || inputs.scope == 'full' || "
            "inputs.scope == 'windows-general-sandbox')"
        )
        for job in jobs:
            condition = job_body(self.workflow, job).splitlines()[0].strip()
            with self.subTest(job=job):
                if job == "changes":
                    # The change detector is infrastructure for every other
                    # job, including the Windows probe, which reads its output.
                    # Guarding it by scope would skip it on a probe dispatch and
                    # take the probe down with it, so it runs unconditionally
                    # and reports code=true for the general-sandbox dispatch.
                    self.assertNotIn("inputs.scope", condition)
                    continue
                # Parse the expression rather than substring-match it:
                # `a && b || c` contains the right words but means
                # `(a && b) || c` (mini-agent-6ap52, dependency-policy).
                self.assertTrue(condition.startswith("if: "), condition)
                conjuncts = top_level_split(condition[len("if: "):], "&&")
                for term in conjuncts:
                    self.assertEqual(
                        [term],
                        top_level_split(term, "||"),
                        f"an unparenthesised `||` leaks out of {condition!r}",
                    )
                if job in ("fmt", "ci-success"):
                    expected = "(github.event_name != 'workflow_dispatch' || inputs.scope == 'full')"
                else:
                    expected = probe_scope if job == "windows-general-sandbox-policy" else skip_probe_scope
                self.assertIn(expected, conjuncts, condition)
                if "needs.changes.outputs.code" in condition:
                    expected_code = "needs.changes.outputs.code == 'true'"
                    if job == "windows-default-compile":
                        expected_code = "(needs.changes.outputs.code == 'true' || (github.event_name == 'workflow_dispatch' && (inputs.scope == 'windows-command-family' || inputs.scope == 'windows-session-storage')))"
                    self.assertIn(expected_code, conjuncts)

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
        for regression in (
            "profile_record_publication_process_death_never_exposes_partial_final_authority",
            "profile_record_publication_preserves_live_lease_and_rejects_oversized_payload",
            "profile_record_publication_corrupt_final_and_invalid_staging_remain_fail_closed",
            "profile_record_publication_create_only_rename_cannot_replace_authority",
        ):
            self.assertIn(regression, native_step)
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
                "ubuntu-22.04",
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

    # mini-agent-dw9ru: real Seatbelt tests for the general command sandbox and
    # JS spawn, keyed by their libtest path, with the source file defining them.
    SEATBELT_GENERAL_SANDBOX_TESTS = {
        "sandbox::sandbox_tests::macos_seatbelt_policy_enforces_real_backend": (
            "src/sandbox.rs"
        ),
        "tests::sandbox_platform_tests::macos_seatbelt_denies_symlinked_credential_reads": (
            "src/tests/sandbox_platform_tests.rs"
        ),
        "extras::js::host::tests::spawn_uses_real_macos_seatbelt_write_boundary": (
            "src/extras/js/host.rs"
        ),
    }

    def test_macos_gate_fails_when_general_seatbelt_tests_skip(self) -> None:
        body = job_body(self.workflow, "macos-worker-containment-gate")
        marker = "name: Require the real Seatbelt general command sandbox on this runner"
        self.assertIn(marker, body)
        step = body.split(marker, 1)[1].split("- name:", 1)[0]
        # Runs on every macOS matrix leg and can never be tolerated as a failure.
        self.assertNotIn("if:", step)
        self.assertNotIn("continue-on-error", step)
        self.assertRegex(step, r"MINI_AGENT_REQUIRE_REAL_SANDBOX: '1'")
        self.assertIn("--no-default-features --features js,sandbox", step)
        self.assertIn("--exact --nocapture", step)
        self.assertIn("grep -qi 'skipping'", step)
        self.assertIn(r"test result: ok\. 1 passed; 0 failed; 0 ignored;", step)
        self.assertIn('grep -Fxq "test $test_name ... ok" <<< "$output"', step)
        for test_name, source in self.SEATBELT_GENERAL_SANDBOX_TESTS.items():
            with self.subTest(test=test_name):
                self.assertEqual(1, step.count(f"'{test_name}'"))
                function = test_name.rsplit("::", 1)[1]
                text = (REPOSITORY_ROOT / source).read_text(encoding="utf-8")
                header = re.search(
                    rf"(?m)^(?P<indent> *)(?:async )?fn {re.escape(function)}\(", text
                )
                self.assertIsNotNone(header)
                # The early return must go through the env-var-aware helper,
                # not a bare eprintln that CI cannot turn into a failure.
                definition = text[header.end():].split(
                    f"\n{header.group('indent')}}}\n", 1
                )[0]
                self.assertIn("skip_unusable_real_sandbox_test(", definition)
                self.assertNotIn('eprintln!("skipping', definition)

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
        self.assertIn("public_config_status = 'not-probed-unattested'", standard_user_step)
        self.assertIn("installed_runtime_check = 'passed'", standard_user_step)
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

    def test_superseded_branch_and_pr_runs_are_cancelled_but_never_tag_runs(self) -> None:
        block = self.workflow.split("\nconcurrency:\n", 1)[1].split("\njobs:", 1)[0]
        group = re.search(r"(?m)^  group: (.+)$", block).group(1)
        cancel = re.search(r"(?m)^  cancel-in-progress: (.+)$", block).group(1)
        self.assertIn("github.ref", group)
        self.assertIn("inputs.scope", group)
        # Scheduled and dispatched runs must not share a group with push runs.
        self.assertIn("github.event_name == 'schedule'", group)
        self.assertIn("github.event_name == 'workflow_dispatch'", group)
        self.assertIn("github.event_name == 'pull_request'", cancel)
        self.assertIn("github.event_name == 'push' && !startsWith(github.ref, 'refs/tags/')", cancel)
        self.assertNotIn("schedule", cancel)
        self.assertNotIn("workflow_dispatch", cancel)

    def test_macos_runs_only_platform_feature_rows(self) -> None:
        header = job_body(self.workflow, "test").split("    steps:\n", 1)[0]
        self.assertIn("os: [ubuntu-latest, macos-latest]", header)
        exclude = header.split("        exclude:\n", 1)[1].split("    runs-on:", 1)[0]
        pairs = re.findall(
            r'- os: (\S+)\n\s+features: "([^"]*)"', exclude
        )
        self.assertTrue(pairs)
        # Linux keeps the full matrix: nothing may be excluded for it.
        self.assertEqual({os_name for os_name, _ in pairs}, {"macos-latest"})
        all_rows = set(self.matrix_features(job_body(self.workflow, "test")))
        excluded = {features for _, features in pairs}
        self.assertLess(excluded, all_rows)
        remaining = all_rows - excluded
        self.assertEqual(
            remaining,
            {
                "",
                "--no-default-features --features sandbox",
                "--no-default-features --features js",
                "--no-default-features --features js,sandbox",
                "--no-default-features --features hooks,advisor,lsp,multimodal,pdf",
                "--no-default-features --features hooks,js,subagents,skills,sandbox",
            },
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

        # The complete feature union (including `skills-embed`, which no matrix
        # row enables) is tested by the all-features job, so that job lints it
        # strictly too, before its tests run (mini-agent-6ap52).
        all_features = job_body(self.workflow, "all-features")
        self.assertIn("runs-on: ubuntu-latest", all_features)
        lint = "run: cargo clippy --locked --all-targets --all-features -- -D warnings\n"
        self.assertIn(lint, all_features)
        self.assertIn("cargo test --locked --all-features", all_features)
        self.assertLess(
            all_features.index(lint),
            all_features.index("cargo test --locked --all-features"),
        )
        lint_step = all_features.split(lint, 1)[0].rsplit("- name:", 1)[1]
        self.assertNotIn("continue-on-error", lint_step)
        self.assertNotIn("if:", lint_step)

    def test_every_job_declares_a_bounded_timeout(self) -> None:
        # Without a job-level timeout a hung job holds a runner for GitHub's
        # 360-minute default (mini-agent-6ap52).
        for job in workflow_jobs(self.workflow):
            with self.subTest(job=job):
                header = job_body(self.workflow, job).split("    steps:\n", 1)[0]
                timeouts = re.findall(r"(?m)^    timeout-minutes: (\d+)$", header)
                self.assertEqual(1, len(timeouts), f"{job} must declare one job-level timeout")
                minutes = int(timeouts[0])
                self.assertGreater(minutes, 0)
                self.assertLessEqual(minutes, 120)
        for job in (
            "windows-general-sandbox-policy",
            "windows-worker-containment-gate",
            "macos-worker-containment-gate",
        ):
            with self.subTest(gate=job):
                header = job_body(self.workflow, job).split("    steps:\n", 1)[0]
                self.assertIn("    timeout-minutes: 90\n", header)

    def test_schedule_runs_only_the_drift_detection_jobs(self) -> None:
        # The weekly run re-checks what can drift without a commit: advisories,
        # the hosted runner images and toolchain (mini-agent-6ap52).
        drift_jobs = {
            "changes",
            "dependency-policy",
            "harness-regression",
            "parallel-test-smoke",
            "linux-sandbox-policy",
            "macos-worker-containment-gate",
            "windows-general-sandbox-policy",
            "windows-worker-containment-gate",
            "phase6-cross-platform-gate",
        }
        header = self.workflow.split("jobs:\n", 1)[0]
        self.assertIn("  schedule:\n    - cron: '23 3 * * 1'\n", header)
        self.assertIn("drift detection", header)
        jobs = workflow_jobs(self.workflow)
        self.assertLessEqual(drift_jobs, set(jobs))
        for job in jobs:
            condition = job_body(self.workflow, job).splitlines()[0].strip()
            with self.subTest(job=job):
                if job in drift_jobs:
                    self.assertNotIn("'schedule'", condition)
                else:
                    self.assertTrue(condition.startswith("if: "), condition)
                    self.assertIn(
                        "github.event_name != 'schedule'",
                        top_level_split(condition[len("if: "):], "&&"),
                    )


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

    def test_linux_sandbox_policy_proves_workspace_git_metadata_is_read_only(
        self,
    ) -> None:
        linux = job_body(self.workflow, "linux-sandbox-policy")
        # The ignored real-bwrap probe runs a sandboxed `git config` and hook
        # write that must fail while `git add`/`git commit` still succeed.
        self.assertIn(
            "cargo test --locked \\\n"
            "            sandbox::sandbox_tests::bwrap_workspace_git_metadata_is_read_only \\\n"
            "            -- --exact --ignored --nocapture",
            linux,
        )
        self.assertLess(
            linux.index("bash scripts/install-ci-bubblewrap.sh"),
            linux.index("bwrap_workspace_git_metadata_is_read_only"),
        )

    def test_hosted_platform_prerequisites_preserve_real_security_gates(self) -> None:
        linux = job_body(self.workflow, "linux-sandbox-policy")
        self.assertIn("bash scripts/install-ci-bubblewrap.sh", linux)
        self.assertIn("kernel.apparmor_restrict_unprivileged_userns", linux)
        self.assertNotIn("sysctl -w", linux)
        self.assertIn("Record unchanged host namespace policy", linux)

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

    def test_standard_user_config_requires_only_static_unprobed_worker_fields(self) -> None:
        windows = job_body(self.workflow, "windows-worker-containment-gate")
        step = windows.split(
            "name: Validate the complete gate from a separate standard-user installation", 1
        )[1].split("name: Run Phase 6 adversarial suites", 1)[0]
        table = step.split("$requiredStatus = [ordered]@{", 1)[1].split("          }", 1)[0]
        patterns = dict(re.findall(r"(?m)^ +([a-z]+) = '([^']+)'$", table))
        self.assertEqual(set(patterns), {"compiled", "backend", "status", "assurance", "launch", "spawn"})
        static = (
            "  compiled true\n  backend windows-lpac\n  status not probed\n"
            "  assurance not attested\n  worker launch not started; containment not probed\n"
            "  parent-brokered spawn not advertised; containment not probed\n"
        )
        for key, pattern in patterns.items():
            self.assertRegex(static, pattern, key)
        for old, replacement, key in (
            ("status not probed", "status available", "status"),
            ("assurance not attested", "assurance enforced", "assurance"),
            ("not started; containment not probed", "enabled with validated containment", "launch"),
            ("not advertised; containment not probed", "disabled on Windows", "spawn"),
            ("compiled true", "compiled false", "compiled"),
            ("backend windows-lpac", "backend seatbelt", "backend"),
        ):
            with self.subTest(key=key):
                self.assertNotRegex(static.replace(old, replacement), patterns[key])

    def test_standard_user_explicit_runtime_check_precedes_all_native_proofs(self) -> None:
        windows = job_body(self.workflow, "windows-worker-containment-gate")
        step = windows.split(
            "name: Validate the complete gate from a separate standard-user installation", 1
        )[1].split("name: Run Phase 6 adversarial suites", 1)[0]

        def require_contract(candidate: str) -> None:
            self.assertIn("& $installedBinary --js-runtime-check *> $runtimeLog", candidate)
            self.assertRegex(candidate, r"(?s)& \$installedBinary --js-runtime-check.+?if \(\$LASTEXITCODE -ne 0\).*?exit 27")
            self.assertIn("$runtimeText -notmatch '(?m)^JS runtime check: PASS \\(2\\)\\r?$'", candidate)
            self.assertIn("failure = 'nonzero-exit'", candidate)
            self.assertIn("failure = 'missing-pass-witness'", candidate)
            self.assertIn("exit 28", candidate)
            self.assertIn("Remove-Item -LiteralPath $runtimeLog -Force", candidate)
            self.assertLess(candidate.index("& $installedBinary --js-runtime-check"), candidate.index("$referenceResource ="))
            self.assertLess(candidate.index("$referenceResource ="), candidate.index("$probeLog ="))
            self.assertIn("js_worker_resource_benchmark", candidate)
            self.assertIn("windows_js_worker_containment", candidate)
            self.assertIn("WINDOWS_CONTAINMENT_PASS backend=lpac job_close=pass nested_parent_job=pass protocol=pass", candidate)
            self.assertIn("exit 21", candidate)
            self.assertIn("exit 22", candidate)

        require_contract(step)
        for candidate in (
            step.replace("& $installedBinary --js-runtime-check *> $runtimeLog", "Write-Output 'JS runtime check: PASS (2)'", 1),
            step.replace("exit 27", "return", 1),
            step.replace("exit 28", "return", 1),
            step.replace("windows_js_worker_containment", "unrelated_test", 1),
            step.replace("js_worker_resource_benchmark", "unrelated_test"),
        ):
            with self.subTest(mutation=candidate[:60]), self.assertRaises(AssertionError):
                require_contract(candidate)

    def test_windows_hosted_administrator_status_is_fail_closed(self) -> None:
        body = job_body(self.workflow, "windows-worker-containment-gate")
        status_step = body.split(
            "name: Verify the hosted administrator path remains fail closed",
            1,
        )[1].split("name: Archive Windows containment evidence", 1)[0]
        self.assertIn("--print-config", status_step)
        for claim in (
            "windows-lpac",
            "not probed",
            "not attested",
            "not started; containment not probed",
            "not advertised; containment not probed",
        ):
            with self.subTest(claim=claim):
                self.assertIn(claim, status_step)
        self.assertIn("$evidence -notmatch", status_step)
        self.assertNotIn("continue-on-error", status_step)
        self.assertIn("--js-runtime-check", status_step)
        self.assertIn("if ($runtimeExit -eq 0)", status_step)
        self.assertIn("JavaScript worker containment is unavailable", status_step)
        self.assertIn("runtime self-check lacks the closed", status_step)
        self.assertIn("runtime self-check emitted contradictory pass evidence", status_step)
        self.assertIn("$global:LASTEXITCODE = 0", status_step)

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
    def test_pinned_native_archive_and_each_stage_fail_closed_without_policy_changes(self) -> None:
        installer = REPOSITORY_ROOT / "scripts/install-ci-bubblewrap.sh"
        text = installer.read_text()
        self.assertNotIn("sysctl -w", text)
        self.assertIn('"$pinned_native" -eq 1 && ! -r "$default_sources"', text)
        expected_hash = "cfeeb15fcc47d177d195f06fdf0847e93ee3aa6bf46f6ac0a141fa142759e2c3"
        phases = ["update", "install", "curl", "checksum", "tar", "meson", "ninja", "install_binary", "probe", "fd-probe"]
        for failure in ["none", "curl", "checksum", "meson", "install_binary", "probe", "fd-probe"]:
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                sources = root / "ubuntu sources.sources"
                sources.write_text("Types: deb\nURIs: https://archive.ubuntu.com/ubuntu\nSuites: jammy\nComponents: universe\n")
                log = root / "calls.jsonl"
                binary = root / "bin"
                binary.mkdir()
                builds = root / "builds"
                builds.mkdir()
                sudo = binary / "sudo"
                sudo.write_text('#!/bin/sh\nexec "$@"\n')
                sudo.chmod(0o755)
                stub_source = (
                    f"#!{sys.executable}\nimport json,sys\nfrom pathlib import Path\n"
                    "name=Path(sys.argv[0]).name; args=sys.argv[1:]; options={}\n"
                    "while args and args[0]=='-o' and name=='apt-get':\n"
                    " key,value=args[1].split('=',1);options[key]=value;args=args[2:]\n"
                    "phase={'sha256sum':'checksum','install':'install_binary'}.get(name,name)\n"
                    "if name=='apt-get': phase=args[0]\n"
                    "if name=='bwrap': phase='probe' if args==['--version'] else 'fd-probe'\n"
                    "stdin=sys.stdin.read() if name=='sha256sum' else ''\n"
                    f"with open({str(log)!r},'a') as out:out.write(json.dumps([phase,args,options,stdin])+'\\n')\n"
                    "if name=='apt-get':\n"
                    f" assert options=={{'Dir::Etc::sourcelist':{str(sources)!r},'Dir::Etc::sourceparts':'-'}}\n"
                    f"if phase=={failure!r}:sys.exit(19)\n"
                    "if name=='bwrap':print('bubblewrap 0.11.0')\n"
                )
                for name in ["apt-get", "curl", "sha256sum", "tar", "meson", "ninja", "install", "bwrap"]:
                    stub = binary / name
                    stub.write_text(stub_source)
                    stub.chmod(0o755)
                result = subprocess.run(["bash", str(installer), "--pinned-native", str(sources)],
                    env={**os.environ, "PATH": str(binary) + os.pathsep + os.environ.get("PATH", ""), "TMPDIR": str(builds)},
                    capture_output=True, text=True, timeout=5)
                calls = [json.loads(line) for line in log.read_text().splitlines()]
                wanted = phases if failure == "none" else phases[:phases.index(failure)+1]
                self.assertEqual([call[0] for call in calls], wanted, result.stderr)
                self.assertEqual(result.returncode, 0 if failure == "none" else 19, result.stderr)
                self.assertEqual(list(builds.iterdir()), [])
                by_phase = {call[0]: call for call in calls}
                if "curl" in by_phase:
                    args = by_phase["curl"][1]
                    self.assertIn("https://github.com/containers/bubblewrap/archive/refs/tags/v0.11.0.tar.gz", args)
                    self.assertIn("--disable", args)
                    self.assertIn("--max-time", args)
                if "checksum" in by_phase:
                    self.assertTrue(by_phase["checksum"][3].startswith(expected_hash + "  "))
                    self.assertEqual(by_phase["checksum"][1], ["--check", "--status"])
                if "install_binary" in by_phase:
                    self.assertEqual(by_phase["install_binary"][1][-1], "/usr/bin/bwrap")
                    self.assertEqual(by_phase["install_binary"][1][:7], ["-o", "root", "-g", "root", "-m", "0755", "--"])
                if "fd-probe" in by_phase:
                    self.assertEqual(by_phase["fd-probe"][1], ["--bind-fd", "0", "/", "--ro-bind-fd", "0", "/", "--version"])

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



class WindowsBoundedProjectTrustCiTests(unittest.TestCase):
    """The changed Windows readers need native execution, not compile-only coverage."""

    @staticmethod
    def validate(workflow: str) -> str:
        job = job_body(workflow, "platform-paths")
        name = "Test native Windows bounded project trust readers"
        if job.count("- name: " + name) != 1:
            raise AssertionError("one owning native bounded trust step is required")
        step = job.split("- name: " + name, 1)[1].split("- name:", 1)[0]
        required = [
            "if: matrix.os == 'windows-latest' && matrix.feature.name == 'default'",
            "shell: pwsh",
            "$filter = 'config::load::bounded_project_trust_tests::'",
            "cargo test --locked --bin mini-agent $filter -- --list 2>&1",
            "$actual.Count -ne 2",
            "Compare-Object -ReferenceObject $expected -DifferenceObject $actual",
            "'^config::load::bounded_project_trust_tests::.*: test$'",
            "cargo test --locked --bin mini-agent $filter -- --nocapture --test-threads=1",
        ]
        for fragment in required:
            if fragment not in step:
                raise AssertionError("missing native bounded trust guard: " + fragment)
        expected = {
            "bounded_config_and_private_state_reject_size_utf8_links_and_keep_mode_repair",
            "bounded_trust_preserves_exact_schema_hash_and_sensitive_application",
        }
        actual = re.findall(r"'config::load::bounded_project_trust_tests::(\w+): test'", step)
        if len(actual) != 2 or set(actual) != expected:
            raise AssertionError("exact two changed Windows reader controls are required")
        if step.count("if ($LASTEXITCODE -ne 0)") != 2 or step.count("throw ") != 3:
            raise AssertionError("discovery, exact set and native proof must fail closed")
        if "--ignored" in step or "continue-on-error" in step or "cargo build" in step:
            raise AssertionError("native trust proof cannot be replaced or waived")
        return step

    def test_existing_windows_default_job_executes_exact_changed_readers(self) -> None:
        self.validate(WORKFLOW.read_text())

    def test_missing_or_weakened_reader_discovery_is_rejected(self) -> None:
        original = WORKFLOW.read_text()
        for before, after in [
            ("Test native Windows bounded project trust readers", "Removed bounded trust proof"),
            ("$actual.Count -ne 2", "$actual.Count -lt 0"),
            ("Compare-Object -ReferenceObject $expected -DifferenceObject $actual", "$null"),
            ("--bin mini-agent $filter -- --list", "--bin mini-agent $filter"),
            ("bounded_trust_preserves_exact_schema_hash_and_sensitive_application: test", "other_reader: test"),
        ]:
            with self.subTest(before=before), self.assertRaises(AssertionError):
                self.validate(original.replace(before, after))

    def test_native_execution_scope_and_failure_checks_cannot_be_widened(self) -> None:
        original = WORKFLOW.read_text()
        name = "Test native Windows bounded project trust readers"
        prefix, suffix = original.split("- name: " + name, 1)
        step, rest = suffix.split("- name:", 1)
        for before, after in [
            ("if: matrix.os == 'windows-latest' && matrix.feature.name == 'default'", "if: matrix.os == 'windows-latest'"),
            ("--bin mini-agent $filter -- --nocapture --test-threads=1", "--bin mini-agent -- --nocapture --test-threads=1"),
            ("if ($LASTEXITCODE -ne 0)", "if ($LASTEXITCODE -lt 0)"),
            ("--nocapture --test-threads=1", "--ignored --nocapture --test-threads=1"),
            ("throw 'Native Windows bounded project trust proof failed'", "Write-Host 'Native Windows bounded project trust proof failed'"),
        ]:
            with self.subTest(before=before), self.assertRaises(AssertionError):
                self.validate(prefix + "- name: " + name + step.replace(before, after) + "- name:" + rest)


class WindowsSessionStorageDispatchTests(unittest.TestCase):
    """Manual diagnostics execute the owning tests without claiming full CI."""

    def test_manual_scope_reuses_one_job_and_skips_unrelated_steps(self) -> None:
        workflow = WORKFLOW.read_text()
        body = job_body(workflow, "windows-default-compile")
        condition = body.splitlines()[0]
        self.assertIn("inputs.scope == 'windows-session-storage'", condition)
        for name in (
            "Compile the Windows default feature suite",
            "Test native ledger lock and persistence contracts",
            "Test native one MiB App event loops",
            "Test native Windows GH argv and actual foreground authority",
        ):
            step = body.split("- name: " + name, 1)[1].split("- name:", 1)[0]
            self.assertIn("if: github.event_name != 'workflow_dispatch' || inputs.scope != 'windows-session-storage'", step)
        self.assertIn("inputs.scope == 'full'", job_body(workflow, "ci-success").splitlines()[0])
        self.assertIn("group: ci-${{ github.ref }}-${{ inputs.scope || 'full' }}", workflow)

    def test_native_proof_requires_exact_discovery_execution_and_success(self) -> None:
        workflow = WORKFLOW.read_text()
        body = job_body(workflow, "windows-default-compile")
        step = body.split("- name: Test exact Windows session private storage", 1)[1]
        self.assertIn("if: github.event_name == 'workflow_dispatch' && inputs.scope == 'windows-session-storage'", step)
        self.assertIn("session_storage_permissions_windows_dacls_exclude_broad_principals'", step)
        self.assertIn("session_storage_permissions_windows_reject_reparse_paths_without_mutation'", step)
        self.assertIn("$actual.Count -ne 2 -or $difference.Count -ne 0", step)
        self.assertIn("$nativeExit = $LASTEXITCODE", step)
        self.assertIn("if ($nativeExit -ne 0 -or -not ($native -match '^test result: ok\\. 2 passed; 0 failed; 0 ignored;'))", step)
        self.assertEqual(2, step.count("cargo test --locked --no-default-features --features mcp --bin mini-agent -- @tests"))
        self.assertNotIn("--ignored", step)
        self.assertNotIn("continue-on-error", step)
        source = (REPOSITORY_ROOT / "src/tests/session_storage_tests.rs").read_text()
        for name in re.findall(r"'tests::session_storage_tests::([^']+)'", step):
            self.assertIn("fn " + name + "()", source)


class WindowsMcpPrivateTemporaryCiTests(unittest.TestCase):
    """The new retained-handle owner initialization requires a native Windows proof."""

    @staticmethod
    def validate(workflow: str) -> str:
        job = job_body(workflow, "platform-paths")
        name = "Test native Windows MCP private temporary ownership"
        if job.count("- name: " + name) != 1:
            raise AssertionError("one owning native MCP private temporary step is required")
        step = job.split("- name: " + name, 1)[1].split("- name:", 1)[0]
        required = [
            "if: matrix.os == 'windows-latest' && matrix.feature.name == 'default'",
            "shell: pwsh",
            "$test = 'fs::private::windows::mcp_native_windows_new_temp_is_user_owned_private_and_nonempty_or_linked_files_are_unchanged'",
            "$discovered = @(cargo test --locked --bin mini-agent $test -- --exact --list 2>&1)",
            '$_ -ceq "${test}: test"',
            "$actual.Count -ne 1",
            "$native = @(cargo test --locked --bin mini-agent $test -- --exact --nocapture --test-threads=1 2>&1)",
            "$nativeExit = $LASTEXITCODE",
            "$native | Write-Output",
            "if ($LASTEXITCODE -ne 0) { throw 'Windows MCP private temporary discovery failed' }",
            "if ($nativeExit -ne 0) { throw 'Native Windows MCP private temporary proof failed' }",
            "if (-not ($native -match '^test result: ok\\. 1 passed; 0 failed; 0 ignored;'))",
            "throw 'Native Windows MCP private temporary proof did not execute exactly one passing test'",
        ]
        for fragment in required:
            if fragment not in step:
                raise AssertionError("missing native MCP ownership guard: " + fragment)
        if step.count("cargo test ") != 2 or step.count("throw ") != 4:
            raise AssertionError("exact discovery and real execution must both fail closed")
        if any(fragment in step for fragment in ("--ignored", "--no-run", "continue-on-error", "cargo build")):
            raise AssertionError("native ownership execution cannot be replaced or waived")
        return step

    def test_existing_windows_default_job_executes_exact_new_private_owner_control(self) -> None:
        self.validate(WORKFLOW.read_text())

    def test_missing_or_weakened_native_owner_step_is_rejected(self) -> None:
        original = WORKFLOW.read_text()
        name = "Test native Windows MCP private temporary ownership"
        prefix, suffix = original.split("- name: " + name, 1)
        step, rest = suffix.split("- name:", 1)
        for before, after in [
            ("if: matrix.os == 'windows-latest' && matrix.feature.name == 'default'", "if: matrix.os == 'windows-latest'"),
            ("$actual.Count -ne 1", "$actual.Count -lt 0"),
            ('$_ -ceq "${test}: test"', '$_ -match "test"'),
            ("--exact --list", "--list"),
            ("--exact --nocapture", "--nocapture"),
            ("$nativeExit = $LASTEXITCODE", "$nativeExit = 0"),
            ("if ($nativeExit -ne 0)", "if ($nativeExit -lt 0)"),
            ("if ($LASTEXITCODE -ne 0)", "if ($LASTEXITCODE -lt 0)"),
            ("1 passed; 0 failed; 0 ignored;", "0 passed; 0 failed; 1 ignored;"),
            ("--nocapture --test-threads=1", "--ignored --nocapture --test-threads=1"),
            ("throw 'Native Windows MCP private temporary proof failed'", "Write-Host 'Native Windows MCP private temporary proof failed'"),
        ]:
            with self.subTest(before=before), self.assertRaises(AssertionError):
                self.validate(prefix + "- name: " + name + step.replace(before, after) + "- name:" + rest)
        with self.assertRaises(AssertionError):
            self.validate(original.replace(name, "Removed native ownership proof"))


class WindowsMcpOperatorLeaseExperimentCiTests(unittest.TestCase):
    """Experiments can reject a candidate; they never establish production mutation."""

    @staticmethod
    def validate(workflow: str) -> str:
        job = job_body(workflow, "platform-paths")
        name = "Falsify native Windows MCP operator config lease assumptions"
        if job.count("- name: " + name) != 1:
            raise AssertionError("one bounded native lease experiment step required")
        step = job.split("- name: " + name, 1)[1].split("- name:", 1)[0]
        for fragment in [
            "if: matrix.os == 'windows-latest' && matrix.feature.name == 'default'", "shell: pwsh",
            "$filter = 'fs::private::windows::operator_config_lease_probe::'",
            "cargo test --locked --bin mini-agent $filter -- --list 2>&1",
            "$actual.Count -ne 5", "Compare-Object -ReferenceObject $expected -DifferenceObject $actual",
            "'^fs::private::windows::operator_config_lease_probe::.*: test$'",
            "cargo test --locked --bin mini-agent $filter -- --nocapture --test-threads=1 2>&1",
            "if ($LASTEXITCODE -ne 0)", "$nativeExit = $LASTEXITCODE", "if ($nativeExit -ne 0)",
            r"'^test result: ok\. 5 passed; 0 failed; 0 ignored;'",
        ]:
            if fragment not in step:
                raise AssertionError("missing exact experiment guard: " + fragment)
        expected = {
            "exact_relative_lease_surviving_mapping_and_data_conflicts",
            "namespace_alias_and_private_metadata_are_observed_before_any_candidate_write",
            "reader_lease_precedes_marker_and_late_marker_refuses_admission",
            "retained_retirement_preserves_full_surviving_readonly_views_and_refuses_conflicts",
            "read_delete_share_zero_distinguishes_data_exclusion_from_mutable_metadata",
        }
        names = re.findall(r"'fs::private::windows::operator_config_lease_probe::(\w+): test'", step)
        if len(names) != 5 or set(names) != expected:
            raise AssertionError("exact five experiments required")
        if step.count("cargo test ") != 2 or step.count("throw ") != 4:
            raise AssertionError("discovery/result failures must stay closed")
        if any(value in step for value in ["--ignored", "--no-run", "continue-on-error", "cargo build"]):
            raise AssertionError("native experiments cannot be replaced or waived")
        return step

    def test_existing_native_job_runs_exact_lease_experiments(self) -> None:
        self.validate(WORKFLOW.read_text())

    def test_missing_or_weakened_experiment_guards_fail_closed(self) -> None:
        original = WORKFLOW.read_text()
        name = "Falsify native Windows MCP operator config lease assumptions"
        prefix, suffix = original.split("- name: " + name, 1)
        step, rest = suffix.split("- name:", 1)
        for before, after in [
            ("$actual.Count -ne 5", "$actual.Count -lt 0"),
            ("Compare-Object -ReferenceObject $expected -DifferenceObject $actual", "$null"),
            ("--bin mini-agent $filter -- --list", "--bin mini-agent $filter -- --no-run"),
            ("reader_lease_precedes_marker_and_late_marker_refuses_admission: test", "other: test"),
            ("retained_retirement_preserves_full_surviving_readonly_views_and_refuses_conflicts: test", "other_retirement: test"),
            ("read_delete_share_zero_distinguishes_data_exclusion_from_mutable_metadata: test", "other_rights: test"),
            ("if: matrix.os == 'windows-latest' && matrix.feature.name == 'default'", "if: matrix.os == 'windows-latest'"),
            ("--bin mini-agent $filter -- --nocapture", "--bin mini-agent -- --nocapture"),
            ("$nativeExit = $LASTEXITCODE", "$nativeExit = 0"),
            ("if ($nativeExit -ne 0)", "if ($nativeExit -lt 0)"),
            ("5 passed; 0 failed; 0 ignored;", "0 passed; 0 failed; 5 ignored;"),
        ]:
            with self.subTest(before=before), self.assertRaises(AssertionError):
                self.validate(prefix + "- name: " + name + step.replace(before, after) + "- name:" + rest)
        with self.assertRaises(AssertionError):
            self.validate(original.replace(name, "Removed native experiment"))


class MacosExactOutputGuardTests(unittest.TestCase):
    def run_guard(
        self, case: str,
        step_name: str = "Require the real Seatbelt general command sandbox on this runner",
    ) -> subprocess.CompletedProcess[str]:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        body = job_body(workflow, "macos-worker-containment-gate")
        name = "name: " + step_name
        step = body.split(name, 1)[1].split("- name:", 1)[0]
        script = textwrap.dedent(step.split("run: |\n", 1)[1])
        # Exercise the actual workflow shell. Padding follows the early success
        # match, so an early-exiting consumer exposes a producer SIGPIPE.
        cargo = r"""
cargo() {
  status=0
  result='ok. 1 passed; 0 failed; 0 ignored; 0 measured; 2719 filtered out; finished in 0.15s'
  name="${test_name:-extras::js::tests::worker_containment::macos_js_worker_containment}"
  outcome=ok
  case "$GUARD_CASE" in
    missing) result='ok. 0 passed; 0 failed; 0 ignored'; outcome=missing ;;
    wrong_name) name=unrelated::test ;;
    wrong_count) result='ok. 2 passed; 0 failed; 0 ignored' ;;
    ignored) result='ok. 0 passed; 0 failed; 1 ignored'; outcome=ignored ;;
    failed) result='FAILED. 0 passed; 1 failed; 0 ignored'; outcome=FAILED; status=101 ;;
    skipped) printf '%s\n' 'skipping: controlled unavailable Seatbelt' ;;
  esac
  printf 'test %s ... %s\n' "$name" "$outcome"
  printf 'test result: %s\n' "$result"
  cat "$GUARD_PADDING"
  return "$status"
}
"""
        with tempfile.TemporaryDirectory(prefix="mini-agent-macos-output-guard-") as root:
            padding = Path(root) / "padding.txt"
            padding.write_text(("nonmatching output after the result\n" * 8192), encoding="utf-8")
            environment = dict(
                os.environ, GUARD_CASE=case, GUARD_PADDING=str(padding),
                GITHUB_ENV=str(Path(root) / "github-env"),
            )
            return subprocess.run(
                ["bash", "-e", "-o", "pipefail", "-c", cargo + script],
                env=environment,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                text=True,
                timeout=10,
                check=False,
            )

    def test_large_passing_output_does_not_fail_under_pipefail(self) -> None:
        for step in [
            "Require the real Seatbelt general command sandbox on this runner",
            "Confirm the worker profile denies named FIFOs on this runner",
            "Confirm the Seatbelt facts behind the one-time worker image",
        ]:
            with self.subTest(step=step):
                result = self.run_guard("passed", step)
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_missing_wrong_failed_ignored_and_skipped_results_are_rejected(self) -> None:
        for case in ["missing", "wrong_name", "wrong_count", "ignored", "failed", "skipped"]:
            with self.subTest(case=case):
                result = self.run_guard(case)
                self.assertNotEqual(result.returncode, 0)


class WindowsGeneralPreflightDiagnosticsCiTests(unittest.TestCase):
    @staticmethod
    def validate(workflow: str, source: str) -> None:
        job = job_body(workflow, "windows-general-sandbox-policy")
        native = job.split("name: Test native general-sandbox policy and recovery", 1)[1].split("- name:", 1)[0]
        for item in [
            "preflight_diagnostics_bound_closed_timing_and_late_exit_categories",
            "preflight_diagnostics_cached_result_never_reprobes_or_emits_private_error_data",
            "timed_out_general_preflight_reaps_tree_and_removes_recovery_state",
            ".Count -ne 1", "$nativeExit = $LASTEXITCODE", "if ($nativeExit -ne 0)",
            r"'^test result: ok\. 40 passed; 0 failed; 0 ignored;'",
        ]:
            if item not in native:
                raise AssertionError("missing native diagnostic execution guard: " + item)
        observer = job.split("name: Test the exact installed-preflight observer controls", 1)[1].split("- name:", 1)[0]
        for item in [
            "sandbox::windows::preflight_witness::tests::preflight_helper_observation_decodes_only_closed_stages_and_exact_header",
            "sandbox::windows::preflight_witness::tests::ordinary_launch_request_omits_observer_and_diagnostics_are_value_free",
            "--exact --list", "--exact --nocapture --test-threads=1", ".Count -ne 1",
            "$observerExit = $LASTEXITCODE", "if ($observerExit -ne 0)",
            r"'^test result: ok\. 2 passed; 0 failed; 0 ignored;'",
        ]:
            if item not in observer:
                raise AssertionError("missing exact observer execution guard: " + item)
        probe = job.split("name: Attest write, authority, desktop, and parent-owned Job boundaries", 1)[1]
        for item in [
            "foreach ($attempt in 1..3)", "--mini-agent-windows-sandbox-runtime-check 2>&1",
            "$probeExit = $LASTEXITCODE", "if ($probeExit -ne 0)",
            "$diagnostics.Count -lt 1 -or $diagnostics.Count -gt 34",
            "[int]$match.Groups[3].Value -gt 60000", "[int]$match.Groups[4].Value -gt 60000",
            "if (-not $match.Success", " stage=finished category=available ", " stage=cached category=available ",
            "WINDOWS_GENERAL_SANDBOX_PASS appcontainer=regular", "breakaway=denied", "network=denied",
            "WINDOWS_GENERAL_PREFLIGHT_HELPER_DIAGNOSTIC ", "$helper.Count -ne 1",
            "phase=cleanup elapsed_ms=([0-9]{1,5}) phase_entered_ms=([0-9]{1,8}) phase_clock=attachment phase_time_capped=false$",
            "[int]$helperMatch.Groups[1].Value -gt 60000", "[int]$helperMatch.Groups[2].Value -gt 60000",
        ]:
            if item not in probe:
                raise AssertionError("missing installed diagnostic guard: " + item)
        direct = source.split("pub(crate) fn verify_direct_command(", 1)[1].split("\nfn build_helper(", 1)[0]
        normalized = re.sub(r"\s+", "", direct)
        for item in [
            "run_bounded_preflight_helper(&mutcommand,run_deadline,cleanup_deadline,None)",
            "run_deadline=started+Duration::from_secs(3)",
            "cleanup_deadline=run_deadline+Duration::from_secs(3)",
            ".stdout(Stdio::null())", ".stderr(Stdio::null())",
        ]:
            if item not in normalized:
                raise AssertionError("ordinary direct-command diagnostic scope changed: " + item)
        if any(item in direct for item in ["PreflightDiagnostics::new", "record_preflight", "println!", "write_all"]):
            raise AssertionError("ordinary direct command must not collect or emit self-check diagnostics")
        for item in [
            "const GENERAL_PREFLIGHT_RUN_TIMEOUT: Duration = Duration::from_secs(5);",
            "const GENERAL_PREFLIGHT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);",
            "general_sandbox_preflight_observed(None)",
        ]:
            if item not in source:
                raise AssertionError("production deadline/silent scope changed: " + item)

    def test_native_and_installed_closed_diagnostics_are_required(self) -> None:
        self.validate(WORKFLOW.read_text(), (REPOSITORY_ROOT / "src/sandbox/windows.rs").read_text())

    def test_native_guard_waivers_and_unbounded_installed_records_are_rejected(self) -> None:
        original = WORKFLOW.read_text()
        source = (REPOSITORY_ROOT / "src/sandbox/windows.rs").read_text()
        for before, after in [
            ("preflight_diagnostics_bound_closed_timing_and_late_exit_categories", "missing_bound_test"),
            ("preflight_diagnostics_cached_result_never_reprobes_or_emits_private_error_data", "missing_cache_test"),
            ("40 passed; 0 failed; 0 ignored;", "0 passed; 0 failed; 40 ignored;"),
            ("$observerExit = $LASTEXITCODE", "$observerExit = 0"),
            ("2 passed; 0 failed; 0 ignored;", "0 passed; 0 failed; 2 ignored;"),
            ("--exact --nocapture --test-threads=1", "--ignored --nocapture --test-threads=1"),
            ("[int]$helperMatch.Groups[2].Value -gt 60000", "[int]$helperMatch.Groups[2].Value -lt 0"),
            ("$probeExit = $LASTEXITCODE", "$probeExit = 0"),
            ("$diagnostics.Count -gt 34", "$diagnostics.Count -lt 0"),
            ("[int]$match.Groups[3].Value -gt 60000", "[int]$match.Groups[3].Value -lt 0"),
            (" stage=cached category=available ", " ignored-cache-observation "),
        ]:
            with self.subTest(before=before), self.assertRaises(AssertionError):
                self.validate(original.replace(before, after), source)

    def test_direct_verification_remains_silent_with_unchanged_deadlines(self) -> None:
        workflow = WORKFLOW.read_text()
        original = (REPOSITORY_ROOT / "src/sandbox/windows.rs").read_text()
        prefix, tail = original.split("pub(crate) fn verify_direct_command(", 1)
        body, rest = tail.split("\nfn build_helper(", 1)
        for before, after in [
            ("cleanup_deadline, None", "cleanup_deadline, diagnostics"),
            ("Duration::from_secs(3)", "Duration::from_secs(30)"),
            (".stderr(Stdio::null())", ".stderr(Stdio::piped())"),
        ]:
            with self.subTest(before=before), self.assertRaises(AssertionError):
                self.validate(workflow, prefix + "pub(crate) fn verify_direct_command(" + body.replace(before, after) + "\nfn build_helper(" + rest)


if __name__ == "__main__":
    unittest.main()
