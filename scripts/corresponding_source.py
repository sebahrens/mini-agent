#!/usr/bin/env python3
"""Helpers for the GPL Corresponding Source archive.

``package-corresponding-source.sh`` uses this module to vendor the VS Code
extension's locked npm dependency tarballs, to verify them against
``editors/vscode/package-lock.json``, and to write a byte-reproducible
``.tar.gz`` (sorted entries, fixed mtimes, normalized ownership and modes).
"""

from __future__ import annotations

import argparse
import base64
import concurrent.futures
import gzip
import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
from dataclasses import dataclass
from pathlib import Path

MANIFEST_NAME = "npm-sources.json"
MANIFEST_SCHEMA = 1
SRI_ALGORITHMS = ("sha512", "sha384", "sha256", "sha1")
PACK_BATCH_SIZE = 16
PACK_JOBS = 8


class SourceError(Exception):
    """A fail-closed Corresponding Source packaging error."""


@dataclass(frozen=True)
class LockedPackage:
    path: str
    name: str
    version: str
    resolved: str
    integrity: str
    dev: bool
    optional: bool

    @property
    def file(self) -> str:
        return tarball_file_name(self.name, self.version)


def tarball_file_name(name: str, version: str) -> str:
    """Return npm pack's file name for ``name@version``."""
    return f"{name.removeprefix('@').replace('/', '-')}-{version}.tgz"


def package_name_from_path(path: str) -> str:
    marker = "node_modules/"
    index = path.rfind(marker)
    if index < 0:
        raise SourceError(f"lockfile package path is not under node_modules: {path}")
    return path[index + len(marker) :]


