"""Tests for scripts/update-release-checksums.sh.

The updater pins package-recipe digests from the published release, so it must
refuse assets that disagree with the release's SHA256SUMS manifest or whose
build provenance fails `gh attestation verify`, before changing any recipe.
curl and gh are stubbed on a PATH that contains no host gh.
"""

from __future__ import annotations

import hashlib
import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).parents[2]
UPDATER = ROOT / "scripts/update-release-checksums.sh"
RECIPES = (
    "packaging/aur/PKGBUILD",
    "packaging/conda/zerostack/meta.yaml",
    "packaging/conda/zerostack-bin/meta.yaml",
    "packaging/homebrew/zerostack.rb",
)
TOOLS = (
    "awk", "bash", "cat", "cp", "dirname", "head", "mktemp", "rm", "sed",
    "sh", "sha256sum", "shasum",
)
VERSION = re.search(
    r'^version = "([^"]+)"', (ROOT / "Cargo.toml").read_text(), re.MULTILINE
).group(1)
ASSETS = (
    "mini-agent-x86_64-unknown-linux-musl.tar.gz",
    "mini-agent-aarch64-unknown-linux-musl.tar.gz",
    "mini-agent-x86_64-apple-darwin.tar.gz",
    "mini-agent-aarch64-apple-darwin.tar.gz",
    f"mini-agent-v{VERSION}-source.tar.gz",
)

CURL_STUB = """#!/bin/bash
set -euo pipefail
output=""
for ((index = 1; index <= $#; index++)); do
    next=$((index + 1))
    if [[ "${!index}" == "-o" ]]; then
        output="${!next}"
    fi
done
url="${!#}"
printf '%s\\n' "$url" >> "$UPDATE_TEST_CURL_LOG"
case "$url" in
    https://github.com/sebahrens/mini-agent/releases/download/v*/*) ;;
    https://raw.githubusercontent.com/sebahrens/mini-agent/v*/LICENSE) ;;
    *) exit 22 ;;
esac
source_file="${UPDATE_TEST_RELEASE}/${url##*/}"
[[ -f "$source_file" ]] || exit 22
cp "$source_file" "$output"
"""

GH_STUB = """#!/bin/bash
printf '%s\\n' "$*" >> "$UPDATE_TEST_GH_LOG"
case "$1 $2" in
    "auth status") exit 0 ;;
    "attestation verify")
        [[ "$3" == "--help" ]] && exit 0
        [[ "${3##*/}" == "${UPDATE_TEST_REJECT_ATTESTATION:-}" ]] && exit 1
        exit 0
        ;;
esac
exit 2
"""


class UpdateReleaseChecksumsTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tempdir.cleanup)
        base = Path(self.tempdir.name)
        # A copy of the repository pieces the updater reads and writes.
        self.repo = base / "repo"
        (self.repo / "scripts").mkdir(parents=True)
        shutil.copy2(UPDATER, self.repo / "scripts/update-release-checksums.sh")
        shutil.copy2(ROOT / "Cargo.toml", self.repo / "Cargo.toml")
        for recipe in RECIPES:
            destination = self.repo / recipe
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / recipe, destination)
        self.original = {recipe: (self.repo / recipe).read_bytes() for recipe in RECIPES}

        # The simulated release: distinct bytes per asset and a matching manifest.
        self.release = base / "release"
        self.release.mkdir()
        self.digests: dict[str, str] = {}
        for name in ASSETS:
            data = f"release asset {name}\n".encode()
            (self.release / name).write_bytes(data)
            self.digests[name] = hashlib.sha256(data).hexdigest()
        shutil.copy2(ROOT / "LICENSE", self.release / "LICENSE")
        self.write_manifest(self.digests)

        self.stub_bin = base / "stub-bin"
        self.stub_bin.mkdir()
        curl = self.stub_bin / "curl"
        curl.write_text(CURL_STUB, encoding="utf-8")
        curl.chmod(0o755)
        self.tools = base / "tools"
        self.tools.mkdir()
        for name in TOOLS:
            resolved = shutil.which(name)
            if resolved is not None:
                (self.tools / name).symlink_to(resolved)
        self.gh_log = base / "gh.log"
        self.curl_log = base / "curl.log"

    def write_manifest(self, digests: dict[str, str]) -> None:
        # The real manifest also lists assets the updater does not use.
        lines = [f"{'1' * 64}  mini-agent-x86_64-pc-windows-msvc.tar.gz"]
        lines += [f"{digest}  {name}" for name, digest in sorted(digests.items())]
        (self.release / "SHA256SUMS").write_text("\n".join(lines) + "\n")

    def install_gh(self) -> None:
        gh = self.stub_bin / "gh"
        gh.write_text(GH_STUB, encoding="utf-8")
        gh.chmod(0o755)

    def run_updater(self, **extra_env: str) -> subprocess.CompletedProcess[str]:
        path = f"{self.stub_bin}{os.pathsep}{self.tools}"
        if not (self.stub_bin / "gh").exists():
            # A host gh must never leak into the "gh absent" cases.
            self.assertIsNone(shutil.which("gh", path=path))
        env = {
            "PATH": path,
            "HOME": self.tempdir.name,
            "UPDATE_TEST_RELEASE": str(self.release),
            "UPDATE_TEST_GH_LOG": str(self.gh_log),
            "UPDATE_TEST_CURL_LOG": str(self.curl_log),
        }
        env.update(extra_env)
        return subprocess.run(
            ["bash", str(self.repo / "scripts/update-release-checksums.sh"), "all"],
            env=env,
            capture_output=True,
            text=True,
        )

    def assert_recipes_unchanged(self) -> None:
        for recipe in RECIPES:
            self.assertEqual(
                self.original[recipe], (self.repo / recipe).read_bytes(), recipe
            )

    def attestation_calls(self) -> list[str]:
        if not self.gh_log.exists():
            return []
        return [
            line
            for line in self.gh_log.read_text().splitlines()
            if line.startswith("attestation verify ") and "--help" not in line
        ]

    def test_matching_manifest_updates_every_recipe(self) -> None:
        result = self.run_updater()

        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("build provenance is not verified", result.stderr)
        self.assertIn("gh attestation verify <asset> --repo sebahrens/mini-agent", result.stderr)
        homebrew = (self.repo / "packaging/homebrew/zerostack.rb").read_text()
        for name in ASSETS[:4]:
            self.assertIn(f'sha256 "{self.digests[name]}"', homebrew)
        conda_source = (self.repo / "packaging/conda/zerostack/meta.yaml").read_text()
        self.assertIn(f"sha256: {self.digests[ASSETS[4]]}", conda_source)
        self.assertIn(
            f"https://github.com/sebahrens/mini-agent/releases/download/v{VERSION}/SHA256SUMS",
            self.curl_log.read_text().splitlines(),
        )

    def test_digest_disagreeing_with_manifest_fails_before_any_recipe_change(self) -> None:
        digests = dict(self.digests)
        digests["mini-agent-aarch64-apple-darwin.tar.gz"] = "f" * 64
        self.write_manifest(digests)

        result = self.run_updater()

        self.assertNotEqual(0, result.returncode)
        self.assertIn(
            "mini-agent-aarch64-apple-darwin.tar.gz does not match SHA256SUMS", result.stderr
        )
        self.assertIn("f" * 64, result.stderr)
        self.assertNotIn("Updated all release checksums", result.stdout)
        self.assert_recipes_unchanged()

    def test_replaced_asset_fails_against_the_manifest(self) -> None:
        (self.release / f"mini-agent-v{VERSION}-source.tar.gz").write_bytes(b"replaced\n")

        result = self.run_updater()

        self.assertNotEqual(0, result.returncode)
        self.assertIn("-source.tar.gz does not match SHA256SUMS", result.stderr)
        self.assert_recipes_unchanged()

    def test_missing_manifest_entry_fails(self) -> None:
        digests = dict(self.digests)
        del digests["mini-agent-x86_64-apple-darwin.tar.gz"]
        self.write_manifest(digests)

        result = self.run_updater()

        self.assertNotEqual(0, result.returncode)
        self.assertIn("has 0 entries for mini-agent-x86_64-apple-darwin.tar.gz", result.stderr)
        self.assert_recipes_unchanged()

    def test_malformed_manifest_entry_fails(self) -> None:
        manifest = self.release / "SHA256SUMS"
        manifest.write_text(
            manifest.read_text().replace(
                f"{self.digests[ASSETS[0]]}  {ASSETS[0]}",
                f"{self.digests[ASSETS[0]].upper()}  {ASSETS[0]}",
            )
        )

        result = self.run_updater()

        self.assertNotEqual(0, result.returncode)
        self.assertIn(f"malformed entry for {ASSETS[0]}", result.stderr)
        self.assert_recipes_unchanged()

    def test_missing_manifest_fails(self) -> None:
        (self.release / "SHA256SUMS").unlink()

        result = self.run_updater()

        self.assertNotEqual(0, result.returncode)
        self.assert_recipes_unchanged()

    def test_attestation_is_verified_for_every_release_asset(self) -> None:
        self.install_gh()

        result = self.run_updater()

        self.assertEqual(0, result.returncode, result.stderr)
        self.assertNotIn("build provenance is not verified", result.stderr)
        calls = self.attestation_calls()
        self.assertEqual(len(ASSETS), len(calls), calls)
        for name in ASSETS:
            self.assertTrue(
                any(call.endswith(f"/{name} --repo sebahrens/mini-agent") for call in calls),
                (name, calls),
            )

    def test_failed_attestation_fails_before_any_recipe_change(self) -> None:
        self.install_gh()

        result = self.run_updater(
            UPDATE_TEST_REJECT_ATTESTATION="mini-agent-x86_64-apple-darwin.tar.gz"
        )

        self.assertNotEqual(0, result.returncode)
        self.assertIn(
            "rejected mini-agent-x86_64-apple-darwin.tar.gz", result.stderr
        )
        self.assert_recipes_unchanged()

    def test_skip_attestation_switch_still_checks_the_manifest(self) -> None:
        self.install_gh()
        digests = dict(self.digests)
        digests[ASSETS[1]] = "0" * 64
        self.write_manifest(digests)

        result = self.run_updater(MINI_AGENT_SKIP_ATTESTATION="1")

        self.assertNotEqual(0, result.returncode)
        self.assertIn("MINI_AGENT_SKIP_ATTESTATION=1", result.stderr)
        self.assertIn(f"{ASSETS[1]} does not match SHA256SUMS", result.stderr)
        self.assertEqual([], self.attestation_calls())
        self.assert_recipes_unchanged()


if __name__ == "__main__":
    unittest.main()
