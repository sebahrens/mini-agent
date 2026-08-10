import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "package-corresponding-source.sh"


class CorrespondingSourceIdentityTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary_directory.cleanup)
        self.repository = Path(self.temporary_directory.name)
        subprocess.run(["git", "init", "--quiet"], cwd=self.repository, check=True)
        subprocess.run(
            ["git", "config", "user.email", "source-test@example.invalid"],
            cwd=self.repository,
            check=True,
        )
        subprocess.run(
            ["git", "config", "user.name", "Source Test"],
            cwd=self.repository,
            check=True,
        )
        marker = self.repository / "marker"
        marker.write_text("tagged\n", encoding="utf-8")
        subprocess.run(["git", "add", "marker"], cwd=self.repository, check=True)
        subprocess.run(
            ["git", "commit", "--quiet", "-m", "tagged"],
            cwd=self.repository,
            check=True,
        )

    def run_packager(self, *arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["bash", str(SCRIPT), *arguments],
            cwd=self.repository,
            capture_output=True,
            text=True,
        )

    def test_missing_release_tag_fails_closed(self) -> None:
        result = self.run_packager("v1.2.3", str(self.repository), "HEAD")

        self.assertEqual(2, result.returncode)
        self.assertIn("release tag does not exist", result.stderr)

    def test_release_tag_must_match_selected_commit(self) -> None:
        subprocess.run(
            ["git", "tag", "v1.2.3"], cwd=self.repository, check=True
        )
        marker = self.repository / "marker"
        marker.write_text("later\n", encoding="utf-8")
        subprocess.run(["git", "add", "marker"], cwd=self.repository, check=True)
        subprocess.run(
            ["git", "commit", "--quiet", "-m", "later"],
            cwd=self.repository,
            check=True,
        )

        result = self.run_packager("v1.2.3", str(self.repository), "HEAD")

        self.assertEqual(2, result.returncode)
        self.assertIn("does not resolve to release tag", result.stderr)

    def test_untagged_bypass_is_restricted_to_ci_labels(self) -> None:
        result = self.run_packager(
            "v1.2.3",
            str(self.repository),
            "HEAD",
            "--allow-untagged-label",
        )

        self.assertEqual(2, result.returncode)
        self.assertIn("restricted to labels ending in -ci", result.stderr)

    def test_modified_license_fails_before_source_packaging(self) -> None:
        (self.repository / "LICENSE").write_text("not the GPL\n", encoding="utf-8")
        subprocess.run(["git", "add", "LICENSE"], cwd=self.repository, check=True)
        subprocess.run(
            ["git", "commit", "--quiet", "-m", "bad license"],
            cwd=self.repository,
            check=True,
        )
        subprocess.run(
            ["git", "tag", "v1.2.3"], cwd=self.repository, check=True
        )

        result = self.run_packager("v1.2.3", str(self.repository))

        self.assertEqual(2, result.returncode)
        self.assertIn("canonical GPL-3.0-only", result.stderr)


if __name__ == "__main__":
    unittest.main()
