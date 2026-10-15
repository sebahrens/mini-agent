import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "prepare-git-runtime.py"
SPEC = importlib.util.spec_from_file_location("prepare_git_runtime", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class GitRuntimePreparationTests(unittest.TestCase):
    def setUp(self):
        owner = tempfile.TemporaryDirectory(prefix="mini-agent-git-bundle-test-")
        self.addCleanup(owner.cleanup)
        self.root = Path(owner.name)
        (self.root / "scripts/git-runtime").mkdir(parents=True)
        (self.root / "scripts/git-runtime/policy.patch").write_bytes(b"owned policy")
        self.archive = self.root / "source.tar.xz"
        self.archive.write_bytes(b"source fixture")
        self.enterContext(patch.object(MODULE, "SOURCE_SHA256", MODULE.digest(b"source fixture")))
        self.git = self.root / "compiled-git"
        self.http = self.root / "compiled-http"
        self.git.write_bytes(b"trusted publisher Git bytes")
        self.http.write_bytes(b"trusted publisher HTTP bytes")

    def prepare(self, output=None, target=None, libraries=None):
        return MODULE.prepare(
            root=self.root, target=target or MODULE.TARGETS[0],
            source_archive=self.archive, git=self.git, http_helper=self.http,
            https_helper=None, libraries=libraries or [], output=output or self.root / "bundle",
        )

    def test_all_target_inventories_capture_independent_regular_files(self):
        # Inventory support does not claim these fixture bytes run on the target.
        for index, target in enumerate(MODULE.TARGETS):
            out = self.root / f"bundle-{index}"
            identity = self.prepare(output=out, target=target)
            manifest = (out / "bundle.manifest").read_bytes()
            self.assertEqual(identity, hashlib.sha256(manifest).hexdigest())
            self.assertIn(f"target={target}\n".encode(), manifest)
            suffix = ".exe" if "windows" in target else ""
            for name, expected in [("git", self.git.read_bytes()), ("git-remote-http", self.http.read_bytes()), ("git-remote-https", self.http.read_bytes())]:
                file = out / (name + suffix)
                self.assertFalse(file.is_symlink())
                self.assertEqual(file.read_bytes(), expected)
                self.assertEqual(file.stat().st_nlink, 1)
                self.assertIn(MODULE.digest(expected).encode(), manifest)
            self.assertEqual(len(list(out.iterdir())), 4)

    def test_wrong_source_does_not_create_output(self):
        self.archive.write_bytes(b"different source")
        with self.assertRaisesRegex(ValueError, "pinned source"):
            self.prepare()
        self.assertFalse((self.root / "bundle").exists())

    def test_existing_output_is_never_adopted_or_removed(self):
        out = self.root / "bundle"
        out.mkdir()
        (out / "sentinel").write_bytes(b"unrelated existing bytes")
        with self.assertRaises(FileExistsError):
            self.prepare()
        self.assertEqual((out / "sentinel").read_bytes(), b"unrelated existing bytes")
        self.assertEqual(len(list(out.iterdir())), 1)

    def test_case_collision_and_device_names_are_rejected_before_output(self):
        for name in ["GIT", "NUL.dll", "COM1.dll", "unsafe."]:
            with self.subTest(name=name):
                with self.assertRaisesRegex(ValueError, "filename"):
                    self.prepare(libraries=[self.root / name])
                self.assertFalse((self.root / "bundle").exists())

    def test_payload_bound_and_empty_program_refuse_publication(self):
        with self.git.open("wb") as file:
            file.truncate(MODULE.MAX_FILE + 1)
        with self.assertRaisesRegex(ValueError, "size bound"):
            self.prepare()
        self.git.write_bytes(b"")
        with self.assertRaisesRegex(ValueError, "payload size"):
            self.prepare()
        self.assertFalse((self.root / "bundle").exists())

    def test_failed_owned_publication_removes_only_new_output(self):
        out = self.root / "bundle"
        original = Path.open

        def fail_one(path, *args, **kwargs):
            if path.parent == out and path.name == "git-remote-http":
                raise OSError("injected private fixture failure")
            return original(path, *args, **kwargs)

        with patch.object(Path, "open", fail_one):
            with self.assertRaises(OSError):
                self.prepare()
        self.assertFalse(out.exists())
        self.assertEqual(self.git.read_bytes(), b"trusted publisher Git bytes")
        self.assertEqual(self.http.read_bytes(), b"trusted publisher HTTP bytes")

    def test_output_is_a_snapshot_not_a_later_input_read(self):
        self.prepare()
        self.git.write_bytes(b"changed after publication")
        self.assertEqual((self.root / "bundle/git").read_bytes(), b"trusted publisher Git bytes")


if __name__ == "__main__":
    unittest.main()
