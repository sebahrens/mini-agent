import importlib.util
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "extract-changelog.py"
SPEC = importlib.util.spec_from_file_location("extract_changelog", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
EXTRACT_CHANGELOG = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(EXTRACT_CHANGELOG)


class ExtractChangelogTests(unittest.TestCase):
    def test_extracts_dated_version_section_only(self) -> None:
        changelog = """# Changelog

## [Unreleased]

Next.

## [1.8.0] - 2026-08-25

### Added

- Structured Git.

## [1.7.2] - 2026-07-01

- Older.
"""

        notes = EXTRACT_CHANGELOG.extract_version(changelog, "1.8.0")

        self.assertEqual("### Added\n\n- Structured Git.\n", notes)

    def test_rejects_missing_version(self) -> None:
        with self.assertRaisesRegex(ValueError, "no release notes for 2.0.0"):
            EXTRACT_CHANGELOG.extract_version("## [1.8.0]\n\n- Present.\n", "2.0.0")

    def test_oldest_release_excludes_reference_links(self) -> None:
        changelog = """# Changelog

## [1.0.0]

- First release.

[Unreleased]: https://example.invalid/compare/v1.0.0...HEAD
[1.0.0]: https://example.invalid/releases/v1.0.0
"""

        self.assertEqual(
            "- First release.\n",
            EXTRACT_CHANGELOG.extract_version(changelog, "1.0.0"),
        )


if __name__ == "__main__":
    unittest.main()
