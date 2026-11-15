#!/usr/bin/env python3
"""Native 1.21.1 oracle runner for `minecraft:end_city` and `minecraft:mansion`.

Both structures are multi-piece template assemblies, so a case pins four
different things at once:

  * the generation-point admission rule (`getLowestYIn5by5BoxOffset7Blocks`
    over four *base heights*, refused below y=60) and the single
    `Rotation.getRandom` draw that precedes the base-height reads,
  * the piece tree: every `TemplateStructurePiece`'s template name, bounding
    box, rotation, mirror and `genDepth`, in list order,
  * the chunk placement itself, over several chunk boxes sharing one
    `RandomSource` — the writes, the block entities (loot tables and their
    seeds) and the entity payloads,
  * the post-placement RNG state, which for the mansion includes the
    `afterPlace` foundation pass (it draws nothing, but the piece order and the
    marker order before it do).

`--write` regenerates `tests/data/end_city_mansion_cases_1211.json` and
`tests/data/end_city_mansion_1211.tsv`; without it the oracle is re-run and the
result is compared byte for byte with the checked-in fixture.
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
INPUT = ROOT / 'tests/data/end_city_mansion_cases_1211.json'
EXPECTED = ROOT / 'tests/data/end_city_mansion_1211.tsv'

END_CITY = 'datapacks/vanilla/data/minecraft/worldgen/structure/end_city.json'
MANSION = 'datapacks/vanilla/data/minecraft/worldgen/structure/mansion.json'


def flat(height, cx, cz, sea_level=63):
    """A flat world whose columns answer `height` for the whole 5x5 probe box.

    `ScriptedTerrain` defaults a missing column to 70, so the probe box the
    admission rule reads at `chunk*16+7` has to be spelled out explicitly for a
    case whose height is not 70.
    """
    x = cx * 16 + 7
    z = cz * 16 + 7
    heights = {}
    for dx in range(-5, 6):
        for dz in range(-5, 6):
            key = f'{x + dx},{z + dz}'
            heights[key] = height
    return {
        'min_y': -64,
        'height': 384,
        'sea_level': sea_level,
        'heights': heights,
        'top_block': 'minecraft:stone',
        'fill_block': 'minecraft:stone',
    }


def box_chunks(cx, cz, radius):
    """Every chunk in the square around the structure's own chunk."""
    return [[cx + x, cz + z]
            for x in range(-radius, radius + 1)
            for z in range(-radius, radius + 1)]


