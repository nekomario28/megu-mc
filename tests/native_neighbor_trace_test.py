#!/usr/bin/env python3
"""Fail-closed native request-trace syntax, order and timing checks."""
from pathlib import Path
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
from native_neighbor_trace import SCENES, validate_trace, validate_delivery


class NativeRequestTraceGuardTest(unittest.TestCase):
    def sample(self):
        events = {
            "waterlogged_pane": ["tick fluid minecraft:water 0,64,0 105 NORMAL 0"],
            "waterlogged_fence": ["tick fluid minecraft:water 0,64,0 105 NORMAL 0"],
            "waterlogged_stairs": ["tick fluid minecraft:water 0,64,0 105 NORMAL 0"],
            "leaf_next_to_log": ["tick block minecraft:oak_leaves 0,64,0 101 NORMAL 0"],
            "falling_sand": ["tick block minecraft:sand 0,64,0 102 NORMAL 0"],
            "falling_gravel": ["tick block minecraft:gravel 0,64,0 102 NORMAL 0"],
            "falling_red_sand": ["tick block minecraft:red_sand 0,64,0 102 NORMAL 0"],
            "falling_concrete_powder": ["tick block minecraft:white_concrete_powder 0,64,0 102 NORMAL 0"],
            "falling_sand_supported": ["tick block minecraft:sand 0,64,0 102 NORMAL 0"],
            "fragile_torch": ["shape fragile_torch minecraft:air"],
            "mixed_scheduling_requests": [
                "tick fluid minecraft:water 0,64,0 105 NORMAL 0",
                "tick block minecraft:oak_leaves 0,64,0 101 NORMAL 1",
                "tick block minecraft:sand 0,64,0 102 NORMAL 2",
            ],
            "native_neighbor_pass": [
                "set 1,64,0 minecraft:oak_fence[north=false]",
            ],
        }
        return [
            f"{scene}\t{i}\t{value}\n"
            for scene in SCENES for i, value in enumerate(events[scene])
        ]

    def test_complete_trace(self):
        got = validate_trace(self.sample())
        self.assertEqual(got["scenes"], len(SCENES))

    def test_missing_and_wrong_order_are_rejected(self):
        base = self.sample()
        for rows in (base[:-1], base[::-1]):
            with self.assertRaises(ValueError):
                validate_trace(rows)

    def test_timing_and_suborder_fail_closed(self):
        base = self.sample()
        bad_time = [s.replace(" 105 NORMAL", " 104 NORMAL") if
                    s.startswith("waterlogged_pane") else s for s in base]
        with self.assertRaises(ValueError):
            validate_trace(bad_time)
        bad_order = list(base)
        i = next(i for i, line in enumerate(base)
                 if line.startswith("mixed_scheduling_requests\t1\t"))
        bad_order[i] = bad_order[i].replace(" NORMAL 1", " NORMAL 0")
        with self.assertRaises(ValueError):
            validate_trace(bad_order)

    def test_falling_two_tick_bookings_fail_closed(self):
        base = self.sample()
        for case in ("falling_sand", "falling_gravel", "falling_red_sand",
                     "falling_concrete_powder", "falling_sand_supported"):
            selected = next(i for i, row in enumerate(base)
                            if row.startswith(case + "\\t"))
            for corrupted in (
                base[selected].replace(" 102 NORMAL", " 101 NORMAL"),
                base[selected].replace(" 102 NORMAL", " 103 NORMAL"),
                base[selected].replace(" NORMAL", " HIGH"),
                base[selected].replace(" NORMAL 0", " NORMAL 1"),
            ):
                rows = list(base)
                rows[selected] = corrupted
                with self.subTest(case=case, corrupted=corrupted), self.assertRaises(ValueError):
                    validate_trace(rows)

    def test_native_level_ticks_delivery_matches_requests(self):
        booked = [
            "mixed_scheduling_requests\t0\ttick fluid minecraft:water 0,64,0 105 NORMAL 0\n",
            "mixed_scheduling_requests\t1\ttick block minecraft:oak_leaves 0,64,0 101 NORMAL 1\n",
            "mixed_scheduling_requests\t2\ttick block minecraft:sand 1,64,0 102 NORMAL 2\n",
        ]
        delivered = [
            "mixed_scheduling_requests\tblock\t101\tminecraft:oak_leaves\t0,64,0\n",
            "mixed_scheduling_requests\tblock\t102\tminecraft:sand\t1,64,0\n",
            "mixed_scheduling_requests\tfluid\t105\tminecraft:water\t0,64,0\n",
        ]
        self.assertEqual(validate_delivery(booked, delivered), 3)
        with self.assertRaises(ValueError):
            validate_delivery(booked, delivered[:-1])
        with self.assertRaises(ValueError):
            validate_delivery(booked, delivered + delivered[-1:])
        with self.assertRaises(ValueError):
            validate_delivery(booked, [v.replace("\t105\t", "\t104\t")
                                       for v in delivered])

    def test_native_scene_list_matches_java(self):
        import re
        source = (ROOT / "tools/structure-oracle/NativeNeighborTraceOracle.java").read_text()
        cases = tuple(re.findall(r'new Scene\("([^"]+)"', source))
        self.assertEqual(cases + ("mixed_scheduling_requests", "native_neighbor_pass"), SCENES)


if __name__ == "__main__":
    unittest.main()
