#!/usr/bin/env python3
"""Portable full-registration tests; no Minecraft JAR or rustc needed."""
from collections import Counter
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
from native_support_inventory import (
    BLOCKS, PINNED_BLOCKS, PINNED_STATES, inventory, states, values,
)


class RegistryCoverageTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.all = list(inventory(BLOCKS, "all"))
        cls.defaults = list(inventory(BLOCKS, "default"))

    def test_all_26684_states_and_1060_default_families_are_present(self):
        self.assertEqual(len(self.all), PINNED_STATES)
        self.assertEqual(len(self.defaults), PINNED_BLOCKS)
        self.assertEqual(set(sid for sid, _ in self.all), set(range(PINNED_STATES)))
        self.assertEqual(len(set(e["Name"] for _, e in self.defaults)), PINNED_BLOCKS)

    def test_per_state_properties_and_default_values(self):
        mapping = dict(self.all)
        by_name = {e["Name"]: (sid, e) for sid, e in self.defaults}
        self.assertEqual(
            mapping[by_name["minecraft:oak_fence"][0]]["Properties"]["waterlogged"],
            "false",
        )
        self.assertEqual(
            mapping[by_name["minecraft:glass_pane"][0]]["Properties"]["waterlogged"],
            "false",
        )
        self.assertIn("facing", by_name["minecraft:oak_stairs"][1]["Properties"])
        self.assertNotIn("Properties", by_name["minecraft:stone"][1])
        self.assertEqual(len([e for _, e in self.all if e["Name"] == "minecraft:fire"]), 512)

    def test_invalid_enumerations_and_ranges_fail_closed(self):
        bad = {"name": "demo", "minStateId": 1, "maxStateId": 2,
               "states": [{"name": "facing", "type": "enum", "num_values": 2,
                           "values": ["north"]}]}
        with self.assertRaises(ValueError):
            list(states(bad))
        with self.assertRaises(ValueError):
            values({"name": "unknown", "type": "unsupported", "num_values": 1})

    def test_all_scope_and_default_scope_have_no_hidden_omissions(self):
        ids = {sid for sid, _ in self.all}
        self.assertTrue(all(sid in ids for sid, _ in self.defaults))
        counts = Counter(e["Name"] for _, e in self.all)
        self.assertTrue(all(count > 0 for count in counts.values()))
        self.assertEqual(len(counts), PINNED_BLOCKS)


if __name__ == "__main__":
    unittest.main()
