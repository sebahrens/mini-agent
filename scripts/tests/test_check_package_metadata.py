import importlib.util
import hashlib
import os
import shutil
import subprocess
import tempfile
import tomllib
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "check-package-metadata.py"
SPEC = importlib.util.spec_from_file_location("check_package_metadata", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
CHECK_PACKAGE_METADATA = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK_PACKAGE_METADATA)


class ReleaseWorkflowValidationTests(unittest.TestCase):
    def test_wrong_binary_source_path_is_rejected(self) -> None:
        workflow = """
env:
  CANONICAL_BINARY: mini-agent
steps:
  - run: cp target/${{ matrix.target }}/release/zerostack mini-agent
"""

        errors = CHECK_PACKAGE_METADATA.validate_workflow(workflow, "mini-agent")

        self.assertTrue(
            any(
                "target/${{ matrix.target }}/release/zerostack" in error
                for error in errors
            )
        )

    def test_checked_in_workflow_matches_canonical_binary(self) -> None:
        workflow = (
            SCRIPT.parents[1] / ".github/workflows/release.yml"
        ).read_text(encoding="utf-8")

        self.assertEqual(
            [], CHECK_PACKAGE_METADATA.validate_workflow(workflow, "mini-agent")
        )


class RepositoryCoordinateValidationTests(unittest.TestCase):
    def test_stale_active_coordinate_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "install.sh"
            path.write_text(
                "https://github.com/" + "gi-" + "dellav/zerostack/releases/latest",
                encoding="utf-8",
            )

            errors = CHECK_PACKAGE_METADATA.validate_stale_coordinates(
                root, ["install.sh"]
            )

            self.assertEqual(1, len(errors))
            self.assertIn("stale active coordinate", errors[0])


class ReleaseChecksumUpdateTests(unittest.TestCase):
    def test_http_failure_does_not_mutate_package_metadata(self) -> None:
        repository = SCRIPT.parents[1]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "scripts").mkdir()
            (root / "packaging/aur").mkdir(parents=True)
            shutil.copy(repository / "Cargo.toml", root / "Cargo.toml")
            shutil.copy(
                repository / "scripts/update-release-checksums.sh",
                root / "scripts/update-release-checksums.sh",
            )
            package = root / "packaging/aur/PKGBUILD"
            package.write_text("unchanged\n", encoding="utf-8")

            stub_bin = root / "stub-bin"
            stub_bin.mkdir()
            curl = stub_bin / "curl"
            curl.write_text(
                "#!/bin/sh\nprintf 'not found\\n' >&2\nexit 22\n",
                encoding="utf-8",
            )
            curl.chmod(0o755)
            env = os.environ.copy()
            env["PATH"] = f"{stub_bin}:{env['PATH']}"

            result = subprocess.run(
                ["bash", str(root / "scripts/update-release-checksums.sh"), "aur"],
                cwd=root,
                env=env,
                capture_output=True,
                text=True,
            )

            self.assertNotEqual(0, result.returncode)
            self.assertEqual("unchanged\n", package.read_text(encoding="utf-8"))

    def test_all_downloads_succeed_before_portable_recipe_updates(self) -> None:
        repository = SCRIPT.parents[1]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "scripts").mkdir()
            shutil.copy(repository / "Cargo.toml", root / "Cargo.toml")
            shutil.copy(
                repository / "scripts/update-release-checksums.sh",
                root / "scripts/update-release-checksums.sh",
            )
            for relative in (
                "packaging/aur/PKGBUILD",
                "packaging/conda/zerostack/meta.yaml",
                "packaging/conda/zerostack-bin/meta.yaml",
                "packaging/homebrew/zerostack.rb",
            ):
                destination = root / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy(repository / relative, destination)

            stub_bin = root / "stub-bin"
            stub_bin.mkdir()
            curl = stub_bin / "curl"
            curl.write_text(
                """#!/bin/bash
set -euo pipefail
out=""
for ((i = 1; i <= $#; i++)); do
    if [[ "${!i}" == "-o" ]]; then
        next=$((i + 1))
        out="${!next}"
    fi
done
printf '%s' "${!#}" > "$out"
""",
                encoding="utf-8",
            )
            curl.chmod(0o755)
            env = os.environ.copy()
            env["PATH"] = f"{stub_bin}:{env['PATH']}"

            result = subprocess.run(
                ["bash", str(root / "scripts/update-release-checksums.sh"), "all"],
                cwd=root,
                env=env,
                capture_output=True,
                text=True,
            )

            self.assertEqual(0, result.returncode, result.stderr)
            version = tomllib.loads((root / "Cargo.toml").read_text())["package"][
                "version"
            ]
            release = (
                "https://github.com/sebahrens/mini-agent/releases/download/"
                f"v{version}"
            )
            expected_by_recipe = {
                "packaging/aur/PKGBUILD": (
                    f"{release}/mini-agent-x86_64-unknown-linux-musl.tar.gz",
                    f"{release}/mini-agent-aarch64-unknown-linux-musl.tar.gz",
                    f"https://raw.githubusercontent.com/sebahrens/mini-agent/v{version}/LICENSE",
                ),
                "packaging/conda/zerostack/meta.yaml": (
                    f"https://github.com/sebahrens/mini-agent/archive/refs/tags/v{version}.tar.gz",
                ),
                "packaging/conda/zerostack-bin/meta.yaml": (
                    f"{release}/mini-agent-x86_64-unknown-linux-musl.tar.gz",
                    f"{release}/mini-agent-aarch64-unknown-linux-musl.tar.gz",
                    f"https://raw.githubusercontent.com/sebahrens/mini-agent/v{version}/LICENSE",
                ),
                "packaging/homebrew/zerostack.rb": (
                    f"{release}/mini-agent-x86_64-apple-darwin.tar.gz",
                    f"{release}/mini-agent-aarch64-apple-darwin.tar.gz",
                    f"{release}/mini-agent-x86_64-unknown-linux-musl.tar.gz",
                    f"{release}/mini-agent-aarch64-unknown-linux-musl.tar.gz",
                ),
            }
            for relative, urls in expected_by_recipe.items():
                text = (root / relative).read_text(encoding="utf-8")
                for url in urls:
                    self.assertIn(hashlib.sha256(url.encode()).hexdigest(), text)
            self.assertFalse(any(root.rglob("*.bak")))

    def test_historical_specs_are_narrowly_allowed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            relative = "docs/specs/superseded/historical.md"
            path = root / relative
            path.parent.mkdir(parents=True)
            path.write_text(
                "https://github.com/" + "gi-" + "dellav/zerostack",
                encoding="utf-8",
            )

            self.assertEqual(
                [],
                CHECK_PACKAGE_METADATA.validate_stale_coordinates(root, [relative]),
            )

    def test_mixed_case_stale_coordinate_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "active.md"
            path.write_text(
                "https://github.com/" + "GI-" + "DELLAV/ZEROSTACK/releases/latest",
                encoding="utf-8",
            )

            errors = CHECK_PACKAGE_METADATA.validate_stale_coordinates(
                root, ["active.md"]
            )

            self.assertEqual(1, len(errors))
            self.assertIn("stale active coordinate", errors[0])


if __name__ == "__main__":
    unittest.main()
