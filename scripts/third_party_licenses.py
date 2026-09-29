#!/usr/bin/env python3
"""Generate and verify the THIRD_PARTY_LICENSES inventory of a release binary.

The inventory is derived from `cargo metadata --locked --offline` for one
release target and feature set. It names every package in that locked
resolution (other than the workspace's own GPL package) with its version,
declared license expression, and the verbatim license, copyright, and NOTICE
files the package ships. Build-script crates that compile vendored native
sources into the binary (`*-sys` and `*-src` packages) also contribute the
license files of those vendored trees. A package that ships no license file
at all receives the canonical text of its declared license from
packaging/license-texts/, and generation fails closed when no such text is
available, so a new dependency cannot silently ship without its notice.

Identical texts are printed once in the "License texts" part and referenced
by every package that ships them.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from collections.abc import Iterable, Mapping
from pathlib import Path
from typing import Any, NamedTuple


ROOT = Path(__file__).resolve().parents[1]
DOCUMENT_NAME = "THIRD_PARTY_LICENSES"
HEADER = "mini-agent third-party license inventory"
FORMAT_LINE = "Format: mini-agent-third-party-licenses/1"
CANONICAL_TEXTS = ROOT / "packaging" / "license-texts"
LICENSE_FILE = re.compile(
    r"^(?:licen[cs]e|copying|copyright|notice|unlicense)(?:[-._].*)?$",
    re.IGNORECASE,
)
VENDORED_NATIVE_SUFFIXES = ("-sys", "-src")
VENDORED_SCAN_DEPTH = 3
VENDORED_SKIP_DIRECTORIES = frozenset(
    {".git", "tests", "test", "fuzz", "examples", "benches", "docs", "doc"}
)
PACKAGE_LINE = re.compile(r"^Package: (?P<name>\S+) (?P<version>\S+)$")
SPDX_IDENTIFIER = re.compile(r"[A-Za-z0-9][A-Za-z0-9.+-]*")
SPDX_OPERATORS = frozenset({"AND", "OR", "WITH"})
MAX_LICENSE_FILE_BYTES = 1024 * 1024


class InventoryError(ValueError):
    """The third-party inventory cannot be generated or does not verify."""


class Package(NamedTuple):
    name: str
    version: str
    license: str
    repository: str
    source: str
    directory: Path
    license_file: str | None
    authors: tuple[str, ...]


def cargo_metadata(
    *, root: Path, target: str | None, no_default_features: bool
) -> dict[str, Any]:
    command = ["cargo", "metadata", "--locked", "--offline", "--format-version", "1"]
    if target:
        command += ["--filter-platform", target]
    if no_default_features:
        command.append("--no-default-features")
    try:
        completed = subprocess.run(
            command, cwd=root, capture_output=True, text=True, check=False
        )
    except OSError as error:
        raise InventoryError(f"cannot run cargo metadata: {error}") from error
    if completed.returncode != 0:
        raise InventoryError(
            f"cargo metadata failed ({completed.returncode}): {completed.stderr.strip()}"
        )
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise InventoryError(f"cargo metadata returned invalid JSON: {error}") from error


def resolved_packages(metadata: Mapping[str, Any]) -> list[Package]:
    """Return every non-workspace package of the locked resolution, sorted."""

    resolve = metadata.get("resolve")
    if not isinstance(resolve, dict) or not isinstance(resolve.get("nodes"), list):
        raise InventoryError("cargo metadata has no dependency resolution")
    members = set(metadata.get("workspace_members") or [])
    by_id = {package["id"]: package for package in metadata.get("packages", [])}
    packages: list[Package] = []
    for node in resolve["nodes"]:
        identifier = node.get("id")
        if identifier in members:
            continue
        package = by_id.get(identifier)
        if package is None:
            raise InventoryError(f"resolved package {identifier!r} has no metadata")
        packages.append(
            Package(
                name=package["name"],
                version=package["version"],
                license=(package.get("license") or "").strip(),
                repository=package.get("repository") or "",
                source=package.get("source") or "",
                directory=Path(package["manifest_path"]).parent,
                license_file=package.get("license_file"),
                authors=tuple(package.get("authors") or ()),
            )
        )
    packages.sort(key=lambda package: (package.name, package.version, package.source))
    return packages


def expected_package_names(metadata: Mapping[str, Any]) -> set[tuple[str, str]]:
    return {(package.name, package.version) for package in resolved_packages(metadata)}


def _read_text(path: Path) -> str:
    size = path.stat().st_size
    if size > MAX_LICENSE_FILE_BYTES:
        raise InventoryError(f"license file is unexpectedly large: {path}")
    text = path.read_bytes().decode("utf-8", errors="replace")
    text = text.replace("\r\n", "\n").replace("\r", "\n")
    return text.strip("\n") + "\n"


def _license_files(
    package: Package,
) -> tuple[list[tuple[str, Path]], list[tuple[str, Path]]]:
    """Return the package's own license files and those of vendored native trees."""

    own: dict[str, Path] = {}
    directory = package.directory
    if package.license_file:
        declared = (directory / package.license_file).resolve()
        if declared.is_file():
            own[package.license_file] = declared
    for candidate in sorted(directory.iterdir()):
        if candidate.is_file() and LICENSE_FILE.match(candidate.name):
            own.setdefault(candidate.name, candidate)
    vendored: dict[str, Path] = {}
    if package.name.endswith(VENDORED_NATIVE_SUFFIXES):
        for candidate in _vendored_license_files(directory):
            vendored.setdefault(candidate.relative_to(directory).as_posix(), candidate)
    return sorted(own.items()), sorted(vendored.items())


