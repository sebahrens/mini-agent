import importlib.util
import hashlib
import os
import shutil
import subprocess
import tempfile
import tomllib
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "check-package-metadata.py"
SPEC = importlib.util.spec_from_file_location("check_package_metadata", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
CHECK_PACKAGE_METADATA = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK_PACKAGE_METADATA)


class ReleaseWorkflowValidationTests(unittest.TestCase):
    def test_mutable_release_action_is_rejected(self) -> None:
        errors = CHECK_PACKAGE_METADATA.validate_release_action_pins(
            "steps:\n  - uses: actions/upload-artifact@v4\n"
        )

        self.assertTrue(any("full 40-character" in error for error in errors))
        self.assertTrue(any("version comment" in error for error in errors))

    def test_release_action_without_version_comment_is_rejected(self) -> None:
        errors = CHECK_PACKAGE_METADATA.validate_release_action_pins(
            "steps:\n"
            "  - uses: actions/download-artifact@"
            "d3f86a106a0bac45b974a628896c90dbdf5c8093\n"
        )

        self.assertEqual(1, len(errors))
        self.assertIn("version comment", errors[0])

    def test_malformed_release_uses_entry_fails_closed(self) -> None:
        errors = CHECK_PACKAGE_METADATA.validate_release_action_pins(
            "steps:\n  - uses: actions/download-artifact@v4 # v4 plus words\n"
        )

        self.assertTrue(any("malformed uses entry" in error for error in errors))

    def test_quoted_release_action_is_still_validated(self) -> None:
        errors = CHECK_PACKAGE_METADATA.validate_release_action_pins(
            'steps:\n  - uses: "actions/upload-artifact@v4" # v4.6.2\n'
        )

        self.assertEqual(1, len(errors))
        self.assertIn("full 40-character", errors[0])

    def test_inline_release_uses_mapping_fails_closed(self) -> None:
        errors = CHECK_PACKAGE_METADATA.validate_release_action_pins(
            "steps:\n  - { uses: actions/upload-artifact@v4 }\n"
        )

        self.assertEqual(1, len(errors))
        self.assertIn("canonical block-style", errors[0])

    def test_job_level_flow_reusable_workflow_fails_closed(self) -> None:
        errors = CHECK_PACKAGE_METADATA.validate_release_action_pins(
            "jobs: { delegated: { uses: org/repo/.github/workflows/reuse.yml@v1 } }\n"
        )

        self.assertEqual(1, len(errors))
        self.assertIn("canonical block-style", errors[0])

    def test_flow_action_cannot_borrow_comment_from_block_scalar(self) -> None:
        reference = (
            "actions/upload-artifact@"
            "ea165f8d65b6e75b540449e92b4886f43607fa02"
        )
        errors = CHECK_PACKAGE_METADATA.validate_release_action_pins(
            "steps:\n"
            "  - run: |\n"
            f"      uses: {reference} # v4.6.2\n"
            f"  - {{ uses: {reference} }}\n"
        )

        self.assertTrue(
            any("line 4" in error and "without a canonical" in error for error in errors)
        )
        self.assertTrue(
            any("line 3" in error and "not an executable" in error for error in errors)
        )

    def test_quoted_release_uses_key_is_still_validated(self) -> None:
        errors = CHECK_PACKAGE_METADATA.validate_release_action_pins(
            "steps:\n  - 'uses': actions/upload-artifact@v4 # v4.6.2\n"
        )

        self.assertEqual(1, len(errors))
        self.assertIn("full 40-character", errors[0])

    def test_explicitly_allowlisted_release_action_is_accepted(self) -> None:
        errors = CHECK_PACKAGE_METADATA.validate_release_action_pins(
            "steps:\n  - uses: example/action@floating\n",
            allowlist=frozenset({"example/action"}),
        )

        self.assertEqual([], errors)

    def test_local_release_action_requires_explicit_allowlist(self) -> None:
        workflow = "steps:\n  - uses: ./.github/actions/package\n"

        errors = CHECK_PACKAGE_METADATA.validate_release_action_pins(workflow)
        allowlisted = CHECK_PACKAGE_METADATA.validate_release_action_pins(
            workflow,
            allowlist=frozenset({"./.github/actions/package"}),
        )

        self.assertTrue(any("malformed action reference" in error for error in errors))
        self.assertEqual([], allowlisted)

    def test_fabricated_full_sha_is_rejected(self) -> None:
        errors = CHECK_PACKAGE_METADATA.validate_release_action_pins(
            "steps:\n"
            "  - uses: actions/upload-artifact@"
            f"{'0' * 40} # v4.6.2\n"
        )

        self.assertEqual(1, len(errors))
        self.assertIn("does not match the reviewed approval map", errors[0])

    def test_cross_action_sha_is_rejected(self) -> None:
        errors = CHECK_PACKAGE_METADATA.validate_release_action_pins(
            "steps:\n"
            "  - uses: actions/upload-artifact@"
            "d3f86a106a0bac45b974a628896c90dbdf5c8093 # v4.6.2\n"
        )

        self.assertEqual(1, len(errors))
        self.assertIn("does not match the reviewed approval map", errors[0])

    def test_fabricated_version_comment_is_rejected(self) -> None:
        errors = CHECK_PACKAGE_METADATA.validate_release_action_pins(
            "steps:\n"
            "  - uses: actions/upload-artifact@"
            "ea165f8d65b6e75b540449e92b4886f43607fa02 # v4.6.1\n"
        )

        self.assertEqual(1, len(errors))
        self.assertIn("unapproved action/version pair", errors[0])

    def test_dependabot_must_update_root_github_actions(self) -> None:
        errors = CHECK_PACKAGE_METADATA.validate_github_actions_updates(
            "version: 2\n"
            "updates:\n"
            "  - package-ecosystem: cargo\n"
            "    directory: /\n"
        )

        self.assertEqual(1, len(errors))
        self.assertIn("github-actions", errors[0])

    def test_dependabot_github_actions_updater_must_be_scheduled(self) -> None:
        errors = CHECK_PACKAGE_METADATA.validate_github_actions_updates(
            "version: 2\n"
            "updates:\n"
            "  - package-ecosystem: github-actions\n"
            "    directory: /\n"
        )

        self.assertEqual(1, len(errors))
        self.assertIn("schedule", errors[0])

    def test_dependabot_zero_pull_request_limit_is_rejected(self) -> None:
        errors = CHECK_PACKAGE_METADATA.validate_github_actions_updates(
            "version: 2\n"
            "updates:\n"
            "  - package-ecosystem: github-actions\n"
            "    directory: /\n"
            "    schedule:\n"
            "      interval: weekly\n"
            "    open-pull-requests-limit: 0\n"
        )

        self.assertEqual(1, len(errors))
        self.assertIn("nonzero", errors[0])

    def test_dependabot_ignore_all_actions_is_rejected(self) -> None:
        errors = CHECK_PACKAGE_METADATA.validate_github_actions_updates(
            "version: 2\n"
            "updates:\n"
            "  - package-ecosystem: github-actions\n"
            "    directory: /\n"
            "    schedule:\n"
            "      interval: weekly\n"
            "    ignore:\n"
            "      - dependency-name: '*'\n"
        )

        self.assertEqual(1, len(errors))
        self.assertIn("non-ignored", errors[0])

    def test_dependabot_collective_ignore_patterns_are_rejected(self) -> None:
        errors = CHECK_PACKAGE_METADATA.validate_github_actions_updates(
            "version: 2\n"
            "updates:\n"
            "  - package-ecosystem: github-actions\n"
            "    directory: /\n"
            "    schedule:\n"
            "      interval: weekly\n"
            "    ignore:\n"
            "      - dependency-name: 'actions/*'\n"
            "      - dependency-name: 'actions-rust-lang/*'\n"
            "      - dependency-name: 'taiki-e/*'\n"
        )

        self.assertEqual(1, len(errors))
        self.assertIn("non-ignored", errors[0])

    def test_checked_in_dependabot_updates_root_github_actions(self) -> None:
        dependabot = (SCRIPT.parents[1] / ".github/dependabot.yml").read_text(
            encoding="utf-8"
        )

        self.assertEqual(
            [], CHECK_PACKAGE_METADATA.validate_github_actions_updates(dependabot)
        )

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


