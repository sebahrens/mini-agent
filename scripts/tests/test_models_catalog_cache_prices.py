import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class ModelCatalogCachePriceTests(unittest.TestCase):
    @unittest.skipUnless(os.name == "posix" and shutil.which("jq") and shutil.which("bash"),
                         "the Unix catalog-refresh workflow requires bash and jq")
    def test_generator_preserves_zero_cache_prices_and_missing_tiers(self):
        script = Path(__file__).resolve().parents[1] / "gen-models-catalog.sh"
        with tempfile.TemporaryDirectory(prefix="mini-agent-cache-catalog-") as temporary:
            root = Path(temporary)
            (root / "scripts").mkdir()
            (root / "data").mkdir()
            (root / "bin").mkdir()
            copied = root / "scripts" / script.name
            shutil.copyfile(script, copied)
            common = {"name": "fixture", "release_date": "2026-10-12",
                      "modalities": {"output": ["text"]}, "limit": {"context": 1000}}
            fixture = root / "api.json"
            fixture.write_text(json.dumps({
                "anthropic": {"models": {
                    "priced": {**common, "id": "priced", "modalities": {"input": ["text", "image"], "output": ["text"]}, "cost": {
                        "input": 4, "output": 20, "cache_read": 0, "cache_write": 5}},
                    "text-only": {**common, "id": "text-only", "modalities": {"input": ["text"], "output": ["text"]}, "cost": {"input": 1, "output": 2}},
                    "legacy": {**common, "id": "legacy", "cost": {
                        "input": 3, "output": 15}},
                }},
                "openai": {"models": {}}, "google": {"models": {}},
                "openrouter": {"models": {}},
            }))
            curl = root / "bin" / "curl"
            curl.write_text('#!/bin/sh\ncat "$MODEL_CATALOG_FIXTURE"\n')
            curl.chmod(0o700)
            environment = dict(os.environ, PATH=f"{root / 'bin'}{os.pathsep}{os.environ['PATH']}",
                               MODEL_CATALOG_FIXTURE=str(fixture))
            subprocess.run(["bash", str(copied)], env=environment, check=True,
                           stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
            generated = {entry["id"]: entry for entry in
                         json.loads((root / "data" / "models.json").read_text())["anthropic"]}
            self.assertEqual(generated["priced"]["cache_read_price"], 0)
            self.assertEqual(generated["priced"]["cache_write_price"], 5)
            self.assertIsNone(generated["legacy"]["cache_read_price"])
            self.assertIsNone(generated["legacy"]["cache_write_price"])
            self.assertEqual(generated["priced"]["input_price"], 4)
            self.assertIs(generated["priced"]["image_input"], True)
            self.assertIsNone(generated["legacy"]["image_input"])
            self.assertIs(generated["text-only"]["image_input"], False)


if __name__ == "__main__":
    unittest.main()
