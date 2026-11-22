#!/usr/bin/env python3
"""Diff megu's offline chunk dump against a vanilla (Anvil) world.

  tools/carver_diff.py <dump.bin> <world_dir> [--n 6] [--y 0:60] [--cells N]

The dump comes from `MEGU_WG_DUMP=<file> MEGU_WG_DUMP_N=<n>` (see
`main.rs`): `n*n` chunks from (0,0) in chunk-major order, each 24 sections of
`(ly << 8) | (lz << 4) | lx` `u32` block states, one section per 16 block y.

Vanilla's side is read straight from the region file and translated through
`assets/blocks_1211.json` (the same state ordering the server uses), so both
sides compare as state ids, not names: a water level or an axis property
mismatch is visible instead of being flattened away.

Why this exists: a carver port is only correct when the *cell set* matches, and
`chunk_block_diff.py` needs two live servers. This reads a saved world, so
"change code -> dump chunk -> count" takes seconds.

Every unequal state is counted, including leaf distance and fluid levels.
A saved world includes runtime updates, but a property name does not establish
which stage wrote it: TreeFeature and LeavesBlock.tick both change distance.
Classify stages with native writer traces, not by suppressing matching block
names. The historical --tick flag remains accepted; all states are now counted
with or without it.

Classifies every differing cell by the layer a worldgen fix would live in:

  carver    air/water/lava where vanilla has one, or the reverse
  deep      non-air texture differences below the surface window
  surface   the top few solid blocks of the column (material rule)
  feature   a non-air state above the column's vanilla top
"""
import json
import os
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
from vanilla_region_blocks import chunk_bytes, parse_nbt, cells_of  # noqa: E402

MIN_Y = -64
HEIGHT = 384
SECTIONS = HEIGHT // 16
CELLS = SECTIONS * 4096


def load_names():
    """block state id -> block name, for reading a `--cells` report."""
    names = {}
    for block in json.loads((ROOT / "assets" / "blocks_1211.json").read_text()):
        for state in range(block["minStateId"], block["maxStateId"] + 1):
            names[state] = block["name"]
    return names


