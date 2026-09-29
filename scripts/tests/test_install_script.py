from __future__ import annotations

import hashlib
import os
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).parents[2]
INSTALLER = ROOT / "install.sh"
PACKAGER = ROOT / "scripts/package-release-binary.py"


class InstallScriptTests(unittest.TestCase):
    def test_unknown_option_is_a_usage_error(self) -> None:
        result = subprocess.run(
            ["bash", str(INSTALLER), "--not-an-option"],
            capture_output=True,
            text=True,
        )

        self.assertEqual(2, result.returncode)
        self.assertIn("Unknown option: --not-an-option", result.stderr)

    def test_help_is_successful(self) -> None:
        result = subprocess.run(
            ["bash", str(INSTALLER), "--help"],
            capture_output=True,
            text=True,
        )

        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("Usage: install.sh", result.stdout)

    def make_fixture(self, directory: str, *, include_notice: bool = True) -> tuple[Path, Path]:
        root = Path(directory)
        release = root / "release"
        release.mkdir()
        binary = root / "mini-agent"
        binary.write_text("#!/bin/sh\necho mini-agent 1.7.2\n", encoding="utf-8")
        binary.chmod(0o755)
        archive = release / "mini-agent-aarch64-apple-darwin.tar.gz"

        if include_notice:
            subprocess.run(
                [
                    "python3",
                    str(PACKAGER),
                    "--root",
                    str(ROOT),
                    "--binary",
                    str(binary),
                    "--archive",
                    str(archive),
                    "--executable-name",
                    "mini-agent",
                ],
                check=True,
            )
        else:
            with tarfile.open(archive, "w:gz") as packaged:
                packaged.add(binary, arcname="mini-agent")
                packaged.add(ROOT / "LICENSE", arcname="LICENSE")
                packaged.add(ROOT / "SOURCE.md", arcname="SOURCE.md")

        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        (release / "SHA256SUMS").write_text(
            f"{digest}  {archive.name}\n", encoding="utf-8"
        )

        stub_bin = root / "stub-bin"
        stub_bin.mkdir()
        curl = stub_bin / "curl"
        # Anonymous requests are served from INSTALL_TEST_RELEASE. Requests to
        # the GitHub REST API are served from INSTALL_TEST_AUTH_RELEASE and
        # only when a header file carries "Bearer $INSTALL_TEST_TOKEN".
        curl.write_text(
            """#!/bin/bash
set -euo pipefail
output=""
auth=""
for ((index = 1; index <= $#; index++)); do
    next=$((index + 1))
    if [[ "${!index}" == "-o" ]]; then
        output="${!next}"
    elif [[ "${!index}" == "-H" && "${!next}" == @* ]]; then
        header_file="${!next#@}"
        auth="$(cat "$header_file")"
    fi
done
url="${!#}"
printf '%s\\n' "$*" >> "${INSTALL_TEST_CURL_LOG:-/dev/null}"
case "$url" in
    https://api.github.com/*)
        if [[ "$auth" != "Authorization: Bearer ${INSTALL_TEST_TOKEN:-}" ]]; then
            exit 22
        fi
        case "$url" in
            */releases/tags/v1.7.2|*/releases/latest)
                cp "${INSTALL_TEST_AUTH_RELEASE}/release.json" "$output" ;;
            */releases/assets/101)
                cp "${INSTALL_TEST_AUTH_RELEASE}/mini-agent-aarch64-apple-darwin.tar.gz" "$output" ;;
            */releases/assets/102)
                cp "${INSTALL_TEST_AUTH_RELEASE}/SHA256SUMS" "$output" ;;
            *) exit 22 ;;
        esac
        ;;
    *)
        filename="${url##*/}"
        cp "${INSTALL_TEST_RELEASE}/${filename}" "$output"
        ;;
esac
""",
            encoding="utf-8",
        )
        curl.chmod(0o755)
        gh = stub_bin / "gh"
        gh.write_text(
            """#!/bin/bash
set -euo pipefail
printf '%s\\n' "$*" >> "${INSTALL_TEST_GH_LOG:-/dev/null}"
if [[ "$1 $2" == "auth status" ]]; then
    exit "${INSTALL_TEST_GH_AUTH_STATUS:-0}"
fi
if [[ "$1 $2" == "release download" ]]; then
    dir=""
    for ((index = 1; index <= $#; index++)); do
        next=$((index + 1))
        if [[ "${!index}" == "--dir" ]]; then
            dir="${!next}"
        fi
    done
    cp "${INSTALL_TEST_AUTH_RELEASE}/mini-agent-aarch64-apple-darwin.tar.gz" "$dir/"
    cp "${INSTALL_TEST_AUTH_RELEASE}/SHA256SUMS" "$dir/"
    exit 0
fi
exit 2
""",
            encoding="utf-8",
        )
        gh.chmod(0o755)
        uname = stub_bin / "uname"
        uname.write_text(
            """#!/bin/sh
if [ "$1" = "-s" ]; then
    echo Darwin
else
    echo arm64
fi
""",
            encoding="utf-8",
        )
        uname.chmod(0o755)
        return release, stub_bin

    def run_installer(
        self,
        root: Path,
        release: Path,
        stub_bin: Path,
        *,
        install_dir: str | None = None,
        path_prefix: list[Path] | None = None,
        home: Path | None = None,
        extra_env: dict[str, str] | None = None,
        release_version: str | None = "1.7.2",
    ) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        # A developer's real credentials and gh login must never reach the
        # installer under test; authenticated tests opt back in explicitly.
        for name in ("GITHUB_TOKEN", "GH_TOKEN", "MINI_AGENT_INSTALL_NO_TOKEN"):
            env.pop(name, None)
        env["MINI_AGENT_INSTALL_NO_GH"] = "1"
        env.update(extra_env or {})
        # Keep any real mini-agent on the developer's PATH from influencing
        # the PATH-resolution checks.
        system_path = os.pathsep.join(
            entry
            for entry in env["PATH"].split(os.pathsep)
            if entry and not (Path(entry) / "mini-agent").exists()
        )
        prefix = "".join(f"{entry}:" for entry in path_prefix or [])
        env["PATH"] = f"{prefix}{stub_bin}:{system_path}"
        env["INSTALL_TEST_RELEASE"] = str(release)
        if home is not None:
            env["HOME"] = str(home)
        release_args = ["--release", release_version] if release_version else []
        return subprocess.run(
            [
                "bash",
                str(INSTALLER),
                *release_args,
                "--dir",
                install_dir if install_dir is not None else str(root / "prefix" / "bin"),
            ],
            env=env,
            cwd=root,
            capture_output=True,
            text=True,
        )

    def assert_rejected_before_install(
        self, root: Path, release: Path, stub_bin: Path, message: str
    ) -> None:
        result = self.run_installer(root, release, stub_bin)
        self.assertNotEqual(0, result.returncode)
        self.assertIn(message, result.stderr)
        self.assertFalse((root / "prefix/bin/mini-agent").exists())

    def test_installs_binary_license_notice_and_source_directions(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)

            result = self.run_installer(root, release, stub_bin)

            self.assertEqual(0, result.returncode, result.stderr)
            self.assertTrue((root / "prefix/bin/mini-agent").is_file())
            for document in ("LICENSE", "NOTICE", "SOURCE.md"):
                self.assertEqual(
                    (ROOT / document).read_bytes(),
                    (root / "prefix/share/doc/mini-agent" / document).read_bytes(),
                )

    def test_missing_notice_fails_before_installing_binary(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory, include_notice=False)

            result = self.run_installer(root, release, stub_bin)

            self.assertNotEqual(0, result.returncode)
            self.assertIn("required GPL document NOTICE", result.stderr)
            self.assertFalse((root / "prefix/bin/mini-agent").exists())

    def test_missing_checksum_entry_fails_before_extraction(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            (release / "SHA256SUMS").write_text(
                "0" * 64 + "  mini-agent-x86_64-unknown-linux-gnu.tar.gz\n",
                encoding="ascii",
            )
            self.assert_rejected_before_install(root, release, stub_bin, "has no entry")

    def test_checksum_download_failure_fails_before_extraction(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            (release / "SHA256SUMS").unlink()

            result = self.run_installer(root, release, stub_bin)

            self.assertNotEqual(0, result.returncode)
            self.assertFalse((root / "prefix/bin/mini-agent").exists())
            self.assertFalse((root / "prefix").exists())

    def test_duplicate_checksum_entry_fails_before_extraction(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            manifest = release / "SHA256SUMS"
            manifest.write_bytes(manifest.read_bytes() * 2)
            self.assert_rejected_before_install(root, release, stub_bin, "duplicate entries")

    def test_malformed_checksum_entry_fails_before_extraction(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            archive = "mini-agent-aarch64-apple-darwin.tar.gz"
            (release / "SHA256SUMS").write_text(
                f"not-a-sha256  {archive}\n", encoding="ascii"
            )
            self.assert_rejected_before_install(root, release, stub_bin, "malformed hash")

    def test_noncanonical_selected_line_fails_before_extraction(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            manifest = release / "SHA256SUMS"
            manifest.write_text(manifest.read_text(encoding="ascii").rstrip() + "  extra\n")
            self.assert_rejected_before_install(root, release, stub_bin, "malformed entry")

    def test_checksum_mismatch_fails_before_extraction(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            archive = release / "mini-agent-aarch64-apple-darwin.tar.gz"
            archive.write_bytes(archive.read_bytes() + b"tampered")
            self.assert_rejected_before_install(root, release, stub_bin, "checksum mismatch")

    def test_filename_matching_treats_dots_literally(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            canonical = "mini-agent-aarch64-apple-darwin.tar.gz"
            misleading = canonical.replace(".", "x")
            (release / "SHA256SUMS").write_text(
                f"{'0' * 64}  {misleading}\n", encoding="ascii"
            )
            self.assert_rejected_before_install(root, release, stub_bin, "has no entry")

    def test_upgrade_replaces_existing_binary_by_rename(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            bin_dir = root / "prefix/bin"
            bin_dir.mkdir(parents=True)
            target = bin_dir / "mini-agent"
            target.write_text("#!/bin/sh\necho old\n", encoding="utf-8")
            target.chmod(0o755)
            # A second link to the old inode shows whether the installer
            # rewrote the running binary in place or replaced the entry.
            running = root / "running-old-binary"
            os.link(target, running)

            result = self.run_installer(root, release, stub_bin)

            self.assertEqual(0, result.returncode, result.stderr)
            self.assertEqual("#!/bin/sh\necho old\n", running.read_text(encoding="utf-8"))
            self.assertIn("mini-agent 1.7.2", target.read_text(encoding="utf-8"))
            self.assertNotEqual(running.stat().st_ino, target.stat().st_ino)
            self.assertTrue(os.access(target, os.X_OK))
            self.assertEqual(["mini-agent"], sorted(p.name for p in bin_dir.iterdir()))

    def test_html_archive_reports_download_problem_not_checksum_entry(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            sign_in = "<!DOCTYPE html>\n<html><body>Sign in to GitHub</body></html>\n"
            (release / "mini-agent-aarch64-apple-darwin.tar.gz").write_text(sign_in)
            (release / "SHA256SUMS").write_text(sign_in)

            result = self.run_installer(root, release, stub_bin)

            self.assertNotEqual(0, result.returncode)
            self.assertIn("not a release asset (received an HTML page)", result.stderr)
            self.assertIn("gh release download", result.stderr)
            self.assertNotIn("has no entry", result.stderr)
            self.assertFalse((root / "prefix").exists())

    def test_html_checksum_manifest_reports_download_problem(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            (release / "SHA256SUMS").write_text("<html><title>Sign in</title></html>\n")

            result = self.run_installer(root, release, stub_bin)

            self.assertNotEqual(0, result.returncode)
            self.assertIn("checksum manifest SHA256SUMS is not a release asset", result.stderr)
            self.assertFalse((root / "prefix").exists())

    def test_non_gzip_archive_is_rejected_before_checksum(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            (release / "mini-agent-aarch64-apple-darwin.tar.gz").write_bytes(b"not found\n")

            result = self.run_installer(root, release, stub_bin)

            self.assertNotEqual(0, result.returncode)
            self.assertIn("data that is not gzip", result.stderr)

    def test_tilde_install_directory_expands_to_home(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            home = root / "home"
            home.mkdir()

            result = self.run_installer(
                root, release, stub_bin, install_dir="~/bin", home=home
            )

            self.assertEqual(0, result.returncode, result.stderr)
            self.assertTrue((home / "bin/mini-agent").is_file())
            self.assertFalse((root / "~").exists())

    def test_path_hint_matches_whole_entries_only(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            lookalike = root / "prefix/bin2"
            lookalike.mkdir(parents=True)

            result = self.run_installer(root, release, stub_bin, path_prefix=[lookalike])

            self.assertEqual(0, result.returncode, result.stderr)
            self.assertIn("is not in your PATH", result.stdout)

    def test_warns_when_path_resolves_to_a_shadowing_binary(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            shadow = root / "shadow"
            shadow.mkdir()
            (shadow / "mini-agent").write_text("#!/bin/sh\necho shadow\n")
            (shadow / "mini-agent").chmod(0o755)
            bin_dir = root / "prefix/bin"

            result = self.run_installer(
                root, release, stub_bin, path_prefix=[shadow, Path(f"{bin_dir}/")]
            )

            self.assertEqual(0, result.returncode, result.stderr)
            self.assertNotIn("is not in your PATH", result.stdout)
            self.assertIn("resolves to a different binary", result.stdout)
            self.assertIn(f"Resolves to: {shadow}/mini-agent", result.stdout)
            self.assertIn("hash -r", result.stdout)

    def test_no_path_warning_when_install_directory_wins(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)

            result = self.run_installer(
                root, release, stub_bin, path_prefix=[root / "prefix/bin"]
            )

            self.assertEqual(0, result.returncode, result.stderr)
            self.assertNotIn("is not in your PATH", result.stdout)
            self.assertNotIn("different binary", result.stdout)

    # ---- authenticated fallbacks (GITHUB_TOKEN / gh) ----

    TOKEN = "ghp_test_token_value"

    def make_private_fixture(self, directory: str) -> tuple[Path, Path, dict[str, str]]:
        """A release whose anonymous URLs serve a sign-in page while the
        authenticated API and gh serve the real assets."""
        root = Path(directory)
        release, stub_bin = self.make_fixture(directory)
        auth_release = root / "auth-release"
        auth_release.mkdir()
        for name in ("mini-agent-aarch64-apple-darwin.tar.gz", "SHA256SUMS"):
            (auth_release / name).write_bytes((release / name).read_bytes())
            (release / name).write_text("<!DOCTYPE html><html>Sign in</html>\n")
        api = "https://api.github.com/repos/sebahrens/mini-agent/releases"
        (auth_release / "release.json").write_text(
            "{\n"
            f'  "url": "{api}/9",\n'
            '  "name": "v1.7.2",\n'
            '  "assets": [\n'
            # A foreign asset URL for the same name must never be used.
            '    {"url": "https://evil.example/repos/x/releases/assets/7",'
            ' "name": "decoy"},\n'
            f'    {{"url": "{api}/assets/101", "id": 101,'
            ' "name": "mini-agent-aarch64-apple-darwin.tar.gz",'
            ' "uploader": {"url": "https://api.github.com/users/someone"}},\n'
            f'    {{"url": "{api}/assets/102", "id": 102, "name": "SHA256SUMS"}}\n'
            "  ]\n"
            "}\n",
            encoding="utf-8",
        )
        env = {
            "INSTALL_TEST_AUTH_RELEASE": str(auth_release),
            "INSTALL_TEST_TOKEN": self.TOKEN,
            "INSTALL_TEST_CURL_LOG": str(root / "curl.log"),
            "INSTALL_TEST_GH_LOG": str(root / "gh.log"),
        }
        return release, stub_bin, env

    def test_github_token_fallback_installs_private_release(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin, env = self.make_private_fixture(directory)
            env["GITHUB_TOKEN"] = self.TOKEN

            result = self.run_installer(root, release, stub_bin, extra_env=env)

            self.assertEqual(0, result.returncode, result.stderr)
            self.assertIn("retrying with GITHUB_TOKEN", result.stderr)
            self.assertIn("Downloaded release assets with GITHUB_TOKEN", result.stdout)
            self.assertTrue((root / "prefix/bin/mini-agent").is_file())
            curl_log = (root / "curl.log").read_text()
            # The token travels in a header file, never on the command line.
            self.assertNotIn(self.TOKEN, curl_log)
            self.assertIn("/releases/tags/v1.7.2", curl_log)
            self.assertIn("/releases/assets/101", curl_log)
            self.assertNotIn("evil.example", curl_log)
            self.assertFalse((root / "gh.log").exists())

    def test_github_token_fallback_still_verifies_checksum(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin, env = self.make_private_fixture(directory)
            archive = Path(env["INSTALL_TEST_AUTH_RELEASE"]) / (
                "mini-agent-aarch64-apple-darwin.tar.gz"
            )
            archive.write_bytes(archive.read_bytes() + b"tampered")
            env["GITHUB_TOKEN"] = self.TOKEN

            result = self.run_installer(root, release, stub_bin, extra_env=env)

            self.assertNotEqual(0, result.returncode)
            self.assertIn("checksum mismatch", result.stderr)
            self.assertFalse((root / "prefix/bin/mini-agent").exists())

    def test_rejected_github_token_reports_original_problem(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin, env = self.make_private_fixture(directory)
            env["GITHUB_TOKEN"] = "wrong-token"

            result = self.run_installer(root, release, stub_bin, extra_env=env)

            self.assertNotEqual(0, result.returncode)
            self.assertIn("not a release asset (received an HTML page)", result.stderr)
            self.assertIn("Authenticated retries also failed: GITHUB_TOKEN", result.stderr)
            self.assertFalse((root / "prefix").exists())

    def test_no_token_switch_ignores_github_token(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin, env = self.make_private_fixture(directory)
            env["GITHUB_TOKEN"] = self.TOKEN
            env["MINI_AGENT_INSTALL_NO_TOKEN"] = "1"

            result = self.run_installer(root, release, stub_bin, extra_env=env)

            self.assertNotEqual(0, result.returncode)
            self.assertNotIn("api.github.com", (root / "curl.log").read_text())

    def test_authenticated_gh_fallback_installs_private_release(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin, env = self.make_private_fixture(directory)
            env["MINI_AGENT_INSTALL_NO_GH"] = ""

            result = self.run_installer(root, release, stub_bin, extra_env=env)

            self.assertEqual(0, result.returncode, result.stderr)
            self.assertIn("Downloaded release assets with gh", result.stdout)
            self.assertTrue((root / "prefix/bin/mini-agent").is_file())
            gh_log = (root / "gh.log").read_text().splitlines()
            self.assertEqual("auth status", gh_log[0])
            self.assertEqual(
                "release download v1.7.2 --repo sebahrens/mini-agent"
                " --pattern mini-agent-aarch64-apple-darwin.tar.gz"
                " --pattern SHA256SUMS --dir ",
                gh_log[1][: gh_log[1].index("--dir ") + len("--dir ")],
            )

    def test_gh_fallback_for_latest_release_omits_tag(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin, env = self.make_private_fixture(directory)
            env["MINI_AGENT_INSTALL_NO_GH"] = ""

            result = self.run_installer(
                root, release, stub_bin, extra_env=env, release_version=None
            )

            self.assertEqual(0, result.returncode, result.stderr)
            gh_log = (root / "gh.log").read_text().splitlines()
            self.assertTrue(gh_log[1].startswith("release download --repo "), gh_log)

    def test_gh_fallback_still_verifies_checksum(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin, env = self.make_private_fixture(directory)
            (Path(env["INSTALL_TEST_AUTH_RELEASE"]) / "SHA256SUMS").write_text(
                "0" * 64 + "  mini-agent-aarch64-apple-darwin.tar.gz\n"
            )
            env["MINI_AGENT_INSTALL_NO_GH"] = ""

            result = self.run_installer(root, release, stub_bin, extra_env=env)

            self.assertNotEqual(0, result.returncode)
            self.assertIn("checksum mismatch", result.stderr)
            self.assertFalse((root / "prefix/bin/mini-agent").exists())

    def test_unauthenticated_gh_is_not_used(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin, env = self.make_private_fixture(directory)
            env["MINI_AGENT_INSTALL_NO_GH"] = ""
            env["INSTALL_TEST_GH_AUTH_STATUS"] = "1"

            result = self.run_installer(root, release, stub_bin, extra_env=env)

            self.assertNotEqual(0, result.returncode)
            self.assertIn("gh auth login", result.stderr)
            self.assertEqual(["auth status"], (root / "gh.log").read_text().splitlines())
            self.assertFalse((root / "prefix").exists())

    def test_no_gh_switch_never_invokes_gh(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin, env = self.make_private_fixture(directory)

            result = self.run_installer(root, release, stub_bin, extra_env=env)

            self.assertNotEqual(0, result.returncode)
            self.assertFalse((root / "gh.log").exists())

    def test_successful_anonymous_download_never_uses_credentials(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release, stub_bin = self.make_fixture(directory)
            env = {
                "GITHUB_TOKEN": self.TOKEN,
                "MINI_AGENT_INSTALL_NO_GH": "",
                "INSTALL_TEST_CURL_LOG": str(root / "curl.log"),
                "INSTALL_TEST_GH_LOG": str(root / "gh.log"),
            }

            result = self.run_installer(root, release, stub_bin, extra_env=env)

            self.assertEqual(0, result.returncode, result.stderr)
            self.assertNotIn("api.github.com", (root / "curl.log").read_text())
            self.assertNotIn(" -H ", (root / "curl.log").read_text())
            self.assertFalse((root / "gh.log").exists())


if __name__ == "__main__":
    unittest.main()
