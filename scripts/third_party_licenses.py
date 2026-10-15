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
import os
import stat
import re
import subprocess
import sys
import tarfile
import io
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
GIT_SOURCE_SHA256 = "26c56c296b38c0695b26fa95f475f1d01704d2d38e73465ca30b0b2f5dc789d3"
GIT_POLICY_SHA256 = "3138c02584ea00a7234eef01366b0ca5757c0b5e4f28b4878381544b0b0e97b2"
NATIVE_FORMAT = "mini-agent-native-components/1"
NATIVE_LINE = re.compile(r"^Native-Component: (?P<name>\S+) (?P<version>\S+)$")


def checked_native_asset(root: Path, item: Mapping[str, Any], limit: int) -> Path:
    if not isinstance(item, dict) or set(item) != {"path", "sha256"} or not isinstance(item["path"], str) or "\\" in item["path"]:
        raise InventoryError("invalid native source/notice asset")
    path = Path(item["path"])
    if path.is_absolute() or not path.parts or any(part in {".", ".."} for part in path.parts):
        raise InventoryError("native asset must use a relative contained path")
    current = root
    for part in path.parts:
        current = current / part
        if current.is_symlink():
            raise InventoryError("native assets cannot traverse symlinks")
    if not re.fullmatch(r"[a-f0-9]{64}", str(item["sha256"])):
        raise InventoryError("native asset has an invalid SHA-256")
    if not stat.S_ISREG(current.stat().st_mode):
        raise InventoryError("native asset must be a regular file")
    with current.open("rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_size > limit:
            raise InventoryError("native asset is not a bounded regular file")
        digest = hashlib.sha256()
        total = 0
        while block := stream.read(1024 * 1024):
            total += len(block)
            if total > limit:
                raise InventoryError("native asset exceeds size bound")
            digest.update(block)
    if digest.hexdigest() != item["sha256"]:
        raise InventoryError("native source/notice hash mismatch")
    return current


def load_native_record(path: Path, target: str, bundle: Path | None = None) -> list[dict[str, Any]]:
    if path.is_symlink() or not stat.S_ISREG(path.stat().st_mode) or path.stat().st_size > 1024 * 1024:
        raise InventoryError("native component record is not bounded regular input")
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise InventoryError("duplicate native component record key")
            result[key] = value
        return result
    with path.open("rb") as stream:
        raw = stream.read(1024 * 1024 + 1)
    if len(raw) > 1024 * 1024:
        raise InventoryError("native component record exceeds bound")
    record = json.loads(raw, object_pairs_hook=unique)
    if not isinstance(record, dict) or set(record) != {"format", "target", "bundle_manifest_sha256", "components"} or record["format"] != NATIVE_FORMAT or record["target"] != target:
        raise InventoryError("native component record format/target mismatch")
    if not re.fullmatch(r"[a-f0-9]{64}", str(record["bundle_manifest_sha256"])):
        raise InventoryError("native component record lacks bundle binding")
    components = record["components"]
    if not isinstance(components, list) or not 1 <= len(components) <= 128:
        raise InventoryError("native component count is invalid")
    seen, covered = set(), {}
    total_source_bytes = 0
    for component in components:
        if not isinstance(component, dict) or set(component) != {"name", "version", "license", "source_url", "bundled_files", "license_files", "source_files"}:
            raise InventoryError("invalid native component fields")
        for key in ("name", "version"):
            if not isinstance(component[key], str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.+:-]{0,127}", component[key]):
                raise InventoryError("invalid native component identity")
        identity = component["name"], component["version"]
        if identity in seen:
            raise InventoryError("duplicate native component identity")
        seen.add(identity)
        if not isinstance(component["license"], str) or not 1 <= len(component["license"]) <= 512 or any(c in component["license"] for c in "\r\n"):
            raise InventoryError("invalid native component license declaration")
        if not isinstance(component["source_url"], str) or not component["source_url"].startswith("https://") or any(c in component["source_url"] for c in "\r\n"):
            raise InventoryError("native component requires an HTTPS source origin")
        for key, limit in (("license_files", MAX_LICENSE_FILE_BYTES), ("source_files", 256 * 1024 * 1024)):
            assets = component[key]
            if not isinstance(assets, list) or not 1 <= len(assets) <= 128:
                raise InventoryError("native component requires bounded notices and actual source files")
            for asset in assets:
                checked = checked_native_asset(path.parent, asset, limit)
                total_source_bytes += checked.stat().st_size
                if total_source_bytes > 2 * 1024 * 1024 * 1024:
                    raise InventoryError("native source/notice set exceeds total size bound")
        if not isinstance(component["bundled_files"], list) or len(component["bundled_files"]) > 125:
            raise InventoryError("invalid native DLL coverage list")
        for item in component["bundled_files"]:
            if not isinstance(item, dict) or set(item) != {"name", "sha256"} or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.+-]{0,127}", str(item["name"])) or not re.fullmatch(r"[a-f0-9]{64}", str(item["sha256"])) or item["name"].lower() in covered:
                raise InventoryError("invalid/duplicate native file coverage")
            covered[item["name"].lower()] = item["sha256"]
    if bundle is not None:
        with (bundle / "bundle.manifest").open("rb") as stream:
            manifest = stream.read(64 * 1024 + 1)
        if len(manifest) > 64 * 1024 or hashlib.sha256(manifest).hexdigest() != record["bundle_manifest_sha256"]:
            raise InventoryError("native record does not bind this runtime bundle")
        lines = manifest.decode().splitlines()
        if f"target={target}" not in lines[:5]:
            raise InventoryError("runtime bundle target mismatch")
        libraries = {}
        for line in lines[5:]:
            fields = line.split("\t")
            if len(fields) != 3 or not fields[0].startswith("file="):
                raise InventoryError("invalid runtime bundle inventory")
            if fields[2] == "r":
                name = fields[0][5:]
                if name.lower() in libraries:
                    raise InventoryError("duplicate runtime library inventory")
                checked_native_asset(bundle, {"path": name, "sha256": fields[1]}, 64 * 1024 * 1024)
                libraries[name.lower()] = fields[1]
        if libraries != covered:
            raise InventoryError("native source/notice record omits or adds runtime DLLs")
    return components


