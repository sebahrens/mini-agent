import importlib.util
import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "check-package-metadata.py"
SPEC = importlib.util.spec_from_file_location("check_package_metadata", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
CHECK_PACKAGE_METADATA = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK_PACKAGE_METADATA)


class ReleaseWorkflowValidationTests(unittest.TestCase):
    def test_wrong_binary_source_path_is_rejected(self) -> None:
        workflow = """
env:
  CANONICAL_BINARY: mini-agent
steps:
  - run: cp target/${{ matrix.target }}/release/zerostack mini-agent
"""

        errors = CHECK_PACKAGE_METADATA.validate_workflow(workflow, "mini-agent")

        self.assertTrue(
            any(
                "target/${{ matrix.target }}/release/zerostack" in error
                for error in errors
            )
        )

    def test_checked_in_workflow_matches_canonical_binary(self) -> None:
        workflow = (
            SCRIPT.parents[1] / ".github/workflows/release.yml"
        ).read_text(encoding="utf-8")

        self.assertEqual(
            [], CHECK_PACKAGE_METADATA.validate_workflow(workflow, "mini-agent")
        )

    def test_manual_release_dispatch_is_rejected(self) -> None:
        workflow = (
            SCRIPT.parents[1] / ".github/workflows/release.yml"
        ).read_text(encoding="utf-8")
        workflow = workflow.replace(
            'on:\n  push:\n    tags:\n      - "v*"',
            'on:\n  workflow_dispatch: {}',
        )

        errors = CHECK_PACKAGE_METADATA.validate_workflow(workflow, "mini-agent")

        self.assertTrue(any("tag pushes" in error for error in errors))
        self.assertTrue(any("manual dispatch" in error for error in errors))

    def test_missing_release_identity_gate_is_rejected(self) -> None:
        workflow = (
            SCRIPT.parents[1] / ".github/workflows/release.yml"
        ).read_text(encoding="utf-8")
        workflow = workflow.replace(
            '--release-tag "$RELEASE_TAG"',
            '--release-tag "v0.0.0"',
        )

        errors = CHECK_PACKAGE_METADATA.validate_workflow(workflow, "mini-agent")

        self.assertTrue(any("release identity" in error for error in errors))

    def test_tag_recipes_require_committed_metadata_before_tagging(self) -> None:
        justfile = (SCRIPT.parents[1] / "justfile").read_text(encoding="utf-8")
        add_tag = justfile[justfile.index("add-tag:") : justfile.index("remove-tag")]
        release = justfile[
            justfile.index("release BUMP:") : justfile.index("pre-release:")
        ]

        self.assertLess(add_tag.index("--require-clean"), add_tag.index("git tag -a"))
        committed_guard = release.rindex("--require-clean")
        self.assertLess(release.index('git commit -am "bump'), committed_guard)
        self.assertLess(committed_guard, release.index("git tag -a"))


class ReleaseIdentityValidationTests(unittest.TestCase):
    def test_matching_stable_tag_is_accepted(self) -> None:
        self.assertEqual(
            [],
            CHECK_PACKAGE_METADATA.validate_release_identity(
                version="1.7.2", ref_type="tag", release_tag="v1.7.2"
            ),
        )

    def test_matching_prerelease_tag_is_accepted(self) -> None:
        self.assertEqual(
            [],
            CHECK_PACKAGE_METADATA.validate_release_identity(
                version="2.0.0-rc.1", ref_type="tag", release_tag="v2.0.0-rc.1"
            ),
        )

    def test_version_mismatch_is_rejected(self) -> None:
        errors = CHECK_PACKAGE_METADATA.validate_release_identity(
            version="1.7.2", ref_type="tag", release_tag="v1.7.3"
        )

        self.assertTrue(
            any("does not match Cargo package version" in error for error in errors)
        )

    def test_branch_ref_is_rejected(self) -> None:
        errors = CHECK_PACKAGE_METADATA.validate_release_identity(
            version="1.7.2", ref_type="branch", release_tag="main"
        )

        self.assertTrue(any("tag ref" in error for error in errors))

    def test_malformed_tag_is_rejected(self) -> None:
        for release_tag in ("1.7.2", "v1.7", "v01.7.2", "v1.7.2-"):
            with self.subTest(release_tag=release_tag):
                errors = CHECK_PACKAGE_METADATA.validate_release_identity(
                    version="1.7.2", ref_type="tag", release_tag=release_tag
                )
                self.assertTrue(any("valid release tag" in error for error in errors))


class CleanTrackedWorktreeValidationTests(unittest.TestCase):
    def test_modified_or_staged_release_metadata_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "--quiet"], cwd=root, check=True)
            subprocess.run(
                ["git", "config", "user.email", "release-test@example.invalid"],
                cwd=root,
                check=True,
            )
            subprocess.run(
                ["git", "config", "user.name", "Release Test"],
                cwd=root,
                check=True,
            )
            metadata = root / "Cargo.toml"
            metadata.write_text('version = "1.7.2"\n', encoding="utf-8")
            subprocess.run(["git", "add", "Cargo.toml"], cwd=root, check=True)
            subprocess.run(
                ["git", "commit", "--quiet", "-m", "baseline"],
                cwd=root,
                check=True,
            )

            self.assertEqual(
                [], CHECK_PACKAGE_METADATA.validate_clean_tracked_worktree(root)
            )

            metadata.write_text('version = "1.7.3"\n', encoding="utf-8")
            unstaged_errors = (
                CHECK_PACKAGE_METADATA.validate_clean_tracked_worktree(root)
            )
            self.assertTrue(
                any("working tree is dirty" in error for error in unstaged_errors)
            )

            subprocess.run(["git", "add", "Cargo.toml"], cwd=root, check=True)
            staged_errors = (
                CHECK_PACKAGE_METADATA.validate_clean_tracked_worktree(root)
            )
            self.assertTrue(any("index is dirty" in error for error in staged_errors))


if __name__ == "__main__":
    unittest.main()
