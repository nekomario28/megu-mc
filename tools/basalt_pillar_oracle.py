#!/usr/bin/env python3
"""Capture actual 1.21.1 basalt_pillar cells and RNG consumption.

The native class is `ebj` (BasaltPillarFeature), identified through
`BuiltInRegistries.FEATURE` in tests/data/feature_classes_1211.tsv.
"""
import argparse
from pathlib import Path

from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
VECTORS = ROOT / 'tests/data/basalt_pillar_1211.tsv'


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--jar', type=Path, default=DEFAULT_JAR)
    p.add_argument('--write', action='store_true')
    p.add_argument('--cells', type=Path, help='also write the per-case cell list')
    args = p.parse_args()
    with pinned_runtime(args.jar) as (work, cp, _):
        output = work / 'basalt_pillar.tsv'
        cells = work / 'basalt_pillar_cells.tsv'
        args = [output]
        if args_cells := getattr(p.parse_args(), 'cells', None):
            args.append(cells)
        run_probe(work, cp, ROOT / 'tools/structure-oracle/BasaltPillarOracle.java',
                  'dev.megu.tools.BasaltPillarOracle', *args)
        actual = output.read_bytes()
        if args_cells:
            args_cells.write_bytes(cells.read_bytes())
    if args.write:
        VECTORS.write_bytes(actual)
    elif not VECTORS.exists() or VECTORS.read_bytes() != actual:
        raise SystemExit('native basalt_pillar feature differs')
    print('basalt_pillar oracle: ' + ('written' if args.write else 'match'))


if __name__ == '__main__':
    main()
