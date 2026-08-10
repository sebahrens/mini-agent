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
    "process-wrap": "mcp",
    "rmcp": "mcp",
    "rquickjs": "js",
    "rusqlite": "skills",
    "url": "lsp",
    "which": "lsp",
}
OPTIONAL_PACKAGES = frozenset(OPTIONAL_PACKAGE_OWNERS)
# These direct optional dependencies are also present transitively in the base
# graph, so `cargo tree` cannot prove whether their direct edge is active.
# `validate_manifest` still verifies their owning feature's semantic closure.
TRANSITIVELY_PRESENT_OPTIONAL_PACKAGES = frozenset({"url", "which"})
ACTIVATION_PACKAGES = OPTIONAL_PACKAGES - TRANSITIVELY_PRESENT_OPTIONAL_PACKAGES
JS_PACKAGES = frozenset({"rquickjs"})
SKILLS_PACKAGES = JS_PACKAGES | {
    "hnsw_rs",
    "matrixmultiply",
    "rusqlite",
}
ACP_PACKAGES = frozenset({"agent-client-protocol", "blocking"})
MCP_PACKAGES = frozenset({"process-wrap", "rmcp"})
LSP_PACKAGES = frozenset({"lsp-types", "process-wrap", "url", "which"})
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
    return FeatureRow(name, features, required, ACTIVATION_PACKAGES - required)


