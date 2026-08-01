#!/usr/bin/env python3
"""Verify supported Cargo feature relationships and dependency activation."""

from __future__ import annotations

import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
OPTIONAL_PACKAGES = frozenset(
    {
        "agent-client-protocol",
        "blocking",
        "fastembed",
        "hnsw_rs",
        "lsp-types",
        "matrixmultiply",
        "ort",
        "rmcp",
        "rquickjs",
        "rusqlite",
    }
)
JS_PACKAGES = frozenset({"rquickjs"})
SKILLS_PACKAGES = JS_PACKAGES | {
    "hnsw_rs",
    "matrixmultiply",
    "rusqlite",
}


@dataclass(frozen=True)
class FeatureRow:
    name: str
    features: str | None
    required: frozenset[str]
    forbidden: frozenset[str]


def row(name: str, features: str | None, required: frozenset[str]) -> FeatureRow:
    return FeatureRow(name, features, required, OPTIONAL_PACKAGES - required)


FEATURE_ROWS = (
    row("no-default", None, frozenset()),
    row("memory", "memory", frozenset()),
    row("js", "js", JS_PACKAGES),
    row("sandbox", "sandbox", frozenset()),
    row("skills", "skills", SKILLS_PACKAGES),
    row("js-sandbox", "js,sandbox", JS_PACKAGES),
    row("mcp", "mcp", frozenset({"rmcp"})),
    row("js-skills", "js,skills", SKILLS_PACKAGES),
    row(
        "full",
        "mcp,js,sandbox,skills,memory",
        SKILLS_PACKAGES | {"rmcp"},
    ),
)


def load_manifest(path: Path) -> dict[str, Any]:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def feature_closure(features: dict[str, Any], root: str) -> set[str]:
    closure: set[str] = set()
    pending = [root]
    while pending:
        feature = pending.pop()
        if feature in closure:
            continue
        closure.add(feature)
        for member in features.get(feature, []):
            if isinstance(member, str) and member in features:
                pending.append(member)
    return closure


def validate_manifest(manifest: dict[str, Any]) -> list[str]:
    errors: list[str] = []
    features = manifest.get("features", {})
    dependencies = manifest.get("dependencies", {})

    default_features = set(features.get("default", []))
    for feature in ("js", "sandbox", "memory"):
        if feature not in default_features:
            errors.append(f"default features must include {feature!r}")

    if "js" not in feature_closure(features, "skills"):
        errors.append("feature 'skills' must imply 'js'")
    if "js" in feature_closure(features, "sandbox"):
        errors.append("feature 'sandbox' must not imply 'js'")

    required_members = {
        "js": {"dep:rquickjs"},
        "skills": {
            "js",
            "dep:hnsw_rs",
            "dep:matrixmultiply",
            "dep:rusqlite",
        },
        "skills-embed": {"skills", "dep:fastembed", "dep:ort"},
        "mcp": {"dep:rmcp"},
    }
    for feature, required in required_members.items():
        members = set(features.get(feature, []))
        for missing in sorted(required - members):
            errors.append(f"feature {feature!r} must include {missing!r}")

    manifest_optional_packages = {
        package
        for package, dependency in dependencies.items()
        if isinstance(dependency, dict) and dependency.get("optional") is True
    }
    for package in sorted(manifest_optional_packages - OPTIONAL_PACKAGES):
        errors.append(
            f"optional dependency {package!r} is not covered by feature rows"
        )

    for package in sorted(OPTIONAL_PACKAGES):
        dependency = dependencies.get(package)
        if not isinstance(dependency, dict) or dependency.get("optional") is not True:
            errors.append(f"dependency {package!r} must remain optional")

    return errors


def cargo_tree_command(feature_row: FeatureRow) -> list[str]:
    command = [
        "cargo",
        "tree",
        "--locked",
        "--package",
        "mini-agent",
        "--no-default-features",
    ]
    if feature_row.features:
        command.extend(("--features", feature_row.features))
    command.extend(("--edges", "normal", "--prefix", "none", "--format", "{p}"))
    return command


def cargo_tree_packages(feature_row: FeatureRow, root: Path) -> set[str]:
    completed = subprocess.run(
        cargo_tree_command(feature_row),
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    )
    return {
        line.split(" ", 1)[0]
        for line in completed.stdout.splitlines()
        if line.strip()
    }


def expected_package_sets() -> dict[str, set[str]]:
    return {feature_row.name: set(feature_row.required) for feature_row in FEATURE_ROWS}


def validate_activation(packages_by_row: dict[str, set[str]]) -> list[str]:
    errors: list[str] = []
    for feature_row in FEATURE_ROWS:
        packages = packages_by_row.get(feature_row.name)
        if packages is None:
            errors.append(f"missing Cargo feature row {feature_row.name!r}")
            continue
        for package in sorted(feature_row.required - packages):
            errors.append(
                f"{feature_row.name} must activate optional package {package!r}"
            )
        for package in sorted(feature_row.forbidden & packages):
            errors.append(
                f"{feature_row.name} unexpectedly activates optional package {package!r}"
            )
    return errors


def main() -> int:
    errors = validate_manifest(load_manifest(ROOT / "Cargo.toml"))
    try:
        packages_by_row = {
            feature_row.name: cargo_tree_packages(feature_row, ROOT)
            for feature_row in FEATURE_ROWS
        }
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"feature graph check failed: {error}", file=sys.stderr)
        return 1

    errors.extend(validate_activation(packages_by_row))
    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1

    print("Cargo feature relationships and optional dependency activation are valid.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
