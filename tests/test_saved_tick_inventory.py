import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
from saved_tick_inventory import inventory


class SavedTickInventoryTests(unittest.TestCase):
    def test_reports_raw_delays_and_type_position_duplicates(self):
        tick = {"i": "minecraft:oak_leaves", "x": -1, "y": 64, "z": 0,
                "t": -2, "p": -1}
        chunk = {"Status": "minecraft:full", "xPos": -1, "zPos": 0,
                 "block_ticks": [tick, tick.copy()], "fluid_ticks": []}
        with tempfile.TemporaryDirectory() as tmp:
            world = Path(tmp)
            (world / "region").mkdir()
            (world / "region" / "r.-1.0.mca").write_bytes(b"fixture")
            with patch("saved_tick_inventory.chunk_bytes", return_value=b""), \
                 patch("saved_tick_inventory.parse_nbt", return_value=(chunk, 0)):
                report = inventory(world, -1, 0, 1, 1, include_entries=True)
        self.assertEqual(report["stored_delays"]["block_ticks"], {"-2": 2})
        self.assertEqual(report["duplicate_type_and_position_entries"], 1)
        self.assertEqual(report["tick_types"]["block_ticks"], {"minecraft:oak_leaves": 2})
        self.assertEqual([row["entry"] for row in report["entries"]], [tick, tick])

    def test_refuses_incomplete_or_misplaced_ticks(self):
        chunk = {"Status": "minecraft:full", "xPos": 0, "zPos": 0,
                 "block_ticks": [{"i": "minecraft:sand", "x": 16, "y": 64,
                                  "z": 0, "t": 1}], "fluid_ticks": []}
        with patch("saved_tick_inventory.chunk_bytes", return_value=b""), \
             patch("saved_tick_inventory.parse_nbt", return_value=(chunk, 0)):
            with self.assertRaisesRegex(ValueError, "wrong chunk"):
                inventory(Path("unused"), 0, 0, 1, 1)
            del chunk["fluid_ticks"]
            chunk["block_ticks"] = []
            with self.assertRaisesRegex(ValueError, "missing fluid_ticks"):
                inventory(Path("unused"), 0, 0, 1, 1)
            chunk["Status"] = "minecraft:features"
            with self.assertRaisesRegex(ValueError, "not FULL"):
                inventory(Path("unused"), 0, 0, 1, 1)


if __name__ == "__main__":
    unittest.main()
