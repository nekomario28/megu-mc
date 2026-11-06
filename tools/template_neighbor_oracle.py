#!/usr/bin/env python3
"""Verify bounded fence neighbor transitions against the pinned vanilla 1.21.1 game.

Not part of the portable gate: Java 21 and the user's authenticated server JAR
are required. This invokes the native BlockState.updateShape through the existing
scripted-level harness. It is not full structure placement or live-server parity.
"""
import argparse
from pathlib import Path

from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

REPO = Path(__file__).resolve().parents[1]
CASES = (
    "oak_north_bone",
    "oak_south_bone",
    "oak_west_bone",
    "oak_east_bone",
    "nether_west_bone",
    "pane_west_bone",
    "bars_east_bone",
    "wall_west_bone",
    "stairs_outer_left",
    "stairs_inner_left",
    "torch_down_air",
    "wall_torch_south_air",
    "lantern_up_air",
    "oak_west_air",
    "pane_west_air",
    "bone_west_bone",
)


def validate_rows(text: str) -> None:
    rows = text.splitlines()
    if len(rows) != len(CASES):
        raise ValueError(f"native neighbor probe returned {len(rows)} rows; expected {len(CASES)}")
    for want, line in zip(CASES, rows):
        fields = line.split("\t")
        if len(fields) != 3 or fields[0] != want:
            raise ValueError(f"native neighbor probe omitted or reordered case {want}: {line!r}")
        if not fields[1].startswith("minecraft:") or not fields[2].startswith("minecraft:"):
            raise ValueError(f"native neighbor probe omitted block-state text: {line!r}")
        unchanged = want in {"oak_west_air", "pane_west_air", "bone_west_bone"}
        if unchanged and fields[1] != fields[2]:
            raise ValueError(f"negative neighbor control changed state: {want}")
        if not unchanged and fields[1] == fields[2]:
            raise ValueError(f"positive neighbor control did not change state: {want}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    args = parser.parse_args()
    with pinned_runtime(args.jar) as (work, classpath, _):
        output = work / "neighbor-shapes.tsv"
        run_probe(
            work, classpath, REPO / "tools/structure-oracle/NeighborShapeOracle.java",
            "dev.mcspine.tools.NeighborShapeOracle", output,
            extra_sources=(REPO / "tools/structure-oracle/NativeHarness.java",),
        )
        validate_rows(output.read_text(encoding="utf-8"))
    print(f"native neighbor shapes: {len(CASES)} cases matched")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
