#!/usr/bin/env python3
"""Evaluate actual 1.21.1 SurfaceRules conditions against synthetic contexts."""
import argparse
import hashlib
import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from vanilla_java import DEFAULT_JAR, JAR_SHA1, pinned_runtime, run_probe  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "tools" / "surface-context-oracle" / "SurfaceConditionOracle.java"
FIXTURE = ROOT / "evidence" / "foundation" / "surface-context" / "conditions.json"


def validate(data):
    assert set(data) == {"minecraft", "server_sha1", "inner_game_sha256",
                         "oracle_sha256", "context", "rows"}
    assert data["minecraft"] == "1.21.1" and data["server_sha1"] == JAR_SHA1
    assert data["oracle_sha256"] == hashlib.sha256(SOURCE.read_bytes()).hexdigest()
    rows = data["rows"]
    assert len(rows) == 4037, len(rows)
    kinds = {r["kind"] for r in rows}
    assert kinds == {"stone", "water", "hole", "prelim"}, kinds
    assert all(type(r["expected"]) is bool for r in rows)
    assert any(r["expected"] for r in rows) and any(not r["expected"] for r in rows)
    for r in rows:
        if r["kind"] == "stone":
            assert r["ceiling"] in (True, False) and r["secondary"] in (0, 5)
        if r["kind"] == "water":
            assert r["mult"] in (-1, 0, 2)
    return {"rows": len(rows), "kinds": sorted(kinds)}


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
        out = work / "conditions.json"
        run_probe(work, classpath, SOURCE, "dev.megu.tools.SurfaceConditionOracle", out)
        payload = json.loads(out.read_text())
    fixture = {"minecraft": "1.21.1", "server_sha1": JAR_SHA1,
               "inner_game_sha256": hashlib.sha256(inner).hexdigest(),
               "oracle_sha256": hashlib.sha256(SOURCE.read_bytes()).hexdigest(),
               "context": "actual dzp$g with synthetic t/B/C/D/E/x/v fields; no chunk/world",
               "rows": payload["cases"]}
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
    print("surface condition oracle: %d rows match the pinned classes" % counts["rows"])


if __name__ == "__main__":
    main()
