#!/usr/bin/env python3
"""Check the R1 generation/tick boundary, without excluding any unequal states.

Inputs are completed native_fixture.sh captures (frozen, ticked), a production
port dump, and the unchanged canonical world. This verifies the whole requested
region and attributes differences using actual native last-writer stacks.
"""
import argparse
import array
from collections import Counter
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import re
import sys

from carver_diff import CELLS, load_states, vanilla_chunk
from vanilla_java import JAR_SHA1


def now():
    return datetime.now(timezone.utc).isoformat()


def require(condition, message):
    if not condition:
        raise ValueError(message)


def raw(path):
    data = path.read_bytes()
    require(len(data) == 9 * CELLS * 4, f"unexpected dump length: {path}: {len(data)}")
    values = array.array("I")
    values.frombytes(data)
    if sys.byteorder != "little":
        values.byteswap()
    return values


def world(path, table):
    values = array.array("I")
    for z in range(32, 35):
        for x in range(-64, -61):
            chunk = vanilla_chunk(str(path), x, z, table)
            require(chunk is not None, f"not FULL: {path} chunk ({x},{z})")
            require(len(chunk) == CELLS and all(v >= 0 for v in chunk),
                    f"unmapped/missing serialized states: {path} chunk ({x},{z})")
            values.extend(chunk)
    return values


def pos(index):
    chunk, cell = divmod(index, CELLS)
    return ((-64 + chunk % 3)*16 + (cell & 15), -64 + cell//256,
            (32 + chunk//3)*16 + ((cell >> 4) & 15))


def runtime_writers(lines):
    writers = {}
    pattern = re.compile(r"RUNTIME (-?\d+) (-?\d+) (-?\d+) state=(\d+) wrote=true stack=(.*)")
    for line in lines:
        match = pattern.search(line)
        if match:
            writers[tuple(map(int,match.group(1,2,3)))] = (int(match[4]),match[5])
    return writers


def main():
    start = now()
    p = argparse.ArgumentParser(description=__doc__)
    for name in ("frozen", "ticked", "port", "canonical"):
        p.add_argument(name, type=Path)
    p.add_argument("--out", type=Path)
    args = p.parse_args()
    table = load_states()
    captures = []
    for directory, mode in ((args.frozen, "frozen"), (args.ticked, "ticked")):
        require((directory/"finished.txt").exists(), f"capture unfinished: {directory}")
        boundary = json.loads((directory/"boundary.json").read_text())
        require(boundary["mode"] == mode, f"wrong stage: {directory}")
        require(boundary["chunks"] == 9 and boundary["decorated_chunks"] == 25,
                f"wrong footprint: {directory}")
        with (directory/"server.jar").open("rb") as source:
            require(hashlib.file_digest(source, "sha1").hexdigest() == JAR_SHA1,
                    f"unauthenticated native executable: {directory}")
        states = raw(directory/"native.bin")
        require(states == world(directory/"world", table), f"stale native.bin: {directory}")
        captures.append((boundary, states))
    frozen_boundary, frozen = captures[0]
    ticked_boundary, ticked = captures[1]
    require(frozen_boundary["game_times"][0] == frozen_boundary["game_times"][1]
            and frozen_boundary["runtime_writes"] == 0, "frozen stage advanced")
    require(ticked_boundary["game_times"][0] < ticked_boundary["game_times"][1],
            "ticked control did not advance")
    require((args.frozen/"inputs.sha256").read_bytes() == (args.ticked/"inputs.sha256").read_bytes(),
            "native executable/instrumentation arms differ")
    port = raw(args.port)
    gen_diff = sum(a != b for a,b in zip(port,frozen))
    require(gen_diff == 0, f"production generation differs at {gen_diff} cells")
    canonical = world(args.canonical, table)
    saved_diff = sum(a != b for a,b in zip(ticked,canonical))
    require(saved_diff == 0, f"fresh ticked native differs from canonical at {saved_diff} cells")
    with (args.ticked/"run.log").open() as log:
        writers = runtime_writers(log)
    actors = Counter()
    for i,(a,b) in enumerate(zip(frozen,ticked)):
        if a == b:
            continue
        point = pos(i)
        require(point in writers, f"difference has no runtime writer: {point}")
        state, stack = writers[point]
        require(state == b, f"last runtime write does not explain saved state: {point}")
        if "dki.a(" in stack:
            actors["LeavesBlock.tick"] += 1
        elif "epc.b(" in stack:
            actors["FlowingFluid.tick"] += 1
        else:
            raise ValueError(f"unclassified actual runtime writer at {point}: {stack}")
    report = {"started_at": start, "finished_at": now(), "region": [-64,32,3],
              "y": [-64,319], "cells_compared": len(port),
              "production_vs_frozen": gen_diff, "fresh_ticked_native_vs_canonical": saved_diff,
              "frozen_vs_ticked": sum(actors.values()), "actual_last_writers": dict(actors),
              "frozen_boundary": frozen_boundary, "ticked_boundary": ticked_boundary,
              "sha256": {label: hashlib.sha256(path.read_bytes()).hexdigest()
                         for label,path in [("production_dump",args.port),
                                            ("native_frozen_dump",args.frozen/"native.bin"),
                                            ("native_ticked_dump",args.ticked/"native.bin"),
                                            ("native_frozen_order",args.frozen/"order.tsv"),
                                            ("native_ticked_order",args.ticked/"order.tsv")]},
              "scope": "generation parity with explicit order; not port runtime parity"}
    text = json.dumps(report,indent=2)+"\n"
    if args.out:
        args.out.write_text(text)
    print(text,end="")


if __name__ == "__main__":
    main()
