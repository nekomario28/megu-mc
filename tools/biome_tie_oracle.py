#!/usr/bin/env python3
"""Recompute the committed biome tie vectors from the pinned 1.21.1 Climate tree."""
import argparse
import hashlib
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from vanilla_java import DEFAULT_JAR, JAR_SHA1, pinned_runtime, run_probe  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "tools" / "biome-tie-oracle" / "BiomeTieOracle.java"
TSV = ROOT / "tests" / "data" / "biome_probe_ties_1211.tsv"


def validate(tsv):
    rows = [line.split("\t") for line in tsv.read_text().splitlines() if line]
    assert len(rows) == 2500, len(rows)
    ties = 0
    for row in rows:
        assert len(row) == 8, row
        for value in row[:6]:
            int(value)
        assert row[6].startswith("minecraft:") and row[6].count(":") == 1
        assert row[7] in {"0", "1"}
        ties += int(row[7])
    assert ties >= 2000, ties
    return {"rows": len(rows), "ties": ties,
            "sha256": hashlib.sha256(tsv.read_bytes()).hexdigest()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    parser.add_argument("--tsv", type=Path, default=TSV)
    args = parser.parse_args()
    if args.jar.exists() and hashlib.sha1(args.jar.read_bytes()).hexdigest() != JAR_SHA1:
        parser.exit(2, "not the pinned 1.21.1 server jar: %s\n" % args.jar)
    info = validate(args.tsv)
    with pinned_runtime(args.jar) as (work, classpath, inner):
        run_probe(work, classpath, SOURCE, "dev.mcspine.tools.BiomeTieOracle",
                  str(args.tsv.resolve()))
    print("biome tie oracle: %d rows (%d ties) match the pinned classes (%s)" %
          (info["rows"], info["ties"], info["sha256"][:12]))


if __name__ == "__main__":
    main()
