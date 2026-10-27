#!/usr/bin/env python3
"""Capture LevelTicks ordering and deduplication from the pinned native jar."""
import argparse
from pathlib import Path
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parent.parent
VECTORS = ROOT / "tests/data/scheduled_tick_1211.tsv"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    parser.add_argument("--write", action="store_true")
    args = parser.parse_args()
    with pinned_runtime(args.jar) as (work, classpath, _):
        out = work / "scheduled.tsv"
        run_probe(work, classpath, ROOT / "tools/leaf-distance-oracle/ScheduledTickOracle.java",
                  "dev.mcspine.tools.ScheduledTickOracle", out)
        actual = out.read_bytes()
    if args.write:
        VECTORS.write_bytes(actual)
    elif actual != VECTORS.read_bytes():
        raise SystemExit("scheduled tick oracle: native vectors differ")
    print("scheduled tick oracle: vectors " + ("written" if args.write else "match"))


if __name__ == "__main__":
    main()
