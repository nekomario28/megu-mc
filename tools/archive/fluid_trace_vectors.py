#!/usr/bin/env python3
"""Extract bounded, ordered native fluid-tick scenes from a 7x7 trace.

Only the two observed cave springs and their nearby updates are selected.
Incomplete chunks and unsupported state predicates fail rather than default.
"""
import argparse
from pathlib import Path
import re

from carver_diff import load_states, vanilla_chunk

ROOT = Path(__file__).resolve().parents[1]
RUN = re.compile(r"TICKTRACE run (-?\d+) (-?\d+) (-?\d+) type=(ep[hk]\$[ab])@")
ANY_RUN = re.compile(r"TICKTRACE run ")
WRITE = re.compile(r"RUNTIME (-?\d+) (-?\d+) (-?\d+) state=(\d+) wrote=(true|false) stack=(.*)")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("world", type=Path)
    parser.add_argument("log", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    table = load_states()
    chunks = {}
    for z in range(30, 37):
        for x in range(-66, -59):
            cells = vanilla_chunk(str(args.world), x, z, table)
            if cells is None or min(cells) < 0:
                raise ValueError(f"incomplete chunk {(x,z)}")
            chunks[x, z] = cells
    predicates = (ROOT / "assets/block_predicates_1211.bin").read_bytes()

    def get(x, y, z):
        return chunks[x >> 4, z >> 4][(y + 64) * 256 + (z & 15) * 16 + (x & 15)]

    def classify(st):
        if st == 0: return "A"
        if 80 <= st <= 95: return "W" + str(st - 80)
        if 96 <= st <= 111: return "L" + str(st - 96)
        if st >= len(predicates): raise ValueError(f"unmapped state {st}")
        return "S" if predicates[st] & 1 else "U"

    rows = []
    current = None
    writes = []
    started = False

    def finish():
        if current is not None:
            rows.append(current + "\t" + ";".join(writes))

    with args.log.open() as source:
        for line in source:
            if "RESUME_START" in line:
                started = True
                continue
            if not started: continue
            if ANY_RUN.search(line):
                finish()
                current = None
                writes = []
                m = RUN.search(line)
                if m:
                    x, y, z = map(int, m.group(1, 2, 3))
                    near_lava = abs(x + 1024) <= 8 and abs(z - 512) <= 8 and -26 <= y <= -12
                    near_water = abs(x + 989) <= 8 and abs(z - 512) <= 8 and 3 <= y <= 16
                    if near_lava or near_water:
                        kind = "W" if m[4].startswith("epk") else "L"
                        is_source = "1" if m[4].endswith("$b") else "0"
                        cells = []
                        for dy in range(-1, 2):
                            for dx in range(-6, 7):
                                for dz in range(-6, 7):
                                    cells.append(classify(get(x + dx, y + dy, z + dz)))
                        current = "\t".join((str(x), str(y), str(z), kind, is_source, ",".join(cells)))
                continue
            m = WRITE.search(line)
            if m and m[5] == "true":
                x, y, z, state = map(int, m.group(1, 2, 3, 4))
                if current is not None and "epc." in m[6]:
                    writes.append(f"{x},{y},{z}:{classify(state)}")
                chunks[x >> 4, z >> 4][(y + 64) * 256 + (z & 15) * 16 + (x & 15)] = state
    finish()
    if len(rows) != 28:
        raise ValueError(f"expected 28 observed tick scenes; found {len(rows)}")
    args.output.write_text("\n".join(rows) + "\n")
    print(f"captured {len(rows)} native fluid tick scenes")


if __name__ == "__main__":
    main()
