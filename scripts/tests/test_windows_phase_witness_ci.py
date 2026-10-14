"""Private native experiment scope: never expand the full CI or weaken its gates."""
import re
import hashlib
import subprocess
import tempfile
import unittest
from pathlib import Path

from scripts.tests.test_ci_docs_filter import CiDocsFilterTests, filter_script
from scripts.tests.test_windows_gh_ci import admitted_jobs, condition_value, job_body, validate_scope

ROOT = Path(__file__).resolve().parents[2]
SCOPE = "windows-helper-phase-witness"
STEP = "Test four native Windows helper phase witnesses"
PRIOR = (
    "Compile the Windows default feature suite",
    "Test native ledger lock and persistence contracts",
    "Test native one MiB App event loops",
    "Test native Windows GH argv and actual foreground authority",
)
PREFIX = "sandbox::windows::phase_witness_tests::"
NAMES = {
    PREFIX + "windows_phase_witness_known_store_survives_forced_kill_after_exact_retirement",
    PREFIX + "windows_phase_witness_target_cannot_inherit_noninheritable_observer",
    PREFIX + "windows_phase_witness_whole_request_and_feeder_errors_retire_without_effect",
    PREFIX + "windows_phase_witness_invalid_stage_and_none_preserve_fixed_target_outcome",
}
SOURCES = {
    "docs/agent/TOOL_RUNTIME.md", "src/sandbox/windows.rs",
    "src/tests/windows_foreground_job_tests.rs", "src/tests/windows_phase_witness_tests.rs",
    "tests/fixtures/windows_gh_fixture.rs", "tests/fixtures/windows_phase_probe.rs",
}


def validate(workflow):
    validate_scope(workflow)  # Existing full/PR/push/general/command policy.
    if admitted_jobs(workflow, "workflow_dispatch", SCOPE, "false") != {"changes", "windows-default-compile"}:
        raise AssertionError("phase dispatch must admit only changes and its native job")
    if f"          - {SCOPE}\n" not in workflow.split("  push:", 1)[0]:
        raise AssertionError("exact manual choice missing")
    native = job_body(workflow, "windows-default-compile")
    for name in PRIOR:
        step = native.split("      - name: " + name + "\n", 1)[1]
        condition = step.splitlines()[0].strip().removeprefix("if: ")
        if condition_value(condition, "workflow_dispatch", SCOPE, "false"):
            raise AssertionError("phase dispatch repeats unchanged gate: " + name)
        for event, scope in (("push", SCOPE), ("pull_request", SCOPE), ("workflow_dispatch", "full"), ("workflow_dispatch", "windows-command-family")):
            if not condition_value(condition, event, scope, "true"):
                raise AssertionError("old gate behavior changed: " + name)
    step = native.split("      - name: " + STEP + "\n", 1)[1].split("\n      - name:", 1)[0]
    condition = step.splitlines()[0].strip().removeprefix("if: ")
    if not condition_value(condition, "workflow_dispatch", SCOPE, "false"):
        raise AssertionError("phase native step missing")
    for event, scope in (("push", SCOPE), ("pull_request", SCOPE), ("workflow_dispatch", "full"), ("workflow_dispatch", "windows-command-family")):
        if condition_value(condition, event, scope, "true"):
            raise AssertionError("phase experiment admitted on another event/scope")
    names = set(re.findall(r"'(sandbox::windows::phase_witness_tests::\w+): test'", step))
    if names != NAMES:
        raise AssertionError("phase discovery names changed")
    sources = re.findall(r"'([^']+)' = '[a-f0-9]{64}'", step)
    if len(sources) != 6 or set(sources) != SOURCES:
        raise AssertionError("phase exact six source identities changed")
    for required in (
        "$source -ne '22d2372853b5a1fe2c53497fecc153d733a0b4d4'",
        "git cat-file -p HEAD", "$parents.Count -ne 1",
        "Get-FileHash -LiteralPath $entry.Key -Algorithm SHA256",
        "$size -gt 1048576", "PHASE108 fixed_source=",
        "$expectedHashes.Count -ne 6",
        "git -c core.autocrlf=false checkout-index --force -- @sourcePaths",
        "$sourcePaths = @($expectedHashes.Keys | Sort-Object)",
        "if ($LASTEXITCODE -ne 0) { throw 'Phase fixed source materialization failed' }",
        "$features = 'sandbox,git-worktree'",
        "--test windows_gh_fixture --no-run --message-format=json",
        "$_.reason -eq 'compiler-artifact'",
        "$_.target.kind -contains 'test'",
        "$images.Count -ne 1", "$actual.Count -ne 4", "$difference.Count -ne 0",
        "Compare-Object -ReferenceObject $expected -DifferenceObject $actual",
        "$filter -- --ignored --list", "$name -- --exact --ignored --nocapture --test-threads=1",
        "$exit -ne 0", "^test result: ok\\. 1 passed; 0 failed; 0 ignored;",
        "$env:MINI_AGENT_WINDOWS_GH_FIXTURE_EXE = $fixture", "} finally {",
        "Remove-Item Env:MINI_AGENT_WINDOWS_GH_FIXTURE_EXE", "if ($failed) { throw",
    ):
        if required not in step:
            raise AssertionError("phase guard missing: " + required)


