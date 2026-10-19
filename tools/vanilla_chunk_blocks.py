#!/usr/bin/env python3
"""Decode a live server's chunk sections into block ids, column by column.

  tools/vanilla_chunk_blocks.py <port> <cx> <cz> [x z [y0 [y1]]]

`protocol767.decode_chunk_light` asserts that each section's non-air count equals
the count of non-air palette ids. That assertion is a *fixture* check: it compares
against `assets/blocks_1211.json`'s air state ids, which need not match another
server's registry, so a vanilla server fails it even though the sections parse
perfectly. Skipping only the assertion and reading palettes without a registry
upper bound makes vanilla's blocks readable, which is what block-by-block parity
work needs.

Prints, for the requested columns, `y:state` over [y0, y1] (default 58..69) and a
digest of the whole section blob so two captures can be compared cheaply.
"""
import hashlib
import os
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tests"))
import protocol767  # noqa: E402
from protocol767 import Client, Reader  # noqa: E402

protocol767.STRICT = False


def palette(reader, count, indirect_max, minimum_bits):
    """Read a paletted container without the fixture's registry bound."""
    bits = reader.number("B")
    if bits == 0:
        value = reader.varint()
        reader.varint()  # zero-length data array
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


def parse_sections(body):
    reader = Reader(body)
    coord = (reader.number(">i"), reader.number(">i"))
    reader.nbt()
    data = Reader(reader.take(reader.varint()))
    sections = []
    for _ in range(24):
        data.number(">h")  # non-air count, unchecked against the fixture
        sections.append(palette(data, 4096, 8, 4))
        palette(data, 64, 3, 1)  # biomes
    return coord, data.data, sections


def main():
    if len(sys.argv) < 4:
        print(__doc__)
        return 2
    port, cx, cz = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3])
    px, pz = (int(sys.argv[4]), int(sys.argv[5])) if len(sys.argv) > 5 else (8, 8)
    y0 = int(sys.argv[6]) if len(sys.argv) > 6 else 58
    y1 = int(sys.argv[7]) if len(sys.argv) > 7 else 69
    captured = {}

    def capture(body, _biomes):
        reader = Reader(body)
        coord = (reader.number(">i"), reader.number(">i"))
        captured[coord] = body
        return coord, None, [0, 0, 0, 0], [], []

    protocol767.decode_chunk_light = capture
    with Client(port, "Blocks") as client:
        try:
            client.login(view=2)
        except AssertionError:
            pass  # a re-sent chunk trips the fixture's duplicate check; capture already ran
        # Vanilla ignores a teleport larger than its movement threshold ("moved too quickly"),
        # so walk to the target in small steps and let it stream the chunks on the way.
        tx, tz = cx * 16 + px, cz * 16 + pz
        steps = int(max(abs(tx), abs(tz)) // 8) + 1
        for i in range(1, steps + 1):
            client.move(tx * i / steps, 80, tz * i / steps)
            try:
                client.read(0.05)
            except TimeoutError:
                pass
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and (cx, cz) not in captured:
            try:
                client.read(1.0)
            except TimeoutError:
                break
            except AssertionError:
                continue
    if (cx, cz) not in captured:
        print(f"no chunk packet for {(cx, cz)}")
        return 1
    coord, blob, sections = parse_sections(captured[(cx, cz)])

    def block(x, y, z):
        section = (y - protocol767.MIN_Y) // 16
        local_y = (y - protocol767.MIN_Y) % 16
        return sections[section][(local_y << 8) | ((z & 15) << 4) | (x & 15)]

    print(f"chunk {coord} sections_blob={len(blob)} sha256={hashlib.sha256(blob).hexdigest()[:16]}")
    for lz in range(pz, pz + 1):
        for lx in range(px, px + 1):
            print(f"x={cx * 16 + lx} z={cz * 16 + lz}: " +
                  " ".join(f"{y}:{block(lx, y, lz)}" for y in range(y0, y1 + 1)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
