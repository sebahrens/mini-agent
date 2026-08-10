#!/usr/bin/env python3
"""Faithfully stage package recipes and verify their GPL compliance payloads."""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import stat
import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
CHANNELS = ("aur", "conda-bin", "conda-source", "homebrew")
DOCUMENTS = ("LICENSE", "NOTICE", "SOURCE.md")
CANONICAL_GPL3_LICENSE_SHA256 = (
    "3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986"
)


def run(
    command: list[str], *, cwd: Path, env: dict[str, str] | None = None
) -> None:
    subprocess.run(command, cwd=cwd, env=env, check=True)


def write_executable(path: Path, text: str) -> None:
    path.write_text(text, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)


def controlled_tools(root: Path, binary: Path) -> Path:
    tools = root / "controlled-tools"
    tools.mkdir()
    write_executable(
        tools / "install",
        """#!/usr/bin/env python3
import os
import shutil
import sys
from pathlib import Path

args = sys.argv[1:]
if len(args) != 3 or not args[0].startswith("-Dm"):
    raise SystemExit(f"unsupported controlled install invocation: {args!r}")
mode = int(args[0][3:], 8)
source, destination = Path(args[1]), Path(args[2])
destination.parent.mkdir(parents=True, exist_ok=True)
shutil.copyfile(source, destination)
destination.chmod(mode)
""",
    )
    write_executable(
        tools / "cargo",
        """#!/usr/bin/env python3
import os
import shutil
import sys
from pathlib import Path

args = sys.argv[1:]
expected = ["auditable", "install", "--locked", "--no-track", "--bins"]
if args[:len(expected)] != expected or "--root" not in args:
    raise SystemExit(f"unsupported controlled cargo invocation: {args!r}")
root = Path(args[args.index("--root") + 1])
destination = root / "bin" / "mini-agent"
destination.parent.mkdir(parents=True, exist_ok=True)
shutil.copyfile(os.environ["STUB_BINARY"], destination)
destination.chmod(0o755)
""",
    )
    write_executable(
        tools / "cargo-bundle-licenses",
        """#!/usr/bin/env python3
import sys
from pathlib import Path

args = sys.argv[1:]
if args[:2] != ["--format", "yaml"] or "--output" not in args:
    raise SystemExit(f"unsupported controlled cargo-bundle-licenses invocation: {args!r}")
Path(args[args.index("--output") + 1]).write_text("third-party: []\\n", encoding="utf-8")
""",
    )
    return tools


def make_payload(root: Path) -> tuple[Path, Path]:
    payload = root / "payload"
    payload.mkdir()
    binary = payload / "mini-agent"
    binary.write_text(
        "#!/bin/sh\n"
        'case "${1:-}" in\n'
        "  --help|--version) exit 0 ;;\n"
        "  *) exit 2 ;;\n"
        "esac\n",
        encoding="utf-8",
    )
    binary.chmod(0o755)
    license_digest = hashlib.sha256((ROOT / "LICENSE").read_bytes()).hexdigest()
    if license_digest != CANONICAL_GPL3_LICENSE_SHA256:
        raise RuntimeError("LICENSE is not the canonical GPL-3.0-only text")
    for document in DOCUMENTS:
        shutil.copyfile(ROOT / document, payload / document)
    return payload, binary


def recipe_env(tools: Path, **values: str) -> dict[str, str]:
    env = os.environ.copy()
    env.update(values)
    env["PATH"] = f"{tools}{os.pathsep}{env.get('PATH', '')}"
    return env


def assert_files(stage: Path, expected: dict[str, Path]) -> None:
    for relative, source in expected.items():
        installed = stage / relative
        if not installed.is_file():
            raise RuntimeError(f"staged package is missing {relative}")
        if installed.read_bytes() != source.read_bytes():
            raise RuntimeError(f"staged package changed {relative}")


def stage_aur(root: Path, payload: Path, binary: Path, tools: Path) -> None:
    stage = root / "aur"
    env = recipe_env(
        tools,
        RECIPE=str(ROOT / "packaging/aur/PKGBUILD"),
        pkgdir=str(stage),
    )
    run(
        ["bash", "-c", 'set -euo pipefail; source "$RECIPE"; package'],
        cwd=payload,
        env=env,
    )
    assert_files(
        stage,
        {
            "usr/bin/mini-agent": binary,
            "usr/share/licenses/zerostack-bin/LICENSE": payload / "LICENSE",
            "usr/share/doc/zerostack-bin/NOTICE": payload / "NOTICE",
            "usr/share/doc/zerostack-bin/SOURCE.md": payload / "SOURCE.md",
        },
    )


