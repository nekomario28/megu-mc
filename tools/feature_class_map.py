#!/usr/bin/env python3
"""Read the actual 1.21.1 feature type -> class table out of the pinned jar.

The port's `p_cf` arms are keyed by the datapack's configured-feature `type`.
Knowing which native class implements each type is a precondition for porting
its body, and reading it from `BuiltInRegistries.FEATURE` keeps the table tied to
the authenticated jar instead of a decompiler dump.

Writes `tests/data/feature_classes_1211.tsv`: type, class, superclass chain.
"""
import argparse
import json
from pathlib import Path

from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
VECTORS = ROOT / 'tests/data/feature_classes_1211.tsv'


def pack_types():
    """Every configured-feature type the pinned pack names, inline ones included.

    Only a configured feature's own `type` and the `type` of a feature nested under
    a `feature` key count. Placement modifiers and structure/processor/block types
    also carry a `type` key and must not be mistaken for feature implementations.
    """
    types = set()
    base = ROOT / 'datapacks/vanilla/data/minecraft/worldgen'

    def nested(value):
        if isinstance(value, dict):
            feature = value.get('feature')
            if isinstance(feature, dict) and isinstance(feature.get('type'), str):
                types.add(feature['type'].replace('minecraft:', ''))
            for item in value.values():
                nested(item)
        elif isinstance(value, list):
            for item in value:
                nested(item)

    for f in (base / 'configured_feature').rglob('*.json'):
        j = json.loads(f.read_text())
        if isinstance(j.get('type'), str):
            types.add(j['type'].replace('minecraft:', ''))
        nested(j)
    for f in (base / 'placed_feature').rglob('*.json'):
        nested(json.loads(f.read_text()))
    return sorted(types)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--jar', type=Path, default=DEFAULT_JAR)
    p.add_argument('--write', action='store_true')
    args = p.parse_args()
    types = pack_types()
    with pinned_runtime(args.jar) as (work, cp, _):
        (work / 'types.txt').write_text('\n'.join(types) + '\n')
        output = work / 'feature_classes.tsv'
        run_probe(work, cp, ROOT / 'tools/structure-oracle/FeatureClassMap.java',
                  'dev.mcspine.tools.FeatureClassMap', work / 'types.txt', output)
        actual = output.read_bytes()
    if args.write:
        VECTORS.write_bytes(actual)
    elif not VECTORS.exists() or VECTORS.read_bytes() != actual:
        raise SystemExit('native feature class table differs')
    print('feature class table: ' + ('written' if args.write else 'match')
          + f' ({len(types)} types)')


if __name__ == '__main__':
    main()
