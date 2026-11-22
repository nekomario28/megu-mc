#!/usr/bin/env python3
"""Diff one chunk's blocks between two live servers, layer by layer.

  tools/chunk_block_diff.py <port_a> <port_b> <cx> <cz> [limit]

Both servers are assumed to speak protocol 767 and share the block-state id
order (true for vanilla 1.21.1 and megu's assets/blocks_1211.json), so the
decoded ids are directly comparable.

Each differing cell is classified into the layer a worldgen fix would live in:

  bedrock  y <= min_y + 4           — the per-cell floor pattern
  feature  above the column's top   — air vs a placed block (tree, ore surface)
  surface  the top few solid blocks — the material rule (grass/dirt/deepslate)
  deep     the rest                 — density / ore / deepslate gradient

and the block-state pairs are counted, so "which fix buys the most" is a number,
not a guess.
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
SURFACE_BAND = 4


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
            client.login(view=int(os.environ.get("MC_PROBE_VIEW", "8")))
        except AssertionError:
            pass
        # Vanilla rejects a move that exceeds its per-tick threshold ("moved too quickly"),
        # so step one block at a time and give the server a tick between moves.
        tx, tz = (cx * 16 + 8, cz * 16 + 8) if True else (0, 0)
        sx, sz = 0.0, 0.0
        steps = int(max(abs(tx - sx), abs(tz - sz))) + 1
        for i in range(1, steps + 1):
            client.move(sx + (tx - sx) * i / steps, 80, sz + (tz - sz) * i / steps)
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


def top_solid(sections, cx, cz):
    """Highest non-air y per column, from the reference side."""
    tops = {}
    for section in range(HEIGHT // 16 - 1, -1, -1):
        for index in range(4096):
            if sections[section][index] != 0:
                y = MIN_Y + section * 16 + (index >> 8)
                wx = cx * 16 + (index & 15)
                wz = cz * 16 + ((index >> 4) & 15)
                if (wx, wz) not in tops:
                    tops[(wx, wz)] = y
    return tops


def main():
    if len(sys.argv) < 5:
        print(__doc__)
        return 2
    pa, pb, cx, cz = (int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]))
    limit = int(sys.argv[5]) if len(sys.argv) > 5 else 12
    a, b = fetch(pa, cx, cz), fetch(pb, cx, cz)
    tops = top_solid(a, cx, cz)

    layers = Counter()
    pairs = Counter()
    samples = []
    for section in range(HEIGHT // 16):
        for index in range(4096):
            va, vb = a[section][index], b[section][index]
            if va == vb:
                continue
            y = MIN_Y + section * 16 + (index >> 8)
            wx = cx * 16 + (index & 15)
            wz = cz * 16 + ((index >> 4) & 15)
            top = tops.get((wx, wz), MIN_Y)
            if y <= MIN_Y + 4:
                layer = "bedrock"
            elif y > top:
                layer = "feature"
            elif y >= top - SURFACE_BAND:
                layer = "surface"
            else:
                layer = "deep"
            layers[layer] += 1
            pairs[(va, vb)] += 1
            if len(samples) < limit:
                samples.append((layer, wx, y, wz, va, vb))
    total = sum(layers.values())
    print(f"chunk {(cx, cz)}: {total} differing cells of {HEIGHT * 256}")
    for layer, count in layers.most_common():
        print(f"  {layer:8s} {count:6d}  {100.0 * count / max(total, 1):5.1f}%")
    print("  top state pairs (a != b):")
    for (va, vb), count in pairs.most_common(10):
        print(f"    {va} -> {vb}: {count}")
    for layer, wx, y, wz, va, vb in samples:
        print(f"  [{layer}] ({wx},{y},{wz}) a={va} b={vb}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
