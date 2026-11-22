#!/usr/bin/env python3
"""Exhaustive pinned-vanilla canSurvive sweep, with strict coverage validation.

Requires the authenticated 1.21.1 server JAR. Both block and fluid tags are
loaded from the native built-in pack. Unsupported calls are recorded and cause
failure; no incomplete matrix is ever labelled a parity pass.
"""
import argparse
from pathlib import Path
import shutil
import subprocess

from native_support_inventory import inventory, PINNED_BLOCKS, PINNED_STATES
from vanilla_java import DEFAULT_JAR, pinned_runtime

ROOT = Path(__file__).resolve().parents[1]
CONTEXTS = (
    "empty", "floor_stone", "ceiling_stone", "north_stone",
    "south_stone", "west_stone", "east_stone", "all_stone",
    "all_except_floor", "all_except_north", "floor_water", "floor_lava",
)


def canonical(entry):
    name = entry["Name"]
    properties = entry.get("Properties", {})
    if not properties:
        return name
    return name + "[" + ",".join(
        f"{key}={value}" for key, value in sorted(properties.items())
    ) + "]"


def validate_matrix(rows, expected, strict=True):
    """One native result for each state and scene, in actual input order."""
    count = unsupported = 0
    it = iter(rows)
    for state_id, entry in expected:
        want_name = canonical(entry)
        for context in CONTEXTS:
            row = next(it, None)
            if row is None:
                raise ValueError(f"missing native survival row {state_id}/{context}")
            fields = row.rstrip("\n").split("\t")
            if len(fields) != 5 or fields[0] != str(state_id) or fields[1] != context:
                raise ValueError(f"missing/reordered native survival result {state_id}/{context}")
            result, actual, reads = fields[2:]
            if result not in {"T", "F"} and not result.startswith("U:"):
                raise ValueError(f"invalid survival status {state_id}/{context}: {result}")
            if actual != want_name:
                raise ValueError(
                    f"wrong native block state {state_id}: {actual!r} != {want_name!r}"
                )
            if reads:
                for coordinate in reads.split(";"):
                    parts = coordinate.split(",")
                    if len(parts) != 3:
                        raise ValueError(f"invalid native read trace {coordinate!r}")
                    for part in parts:
                        int(part)
            unsupported += result.startswith("U:")
            count += 1
    if next(it, None) is not None:
        raise ValueError("trailing native results after all registered states")
    if strict and unsupported:
        raise ValueError(f"{unsupported} native survival calls unsupported (U rows)")
    return count, unsupported


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    parser.add_argument("--scope", choices=("all", "default"), default="all")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--timeout", type=int, default=1800)
    args = parser.parse_args()
    states = list(inventory(scope=args.scope))
    expected = PINNED_STATES if args.scope == "all" else PINNED_BLOCKS
    if len(states) != expected:
        raise ValueError(f"incomplete inventory: {len(states)} != {expected}")
    import json
    import os
    with pinned_runtime(args.jar) as (work, classpath, _):
        source = work / "states.tsv"
        output = work / "survival.tsv"
        with source.open("w", encoding="utf-8") as stream:
            for state_id, entry in states:
                stream.write(f"{state_id}\t{json.dumps(entry, separators=(',', ':'))}\n")
        javac = [
            "javac", "-proc:none", "-cp", classpath, "-d", str(work),
            str(ROOT / "tools/structure-oracle/NativeSurvivalOracle.java"),
            str(ROOT / "tools/structure-oracle/NativeHarness.java"),
            str(ROOT / "tools/vanilla-probe/VanillaBlockTags.java"),
            str(ROOT / "tools/structure-oracle/NativeFluidTags.java"),
        ]
        subprocess.run(javac, check=True, timeout=120)
        result = subprocess.run([
            "java", "-Xmx1024m", "-cp", str(work) + os.pathsep + classpath,
            "dev.megu.tools.NativeSurvivalOracle", str(source), str(output),
            str(expected),
        ], cwd=work, timeout=args.timeout, check=False)
        if not output.exists():
            raise RuntimeError("native oracle did not emit its coverage matrix")
        args.output.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(output, args.output)
        with args.output.open(encoding="utf-8") as stream:
            observations, unsupported = validate_matrix(stream, states, strict=False)
        print(f"native observed={observations}, unsupported={unsupported}, result={args.output}")
        if result.returncode != 0 or unsupported:
            raise RuntimeError(
                "native survival sweep INCOMPLETE; inspect U rows in saved output "
                "(unsupported is not success)"
            )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
