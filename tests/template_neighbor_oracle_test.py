#!/usr/bin/env python3
"""Portable guard for the native neighbor-oracle output contract (no JAR needed)."""
import sys
from pathlib import Path
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
from template_neighbor_oracle import CASES, validate_rows


class NativeNeighborOutputTest(unittest.TestCase):
    def sample(self):
        rows = []
        for case in CASES:
            before = "minecraft:oak_fence[west=false]"
            after = before if case in {"oak_west_air", "pane_west_air", "bone_west_bone", "nether_no_wood_join"} else "minecraft:oak_fence[west=true]"
            rows.append(f"{case}\t{before}\t{after}\tminecraft:bone_block\tWEST")
        return rows

    def test_complete_native_output_is_accepted(self):
        validate_rows("\n".join(self.sample()) + "\n")

    def test_missing_case_is_rejected(self):
        with self.assertRaises(ValueError):
            validate_rows("\n".join(self.sample()[:-1]))

    def test_reordered_case_is_rejected(self):
        rows = self.sample()
        rows[0], rows[1] = rows[1], rows[0]
        with self.assertRaises(ValueError):
            validate_rows("\n".join(rows))

    def test_native_positive_control_must_change(self):
        rows = self.sample()
        parts = rows[0].split("\t")
        rows[0] = "\t".join((parts[0], parts[1], parts[1]))
        with self.assertRaises(ValueError):
            validate_rows("\n".join(rows))

    def test_java_probe_and_python_case_ids_match(self):
        import re
        source = (Path(__file__).resolve().parents[1]
                  / "tools/structure-oracle/NeighborShapeOracle.java").read_text()
        ids = re.findall(r'new Case\("([^"]+)"', source)
        self.assertEqual(tuple(ids), CASES)

    def test_nether_fence_must_not_join_wood(self):
        rows = self.sample()
        i = CASES.index("nether_no_wood_join")
        parts = rows[i].split("\t")
        rows[i] = "\t".join((parts[0], parts[1], "minecraft:nether_brick_fence[east=true]", *parts[3:]))
        with self.assertRaises(ValueError):
            validate_rows("\n".join(rows))

    def test_native_negative_control_must_not_change(self):
        rows = self.sample()
        # Only the native result is corrupted; the input state stays intact.
        parts = rows[-1].split("\t")
        rows[-1] = "\t".join((parts[0], parts[1], "minecraft:oak_fence[west=true]", *parts[3:]))
        with self.assertRaises(ValueError):
            validate_rows("\n".join(rows))


if __name__ == "__main__":
    unittest.main()
