#!/usr/bin/env python3
"""Render the winget, scoop and npm package-manager manifests for one release.

Nothing here publishes. The templates under packaging/ hold `@TOKEN@` placeholders;
this script fills them from a release's `SHA256SUMS` / `MSI_SHA256SUMS` manifests and
fails closed when a digest is missing, duplicated or malformed, or when a rendered file
still contains a placeholder. Publishing steps are in docs/agent/PUBLISHING_RELEASES.md.

  render_package_manifests.py winget --version 1.12.0 --msi-sums MSI_SHA256SUMS --out out/winget
  render_package_manifests.py scoop  --version 1.12.0 --sums SHA256SUMS --out out/scoop
  render_package_manifests.py npm    --version 1.12.0 --sums SHA256SUMS --archives DIR --out out/npm
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import sys
import tarfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PACKAGING = ROOT / "packaging"
VERSION_PATTERN = re.compile(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?$")
HEX_SHA256 = re.compile(r"^[0-9a-fA-F]{64}$")
TOKEN = re.compile(r"@[A-Z0-9_]+@")

MSI_ASSET = "mini-agent-windows-x64.msi"
MSI_ARM64_ASSET = "mini-agent-windows-arm64.msi"
SCOOP_ASSET = "mini-agent-x86_64-pc-windows-msvc.tar.gz"
SCOOP_ARM64_ASSET = "mini-agent-aarch64-pc-windows-msvc.tar.gz"
# npm platform name -> (node os, node cpu, release target, binary name)
NPM_PLATFORMS = {
    "darwin-arm64": ("darwin", "arm64", "aarch64-apple-darwin", "mini-agent"),
    "darwin-x64": ("darwin", "x64", "x86_64-apple-darwin", "mini-agent"),
    "linux-arm64": ("linux", "arm64", "aarch64-unknown-linux-musl", "mini-agent"),
    "linux-x64": ("linux", "x64", "x86_64-unknown-linux-musl", "mini-agent"),
    "win32-x64": ("win32", "x64", "x86_64-pc-windows-msvc", "mini-agent.exe"),
}
NPM_LICENSE_FILES = ("LICENSE", "NOTICE", "SOURCE.md", "THIRD_PARTY_LICENSES")


class RenderError(Exception):
    pass


def check_version(version: str) -> str:
    if not VERSION_PATTERN.match(version):
        raise RenderError(f"not a release version: {version!r}")
    return version


def digest_for(sums_text: str, asset: str) -> str:
    """The digest of exactly one well-formed `<hex>  <name>` line, lowercase."""
    matches = []
    for line in sums_text.splitlines():
        parts = line.split()
        if len(parts) == 2 and parts[1].lstrip("*") == asset:
            matches.append(parts[0])
    if len(matches) != 1:
        raise RenderError(f"checksum manifest has {len(matches)} entries for {asset}; expected exactly one")
    if not HEX_SHA256.match(matches[0]):
        raise RenderError(f"checksum manifest has a malformed digest for {asset}")
    return matches[0].lower()


def fill(template: str, values: dict[str, str]) -> str:
    text = template
    for key, value in values.items():
        text = text.replace(f"@{key}@", value)
    leftover = TOKEN.findall(text)
    if leftover:
        raise RenderError(f"unresolved placeholders: {sorted(set(leftover))}")
    return text


def read_template(relative: str) -> str:
    return (PACKAGING / relative).read_text(encoding="utf-8")


def render_winget(version: str, msi_sums: str, out: Path) -> list[Path]:
    values = {
        "VERSION": version,
        "MSI_SHA256": digest_for(msi_sums, MSI_ASSET).upper(),
        "MSI_ARM64_SHA256": digest_for(msi_sums, MSI_ARM64_ASSET).upper(),
    }
    out.mkdir(parents=True, exist_ok=True)
    written = []
    for template in sorted((PACKAGING / "winget").glob("*.yaml.in")):
        target = out / template.name.removesuffix(".in").replace("MiniAgent", "sebahrens.MiniAgent", 1)
        target.write_text(fill(template.read_text(encoding="utf-8"), values), encoding="utf-8")
        written.append(target)
    return written


def render_scoop(version: str, sums: str, out: Path) -> list[Path]:
    values = {
        "VERSION": version,
        "ARCHIVE_SHA256": digest_for(sums, SCOOP_ASSET),
        "ARCHIVE_ARM64_SHA256": digest_for(sums, SCOOP_ARM64_ASSET),
    }
    out.mkdir(parents=True, exist_ok=True)
    target = out / "mini-agent.json"
    rendered = fill(read_template("scoop/mini-agent.json.in"), values)
    json.loads(rendered)  # fail early on a broken template
    target.write_text(rendered, encoding="utf-8")
    return [target]


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def extract_member(archive: Path, name: str, destination: Path) -> None:
    with tarfile.open(archive, "r:gz") as tar:
        member = next((m for m in tar.getmembers() if m.name.removeprefix("./") == name), None)
        if member is None or not member.isfile():
            raise RenderError(f"{archive.name} has no regular file {name}")
        source = tar.extractfile(member)
        assert source is not None
        with destination.open("wb") as target:
            shutil.copyfileobj(source, target)
        destination.chmod(0o755 if name.startswith("mini-agent") else 0o644)


def render_npm(version: str, sums: str, archives: Path, out: Path) -> list[Path]:
    out.mkdir(parents=True, exist_ok=True)
    written = []
    wrapper = out / "mini-agent"
    (wrapper / "bin").mkdir(parents=True, exist_ok=True)
    (wrapper / "package.json").write_text(
        fill(read_template("npm/package.json.in"), {"VERSION": version}), encoding="utf-8"
    )
    shutil.copy(PACKAGING / "npm/bin/mini-agent.js", wrapper / "bin/mini-agent.js")
    for name in ("LICENSE", "NOTICE", "SOURCE.md"):
        shutil.copy(ROOT / name, wrapper / name)
    written.append(wrapper)
    for platform, (os_name, cpu, target, binary) in NPM_PLATFORMS.items():
        asset = f"mini-agent-{target}.tar.gz"
        expected = digest_for(sums, asset)
        archive = archives / asset
        if not archive.is_file():
            raise RenderError(f"missing archive {archive}")
        actual = sha256_file(archive)
        if actual != expected:
            raise RenderError(f"{asset} digest {actual} does not match SHA256SUMS {expected}")
        package = out / f"mini-agent-{platform}"
        package.mkdir(parents=True, exist_ok=True)
        (package / "package.json").write_text(
            fill(
                read_template("npm/platform-package.json.in"),
                {
                    "VERSION": version,
                    "PLATFORM": platform,
                    "OS": os_name,
                    "CPU": cpu,
                    "BINARY": binary,
                    "ARCHIVE": asset,
                    "ARCHIVE_SHA256": expected,
                },
            ),
            encoding="utf-8",
        )
        extract_member(archive, binary, package / binary)
        for name in NPM_LICENSE_FILES:
            extract_member(archive, name, package / name)
        written.append(package)
    return written


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    for name in ("winget", "scoop", "npm"):
        p = sub.add_parser(name)
        p.add_argument("--version", required=True)
        p.add_argument("--out", required=True, type=Path)
        if name == "winget":
            p.add_argument("--msi-sums", required=True, type=Path)
        else:
            p.add_argument("--sums", required=True, type=Path)
        if name == "npm":
            p.add_argument("--archives", required=True, type=Path)
    args = parser.parse_args(argv)
    try:
        version = check_version(args.version)
        if args.command == "winget":
            written = render_winget(version, args.msi_sums.read_text(encoding="utf-8"), args.out)
        elif args.command == "scoop":
            written = render_scoop(version, args.sums.read_text(encoding="utf-8"), args.out)
        else:
            written = render_npm(version, args.sums.read_text(encoding="utf-8"), args.archives, args.out)
    except (RenderError, OSError, tarfile.TarError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    for path in written:
        print(path)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
