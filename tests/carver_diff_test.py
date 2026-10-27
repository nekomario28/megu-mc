#!/usr/bin/env python3
"""A property difference must not become a false-green world comparison."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import struct
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("carver_diff", ROOT / "tools/carver_diff.py")
diff = importlib.util.module_from_spec(spec)
spec.loader.exec_module(diff)


class StateComparison(unittest.TestCase):
    def compare(self, got, want, *flags):
        with tempfile.TemporaryDirectory() as tmp:
            dump = Path(tmp) / "dump.bin"
            dump.write_bytes(struct.pack("<I", got) + bytes((diff.CELLS - 1) * 4))
            reference = [want] + [0] * (diff.CELLS - 1)
            output = io.StringIO()
            with patch.object(sys, "argv", ["carver_diff.py", str(dump), tmp, "--n", "1", *flags]), \
                    patch.object(diff, "vanilla_chunk", return_value=reference), \
                    contextlib.redirect_stdout(output):
                status = diff.main()
        return status, output.getvalue()

    def test_leaf_distance_cannot_be_assumed_to_be_a_runtime_artifact(self):
        leaf = next(b for b in json.loads((ROOT / "assets/blocks_1211.json").read_text())
                    if b["name"] == "oak_leaves")
        # distance is the outer property; persistent and waterlogged each have two values.
        a = leaf["minStateId"]
        for flags in [(), ("--tick",)]:
            with self.subTest(flags=flags):
                status, output = self.compare(a, a + 4, *flags)
                self.assertEqual(status, 1, output)
                self.assertIn("differing cells: 1", output)
                self.assertNotIn("excluded", output)

    def test_a_matching_subset_does_not_prove_the_requested_region(self):
        with tempfile.TemporaryDirectory() as tmp:
            dump = Path(tmp) / "dump.bin"
            dump.write_bytes(bytes(4 * diff.CELLS * 4))
            output = io.StringIO()
            with patch.object(sys, "argv", ["carver_diff.py", str(dump), tmp, "--n", "2"]), \
                    patch.object(diff, "vanilla_chunk", side_effect=[[0]*diff.CELLS, None, None, None]), \
                    contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
                status = diff.main()
            self.assertEqual(status, 2, output.getvalue())
            self.assertIn("1/4 full chunks", output.getvalue())
            self.assertIn("(1, 0)", output.getvalue())

    def test_exact_states_are_green(self):
        status, output = self.compare(0, 0)
        self.assertEqual(status, 0, output)
        self.assertIn("differing cells: 0", output)


if __name__ == "__main__":
    unittest.main()
