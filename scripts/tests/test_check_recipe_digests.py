import importlib.util
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "check_recipe_digests.py"
spec = importlib.util.spec_from_file_location("check_recipe_digests", SCRIPT)
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)


class RecipeDigestTests(unittest.TestCase):
    def make_root(self, body: str) -> Path:
        root = Path(tempfile.mkdtemp())
        for name in checker.RECIPES:
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(body)
        return root

    def test_counts_only_exact_all_zero_digests(self):
        real = "ab" * 32
        root = self.make_root(f'sha256 "{"0" * 64}"\nsha256 "{real}"\nsha256 "{"0" * 65}"\n')
        self.assertEqual(set(checker.placeholder_counts(root).values()), {1})

    def test_filled_recipes_report_nothing(self):
        root = self.make_root(f'sha256 "{"ab" * 32}"\n')
        self.assertEqual(set(checker.placeholder_counts(root).values()), {0})

    def test_repository_recipes_are_readable(self):
        self.assertEqual(set(checker.placeholder_counts()), set(checker.RECIPES))

    def test_strict_mode_fails_while_placeholders_remain(self):
        self.assertEqual(checker.main(["--bogus"]), 2)
        unfilled = any(checker.placeholder_counts().values())
        self.assertEqual(checker.main(["--require-filled"]), 1 if unfilled else 0)
        self.assertEqual(checker.main([]), 0)


if __name__ == "__main__":
    unittest.main()
