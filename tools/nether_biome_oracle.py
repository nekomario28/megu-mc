#!/usr/bin/env python3
"""Capture actual 1.21.1 Nether biome selection with its seeded RandomState."""
import argparse
from pathlib import Path
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
VECTORS = ROOT / 'tests/data/nether_biome_1211.tsv'
SURFACE = ROOT / 'tests/data/nether_surface_1211.tsv'


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--jar', type=Path, default=DEFAULT_JAR)
    p.add_argument('--write', action='store_true')
    args = p.parse_args()
    with pinned_runtime(args.jar) as (work, cp, _):
        output = work / 'nether-biomes.tsv'
        surface = work / 'nether-surface.tsv'
        run_probe(work, cp, ROOT / 'tools/structure-oracle/NetherBiomeOracle.java',
                  'dev.mcspine.tools.NetherBiomeOracle', output, surface)
        captures = [(VECTORS, output.read_bytes()), (SURFACE, surface.read_bytes())]
    for path, actual in captures:
        if args.write:
            path.write_bytes(actual)
        elif path.read_bytes() != actual:
            raise SystemExit(f'native Nether contract differs: {path.name}')
    print('Nether biome oracle: ' + ('written' if args.write else 'match'))


if __name__ == '__main__':
    main()
