from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).parents[1] / "third_party_licenses.py"
SPEC = importlib.util.spec_from_file_location("third_party_licenses", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
INVENTORY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(INVENTORY)
ROOT = Path(__file__).parents[2]
TARGET = "x86_64-unknown-linux-musl"


class InventoryFixture:
    def __init__(self, directory: str) -> None:
        self.base = Path(directory)
        self.root_id = "path+file:///repo#mini-agent@1.9.4"
        self.packages: list[dict] = [
            {
                "id": self.root_id,
                "name": "mini-agent",
                "version": "1.9.4",
                "license": "GPL-3.0-only",
                "manifest_path": str(self.base / "repo/Cargo.toml"),
            }
        ]

    def crate(
        self,
        name: str,
        version: str,
        license: str | None,
        files: dict[str, str] | None = None,
        *,
        authors: list[str] | None = None,
        license_file: str | None = None,
    ) -> None:
        directory = self.base / f"{name}-{version}"
        directory.mkdir(parents=True)
        (directory / "Cargo.toml").write_text("[package]\n", encoding="utf-8")
        for relative, text in (files or {}).items():
            path = directory / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")
        self.packages.append(
            {
                "id": f"registry+https://github.com/rust-lang/crates.io-index#{name}@{version}",
                "name": name,
                "version": version,
                "license": license,
                "license_file": license_file,
                "authors": authors or [],
                "repository": f"https://example.invalid/{name}",
                "source": "registry+https://github.com/rust-lang/crates.io-index",
                "manifest_path": str(directory / "Cargo.toml"),
            }
        )

    def metadata(self) -> dict:
        return {
            "packages": self.packages,
            "workspace_members": [self.root_id],
            "resolve": {
                "root": self.root_id,
                "nodes": [{"id": package["id"]} for package in self.packages],
            },
        }


class ThirdPartyInventoryTests(unittest.TestCase):
    def render(self, fixture: InventoryFixture, **kwargs) -> str:
        return INVENTORY.render(
            fixture.metadata(),
            target=TARGET,
            no_default_features=kwargs.pop("no_default_features", False),
            texts_root=ROOT / "packaging/license-texts",
            **kwargs,
        )

    def test_inventory_names_every_package_with_its_verbatim_license_files(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = InventoryFixture(directory)
            fixture.crate(
                "ahash",
                "0.8.12",
                "MIT OR Apache-2.0",
                {"LICENSE-MIT": "Copyright (c) 2018 Tom Kaitchuck\nMIT terms\n",
                 "LICENSE-APACHE": "Apache terms\n",
                 "README.md": "not a license\n"},
            )
            fixture.crate(
                "ring",
                "0.17.14",
                "Apache-2.0 AND ISC",
                {"LICENSE": "ring ISC\n", "NOTICE": "ring notice\n"},
            )
            fixture.crate("other", "1.0.0", "Apache-2.0", {"LICENSE": "Apache terms\n"})
            document = self.render(fixture)

        self.assertTrue(document.startswith(INVENTORY.HEADER + "\n"))
        self.assertIn(f"Target: {TARGET}\n", document)
        self.assertIn("Features: default\n", document)
        self.assertIn("Packages: 3\n", document)
        self.assertNotIn("Package: mini-agent", document)
        for line in (
            "Package: ahash 0.8.12\nLicense: MIT OR Apache-2.0\n",
            "Package: ring 0.17.14\nLicense: Apache-2.0 AND ISC\n",
            "Package: other 1.0.0\nLicense: Apache-2.0\n",
            "Copyright (c) 2018 Tom Kaitchuck\nMIT terms\n",
            "ring notice\n",
        ):
            self.assertIn(line, document)
        self.assertNotIn("not a license", document)
        # Identical texts are printed once and referenced by each package.
        self.assertEqual(1, document.count("Apache terms\n"))
        self.assertEqual(
            {("ahash", "0.8.12"), ("ring", "0.17.14"), ("other", "1.0.0")},
            INVENTORY.parse_inventory(document)["packages"],
        )

    def test_vendored_native_trees_contribute_their_license_files(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = InventoryFixture(directory)
            fixture.crate(
                "rquickjs-sys",
                "0.12.2",
                "MIT",
                {
                    "quickjs/LICENSE": "Copyright (c) 2017-2026 Fabrice Bellard\n",
                    "quickjs/tests/LICENSE": "test fixture license\n",
                },
                authors=["Mees Delzenne <mees.delzenne@gmail.com>"],
            )
            document = self.render(fixture)

        self.assertIn("  quickjs/LICENSE: [", document)
        self.assertIn("Copyright (c) 2017-2026 Fabrice Bellard\n", document)
        self.assertNotIn("test fixture license", document)
        # The crate ships no license of its own, so its canonical MIT text is
        # added with the declared authors in addition to the vendored tree's.
        self.assertIn("  MIT (canonical text): [", document)
        self.assertIn("Copyright (c) Mees Delzenne <mees.delzenne@gmail.com>\n", document)

    def test_package_without_license_files_gets_canonical_text(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = InventoryFixture(directory)
            fixture.crate("rmcp", "2.2.0", "Apache-2.0")
            fixture.crate("valuable", "0.1.1", "MIT")
            document = self.render(fixture)

        self.assertIn("  Apache-2.0 (canonical text): [", document)
        self.assertIn("TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION", document)
        self.assertIn("Copyright (c) the valuable authors\n", document)

    def test_unknown_license_without_files_fails_closed(self) -> None:
        cases = (
            ("BSD-3-Clause", "no canonical text"),
            ("MIT AND BSD-3-Clause", "no canonical text"),
            (None, "declares no license"),
        )
        for license, message in cases:
            with self.subTest(license=license), tempfile.TemporaryDirectory() as directory:
                fixture = InventoryFixture(directory)
                fixture.crate("mystery", "1.0.0", license)
                with self.assertRaisesRegex(INVENTORY.InventoryError, message):
                    self.render(fixture)

    def test_declared_license_file_is_included(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = InventoryFixture(directory)
            fixture.crate(
                "custom",
                "1.0.0",
                None,
                {"legal/TERMS.txt": "custom terms\n"},
                license_file="legal/TERMS.txt",
            )
            document = self.render(fixture)

        self.assertIn("License: see the license files below\n", document)
        self.assertIn("  legal/TERMS.txt: [", document)
        self.assertIn("custom terms\n", document)

    def test_verify_rejects_omissions_and_mismatched_builds(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = InventoryFixture(directory)
            fixture.crate("ahash", "0.8.12", "MIT", {"LICENSE": "MIT\n"})
            document = self.render(fixture)
            lite = self.render(fixture, no_default_features=True)
            self.assertEqual(
                1,
                INVENTORY.verify_inventory(
                    document, fixture.metadata(), target=TARGET, no_default_features=False
                ),
            )
            self.assertIn("Features: --no-default-features\n", lite)
            with self.assertRaisesRegex(INVENTORY.InventoryError, "features"):
                INVENTORY.verify_inventory(
                    lite, fixture.metadata(), target=TARGET, no_default_features=False
                )
            with self.assertRaisesRegex(INVENTORY.InventoryError, "target"):
                INVENTORY.verify_inventory(
                    document,
                    fixture.metadata(),
                    target="aarch64-apple-darwin",
                    no_default_features=False,
                )
            fixture.crate("zlib-rs", "0.5.0", "Zlib", {"LICENSE": "zlib\n"})
            with self.assertRaisesRegex(INVENTORY.InventoryError, "omits 1.*zlib-rs 0.5.0"):
                INVENTORY.verify_inventory(
                    document, fixture.metadata(), target=TARGET, no_default_features=False
                )

    def test_tampered_package_count_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = InventoryFixture(directory)
            fixture.crate("ahash", "0.8.12", "MIT", {"LICENSE": "MIT\n"})
            document = self.render(fixture)
        with self.assertRaisesRegex(INVENTORY.InventoryError, "count"):
            INVENTORY.parse_inventory(document.replace("Packages: 1", "Packages: 2"))

    def test_license_identifiers_cover_legacy_and_spdx_expressions(self) -> None:
        self.assertEqual(["MIT", "Apache-2.0"], INVENTORY.license_identifiers("MIT/Apache-2.0"))
        self.assertEqual(
            ["Apache-2.0", "LLVM-exception", "MIT"],
            INVENTORY.license_identifiers("(Apache-2.0 WITH LLVM-exception) OR MIT"),
        )

    def test_canonical_fallback_texts_are_vendored(self) -> None:
        texts = ROOT / "packaging/license-texts"
        self.assertIn("<copyright holders>", (texts / "MIT.txt").read_text(encoding="utf-8"))
        self.assertIn(
            "Apache License\n                           Version 2.0, January 2004",
            (texts / "Apache-2.0.txt").read_text(encoding="utf-8"),
        )


if __name__ == "__main__":
    unittest.main()
