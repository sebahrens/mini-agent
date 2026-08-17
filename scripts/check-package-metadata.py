#!/usr/bin/env python3
"""Validate that release and package metadata use Cargo's canonical binary."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
import tomllib
from fnmatch import fnmatchcase
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
RELEASE_TAG_PATTERN = re.compile(
    r"^v(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)"
    r"(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"
)
FULL_COMMIT_SHA = re.compile(r"^[0-9a-f]{40}$")
VERSION_COMMENT = re.compile(r"^v\d+(?:\.\d+){0,2}(?:[-+][0-9A-Za-z.-]+)?$")
# Remote actions may only bypass immutable pins after an explicit, reviewed entry here.
RELEASE_ACTION_PIN_ALLOWLIST: frozenset[str] = frozenset()
CANONICAL_GPL3_LICENSE_SHA256 = (
    "3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986"
)
APPROVED_RELEASE_ACTIONS = {
    ("actions/checkout", "v7.0.1"): "3d3c42e5aac5ba805825da76410c181273ba90b1",
    (
        "actions-rust-lang/setup-rust-toolchain",
        "v1.17.0",
    ): "166cdcfd11aee3cb47222f9ddb555ce30ddb9659",
    (
        "taiki-e/install-action",
        "v2.85.9",
    ): "91ddec75689c4c78665b598d188dc821c5a43e5c",
    (
        "actions/upload-artifact",
        "v7.0.1",
    ): "043fb46d1a93c77aae656e7c1c64a875d1fc6a0a",
    (
        "actions/download-artifact",
        "v4.3.0",
    ): "d3f86a106a0bac45b974a628896c90dbdf5c8093",
    (
        "actions/setup-node",
        "v4.4.0",
    ): "49933ea5288caeca8642d1e84afbd3f7d6820020",
}
DISTRIBUTION_NOTICE_FRAGMENTS: dict[str, tuple[str, ...]] = {
    "packaging/homebrew/zerostack.rb": (
        'pkgshare.install "LICENSE", "NOTICE", "SOURCE.md"',
    ),
    "packaging/aur/PKGBUILD": (
        'install -Dm644 NOTICE "${pkgdir}/usr/share/doc/${pkgname}/NOTICE"',
        'install -Dm644 SOURCE.md "${pkgdir}/usr/share/doc/${pkgname}/SOURCE.md"',
    ),
    "packaging/conda/zerostack-bin/build.sh": (
        'install -Dm644 "${SRC_DIR}/NOTICE" "${PREFIX}/share/doc/${PKG_NAME}/NOTICE"',
        'install -Dm644 "${SRC_DIR}/SOURCE.md" "${PREFIX}/share/doc/${PKG_NAME}/SOURCE.md"',
    ),
    "packaging/conda/zerostack/build.sh": (
        'install -Dm644 NOTICE "${PREFIX}/share/doc/${PKG_NAME}/NOTICE"',
        'install -Dm644 SOURCE.md "${PREFIX}/share/doc/${PKG_NAME}/SOURCE.md"',
    ),
}
USES_ENTRY = re.compile(
    r"(?P<quote>['\"]?)(?P<reference>[^\s#'\"]+)(?P=quote)"
    r"(?:\s+#\s*(?P<version>\S+))?"
)
RUBY_YAML_TO_JSON = """
require "json"
require "yaml"
document = YAML.safe_load(STDIN.read, aliases: false)
STDOUT.write(JSON.generate(document))
"""
RUBY_YAML_USES_TO_JSON = """
require "json"
require "psych"
entries = []
errors = []
walk = lambda do |node|
  if node.is_a?(Psych::Nodes::Mapping)
    node.children.each_slice(2) do |key, value|
      if key.is_a?(Psych::Nodes::Scalar) && key.value == "uses"
        if value.is_a?(Psych::Nodes::Scalar)
          entries << {"reference" => value.value, "line" => key.start_line + 1}
        else
          errors << "release workflow has a non-string uses value"
        end
      end
      walk.call(key)
      walk.call(value)
    end
  elsif node.respond_to?(:children) && node.children
    node.children.each { |child| walk.call(child) }
  end
