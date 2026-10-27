#!/usr/bin/env python3
"""Replay actual TreeFeature.updateLeaves with vanilla block tags bound."""
import argparse
from pathlib import Path

from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parent.parent
VECTORS = ROOT / "tests/data/leaf_distance_1211.tsv"
QUEUE_VECTORS = ROOT / "tests/data/leaf_queue_1211.tsv"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    parser.add_argument("--write", action="store_true")
    args = parser.parse_args()
    with pinned_runtime(args.jar) as (work, classpath, _):
        output = work / "leaf-distance.tsv"
        queue = work / "leaf-queue.tsv"
        run_probe(work, classpath, ROOT / "tools/leaf-distance-oracle/LeafDistanceOracle.java",
                  "dev.mcspine.tools.LeafDistanceOracle", output, queue,
                  extra_sources=[ROOT / "tools/vanilla-probe/VanillaBlockTags.java",
                                 ROOT / "tools/leaf-distance-oracle/LeafQueueOracle.java"])
        captures = [(VECTORS, output.read_bytes()), (QUEUE_VECTORS, queue.read_bytes())]
    for path, actual in captures:
        if args.write:
            path.write_bytes(actual)
        elif actual != path.read_bytes():
            raise SystemExit(f"leaf-distance oracle: native vectors differ: {path.name}")
    print("leaf-distance oracle: vectors " + ("written" if args.write else "match"))


if __name__ == "__main__":
    main()
