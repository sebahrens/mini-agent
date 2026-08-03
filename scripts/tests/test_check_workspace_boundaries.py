#!/usr/bin/env python3
"""Regression tests for the production/research Cargo workspace boundary."""

from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "check_workspace_boundaries.py"
SPEC = importlib.util.spec_from_file_location("check_workspace_boundaries", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
BOUNDARIES = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BOUNDARIES)


def metadata(name: str, manifest: Path, *, extra: str | None = None) -> dict:
    package_id = f"path+file://{manifest.parent}#{name}@1.0.0"
    packages = [
        {"name": name, "manifest_path": str(manifest), "id": package_id}
    ]
    members = [package_id]
    if extra is not None:
        extra_id = f"path+file:///tmp/{extra}#{extra}@1.0.0"
        packages.append(
            {
                "name": extra,
                "manifest_path": f"/tmp/{extra}/Cargo.toml",
                "id": extra_id,
            }
        )
        members.append(extra_id)
    return {"packages": packages, "workspace_members": members}


class WorkspaceBoundaryTests(unittest.TestCase):
    def test_single_expected_package_is_accepted(self) -> None:
        manifest = ROOT / "Cargo.toml"
        self.assertEqual(
            [],
            BOUNDARIES.validate_metadata(
                metadata("mini-agent", manifest),
                expected_name="mini-agent",
                expected_manifest=manifest,
            ),
        )

    def test_research_package_in_root_metadata_is_rejected(self) -> None:
        errors = BOUNDARIES.validate_metadata(
            metadata("mini-agent", ROOT / "Cargo.toml", extra="spike"),
            expected_name="mini-agent",
            expected_manifest=ROOT / "Cargo.toml",
        )
        self.assertTrue(any("expected only package" in error for error in errors))

    def test_wrong_manifest_or_workspace_members_are_rejected(self) -> None:
        manifest = ROOT / "spike" / "Cargo.toml"
        candidate = metadata("spike", manifest)
        candidate["workspace_members"] = []
        errors = BOUNDARIES.validate_metadata(
            candidate,
            expected_name="spike",
            expected_manifest=ROOT / "Cargo.toml",
        )
        self.assertTrue(any("expected only manifest" in error for error in errors))
        self.assertTrue(any("workspace members" in error for error in errors))


if __name__ == "__main__":
    unittest.main()
