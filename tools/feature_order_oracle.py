#!/usr/bin/env python3
"""Capture vanilla biome-source iteration and FeatureSorter decoration order."""
import argparse
from pathlib import Path
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
VECTORS = ROOT / 'assets/feature_order_1211.json'


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--jar', type=Path, default=DEFAULT_JAR)
    p.add_argument('--write', action='store_true')
    args = p.parse_args()
    with pinned_runtime(args.jar) as (work, cp, _):
        output = work / 'feature-order.json'
        run_probe(work, cp, ROOT / 'tools/structure-oracle/FeatureOrderOracle.java',
                  'dev.megu.tools.FeatureOrderOracle', output)
        actual = output.read_bytes()
    if args.write:
        VECTORS.write_bytes(actual)
    elif VECTORS.read_bytes() != actual:
        raise SystemExit('native feature order differs')
    print('feature order oracle: ' + ('written' if args.write else 'match'))


if __name__ == '__main__':
    main()
