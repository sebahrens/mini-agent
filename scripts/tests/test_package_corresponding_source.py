import base64
import hashlib
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "package-corresponding-source.sh"
REPOSITORY_ROOT = SCRIPT.parents[1]
HELPER = REPOSITORY_ROOT / "scripts" / "corresponding_source.py"
REAL_LOCKFILE = REPOSITORY_ROOT / "editors" / "vscode" / "package-lock.json"
HELPER_SPEC = importlib.util.spec_from_file_location("corresponding_source", HELPER)
assert HELPER_SPEC is not None and HELPER_SPEC.loader is not None
CORRESPONDING_SOURCE = importlib.util.module_from_spec(HELPER_SPEC)
sys.modules["corresponding_source"] = CORRESPONDING_SOURCE
HELPER_SPEC.loader.exec_module(CORRESPONDING_SOURCE)
SourceError = CORRESPONDING_SOURCE.SourceError

FAKE_NPM = """#!/usr/bin/env python3
import json, os, shutil, sys
arguments = sys.argv[1:]
if arguments == ["--version"]:
    print("11.0.0")
    raise SystemExit(0)
assert arguments[0] == "pack", arguments
destination = arguments[arguments.index("--pack-destination") + 1]
tarballs = json.loads(os.environ["FAKE_NPM_TARBALLS"])
log = os.environ.get("FAKE_NPM_LOG")
if log:
    with open(log, "a", encoding="utf-8") as handle:
        for url in arguments[1:]:
            if url.startswith("https://"):
                handle.write(url + "\\n")
for index, url in enumerate(a for a in arguments[1:] if a.startswith("https://")):
    # Real npm names output after the embedded package.json, not the lockfile
    # key; an unrelated name proves the vendor step matches by content hash.
    shutil.copyfile(tarballs[url], os.path.join(destination, f"packed-{os.getpid()}-{index}.tgz"))
"""


def sri_sha512(data: bytes) -> str:
    return "sha512-" + base64.b64encode(hashlib.sha512(data).digest()).decode("ascii")


class CorrespondingSourceIdentityTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary_directory.cleanup)
        self.repository = Path(self.temporary_directory.name)
        subprocess.run(["git", "init", "--quiet"], cwd=self.repository, check=True)
        subprocess.run(
            ["git", "config", "user.email", "source-test@example.invalid"],
            cwd=self.repository,
            check=True,
        )
        subprocess.run(
            ["git", "config", "user.name", "Source Test"],
            cwd=self.repository,
            check=True,
        )
        marker = self.repository / "marker"
        marker.write_text("tagged\n", encoding="utf-8")
        subprocess.run(["git", "add", "marker"], cwd=self.repository, check=True)
        subprocess.run(
            ["git", "commit", "--quiet", "-m", "tagged"],
            cwd=self.repository,
            check=True,
        )

    def run_packager(self, *arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["bash", str(SCRIPT), *arguments],
            cwd=self.repository,
            capture_output=True,
            text=True,
        )

    def test_missing_release_tag_fails_closed(self) -> None:
        result = self.run_packager("v1.2.3", str(self.repository), "HEAD")

        self.assertEqual(2, result.returncode)
        self.assertIn("release tag does not exist", result.stderr)

    def test_release_tag_must_match_selected_commit(self) -> None:
        subprocess.run(
            ["git", "tag", "v1.2.3"], cwd=self.repository, check=True
        )
        marker = self.repository / "marker"
        marker.write_text("later\n", encoding="utf-8")
        subprocess.run(["git", "add", "marker"], cwd=self.repository, check=True)
        subprocess.run(
            ["git", "commit", "--quiet", "-m", "later"],
            cwd=self.repository,
            check=True,
        )

        result = self.run_packager("v1.2.3", str(self.repository), "HEAD")

        self.assertEqual(2, result.returncode)
        self.assertIn("does not resolve to release tag", result.stderr)

    def test_untagged_bypass_is_restricted_to_ci_labels(self) -> None:
        result = self.run_packager(
            "v1.2.3",
            str(self.repository),
            "HEAD",
            "--allow-untagged-label",
        )

        self.assertEqual(2, result.returncode)
        self.assertIn("restricted to labels ending in -ci", result.stderr)

    def test_modified_license_fails_before_source_packaging(self) -> None:
        (self.repository / "LICENSE").write_text("not the GPL\n", encoding="utf-8")
        subprocess.run(["git", "add", "LICENSE"], cwd=self.repository, check=True)
        subprocess.run(
            ["git", "commit", "--quiet", "-m", "bad license"],
            cwd=self.repository,
            check=True,
        )
        subprocess.run(
            ["git", "tag", "v1.2.3"], cwd=self.repository, check=True
        )

        result = self.run_packager("v1.2.3", str(self.repository))

        self.assertEqual(2, result.returncode)
        self.assertIn("canonical GPL-3.0-only", result.stderr)

    def test_missing_npm_fails_clearly_before_packaging(self) -> None:
        result = subprocess.run(
            [
                "bash",
                str(SCRIPT),
                "v1.2.3-ci",
                str(self.repository / "out"),
                "HEAD",
                "--allow-untagged-label",
            ],
            cwd=self.repository,
            capture_output=True,
            text=True,
            env={**os.environ, "NPM": str(self.repository / "no-such-npm")},
        )

        self.assertEqual(2, result.returncode)
        self.assertIn("npm is required", result.stderr)
        self.assertIn("editors/vscode/.nvmrc", result.stderr)
        self.assertFalse((self.repository / "out").exists())


