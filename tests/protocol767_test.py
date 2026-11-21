#!/usr/bin/env python3
"""Known-answer/negative checks for the CLI decoder, not just its happy-path server."""
import socket
import struct
import threading
import unittest
import zlib
from protocol767 import (Client, ROOT, Reader, decode_chunk, frame, frame_packet,
                         palette, position, unframe_packet, unpack_position, vint)
from wire_smoke import recv_exact, recv_vint


def nbt_compound(entries):
    out = bytearray([10])
    for key, (tag, value) in entries.items():
        name = key.encode("utf-8")
        out.append(tag)
        out.extend(struct.pack(">H", len(name)))
        out.extend(name)
        if tag == 1:
            out.extend(struct.pack("b", value))
        elif tag == 3:
            out.extend(struct.pack(">i", value))
        else:
            raise ValueError(f"unsupported test NBT tag {tag}")
    out.append(0)
    return bytes(out)


def encode_string(value):
    raw = value.encode("utf-8")
    return vint(len(raw)) + raw


def dimension_registry_packet(entries):
    out = bytearray(encode_string("minecraft:dimension_type"))
    out.extend(vint(len(entries)))
    for name, min_y, height, has_skylight in entries:
        out.extend(encode_string(name))
        out.append(1)
        out.extend(nbt_compound({
            "min_y": (3, min_y),
            "height": (3, height),
            "has_skylight": (1, 1 if has_skylight else 0),
        }))
    return bytes(out)


def spawn_info_bytes(dim_type_id, dim_name, death=None):
    out = bytearray()
    out.extend(vint(dim_type_id))
    out.extend(encode_string(dim_name))
    out.extend(struct.pack(">q", 0))
    out.extend(bytes([1, 255 & 0xff, 0, 1]))
    if death is None:
        out.append(0)
    else:
        death_dim, death_pos = death
        out.append(1)
        out.extend(encode_string(death_dim))
        out.extend(position(*death_pos))
    out.extend(vint(0))
    return bytes(out)


def join_game_packet(dim_type_id, dim_name, dim_names=None):
    if dim_names is None:
        dim_names = [dim_name]
    out = bytearray()
    out.extend(struct.pack(">i", 1))
    out.append(0)
    out.extend(vint(len(dim_names)))
    for name in dim_names:
        out.extend(encode_string(name))
    out.extend(vint(20))
    out.extend(vint(8))
    out.extend(vint(8))
    out.extend(bytes([0, 1, 0]))
    out.extend(spawn_info_bytes(dim_type_id, dim_name))
    out.append(0)
    return bytes(out)


def respawn_packet(dim_type_id, dim_name, death=None):
    return spawn_info_bytes(dim_type_id, dim_name, death=death) + b"\0"


