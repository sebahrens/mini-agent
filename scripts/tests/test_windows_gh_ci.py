"""Guard the narrowly scoped native Windows PR acceptance lane."""
from pathlib import Path
import re
import tomllib
import unittest
from scripts.tests.test_phase6_ci_workflow import job_body

ROOT = Path(__file__).resolve().parents[2]
STEP = "Test native Windows GH argv and actual foreground authority"
CASES = {
    "windows_native_pr_cancel_settles_job_before_releasing_late_effect",
    "windows_native_pr_nonzero_ambiguous_incomplete_stdout_remain_errors",
    "windows_native_pr_posthook_text_cannot_spoof_actual_result",
    "windows_native_pr_powershell_args_wait_for_normal_ask_and_use_actual_stdout",
    "windows_native_pr_typed_foreground_observes_real_exit_and_bounded_completeness",
}


def validate_native_step(workflow):
    job = job_body(workflow, "windows-default-compile")
    if "runs-on: windows-latest" not in job or job.count("- name: " + STEP) != 1:
        raise AssertionError("one owning native Windows step is required")
    body = job.split("- name: " + STEP, 1)[1].split("\n  #", 1)[0]
    required = [
        "shell: pwsh", "$features = 'git-worktree,sandbox,hooks'",
        "--test windows_gh_fixture --no-run --message-format=json",
        "$_.reason -eq 'compiler-artifact'", "$_.target.name -eq 'windows_gh_fixture'",
        "$_.target.kind -contains 'test'", "$images.Count -ne 1",
        "Test-Path -LiteralPath $fixture -PathType Leaf", "GetExtension($fixture) -ne '.exe'",
        "& $fixture --self-test", "$filter = 'ui::slash::git::windows_native_pr_tests::'",
        "--bin mini-agent $filter -- --ignored --list", "$actual.Count -ne 5",
        "Compare-Object -ReferenceObject $expected -DifferenceObject $actual",
        "$env:MINI_AGENT_WINDOWS_GH_FIXTURE_EXE = $fixture", "finally {",
        "Remove-Item Env:MINI_AGENT_WINDOWS_GH_FIXTURE_EXE",
        "--bin mini-agent $filter -- --ignored --nocapture --test-threads=1",
    ]
    for fragment in required:
        if fragment not in body:
            raise AssertionError("missing native proof guard: " + fragment)
    names = set(re.findall(r"ui::slash::git::windows_native_pr_tests::(\w+): test", body))
    if names != CASES:
        raise AssertionError("exact five acceptance cases required")
    if body.count("if ($LASTEXITCODE -ne 0)") != 4:
        raise AssertionError("each compilation/discovery/proof exit must be checked")
    if "cargo build" in body or "--release" in body or "RUST_MIN_STACK" in body:
        raise AssertionError("development policy or fixed stack boundary widened")
    return body


class WindowsGhCiTests(unittest.TestCase):
    def test_exact_native_lane_and_fixture_role_are_guarded(self):
        validate_native_step((ROOT / ".github/workflows/ci.yml").read_text())
        manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())
        targets = [entry for entry in manifest["test"] if entry["name"] == "windows_gh_fixture"]
        self.assertEqual(len(targets), 1)
        self.assertFalse(targets[0]["harness"])
        self.assertEqual(targets[0]["required-features"], ["git-worktree", "sandbox"])
        self.assertNotIn("windows_gh_fixture", [entry["name"] for entry in manifest.get("bin", [])])
        source = (ROOT / targets[0]["path"]).read_text()
        self.assertIn('"MINI_AGENT_TEST_GH_ROLE"', source)
        self.assertIn('Some(std::ffi::OsStr::new("native32"))', source)
        self.assertIn('return Ok(0);', source)
        self.assertNotIn('Command::new', source)
        self.assertNotIn('reqwest', source)
        native = (ROOT / "src/ui/slash/git.rs").read_text().split("mod windows_native_pr_tests", 1)[1]
        self.assertEqual(set(re.findall(r"async fn (windows_native_pr_\w+)\(", native)), CASES)
        self.assertEqual(native.count('#[ignore = "requires guarded native Windows GH fixture discovery"]'), 5)

    def test_stack_lane_requires_exact_three_actual_portable_consumers(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        body = job_body(workflow, "windows-default-compile").split("- name: Test native one MiB App event loops", 1)[1].split("- name: " + STEP, 1)[0]
        self.assertIn("$actual.Count -ne 3", body)
        self.assertIn("Compare-Object -ReferenceObject $expected -DifferenceObject $actual", body)
        self.assertIn("ui::app::git_confirmation_stack_tests::actual_combined_context_and_git_confirmation_event_loop_runs_on_one_mib_stack: test", body)
        self.assertIn("-- @filters --test-threads=1", body)
        source = (ROOT / "src/ui/app.rs").read_text().split("mod git_confirmation_stack_tests", 1)[1]
        self.assertIn(".stack_size(1024 * 1024)", source)
        self.assertIn('root.path().join(".git").exists()', source)
        self.assertNotIn("Repo::", source)
        self.assertNotIn("repo.git", source)
        self.assertNotIn('target_os = "macos"', source)
        self.assertIn("127.0.0.1:9", source)

    def test_missing_or_weakened_discovery_controls_are_rejected(self):
        original = (ROOT / ".github/workflows/ci.yml").read_text()
        for before, after in [
            ("$images.Count -ne 1", "$images.Count -lt 0"),
            ("$actual.Count -ne 5", "$actual.Count -lt 0"),
            ("--ignored --list", "--list"),
            ("--ignored --nocapture --test-threads=1", "--nocapture"),
            ("$_.target.kind -contains 'test'", "$_.target.kind -contains 'bin'"),
            ("--bin mini-agent $filter", "--bin mini-agent"),
            ("windows_native_pr_posthook_text_cannot_spoof_actual_result: test", "other: test"),
        ]:
            with self.subTest(before=before), self.assertRaises(AssertionError):
                validate_native_step(original.replace(before, after))


if __name__ == "__main__":
    unittest.main()
