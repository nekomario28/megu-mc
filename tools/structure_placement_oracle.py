#!/usr/bin/env python3
"""Capture actual 1.21.1 random-spread and frequency-reduction results."""
import argparse
import io
from pathlib import Path
import zipfile
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
VECTORS = ROOT / 'tests/data/structure_placement_1211.tsv'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--jar', type=Path, default=DEFAULT_JAR)
    parser.add_argument('--write', action='store_true')
    args = parser.parse_args()
    with pinned_runtime(args.jar) as (work, cp, inner):
        data = work / 'sets'
        data.mkdir()
        with zipfile.ZipFile(io.BytesIO(inner)) as archive:
            prefix = 'data/minecraft/worldgen/structure_set/'
            for name in archive.namelist():
                if name.startswith(prefix) and name.endswith('.json'):
                    (data / Path(name).name).write_bytes(archive.read(name))
        out = work / 'placements.tsv'
        run_probe(work, cp, ROOT / 'tools/structure-oracle/StructurePlacementOracle.java',
                  'dev.megu.tools.StructurePlacementOracle', data, out)
        actual = out.read_bytes()
    if args.write:
        VECTORS.write_bytes(actual)
    elif actual != VECTORS.read_bytes():
        raise SystemExit('structure placement oracle: vectors differ')
    print('structure placement oracle: ' + ('written' if args.write else 'match'))


if __name__ == '__main__':
    main()
