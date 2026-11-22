#!/usr/bin/env python3
"""Compare a stored vanilla chunk's heightmaps against the port's own dump.

  tools/heightmap_diff.py <world_dir> <cx> <cz> <port_hm.bin> [--n 6] [--limit 12]

Why this exists: the kelp residual in a 6x6 ocean fixture
(`evidence/gameplay/carvers/README.md`, section 62.1) was attributed column by
column, and 53 of the 484 reference columns were put down to "the two sides read
a different `getHeight(OCEAN_FLOOR)`". A saved vanilla chunk carries its own
heightmap, so that attribution can be checked rather than assumed: the port
writes the same quantity with `MEGU_WG_DUMP_HM` and this compares them.

Both sides are the value a feature reads, i.e. the first free y above the floor.
The port's is `max(chunk top, feature layer)`; vanilla's went through its own
`Heightmap.update` on every `setBlock`, which is monotone in the same way for
floor blocks but not identical for every removal - a difference in the *final*
map is therefore evidence about the heightmap, not automatically about the kelp
decision that used it earlier in the same pass.

Exit status is 0 even when columns differ: this is a measurement, and the caller
decides what a mismatch means.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from vanilla_region_blocks import chunk_bytes, parse_nbt  # noqa: E402

MIN_Y = -64
BITS = 9
# A stored heightmap entry is relative to the dimension floor: the first free y is
# `value + MIN_Y`. Measured on chunk (79,-1) of the pinned fixture: the saved
# WORLD_SURFACE reads 127 where sea level is 63 and the top water block is 62, and
# 62 + 64 = 126 is the *block*, so the entry is the first free y one above it:
# 63 + 64 = 127. The port writes that absolute value, so the comparison converts
# the reference and a wrong conversion shows up as a constant +-1.
PER_LONG = 64 // BITS
MASK = (1 << BITS) - 1
# the heightmap index is (z << 4) | x
def unpack(longs, n=256):
    out = []
    for i in range(n):
        word = longs[i // PER_LONG] & 0xFFFFFFFFFFFFFFFF
        out.append((word >> ((i % PER_LONG) * BITS)) & MASK)
    return out


def reference_heights(world, cx, cz, name):
    """The stored heightmap of one chunk, or None when it is not saved."""
    nbt, _ = parse_nbt(chunk_bytes(world, cx, cz))
    hm = nbt.get("Heightmaps", {})
    longs = hm.get(name)
    if not longs:
        return None
    return unpack(longs)


def main():
    if len(sys.argv) < 5:
        raise SystemExit(__doc__)
    world, cx, cz, port_path = sys.argv[1], int(sys.argv[2], 0), int(sys.argv[3], 0), sys.argv[4]
    n = 6
    limit = 12
    name = "OCEAN_FLOOR"
    args = sys.argv[5:]
    while args:
        a = args.pop(0)
        if a == "--n":
            n = int(args.pop(0))
        elif a == "--limit":
            limit = int(args.pop(0))
        elif a == "--heightmap":
            name = args.pop(0)
        else:
            raise SystemExit(f"unknown argument {a}")
    raw = open(port_path, "rb").read()
    port = []
    for i in range(0, len(raw), 4):
        port.append(int.from_bytes(raw[i:i + 4], "little", signed=True))
    want = n * n * 256
    if len(port) != want:
        raise SystemExit(f"port dump holds {len(port)} columns, expected {want} for {n}x{n}")

    total = 0
    differ = 0
    missing = 0
    edge_tot = 0
    hist = {}
    examples = []
    diffs = []
    for dz in range(n):
        for dx in range(n):
            ccx, ccz = cx + dx, cz + dz
            ref = reference_heights(world, ccx, ccz, name)
            if ref is None:
                missing += 1
                continue
            base = (dz * n + dx) * 256
            is_edge = dx in (0, n - 1) or dz in (0, n - 1)
            for idx in range(256):
                lx, lz = idx & 15, idx >> 4
                got = port[base + idx]
                exp = ref[idx] + MIN_Y
                total += 1
                if is_edge:
                    edge_tot += 1
                if got != exp:
                    differ += 1
                    diffs.append((is_edge, lx, lz, got - exp))
                    key = got - exp
                    hist[key] = hist.get(key, 0) + 1
                    if len(examples) < limit:
                        examples.append((ccx * 16 + lx, ccz * 16 + lz, exp, got, key))
    print(
        f"heightmap {name}: {total} columns compared, {differ} differ, {missing} chunks unreadable"
    )
    # Where the differences are, because "a band inside every chunk" and "the
    # region's outer chunks" are different bugs. lx/lz are within-chunk.
    edge_d = sum(1 for c in diffs if c[0])
    print(f"  region-edge chunks {edge_d}/{edge_tot}, interior {differ - edge_d}/{total - edge_tot}")
    if differ - edge_d:
        rows = [0] * 16
        cols = [0] * 16
        for is_edge, lx, lz, _ in diffs:
            if not is_edge:
                rows[lz] += 1
                cols[lx] += 1
        print("  interior differences by lz:", rows)
        print("  interior differences by lx:", cols)
    if hist:
        top = sorted(hist.items(), key=lambda kv: -kv[1])[:8]
        print("  port - reference:", ", ".join(f"{k:+d} x{v}" for k, v in top))
    for x, z, exp, got, d in examples:
        print(f"  ({x:6d},{z:6d}) reference={exp:5d} port={got:5d} ({d:+d})")
    if differ > limit:
        print(f"  ... and {differ - limit} more")


if __name__ == "__main__":
    main()
