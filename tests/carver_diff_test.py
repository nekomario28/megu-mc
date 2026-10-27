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

    def test_end_height_compares_the_top_section_without_overworld_padding(self):
        count = 128 * 256
        with tempfile.TemporaryDirectory() as tmp:
            dump = Path(tmp) / 'end.bin'
            dump.write_bytes(bytes((count - 1) * 4) + struct.pack('<I', 1))
            output = io.StringIO()
            with patch.object(sys, 'argv', ['carver_diff.py', str(dump), tmp, '--n', '1', '--min-y', '0', '--height', '128', '--cells', '1']), patch.object(diff, 'vanilla_chunk', return_value=[0] * count) as reader, contextlib.redirect_stdout(output):
                self.assertEqual(diff.main(), 1)
            reader.assert_called_once()
            self.assertEqual(reader.call_args.kwargs, {'min_y': 0, 'height': 128})
            self.assertIn('band y=0..127', output.getvalue())
            self.assertIn('cell 15,127,15', output.getvalue())

    def test_native_end_sections_use_their_own_height(self):
        chunk = {'Status': 'minecraft:full', 'sections': [
            {'Y': y, 'block_states': {'palette': [{'Name': 'minecraft:end_stone'}]}}
            for y in range(8)]}
        with patch.object(diff, 'chunk_bytes', return_value=b''), patch.object(diff, 'parse_nbt', return_value=(chunk, 0)):
            actual = diff.vanilla_chunk('unused', 0, 0, {('end_stone', ()): 12}, min_y=0, height=128)
        self.assertEqual(actual, [12] * (128 * 256))

    def test_extra_dump_bytes_cannot_be_ignored(self):
        with tempfile.TemporaryDirectory() as tmp:
            dump = Path(tmp) / 'dump.bin'
            dump.write_bytes(bytes(diff.CELLS * 4 + 1))
            with patch.object(sys, 'argv', ['carver_diff.py', str(dump), tmp, '--n', '1']):
                with self.assertRaisesRegex(SystemExit, 'dump length'):
                    diff.main()


if __name__ == "__main__":
    unittest.main()
