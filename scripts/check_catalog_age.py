#!/usr/bin/env python3
"""Fail when data/models.generated is older than --max-days (release gate).

The embedded model catalog drives context windows and prices; a release cut
from a stale snapshot ships models without prices (mini-agent-nvnkx.36).
"""
import argparse
import datetime
import pathlib
import sys


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--max-days", type=int, default=45)
    parser.add_argument("--file", default="data/models.generated")
    args = parser.parse_args()
    stamp = pathlib.Path(args.file).read_text().strip()
    generated = datetime.datetime.strptime(stamp, "%Y-%m-%dT%H:%M:%SZ").replace(
        tzinfo=datetime.timezone.utc
    )
    age = datetime.datetime.now(datetime.timezone.utc) - generated
    if age.days > args.max_days:
        print(
            f"model catalog snapshot is {age.days} days old (limit {args.max_days}); "
            "run scripts/gen-models-catalog.sh and review the diff before tagging",
            file=sys.stderr,
        )
        return 1
    print(f"model catalog snapshot is {age.days} days old")
    return 0


if __name__ == "__main__":
    sys.exit(main())
