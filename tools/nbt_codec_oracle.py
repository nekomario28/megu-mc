#!/usr/bin/env python3
"""Pin NbtIo typed values, malformed input and Java modified UTF-8 behavior."""
import argparse
from pathlib import Path
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
VECTORS = ROOT/'tests/data/nbt_codec_1211.tsv'


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--jar', type=Path, default=DEFAULT_JAR)
    p.add_argument('--write', action='store_true')
    args = p.parse_args()
    with pinned_runtime(args.jar) as (work, cp, _):
        output = work/'nbt.tsv'
        run_probe(work, cp, ROOT/'tools/structure-oracle/NbtCodecOracle.java',
                  'dev.mcspine.tools.NbtCodecOracle', output,
                  extra_sources=(ROOT/'tools/structure-oracle/StructureTemplateOracle.java',))
        actual = output.read_bytes()
    if args.write:
        VECTORS.write_bytes(actual)
    elif VECTORS.read_bytes() != actual:
        raise SystemExit('native NBT codec differs')
    print('NBT codec oracle: ' + ('written' if args.write else 'match'))


if __name__ == '__main__':
    main()
