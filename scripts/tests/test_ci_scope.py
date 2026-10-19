#!/usr/bin/env python3
"""Tests for CI test-impact scope classification and ci-success acceptance (mini-agent-m7wn5)."""

from __future__ import annotations

import re
import subprocess
import unittest
from pathlib import Path

from scripts import ci_scope, ci_success

REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = (REPOSITORY_ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
BASE, HEAD = "1" * 40, "2" * 40


class ClassifyTests(unittest.TestCase):
    def scope(self, *paths: str, sources: dict[str, str] | None = None) -> ci_scope.Scope:
        return ci_scope.classify(paths, lambda path: (sources or {}).get(path))

    def test_docs(self) -> None:
        self.assertEqual("docs", self.scope("README.md", "docs/plans/idea.md").scope)
        self.assertEqual("docs", self.scope().scope)

    def test_consumed_docs_are_code(self) -> None:
        for path in ("docs/specs/x.md", "docs/agent/CONFIG.md", "docs/acp-registry.json", "LICENSE"):
            self.assertEqual("code", self.scope(path).scope, path)

    def test_test_only_paths(self) -> None:
        for path in (
            "src/tests/harness_eval_tests.rs",
            "src/ui/feed_tests.rs",
            "src/extras/js/tests/worker.rs",
            "tests/cli.rs",
            "tests/fixtures/a.json",
        ):
            self.assertEqual(ci_scope.Scope("tests", False, False), self.scope(path), path)

    def test_docs_do_not_turn_a_test_diff_into_code(self) -> None:
        self.assertEqual("tests", self.scope("src/tests/a.rs", "docs/plans/idea.md").scope)

    def test_production_file_with_inline_tests_is_code(self) -> None:
        self.assertEqual("code", self.scope("src/permission.rs").scope)
        self.assertEqual("code", self.scope("src/extras/js/tests.rs").scope)

    def test_mixed_diff_is_code(self) -> None:
        self.assertEqual("code", self.scope("src/tests/a.rs", "src/main.rs").scope)
        self.assertEqual("code", self.scope("src/tests/a.rs", "Cargo.lock").scope)

    def test_platform_specific_test_paths_keep_their_platform(self) -> None:
        cases = {
            "src/sandbox/windows/tests/appcontainer_tests.rs": (False, True),
            "src/tests/lpac_tests.rs": (False, True),
            "src/tests/seatbelt_tests.rs": (True, False),
            "src/tests/macos_gym_tests.rs": (True, False),
            "src/tests/bwrap_tests.rs": (False, False),
            "src/tests/linux_tests.rs": (False, False),
        }
        for path, (macos, windows) in cases.items():
            with self.subTest(path=path):
                self.assertEqual(ci_scope.Scope("tests", macos, windows), self.scope(path))

    def test_whole_file_cfg_gate_keeps_its_platform(self) -> None:
        gated = {
            "src/tests/a_tests.rs": '#![cfg(target_os = "windows")]\nfn x() {}',
            "src/tests/b_tests.rs": '#![cfg(target_os = "macos")]\n',
            "src/tests/c_tests.rs": "#![cfg(unix)]\n",
            "src/tests/d_tests.rs": '#![cfg(target_os = "linux")]\n',
        }
        self.assertEqual((False, True), self.scope("src/tests/a_tests.rs", sources=gated)[1:])
        self.assertEqual((True, False), self.scope("src/tests/b_tests.rs", sources=gated)[1:])
        self.assertEqual((True, False), self.scope("src/tests/c_tests.rs", sources=gated)[1:])
        self.assertEqual((False, False), self.scope("src/tests/d_tests.rs", sources=gated)[1:])

    def test_outputs(self) -> None:
        tests = ci_scope.Scope("tests", False, False)
        self.assertIn("full=false\nmacos=false\nwindows=false\n", ci_scope.render(tests))
        self.assertIn('test_os=["ubuntu-latest"]', ci_scope.render(tests))
        mac = ci_scope.Scope("tests", True, False)
        self.assertIn('test_os=["ubuntu-latest","macos-latest"]', ci_scope.render(mac))
        self.assertIn("macos=true\nwindows=false", ci_scope.render(mac))
        out = ci_scope.render(ci_scope.FULL)
        self.assertIn("scope=code\nfull=true\nmacos=true\nwindows=true\n", out)


class ResolveTests(unittest.TestCase):
    def resolve(self, paths, **env):
        def run(cmd, **kwargs):
            if isinstance(paths, Exception):
                raise paths
            return subprocess.CompletedProcess(cmd, 0, stdout="".join(f"{p}\n" for p in paths))

        base = {"EVENT": "pull_request", "GITHUB_REF": "refs/pull/1/merge", "BASE_SHA": BASE, "HEAD_SHA": HEAD}
        base.update(env)
        return ci_scope.resolve(base, run=run)

    def test_test_only_pull_request_and_push(self) -> None:
        self.assertEqual("tests", self.resolve(["src/tests/a_tests.rs"]).scope)
        self.assertEqual("tests", self.resolve(["tests/a.rs"], EVENT="push", GITHUB_REF="refs/heads/main").scope)

    def test_tags_schedule_and_dispatch_never_narrow(self) -> None:
        self.assertEqual("code", self.resolve(["tests/a.rs"], EVENT="push", GITHUB_REF="refs/tags/v1").scope)
        for event in ("schedule", "workflow_dispatch", ""):
            self.assertEqual("code", self.resolve(["tests/a.rs"], EVENT=event).scope, event)

    def test_every_doubt_fails_open(self) -> None:
        for env in ({"BASE_SHA": ""}, {"BASE_SHA": "0" * 40}, {"HEAD_SHA": ""}):
            self.assertEqual(ci_scope.FULL, self.resolve(["tests/a.rs"], **env), env)
        self.assertEqual(ci_scope.FULL, self.resolve(subprocess.CalledProcessError(1, "git")))
        self.assertEqual(ci_scope.FULL, self.resolve(OSError("no git")))

    def test_docs_verdict_after_a_code_verdict_fails_open(self) -> None:
        self.assertEqual(ci_scope.FULL, self.resolve(["README.md"]))


class DocsFilterEquivalenceTests(unittest.TestCase):
    def test_python_docs_rule_matches_the_shell_filter_regexes(self) -> None:
        for pattern in (ci_scope.DOCS_ONLY.pattern, ci_scope.CONSUMED_DOCS.pattern):
            self.assertIn(pattern.replace("\\", "\\"), WORKFLOW)


class WorkflowWiringTests(unittest.TestCase):
    def job(self, name: str) -> str:
        match = re.search(rf"(?ms)^  {re.escape(name)}:\n(?P<body>.*?)(?=^  [a-zA-Z0-9_-]+:\n|\Z)", WORKFLOW)
        self.assertIsNotNone(match, name)
        return match.group("body")

    def test_changes_job_fails_open_by_default(self) -> None:
        header = self.job("changes").split("    steps:\n", 1)[0]
        for default in (
            "scope: ${{ steps.scope.outputs.scope || 'code' }}",
            "full: ${{ steps.scope.outputs.full || 'true' }}",
            "macos: ${{ steps.scope.outputs.macos || 'true' }}",
            "windows: ${{ steps.scope.outputs.windows || 'true' }}",
        ):
            self.assertIn(default, header)
        self.assertIn('|| \'["ubuntu-latest","macos-latest"]\'', header)

    def test_skippable_jobs_are_gated_by_the_matching_scope_output(self) -> None:
        for job, platform in ci_success.TESTS_SCOPE_SKIPPABLE.items():
            condition = self.job(job).splitlines()[0]
            key = platform or "full"
            with self.subTest(job=job):
                self.assertIn(f"needs.changes.outputs.{key} == 'true'", condition)

    def test_jobs_that_run_for_test_only_diffs_are_not_scope_gated(self) -> None:
        for job in ("fmt", "clippy", "test", "all-features", "parallel-test-smoke", "linux-sandbox-policy", "ci-success"):
            condition = self.job(job).splitlines()[0]
            with self.subTest(job=job):
                for key in ("full", "macos", "windows"):
                    self.assertNotIn(f"outputs.{key}", condition)

    def test_macos_rows_follow_the_scope(self) -> None:
        for job in ("test", "parallel-test-smoke"):
            self.assertIn("os: ${{ fromJSON(needs.changes.outputs.test_os) }}", self.job(job))


class CiSuccessScopeTests(unittest.TestCase):
    SKIPPABLE = tuple(ci_success.TESTS_SCOPE_SKIPPABLE)

    def needs(self, scope="tests", macos="false", windows="false", **results):
        jobs = {
            "changes": {"result": "success", "outputs": {"code": "true", "scope": scope, "macos": macos, "windows": windows}},
            "fmt": {"result": "success", "outputs": {}},
            "test": {"result": "success", "outputs": {}},
        }
        for job in self.SKIPPABLE:
            jobs[job] = {"result": "skipped", "outputs": {}}
        for job, result in results.items():
            jobs[job.replace("_", "-")] = {"result": result, "outputs": {}}
        return jobs

    def test_tests_scope_accepts_skips_on_push_and_pull_request(self) -> None:
        for event, ref in (("push", "refs/heads/main"), ("pull_request", "refs/pull/1/merge")):
            with self.subTest(event=event):
                self.assertEqual([], ci_success.evaluate(self.needs(), event, ref))

    def test_tags_schedule_and_dispatch_require_every_job(self) -> None:
        for event, ref in (("push", "refs/tags/v1"), ("schedule", "refs/heads/main"), ("workflow_dispatch", "refs/heads/main")):
            with self.subTest(event=event):
                errors = ci_success.evaluate(self.needs(), event, ref)
                self.assertEqual(len(self.SKIPPABLE), len(errors), errors)

    def test_code_scope_still_rejects_skips(self) -> None:
        self.assertTrue(ci_success.evaluate(self.needs(scope="code"), "push", "refs/heads/main"))

    def test_platform_flag_requires_that_platform(self) -> None:
        windows = ci_success.evaluate(self.needs(windows="true"), "push", "refs/heads/main")
        self.assertEqual(
            {"windows-default-compile", "windows-hooks-compile", "windows-worker-launcher-unit"},
            {e.split(" ")[0] for e in windows},
        )
        macos = ci_success.evaluate(self.needs(macos="true"), "pull_request", "refs/pull/1/merge")
        self.assertEqual(["macos-gym-ownership finished with 'skipped'"], macos)
        ran = self.needs(windows="true", macos="true")
        for job in ("windows-default-compile", "windows-hooks-compile", "windows-worker-launcher-unit", "macos-gym-ownership"):
            ran[job]["result"] = "success"
        self.assertEqual([], ci_success.evaluate(ran, "push", "refs/heads/main"))

    def test_failures_and_unknown_scopes_are_never_accepted(self) -> None:
        self.assertTrue(ci_success.evaluate(self.needs(vscode="failure"), "push", "refs/heads/main"))
        self.assertTrue(ci_success.evaluate(self.needs(test="skipped"), "push", "refs/heads/main"))
        self.assertTrue(ci_success.evaluate(self.needs(scope="bogus"), "push", "refs/heads/main"))

    def test_containment_gates_are_not_skippable_via_scope(self) -> None:
        for gate in ci_success.HEAVY_GATES:
            self.assertNotIn(gate, ci_success.TESTS_SCOPE_SKIPPABLE)
        self.assertNotIn("linux-sandbox-policy", ci_success.TESTS_SCOPE_SKIPPABLE)
        needs = self.needs(linux_sandbox_policy="skipped")
        self.assertTrue(ci_success.evaluate(needs, "push", "refs/heads/main"))


if __name__ == "__main__":
    unittest.main()
