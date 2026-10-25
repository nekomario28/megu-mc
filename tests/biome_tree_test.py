#!/usr/bin/env python3
"""The exported climate tree must fail closed before it replaces the working file."""
import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
from biome_tree_from_jar import export, validate_tree  # noqa: E402


def leaf():
    return {"_type": "leaf", "biome": "minecraft:plains",
            "parameters": [{"min": -10000, "max": 10000} for _ in range(7)]}


def tree():
    return {preset: {"_type": "branch", "parameters": leaf()["parameters"],
                     "subTree": [leaf() for _ in range(count)]}
            for preset, count in [("overworld", 100), ("nether", 5)]}


class BiomeTreeTests(unittest.TestCase):
    def test_complete_quantized_tree(self):
        self.assertEqual(validate_tree(tree(), {"minecraft:plains"}), {"overworld": 100, "nether": 5})

    def test_foreign_biome_is_rejected(self):
        data = tree()
        data["overworld"]["subTree"][0]["biome"] = "minecraft:dappled_forest"
        with self.assertRaisesRegex(ValueError, "biome absent from pinned jar"):
            validate_tree(data, {"minecraft:plains"})

    def test_incomplete_or_malformed_export_is_rejected(self):
        base = tree()
        variants = [{}, {"overworld": base["overworld"]}]
        for change in ["empty_branch", "axis_count", "reversed", "noninteger", "unknown_kind", "short_export"]:
            data = copy.deepcopy(base)
            node = data["overworld"]["subTree"][0]
            if change == "empty_branch":
                data["overworld"]["subTree"] = []
            elif change == "axis_count":
                node["parameters"].pop()
            elif change == "reversed":
                node["parameters"][0] = {"min": 1, "max": -1}
            elif change == "noninteger":
                node["parameters"][0]["min"] = -1.5
            elif change == "unknown_kind":
                node["_type"] = "new_node"
            else:
                data["overworld"]["subTree"].pop()
            variants.append(data)
        for data in variants:
            with self.subTest(data=str(data)[:80]), self.assertRaises(ValueError):
                validate_tree(data, {"minecraft:plains"})

    def test_wrong_jar_never_replaces_destination(self):
        with tempfile.TemporaryDirectory() as tmp:
            folder = Path(tmp)
            jar, destination = folder / "server.jar", folder / "tree.json"
            jar.write_bytes(b"wrong version")
            destination.write_text("old tree\n")
            with self.assertRaisesRegex(ValueError, "not the pinned"):
                export(jar, destination)
            # Also test the actual CLI's status and preservation, not just the validator.
            result = subprocess.run([sys.executable, str(ROOT / "tools/biome_tree_from_jar.py"),
                                     "--jar", str(jar), "--output", str(destination)],
                                    capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("not the pinned", result.stderr)
            self.assertEqual(destination.read_text(), "old tree\n")

    def test_installed_tree_has_pinned_biomes_and_provenance(self):
        tree_path = ROOT / "datapacks/vanilla/multi_noise_biome_tree.json"
        data = json.loads(tree_path.read_text())
        receipt = data.pop("_provenance")
        from biome_tree_from_jar import JAR_SHA1, SOURCE
        import hashlib
        self.assertEqual(receipt["server_sha1"], JAR_SHA1)
        self.assertEqual(receipt["minecraft"], "1.21.1")
        self.assertEqual(receipt["exporter_sha256"], hashlib.sha256(SOURCE.read_bytes()).hexdigest())
        biomes = {"minecraft:" + p.stem for p in
                  (ROOT / "datapacks/vanilla/data/minecraft/worldgen/biome").glob("*.json")}
        validate_tree(data, biomes)


if __name__ == "__main__":
    unittest.main()
