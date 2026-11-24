#!/usr/bin/env python3
"""Replay NormalNoise.create over a LegacyRandomSource from pinned 1.21.1."""
import argparse
from pathlib import Path

from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parent.parent
VECTORS = ROOT / "tests/data/normal_noise_legacy_source_1211.tsv"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    parser.add_argument("--write", action="store_true")
    args = parser.parse_args()
    with pinned_runtime(args.jar) as (work, classpath, _):
        output = work / "normal-noise.tsv"
        run_probe(work, classpath, ROOT / "tools/normal-noise-oracle/NormalNoiseOracle.java",
                  "dev.megu.tools.NormalNoiseOracle", output)
        actual = output.read_bytes()
    if args.write:
        VECTORS.write_bytes(actual)
    elif actual != VECTORS.read_bytes():
        raise SystemExit("normal noise oracle: native samples differ")
    print("normal noise oracle: samples " + ("written" if args.write else "match"))


if __name__ == "__main__":
    main()
