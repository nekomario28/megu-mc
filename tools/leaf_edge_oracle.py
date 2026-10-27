#!/usr/bin/env python3
"""Capture native tree leaf shape-edge scheduled block ticks."""
import argparse
from pathlib import Path
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parent.parent
VECTORS = ROOT / "tests/data/leaf_edge_1211.tsv"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    parser.add_argument("--write", action="store_true")
    args = parser.parse_args()
    source = ROOT / "tools/leaf-distance-oracle"
    with pinned_runtime(args.jar) as (work, classpath, _):
        out = work / "leaf-edge.tsv"
        run_probe(work, classpath, source / "LeafEdgeOracle.java", "dev.mcspine.tools.LeafEdgeOracle", out,
                  extra_sources=[source / "LeafDistanceOracle.java", source / "LeafQueueOracle.java",
                                 ROOT / "tools/vanilla-probe/VanillaBlockTags.java"])
        actual = out.read_bytes()
    if args.write:
        VECTORS.write_bytes(actual)
    elif actual != VECTORS.read_bytes():
        raise SystemExit("leaf edge oracle: native vectors differ")
    print("leaf edge oracle: vectors " + ("written" if args.write else "match"))


if __name__ == "__main__":
    main()
