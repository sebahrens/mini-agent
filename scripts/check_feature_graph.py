#!/usr/bin/env python3
"""Verify supported Cargo feature relationships and dependency activation."""

from __future__ import annotations

import json
import re
import shlex
import subprocess
import sys
import tomllib
from collections import Counter
from dataclasses import dataclass
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
OPTIONAL_PACKAGE_OWNERS = {
    "agent-client-protocol": "acp",
    "blocking": "acp",
    "fastembed": "skills-embed",
    "hnsw_rs": "skills",
    "lsp-types": "lsp",
    "matrixmultiply": "skills",
    "ort": "skills-embed",
    "rmcp": "mcp",
    "rquickjs": "js",
    "rusqlite": "skills",
}
OPTIONAL_PACKAGES = frozenset(OPTIONAL_PACKAGE_OWNERS)
JS_PACKAGES = frozenset({"rquickjs"})
SKILLS_PACKAGES = JS_PACKAGES | {
    "hnsw_rs",
    "matrixmultiply",
    "rusqlite",
}
ACP_PACKAGES = frozenset({"agent-client-protocol", "blocking"})
LSP_PACKAGES = frozenset({"lsp-types"})
EMBED_PACKAGES = SKILLS_PACKAGES | {"fastembed", "ort"}


@dataclass(frozen=True)
class FeatureRow:
    name: str
    features: str | None
    required: frozenset[str]
    forbidden: frozenset[str]


@dataclass(frozen=True)
class FeatureResolution:
    features: frozenset[str]
    packages: frozenset[str]
    dependency_features: frozenset[tuple[str, str]]


@dataclass(frozen=True)
class CargoInvocation:
    default_features: bool
    features: frozenset[str]


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
    row("acp", "acp", ACP_PACKAGES),
    row("lsp", "lsp", LSP_PACKAGES),
    row("js-skills", "js,skills", SKILLS_PACKAGES),
    row("skills-embed", "skills-embed", EMBED_PACKAGES),
    row("skills-embed-dynamic", "skills-embed-dynamic", EMBED_PACKAGES),
    row(
        "full",
        "mcp,js,sandbox,skills,memory",
        SKILLS_PACKAGES | {"rmcp"},
    ),
)

TEST_MATRIX_ROWS = (
    "default",
    "no-default",
    "memory",
    "js",
    "sandbox",
    "skills",
    "js-sandbox",
    "mcp",
    "js-skills",
    "full",
)
CLIPPY_MATRIX_ROWS = (
    "default",
    "no-default",
    "memory",
    "sandbox",
    "js-skills",
    "full",
)


def load_manifest(path: Path) -> dict[str, Any]:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def feature_row_by_name(name: str) -> FeatureRow:
    return next(feature_row for feature_row in FEATURE_ROWS if feature_row.name == name)


def cargo_args_for_row(name: str) -> str:
    if name == "default":
        return ""
    feature_row = feature_row_by_name(name)
    if feature_row.features is None:
        return "--no-default-features"
    return f"--no-default-features --features {feature_row.features}"


def expected_workflow_matrices() -> dict[str, list[str]]:
    return {
        "test": [cargo_args_for_row(name) for name in TEST_MATRIX_ROWS],
        "clippy": [cargo_args_for_row(name) for name in CLIPPY_MATRIX_ROWS],
    }


def parse_cargo_invocation(arguments: str) -> CargoInvocation:
    tokens = shlex.split(arguments)
    default_features = True
    features: set[str] = set()
    index = 0
    while index < len(tokens):
        token = tokens[index]
        if token == "--no-default-features":
            default_features = False
        elif token == "--features":
            index += 1
            if index >= len(tokens):
                raise ValueError("--features requires a value")
            features.update(filter(None, tokens[index].split(",")))
        elif token.startswith("--features="):
            features.update(filter(None, token.split("=", 1)[1].split(",")))
        else:
            raise ValueError(f"unsupported Cargo feature argument {token!r}")
        index += 1
    return CargoInvocation(default_features, frozenset(features))


def _yaml_string(value: str) -> str:
    value = value.strip()
    if value.startswith('"'):
        decoded = json.loads(value)
        if not isinstance(decoded, str):
            raise ValueError(f"matrix row must be a string: {value}")
        return decoded
    if value.startswith("'") and value.endswith("'"):
        return value[1:-1].replace("''", "'")
    return value


