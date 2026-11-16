#!/usr/bin/env python3
"""Compare the sculk cells of a saved vanilla region against the port's worldgen dump.

  tools/sculk_gate.py <region_world> <dump.bin> <n> <x0> <z0> [--require-match] [--json-out PATH]

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

Same block name is not the same state. `sculk_vein` carries `up/down/north/...` and
`waterlogged`, `sculk_catalyst` carries `bloom`; comparing names alone silently passed
every one of those property differences, so the comparison is on the canonical block
state id from `assets/blocks_1211.json` (the ordering the reference and `carver_diff`
already translate through, so both sides are the same keyspace).

A capture with no sculk at all is an error, not a pass: a region without the feature
cannot prove anything about it. So is a request this gate cannot honour - a non-positive
`n`, a dump that is not `n*n` chunks of the full overworld height (24 sections), a chunk
that is missing or not `full` in the saved region (a partial grid would silently compare
fewer chunks than asked), or a serialized state id the block table cannot map. All of
those are invalid inputs and exit 2, never a traceback and never PASS.

Exit codes:
  0  the compared cells agree (or report-only mode, see --require-match)
  1  a sculk cell disagrees and --require-match was passed
  2  invalid input: bad n/dump/world, missing or non-full chunk, unmappable state id,
     or a reference with no sculk cells to prove anything with

`--json-out` writes the counts this run measured, so a caller does not have to parse the
human report. This gate still only covers the sculk family in existing saved artifacts:
it is not a whole-world parity proof and it captures nothing itself.
"""
import hashlib
import json
import sys
from collections import Counter
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from carver_diff import load_names, load_states, vanilla_chunk

# `carver_diff.load_names` holds names without the namespace prefix ("sculk", not
# "minecraft:sculk"); the family is matched on those.
SCULK = {"sculk", "sculk_vein", "sculk_catalyst", "sculk_shrieker"}
# The saved region this gate reads: the Overworld's full dimension height. A dump is
# `n*n` chunks of this many state cells, 4 bytes each, chunk-major from (x0, z0).
MIN_Y = -64
HEIGHT = 384
SECTIONS = HEIGHT // 16
CELLS = SECTIONS * 4096
BYTES_PER_CELL = 4


class Usage(Exception):
    """A request this gate cannot answer (exit 2); no report is produced for it."""

    def __init__(self, message, doc=False):
        super().__init__(message)
        self.doc = doc


def parse_args(argv):
    positional = []
    require = False
    json_out = None
    i = 0
    while i < len(argv):
        arg = argv[i]
        if arg == "--require-match":
            require = True
            i += 1
        elif arg == "--json-out":
            if i + 1 >= len(argv):
                raise Usage("--json-out needs a path")
            json_out = Path(argv[i + 1])
            i += 2
        elif arg in ("-h", "--help"):
            raise Usage("", doc=True)
        elif arg.startswith("--"):
            raise Usage(f"unknown option {arg}")
        else:
            positional.append(arg)
            i += 1
    if len(positional) != 5:
        raise Usage(f"expected 5 arguments (world dump n x0 z0), got {len(positional)}", doc=True)
    world, dump = positional[0], Path(positional[1])
    try:
        n, x0, z0 = int(positional[2]), int(positional[3]), int(positional[4])
    except ValueError as exc:
        raise Usage(f"n/x0/z0 must be integers: {exc}") from exc
    if n <= 0:
        raise Usage(f"n must be positive, got {n}")
    return world, dump, n, x0, z0, require, json_out


def compare(world, raw, n, x0, z0, names, states):
    """Count sculk-family cells; raises Usage for any input this gate cannot read."""
    cell_count = len(raw) // (n * n * BYTES_PER_CELL)
    if len(raw) != n * n * cell_count * BYTES_PER_CELL:
        raise Usage(f"dump length {len(raw)} is not n*n*chunks*{BYTES_PER_CELL} bytes")
    if cell_count != CELLS:
        raise Usage(
            f"dump holds {cell_count} cells per chunk; this gate needs the full "
            f"{SECTIONS}-section overworld height ({CELLS} cells, {HEIGHT} blocks from y={MIN_Y})"
        )

    # A section the saved chunk does not carry is air; resolve the id from the same
    # table instead of assuming 0.
    air = next((sid for sid, name in names.items() if name == "air"), 0)

    total_ref = Counter()
    total_port = Counter()
    missing = []      # vanilla placed it, the port did not
    extra = []        # the port placed it, vanilla did not
    wrong = []        # both placed sculk, different canonical state
    incomplete = []
    compared = 0
    for dz in range(n):
        for dx in range(n):
            cx, cz = x0 + dx, z0 + dz
            try:
                want = vanilla_chunk(world, cx, cz, states, min_y=MIN_Y, height=HEIGHT)
            except (SystemExit, ValueError) as exc:
                raise Usage(f"cannot read chunk ({cx},{cz}) from {world}: {exc}") from exc
            if want is None:
                # `vanilla_chunk` returns None for any status other than full; skipping it
                # would let a partial grid PASS on the chunks that happen to be complete.
                incomplete.append((cx, cz))
                continue
            compared += 1
            cells = raw[(dz * n + dx) * cell_count * BYTES_PER_CELL:(dz * n + dx + 1) * cell_count * BYTES_PER_CELL]
            got = memoryview(cells).cast("I") if sys.byteorder == "little" else None
            for sec in range(SECTIONS):
                y0 = MIN_Y + sec * 16
                for i in range(4096):
                    off = sec * 4096 + i
                    g = got[off] if got is not None else int.from_bytes(cells[off * 4:off * 4 + 4], "little")
                    gn = names.get(g)
                    if gn is None:
                        raise Usage(f"port dump has unmappable block state id {g} at chunk ({cx},{cz}) cell {off}")
                    w = want[off]
                    if w == -1:
                        w = air  # a section the saved chunk does not carry is air
                    elif w == -2 or w not in names:
                        raise Usage(
                            f"{world} chunk ({cx},{cz}) has a block state the table cannot map "
                            f"(cell {off}); the reference is not comparable"
                        )
                    wn = names[w]
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
                    elif g != w:
                        wrong.append((x, y, z, wn, w, gn, g))
    if incomplete:
        raise Usage(
            f"{len(incomplete)}/{n * n} chunk(s) missing or not full in {world}: {incomplete}; "
            "this gate compares every chunk of the requested grid"
        )
    return {
        "compared": compared,
        "reference": total_ref,
        "port": total_port,
        "missing": missing,
        "extra": extra,
        "wrong": wrong,
    }


