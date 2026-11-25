#!/usr/bin/env python3
"""Emit a cases skeleton for a feature type from the pinned pack and class table.

  tools/feature_cases/gen.py pointed_dripstone [--force] [--support minecraft:stone]

Why: the class name and the config object are mechanical (class table + pack JSON), and a
first scene set is the same seven shapes for almost every block-writing feature: air, a
floor, a ceiling, both, a water column, a blocked cell above, and leaves. What is *not*
mechanical is the one or two scenes that discriminate this type's rules, so this writes the
mechanical part and leaves a marked place to add those.

The output is a skeleton, not a fixture: the native capture (and the port) still decide what
is true. Class names come from `tests/data/feature_classes_1211.tsv` (recovered from the
pinned jar's registry), the config from the pack's own `configured_feature` JSON (nested and
inline included), and `support` is the ground the scenes use.
"""
import argparse
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
DATA = ROOT / 'datapacks/vanilla/data/minecraft/worldgen'
CLASSES = ROOT / 'tests/data/feature_classes_1211.tsv'
CASES = ROOT / 'tests/feature_cases'


def class_of(type_name):
    for line in CLASSES.read_text().splitlines():
        parts = line.split('\t')
        if parts and parts[0] == type_name:
            return parts[1] if len(parts) > 1 else None
    return None


def find_configs(node, type_name, found):
    """Every dict in the pack whose `type` is our feature, nested ones included."""
    if isinstance(node, dict):
        if node.get('type') == 'minecraft:' + type_name or node.get('type') == type_name:
            found.append(node)
        for value in node.values():
            find_configs(value, type_name, found)
    elif isinstance(node, list):
        for value in node:
            find_configs(value, type_name, found)
    return found


def pack_config(type_name):
    """The pack's first config for this type, plus the files that carry it."""
    for folder in ('configured_feature', 'placed_feature', 'biome'):
        for path in sorted((DATA / folder).rglob('*.json')):
            try:
                node = json.loads(path.read_text())
            except json.JSONDecodeError:
                continue
            hits = find_configs(node, type_name, [])
            if hits:
                return hits[0].get('config', {}), path.relative_to(ROOT).as_posix()
    return None, None


def scenes(support):
    """The mechanical scene set; add the discriminating ones by hand below the marker."""
    box = {'dx': [-4, 4], 'dz': [-4, 4]}
    return [
        {'name': 'air', 'plies': []},
        {'name': 'floor', 'plies': [
            {'block': support, 'dx': [-4, 4], 'dy': [-1, -1], 'dz': [-4, 4]}]},
        {'name': 'ceiling', 'plies': [
            {'block': support, 'dx': [-4, 4], 'dy': [1, 1], 'dz': [-4, 4]}]},
        {'name': 'both', 'plies': [
            {'block': support, 'dx': [-4, 4], 'dy': [-1, -1], 'dz': [-4, 4]},
            {'block': support, 'dx': [-4, 4], 'dy': [1, 1], 'dz': [-4, 4]}]},
        {'name': 'water_column', 'plies': [
            {'block': support, 'dx': [-4, 4], 'dy': [-1, -1], 'dz': [-4, 4]},
            {'block': 'minecraft:water', 'dx': [-4, 4], 'dy': [0, 2], 'dz': [-4, 4]}]},
        {'name': 'blocked_above', 'plies': [
            {'block': support, 'dx': [-4, 4], 'dy': [-1, -1], 'dz': [-4, 4]},
            {'block': 'minecraft:glass', **box, 'dy': [1, 1]}]},
        {'name': 'leaves_above', 'plies': [
            {'block': support, 'dx': [-4, 4], 'dy': [-1, -1], 'dz': [-4, 4]},
            {'block': 'minecraft:oak_leaves', 'dx': [-2, 2], 'dy': [3, 4], 'dz': [-2, 2]}]},
    ]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('type')
    p.add_argument('--support', default='minecraft:stone')
    p.add_argument('--force', action='store_true')
    p.add_argument('--out', type=Path)
    args = p.parse_args()
    type_name = args.type.replace('minecraft:', '')

    cls = class_of(type_name)
    if cls is None:
        raise SystemExit(f'gen: {type_name} is not in {CLASSES.relative_to(ROOT)}')
    config, source = pack_config(type_name)
    if config is None:
        raise SystemExit(
            f'gen: the pack has no config for {type_name}; pass one by hand '
            f'(the type may need a config the pack expresses differently)')

    cases = {
        '_comment': (
            'FeatureOracle cases. Coordinates are origin-relative; ranges are inclusive; a '
            'cell covered by no ply is air. Generated skeleton: add the scenes that '
            'discriminate this type\'s own rules (the mechanism below), then capture the '
            'native fixture with tools/feature_oracles.py --cases.'),
        'type': 'minecraft:' + type_name,
        'class': cls,
        'config': config,
        'scenes': scenes(args.support),
        'seeds': [0, 1, -1, 15, 20, 42, 60007],
        'oy': 64,
        'extra': [
            {'scene': 'floor', 'oy': -63, 'seed': 0},
            {'scene': 'ceiling', 'oy': 318, 'seed': 0},
        ],
    }
    out = args.out or (CASES / f'{type_name}.json')
    if out.exists() and not args.force:
        raise SystemExit(f'gen: {out} exists (use --force to overwrite)')
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(cases, indent=2) + '\n')
    print(f'gen: {type_name} -> {out}  (class {cls}, config from {source})')
    print('gen: next:')
    print(f'  tools/feature_oracles.py {type_name} --cases {out.relative_to(ROOT)} --write')
    print(f'  tools/feature_oracles.py {type_name} --cases {out.relative_to(ROOT)}')


if __name__ == '__main__':
    main()