def locked_packages(lockfile: Path) -> list[LockedPackage]:
    """Return every registry package pinned by an npm v2/v3 lockfile, sorted by path."""
    try:
        lock = json.loads(lockfile.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise SourceError(f"cannot read npm lockfile {lockfile}: {error}") from error
    if lock.get("lockfileVersion") not in (2, 3):
        raise SourceError(f"{lockfile} must use npm lockfileVersion 2 or 3")
    packages = lock.get("packages")
    if not isinstance(packages, dict):
        raise SourceError(f"{lockfile} has no packages table")

    result: list[LockedPackage] = []
    for path in sorted(packages):
        if path == "":
            continue
        entry = packages[path]
        if entry.get("link"):
            raise SourceError(f"linked lockfile package is not supported: {path}")
        resolved = entry.get("resolved")
        integrity = entry.get("integrity")
        version = entry.get("version")
        if not resolved or not integrity or not version:
            raise SourceError(
                f"lockfile package {path} lacks resolved, integrity, or version; "
                "its source cannot be vendored and verified"
            )
        if not resolved.startswith("https://"):
            raise SourceError(f"lockfile package {path} does not resolve to an https tarball")
        result.append(
            LockedPackage(
                path=path,
                name=entry.get("name") or package_name_from_path(path),
                version=version,
                resolved=resolved,
                integrity=integrity,
                dev=bool(entry.get("dev", False)),
                optional=bool(entry.get("optional", False)),
            )
        )
    return result


def strongest_hash(integrity: str) -> tuple[str, str]:
    """Return the strongest supported ``(algorithm, base64 digest)`` of an SRI string."""
    hashes: dict[str, list[str]] = {}
    for token in integrity.split():
        algorithm, separator, digest = token.partition("-")
        if separator and algorithm in SRI_ALGORITHMS:
            hashes.setdefault(algorithm, []).append(digest.split("?", 1)[0])
    for algorithm in SRI_ALGORITHMS:
        if algorithm in hashes:
            return algorithm, hashes[algorithm][0]
    raise SourceError(f"unsupported integrity value: {integrity!r}")


def file_integrity(path: Path, algorithm: str) -> str:
    digest = hashlib.new(algorithm)
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return base64.b64encode(digest.digest()).decode("ascii")


def matches_integrity(path: Path, integrity: str) -> bool:
    algorithm, expected = strongest_hash(integrity)
    return file_integrity(path, algorithm) == expected


def unique_tarballs(packages: list[LockedPackage]) -> dict[str, LockedPackage]:
    """Map each tarball file name to one locked package, rejecting collisions."""
    by_file: dict[str, LockedPackage] = {}
    for package in packages:
        previous = by_file.get(package.file)
        if previous is None:
            by_file[package.file] = package
        elif previous.integrity != package.integrity or previous.resolved != package.resolved:
            raise SourceError(
                f"lockfile packages {previous.path} and {package.path} both map to "
                f"{package.file} with different sources"
            )
    return by_file


def run_npm_pack(npm: str, urls: list[str], destination: Path) -> None:
    command = [
        npm,
        "pack",
        "--ignore-scripts",
        "--silent",
        "--pack-destination",
        str(destination),
        *urls,
    ]
    result = subprocess.run(command, capture_output=True, text=True)
    if result.returncode != 0:
        raise SourceError(
            f"npm pack failed with exit status {result.returncode}: {result.stderr.strip()}"
        )


def manifest_for(lockfile: Path, packages: list[LockedPackage]) -> dict[str, object]:
    return {
        "schema": MANIFEST_SCHEMA,
        "lockfile": "editors/vscode/package-lock.json",
        "lockfile_sha256": hashlib.sha256(lockfile.read_bytes()).hexdigest(),
        "packages": [
            {
                "path": package.path,
                "name": package.name,
                "version": package.version,
                "resolved": package.resolved,
                "integrity": package.integrity,
                "file": package.file,
                "dev": package.dev,
                "optional": package.optional,
            }
            for package in packages
        ],
    }


def vendor_npm(
    lockfile: Path,
    destination: Path,
    npm: str,
    mtime: int,
    jobs: int = PACK_JOBS,
) -> list[LockedPackage]:
    """Fetch every locked npm tarball with ``npm pack`` and verify its integrity."""
    packages = locked_packages(lockfile)
    by_file = unique_tarballs(packages)
    by_url = {package.resolved: package for package in by_file.values()}
    if destination.exists():
        raise SourceError(f"npm vendor destination already exists: {destination}")

    with tempfile.TemporaryDirectory(prefix="npm-pack-") as scratch:
        scratch_root = Path(scratch)
        urls = sorted(by_url)
        batches = [urls[i : i + PACK_BATCH_SIZE] for i in range(0, len(urls), PACK_BATCH_SIZE)]
        batch_dirs = []
        for index in range(len(batches)):
            batch_dir = scratch_root / f"batch-{index:05d}"
            batch_dir.mkdir()
            batch_dirs.append(batch_dir)
        with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, jobs)) as pool:
            futures = [
                pool.submit(run_npm_pack, npm, batch, batch_dir)
                for batch, batch_dir in zip(batches, batch_dirs)
            ]
            for future in futures:
                future.result()

        destination.mkdir(parents=True)
        # npm names downloaded tarballs from their embedded package.json, so
        # match them to lockfile entries by content hash, never by file name.
        remaining = {
            strongest_hash(package.integrity): file for file, package in by_file.items()
        }
        algorithms = sorted({algorithm for algorithm, _ in remaining})
        for batch_dir in batch_dirs:
            for produced in sorted(batch_dir.iterdir()):
                matched = None
                for algorithm in algorithms:
                    key = (algorithm, file_integrity(produced, algorithm))
                    if key in remaining:
                        matched = key
                        break
                if matched is None:
                    raise SourceError(
                        f"npm pack produced {produced.name}, which matches no "
                        "package-lock.json integrity hash"
                    )
                shutil.move(str(produced), destination / remaining.pop(matched))
        if remaining:
            missing = ", ".join(sorted(remaining.values()))
            raise SourceError(f"npm pack did not produce locked tarballs: {missing}")

    manifest = destination / MANIFEST_NAME
    manifest.write_text(
        json.dumps(manifest_for(lockfile, packages), indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    for entry in [destination, *destination.iterdir()]:
        os.utime(entry, (mtime, mtime))
    verify_npm(lockfile, destination)
    return packages


def verify_npm(lockfile: Path, destination: Path) -> list[LockedPackage]:
    """Check that ``destination`` holds exactly the lockfile's tarballs, by integrity."""
    packages = locked_packages(lockfile)
    by_file = unique_tarballs(packages)
    manifest_path = destination / MANIFEST_NAME
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise SourceError(f"cannot read npm source manifest {manifest_path}: {error}") from error
    if manifest != manifest_for(lockfile, packages):
        raise SourceError(f"{manifest_path} does not match {lockfile}")

    present = {entry.name for entry in destination.iterdir() if entry.name != MANIFEST_NAME}
    expected = set(by_file)
    if present != expected:
        missing = sorted(expected - present)
        extra = sorted(present - expected)
        raise SourceError(f"vendored npm sources differ from lockfile: missing={missing} extra={extra}")
    for file, package in sorted(by_file.items()):
        tarball = destination / file
        if not tarball.is_file() or tarball.is_symlink():
            raise SourceError(f"vendored npm source is not a regular file: {file}")
        if not matches_integrity(tarball, package.integrity):
            raise SourceError(f"{file} does not match package-lock.json integrity for {package.path}")
    return packages


def normalized_tarinfo(path: Path, arcname: str, mtime: int) -> tarfile.TarInfo:
    info = tarfile.TarInfo(arcname)
    status = path.lstat()
    info.mtime = mtime
    info.uid = info.gid = 0
    info.uname = info.gname = ""
    if stat.S_ISLNK(status.st_mode):
        info.type = tarfile.SYMTYPE
        info.linkname = os.readlink(path)
        info.mode = 0o777
    elif stat.S_ISDIR(status.st_mode):
        info.type = tarfile.DIRTYPE
        info.mode = 0o755
    elif stat.S_ISREG(status.st_mode):
        info.type = tarfile.REGTYPE
        info.size = status.st_size
        info.mode = 0o755 if status.st_mode & 0o111 else 0o644
    else:
        raise SourceError(f"unsupported file type in source tree: {path}")
    return info


def deterministic_tar(parent: Path, root: str, output: Path, mtime: int) -> None:
    """Write ``parent/root`` as a reproducible gzip-compressed tar archive."""
    source = parent / root
    if not source.is_dir():
        raise SourceError(f"source root is not a directory: {source}")
    paths = [source]
    for directory, dirnames, filenames in os.walk(source):
        dirnames.sort()
        base = Path(directory)
        for name in dirnames:
            paths.append(base / name)
        for name in filenames:
            paths.append(base / name)
    entries = sorted((path.relative_to(parent).as_posix(), path) for path in paths)
    with output.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0, compresslevel=9) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as archive:
                for arcname, path in entries:
                    info = normalized_tarinfo(path, arcname, mtime)
                    if info.type == tarfile.REGTYPE:
                        with path.open("rb") as handle:
                            archive.addfile(info, handle)
                    else:
                        archive.addfile(info)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)

    vendor = commands.add_parser("vendor-npm", help="vendor and verify locked npm tarballs")
    vendor.add_argument("lockfile", type=Path)
    vendor.add_argument("destination", type=Path)
    vendor.add_argument("--npm", default="npm")
    vendor.add_argument("--mtime", type=int, required=True)
    vendor.add_argument("--jobs", type=int, default=PACK_JOBS)

    verify = commands.add_parser("verify-npm", help="verify vendored npm tarballs")
    verify.add_argument("lockfile", type=Path)
    verify.add_argument("destination", type=Path)

    archive = commands.add_parser("tar", help="write a reproducible .tar.gz")
    archive.add_argument("parent", type=Path)
    archive.add_argument("root")
    archive.add_argument("output", type=Path)
    archive.add_argument("--mtime", type=int, required=True)

    arguments = parser.parse_args(argv)
    try:
        if arguments.command == "vendor-npm":
            packages = vendor_npm(
                arguments.lockfile,
                arguments.destination,
                arguments.npm,
                arguments.mtime,
                arguments.jobs,
            )
            print(f"vendored {len(unique_tarballs(packages))} npm tarballs for {len(packages)} locked packages")
        elif arguments.command == "verify-npm":
            verify_npm(arguments.lockfile, arguments.destination)
        else:
            deterministic_tar(arguments.parent, arguments.root, arguments.output, arguments.mtime)
    except SourceError as error:
        print(f"Error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