end
walk.call(Psych.parse_stream(STDIN.read))
STDOUT.write(JSON.generate({"entries" => entries, "errors" => errors}))
"""


def parse_yaml_document(text: str) -> Any:
    """Parse YAML with Ruby's standard Psych parser and return JSON-compatible data."""

    result = subprocess.run(
        ["ruby", "--disable-gems", "-e", RUBY_YAML_TO_JSON],
        input=text,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        detail = result.stderr.strip().splitlines()
        message = detail[-1] if detail else "unknown parser failure"
        raise ValueError(f"YAML parsing failed: {message}")
    return json.loads(result.stdout)


def parse_yaml_uses_entries(text: str) -> tuple[list[dict[str, Any]], list[str]]:
    """Collect every uses node with its exact one-based YAML source line."""

    result = subprocess.run(
        ["ruby", "--disable-gems", "-e", RUBY_YAML_USES_TO_JSON],
        input=text,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        detail = result.stderr.strip().splitlines()
        message = detail[-1] if detail else "unknown parser failure"
        raise ValueError(f"YAML source parsing failed: {message}")
    parsed = json.loads(result.stdout)
    if not isinstance(parsed, dict):
        raise ValueError("YAML source parser returned an invalid document")
    entries = parsed.get("entries")
    errors = parsed.get("errors")
    if not isinstance(entries, list) or not isinstance(errors, list):
        raise ValueError("YAML source parser returned invalid uses metadata")
    return entries, errors


CANONICAL_REPOSITORY = "sebahrens/mini-agent"
CANONICAL_REPOSITORY_URL = f"https://github.com/{CANONICAL_REPOSITORY}"
LEGACY_COORDINATES = (
    ("gi-" + "dellav/zerostack").casefold(),
    ("gi-" + "dellav.github.io/zerostack").casefold(),
)
HISTORICAL_COORDINATE_ALLOWLIST = (
    "docs/specs/superseded/",
)
UPSTREAM_PROVENANCE_FILES = frozenset({"README.md", "NOTICE"})
SUPPORTED_PACKAGE_CHANNELS = ("cargo", "aur", "conda", "homebrew")
REMOVED_NIX_ENTRYPOINTS = (
    "default.nix",
    "release.nix",
    "shell.nix",
    "nix/overlay/default.nix",
    "nix/overlay/development.nix",
    "nix/package/dev-shell.nix",
    "nix/package/zerostack.nix",
)
EXPECTED_RELEASE_ARCHIVES = (
    "mini-agent-x86_64-unknown-linux-gnu.tar.gz",
    "mini-agent-aarch64-unknown-linux-gnu.tar.gz",
    "mini-agent-x86_64-apple-darwin.tar.gz",
    "mini-agent-aarch64-apple-darwin.tar.gz",
    "mini-agent-x86_64-unknown-linux-musl.tar.gz",
    "mini-agent-aarch64-unknown-linux-musl.tar.gz",
    "mini-agent-x86_64-pc-windows-msvc.tar.gz",
    "mini-agent-lite-x86_64-unknown-linux-gnu.tar.gz",
    "mini-agent-lite-aarch64-unknown-linux-gnu.tar.gz",
    "mini-agent-lite-x86_64-apple-darwin.tar.gz",
    "mini-agent-lite-aarch64-apple-darwin.tar.gz",
    "mini-agent-lite-x86_64-unknown-linux-musl.tar.gz",
    "mini-agent-lite-aarch64-unknown-linux-musl.tar.gz",
    "mini-agent-lite-x86_64-pc-windows-msvc.tar.gz",
    "mini-agent-${GITHUB_REF_NAME}-source.tar.gz",
)
EXPECTED_VSCODE_RELEASE_ARTIFACTS = (
    "VSIX_SHA256SUMS",
    "MSI_SHA256SUMS",
    "mini-agent-windows-x64.msi",
    "mini-agent-1.8.0-linux-x64.vsix",
    "mini-agent-1.8.0-linux-arm64.vsix",
    "mini-agent-1.8.0-darwin-x64.vsix",
    "mini-agent-1.8.0-darwin-arm64.vsix",
    "mini-agent-1.8.0-win32-x64.vsix",
    "mini-agent-1.8.0-linux-x64.cdx.json",
    "mini-agent-1.8.0-linux-arm64.cdx.json",
    "mini-agent-1.8.0-darwin-x64.cdx.json",
    "mini-agent-1.8.0-darwin-arm64.cdx.json",
    "mini-agent-1.8.0-win32-x64.cdx.json",
)
EXPECTED_CROSS_IMAGES = {
    "aarch64-unknown-linux-musl": (
        "ghcr.io/cross-rs/aarch64-unknown-linux-musl@"
        "sha256:35b37736695ca86f2725c008f097195d4b954e3604c549e3bbd03dadc70ea790"
    ),
    "x86_64-unknown-linux-musl": (
        "ghcr.io/cross-rs/x86_64-unknown-linux-musl@"
        "sha256:a3942dd42a4de523dc77977b15f7bfc9007c242fe84bfbc555007bdb16703b61"
    ),
}
EXPECTED_ARCHIVE_ARRAY = re.compile(
    r"^\s*expected=\(\n(?P<body>(?:\s+[^\n]+\n)+?)\s*\)$", re.MULTILINE
)


def cargo_metadata(root: Path) -> dict[str, Any]:
    result = subprocess.run(
        [
            "cargo",
            "metadata",
            "--locked",
            "--no-deps",
            "--format-version",
            "1",
        ],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


def validate_clean_tracked_worktree(root: Path) -> list[str]:
    """Require tracked index and worktree content to match the tagged commit."""
    errors: list[str] = []
    for command, label in (
        (["git", "diff", "--quiet", "--"], "working tree"),
        (["git", "diff", "--cached", "--quiet", "--"], "index"),
    ):
        try:
            result = subprocess.run(command, cwd=root, check=False)
        except OSError as error:
            errors.append(f"could not inspect tracked release {label}: {error}")
            continue
        if result.returncode == 1:
            errors.append(
                f"tracked release {label} is dirty; commit or restore changes "
                "before creating a tag"
            )
        elif result.returncode != 0:
            errors.append(
                f"could not inspect tracked release {label}: git exited "
                f"with status {result.returncode}"
            )
    return errors


def validate_license_identity(root: Path) -> list[str]:
    license_path = root / "LICENSE"
    if not license_path.is_file():
        return ["LICENSE is missing"]
    if hashlib.sha256(license_path.read_bytes()).hexdigest() != CANONICAL_GPL3_LICENSE_SHA256:
        return ["LICENSE is not the canonical GPL-3.0-only text"]
    return []


def canonical_binary(metadata: dict[str, Any], root: Path) -> tuple[str | None, list[str]]:
    manifest = (root / "Cargo.toml").resolve()
    package = next(
        (
            candidate
            for candidate in metadata.get("packages", [])
            if Path(candidate["manifest_path"]).resolve() == manifest
        ),
        None,
    )
    if package is None:
        return None, [f"Cargo metadata does not contain root package {manifest}"]

    binary = (
        package.get("metadata", {})
        .get("release", {})
        .get("canonical-binary")
    )
    if not isinstance(binary, str) or not binary:
        return None, [
            "Cargo.toml must define package.metadata.release.canonical-binary"
        ]

    binary_targets = {
        target["name"]
        for target in package.get("targets", [])
        if "bin" in target.get("kind", [])
    }
    if binary not in binary_targets:
        return binary, [
            f"canonical binary {binary!r} is not a Cargo binary target: "
            f"{sorted(binary_targets)}"
        ]
    return binary, []


def validate_workflow(text: str, binary: str) -> list[str]:
    errors: list[str] = []
    if not re.search(
        r'^on:\n  push:\n    tags:\n      - ["\']v\*["\']$',
        text,
        re.MULTILINE,
    ):
        errors.append(
            ".github/workflows/release.yml must trigger on v* tag pushes"
        )
    if re.search(r"^\s*workflow_dispatch\s*:", text, re.MULTILINE):
        errors.append(
            ".github/workflows/release.yml must not allow manual dispatch"
        )

    package_metadata_start = text.find("\n  package-metadata:")
    build_start = text.find("\n  build:", package_metadata_start + 1)
    package_metadata_job = (
        text[package_metadata_start:build_start]
        if package_metadata_start >= 0 and build_start > package_metadata_start
        else ""
    )
    release_identity_fragments = (
        "RELEASE_REF_TYPE: ${{ github.ref_type }}",
        "RELEASE_TAG: ${{ github.ref_name }}",
        '--ref-type "$RELEASE_REF_TYPE"',
        '--release-tag "$RELEASE_TAG"',
    )
    if not all(
        fragment in package_metadata_job
        for fragment in release_identity_fragments
    ):
        errors.append(
            ".github/workflows/release.yml package-metadata job must validate "
            "release identity before builds"
        )
    required_counts = {
        f"CANONICAL_BINARY: {binary}": 1,
        'RUSTFLAGS: ""': 1,
        "tool: cross@0.2.5": 1,
        "run: cargo build --release --target ${{ matrix.target }}": 2,
        "run: cross build --release --target ${{ matrix.target }}": 1,
        # 3 jobs produce archives: build (Linux/macOS), build-static (musl), build-windows
        'archive="${CANONICAL_BINARY}-${{ matrix.target }}.tar.gz"': 3,
        'archive="${CANONICAL_BINARY}-lite-${{ matrix.target }}.tar.gz"': 3,
        # All binary archives use the fail-closed GPL payload packager.
        "python3 scripts/package-release-binary.py \\": 6,
        '--executable-name "$CANONICAL_BINARY"': 4,
        '--executable-name "${CANONICAL_BINARY}.exe"': 2,
        '"$smoke_dir/$CANONICAL_BINARY" --version | grep -Fq -- '
        '"$CANONICAL_BINARY "': 4,
        'file "$smoke_dir/$CANONICAL_BINARY" | grep -Fq -- "ARM aarch64"': 2,
        'readelf -l "$smoke_dir/$CANONICAL_BINARY" > '
        '"$smoke_dir/program-headers"': 2,
        'if grep -Fq -- "INTERP" "$smoke_dir/program-headers"; then': 2,
    }
    for fragment, expected_count in required_counts.items():
        observed_count = text.count(fragment)
        if observed_count != expected_count:
            errors.append(
                ".github/workflows/release.yml must contain "
                f"{fragment!r} exactly {expected_count} time(s), found "
                f"{observed_count}"
            )

    corresponding_source_fragments = (
        "corresponding-source:",
        'bash scripts/package-corresponding-source.sh "$GITHUB_REF_NAME" . HEAD',
        "name: corresponding-source",
    )
    missing_source_fragments = [
        fragment for fragment in corresponding_source_fragments if fragment not in text
    ]
    if missing_source_fragments:
        errors.append(
            ".github/workflows/release.yml must build and validate vendored "
            f"Corresponding Source; missing={missing_source_fragments}"
        )

    static_start = text.find("\n  build-static:")
    static_end = text.find("\n  build-windows:", static_start + 1)
    static_job = (
        text[static_start:static_end]
        if static_start >= 0 and static_end > static_start
        else ""
    )
    if (
        static_job.count("          - os: ubuntu-latest") != 2
        or "ubuntu-24.04-arm" in static_job
    ):
        errors.append(
            ".github/workflows/release.yml musl builds must run cross from "
            "ubuntu-latest x86_64 hosts"
        )

    if "build --release --all-features" in text:
        errors.append(
            ".github/workflows/release.yml full archives must use the "
            "supported default feature set, not --all-features"
        )
    forbidden = (
        "target/${{ matrix.target }}/release/zerostack",
        "zerostack-${{ matrix.target }}.tar.gz",
        "zerostack-lite-${{ matrix.target }}.tar.gz",
    )
    for fragment in forbidden:
        if fragment in text:
            errors.append(
                ".github/workflows/release.yml must not reference "
                f"noncanonical binary path {fragment!r}"
            )
    errors.extend(validate_release_archive_gates(text))
    errors.extend(validate_release_action_pins(text))
    return errors


def validate_release_archive_gates(text: str) -> list[str]:
    """Require exact, duplicate-free archive sets before checksum and release."""
    matches = list(EXPECTED_ARCHIVE_ARRAY.finditer(text))
    if len(matches) != 2:
        return [
            ".github/workflows/release.yml must contain exactly two expected "
            f"archive gates, found {len(matches)}"
        ]

    expected_archives = set(EXPECTED_RELEASE_ARCHIVES)
    gates = (
        ("checksum", expected_archives),
        (
            "publication",
            expected_archives
            | {"SHA256SUMS"}
            | set(EXPECTED_VSCODE_RELEASE_ARTIFACTS),
        ),
    )
    errors: list[str] = []
    for match, (gate_name, expected) in zip(matches, gates, strict=True):
        entries = [line.strip() for line in match.group("body").splitlines()]
        observed = set(entries)
        duplicates = sorted(entry for entry in observed if entries.count(entry) > 1)
        if duplicates:
            errors.append(
                f"release {gate_name} gate contains duplicate entries: {duplicates}"
            )
        missing = sorted(expected - observed)
        unexpected = sorted(observed - expected)
        if missing or unexpected:
            errors.append(
                f"release {gate_name} gate has an invalid archive set; "
                f"missing={missing}, unexpected={unexpected}"
            )
    return errors


def validate_cross_images(text: str) -> list[str]:
    """Require reviewed, immutable cross images for static release targets."""
    try:
        document = tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        return [f"Cross.toml is invalid TOML: {error}"]
    targets = document.get("target", {})
    errors: list[str] = []
    for target, image in EXPECTED_CROSS_IMAGES.items():
        target_config = targets.get(target, {})
        observed = target_config.get("image")
        if observed != image:
            errors.append(
                f"Cross.toml must pin {target} to reviewed image {image!r} "
                f"but found {observed!r}"
            )
    return errors


def validate_release_identity(
    *, version: str, ref_type: str, release_tag: str
) -> list[str]:
    """Require a canonical release tag for the Cargo package version."""
    errors: list[str] = []
    if ref_type != "tag":
        errors.append(
            f"release identity requires a tag ref, got {ref_type!r}"
        )
    if not RELEASE_TAG_PATTERN.fullmatch(release_tag):
        errors.append(
            f"release identity requires a valid release tag, got {release_tag!r}"
        )
    expected_tag = f"v{version}"
    if release_tag != expected_tag:
        errors.append(
            f"release tag {release_tag!r} does not match Cargo package version "
            f"{version!r} (expected {expected_tag!r})"
        )
    return errors


def validate_release_action_pins(
    text: str,
    *,
    allowlist: frozenset[str] = RELEASE_ACTION_PIN_ALLOWLIST,
) -> list[str]:
    """Require immutable SHAs and visible versions for release dependencies."""

    try:
        # Reject aliases and unsafe YAML types, then use the AST for exact source locations.
        parse_yaml_document(text)
        parsed_entries, errors = parse_yaml_uses_entries(text)
    except (FileNotFoundError, json.JSONDecodeError, ValueError) as error:
        return [f"release workflow cannot be validated: {error}"]

    canonical_entries: dict[int, str] = {}
    for line_number, line in enumerate(text.splitlines(), start=1):
        key_match = re.match(
            r"^\s*(?:-\s+)?(?:uses|['\"]uses['\"])\s*:\s*(.*?)\s*$", line
        )
        if key_match is None:
            continue

        entry_match = USES_ENTRY.fullmatch(key_match.group(1))
        if entry_match is None:
            errors.append(
                f"release workflow line {line_number} has malformed uses entry"
            )
            continue

        reference = entry_match.group("reference")
        version = entry_match.group("version")
        canonical_entries[line_number] = reference
        if reference in allowlist:
            continue

        action, separator, revision = reference.rpartition("@")
        if not separator:
            errors.append(
                f"release workflow line {line_number} has malformed action reference "
                f"{reference!r}"
            )
            continue
        if action in allowlist:
            continue
        revision_is_sha = FULL_COMMIT_SHA.fullmatch(revision) is not None
        if not revision_is_sha:
            errors.append(
                f"release workflow line {line_number} must pin {action!r} to a full "
                "40-character lowercase commit SHA"
            )
        version_is_valid = (
            version is not None and VERSION_COMMENT.fullmatch(version) is not None
        )
        if not version_is_valid:
            errors.append(
                f"release workflow line {line_number} must give {action!r} a version "
                "comment such as '# v4.6.2'"
            )
        if revision_is_sha and version_is_valid:
            approved = APPROVED_RELEASE_ACTIONS.get((action, version))
            if approved is None:
                errors.append(
                    f"release workflow line {line_number} uses unapproved action/version "
                    f"pair {action}@{version}"
                )
            elif revision != approved:
                errors.append(
                    f"release workflow line {line_number} SHA for {action}@{version} "
                    "does not match the reviewed approval map"
                )

    parsed_lines: set[int] = set()
    for entry in parsed_entries:
        if (
            not isinstance(entry, dict)
            or not isinstance(entry.get("reference"), str)
            or type(entry.get("line")) is not int
        ):
            errors.append("release workflow parser returned invalid uses source metadata")
            continue
        reference = entry["reference"]
        line_number = entry["line"]
        parsed_lines.add(line_number)
        if canonical_entries.get(line_number) != reference:
            errors.append(
                f"release workflow line {line_number} uses {reference!r} without a "
                "canonical block-style source line and version comment"
            )
    for line_number, reference in canonical_entries.items():
        if line_number not in parsed_lines:
            errors.append(
                f"release workflow line {line_number} text for {reference!r} is not an "
                "executable YAML uses node"
            )
    return errors


def validate_github_actions_updates(text: str) -> list[str]:
    """Keep Dependabot able to move action SHAs and version comments together."""

    try:
        document = parse_yaml_document(text)
    except (FileNotFoundError, json.JSONDecodeError, ValueError) as error:
        return [f".github/dependabot.yml cannot be validated: {error}"]
    if not isinstance(document, dict) or not isinstance(document.get("updates"), list):
        return [".github/dependabot.yml must contain an updates list"]

    for entry in document["updates"]:
        if not isinstance(entry, dict) or entry.get("package-ecosystem") != "github-actions":
            continue
        schedule = entry.get("schedule")
        limit = entry.get("open-pull-requests-limit", 5)
        ignores = entry.get("ignore", [])
        approved_actions = {action for action, _version in APPROVED_RELEASE_ACTIONS}
        ignored_actions: set[str] = set()
        if isinstance(ignores, list):
            for rule in ignores:
                if not isinstance(rule, dict):
                    continue
                pattern = rule.get("dependency-name")
                if isinstance(pattern, str):
                    ignored_actions.update(
                        action
                        for action in approved_actions
                        if fnmatchcase(action, pattern)
                    )
        ignores_all = approved_actions <= ignored_actions
        if (
            entry.get("directory") == "/"
            and isinstance(schedule, dict)
            and schedule.get("interval") in {"daily", "weekly", "monthly"}
            and type(limit) is int
            and limit > 0
            and not ignores_all
        ):
            return []
    return [
        ".github/dependabot.yml must schedule nonzero, non-ignored github-actions "
        "updates at repository directory /"
    ]


def validate_file_fragments(root: Path, binary: str) -> list[str]:
    required: dict[str, tuple[str, ...]] = {
        "Cargo.toml": (
            f'name = "{binary}"',
            f'repository = "{CANONICAL_REPOSITORY_URL}"',
            f'homepage = "{CANONICAL_REPOSITORY_URL}"',
            "[target.'cfg(target_env = \"musl\")'.dependencies]",
            'openssl = { version = "0.10", features = ["vendored"] }',
        ),
        "src/cli.rs": (f'#[command(name = "{binary}"',),
        "install.sh": (
            f'BINARY_NAME="{binary}"',
            f'REPO="{CANONICAL_REPOSITORY}"',
            'if [[ ! -f "${TMPDIR}/${BINARY_NAME}" ]]',
            'cp "${TMPDIR}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"',
            'REQUIRED_DOCUMENTS=("LICENSE" "NOTICE" "SOURCE.md")',
            'cp "${TMPDIR}/${document}" "${DOC_DIR}/${document}"',
        ),
        "NOTICE": (
            "997b825a69d67022b169f36825632bdbcee296a0",
            "2026-07-27",
            "modified version of ZeroStack",
        ),
        "SOURCE.md": (
            "mini-agent-v<VERSION>-source.tar.gz",
            "vendored",
            "for as long as it distributes",
        ),
        "scripts/package-release-binary.py": (
            'REQUIRED_DOCUMENTS = ("LICENSE", "NOTICE", "SOURCE.md")',
            "3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986",
            "LICENSE is not the canonical GPL-3.0-only text",
            "release archive payload mismatch",
        ),
        "scripts/package-corresponding-source.sh": (
            'SOURCE_ROOT="${BINARY_NAME}-${RELEASE_TAG}-source"',
            'TAG_COMMIT=$(git rev-parse --verify "refs/tags/${RELEASE_TAG}^{commit}"',
            'if [[ "$SOURCE_COMMIT" != "$TAG_COMMIT" ]]; then',
            'elif [[ "$ALLOW_UNTAGGED_LABEL" != true ]]; then',
            '--allow-untagged-label is restricted to labels ending in -ci',
            '--compliance-docs requires a directory',
            'CANONICAL_GPL3_LICENSE_SHA256="3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986"',
            "LICENSE is not the canonical GPL-3.0-only text",
            "cargo vendor --locked --versioned-dirs vendor > .cargo/config.toml",
            "cargo metadata --locked --offline --format-version 1 > /dev/null",
            'for required in LICENSE NOTICE SOURCE.md Cargo.toml Cargo.lock rust-toolchain.toml Cross.toml .cargo/config.toml; do',
            'tar tzf "$STAGING_DIR/$ARCHIVE_NAME" > "$ARCHIVE_LISTING"',
            'grep -Eq -- "^$ESCAPED_SOURCE_ROOT/vendor/',
        ),
        "scripts/smoke-package-compliance.py": (
            'CHANNELS = ("aur", "conda-bin", "conda-source", "homebrew")',
            "3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986",
            "LICENSE is not the canonical GPL-3.0-only text",
            'run(["ruby", "--disable-gems", "-e", HOMEBREW_HARNESS]',
            'source "$RECIPE"; package',
            '"info/licenses/LICENSE": payload / "LICENSE"',
        ),
        "packaging/homebrew/zerostack.rb": (
            f'homepage "{CANONICAL_REPOSITORY_URL}"',
            f"{binary}-x86_64-apple-darwin.tar.gz",
            f"{binary}-aarch64-apple-darwin.tar.gz",
            f"{binary}-x86_64-unknown-linux-musl.tar.gz",
            f"{binary}-aarch64-unknown-linux-musl.tar.gz",
            f'bin.install "{binary}"',
            f'shell_output("#{{bin}}/{binary} --version")',
        ),
        "packaging/aur/PKGBUILD": (
            f'url="{CANONICAL_REPOSITORY_URL}"',
            f"{binary}-x86_64-unknown-linux-musl.tar.gz",
            f"{binary}-aarch64-unknown-linux-musl.tar.gz",
            f"provides=('{binary}')",
            f'"${{pkgdir}}/usr/bin/{binary}"',
        ),
        "packaging/aur/.SRCINFO": (
            f"url = {CANONICAL_REPOSITORY_URL}",
            f"provides = {binary}",
            f"/{binary}-x86_64-unknown-linux-musl.tar.gz",
            f"/{binary}-aarch64-unknown-linux-musl.tar.gz",
        ),
        "packaging/conda/zerostack-bin/build.sh": (
            f'"${{SRC_DIR}}/{binary}"',
            f'"${{PREFIX}}/bin/{binary}"',
        ),
        "packaging/conda/zerostack-bin/meta.yaml": (
            f"home: {CANONICAL_REPOSITORY_URL}",
            f"{binary}-x86_64-unknown-linux-musl.tar.gz",
            f"{binary}-aarch64-unknown-linux-musl.tar.gz",
            f"- {binary} --help",
            f"- {binary} --version",
        ),
        "packaging/conda/zerostack/meta.yaml": (
            f"repository: {CANONICAL_REPOSITORY_URL}",
            f"- {binary} --help",
            f"- {binary} --version",
            "- test -f ${PREFIX}/THIRDPARTY.yml",
        ),
        "packaging/conda/zerostack/build.sh": (
            'install -Dm644 THIRDPARTY.yml "${PREFIX}/THIRDPARTY.yml"',
        ),
        "justfile": (
            "bash scripts/update-release-checksums.sh all",
            "bash scripts/smoke-canonical-installer.sh",
            '--release-tag "v${VERSION}"',
            '--release-tag "v${NEW_VERSION}"',
            "--require-clean",
            "cargo metadata --format-version 1 --no-deps >/dev/null",
        ),
        "scripts/update-release-checksums.sh": (
            CANONICAL_REPOSITORY_URL,
            "curl -fsSL",
            f"{binary}-x86_64-apple-darwin.tar.gz",
            f"{binary}-aarch64-apple-darwin.tar.gz",
            f"{binary}-x86_64-unknown-linux-musl.tar.gz",
            f"{binary}-aarch64-unknown-linux-musl.tar.gz",
        ),
        "README.md": (
            f"The Cargo package, installed CLI, and every binary release archive use the executable name\n`{binary}`.",
        ),
        "docs/agent/PUBLISHING_RELEASES.md": (
            f"Cargo and every package channel install the public executable as `{binary}`.",
            "The release workflow accepts only pushed `v*` tags.",
            f"`{CANONICAL_REPOSITORY}`",
            "`.zerostack`",
            "`ZEROSTACK_*`",
            "Supported package channels are source/Cargo, AUR, Conda, and Homebrew",
            "Nix packaging is intentionally unsupported",
            "pinned inputs, Linux and macOS CI",
            "default-feature parity",
            "exact store output",
        ),
        "docs/agent/GET_STARTED.md": (
            f"https://raw.githubusercontent.com/{CANONICAL_REPOSITORY}/main/install.sh",
            f"https://github.com/{CANONICAL_REPOSITORY}",
        ),
        "scripts/smoke-canonical-installer.sh": (
            'bash "${ROOT_DIR}/install.sh" --release "$VERSION" --dir "${INSTALL_ROOT}/bin"',
            '"${INSTALL_ROOT}/bin/mini-agent" --version',
            'EXPECTED_OUTPUT="mini-agent ${VERSION}"',
            'for document in LICENSE NOTICE SOURCE.md; do',
            '"${INSTALL_ROOT}/share/doc/mini-agent/${document}"',
        ),
        ".github/workflows/pages.yml": ("https://sebahrens.github.io/mini-agent",),
        ".github/workflows/ci.yml": (
            "package-compliance-smoke:",
            "python3 scripts/smoke-package-compliance.py ${{ matrix.channels }}",
            "--channel aur --channel conda-bin --channel conda-source",
            "--channel homebrew",
        ),
        "src/product.rs": (
            f'pub const PUBLIC_NAME: &str = "{binary}";',
            f'pub const REPOSITORY_SLUG: &str = "{CANONICAL_REPOSITORY}";',
            f'pub const REPOSITORY_URL: &str = "{CANONICAL_REPOSITORY_URL}";',
            'pub const LEGACY_APP_COMPONENT: &str = "zerostack";',
            'pub const LEGACY_PROJECT_DIRECTORY: &str = ".zerostack";',
            'pub const LEGACY_ENV_PREFIX: &str = "ZEROSTACK_";',
        ),
        "src/provider.rs": (
            ".with_app_identity(crate::product::PUBLIC_NAME, crate::product::REPOSITORY_URL)",
        ),
        "src/extras/acp/mod.rs": (
            ".agent_info(Implementation::new(",
            "crate::product::PUBLIC_NAME",
        ),
        "src/extras/lsp/client.rs": ('"name": crate::product::PUBLIC_NAME',),
        "src/extras/mcp/oauth.rs": (
            "const CLIENT_NAME: &str = crate::product::PUBLIC_NAME;",
            '"<html><body><h3>{}: authorization complete.',
        ),
        "src/extras/export.rs": (
            ".header(reqwest::header::USER_AGENT, crate::product::PUBLIC_NAME)",
        ),
        "src/ui/events.rs": (
            'format!("  Website: {}", crate::product::REPOSITORY_URL)',
            "crate::product::PUBLIC_NAME",
        ),
        "src/setup/mod.rs": ("crate::product::PUBLIC_NAME.to_uppercase()",),
        "src/docs.rs": ("crate::product::PUBLIC_NAME",),
    }
    errors: list[str] = []
    for relative_path, fragments in required.items():
        path = root / relative_path
        if not path.is_file():
            errors.append(f"required package metadata file is missing: {relative_path}")
            continue
        text = path.read_text(encoding="utf-8")
        for fragment in fragments:
            if fragment not in text:
                errors.append(f"{relative_path} is missing required text {fragment!r}")

    forbidden_asset = "zerostack-"
    for relative_path in (
        "packaging/homebrew/zerostack.rb",
        "packaging/aur/PKGBUILD",
        "packaging/conda/zerostack-bin/meta.yaml",
        "justfile",
    ):
        text = (root / relative_path).read_text(encoding="utf-8")
        for architecture in ("x86_64", "aarch64"):
            fragment = f"{forbidden_asset}{architecture}"
            if fragment in text:
                errors.append(
                    f"{relative_path} references noncanonical asset {fragment!r}"
                )
    return errors


def validate_distribution_notice_installs(root: Path) -> list[str]:
    """Require every maintained package recipe to install compliance documents."""

    errors: list[str] = []
    for relative, fragments in DISTRIBUTION_NOTICE_FRAGMENTS.items():
        path = root / relative
        if not path.is_file():
            errors.append(f"{relative} is missing")
            continue
        text = path.read_text(encoding="utf-8")
        for fragment in fragments:
            if fragment not in text:
                errors.append(
                    f"{relative} must install the release compliance payload; "
                    f"missing {fragment!r}"
                )
    return errors


def validate_aur_srcinfo_checksums(root: Path) -> list[str]:
    """Keep generated AUR metadata aligned with the checked-in PKGBUILD."""

    pkgbuild_path = root / "packaging/aur/PKGBUILD"
    srcinfo_path = root / "packaging/aur/.SRCINFO"
    if not pkgbuild_path.is_file() or not srcinfo_path.is_file():
        return ["AUR PKGBUILD and .SRCINFO are both required"]

    pkgbuild = pkgbuild_path.read_text(encoding="utf-8")
    srcinfo = srcinfo_path.read_text(encoding="utf-8")
    errors: list[str] = []
    for architecture in ("x86_64", "aarch64"):
        array = re.search(
            rf"^sha256sums_{architecture}=\(([^)]*)\)$", pkgbuild, re.MULTILINE
        )
        pkgbuild_hashes = re.findall(r"'([0-9a-f]{64})'", array.group(1)) if array else []
        srcinfo_hashes = re.findall(
            rf"^\s*sha256sums_{architecture} = ([0-9a-f]{{64}})$",
            srcinfo,
            re.MULTILINE,
        )
        if pkgbuild_hashes != srcinfo_hashes:
            errors.append(
                f"packaging/aur/.SRCINFO {architecture} checksums must match PKGBUILD"
            )

    pkgrel = re.search(r"^pkgrel=([0-9]+)$", pkgbuild, re.MULTILINE)
    srcinfo_pkgrel = re.search(r"^\s*pkgrel = ([0-9]+)$", srcinfo, re.MULTILINE)
    if not pkgrel or not srcinfo_pkgrel or pkgrel.group(1) != srcinfo_pkgrel.group(1):
        errors.append("packaging/aur/.SRCINFO pkgrel must match PKGBUILD")
    return errors


def indexed_files(root: Path) -> list[str]:
    """Return only files in Git's release index, excluding developer-local files."""
    result = subprocess.run(
        ["git", "ls-files", "-z", "--cached"],
        cwd=root,
        check=True,
        capture_output=True,
    )
    return [entry.decode("utf-8") for entry in result.stdout.split(b"\0") if entry]


def validate_stale_coordinates(
    root: Path, relative_paths: list[str] | None = None
) -> list[str]:
    """Reject old active repository coordinates outside historical specs."""
    paths = indexed_files(root) if relative_paths is None else relative_paths
    errors: list[str] = []
    for relative_path in paths:
        if relative_path.startswith(HISTORICAL_COORDINATE_ALLOWLIST):
            continue
        if relative_path in UPSTREAM_PROVENANCE_FILES:
            continue
        path = root / relative_path
        if not path.is_file():
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        folded_text = text.casefold()
        for coordinate in LEGACY_COORDINATES:
            if coordinate in folded_text:
                errors.append(
                    f"{relative_path} contains stale active coordinate {coordinate!r}"
                )
    return errors


def validate_removed_nix_surface(
    root: Path, relative_paths: list[str] | None = None
) -> list[str]:
    """Keep the unsupported, unverified Nix entry points out of the release surface."""
    paths = indexed_files(root) if relative_paths is None else relative_paths
    errors: list[str] = []
    for relative_path in paths:
        path = root / relative_path
        if not path.exists():
            continue
        parts = Path(relative_path).parts
        is_nix_surface = (
            path.suffix == ".nix"
            or path.name == "flake.lock"
            or "nix" in parts[:-1]
        )
        if is_nix_surface:
            errors.append(
                "unsupported Nix packaging surface must remain removed: "
                f"{relative_path}"
            )
    return errors


def cargo_version(metadata: dict[str, Any], root: Path) -> str | None:
    manifest = (root / "Cargo.toml").resolve()
    package = next(
        (
            p
            for p in metadata.get("packages", [])
            if Path(p["manifest_path"]).resolve() == manifest
        ),
        None,
    )
    return package.get("version") if package else None


def validate_versions(root: Path, version: str) -> list[str]:
    """Check that AUR, Conda, and Homebrew metadata agree with Cargo version."""
    import re

    errors: list[str] = []

    checks: list[tuple[str, str]] = [
        ("packaging/aur/PKGBUILD", rf"^pkgver={re.escape(version)}$"),
        ("packaging/aur/.SRCINFO", rf"^\s*pkgver = {re.escape(version)}$"),
        ("packaging/conda/zerostack/meta.yaml", rf"^\s+version: {re.escape(version)}$"),
        ("packaging/conda/zerostack-bin/meta.yaml", rf"^\s+version: {re.escape(version)}$"),
        ("packaging/homebrew/zerostack.rb", rf'^\s+version "{re.escape(version)}"$'),
    ]

    for relative_path, pattern in checks:
        path = root / relative_path
        if not path.is_file():
            errors.append(f"packaging file missing: {relative_path}")
            continue
        text = path.read_text(encoding="utf-8")
        if not re.search(pattern, text, re.MULTILINE):
            errors.append(
                f"{relative_path} does not contain expected version {version!r}"
                f" (pattern {pattern!r})"
            )

    return errors


def validate(root: Path, metadata: dict[str, Any]) -> list[str]:
    binary, errors = canonical_binary(metadata, root)
    if binary is None:
        return errors

    workflow_path = root / ".github/workflows/release.yml"
    if not workflow_path.is_file():
        errors.append("release workflow is missing")
    else:
        errors.extend(
            validate_workflow(workflow_path.read_text(encoding="utf-8"), binary)
        )
    cross_config_path = root / "Cross.toml"
    if not cross_config_path.is_file():
        errors.append("Cross.toml is required for immutable musl build images")
    else:
        errors.extend(
            validate_cross_images(cross_config_path.read_text(encoding="utf-8"))
        )
    dependabot_path = root / ".github/dependabot.yml"
    if not dependabot_path.is_file():
        errors.append(".github/dependabot.yml is missing")
    else:
        errors.extend(
            validate_github_actions_updates(
                dependabot_path.read_text(encoding="utf-8")
            )
        )
    errors.extend(validate_file_fragments(root, binary))
    errors.extend(validate_license_identity(root))
    errors.extend(validate_distribution_notice_installs(root))
    errors.extend(validate_aur_srcinfo_checksums(root))
    errors.extend(validate_stale_coordinates(root))
    errors.extend(validate_removed_nix_surface(root))

    version = cargo_version(metadata, root)
    if version:
        errors.extend(validate_versions(root, version))

    return errors


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Validate release and package metadata"
    )
    parser.add_argument(
        "--release-tag",
        help="release tag to validate against the root Cargo package version",
    )
    parser.add_argument(
        "--ref-type",
        help="GitHub ref type; must be 'tag' when --release-tag is supplied",
    )
    parser.add_argument(
        "--require-clean",
        action="store_true",
        help="require tracked worktree and index content to match HEAD",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if bool(args.release_tag) != bool(args.ref_type):
        print(
            "package metadata check failed: --release-tag and --ref-type "
            "must be supplied together",
            file=sys.stderr,
        )
        return 2

    try:
        metadata = cargo_metadata(ROOT)
    except (OSError, subprocess.CalledProcessError, json.JSONDecodeError) as error:
        print(f"package metadata check failed: {error}", file=sys.stderr)
        return 1

    errors = validate(ROOT, metadata)
    if args.require_clean:
        errors.extend(validate_clean_tracked_worktree(ROOT))
    version = cargo_version(metadata, ROOT)
    if args.release_tag and args.ref_type and version:
        errors.extend(
            validate_release_identity(
                version=version,
                ref_type=args.ref_type,
                release_tag=args.release_tag,
            )
        )
    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1

    binary, _ = canonical_binary(metadata, ROOT)
    print(f"package metadata consistent: canonical binary {binary!r}, version {version!r}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
