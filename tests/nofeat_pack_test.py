#!/usr/bin/env python3
"""Portable checks for the pinned 1.21.1 terrain-only capture pack."""
from pathlib import Path
import json
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
TOOL = ROOT / "tools/nofeat_pack.py"


class NoFeaturePackTests(unittest.TestCase):
    def test_generated_pack_has_correct_native_version_and_no_features(self):
        with tempfile.TemporaryDirectory() as tmp:
            src = Path(tmp) / "source"
            dst = Path(tmp) / "terrain-only"
            path = src / "data/minecraft/worldgen/biome/plains.json"
            path.parent.mkdir(parents=True)
            original = {
                "temperature": 0.8,
                "precipitation": True,
                "features": [["minecraft:ore"], [], ["minecraft:tree"]],
                "carvers": {"air": ["minecraft:cave"], "liquid": []},
            }
            path.write_text(json.dumps(original), encoding="utf-8")
            run = subprocess.run(
                [sys.executable, str(TOOL), str(src), str(dst)],
                text=True, capture_output=True, check=False,
            )
            self.assertEqual(run.returncode, 0, run.stderr)
            self.assertIn("dropped 2 placed-feature references", run.stdout)
            self.assertIn("dropped 1 carver entries", run.stdout)
            pack = json.loads((dst / "pack.mcmeta").read_text(encoding="utf-8"))
            # 34 belongs to the 1.21.1 RESOURCE pack, not the DATA pack.
            self.assertEqual(pack["pack"]["pack_format"], 48)
            parsed = json.loads(
                (dst / "data/minecraft/worldgen/biome/plains.json").read_text(encoding="utf-8")
            )
            self.assertEqual(parsed["features"], [[] for _ in range(11)])
            self.assertEqual(parsed["carvers"], {})
            self.assertEqual(parsed["temperature"], original["temperature"])
            self.assertEqual(parsed["precipitation"], original["precipitation"])
            self.assertEqual(json.loads(path.read_text(encoding="utf-8")), original)

    def test_missing_source_biomes_is_a_failure(self):
        with tempfile.TemporaryDirectory() as tmp:
            src, dst = Path(tmp) / "empty", Path(tmp) / "out"
            src.mkdir()
            run = subprocess.run(
                [sys.executable, str(TOOL), str(src), str(dst)],
                text=True, capture_output=True, check=False,
            )
            self.assertEqual(run.returncode, 2)
            self.assertIn("no biome json", run.stderr)


if __name__ == "__main__":
    unittest.main()
