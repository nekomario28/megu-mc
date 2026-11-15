#!/usr/bin/env python3
"""Native 1.21.1 oracle runner for BlockAgeProcessor, extended StructureTemplate placement,
and the 5 single/small-template structure types (Igloo, NetherFossil, Shipwreck,
OceanRuin, RuinedPortal).
"""
import argparse
import json
import os
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from feature_oracles import block_tags, fluid_tags
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
INPUT = ROOT / 'tests/data/template_structure_cases_1211.json'
EXPECTED = ROOT / 'tests/data/template_structures_1211.tsv'


def state(name, **props):
    return {'Name': name, **({'Properties': props} if props else {})}


def build_cases():
    block_age_blocks = [
        {'pos': [0, 0, 0], 'state': state('minecraft:stone_bricks')},
        {'pos': [1, 0, 0], 'state': state('minecraft:stone')},
        {'pos': [2, 0, 0], 'state': state('minecraft:chiseled_stone_bricks')},
        {'pos': [3, 0, 0], 'state': state('minecraft:oak_stairs', facing='east', half='top', shape='inner_left', waterlogged='true')},
        {'pos': [4, 0, 0], 'state': state('minecraft:stone_brick_stairs', facing='west', half='bottom', shape='straight', waterlogged='true')},
        {'pos': [5, 0, 0], 'state': state('minecraft:stone_brick_slab', type='top', waterlogged='true')},
        {'pos': [6, 0, 0], 'state': state('minecraft:cobblestone_wall', north='low', up='true', waterlogged='true')},
        {'pos': [7, 0, 0], 'state': state('minecraft:obsidian')},
        {'pos': [8, 0, 0], 'state': state('minecraft:mossy_stone_bricks')},
    ]
    block_age = []
    for mossiness in (0.0, 0.25, 0.5, 0.8, 1.0):
        for mode in ('shared', 'positional'):
            for m, r in ((0, 0), (1, 2), (2, 3)):
                block_age.append({
                    'id': f'age-m{mossiness}-{mode}-m{m}-r{r}',
                    'mirror': m,
                    'rotation': r,
                    'pivot': [2, 0, 1],
                    'origin': [17, 64, -9],
                    'seed': 123456789 if mode == 'shared' else 0,
                    'random': mode,
                    'processors': [{'processor_type': 'minecraft:block_age', 'mossiness': mossiness}],
                    'blocks': block_age_blocks,
                })

    # Extended placeInWorld tests
    # Waterlog grid (ApplyWaterlogging vs IgnoreWaterlogging, fixed-point 5-neighbor propagation,
    # double slabs/kelp exclusion, BARRIER clear for NBT blocks, JIGSAW final_state, RandomizableContainer)
    waterlog_cells = {}
    # Source water at (0, 64, 0) adjacent to stair placed at (1, 64, 0), chained to slab at (2, 64, 0),
    # chained to fence at (2, 65, 0), while (1, 65, 0) gets pre-existing source water
    waterlog_cells['0,64,0'] = 'minecraft:water'
    waterlog_cells['5,64,0'] = 'minecraft:water'
    # Pre-existing block at (3, 64, 0) for NBT barrier test
    waterlog_cells['3,64,0'] = 'minecraft:stone'
    waterlog_blocks = [
        {'pos': [1, 0, 0], 'state': state('minecraft:oak_stairs', facing='north', half='bottom', waterlogged='false')},
        {'pos': [2, 0, 0], 'state': state('minecraft:oak_slab', type='bottom', waterlogged='false')},
        {'pos': [2, 1, 0], 'state': state('minecraft:oak_fence', waterlogged='false')},
        # Double slab must NOT get waterlogged even next to water
        {'pos': [4, 0, 0], 'state': state('minecraft:oak_slab', type='double', waterlogged='false')},
        # Pre-waterlogged stair at (6, 0, 0) schedules fluid tick
        {'pos': [6, 0, 0], 'state': state('minecraft:stone_brick_stairs', facing='east', waterlogged='true')},
        # Chest with NBT (RandomizableContainer -> draws LootTableSeed, BARRIER pre-clear)
        {'pos': [3, 0, 0], 'state': state('minecraft:chest', facing='south'),
         'nbt': '{id:"minecraft:chest",LootTable:"minecraft:chests/simple_dungeon"}'},
        # Chest without LootTable NBT (RandomizableContainer STILL draws LootTableSeed!)
        {'pos': [3, 1, 0], 'state': state('minecraft:barrel', facing='up'),
         'nbt': '{id:"minecraft:barrel",Items:[]}'},
        # Furnace with NBT (NOT RandomizableContainer -> must NOT draw RNG)
        {'pos': [4, 1, 0], 'state': state('minecraft:furnace', facing='north'),
         'nbt': '{id:"minecraft:furnace",Items:[]}'},
    ]
    place_extended = [
        {
            'id': 'waterlog-apply-chain',
            'blocks': waterlog_blocks,
            'mirror': 0,
            'rotation': 0,
            'pivot': [0, 0, 0],
            'origin': [0, 64, 0],
            'ignore_waterlogging': False,
            'processors': [],
            'terrain': {'heights': {'0,0': 60}, 'cells': waterlog_cells},
            'world_seed': 42,
            'seed': 98765,
        },
        {
            'id': 'waterlog-ignore',
            'blocks': waterlog_blocks,
            'mirror': 0,
            'rotation': 0,
            'pivot': [0, 0, 0],
            'origin': [0, 64, 0],
            'ignore_waterlogging': True,
            'processors': [],
            'terrain': {'heights': {'0,0': 60}, 'cells': waterlog_cells},
            'world_seed': 42,
            'seed': 98765,
        },
        {
            'id': 'igloo-bottom-entities-and-brewing',
            'template': 'datapacks/vanilla/data/minecraft/structure/igloo/bottom.nbt',
            'mirror': 0,
            'rotation': 1,
            'pivot': [3, 6, 7],
            'origin': [0, 40, 0],
            'ignore_waterlogging': True,
            'processors': [{'processor_type': 'minecraft:block_ignore', 'blocks': [{'Name': 'minecraft:structure_block'}]}],
            'terrain': {'heights': {}},
            'world_seed': 777,
            'seed': 111222,
        },
        {
            'id': 'shipwreck-with-mast-waterlog',
            'template': 'datapacks/vanilla/data/minecraft/structure/shipwreck/with_mast.nbt',
            'mirror': 0,
            'rotation': 2,
            'pivot': [4, 0, 15],
            'origin': [0, 56, 0],
            'ignore_waterlogging': False,
            'processors': [{'processor_type': 'minecraft:block_ignore',
                            'blocks': [{'Name': 'minecraft:air'}, {'Name': 'minecraft:structure_block'}]}],
            'terrain': {'ocean': True, 'heights': {'0,0': 64}},
            'world_seed': 999,
            'seed': 54321,
        },
    ]

    # Full 5 structures
    flat_68 = {'heights': {f'{x},{z}': 68 for x in range(-32, 48) for z in range(-32, 48)}}
    beached_terrain = {
        'heights': {f'{x},{z}': (66 if x < 12 else 63) for x in range(-32, 48) for z in range(-32, 48)},
    }
    ocean_terrain = {
        'ocean': True,
        'heights': {f'{x},{z}': {'ws': 64, 'oc': 52} for x in range(-32, 64) for z in range(-32, 64)},
    }
    ocean_uneven = {
        'ocean': True,
        'heights': {f'{x},{z}': {'ws': 64, 'oc': 55 if (x + z) % 5 != 0 else 44}
                    for x in range(-32, 64) for z in range(-32, 64)},
    }
    # Nether fossil terrain: columns with solid roof/floor bands around y=40..90
    nether_cells = {}
    for x in range(-16, 48):
        for z in range(-16, 48):
            for y in range(28, 35):
                nether_cells[f'{x},{y},{z}'] = 'minecraft:soul_sand' if (x + z) % 2 == 0 else 'minecraft:netherrack'
            for y in range(35, 100):
                nether_cells[f'{x},{y},{z}'] = 'minecraft:air'
            for y in range(100, 128):
                nether_cells[f'{x},{y},{z}'] = 'minecraft:netherrack'
    # Also a failing column where everything is netherrack (no air above floor)
    nether_no_air_cells = {
        f'{x},{y},{z}': 'minecraft:netherrack'
        for x in range(0, 16) for z in range(0, 16) for y in range(28, 128)
    }

    # Underwater ruin with air pocket / gravel bottom for suspicious_gravel & findHeight trigger
    cold_ruin_cells = dict()
    for x in range(-32, 64):
        for z in range(-32, 64):
            for y in range(44, 52):
                cold_ruin_cells[f'{x},{y},{z}'] = 'minecraft:air'
            cold_ruin_cells[f'{x},43,{z}'] = 'minecraft:gravel'
            cold_ruin_cells[f'{x},52,{z}'] = 'minecraft:ice'
    cold_ruin_terrain = {
        'ocean': True,
        'heights': {f'{x},{z}': {'ws': 64, 'oc': 53} for x in range(-32, 64) for z in range(-32, 64)},
        'cells': cold_ruin_cells,
    }

    # Lava terrain for subterranean/nether ruined portal
    lava_cells = {}
    for x in range(-32, 48):
        for z in range(-32, 48):
            for y in range(15, 25):
                lava_cells[f'{x},{y},{z}'] = 'minecraft:lava'
            for y in range(25, 32):
                lava_cells[f'{x},{y},{z}'] = 'minecraft:netherrack'
    nether_portal_terrain = {
        'min_y': 0,
        'height': 128,
        'sea_level': 32,
        'heights': {f'{x},{z}': 75 for x in range(-32, 48) for z in range(-32, 48)},
        'cells': lava_cells,
    }

    structures = [
        # Igloo (both with and without basement across multiple seeds/chunks)
        {
            'id': 'igloo-seed1-c0-0',
            'structure': 'datapacks/vanilla/data/minecraft/worldgen/structure/igloo.json',
            'cold_biome': True,
            'world_seed': 101,
            'cx': 0, 'cz': 0,
            'post_seed': 5551,
            'chunk_bb': [-16, -64, -16, 31, 319, 31],
            'terrain': flat_68,
        },
        {
            'id': 'igloo-seed4-c2-m1',
            'structure': 'datapacks/vanilla/data/minecraft/worldgen/structure/igloo.json',
            'cold_biome': True,
            'world_seed': 4444,
            'cx': 1, 'cz': 0,
            'post_seed': 5552,
            'chunk_bb': [0, -64, -16, 47, 319, 31],
            'terrain': flat_68,
        },
        {
            'id': 'igloo-seed9-chunk-clip',
            'structure': 'datapacks/vanilla/data/minecraft/worldgen/structure/igloo.json',
            'cold_biome': True,
            'world_seed': 9001,
            'cx': 0, 'cz': 0,
            'post_seed': 5553,
            'chunk_bb': [0, -64, 0, 15, 319, 15],
            'terrain': flat_68,
        },
        # Nether fossil
        {
            'id': 'nether-fossil-valid-1',
            'structure': 'datapacks/vanilla/data/minecraft/worldgen/structure/nether_fossil.json',
            'world_seed': 1234,
            'cx': 0, 'cz': 0,
            'post_seed': 7771,
            'chunk_bb': [0, 0, 0, 31, 127, 31],
            'terrain': {'min_y': 0, 'height': 128, 'sea_level': 32, 'cells': nether_cells},
        },
        {
            'id': 'nether-fossil-valid-2',
            'structure': 'datapacks/vanilla/data/minecraft/worldgen/structure/nether_fossil.json',
            'world_seed': 56789,
            'cx': 1, 'cz': 1,
            'post_seed': 7772,
            'chunk_bb': [0, 0, 0, 47, 127, 47],
            'terrain': {'min_y': 0, 'height': 128, 'sea_level': 32, 'cells': nether_cells},
        },
        {
            'id': 'nether-fossil-below-sea',
            'structure': 'datapacks/vanilla/data/minecraft/worldgen/structure/nether_fossil.json',
            'world_seed': 42,
            'cx': 0, 'cz': 0,
            'post_seed': 7773,
            'chunk_bb': [0, 0, 0, 15, 127, 15],
            'terrain': {'min_y': 0, 'height': 128, 'sea_level': 32, 'cells': nether_no_air_cells},
        },
        # Shipwrecks (beached and ocean)
        {
            'id': 'shipwreck-ocean-1',
            'structure': 'datapacks/vanilla/data/minecraft/worldgen/structure/shipwreck.json',
            'world_seed': 31415,
            'cx': 0, 'cz': 0,
            'post_seed': 8881,
            'chunk_bb': [-32, -64, -32, 47, 319, 47],
            'terrain': ocean_terrain,
        },
        {
            'id': 'shipwreck-beached-1',
            'structure': 'datapacks/vanilla/data/minecraft/worldgen/structure/shipwreck_beached.json',
            'world_seed': 27182,
            'cx': 0, 'cz': 0,
            'post_seed': 8882,
            'chunk_bb': [-32, -64, -32, 47, 319, 47],
            'terrain': beached_terrain,
        },
        {
            'id': 'shipwreck-ocean-chunk-split',
            'structure': 'datapacks/vanilla/data/minecraft/worldgen/structure/shipwreck.json',
            'world_seed': 99999,
            'cx': 0, 'cz': 0,
            'post_seed': 8883,
            'chunk_bb': [0, -64, 0, 15, 319, 15],
            'terrain': ocean_uneven,
        },
        # Ocean ruins (cold + warm, cluster + single)
        {
            'id': 'ocean-ruin-warm-1',
            'structure': 'datapacks/vanilla/data/minecraft/worldgen/structure/ocean_ruin_warm.json',
            'world_seed': 11111,
            'cx': 0, 'cz': 0,
            'post_seed': 9991,
            'level_rng_seed': 3331,
            'chunk_bb': [-32, -64, -32, 47, 319, 47],
            'terrain': ocean_terrain,
        },
        {
            'id': 'ocean-ruin-warm-2',
            'structure': 'datapacks/vanilla/data/minecraft/worldgen/structure/ocean_ruin_warm.json',
            'world_seed': 22222,
            'cx': 0, 'cz': 0,
            'post_seed': 9992,
            'level_rng_seed': 3332,
            'chunk_bb': [-32, -64, -32, 47, 319, 47],
            'terrain': ocean_terrain,
        },
        {
            'id': 'ocean-ruin-cold-1',
            'structure': 'datapacks/vanilla/data/minecraft/worldgen/structure/ocean_ruin_cold.json',
            'cold_biome': True,
            'world_seed': 33333,
            'cx': 0, 'cz': 0,
            'post_seed': 9993,
            'level_rng_seed': 3333,
            'chunk_bb': [-32, -64, -32, 47, 319, 47],
            'terrain': cold_ruin_terrain,
        },
        {
            'id': 'ocean-ruin-cold-2',
            'structure': 'datapacks/vanilla/data/minecraft/worldgen/structure/ocean_ruin_cold.json',
            'cold_biome': True,
            'world_seed': 44445,
            'cx': 0, 'cz': 0,
            'post_seed': 9994,
            'level_rng_seed': 3334,
            'chunk_bb': [-32, -64, -32, 47, 319, 47],
            'terrain': ocean_terrain,
        },
    ]

    # All 7 ruined_portal variants + extra seeds
    for variant, cold, terr in (
        ('ruined_portal', False, flat_68),
        ('ruined_portal', True, flat_68),
        ('ruined_portal_desert', False, flat_68),
        ('ruined_portal_jungle', False, flat_68),
        ('ruined_portal_mountain', False, flat_68),
        ('ruined_portal_nether', False, nether_portal_terrain),
        ('ruined_portal_ocean', False, ocean_terrain),
        ('ruined_portal_swamp', False, flat_68),
    ):
        for idx, ws in enumerate((10007, 20011, 30029)):
            structures.append({
                'id': f'{variant}-{"cold" if cold else "warm"}-s{idx}',
                'structure': f'datapacks/vanilla/data/minecraft/worldgen/structure/{variant}.json',
                'cold_biome': cold,
                'world_seed': ws,
                'cx': 0, 'cz': 0,
                'post_seed': 4000 + idx,
                'chunk_bb': [-32, -64, -32, 47, 319, 47],
                'terrain': terr,
            })

    return {
        'block_age': block_age,
        'place_extended': place_extended,
        'structures': structures,
    }


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--jar', type=Path, default=DEFAULT_JAR)
    p.add_argument('--write', action='store_true')
    a = p.parse_args()
    data = build_cases()
    inputs = (json.dumps(data, ensure_ascii=False, separators=(',', ':')) + '\n').encode()
    with tempfile.TemporaryDirectory(prefix='mc-template-structures-') as temp:
        work = Path(temp)
        source = work / INPUT.name
        source.write_bytes(inputs)
        actual = work / EXPECTED.name
        native = ROOT / 'tools/structure-oracle'
        with pinned_runtime(a.jar) as (runtime, cp, inner):
            os.environ['MC_NATIVE_BLOCK_TAGS'] = str(block_tags(work / 'block-tags.tsv'))
            os.environ['MC_NATIVE_FLUID_TAGS'] = str(fluid_tags(inner, work / 'fluid-tags.tsv'))
            run_probe(
                runtime,
                cp,
                native / 'TemplateStructureOracle.java',
                'dev.mcspine.tools.TemplateStructureOracle',
                source,
                actual,
                ROOT,
                extra_sources=(
                    native / 'NativeHarness.java',
                    native / 'StructureTemplateOracle.java',
                ),
            )
        for path, payload in ((INPUT, inputs), (EXPECTED, actual.read_bytes())):
            if not a.write:
                if not path.exists() or path.read_bytes() != payload:
                    raise SystemExit(f'native template structure mismatch: {path}')
            else:
                path.write_bytes(payload)
        total = len(data['block_age']) + len(data['place_extended']) + len(data['structures'])
        print(
            f'template structures: {total} native cases MATCH'
            if not a.write
            else f'template structures: wrote {total} native cases'
        )


if __name__ == '__main__':
    main()
