#!/usr/bin/env python3
"""Check actual 1.21.1 FossilFeature block/RNG cases; regenerate only with --write.

Inputs are authored scripted worlds plus pinned native resources. Captured initial
cell and frozen-height inputs let the portable Rust test replay exactly the native
read boundary without falling through to generated terrain. This does not compare
server scheduling, tick delivery, packets, light, or arbitrary template entity/shape effects.
"""
import argparse
import json
import os
from pathlib import Path

from feature_oracles import block_tags, fluid_tags
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

REPO = Path(__file__).resolve().parents[1]
CASES = REPO / 'tests/data/fossil_cases_1211.json'
EXPECTED = REPO / 'tests/data/fossil_1211.tsv'
PROTO_EXPECTED = REPO / 'tests/data/fossil_proto_fluids_1211.tsv'
JAVA = REPO / 'tools/structure-oracle/FossilOracle.java'


def validate(text, cases):
    rows = text.splitlines()
    if len(rows) != len(cases) or not rows:
        raise ValueError('native fossil output omitted cases')
    for row, case in zip(rows, cases):
        fields = row.split('\t')
        if len(fields) != 9 or fields[0] != case['id'] or fields[1] not in ('true', 'false'):
            raise ValueError(f'invalid native fossil row for {case["id"]}')
        int(fields[2])
        for field in fields[3:5]:
            if len(field) != 64 or any(c not in '0123456789abcdef' for c in field):
                raise ValueError('invalid native fossil digest')
        if len(fields[5].split(',')) != 3:
            raise ValueError('missing following RNG probes')
        if not isinstance(json.loads(fields[8]), list):
            raise ValueError('missing native proto-fluid tick membership')
        if not json.loads(fields[6]) or not json.loads(fields[7]):
            raise ValueError('missing scripted input boundary')


def validate_proto(text, cases, placement):
    rows = text.splitlines()
    if len(rows) != len(cases):
        raise ValueError('native proto-fluid order omitted cases')
    for row, case, result in zip(rows, cases, placement.splitlines()):
        fields = row.split('\t')
        if len(fields) != 2 or fields[0] != case['id']:
            raise ValueError('invalid native proto-fluid row')
        groups = json.loads(fields[1])
        if not isinstance(groups, dict) or any(not isinstance(v, list) or not v for v in groups.values()):
            raise ValueError('invalid native per-chunk proto-fluid order')
        cells = []
        for chunk, values in groups.items():
            if len(values) != len(set(values)):
                raise ValueError('duplicate native pending fluid')
            for value in values:
                xyz, _ = value.split(' ')
                x, _, z = map(int, xyz.split(','))
                if chunk != f'{x >> 4},{z >> 4}':
                    raise ValueError('wrong native proto-fluid chunk')
                cells.append(value)
        if sorted(cells) != json.loads(result.split('\t')[8]):
            raise ValueError('proto-fluid order differs from native membership')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--jar', type=Path, default=DEFAULT_JAR)
    parser.add_argument('--cases', type=Path, default=CASES)
    parser.add_argument('--expected', type=Path, default=EXPECTED)
    parser.add_argument('--proto-expected', type=Path, default=PROTO_EXPECTED)
    parser.add_argument('--write', action='store_true')
    args = parser.parse_args()
    cases = json.loads(args.cases.read_text())
    if not cases or len({case['id'] for case in cases}) != len(cases):
        raise ValueError('empty or duplicate fossil cases')
    with pinned_runtime(args.jar) as (work, classpath, inner):
        os.environ['MC_NATIVE_BLOCK_TAGS'] = str(block_tags(work / 'block-tags.tsv'))
        os.environ['MC_NATIVE_FLUID_TAGS'] = str(fluid_tags(inner, work / 'fluid-tags.tsv'))
        generated = work / 'fossil.tsv'
        proto = work / 'proto.tsv'
        run_probe(work, classpath, JAVA, 'dev.mcspine.tools.FossilOracle',
                  args.cases.resolve(), generated, proto,
                  extra_sources=(REPO / 'tools/structure-oracle/NativeHarness.java',))
        text = generated.read_text()
        validate(text, cases)
        proto_text = proto.read_text()
        validate_proto(proto_text, cases, text)
        if args.write:
            args.expected.write_text(text)
            args.proto_expected.write_text(proto_text)
        elif args.expected.read_text() != text or args.proto_expected.read_text() != proto_text:
            raise SystemExit('fossil native vectors differ; inspect before an explicit --write')
    print(f'fossil: {len(cases)} actual native cases MATCH' + (' (written)' if args.write else ''))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
