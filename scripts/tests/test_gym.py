from __future__ import annotations

import importlib.util
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


def load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


MINE = load("gym_mine_tasks", ROOT / "scripts/gym/mine_tasks.py")


class GymScriptsTests(unittest.TestCase):
    def test_miner_extracts_text_delta_and_validates_fail_to_pass(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
            subprocess.run(["git", "config", "user.email", "gym@example.invalid"], cwd=repo, check=True)
            subprocess.run(["git", "config", "user.name", "Gym Test"], cwd=repo, check=True)
            (repo / "value.txt").write_text("broken\n", encoding="utf-8")
            subprocess.run(["git", "add", "value.txt"], cwd=repo, check=True)
            subprocess.run(["git", "commit", "-qm", "broken"], cwd=repo, check=True)
            parent = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
            (repo / "value.txt").write_text("fixed\n", encoding="utf-8")
            subprocess.run(["git", "commit", "-qam", "fix mini-agent-test"], cwd=repo, check=True)
            commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()

            before, after = MINE.changed_text_files(repo, parent, commit)
            self.assertEqual(before, {"value.txt": "broken\n"})
            self.assertEqual(after, {"value.txt": "fixed\n"})
            self.assertFalse(MINE.oracle_at(repo, parent, "grep -qx fixed value.txt"))
            self.assertTrue(MINE.oracle_at(repo, commit, "grep -qx fixed value.txt"))

    def test_shell_entrypoints_are_syntax_valid_and_training_is_non_production(self) -> None:
        for script in (ROOT / "scripts/gym/setup.sh", ROOT / "scripts/gym/train.sh"):
            subprocess.run(["bash", "-n", str(script)], check=True)
        source = (ROOT / "scripts/gym/train.py").read_text(encoding="utf-8")
        self.assertIn('"MINI_AGENT_GYM": "1"', source)
        self.assertIn('for arm in ("none", "library")', source)


if __name__ == "__main__":
    unittest.main()
