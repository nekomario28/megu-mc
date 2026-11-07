#!/usr/bin/env python3
"""Capture ordered native updateShape/setBlock/scheduleTick requests.

The pinned 1.21.1 JAR is mandatory. The source of tick due/priority/sub-order is
native LevelAccessor and ScheduledTick, not a guessed Python schedule.
This does not claim LevelTicks delivery or a full ServerLevel neighbor cascade.
"""
import argparse
import hashlib
from pathlib import Path
import shutil

from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
SCENES = (
    "waterlogged_pane", "waterlogged_fence", "waterlogged_stairs",
    "leaf_next_to_log", "falling_sand", "falling_gravel",
    "falling_red_sand", "falling_concrete_powder", "falling_sand_supported",
    "fragile_torch",
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
    # Native FallingBlock.updateShape books exactly one NORMAL-priority tick at
    # game-time 100 + 2. This includes the supported-state callback: the actual
    # falling decision is deferred until its scheduled callback runs.
    falling = {
        "falling_sand": "minecraft:sand",
        "falling_gravel": "minecraft:gravel",
        "falling_red_sand": "minecraft:red_sand",
        "falling_concrete_powder": "minecraft:white_concrete_powder",
        "falling_sand_supported": "minecraft:sand",
    }
    for case, identity in falling.items():
        scheduled = ticks.get(case, [])
        if scheduled != [("block", identity, 102, 0)]:
            raise ValueError(
                f"{case} must schedule one exact 2-GT native falling callback: {scheduled!r}"
            )
        raw = [event for c, event in events
               if c == case and event.startswith("tick ")]
        if len(raw) != 1 or not raw[0].endswith(" 102 NORMAL 0"):
            raise ValueError(f"{case} native priority/suborder differ: {raw!r}")
    if sets.get("native_neighbor_pass", 0) == 0:
        raise ValueError("native neighbor pass did not produce a state change")
    suborders = [row[3] for row in ticks.get("mixed_scheduling_requests", [])]
    if len(suborders) < 3 or suborders != sorted(suborders) or len(set(suborders)) != len(suborders):
        raise ValueError("mixed native tick requests did not preserve sub-tick FIFO")
    return {"scenes": len(seen), "events": len(events), "tick_requests": sum(map(len, ticks.values()))}


def validate_delivery(request_rows, delivered_rows):
    """An appointment survives once per identity/position in its own native queue."""
    source = {}
    for raw in request_rows:
        cols = raw.rstrip("\n").split("\t", 2)
        if len(cols) != 3 or not cols[2].startswith("tick "):
            continue
        parts = cols[2].split(" ")
        if len(parts) != 7:
            raise ValueError(f"malformed native request {raw!r}")
        _, phase, identity, pos, due, priority, sub = parts
        source.setdefault((cols[0], phase, identity, pos), int(due))
    observed = set()
    for row in delivered_rows:
        fields = row.rstrip("\n").split("\t")
        if len(fields) != 5:
            raise ValueError(f"bad native delivery {row!r}")
        case, phase, at, identity, pos = fields
        key = case, phase, identity, pos
        if key not in source:
            raise ValueError(f"delivered unrequested native tick: {key}")
        if key in observed:
            raise ValueError(f"duplicate native tick delivery: {key}")
        if int(at) != source[key]:
            raise ValueError(f"native due/delivery mismatch at {key}: {at} != {source[key]}")
        observed.add(key)
    if observed != set(source):
        raise ValueError(f"missing native tick delivery for {set(source)-observed}")
    if not observed:
        raise ValueError("no native tick delivery")
    return len(observed)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--compare", type=Path, help="previously captured native reference")
    parser.add_argument("--delivery-output", type=Path,
                        help="LevelTicks delivery output, default next to request trace")
    parser.add_argument("--compare-delivery", type=Path,
                        help="previous native delivery baseline")
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
        delivered_a = work / "delivered-first.tsv"
        delivered_b = work / "delivered-repeat.tsv"
        delivery_source = src / "NativeTraceDeliveryOracle.java"
        for target in (delivered_a, delivered_b):
            run_probe(work, classpath, delivery_source,
                      "dev.mcspine.tools.NativeTraceDeliveryOracle", left, target)
            with left.open(encoding="utf-8") as requests, target.open(encoding="utf-8") as delivered:
                delivery_count = validate_delivery(requests, delivered)
        if delivered_a.read_bytes() != delivered_b.read_bytes():
            raise ValueError("native LevelTicks delivery is nondeterministic across JVMs")
        if args.compare_delivery and delivered_a.read_bytes() != args.compare_delivery.read_bytes():
            raise ValueError(f"native delivery diverged from {args.compare_delivery}")
        args.output.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(left, args.output)
        delivery_output = args.delivery_output or args.output.with_suffix(".delivered.tsv")
        delivery_output.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(delivered_a, delivery_output)
    digest = hashlib.sha256(args.output.read_bytes()).hexdigest()
    print(f"native requests: {summary} sha256={digest} output={args.output}")
    print(f"native LevelTicks deliveries: {delivery_count} output={delivery_output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
