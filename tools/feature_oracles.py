#!/usr/bin/env python3
"""Run one native feature oracle and write or compare its fixture.

Each oracle is a class in `tools/structure-oracle/` that drives the *actual* pinned
1.21.1 feature or block-behaviour class through `NativeHarness` (a scripted level plus the class's own
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
    'huge_fungus_crimson': 'tests/data/huge_fungus_crimson_1211.tsv',
    'end_gateway': 'tests/data/end_gateway_1211.tsv',
    'end_spike': 'tests/data/end_spike_1211.tsv',
    'fossil': 'tests/data/fossil_1211.tsv',
    'sculk_patch': 'tests/data/sculk_patch_1211.tsv',
    'sculk_decay': 'tests/data/sculk_decay_1211.tsv',
    'sculk_discharge': 'tests/data/sculk_discharge_1211.tsv',
    'bonus_chest': 'tests/data/bonus_chest_1211.tsv',
    'block_column_up': 'tests/data/block_column_up_1211.tsv',
    'block_column_down': 'tests/data/block_column_down_1211.tsv',
    'freeze_top_layer': 'tests/data/freeze_top_layer_1211.tsv',
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


def fluid_tags(inner_jar, path):
    """Fluid tags from the pinned jar: the installed pack ships none.

    `LevelReader.isWaterAt` is `getFluidState(pos).is(FluidTags.WATER)`, so an unbound
    fluid tag makes water not water and every derived `waterlogged` property false.
    """
    import io
    import zipfile

    out = {}
    with zipfile.ZipFile(io.BytesIO(inner_jar)) as jar:
        for name in jar.namelist():
            prefix = 'data/minecraft/tags/fluid/'
            if not (name.startswith(prefix) and name.endswith('.json')):
                continue
            tag = 'minecraft:' + name[len(prefix):-len('.json')]
            values = json.loads(jar.read(name)).get('values', [])
            out[tag] = [v if isinstance(v, str) else v.get('id') for v in values]
    path.write_text(''.join(f"{tag}\t{','.join(out[tag])}\n" for tag in sorted(out)))
    return path


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('name', choices=sorted(ORACLES))
    p.add_argument('--jar', type=Path, default=DEFAULT_JAR)
    p.add_argument('--write', action='store_true')
    p.add_argument('--cells', type=Path,
                   help='also keep the per-case cell list (the digest cannot name a property)')
    p.add_argument('--cases', type=Path,
                   help='data-driven scenes: run tools/structure-oracle/FeatureOracle.java '
                        'with this cases JSON instead of the per-type oracle class')
    args = p.parse_args()
    if args.cases:
        args.cases = args.cases.resolve()  # the probe runs with its own cwd
    fixture = ROOT / ORACLES[args.name]
    if args.cases:
        oracle = ROOT / 'tools/structure-oracle/FeatureOracle.java'
        main = 'dev.megu.tools.FeatureOracle'
    else:
        oracle = ROOT / f'tools/structure-oracle/{args.name.title().replace("_", "")}Oracle.java'
        main = 'dev.megu.tools.' + oracle.stem
    with pinned_runtime(args.jar) as (work, cp, inner):
        os.environ['MC_NATIVE_BLOCK_TAGS'] = str(block_tags(work / 'block_tags.tsv'))
        os.environ['MC_NATIVE_FLUID_TAGS'] = str(fluid_tags(inner, work / 'fluid_tags.tsv'))
        output = work / f'{args.name}.tsv'
        cells = work / f'{args.name}_cells.tsv'
        argv = ([args.cases] if args.cases else []) + [output] \
            + ([args.cells] if args.cells else [])
        run_probe(work, cp, oracle, main, *argv, extra_sources=(HARNESS,))
        actual = output.read_bytes()
        cells_actual = args.cells.read_bytes() if args.cells else None
    if cells_actual is not None:
        args.cells.write_bytes(cells_actual)
    if args.write:
        fixture.write_bytes(actual)
    elif not fixture.exists() or fixture.read_bytes() != actual:
        raise SystemExit(f'native {args.name} feature differs')
    print(f'{args.name} oracle: ' + ('written' if args.write else 'match'))


if __name__ == '__main__':
    main()
