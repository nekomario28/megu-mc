#!/usr/bin/env python3
"""Structures must still decorate a datapack with no placed biome features."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
FAIL = 'mineshaft dispatch: FAIL structures without placed features'


def main():
    binary = Path(sys.argv[1]).resolve()
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(('MEGU_', 'SPC_', 'FSEED_', 'BIOMEF_', 'FEATURE_', 'MINESHAFT_'))}
    with tempfile.TemporaryDirectory(prefix='mineshaft-dispatch-') as tmp:
        cwd = Path(tmp)
        (cwd / 'assets').symlink_to(ROOT / 'assets', target_is_directory=True)
        packs = cwd / 'datapacks'
        packs.mkdir()
        (packs / 'vanilla').symlink_to(ROOT / 'datapacks/vanilla', target_is_directory=True)
        target = packs / 'zz_empty_features/data/minecraft/worldgen/biome'
        target.mkdir(parents=True)
        for source in (ROOT / 'datapacks/vanilla/data/minecraft/worldgen/biome').glob('*.json'):
            biome = json.loads(source.read_text())
            biome['features'] = []
            (target / source.name).write_text(json.dumps(biome))
        assert len(list(target.glob('*.json'))) == 64, 'test overlay did not cover the biome registry'
        dump, canon = cwd / 'chunk.bin', cwd / 'chunk.tsv'
        env.update(MEGU_WG_PROFILE='0', MEGU_SEED='4242', MEGU_WG_DUMP_N='1',
                   MEGU_WG_DUMP_X='3', MEGU_WG_DUMP_Z='-2',
                   MEGU_WG_DUMP=str(dump), MEGU_WG_DUMP_CANON=str(canon),
                   FEATURE_ATTEMPT_TRACE='1')
        result = subprocess.run([str(binary)], cwd=cwd, env=env,
                                capture_output=True, text=True, timeout=90)
        assert result.returncode == 0, result.stdout + result.stderr
        assert dump.stat().st_size == 98304 * 4
        cells = {}
        for line in canon.read_text().splitlines():
            if not line or line.startswith('#'):
                continue
            x, y, z, state = line.split('\t')
            cells[int(x), int(y), int(z)] = state
        if any(cells.get((x, 21, -17)) != 'minecraft:oak_planks' for x in [62, 63]):
            print(FAIL, flush=True)
            raise AssertionError('empty placed-feature order suppressed the procedural mineshaft floor')
        log = result.stdout + result.stderr
        assert 'SPASS ' in log and 'SATTEMPT ' not in log, 'test overlay did not disable placed features'
        assert not list(cwd.glob('world_overlay.bin*')), 'offline dump entered persistence'
    print('mineshaft dispatch: PASS')


if __name__ == '__main__':
    main()
