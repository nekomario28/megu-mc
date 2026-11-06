#!/usr/bin/env python3
"""Capture ordered native updateShape/setBlock/scheduleTick requests.

The pinned 1.21.1 JAR is mandatory. The source of tick due/priority/sub-order is
native LevelAccessor and ScheduledTick, not a guessed Python schedule.
This does not claim LevelTicks delivery or a full ServerLevel neighbor cascade.
"""
import argparse
import hashlib
import os
from pathlib import Path
import shutil

from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
SCENES = (
    "waterlogged_pane", "waterlogged_fence", "waterlogged_stairs",
    "leaf_next_to_log", "falling_sand", "fragile_torch",
    "mixed_scheduling_requests", "native_neighbor_pass",
)


def validate_trace(lines):
    seen = []
    offsets = {}
    ticks = {}
    sets = {}
    events = []
    for text in lines:
        row = text.rstrip("\n").split("\t")
        if len(row) != 3:
            raise ValueError(f"bad trace row: {text!r}")
        case, sequence, event = row
        if case not in SCENES:
            raise ValueError(f"unrecognized case {case}")
        if case not in offsets:
            if len(seen) >= len(SCENES) or SCENES[len(seen)] != case:
                raise ValueError(f"native case missing or reordered: {case}")
            seen.append(case)
            offsets[case] = 0
        if sequence != str(offsets[case]):
            raise ValueError(f"nonconsecutive event sequence {case}/{sequence}")
        offsets[case] += 1
        if event.startswith("tick "):
            fields = event.split(" ")
            if len(fields) != 7 or fields[1] not in ("fluid", "block"):
                raise ValueError(f"invalid native scheduled-tick event {event!r}")
            _, kind, identity, pos, due, priority, suborder = fields
            if not identity.startswith("minecraft:") or len(pos.split(",")) != 3:
                raise ValueError(f"invalid tick identity/position {event!r}")
            for coordinate in pos.split(","):
                int(coordinate)
            if int(due) < 100 or int(suborder) < 0 or not priority:
                raise ValueError(f"invalid native tick time/priority {event!r}")
            ticks.setdefault(case, []).append((kind, identity, int(due), int(suborder)))
        elif event.startswith("set "):
            sets[case] = sets.get(case, 0) + 1
        elif not event.startswith("shape "):
            raise ValueError(f"unexpected native event {event!r}")
        events.append((case, event))
    if tuple(seen) != SCENES:
        raise ValueError(f"missing native scenes: {set(SCENES) - set(seen)}")
    for case in ("waterlogged_pane", "waterlogged_fence", "waterlogged_stairs"):
        if not any(kind == "fluid" and "water" in identity and due == 105
                   for kind, identity, due, _ in ticks.get(case, [])):
            raise ValueError(f"{case} did not book native 5-tick water event")
    if not any(kind == "block" for kind, *_ in ticks.get("leaf_next_to_log", [])):
        raise ValueError("native leaves did not reserve a block tick")
    if not any(kind == "block" for kind, *_ in ticks.get("falling_sand", [])):
        raise ValueError("native sand did not reserve a block tick")
    if sets.get("native_neighbor_pass", 0) == 0:
        raise ValueError("native neighbor pass did not produce a state change")
    suborders = [row[3] for row in ticks.get("mixed_scheduling_requests", [])]
    if len(suborders) < 3 or suborders != sorted(suborders) or len(set(suborders)) != len(suborders):
        raise ValueError("mixed native tick requests did not preserve sub-tick FIFO")
    return {"scenes": len(seen), "events": len(events), "tick_requests": sum(map(len, ticks.values()))}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--compare", type=Path, help="previously captured native reference")
    args = parser.parse_args()
    with pinned_runtime(args.jar) as (work, classpath, _):
        src = ROOT / "tools/structure-oracle"
        left, right = work / "native-trace-first.tsv", work / "native-trace-repeat.tsv"
        extras = (src / "NativeHarness.java",
                  ROOT / "tools/vanilla-probe/VanillaBlockTags.java",
                  src / "NativeFluidTags.java")
        for target in (left, right):
            run_probe(work, classpath, src / "NativeNeighborTraceOracle.java",
                      "dev.mcspine.tools.NativeNeighborTraceOracle", target,
                      extra_sources=extras)
            with target.open(encoding="utf-8") as rows:
                summary = validate_trace(rows)
        if left.read_bytes() != right.read_bytes():
            raise ValueError("native request trace is nondeterministic across fresh JVMs")
        if args.compare and left.read_bytes() != args.compare.read_bytes():
            raise ValueError(f"native ordered trace differs from {args.compare}")
        args.output.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(left, args.output)
    digest = hashlib.sha256(args.output.read_bytes()).hexdigest()
    print(f"native requests: {summary} sha256={digest} output={args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
