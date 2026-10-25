#!/usr/bin/env python3
"""Record/check small real-class 1.21.1 container vectors. No server or downloads."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import zipfile

from vanilla_java import DEFAULT_JAR, JAR_SHA1, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "tools/container-oracle/ContainerOracle.java"
FIXTURE = ROOT / "evidence/gameplay/containers/vanilla.json"


def cases():
    result = []
    for item, cap, patch in [("stone", 64, {}), ("snowball", 16, {}),
                             ("diamond_sword", 1, {}), ("stone", 16, {"cap": 16})]:
        for count in [1, max(1, cap - 1), cap]:
            for target in [0, max(1, cap - 1), cap]:
                for amount in [1, count]:
                    result.append({"op": "insert", "source": {"item": item, "count": count, **patch},
                                   "target": {"item": item, "count": target, **patch} if target else None,
                                   "amount": amount})
    # Unlike components and unlike items must not merge in safeInsert.
    result.extend([
        {"op": "insert", "source": {"item": "stone", "count": 10},
         "target": {"item": "stone", "count": 4, "cap": 16}, "amount": 10},
        {"op": "insert", "source": {"item": "stone", "count": 10},
         "target": {"item": "dirt", "count": 4}, "amount": 10},
    ])
    stone = lambda count: {"item": "stone", "count": count}
    for rows in [3, 6]:
        n = rows * 9
        result.extend([
            {"op": "quick_move", "rows": rows, "clicked": 0, "slots": {"0": stone(10)}},
            {"op": "quick_move", "rows": rows, "clicked": 0,
             "slots": {"0": stone(10), str(n): stone(60), str(n + 35): stone(60)}},
            {"op": "quick_move", "rows": rows, "clicked": n,
             "slots": {str(n): stone(10), str(n - 1): stone(60)}},
            {"op": "quick_move", "rows": rows, "clicked": 0,
             "slots": {"0": stone(10), **{str(i): stone(64) for i in range(n, n + 36)}}},
        ])
    for i, row in enumerate(result):
        row["id"] = f"container-{i:03}"
    return result


def require_equal(expected, actual):
    # Full structure equality, not zip(): an empty/missing tail is a failure.
    if not actual.get("cases") or expected != actual:
        raise ValueError("container reference differs, is incomplete, or has stale provenance")


def reference(jar, requested=None):
    if requested is None:
        requested = cases()
    with pinned_runtime(jar) as (work, cp, _inner):
        input_path, output_path = work / "cases.json", work / "results.json"
        input_path.write_text(json.dumps(requested))
        run_probe(work, cp, SOURCE, "dev.mcspine.tools.ContainerOracle", input_path, output_path)
        rows = json.loads(output_path.read_text())
    if not rows or len(rows) != len(requested):
        raise ValueError("empty or truncated container reference")
    for i, row in enumerate(rows):
        if {k: v for k, v in row.items() if k != "expected"} != requested[i] or not row.get("expected"):
            raise ValueError("missing/changed container case: " + requested[i]["id"])
    return {"minecraft": "1.21.1", "server_sha1": JAR_SHA1,
            "oracle_sha256": hashlib.sha256(SOURCE.read_bytes()).hexdigest(), "cases": rows}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--output", type=Path, help="record after complete successful execution")
    mode.add_argument("--check", type=Path, default=FIXTURE, help="default: check committed vectors")
    args = parser.parse_args()
    try:
        actual = reference(args.jar)
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(actual, indent=2, sort_keys=True) + "\n")
        else:
            require_equal(json.loads(args.check.read_text()), actual)
        print(f"containers: {len(actual['cases'])} actual-class cases, pinned 1.21.1")
    except (OSError, ValueError, KeyError, subprocess.SubprocessError, zipfile.BadZipFile) as error:
        parser.exit(1, f"containers: {error}\n")


if __name__ == "__main__":
    main()
