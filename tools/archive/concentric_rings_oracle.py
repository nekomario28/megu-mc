#!/usr/bin/env python3
"""Capture native ring positions and RNG-sensitive fixed-biome relocation."""
import argparse
from pathlib import Path
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
VECTORS = ROOT/'tests/data/concentric_rings_1211.tsv'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--jar', type=Path, default=DEFAULT_JAR)
    parser.add_argument('--write', action='store_true')
    args = parser.parse_args()
    with pinned_runtime(args.jar) as (work, cp, _):
        output = work/'rings.tsv'
        run_probe(work, cp, ROOT/'tools/structure-oracle/ConcentricRingsOracle.java',
                  'dev.megu.tools.ConcentricRingsOracle', output)
        actual = output.read_bytes()
    if args.write:
        VECTORS.write_bytes(actual)
    elif VECTORS.read_bytes() != actual:
        raise SystemExit('native concentric rings differ')
    print('concentric rings oracle: ' + ('written' if args.write else 'match'))


if __name__ == '__main__':
    main()
