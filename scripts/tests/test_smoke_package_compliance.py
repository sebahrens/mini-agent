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

    def test_recipe_that_drops_the_third_party_inventory_fails(self) -> None:
        original_root = SMOKE_PACKAGE_COMPLIANCE.ROOT
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repository = root / "repository"
            for relative in (
                "LICENSE",
                "NOTICE",
                "SOURCE.md",
                "packaging/aur/PKGBUILD",
                "packaging/conda/zerostack-bin/build.sh",
                "packaging/conda/zerostack-bin/meta.yaml",
            ):
                (repository / relative).parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(original_root / relative, repository / relative)
            pkgbuild = repository / "packaging/aur/PKGBUILD"
            pkgbuild.write_text(
                "\n".join(
                    line
                    for line in pkgbuild.read_text(encoding="utf-8").splitlines()
                    if "THIRD_PARTY_LICENSES" not in line
                )
                + "\n",
                encoding="utf-8",
            )
            meta = repository / "packaging/conda/zerostack-bin/meta.yaml"
            meta.write_text(
                meta.read_text(encoding="utf-8").replace("    - THIRD_PARTY_LICENSES\n", ""),
                encoding="utf-8",
            )

            SMOKE_PACKAGE_COMPLIANCE.ROOT = repository
            try:
                work = root / "work"
                work.mkdir()
                payload, binary = SMOKE_PACKAGE_COMPLIANCE.make_payload(work)
                self.assertTrue((payload / "THIRD_PARTY_LICENSES").is_file())
                tools = SMOKE_PACKAGE_COMPLIANCE.controlled_tools(work, binary)
                with self.assertRaisesRegex(
                    RuntimeError, "missing usr/share/licenses/zerostack-bin/THIRD_PARTY_LICENSES"
                ):
                    SMOKE_PACKAGE_COMPLIANCE.stage_aur(work, payload, binary, tools)
                with self.assertRaisesRegex(RuntimeError, "license_file THIRD_PARTY_LICENSES"):
                    SMOKE_PACKAGE_COMPLIANCE.stage_conda_binary(work, payload, binary, tools)
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
