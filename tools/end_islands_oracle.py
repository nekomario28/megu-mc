#!/usr/bin/env python3
"""Capture actual seeded 1.21.1 End island density and float height results."""
import argparse
from pathlib import Path
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
VECTORS = ROOT/'tests/data/end_islands_1211.tsv'


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--jar', type=Path, default=DEFAULT_JAR)
    p.add_argument('--write', action='store_true')
    args = p.parse_args()
    with pinned_runtime(args.jar) as (work, cp, _):
        output = work/'end-islands.tsv'
        run_probe(work, cp, ROOT/'tools/structure-oracle/EndIslandsOracle.java',
                  'dev.mcspine.tools.EndIslandsOracle', output)
        actual = output.read_bytes()
    if args.write:
        VECTORS.write_bytes(actual)
    elif VECTORS.read_bytes() != actual:
        raise SystemExit('native End islands differ')
    print('End islands oracle: ' + ('written' if args.write else 'match'))


if __name__ == '__main__':
    main()
