#!/usr/bin/env python3
"""Report packaging recipes that still carry all-zero placeholder digests.

A recipe with a placeholder sha256 cannot install, so no instruction may
advertise it as a working channel. Default mode lists them and exits 0 (the
in-repo recipes are templates until a release fills them); `--require-filled`
exits 1 when any remain, for use as a gate before a recipe is published.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RECIPES = (
    "packaging/homebrew/zerostack.rb",
    "packaging/aur/PKGBUILD",
    "packaging/conda/zerostack/meta.yaml",
    "packaging/conda/zerostack-bin/meta.yaml",
)
PLACEHOLDER = re.compile(r"(?<![0-9a-fA-F])0{64}(?![0-9a-fA-F])")


def placeholder_counts(root: Path = ROOT) -> dict:
    counts = {}
    for name in RECIPES:
        path = root / name
        if not path.is_file():
            raise SystemExit(f"missing recipe: {name}")
        counts[name] = len(PLACEHOLDER.findall(path.read_text()))
    return counts


def main(argv) -> int:
    strict = "--require-filled" in argv
    unknown = [arg for arg in argv if arg != "--require-filled"]
    if unknown:
        print(f"usage: check_recipe_digests.py [--require-filled]", file=sys.stderr)
        return 2
    counts = placeholder_counts()
    unfilled = {name: count for name, count in counts.items() if count}
    for name, count in unfilled.items():
        print(f"{name}: {count} placeholder sha256 digest(s)")
    if unfilled and strict:
        print("recipes with placeholder digests cannot be published", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
