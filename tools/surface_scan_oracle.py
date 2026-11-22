#!/usr/bin/env python3
"""Replay actual 1.21.1 SurfaceSystem.buildSurface on synthetic stone/water columns."""
import argparse
import hashlib
import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from vanilla_java import DEFAULT_JAR, JAR_SHA1, pinned_runtime, run_probe  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "tools" / "surface-context-oracle" / "BuildSurfaceScanOracle.java"
FIXTURE = ROOT / "evidence" / "foundation" / "surface-context" / "buildsurface.json"


def validate(data):
    assert set(data) == {"minecraft", "server_sha1", "inner_game_sha256", "oracle_sha256", "columns"}
    assert data["minecraft"] == "1.21.1" and data["server_sha1"] == JAR_SHA1
    assert data["oracle_sha256"] == hashlib.sha256(SOURCE.read_bytes()).hexdigest()
    rows = data["columns"]
    assert len(rows) == 768, len(rows)
    assert {r["offset"] for r in rows} == {0, 1, 2}
    assert len({(r["offset"], r["x"], r["z"]) for r in rows}) == 768
    for r in rows:
        assert 50 <= r["top"] <= 60 and r["sea"] == 63
        assert r["gold"] == list(range(r["top"] - r["offset"], r["top"] + 1)), r
    return {"columns": len(rows), "gold_cells": sum(len(r["gold"]) for r in rows)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--check", type=Path, default=FIXTURE)
    group.add_argument("--output", type=Path)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    args = parser.parse_args()
    if args.jar.exists() and hashlib.sha1(args.jar.read_bytes()).hexdigest() != JAR_SHA1:
        parser.exit(2, "not the pinned 1.21.1 server jar: %s\n" % args.jar)
    with pinned_runtime(args.jar) as (work, classpath, inner):
        out = work / "scan.json"
        run_probe(work, classpath, SOURCE, "dev.megu.tools.BuildSurfaceScanOracle", out)
        payload = json.loads(out.read_text())
    fixture = {"minecraft": "1.21.1", "server_sha1": JAR_SHA1,
               "inner_game_sha256": hashlib.sha256(inner).hexdigest(),
               "oracle_sha256": hashlib.sha256(SOURCE.read_bytes()).hexdigest(),
               "columns": payload["cases"]}
    counts = validate(fixture)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(fixture, indent=2, sort_keys=True) + "\n")
        print("wrote", args.output, counts)
        return
    expected = json.loads(args.check.read_text())
    validate(expected)
    if expected != fixture:
        for key in fixture:
            if expected.get(key) != fixture.get(key):
                print("fixture mismatch:", key, file=sys.stderr)
        raise SystemExit(1)
    print("buildSurface scan oracle: %d columns match the pinned classes" % counts["columns"])


if __name__ == "__main__":
    main()
