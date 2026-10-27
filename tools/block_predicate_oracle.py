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


def encode(rows, blocks):
    native = {}
    for row in rows.splitlines():
        text, value, support = row.split("\t")
        match = re.fullmatch(r"Block\{minecraft:(.*?)\}(?:\[(.*)\])?", text)
        if (match is None or not value.isdigit() or not 0 <= int(value) < 256
                or not support.isdigit() or not 0 <= int(support) < 64):
            raise ValueError("invalid native state: " + row)
        name, props = match.groups()
        key = (name, tuple(sorted(tuple(p.split("=")) for p in props.split(","))) if props else ())
        if key in native:
            raise ValueError("duplicate native state: " + text)
        native[key] = (int(value), int(support))
    wire = {}
    for block in blocks:
        for state in range(block["minStateId"], block["maxStateId"] + 1):
            offset = state - block["minStateId"]
            props = []
            for prop in reversed(block["states"]):
                values = prop.get("values") or ["true", "false"]
                props.append((prop["name"], str(values[offset % len(values)])))
                offset //= len(values)
            key = (block["name"], tuple(sorted(props)))
            if state in wire:
                raise ValueError("duplicate wire id")
            wire[state] = native.pop(key)  # missing names/properties fail, never substitute false
    if native or not wire or set(wire) != set(range(len(wire))):
        raise ValueError("native/wire state sets differ or contain gaps")
    return tuple(bytes(wire[i][column] for i in range(len(wire))) for column in range(2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    parser.add_argument("--write", action="store_true")
    args = parser.parse_args()
    with pinned_runtime(args.jar) as (work, classpath, _):
        output = work / "block-predicates.tsv"
        run_probe(work, classpath, ROOT / "tools/block-predicate-oracle/BlockPredicateOracle.java",
                  "dev.mcspine.tools.BlockPredicateOracle", output)
        predicates, support = encode(output.read_text(), json.loads((ROOT / "assets/blocks_1211.json").read_text()))
    for path, data in [(ASSET, predicates), (SUPPORT, support)]:
        if args.write:
            path.write_bytes(data)
        elif data != path.read_bytes():
            raise SystemExit(f"{path.name} differs from the pinned classes")
    print(f"block predicate oracle: {len(predicates)} states " + ("written" if args.write else "match"))


if __name__ == "__main__":
    main()
