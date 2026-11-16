#!/usr/bin/env python3
"""Verify pinned block/fluid predicates, keyed by block names and properties."""
import argparse
import json
from pathlib import Path
import re

from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parent.parent
ASSET = ROOT / "assets/block_predicates_1211.bin"
SUPPORT = ROOT / "assets/multiface_support_1211.bin"
COLLISION_FULL = ROOT / "assets/collision_shape_full_1211.bin"


def native_key(text):
    """`Block{minecraft:oak_slab}[type=bottom,waterlogged=false]` -> (name, sorted properties)."""
    match = re.fullmatch(r"Block\{minecraft:(.*?)\}(?:\[(.*)\])?", text)
    if match is None:
        raise ValueError("unparsable native state: " + text)
    name, props = match.groups()
    return (name, tuple(sorted(tuple(p.split("=")) for p in props.split(","))) if props else ())


def wire_states(blocks):
    """(state id, native key) for every protocol state id, in id order.

    State ids are a mixed-radix product per block, but each asset may order or extend the
    property values differently. Translate names and values, never numeric state ids.
    """
    for block in blocks:
        for state in range(block["minStateId"], block["maxStateId"] + 1):
            offset = state - block["minStateId"]
            props = []
            for prop in reversed(block["states"]):
                values = prop.get("values") or ["true", "false"]
                props.append((prop["name"], str(values[offset % len(values)])))
                offset //= len(values)
            yield state, (block["name"], tuple(sorted(props)))


def encode(rows, blocks):
    native = {}
    for row in rows.splitlines():
        text, value, support, collision_full = row.split("\t")
        if (not value.isdigit() or not 0 <= int(value) < 256
                or not support.isdigit() or not 0 <= int(support) < 64
                or collision_full not in ("0", "1")):
            raise ValueError("invalid native state: " + row)
        key = native_key(text)
        if key in native:
            raise ValueError("duplicate native state: " + text)
        native[key] = (int(value), int(support), int(collision_full))
    wire = {}
    for state, key in wire_states(blocks):
        if state in wire:
            raise ValueError("duplicate wire id")
        wire[state] = native.pop(key)  # missing names/properties fail, never substitute false
    if native or not wire or set(wire) != set(range(len(wire))):
        raise ValueError("native/wire state sets differ or contain gaps")
    return tuple(bytes(wire[i][column] for i in range(len(wire))) for column in range(3))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    parser.add_argument("--write", action="store_true")
    args = parser.parse_args()
    with pinned_runtime(args.jar) as (work, classpath, _):
        output = work / "block-predicates.tsv"
        run_probe(work, classpath, ROOT / "tools/block-predicate-oracle/BlockPredicateOracle.java",
                  "dev.mcspine.tools.BlockPredicateOracle", output)
        predicates, support, collision_full = encode(output.read_text(), json.loads((ROOT / "assets/blocks_1211.json").read_text()))
    for path, data in [(ASSET, predicates), (SUPPORT, support), (COLLISION_FULL, collision_full)]:
        if args.write:
            path.write_bytes(data)
        elif data != path.read_bytes():
            raise SystemExit(f"{path.name} differs from the pinned classes")
    print(f"block predicate oracle: {len(predicates)} states " + ("written" if args.write else "match"))


if __name__ == "__main__":
    main()
