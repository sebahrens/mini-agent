#!/usr/bin/env python3
"""Capture publisher-built Git files into a target-specific embedding bundle.

This inventories already-built files. The trusted source/build pipeline must
produce those files from the pinned archive and repository policy patch; this
utility does not attest a compiler invocation or build missing target binaries.
"""

from __future__ import annotations

import argparse
import hashlib
import shutil
from pathlib import Path

SOURCE_SHA256 = "26c56c296b38c0695b26fa95f475f1d01704d2d38e73465ca30b0b2f5dc789d3"
POLICY = "mini-agent-executable-scope-v3-env"
TARGETS = (
    "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin", "aarch64-apple-darwin", "x86_64-pc-windows-msvc",
)
MAX_FILE = 64 * 1024 * 1024
MAX_TOTAL = 256 * 1024 * 1024


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def read_bounded(path: Path, limit: int) -> bytes:
    # These are explicit trusted build inputs, not runtime/model-selected paths.
    with path.open("rb") as source:
        if not path.is_file():
            raise ValueError("Git build input is not a regular file")
        result = source.read(limit + 1)
    if len(result) > limit:
        raise ValueError("Git build input exceeds its size bound")
    return result


def safe_leaf(name: str) -> bool:
    stem = name.split(".", 1)[0].upper()
    return (
        0 < len(name) <= 128 and name[0].isascii() and name[0].isalnum()
        and all(c.isascii() and (c.isalnum() or c in "._-") for c in name)
        and ".." not in name and not name.endswith(".")
        and stem not in {"CON", "PRN", "AUX", "NUL"}
        and not (len(stem) == 4 and stem[:3] in {"COM", "LPT"} and stem[3] in "123456789")
    )


def prepare(
    *, root: Path, target: str, source_archive: Path, git: Path,
    http_helper: Path, https_helper: Path | None, libraries: list[Path], output: Path,
) -> str:
    if target not in TARGETS:
        raise ValueError("Unsupported Git runtime target")
    if digest(read_bounded(source_archive, 32 * 1024 * 1024)) != SOURCE_SHA256:
        raise ValueError("Git source archive does not match the pinned source")
    policy_hash = digest(read_bounded(root / "scripts/git-runtime/policy.patch", 1024 * 1024))
    suffix = ".exe" if "windows" in target else ""
    inputs = [
        ("git" + suffix, git, "x"),
        ("git-remote-http" + suffix, http_helper, "x"),
        ("git-remote-https" + suffix, https_helper or http_helper, "x"),
        *((library.name, library, "r") for library in libraries),
    ]
    if len(inputs) > 128:
        raise ValueError("Too many Git runtime files")
    names: set[str] = set()
    captured: list[tuple[str, bytes, str]] = []
    total = 0
    for name, path, mode in inputs:
        if not safe_leaf(name) or name.lower() in names:
            raise ValueError("Unsafe or duplicate Git runtime filename")
        names.add(name.lower())
        data = read_bounded(path, MAX_FILE)
        total += len(data)
        if total > MAX_TOTAL or (mode == "x" and not data):
            raise ValueError("Invalid Git runtime payload size")
        captured.append((name, data, mode))
    text = (
        f"mini-agent-git-runtime-v1\ntarget={target}\nsource-sha256={SOURCE_SHA256}\n"
        f"policy-sha256={policy_hash}\npolicy={POLICY}\n"
    )
    # Exclusive creation means cleanup can never adopt or remove another bundle.
    output.mkdir(mode=0o700)
    try:
        for name, data, mode in captured:
            path = output / name
            with path.open("xb") as destination:
                destination.write(data)
            path.chmod(0o700 if mode == "x" else 0o600)
            text += f"file={name}\t{digest(data)}\t{mode}\n"
        manifest = output / "bundle.manifest"
        manifest.write_text(text, encoding="utf-8", newline="\n")
        manifest.chmod(0o600)
    except BaseException:
        shutil.rmtree(output)
        raise
    return digest(text.encode())


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, choices=TARGETS)
    parser.add_argument("--source-archive", type=Path, required=True)
    parser.add_argument("--git", type=Path, required=True)
    parser.add_argument("--http-helper", type=Path, required=True)
    parser.add_argument("--https-helper", type=Path)
    parser.add_argument("--library", type=Path, action="append", default=[])
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    print(prepare(root=Path(__file__).resolve().parents[1], target=args.target,
                  source_archive=args.source_archive, git=args.git,
                  http_helper=args.http_helper, https_helper=args.https_helper,
                  libraries=args.library, output=args.output))


if __name__ == "__main__":
    main()
