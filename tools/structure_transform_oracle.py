#!/usr/bin/env python3
"""Capture native template transforms and map native state identities to wire IDs."""
import argparse
import json
from pathlib import Path
import re
import struct
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
VECTORS = ROOT / 'tests/data/structure_transform_1211.tsv'
STATES = ROOT / 'assets/structure_state_transforms_1211.bin'


def encode(text, blocks):
    rows = [row.split('\t') for row in text.splitlines()]
    wire = {}
    for block in blocks:
        for state in range(block['minStateId'], block['maxStateId'] + 1):
            offset, properties = state-block['minStateId'], []
            for prop in reversed(block['states']):
                values = prop.get('values') or ['true', 'false']
                properties.append((prop['name'], str(values[offset % len(values)])))
                offset //= len(values)
            key = (block['name'], tuple(sorted(properties)))
            if key in wire:
                raise ValueError('duplicate wire state')
            wire[key] = state
    native = []
    seen = set()
    for row in rows:
        if len(row) != 13:
            raise ValueError('incomplete native state transforms')
        match = re.fullmatch(r'Block\{minecraft:(.*?)\}(?:\[(.*)\])?', row[0])
        if not match:
            raise ValueError('invalid native state identity')
        name, props = match.groups()
        key = (name, tuple(sorted(tuple(p.split('=')) for p in props.split(','))) if props else ())
        if key in seen or key not in wire:
            raise ValueError('duplicate or unmatched native state')
        seen.add(key)
        native.append(wire[key])
    if len(native) != len(wire) or set(native) != set(range(len(wire))):
        raise ValueError('incomplete state coverage')
    output = [None] * len(wire)
    for i, row in enumerate(rows):
        targets = list(map(int, row[1:]))
        if any(v < 0 or v >= len(native) for v in targets):
            raise ValueError('transformed native state outside registry')
        output[native[i]] = b''.join(struct.pack('>I', native[v]) for v in targets)
    return b''.join(output)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--jar', type=Path, default=DEFAULT_JAR)
    parser.add_argument('--write', action='store_true')
    args = parser.parse_args()
    with pinned_runtime(args.jar) as (work, cp, _):
        vectors, states = work/'transforms.tsv', work/'states.tsv'
        run_probe(work, cp, ROOT/'tools/structure-oracle/StructureTransformOracle.java',
                  'dev.megu.tools.StructureTransformOracle', vectors, states)
        actual = vectors.read_bytes()
        mapping = encode(states.read_text(), json.loads((ROOT/'assets/blocks_1211.json').read_text()))
    for path, data in [(VECTORS, actual), (STATES, mapping)]:
        if args.write:
            path.write_bytes(data)
        elif path.read_bytes() != data:
            raise SystemExit('native structure transforms differ: ' + str(path))
    print('structure transform oracle: ' + ('written' if args.write else 'match'))


if __name__ == '__main__':
    main()
