#!/usr/bin/env python3
"""Build a static musl Git bundle using the explicit, pinned Alpine toolchain."""
from __future__ import annotations
import argparse
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import uuid

ROOT = Path(__file__).resolve().parents[1]
PLATFORMS = {"x86_64-unknown-linux-musl": "linux/amd64", "aarch64-unknown-linux-musl": "linux/arm64"}
CURL_SHA256 = "8dadd7d2536a314f982ad05a651b713f426bf7d02cc2c76cffb9796f2218e6c5"


def run(command: list[str], timeout: int) -> None:
    subprocess.run(command, check=True, timeout=timeout, stdout=sys.stderr, stderr=sys.stderr)


def build(*, target: str, output: Path, root: Path = ROOT, jobs: int = 8) -> Path:
    if target not in PLATFORMS or not 1 <= jobs <= 64:
        raise ValueError("Expected a supported musl target and jobs between 1 and 64")
    root = root.resolve(strict=True)
    output = output.absolute()
    if output.exists() or output.is_symlink():
        raise ValueError("Musl bundle output must not already exist")
    if any(character in str(path) for path in (root, output.parent) for character in (",", "\n", "\r")):
        raise ValueError("Docker bind paths cannot contain commas or newlines")
    context = root / "scripts/git-runtime"
    archive = context / "curl-8.23.0.tar.xz"
    if archive.is_symlink() or not archive.is_file() or archive.stat().st_size > 16 * 1024 * 1024:
        raise ValueError("Pinned curl source archive is missing or invalid")
    if hashlib.sha256(archive.read_bytes()).hexdigest() != CURL_SHA256:
        raise ValueError("Pinned curl source hash mismatch")
    recipe = context / "Dockerfile.musl"
    image_key = hashlib.sha256(recipe.read_bytes()).hexdigest()[:20]
    image = "mini-agent-git-musl-" + target.split("-", 1)[0] + ":" + image_key
    platform = PLATFORMS[target]
    output.parent.mkdir(parents=True, exist_ok=True)
    cache = root / "target/git-runtime-musl-cache"
    cache.mkdir(mode=0o700, parents=True, exist_ok=True)
    run(["docker", "build", "--platform", platform, "--file", str(recipe), "--tag", image, str(context)], 1800)
    name = "mini-agent-git-musl-" + uuid.uuid4().hex
    command = ["docker", "run", "--rm", "--name", name, "--platform", platform, "--network", "none", "--read-only", "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--user", f"{os.getuid()}:{os.getgid()}", "--tmpfs", "/tmp:rw,nosuid,nodev", "--mount", f"type=bind,src={root},dst=/source,readonly", "--mount", f"type=bind,src={cache},dst=/cache", "--mount", f"type=bind,src={output.parent},dst=/output", "--workdir", "/source", image, "python3", "scripts/build-git-runtime.py", "--target", target, "--output", "/output/" + output.name, "--cache-root", "/cache", "--jobs", str(jobs)]
    try:
        run(command, 1800)
    except BaseException:
        # A killed Docker client need not retire its container. Remove only the
        # exact uniquely named build actor before returning a failed build.
        subprocess.run(["docker", "rm", "--force", name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
        raise
    if not (output / "bundle.manifest").is_file():
        raise ValueError("Musl build did not publish a bundle")
    return output




def collect_sources(*, target: str, bundle: Path, output: Path, root: Path = ROOT) -> Path:
    """Separate public-download phase; no host credentials or privileged mounts."""
    if target not in PLATFORMS:
        raise ValueError("Expected a supported musl target")
    root = root.resolve(strict=True)
    bundle = bundle.resolve(strict=True)
    output = output.absolute()
    if output.exists() or output.is_symlink():
        raise ValueError("Native source output must not already exist")
    if any(character in str(path) for path in (root, bundle, output.parent) for character in (",", "\n", "\r")):
        raise ValueError("Docker bind paths cannot contain commas or newlines")
    if not (bundle / "bundle.manifest").is_file():
        raise ValueError("Native source collection requires the built bundle")
    output.parent.mkdir(parents=True, exist_ok=True)
    image_key = hashlib.sha256((root / "scripts/git-runtime/Dockerfile.musl").read_bytes()).hexdigest()[:20]
    image = "mini-agent-git-musl-" + target.split("-", 1)[0] + ":" + image_key
    name = "mini-agent-git-sources-" + uuid.uuid4().hex
    command = ["docker", "run", "--rm", "--name", name, "--platform", PLATFORMS[target], "--network", "bridge", "--read-only", "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--user", f"{os.getuid()}:{os.getgid()}", "--tmpfs", "/tmp:rw,nosuid,nodev", "--mount", f"type=bind,src={root},dst=/source,readonly", "--mount", f"type=bind,src={bundle},dst=/bundle,readonly", "--mount", f"type=bind,src={output.parent},dst=/output", "--workdir", "/source", image, "python3", "scripts/collect-git-native-sources.py", "--distro", "alpine", "--target", target, "--bundle", "/bundle", "--output", "/output/" + output.name, "--curl-source", "/source/scripts/git-runtime/curl-8.23.0.tar.xz", "--static-curl-closure"]
    try:
        run(command, 1800)
    except BaseException:
        subprocess.run(["docker", "rm", "--force", name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
        raise
    record = output / "native-components.json"
    if not record.is_file():
        raise ValueError("Native source collection did not publish its record")
    return record


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, choices=PLATFORMS)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--jobs", type=int, default=min(os.cpu_count() or 1, 8))
    parser.add_argument("--native-sources-output", type=Path, help="also capture verified target-native sources in a separate public-download phase")
    args = parser.parse_args()
    try:
        sources = args.native_sources_output
        if sources is not None and (sources.exists() or sources.is_symlink()):
            raise ValueError("Native source output must not already exist")
        bundle = build(target=args.target, output=args.output, jobs=args.jobs)
        if sources is not None:
            collect_sources(target=args.target, bundle=bundle, output=sources)
        print(bundle)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(1, f"Musl Git runtime build failed: {error}\n")


if __name__ == "__main__":
    main()
