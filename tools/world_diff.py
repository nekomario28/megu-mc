#!/usr/bin/env python3
"""Diff two saved vanilla worlds chunk by chunk - the reference's own noise floor.

  tools/world_diff.py <worldA> <worldB> [--n 4] [--x0 0] [--z0 0]

Vanilla decorates chunks in parallel, so two runs of the same fixture are not identical;
every difference below that floor is noise, not an implementation bug. This compares the
two references directly, reports the floor next to the chunk count, and lists the pairs so
the noise's *shape* (leaf presence? fluid? ore?) is visible.

Chunks that are not `full` in both worlds are skipped on both sides, and a run that
compares nothing fails rather than printing a zero.
"""
import json
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
from vanilla_region_blocks import chunk_bytes, parse_nbt  # noqa: E402

MIN_Y = -64
HEIGHT = 384
SECTIONS = HEIGHT // 16
CELLS = SECTIONS * 4096


def load_names():
    names = {}
    for block in json.loads((ROOT / "assets" / "blocks_1211.json").read_text()):
        for state in range(block["minStateId"], block["maxStateId"] + 1):
            names[state] = block["name"]
    return names


def load_states():
    table = {}
    for block in json.loads((ROOT / "assets" / "blocks_1211.json").read_text()):
        props = block["states"]
        base = block["minStateId"]
        for state in range(base, block["maxStateId"] + 1):
            off = state - base
            key, mult = [], 1
            for p in reversed(props):
                vals = p.get("values") or ["true", "false"]
                key.append(vals[(off // mult) % len(vals)])
                mult *= len(vals)
            table[(block["name"], tuple(reversed(key)))] = state
    return table


def chunk_states(world, cx, cz, table):
    chunk, _ = parse_nbt(chunk_bytes(world, cx, cz))
    if chunk.get("Status") not in ("minecraft:full", "full"):
        return None
    out = [-1] * CELLS
    for s in chunk.get("sections", []):
        idx = s.get("Y", 0) - MIN_Y // 16
        bs = s.get("block_states")
        if not (0 <= idx < SECTIONS) or not bs:
            continue
        palette = bs.get("palette", [])
        if not palette:
            continue
        ids = []
        for p in palette:
            name = p.get("Name", "minecraft:air").replace("minecraft:", "")
            props = tuple(sorted((p.get("Properties") or {}).items()))
            ids.append(table.get((name, props)))
        data = bs.get("data") or []
        if len(palette) == 1:
            for i in range(4096):
                out[idx * 4096 + i] = ids[0]
            continue
        bits = max(4, (len(palette) - 1).bit_length())
        per = 64 // bits
        mask = (1 << bits) - 1
        for i in range(4096):
            w = data[i // per] & 0xFFFFFFFFFFFFFFFF
            pick = (w >> ((i % per) * bits)) & mask
            if pick < len(ids):
                out[idx * 4096 + i] = ids[pick]
    return out


def main():
    if len(sys.argv) < 3:
        raise SystemExit(__doc__)
    a, b = sys.argv[1], sys.argv[2]
    n, x0, z0 = 4, 0, 0
    if "--n" in sys.argv:
        n = int(sys.argv[sys.argv.index("--n") + 1])
    if "--x0" in sys.argv:
        x0 = int(sys.argv[sys.argv.index("--x0") + 1])
    if "--z0" in sys.argv:
        z0 = int(sys.argv[sys.argv.index("--z0") + 1])
    names, table = load_names(), load_states()
    total, compared, per = 0, 0, Counter()
    pairs = Counter()
    for dz in range(n):
        for dx in range(n):
            cx, cz = x0 + dx, z0 + dz
            A = chunk_states(a, cx, cz, table)
            B = chunk_states(b, cx, cz, table)
            if A is None or B is None:
                continue
            compared += 1
            d = 0
            for i, (x, y) in enumerate(zip(A, B)):
                if x != y:
                    d += 1
                    pairs[(names.get(x, str(x)), names.get(y, str(y)))] += 1
            per[(cx, cz)] = d
            total += d
    if compared == 0:
        print("NOTHING COMPARED: no chunk is full in both worlds", file=sys.stderr)
        return 2
    print(f"two reference runs, {compared} chunks compared: noise floor {total} cells")
    print("per chunk:", " ".join(str(v) for v in per.values()))
    for (x, y), c in pairs.most_common(10):
        print(f"  {x:>20} != {y:<20} {c}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
