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
            ):
                destination = root / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy(REPOSITORY / relative, destination)

            cargo = root / "Cargo.toml"
            cargo.write_text(
                cargo.read_text(encoding="utf-8").replace(
                    'version = "1.7.2"', 'version = "9.8.7"', 1
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


if __name__ == "__main__":
    unittest.main()
