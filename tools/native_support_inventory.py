#!/usr/bin/env python3
"""Enumerate *every* pinned 1.21.1 block state for native canSurvive testing.

One row per state, not merely one default state per block family. The output is
a temporary local fixture for the user's pinned Minecraft JAR and is not
committed or treated as licensed-to-redistribute game data.
"""
import argparse
import hashlib
import itertools
import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
BLOCKS = ROOT / "assets/blocks_1211.json"
PINNED_BLOCKS = 1060
PINNED_STATES = 26684


def values(spec):
    kind = spec["type"]
    if kind == "bool":
        choices = ("true", "false")
    elif kind in ("int", "enum"):
        choices = tuple(spec["values"])
    else:
        raise ValueError(f"unknown property kind: {kind}")
    if len(choices) != spec["num_values"] or len(set(choices)) != len(choices):
        raise ValueError(f"invalid property values for {spec['name']}")
    return choices


def states(block):
    properties = block.get("states", [])
    choices = [values(spec) for spec in properties]
    count = 1
    for opts in choices:
        count *= len(opts)
    lo = block["minStateId"]
    hi = block["maxStateId"]
    if lo + count - 1 != hi:
        raise ValueError(f"{block['name']}: state-id range does not match property combinations")
    for offset, combination in enumerate(itertools.product(*choices)):
        entry = {"Name": f"minecraft:{block['name']}"}
        if properties:
            entry["Properties"] = {
                spec["name"]: value for spec, value in zip(properties, combination)
            }
        yield lo + offset, entry


def inventory(path=BLOCKS, scope="all"):
    blocks = json.loads(Path(path).read_text(encoding="utf-8"))
    if len(blocks) != PINNED_BLOCKS:
        raise ValueError(f"unexpected block family count {len(blocks)} != {PINNED_BLOCKS}")
    seen = set()
    state_count = 0
    for block in blocks:
        lo, hi = block["minStateId"], block["maxStateId"]
        state_count += hi - lo + 1
        default = block["defaultState"]
        if not lo <= default <= hi:
            raise ValueError(f"{block['name']}: default state lies outside block range")
        for state_id, entry in states(block):
            if state_id in seen:
                raise ValueError(f"duplicate block state ID {state_id}")
            seen.add(state_id)
            if scope == "all" or state_id == default:
                yield state_id, entry
    if state_count != PINNED_STATES or len(seen) != PINNED_STATES:
        raise ValueError(f"expected {PINNED_STATES} registered states; got {state_count}")
    if min(seen) != 0 or max(seen) != PINNED_STATES - 1:
        raise ValueError("block state IDs do not span the pinned registry contiguously")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scope", choices=("default", "all"), default="all")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--blocks", type=Path, default=BLOCKS)
    args = parser.parse_args()
    rows = 0
    digest = hashlib.sha256()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("w", encoding="utf-8", newline="\n") as output:
        for state_id, entry in inventory(args.blocks, args.scope):
            row = str(state_id) + "\t" + json.dumps(
                entry, ensure_ascii=False, separators=(",", ":")
            ) + "\n"
            output.write(row)
            digest.update(row.encode("utf-8"))
            rows += 1
    expected = PINNED_STATES if args.scope == "all" else PINNED_BLOCKS
    if rows != expected:
        raise ValueError(f"incomplete inventory: {rows} != {expected}")
    print(f"states={rows} scope={args.scope} sha256={digest.hexdigest()}", file=sys.stderr)


if __name__ == "__main__":
    main()
