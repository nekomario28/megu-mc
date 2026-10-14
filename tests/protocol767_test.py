#!/usr/bin/env python3
"""Known-answer/negative checks for the CLI decoder, not just its happy-path server."""
import socket
import struct
import unittest
from protocol767 import Client, Reader, decode_chunk, frame, palette, position, unpack_position, vint


def empty_chunk():
    name = b"MOTION_BLOCKING"
    height = b"\x0a\x0c" + struct.pack(">H", len(name)) + name + struct.pack(">i", 37) + bytes(37 * 8) + b"\0"
    sections = (bytes(2) + bytes([0, 0, 0]) + bytes([0, 0, 0])) * 24
    masks = (b"\1" + struct.pack(">Q", (1 << 26) - 1)) * 2 + bytes(2)
    light = vint(26) + (vint(2048) + bytes(2048)) * 26
    return struct.pack(">ii", -1, 2) + height + vint(len(sections)) + sections + b"\0" + masks + light * 2


class DecoderTests(unittest.TestCase):
    def test_scalar_integer_limits(self):
        self.assertEqual(Reader(vint(0xffffffff)).varint(), 0xffffffff)
        self.assertEqual(Reader(vint(0xffffffffffffffff)).varlong(), 0xffffffffffffffff)
        for raw in [b"\x80" * 5 + b"\0", b"\xff" * 5, b"\x80"]:
            with self.assertRaises(AssertionError):
                Reader(raw).varint()

    def test_signed_block_position(self):
        for pos in [(-17, -64, 63), (33554431, 319, -33554432), (0, -1, 0)]:
            self.assertEqual(unpack_position(position(*pos)), pos)

    def test_nbt_compound_known_answer(self):
        raw = bytes.fromhex("0a0300016e0000002a0800017300026f6b0b0001610000000200000001ffffffff00")
        reader = Reader(raw)
        self.assertEqual(reader.nbt(), {"n": 42, "s": "ok", "a": [1, -1]})
        self.assertEqual(reader.pos, len(raw))
        with self.assertRaises(AssertionError):
            Reader(raw[:-1]).nbt()

    def test_palette_low_bits_come_first(self):
        word = sum(i << (i * 4) for i in range(16))
        raw = bytes([4, 16]) + bytes(range(16)) + b"\1" + struct.pack(">Q", word)
        self.assertEqual(palette(Reader(raw), 16, 8, 4, 26684), list(range(16)))
        with self.assertRaises(AssertionError):
            palette(Reader(raw[:-1]), 16, 8, 4, 26684)

    def test_chunk_sections_and_light_masks(self):
        raw = empty_chunk()
        coord, sections = decode_chunk(raw, 1)
        self.assertEqual(coord, (-1, 2))
        self.assertEqual(len(sections), 24)
        self.assertTrue(all(st == 0 for section in sections for st in section))
        mask = b"\1" + struct.pack(">Q", (1 << 26) - 1)
        corrupted = raw.replace(mask, b"\1" + struct.pack(">Q", (1 << 26) - 2), 1)
        for bad in [raw[:-1], raw + b"\0", corrupted]:
            with self.assertRaises(AssertionError):
                decode_chunk(bad, 1)

    def test_section_delta_negative_section_and_local_axes(self):
        a, b = socket.socketpair()
        with a, b:
            client = Client.__new__(Client)
            client.socket, client.phase = a, "play"
            client.chunks = {(-2, 3): [[0] * 4096 for _ in range(24)]}
            client.updates = []
            section = ((-2 & 0x3fffff) << 42) | (3 << 20) | (-1 & 0xfffff)
            record = (123 << 12) | (13 << 8) | (7 << 4) | 2
            b.sendall(frame(0x49, struct.pack(">Q", section) + vint(1) + vint(record)))
            self.assertEqual(client.read(1)[0], 0x49)
            self.assertEqual(client.updates, [((-19, -14, 55), 123)])
            self.assertEqual(client.block(-19, -14, 55), 123)


if __name__ == "__main__":
    unittest.main()
