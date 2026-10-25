#!/usr/bin/env python3
"""Record/check actual pinned entity-packet angle constructors; no server or downloads."""
import argparse
import hashlib
import json
from pathlib import Path
import struct
import subprocess
import zipfile

from vanilla_java import DEFAULT_JAR, JAR_SHA1, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "tools/packet-angles-oracle/PacketAnglesOracle.java"
FIXTURE = ROOT / "evidence/gameplay/entity-angles/vanilla.json"


def cases():
    def bits(value):
        return struct.unpack("!I", struct.pack("!f", value))[0]
    inputs = {bits(v) for v in [0.0, -0.0, -1080.0, -720.0, -361.0, -360.0, -270.0,
              -180.0, -90.0, -1.0, -0.1, 0.1, 0.7, 1.0, 45.0, 90.0, 179.0, 180.0,
              181.0, 270.0, 359.0, 360.0, 361.0, 720.0, 1080.0, 1e7, -1e7,
              16777216.0, -16777216.0, 3e38, -3e38]}
    # Adjacent f32s around byte-angle boundaries; not rounded decimal approximations.
    for turn in [-2, -1, 0, 1, 2]:
        for step in [1, 63, 64, 127, 128, 255]:
            boundary = bits(turn * 360.0 + step * 360.0 / 256.0)
            inputs.update([boundary - 1, boundary, boundary + 1])
    # Negative subnormals distinguish multiply-then-divide from division first.
    inputs.update([1, 2, 3, 0x80000001, 0x80000002, 0x80000003])
    ordered = sorted(inputs)
    return [{"id": f"angle-{i:03}", "yaw_bits": raw,
             "pitch_bits": ordered[(i * 37 + 11) % len(ordered)]}
            for i, raw in enumerate(ordered)]


def require_equal(expected, actual):
    if (not actual.get("cases") or expected != actual
            or json.dumps(expected, sort_keys=True) != json.dumps(actual, sort_keys=True)):
        raise ValueError("entity angles differ, are incomplete, or have stale provenance")


def reference(jar):
    requested = cases()
    with pinned_runtime(jar) as (work, cp, _):
        src, dst = work / "input.json", work / "output.json"
        src.write_text(json.dumps({"cases": requested}))
        run_probe(work, cp, SOURCE, "dev.mcspine.tools.PacketAnglesOracle", src, dst)
        rows = json.loads(dst.read_text())
    if not rows or len(rows) != len(requested):
        raise ValueError("empty or truncated entity angles")
    for request, row in zip(requested, rows, strict=True):
        result = row.get("expected", {})
        if ({k: v for k, v in row.items() if k != "expected"} != request
                or set(result) != {"spawn", "teleport", "head_update"}
                or len(result["spawn"]) != 3 or len(result["teleport"]) != 2
                or any(type(b) is not int or not 0 <= b <= 255
                       for b in result["spawn"] + result["teleport"] + [result["head_update"]])):
            raise ValueError("missing/changed entity-angle case: " + request["id"])
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
        print(f"entity angles: {len(actual['cases'])} actual-class cases, pinned 1.21.1")
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError, zipfile.BadZipFile) as error:
        parser.exit(1, f"entity angles: {error}\n")


if __name__ == "__main__":
    main()
