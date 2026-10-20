#!/usr/bin/env python3
"""Predict an ore feature's cells the way vanilla 1.21.1 does, then compare with
the blocks a replayed vanilla world actually contains.

  tools/ore_oracle.py <world_dir> <cx> <cz> <feature> <size> <count> <y0> <y1> [gi ...]

`feature` picks the target tag: `stone` means `base_stone_overworld`
(stone/granite/diorite/andesite/tuff/deepslate), `deepslate` means
`deepslate_ore_replaceables`. `count` is the constant CountPlacement, `y0..y1`
the uniform absolute height range, and the trailing `gi` values are tried as the
step index in the feature seed, so the caller can find which index a vanilla
world was generated with.

This exists because "the seeds match but the cells do not" cannot be resolved by
reading two implementations side by side: the bytecode of `OreFeature.place`
mixes three precisions (Math.sin for the centre, Mth.sin's table for the radii,
double lerp for the points), and the per-cell `nextFloat` draws of the air
exposure check mean that a single off-by-one cell shifts every later draw of the
vein. Reproducing it here gives an oracle to diff a Rust dump against, without a
JVM.
"""
import math
import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from vanilla_region_blocks import chunk_bytes, parse_nbt  # noqa: E402

M64 = (1 << 64) - 1
PI_F32 = struct.unpack("<f", struct.pack("<f", math.pi))[0]


def f32(x):
    return struct.unpack("<f", struct.pack("<f", x))[0]


def stafford13(z):
    z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & M64
    z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & M64
    return (z ^ (z >> 31)) & M64


def rotl(x, n):
    return ((x << n) | (x >> (64 - n))) & M64


class Xor:
    """Xoroshiro128++ as seeded by vanilla's `XoroshiroRandomSource(long)`."""

    def __init__(self, seed):
        l = (seed ^ 0x6A09E667F3BCC909) & M64
        m = (l + 0x9E3779B97F4A7C15) & M64
        self.lo, self.hi = stafford13(l), stafford13(m)
        if (self.lo | self.hi) == 0:
            self.lo, self.hi = 0x9E3779B97F4A7C15, 0x6A09E667F3BCC909

    def next_long(self):
        l, h = self.lo, self.hi
        n = (rotl((l + h) & M64, 17) + l) & M64
        h ^= l
        self.lo = rotl(l, 49) ^ h ^ ((h << 21) & M64)
        self.hi = rotl(h, 28)
        return n

    def next(self, bits):
        return self.next_long() >> (64 - bits)

    def next_i32(self):
        v = self.next_long() & 0xFFFFFFFF
        return v - (1 << 32) if v >= (1 << 31) else v

    def next_bound(self, bound):
        if bound <= 0:
            raise ValueError("bound")
        l = self.next_i32() & 0xFFFFFFFF
        m = l * bound
        n = m & 0xFFFFFFFF
        if n < bound:
            i = ((1 << 64) - bound) % bound
            while n < i:
                l = self.next_i32() & 0xFFFFFFFF
                m = l * bound
                n = m & 0xFFFFFFFF
        return m >> 32

    def next_f32(self):
        return f32(self.next(24) * 5.9604645e-8)

    def next_f64(self):
        return (self.next_long() >> 11) * (2.0 ** -53)


class Lcg:
    def __init__(self, seed):
        self.seed = (seed ^ 0x5DEECE66D) & 0xFFFFFFFFFFFF

    def nx(self, bits):
        self.seed = (self.seed * 0x5DEECE66D + 11) & 0xFFFFFFFFFFFF
        return self.seed >> (48 - bits)

    def next_i64(self):
        v = (self.nx(32) << 32) + self.nx(32)
        return v - (1 << 64) if v >= (1 << 63) else v


SIN = [f32(math.sin(i * math.pi * 2.0 / 65536.0)) for i in range(65536)]


def mth_sin(v):
    return SIN[int(f32(v * f32(10430.378))) & 65535]


def pop_seed(seed, bx, bz):
    r = Lcg(seed)
    l = r.next_i64() | 1
    m = r.next_i64() | 1
    return (((bx * l + bz * m) & M64) ^ seed) & M64


TAGS = {
    "stone": {"minecraft:stone", "minecraft:granite", "minecraft:diorite",
              "minecraft:andesite", "minecraft:tuff", "minecraft:deepslate"},
    "deepslate": {"minecraft:deepslate", "minecraft:tuff"},
}


def place(seed, origin, size, count, y0, y1, tag):
    """Yields the cells the feature writes, in vanilla's order, as (x, y, z)."""
    rng = Xor(seed)
    cells = []
    for _ in range(count):
        x = origin[0] + rng.next_bound(16)
        z = origin[2] + rng.next_bound(16)
        y = rng.next_bound(y1 - y0 + 1) + y0 if y1 > y0 else y0
        out = ore(rng, (x, y, z), size, tag)
        cells.extend(out)
    return cells


def ore(rng, pos, size, tag):
    allowed = TAGS[tag]
    f = f32(rng.next_f32() * PI_F32)
    g = f32(size / 8.0)
    i = int(math.ceil(f32(f32(f32(size / 16.0) * 2.0) + 1.0) / 2.0))
    s = math.sin(f)
    c = math.cos(f)
    d = pos[0] + s * g
    e = pos[0] - s * g
    h = pos[2] + c * g
    j = pos[2] - c * g
    l = pos[1] + rng.next_bound(3) - 2
    m = pos[1] + rng.next_bound(3) - 2
    cs = int(math.ceil(g))
    n = pos[0] - cs - i
    o = pos[1] - 2 - i
    p = pos[2] - cs - i
    q = 2 * (cs + i)
    r2 = 2 * (2 + i)
    return vein(rng, d, e, h, j, l, m, n, o, p, q, r2, size, allowed)


