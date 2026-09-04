import shutil
import tempfile
import unittest
from pathlib import Path

from scripts import check_windows_msi


REPOSITORY = Path(__file__).resolve().parents[2]


class WindowsMsiPolicyTests(unittest.TestCase):
    def test_checked_in_installer_satisfies_policy(self) -> None:
        self.assertEqual([], check_windows_msi.validate(REPOSITORY))

    def test_scope_and_commit_action_drift_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            shutil.copytree(REPOSITORY / "packaging/windows", root / "packaging/windows")
            workflow = root / ".github/workflows/release.yml"
            workflow.parent.mkdir(parents=True)
            shutil.copy(REPOSITORY / ".github/workflows/release.yml", workflow)
            shutil.copy(REPOSITORY / ".github/workflows/ci.yml", workflow.parent / "ci.yml")
            source = root / "packaging/windows/mini-agent.wxs"
            source.write_text(
                source.read_text(encoding="utf-8")
                .replace('Scope="perUserOrMachine"', 'Scope="perMachine"', 1)
                .replace('Execute="commit"', 'Execute="immediate"', 1),
                encoding="utf-8",
            )

            errors = check_windows_msi.validate(root)

            self.assertTrue(any("dual-purpose" in error for error in errors))
            self.assertTrue(any("Execute=commit" in error for error in errors))

    def test_release_smoke_is_mandatory(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            shutil.copytree(REPOSITORY / "packaging/windows", root / "packaging/windows")
            workflow = root / ".github/workflows/release.yml"
            workflow.parent.mkdir(parents=True)
            text = (REPOSITORY / ".github/workflows/release.yml").read_text(encoding="utf-8")
            workflow.write_text(text.replace("msiexec.exe", "disabled-installer", 1), encoding="utf-8")
            shutil.copy(REPOSITORY / ".github/workflows/ci.yml", workflow.parent / "ci.yml")

            errors = check_windows_msi.validate(root)

            self.assertTrue(any("msiexec.exe" in error for error in errors))

    def test_release_must_wait_for_msiexec(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            shutil.copytree(REPOSITORY / "packaging/windows", root / "packaging/windows")
            workflow = root / ".github/workflows/release.yml"
            workflow.parent.mkdir(parents=True)
            text = (REPOSITORY / ".github/workflows/release.yml").read_text(encoding="utf-8")
            workflow.write_text(text.replace("-Wait -PassThru", "-PassThru", 1), encoding="utf-8")
            shutil.copy(REPOSITORY / ".github/workflows/ci.yml", workflow.parent / "ci.yml")

            errors = check_windows_msi.validate(root)

            self.assertTrue(any("wait for both msiexec.exe" in error for error in errors))

    def test_release_must_run_installed_binary_startup_smoke(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            shutil.copytree(REPOSITORY / "packaging/windows", root / "packaging/windows")
            workflow = root / ".github/workflows/release.yml"
            workflow.parent.mkdir(parents=True)
            text = (REPOSITORY / ".github/workflows/release.yml").read_text(encoding="utf-8")
            workflow.write_text(
                text.replace("& $installed --print-config", "# startup smoke removed", 1),
                encoding="utf-8",
            )
            shutil.copy(REPOSITORY / ".github/workflows/ci.yml", workflow.parent / "ci.yml")

            errors = check_windows_msi.validate(root)

            self.assertTrue(any("--print-config" in error for error in errors))

    def test_per_machine_extension_install_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            shutil.copytree(REPOSITORY / "packaging/windows", root / "packaging/windows")
            workflow = root / ".github/workflows/release.yml"
            workflow.parent.mkdir(parents=True)
            shutil.copy(REPOSITORY / ".github/workflows/release.yml", workflow)
            shutil.copy(REPOSITORY / ".github/workflows/ci.yml", workflow.parent / "ci.yml")
            source = root / "packaging/windows/mini-agent.wxs"
            source.write_text(
                source.read_text(encoding="utf-8").replace(
                    "NOT Installed AND ALLUSERS &lt;&gt; 1", "NOT Installed"
                ),
                encoding="utf-8",
            )

            errors = check_windows_msi.validate(root)

            self.assertEqual(
                2,
                sum("restricted to a first per-user install" in error for error in errors),
            )


if __name__ == "__main__":
    unittest.main()
