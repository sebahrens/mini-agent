#!/usr/bin/env python3
"""Regression tests for ordinary VS Code extension CI coverage."""

from __future__ import annotations

import json
import re
import unittest
from pathlib import Path

from scripts.ci_success import evaluate
from scripts.tests.test_phase6_ci_workflow import job_body


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = REPOSITORY_ROOT / ".github" / "workflows" / "ci.yml"
WINDOWS_FIXTURE_STEP = "Test native Windows VS Code private fixture contracts"
WINDOWS_FIXTURE_COMMAND = "node --test editors/vscode/test/integration-fixture.node.mjs"


def validate_windows_fixture_gate(workflow: str) -> None:
    job = job_body(workflow, "windows-default-compile")
    if "runs-on: windows-latest" not in job:
        raise AssertionError("fixture ACL controls require native Windows")
    marker = "- name: " + WINDOWS_FIXTURE_STEP
    if job.count(marker) != 1:
        raise AssertionError("one native fixture acceptance step is required")
    step = job.split(marker, 1)[1].split("\n      - ", 1)[0]
    if any(flag in step for flag in ("if:", "continue-on-error:", "npm ")):
        raise AssertionError("fixture acceptance must run without skips or npm dependencies")
    for required in ("shell: pwsh", WINDOWS_FIXTURE_COMMAND,
                     "if ($LASTEXITCODE -ne 0) { throw"):
        if required not in step:
            raise AssertionError("missing native fixture execution guard: " + required)
    setup = "- name: Select Node for native Windows fixture contracts"
    if setup not in job:
        raise AssertionError("pinned fixture Node setup is required")
    node = job.split(setup, 1)[1].split("\n      - ", 1)[0]
    if "if:" in node or "actions/setup-node@49933ea5288caeca8642d1e84afbd3f7d6820020" not in node or "node-version-file: editors/vscode/.nvmrc" not in node:
        raise AssertionError("fixture controls must use the manifest Node toolchain")
    if not job.index(setup) < job.index(marker) < job.index("- name: Prepare guarded Git runtime") < job.index("cargo test"):
        raise AssertionError("fixture acceptance must run before native producer and Cargo work")
    aggregate = job_body(workflow, "ci-success").split("    runs-on:", 1)[0]
    if "      - windows-default-compile\n" not in aggregate:
        raise AssertionError("native fixture job must gate ci-success")


class VsCodeCiWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        match = re.search(
            r"(?ms)^  vscode:\n(?P<body>.*?)(?=^  [a-zA-Z0-9_-]+:\n|\Z)",
            workflow,
        )
        if match is None:
            raise AssertionError("workflow job 'vscode' is missing")
        cls.body = match.group("body")

    def test_ci_and_release_install_the_manifest_toolchain_before_dependencies(self) -> None:
        manifest = json.loads((REPOSITORY_ROOT / "editors/vscode/package.json").read_text())
        self.assertRegex(manifest["packageManager"], r"^npm@\d+\.\d+\.\d+$")
        release = (REPOSITORY_ROOT / ".github/workflows/release.yml").read_text()
        release_job = re.search(
            r"(?ms)^  vscode-vsix:\n(?P<body>.*?)(?=^  [a-zA-Z0-9_-]+:\n|\Z)",
            release,
        )
        self.assertIsNotNone(release_job, "the native release job must exist")
        for name, body in (("ci", self.body), ("release", release_job.group("body"))):
            with self.subTest(workflow=name):
                self.assertIn("node-version-file: editors/vscode/.nvmrc", body)
                self.assertIn("working-directory: editors/vscode", body)
                self.assertIn("cache-dependency-path: editors/vscode/package-lock.json", body)
                # A version assertion alone cannot install the resolver declared
                # by packageManager. Both workflows must actually select it, from
                # the extension directory, before resolving its lockfile.
                install = "npm install --global \"$(node --print \"require('./package.json').packageManager\")\""
                verify = "test \"npm@$(npm --version)\" = \"$(node --print \"require('./package.json').packageManager\")\""
                self.assertIn(install, body)
                self.assertIn(verify, body)
                self.assertLess(body.index(install), body.index(verify))
                self.assertLess(body.index(verify), body.index("npm ci --no-audit --no-fund"))

    def test_job_runs_every_required_extension_gate(self) -> None:
        for command in (
            "npm ci --no-audit --no-fund",
            "npm run typecheck",
            "npm run lint",
            "npm test",
            "npm audit --audit-level=high",
            "cargo install --path . --debug --locked",
            "cp \"$RUNNER_TEMP/vscode-native/bin/mini-agent\" editors/vscode/bin/linux-x64/mini-agent",
            "npm run package:linux-x64",
            "npm run sbom",
        ):
            with self.subTest(command=command):
                self.assertIn(command, self.body)

    def test_vsix_bundles_the_native_binary_license_inventory(self) -> None:
        self.assertIn(
            "python3 scripts/third_party_licenses.py generate \\\n"
            "            --target x86_64-unknown-linux-gnu \\\n"
            "            --output editors/vscode/bin/linux-x64/THIRD_PARTY_LICENSES",
            self.body,
        )
        release = (REPOSITORY_ROOT / ".github/workflows/release.yml").read_text()
        self.assertIn(
            '-C "editors/vscode/bin/$VSCODE_TARGET" "$BINARY_NAME" THIRD_PARTY_LICENSES',
            release,
        )
        scripts = REPOSITORY_ROOT / "editors/vscode/scripts"
        self.assertIn(
            "`!bin/${target}/THIRD_PARTY_LICENSES`",
            (scripts / "package-target.mjs").read_text(encoding="utf-8"),
        )
        self.assertIn(
            "`extension/bin/${target}/THIRD_PARTY_LICENSES`",
            (scripts / "verify-vsix.mjs").read_text(encoding="utf-8"),
        )

    def test_job_uses_the_standard_non_scheduled_guard(self) -> None:
        condition = self.body.splitlines()[0].strip()
        self.assertIn("github.event_name != 'schedule'", condition)
        self.assertIn("inputs.scope != 'windows-general-sandbox'", condition)

    def test_native_windows_fixture_acceptance_precedes_producer_and_cargo(self) -> None:
        validate_windows_fixture_gate(WORKFLOW.read_text(encoding="utf-8"))
        for result in ("failure", "cancelled", "skipped"):
            with self.subTest(result=result):
                needs = {
                    "changes": {"result": "success", "outputs": {"code": "true"}},
                    "fmt": {"result": "success"},
                    "windows-default-compile": {"result": result},
                }
                errors = evaluate(needs, "push", "refs/tags/vfixture")
                self.assertTrue(any("windows-default-compile" in error for error in errors))

    def test_native_windows_fixture_gate_rejects_missing_skipped_or_non_native_proof(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        marker = "- name: " + WINDOWS_FIXTURE_STEP
        mutations = (
            workflow.replace(WINDOWS_FIXTURE_COMMAND, "echo fixture proof omitted"),
            workflow.replace(marker, marker + "\n        if: false"),
            workflow.replace(marker, marker + "\n        continue-on-error: true"),
            workflow.replace("- name: Select Node for native Windows fixture contracts", "- name: Omitted fixture Node setup"),
            workflow.replace("actions/setup-node@49933ea5288caeca8642d1e84afbd3f7d6820020", "actions/setup-node@v4"),
            workflow.replace("runs-on: windows-latest", "runs-on: ubuntu-latest"),
            workflow.replace("      - windows-default-compile\n", ""),
        )
        for index, mutated in enumerate(mutations):
            with self.subTest(mutation=index), self.assertRaises(AssertionError):
                validate_windows_fixture_gate(mutated)


if __name__ == "__main__":
    unittest.main()