class NpmVendorTests(unittest.TestCase):
    """The npm vendor step against a fake ``npm pack`` and a synthetic lockfile."""

    def setUp(self) -> None:
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary_directory.cleanup)
        self.root = Path(self.temporary_directory.name)
        self.tarballs: dict[str, Path] = {}
        packages: dict[str, object] = {"": {"name": "fixture", "version": "1.0.0"}}
        fixtures = (
            ("node_modules/zod", "zod", "4.4.3", False),
            ("node_modules/@agentclientprotocol/sdk", "@agentclientprotocol/sdk", "1.3.0", False),
            ("node_modules/string-width-cjs", "string-width", "4.2.3", True),
            ("node_modules/eslint/node_modules/zod", "zod", "4.4.3", True),
        )
        for path, name, version, dev in fixtures:
            base = name.split("/")[-1]
            url = f"https://registry.npmjs.org/{name}/-/{base}-{version}.tgz"
            payload = f"{name}@{version}\n".encode()
            tarball = self.root / "registry" / f"{name.replace('/', '_')}-{version}.tgz"
            tarball.parent.mkdir(parents=True, exist_ok=True)
            tarball.write_bytes(payload)
            self.tarballs[url] = tarball
            entry: dict[str, object] = {
                "version": version,
                "resolved": url,
                "integrity": sri_sha512(payload),
            }
            if name != package_name(path):
                entry["name"] = name
            if dev:
                entry["dev"] = True
            packages[path] = entry
        self.lockfile = self.root / "package-lock.json"
        self.write_lock(packages)
        self.packages = packages
        self.npm = self.root / "bin" / "npm"
        self.npm.parent.mkdir()
        self.npm.write_text(FAKE_NPM, encoding="utf-8")
        self.npm.chmod(0o755)
        self.destination = self.root / "vendor-npm"

    def write_lock(self, packages: dict[str, object]) -> None:
        self.lockfile.write_text(
            json.dumps({"name": "fixture", "lockfileVersion": 3, "packages": packages}),
            encoding="utf-8",
        )

    def vendor(
        self,
        destination: Path | None = None,
        cache: Path | None = None,
        npm: Path | None = None,
    ) -> list[object]:
        previous = os.environ.get("FAKE_NPM_TARBALLS")
        os.environ["FAKE_NPM_TARBALLS"] = json.dumps(
            {url: str(path) for url, path in self.tarballs.items()}
        )
        try:
            return CORRESPONDING_SOURCE.vendor_npm(
                self.lockfile,
                destination or self.destination,
                str(npm or self.npm),
                1_700_000_000,
                jobs=2,
                cache=cache,
            )
        finally:
            if previous is None:
                del os.environ["FAKE_NPM_TARBALLS"]
            else:
                os.environ["FAKE_NPM_TARBALLS"] = previous

    def test_every_locked_package_is_vendored_and_matches_its_integrity(self) -> None:
        self.vendor()

        manifest = json.loads(
            (self.destination / CORRESPONDING_SOURCE.MANIFEST_NAME).read_text()
        )
        locked = {path: entry for path, entry in self.packages.items() if path}
        self.assertEqual(sorted(locked), [entry["path"] for entry in manifest["packages"]])
        self.assertEqual(
            hashlib.sha256(self.lockfile.read_bytes()).hexdigest(),
            manifest["lockfile_sha256"],
        )
        for entry in manifest["packages"]:
            with self.subTest(path=entry["path"]):
                self.assertEqual(locked[entry["path"]]["integrity"], entry["integrity"])
                data = (self.destination / entry["file"]).read_bytes()
                self.assertEqual(entry["integrity"], sri_sha512(data))
        self.assertEqual(
            {
                "agentclientprotocol-sdk-1.3.0.tgz",
                "string-width-4.2.3.tgz",
                "zod-4.4.3.tgz",
                CORRESPONDING_SOURCE.MANIFEST_NAME,
            },
            {entry.name for entry in self.destination.iterdir()},
        )
        for entry in [self.destination, *self.destination.iterdir()]:
            self.assertEqual(1_700_000_000, int(entry.stat().st_mtime))
        CORRESPONDING_SOURCE.verify_npm(self.lockfile, self.destination)

    def test_tarball_that_differs_from_the_lockfile_is_rejected(self) -> None:
        self.tarballs["https://registry.npmjs.org/zod/-/zod-4.4.3.tgz"].write_bytes(b"tampered\n")

        with self.assertRaisesRegex(SourceError, "matches no package-lock.json integrity"):
            self.vendor()

    def test_lock_entry_without_integrity_fails_closed(self) -> None:
        del self.packages["node_modules/zod"]["integrity"]
        self.write_lock(self.packages)

        with self.assertRaisesRegex(SourceError, "lacks resolved, integrity, or version"):
            self.vendor()

    def test_verification_rejects_missing_extra_and_modified_tarballs(self) -> None:
        self.vendor()
        zod = self.destination / "zod-4.4.3.tgz"
        original = zod.read_bytes()

        zod.write_bytes(b"modified\n")
        with self.assertRaisesRegex(SourceError, "does not match package-lock.json integrity"):
            CORRESPONDING_SOURCE.verify_npm(self.lockfile, self.destination)

        zod.unlink()
        with self.assertRaisesRegex(SourceError, "missing=\\['zod-4.4.3.tgz'\\]"):
            CORRESPONDING_SOURCE.verify_npm(self.lockfile, self.destination)

        zod.write_bytes(original)
        (self.destination / "extra-1.0.0.tgz").write_bytes(b"extra\n")
        with self.assertRaisesRegex(SourceError, "extra=\\['extra-1.0.0.tgz'\\]"):
            CORRESPONDING_SOURCE.verify_npm(self.lockfile, self.destination)

    def failing_npm(self) -> Path:
        npm = self.root / "bin" / "npm-offline"
        npm.write_text("#!/bin/sh\necho 'npm must not run' >&2\nexit 1\n", encoding="utf-8")
        npm.chmod(0o755)
        return npm

    def archive_digest(self, vendor_dir: Path, label: str) -> str:
        parent = self.root / f"archive-{label}"
        root = parent / "mini-agent-v1.2.3-ci-source"
        root.mkdir(parents=True)
        shutil.copytree(vendor_dir, root / "vendor-npm")
        output = parent / "out.tar.gz"
        CORRESPONDING_SOURCE.deterministic_tar(
            parent, "mini-agent-v1.2.3-ci-source", output, 1_700_000_000
        )
        return hashlib.sha256(output.read_bytes()).hexdigest()

    def test_cache_reuse_produces_the_same_manifest_and_archive_bytes(self) -> None:
        cache = self.root / "cache"
        uncached = self.root / "uncached"
        self.vendor(destination=uncached)
        self.vendor(cache=cache)
        self.assertEqual(
            {"agentclientprotocol-sdk-1.3.0.tgz", "string-width-4.2.3.tgz", "zod-4.4.3.tgz"},
            {entry.name for entry in cache.iterdir()},
        )

        # A warm cache needs no npm at all; the CLI flag drives the same path.
        reused = self.root / "reused"
        status = CORRESPONDING_SOURCE.main(
            [
                "vendor-npm",
                str(self.lockfile),
                str(reused),
                "--npm",
                str(self.failing_npm()),
                "--mtime",
                "1700000000",
                "--cache",
                str(cache),
            ]
        )
        self.assertEqual(0, status)

        for other in (self.destination, reused):
            with self.subTest(vendor=other.name):
                self.assertEqual(
                    (uncached / CORRESPONDING_SOURCE.MANIFEST_NAME).read_bytes(),
                    (other / CORRESPONDING_SOURCE.MANIFEST_NAME).read_bytes(),
                )
                self.assertEqual(
                    {entry.name: entry.read_bytes() for entry in uncached.iterdir()},
                    {entry.name: entry.read_bytes() for entry in other.iterdir()},
                )
                for entry in [other, *other.iterdir()]:
                    self.assertEqual(1_700_000_000, int(entry.stat().st_mtime))
        digests = {
            self.archive_digest(vendor, vendor.name)
            for vendor in (uncached, self.destination, reused)
        }
        self.assertEqual(1, len(digests))

    def test_cache_supplies_only_tarballs_that_match_the_lockfile(self) -> None:
        cache = self.root / "cache"
        cache.mkdir()
        for url, tarball in self.tarballs.items():
            if "string-width" not in url:
                continue
            shutil.copyfile(tarball, cache / "string-width-4.2.3.tgz")
        (cache / "zod-4.4.3.tgz").write_bytes(b"poisoned cache entry\n")
        (cache / "stale-0.0.1.tgz").write_bytes(b"from an older lockfile\n")
        (cache / "agentclientprotocol-sdk-1.3.0.tgz").symlink_to(
            self.tarballs["https://registry.npmjs.org/@agentclientprotocol/sdk/-/sdk-1.3.0.tgz"]
        )
        log = self.root / "npm.log"
        os.environ["FAKE_NPM_LOG"] = str(log)
        self.addCleanup(os.environ.pop, "FAKE_NPM_LOG", None)

        self.vendor(cache=cache)

        packed = sorted(log.read_text(encoding="utf-8").split())
        self.assertEqual(
            [
                "https://registry.npmjs.org/@agentclientprotocol/sdk/-/sdk-1.3.0.tgz",
                "https://registry.npmjs.org/zod/-/zod-4.4.3.tgz",
            ],
            packed,
        )
        CORRESPONDING_SOURCE.verify_npm(self.lockfile, self.destination)
        # The refreshed cache holds exactly the verified set, as regular files.
        self.assertEqual(
            {"agentclientprotocol-sdk-1.3.0.tgz", "string-width-4.2.3.tgz", "zod-4.4.3.tgz"},
            {entry.name for entry in cache.iterdir()},
        )
        for entry in cache.iterdir():
            with self.subTest(cached=entry.name):
                self.assertFalse(entry.is_symlink())
                self.assertEqual(
                    (self.destination / entry.name).read_bytes(), entry.read_bytes()
                )

    def test_missing_cache_falls_back_to_npm_pack(self) -> None:
        self.vendor(cache=self.root / "absent" / "cache")
        CORRESPONDING_SOURCE.verify_npm(self.lockfile, self.destination)
        self.assertTrue((self.root / "absent" / "cache" / "zod-4.4.3.tgz").is_file())

    def test_manifest_must_describe_the_current_lockfile(self) -> None:
        self.vendor()
        self.packages["node_modules/zod"]["dev"] = True
        self.write_lock(self.packages)

        with self.assertRaisesRegex(SourceError, "does not match"):
            CORRESPONDING_SOURCE.verify_npm(self.lockfile, self.destination)