def load_states():
    """block state id for every (name, properties) the vanilla world can hold."""
    table = {}
    for block in json.loads((ROOT / "assets" / "blocks_1211.json").read_text()):
        props = block["states"]
        base = block["minStateId"]
        for state in range(base, block["maxStateId"] + 1):
            off = state - base
            key = []
            mult = 1
            for p in reversed(props):
                vals = p.get("values") or ["true", "false"]
                key.append(vals[(off // mult) % len(vals)])
                mult *= len(vals)
            table[(block["name"], tuple(reversed(key)))] = state
    return table


def vanilla_chunk(world, cx, cz, table, min_y=MIN_Y, height=HEIGHT):
    """State ids for one chunk, or None when the world never finished generating it.

    A saved world holds partially generated chunks (`Status: minecraft:biomes`)
    wherever a client asked for terrain the server did not finish; comparing
    against those would report the whole chunk as missing terrain.
    """
    if height <= 0 or height % 16 or min_y % 16:
        raise ValueError('chunk height must be positive and section aligned')
    sections = height // 16
    chunk, _ = parse_nbt(chunk_bytes(world, cx, cz))
    if chunk.get("Status") not in ("minecraft:full", "full"):
        return None
    out = [-1] * (sections * 4096)
    for s in chunk.get("sections", []):
        idx = s.get("Y", 0) - min_y // 16
        bs = s.get("block_states")
        if not (0 <= idx < sections) or not bs:
            continue  # a section without block_states is all air
        palette = bs.get("palette", [])
        if not palette:
            continue
        ids = []
        for p in palette:
            name = p.get("Name", "minecraft:air").replace("minecraft:", "")
            props = tuple(v for _, v in sorted((p.get("Properties") or {}).items()))
            ids.append(table.get((name, props)))
        if len(palette) == 1:
            vals = [ids[0]] * 4096
        else:
            bits = max(4, (len(palette) - 1).bit_length())
            pick = cells_of(bs, bits, bs.get("data") or [], list(range(len(palette))))
            vals = [ids[pick[i]] for i in range(4096)]
        for i in range(4096):
            out[idx * 4096 + i] = vals[i] if vals[i] is not None else -2
    return out


def main():
    if len(sys.argv) < 3:
        raise SystemExit(__doc__)
    dump, world = sys.argv[1], sys.argv[2]
    n, x0, z0 = 6, 0, 0
    min_y, height = MIN_Y, HEIGHT
    if '--min-y' in sys.argv:
        min_y = int(sys.argv[sys.argv.index('--min-y') + 1])
    if '--height' in sys.argv:
        height = int(sys.argv[sys.argv.index('--height') + 1])
    if height <= 0 or height % 16 or min_y % 16:
        raise SystemExit('height must be positive and section aligned')
    sections = height // 16
    cell_count = sections * 4096
    if "--n" in sys.argv:
        n = int(sys.argv[sys.argv.index("--n") + 1])
    if "--x0" in sys.argv:
        x0 = int(sys.argv[sys.argv.index("--x0") + 1])
    if "--z0" in sys.argv:
        z0 = int(sys.argv[sys.argv.index("--z0") + 1])
    band = (min_y, min_y + height)
    compared = 0
    detail = []
    if "--cells" in sys.argv:
        want_n = int(sys.argv[sys.argv.index("--cells") + 1])
    else:
        want_n = 0
    names = load_names()
    if "--y" in sys.argv:
        lo, hi = sys.argv[sys.argv.index("--y") + 1].split(":")
        band = (int(lo), int(hi))
    raw = Path(dump).read_bytes()
    if len(raw) != n * n * cell_count * 4:
        raise SystemExit('dump length does not match the requested region and dimension height')
    table = load_states()
    total = 0
    per_chunk = {}
    skipped = []
    pairs = Counter()
    layers = Counter()
    airlike = {0, 12959} | set(range(80, 112))  # air, cave_air, water, lava
    for dz in range(n):
        for dx in range(n):
            (cx, cz) = (x0 + dx, z0 + dz)
            cells = raw[(dz * n + dx) * cell_count * 4:(dz * n + dx + 1) * cell_count * 4]
            if len(cells) != cell_count * 4:
                raise SystemExit(f"dump too short at chunk {cx},{cz}")
            got = [int.from_bytes(cells[i * 4:i * 4 + 4], "little") for i in range(cell_count)]
            want = vanilla_chunk(world, cx, cz, table, min_y=min_y, height=height)
            if want is None:
                skipped.append((cx, cz))
                continue
            compared += 1
            diff = 0
            for sec in range(sections):
                y0 = min_y + sec * 16
                if y0 + 16 <= band[0] or y0 >= band[1]:
                    continue
                for i in range(4096):
                    g = got[sec * 4096 + i]
                    w = want[sec * 4096 + i]
                    if g == w:
                        continue
                    diff += 1
                    pairs[(g, w)] += 1
                    if len(detail) < want_n:
                        y = y0 + (i >> 8)
                        detail.append((cx * 16 + (i & 15), y, cz * 16 + ((i >> 4) & 15),
                                       names.get(g, str(g)), names.get(w, str(w))))
                    if (g in airlike) != (w in airlike):
                        layers["carver"] += 1
                    elif sec >= 20:
                        layers["surface"] += 1
                    else:
                        layers["deep"] += 1
            per_chunk[(cx, cz)] = diff
            total += diff
    print(f"chunks {n}x{n}  band y={band[0]}..{band[1] - 1}  differing cells: {total}")
    print("per chunk:", " ".join(str(v) for v in per_chunk.values()))
    if skipped:
        print(f"skipped {len(skipped)} partially generated chunks: {skipped}")
    print("layers:", dict(layers))
    for x, y, z, gn, wn in detail:
        print(f"  cell {x},{y},{z}: got {gn} want {wn}")
    if pairs:
        print("top pairs (got, want) x count:")
        for (g, w), c in pairs.most_common(15):
            print(f"  {g:6d} != {w:6d}  x{c}")
    # A subset cannot establish parity for the requested region, even if all
    # chunks that happened to finish match. The skipped coordinates above are
    # evidence of an incomplete reference, never an exclusion from the claim.
    if skipped or compared == 0:
        print(f"INCOMPLETE COMPARISON: {compared}/{n*n} full chunks compared; skipped={skipped}", file=sys.stderr)
        return 2
    return 1 if total else 0


if __name__ == "__main__":
    sys.exit(main())
