#!/usr/bin/env python3
"""Diff one chunk's blocks between two live servers, state id by state id.

  tools/chunk_block_diff.py <port_a> <port_b> <cx> <cz> [limit]

Both servers are assumed to speak protocol 767 and share the block-state id
order (true for vanilla 1.21.1 and mc-spine's assets/blocks_1211.json), so the
decoded ids are directly comparable. Prints the number of differing cells, a
per-y histogram, and up to `limit` sample positions with both ids.

The sections are read without the fixture's registry-bound assertion: that
assertion compares against one server's air state ids and rejects the other
before the blocks are read. See tools/vanilla_chunk_blocks.py.
"""
import os
import sys
import time
from collections import Counter

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tests"))
import protocol767  # noqa: E402
from protocol767 import Client, Reader  # noqa: E402

protocol767.STRICT = False
MIN_Y = protocol767.MIN_Y
HEIGHT = 384


def palette(reader, count, indirect_max):
    bits = reader.number("B")
    if bits == 0:
        value = reader.varint()
        reader.varint()
        return [value] * count
    if bits <= indirect_max:
        values = [reader.varint() for _ in range(reader.varint())]
    else:
        values = None
    words = [reader.number(">Q") for _ in range(reader.varint())]
    per_word = 64 // bits
    indices = [(words[i // per_word] >> ((i % per_word) * bits)) & ((1 << bits) - 1)
               for i in range(count)]
    return [values[i] for i in indices] if values is not None else indices


def fetch(port, cx, cz):
    captured = {}

    def capture(body, _biomes):
        reader = Reader(body)
        coord = (reader.number(">i"), reader.number(">i"))
        captured[coord] = body
        return coord, None, [0, 0, 0, 0], [], []

    protocol767.decode_chunk_light = capture
    with Client(port, "Diff") as client:
        try:
            client.login(view=2)
        except AssertionError:
            pass
        client.move(cx * 16 + 8, 80, cz * 16 + 8)
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and (cx, cz) not in captured:
            try:
                client.read(1.0)
            except TimeoutError:
                break
            except AssertionError:
                continue
    if (cx, cz) not in captured:
        raise SystemExit(f"port {port}: no chunk for {(cx, cz)}")
    reader = Reader(captured[(cx, cz)])
    reader.number(">i")
    reader.number(">i")
    reader.nbt()
    data = Reader(reader.take(reader.varint()))
    sections = []
    for _ in range(HEIGHT // 16):
        data.number(">h")
        sections.append(palette(data, 4096, 8))
        palette(data, 64, 3)
    return sections


def main():
    if len(sys.argv) < 5:
        print(__doc__)
        return 2
    pa, pb, cx, cz = (int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]))
    limit = int(sys.argv[5]) if len(sys.argv) > 5 else 20
    a, b = fetch(pa, cx, cz), fetch(pb, cx, cz)

    diffs = []
    for section in range(HEIGHT // 16):
        for index in range(4096):
            if a[section][index] != b[section][index]:
                y = MIN_Y + section * 16 + (index >> 8)
                x = (cx * 16 + (index & 15), cz * 16 + ((index >> 4) & 15))
                diffs.append((y, x, a[section][index], b[section][index]))
    print(f"chunk {(cx, cz)}: {len(diffs)} differing cells of {HEIGHT * 256}")
    hist = Counter(y for y, _, _, _ in diffs)
    for y in sorted(hist):
        print(f"  y={y}: {hist[y]}")
    for y, (wx, wz), va, vb in sorted(diffs)[:limit]:
        print(f"  ({wx},{y},{wz}) a={va} b={vb}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
