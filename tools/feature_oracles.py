#!/usr/bin/env python3
"""Run one native feature oracle and write or compare its fixture.

Each oracle is a class in `tools/structure-oracle/` that drives the *actual* pinned
1.21.1 feature class through `NativeHarness` (a scripted level plus the class's own
default-method bodies). A fixture is only evidence of the scenes it contains, so
every comparison here is scene-by-scene, not an aggregate.
"""
import argparse
import json
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
HARNESS = ROOT / 'tools/structure-oracle/NativeHarness.java'

# name -> fixture file
ORACLES = {
    'void_start_platform': 'tests/data/void_start_platform_1211.tsv',
    'end_platform': 'tests/data/end_platform_1211.tsv',
    'nether_forest_vegetation': 'tests/data/nether_forest_vegetation_1211.tsv',
    'netherrack_replace_blobs': 'tests/data/netherrack_replace_blobs_1211.tsv',
    'basalt_columns': 'tests/data/basalt_columns_1211.tsv',
    'delta_feature': 'tests/data/delta_feature_1211.tsv',
    'twisting_vines': 'tests/data/twisting_vines_1211.tsv',
    'weeping_vines': 'tests/data/weeping_vines_1211.tsv',
    'coral_tree': 'tests/data/coral_tree_1211.tsv',
    'coral_claw': 'tests/data/coral_claw_1211.tsv',
    'coral_mushroom': 'tests/data/coral_mushroom_1211.tsv',
    'pointed_dripstone': 'tests/data/pointed_dripstone_1211.tsv',
    'huge_brown_mushroom': 'tests/data/huge_brown_mushroom_1211.tsv',
    'huge_red_mushroom': 'tests/data/huge_red_mushroom_1211.tsv',
    'huge_fungus': 'tests/data/huge_fungus_1211.tsv',
    'end_gateway': 'tests/data/end_gateway_1211.tsv',
    'end_spike': 'tests/data/end_spike_1211.tsv',
    'fossil': 'tests/data/fossil_1211.tsv',
    'sculk_patch': 'tests/data/sculk_patch_1211.tsv',
    'bonus_chest': 'tests/data/bonus_chest_1211.tsv',
}


def block_tags(path):
    """Every block tag the pinned pack defines, with nested `#tag` references expanded."""
    base = ROOT / 'datapacks/vanilla/data/minecraft/tags/block'
    raw = {}
    for f in base.rglob('*.json'):
        tag = 'minecraft:' + f.relative_to(base).with_suffix('').as_posix()
        values = json.loads(f.read_text()).get('values', [])
        raw[tag] = [v if isinstance(v, str) else v.get('id') for v in values]

    def expand(tag, seen=()):
        if tag in seen:
            return []
        out = []
        for value in raw.get(tag, []):
            if value.startswith('#'):
                inner = value[1:]
                inner = inner if inner.startswith('minecraft:') else 'minecraft:' + inner
                out += expand(inner, seen + (tag,))
            else:
                out.append(value if value.startswith('minecraft:') else 'minecraft:' + value)
        return out

    lines = [tag + '\t' + ','.join(expand(tag)) for tag in sorted(raw)]
    path.write_text('\n'.join(lines) + '\n')
    return path


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('name', choices=sorted(ORACLES))
    p.add_argument('--jar', type=Path, default=DEFAULT_JAR)
    p.add_argument('--write', action='store_true')
    args = p.parse_args()
    fixture = ROOT / ORACLES[args.name]
    oracle = ROOT / f'tools/structure-oracle/{args.name.title().replace("_", "")}Oracle.java'
    with pinned_runtime(args.jar) as (work, cp, _):
        os.environ['MC_NATIVE_BLOCK_TAGS'] = str(block_tags(work / 'block_tags.tsv'))
        output = work / f'{args.name}.tsv'
        run_probe(work, cp, oracle, 'dev.mcspine.tools.'
                  + oracle.stem, output, extra_sources=(HARNESS,))
        actual = output.read_bytes()
    if args.write:
        fixture.write_bytes(actual)
    elif not fixture.exists() or fixture.read_bytes() != actual:
        raise SystemExit(f'native {args.name} feature differs')
    print(f'{args.name} oracle: ' + ('written' if args.write else 'match'))


if __name__ == '__main__':
    main()
