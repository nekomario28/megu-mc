#!/usr/bin/env python3
"""Replay actual pinned multiface growth/spreading over controlled worlds."""
import argparse
from pathlib import Path

from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parent.parent
VECTORS = ROOT / "tests/data/multiface_1211.tsv"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    parser.add_argument("--write", action="store_true")
    args = parser.parse_args()
    with pinned_runtime(args.jar) as (work, classpath, _):
        output = work / "multiface.tsv"
        run_probe(work, classpath, ROOT / "tools/multiface-oracle/MultifaceOracle.java",
                  "dev.megu.tools.MultifaceOracle", output)
        actual = output.read_bytes()
    if args.write:
        VECTORS.write_bytes(actual)
    elif actual != VECTORS.read_bytes():
        raise SystemExit("multiface oracle: native vectors differ")
    print("multiface oracle: vectors " + ("written" if args.write else "match"))


if __name__ == "__main__":
    main()