def package_name(path: str) -> str:
    return CORRESPONDING_SOURCE.package_name_from_path(path)


class RealLockfileTests(unittest.TestCase):
    def test_extension_lockfile_is_fully_vendorable(self) -> None:
        lock = json.loads(REAL_LOCKFILE.read_text(encoding="utf-8"))
        packages = CORRESPONDING_SOURCE.locked_packages(REAL_LOCKFILE)

        self.assertEqual(
            sorted(path for path in lock["packages"] if path),
            [package.path for package in packages],
        )
        for package in packages:
            with self.subTest(path=package.path):
                self.assertTrue(package.integrity.startswith("sha512-"))
                self.assertTrue(package.resolved.startswith("https://registry.npmjs.org/"))
        # Collisions between distinct sources would drop a tarball.
        CORRESPONDING_SOURCE.unique_tarballs(packages)

    def test_runtime_dependencies_bundled_into_the_vsix_are_vendored(self) -> None:
        manifest = json.loads(
            (REPOSITORY_ROOT / "editors" / "vscode" / "package.json").read_text()
        )
        runtime = {
            package.name: package
            for package in CORRESPONDING_SOURCE.locked_packages(REAL_LOCKFILE)
            if not package.dev
        }
        for name, version in manifest["dependencies"].items():
            with self.subTest(dependency=name):
                self.assertIn(name, runtime)
                self.assertEqual(version, runtime[name].version)


