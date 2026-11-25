#!/usr/bin/env python3
"""Compare the cells of one isolated placed feature against a vanilla world.

  tools/ore_diff.py <dump.bin> <world_dir> <block-name> [--n 6] [--x0 0] [--z0 0]

`tools/ore_fixture.sh` zeroes every other ore and disk placement so that exactly one
feature can place, in both worlds. Then the two cell sets for that feature's block
compare directly, and `origin` (the feature's placement position, from
`--trace-origins`) explains a shape difference that a cell count alone hides.

Also prints how many coarse clusters each side's cells form (`--clusters`), because
"same count, different blobs" and "fewer cells, same blobs" are different bugs.
"""
import json
import sys
from collections import Counter, deque
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


def clumps(cells):
    """Connected components (6-neighbour) of a cell set, largest first."""
    todo = set(cells)
    out = []
    while todo:
        start = todo.pop()
        q = deque([start])
        n = 1
        while q:
            x, y, z = q.popleft()
            for d in ((1, 0, 0), (-1, 0, 0), (0, 1, 0), (0, -1, 0), (0, 0, 1), (0, 0, -1)):
                p = (x + d[0], y + d[1], z + d[2])
                if p in todo:
                    todo.discard(p)
                    q.append(p)
                    n += 1
        out.append(n)
    return sorted(out, reverse=True)


def main():
    if len(sys.argv) < 4:
        raise SystemExit(__doc__)
    dump, world, block = sys.argv[1], sys.argv[2], sys.argv[3]
    n, x0, z0 = 6, 0, 0
    if "--n" in sys.argv:
        n = int(sys.argv[sys.argv.index("--n") + 1])
    if "--x0" in sys.argv:
        x0 = int(sys.argv[sys.argv.index("--x0") + 1])
    if "--z0" in sys.argv:
        z0 = int(sys.argv[sys.argv.index("--z0") + 1])
    names = load_names()
    raw = open(dump, "rb").read()
    ours, theirs = set(), set()
    compared = 0
    for dz in range(n):
        for dx in range(n):
            cx, cz = x0 + dx, z0 + dz
            # the reference's status decides first: a partially generated chunk holds
            # no features, so counting *our* cells for it would read as "this port
            # placed thousands of cells vanilla does not have"
            chunk, _ = parse_nbt(chunk_bytes(world, cx, cz))
            if chunk.get("Status") not in ("minecraft:full", "full"):
                continue
            compared += 1
            cells = raw[(dz * n + dx) * CELLS * 4:(dz * n + dx + 1) * CELLS * 4]
            got = [int.from_bytes(cells[i * 4:i * 4 + 4], "little") for i in range(CELLS)]
            for i, v in enumerate(got):
                if names.get(v) == block:
                    ours.add((cx * 16 + (i & 15), (i >> 8) - 64, cz * 16 + ((i >> 4) & 15)))
            for s in chunk.get("sections", []):
                idx = s.get("Y", 0) - MIN_Y // 16
                bs = s.get("block_states")
                if not (0 <= idx < SECTIONS) or not bs:
                    continue
                pal = bs.get("palette", [])
                if not pal:
                    continue
                ids = [p.get("Name", "minecraft:air").replace("minecraft:", "") for p in pal]
                if len(pal) == 1:
                    hit = ids[0] == block
                    picks = [0] * 4096 if hit else []
                else:
                    bits = max(4, (len(pal) - 1).bit_length())
                    data = bs.get("data") or []
                    per = 64 // bits
                    picks = []
                    for i in range(4096):
                        w = data[i // per] & 0xFFFFFFFFFFFFFFFF
                        picks.append((w >> ((i % per) * bits)) & ((1 << bits) - 1))
                for i, p in enumerate(picks):
                    if ids[p] == block:
                        y0 = MIN_Y + idx * 16
                        theirs.add((cx * 16 + (i & 15), y0 + (i >> 8), cz * 16 + ((i >> 4) & 15)))
    if compared == 0:
        raise SystemExit("NOTHING COMPARED: every requested chunk is incomplete in the reference")
    shared = ours & theirs
    print(f"{block}: ours {len(ours)} vanilla {len(theirs)} shared {len(shared)} "
          f"ours-only {len(ours - theirs)} vanilla-only {len(theirs - ours)}")
    if ours or theirs:
        print(f"  clusters  ours {clumps(ours)[:8]}  vanilla {clumps(theirs)[:8]}")
    ys = Counter(y for _, y, _ in (theirs - ours))
    if ys:
        print(f"  vanilla-only by y: {dict(sorted(ys.items())[:6])} ... {dict(sorted(ys.items())[-4:])}")
    return 1 if (ours != theirs) else 0


if __name__ == "__main__":
    sys.exit(main())