FEATURE_ROWS = (
    row("no-default", None, frozenset()),
    row("memory", "memory", frozenset()),
    row("js", "js", JS_PACKAGES),
    row("sandbox", "sandbox", frozenset()),
    row("skills", "skills", SKILLS_PACKAGES),
    row("js-sandbox", "js,sandbox", JS_PACKAGES),
    row("mcp", "mcp", MCP_PACKAGES),
    row("acp", "acp", ACP_PACKAGES),
    row("lsp", "lsp", LSP_PACKAGES),
    row("js-skills", "js,skills", SKILLS_PACKAGES),
    row("skills-embed", "skills-embed", EMBED_PACKAGES),
    row("skills-embed-dynamic", "skills-embed-dynamic", EMBED_PACKAGES),
    row(
        "full",
        "mcp,js,sandbox,skills,memory",
        SKILLS_PACKAGES | MCP_PACKAGES,
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
    "acp",
    "js-skills",
    "full",
)
CLIPPY_MATRIX_ROWS = (
    "default",
    "no-default",
    "memory",
    "sandbox",
    "acp",
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
    value = _strip_yaml_comment(value).strip()
    if not value:
        raise ValueError("matrix row must be a non-empty YAML string")
    if value.startswith('"'):
        try:
            decoded = json.loads(value)
        except json.JSONDecodeError as error:
            raise ValueError(f"invalid double-quoted YAML scalar: {value}") from error
        if not isinstance(decoded, str):
            raise ValueError(f"matrix row must be a string: {value}")
        return decoded
    if value.startswith("'"):
        if not re.fullmatch(r"'(?:[^']|'')*'", value):
            raise ValueError(f"invalid single-quoted YAML scalar: {value}")
        return value[1:-1].replace("''", "'")
    return value


def _strip_yaml_comment(value: str) -> str:
    quote: str | None = None
    index = 0
    while index < len(value):
        character = value[index]
        if quote == "'":
            if character == "'":
                if index + 1 < len(value) and value[index + 1] == "'":
                    index += 2
                    continue
                quote = None
        elif quote == '"':
            if character == "\\":
                index += 2
                continue
            if character == '"':
                quote = None
        elif character in {'"', "'"}:
            quote = character
        elif character == "#" and (index == 0 or value[index - 1].isspace()):
            return value[:index].rstrip()
        index += 1
    if quote is not None:
        raise ValueError("unterminated quoted scalar in feature matrix")
    return value.rstrip()


def _mapping_entry(line: str) -> tuple[int, str] | None:
    match = re.fullmatch(r"( *)([A-Za-z0-9_-]+):(?: *#.*)?", line)
    if not match:
        return None
    return len(match.group(1)), match.group(2)


def _child_section(
    lines: list[str],
    key: str,
    start: int,
    end: int,
    parent_indent: int,
) -> tuple[int, int, int]:
    entries = [
        (index, entry[0], entry[1])
        for index in range(start, end)
        if (entry := _mapping_entry(lines[index])) is not None
        and entry[0] > parent_indent
    ]
    if not entries:
        raise ValueError(f"workflow mapping {key!r} is missing")
    child_indent = min(indent for _, indent, _ in entries)
    try:
        section_start = next(
            index
            for index, indent, name in entries
            if indent == child_indent and name == key
        )
    except StopIteration as error:
        raise ValueError(f"workflow mapping {key!r} is missing") from error

    section_end = end
    for index in range(section_start + 1, end):
        stripped = lines[index].strip()
        if not stripped or stripped.startswith("#"):
            continue
        indent = len(lines[index]) - len(lines[index].lstrip(" "))
        if indent <= child_indent:
            section_end = index
            break
    return section_start, section_end, child_indent


def _workflow_job_section(
    lines: list[str], job: str
) -> tuple[int, int, int]:
    jobs_start, jobs_end, jobs_indent = _child_section(
        lines, "jobs", 0, len(lines), -1
    )
    return _child_section(lines, job, jobs_start + 1, jobs_end, jobs_indent)


def workflow_matrix_values(text: str, job: str) -> list[str]:
    lines = text.splitlines()
    job_start, job_end, job_indent = _workflow_job_section(lines, job)
    strategy_start, strategy_end, strategy_indent = _child_section(
        lines, "strategy", job_start + 1, job_end, job_indent
    )
    matrix_start, matrix_end, matrix_indent = _child_section(
        lines,
        "matrix",
        strategy_start + 1,
        strategy_end,
        strategy_indent,
    )
    features_start, features_end, features_indent = _child_section(
        lines, "features", matrix_start + 1, matrix_end, matrix_indent
    )

    values: list[str] = []
    item_indent: int | None = None
    for line in lines[features_start + 1 : features_end]:
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        match = re.fullmatch(r"( *)-\s+(.+)", line)
        if not match or len(match.group(1)) <= features_indent:
            raise ValueError(f"{job} feature matrix must be a flat string sequence")
        current_indent = len(match.group(1))
        if item_indent is None:
            item_indent = current_indent
        elif current_indent != item_indent:
            raise ValueError(f"{job} feature matrix has inconsistent indentation")
        values.append(_yaml_string(match.group(2)))
    return values


def _workflow_run_commands(text: str, job: str) -> list[str]:
    lines = text.splitlines()
    job_start, job_end, _ = _workflow_job_section(lines, job)
    commands: list[str] = []
    index = job_start + 1
    while index < job_end:
        match = re.fullmatch(r"( *)run:\s*(.*)", lines[index])
        if not match:
            index += 1
            continue
        run_indent = len(match.group(1))
        value = match.group(2)
        header = _strip_yaml_comment(value).strip()
        if re.fullmatch(r"[>|][+-]?", header):
            block: list[str] = []
            index += 1
            while index < job_end:
                line = lines[index]
                if line.strip():
                    indent = len(line) - len(line.lstrip(" "))
                    if indent <= run_indent:
                        break
                block.append(line)
                index += 1
            commands.append("\n".join(block))
            continue
        commands.append(_yaml_run_command(value))
        index += 1
    return commands


def _yaml_run_command(value: str) -> str:
    value = _strip_yaml_comment(value).strip()
    if not value:
        return ""
    if value.startswith('"'):
        try:
            decoded = json.loads(value)
        except json.JSONDecodeError as error:
            raise ValueError(f"invalid double-quoted run command: {value}") from error
        if not isinstance(decoded, str):
            raise ValueError(f"run command must be a string: {value}")
        return decoded
    if value.startswith("'"):
        if not re.fullmatch(r"'(?:[^']|'')*'", value):
            raise ValueError(f"invalid single-quoted run command: {value}")
        return value[1:-1].replace("''", "'")
    return value


def _shell_command_segments(command: str) -> list[str]:
    segments: list[str] = []
    current: list[str] = []
    quote: str | None = None
    index = 0

    def finish_segment() -> None:
        segment = "".join(current).strip()
        if segment:
            segments.append(segment)
        current.clear()

    while index < len(command):
        character = command[index]
        if quote == "'":
            current.append(character)
            if character == "'":
                quote = None
            index += 1
            continue
        if quote == '"':
            current.append(character)
            if character == "\\" and index + 1 < len(command):
                current.append(command[index + 1])
                index += 2
                continue
            if character == '"':
                quote = None
            index += 1
            continue
        if character in {'"', "'"}:
            quote = character
            current.append(character)
            index += 1
            continue
        if character == "\\" and index + 1 < len(command):
            if command[index + 1] == "\n":
                current.append(" ")
            else:
                current.extend((character, command[index + 1]))
            index += 2
            continue
        if character == "#" and (
            index == 0
            or command[index - 1].isspace()
            or command[index - 1] in ";|&<>()"
        ):
            while index < len(command) and command[index] != "\n":
                index += 1
            finish_segment()
            continue
        if character == "\n" or character in ";|&<>()":
            finish_segment()
            index += 1
            if index < len(command) and command[index] == character:
                index += 1
            continue
        current.append(character)
        index += 1

    if quote is not None:
        raise ValueError("unterminated shell quote in workflow run command")
    finish_segment()
    return segments


def _cargo_invocation_consumes_matrix(
    invocation: str,
    subcommand: str,
    interpolation: str,
) -> bool:
    if interpolation not in invocation:
        return False
    sentinel = "__CI_MATRIX_FEATURES_ARGUMENT__"
    while sentinel in invocation:
        sentinel += "_"
    try:
        arguments = shlex.split(invocation.replace(interpolation, sentinel))
    except ValueError:
        return False
    return (
        len(arguments) >= 3
        and arguments[0] == "cargo"
        and arguments[1] == subcommand
        and sentinel in arguments[2:]
    )


def validate_workflow_commands(text: str) -> list[str]:
    errors: list[str] = []
    interpolation = "${{ matrix.features }}"
    for job, subcommand in (("test", "test"), ("clippy", "clippy")):
        commands = _workflow_run_commands(text, job)
        invocations = [
            invocation
            for command in commands
            for invocation in _shell_command_segments(command)
        ]
        if not any(
            _cargo_invocation_consumes_matrix(
                invocation,
                subcommand,
                interpolation,
            )
            for invocation in invocations
        ):
            errors.append(
                f"{job} job Cargo command must consume {interpolation!r}"
            )
    return errors


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
        workflow_path = ROOT / ".github" / "workflows" / "ci.yml"
        workflow_text = workflow_path.read_text(encoding="utf-8")
        errors.extend(
            validate_workflow_matrices(
                {
                    job: workflow_matrix_values(workflow_text, job)
                    for job in ("test", "clippy")
                }
            )
        )
        errors.extend(validate_workflow_commands(workflow_text))
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
