#!/usr/bin/env python3
"""Dump one saved vanilla chunk as canonical block states, for whole-chunk comparison.

  tools/terrain_chunk_dump.py <world_dir> <cx> <cz> [--out path.tsv] [--all]

Why this exists: the End and Nether terrain corpora were captured from a server save;
the Overworld has none, and its `terrain_cell` mixes biomes, the aquifer and the material
rule, so the other dimensions' parity does not transfer. The region file on disk already
holds what a server produced, so this reads it back rather than starting another server.

Output is name-based rather than numeric on purpose. A saved palette entry carries the
state's name and its property values, while the numeric id space the End/Nether fixtures
use (`assets/blocks_1211.json`: air 0, end_stone 7415) has no documented
property-to-id order. Canonical names survive an id renumbering; ids do not.

Every non-air cell is emitted as `<x>\\t<y>\\t<z>\\t<state>`, sorted by y then z then x, with
the dimension's `min_y`/`height` and the chunk's own counts in a `#` header. `--all` also
emits air cells (much larger, occasionally useful).
"""
import argparse
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from vanilla_region_blocks import chunk_bytes, parse_nbt  # noqa: E402


def find(node, key):
    if isinstance(node, dict):
        if key in node:
            return node[key]
        for value in node.values():
            found = find(value, key)
            if found is not None:
                return found
    return None


def state_name(palette_entry):
    name = palette_entry.get("Name")
    props = palette_entry.get("Properties") or {}
    if not props:
        return name
    joined = ",".join(f"{k}={props[k]}" for k in sorted(props))
    return f"{name}[{joined}]"


def cells(world, cx, cz, include_air):
    nbt = parse_nbt(chunk_bytes(world, cx, cz))[0]
    min_y = int(find(nbt, "yPos") if find(nbt, "yPos") is not None else -4) * 16
    rows = []
    for section in find(nbt, "sections") or []:
        if "block_states" not in section:
            continue
        states = section["block_states"]
        palette = [state_name(entry) for entry in states.get("palette") or []]
        if not palette:
            continue
        data = states.get("data")
        bits = max(4, (len(palette) - 1).bit_length())
        per = 64 // bits
        mask = (1 << bits) - 1
        sy = int(section["Y"])
        for i in range(4096):
            if data:
                word = data[i // per] & 0xFFFFFFFFFFFFFFFF
                value = (word >> ((i % per) * bits)) & mask
            else:
                value = 0
            state = palette[value] if value < len(palette) else "<bad-palette>"
            air = state in ("minecraft:air", "minecraft:cave_air", "minecraft:void_air")
            if air and not include_air:
                continue
            x = i & 15
            z = (i >> 4) & 15
            y = sy * 16 + (i >> 8)
            rows.append((y, z, x, state))
    rows.sort()
    return min_y, rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("world")
    ap.add_argument("cx", type=int)
    ap.add_argument("cz", type=int)
    ap.add_argument("--out")
    ap.add_argument("--all", action="store_true")
    args = ap.parse_args()
    min_y, rows = cells(args.world, args.cx, args.cz, args.all)
    above = max((y for y, _, _, _ in rows), default=min_y)
    height = above - min_y + 1
    lines = [f"# chunk {args.cx},{args.cz} min_y {min_y} height {height} cells {len(rows)}"]
    for y, z, x, state in rows:
        lines.append(f"{x}\t{y}\t{z}\t{state}")
    text = "\n".join(lines) + "\n"
    if args.out:
        Path(args.out).write_text(text)
        print(f"wrote {len(rows)} cells to {args.out}")
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