class RepositoryCoordinateValidationTests(unittest.TestCase):
    def test_stale_active_coordinate_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "install.sh"
            path.write_text(
                "https://github.com/" + "gi-" + "dellav/zerostack/releases/latest",
                encoding="utf-8",
            )

            errors = CHECK_PACKAGE_METADATA.validate_stale_coordinates(
                root, ["install.sh"]
            )

            self.assertEqual(1, len(errors))
            self.assertIn("stale active coordinate", errors[0])


class ReleaseChecksumUpdateTests(unittest.TestCase):
    def test_http_failure_does_not_mutate_package_metadata(self) -> None:
        repository = SCRIPT.parents[1]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "scripts").mkdir()
            (root / "packaging/aur").mkdir(parents=True)
            shutil.copy(repository / "Cargo.toml", root / "Cargo.toml")
            shutil.copy(
                repository / "scripts/update-release-checksums.sh",
                root / "scripts/update-release-checksums.sh",
            )
            package = root / "packaging/aur/PKGBUILD"
            package.write_text("unchanged\n", encoding="utf-8")

            stub_bin = root / "stub-bin"
            stub_bin.mkdir()
            curl = stub_bin / "curl"
            curl.write_text(
                "#!/bin/sh\nprintf 'not found\\n' >&2\nexit 22\n",
                encoding="utf-8",
            )
            curl.chmod(0o755)
            env = os.environ.copy()
            env["PATH"] = f"{stub_bin}:{env['PATH']}"

            result = subprocess.run(
                ["bash", str(root / "scripts/update-release-checksums.sh"), "aur"],
                cwd=root,
                env=env,
                capture_output=True,
                text=True,
            )

            self.assertNotEqual(0, result.returncode)
            self.assertEqual("unchanged\n", package.read_text(encoding="utf-8"))

    def test_all_downloads_succeed_before_portable_recipe_updates(self) -> None:
        repository = SCRIPT.parents[1]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "scripts").mkdir()
            shutil.copy(repository / "Cargo.toml", root / "Cargo.toml")
            shutil.copy(
                repository / "scripts/update-release-checksums.sh",
                root / "scripts/update-release-checksums.sh",
            )
            for relative in (
                "packaging/aur/PKGBUILD",
                "packaging/conda/zerostack/meta.yaml",
                "packaging/conda/zerostack-bin/meta.yaml",
                "packaging/homebrew/zerostack.rb",
            ):
                destination = root / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy(repository / relative, destination)

            stub_bin = root / "stub-bin"
            stub_bin.mkdir()
            curl = stub_bin / "curl"
            curl.write_text(
                """#!/bin/bash
set -euo pipefail
out=""
for ((i = 1; i <= $#; i++)); do
    if [[ "${!i}" == "-o" ]]; then
        next=$((i + 1))
        out="${!next}"
    fi
done
printf '%s' "${!#}" > "$out"
""",
                encoding="utf-8",
            )
            curl.chmod(0o755)
            env = os.environ.copy()
            env["PATH"] = f"{stub_bin}:{env['PATH']}"

            result = subprocess.run(
                ["bash", str(root / "scripts/update-release-checksums.sh"), "all"],
                cwd=root,
                env=env,
                capture_output=True,
                text=True,
            )

            self.assertEqual(0, result.returncode, result.stderr)
            version = tomllib.loads((root / "Cargo.toml").read_text())["package"][
                "version"
            ]
            release = (
                "https://github.com/sebahrens/mini-agent/releases/download/"
                f"v{version}"
            )
            expected_by_recipe = {
                "packaging/aur/PKGBUILD": (
                    f"{release}/mini-agent-x86_64-unknown-linux-musl.tar.gz",
                    f"{release}/mini-agent-aarch64-unknown-linux-musl.tar.gz",
                    f"https://raw.githubusercontent.com/sebahrens/mini-agent/v{version}/LICENSE",
                ),
                "packaging/conda/zerostack/meta.yaml": (
                    f"https://github.com/sebahrens/mini-agent/archive/refs/tags/v{version}.tar.gz",
                ),
                "packaging/conda/zerostack-bin/meta.yaml": (
                    f"{release}/mini-agent-x86_64-unknown-linux-musl.tar.gz",
                    f"{release}/mini-agent-aarch64-unknown-linux-musl.tar.gz",
                    f"https://raw.githubusercontent.com/sebahrens/mini-agent/v{version}/LICENSE",
                ),
                "packaging/homebrew/zerostack.rb": (
                    f"{release}/mini-agent-x86_64-apple-darwin.tar.gz",
                    f"{release}/mini-agent-aarch64-apple-darwin.tar.gz",
                    f"{release}/mini-agent-x86_64-unknown-linux-musl.tar.gz",
                    f"{release}/mini-agent-aarch64-unknown-linux-musl.tar.gz",
                ),
            }
            for relative, urls in expected_by_recipe.items():
                text = (root / relative).read_text(encoding="utf-8")
                for url in urls:
                    self.assertIn(hashlib.sha256(url.encode()).hexdigest(), text)
            self.assertFalse(any(root.rglob("*.bak")))

    def test_historical_specs_are_narrowly_allowed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            relative = "docs/specs/superseded/historical.md"
            path = root / relative
            path.parent.mkdir(parents=True)
            path.write_text(
                "https://github.com/" + "gi-" + "dellav/zerostack",
                encoding="utf-8",
            )

            self.assertEqual(
                [],
                CHECK_PACKAGE_METADATA.validate_stale_coordinates(root, [relative]),
            )

    def test_mixed_case_stale_coordinate_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "active.md"
            path.write_text(
                "https://github.com/" + "GI-" + "DELLAV/ZEROSTACK/releases/latest",
                encoding="utf-8",
            )

            errors = CHECK_PACKAGE_METADATA.validate_stale_coordinates(
                root, ["active.md"]
            )

            self.assertEqual(1, len(errors))
            self.assertIn("stale active coordinate", errors[0])


