#!/usr/bin/env python3
"""Validate, smoke, and checksum private release artifacts."""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import subprocess
import sys
import tarfile
import tempfile
from collections.abc import Mapping
from pathlib import Path


SAFE_NAME = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._+-]*$")
SHA256_LINE = re.compile(r"^([0-9a-f]{64})  ([A-Za-z0-9][A-Za-z0-9._+-]*)$")
REQUIRED_DOCUMENTS = ("LICENSE", "NOTICE", "SOURCE.md")
WINDOWS_PRIVATE_DIRECTORY_SCRIPT = r"""
$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$acl = New-Object Security.AccessControl.DirectorySecurity
$acl.SetOwner($identity.User)
$acl.SetAccessRuleProtection($true, $false)
$inheritance = [Security.AccessControl.InheritanceFlags]::ContainerInherit -bor `
    [Security.AccessControl.InheritanceFlags]::ObjectInherit
foreach ($sidValue in @($identity.User.Value, 'S-1-5-18', 'S-1-5-32-544')) {
    $sid = New-Object Security.Principal.SecurityIdentifier($sidValue)
    $rule = New-Object Security.AccessControl.FileSystemAccessRule(
        $sid,
        [Security.AccessControl.FileSystemRights]::FullControl,
        $inheritance,
        [Security.AccessControl.PropagationFlags]::None,
        [Security.AccessControl.AccessControlType]::Allow
    )
    [void]$acl.AddAccessRule($rule)
}
[IO.Directory]::SetAccessControl(
    $env:MINI_AGENT_RELEASE_SMOKE_DIRECTORY,
    $acl
)
"""


class ReleaseArtifactError(ValueError):
    """A release candidate failed a closed validation gate."""


def read_expected(path: Path) -> list[str]:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError as error:
        raise ReleaseArtifactError(f"cannot read expected artifact set: {error}") from error
    if not lines:
        raise ReleaseArtifactError("expected artifact set is empty")
    if any(not SAFE_NAME.fullmatch(name) for name in lines):
        raise ReleaseArtifactError("expected artifact set contains an unsafe filename")
    if len(lines) != len(set(lines)):
        raise ReleaseArtifactError("expected artifact set contains a duplicate filename")
    return sorted(lines)


def collect_candidates(root: Path) -> dict[str, Path]:
    if not root.is_dir():
        raise ReleaseArtifactError(f"artifact root is not a directory: {root}")
    candidates: dict[str, Path] = {}
    for candidate in sorted(root.rglob("*")):
        if candidate.is_symlink():
            raise ReleaseArtifactError(f"artifact candidate is a symlink: {candidate}")
        if candidate.is_dir():
            continue
        if not candidate.is_file():
            raise ReleaseArtifactError(f"artifact candidate is not a regular file: {candidate}")
        name = candidate.name
        if not SAFE_NAME.fullmatch(name):
            raise ReleaseArtifactError(f"unsafe artifact filename: {name!r}")
        if name in candidates:
            raise ReleaseArtifactError(f"duplicate artifact filename: {name}")
        candidates[name] = candidate
    return candidates


def validate_candidate_set(root: Path, expected: list[str]) -> dict[str, Path]:
    candidates = collect_candidates(root)
    observed = set(candidates)
    required = set(expected)
    missing = sorted(required - observed)
    extra = sorted(observed - required)
    if missing or extra:
        raise ReleaseArtifactError(
            f"artifact set mismatch: missing={missing}, unexpected={extra}"
        )
    return candidates


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def build_manifest(root: Path, expected_file: Path, output: Path) -> None:
    expected = read_expected(expected_file)
    candidates = validate_candidate_set(root, expected)
    manifest = "".join(f"{sha256(candidates[name])}  {name}\n" for name in expected)
    output.write_text(manifest, encoding="ascii", newline="\n")


