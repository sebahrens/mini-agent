from __future__ import annotations

import hashlib
import os
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).parents[2]
INSTALLER = ROOT / "install.sh"
PACKAGER = ROOT / "scripts/package-release-binary.py"


class InstallScriptTests(unittest.TestCase):
    def test_unknown_option_is_a_usage_error(self) -> None:
        result = subprocess.run(
            ["bash", str(INSTALLER), "--not-an-option"],
            capture_output=True,
            text=True,
        )

        self.assertEqual(2, result.returncode)
        self.assertIn("Unknown option: --not-an-option", result.stderr)

    def test_help_is_successful(self) -> None:
        result = subprocess.run(
            ["bash", str(INSTALLER), "--help"],
            capture_output=True,
            text=True,
        )

        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("Usage: install.sh", result.stdout)

    def make_fixture(self, directory: str, *, include_notice: bool = True) -> tuple[Path, Path]:
        root = Path(directory)
        release = root / "release"
        release.mkdir()
        binary = root / "mini-agent"
        binary.write_text("#!/bin/sh\necho mini-agent 1.7.2\n", encoding="utf-8")
        binary.chmod(0o755)
        archive = release / "mini-agent-aarch64-apple-darwin.tar.gz"

        if include_notice:
            subprocess.run(
                [
                    "python3",
                    str(PACKAGER),
                    "--root",
                    str(ROOT),
                    "--binary",
                    str(binary),
                    "--archive",
                    str(archive),
                    "--executable-name",
                    "mini-agent",
                ],
                check=True,
            )
        else:
            with tarfile.open(archive, "w:gz") as packaged:
                packaged.add(binary, arcname="mini-agent")
                packaged.add(ROOT / "LICENSE", arcname="LICENSE")
                packaged.add(ROOT / "SOURCE.md", arcname="SOURCE.md")

        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        (release / "SHA256SUMS").write_text(
            f"{digest}  {archive.name}\n", encoding="utf-8"
        )

        stub_bin = root / "stub-bin"
        stub_bin.mkdir()
        curl = stub_bin / "curl"
        curl.write_text(
            """#!/bin/bash
set -euo pipefail
output=""
for ((index = 1; index <= $#; index++)); do
    if [[ "${!index}" == "-o" ]]; then
        next=$((index + 1))
        output="${!next}"
    fi
done
url="${!#}"
filename="${url##*/}"
cp "${INSTALL_TEST_RELEASE}/${filename}" "$output"
""",
            encoding="utf-8",
        )
        curl.chmod(0o755)
        uname = stub_bin / "uname"
        uname.write_text(
            """#!/bin/sh
if [ "$1" = "-s" ]; then
    echo Darwin
else
    echo arm64
fi
""",
            encoding="utf-8",
        )
        uname.chmod(0o755)
        return release, stub_bin

    def run_installer(
        self,
        root: Path,
        release: Path,
        stub_bin: Path,
        *,
        install_dir: str | None = None,
        path_prefix: list[Path] | None = None,
        home: Path | None = None,
    ) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        # Keep any real mini-agent on the developer's PATH from influencing
        # the PATH-resolution checks.
        system_path = os.pathsep.join(
            entry
            for entry in env["PATH"].split(os.pathsep)
            if entry and not (Path(entry) / "mini-agent").exists()
        )
        prefix = "".join(f"{entry}:" for entry in path_prefix or [])
        env["PATH"] = f"{prefix}{stub_bin}:{system_path}"
        env["INSTALL_TEST_RELEASE"] = str(release)
        if home is not None:
            env["HOME"] = str(home)
        return subprocess.run(
            [
                "bash",
                str(INSTALLER),
                "--release",
                "1.7.2",
                "--dir",
                install_dir if install_dir is not None else str(root / "prefix" / "bin"),
            ],
            env=env,
            cwd=root,
            capture_output=True,
            text=True,
        )

    def assert_rejected_before_install(
        self, root: Path, release: Path, stub_bin: Path, message: str
    ) -> None:
        result = self.run_installer(root, release, stub_bin)
        self.assertNotEqual(0, result.returncode)
        self.assertIn(message, result.stderr)
        self.assertFalse((root / "prefix/bin/mini-agent").exists())

    def test_installs_binary_license_notice_and_source_directions(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)

            result = self.run_installer(root, release, stub_bin)

            self.assertEqual(0, result.returncode, result.stderr)
            self.assertTrue((root / "prefix/bin/mini-agent").is_file())
            for document in ("LICENSE", "NOTICE", "SOURCE.md"):
                self.assertEqual(
                    (ROOT / document).read_bytes(),
                    (root / "prefix/share/doc/mini-agent" / document).read_bytes(),
                )

    def test_missing_notice_fails_before_installing_binary(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory, include_notice=False)

            result = self.run_installer(root, release, stub_bin)

            self.assertNotEqual(0, result.returncode)
            self.assertIn("required GPL document NOTICE", result.stderr)
            self.assertFalse((root / "prefix/bin/mini-agent").exists())

    def test_missing_checksum_entry_fails_before_extraction(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            (release / "SHA256SUMS").write_text(
                "0" * 64 + "  mini-agent-x86_64-unknown-linux-gnu.tar.gz\n",
                encoding="ascii",
            )
            self.assert_rejected_before_install(root, release, stub_bin, "has no entry")

    def test_checksum_download_failure_fails_before_extraction(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            (release / "SHA256SUMS").unlink()

            result = self.run_installer(root, release, stub_bin)

            self.assertNotEqual(0, result.returncode)
            self.assertFalse((root / "prefix/bin/mini-agent").exists())
            self.assertFalse((root / "prefix").exists())

    def test_duplicate_checksum_entry_fails_before_extraction(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            manifest = release / "SHA256SUMS"
            manifest.write_bytes(manifest.read_bytes() * 2)
            self.assert_rejected_before_install(root, release, stub_bin, "duplicate entries")

    def test_malformed_checksum_entry_fails_before_extraction(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            archive = "mini-agent-aarch64-apple-darwin.tar.gz"
            (release / "SHA256SUMS").write_text(
                f"not-a-sha256  {archive}\n", encoding="ascii"
            )
            self.assert_rejected_before_install(root, release, stub_bin, "malformed hash")

    def test_noncanonical_selected_line_fails_before_extraction(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            manifest = release / "SHA256SUMS"
            manifest.write_text(manifest.read_text(encoding="ascii").rstrip() + "  extra\n")
            self.assert_rejected_before_install(root, release, stub_bin, "malformed entry")

    def test_checksum_mismatch_fails_before_extraction(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            archive = release / "mini-agent-aarch64-apple-darwin.tar.gz"
            archive.write_bytes(archive.read_bytes() + b"tampered")
            self.assert_rejected_before_install(root, release, stub_bin, "checksum mismatch")

    def test_filename_matching_treats_dots_literally(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            canonical = "mini-agent-aarch64-apple-darwin.tar.gz"
            misleading = canonical.replace(".", "x")
            (release / "SHA256SUMS").write_text(
                f"{'0' * 64}  {misleading}\n", encoding="ascii"
            )
            self.assert_rejected_before_install(root, release, stub_bin, "has no entry")

    def test_upgrade_replaces_existing_binary_by_rename(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            bin_dir = root / "prefix/bin"
            bin_dir.mkdir(parents=True)
            target = bin_dir / "mini-agent"
            target.write_text("#!/bin/sh\necho old\n", encoding="utf-8")
            target.chmod(0o755)
            # A second link to the old inode shows whether the installer
            # rewrote the running binary in place or replaced the entry.
            running = root / "running-old-binary"
            os.link(target, running)

            result = self.run_installer(root, release, stub_bin)

            self.assertEqual(0, result.returncode, result.stderr)
            self.assertEqual("#!/bin/sh\necho old\n", running.read_text(encoding="utf-8"))
            self.assertIn("mini-agent 1.7.2", target.read_text(encoding="utf-8"))
            self.assertNotEqual(running.stat().st_ino, target.stat().st_ino)
            self.assertTrue(os.access(target, os.X_OK))
            self.assertEqual(["mini-agent"], sorted(p.name for p in bin_dir.iterdir()))

    def test_html_archive_reports_download_problem_not_checksum_entry(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            sign_in = "<!DOCTYPE html>\n<html><body>Sign in to GitHub</body></html>\n"
            (release / "mini-agent-aarch64-apple-darwin.tar.gz").write_text(sign_in)
            (release / "SHA256SUMS").write_text(sign_in)

            result = self.run_installer(root, release, stub_bin)

            self.assertNotEqual(0, result.returncode)
            self.assertIn("not a release asset (received an HTML page)", result.stderr)
            self.assertIn("gh release download", result.stderr)
            self.assertNotIn("has no entry", result.stderr)
            self.assertFalse((root / "prefix").exists())

    def test_html_checksum_manifest_reports_download_problem(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            (release / "SHA256SUMS").write_text("<html><title>Sign in</title></html>\n")

            result = self.run_installer(root, release, stub_bin)

            self.assertNotEqual(0, result.returncode)
            self.assertIn("checksum manifest SHA256SUMS is not a release asset", result.stderr)
            self.assertFalse((root / "prefix").exists())

    def test_non_gzip_archive_is_rejected_before_checksum(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            (release / "mini-agent-aarch64-apple-darwin.tar.gz").write_bytes(b"not found\n")

            result = self.run_installer(root, release, stub_bin)

            self.assertNotEqual(0, result.returncode)
            self.assertIn("data that is not gzip", result.stderr)

    def test_tilde_install_directory_expands_to_home(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            home = root / "home"
            home.mkdir()

            result = self.run_installer(
                root, release, stub_bin, install_dir="~/bin", home=home
            )

            self.assertEqual(0, result.returncode, result.stderr)
            self.assertTrue((home / "bin/mini-agent").is_file())
            self.assertFalse((root / "~").exists())

    def test_path_hint_matches_whole_entries_only(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            lookalike = root / "prefix/bin2"
            lookalike.mkdir(parents=True)

            result = self.run_installer(root, release, stub_bin, path_prefix=[lookalike])

            self.assertEqual(0, result.returncode, result.stderr)
            self.assertIn("is not in your PATH", result.stdout)

    def test_warns_when_path_resolves_to_a_shadowing_binary(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            shadow = root / "shadow"
            shadow.mkdir()
            (shadow / "mini-agent").write_text("#!/bin/sh\necho shadow\n")
            (shadow / "mini-agent").chmod(0o755)
            bin_dir = root / "prefix/bin"

            result = self.run_installer(
                root, release, stub_bin, path_prefix=[shadow, Path(f"{bin_dir}/")]
            )

            self.assertEqual(0, result.returncode, result.stderr)
            self.assertNotIn("is not in your PATH", result.stdout)
            self.assertIn("resolves to a different binary", result.stdout)
            self.assertIn(f"Resolves to: {shadow}/mini-agent", result.stdout)
            self.assertIn("hash -r", result.stdout)

    def test_no_path_warning_when_install_directory_wins(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)

            result = self.run_installer(
                root, release, stub_bin, path_prefix=[root / "prefix/bin"]
            )

            self.assertEqual(0, result.returncode, result.stderr)
            self.assertNotIn("is not in your PATH", result.stdout)
            self.assertNotIn("different binary", result.stdout)


if __name__ == "__main__":
    unittest.main()