class DeterministicTarTests(unittest.TestCase):
    def build_tree(self, parent: Path, mtime: int) -> None:
        root = parent / "mini-agent-v1.2.3-source"
        (root / "b" / "nested").mkdir(parents=True)
        (root / "a.txt").write_text("a\n", encoding="utf-8")
        (root / "b" / "nested" / "z.txt").write_text("z\n", encoding="utf-8")
        tool = root / "b" / "tool.sh"
        tool.write_text("#!/bin/sh\n", encoding="utf-8")
        tool.chmod(0o775)
        (root / "b" / "link").symlink_to("tool.sh")
        for path in [root, *root.rglob("*")]:
            if not path.is_symlink():
                os.utime(path, (mtime, mtime))

    def test_archive_bytes_do_not_depend_on_filesystem_metadata(self) -> None:
        digests = []
        with tempfile.TemporaryDirectory() as first, tempfile.TemporaryDirectory() as second:
            for directory, mtime in ((first, 1_000_000_000), (second, 1_800_000_000)):
                parent = Path(directory)
                self.build_tree(parent, mtime)
                output = parent / "out.tar.gz"
                CORRESPONDING_SOURCE.deterministic_tar(
                    parent, "mini-agent-v1.2.3-source", output, 1_600_000_000
                )
                digests.append(hashlib.sha256(output.read_bytes()).hexdigest())
                with tarfile.open(output) as archive:
                    members = archive.getmembers()
            self.assertEqual(digests[0], digests[1])

        names = [member.name for member in members]
        self.assertEqual(sorted(names), names)
        self.assertEqual("mini-agent-v1.2.3-source", names[0])
        for member in members:
            with self.subTest(member=member.name):
                self.assertEqual(1_600_000_000, member.mtime)
                self.assertEqual((0, 0, "", ""), (member.uid, member.gid, member.uname, member.gname))
        by_name = {member.name: member for member in members}
        self.assertEqual(0o755, by_name["mini-agent-v1.2.3-source/b/tool.sh"].mode)
        self.assertEqual(0o644, by_name["mini-agent-v1.2.3-source/a.txt"].mode)
        self.assertTrue(by_name["mini-agent-v1.2.3-source/b/link"].issym())


