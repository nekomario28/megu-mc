#!/usr/bin/env python3
"""The no-feature pack has to declare the data-pack format, not the resource one."""
import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
import nofeat_pack  # noqa: E402


class NoFeaturePackTests(unittest.TestCase):
    def test_declares_the_1211_data_pack_format(self):
        self.assertEqual(nofeat_pack.DATA_PACK_FORMAT_1211, 48)

    def test_writes_a_pack_that_empties_features_and_carvers(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            src = root / "pack"
            biome = src / "data/minecraft/worldgen/biome"
            biome.mkdir(parents=True)
            (biome / "plains.json").write_text(
                json.dumps(
                    {
                        "features": [["minecraft:oak"], [], ["minecraft:coal_ore"]],
                        "carvers": {"air": ["minecraft:cave"]},
                        "temperature": 0.8,
                    }
                )
            )
            out = root / "nofeat"
            sys.argv = ["nofeat_pack.py", str(src), str(out)]
            self.assertEqual(nofeat_pack.main(), 0)
            meta = json.loads((out / "pack.mcmeta").read_text())
            self.assertEqual(meta["pack"]["pack_format"], 48)
            stripped = json.loads((out / "data/minecraft/worldgen/biome/plains.json").read_text())
            self.assertEqual(stripped["features"], [[] for _ in range(nofeat_pack.STEPS)])
            self.assertEqual(stripped["carvers"], {})
            self.assertEqual(stripped["temperature"], 0.8)


if __name__ == "__main__":
    unittest.main()
