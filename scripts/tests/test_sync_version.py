import json
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


REPOSITORY = Path(__file__).resolve().parents[2]
SCRIPT = REPOSITORY / "scripts/sync-version.sh"


class SyncVersionTests(unittest.TestCase):
    def test_future_version_updates_source_asset_and_resets_package_revisions(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "scripts").mkdir()
            shutil.copy(SCRIPT, root / "scripts/sync-version.sh")
            shutil.copy(REPOSITORY / "Cargo.toml", root / "Cargo.toml")
            for relative in (
                "packaging/aur/PKGBUILD",
                "packaging/aur/.SRCINFO",
                "packaging/conda/zerostack/meta.yaml",
                "packaging/conda/zerostack-bin/meta.yaml",
                "packaging/homebrew/zerostack.rb",
                "editors/vscode/package.json",
                "editors/vscode/package-lock.json",
                "editors/vscode/SOURCE.md",
                "packaging/windows/README.md",
                "docs/acp-registry.json",
            ):
                destination = root / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy(REPOSITORY / relative, destination)

            cargo = root / "Cargo.toml"
            cargo.write_text(
                re.sub(
                    r'^version = "[^"]+"$',
                    'version = "9.8.7"',
                    cargo.read_text(encoding="utf-8"),
                    count=1,
                    flags=re.MULTILINE,
                ),
                encoding="utf-8",
            )

            result = subprocess.run(
                ["bash", str(root / "scripts/sync-version.sh")],
                cwd=root,
                capture_output=True,
                text=True,
            )

            self.assertEqual(0, result.returncode, result.stderr)
            pkgbuild = (root / "packaging/aur/PKGBUILD").read_text()
            srcinfo = (root / "packaging/aur/.SRCINFO").read_text()
            conda_source = (root / "packaging/conda/zerostack/meta.yaml").read_text()
            conda_bin = (root / "packaging/conda/zerostack-bin/meta.yaml").read_text()
            homebrew = (root / "packaging/homebrew/zerostack.rb").read_text()

            self.assertIn("pkgver=9.8.7", pkgbuild)
            self.assertIn("pkgrel=1", pkgbuild)
            self.assertIn("pkgver = 9.8.7", srcinfo)
            self.assertIn("pkgrel = 1", srcinfo)
            self.assertIn(
                "/download/v9.8.7/mini-agent-v9.8.7-source.tar.gz",
                conda_source,
            )
            self.assertIn("version: 9.8.7", conda_source)
            self.assertIn("number: 0", conda_source)
            self.assertIn("/download/v9.8.7/", conda_bin)
            self.assertIn("number: 0", conda_bin)
            self.assertIn('version "9.8.7"', homebrew)
            self.assertIn("/download/v9.8.7/", homebrew)
            self.assertNotIn("revision 1", homebrew)

            # A version change invalidates every previously recorded release
            # digest; only the version-independent GPL LICENSE digest survives.
            license_digest = (
                "3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986"
            )
            for text in (pkgbuild, srcinfo, conda_source, conda_bin, homebrew):
                digests = re.findall(r"\b[0-9a-f]{64}\b", text)
                self.assertTrue(digests)
                self.assertEqual(
                    set(),
                    {d for d in digests if d not in ("0" * 64, license_digest)},
                )
            self.assertIn(license_digest, pkgbuild)
            self.assertIn(license_digest, srcinfo)
            self.assertIn(license_digest, conda_bin)
            self.assertIn("0" * 64, conda_source)

            package = json.loads(
                (root / "editors/vscode/package.json").read_text(encoding="utf-8")
            )
            lock = json.loads(
                (root / "editors/vscode/package-lock.json").read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual("9.8.7", package["version"])
            self.assertEqual("9.8.7", lock["version"])
            self.assertEqual("9.8.7", lock["packages"][""]["version"])
            self.assertEqual("1.3.0", package["dependencies"]["@agentclientprotocol/sdk"])

            source = (root / "editors/vscode/SOURCE.md").read_text(encoding="utf-8")
            self.assertIn("for version 9.8.7 is the `v9.8.7` tree at", source)
            self.assertIn("/tree/v9.8.7>", source)
            self.assertIn("mini-agent-v9.8.7-source.tar.gz", source)

            windows = (root / "packaging/windows/README.md").read_text(
                encoding="utf-8"
            )
            self.assertIn("-p:ProductVersion=9.8.7 `", windows)
            self.assertIn("mini-agent-9.8.7-win32-x64.vsix", windows)

            registry = json.loads(
                (root / "docs/acp-registry.json").read_text(encoding="utf-8")
            )
            self.assertEqual("9.8.7", registry["agent"]["version"])
            self.assertEqual("1.3.0", registry["agent"]["protocol"]["version"])

    def test_unchanged_version_keeps_recorded_release_digests(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "scripts").mkdir()
            shutil.copy(SCRIPT, root / "scripts/sync-version.sh")
            shutil.copy(REPOSITORY / "Cargo.toml", root / "Cargo.toml")
            recipe = REPOSITORY / "packaging/aur/PKGBUILD"
            destination = root / "packaging/aur/PKGBUILD"
            destination.parent.mkdir(parents=True)
            text = recipe.read_text(encoding="utf-8").replace("0" * 64, "e" * 64)
            destination.write_text(text, encoding="utf-8")
            srcinfo = root / "packaging/aur/.SRCINFO"
            srcinfo.write_text(
                (REPOSITORY / "packaging/aur/.SRCINFO")
                .read_text(encoding="utf-8")
                .replace("0" * 64, "e" * 64),
                encoding="utf-8",
            )
            for relative in (
                "packaging/conda/zerostack/meta.yaml",
                "packaging/conda/zerostack-bin/meta.yaml",
            ):
                target = root / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy(REPOSITORY / relative, target)

            result = subprocess.run(
                ["bash", str(root / "scripts/sync-version.sh")],
                cwd=root,
                capture_output=True,
                text=True,
            )

            self.assertEqual(0, result.returncode, result.stderr)
            self.assertIn("e" * 64, destination.read_text(encoding="utf-8"))
            self.assertIn("e" * 64, srcinfo.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