def empty_chunk(section_count=24, coord=(-1, 2), has_skylight=True, blocks=None):
    name = b"MOTION_BLOCKING"
    height = b"\x0a\x0c" + struct.pack(">H", len(name)) + name + struct.pack(">i", 37) + bytes(37 * 8) + b"\0"
    blocks = blocks or {}
    sections = bytearray()
    for sec_idx in range(section_count):
        sec_blocks = {k: v for k, v in blocks.items() if k[0] == sec_idx}
        if not sec_blocks:
            sections.extend(bytes(2) + bytes([0, 0, 0]) + bytes([0, 0, 0]))
        else:
            grid = [0] * 4096
            for (_s, lx, ly, lz), state in sec_blocks.items():
                grid[(ly << 8) | (lz << 4) | lx] = state
            non_air = sum(1 for st in grid if st != 0)
            sections.extend(struct.pack(">h", non_air))
            palette_vals = sorted(set(grid))
            bits = max(4, (len(palette_vals) - 1).bit_length())
            sections.append(bits)
            sections.extend(vint(len(palette_vals)))
            for val in palette_vals:
                sections.extend(vint(val))
            val_to_idx = {val: idx for idx, val in enumerate(palette_vals)}
            per_word = 64 // bits
            word_count = (4096 + per_word - 1) // per_word
            sections.extend(vint(word_count))
            for w in range(word_count):
                word = 0
                for j in range(per_word):
                    cell = w * per_word + j
                    if cell < 4096:
                        word |= val_to_idx[grid[cell]] << (j * bits)
                sections.extend(struct.pack(">Q", word))
            sections.extend(bytes([0, 0, 0]))  # singleton biome 0
    light_sections = section_count + 2
    full_mask = (1 << light_sections) - 1
    sky_mask = (b"\1" + struct.pack(">Q", full_mask)) if has_skylight else b"\0"
    block_mask = b"\1" + struct.pack(">Q", full_mask)
    masks = sky_mask + block_mask + b"\0\0"
    sky_arrays = (vint(light_sections) + (vint(2048) + bytes(2048)) * light_sections) if has_skylight else b"\0"
    block_arrays = vint(light_sections) + (vint(2048) + bytes(2048)) * light_sections
    return (struct.pack(">ii", *coord) + height + vint(len(sections)) + bytes(sections) +
            b"\0" + masks + sky_arrays + block_arrays)


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

    def test_light_update_known_answer_and_bad_mask_count(self):
        a, b = socket.socketpair()
        with a, b:
            client = Client.__new__(Client)
            client.socket, client.phase = a, "play"
            client.light_updates = []
            mask = b"\1" + struct.pack(">Q", (1 << 26) - 1)
            zero = b"\0"
            arrays = vint(26) + (vint(2048) + bytes(2048)) * 26
            # sky mask 0, block mask all-26, empty sky 0, empty block 0, no sky arrays, block arrays
            body = vint(9) + vint((-4) & 0xffffffff) + zero + mask + zero + zero + zero + arrays
            b.sendall(frame(0x2a, body))
            self.assertEqual(client.read(1)[0], 0x2a)
            self.assertEqual(client.light_updates, [(9, -4)])
            # The block mask has 26 bits set, so a count of 25 must be rejected.
            bad = vint(0) + vint(0) + zero + mask + zero + zero + zero + vint(25) + (vint(2048) + bytes(2048)) * 25
            b.sendall(frame(0x2a, bad))
            with self.assertRaises(AssertionError):
                client.read(1)

    def _make_client(self, sock):
        client = Client.__new__(Client)
        client.socket = sock
        client.phase = "configuration"
        client.chunks = {}
        client.raw_chunk = {}
        client.chunk_light = {}
        client.updates = []
        client.light_updates = []
        client.registry_counts = {"minecraft:worldgen/biome": 1}
        client.dimension_types_by_id = {}
        client.dimension_types_by_name = {}
        return client

    def _feed_dimension_session(self, target_id, target_name, coord, blocks):
        entries = [
            ("minecraft:overworld", -64, 384, True),
            ("minecraft:overworld_caves", -64, 384, True),
            ("minecraft:the_end", 0, 256, False),
            ("minecraft:the_nether", 0, 256, False),
        ]
        a, b = socket.socketpair()
        with a, b:
            client = self._make_client(a)
            b.sendall(frame(7, dimension_registry_packet(entries)))
            self.assertEqual(client.read(1)[0], 7)
            client.phase = "play"
            b.sendall(frame(0x2b, join_game_packet(
                target_id, target_name, [e[0] for e in entries])))
            self.assertEqual(client.read(1)[0], 0x2b)
            self.assertEqual(client.min_y, 0)
            self.assertEqual(client.height, 256)
            self.assertEqual(client.max_y, 256)
            self.assertEqual(client.section_count, 16)
            self.assertEqual(client.light_section_count, 18)
            self.assertIs(client.has_skylight, False)

            chunk_pkt = empty_chunk(section_count=16, coord=coord, has_skylight=False, blocks=blocks)
            b.sendall(frame(0x27, chunk_pkt))
            self.assertEqual(client.read(1)[0], 0x27)
            self.assertEqual(len(client.chunks[coord]), 16)

            mask18 = b"\1" + struct.pack(">Q", (1 << 18) - 1)
            light18 = vint(18) + (vint(2048) + bytes(2048)) * 18
            b.sendall(frame(0x2a, vint(coord[0] & 0xffffffff) + vint(coord[1] & 0xffffffff) + b"\0" + mask18 + b"\0\0\0" + light18))
            self.assertEqual(client.read(1)[0], 0x2a)
            self.assertEqual(client.light_updates, [coord])
            return client

    def test_nether_dimension_wire_decoding_and_bounds(self):
        client = self._feed_dimension_session(
            3, "minecraft:the_nether", (2, -3),
            {(0, 4, 0, 5): 79, (15, 7, 15, 9): 123},
        )
        self.assertEqual(client.block(2 * 16 + 4, 0, -3 * 16 + 5), 79)
        self.assertEqual(client.block(2 * 16 + 7, 255, -3 * 16 + 9), 123)
        self.assertEqual(client.block(2 * 16, 128, -3 * 16), 0)
        for bad_y in (-1, -64, 256, 319):
            with self.assertRaises(AssertionError):
                client.block(2 * 16 + 4, bad_y, -3 * 16 + 5)
            with self.assertRaises(AssertionError):
                client._block_update(2 * 16 + 4, bad_y, -3 * 16 + 5, 1)

        # Reject 26-section or sky-lit light updates in a 16-section non-skylight dimension.
        a, b = socket.socketpair()
        with a, b:
            client.socket = a
            mask26 = b"\1" + struct.pack(">Q", (1 << 26) - 1)
            light26 = vint(26) + (vint(2048) + bytes(2048)) * 26
            b.sendall(frame(0x2a, vint(0) + vint(0) + b"\0" + mask26 + b"\0\0\0" + light26))
            with self.assertRaises(AssertionError):
                client.read(1)

            mask1 = b"\1" + struct.pack(">Q", 1)
            light1 = vint(1) + vint(2048) + bytes(2048)
            b.sendall(frame(0x2a, vint(0) + vint(0) + mask1 + b"\0\0\0" + light1 + b"\0"))
            with self.assertRaises(AssertionError):
                client.read(1)

            # Reject a 24-section Overworld chunk or a sky-lit 16-section chunk in Nether.
            b.sendall(frame(0x27, empty_chunk(section_count=24, coord=(5, 5), has_skylight=False)))
            with self.assertRaises(AssertionError):
                client.read(1)

            b.sendall(frame(0x27, empty_chunk(section_count=16, coord=(6, 6), has_skylight=True)))
            with self.assertRaises(AssertionError):
                client.read(1)

    def test_end_dimension_wire_decoding(self):
        client = self._feed_dimension_session(
            2, "minecraft:the_end", (-1, 1),
            {(0, 1, 0, 2): 500, (15, 2, 15, 3): 501},
        )
        self.assertEqual(client.block(-16 + 1, 0, 16 + 2), 500)
        self.assertEqual(client.block(-16 + 2, 255, 16 + 3), 501)

    def test_respawn_switches_dimension_and_clears_old_chunks(self):
        entries = [
            ("minecraft:overworld", -64, 384, True),
            ("minecraft:the_nether", 0, 256, False),
        ]
        a, b = socket.socketpair()
        with a, b:
            client = self._make_client(a)
            b.sendall(frame(7, dimension_registry_packet(entries)))
            self.assertEqual(client.read(1)[0], 7)
            client.phase = "play"
            b.sendall(frame(0x2b, join_game_packet(0, "minecraft:overworld")))
            self.assertEqual(client.read(1)[0], 0x2b)
            self.assertEqual((client.min_y, client.height, client.has_skylight), (-64, 384, True))

            b.sendall(frame(0x27, empty_chunk(section_count=24, coord=(0, 0), has_skylight=True)))
            self.assertEqual(client.read(1)[0], 0x27)
            self.assertEqual(len(client.chunks[(0, 0)]), 24)
            self.assertEqual(client.block(0, -64, 0), 0)

            b.sendall(frame(0x47, respawn_packet(
                1, "minecraft:the_nether", death=("minecraft:overworld", (1, 64, -2)))))
            self.assertEqual(client.read(1)[0], 0x47)
            self.assertEqual((client.min_y, client.height, client.has_skylight), (0, 256, False))
            self.assertEqual(client.chunks, {})

            b.sendall(frame(0x27, empty_chunk(
                section_count=16, coord=(0, 0), has_skylight=False, blocks={(10, 3, 2, 4): 88})))
            self.assertEqual(client.read(1)[0], 0x27)
            self.assertEqual(len(client.chunks[(0, 0)]), 16)
            self.assertEqual(client.block(3, 10 * 16 + 2, 4), 88)

    def test_config_packets_bin_dimension_registry(self):
        blob = (ROOT / "config_packets.bin").read_bytes()
        reader = Reader(blob)
        packets = []
        while reader.pos < len(blob):
            packets.append(reader.take(reader.varint()))
        a, b = socket.socketpair()
        with a, b:
            client = self._make_client(a)
            for raw_pkt in packets:
                b.sendall(vint(len(raw_pkt)) + raw_pkt)
                client.read(1)
        self.assertEqual(client.dimension_types_by_name["minecraft:overworld"], (-64, 384, True))
        self.assertEqual(client.dimension_types_by_name["minecraft:overworld_caves"], (-64, 384, True))
        self.assertEqual(client.dimension_types_by_name["minecraft:the_end"], (0, 256, False))
        self.assertEqual(client.dimension_types_by_name["minecraft:the_nether"], (0, 256, False))
        self.assertEqual(client.dimension_types_by_id[0], (-64, 384, True))
        self.assertEqual(client.dimension_types_by_id[2], (0, 256, False))
        self.assertEqual(client.dimension_types_by_id[3], (0, 256, False))


