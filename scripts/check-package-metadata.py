#!/usr/bin/env python3
"""Validate that release and package metadata use Cargo's canonical binary."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
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
APPROVED_RELEASE_ACTIONS = {
    ("actions/checkout", "v7.0.1"): "3d3c42e5aac5ba805825da76410c181273ba90b1",
    (
        "actions-rust-lang/setup-rust-toolchain",
        "v1.17.0",
    ): "166cdcfd11aee3cb47222f9ddb555ce30ddb9659",
    (
        "taiki-e/install-action",
        "v2.84.1",
    ): "c44f6b046f1c29ae5918b1e0bfdbb2f1813836fd",
    (
        "actions/upload-artifact",
        "v4.6.2",
    ): "ea165f8d65b6e75b540449e92b4886f43607fa02",
    (
        "actions/download-artifact",
        "v4.3.0",
    ): "d3f86a106a0bac45b974a628896c90dbdf5c8093",
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
        # 3 jobs produce archives: build (Linux/macOS), build-static (musl), build-windows
        'archive="${CANONICAL_BINARY}-${{ matrix.target }}.tar.gz"': 3,
        'archive="${CANONICAL_BINARY}-lite-${{ matrix.target }}.tar.gz"': 3,
        # Unix jobs: 4 smoke invocations (2 jobs × 2 archives each)
        'tar czf "$archive" -C "target/${{ matrix.target }}/release" '
        '"$CANONICAL_BINARY"': 4,
        'test "$(tar tzf "$archive")" = "$CANONICAL_BINARY"': 4,
        '"$smoke_dir/$CANONICAL_BINARY" --version | grep -Fq -- '
        '"$CANONICAL_BINARY "': 4,
    }
    for fragment, expected_count in required_counts.items():
        observed_count = text.count(fragment)
        if observed_count != expected_count:
            errors.append(
                ".github/workflows/release.yml must contain "
                f"{fragment!r} exactly {expected_count} time(s), found "
                f"{observed_count}"
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
    errors.extend(validate_release_action_pins(text))
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
        ),
        "src/cli.rs": (f'#[command(name = "{binary}"',),
        "install.sh": (
            f'BINARY_NAME="{binary}"',
            f'REPO="{CANONICAL_REPOSITORY}"',
            'if [[ ! -f "${TMPDIR}/${BINARY_NAME}" ]]',
            'cp "${TMPDIR}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"',
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
        ),
        "justfile": (
            "bash scripts/update-release-checksums.sh all",
            "bash scripts/smoke-canonical-installer.sh",
        ),
        "scripts/update-release-checksums.sh": (
            CANONICAL_REPOSITORY_URL,
            "curl -fsSL",
            f"{binary}-x86_64-apple-darwin.tar.gz",
            f"{binary}-aarch64-apple-darwin.tar.gz",
            f"{binary}-x86_64-unknown-linux-musl.tar.gz",
            f"{binary}-aarch64-unknown-linux-musl.tar.gz",
            '--release-tag "v${VERSION}"',
            '--release-tag "v${NEW_VERSION}"',
            "--require-clean",
            "cargo metadata --format-version 1 --no-deps >/dev/null",
        ),
        "README.md": (
            f"The Cargo package, installed CLI, and every release archive use the executable name\n`{binary}`.",
        ),
        "docs/agent/PUBLISHING_RELEASES.md": (
            f"Cargo and every package channel install the public executable as `{binary}`.",
            "The release workflow accepts only pushed `v*` tags.",
            f"`{CANONICAL_REPOSITORY}`",
            "`.zerostack`",
            "`ZEROSTACK_*`",
            "Supported package channels are Cargo/crates.io, AUR, Conda, and Homebrew",
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
        ),
        ".github/workflows/pages.yml": ("https://sebahrens.github.io/mini-agent",),
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


def tracked_files(root: Path) -> list[str]:
    result = subprocess.run(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        cwd=root,
        check=True,
        capture_output=True,
    )
    return [entry.decode("utf-8") for entry in result.stdout.split(b"\0") if entry]


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
    paths = tracked_files(root) if relative_paths is None else relative_paths
    errors: list[str] = []
    for relative_path in paths:
        if relative_path.startswith(HISTORICAL_COORDINATE_ALLOWLIST):
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
