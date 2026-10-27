#!/usr/bin/env python3
"""Compare a frozen capture and both checkpoints from its resumed copy."""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import re

from forest_stage_check import raw, require, runtime_writers, world, pos
from carver_diff import load_states


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("resume", type=Path)
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    require((args.source / "finished.txt").exists(), "source capture unfinished")
    require((args.resume / "finished.txt").exists(), "resume capture unfinished")
    boundary = json.loads((args.source / "boundary.json").read_text())
    require(boundary["mode"] == "frozen" and boundary["game_times"][0] == boundary["game_times"][1]
            and boundary["runtime_writes"] == 0, "invalid original frozen boundary")
    table = load_states()
    source = world(args.source / "world", table)
    require(source == raw(args.source / "native.bin"), "source native dump does not match its world")
    before = world(args.resume / "snapshot-frozen", table)
    after = world(args.resume / "snapshot-ticked", table)
    before_diff = sum(a != b for a, b in zip(source, before))
    require(before_diff == 0, f"resume changed {before_diff} cells before unfreeze")
    log = (args.resume / "run.log").read_text()
    first_freeze = log.find("The game is frozen")
    require(first_freeze >= 0 and log.count("The game is frozen") >= 2, "missing freeze boundaries")
    require("RESUME_START" in log and log.count("Saved the game") >= 2, "missing checkpoint boundaries")
    early = runtime_writers(log[:first_freeze].splitlines())
    require(not early, f"{len(early)} cells changed before resume froze")
    times = list(map(int, re.findall(r"The time is (\d+)", log)))
    require(len(times) == 2 and times[1] > times[0], f"invalid resume times {times}")
    writers = runtime_writers(log.splitlines())
    actors = Counter()
    for i, (a, b) in enumerate(zip(before, after)):
        if a == b:
            continue
        point = pos(i)
        require(point in writers and writers[point][0] == b,
                f"no matching final runtime writer at {point}")
        stack = writers[point][1]
        actor = "LeavesBlock.tick" if "dki.a(" in stack else (
            "FlowingFluid.tick" if "epc.b(" in stack else "other")
        actors[actor] += 1
    paths = {"source_native": args.source / "native.bin",
             "source_region": args.source / "world/region/r.-2.1.mca",
             "resumed_frozen_region": args.resume / "snapshot-frozen/region/r.-2.1.mca",
             "resumed_ticked_region": args.resume / "snapshot-ticked/region/r.-2.1.mca",
             "resume_log": args.resume / "run.log"}
    report = {"region": [-64, 32, 3], "cells_compared": len(source),
              "source_game_times": boundary["game_times"], "resume_game_times": times,
              "source_vs_resumed_frozen": before_diff, "pre_freeze_runtime_writes": len(early),
              "frozen_vs_resumed_ticked": sum(actors.values()),
              "last_runtime_writers": dict(actors),
              "sha256": {name: sha(path) for name, path in paths.items()},
              "scope": "same copied native world; no port runtime comparison"}
    text = json.dumps(report, indent=2) + "\n"
    if args.out:
        args.out.write_text(text)
    print(text, end="")


if __name__ == "__main__":
    main()