def vein(rng, sx, ex, sz, ez, sy, ey, xb, yb, zb, hs, vs, size, allowed):
    j = size
    ds = [0.0] * (j * 4)
    for k in range(j):
        fr = f32(k / j)
        ds[k * 4] = sx + (ex - sx) * fr
        ds[k * 4 + 1] = sy + (ey - sy) * fr
        ds[k * 4 + 2] = sz + (ez - sz) * fr
        hh = rng.next_f64() * j / 16.0
        s1 = f32(mth_sin(f32(PI_F32 * fr)) + 1.0)
        ds[k * 4 + 3] = (s1 * hh + 1.0) / 2.0
    for k in range(j - 1):
        if ds[k * 4 + 3] <= 0.0:
            continue
        for mm in range(k + 1, j):
            if ds[mm * 4 + 3] <= 0.0:
                continue
            hv = ds[k * 4 + 3] - ds[mm * 4 + 3]
            dv = ds[k * 4] - ds[mm * 4]
            ev = ds[k * 4 + 1] - ds[mm * 4 + 1]
            gv = ds[k * 4 + 2] - ds[mm * 4 + 2]
            if hv * hv > dv * dv + ev * ev + gv * gv:
                if hv > 0.0:
                    ds[mm * 4 + 3] = -1.0
                else:
                    ds[k * 4 + 3] = -1.0
    bits = bytearray(hs * vs * hs)
    out = []
    for mm in range(j):
        d = ds[mm * 4 + 3]
        if d <= 0.0:
            continue
        cx, cy, cz = ds[mm * 4], ds[mm * 4 + 1], ds[mm * 4 + 2]
        x0 = max(int(math.floor(cx - d)), xb)
        y0 = max(int(math.floor(cy - d)), yb)
        z0 = max(int(math.floor(cz - d)), zb)
        x1 = max(int(math.floor(cx + d)), x0)
        y1 = max(int(math.floor(cy + d)), y0)
        z1 = max(int(math.floor(cz + d)), z0)
        for xx in range(x0, x1 + 1):
            ux = (xx + 0.5 - cx) / d
            if ux * ux >= 1.0:
                continue
            for yy in range(y0, y1 + 1):
                uy = (yy + 0.5 - cy) / d
                if ux * ux + uy * uy >= 1.0:
                    continue
                base = (xx - xb) + (yy - yb) * hs
                for zz in range(z0, z1 + 1):
                    uz = (zz + 0.5 - cz) / d
                    if ux * ux + uy * uy + uz * uz >= 1.0:
                        continue
                    bi = base + (zz - zb) * hs * vs
                    if bi >= len(bits) or bits[bi]:
                        continue
                    bits[bi] = 1
                    out.append((xx, yy, zz))
    return out


def vanilla_andesite(world, cx, cz):
    chunk, _ = parse_nbt(chunk_bytes(world, cx, cz))
    cells = set()
    for s in chunk.get("sections", []):
        sy = s.get("Y", 0)
        bs = s.get("block_states")
        if not bs:
            continue
        palette = [p.get("Name", "?") for p in bs.get("palette", [])]
        data = bs.get("data") or []
        if len(palette) <= 1:
            names = [palette[0]] * 4096 if palette else ["minecraft:air"] * 4096
        else:
            bits = max(4, (len(palette) - 1).bit_length())
            per = 64 // bits
            mask = (1 << bits) - 1
            names = []
            for i in range(4096):
                v = 0 if not data else (data[i // per] >> ((i % per) * bits)) & mask
                names.append(palette[v])
        for i, name in enumerate(names):
            if name == "minecraft:andesite":
                lx, lz, ly = i & 15, (i >> 4) & 15, i >> 8
                cells.add((cx * 16 + lx, sy * 16 + ly, cz * 16 + lz))
    return cells


def main():
    if len(sys.argv) < 9:
        print(__doc__)
        return 2
    world, cx, cz = sys.argv[1], int(sys.argv[2], 0), int(sys.argv[3], 0)
    tag, size, count, y0, y1 = sys.argv[4], int(sys.argv[5]), int(sys.argv[6]), int(sys.argv[7]), int(sys.argv[8])
    gis = [int(g) for g in sys.argv[9:]] or [0]
    want = vanilla_andesite(world, cx, cz) if tag == "stone" else set()
    base = pop_seed(0, cx * 16, cz * 16)
    for gi in gis:
        seed = (base + gi + 10000 * 6) & M64
        cells = place(seed, (cx * 16, 0, cz * 16), size, count, y0, y1, tag)
        pred = set(cells)
        inter = len(pred & want)
        if want:
            print(f"gi={gi} cells={len(pred)} vanilla={len(want)} common={inter} "
                  f"recall={inter/max(1,len(want)):.3f} precision={inter/max(1,len(pred)):.3f}")
        else:
            print(f"gi={gi} cells={len(pred)}")
        if os.environ.get("ORE_DUMP"):
            for (x, y, z) in sorted(pred):
                print(f"ORE {gi} {x} {y} {z}")


if __name__ == "__main__":
    sys.exit(main())
