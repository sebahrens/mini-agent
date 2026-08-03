#!/usr/bin/env python3
"""Validate that release and package metadata use Cargo's canonical binary."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
CANONICAL_REPOSITORY = "sebahrens/mini-agent"
CANONICAL_REPOSITORY_URL = f"https://github.com/{CANONICAL_REPOSITORY}"
LEGACY_COORDINATES = (
    ("gi-" + "dellav/zerostack").casefold(),
    ("gi-" + "dellav.github.io/zerostack").casefold(),
)
HISTORICAL_COORDINATE_ALLOWLIST = (
    "docs/specs/superseded/",
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
    return errors


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
        "nix/package/zerostack.nix": (f'mainProgram = "{binary}";',),
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
        ),
        "README.md": (
            f"The Cargo package, installed CLI, and every release archive use the executable name\n`{binary}`.",
        ),
        "docs/agent/PUBLISHING_RELEASES.md": (
            f"Cargo and every package channel install the public executable as `{binary}`.",
            f"`{CANONICAL_REPOSITORY}`",
            "`.zerostack`",
            "`ZEROSTACK_*`",
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
    errors.extend(validate_file_fragments(root, binary))
    errors.extend(validate_stale_coordinates(root))

    version = cargo_version(metadata, root)
    if version:
        errors.extend(validate_versions(root, version))

    return errors


def main() -> int:
    try:
        metadata = cargo_metadata(ROOT)
    except (OSError, subprocess.CalledProcessError, json.JSONDecodeError) as error:
        print(f"package metadata check failed: {error}", file=sys.stderr)
        return 1

    errors = validate(ROOT, metadata)
    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1

    binary, _ = canonical_binary(metadata, ROOT)
    version = cargo_version(metadata, ROOT)
    print(f"package metadata consistent: canonical binary {binary!r}, version {version!r}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
