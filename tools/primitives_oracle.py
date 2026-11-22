#!/usr/bin/env python3
"""Compare three small seams against actual authenticated 1.21.1 classes."""
import argparse
import hashlib
import json
from pathlib import Path
import random
import subprocess
import zipfile
from vanilla_java import DEFAULT_JAR, JAR_SHA1, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "tools/primitives-oracle/PrimitivesOracle.java"
FIXTURE = ROOT / "evidence/foundation/valence-primitives/vanilla.json"


def cases():
    out = []
    for n in [1, 2, 16, 17, 32, 33, 64, 65, 128, 129, 256, 257, 1024, 4096]:
        out.append(dict(kind="section", states=list(range(n)), biomes=[0]))
    for n in [1, 2, 3, 4, 5, 8, 9, 16, 32, 64]:
        out.append(dict(kind="section", states=[26683, 1], biomes=list(range(n))))
    for states in [[12958], [12959], [0, 12958, 12959, 1], [26683], [1, 26683, 12959]]:
        out.append(dict(kind="section", states=states, biomes=[63]))
    for units, repeat in [([], 1), ([65, 66, 67], 1), ([0], 1), ([0, 127, 128, 2047, 2048, 65535], 1),
                          ([26085, 26412, 35486], 1), ([0xd83d, 0xdca3], 1),
                          ([0xd800], 1), ([0xdc00], 1), ([0xd800, 65, 0xdc00], 1),
                          ([65], 65535), ([0], 32767), ([0xd83d, 0xdca3], 10922)]:
        out.append(dict(kind="string", units=units, repeat=repeat))
    box = [0, 0, 0, 1, 1, 1]
    for start, end in [([-1, .5, .5], [2, .5, .5]), ([2, .5, .5], [-1, .5, .5]),
                       ([.5, -1, .5], [.5, 2, .5]), ([.5, 2, .5], [.5, -1, .5]),
                       ([.5, .5, -1], [.5, .5, 2]), ([.5, .5, 2], [.5, .5, -1]),
                       ([-1, .5, .5], [0, .5, .5]), ([0, .5, .5], [1, .5, .5]),
                       ([.5, .5, .5], [2, .5, .5]), ([.5, .5, .5], [.5, .5, .5]),
                       ([-1, 0, 0], [2, 0, 0]), ([-1, 1, 1], [2, 1, 1]),
                       ([-1, -1e-7, .5], [2, -1e-7, .5]),
                       ([-1, -0.999e-7, .5], [2, -0.999e-7, .5]),
                       ([-.5e-7, .5, .5], [.5e-7, .5, .5]),
                       ([-.51e-7, .5, .5], [.51e-7, .5, .5]),
                       ([-1, -1, -1], [2, 2, 2]), ([2, 2, 2], [-1, -1, -1]),
                       ([-1, 2, .5], [2, 2, .5])]:
        out.append(dict(kind="clip", box=box, start=start, end=end))
    rng = random.Random(767)
    for _ in range(80):
        low = [rng.uniform(-30, 30) for _ in range(3)]
        high = [x + rng.uniform(.001, 3) for x in low]
        start = [x + rng.uniform(-3, 3) for x in low]
        end = [x + rng.uniform(-3, 3) for x in high]
        out.append(dict(kind="clip", box=low + high, start=start, end=end))
    for i, row in enumerate(out):
        row["id"] = f"primitive-{i:03}"
    return out


def require_equal(expected, actual):
    if (not actual.get("cases") or
            json.dumps(expected, sort_keys=True) != json.dumps(actual, sort_keys=True)):
        raise ValueError("primitives differ, are incomplete, or have stale provenance")


def validate(rows, requested):
    if not rows or len(rows) != len(requested):
        raise ValueError("empty/truncated primitive oracle")
    for req, row in zip(requested, rows, strict=True):
        result = row.get("expected", {})
        if {k: v for k, v in row.items() if k != "expected"} != req:
            raise ValueError("changed primitive request: " + req["id"])
        keys = ({"sha1", "length", "non_air", "block_bits", "biome_bits"} if req["kind"] == "section"
                else {"hit_bits"} if req["kind"] == "clip"
                else {"sha1", "length"} | ({"hex"} if result.get("length", 100) < 100 else set()))
        if set(result) != keys:
            raise ValueError("changed primitive result: " + req["id"])
        if req["kind"] == "clip":
            bits = result["hit_bits"]
            if not isinstance(bits, list) or len(bits) not in (0, 3):
                raise ValueError("invalid hit")
            for b in bits:
                if type(b) is not str or len(b) != 16 or int(b, 16) < 0:
                    raise ValueError("invalid hit bits")
        else:
            if type(result["sha1"]) is not str or len(bytes.fromhex(result["sha1"])) != 20:
                raise ValueError("invalid digest")
            for k in keys - {"sha1", "hex"}:
                if type(result[k]) is not int or result[k] < 0:
                    raise ValueError("invalid integer")
            if "hex" in result and len(bytes.fromhex(result["hex"])) != result["length"]:
                raise ValueError("invalid string bytes")


def reference(jar):
    requested = cases()
    with pinned_runtime(jar) as (work, cp, _):
        src, dst = work / "input.json", work / "output.json"
        src.write_text(json.dumps({"cases": requested}))
        run_probe(work, cp, SOURCE, "dev.megu.tools.PrimitivesOracle", src, dst)
        rows = json.loads(dst.read_text())
    validate(rows, requested)
    return {"minecraft": "1.21.1", "server_sha1": JAR_SHA1,
            "oracle_sha256": hashlib.sha256(SOURCE.read_bytes()).hexdigest(), "cases": rows}


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
        print(f"primitives: {len(actual['cases'])} actual-class cases, pinned 1.21.1")
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError, zipfile.BadZipFile) as error:
        parser.exit(1, f"primitives: {error}\n")


if __name__ == "__main__":
    main()
