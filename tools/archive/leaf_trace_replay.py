#!/usr/bin/env python3
"""Check native scheduled-leaf calculations against observed runtime writes.

The centre chunk has a one-block halo within the traced 3x3. This diagnostic
replays actual writes from the trace; it does not model the port scheduler.
"""
import argparse
import json
from pathlib import Path
import re

from carver_diff import CELLS, load_states, vanilla_chunk

ROOT = Path(__file__).resolve().parents[1]
DRAIN = re.compile(r"TICKTRACE run (-?\d+) (-?\d+) (-?\d+) type=Block\{(minecraft:[^}]*_leaves)\}")
WRITE = re.compile(r"RUNTIME (-?\d+) (-?\d+) (-?\d+) state=(\d+) wrote=(true|false) stack=(.*)")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("frozen_world", type=Path)
    parser.add_argument("trace", type=Path)
    args = parser.parse_args()
    table = load_states()
    reverse = {st: key for key, st in table.items()}
    properties = {b["name"]: [p["name"] for p in b["states"]]
                  for b in json.loads((ROOT / "assets/blocks_1211.json").read_text())}
    chunks = {}
    for z in range(32, 35):
        for x in range(-64, -61):
            values = vanilla_chunk(str(args.frozen_world), x, z, table)
            if values is None or len(values) != CELLS or min(values) < 0:
                raise ValueError(f"incomplete chunk {x},{z}")
            chunks[(x, z)] = values

    def get(x, y, z):
        if y < -64 or y > 319:
            raise ValueError(f"out of height range {x},{y},{z}")
        return chunks[(x >> 4, z >> 4)][(y + 64) * 256 + (z & 15) * 16 + (x & 15)]

    def put(x, y, z, st):
        chunks[(x >> 4, z >> 4)][(y + 64) * 256 + (z & 15) * 16 + (x & 15)] = st

    def distance(st):
        name, vals = reverse[st]
        if name in ("oak_log", "birch_log", "oak_wood", "birch_wood",
                    "stripped_oak_log", "stripped_birch_log", "stripped_oak_wood", "stripped_birch_wood"):
            return 0
        props = dict(zip(properties[name], vals))
        return int(props["distance"]) if name.endswith("_leaves") else 7

    expected = None
    count = changed = 0
    started = False
    with args.trace.open() as source:
        for line in source:
            if "RESUME_START" in line:
                started = True
                continue
            if not started:
                continue
            event = DRAIN.search(line)
            if event:
                x, y, z = map(int, event.group(1, 2, 3))
                expected = None
                if not (-1008 <= x <= -993 and 528 <= z <= 543 and -63 <= y <= 318):
                    continue
                st = get(x, y, z)
                name, vals = reverse[st]
                if "minecraft:" + name != event[4]:
                    continue
                ps = properties[name]
                values = dict(zip(ps, vals))
                d = min(7, *(distance(get(x + dx, y + dy, z + dz)) + 1 for dx, dy, dz in
                             [(0,-1,0),(0,1,0),(0,0,-1),(0,0,1),(-1,0,0),(1,0,0)]))
                values["distance"] = str(d)
                expected = ((x, y, z), table[(name, tuple(values[p] for p in ps))])
                continue
            write = WRITE.search(line)
            if not write:
                continue
            x, y, z, st = map(int, write.group(1, 2, 3, 4))
            if expected is not None and (x, y, z) == expected[0] and "dki.a(" in write[6]:
                if st != expected[1]:
                    raise ValueError(f"leaf result mismatch at {(x,y,z)}: actual={st}, predicted={expected[1]}")
                count += 1
                changed += write[5] == "true"
                expected = None
            if write[5] == "true":
                put(x, y, z, st)
    if count == 0:
        raise ValueError("no leaf ticks matched; check capture and trace")
    print(json.dumps({"checked_native_leaf_writes": count, "changed": changed,
                      "region": "central chunk, using actual traced 3x3 writes",
                      "scope": "native leaf calculations only; not port runtime"}, indent=2))


if __name__ == "__main__":
    main()
