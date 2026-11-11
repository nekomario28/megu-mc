#!/usr/bin/env python3
"""Capture the native entity-offset transform of a structure template.

`StructureTemplate.transform(Vec3, Mirror, Rotation, BlockPos)` is what places an entity
that a template carries; it is pure geometry, so it can be captured without a level (the
entity itself cannot: creating one needs a real `Level`). The port has no entity
placement yet, so this corpus is the contract for that slice, not a passing comparison.

Regenerate only with `--write`; the default run checks the fixture's shape.
"""
import argparse
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe  # noqa: E402

REPO = pathlib.Path(__file__).resolve().parents[1]
FIXTURE = REPO / "tests/data/template_entity_1211.tsv"
MIRRORS = ("none", "left_right", "front_back")
ROTATIONS = ("none", "clockwise_90", "clockwise_180", "counterclockwise_90")
PIVOTS = 2
OFFSETS = 9


def validate_rows(text):
    rows = [line.split("\t") for line in text.splitlines() if line.strip()]
    expected = PIVOTS * len(MIRRORS) * len(ROTATIONS) * OFFSETS
    if len(rows) != expected:
        raise ValueError(f"expected {expected} rows, got {len(rows)}")
    seen = set()
    for row in rows:
        if len(row) != 11:
            raise ValueError(f"expected 11 columns: {row}")
        pivot, mirror, rotation, mirror_name, rotation_name = row[0], row[1], row[2], row[3], row[4]
        if not pivot.isdigit() or not mirror.isdigit() or not rotation.isdigit():
            raise ValueError(f"indices must be digits: {row}")
        if mirror_name not in MIRRORS:
            raise ValueError(f"unknown mirror {mirror_name}")
        if rotation_name not in ROTATIONS:
            raise ValueError(f"unknown rotation {rotation_name}")
        values = [float(v) for v in row[5:]]
        if any(v != v or abs(v) > 1e6 for v in values):
            raise ValueError(f"unreasonable coordinate: {row}")
        key = (pivot, mirror, rotation, tuple(row[5:8]))
        if key in seen:
            raise ValueError(f"duplicate case {key}")
        seen.add(key)
    # The identity case must stay trivial, or nothing else means anything.
    for row in rows:
        if row[3] == "none" and row[4] == "none":
            if [float(v) for v in row[5:8]] != [float(v) for v in row[8:11]]:
                raise ValueError(f"identity transform moved the point: {row}")
    return rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--jar")
    ap.add_argument("--write", action="store_true")
    args = ap.parse_args()
    with pinned_runtime(pathlib.Path(args.jar) if args.jar else DEFAULT_JAR) as (work, classpath, _inner):
        stdout = run_probe(
            work,
            classpath,
            REPO / "tools/structure-oracle/TemplateEntityOracle.java",
            "dev.mcspine.tools.TemplateEntityOracle",
            extra_sources=(REPO / "tools/structure-oracle/NativeHarness.java",),
            capture=True,
        )
    rows = []
    for line in stdout.splitlines():
        at = line.find("ENT\t")
        if at != -1:
            rows.append(line[at + 4:])
    text = "\n".join(rows) + "\n"
    validate_rows(text)
    if args.write:
        FIXTURE.write_text(text)
        print(f"WRITTEN {FIXTURE} ({len(rows)} rows)")
        return 0
    if FIXTURE.read_text() != text:
        print("template entity transform: MISMATCH", file=sys.stderr)
        return 1
    print(f"template entity transform: {len(rows)} rows MATCH")
    return 0


if __name__ == "__main__":
    sys.exit(main())
