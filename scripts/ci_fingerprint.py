#!/usr/bin/env python3
"""Per-job pass fingerprints for CI (mini-agent-m7wn5).

GitHub results are per commit, so a one-line test fix re-runs jobs whose inputs
are identical to an already-green commit. A job fingerprint is the SHA-256 of
the tracked files in the job's input class plus the job id, feature row,
runner OS/image/arch, toolchain, Cargo.lock and the workflow file:

* `production`: every tracked file except documentation that nothing embeds or
  reads, and test-only paths (src/tests/**, *_tests.rs, src/**/tests/**,
  tests/**).
* `production+tests`: the same without removing test-only paths, for jobs that
  run or lint tests.

After a successful job on a push to main the composite action saves a marker
under `ci-pass-<job>-<fingerprint>`; later runs restore it lookup-only and skip
the job's remaining steps on a hit. Tags, the weekly schedule and manual
dispatches never read or write markers, and pull requests only read them.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import subprocess
import sys
from pathlib import Path

try:
    from scripts import ci_scope
except ImportError:  # executed as a script: scripts/ is sys.path[0]
    import ci_scope  # type: ignore[no-redef]

CLASSES = ("production", "production+tests")
PINNED_FILES = ("Cargo.lock", "rust-toolchain.toml", ".github/workflows/ci.yml")


def tracked_files(root: str = ".") -> list[tuple[str, str]]:
    """(path, git object id) of every tracked file."""

    out = subprocess.run(
        ["git", "ls-files", "-s", "-z"],
        cwd=root,
        capture_output=True,
        check=True,
    ).stdout.decode("utf-8", "surrogateescape")
    entries = []
    for record in filter(None, out.split("\0")):
        meta, _, path = record.partition("\t")
        entries.append((path, meta.split()[1]))
    return sorted(entries)


def select(entries: list[tuple[str, str]], input_class: str) -> list[tuple[str, str]]:
    if input_class not in CLASSES:
        raise ValueError(f"unknown input class {input_class!r}")
    chosen = [(p, oid) for p, oid in entries if not ci_scope.is_docs(p)]
    if input_class == "production":
        chosen = [(p, oid) for p, oid in chosen if not ci_scope.is_test_path(p)]
    return chosen


def fingerprint(
    entries: list[tuple[str, str]],
    *,
    job: str,
    input_class: str,
    row: str = "",
    os_name: str = "",
    image: str = "",
    arch: str = "",
    pinned: dict[str, bytes] | None = None,
) -> str:
    digest = hashlib.sha256()

    def feed(label: str, value: str | bytes) -> None:
        data = value if isinstance(value, bytes) else value.encode("utf-8", "surrogateescape")
        digest.update(label.encode() + b"\0" + str(len(data)).encode() + b"\0" + data + b"\0")

    feed("job", job)
    feed("class", input_class)
    feed("row", row)
    feed("os", os_name)
    feed("image", image)
    feed("arch", arch)
    for name in PINNED_FILES:
        feed("pinned:" + name, (pinned or {}).get(name, b""))
    for path, oid in select(entries, input_class):
        feed("file", f"{path}\0{oid}")
    return digest.hexdigest()


def marker_policy(event: str, ref: str) -> tuple[bool, bool]:
    """(may_read, may_write) for a workflow event; tags/schedule/dispatch never."""

    if ref.startswith("refs/tags/"):
        return False, False
    if event == "pull_request":
        return True, False
    if event == "push":
        return True, ref == "refs/heads/main"
    return False, False


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    compute = sub.add_parser("compute")
    compute.add_argument("--job", required=True)
    compute.add_argument("--classes", required=True, choices=CLASSES)
    compute.add_argument("--row", default="")
    compute.add_argument("--event", default=os.environ.get("GITHUB_EVENT_NAME", ""))
    compute.add_argument("--ref", default=os.environ.get("GITHUB_REF", ""))
    compute.add_argument("--os", dest="os_name", default=os.environ.get("RUNNER_OS", ""))
    compute.add_argument("--image", default=os.environ.get("ImageOS", ""))
    compute.add_argument("--arch", default=os.environ.get("RUNNER_ARCH", ""))
    marker = sub.add_parser("marker")
    marker.add_argument("path")
    marker.add_argument("--key", required=True)
    args = parser.parse_args(argv)

    if args.command == "marker":
        Path(args.path).parent.mkdir(parents=True, exist_ok=True)
        Path(args.path).write_text(args.key + "\n", encoding="utf-8")
        return 0

    may_read, may_write = marker_policy(args.event, args.ref)
    values = {"read": "false", "write": "false", "fingerprint": "", "key": ""}
    if may_read:
        try:
            pinned = {n: Path(n).read_bytes() for n in PINNED_FILES if Path(n).is_file()}
            fp = fingerprint(
                tracked_files(),
                job=args.job,
                input_class=args.classes,
                row=args.row,
                os_name=args.os_name,
                image=args.image,
                arch=args.arch,
                pinned=pinned,
            )
            values.update(
                read="true",
                write=str(may_write).lower(),
                fingerprint=fp,
                key=f"ci-pass-{args.job}-{fp}",
            )
        except (OSError, subprocess.CalledProcessError, ValueError) as error:
            # Fail open: no fingerprint means the job simply runs.
            print(f"::warning::fingerprint unavailable ({error}); running the job")
    text = "".join(f"{k}={v}\n" for k, v in values.items())
    print(text, end="")
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with open(output, "a", encoding="utf-8") as handle:
            handle.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
