#!/usr/bin/env python3
"""Export the pinned 1.21.1 physical facts for every protocol block state.

One 5-byte record per state id (sid), in the order `assets/blocks_1211.json` defines:

    byte 0  flags
            0x01 isAir()
            0x02 liquid()             the block is a liquid, not "holds a fluid state"
            0x04 blocksMotion()
            0x08 canOcclude()
            0x10 isSolidRender(level, pos)
            0x20 getCollisionShape(level, pos).isEmpty()
            0x40 isCollisionShapeFullBlock(level, pos)
            0x80 getFluidState().isEmpty()
    byte 1  lightEmission << 4 | lightBlock        both 0..15
    bytes 2..4  isFaceSturdy(level, pos, direction, SupportType), 18 bits,
                bit index support * 6 + direction, supports (FULL, CENTER, RIGID),
                directions (down, up, north, south, west, east)

Every column is a `BlockBehaviour.BlockStateBase` answer, so the table is a property of the
state: `initCache()` resolves all of them when the state is constructed, and sampling a stone
world, an air world and `EmptyBlockGetter.INSTANCE` produced identical bytes for all 34 columns.

Rows come from `tools/block-state-facts-oracle/BlockStateFactsOracle.java`; that class also
asserts a set of named anchors, so a drifted obfuscated name or enum order fails the export
instead of writing a plausible table.
"""
import argparse
import json
from pathlib import Path

from block_predicate_oracle import native_key, wire_states
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parent.parent
ASSET = ROOT / "assets/block_state_facts_1211.bin"
RECORD = 5
FACE_BITS = 18


def encode(rows, blocks):
    native = {}
    for row in rows.splitlines():
        fields = row.split("\t")
        if len(fields) != 5:
            raise ValueError("invalid native state: " + row)
        text, flags, emission, light_block, faces = fields
        if not all(f.isdigit() for f in (flags, emission, light_block, faces)):
            raise ValueError("invalid native state: " + row)
        if (int(flags) >= 1 << 8 or int(emission) >= 16 or int(light_block) >= 16
                or int(faces) >= 1 << FACE_BITS):
            raise ValueError("native state out of range: " + row)
        key = native_key(text)
        if key in native:
            raise ValueError("duplicate native state: " + text)
        packed = int(faces)
        native[key] = bytes((int(flags), int(emission) << 4 | int(light_block),
                             packed & 0xFF, (packed >> 8) & 0xFF, (packed >> 16) & 0xFF))
    out = bytearray()
    states = 0
    for state, key in wire_states(blocks):
        states += 1
        if state != states - 1:
            raise ValueError(f"wire state {state} is out of order")
        try:
            out += native.pop(key)
        except KeyError:
            raise ValueError(f"wire state {state} {key} has no native row") from None
    if native or len(out) != states * RECORD:
        raise ValueError("native/wire state sets differ")
    return bytes(out)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    parser.add_argument("--write", action="store_true")
    args = parser.parse_args()
    with pinned_runtime(args.jar) as (work, classpath, _):
        output = work / "block-state-facts.tsv"
        run_probe(work, classpath,
                  ROOT / "tools/block-state-facts-oracle/BlockStateFactsOracle.java",
                  "dev.mcspine.tools.BlockStateFactsOracle", output)
        data = encode(output.read_text(), json.loads((ROOT / "assets/blocks_1211.json").read_text()))
    if args.write:
        ASSET.write_bytes(data)
    elif data != ASSET.read_bytes():
        raise SystemExit(f"{ASSET.name} differs from the pinned classes")
    print(f"block state facts oracle: {len(data) // RECORD} states "
          + ("written" if args.write else "match"))


if __name__ == "__main__":
    main()
