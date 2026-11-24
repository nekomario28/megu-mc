#!/usr/bin/env python3
"""Capture all five End biomes through native source and actual RandomState."""
import argparse
from pathlib import Path
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
VECTORS = ROOT/'tests/data/end_biome_1211.tsv'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--jar', type=Path, default=DEFAULT_JAR)
    parser.add_argument('--write', action='store_true')
    args = parser.parse_args()
    with pinned_runtime(args.jar) as (work, cp, _):
        output = work/'end-biomes.tsv'
        run_probe(work, cp, ROOT/'tools/structure-oracle/EndBiomeOracle.java',
                  'dev.megu.tools.EndBiomeOracle', output)
        actual = output.read_bytes()
    if args.write:
        VECTORS.write_bytes(actual)
    elif VECTORS.read_bytes() != actual:
        raise SystemExit('native End biomes differ')
    print('End biome oracle: ' + ('written' if args.write else 'match'))


if __name__ == '__main__':
    main()
