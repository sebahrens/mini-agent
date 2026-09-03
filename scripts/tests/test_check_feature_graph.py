#!/usr/bin/env python3
"""Unit tests for the supported Cargo feature graph gate."""

from __future__ import annotations

import copy
import re
import sys
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPOSITORY_ROOT / "scripts"))

import check_feature_graph as feature_graph  # noqa: E402


class FeatureGraphTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.manifest = feature_graph.load_manifest(REPOSITORY_ROOT / "Cargo.toml")
        cls.workflow_text = (
            REPOSITORY_ROOT / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        cls.workflow_matrices = feature_graph.load_workflow_matrices(
            REPOSITORY_ROOT / ".github" / "workflows" / "ci.yml"
        )

    MATRIX_INTERPOLATION = "${{ matrix.features }}"

    def cargo_command_line(self, subcommand: str) -> str:
        """Return the checked-in single-line run command that consumes the matrix.

        The exact Cargo flags around the interpolation change over time, so tests
        locate the line by its subcommand instead of hard-coding the flag list.
        """
        pattern = re.compile(
            rf"^ *run: cargo {re.escape(subcommand)} --locked\b[^\n]*"
            rf"{re.escape(self.MATRIX_INTERPOLATION)}[^\n]*$",
            re.MULTILINE,
        )
        matches = pattern.findall(self.workflow_text)
        self.assertEqual(
            1,
            len(matches),
            f"expected exactly one single-line cargo {subcommand} matrix command",
        )
        return matches[0]

    def mutate_cargo_command(self, subcommand: str, replacement: str) -> str:
        line = self.cargo_command_line(subcommand)
        mutated = self.workflow_text.replace(line, replacement, 1)
        self.assertNotEqual(self.workflow_text, mutated, "mutation must change the workflow")
        return mutated

    def test_repository_manifest_encodes_supported_relationships(self) -> None:
        self.assertEqual([], feature_graph.validate_manifest(self.manifest))

    def test_skills_must_imply_js(self) -> None:
        manifest = copy.deepcopy(self.manifest)
        manifest["features"]["skills"].remove("js")

        self.assertIn(
            "feature 'skills' must imply feature 'js'",
            feature_graph.validate_manifest(manifest),
        )

    def test_sandbox_must_remain_independent_from_js(self) -> None:
        manifest = copy.deepcopy(self.manifest)
        manifest["features"]["sandbox"].append("js")

        self.assertIn(
            "feature 'sandbox' must not imply 'js'",
            feature_graph.validate_manifest(manifest),
        )

    def test_dependency_ownership_uses_semantic_feature_closure(self) -> None:
        manifest = copy.deepcopy(self.manifest)
        manifest["features"]["js-runtime"] = manifest["features"]["js"]
        manifest["features"]["js"] = ["js-runtime"]

        self.assertEqual([], feature_graph.validate_manifest(manifest))

    def test_clearing_optional_owner_features_is_rejected(self) -> None:
        expectations = {
            "acp": "feature 'acp' must activate optional dependency 'agent-client-protocol'",
            "lsp": "feature 'lsp' must activate optional dependency 'lsp-types'",
            "skills-embed-dynamic": (
                "feature 'skills-embed-dynamic' must imply feature 'skills-embed'"
            ),
        }
        for owner, expected in expectations.items():
            with self.subTest(owner=owner):
                manifest = copy.deepcopy(self.manifest)
                manifest["features"][owner] = []
                self.assertIn(expected, feature_graph.validate_manifest(manifest))

    def test_optional_owner_rows_cover_acp_lsp_and_dynamic_embedding(self) -> None:
        rows = {row.name: row for row in feature_graph.FEATURE_ROWS}

        self.assertEqual(
            {"agent-client-protocol", "blocking"}, rows["acp"].required
        )
        self.assertEqual(
            {"lsp-types", "process-wrap", "url", "which"}, rows["lsp"].required
        )
        self.assertEqual(
            {
                "fastembed",
                "hnsw_rs",
                "matrixmultiply",
                "ort",
                "rquickjs",
                "rusqlite",
            },
            rows["skills-embed-dynamic"].required,
        )

    def test_extras_row_covers_every_otherwise_uncompiled_feature(self) -> None:
        rows = {row.name: row for row in feature_graph.FEATURE_ROWS}
        extras = rows["extras"]

        self.assertEqual(
            {"hooks", "advisor", "lsp", "multimodal", "pdf"},
            set(extras.features.split(",")),
        )
        self.assertEqual({"lsp-types", "process-wrap", "url", "which"}, extras.required)
        self.assertNotIn("skills-embed", extras.features)
        for job, names in (
            ("test", feature_graph.TEST_MATRIX_ROWS),
            ("clippy", feature_graph.CLIPPY_MATRIX_ROWS),
        ):
            with self.subTest(job=job):
                self.assertIn("extras", names)
                self.assertNotIn("skills-embed", names)

    def test_workflow_rejects_missing_extras_row(self) -> None:
        extras = "--no-default-features --features hooks,advisor,lsp,multimodal,pdf"
        for job in ("test", "clippy"):
            with self.subTest(job=job):
                matrices = copy.deepcopy(feature_graph.expected_workflow_matrices())
                matrices[job].remove(extras)
                self.assertIn(
                    f"{job} matrix is missing focused row 'extras'",
                    feature_graph.validate_workflow_matrices(matrices),
                )

    def test_every_optional_dependency_must_be_classified(self) -> None:
        manifest = copy.deepcopy(self.manifest)
        manifest["dependencies"]["new-backend"] = {
            "version": "1",
            "optional": True,
        }

        self.assertIn(
            "optional dependency 'new-backend' is not covered by feature rows",
            feature_graph.validate_manifest(manifest),
        )

    def test_dependency_leak_is_reported_for_disabled_feature(self) -> None:
        packages = feature_graph.expected_package_sets()
        packages["no-default"].add("rquickjs")

        self.assertIn(
            "no-default unexpectedly activates optional package 'rquickjs'",
            feature_graph.validate_activation(packages),
        )

    def test_missing_implied_dependency_is_reported(self) -> None:
        packages = feature_graph.expected_package_sets()
        packages["skills"].remove("rquickjs")

        self.assertIn(
            "skills must activate optional package 'rquickjs'",
            feature_graph.validate_activation(packages),
        )

    def test_every_focused_row_disables_default_features(self) -> None:
        for row in feature_graph.FEATURE_ROWS:
            command = feature_graph.cargo_tree_command(row)
            self.assertIn("--no-default-features", command, row.name)

    def test_repository_workflow_matches_required_feature_rows(self) -> None:
        self.assertEqual(
            [], feature_graph.validate_workflow_matrices(self.workflow_matrices)
        )
        self.assertEqual(
            [], feature_graph.validate_workflow_commands(self.workflow_text)
        )

    def test_workflow_parser_accepts_inline_comments(self) -> None:
        workflow = self.workflow_text.replace(
            '          - ""', '          - "" # default row'
        )

        matrices = {
            job: feature_graph.workflow_matrix_values(workflow, job)
            for job in ("test", "clippy")
        }
        self.assertEqual([], feature_graph.validate_workflow_matrices(matrices))

    def test_workflow_parser_accepts_deeper_sequence_indentation(self) -> None:
        workflow = self.workflow_text.replace("\n          - ", "\n              - ")

        matrices = {
            job: feature_graph.workflow_matrix_values(workflow, job)
            for job in ("test", "clippy")
        }
        self.assertEqual([], feature_graph.validate_workflow_matrices(matrices))

    def test_workflow_parser_rejects_malformed_quoted_values(self) -> None:
        workflow = self.workflow_text.replace(
            '          - "--no-default-features"',
            '          - "--no-default-features',
            1,
        )

        with self.assertRaisesRegex(ValueError, "unterminated quoted scalar"):
            feature_graph.workflow_matrix_values(workflow, "clippy")

    def test_workflow_rejects_missing_memory_and_sandbox_rows(self) -> None:
        matrices = copy.deepcopy(feature_graph.expected_workflow_matrices())
        matrices["test"].remove("--no-default-features --features memory")
        matrices["clippy"].remove("--no-default-features --features sandbox")

        errors = feature_graph.validate_workflow_matrices(matrices)
        self.assertIn("test matrix is missing focused row 'memory'", errors)
        self.assertIn("clippy matrix is missing focused row 'sandbox'", errors)

    def test_workflow_comparison_is_semantic(self) -> None:
        matrices = copy.deepcopy(feature_graph.expected_workflow_matrices())
        full = "--no-default-features --features mcp,js,sandbox,skills,memory"
        matrices["test"][matrices["test"].index(full)] = (
            "--features skills,memory,mcp,js,sandbox --no-default-features"
        )
        matrices["clippy"][matrices["clippy"].index(full)] = (
            "--features sandbox,js,memory,skills,mcp --no-default-features"
        )

        self.assertEqual([], feature_graph.validate_workflow_matrices(matrices))

    def test_workflow_commands_must_consume_their_feature_matrix(self) -> None:
        mutations = {
            "test": self.workflow_text.replace(
                "cargo test --locked ${{ matrix.features }}",
                "cargo test --locked",
            ),
            "clippy": self.mutate_cargo_command(
                "clippy",
                self.cargo_command_line("clippy").replace(
                    self.MATRIX_INTERPOLATION, "${{ matrix.feature_args }}", 1
                ),
            ),
        }
        for job, workflow in mutations.items():
            with self.subTest(job=job):
                self.assertIn(
                    f"{job} job Cargo command must consume "
                    "'${{ matrix.features }}'",
                    feature_graph.validate_workflow_commands(workflow),
                )

    def test_workflow_commands_reject_interpolation_in_comments(self) -> None:
        mutations = {
            "clippy YAML comment": (
                "clippy",
                self.mutate_cargo_command(
                    "clippy",
                    self.cargo_command_line("clippy").replace(
                        f" {self.MATRIX_INTERPOLATION}", "", 1
                    )
                    + f" # {self.MATRIX_INTERPOLATION}",
                ),
            ),
            "test block shell comment": (
                "test",
                self.workflow_text.replace(
                    "cargo test --locked ${{ matrix.features }}",
                    "cargo test --locked # ${{ matrix.features }}",
                ),
            ),
        }
        for name, (job, workflow) in mutations.items():
            with self.subTest(name=name):
                self.assertIn(
                    f"{job} job Cargo command must consume "
                    "'${{ matrix.features }}'",
                    feature_graph.validate_workflow_commands(workflow),
                )

    def test_interpolation_in_another_shell_command_does_not_bind_matrix(self) -> None:
        workflow = self.workflow_text.replace(
            "cargo test --locked ${{ matrix.features }}",
            "cargo test --locked; echo ${{ matrix.features }}",
        )

        self.assertIn(
            "test job Cargo command must consume '${{ matrix.features }}'",
            feature_graph.validate_workflow_commands(workflow),
        )

    def test_redirection_operands_do_not_bind_feature_matrix(self) -> None:
        redirections = (">", "2>", ">>", "2>>", "<", "<<", "<<<")
        for redirection in redirections:
            with self.subTest(redirection=redirection):
                workflow = self.workflow_text.replace(
                    "cargo test --locked ${{ matrix.features }}",
                    f"cargo test --locked {redirection} ${{{{ matrix.features }}}}",
                )
                self.assertIn(
                    "test job Cargo command must consume "
                    "'${{ matrix.features }}'",
                    feature_graph.validate_workflow_commands(workflow),
                )

    def test_interpolation_must_be_a_complete_cargo_argument(self) -> None:
        workflow = self.workflow_text.replace(
            "cargo test --locked ${{ matrix.features }}",
            "cargo test --locked prefix-${{ matrix.features }}",
        )

        self.assertIn(
            "test job Cargo command must consume '${{ matrix.features }}'",
            feature_graph.validate_workflow_commands(workflow),
        )

    def test_workflow_command_accepts_quoted_matrix_argument(self) -> None:
        workflow = self.workflow_text.replace(
            "cargo test --locked ${{ matrix.features }}",
            'cargo test --locked "${{ matrix.features }}"',
        )

        self.assertEqual([], feature_graph.validate_workflow_commands(workflow))

    def test_workflow_command_accepts_multiline_cargo_invocation(self) -> None:
        workflow = self.workflow_text.replace(
            "cargo test --locked ${{ matrix.features }}",
            "cargo test --locked",
        ).replace(
            "run: cargo test --locked",
            "run: |\n"
            "          cargo test --locked \\\n"
            "            ${{ matrix.features }}",
            1,
        )

        self.assertEqual([], feature_graph.validate_workflow_commands(workflow))


if __name__ == "__main__":
    unittest.main()
