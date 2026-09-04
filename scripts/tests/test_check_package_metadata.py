import importlib.util
import hashlib
import json
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


class LicenseIdentityValidationTests(unittest.TestCase):
    def test_canonical_gpl_text_is_accepted_and_modified_text_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            shutil.copyfile(SCRIPT.parents[1] / "LICENSE", root / "LICENSE")
            self.assertEqual([], CHECK_PACKAGE_METADATA.validate_license_identity(root))

            (root / "LICENSE").write_text("not the GPL\n", encoding="utf-8")
            errors = CHECK_PACKAGE_METADATA.validate_license_identity(root)
            self.assertEqual(1, len(errors))
            self.assertIn("canonical GPL-3.0-only", errors[0])


class ReleaseWorkflowValidationTests(unittest.TestCase):
    def test_reviewed_current_release_action_pins_are_accepted(self) -> None:
        workflow = (
            "steps:\n"
            "  - uses: actions/upload-artifact@"
            "043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1\n"
            "  - uses: taiki-e/install-action@"
            "91ddec75689c4c78665b598d188dc821c5a43e5c # v2.85.9\n"
            "  - uses: actions/attest@"
            "1e69f48acb82d1966a394da916b4c1698aa569d6 # v4.2.2\n"
        )

        self.assertEqual(
            [], CHECK_PACKAGE_METADATA.validate_release_action_pins(workflow)
        )

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
            f"{'0' * 40} # v7.0.1\n"
        )

        self.assertEqual(1, len(errors))
        self.assertIn("does not match the reviewed approval map", errors[0])

    def test_cross_action_sha_is_rejected(self) -> None:
        errors = CHECK_PACKAGE_METADATA.validate_release_action_pins(
            "steps:\n"
            "  - uses: actions/upload-artifact@"
            "d3f86a106a0bac45b974a628896c90dbdf5c8093 # v7.0.1\n"
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

    def test_release_workflow_neutralizes_setup_rust_warning_denial(self) -> None:
        workflow = (
            SCRIPT.parents[1] / ".github/workflows/release.yml"
        ).read_text(encoding="utf-8")
        workflow = workflow.replace('  RUSTFLAGS: ""\n', "", 1)

        errors = CHECK_PACKAGE_METADATA.validate_workflow(workflow, "mini-agent")

        self.assertTrue(any("RUSTFLAGS" in error for error in errors))

    def test_release_full_archives_reject_unsupported_all_features(self) -> None:
        workflow = (
            SCRIPT.parents[1] / ".github/workflows/release.yml"
        ).read_text(encoding="utf-8")
        workflow = workflow.replace(
            "build --locked --release --target",
            "build --locked --release --all-features --target",
            1,
        )

        errors = CHECK_PACKAGE_METADATA.validate_workflow(workflow, "mini-agent")

        self.assertTrue(any("all-features" in error for error in errors))

    def test_release_builds_must_pass_locked(self) -> None:
        workflow = (
            SCRIPT.parents[1] / ".github/workflows/release.yml"
        ).read_text(encoding="utf-8")
        for command in (
            "run: cargo build --locked --release --target",
            "run: cargo build --locked --release --no-default-features --target",
            "run: cross build --locked --release --target",
            "run: cross build --locked --release --no-default-features --target",
        ):
            with self.subTest(command=command):
                self.assertIn(command, workflow)
                mutated = workflow.replace(
                    command, command.replace("--locked ", ""), 1
                )
                errors = CHECK_PACKAGE_METADATA.validate_workflow(
                    mutated, "mini-agent"
                )
                self.assertTrue(
                    any("--locked" in error for error in errors), errors
                )

    def test_release_version_is_read_once_from_cargo_manifest(self) -> None:
        workflow = (
            SCRIPT.parents[1] / ".github/workflows/release.yml"
        ).read_text(encoding="utf-8")

        self.assertEqual(
            [], CHECK_PACKAGE_METADATA.validate_release_version_source(workflow)
        )
        self.assertIsNone(
            CHECK_PACKAGE_METADATA.HARDCODED_RELEASE_VERSION.search(workflow)
        )

        mutations = {
            "hard-coded VSIX version": workflow.replace(
                'test -f "mini-agent-$RELEASE_VERSION-$target.vsix"',
                'test -f "mini-agent-1.8.0-$target.vsix"',
                1,
            ),
            "missing version output": workflow.replace(
                CHECK_PACKAGE_METADATA.RELEASE_VERSION_EXPORT, "true", 1
            ),
            "consumer without env": workflow.replace(
                "      RELEASE_VERSION: ${{ needs.package-metadata.outputs.version }}\n",
                "",
                1,
            ),
            "consumer without needs edge": workflow.replace(
                "needs: [package-metadata, vscode-vsix]", "needs: [vscode-vsix]", 1
            ),
        }
        for name, mutated in mutations.items():
            with self.subTest(name=name):
                self.assertNotEqual(workflow, mutated)
                errors = CHECK_PACKAGE_METADATA.validate_workflow(
                    mutated, "mini-agent"
                )
                self.assertTrue(
                    any(
                        "RELEASE_VERSION" in error
                        or "package-metadata" in error
                        for error in errors
                    ),
                    errors,
                )

    def test_release_requires_versioned_changelog_notes(self) -> None:
        workflow = (
            SCRIPT.parents[1] / ".github/workflows/release.yml"
        ).read_text(encoding="utf-8")
        workflow = workflow.replace(
            "--notes-file release-notes.md", "--generate-notes", 1
        )

        errors = CHECK_PACKAGE_METADATA.validate_workflow(workflow, "mini-agent")

        self.assertTrue(any("CHANGELOG section" in error for error in errors))

    def test_release_notes_job_requires_checkout(self) -> None:
        workflow = (
            SCRIPT.parents[1] / ".github/workflows/release.yml"
        ).read_text(encoding="utf-8")
        publish_start = workflow.index("\n  publish-release:")
        prefix, publish_job = workflow[:publish_start], workflow[publish_start:]
        publish_job = publish_job.replace(
            "      - uses: actions/checkout@", "      - uses: removed/checkout@", 1
        )

        errors = CHECK_PACKAGE_METADATA.validate_workflow(
            prefix + publish_job, "mini-agent"
        )

        self.assertTrue(any("CHANGELOG section" in error for error in errors))

    def test_release_requires_vendored_corresponding_source(self) -> None:
        workflow = (
            SCRIPT.parents[1] / ".github/workflows/release.yml"
        ).read_text(encoding="utf-8")
        workflow = workflow.replace(
            'bash scripts/package-corresponding-source.sh "$GITHUB_REF_NAME" . HEAD',
            "true",
            1,
        )

        errors = CHECK_PACKAGE_METADATA.validate_workflow(workflow, "mini-agent")

        self.assertTrue(any("Corresponding Source" in error for error in errors))

    def test_corresponding_source_script_avoids_pipefail_archive_listing(self) -> None:
        script = (
            SCRIPT.parents[1] / "scripts/package-corresponding-source.sh"
        ).read_text(encoding="utf-8")

        self.assertIn(
            'tar tzf "$STAGING_DIR/$ARCHIVE_NAME" > "$ARCHIVE_LISTING"', script
        )
        self.assertNotIn("tar tzf \"$STAGING_DIR/$ARCHIVE_NAME\" | grep", script)

    def test_corresponding_source_script_vendors_and_checks_offline_metadata(
        self,
    ) -> None:
        script = (
            SCRIPT.parents[1] / "scripts/package-corresponding-source.sh"
        ).read_text(encoding="utf-8")

        self.assertIn(
            "cargo vendor --locked --versioned-dirs vendor > .cargo/config.toml",
            script,
        )
        self.assertIn(
            "cargo metadata --locked --offline --format-version 1 > /dev/null",
            script,
        )
        self.assertIn(
            'if [[ "$SOURCE_COMMIT" != "$TAG_COMMIT" ]]; then', script
        )
        self.assertIn(
            'elif [[ "$ALLOW_UNTAGGED_LABEL" != true ]]; then', script
        )

    def test_release_requires_gpl_binary_packager_for_every_archive(self) -> None:
        workflow = (
            SCRIPT.parents[1] / ".github/workflows/release.yml"
        ).read_text(encoding="utf-8")
        workflow = workflow.replace(
            "python3 scripts/package-release-binary.py \\",
            "python3 ignored-packager.py \\",
            1,
        )

        errors = CHECK_PACKAGE_METADATA.validate_workflow(workflow, "mini-agent")

        self.assertTrue(
            any("package-release-binary.py" in error for error in errors)
        )

    def test_release_requires_native_archive_smoke_before_checksums(self) -> None:
        workflow = (
            SCRIPT.parents[1] / ".github/workflows/release.yml"
        ).read_text(encoding="utf-8")
        mutations = (
            ("--expect-js yes", "--expect-js no"),
            (
                "needs: [archive-smoke, corresponding-source]",
                "needs: [build, build-static, build-windows, corresponding-source]",
            ),
        )
        for old, new in mutations:
            with self.subTest(fragment=old):
                modified = workflow.replace(old, new, 1)
                errors = CHECK_PACKAGE_METADATA.validate_workflow(modified, "mini-agent")
                self.assertTrue(any("native-smoke" in error or "private artifact" in error for error in errors))

    def test_release_requires_strict_manifest_and_atomic_publication_gates(self) -> None:
        workflow = (
            SCRIPT.parents[1] / ".github/workflows/release.yml"
        ).read_text(encoding="utf-8")
        mutations = (
            ("python3 scripts/release_artifacts.py manifest \\", "sha256sum *.tar.gz"),
            ("python3 scripts/release_artifacts.py verify \\", "true # skipped verify"),
            ('gh release create "$tag"', 'gh release upload "$tag"'),
            ("release_flags=(--draft --verify-tag)", "release_flags=()"),
            (
                "actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6",
                "true # skipped provenance",
            ),
            ("subject-path: private-publish/**/*", "subject-path: one-file"),
            ("artifact-metadata: write", "artifact-metadata: read"),
            ("attestations: write", "attestations: read"),
            ("id-token: write", "id-token: none"),
            ("--jq '.assets[].name' | sort", "--jq '.assets[0].name' | sort"),
            (
                'gh release edit "$tag" --repo "$GITHUB_REPOSITORY" --draft=false',
                "true # left as draft",
            ),
        )
        for old, new in mutations:
            with self.subTest(fragment=old):
                modified = workflow.replace(old, new, 1)
                errors = CHECK_PACKAGE_METADATA.validate_workflow(modified, "mini-agent")
                self.assertTrue(
                    any("private artifact" in error or "atomic publication" in error for error in errors),
                    errors,
                )

    def test_release_rejects_publication_before_draft_verification(self) -> None:
        workflow = (
            SCRIPT.parents[1] / ".github/workflows/release.yml"
        ).read_text(encoding="utf-8")
        create = 'gh release create "$tag"'
        publish = 'gh release edit "$tag" --repo "$GITHUB_REPOSITORY" --draft=false'
        modified = workflow.replace(create, "PUBLICATION_ORDER_PLACEHOLDER", 1)
        modified = modified.replace(publish, create, 1)
        modified = modified.replace("PUBLICATION_ORDER_PLACEHOLDER", publish, 1)

        errors = CHECK_PACKAGE_METADATA.validate_workflow(modified, "mini-agent")

        self.assertTrue(any("atomic publication steps" in error for error in errors))

    def test_static_release_matrix_rejects_arm_cross_host(self) -> None:
        workflow = (
            SCRIPT.parents[1] / ".github/workflows/release.yml"
        ).read_text(encoding="utf-8")
        workflow = workflow.replace(
            "          - os: ubuntu-latest\n"
            "            target: aarch64-unknown-linux-musl",
            "          - os: ubuntu-24.04-arm\n"
            "            target: aarch64-unknown-linux-musl",
            1,
        )

        errors = CHECK_PACKAGE_METADATA.validate_workflow(workflow, "mini-agent")

        self.assertTrue(any("x86_64 hosts" in error for error in errors))

    def test_static_arm_release_archives_require_elf_validation(self) -> None:
        workflow = (
            SCRIPT.parents[1] / ".github/workflows/release.yml"
        ).read_text(encoding="utf-8")
        commands = (
            'file "$smoke_dir/$CANONICAL_BINARY" | grep -Fq -- "ARM aarch64"',
            'readelf -l "$smoke_dir/$CANONICAL_BINARY" > '
            '"$smoke_dir/program-headers"',
            'if grep -Fq -- "INTERP" "$smoke_dir/program-headers"; then',
        )

        for command in commands:
            with self.subTest(command=command):
                modified = workflow.replace(command, "true", 1)
                errors = CHECK_PACKAGE_METADATA.validate_workflow(
                    modified, "mini-agent"
                )
                self.assertTrue(any(command in error for error in errors))

    def test_checked_in_cross_images_are_reviewed_and_immutable(self) -> None:
        cross_config = (SCRIPT.parents[1] / "Cross.toml").read_text(
            encoding="utf-8"
        )

        self.assertEqual(
            [], CHECK_PACKAGE_METADATA.validate_cross_images(cross_config)
        )

    def test_mutable_cross_image_tag_is_rejected(self) -> None:
        target, expected = next(
            iter(CHECK_PACKAGE_METADATA.EXPECTED_CROSS_IMAGES.items())
        )
        cross_config = f'[target.{target}]\nimage = "{expected}"\n'
        cross_config = cross_config.replace(
            "@sha256:" + expected.rsplit("@sha256:", 1)[1], ":main"
        )

        errors = CHECK_PACKAGE_METADATA.validate_cross_images(cross_config)

        self.assertTrue(
            any(target in error and "reviewed image" in error for error in errors)
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

    def test_duplicate_expected_release_archive_is_rejected(self) -> None:
        workflow = (
            SCRIPT.parents[1] / ".github/workflows/release.yml"
        ).read_text(encoding="utf-8")
        workflow = workflow.replace(
            "mini-agent-lite-aarch64-unknown-linux-gnu.tar.gz",
            "mini-agent-lite-x86_64-unknown-linux-gnu.tar.gz",
            1,
        )

        errors = CHECK_PACKAGE_METADATA.validate_workflow(workflow, "mini-agent")

        self.assertTrue(any("duplicate entries" in error for error in errors))
        self.assertTrue(any("invalid archive set" in error for error in errors))

    def test_unexpected_release_archive_is_rejected(self) -> None:
        workflow = (
            SCRIPT.parents[1] / ".github/workflows/release.yml"
        ).read_text(encoding="utf-8")
        workflow = workflow.replace(
            "mini-agent-lite-aarch64-unknown-linux-gnu.tar.gz",
            "mini-agent-lite-riscv64-unknown-linux-gnu.tar.gz",
            1,
        )

        errors = CHECK_PACKAGE_METADATA.validate_workflow(workflow, "mini-agent")

        self.assertTrue(any("missing=" in error for error in errors))
        self.assertTrue(any("unexpected=" in error for error in errors))

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
    def test_default_scan_ignores_untracked_local_files(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "--quiet"], cwd=root, check=True)
            tracked = root / "install.sh"
            tracked.write_text(
                "https://github.com/sebahrens/mini-agent/releases/latest",
                encoding="utf-8",
            )
            subprocess.run(["git", "add", "install.sh"], cwd=root, check=True)
            local_plan = root / "docs/plans/historical.md"
            local_plan.parent.mkdir(parents=True)
            local_plan.write_text(
                "https://github.com/" + "gi-" + "dellav/zerostack",
                encoding="utf-8",
            )

            self.assertEqual(
                [], CHECK_PACKAGE_METADATA.validate_stale_coordinates(root)
            )

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

    def test_checked_in_release_fragments_are_assigned_to_their_owners(self) -> None:
        repository = SCRIPT.parents[1]

        self.assertEqual(
            [],
            CHECK_PACKAGE_METADATA.validate_file_fragments(
                repository, "mini-agent"
            ),
        )

    def test_checked_in_package_recipes_install_compliance_documents(self) -> None:
        self.assertEqual(
            [],
            CHECK_PACKAGE_METADATA.validate_distribution_notice_installs(
                SCRIPT.parents[1]
            ),
        )

    def test_each_package_recipe_compliance_install_is_fail_closed(self) -> None:
        repository = SCRIPT.parents[1]
        for relative, fragments in (
            CHECK_PACKAGE_METADATA.DISTRIBUTION_NOTICE_FRAGMENTS.items()
        ):
            for fragment in fragments:
                with self.subTest(relative=relative, fragment=fragment):
                    with tempfile.TemporaryDirectory() as directory:
                        root = Path(directory)
                        for source_relative in (
                            CHECK_PACKAGE_METADATA.DISTRIBUTION_NOTICE_FRAGMENTS
                        ):
                            source = repository / source_relative
                            destination = root / source_relative
                            destination.parent.mkdir(parents=True, exist_ok=True)
                            shutil.copy(source, destination)
                        path = root / relative
                        path.write_text(
                            path.read_text(encoding="utf-8").replace(fragment, "", 1),
                            encoding="utf-8",
                        )

                        errors = (
                            CHECK_PACKAGE_METADATA.validate_distribution_notice_installs(
                                root
                            )
                        )

                        self.assertTrue(
                            any(relative in error and fragment in error for error in errors)
                        )

    def test_checked_in_aur_srcinfo_matches_pkgbuild(self) -> None:
        self.assertEqual(
            [],
            CHECK_PACKAGE_METADATA.validate_aur_srcinfo_checksums(SCRIPT.parents[1]),
        )

    def test_stale_aur_srcinfo_checksum_is_rejected(self) -> None:
        repository = SCRIPT.parents[1]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            destination = root / "packaging/aur"
            destination.mkdir(parents=True)
            shutil.copy(repository / "packaging/aur/PKGBUILD", destination)
            shutil.copy(repository / "packaging/aur/.SRCINFO", destination)
            srcinfo = destination / ".SRCINFO"
            text = srcinfo.read_text(encoding="utf-8")
            first_digest = CHECK_PACKAGE_METADATA.SHA256_DIGEST.search(text)
            assert first_digest is not None
            srcinfo.write_text(
                text.replace(first_digest.group(0), "f" * 64, 1),
                encoding="utf-8",
            )

            errors = CHECK_PACKAGE_METADATA.validate_aur_srcinfo_checksums(root)

            self.assertTrue(any("checksums must match" in error for error in errors))


class ReleaseDigestValidationTests(unittest.TestCase):
    PREVIOUS_TAG = "v1.7.2"
    PREVIOUS_DIGEST = (
        "d0bae6b5b7813f4a4fe1aebf1ee5aeaac97e64698781a16cd00728c3d14f3f97"
    )
    LICENSE = CHECK_PACKAGE_METADATA.CANONICAL_GPL3_LICENSE_SHA256

    def write_recipes(self, root: Path, digest: str) -> None:
        recipes = {
            "packaging/homebrew/zerostack.rb": (
                '  url "https://github.com/sebahrens/mini-agent/releases/download/v1.8.0/x.tar.gz"\n'
                f'  sha256 "{digest}"\n'
            ),
            "packaging/aur/PKGBUILD": (
                f"sha256sums_x86_64=('{digest}' '{self.LICENSE}')\n"
            ),
            "packaging/aur/.SRCINFO": (
                f"\tsha256sums_x86_64 = {digest}\n"
                f"\tsha256sums_x86_64 = {self.LICENSE}\n"
            ),
            "packaging/conda/zerostack-bin/meta.yaml": (
                f"    sha256: {digest}\n    sha256: {self.LICENSE}\n"
            ),
            "packaging/conda/zerostack/meta.yaml": f"  sha256: {digest}\n",
        }
        for relative, text in recipes.items():
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")

    def test_license_digest_is_not_a_release_digest(self) -> None:
        self.assertEqual(
            ["a" * 64],
            CHECK_PACKAGE_METADATA.recipe_release_digests(
                f"sha256 {'a' * 64}\nsha256 {self.LICENSE}\n"
            ),
        )

    def test_digest_copied_from_previous_release_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_recipes(root, self.PREVIOUS_DIGEST)

            errors = CHECK_PACKAGE_METADATA.validate_release_digests(
                root,
                "1.8.0",
                previous=(self.PREVIOUS_TAG, {self.PREVIOUS_DIGEST, self.LICENSE}),
            )

            self.assertEqual(5, len(errors), errors)
            self.assertTrue(
                all("recorded for v1.7.2" in error for error in errors), errors
            )

    def test_placeholder_is_allowed_before_post_release_only(self) -> None:
        placeholder = CHECK_PACKAGE_METADATA.RELEASE_DIGEST_PLACEHOLDER
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_recipes(root, placeholder)
            previous = (self.PREVIOUS_TAG, {self.PREVIOUS_DIGEST, placeholder})

            self.assertEqual(
                [],
                CHECK_PACKAGE_METADATA.validate_release_digests(
                    root, "1.8.0", previous=previous
                ),
            )
            errors = CHECK_PACKAGE_METADATA.validate_release_digests(
                root, "1.8.0", previous=previous, require_release_digests=True
            )
            self.assertEqual(5, len(errors), errors)
            self.assertTrue(
                all("just post-release" in error for error in errors), errors
            )

    def test_fresh_release_digests_are_accepted(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_recipes(root, "b" * 64)

            self.assertEqual(
                [],
                CHECK_PACKAGE_METADATA.validate_release_digests(
                    root,
                    "1.8.0",
                    previous=(self.PREVIOUS_TAG, {self.PREVIOUS_DIGEST}),
                    require_release_digests=True,
                ),
            )

    def test_previous_tag_lookup_ignores_current_and_newer_tags(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "--quiet"], cwd=root, check=True)
            for key, value in (
                ("user.email", "release-test@example.invalid"),
                ("user.name", "Release Test"),
            ):
                subprocess.run(["git", "config", key, value], cwd=root, check=True)
            recipe = root / "packaging/conda/zerostack/meta.yaml"
            recipe.parent.mkdir(parents=True)
            for tag, digest in (("v1.7.2", "c" * 64), ("v1.8.0", "d" * 64)):
                recipe.write_text(f"  sha256: {digest}\n", encoding="utf-8")
                subprocess.run(["git", "add", "-A"], cwd=root, check=True)
                subprocess.run(
                    ["git", "commit", "--quiet", "-m", tag], cwd=root, check=True
                )
                subprocess.run(["git", "tag", tag], cwd=root, check=True)

            previous_tag, digests = CHECK_PACKAGE_METADATA.previous_release_digests(
                root, "1.8.0"
            )

            self.assertEqual("v1.7.2", previous_tag)
            self.assertEqual({"c" * 64}, digests)

    def test_checked_in_recipes_carry_no_previous_release_digest(self) -> None:
        repository = SCRIPT.parents[1]
        version = tomllib.loads(
            (repository / "Cargo.toml").read_text(encoding="utf-8")
        )["package"]["version"]

        self.assertEqual(
            [],
            CHECK_PACKAGE_METADATA.validate_release_digests(repository, version),
        )


class VersionLiteralValidationTests(unittest.TestCase):
    VERSION_FILES = (
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
    )

    def copy_version_files(self, root: Path) -> None:
        repository = SCRIPT.parents[1]
        for relative in self.VERSION_FILES:
            destination = root / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy(repository / relative, destination)

    def test_checked_in_version_literals_match_cargo(self) -> None:
        repository = SCRIPT.parents[1]
        version = tomllib.loads(
            (repository / "Cargo.toml").read_text(encoding="utf-8")
        )["package"]["version"]

        self.assertEqual(
            [], CHECK_PACKAGE_METADATA.validate_versions(repository, version)
        )

    def test_every_version_bearing_file_is_checked(self) -> None:
        repository = SCRIPT.parents[1]
        version = tomllib.loads(
            (repository / "Cargo.toml").read_text(encoding="utf-8")
        )["package"]["version"]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.copy_version_files(root)
            for relative in self.VERSION_FILES:
                with self.subTest(relative=relative):
                    path = root / relative
                    original = path.read_text(encoding="utf-8")
                    path.write_text(
                        original.replace(version, "0.0.1"), encoding="utf-8"
                    )
                    errors = CHECK_PACKAGE_METADATA.validate_versions(root, version)
                    self.assertTrue(
                        any(relative in error for error in errors), errors
                    )
                    path.write_text(original, encoding="utf-8")

    def test_protocol_version_in_registry_is_not_the_agent_version(self) -> None:
        registry = json.loads(
            (SCRIPT.parents[1] / "docs/acp-registry.json").read_text(
                encoding="utf-8"
            )
        )

        self.assertNotEqual(
            registry["agent"]["version"], registry["agent"]["protocol"]["version"]
        )


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
                    f"https://github.com/sebahrens/mini-agent/releases/download/v{version}/mini-agent-v{version}-source.tar.gz",
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

    def test_upstream_provenance_files_may_name_zerostack_repository(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for relative in CHECK_PACKAGE_METADATA.UPSTREAM_PROVENANCE_FILES:
                (root / relative).write_text(
                    "https://github.com/" + "gi-" + "dellav/zerostack",
                    encoding="utf-8",
                )

            self.assertEqual(
                [],
                CHECK_PACKAGE_METADATA.validate_stale_coordinates(
                    root, list(CHECK_PACKAGE_METADATA.UPSTREAM_PROVENANCE_FILES)
                ),
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
