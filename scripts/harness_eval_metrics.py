#!/usr/bin/env python3
"""Extract harness-evaluation metric records from a libtest log.

libtest prints its progress line and the test's captured output on the same
line, so the first record of a test arrives as

    test tests::harness_eval_tests::persona_regression_eval ... PERSONA_EVAL {...}

A line-anchored ``grep '^PERSONA_EVAL '`` silently drops exactly that record,
which is how a nightly run with nine passing personas produced eight records
and failed its own count check. Records are located by their marker anywhere in
the line, validated as JSON with the marker's required keys, and written one
per line.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Iterable

#: Keys every record of a marker must carry. Extra keys are preserved.
REQUIRED_KEYS = {
    "HARNESS_EVAL": ("name", "success", "provider_turns", "tool_calls", "total_tokens"),
    "PERSONA_EVAL": ("agent_type", "success", "expected_finding"),
}


class ExtractionError(Exception):
    """A record was missing, malformed, or failed its schema."""


def extract_records(lines: Iterable[str], marker: str) -> list[dict]:
    """Every ``marker`` record in ``lines``, in order.

    The marker is matched anywhere in the line so a libtest progress prefix
    cannot hide a record; everything before it is discarded.
    """
    needle = f"{marker} "
    records: list[dict] = []
    required = REQUIRED_KEYS.get(marker, ())
    for number, line in enumerate(lines, start=1):
        position = line.find(needle)
        if position < 0:
            continue
        payload = line[position + len(needle) :].strip()
        try:
            record = json.loads(payload)
        except json.JSONDecodeError as error:
            raise ExtractionError(
                f"line {number}: {marker} record is not valid JSON: {error}"
            ) from error
        if not isinstance(record, dict):
            raise ExtractionError(f"line {number}: {marker} record is not a JSON object")
        missing = [key for key in required if key not in record]
        if missing:
            raise ExtractionError(
                f"line {number}: {marker} record is missing {', '.join(missing)}"
            )
        records.append(record)
    return records


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--marker", required=True, help="record marker, e.g. PERSONA_EVAL")
    parser.add_argument("--log", required=True, type=Path, help="libtest output to read")
    parser.add_argument("--out", required=True, type=Path, help="JSONL file to write")
    parser.add_argument(
        "--expect",
        type=int,
        default=None,
        help="exact number of records the run must produce",
    )
    parser.add_argument(
        "--min",
        type=int,
        default=None,
        help="minimum number of records the run must produce",
    )
    parser.add_argument(
        "--require",
        action="append",
        default=[],
        metavar="KEY=VALUE",
        help="require at least one record whose KEY equals VALUE",
    )
    args = parser.parse_args(argv)

    text = args.log.read_text(encoding="utf-8", errors="replace")
    try:
        records = extract_records(text.splitlines(), args.marker)
    except ExtractionError as error:
        print(f"harness metrics: {error}", file=sys.stderr)
        return 1

    if args.expect is not None and len(records) != args.expect:
        print(
            f"harness metrics: expected {args.expect} {args.marker} records, found {len(records)}",
            file=sys.stderr,
        )
        return 1
    if args.min is not None and len(records) < args.min:
        print(
            f"harness metrics: expected at least {args.min} {args.marker} records, "
            f"found {len(records)}",
            file=sys.stderr,
        )
        return 1
    for requirement in args.require:
        key, _, value = requirement.partition("=")
        if not any(str(record.get(key)) == value for record in records):
            print(
                f"harness metrics: no {args.marker} record has {key}={value}",
                file=sys.stderr,
            )
            return 1

    args.out.parent.mkdir(parents=True, exist_ok=True)
    with args.out.open("w", encoding="utf-8") as handle:
        for record in records:
            handle.write(json.dumps(record, sort_keys=True))
            handle.write("\n")
    print(f"harness metrics: wrote {len(records)} {args.marker} records to {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
