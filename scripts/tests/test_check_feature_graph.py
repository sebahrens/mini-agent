#!/usr/bin/env python3
"""Unit tests for the supported Cargo feature graph gate."""

from __future__ import annotations

import copy
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
        self.assertEqual({"lsp-types"}, rows["lsp"].required)
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
                1,
            ),
            "clippy": self.workflow_text.replace(
                "cargo clippy --locked ${{ matrix.features }}",
                "cargo clippy --locked ${{ matrix.feature_args }}",
                1,
            ),
        }
        for job, workflow in mutations.items():
            with self.subTest(job=job):
                self.assertIn(
                    f"{job} job Cargo command must consume "
                    "'${{ matrix.features }}'",
                    feature_graph.validate_workflow_commands(workflow),
                )


if __name__ == "__main__":
    unittest.main()
