#!/usr/bin/env python3
"""Validate that release and package metadata use Cargo's canonical binary."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]


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
        'archive="${CANONICAL_BINARY}-${{ matrix.target }}.tar.gz"': 2,
        'archive="${CANONICAL_BINARY}-lite-${{ matrix.target }}.tar.gz"': 2,
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
        "src/cli.rs": (f'#[command(name = "{binary}"',),
        "install.sh": (
            f'BINARY_NAME="{binary}"',
            'if [[ ! -f "${TMPDIR}/${BINARY_NAME}" ]]',
            'cp "${TMPDIR}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"',
        ),
        "packaging/homebrew/zerostack.rb": (
            f"{binary}-x86_64-apple-darwin.tar.gz",
            f"{binary}-aarch64-apple-darwin.tar.gz",
            f"{binary}-x86_64-unknown-linux-musl.tar.gz",
            f"{binary}-aarch64-unknown-linux-musl.tar.gz",
            f'bin.install "{binary}"',
            f'shell_output("#{{bin}}/{binary} --version")',
        ),
        "packaging/aur/PKGBUILD": (
            f"{binary}-x86_64-unknown-linux-musl.tar.gz",
            f"{binary}-aarch64-unknown-linux-musl.tar.gz",
            f"provides=('{binary}')",
            f'"${{pkgdir}}/usr/bin/{binary}"',
        ),
        "packaging/conda/zerostack-bin/build.sh": (
            f'"${{SRC_DIR}}/{binary}"',
            f'"${{PREFIX}}/bin/{binary}"',
        ),
        "packaging/conda/zerostack-bin/meta.yaml": (
            f"{binary}-x86_64-unknown-linux-musl.tar.gz",
            f"{binary}-aarch64-unknown-linux-musl.tar.gz",
            f"- {binary} --help",
            f"- {binary} --version",
        ),
        "packaging/conda/zerostack/meta.yaml": (
            f"- {binary} --help",
            f"- {binary} --version",
        ),
        "nix/package/zerostack.nix": (f'mainProgram = "{binary}";',),
        "justfile": (
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
        ),
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
    print(f"package metadata consistent: canonical binary {binary!r}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
