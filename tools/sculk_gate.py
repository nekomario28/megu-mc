#!/usr/bin/env python3
"""Compare the sculk cells of a saved vanilla region against the port's worldgen dump.

  tools/sculk_gate.py <region_world> <dump.bin> <n> <x0> <z0> [--require-match]

Why: `sculk_patch` (and the sculk vein spread) cannot be driven through the
feature-level oracle - `SculkVeinBlock`'s shape update asks the level for a
`ChunkAccess`, which a scripted level deliberately cannot answer
(`evidence/gameplay/feature-parity/native-oracle.md`). The whole-chunk capture is the
only route, so this is the gate for that route.

It compares *only* the cells whose reference or port block is one of the sculk family
(`sculk`, `sculk_vein`, `sculk_catalyst`, `sculk_shrieker`), because the rest of the
region carries differences that belong to other layers (leaf distance, deepslate/stone
material, carver air; `tools/carver_diff.py` reports those in full). Narrowing the
comparison is what makes the claim specific: "every sculk cell vanilla placed in these
chunks is placed by the port with the same state", not "the region matches".

A capture with no sculk at all is an error, not a pass: a region without the feature
cannot prove anything about it. `--require-match` turns the missing/mismatching cells
into a non-zero exit; without it the tool is a reporter.
"""
import sys
from collections import Counter
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from carver_diff import load_names, vanilla_chunk

# `carver_diff.load_names` holds names without the namespace prefix ("sculk", not
# "minecraft:sculk"); the family is matched on those.
SCULK = {"sculk", "sculk_vein", "sculk_catalyst", "sculk_shrieker"}


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if len(args) < 5:
        raise SystemExit(__doc__)
    world, dump, n, x0, z0 = args[0], Path(args[1]), int(args[2]), int(args[3]), int(args[4])
    require = "--require-match" in sys.argv

    from carver_diff import load_states

    names = load_names()
    states = load_states()
    raw = dump.read_bytes()
    cell_count = len(raw) // (n * n * 4)  # 4 bytes per state id
    if len(raw) != n * n * cell_count * 4 or cell_count % 4096:
        raise SystemExit(f"dump length {len(raw)} is not n*n*sections*4096*4")

    total_ref = Counter()
    total_port = Counter()
    missing = []      # vanilla placed it, the port did not
    extra = []        # the port placed it, vanilla did not
    wrong = []        # both placed something of the family, different state
    compared = 0
    for dz in range(n):
        for dx in range(n):
            cx, cz = x0 + dx, z0 + dz
            want = vanilla_chunk(world, cx, cz, states, min_y=-64, height=384)
            if want is None:
                continue
            compared += 1
            cells = raw[(dz * n + dx) * cell_count * 4:(dz * n + dx + 1) * cell_count * 4]
            got = [int.from_bytes(cells[i * 4:i * 4 + 4], "little") for i in range(cell_count)]
            for sec in range(cell_count // 4096):
                y0 = -64 + sec * 16
                for i in range(4096):
                    g, w = got[sec * 4096 + i], want[sec * 4096 + i]
                    gn, wn = names.get(g, str(g)), names.get(w, str(w))
                    gs, ws = gn in SCULK, wn in SCULK
                    if not (gs or ws):
                        continue
                    x, y, z = cx * 16 + (i & 15), y0 + (i >> 8), cz * 16 + ((i >> 4) & 15)
                    if ws:
                        total_ref[wn] += 1
                    if gs:
                        total_port[gn] += 1
                    if ws and not gs:
                        missing.append((x, y, z, wn, gn))
                    elif gs and not ws:
                        extra.append((x, y, z, gn, wn))
                    elif gn != wn:
                        wrong.append((x, y, z, wn, gn))

    print(f"chunks compared: {compared}/{n * n}")
    print("reference sculk cells:", dict(total_ref), "total", sum(total_ref.values()))
    print("port sculk cells:     ", dict(total_port), "total", sum(total_port.values()))
    print(f"missing {len(missing)}  extra {len(extra)}  wrong-state {len(wrong)}")
    for label, rows in (("missing", missing), ("extra", extra), ("wrong-state", wrong)):
        for x, y, z, wn, gn in rows[:8]:
            print(f"  {label}: {x},{y},{z} vanilla {wn} port {gn}")
    if sum(total_ref.values()) == 0:
        print("sculk gate: FAIL - the reference has no sculk cells, so it proves nothing")
        return 2
    bad = len(missing) + len(extra) + len(wrong)
    if require and bad:
        print(f"sculk gate: FAIL - {bad} sculk cells disagree")
        return 1
    print("sculk gate: " + ("PASS" if bad == 0 else f"REPORT ONLY ({bad} disagreements)"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
