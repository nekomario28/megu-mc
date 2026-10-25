#!/usr/bin/env python3
"""Record/check pinned surface codecs and biome-temperature vectors; no server or downloads."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import zipfile

from vanilla_java import DEFAULT_JAR, JAR_SHA1, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "tools/surface-oracle/SurfaceOracle.java"
SETTINGS = ROOT / "datapacks/vanilla/data/minecraft/worldgen/noise_settings/overworld.json"
FIXTURE = ROOT / "evidence/foundation/surface-loader/vanilla.json"


def cases():
    states = {}

    def walk(value):
        if isinstance(value, dict):
            if "result_state" in value:
                state = value["result_state"]
                states[json.dumps(state, sort_keys=True)] = state
            for child in value.values():
                walk(child)
        elif isinstance(value, list):
            for child in value:
                walk(child)

    walk(json.loads(SETTINGS.read_text())["surface_rule"])
    for name, prop, values in [("deepslate", "axis", ["x", "y", "z"]),
                               ("grass_block", "snowy", ["true", "false"]),
                               ("water", "level", ["0", "7", "15"])]:
        for value in values:
            state = {"Name": "minecraft:" + name, "Properties": {prop: value}}
            states[json.dumps(state, sort_keys=True)] = state
    rows = [{"op": "surface_rule"}]
    rows.extend({"op": "state", "state": states[key]} for key in sorted(states))
    # Temperature thresholds can hide a broken noise value. Compare the real
    # Biome static fields too, before their values collapse into a branch.
    for source in ["height", "frozen", "info"]:
        for pos in [[0.0, 0.0], [0.85, -1.55], [-6.85, 12.45]]:
            rows.append({"op": "simplex", "source": source, "pos": pos})
    for frozen in [False, True]:
        for base in [0.0, 0.15, 0.5, 2.0]:
            for pos in [[0, 64, 0], [0, 80, 0], [0, 81, 0], [17, 150, -31],
                        [-137, 320, 249], [12345, 64, -98765], [30000000, 81, -30000000]]:
                rows.append({"op": "temperature", "base_temperature": base,
                             "frozen": frozen, "pos": pos})
    for base in [0.14999998, 0.15000002]:
        rows.append({"op": "temperature", "base_temperature": base,
                     "frozen": False, "pos": [0, 64, 0]})
    for i, row in enumerate(rows):
        row["id"] = f"surface-{i:03}"
    return rows


def require_equal(expected, actual):
    if not actual.get("cases") or expected != actual:
        raise ValueError("surface reference differs, is incomplete, or has stale provenance")


def reference(jar):
    requested = cases()
    with pinned_runtime(jar) as (work, cp, inner):
        # A different installed pack is not silently accepted as a vanilla input.
        import io
        with zipfile.ZipFile(io.BytesIO(inner)) as game:
            settings = game.read("data/minecraft/worldgen/noise_settings/overworld.json")
        if json.loads(settings) != json.loads(SETTINGS.read_bytes()):
            raise ValueError("installed surface settings differ from the pinned jar")
        src, dst = work / "input.json", work / "output.json"
        src.write_text(json.dumps({"cases": requested, "surface_rule": json.loads(settings)["surface_rule"]}))
        run_probe(work, cp, SOURCE, "dev.mcspine.tools.SurfaceOracle", src, dst)
        rows = json.loads(dst.read_text())
    if not rows or len(rows) != len(requested):
        raise ValueError("empty or truncated surface reference")
    shapes = {"surface_rule": {"decoded"}, "state": {"state_id"},
              "simplex": {"value_bits"}, "temperature": {"temperature_bits", "cold"}}
    for request, row in zip(requested, rows, strict=True):
        if ({k: v for k, v in row.items() if k != "expected"} != request
                or set(row.get("expected", {})) != shapes[request["op"]]):
            raise ValueError("missing/changed surface case: " + request["id"])
    return {"minecraft": "1.21.1", "server_sha1": JAR_SHA1,
            "oracle_sha256": hashlib.sha256(SOURCE.read_bytes()).hexdigest(),
            "settings_sha256": hashlib.sha256(settings).hexdigest(), "cases": rows}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--output", type=Path, help="record only after complete successful execution")
    mode.add_argument("--check", type=Path, default=FIXTURE)
    args = parser.parse_args()
    try:
        actual = reference(args.jar)
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(actual, indent=2, sort_keys=True) + "\n")
        else:
            require_equal(json.loads(args.check.read_text()), actual)
        print(f"surface: {len(actual['cases'])} actual-class cases, pinned 1.21.1")
    except (OSError, ValueError, KeyError, subprocess.SubprocessError, zipfile.BadZipFile) as error:
        parser.exit(1, f"surface: {error}\n")


if __name__ == "__main__":
    main()
