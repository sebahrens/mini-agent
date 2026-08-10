#!/usr/bin/env python3
"""Create a release binary archive with its required GPL documents."""

from __future__ import annotations

import argparse
import hashlib
import os
import tarfile
from pathlib import Path


REQUIRED_DOCUMENTS = ("LICENSE", "NOTICE", "SOURCE.md")
CANONICAL_GPL3_LICENSE_SHA256 = (
    "3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986"
)


def _normalized(info: tarfile.TarInfo, *, executable: bool) -> tarfile.TarInfo:
    info.uid = 0
    info.gid = 0
    info.uname = "root"
    info.gname = "root"
    info.mtime = int(os.environ.get("SOURCE_DATE_EPOCH", "0"))
    info.mode = 0o755 if executable else 0o644
    return info


def package_binary(
    *, root: Path, binary: Path, archive: Path, executable_name: str
) -> None:
    root = root.resolve()
    binary = binary.resolve()
    archive = archive.resolve()

    if not binary.is_file():
        raise ValueError(f"release binary is missing or not a file: {binary}")
    if Path(executable_name).name != executable_name or executable_name in {"", ".", ".."}:
        raise ValueError("executable name must be a single safe archive member")

    documents = [root / name for name in REQUIRED_DOCUMENTS]
    missing = [path.name for path in documents if not path.is_file()]
    if missing:
        raise ValueError(f"required release documents are missing: {', '.join(missing)}")
    license_digest = hashlib.sha256((root / "LICENSE").read_bytes()).hexdigest()
    if license_digest != CANONICAL_GPL3_LICENSE_SHA256:
        raise ValueError("LICENSE is not the canonical GPL-3.0-only text")

    archive.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive, mode="w:gz", format=tarfile.PAX_FORMAT) as output:
        output.add(
            binary,
            arcname=executable_name,
            recursive=False,
            filter=lambda info: _normalized(info, executable=True),
        )
        for document in documents:
            output.add(
                document,
                arcname=document.name,
                recursive=False,
                filter=lambda info: _normalized(info, executable=False),
            )

    with tarfile.open(archive, mode="r:gz") as packaged:
        members = packaged.getmembers()
    expected = [executable_name, *REQUIRED_DOCUMENTS]
    observed = [member.name for member in members]
    if observed != expected or any(not member.isfile() for member in members):
        archive.unlink(missing_ok=True)
        raise ValueError(
            f"release archive payload mismatch: expected {expected}, observed {observed}"
        )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--executable-name", required=True)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        package_binary(
            root=args.root,
            binary=args.binary,
            archive=args.archive,
            executable_name=args.executable_name,
        )
    except (OSError, ValueError, tarfile.TarError) as error:
        raise SystemExit(f"error: {error}") from error
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
