#!/usr/bin/env python3
"""Region NBT floats must retain IEEE-754 semantics for entity positions."""
import math
from pathlib import Path
import struct
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'tools'))
from vanilla_region_blocks import parse_payload


class NbtRegionTests(unittest.TestCase):
    def test_float_and_double_preserve_numeric_values_and_negative_zero(self):
        for kind, fmt in [(5, '>f'), (6, '>d')]:
            for value in [0.0, -0.0, 1.5, -2.25, float('inf'), float('-inf'), float('nan')]:
                with self.subTest(kind=kind, value=value):
                    payload = struct.pack(fmt, value)
                    decoded, end = parse_payload(payload, 0, kind)
                    self.assertEqual(end, len(payload))
                    if math.isnan(value):
                        self.assertTrue(math.isnan(decoded))
                    else:
                        self.assertEqual(decoded, value)
                        self.assertEqual(math.copysign(1.0, decoded), math.copysign(1.0, value))


if __name__ == '__main__':
    unittest.main()
