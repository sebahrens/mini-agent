import contextlib
import hashlib
import importlib.util
import io
import subprocess
from pathlib import Path
import tarfile
import tempfile
import unittest
import urllib.error
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "collect-git-native-sources.py"
SPEC = importlib.util.spec_from_file_location("native_source_collector", SCRIPT)
COLLECT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(COLLECT)


def tar_bytes(files):
    result = io.BytesIO()
    with tarfile.open(fileobj=result, mode="w:gz") as source:
        for name, data in files.items():
            member = tarfile.TarInfo(name)
            member.size = len(data)
            source.addfile(member, io.BytesIO(data))
    return result.getvalue()


class NativeSourceCollectorTests(unittest.TestCase):
    def setUp(self):
        owner = tempfile.TemporaryDirectory(prefix="mini-agent-native-source-test-")
        self.addCleanup(owner.cleanup)
        self.root = Path(owner.name).resolve()

    def test_msys_signature_failure_reports_bounded_reason_and_exact_files(self):
        command = ["/usr/bin/gpg", "--status-fd", "1", "--verify", "/owned/package.pkg.tar.zst.sig", "/var/cache/pacman/pkg/package.pkg.tar.zst"]
        reason = b"gpg: Can't check signature: No public key\n\x1b[31m" + b"x" * 5000
        def fail(argv, **options):
            self.assertEqual(argv, command)
            self.assertEqual(options["timeout"], 120)
            options["stderr"].write(reason)
            return subprocess.CompletedProcess(argv, 1)
        with patch.object(COLLECT.subprocess, "run", side_effect=fail), self.assertRaises(COLLECT.CollectionError) as raised:
            COLLECT.capture(command)
        detail = str(raised.exception)
        self.assertIn("No public key", detail)
        self.assertIn('"exit": 1', detail)
        self.assertIn('"files": ["package.pkg.tar.zst.sig", "package.pkg.tar.zst"]', detail)
        self.assertNotIn("/owned", detail)
        self.assertNotIn("\x1b", detail)
        self.assertIn("\\u001b", detail)
        self.assertLess(len(detail), 4500)

    def test_other_metadata_failure_keeps_diagnostics_private(self):
        def fail(argv, **options):
            options["stderr"].write(b"unrelated private command diagnostic")
            return subprocess.CompletedProcess(argv, 2)
        with patch.object(COLLECT.subprocess, "run", side_effect=fail), self.assertRaises(COLLECT.CollectionError) as raised:
            COLLECT.capture(["pacman", "-Q", "package"])
        self.assertEqual(str(raised.exception), "required distro metadata/signature command failed: pacman")

    def test_msys_verification_requires_valid_signature_and_same_signature_full_trust(self):
        valid = "[GNUPG:] VALIDSIG " + "A" * 40 + " 2026-10-15 1 0 4 0 1 10 00 " + "A" * 40 + "\n"
        cases = [
            (valid + "[GNUPG:] TRUST_FULLY 0 pgp\n", True),
            (valid + "[GNUPG:] TRUST_ULTIMATE 0 pgp\n", True),
            (valid + "[GNUPG:] TRUST_MARGINAL 0 pgp\n", False),
            (valid, False),
            ("[GNUPG:] TRUST_FULLY 0 pgp\n", False),
            ("[GNUPG:] TRUST_FULLY 0 pgp\n" + valid, False),
            (valid + "[GNUPG:] NEWSIG\n[GNUPG:] TRUST_FULLY 0 pgp\n", False),
        ]
        for status, accepted in cases:
            calls = []
            def capture(command):
                calls.append(command)
                return "/exact/configured/pacman-keyring\n" if command == ["pacman-conf", "GPGDir"] else status
            with self.subTest(status=status), patch.object(COLLECT, "capture", side_effect=capture):
                if accepted:
                    COLLECT.verify_msys2_signature(Path("exact.sig"), Path("exact.pkg.tar.zst"))
                else:
                    with self.assertRaisesRegex(COLLECT.CollectionError, "valid fully/ultimately trusted"):
                        COLLECT.verify_msys2_signature(Path("exact.sig"), Path("exact.pkg.tar.zst"))
            self.assertEqual(calls[0], ["pacman-conf", "GPGDir"])
            self.assertEqual(calls[1], [
                "/usr/bin/gpg", "--homedir", "/exact/configured/pacman-keyring", "--batch",
                "--no-auto-key-retrieve", "--no-auto-key-import", "--auto-key-locate", "clear",
                "--no-auto-check-trustdb", "--lock-never", "--no-random-seed-file",
                "--status-fd", "1", "--verify", "exact.sig", "exact.pkg.tar.zst",
            ])
        with patch.object(COLLECT, "capture", return_value="relative/keyring"), self.assertRaisesRegex(COLLECT.CollectionError, "one absolute"):
            COLLECT.verify_msys2_signature(Path("exact.sig"), Path("exact.pkg.tar.zst"))

    def test_alpine_installed_origin_commit_and_literal_checksums(self):
        database = self.root / "installed"
        database.write_text("P:native-static\nV:1.2-r3\no:native\nc:" + "a" * 40 + "\nL:MIT\n\n")
        fields = COLLECT.alpine_packages(database)["native-static"]
        self.assertEqual(fields["V"], "1.2-r3")
        self.assertEqual(fields["o"], "native")
        self.assertEqual(fields["c"], "a" * 40)
        self.assertEqual(COLLECT.safe("__stack_chk_fail_local.c"), "__stack_chk_fail_local.c")
        checksum = "b" * 128
        self.assertEqual(COLLECT.apk_checksums('sha512sums="\n' + checksum + '  source.tar.gz\n"'), [(checksum, "source.tar.gz")])
        for recipe in ['sha512sums="$computed"', 'sha512sums="\n' + checksum + ' ../escape\n"', 'sha512sums="\nSKIP source.tar.gz\n"']:
            with self.assertRaises(COLLECT.CollectionError):
                COLLECT.apk_checksums(recipe)

    def test_alpine_capture_verifies_sources_and_recipe_local_patch(self):
        source = tar_bytes({"native-1.2/LICENSE": b"Exact upstream native notice"})
        patch_bytes = b"exact patch"
        recipe = ('sha512sums="\n' + hashlib.sha512(source).hexdigest() + ' source.tar.gz\n' + hashlib.sha512(patch_bytes).hexdigest() + ' fix.patch\n"').encode()
        fields = {"o": "native", "V": "1.2-r3", "c": "a" * 40, "L": "MIT"}
        requests = []

        def fetch(url, destination, expected=None, algorithm="sha512"):
            requests.append(url)
            if url.endswith("APKBUILD"):
                data = recipe
            elif url.endswith("source.tar.gz"):
                data = source
            elif "distfiles" in url:
                error = urllib.error.HTTPError(url, 404, "recipe-local", {}, io.BytesIO())
                error.close()
                raise error
            else:
                data = patch_bytes
            if expected:
                self.assertEqual(hashlib.new(algorithm, data).hexdigest(), expected)
            destination.write_bytes(data)

        with patch.object(COLLECT, "fetch", side_effect=fetch):
            version, license, url, sources, notices = COLLECT.collect_alpine("native-static", fields, self.root, "v3.24")
        self.assertEqual((version, license), ("1.2-r3", "MIT"))
        self.assertIn("a" * 40, url)
        self.assertEqual({p.name for p in sources}, {"APKBUILD", "source.tar.gz", "fix.patch", "aports-commit.txt"})
        self.assertEqual([p.read_bytes() for p in notices], [b"Exact upstream native notice"])
        self.assertTrue(any("/distfiles/v3.24/fix.patch" in url for url in requests))
        self.assertTrue(any("/main/native/fix.patch" in url for url in requests))

    def msys_fixture(self, bad_recipe=False, missing_source=False):
        name = "mingw-w64-ucrt-x86_64-native"
        version = "1.2-3"
        upstream = tar_bytes({"native-1.2/LICENSE": b"Verbatim MSYS2 upstream notice"})
        recipe = ("sha256sums=('" + hashlib.sha256(upstream).hexdigest() + "')\n").encode()
        build = ("pkgbase = mingw-w64-native\npkgbuild_sha256sum = " + ("0" * 64 if bad_recipe else hashlib.sha256(recipe).hexdigest()) + "\n").encode()
        info = f"pkgname = {name}\npkgver = {version}\nlicense = MIT\n".encode()
        binary = self.root / f"{name}-{version}-any.pkg.tar.zst"
        binary.write_bytes(tar_bytes({".BUILDINFO": build, ".PKGINFO": info}))
        Path(str(binary) + ".sig").write_bytes(b"controlled verified signature fixture")
        package = tar_bytes({"native/PKGBUILD": recipe, "native/upstream.tar.gz": b"changed" if missing_source else upstream})
        directory = self.root / "output"
        directory.mkdir()
        return name, version, binary, package, directory

    @contextlib.contextmanager
    def fixture_archive(self, path):
        # Synthetic archives use gzip while retaining distribution filenames;
        # actual Windows .zst collection remains hosted acceptance.
        with tarfile.open(path, "r:*") as source:
            yield source, source.getmembers()

    def test_msys_signed_buildinfo_anchors_recipe_and_all_source_checksums(self):
        name, version, binary, package, directory = self.msys_fixture()
        calls = []
        def capture(command):
            calls.append(command)
            if command[:2] == ["pacman", "-Q"]:
                return name + " " + version
            if command == ["pacman-conf", "GPGDir"]:
                return "/configured/keyring"
            return "[GNUPG:] VALIDSIG " + "A" * 40 + "\n[GNUPG:] TRUST_FULLY 0 pgp\n"
        def fetch(url, destination, expected=None):
            self.assertTrue(url.endswith("mingw-w64-native-1.2-3.src.tar.zst"))
            destination.write_bytes(package)
        with patch.object(COLLECT, "capture", side_effect=capture), patch.object(COLLECT, "fetch", side_effect=fetch), patch.object(COLLECT, "archive", side_effect=self.fixture_archive):
            result = COLLECT.collect_msys2(name, directory, self.root)
        self.assertEqual(result[:2], (version, "MIT"))
        self.assertEqual(calls[-1][-3:], ["--verify", str(binary) + ".sig", str(binary)])
        self.assertIn("BUILDINFO", {p.name for p in result[3]})
        self.assertEqual([p.read_bytes() for p in result[4]], [b"Verbatim MSYS2 upstream notice"])

    def test_msys_wrong_recipe_or_missing_checksum_source_refuse(self):
        for bad_recipe, missing_source in [(True, False), (False, True)]:
            with tempfile.TemporaryDirectory() as temporary:
                self.root = Path(temporary)
                name, version, _, package, directory = self.msys_fixture(bad_recipe, missing_source)
                def capture(command):
                    if command[:2] == ["pacman", "-Q"]:
                        return name + " " + version
                    if command == ["pacman-conf", "GPGDir"]:
                        return "/configured/keyring"
                    return "[GNUPG:] VALIDSIG " + "A" * 40 + "\n[GNUPG:] TRUST_FULLY 0 pgp\n"
                expected = "recipe does not match" if bad_recipe else "omits checksum-pinned"
                with patch.object(COLLECT, "capture", side_effect=capture), patch.object(COLLECT, "fetch", side_effect=lambda url, destination, expected=None: destination.write_bytes(package)), patch.object(COLLECT, "archive", side_effect=self.fixture_archive), self.assertRaisesRegex(COLLECT.CollectionError, expected):
                    COLLECT.collect_msys2(name, directory, self.root)

    def test_static_closure_captures_actual_archive_owners_and_compiler_defaults(self):
        for name in ('curl', 'z', 'c', 'gcc', 'gcc_eh'):
            (self.root / ('lib' + name + '.a')).write_bytes(name.encode())
        installed = {'zlib-static': {'V': '1.3-r0'}, 'musl-dev': {'V': '1.2-r0'}, 'gcc': {'V': '15-r0'}, 'libgcc-static': {'V': '15-r0'}}
        owners = {'z': 'zlib-static-1.3-r0', 'c': 'musl-dev-1.2-r0', 'gcc': 'libgcc-static-15-r0', 'gcc_eh': 'gcc-15-r0'}
        def capture(command):
            if command == ['pkg-config', '--modversion', 'libcurl']:
                return '8.23.0'
            if command == ['pkg-config', '--libs', '--static', 'libcurl']:
                return '-L' + str(self.root) + ' -lcurl -lz -pthread'
            if command[:3] == ['apk', 'info', '--who-owns']:
                path = Path(command[3])
                name = path.name.removeprefix('lib').removesuffix('.a')
                return str(path) + ' is owned by ' + owners[name]
            self.fail('Unexpected metadata command: ' + repr(command))
        with patch.object(COLLECT, 'capture', side_effect=capture):
            packages, witness = COLLECT.static_closure(installed, custom_curl=self.root / 'libcurl.a')
            self.assertEqual(packages, set(installed))
            self.assertEqual(len(witness['archives_and_compiler_defaults']), 5)
            self.assertTrue(all(len(item['sha256']) == 64 for item in witness['archives_and_compiler_defaults']))
            owners['z'] = 'not-installed-1.0'
            with self.assertRaisesRegex(COLLECT.CollectionError, 'exact installed package owner'):
                COLLECT.static_closure(installed, custom_curl=self.root / 'libcurl.a')
        with patch.object(COLLECT, 'capture', side_effect=['8.23.0', '-lcurl -Wl,unknown']), self.assertRaisesRegex(COLLECT.CollectionError, 'unsupported static linker flag'):
            COLLECT.static_closure(installed, custom_curl=self.root / 'libcurl.a')

    def test_source_cache_reuses_only_verified_bytes_and_preserves_existing_destination(self):
        cache = self.root / "cache"
        cache.mkdir(mode=0o700)
        data = b"exact distro source"
        expected = hashlib.sha512(data).hexdigest()

        def response():
            stream = io.BytesIO(data)
            stream.url = "https://example.invalid/source"
            return stream

        with patch.object(COLLECT, "SOURCE_CACHE", cache), patch.object(COLLECT.urllib.request, "urlopen", side_effect=lambda *a, **k: response()) as request:
            COLLECT.fetch("https://example.invalid/source", self.root / "first", expected)
            COLLECT.fetch("https://example.invalid/source", self.root / "second", expected)
            self.assertEqual(request.call_count, 1)
            self.assertEqual((self.root / "second").read_bytes(), data)
            (cache / ("sha512-" + expected)).write_bytes(b"changed")
            with self.assertRaisesRegex(COLLECT.CollectionError, "cache checksum"):
                COLLECT.fetch("https://example.invalid/source", self.root / "not-created", expected)
            self.assertFalse((self.root / "not-created").exists())
        existing = self.root / "existing"
        existing.write_bytes(b"preserve")
        with patch.object(COLLECT, "SOURCE_CACHE", None), patch.object(COLLECT.urllib.request, "urlopen", side_effect=lambda *a, **k: response()), self.assertRaises(FileExistsError):
            COLLECT.fetch("https://example.invalid/source", existing)
        self.assertEqual(existing.read_bytes(), b"preserve")


if __name__ == "__main__":
    unittest.main()