def _vendored_license_files(directory: Path) -> Iterable[Path]:
    pending = [(child, 1) for child in sorted(directory.iterdir()) if child.is_dir()]
    while pending:
        current, depth = pending.pop(0)
        if current.name in VENDORED_SKIP_DIRECTORIES or current.is_symlink():
            continue
        for child in sorted(current.iterdir()):
            if child.is_file() and LICENSE_FILE.match(child.name):
                yield child
            elif child.is_dir() and depth < VENDORED_SCAN_DEPTH:
                pending.append((child, depth + 1))


def license_identifiers(expression: str) -> list[str]:
    identifiers: list[str] = []
    for token in SPDX_IDENTIFIER.findall(expression.replace("/", " OR ")):
        if token in SPDX_OPERATORS or token in identifiers:
            continue
        identifiers.append(token)
    return identifiers


def canonical_texts(package: Package, texts_root: Path) -> list[tuple[str, str]]:
    """Return canonical license texts for a package that ships none."""

    identifiers = license_identifiers(package.license)
    if not identifiers:
        raise InventoryError(
            f"{package.name} {package.version} declares no license and ships no license file"
        )
    available: list[tuple[str, str]] = []
    for identifier in identifiers:
        path = texts_root / f"{identifier}.txt"
        if not path.is_file():
            continue
        text = _read_text(path)
        holders = ", ".join(package.authors) or f"the {package.name} authors"
        text = text.replace("<copyright holders>", holders)
        available.append((f"{identifier} (canonical text)", text))
    requires_all = " AND " in f" {package.license} "
    if not available or (requires_all and len(available) != len(identifiers)):
        raise InventoryError(
            f"{package.name} {package.version} ships no license file and "
            f"packaging/license-texts/ has no canonical text for {package.license!r}"
        )
    return available


def render(
    metadata: Mapping[str, Any],
    *,
    target: str,
    no_default_features: bool,
    texts_root: Path = CANONICAL_TEXTS,
) -> str:
    packages = resolved_packages(metadata)
    if not packages:
        raise InventoryError("the locked resolution contains no third-party packages")
    root_package = metadata.get("resolve", {}).get("root")
    root_label = "mini-agent"
    for package in metadata.get("packages", []):
        if package.get("id") == root_package:
            root_label = f"{package['name']} {package['version']}"

    text_ids: dict[str, str] = {}
    texts: list[tuple[str, str]] = []
    entries: list[str] = []
    for package in packages:
        own, vendored = _license_files(package)
        files = [(name, _read_text(path)) for name, path in own]
        if not files:
            # The package's own notice is required even when a vendored
            # native tree ships a license of its own.
            files = canonical_texts(package, texts_root)
        files += [(name, _read_text(path)) for name, path in vendored]
        references: list[str] = []
        for name, text in files:
            digest = hashlib.sha256(text.encode("utf-8")).hexdigest()
            identifier = text_ids.get(digest)
            if identifier is None:
                identifier = f"T{len(texts) + 1:04d}"
                text_ids[digest] = identifier
                texts.append((identifier, text))
            references.append(f"  {name}: [{identifier}]")
        source = package.source or "local path"
        lines = [
            f"Package: {package.name} {package.version}",
            f"License: {package.license or 'see the license files below'}",
        ]
        if package.repository:
            lines.append(f"Repository: {package.repository}")
        lines.append(f"Source: {source}")
        lines.append("License files:")
        lines.extend(references)
        entries.append("\n".join(lines))

    features = "--no-default-features" if no_default_features else "default"
    header = [
        HEADER,
        "=" * len(HEADER),
        FORMAT_LINE,
        f"Binary: {root_label}",
        f"Target: {target}",
        f"Features: {features}",
        f"Packages: {len({(package.name, package.version) for package in packages})}",
        "",
        "mini-agent itself is licensed under GPL-3.0-only (see LICENSE); NOTICE",
        "records its provenance and the non-Cargo components embedded in the",
        "executable (QuickJS through rquickjs-sys, and AJV).",
        "",
        "This file lists every package of the locked Cargo dependency resolution",
        "for the target and feature set above, including build-time tools and",
        "test-only packages that are not linked into the executable. Each entry",
        "gives the package's declared license expression and references the",
        "verbatim license, copyright, and NOTICE files it ships; those texts",
        "follow in the \"License texts\" part, each printed once. Where a package",
        "offers a choice of licenses, all offered texts that it ships are included.",
        "",
    ]
    body = ["Packages", "--------", ""]
    body.append("\n\n".join(entries))
    body += ["", "", "License texts", "-------------", ""]
    for identifier, text in texts:
        body.append(f"===== [{identifier}] =====")
        body.append(text)
    return "\n".join(header + body).rstrip("\n") + "\n"