def git_component(root: Path) -> dict[str, Any] | None:
    archive = root / "scripts/git-runtime/git-2.56.0.tar.xz"
    if not archive.exists():
        if (root / "scripts/build-git-runtime.py").exists():
            raise InventoryError("pinned Git archive is missing")
        return None  # Legacy source trees without the embedded Git producer.
    with archive.open("rb") as source:
        raw = source.read(32 * 1024 * 1024 + 1)
    policy = root / "scripts/git-runtime/policy.patch"
    with policy.open("rb") as source:
        policy_bytes = source.read(1024 * 1024 + 1)
    if len(raw) > 32 * 1024 * 1024 or len(policy_bytes) > 1024 * 1024 or hashlib.sha256(raw).hexdigest() != GIT_SOURCE_SHA256 or hashlib.sha256(policy_bytes).hexdigest() != GIT_POLICY_SHA256:
        raise InventoryError("pinned Git source/policy bytes do not match")
    notices = []
    with tarfile.open(fileobj=io.BytesIO(raw), mode="r:xz") as source:
        members = source.getmembers()
        if len(members) > 10000:
            raise InventoryError("Git source archive exceeds member bound")
        for member in members:
            if member.isfile() and LICENSE_FILE.match(Path(member.name).name):
                if member.size > MAX_LICENSE_FILE_BYTES or len(notices) >= 128:
                    raise InventoryError("Git notice files exceed bounds")
                text = source.extractfile(member).read().decode("utf-8", errors="replace")
                notices.append((member.name, text.replace("\r\n", "\n").replace("\r", "\n").strip("\n") + "\n"))
    return {"name": "git", "version": "2.56.0", "license": "GPL-2.0-only (additional component licenses reproduced below)", "source_url": "https://www.kernel.org/pub/software/scm/git/git-2.56.0.tar.xz", "notices": notices}


