import hashlib
import io
import json
import tarfile
import tempfile
import unittest
from pathlib import Path

from scripts import render_package_manifests as render

REPOSITORY = Path(__file__).resolve().parents[2]
SUMS = "\n".join(
    f"{hashlib.sha256(name.encode()).hexdigest()}  {name}"
    for name in [render.SCOOP_ASSET, render.SCOOP_ARM64_ASSET, render.MSI_ASSET]
)


def digest(name: str) -> str:
    return hashlib.sha256(name.encode()).hexdigest()


class RenderManifestTests(unittest.TestCase):
    def test_digest_lookup_requires_exactly_one_wellformed_line(self) -> None:
        self.assertEqual(digest(render.MSI_ASSET), render.digest_for(SUMS, render.MSI_ASSET))
        with self.assertRaises(render.RenderError):
            render.digest_for(SUMS + "\n" + SUMS, render.MSI_ASSET)
        with self.assertRaises(render.RenderError):
            render.digest_for("zz  " + render.MSI_ASSET, render.MSI_ASSET)
        with self.assertRaises(render.RenderError):
            render.digest_for("", render.MSI_ASSET)

    def test_version_must_be_a_release_version(self) -> None:
        self.assertEqual("1.12.0", render.check_version("1.12.0"))
        for bad in ("v1.12.0", "1.12", "latest", "1.12.0;rm"):
            with self.assertRaises(render.RenderError):
                render.check_version(bad)

    def test_winget_manifests_carry_the_exact_msi_digest(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            written = render.render_winget("1.12.0", SUMS, Path(raw))
            self.assertEqual(3, len(written))
            installer = next(p for p in written if "installer" in p.name).read_text()
            self.assertIn(digest(render.MSI_ASSET).upper(), installer)
            self.assertIn("v1.12.0/mini-agent-windows-x64.msi", installer)
            for path in written:
                self.assertEqual([], render.TOKEN.findall(path.read_text()))
                self.assertIn("PackageVersion: 1.12.0", path.read_text())

    def test_scoop_manifest_is_json_with_the_archive_digest(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            (path,) = render.render_scoop("1.12.0", SUMS, Path(raw))
            manifest = json.loads(path.read_text())
            self.assertEqual("1.12.0", manifest["version"])
            self.assertEqual(digest(render.SCOOP_ASSET), manifest["architecture"]["64bit"]["hash"])
            self.assertEqual(digest(render.SCOOP_ARM64_ASSET), manifest["architecture"]["arm64"]["hash"])
            self.assertIn("aarch64-pc-windows-msvc", manifest["architecture"]["arm64"]["url"])
            self.assertIn("aarch64-pc-windows-msvc", manifest["autoupdate"]["architecture"]["arm64"]["url"])

    def test_missing_digest_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            with self.assertRaises(render.RenderError):
                render.render_scoop("1.12.0", "", Path(raw))

    def test_npm_packages_hold_the_verified_binaries(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            archives = root / "archives"
            archives.mkdir()
            lines = []
            for platform, (_os, _cpu, target, binary) in render.NPM_PLATFORMS.items():
                name = f"mini-agent-{target}.tar.gz"
                buffer = io.BytesIO()
                with tarfile.open(fileobj=buffer, mode="w:gz") as tar:
                    for member, body in [(binary, b"bin-" + platform.encode())] + [
                        (n, b"text") for n in render.NPM_LICENSE_FILES
                    ]:
                        info = tarfile.TarInfo(member)
                        info.size = len(body)
                        tar.addfile(info, io.BytesIO(body))
                (archives / name).write_bytes(buffer.getvalue())
                lines.append(f"{hashlib.sha256(buffer.getvalue()).hexdigest()}  {name}")
            out = root / "out"
            written = render.render_npm("1.12.0", "\n".join(lines), archives, out)
            self.assertEqual(6, len(written))
            wrapper = json.loads((out / "mini-agent/package.json").read_text())
            self.assertEqual(5, len(wrapper["optionalDependencies"]))
            self.assertTrue(all(v == "1.12.0" for v in wrapper["optionalDependencies"].values()))
            self.assertNotIn("postinstall", json.dumps(wrapper))
            linux = out / "mini-agent-linux-x64"
            self.assertEqual(b"bin-linux-x64", (linux / "mini-agent").read_bytes())
            meta = json.loads((linux / "package.json").read_text())
            self.assertEqual("x64", meta["cpu"][0])
            self.assertEqual(render.digest_for("\n".join(lines), meta["mini-agent"]["archive"]), meta["mini-agent"]["archiveSha256"])
            # A tampered archive no longer matches its manifest line.
            (archives / "mini-agent-x86_64-unknown-linux-musl.tar.gz").write_bytes(b"tampered")
            with self.assertRaises(render.RenderError):
                render.render_npm("1.12.0", "\n".join(lines), archives, root / "out2")

    def test_templates_hold_only_known_placeholders(self) -> None:
        known = {"VERSION", "MSI_SHA256", "ARCHIVE_SHA256", "ARCHIVE_ARM64_SHA256", "PLATFORM", "OS", "CPU", "BINARY", "ARCHIVE"}
        for template in (REPOSITORY / "packaging").rglob("*.in"):
            tokens = {t.strip("@") for t in render.TOKEN.findall(template.read_text())}
            self.assertLessEqual(tokens, known, template)


if __name__ == "__main__":
    unittest.main()