class SupportedPackageSurfaceTests(unittest.TestCase):
    def test_supported_channels_are_explicit_and_exclude_nix(self) -> None:
        self.assertEqual(
            ("cargo", "aur", "conda", "homebrew"),
            CHECK_PACKAGE_METADATA.SUPPORTED_PACKAGE_CHANNELS,
        )
        self.assertNotIn("nix", CHECK_PACKAGE_METADATA.SUPPORTED_PACKAGE_CHANNELS)

    def test_checked_in_nix_entry_points_are_removed(self) -> None:
        self.assertEqual(
            [], CHECK_PACKAGE_METADATA.validate_removed_nix_surface(SCRIPT.parents[1])
        )

    def test_reintroduced_nix_entry_point_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            entrypoint = root / CHECK_PACKAGE_METADATA.REMOVED_NIX_ENTRYPOINTS[0]
            entrypoint.write_text("{}", encoding="utf-8")

            errors = CHECK_PACKAGE_METADATA.validate_removed_nix_surface(
                root, [entrypoint.name]
            )

            self.assertEqual(1, len(errors))
            self.assertIn("must remain removed", errors[0])

    def test_new_nix_surface_names_cannot_bypass_removal_policy(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            reintroduced = (
                "flake.nix",
                "flake.lock",
                "nix/package/mini-agent.nix",
                "packaging/nix/sources.json",
            )
            for relative in reintroduced:
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("{}", encoding="utf-8")

            errors = CHECK_PACKAGE_METADATA.validate_removed_nix_surface(
                root, list(reintroduced)
            )

            self.assertEqual(4, len(errors))
            for relative in reintroduced:
                self.assertTrue(any(relative in error for error in errors))

    def test_nix_restoration_policy_pins_every_required_gate(self) -> None:
        publishing = (
            SCRIPT.parents[1] / "docs/agent/PUBLISHING_RELEASES.md"
        ).read_text(encoding="utf-8")

        for requirement in (
            "pinned inputs",
            "Linux and macOS CI",
            "default-feature parity",
            "exact store output",
        ):
            self.assertIn(requirement, publishing)

    def test_untracked_local_nix_file_is_not_a_release_surface(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q"], cwd=root, check=True)
            local_shell = root / "shell.nix"
            local_shell.write_text("{}", encoding="utf-8")

            self.assertEqual(
                [], CHECK_PACKAGE_METADATA.validate_removed_nix_surface(root)
            )

            subprocess.run(["git", "add", "shell.nix"], cwd=root, check=True)
            errors = CHECK_PACKAGE_METADATA.validate_removed_nix_surface(root)
            self.assertEqual(1, len(errors))
            self.assertIn("shell.nix", errors[0])


if __name__ == "__main__":
    unittest.main()
