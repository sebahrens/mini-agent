"""Bounded prerequisite and installed-binary checks for Gym setup."""

from __future__ import annotations

import argparse
import os
import re
import stat
import subprocess
import sys
import tomllib
from pathlib import Path

if __package__:
    from .process_capture import ProcessCleanupError, run_bounded
else:
    from process_capture import ProcessCleanupError, run_bounded

PROBE_TIMEOUT_SECS = 30
VERSION_OUTPUT_BYTES = 16 * 1024
HELP_OUTPUT_BYTES = 1024 * 1024
TOOLCHAIN_BYTES = 64 * 1024


class SetupError(ValueError):
    """A prerequisite or installed binary did not satisfy the setup contract."""


def probe(argv: list[str], repo: Path, limit: int) -> str:
    """Read complete metadata within a deadline; never accept a truncated prefix."""
    try:
        result = run_bounded(argv, repo, dict(os.environ), PROBE_TIMEOUT_SECS, stdout_limit=limit)
    except subprocess.TimeoutExpired as error:
        detail = (error.stderr or b"").decode("utf-8", errors="replace").strip()
        raise SetupError(f"{argv[0]} probe timed out after {PROBE_TIMEOUT_SECS}s: {detail}") from error
    if len(result.stdout) > limit:
        raise SetupError(f"{argv[0]} probe exceeded {limit} stdout bytes")
    if result.returncode:
        detail = result.stderr.decode("utf-8", errors="replace").strip()
        raise SetupError(f"{argv[0]} probe exited {result.returncode}: {detail}")
    return result.stdout.decode("utf-8").strip()


def prerequisites(repo: Path) -> None:
    # O_NONBLOCK prevents a replaced FIFO from stalling before type validation.
    # Symlinks to ordinary toolchain files remain supported.
    descriptor = os.open(repo / "rust-toolchain.toml", os.O_RDONLY | os.O_NONBLOCK)
    try:
        if not stat.S_ISREG(os.fstat(descriptor).st_mode):
            raise SetupError("rust-toolchain.toml must be a regular file")
        with os.fdopen(descriptor, "rb", closefd=False) as handle:
            raw = handle.read(TOOLCHAIN_BYTES + 1)
    finally:
        os.close(descriptor)
    if len(raw) > TOOLCHAIN_BYTES:
        raise SetupError(f"rust-toolchain.toml exceeds {TOOLCHAIN_BYTES} bytes")
    toolchain = tomllib.loads(raw.decode("utf-8")).get("toolchain")
    required = toolchain.get("channel") if isinstance(toolchain, dict) else None
    if not isinstance(required, str) or not required.strip():
        raise SetupError("rust-toolchain.toml requires a nonempty toolchain.channel string")
    rust_version = probe(["rustc", "--version"], repo, VERSION_OUTPUT_BYTES)
    match = re.fullmatch(r"rustc ([^\s]+)(?: \([^\r\n]*\))?", rust_version)
    if match is None:
        raise SetupError(f"cannot parse rustc version: {rust_version[:200]!r}")
    actual = match.group(1)
    if actual != required:
        raise SetupError(f"rustc {actual} does not match rust-toolchain.toml {required}")
    git_version = probe(["git", "version"], repo, VERSION_OUTPUT_BYTES)
    match = re.match(r"git version (\d+)\.(\d+)(?:[.\s]|$)", git_version)
    if match is None:
        raise SetupError(f"cannot parse {git_version[:200]!r}; required 2.40 or newer")
    if tuple(int(part) for part in match.groups()) < (2, 40):
        raise SetupError(f"{git_version[:200]} is older than required 2.40")


def installed_binary(repo: Path, binary: Path) -> None:
    help_text = probe([str(binary), "--help"], repo, HELP_OUTPUT_BYTES)
    if "--install-learned-skill-seeds" not in help_text:
        raise SetupError(
            "installed binary does not advertise --install-learned-skill-seeds: "
            "the gym install lost the skills feature"
        )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("check", choices=("prerequisites", "installed-binary"))
    parser.add_argument("repo", type=Path)
    parser.add_argument("binary", type=Path, nargs="?")
    args = parser.parse_args(argv)
    if args.check == "installed-binary" and args.binary is None:
        parser.error("installed-binary requires the installed executable path")
    try:
        repo = args.repo.resolve()
        if args.check == "prerequisites":
            prerequisites(repo)
        else:
            installed_binary(repo, args.binary.resolve())
    except (OSError, ValueError, ProcessCleanupError) as error:
        print(f"gym setup: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
