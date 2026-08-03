import importlib.util
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


if __name__ == "__main__":
    unittest.main()
