#!/usr/bin/env python3
"""Capture LevelTicks ordering and deduplication from the pinned native jar."""
import argparse
from pathlib import Path
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parent.parent
VECTORS = ROOT / "tests/data/scheduled_tick_1211.tsv"
PROTO_VECTORS = ROOT / "tests/data/proto_tick_1211.tsv"


def validate_proto(data):
    rows = data.decode().splitlines()
    if len(rows) != 18:
        raise ValueError("native proto unpack omitted cases")
    for i, row in enumerate(rows):
        fields = row.split('\t')
        if len(fields) != 8 or fields[0] != str(i) or int(fields[2]) <= 0:
            raise ValueError("invalid proto unpack row")
        int(fields[1])
        for text in fields[3].split(';'):
            if text and len(list(map(int, text.split(',')))) != 2:
                raise ValueError("invalid cold chunk")
        for column in (4, 5, 6):
            for text in fields[column].split(';'):
                if not text:
                    continue
                values = text.split(',')
                if len(values) != 7 or values[0] not in ('W', 'L') or values[5] != 'NORMAL':
                    raise ValueError("invalid native tick input")
                for n in (1, 2, 3, 4, 6):
                    int(values[n])
                if column == 5 and int(values[4]) != 0:
                    raise ValueError("generated input is not zero delay")
        waves = fields[7].split('|')
        if len(waves) != 2 or not any(waves):
            raise ValueError("missing native delivery callbacks")
        for wave in waves:
            for text in wave.split(';'):
                if not text:
                    continue
                values = text.split(',')
                if len(values) != 7 or values[0] not in ('W', 'L') or values[5] != 'NORMAL':
                    raise ValueError("invalid native delivery metadata")
                for n in (1, 2, 3, 4, 6):
                    int(values[n])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    parser.add_argument("--write", action="store_true")
    args = parser.parse_args()
    with pinned_runtime(args.jar) as (work, classpath, _):
        out = work / "scheduled.tsv"
        proto = work / "proto.tsv"
        run_probe(work, classpath, ROOT / "tools/leaf-distance-oracle/ScheduledTickOracle.java",
                  "dev.mcspine.tools.ScheduledTickOracle", out, proto)
        actual = out.read_bytes()
        generated = proto.read_bytes()
        if len(actual.splitlines()) != 48:
            raise ValueError("native scheduled collection omitted cases")
        validate_proto(generated)
    if args.write:
        VECTORS.write_bytes(actual)
        PROTO_VECTORS.write_bytes(generated)
    elif actual != VECTORS.read_bytes() or generated != PROTO_VECTORS.read_bytes():
        raise SystemExit("scheduled tick oracle: native vectors differ")
    print("scheduled tick oracle: vectors " + ("written" if args.write else "match"))


if __name__ == "__main__":
    main()
