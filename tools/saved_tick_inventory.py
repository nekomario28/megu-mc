#!/usr/bin/env python3
"""Inventory serialized block/fluid ticks in a complete saved chunk rectangle.

The NBT delay is reported as stored; it is not necessarily an absolute game
time, a future deadline, or evidence that the chunk was eligible to tick.
"""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path

from vanilla_region_blocks import chunk_bytes, parse_nbt


def inventory(world, x0, z0, width, height, include_entries=False):
    if width < 1 or height < 1:
        raise ValueError("chunk rectangle must be nonempty")
    counts = {"block_ticks": Counter(), "fluid_ticks": Counter()}
    delays = {"block_ticks": Counter(), "fluid_ticks": Counter()}
    seen = set()
    duplicates = 0
    entries = []
    region_paths = set()
    for z in range(z0, z0 + height):
        for x in range(x0, x0 + width):
            chunk, _ = parse_nbt(chunk_bytes(str(world), x, z))
            if chunk.get("Status") not in ("minecraft:full", "full"):
                raise ValueError(f"chunk ({x},{z}) is not FULL")
            if (chunk.get("xPos"), chunk.get("zPos")) != (x, z):
                raise ValueError(f"chunk coordinates do not match ({x},{z})")
            region_paths.add(world / "region" / f"r.{x >> 5}.{z >> 5}.mca")
            for kind in counts:
                ticks = chunk.get(kind)
                if not isinstance(ticks, list):
                    raise ValueError(f"chunk ({x},{z}) missing {kind} list")
                for tick in ticks:
                    name = tick["i"]
                    tx, ty, tz = tick["x"], tick["y"], tick["z"]
                    if (tx >> 4, tz >> 4) != (x, z):
                        raise ValueError(f"tick at ({tx},{ty},{tz}) in wrong chunk")
                    key = (kind, name, tx, ty, tz)
                    if key in seen:
                        duplicates += 1
                    seen.add(key)
                    counts[kind][name] += 1
                    delays[kind][str(tick["t"])] += 1
                    if include_entries:
                        entries.append({"chunk": [x, z], "kind": kind,
                                        "entry": tick})
    result = {
        "chunk_rectangle": [x0, z0, width, height],
        "full_chunks": width * height,
        "tick_types": {kind: dict(sorted(values.items())) for kind, values in counts.items()},
        "stored_delays": {kind: dict(sorted(values.items(), key=lambda v: int(v[0])))
                          for kind, values in delays.items()},
        "duplicate_type_and_position_entries": duplicates,
        "region_sha256": {path.name: hashlib.sha256(path.read_bytes()).hexdigest()
                          for path in sorted(region_paths)},
    }
    if include_entries:
        # Chunk traversal here is only the export order; no cross-chunk execution
        # order can be recovered from the saved lists.
        result["entries"] = entries
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("world", type=Path)
    parser.add_argument("x", type=int)
    parser.add_argument("z", type=int)
    parser.add_argument("width", type=int)
    parser.add_argument("height", type=int)
    parser.add_argument("--include-entries", action="store_true",
                        help="include raw entries, preserving each chunk list's order")
    args = parser.parse_args()
    print(json.dumps(inventory(args.world, args.x, args.z, args.width, args.height,
                               args.include_entries), indent=2))


if __name__ == "__main__":
    main()