def build_cases():
    structures = []

    # End cities: the four base-height corners are read at chunk*16+7 with a
    # 5-block box, so a `heights` map only over the 5x5 box still leaves the
    # default (70) for the rest of the world. Several seeds so the tower
    # recursion covers houses, fat towers, bridges and (occasionally) the ship.
    end_city_cases = [
        ('end-city-s11-c0-0', 11, 0, 0, 111, flat(70, 0, 0), 2),
        ('end-city-s7-c3-5', 7, 3, 5, 222, flat(70, 3, 5), 2),
        ('end-city-s23-c-4-2', 23, -4, -2, 333, flat(72, -4, -2), 2),
        ('end-city-s4-c9-9', 4, 9, 9, 444, flat(66, 9, 9), 2),
        ('end-city-s99-c-7-6', 99, -7, -6, 555, flat(80, -7, -6), 2),
        ('end-city-s1234-c1-2', 1234, 1, 2, 666, flat(70, 1, 2), 1),
        # Four corners of the 5x5 probe disagree: the minimum decides admission.
        (
            'end-city-s31-c2-2-tilted',
            31,
            2,
            2,
            777,
            {
                'min_y': -64,
                'height': 384,
                'sea_level': 63,
                'heights': {'23,23': 74, '23,28': 68, '28,23': 71, '28,28': 62},
                'top_block': 'minecraft:stone',
                'fill_block': 'minecraft:stone',
            },
            2,
        ),
        # The admission boundary itself: `getFirstOccupiedHeight` answers 60, so
        # `pos.getY() < 60` admits and `<= 60` would not. Nothing else in the
        # corpus lands on exactly 60.
        ('end-city-s17-c0-0-y60', 17, 0, 0, 1234, flat(61, 0, 0), 1),
        # Refused: the lowest corner is below y=60.
        ('end-city-reject-low', 5, 0, 0, 888, flat(55, 0, 0), 1),
        ('end-city-reject-59', 17, 0, 0, 999, flat(59, 0, 0), 1),
    ]
    for name, seed, cx, cz, post_seed, terrain, radius in end_city_cases:
        structures.append({
            'id': name,
            'kind': 'end_city',
            'structure': END_CITY,
            'world_seed': seed,
            'cx': cx,
            'cz': cz,
            'post_seed': post_seed,
            'chunks': box_chunks(cx, cz, radius),
            'terrain': terrain,
        })
    # None of the seeds above reaches the end ship in the tower recursion, and the
    # ship is the only place the `Elytra` data marker is: the single item frame in
    # its treasure room. The case lists the ship's own 3x2 chunk box instead of a
    # square around the structure's chunk, because the ship hangs ~90 blocks off its
    # tower and a per-chunk pass only delivers a marker inside the chunk it is
    # placing.
    structures.append({
        'id': 'end-city-ship-s0',
        'kind': 'end_city',
        'structure': END_CITY,
        'world_seed': 0,
        'cx': 0,
        'cz': 0,
        'post_seed': 4242,
        'chunks': [[cx, cz] for cx in (3, 4, 5) for cz in (1, 2)],
        'terrain': flat(70, 0, 0),
    })

    # Mansions: the grid RNG stream is shared with the piece placer, so the
    # seeds matter; the third floor only exists when a 1x2 room with a door is
    # present, which not every seed produces.
    mansion_cases = [
        ('mansion-s1-c0-0', 1, 0, 0, 101, flat(70, 0, 0), 2),
        ('mansion-s2-c4-4', 2, 4, 4, 202, flat(70, 4, 4), 2),
        ('mansion-s8-c-3-5', 8, -3, 5, 303, flat(74, -3, 5), 2),
        ('mansion-s42-c12-0', 42, 12, 0, 404, flat(68, 12, 0), 2),
        ('mansion-s777-c-9-9', 777, -9, -9, 505, flat(70, -9, -9), 1),
        # A slope under the mansion: the foundation pass stops at the first
        # non-air cell, so the column heights decide how deep it fills.
        (
            'mansion-s5-c0-0-slope',
            5,
            0,
            0,
            606,
            {
                'min_y': -64,
                'height': 384,
                'sea_level': 63,
                'heights': {
                    '7,7': 70,
                    '-32,-32': 52,
                    '40,40': 96,
                    '-40,40': 63,
                    '40,-40': 80,
                },
                'top_block': 'minecraft:grass_block',
                'fill_block': 'minecraft:stone',
            },
            2,
        ),
        ('mansion-s6-c0-0-y60', 6, 0, 0, 808, flat(61, 0, 0), 1),
        ('mansion-reject-low', 3, 0, 0, 707, flat(58, 0, 0), 1),
    ]
    for name, seed, cx, cz, post_seed, terrain, radius in mansion_cases:
        structures.append({
            'id': name,
            'kind': 'mansion',
            'structure': MANSION,
            'world_seed': seed,
            'cx': cx,
            'cz': cz,
            'post_seed': post_seed,
            'chunks': box_chunks(cx, cz, radius),
            'terrain': terrain,
        })

    return {'structures': structures}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--jar', type=Path, default=DEFAULT_JAR)
    p.add_argument('--write', action='store_true')
    a = p.parse_args()
    data = build_cases()
    inputs = (json.dumps(data, ensure_ascii=False, separators=(',', ':')) + '\n').encode()
    with tempfile.TemporaryDirectory(prefix='mc-end-city-mansion-') as temp:
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
                native / 'EndCityMansionOracle.java',
                'dev.mcspine.tools.EndCityMansionOracle',
                source,
                actual,
                ROOT,
                extra_sources=(
                    native / 'NativeHarness.java',
                    native / 'StructureTemplateOracle.java',
                    native / 'TemplateStructureOracle.java',
                ),
            )
        for path, payload in ((INPUT, inputs), (EXPECTED, actual.read_bytes())):
            if not a.write:
                if not path.exists() or path.read_bytes() != payload:
                    raise SystemExit(f'native end city / mansion mismatch: {path}')
            else:
                path.write_bytes(payload)
        rows = len(data['structures'])
        print(
            f'end city / mansion: {rows} native cases MATCH'
            if not a.write
            else f'end city / mansion: wrote {rows} native cases'
        )


if __name__ == '__main__':
    main()
