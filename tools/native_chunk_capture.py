#!/usr/bin/env python3
"""Capture one native 1.21.1 chunk as the canonical-state TSV the parity tests read.

    tools/native_chunk_capture.py <world> <cx> <cz> [--preset overworld|nether|end]
                                  [--out tests/data/<name>.tsv]

Why this exists: a fixture that pins a whole chunk state-for-state has to carry every
property (`oak_fence[east=true,...]`), not just block names, or it cannot see the
connection bits, stair facings or waterlogging this port keeps getting wrong. This
script is the *producer* for those fixtures, so the committed TSV's provenance is a
command instead of a paragraph: it reads the chunk out of the world's region file, keeps
the palette's canonical `name[prop=value,...]` form, drops `minecraft:air`, and writes
the rows sorted by (local x, y, local z).

The output format is the same one `MEGU_WG_DUMP_CANON` writes from the port, so a
test can compare the two maps cell for cell:

    # chunk <cx>,<cz> min_y <min_y> height <tallest content y - min_y + 1> cells <n>
    <x>\\t<y>\\t<z>\\t<state>

Coordinates are absolute, the same ones `MEGU_WG_DUMP_CANON` prints, and the cells
are the states the pinned server produced for that chunk; nothing here is copied from
the jar.
"""
import argparse
import gzip
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
import chunk_triage  # noqa: E402

PRESET_WINDOW = {"overworld": (-64, 384), "nether": (0, 128), "end": (0, 128)}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("world")
    ap.add_argument("cx", type=int)
    ap.add_argument("cz", type=int)
    ap.add_argument("--preset", default="overworld", choices=sorted(PRESET_WINDOW))
    ap.add_argument("--min-y", type=int, help="Override the preset's window base y (e.g. the Nether's 0..256 dimension window instead of its 0..128 noise window)")
    ap.add_argument("--height", type=int, help="Override the preset's window height")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    min_y, height = PRESET_WINDOW[args.preset]
    if args.min_y is not None:
        min_y = args.min_y
    if args.height is not None:
        height = args.height
    world = chunk_triage.resolve_dimension_world(Path(args.world), preset=args.preset)
    chunk = chunk_triage.load_native_chunk(
        world, args.cx, args.cz, min_y_override=min_y, height_override=height
    )
    rows = []
    top = min_y - 1
    for idx, state in enumerate(chunk["cells"]):
        if state == "minecraft:air":
            continue
        sy, rest = divmod(idx, 4096)
        ly, rest = divmod(rest, 256)
        lz, lx = divmod(rest, 16)
        y = min_y + sy * 16 + ly
        rows.append((args.cx * 16 + lx, y, args.cz * 16 + lz, state))
        top = max(top, y)
    rows.sort()
    lines = [
        f"# chunk {args.cx},{args.cz} min_y {min_y} height {top - min_y + 1} cells {len(rows)}"
    ]
    lines += [f"{x}\t{y}\t{z}\t{state}" for x, y, z, state in rows]
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    body = ("\n".join(lines) + "\n").encode()
    if out.suffix == ".gz":
        out.write_bytes(gzip.compress(body, 9))
    else:
        out.write_bytes(body)
    print(f"{out}: {len(rows)} cells (world {world})")


if __name__ == "__main__":
    main()
