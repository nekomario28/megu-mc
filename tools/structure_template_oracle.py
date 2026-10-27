#!/usr/bin/env python3
"""Compare all pinned template NBT and actual StructureTemplate load ordering."""
import argparse
import io
import json
from pathlib import Path
import zipfile
from block_predicate_oracle import encode
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
VECTORS = ROOT / 'tests/data/structure_templates_1211.tsv'
SOLID = ROOT / 'assets/template_solid_1211.bin'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--jar', type=Path, default=DEFAULT_JAR)
    parser.add_argument('--write', action='store_true')
    args = parser.parse_args()
    with pinned_runtime(args.jar) as (work, cp, inner):
        templates = work / 'templates'
        templates.mkdir()
        with zipfile.ZipFile(io.BytesIO(inner)) as archive:
            prefix = 'data/minecraft/structure/'
            for name in archive.namelist():
                if name.startswith(prefix) and name.endswith('.nbt'):
                    target = templates / name[len(prefix):]
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_bytes(archive.read(name))
        out, states = work / 'templates.tsv', work / 'states.tsv'
        run_probe(work, cp, ROOT / 'tools/structure-oracle/StructureTemplateOracle.java',
                  'dev.mcspine.tools.StructureTemplateOracle', templates, out, states)
        actual = out.read_bytes()
        rows = '\n'.join(line + '\t0' for line in states.read_text().splitlines())
        solid, _ = encode(rows, json.loads((ROOT / 'assets/blocks_1211.json').read_text()))
    for path, data in [(VECTORS, actual), (SOLID, solid)]:
        if args.write:
            path.write_bytes(data)
        elif path.read_bytes() != data:
            raise SystemExit('structure template oracle differs: ' + str(path))
    print('structure template oracle: ' + ('written' if args.write else 'match'))


if __name__ == '__main__':
    main()
