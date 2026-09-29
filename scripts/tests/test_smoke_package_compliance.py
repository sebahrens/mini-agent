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

    def copy_recipes(self, repository: Path) -> None:
        for relative in (
            "LICENSE",
            "NOTICE",
            "SOURCE.md",
            "packaging/aur/PKGBUILD",
            "packaging/conda/zerostack-bin/build.sh",
        ):
            (repository / relative).parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(
                SMOKE_PACKAGE_COMPLIANCE.ROOT / relative, repository / relative
            )

    def test_recipe_that_drops_the_third_party_inventory_fails(self) -> None:
        original_root = SMOKE_PACKAGE_COMPLIANCE.ROOT
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repository = root / "repository"
            self.copy_recipes(repository)
            # Keep the existence guard but lose the install it protects.
            for relative in ("packaging/aur/PKGBUILD", "packaging/conda/zerostack-bin/build.sh"):
                recipe = repository / relative
                recipe.write_text(
                    "\n".join(
                        "    :" if line.strip().startswith("install -Dm644")
                        and "THIRD_PARTY_LICENSES" in line
                        else line
                        for line in recipe.read_text(encoding="utf-8").splitlines()
                    )
                    + "\n",
                    encoding="utf-8",
                )

            SMOKE_PACKAGE_COMPLIANCE.ROOT = repository
            try:
                work = root / "work"
                payload, binary = SMOKE_PACKAGE_COMPLIANCE.make_payload(work)
                self.assertTrue((payload / "THIRD_PARTY_LICENSES").is_file())
                tools = SMOKE_PACKAGE_COMPLIANCE.controlled_tools(work, binary)
                with self.assertRaisesRegex(
                    RuntimeError, "missing usr/share/licenses/zerostack-bin/THIRD_PARTY_LICENSES"
                ):
                    SMOKE_PACKAGE_COMPLIANCE.stage_aur(work, payload, binary, tools)
                with self.assertRaisesRegex(
                    RuntimeError, "missing share/licenses/zerostack-bin/THIRD_PARTY_LICENSES"
                ):
                    SMOKE_PACKAGE_COMPLIANCE.stage_conda_binary(work, payload, binary, tools)
            finally:
                SMOKE_PACKAGE_COMPLIANCE.ROOT = original_root

    def test_recipes_still_install_archives_that_predate_the_inventory(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            work = Path(directory)
            payload, binary = SMOKE_PACKAGE_COMPLIANCE.make_payload(
                work, with_inventory=False
            )
            self.assertFalse((payload / "THIRD_PARTY_LICENSES").exists())
            tools = SMOKE_PACKAGE_COMPLIANCE.controlled_tools(work, binary)
            SMOKE_PACKAGE_COMPLIANCE.stage_aur(work, payload, binary, tools)
            SMOKE_PACKAGE_COMPLIANCE.stage_conda_binary(work, payload, binary, tools)
            SMOKE_PACKAGE_COMPLIANCE.stage_conda_source(work, payload, binary, tools)
            self.assertFalse(
                (work / "aur/usr/share/licenses/zerostack-bin/THIRD_PARTY_LICENSES").exists()
            )
            self.assertFalse(
                (work / "conda-source/share/doc/zerostack/THIRD_PARTY_LICENSES").exists()
            )

    def test_inventory_invented_for_an_old_archive_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            stage = Path(directory)
            (stage / "doc").mkdir()
            (stage / "doc/THIRD_PARTY_LICENSES").write_text("x", encoding="utf-8")
            with self.assertRaisesRegex(RuntimeError, "invented"):
                SMOKE_PACKAGE_COMPLIANCE.assert_inventory(
                    stage, "doc/THIRD_PARTY_LICENSES", stage / "absent"
                )

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
            for variant in ("with-inventory", "without-inventory"):
                self.assertIn(
                    f"package compliance smoke passed: {channel} ({variant})", result.stdout
                )


if __name__ == "__main__":
    unittest.main()