class WorkflowNodePolicyTests(unittest.TestCase):
    """Every job that runs the packager must install the pinned Node and npm first."""

    @staticmethod
    def job(workflow: str, name: str) -> str:
        text = (REPOSITORY_ROOT / ".github" / "workflows" / workflow).read_text(encoding="utf-8")
        match = re.search(
            rf"(?ms)^  {re.escape(name)}:\n(?P<body>.*?)(?=^  [a-zA-Z0-9_-]+:\n|\Z)", text
        )
        if match is None:
            raise AssertionError(f"{workflow} job {name!r} is missing")
        return match.group("body")

    def test_packaging_jobs_install_pinned_node_before_the_packager(self) -> None:
        setup = "uses: actions/setup-node@49933ea5288caeca8642d1e84afbd3f7d6820020 # v4.4.0"
        verify = "test \"npm@$(npm --version)\" = \"$(node --print \"require('./package.json').packageManager\")\""
        for workflow, name in (
            ("release.yml", "corresponding-source"),
            ("ci.yml", "corresponding-source"),
        ):
            with self.subTest(workflow=workflow, job=name):
                body = self.job(workflow, name)
                packager = body.index("bash scripts/package-corresponding-source.sh")
                self.assertLess(body.index(setup), packager)
                self.assertIn("node-version-file: editors/vscode/.nvmrc", body)
                self.assertLess(body.index(verify), packager)

    def test_ci_checks_the_assembled_archive_against_the_lockfile(self) -> None:
        body = self.job("ci.yml", "corresponding-source")
        self.assertIn(
            'MINI_AGENT_SOURCE_ARCHIVE="$RUNNER_TEMP/mini-agent-v${version}-ci-source.tar.gz"',
            body,
        )

    def test_ci_assembly_is_code_gated_and_off_the_docs_only_fast_path(self) -> None:
        # mini-agent-yiodv: the unconditional fmt job must not pay for the
        # multi-minute npm vendoring on documentation-only pushes.
        self.assertNotIn("package-corresponding-source.sh", self.job("ci.yml", "fmt"))
        self.assertNotIn("setup-node", self.job("ci.yml", "fmt"))
        body = self.job("ci.yml", "corresponding-source")
        header = body.split("    steps:\n", 1)[0]
        self.assertIn("    needs: changes\n", header)
        self.assertTrue(
            header.lstrip().startswith("if: needs.changes.outputs.code == 'true' && "), header
        )

    def test_ci_caches_npm_tarballs_by_the_lockfile_hash(self) -> None:
        body = self.job("ci.yml", "corresponding-source")
        hashing = 'sha256sum editors/vscode/package-lock.json'
        cache = "uses: actions/cache@caa296126883cff596d87d8935842f9db880ef25 # v5.1.0"
        packager = body.index("bash scripts/package-corresponding-source.sh")
        self.assertLess(body.index(hashing), body.index(cache))
        self.assertLess(body.index(cache), packager)
        self.assertIn("path: ${{ runner.temp }}/npm-vendor-cache", body)
        self.assertIn(
            "key: corresponding-source-npm-${{ steps.npm-lock.outputs.sha256 }}", body
        )
        self.assertIn('--npm-vendor-cache "$RUNNER_TEMP/npm-vendor-cache"', body)
        # Every action in the job is pinned to a full commit SHA.
        for line in body.splitlines():
            if "uses:" in line:
                with self.subTest(uses=line.strip()):
                    self.assertRegex(line, r"uses: [\w./-]+@[0-9a-f]{40} # v\d")

    def test_release_archive_never_reads_the_ci_cache(self) -> None:
        body = self.job("release.yml", "corresponding-source")
        self.assertNotIn("actions/cache", body)
        self.assertNotIn("--npm-vendor-cache", body)


