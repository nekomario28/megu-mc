#!/usr/bin/env python3
"""Capture actual 1.21.1 chorus feature states and recursive RNG consumption."""
import argparse
from pathlib import Path
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
VECTORS = ROOT / 'tests/data/chorus_plant_1211.tsv'


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--jar', type=Path, default=DEFAULT_JAR)
    p.add_argument('--write', action='store_true')
    args = p.parse_args()
    with pinned_runtime(args.jar) as (work, cp, _):
        output = work / 'chorus-plant.tsv'
        run_probe(work, cp, ROOT / 'tools/structure-oracle/ChorusPlantOracle.java',
                  'dev.megu.tools.ChorusPlantOracle', output)
        actual = output.read_bytes()
    if args.write:
        VECTORS.write_bytes(actual)
    elif VECTORS.read_bytes() != actual:
        raise SystemExit('native chorus plant feature differs')
    print('chorus plant feature oracle: ' + ('written' if args.write else 'match'))


if __name__ == '__main__':
    main()
