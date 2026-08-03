#!/usr/bin/env python3
"""Fail closed if the standalone research spike rejoins production metadata."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
SPIKE = ROOT / "spike"


def cargo_metadata(directory: Path) -> dict[str, Any]:
    result = subprocess.run(
        [
            "cargo",
            "metadata",
            "--locked",
            "--no-deps",
            "--format-version",
            "1",
        ],
        cwd=directory,
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


def validate_metadata(
    metadata: dict[str, Any], *, expected_name: str, expected_manifest: Path
) -> list[str]:
    errors: list[str] = []
    packages = metadata.get("packages")
    if not isinstance(packages, list):
        return ["Cargo metadata packages must be a list"]

    names = [package.get("name") for package in packages]
    if names != [expected_name]:
        errors.append(
            f"expected only package {expected_name!r}, found {names!r}"
        )

    manifests = [
        Path(package.get("manifest_path", "")).resolve()
        for package in packages
    ]
    if manifests != [expected_manifest.resolve()]:
        errors.append(
            f"expected only manifest {str(expected_manifest)!r}, "
            f"found {[str(path) for path in manifests]!r}"
        )

    package_ids = {
        package.get("id") for package in packages if isinstance(package, dict)
    }
    workspace_members = metadata.get("workspace_members")
    if not isinstance(workspace_members, list) or set(workspace_members) != package_ids:
        errors.append(
            "workspace members must exactly match the single expected package"
        )
    return errors


def validate_boundaries(root: Path = ROOT) -> list[str]:
    spike = root / "spike"
    root_metadata = cargo_metadata(root)
    spike_metadata = cargo_metadata(spike)
    errors = [
        f"root workspace: {error}"
        for error in validate_metadata(
            root_metadata,
            expected_name="mini-agent",
            expected_manifest=root / "Cargo.toml",
        )
    ]
    errors.extend(
        f"spike workspace: {error}"
        for error in validate_metadata(
            spike_metadata,
            expected_name="spike",
            expected_manifest=spike / "Cargo.toml",
        )
    )
    return errors


def main() -> int:
    try:
        errors = validate_boundaries()
    except (OSError, subprocess.CalledProcessError, json.JSONDecodeError) as error:
        print(f"workspace boundary check failed: {error}", file=sys.stderr)
        return 1
    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1
    print("workspace boundaries valid: root=mini-agent, spike=spike")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
