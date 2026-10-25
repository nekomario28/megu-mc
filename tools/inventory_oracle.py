#!/usr/bin/env python3
"""Record/check actual 1.21.1 Inventory.add; not ItemEntity/world execution."""
import argparse
import json
from pathlib import Path
import subprocess
import zipfile

from container_oracle import ROOT, SOURCE, reference as container_reference, require_equal
from vanilla_java import DEFAULT_JAR

FIXTURE = ROOT / "evidence/gameplay/inventory-pickup/vanilla.json"


def cases():
    def stack(count, item="stone", **patch):
        return {"item": item, "count": count, **patch}

    def row(source, slots=None, selected=4, creative=False):
        return {"op": "inventory_add", "source": source, "slots": slots or {},
                "selected": selected, "creative": creative}

    full = {str(i): stack(64, "dirt") for i in range(36)}
    result = [
        row(None),
        row(stack(10)),  # empty hotbar precedes empty main inventory
        row(stack(10), {"0": stack(60), "4": stack(60), "40": stack(60), "9": stack(60)}),
        row(stack(6), {"0": stack(60), "40": stack(60), "9": stack(60)}),
        row(stack(10), {"8": stack(60), "9": stack(60)}),
        row(stack(1), {"0": stack(64, "dirt")}, selected=8),
        row(stack(10), {str(i): stack(64, "dirt") for i in range(9)}),
        row(stack(10), {**full, "40": stack(60)}),  # partial insertion, add returns false
        row(stack(10), full),
        row(stack(10), {**full, "4": stack(60)}),
        row(stack(10), {**full, "4": stack(60)}, creative=True),
        row(stack(10), full, creative=True),
        row(stack(10), {"36": stack(60), "37": stack(60)}),  # armor is not a merge target
        row(stack(10), {"4": stack(60, cap=99), "40": stack(60)}),
        row(stack(10), {**full, "40": None}),  # no insertion into an empty offhand
    ]
    for item, cap, patch in [("stone", 64, {}), ("snowball", 16, {}),
                             ("diamond_sword", 1, {}), ("stone", 16, {"cap": 16})]:
        for count in [1, cap, cap + 1]:
            result.append(row(stack(count, item, **patch), {"4": stack(cap - 1, item, **patch)}
                              if cap > 1 else {}))
            result.append(row(stack(count, item, **patch), full))
    # Damaged items use copyAndClear into one free ordinary slot, not the merge path.
    for patch in [{"damage": 3}, {"damage": 3, "cap": 16},
                  {"damage": 3, "cap": 16, "unbreakable": True}]:
        result.extend([
            row(stack(4, "diamond_sword", **patch), {"4": stack(1, "diamond_sword", **patch)}),
            row(stack(1, "diamond_sword", **patch), full),
            row(stack(1, "diamond_sword", **patch), full, creative=True),
        ])
    result.append(row(stack(5, "diamond_sword", cap=16),
                      {"4": stack(1, "diamond_sword", cap=16, damage=3)}))
    for i, value in enumerate(result):
        value["id"] = f"inventory-{i:03}"
    return result


def reference(jar):
    return container_reference(jar, cases())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--output", type=Path)
    mode.add_argument("--check", type=Path, default=FIXTURE)
    args = parser.parse_args()
    try:
        actual = reference(args.jar)
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(actual, indent=2, sort_keys=True) + "\n")
        else:
            require_equal(json.loads(args.check.read_text()), actual)
        print(f"inventory: {len(actual['cases'])} actual-class cases, pinned 1.21.1")
    except (OSError, ValueError, KeyError, subprocess.SubprocessError, zipfile.BadZipFile) as error:
        parser.exit(1, f"inventory: {error}\n")


if __name__ == "__main__":
    main()
