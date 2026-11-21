import shutil
import tempfile
import unittest
from pathlib import Path

from scripts import build_docs_site as site

REPOSITORY = Path(__file__).resolve().parents[2]


class ConfigKeyReferenceTests(unittest.TestCase):
    def test_every_catalogue_key_is_in_the_generated_reference(self) -> None:
        text = (REPOSITORY / "src/config/keys.rs").read_text(encoding="utf-8")
        keys = site.parse_config_keys(text)
        self.assertGreater(len(keys), 150)
        page = site.render_config_keys(keys)
        site.check_reference_covers(keys, page)
        self.assertIn("| `model` |", page)
        aliased = next(k for k in keys if k["name"] == "web-fetch-origins")
        self.assertEqual(["web_fetch_origins"], aliased["aliases"])
        gated = next(k for k in keys if k["name"] == "goal_checks")
        self.assertEqual("goal", gated["feature"])

    def test_a_key_missing_from_the_reference_fails_the_build(self) -> None:
        keys = [{"name": "model", "aliases": [], "feature": None, "live": False, "description": "x"}]
        with self.assertRaises(site.BuildError):
            site.check_reference_covers(keys, "| `other` |")


class ManifestTests(unittest.TestCase):
    def test_checked_in_manifest_lists_exactly_the_published_pages(self) -> None:
        entries = site.parse_manifest((REPOSITORY / "docs/agent/manifest.txt").read_text(encoding="utf-8"))
        with tempfile.TemporaryDirectory() as raw:
            work = Path(raw)
            for md in (REPOSITORY / "docs/agent").rglob("*.md"):
                target = work / md.relative_to(REPOSITORY / "docs/agent")
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy(md, target)
            for name, source in site.EXTRA_PAGES.items():
                shutil.copy(source, work / name)
            (work / site.GENERATED_KEYS_PAGE).write_text("# keys\n", encoding="utf-8")
            site.check_manifest(entries, work)

    def test_entry_without_a_file_fails(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            with self.assertRaisesRegex(site.BuildError, "has no file"):
                site.check_manifest([("Start", "MISSING.md")], Path(raw))

    def test_page_not_in_the_manifest_fails(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            work = Path(raw)
            (work / "A.md").write_text("# A\n")
            (work / "B.md").write_text("# B\n")
            with self.assertRaisesRegex(site.BuildError, "B.md is not in"):
                site.check_manifest([("Start", "A.md")], work)

    def test_malformed_and_duplicate_lines_fail(self) -> None:
        with self.assertRaises(site.BuildError):
            site.parse_manifest("no separator.md")
        with self.assertRaises(site.BuildError):
            site.parse_manifest("S | A.md\nT | A.md")


class LinkTests(unittest.TestCase):
    def test_links_are_rewritten_or_sent_to_github_and_dead_ones_fail(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            work = Path(raw).resolve()
            (work / "A.md").write_text("# A\n")
            (work / "B.md").write_text("# B\n")
            origin = REPOSITORY / "docs/agent/CONFIG.md"
            text = "[a](A.md#x) [src](../../src/update.rs) [web](https://x.test/a.md)"
            out = site.rewrite_links(text, work / "B.md", origin, work)
            self.assertIn("(A.html#x)", out)
            self.assertIn(site.REPO_BLOB + "src/update.rs", out)
            self.assertIn("(https://x.test/a.md)", out)
            with self.assertRaises(site.BuildError):
                site.rewrite_links("[d](nowhere.md)", work / "B.md", origin, work)

    def test_search_index_finds_an_error_message_heading(self) -> None:
        text = (REPOSITORY / "docs/agent/TROUBLESHOOTING.md").read_text(encoding="utf-8")
        meta, body = site.split_front_matter(text)
        entries = site.search_entries("TROUBLESHOOTING.html", meta["title"], body)
        match = [e for e in entries if e["h"] == "setting up uid map: Permission denied"]
        self.assertEqual(1, len(match))
        self.assertEqual("setting-up-uid-map-permission-denied", match[0]["a"])

    def test_full_build_without_pandoc_writes_the_index(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            out = Path(raw) / "site"
            site.build(REPOSITORY / "docs/agent", out, None, REPOSITORY / "src/config/keys.rs")
            self.assertTrue((out / "search-index.json").is_file())


if __name__ == "__main__":
    unittest.main()
