from __future__ import annotations

import importlib.util
import io
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock


SCRIPT = Path(__file__).parents[1] / "release_artifacts.py"
SPEC = importlib.util.spec_from_file_location("release_artifacts", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
RELEASE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RELEASE)
ROOT = Path(__file__).parents[2]


class ReleaseArtifactManifestTests(unittest.TestCase):
    def test_windows_release_smoke_is_clean_runner_and_non_publishing(self) -> None:
        workflow = (ROOT / ".github/workflows/windows-release-smoke.yml").read_text()
        self.assertEqual(workflow.count("runs-on: windows-latest"), 2)
        self.assertIn("needs: build-windows-release-artifact", workflow)
        self.assertIn("RELEASE_TARGET: x86_64-pc-windows-msvc", workflow)
        self.assertIn(
            'cargo build --locked --release --target "$RELEASE_TARGET"', workflow
        )
        self.assertIn("name: windows-release-archive-candidate", workflow)
        self.assertIn("path: smoke-input", workflow)
        self.assertIn("python3 scripts/release_artifacts.py smoke", workflow)
        self.assertIn('--expected-version "$version"', workflow)
        self.assertIn("--expect-js yes", workflow)
        self.assertIn("python3 scripts/release_artifacts.py manifest", workflow)
        self.assertIn("python3 scripts/release_artifacts.py verify", workflow)
        self.assertIn("contents: read", workflow)
        self.assertNotIn("gh release", workflow)
        self.assertNotIn("contents: write", workflow)
        smoke_job = workflow.split("  smoke-windows-release-artifact:", 1)[1]
        self.assertNotIn("cargo build", smoke_job)
        self.assertNotIn("cargo install", smoke_job)

    def test_windows_smoke_uses_existing_local_per_user_install_root(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = RELEASE._smoke_install_parent(
                {"LOCALAPPDATA": directory}, platform_name="nt"
            )
            self.assertEqual(root, Path(directory))

        with self.assertRaisesRegex(
            RELEASE.ReleaseArtifactError, "per-user installation root"
        ):
            RELEASE._smoke_install_parent({}, platform_name="nt")

        self.assertIsNone(
            RELEASE._smoke_install_parent({}, platform_name="posix")
        )

    def fixture(self, directory: str) -> tuple[Path, Path, Path]:
        root = Path(directory) / "private"
        (root / "job-a").mkdir(parents=True)
        (root / "job-b").mkdir()
        (root / "job-a/a.tar.gz").write_bytes(b"a")
        (root / "job-b/b.tar.gz").write_bytes(b"b")
        expected = Path(directory) / "expected.txt"
        expected.write_text("a.tar.gz\nb.tar.gz\n", encoding="ascii")
        return root, expected, Path(directory) / "SHA256SUMS"

    def test_manifest_is_complete_sorted_and_deterministic(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root, expected, manifest = self.fixture(directory)
            RELEASE.build_manifest(root, expected, manifest)
            first = manifest.read_bytes()
            RELEASE.build_manifest(root, expected, manifest)
            self.assertEqual(first, manifest.read_bytes())
            RELEASE.verify_manifest(root, expected, manifest)

    def test_expected_input_order_does_not_change_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root, expected, manifest = self.fixture(directory)
            expected.write_text("b.tar.gz\na.tar.gz\n", encoding="ascii")
            RELEASE.build_manifest(root, expected, manifest)
            self.assertEqual(
                ["a.tar.gz", "b.tar.gz"],
                [line.split("  ", 1)[1] for line in manifest.read_text().splitlines()],
            )

    def test_missing_extra_duplicate_and_unsafe_candidates_fail(self) -> None:
        mutations = ("missing", "extra", "duplicate", "unsafe")
        for mutation in mutations:
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as directory:
                root, expected, manifest = self.fixture(directory)
                if mutation == "missing":
                    (root / "job-a/a.tar.gz").unlink()
                elif mutation == "extra":
                    (root / "job-a/extra.tar.gz").write_bytes(b"extra")
                elif mutation == "duplicate":
                    (root / "job-b/a.tar.gz").write_bytes(b"duplicate")
                else:
                    (root / "job-a/bad name.tar.gz").write_bytes(b"unsafe")
                with self.assertRaises(RELEASE.ReleaseArtifactError):
                    RELEASE.build_manifest(root, expected, manifest)

    def test_one_byte_change_fails_verification(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root, expected, manifest = self.fixture(directory)
            RELEASE.build_manifest(root, expected, manifest)
            (root / "job-a/a.tar.gz").write_bytes(b"A")
            with self.assertRaisesRegex(RELEASE.ReleaseArtifactError, "checksum mismatch"):
                RELEASE.verify_manifest(root, expected, manifest)

    def test_malformed_duplicate_and_unsorted_manifest_fail(self) -> None:
        variants = (
            b"not-a-hash  a.tar.gz\n",
            (b"0" * 64 + b"  a.tar.gz\n") * 2,
            b"0" * 64 + b"  b.tar.gz\n" + b"0" * 64 + b"  a.tar.gz\n",
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "SHA256SUMS"
            for value in variants:
                with self.subTest(value=value):
                    path.write_bytes(value)
                    with self.assertRaises(RELEASE.ReleaseArtifactError):
                        RELEASE.parse_manifest(path)


class ReleaseArchiveLayoutTests(unittest.TestCase):
    def archive(self, directory: str, members: list[tuple[str, bytes, int]]) -> Path:
        path = Path(directory) / "candidate.tar.gz"
        with tarfile.open(path, "w:gz") as bundle:
            for name, contents, mode in members:
                info = tarfile.TarInfo(name)
                info.size = len(contents)
                info.mode = mode
                bundle.addfile(info, io.BytesIO(contents))
        return path

    def test_exact_layout_is_accepted(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            archive = self.archive(
                directory,
                [
                    ("mini-agent", b"binary", 0o755),
                    ("LICENSE", b"license", 0o644),
                    ("NOTICE", b"notice", 0o644),
                    ("SOURCE.md", b"source", 0o644),
                ],
            )
            RELEASE._validate_archive_members(archive, "mini-agent")

    def test_missing_extra_traversal_and_non_executable_members_fail(self) -> None:
        valid = [
            ("mini-agent", b"binary", 0o755),
            ("LICENSE", b"license", 0o644),
            ("NOTICE", b"notice", 0o644),
            ("SOURCE.md", b"source", 0o644),
        ]
        variants = (
            valid[:-1],
            [*valid, ("extra", b"extra", 0o644)],
            [("../mini-agent", b"binary", 0o755), *valid[1:]],
            [("mini-agent", b"binary", 0o644), *valid[1:]],
        )
        for members in variants:
            with self.subTest(members=members), tempfile.TemporaryDirectory() as directory:
                archive = self.archive(directory, members)
                with self.assertRaises(RELEASE.ReleaseArtifactError):
                    RELEASE._validate_archive_members(archive, "mini-agent")

    @unittest.skipIf(RELEASE.os.name == "nt", "fixture is a POSIX shell executable")
    def test_full_archive_runs_exact_version_and_js_runtime_checks(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            executable = (
                b"#!/bin/sh\n"
                b"case \"$1\" in\n"
                b"  --version) echo 'mini-agent 1.8.0' ;;\n"
                b"  --js-runtime-check) echo 'JS runtime check: PASS (2)' ;;\n"
                b"  *) exit 2 ;;\n"
                b"esac\n"
            )
            archive = self.archive(
                directory,
                [
                    ("mini-agent", executable, 0o755),
                    ("LICENSE", b"license", 0o644),
                    ("NOTICE", b"notice", 0o644),
                    ("SOURCE.md", b"source", 0o644),
                ],
            )
            RELEASE.smoke_archive(archive, "mini-agent", "1.8.0", True)

    def test_windows_failure_diagnostic_reports_only_closed_helper_status(self) -> None:
        completed = subprocess.CompletedProcess([], 68, "", "ignored")
        with mock.patch.object(RELEASE, "_run", return_value=completed) as run:
            status = RELEASE._closed_windows_preflight_status(
                Path("mini-agent.exe"), platform_name="nt"
            )

        self.assertEqual(status, "68")
        run.assert_called_once_with(
            Path("mini-agent.exe"),
            "--mini-agent-windows-worker-preflight-v1",
            environment=None,
        )

    def test_windows_smoke_install_directory_gets_private_inheritable_acl(self) -> None:
        environment = {"SystemRoot": r"C:\Windows", "PATH": r"C:\Windows\System32"}
        response = subprocess.CompletedProcess([], 0, "", "")
        with mock.patch.object(RELEASE.subprocess, "run", return_value=response) as run:
            RELEASE._harden_windows_install_directory(
                Path(r"C:\Users\runner\AppData\Local\smoke"),
                platform_name="nt",
                environment=environment,
            )

        run.assert_called_once()
        acl_command = run.call_args.args[0]
        self.assertEqual(
            acl_command[:5],
            [
                str(
                    Path(r"C:\Windows")
                    / "System32"
                    / "WindowsPowerShell"
                    / "v1.0"
                    / "powershell.exe"
                ),
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
            ],
        )
        self.assertIn("SetAccessRuleProtection($true, $false)", acl_command[5])
        self.assertIn("'S-1-5-18'", acl_command[5])
        self.assertIn("'S-1-5-32-544'", acl_command[5])
        acl_environment = run.call_args.kwargs["env"]
        self.assertEqual(
            acl_environment["MINI_AGENT_RELEASE_SMOKE_DIRECTORY"],
            r"C:\Users\runner\AppData\Local\smoke",
        )
        self.assertNotIn("MINI_AGENT_RELEASE_SMOKE_DIRECTORY", environment)

    def test_windows_smoke_install_rejects_missing_system_root(self) -> None:
        with mock.patch.object(RELEASE.subprocess, "run") as run:
            with self.assertRaisesRegex(
                RELEASE.ReleaseArtifactError,
                "cannot create a private Windows smoke-install directory",
            ):
                RELEASE._harden_windows_install_directory(
                    Path(r"C:\private"),
                    platform_name="nt",
                    environment={},
                )
        run.assert_not_called()

    def test_non_windows_smoke_install_does_not_change_acl(self) -> None:
        with mock.patch.object(RELEASE.subprocess, "run") as run:
            RELEASE._harden_windows_install_directory(
                Path("/tmp/private"), platform_name="posix"
            )

        run.assert_not_called()

    def test_windows_smoke_install_rejects_acl_command_failure(self) -> None:
        response = subprocess.CompletedProcess([], 5, "", "access denied")
        with mock.patch.object(RELEASE.subprocess, "run", return_value=response):
            with self.assertRaisesRegex(
                RELEASE.ReleaseArtifactError,
                "cannot create a private Windows smoke-install directory",
            ):
                RELEASE._harden_windows_install_directory(
                    Path(r"C:\private"),
                    platform_name="nt",
                    environment={"SystemRoot": r"C:\Windows"},
                )

    def test_windows_smoke_process_uses_private_directory_as_local_app_data(self) -> None:
        environment = {"PATH": r"C:\Windows\System32", "LOCALAPPDATA": r"C:\shared"}

        result = RELEASE._smoke_environment(
            Path(r"C:\private"), environment=environment, platform_name="nt"
        )

        self.assertEqual(
            result,
            {"PATH": r"C:\Windows\System32", "LOCALAPPDATA": r"C:\private"},
        )
        self.assertEqual(environment["LOCALAPPDATA"], r"C:\shared")

    def test_non_windows_smoke_process_inherits_environment(self) -> None:
        self.assertIsNone(
            RELEASE._smoke_environment(
                Path("/tmp/private"), environment={"PATH": "/bin"}, platform_name="posix"
            )
        )

    @unittest.skipIf(RELEASE.os.name == "nt", "fixture is a POSIX shell executable")
    def test_lite_archive_must_reject_js_runtime_check(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            executable = (
                b"#!/bin/sh\n"
                b"if [ \"$1\" = --version ]; then echo 'mini-agent 1.8.0'; exit 0; fi\n"
                b"echo 'error: unexpected argument' >&2\n"
                b"exit 2\n"
            )
            archive = self.archive(
                directory,
                [
                    ("mini-agent", executable, 0o755),
                    ("LICENSE", b"license", 0o644),
                    ("NOTICE", b"notice", 0o644),
                    ("SOURCE.md", b"source", 0o644),
                ],
            )
            RELEASE.smoke_archive(archive, "mini-agent", "1.8.0", False)


if __name__ == "__main__":
    unittest.main()