class PhaseWitnessCiTests(unittest.TestCase):
    def test_exact_scope_and_old_event_policy(self):
        validate((ROOT / ".github/workflows/ci.yml").read_text())
        fixture = CiDocsFilterTests()
        fixture.script = filter_script()
        self.assertEqual(fixture.classify(event="workflow_dispatch", scope=SCOPE, base=""), "false")
        for event in ("push", "pull_request", "schedule"):
            self.assertEqual(fixture.classify("src/main.rs", event=event, scope=SCOPE), "true")
        for scope in (SCOPE + "-extra", "full", "unknown"):
            self.assertEqual(fixture.classify(event="workflow_dispatch", scope=scope, base=""), "true")

    def test_zero_missing_extra_repeated_or_unbounded_controls_fail(self):
        source = (ROOT / ".github/workflows/ci.yml").read_text()
        changes = (
            ("$actual.Count -ne 4", "$actual.Count -lt 0"),
            ("$images.Count -ne 1", "$images.Count -lt 0"),
            ("$difference.Count -ne 0", "$difference.Count -lt 0"),
            ("$name -- --exact --ignored --nocapture --test-threads=1", "$name -- --ignored"),
            ("^test result: ok\\. 1 passed; 0 failed; 0 ignored;", "test result:"),
            ("$source -ne '22d2372853b5a1fe2c53497fecc153d733a0b4d4'", "$false"),
            ("if: github.event_name != 'workflow_dispatch' || inputs.scope != '" + SCOPE + "'", "if: always()"),
            ("git -c core.autocrlf=false checkout-index --force -- @sourcePaths", "git config --global core.autocrlf false"),
            ("$size -gt 1048576", "$false"),
            ("$expectedHashes.Count -ne 6", "$false"),
        )
        for old, new in changes:
            with self.subTest(old=old), self.assertRaises(AssertionError):
                validate(source.replace(old, new))
        for name in NAMES:
            with self.subTest(name=name), self.assertRaises(AssertionError):
                validate(source.replace(name + ": test", "wrong_test: test"))
        for name in SOURCES:
            with self.subTest(source=name), self.assertRaises(AssertionError):
                validate(source.replace("'" + name + "' =", "'missing' ="))

    def test_transient_index_materialization_restores_exact_lf_without_blessing_changes(self):
        # Controlled fixture of the workflow's exact Git operation. This proves
        # no native cause: the hosted before-SHA/eol report remains authoritative.
        with tempfile.TemporaryDirectory(prefix="mini-agent-phase-checkout-") as directory:
            root = Path(directory)
            def git(*args):
                return subprocess.run(["git", *args], cwd=root, check=True, capture_output=True).stdout
            git("init", "--quiet")
            source = root / "source.rs"
            original = b"fn inert() {}\n// fixed source\n"
            source.write_bytes(original)
            git("-c", "core.autocrlf=false", "add", "--", "source.rs")
            git("config", "core.autocrlf", "true")
            source.unlink()
            git("checkout-index", "--force", "--", "source.rs")
            self.assertEqual(source.read_bytes(), original.replace(b"\n", b"\r\n"))
            self.assertNotEqual(hashlib.sha256(source.read_bytes()).digest(), hashlib.sha256(original).digest())
            git("-c", "core.autocrlf=false", "checkout-index", "--force", "--", "source.rs")
            self.assertEqual(source.read_bytes(), original)
            self.assertEqual(git("config", "core.autocrlf").strip(), b"true")
            source.write_bytes(original + b"// changed\n")
            self.assertNotEqual(hashlib.sha256(source.read_bytes()).digest(), hashlib.sha256(original).digest())


if __name__ == "__main__":
    unittest.main()
