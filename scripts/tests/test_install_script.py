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

    def run_installer(self, root: Path, release: Path, stub_bin: Path) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env["PATH"] = f"{stub_bin}:{env['PATH']}"
        env["INSTALL_TEST_RELEASE"] = str(release)
        return subprocess.run(
            [
                "bash",
                str(INSTALLER),
                "--release",
                "1.7.2",
                "--dir",
                str(root / "prefix" / "bin"),
            ],
            env=env,
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


if __name__ == "__main__":
    unittest.main()
