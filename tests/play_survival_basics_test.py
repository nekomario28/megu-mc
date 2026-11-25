#!/usr/bin/env python3
"""Unit tests for tools/play_survival_basics.py pure helpers (no server)."""
from pathlib import Path
import struct
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
sys.path.insert(0, str(ROOT / "tests"))

import play_survival_basics as psb  # noqa: E402
from protocol767 import Reader, position, vint  # noqa: E402


class FakeClient:
    def __init__(self):
        self.sent = []
        self.messages = []
        self.entity_id = 77

    def send(self, pid, body=b""):
        self.sent.append((pid, body))

    def until(self, pred, label=None):
        raise AssertionError("until() should not run in unit tests")


class PacketBuilderTests(unittest.TestCase):
    def test_sneak_on_and_off(self):
        c = FakeClient()
        psb.sneak(c, 77, True)
        psb.sneak(c, 77, False)
        (pid_on, body_on), (pid_off, body_off) = c.sent
        self.assertEqual(pid_on, 0x25)
        r = Reader(body_on)
        self.assertEqual(r.varint(), 77)
        self.assertEqual(r.varint(), 3)
        self.assertEqual(r.varint(), 0)
        r2 = Reader(body_off)
        r2.varint()
        self.assertEqual(r2.varint(), 4)

    def test_dig_packet_shape(self):
        c = FakeClient()
        psb.dig(c, (10, 64, -3), face=2, sequence=99)
        pid, body = c.sent[0]
        self.assertEqual(pid, 0x24)
        r = Reader(body)
        self.assertEqual(r.varint(), 0)  # start-digging status
        self.assertEqual(body[1:9], position(10, 64, -3))
        self.assertEqual(body[9], 2)  # face byte
        self.assertEqual(body[10:], vint(99))

    def test_place_packet_shape(self):
        c = FakeClient()
        psb.place_block(c, (1, 70, 2), sequence=5)
        pid, body = c.sent[0]
        self.assertEqual(pid, 0x38)
        r = Reader(body)
        self.assertEqual(r.varint(), 0)  # main hand
        self.assertEqual(body[1:9], position(1, 70, 2))
        self.assertEqual(body[9], 1)  # top face
        cx, cy, cz, sneak = struct.unpack(">fff?", body[10:23])
        self.assertEqual((cx, cy, cz, sneak), (0.5, 1.0, 0.5, False))
        self.assertEqual(body[23:], vint(5))

    def test_plain_stack(self):
        self.assertEqual(psb.plain_stack(1, 1), b"\x01\x01\x00\x00")
        self.assertEqual(psb.plain_stack(0, 0), b"\x00")

    @unittest.skipUnless(
        (ROOT / "assets/blocks_1211.json").is_file(),
        "requires local assets/blocks_1211.json",
    )
    def test_breakable_states_nonempty(self):
        states = psb.breakable_states()
        self.assertTrue(states)
        self.assertIn(1, states)  # stone state 1 is diggable


if __name__ == "__main__":
    unittest.main()
