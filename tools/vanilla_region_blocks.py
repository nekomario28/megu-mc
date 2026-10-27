#!/usr/bin/env python3
"""Count block names in a chunk straight from an Anvil region file.

  tools/vanilla_region_blocks.py <world_dir> <cx> <cz> [--cells <name>]

Why this exists: comparing a feature layer against vanilla needs vanilla's own
blocks, but starting a server to serve one chunk costs a JVM and a world save.
The region file on disk already holds them, so this reads
`world/region/r.<cx>>5>.<cz>>5>.mca`, takes the chunk at the requested index,
and walks `sections[].block_states` (1.21 layout: per-section `palette` of
`Name`s plus `data` longs that never span a word).

`--cells <name>` also prints the `x,y,z` of every cell holding that block, so a
vein's shape can be compared against a generator's own dump.
"""
import os
import sys
import struct
import zlib
from collections import Counter


def parse_nbt(buf, p=0):
    tag = buf[p]
    p += 1
    if tag == 0:
        return None, p
    nlen = int.from_bytes(buf[p:p + 2], "big")
    p += 2 + nlen
    return parse_payload(buf, p, tag)


def parse_payload(buf, p, tag):
    if tag == 1:
        return int.from_bytes(buf[p:p + 1], "big", signed=True), p + 1
    if tag == 2:
        return int.from_bytes(buf[p:p + 2], "big", signed=True), p + 2
    if tag == 3:
        return int.from_bytes(buf[p:p + 4], "big", signed=True), p + 4
    if tag == 4:
        return int.from_bytes(buf[p:p + 8], "big", signed=True), p + 8
    if tag == 5:
        return struct.unpack_from(">f", buf, p)[0], p + 4
    if tag == 6:
        return struct.unpack_from(">d", buf, p)[0], p + 8
    if tag == 7:
        n = int.from_bytes(buf[p:p + 4], "big")
        return bytes(buf[p + 4:p + 4 + n]), p + 4 + n
    if tag == 8:
        n = int.from_bytes(buf[p:p + 2], "big")
        return buf[p + 2:p + 2 + n].decode("utf-8", "replace"), p + 2 + n
    if tag == 9:
        t = buf[p]
        n = int.from_bytes(buf[p + 1:p + 5], "big")
        p += 5
        out = []
        for _ in range(n):
            v, p = parse_payload(buf, p, t)
            out.append(v)
        return out, p
    if tag == 10:
        out = {}
        while True:
            t = buf[p]
            p += 1
            if t == 0:
                return out, p
            nlen = int.from_bytes(buf[p:p + 2], "big")
            key = buf[p + 2:p + 2 + nlen].decode("utf-8", "replace")
            p += 2 + nlen
            out[key], p = parse_payload(buf, p, t)
    if tag == 11:
        n = int.from_bytes(buf[p:p + 4], "big")
        return [int.from_bytes(buf[p + 4 + 4 * i:p + 8 + 4 * i], "big", signed=True) for i in range(n)], p + 4 + 4 * n
    if tag == 12:
        n = int.from_bytes(buf[p:p + 4], "big")
        return [int.from_bytes(buf[p + 4 + 8 * i:p + 12 + 8 * i], "big", signed=True) for i in range(n)], p + 4 + 8 * n
    raise ValueError(f"tag {tag} at {p}")


def chunk_bytes(world, cx, cz):
    rx, rz = cx >> 5, cz >> 5
    path = os.path.join(world, "region", f"r.{rx}.{rz}.mca")
    with open(path, "rb") as f:
        raw = f.read()
    idx = (cx & 31) + (cz & 31) * 32
    off = int.from_bytes(raw[idx * 4:idx * 4 + 3], "big")
    if off == 0:
        raise SystemExit(f"chunk {cx},{cz} not present in {path}")
    start = off * 4096
    length = int.from_bytes(raw[start:start + 4], "big")
    kind = raw[start + 4]
    body = raw[start + 5:start + 4 + length]
    if kind == 1:
        import gzip
        return gzip.decompress(body)
    if kind == 2:
        return zlib.decompress(body)
    if kind == 3:
        return body
    raise SystemExit(f"unsupported compression {kind}")


def cells_of(section, bits, data, palette):
    out = []
    per = 64 // bits
    mask = (1 << bits) - 1
    for i in range(4096):
        if data:
            word = data[i // per] & 0xFFFFFFFFFFFFFFFF
            v = (word >> ((i % per) * bits)) & mask
        else:
            v = 0
        out.append(palette[v])
    return out


def main():
    if len(sys.argv) < 4:
        raise SystemExit(__doc__)
    world, cx, cz = sys.argv[1], int(sys.argv[2], 0), int(sys.argv[3], 0)
    want = None
    if "--cells" in sys.argv:
        want = sys.argv[sys.argv.index("--cells") + 1]
    chunk, _ = parse_nbt(chunk_bytes(world, cx, cz))
    counts = Counter()
    cells = []
    for s in chunk.get("sections", []):
        sy = s.get("Y", 0)
        bs = s.get("block_states")
        if not bs:
            continue
        palette = [p.get("Name", "?") for p in bs.get("palette", [])]
        data = bs.get("data") or []
        bits = max(4, (len(palette) - 1).bit_length()) if len(palette) > 1 else 4
        if len(palette) <= 1:
            names = [palette[0] if palette else "minecraft:air"] * 4096
        else:
            names = cells_of(bs, bits, data, palette)
        for i, name in enumerate(names):
            counts[name] += 1
            if want and name == want:
                lx, lz, ly = i & 15, (i >> 4) & 15, i >> 8
                cells.append((cx * 16 + lx, sy * 16 + ly, cz * 16 + lz))
    for name, n in counts.most_common(24):
        print(f"{n:6d} {name}")
    if want:
        for (x, y, z) in sorted(cells, key=lambda t: (t[1], t[0], t[2])):
            print(f"CELL {x} {y} {z}")


if __name__ == "__main__":
    main()