def workflow_matrix_values(text: str, job: str) -> list[str]:
    lines = text.splitlines()
    job_line = f"  {job}:"
    try:
        job_start = lines.index(job_line)
    except ValueError as error:
        raise ValueError(f"workflow job {job!r} is missing") from error

    job_end = len(lines)
    for index in range(job_start + 1, len(lines)):
        if re.fullmatch(r"  [A-Za-z0-9_-]+:", lines[index]):
            job_end = index
            break

    matrix_start = None
    for index in range(job_start + 1, job_end):
        if lines[index] == "        features:":
            matrix_start = index + 1
            break
    if matrix_start is None:
        raise ValueError(f"workflow job {job!r} has no feature matrix")

    values: list[str] = []
    for line in lines[matrix_start:job_end]:
        match = re.fullmatch(r"          - (.+)", line)
        if match:
            values.append(_yaml_string(match.group(1)))
        elif line.strip() and not line.lstrip().startswith("#"):
            break
    return values


def load_workflow_matrices(path: Path) -> dict[str, list[str]]:
    text = path.read_text(encoding="utf-8")
    return {
        job: workflow_matrix_values(text, job)
        for job in ("test", "clippy")
    }


def validate_workflow_matrices(matrices: dict[str, list[str]]) -> list[str]:
    errors: list[str] = []
    expected_names = {
        "test": TEST_MATRIX_ROWS,
        "clippy": CLIPPY_MATRIX_ROWS,
    }
    for job, names in expected_names.items():
        raw_rows = matrices.get(job)
        if raw_rows is None:
            errors.append(f"workflow is missing {job!r} feature matrix")
            continue
        try:
            observed = Counter(parse_cargo_invocation(row) for row in raw_rows)
        except ValueError as error:
            errors.append(f"{job} matrix has invalid Cargo arguments: {error}")
            continue

        expected = {
            name: parse_cargo_invocation(cargo_args_for_row(name))
            for name in names
        }
        expected_rows = set(expected.values())
        for name, invocation in expected.items():
            count = observed[invocation]
            if count == 0:
                errors.append(f"{job} matrix is missing focused row {name!r}")
            elif count > 1:
                errors.append(f"{job} matrix duplicates focused row {name!r}")
        for invocation in observed.keys() - expected_rows:
            errors.append(
                f"{job} matrix contains unsupported row "
                f"default_features={invocation.default_features}, "
                f"features={sorted(invocation.features)!r}"
            )
    return errors


def resolve_feature(features: dict[str, Any], root: str) -> FeatureResolution:
    feature_closure: set[str] = set()
    packages: set[str] = set()
    dependency_features: set[tuple[str, str]] = set()
    pending = [root]
    while pending:
        feature = pending.pop()
        if feature in feature_closure:
            continue
        feature_closure.add(feature)
        for member in features.get(feature, []):
            if not isinstance(member, str):
                continue
            if member in features:
                pending.append(member)
            elif member.startswith("dep:"):
                packages.add(member.removeprefix("dep:"))
            elif "/" in member:
                package, dependency_feature = member.split("/", 1)
                weak = package.endswith("?")
                package = package.removesuffix("?")
                dependency_features.add((package, dependency_feature))
                if not weak:
                    packages.add(package)
    return FeatureResolution(
        frozenset(feature_closure),
        frozenset(packages),
        frozenset(dependency_features),
    )


def validate_manifest(manifest: dict[str, Any]) -> list[str]:
    errors: list[str] = []
    features = manifest.get("features", {})
    dependencies = manifest.get("dependencies", {})

    default_features = set(features.get("default", []))
    for feature in ("js", "sandbox", "memory"):
        if feature not in default_features:
            errors.append(f"default features must include {feature!r}")

    required_feature_implications = {
        "skills": {"js"},
        "skills-embed": {"skills"},
        "skills-embed-dynamic": {"skills-embed"},
    }
    for owner, required_features in required_feature_implications.items():
        resolution = resolve_feature(features, owner)
        for required in sorted(required_features - resolution.features):
            errors.append(f"feature {owner!r} must imply feature {required!r}")

    if "js" in resolve_feature(features, "sandbox").features:
        errors.append("feature 'sandbox' must not imply 'js'")

    for package, owner in OPTIONAL_PACKAGE_OWNERS.items():
        if package not in resolve_feature(features, owner).packages:
            errors.append(
                f"feature {owner!r} must activate optional dependency {package!r}"
            )

    dynamic_resolution = resolve_feature(features, "skills-embed-dynamic")
    if ("ort", "load-dynamic") not in dynamic_resolution.dependency_features:
        errors.append(
            "feature 'skills-embed-dynamic' must enable dependency feature "
            "'ort/load-dynamic'"
        )

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
        errors.extend(
            validate_workflow_matrices(
                load_workflow_matrices(ROOT / ".github" / "workflows" / "ci.yml")
            )
        )
        packages_by_row = {
            feature_row.name: cargo_tree_packages(feature_row, ROOT)
            for feature_row in FEATURE_ROWS
        }
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"feature graph check failed: {error}", file=sys.stderr)
        return 1

    errors.extend(validate_activation(packages_by_row))
    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1

    print(
        "Cargo feature relationships, optional dependency activation, and CI "
        "matrices are valid."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