@unittest.skipUnless(
    os.environ.get("MINI_AGENT_SOURCE_ARCHIVE"),
    "set MINI_AGENT_SOURCE_ARCHIVE to an assembled Corresponding Source archive",
)
class AssembledArchiveTests(unittest.TestCase):
    """Checks a real archive produced by package-corresponding-source.sh."""

    def test_npm_sources_are_present_and_match_the_archived_lockfile(self) -> None:
        archive_path = Path(os.environ["MINI_AGENT_SOURCE_ARCHIVE"])
        root = archive_path.name.removesuffix(".tar.gz")
        lockfile_name = f"{root}/editors/vscode/package-lock.json"
        manifest_name = f"{root}/vendor-npm/{CORRESPONDING_SOURCE.MANIFEST_NAME}"
        vendored: dict[str, str] = {}
        names: list[str] = []
        texts: dict[str, bytes] = {}
        mtimes = set()
        with tarfile.open(archive_path) as archive:
            for member in archive:
                names.append(member.name)
                mtimes.add(member.mtime)
                if member.name in (lockfile_name, manifest_name):
                    texts[member.name] = archive.extractfile(member).read()
                elif member.name.startswith(f"{root}/vendor-npm/") and member.isfile():
                    data = archive.extractfile(member).read()
                    vendored[member.name.rsplit("/", 1)[1]] = sri_sha512(data)

        self.assertEqual(sorted(names), names, "archive entries must be sorted")
        self.assertEqual(1, len(mtimes), "archive entries must share one fixed mtime")
        self.assertIn(lockfile_name, texts)
        self.assertIn(manifest_name, texts)
        with tempfile.TemporaryDirectory() as directory:
            lockfile = Path(directory) / "package-lock.json"
            lockfile.write_bytes(texts[lockfile_name])
            packages = CORRESPONDING_SOURCE.locked_packages(lockfile)
            expected_manifest = CORRESPONDING_SOURCE.manifest_for(lockfile, packages)
        self.assertEqual(expected_manifest, json.loads(texts[manifest_name]))
        expected = {package.file: package.integrity for package in packages}
        self.assertEqual(expected, vendored)
        # A fixed gzip header timestamp keeps the compressed bytes reproducible.
        with archive_path.open("rb") as raw:
            self.assertEqual(b"\x00\x00\x00\x00", raw.read(8)[4:8])


if __name__ == "__main__":
    unittest.main()
