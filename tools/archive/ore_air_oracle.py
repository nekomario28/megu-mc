#!/usr/bin/env python3
"""Replay ore exposure decisions and following RNG against pinned 1.21.1 classes."""
import argparse
from pathlib import Path

from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parent.parent
VECTORS = ROOT / "tests/data/ore_air_1211.tsv"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    parser.add_argument("--write", action="store_true", help="regenerate the native vectors")
    args = parser.parse_args()
    with pinned_runtime(args.jar) as (work, classpath, _):
        output = work / "ore-air.tsv"
        run_probe(work, classpath, ROOT / "tools/ore-air-oracle/OreAirOracle.java",
                  "dev.megu.tools.OreAirOracle", output)
        actual = output.read_bytes()
    if args.write:
        VECTORS.write_bytes(actual)
    elif actual != VECTORS.read_bytes():
        raise SystemExit("ore air oracle: committed vectors differ from the pinned classes")
    print("ore air oracle: native vectors " + ("written" if args.write else "match"))


if __name__ == "__main__":
    main()