def parse_manifest(path: Path) -> dict[str, str]:
    try:
        raw = path.read_bytes()
        text = raw.decode("ascii")
    except (OSError, UnicodeDecodeError) as error:
        raise ReleaseArtifactError(f"cannot read ASCII checksum manifest: {error}") from error
    if not text or not text.endswith("\n") or "\r" in text:
        raise ReleaseArtifactError("checksum manifest must be non-empty LF-terminated ASCII")
    parsed: dict[str, str] = {}
    for line in text.splitlines():
        match = SHA256_LINE.fullmatch(line)
        if match is None:
            raise ReleaseArtifactError(f"malformed checksum line: {line!r}")
        digest, name = match.groups()
        if name in parsed:
            raise ReleaseArtifactError(f"duplicate checksum filename: {name}")
        parsed[name] = digest
    if list(parsed) != sorted(parsed):
        raise ReleaseArtifactError("checksum manifest is not deterministically sorted")
    return parsed


def verify_manifest(root: Path, expected_file: Path, manifest_path: Path) -> None:
    expected = read_expected(expected_file)
    candidates = collect_candidates(root)
    try:
        manifest_inside_root = manifest_path.resolve().is_relative_to(root.resolve())
    except OSError:
        manifest_inside_root = False
    if manifest_inside_root and candidates.get(manifest_path.name) == manifest_path:
        candidates.pop(manifest_path.name)
    observed = set(candidates)
    required = set(expected)
    missing = sorted(required - observed)
    extra = sorted(observed - required)
    if missing or extra:
        raise ReleaseArtifactError(
            f"artifact set mismatch: missing={missing}, unexpected={extra}"
        )
    manifest = parse_manifest(manifest_path)
    if list(manifest) != expected:
        raise ReleaseArtifactError(
            f"checksum filename set mismatch: expected={expected}, observed={list(manifest)}"
        )
    for name in expected:
        actual = sha256(candidates[name])
        if actual != manifest[name]:
            raise ReleaseArtifactError(f"checksum mismatch: {name}")


def _validate_archive_members(archive: Path, executable_name: str) -> None:
    expected = [executable_name, *REQUIRED_DOCUMENTS]
    with tarfile.open(archive, "r:gz") as bundle:
        members = bundle.getmembers()
    names = [member.name for member in members]
    if names != expected:
        raise ReleaseArtifactError(
            f"release archive payload mismatch: expected={expected}, observed={names}"
        )
    if any(not member.isfile() for member in members):
        raise ReleaseArtifactError("release archive contains a non-regular member")
    if os.name != "nt" and members[0].mode & 0o111 == 0:
        raise ReleaseArtifactError("release executable has no executable mode bit")


def _extract_regular_members(archive: Path, destination: Path) -> None:
    with tarfile.open(archive, "r:gz") as bundle:
        for member in bundle.getmembers():
            source = bundle.extractfile(member)
            if source is None:
                raise ReleaseArtifactError(f"cannot read archive member: {member.name}")
            target = destination / member.name
            target.write_bytes(source.read())
            target.chmod(member.mode & 0o777)


