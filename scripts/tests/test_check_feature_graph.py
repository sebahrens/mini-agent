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

    def test_repository_manifest_encodes_supported_relationships(self) -> None:
        self.assertEqual([], feature_graph.validate_manifest(self.manifest))

    def test_skills_must_imply_js(self) -> None:
        manifest = copy.deepcopy(self.manifest)
        manifest["features"]["skills"].remove("js")

        self.assertIn(
            "feature 'skills' must imply 'js'",
            feature_graph.validate_manifest(manifest),
        )

    def test_sandbox_must_remain_independent_from_js(self) -> None:
        manifest = copy.deepcopy(self.manifest)
        manifest["features"]["sandbox"].append("js")

        self.assertIn(
            "feature 'sandbox' must not imply 'js'",
            feature_graph.validate_manifest(manifest),
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


if __name__ == "__main__":
    unittest.main()
