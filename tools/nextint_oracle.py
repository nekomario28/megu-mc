#!/usr/bin/env python3
"""Recompute the committed nextInt(bound) vectors from the pinned 1.21.1 classes."""
import argparse
import hashlib
import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from vanilla_java import DEFAULT_JAR, JAR_SHA1, pinned_runtime, run_probe  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "tools" / "nextint-oracle" / "NextIntOracle.java"
TSV = ROOT / "tests" / "data" / "random_next_int_1211.tsv"


def validate(tsv):
    rows = [line.split("\t") for line in tsv.read_text().splitlines() if line]
    assert len(rows) == 1116, len(rows)
    kinds = {"xoroshiro", "worldgen", "checked"}
    assert {r[0] for r in rows} == kinds
    for row in rows:
        assert len(row) == 6 and row[0] in kinds
        int(row[1]); int(row[2]); int(row[3])
        assert int(row[3]) == 256
        int(row[4], 16); int(row[5])
    return {"rows": len(rows), "sha256": hashlib.sha256(tsv.read_bytes()).hexdigest()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    parser.add_argument("--tsv", type=Path, default=TSV)
    args = parser.parse_args()
    if args.jar.exists() and hashlib.sha1(args.jar.read_bytes()).hexdigest() != JAR_SHA1:
        parser.exit(2, "not the pinned 1.21.1 server jar: %s\n" % args.jar)
    info = validate(args.tsv)
    with pinned_runtime(args.jar) as (work, classpath, inner):
        run_probe(work, classpath, SOURCE, "dev.megu.tools.NextIntOracle",
                  str(args.tsv.resolve()))
    print("nextInt oracle: %d rows match the pinned classes (%s)" %
          (info["rows"], info["sha256"][:12]))


if __name__ == "__main__":
    main()
