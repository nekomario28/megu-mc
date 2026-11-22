#!/usr/bin/env python3
"""Exercise the offline population-plan scope through the real server CLI."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def main():
    binary = Path(sys.argv[1]).resolve()
    core = [(x, z) for x in range(-1, 2) for z in range(-1, 2)]
    recorded = [(3, 0), *reversed(core)]
    base = {k: v for k, v in os.environ.items()
            if not k.startswith(('MEGU_', 'SPC_', 'FSEED_', 'BIOMEF_', 'FEATURE_'))}
    base.update(MEGU_SEED='0', MEGU_WG_PROFILE='0', MEGU_WG_DUMP_N='1',
                MEGU_WG_DUMP_X='0', MEGU_WG_DUMP_Z='0')
    with tempfile.TemporaryDirectory(prefix='population-plan-contract-') as tmp:
        directory = Path(tmp)

        def run(name, order, scope, expected, marker, *, profile=True):
            plan, dump = directory / f'{name}.tsv', directory / f'{name}.bin'
            plan.write_text(''.join(f'{x}\t{z}\n' for x, z in order))
            env = dict(base, MEGU_POP_ORDER=str(plan), MEGU_WG_DUMP=str(dump))
            if scope is not None:
                env['MEGU_POP_ORDER_SCOPE'] = scope
            if not profile:
                env.pop('MEGU_WG_PROFILE')
            try:
                result = subprocess.run([str(binary)], cwd=ROOT if profile else directory, env=env,
                                        capture_output=True, text=True, timeout=45 if profile else 2)
            except subprocess.TimeoutExpired:
                if not profile:
                    print('population plan: FAIL missing-profile', flush=True)
                raise
            log = result.stdout + result.stderr
            if not profile and (result.returncode != expected or marker not in log or dump.exists()):
                print('population plan: FAIL missing-profile', flush=True)
            assert result.returncode == expected, f'{name}: exit {result.returncode}\n{log[-1500:]}'
            assert marker in log, f'{name}: missing {marker!r}\n{log[-1500:]}'
            if expected == 0:
                assert dump.stat().st_size == 16 * 16 * 384 * 4, name
            else:
                assert not dump.exists(), f'{name}: rejected plan published a dump'

        run('recorded', recorded, 'recorded', 0, '10 source chunks executed (scope=recorded)')
        run('default-exact', recorded, None, 2, 'outside')
        run('explicit-exact', recorded, 'exact', 2, 'outside')
        run('missing-core', recorded[:-1], 'recorded', 2, 'incomplete population footprint')
        run('duplicate-extra', [*recorded, (3, 0)], 'recorded', 2, 'duplicate population chunk')
        run('unknown-scope', core, 'unknown', 2, 'invalid MEGU_POP_ORDER_SCOPE')
        run('missing-profile', core, 'exact', 2,
            'MEGU_WG_DUMP requires MEGU_WG_PROFILE', profile=False)
        assert not list(directory.glob('world_overlay.bin*')), 'diagnostic entered persistence path'
    print('population plan: PASS')


if __name__ == '__main__':
    main()
