#!/usr/bin/env python3
"""Run the native 1.21.1 JigsawStructure / JigsawPlacement oracle and write or compare its fixture."""
import argparse
import json
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from feature_oracles import block_tags, fluid_tags
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
HARNESS = ROOT / 'tools/structure-oracle/NativeHarness.java'
ORACLE = ROOT / 'tools/structure-oracle/JigsawOracle.java'
FIXTURE = ROOT / 'tests/data/jigsaw_structures_1211.tsv'
LOCAL_JAR = Path('/home/yuu/mc-spine-local/captures/native/mc-native-overworld-20261111/server.jar')

CASES = [
    # All 10 jigsaw structures with diverse seeds, coordinates, and terrain scenes
    {"id": "village_plains_s1", "structure": "minecraft:village_plains", "seed": 12345, "cx": 0, "cz": 0, "scene": "grass", "flat_top": 64, "max_chunks": 8},
    {"id": "village_plains_s2_sloped", "structure": "minecraft:village_plains", "seed": -987654321, "cx": 3, "cz": -2, "scene": "sloped", "flat_top": 64, "max_chunks": 8},
    {"id": "village_desert_s1", "structure": "minecraft:village_desert", "seed": 424242, "cx": -4, "cz": 5, "scene": "flat", "flat_top": 64, "max_chunks": 8},
    {"id": "village_savanna_s1", "structure": "minecraft:village_savanna", "seed": 777777, "cx": 2, "cz": 2, "scene": "sloped", "flat_top": 64, "max_chunks": 8},
    {"id": "village_snowy_s1", "structure": "minecraft:village_snowy", "seed": 314159265, "cx": -1, "cz": -3, "scene": "grass", "flat_top": 64, "max_chunks": 8},
    {"id": "village_taiga_s1", "structure": "minecraft:village_taiga", "seed": 271828182, "cx": 5, "cz": -4, "scene": "sloped", "flat_top": 64, "max_chunks": 8},
    {"id": "pillager_outpost_s1", "structure": "minecraft:pillager_outpost", "seed": 55555, "cx": 1, "cz": -1, "scene": "grass", "flat_top": 64, "max_chunks": 6},
    {"id": "pillager_outpost_s2", "structure": "minecraft:pillager_outpost", "seed": -11223344, "cx": -6, "cz": 4, "scene": "sloped", "flat_top": 64, "max_chunks": 6},
    {"id": "ancient_city_s1", "structure": "minecraft:ancient_city", "seed": 88888888, "cx": 0, "cz": 0, "scene": "underground", "flat_top": 64, "max_chunks": 8},
    {"id": "ancient_city_s2", "structure": "minecraft:ancient_city", "seed": -42424242, "cx": 4, "cz": -3, "scene": "underground", "flat_top": 64, "max_chunks": 8},
    {"id": "bastion_remnant_s1", "structure": "minecraft:bastion_remnant", "seed": 135792468, "cx": 2, "cz": 1, "scene": "nether", "flat_top": 64, "max_chunks": 8},
    {"id": "bastion_remnant_s2", "structure": "minecraft:bastion_remnant", "seed": -246813579, "cx": -3, "cz": -2, "scene": "nether", "flat_top": 64, "max_chunks": 8},
    {"id": "trail_ruins_s1", "structure": "minecraft:trail_ruins", "seed": 99991, "cx": -2, "cz": 3, "scene": "grass", "flat_top": 64, "max_chunks": 8},
    {"id": "trail_ruins_s2_flooded", "structure": "minecraft:trail_ruins", "seed": 192837465, "cx": 1, "cz": 4, "scene": "flooded", "flat_top": 64, "max_chunks": 8},
    {"id": "trial_chambers_s1", "structure": "minecraft:trial_chambers", "seed": 20241211, "cx": 0, "cz": 0, "scene": "underground", "flat_top": 64, "max_chunks": 8},
    {"id": "trial_chambers_s2_flooded", "structure": "minecraft:trial_chambers", "seed": -70707070, "cx": -3, "cz": 2, "scene": "flooded", "flat_top": 64, "max_chunks": 8},
]


def main():
    default_jar = DEFAULT_JAR if DEFAULT_JAR.exists() else LOCAL_JAR
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--jar', type=Path, default=default_jar)
    p.add_argument('--write', action='store_true')
    args = p.parse_args()

    with pinned_runtime(args.jar) as (work, cp, inner):
        os.environ['MC_NATIVE_BLOCK_TAGS'] = str(block_tags(work / 'block_tags.tsv'))
        os.environ['MC_NATIVE_FLUID_TAGS'] = str(fluid_tags(inner, work / 'fluid_tags.tsv'))
        cases_path = work / 'cases.json'
        cases_path.write_text(json.dumps(CASES, indent=2))
        output = work / 'jigsaw_structures_1211.tsv'
        run_probe(
            work, cp, ORACLE, 'dev.mcspine.tools.JigsawOracle',
            str(cases_path), str(output),
            extra_sources=(HARNESS,),
        )
        actual = output.read_bytes()

    if args.write:
        FIXTURE.parent.mkdir(parents=True, exist_ok=True)
        FIXTURE.write_bytes(actual)
        print(f'jigsaw oracle: wrote {FIXTURE}')
    else:
        if not FIXTURE.exists() or FIXTURE.read_bytes() != actual:
            raise SystemExit('native jigsaw structures differ')
        print('jigsaw oracle: match')


if __name__ == '__main__':
    main()
