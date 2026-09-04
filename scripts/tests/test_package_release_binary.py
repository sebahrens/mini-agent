from __future__ import annotations

import importlib.util
import shutil
import stat
import tarfile
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).parents[1] / "package-release-binary.py"
SPEC = importlib.util.spec_from_file_location("package_release_binary", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
PACKAGE_RELEASE_BINARY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PACKAGE_RELEASE_BINARY)


class PackageReleaseBinaryTests(unittest.TestCase):
    def test_required_legal_documents_are_checked_out_with_lf_on_windows(self) -> None:
        attributes = (SCRIPT.parents[1] / ".gitattributes").read_text(encoding="utf-8")
        for document in ("LICENSE", "NOTICE", "SOURCE.md"):
            self.assertIn(f"{document} text eol=lf", attributes.splitlines())

    def make_root(self, directory: str) -> Path:
        root = Path(directory)
        repository = SCRIPT.parents[1]
        shutil.copyfile(repository / "LICENSE", root / "LICENSE")
        for name in ("NOTICE", "SOURCE.md"):
            (root / name).write_text(f"{name}\n", encoding="utf-8")
        return root

    def test_archive_has_only_executable_and_required_gpl_documents(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = self.make_root(directory)
            binary = root / "build" / "mini-agent"
            binary.parent.mkdir()
            binary.write_bytes(b"binary")
            archive = root / "mini-agent-target.tar.gz"

            PACKAGE_RELEASE_BINARY.package_binary(
                root=root,
                binary=binary,
                archive=archive,
                executable_name="mini-agent",
            )

            with tarfile.open(archive, "r:gz") as packaged:
                members = packaged.getmembers()
            self.assertEqual(
                ["mini-agent", "LICENSE", "NOTICE", "SOURCE.md"],
                [member.name for member in members],
            )
            self.assertTrue(all(member.isfile() for member in members))
            self.assertEqual(0o755, stat.S_IMODE(members[0].mode))
            self.assertTrue(all(stat.S_IMODE(member.mode) == 0o644 for member in members[1:]))

    def test_windows_executable_name_is_preserved(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = self.make_root(directory)
            binary = root / "mini-agent.exe"
            binary.write_bytes(b"windows binary")
            archive = root / "windows.tar.gz"

            PACKAGE_RELEASE_BINARY.package_binary(
                root=root,
                binary=binary,
                archive=archive,
                executable_name="mini-agent.exe",
            )

            with tarfile.open(archive, "r:gz") as packaged:
                self.assertEqual("mini-agent.exe", packaged.getmembers()[0].name)

    def test_missing_notice_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = self.make_root(directory)
            (root / "NOTICE").unlink()
            binary = root / "mini-agent"
            binary.write_bytes(b"binary")

            with self.assertRaisesRegex(ValueError, "NOTICE"):
                PACKAGE_RELEASE_BINARY.package_binary(
                    root=root,
                    binary=binary,
                    archive=root / "release.tar.gz",
                    executable_name="mini-agent",
                )

    def test_modified_license_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = self.make_root(directory)
            (root / "LICENSE").write_text("not the GPL\n", encoding="utf-8")
            binary = root / "mini-agent"
            binary.write_bytes(b"binary")

            with self.assertRaisesRegex(ValueError, "canonical GPL-3.0-only"):
                PACKAGE_RELEASE_BINARY.package_binary(
                    root=root,
                    binary=binary,
                    archive=root / "release.tar.gz",
                    executable_name="mini-agent",
                )

    def test_unsafe_executable_name_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = self.make_root(directory)
            binary = root / "mini-agent"
            binary.write_bytes(b"binary")

            with self.assertRaisesRegex(ValueError, "safe archive member"):
                PACKAGE_RELEASE_BINARY.package_binary(
                    root=root,
                    binary=binary,
                    archive=root / "release.tar.gz",
                    executable_name="../mini-agent",
                )


if __name__ == "__main__":
    unittest.main()
