#!/usr/bin/env python3
"""Extract one version's release notes from a Keep a Changelog document."""

from __future__ import annotations

import argparse
import re
from pathlib import Path


def extract_version(text: str, version: str) -> str:
    heading = re.compile(rf"^## \[{re.escape(version)}\](?: - .+)?$")
    lines: list[str] = []
    found = False
    for line in text.splitlines():
        if heading.fullmatch(line):
            found = True
            continue
        if found and line.startswith("## ["):
            break
        if found and re.match(r"^\[[^]]+\]:\s+", line):
            break
        if found:
            lines.append(line)

    notes = "\n".join(lines).strip()
    if not found or not notes:
        raise ValueError(f"CHANGELOG.md has no release notes for {version}")
    return f"{notes}\n"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--changelog", type=Path, default=Path("CHANGELOG.md"))
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    try:
        notes = extract_version(args.changelog.read_text(encoding="utf-8"), args.version)
    except (OSError, ValueError) as error:
        parser.error(str(error))
    args.output.write_text(notes, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
