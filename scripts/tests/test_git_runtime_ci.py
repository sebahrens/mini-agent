"""Owning policy tests for pinned Windows Git preparation before Cargo."""
from pathlib import Path
import re
import unittest

from scripts.tests.test_phase6_ci_workflow import job_body, workflow_jobs
from scripts.tests.test_check_package_metadata import CHECK_PACKAGE_METADATA


ROOT = Path(__file__).resolve().parents[2]
ACTION = ROOT / ".github/actions/prepare-git-runtime/action.yml"
PREP = "uses: ./.github/actions/prepare-git-runtime"
PIN = "ec48f7c5447b3140e2b088413ae3a55687bccb6e"
CARGO = re.compile(r"\bcargo\s+(?:test|install|build|clippy|check|metadata)\b|& \$CargoExe\b", re.I)
EXPECTED_WINDOWS_JOBS = {
    "windows-default-compile", "windows-hooks-compile",
    "windows-worker-launcher-unit", "windows-general-sandbox-policy",
    "windows-worker-containment-gate", "platform-paths",
    "platform-path-install-smoke", "mcp-stdio", "lsp-lifecycle",
}


def windows_cargo_jobs(workflow):
    result = {}
    for name in workflow_jobs(workflow):
        body = job_body(workflow, name)
        header = body.split("    steps:\n", 1)[0]
        if "windows-latest" in header and CARGO.search(body):
            result[name] = body
    return result


def validate_preparation(body):
    if body.count(PREP) != 1:
        raise AssertionError("every Windows Cargo job needs exactly one Git preparation")
    if body.index("uses: actions/checkout@") > body.index(PREP):
        raise AssertionError("local action requires checkout first")
    cargo = CARGO.search(body)
    if cargo is None or body.index(PREP) > cargo.start():
        raise AssertionError("Git preparation must precede the first Cargo command")
    step = body[:body.index(PREP)].rsplit("      - name:", 1)[1]
    if "continue-on-error" in step:
        raise AssertionError("Git preparation cannot fail open")
    if "runs-on: ${{ matrix.os }}" in body:
        if "if: matrix.os == 'windows-latest'" not in step:
            raise AssertionError("Unix matrix jobs must retain their native default bootstrap")
    elif "if:" in step:
        raise AssertionError("native Windows preparation must not be optional")


WINDOWS_SERVICE_TESTS = {
    "git::runner::windows_runtime_tests::windows_embedded_http_global_helpers_with_cmd_only_path_and_spaces",
    "git::runner::windows_runtime_tests::windows_shell_service_rejects_overlap_later_grants_and_replacement",
    "git::runner::windows_runtime_tests::windows_missing_shell_refuses_only_selected_shell_services",
}


def validate_windows_services(body):
    step = body.split('- name: Test native Windows guarded Git shell services\n', 1)[1].split('\n      - name:', 1)[0]
    names = set(re.findall(r"'(git::runner::windows_runtime_tests::[^']+): test'", step))
    if names != WINDOWS_SERVICE_TESTS:
        raise AssertionError("exact three guarded Windows Git service tests must be discovered")
    required = [
        "$filter = 'git::runner::windows_runtime_tests::'",
        '$listing = @(& cargo test --locked --bin mini-agent $filter -- --list)',
        "if ($LASTEXITCODE -ne 0) { throw 'Native Windows Git service discovery failed' }",
        '$actual.Count -ne 3 -or $difference.Count -ne 0',
        '$result = @(& cargo test --locked --bin mini-agent $filter -- --nocapture --test-threads=1)',
        '$exit = $LASTEXITCODE',
        "$exit -ne 0 -or -not ($result -match '^test result: ok\\. 3 passed; 0 failed; 0 ignored;')",
    ]
    if any(item not in step for item in required) or 'continue-on-error' in step:
        raise AssertionError("native service discovery and actual three-case execution must fail closed")
    prep = body.split('- name: Prepare guarded Git runtime\n', 1)[1].split('\n      - name:', 1)[0]
    if 'collect-sources: "true"' not in prep:
        raise AssertionError("owning Windows row must collect actual signed-package sources")
    artifact = body.split('- name: Upload verified Windows Git sources\n', 1)[1].split('\n      - name:', 1)[0]
    for item in ('actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a',
                 'name: native-git-sources-x86_64-pc-windows-msvc',
                 'path: ${{ env.MINI_AGENT_GIT_NATIVE_SOURCE_ROOT }}', 'if-no-files-found: error'):
        if item not in artifact:
            raise AssertionError("verified Windows source assets must be retained for offline archive proof")


class GitRuntimeCiTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.ci = (ROOT / ".github/workflows/ci.yml").read_text()
        cls.release = (ROOT / ".github/workflows/release.yml").read_text()
        cls.action = ACTION.read_text()

    def test_every_current_windows_cargo_job_prepares_once_before_cargo(self):
        jobs = windows_cargo_jobs(self.ci)
        self.assertEqual(set(jobs), EXPECTED_WINDOWS_JOBS)
        for name, body in jobs.items():
            with self.subTest(job=name):
                validate_preparation(body)
        self.assertNotIn(PREP, job_body(self.ci, "windows-msi"))
        self.assertIn("python3 -m unittest scripts.tests.test_git_runtime_ci", job_body(self.ci, "fmt"))

    def test_changed_yaml_parses_and_release_pins_remain_accepted(self):
        for document in (self.ci, self.release, self.action):
            self.assertIsInstance(CHECK_PACKAGE_METADATA.parse_yaml_document(document), dict)
        self.assertEqual([], CHECK_PACKAGE_METADATA.validate_release_action_pins(self.release))

    def test_missing_late_duplicate_or_overbroad_prep_is_rejected(self):
        body = job_body(self.ci, "platform-paths")
        start = body.index("      - name: Prepare guarded Git runtime")
        end = body.index("\n      - ", body.index(PREP))
        step = body[start:end]
        bad = [
            body.replace(step, "", 1),
            body.replace(step, "", 1) + "\n" + step,
            body + "\n" + step,
            body.replace("if: matrix.os == 'windows-latest'", "if: always()", 1),
        ]
        for candidate in bad:
            with self.subTest(candidate=candidate[-50:]), self.assertRaises(AssertionError):
                validate_preparation(candidate)

    def test_release_full_and_lite_share_one_windows_bundle(self):
        windows = job_body(self.release, "build-windows")
        validate_preparation(windows)
        self.assertLess(windows.index(PREP), windows.index("- name: Build\n"))
        self.assertLess(windows.index(PREP), windows.index("- name: Build lite\n"))
        # Static-musl preparation is owned by the separate static publisher;
        # this action cannot label a Windows/host bundle as musl.
        self.assertNotIn(PREP, job_body(self.release, "build-static"))
        self.assertNotIn(PREP, job_body(self.release, "build"))

    def test_action_pins_official_toolchain_and_does_not_change_home(self):
        self.assertIn("scripts.tests.test_collect_git_native_sources", job_body(self.ci, "fmt"))
        self.assertIn("scripts.tests.test_build_git_runtime_musl", job_body(self.ci, "fmt"))
        self.assertIn("using: composite", self.action)
        self.assertIn(f"uses: msys2/setup-msys2@{PIN}", self.action)
        self.assertEqual(self.action.count("if: runner.os == 'Windows'"), 3)
        self.assertIn("msystem: UCRT64", self.action)
        self.assertIn("path-type: minimal", self.action)
        for package in ["make patch python", "gcc", "curl-winssl", "zlib", "expat", "libiconv"]:
            self.assertIn(package, self.action)
        self.assertIn("shell: msys2 {0}", self.action)
        self.assertIn("/usr/bin/python3 scripts/build-git-runtime-windows.py", self.action)
        self.assertEqual(self.action.count("scripts/build-git-runtime-windows.py"), 1)
        self.assertIn("set -euo pipefail", self.action)
        self.assertIn('test -s "$owned/bundle/bundle.manifest"', self.action)
        self.assertIn('bundle=$(/usr/bin/cygpath -am "$owned/bundle")', self.action)
        self.assertIn("MINI_AGENT_GIT_RUNTIME_BUNDLE=%s", self.action)
        self.assertIn("$present = Test-Path Env:HOME", self.action)
        self.assertIn('export HOME="$MINI_AGENT_BUILD_HOST_HOME"', self.action)
        self.assertIn("unset HOME", self.action)
        self.assertIn('/usr/bin/pacman -Q > "$owned/toolchain-packages.txt"', self.action)
        for forbidden in ["continue-on-error", "cargo ", "git clone", "curl https", "wget ", "sudo "]:
            self.assertNotIn(forbidden, self.action)

    def test_release_native_records_are_required_before_notices_and_source_assembly(self):
        windows = job_body(self.release, "build-windows")
        self.assertIn('collect-sources: "true"', windows)
        self.assertLess(windows.index('collect-sources: "true"'), windows.index('Generate third-party license inventories'))
        static = job_body(self.release, "build-static")
        self.assertIn('--native-sources-output "$RUNNER_TEMP/native-git-sources-${{ matrix.target }}"', static)
        self.assertLess(static.index('--native-sources-output'), static.index('Generate third-party license inventories'))
        self.assertIn('MINI_AGENT_GIT_NATIVE_RECORD=', static)
        # Keep the exact approved cross-architecture provision and relative Cross input.
        self.assertIn('docker/setup-qemu-action@c7c53464625b32c7a7e944ae62b3e17d2b600130', static)
        self.assertIn('tonistiigi/binfmt@sha256:400a4873b838d1b89194d982c45e5fb3cda4593fbfd7e08a02e76b03b21166f0', static)
        self.assertIn('MINI_AGENT_GIT_RUNTIME_BUNDLE=$relative', static)
        for body in (windows, static):
            self.assertIn('name: native-git-sources-${{ matrix.target }}', body)
            self.assertIn('path: ${{ env.MINI_AGENT_GIT_NATIVE_SOURCE_ROOT }}', body)
            self.assertIn('if-no-files-found: error', body)
        source = job_body(self.release, "corresponding-source")
        self.assertIn('needs: [verify-ci, package-metadata, build-static, build-windows]', source)
        self.assertIn('pattern: native-git-sources-*', source)
        self.assertIn('for target in x86_64-pc-windows-msvc x86_64-unknown-linux-musl aarch64-unknown-linux-musl;', source)
        self.assertIn('test -s "$record"', source)
        self.assertIn('records+=(--native-record "$record")', source)
        self.assertIn('HEAD "${records[@]}"', source)

    def test_windows_source_capture_is_explicit_bounded_and_not_runtime_authority(self):
        self.assertIn('collect-sources:', self.action)
        self.assertIn('default: "false"', self.action)
        self.assertIn('make patch python zstd', self.action)
        self.assertIn('if [ "$MINI_AGENT_GIT_COLLECT_SOURCES" = true ]; then', self.action)
        self.assertIn('/usr/bin/python3 scripts/collect-git-native-sources.py', self.action)
        self.assertIn('--distro msys2 --target x86_64-pc-windows-msvc', self.action)
        self.assertIn('--bundle "$owned/bundle" --output "$owned/native-sources"', self.action)
        self.assertIn('source_root=$(/usr/bin/cygpath -am "$owned/native-sources")', self.action)
        self.assertIn('MINI_AGENT_GIT_NATIVE_RECORD=%s/native-components.json', self.action)

    def test_native_windows_shell_services_are_exactly_discovered_executed_and_retained(self):
        body = job_body(self.ci, "windows-default-compile")
        validate_windows_services(body)
        for original, replacement in [
            ('windows_missing_shell_refuses_only_selected_shell_services: test', 'missing_case: test'),
            ('$actual.Count -ne 3', '$actual.Count -ne 2'),
            ('$exit -ne 0 -or -not', '$exit -ne 0 -and -not'),
            ('3 passed; 0 failed; 0 ignored;', '2 passed; 0 failed; 0 ignored;'),
            ('collect-sources: "true"', 'collect-sources: "false"'),
            ('if-no-files-found: error', 'if-no-files-found: warn'),
        ]:
            with self.subTest(original=original), self.assertRaises(AssertionError):
                validate_windows_services(body.replace(original, replacement))

    def test_standard_user_build_uses_copied_input_after_sensitive_env_scrub(self):
        windows = job_body(self.ci, "windows-worker-containment-gate")
        step = windows.split("- name: Validate the complete gate from a separate standard-user installation", 1)[1].split("\n      - name:", 1)[0]
        copy = "Copy-Item -LiteralPath $env:MINI_AGENT_GIT_RUNTIME_BUNDLE -Destination $gitRuntimeBundle -Recurse"
        self.assertIn(copy, step)
        self.assertLess(step.index(copy), step.index("icacls.exe $standardRoot /setowner"))
        self.assertIn("[Parameter(Mandatory)][string] $GitRuntimeBundle,", step)
        self.assertIn("'-GitRuntimeBundle'", step)
        self.assertIn("(Quote-Argument $gitRuntimeBundle)", step)
        self.assertIn("$expectedGitBundle = Join-Path $SourceRoot 'target\\git-runtime-bundle'", step)
        self.assertIn("[System.IO.Path]::GetFullPath($GitRuntimeBundle)", step)
        self.assertIn("exit 14", step)
        assignment = "$env:MINI_AGENT_GIT_RUNTIME_BUNDLE = $GitRuntimeBundle"
        self.assertLess(step.index("if ($remainingSensitive.Count -ne 0)"), step.index(assignment))
        self.assertLess(step.index(assignment), step.index("& $CargoExe install"))
        self.assertIn("'MINI_AGENT_'", step)  # no broad inherited-input exemption
        self.assertIn("$ExpectedAccount", step)
        self.assertIn("$tokenPrincipal.IsInRole", step)


if __name__ == "__main__":
    unittest.main()
