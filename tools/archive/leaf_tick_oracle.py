#!/usr/bin/env python3
"""Capture the actual 1.21.1 scheduled-leaf distance calculation."""
import argparse
from pathlib import Path

from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parent.parent
VECTORS = ROOT / "tests/data/leaf_tick_1211.tsv"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    parser.add_argument("--write", action="store_true")
    args = parser.parse_args()
    source = ROOT / "tools/leaf-distance-oracle"
    with pinned_runtime(args.jar) as (work, classpath, _):
        output = work / "leaf-tick.tsv"
        run_probe(work, classpath, source / "LeafTickOracle.java",
                  "dev.megu.tools.LeafTickOracle", output,
                  extra_sources=[source / "LeafDistanceOracle.java", source / "LeafQueueOracle.java",
                                 ROOT / "tools/vanilla-probe/VanillaBlockTags.java"])
        actual = output.read_bytes()
    if args.write:
        VECTORS.write_bytes(actual)
    elif actual != VECTORS.read_bytes():
        raise SystemExit("scheduled leaf oracle: native vectors differ")
    print("scheduled leaf oracle: vectors " + ("written" if args.write else "match"))


if __name__ == "__main__":
    main()