class CompressionTests(unittest.TestCase):
    def _client(self, sock, threshold=-1, phase="play"):
        client = Client.__new__(Client)
        client.socket = sock
        client.compression_threshold = threshold
        client.phase = phase
        client.game, client.name = 25565, "Tester"
        client.chunks, client.messages, client.updates = {}, [], []
        client.chunk_batches, client.light_updates = [], []
        client.chunk_light, client.raw_chunk = {}, {}
        client.pos, client.contract_started = None, False
        client.registry_counts = {}
        client.dimension_types_by_id = {}
        client.dimension_types_by_name = {}
        return client

    @staticmethod
    def _frame_body(framed):
        reader = Reader(framed)
        body = reader.take(reader.varint())
        assert reader.pos == len(framed), "trailing bytes after frame"
        return body

    @staticmethod
    def _recv_packet(sock, threshold):
        return unframe_packet(recv_exact(sock, recv_vint(sock)), threshold)

    def test_uncompressed_framing_is_unchanged(self):
        raw = vint(0x2f) + b"payload"
        self.assertEqual(frame_packet(raw, -1), frame(0x2f, b"payload"))
        self.assertEqual(unframe_packet(raw, -1), raw)

    def test_compressed_frame_roundtrip(self):
        small = vint(0x2f) + b"tiny"
        inner = Reader(self._frame_body(frame_packet(small, 256)))
        self.assertEqual(inner.varint(), 0)  # below threshold: data_len == 0
        rest = inner.take(len(inner.data) - inner.pos)
        self.assertEqual(rest, small)

        big = vint(0x27) + bytes(300)
        framed = frame_packet(big, 256)
        self.assertLess(len(framed), len(big))  # actually compressed
        inner = Reader(self._frame_body(framed))
        self.assertEqual(inner.varint(), len(big))  # data_len = uncompressed size
        rest = inner.take(len(inner.data) - inner.pos)
        self.assertEqual(zlib.decompress(rest), big)
        self.assertEqual(unframe_packet(self._frame_body(framed), 256), big)

    def test_client_send_read_roundtrip_compressed(self):
        a, b = socket.socketpair()
        with a, b:
            client = self._client(a, threshold=64)
            client.send(0x60, b"hello")
            reader = Reader(self._recv_packet(b, 64))
            self.assertEqual(reader.varint(), 0x60)
            self.assertEqual(reader.take(len(reader.data) - reader.pos), b"hello")
            b.sendall(frame_packet(vint(0x60) + b"teams", 64))
            self.assertEqual(client.read(1), (0x60, b"teams"))

    def _drive_login(self, server_sock, threshold):
        # Handshake and Login Start always travel uncompressed.
        for _ in range(2):
            Reader(recv_exact(server_sock, recv_vint(server_sock))).varint()
        login_success = bytes(16) + vint(1) + b"n" + vint(0) + b"\0"
        if threshold >= 0:
            server_sock.sendall(frame(0x03, vint(threshold)))  # Set Compression
            server_sock.sendall(frame_packet(vint(2) + login_success, threshold))
        else:
            server_sock.sendall(frame(2, login_success))
        # Login Acknowledged, then configuration: the client waits on 0x0e.
        self.assertEqual(Reader(self._recv_packet(server_sock, threshold)).varint(), 3)
        server_sock.sendall(frame_packet(vint(0x0e), threshold))
        config_ids = [Reader(self._recv_packet(server_sock, threshold)).varint()
                      for _ in range(2)]
        self.assertEqual(sorted(config_ids), [0, 7])
        server_sock.sendall(frame_packet(vint(3), threshold))  # Finish Configuration
        self.assertEqual(Reader(self._recv_packet(server_sock, threshold)).varint(), 3)
        # Play: Chunk Batch Finished unlocks login; the client answers 0x08.
        server_sock.sendall(frame_packet(vint(0x0c) + vint(4), threshold))
        self.assertEqual(Reader(self._recv_packet(server_sock, threshold)).varint(), 0x08)

    def _run_login(self, threshold):
        a, b = socket.socketpair()
        with a, b:
            client = self._client(a, phase="login")
            errors = []

            def login():
                try:
                    client.login()
                except Exception as exc:  # surfaced in the asserting thread
                    errors.append(exc)

            worker = threading.Thread(target=login, daemon=True)
            worker.start()
            self._drive_login(b, threshold)
            worker.join(10)
            self.assertFalse(worker.is_alive(), "login did not complete")
            self.assertEqual(errors, [])
            return client

    def test_login_negotiates_set_compression(self):
        client = self._run_login(64)
        self.assertEqual(client.compression_threshold, 64)
        self.assertEqual(client.phase, "play")
        self.assertEqual(client.chunk_batches, [4])

    def test_login_without_set_compression_still_works(self):
        client = self._run_login(-1)
        self.assertEqual(client.compression_threshold, -1)
        self.assertEqual(client.phase, "play")
        self.assertEqual(client.chunk_batches, [4])

    def test_corrupt_compression_fails_closed(self):
        raw = vint(0x60) + bytes(300)
        blob = zlib.compress(raw)
        bad_inners = [
            vint(len(raw) + 1) + blob,   # data_len does not match the stream
            vint(len(raw)) + blob[:-4],  # truncated zlib stream
            vint(10) + blob,             # compressed packet below threshold
            vint(0) + raw,               # data_len 0 yet at/above threshold
            vint(1 << 25) + blob,        # absurd decompressed length
            vint(300) + b"not-zlib-at",  # not a zlib stream at all
        ]
        for inner in bad_inners:
            with self.assertRaises(AssertionError):
                unframe_packet(inner, 64)
        # ... and through the socket path of Client.read as well.
        a, b = socket.socketpair()
        with a, b:
            client = self._client(a, threshold=64)
            b.sendall(frame_packet(vint(0x60) + b"fine", 64))
            self.assertEqual(client.read(1), (0x60, b"fine"))
            bad = vint(len(raw) + 7) + blob
            b.sendall(vint(len(bad)) + bad)
            with self.assertRaises(AssertionError):
                client.read(1)


if __name__ == "__main__":
    unittest.main()
