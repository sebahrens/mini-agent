import importlib.util
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "smoke-package-compliance.py"
SPEC = importlib.util.spec_from_file_location("smoke_package_compliance", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
SMOKE_PACKAGE_COMPLIANCE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SMOKE_PACKAGE_COMPLIANCE)


class PackageComplianceSmokeTests(unittest.TestCase):
    def test_modified_license_fails_before_any_recipe_runs(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repository = root / "repository"
            repository.mkdir()
            shutil.copyfile(
                SMOKE_PACKAGE_COMPLIANCE.ROOT / "NOTICE", repository / "NOTICE"
            )
            shutil.copyfile(
                SMOKE_PACKAGE_COMPLIANCE.ROOT / "SOURCE.md", repository / "SOURCE.md"
            )
            (repository / "LICENSE").write_text("not the GPL\n", encoding="utf-8")

            original_root = SMOKE_PACKAGE_COMPLIANCE.ROOT
            SMOKE_PACKAGE_COMPLIANCE.ROOT = repository
            try:
                work = root / "work"
                work.mkdir()
                with self.assertRaisesRegex(RuntimeError, "canonical GPL-3.0-only"):
                    SMOKE_PACKAGE_COMPLIANCE.make_payload(work)
            finally:
                SMOKE_PACKAGE_COMPLIANCE.ROOT = original_root

    def test_every_maintained_recipe_stages_the_compliance_payload(self) -> None:
        result = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--channel",
                "aur",
                "--channel",
                "conda-bin",
                "--channel",
                "conda-source",
                "--channel",
                "homebrew",
            ],
            check=False,
            capture_output=True,
            text=True,
        )

        self.assertEqual(0, result.returncode, result.stderr)
        for channel in ("aur", "conda-bin", "conda-source", "homebrew"):
            self.assertIn(f"package compliance smoke passed: {channel}", result.stdout)


if __name__ == "__main__":
    unittest.main()
