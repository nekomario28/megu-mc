#!/usr/bin/env python3
"""Compare the bounded template processing stage with actual native methods.

No Java/native dependency is added to the portable verification gate. Expected
rows bind the complete ordered typed-cell encoding, following shared RNG,
shared float calls, and world reads, not just a placement count.
"""
import argparse
import json
import os
from pathlib import Path
import tempfile

from feature_oracles import block_tags
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
INPUT = ROOT / 'tests/data/template_processor_cases_1211.json'
EXPECTED = ROOT / 'tests/data/template_processors_1211.tsv'


def cases():
    def state(name, **props):
        return {'Name': name, **({'Properties': props} if props else {})}

    def rule(source, target, location='minecraft:always_true'):
        return {'processor_type': 'minecraft:rule', 'rules': [{
            'input_predicate': {'predicate_type': 'minecraft:block_match', 'block': source},
            'location_predicate': ({'predicate_type': location} if location == 'minecraft:always_true'
                                   else {'predicate_type': 'minecraft:block_match', 'block': location}),
            'output_state': state(target)}]}

    blocks = [
        {'pos': [3, 1, -2], 'state': state('coal_ore'), 'marker': 17},
        {'pos': [0, 0, 0], 'state': state('minecraft:stone')},
        {'pos': [-2, 4, 1], 'state': state('minecraft:oak_stairs', facing='north', half='bottom', shape='outer_left', waterlogged='true'), 'marker': -3},
        {'pos': [1, -1, 4], 'state': state('minecraft:bone_block', axis='x')},
        {'pos': [2, 2, 2], 'state': state('minecraft:coal_ore')},
        {'pos': [-3, 1, -1], 'state': state('minecraft:air')},
        {'pos': [0, 3, 2], 'state': state('minecraft:bedrock')},
    ]
    world = [state(n) for n in ('bedrock', 'stone', 'air', 'water', 'dirt')]
    protect = {'processor_type': 'minecraft:protected_blocks', 'value': '#minecraft:features_cannot_replace'}
    rot = lambda integrity, **kw: {'processor_type': 'minecraft:block_rot', 'integrity': integrity, **kw}
    variants = {
        'none': [],
        'rot-all': [rot(1.0)],
        'rot-zero': [rot(0.0)],
        'fossil-rot': json.loads((ROOT / 'datapacks/vanilla/data/minecraft/worldgen/processor_list/fossil_rot.json').read_bytes())['processors'],
        'fossil-coal': json.loads((ROOT / 'datapacks/vanilla/data/minecraft/worldgen/processor_list/fossil_coal.json').read_bytes())['processors'],
        'fossil-diamonds': json.loads((ROOT / 'datapacks/vanilla/data/minecraft/worldgen/processor_list/fossil_diamonds.json').read_bytes())['processors'],
        # BlockRot must test the ORIGINAL state after a preceding rule changes it.
        'original-not-current': [rule('stone', 'coal_ore'), rot(0.0, rottable_blocks=['coal_ore'])],
        'current-rule-chain': [rule('stone', 'coal_ore'), rule('coal_ore', 'deepslate_diamond_ore')],
        'world-location': [rule('coal_ore', 'bone_block', 'stone'), protect],
        'protect-before-rot': [protect, rot(0.9)],
        'empty-rottable': [rot(0.0, rottable_blocks=[])],
    }
    result = []
    for m in range(3):
        for r in range(4):
            for variant, processors in variants.items():
                for mode in ('shared', 'positional'):
                    result.append({'id': f'{variant}-m{m}-r{r}-{mode}',
                                   'mirror': m, 'rotation': r, 'pivot': [2, 0, -3],
                                   'origin': [13, 63, -11], 'clip_out': bool((m + r) % 2),
                                   'seed': -1 if (m + r) % 2 else 42, 'random': mode,
                                   'blocks': blocks, 'world': world, 'processors': processors})
    for integrity in (0.0, 1.0, 1.00000001):
        result.append({'id': f'zero-draw-{integrity}', 'mirror': 0, 'rotation': 0,
                       'pivot': [0, 0, 0], 'origin': [0, 0, 0], 'clip_out': False,
                       'seed': 60007, 'random': 'zero', 'blocks': blocks, 'world': world,
                       'processors': [rot(integrity)]})
    extreme = dict(result[12], id='wrapped-world-coordinates', origin=[2147483646, -2147483648, 2147483647])
    result.append(extreme)
    for name, processors, mirror, rotation, actual_world in (
        ('skull_1', variants['fossil-rot'], 1, 1, [state('minecraft:air')]),
        ('skull_1_coal', variants['fossil-diamonds'], 2, 3, world),
    ):
        result.append({'id': f'actual-{name}', 'template': f'datapacks/vanilla/data/minecraft/structure/fossil/{name}.nbt',
                       'mirror': mirror, 'rotation': rotation, 'pivot': [0, 0, 0], 'origin': [0, 0, 0],
                       'clip_out': True, 'seed': 0, 'random': 'positional', 'world': actual_world,
                       'processors': processors})
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--jar', type=Path, default=DEFAULT_JAR)
    p.add_argument('--write', action='store_true')
    a = p.parse_args()
    inputs = (json.dumps(cases(), ensure_ascii=False, separators=(',', ':')) + '\n').encode()
    with tempfile.TemporaryDirectory(prefix='mc-template-processors-') as temp:
        work = Path(temp)
        source = work / INPUT.name
        source.write_bytes(inputs)
        actual = work / EXPECTED.name
        os.environ['MC_NATIVE_BLOCK_TAGS'] = str(block_tags(work / 'block-tags.tsv'))
        native = ROOT / 'tools/structure-oracle'
        with pinned_runtime(a.jar) as (runtime, cp, _):
            run_probe(runtime, cp, native / 'TemplateProcessorOracle.java',
                      'dev.megu.tools.TemplateProcessorOracle', source, actual, ROOT,
                      extra_sources=(native / 'NativeHarness.java', native / 'StructureTemplateOracle.java'))
        for path, data in ((INPUT, inputs), (EXPECTED, actual.read_bytes())):
            if not a.write:
                if path.read_bytes() != data:
                    raise SystemExit(f'native template processor mismatch: {path}')
            else:
                path.write_bytes(data)
        print(f'template processors: {len(cases())} native cases MATCH' if not a.write
              else f'template processors: wrote {len(cases())} native cases')


if __name__ == '__main__':
    main()