def _run(
    binary: Path,
    *arguments: str,
    environment: Mapping[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(
            [str(binary), *arguments],
            capture_output=True,
            text=True,
            timeout=45,
            check=False,
            env=environment,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ReleaseArtifactError(f"packaged executable failed to run: {error}") from error


def _closed_windows_preflight_status(
    binary: Path,
    platform_name: str = os.name,
    environment: Mapping[str, str] | None = None,
) -> str:
    if platform_name != "nt":
        return "not-run"
    helper = _run(
        binary,
        "--mini-agent-windows-worker-preflight-v1",
        environment=environment,
    )
    return str(helper.returncode)


def _harden_windows_install_directory(
    directory: Path,
    platform_name: str = os.name,
    environment: Mapping[str, str] = os.environ,
) -> None:
    if platform_name != "nt":
        return
    system_root = environment.get("SystemRoot")
    if not system_root:
        raise ReleaseArtifactError(
            "cannot create a private Windows smoke-install directory"
        )
    powershell = (
        Path(system_root)
        / "System32"
        / "WindowsPowerShell"
        / "v1.0"
        / "powershell.exe"
    )
    acl_environment = dict(environment)
    acl_environment["MINI_AGENT_RELEASE_SMOKE_DIRECTORY"] = str(directory)
    try:
        hardened = subprocess.run(
            [
                str(powershell),
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                WINDOWS_PRIVATE_DIRECTORY_SCRIPT,
            ],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
            env=acl_environment,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ReleaseArtifactError(
            "cannot create a private Windows smoke-install directory"
        ) from error
    if hardened.returncode != 0:
        raise ReleaseArtifactError(
            "cannot create a private Windows smoke-install directory"
        )


def _smoke_install_parent(
    environment: Mapping[str, str] = os.environ,
    platform_name: str = os.name,
) -> Path | None:
    if platform_name != "nt":
        return None
    value = environment.get("LOCALAPPDATA")
    if not value:
        raise ReleaseArtifactError(
            "Windows archive smoke requires a local per-user installation root"
        )
    root = Path(value)
    if not root.is_dir() or root.is_symlink():
        raise ReleaseArtifactError(
            "Windows archive smoke installation root is unavailable or unsafe"
        )
    return root


def _smoke_environment(
    destination: Path,
    environment: Mapping[str, str] = os.environ,
    platform_name: str = os.name,
) -> dict[str, str] | None:
    if platform_name != "nt":
        return None
    smoke_environment = dict(environment)
    smoke_environment["LOCALAPPDATA"] = str(destination)
    return smoke_environment


def smoke_archive(
    archive: Path,
    executable_name: str,
    expected_version: str,
    expect_js: bool,
) -> None:
    if not archive.is_file() or archive.is_symlink():
        raise ReleaseArtifactError(f"release archive is not a regular file: {archive}")
    if not SAFE_NAME.fullmatch(executable_name):
        raise ReleaseArtifactError("unsafe executable name")
    _validate_archive_members(archive, executable_name)
    install_parent = _smoke_install_parent()
    with tempfile.TemporaryDirectory(
        prefix="mini-agent-release-smoke-", dir=install_parent
    ) as directory:
        destination = Path(directory)
        _harden_windows_install_directory(destination)
        _extract_regular_members(archive, destination)
        binary = destination / executable_name
        smoke_environment = _smoke_environment(destination)
        version = _run(binary, "--version", environment=smoke_environment)
        expected = f"mini-agent {expected_version}"
        if version.returncode != 0 or version.stdout.strip() != expected:
            raise ReleaseArtifactError(
                f"packaged version smoke failed: expected={expected!r}, "
                f"status={version.returncode}, stdout={version.stdout.strip()!r}"
            )
        js = _run(binary, "--js-runtime-check", environment=smoke_environment)
        if expect_js:
            if js.returncode != 0 or js.stdout.strip() != "JS runtime check: PASS (2)":
                helper_status = _closed_windows_preflight_status(
                    binary, environment=smoke_environment
                )
                raise ReleaseArtifactError(
                    "packaged JS runtime smoke failed: "
                    f"status={js.returncode}, stdout={js.stdout.strip()!r}, "
                    f"stderr={js.stderr.strip()!r}, "
                    f"closed_preflight_stage_status={helper_status}"
                )
        elif js.returncode == 0 or "unexpected argument" not in js.stderr:
            raise ReleaseArtifactError("lite archive unexpectedly exposes the JS runtime check")


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    subcommands = root.add_subparsers(dest="command", required=True)
    for name in ("manifest", "verify"):
        command = subcommands.add_parser(name)
        command.add_argument("--root", type=Path, required=True)
        command.add_argument("--expected-file", type=Path, required=True)
        command.add_argument("--manifest", type=Path, required=True)
    candidate_set = subcommands.add_parser("validate-set")
    candidate_set.add_argument("--root", type=Path, required=True)
    candidate_set.add_argument("--expected-file", type=Path, required=True)
    smoke = subcommands.add_parser("smoke")
    smoke.add_argument("--archive", type=Path, required=True)
    smoke.add_argument("--executable-name", required=True)
    smoke.add_argument("--expected-version", required=True)
    smoke.add_argument("--expect-js", choices=("yes", "no"), required=True)
    return root


def main() -> int:
    args = parser().parse_args()
    try:
        if args.command == "manifest":
            build_manifest(args.root, args.expected_file, args.manifest)
        elif args.command == "verify":
            verify_manifest(args.root, args.expected_file, args.manifest)
        elif args.command == "validate-set":
            validate_candidate_set(args.root, read_expected(args.expected_file))
        else:
            smoke_archive(
                args.archive,
                args.executable_name,
                args.expected_version,
                args.expect_js == "yes",
            )
    except ReleaseArtifactError as error:
        print(f"release artifact validation failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