def parse_inventory(text: str) -> dict[str, Any]:
    lines = text.splitlines()
    if len(lines) < 7 or lines[0] != HEADER or lines[2] != FORMAT_LINE:
        raise InventoryError("THIRD_PARTY_LICENSES has an unrecognised header")
    fields: dict[str, str] = {}
    for line in lines[3:7]:
        key, separator, value = line.partition(": ")
        if not separator:
            raise InventoryError(f"malformed THIRD_PARTY_LICENSES header line {line!r}")
        fields[key] = value
    packages: set[tuple[str, str]] = set()
    for line in lines:
        match = PACKAGE_LINE.fullmatch(line)
        if match:
            packages.add((match.group("name"), match.group("version")))
    if not packages:
        raise InventoryError("THIRD_PARTY_LICENSES names no packages")
    if fields.get("Packages") != str(len(packages)):
        raise InventoryError(
            "THIRD_PARTY_LICENSES package count does not match its entries"
        )
    return {
        "target": fields.get("Target"),
        "features": fields.get("Features"),
        "packages": packages,
    }


def verify_inventory(
    text: str,
    metadata: Mapping[str, Any],
    *,
    target: str,
    no_default_features: bool,
) -> int:
    """Require the inventory to name every package of the target's resolution."""

    inventory = parse_inventory(text)
    features = "--no-default-features" if no_default_features else "default"
    if inventory["target"] != target:
        raise InventoryError(
            f"THIRD_PARTY_LICENSES is for target {inventory['target']!r}, expected {target!r}"
        )
    if inventory["features"] != features:
        raise InventoryError(
            f"THIRD_PARTY_LICENSES is for features {inventory['features']!r}, "
            f"expected {features!r}"
        )
    expected = expected_package_names(metadata)
    missing = sorted(expected - inventory["packages"])
    if missing:
        shown = ", ".join(f"{name} {version}" for name, version in missing[:20])
        raise InventoryError(
            f"THIRD_PARTY_LICENSES omits {len(missing)} resolved package(s): {shown}"
        )
    return len(expected)


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("generate", "verify"):
        command = commands.add_parser(name)
        command.add_argument("--root", type=Path, default=ROOT)
        command.add_argument("--target", required=True)
        command.add_argument("--no-default-features", action="store_true")
        if name == "generate":
            command.add_argument("--output", type=Path, required=True)
        else:
            command.add_argument("--inventory", type=Path, required=True)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        metadata = cargo_metadata(
            root=args.root,
            target=args.target,
            no_default_features=args.no_default_features,
        )
        if args.command == "generate":
            document = render(
                metadata,
                target=args.target,
                no_default_features=args.no_default_features,
            )
            verify_inventory(
                document,
                metadata,
                target=args.target,
                no_default_features=args.no_default_features,
            )
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(document, encoding="utf-8", newline="\n")
            count = parse_inventory(document)["packages"]
            print(f"wrote {args.output} ({len(count)} packages)")
        else:
            text = args.inventory.read_text(encoding="utf-8")
            count = verify_inventory(
                text,
                metadata,
                target=args.target,
                no_default_features=args.no_default_features,
            )
            print(f"{args.inventory} names all {count} resolved packages")
    except (InventoryError, OSError, UnicodeDecodeError) as error:
        print(f"third-party license inventory failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