def stage_conda_binary(root: Path, payload: Path, binary: Path, tools: Path) -> None:
    stage = root / "conda-bin"
    env = recipe_env(
        tools,
        SRC_DIR=str(payload),
        PREFIX=str(stage),
        PKG_NAME="zerostack-bin",
    )
    run(
        ["bash", str(ROOT / "packaging/conda/zerostack-bin/build.sh")],
        cwd=payload,
        env=env,
    )
    assert_files(
        stage,
        {
            "bin/mini-agent": binary,
            "share/licenses/zerostack-bin/LICENSE": payload / "LICENSE",
            "share/doc/zerostack-bin/NOTICE": payload / "NOTICE",
            "share/doc/zerostack-bin/SOURCE.md": payload / "SOURCE.md",
        },
    )


def stage_conda_source(root: Path, payload: Path, binary: Path, tools: Path) -> None:
    stage = root / "conda-source"
    source = root / "conda-source-input"
    shutil.copytree(payload, source)
    env = recipe_env(
        tools,
        PREFIX=str(stage),
        PKG_NAME="zerostack",
        STUB_BINARY=str(binary),
    )
    run(
        ["bash", str(ROOT / "packaging/conda/zerostack/build.sh")],
        cwd=source,
        env=env,
    )
    run([str(stage / "bin/mini-agent"), "--help"], cwd=source, env=env)
    run([str(stage / "bin/mini-agent"), "--version"], cwd=source, env=env)
    if not (stage / "THIRDPARTY.yml").is_file():
        raise RuntimeError("Conda source recipe test cannot find PREFIX/THIRDPARTY.yml")

    meta = (ROOT / "packaging/conda/zerostack/meta.yaml").read_text(encoding="utf-8")
    for license_file in ("LICENSE", "THIRDPARTY.yml"):
        if f"    - {license_file}" not in meta:
            raise RuntimeError(f"Conda source recipe omits license_file {license_file}")
        license_dir = stage / "info/licenses"
        license_dir.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source / license_file, license_dir / license_file)

    assert_files(
        stage,
        {
            "bin/mini-agent": binary,
            "THIRDPARTY.yml": source / "THIRDPARTY.yml",
            "info/licenses/LICENSE": payload / "LICENSE",
            "share/doc/zerostack/NOTICE": payload / "NOTICE",
            "share/doc/zerostack/SOURCE.md": payload / "SOURCE.md",
        },
    )


HOMEBREW_HARNESS = r"""
require "fileutils"

class StagePath
  def initialize(path)
    @path = path
  end

  def install(*sources)
    FileUtils.mkdir_p(@path)
    sources.each { |source| FileUtils.cp(source, @path) }
  end
end

class Formula
  def self.on_macos; yield if RUBY_PLATFORM.include?("darwin"); end
  def self.on_linux; yield unless RUBY_PLATFORM.include?("darwin"); end
  def self.test(&block); end
  def self.method_missing(*args, &block); end
  def bin; StagePath.new(ENV.fetch("STAGE_BIN")); end
  def pkgshare; StagePath.new(ENV.fetch("STAGE_SHARE")); end
end

module Hardware
  module CPU
    def self.intel?; true; end
  end
end

load ENV.fetch("FORMULA")
Zerostack.new.install
"""


def stage_homebrew(root: Path, payload: Path, binary: Path, tools: Path) -> None:
    del tools
    stage = root / "homebrew"
    env = os.environ.copy()
    env.update(
        FORMULA=str(ROOT / "packaging/homebrew/zerostack.rb"),
        STAGE_BIN=str(stage / "bin"),
        STAGE_SHARE=str(stage / "share/zerostack"),
    )
    run(["ruby", "--disable-gems", "-e", HOMEBREW_HARNESS], cwd=payload, env=env)
    assert_files(
        stage,
        {
            "bin/mini-agent": binary,
            "share/zerostack/LICENSE": payload / "LICENSE",
            "share/zerostack/NOTICE": payload / "NOTICE",
            "share/zerostack/SOURCE.md": payload / "SOURCE.md",
        },
    )


STAGERS = {
    "aur": stage_aur,
    "conda-bin": stage_conda_binary,
    "conda-source": stage_conda_source,
    "homebrew": stage_homebrew,
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--channel",
        action="append",
        choices=CHANNELS,
        required=True,
        help="package channel to stage; repeat for multiple channels",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    with tempfile.TemporaryDirectory(prefix="mini-agent-package-compliance-") as temporary:
        root = Path(temporary)
        payload, binary = make_payload(root)
        tools = controlled_tools(root, binary)
        for channel in dict.fromkeys(args.channel):
            STAGERS[channel](root, payload, binary, tools)
            print(f"package compliance smoke passed: {channel}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
