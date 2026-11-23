"""Unit tests for the pure packet helpers in fishing_wire.py."""
from pathlib import Path
import struct
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests"))

import fishing_wire as fishing  # noqa: E402
from protocol767 import Reader, position, vint  # noqa: E402


class FishingWireHelperTests(unittest.TestCase):
    def test_air_click_packet_shape(self):
        body = fishing.air_click(23)
        reader = Reader(body)
        self.assertEqual(reader.varint(), 0)
        self.assertEqual(body[1:9], position(-1, 255, -1))
        reader.take(8)
        self.assertEqual(reader.varint(), 0xFFFFFFFF)
        self.assertEqual(struct.unpack(">fff", reader.take(12)), (0.5, 0.5, 0.5))
        self.assertFalse(reader.number("?"))
        self.assertEqual(reader.number("B"), 0)  # inside-block
        self.assertEqual(reader.varint(), 23)
        self.assertEqual(reader.pos, len(body))

    def test_spawn_entity_decoder(self):
        body = (vint(321) + bytes(range(16)) + vint(129)
                + struct.pack(">ddd", 1.25, 64.0, -2.5) + bytes(3)
                + vint(0) + bytes(6))
        self.assertEqual(fishing.spawned_entity(body),
                         (321, 129, 1.25, 64.0, -2.5))

    def test_set_slot_item_decoder(self):
        body = bytes([0]) + vint(4) + struct.pack(">h", 37)
        body += vint(1) + vint(936) + vint(0) + vint(0)
        self.assertEqual(fishing.slot_item(body), (37, 936))

    def test_empty_set_slot(self):
        body = bytes([0]) + vint(4) + struct.pack(">h", 37) + vint(0)
        self.assertEqual(fishing.slot_item(body), (37, None))


if __name__ == "__main__":
    unittest.main()
