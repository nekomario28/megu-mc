#!/usr/bin/env python3
"""The stage classifier must read the actual carve-agent coordinate grammar."""
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
from forest_stage_check import runtime_writers


class WriterTrace(unittest.TestCase):
    def test_native_space_separated_coordinates_and_last_successful_write(self):
        # Captured native FlowingFluid.tick stack; false writes are observations,
        # not state transitions and must not replace the last successful writer.
        stack = "dcw.a(SourceFile:267) | epc.a(SourceFile:136) | epc.b(SourceFile:464)"
        prefix = "[03:50:46] [Server thread/INFO]: [STDOUT]: "
        lines = [prefix + "RUNTIME -1020 -20 517 state=104 wrote=true stack=" + stack,
                 prefix + "RUNTIME -1020 -20 517 state=0 wrote=false stack=other"]
        self.assertEqual(runtime_writers(lines), {(-1020,-20,517): (104,stack)})


if __name__ == "__main__":
    unittest.main()