def metadata_native_root(metadata: Mapping[str, Any]) -> Path | None:
    identifier = metadata.get("resolve", {}).get("root")
    for package in metadata.get("packages", []):
        if package.get("id") == identifier:
            return Path(package["manifest_path"]).parent
    return None


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
    native_components: list[dict[str, Any]] | None = None,
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

    if native_components is None:
        native_root = metadata_native_root(metadata)
        git = git_component(native_root) if native_root is not None else None
        native_components = [git] if git else []
    native_entries = []
    for component in native_components:
        references = []
        for name, text in component["notices"]:
            digest = hashlib.sha256(text.encode("utf-8")).hexdigest()
            identifier = text_ids.get(digest)
            if identifier is None:
                identifier = f"T{len(texts) + 1:04d}"
                text_ids[digest] = identifier
                texts.append((identifier, text))
            references.append(f"  {name}: [{identifier}]")
        native_entries.append("\n".join([
            f"Native-Component: {component['name']} {component['version']}",
            f"License: {component['license']}",
            f"Source: {component['source_url']}",
            "License files:", *references,
        ]))

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
        "executable (QuickJS through rquickjs-sys, and AJV). Native Git and",
        "redistributed native-library notices are listed separately below.",
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
    if native_entries:
        body += ["", "", "Native components (not Cargo packages)", "--------------------------------------", "", "\n\n".join(native_entries)]
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
    native: set[tuple[str, str]] = set()
    for line in lines:
        match = PACKAGE_LINE.fullmatch(line)
        if match:
            packages.add((match.group("name"), match.group("version")))
        match = NATIVE_LINE.fullmatch(line)
        if match:
            native.add((match.group("name"), match.group("version")))
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
        "native_components": native,
    }


def verify_inventory(
    text: str,
    metadata: Mapping[str, Any],
    *,
    target: str,
    no_default_features: bool,
    native_components: list[dict[str, Any]] | None = None,
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
    if native_components is None:
        root = metadata_native_root(metadata)
        git = git_component(root) if root is not None else None
        native_components = [git] if git else []
    missing_native = {(item["name"], item["version"]) for item in native_components} - inventory["native_components"]
    if missing_native:
        raise InventoryError(f"THIRD_PARTY_LICENSES omits native component(s): {sorted(missing_native)}")
    return len(expected)


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("generate", "verify"):
        command = commands.add_parser(name)
        command.add_argument("--root", type=Path, default=ROOT)
        command.add_argument("--target", required=True)
        command.add_argument("--no-default-features", action="store_true")
        command.add_argument("--native-record", type=Path, default=Path(os.environ["MINI_AGENT_GIT_NATIVE_RECORD"]) if os.environ.get("MINI_AGENT_GIT_NATIVE_RECORD") else None, help="verified source/notice record for this target's actual redistributed native dependencies")
        command.add_argument("--git-runtime-bundle", type=Path, default=Path(os.environ["MINI_AGENT_GIT_RUNTIME_BUNDLE"]) if os.environ.get("MINI_AGENT_GIT_RUNTIME_BUNDLE") else None, help="bind native DLL coverage to this actual runtime bundle")
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
        native = []
        git = git_component(args.root)
        if git:
            native.append(git)
        if args.native_record:
            for component in load_native_record(args.native_record, args.target, args.git_runtime_bundle):
                native.append({**component, "notices": [(asset["path"], _read_text(checked_native_asset(args.native_record.parent, asset, MAX_LICENSE_FILE_BYTES))) for asset in component["license_files"]]})
        elif git and ("musl" in args.target or "windows" in args.target) and args.command == "generate":
            raise InventoryError("Windows/static-musl native dependencies require an actual --native-record; Cargo metadata is insufficient")
        if args.command == "generate":
            document = render(
                metadata,
                target=args.target,
                no_default_features=args.no_default_features,
                native_components=native,
            )
            verify_inventory(
                document,
                metadata,
                target=args.target,
                no_default_features=args.no_default_features,
                native_components=native,
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
                native_components=native,
            )
            print(f"{args.inventory} names all {count} resolved packages")
    except (InventoryError, OSError, UnicodeDecodeError, json.JSONDecodeError, tarfile.TarError) as error:
        print(f"third-party license inventory failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