def main():
    argv = sys.argv[1:]
    try:
        world, dump, n, x0, z0, require, json_out = parse_args(argv)
        try:
            raw = dump.read_bytes()
        except OSError as exc:
            raise Usage(f"cannot read dump {dump}: {exc}") from exc
        names = load_names()
        states = load_states()
        res = compare(world, raw, n, x0, z0, names, states)
    except Usage as exc:
        message = str(exc)
        if message:
            print(f"sculk_gate: INVALID INPUT: {message}", file=sys.stderr)
        if exc.doc:
            print(__doc__)
        return 2

    total_ref, total_port = res["reference"], res["port"]
    bad = len(res["missing"]) + len(res["extra"]) + len(res["wrong"])
    print(f"chunks compared: {res['compared']}/{n * n}")
    print("reference sculk cells:", dict(total_ref), "total", sum(total_ref.values()))
    print("port sculk cells:     ", dict(total_port), "total", sum(total_port.values()))
    print(f"missing {len(res['missing'])}  extra {len(res['extra'])}  wrong-state {len(res['wrong'])}")
    for x, y, z, wn, gn in res["missing"][:8]:
        print(f"  missing: {x},{y},{z} vanilla {wn} port {gn}")
    for x, y, z, gn, wn in res["extra"][:8]:
        print(f"  extra: {x},{y},{z} vanilla {wn} port {gn}")
    # Same-name/different-property cells are why this gate compares state ids: print both
    # ids so `sculk_vein` vs `sculk_vein` is visibly not "equal".
    for x, y, z, wn, w, gn, g in res["wrong"][:8]:
        print(f"  wrong-state: {x},{y},{z} vanilla {wn}#{w} port {gn}#{g}")

    if sum(total_ref.values()) == 0:
        print("sculk gate: FAIL - the reference has no sculk cells, so it proves nothing")
        code = 2
    elif require and bad:
        print(f"sculk gate: FAIL - {bad} sculk cells disagree")
        code = 1
    else:
        print("sculk gate: " + ("PASS" if bad == 0 else f"REPORT ONLY ({bad} disagreements)"))
        code = 0

    if json_out is not None:
        report = {
            "tool": "sculk_gate",
            "scope": (
                "sculk family cells (sculk, sculk_vein, sculk_catalyst, sculk_shrieker) "
                "in one saved reference region against one existing port dump; not whole-world parity"
            ),
            "args": {
                "world": str(world), "dump": str(dump), "n": n,
                "origin": [x0, z0], "min_y": MIN_Y, "height": HEIGHT,
                "require_match": require,
            },
            "dump": {"path": str(dump), "bytes": len(raw),
                     "sha256": hashlib.sha256(raw).hexdigest()},
            "chunks_compared": res["compared"],
            "chunks_expected": n * n,
            "reference": {"cells": dict(total_ref), "total": sum(total_ref.values())},
            "port": {"cells": dict(total_port), "total": sum(total_port.values())},
            "counts": {"missing": len(res["missing"]), "extra": len(res["extra"]),
                       "wrong_state": len(res["wrong"]), "diffs": bad},
            "samples": {"missing": res["missing"][:8], "extra": res["extra"][:8],
                        "wrong_state": res["wrong"][:8]},
            "exit_code": code,
        }
        try:
            Path(json_out).write_text(json.dumps(report, indent=2) + "\n")
        except OSError as exc:
            print(f"sculk_gate: cannot write {json_out}: {exc}", file=sys.stderr)
            return 2
    return code


if __name__ == "__main__":
    sys.exit(main())
