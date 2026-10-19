import importlib.util
import os
import tempfile
import time
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "sweep-target.py"
spec = importlib.util.spec_from_file_location("sweep_target", SCRIPT)
sweep = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sweep)


def touch(path: Path, age_days: float, content: bytes = b"x") -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(content)
    stamp = time.time() - age_days * 86400
    os.utime(path, (stamp, stamp))


class SweepTargetTests(unittest.TestCase):
    def build_target(self, root: Path) -> None:
        debug = root / "debug"
        touch(debug / "deps" / "mini_agent-old", 30)
        touch(debug / "deps" / "libmini_agent-old.rmeta", 30)
        touch(debug / "deps" / "mini_agent-new", 1)
        touch(debug / "deps" / "serde-old", 90)
        touch(debug / ".fingerprint" / "mini-agent-old" / "bin-mini-agent", 30)
        touch(debug / "incremental" / "mini_agent-old" / "s-1" / "dep-graph.bin", 30)
        touch(debug / "incremental" / "mini_agent-mixed" / "s-1" / "old.bin", 30)
        touch(debug / "incremental" / "mini_agent-mixed" / "s-2" / "new.bin", 1)

    def test_dry_run_selects_only_stale_own_artefacts(self):
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp)
            self.build_target(target)
            names = {
                str(p.relative_to(target)) for p, _ in sweep.select_stale(target, 14)
            }
            self.assertEqual(
                names,
                {
                    "debug/deps/mini_agent-old",
                    "debug/deps/libmini_agent-old.rmeta",
                    "debug/.fingerprint/mini-agent-old",
                    "debug/incremental/mini_agent-old",
                },
            )

    def test_default_run_deletes_nothing_and_apply_deletes(self):
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp)
            self.build_target(target)
            self.assertEqual(sweep.main(["--target-dir", tmp]), 0)
            self.assertTrue((target / "debug/deps/mini_agent-old").exists())
            self.assertEqual(sweep.main(["--target-dir", tmp, "--apply"]), 0)
            self.assertFalse((target / "debug/deps/mini_agent-old").exists())
            self.assertFalse((target / "debug/incremental/mini_agent-old").exists())
            self.assertTrue((target / "debug/deps/mini_agent-new").exists())
            self.assertTrue((target / "debug/deps/serde-old").exists())
            self.assertTrue((target / "debug/incremental/mini_agent-mixed").exists())

    def test_missing_target_is_an_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(sweep.main(["--target-dir", str(Path(tmp) / "nope")]), 1)


if __name__ == "__main__":
    unittest.main()
