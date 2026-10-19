#!/usr/bin/env python3
"""Prune stale Cargo artefacts from a local target directory.

Every feature row, profile and RUSTFLAGS variant keeps its own hashed artefacts
under target/<profile>/, and Cargo never prunes them. This script removes only:

* incremental compilation directories (``<profile>/incremental/<unit>-<hash>``)
  whose newest file is older than the cutoff, and
* mini-agent's own hashed outputs (``deps/mini_agent-*``, ``deps/libmini_agent-*``
  and ``.fingerprint/mini-agent-*``) older than the cutoff.

Registry dependencies are left alone (use ``cargo sweep --time N`` to prune
those). Nothing is deleted unless ``--apply`` is given; the default is a dry run
that prints what would be removed and how much space it would free.

    python3 scripts/sweep-target.py                 # dry run, 14 days
    python3 scripts/sweep-target.py --days 7 --apply
"""

from __future__ import annotations

import argparse
import os
import shutil
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PROFILE_DIRS = ("debug", "release")
OWN_PREFIXES = ("mini_agent-", "libmini_agent-")
OWN_FINGERPRINT_PREFIX = "mini-agent-"


def tree_stats(path: Path) -> tuple[int, float]:
    """Return (bytes, newest file mtime) for a file or directory, without following links."""
    if path.is_symlink() or path.is_file():
        info = path.lstat()
        return info.st_size, info.st_mtime
    size = 0
    newest = None
    for current, _directories, files in os.walk(path, followlinks=False):
        for name in files:
            try:
                info = os.lstat(os.path.join(current, name))
            except OSError:
                continue
            size += info.st_size
            newest = info.st_mtime if newest is None else max(newest, info.st_mtime)
    if newest is None:  # a directory holding no files: fall back to its own time
        newest = path.lstat().st_mtime
    return size, newest


def candidates(target: Path):
    for profile in PROFILE_DIRS:
        base = target / profile
        incremental = base / "incremental"
        if incremental.is_dir():
            yield from sorted(incremental.iterdir())
        deps = base / "deps"
        if deps.is_dir():
            yield from sorted(p for p in deps.iterdir() if p.name.startswith(OWN_PREFIXES))
        fingerprint = base / ".fingerprint"
        if fingerprint.is_dir():
            yield from sorted(
                p for p in fingerprint.iterdir() if p.name.startswith(OWN_FINGERPRINT_PREFIX)
            )


def select_stale(target: Path, days: float, now: float | None = None):
    """Return [(path, bytes)] for entries whose newest mtime is older than ``days``."""
    cutoff = (time.time() if now is None else now) - days * 86400
    stale = []
    for path in candidates(target):
        size, newest = tree_stats(path)
        if newest < cutoff:
            stale.append((path, size))
    return stale


def remove(path: Path) -> None:
    if path.is_symlink() or path.is_file():
        path.unlink()
    else:
        shutil.rmtree(path)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--target-dir", type=Path, default=ROOT / "target")
    parser.add_argument("--days", type=float, default=14.0, help="age cutoff (default 14)")
    parser.add_argument("--apply", action="store_true", help="delete (default is a dry run)")
    args = parser.parse_args(argv)
    if args.days < 0:
        parser.error("--days must not be negative")
    target = args.target_dir.resolve()
    if not target.is_dir():
        print(f"no target directory at {target}", file=sys.stderr)
        return 1
    stale = select_stale(target, args.days)
    total = sum(size for _, size in stale)
    verb = "removing" if args.apply else "would remove"
    for path, size in stale:
        print(f"{verb} {path.relative_to(target)} ({size / 1e6:.1f} MB)")
        if args.apply:
            remove(path)
    mode = "removed" if args.apply else "would free"
    print(f"{len(stale)} entries older than {args.days:g} days; {mode} {total / 1e9:.2f} GB")
    if not args.apply and stale:
        print("dry run: pass --apply to delete")
    return 0


if __name__ == "__main__":
    sys.exit(main())
