#!/usr/bin/env python3
"""Check native DiscreteVoxelShape face order; replacement requires --write."""
import argparse
import json
from pathlib import Path
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

REPO = Path(__file__).resolve().parents[1]

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--jar', type=Path, default=DEFAULT_JAR)
    parser.add_argument('--cases', type=Path, default=REPO / 'tests/data/voxel_face_cases_1211.json')
    parser.add_argument('--expected', type=Path, default=REPO / 'tests/data/voxel_faces_1211.tsv')
    parser.add_argument('--write', action='store_true')
    args = parser.parse_args()
    cases = json.loads(args.cases.read_text())
    if not cases or len({c['id'] for c in cases}) != len(cases):
        raise ValueError('empty or duplicate voxel cases')
    with pinned_runtime(args.jar) as (work, classpath, _):
        output = work / 'faces.tsv'
        run_probe(work, classpath, REPO / 'tools/structure-oracle/VoxelFaceOracle.java',
                  'dev.megu.tools.VoxelFaceOracle', args.cases.resolve(), output,
                  extra_sources=(REPO / 'tools/structure-oracle/NativeHarness.java',))
        generated = output.read_text()
        rows = generated.splitlines()
        if len(rows) != len(cases):
            raise ValueError('native voxel output omitted cases')
        for case, row in zip(cases, rows):
            f = row.split('\t')
            if len(f) != 3 or f[0] != case['id'] or int(f[1]) < 0 or len(f[2]) != 64 or any(c not in '0123456789abcdef' for c in f[2]):
                raise ValueError('invalid native voxel row')
        if args.write:
            args.expected.write_text(generated)
        elif args.expected.read_text() != generated:
            raise SystemExit('native voxel faces differ; inspect before --write')
    print(f'voxel faces: {len(cases)} native cases ' + ('WRITTEN' if args.write else 'MATCH'))
    return 0

if __name__ == '__main__':
    raise SystemExit(main())
