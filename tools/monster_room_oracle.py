#!/usr/bin/env python3
"""Replay dungeon cells and RNG from the actual pinned MonsterRoomFeature."""
import argparse
from pathlib import Path

from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parent.parent
VECTORS = ROOT / "tests/data/monster_room_1211.tsv"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    parser.add_argument("--write", action="store_true")
    args = parser.parse_args()
    with pinned_runtime(args.jar) as (work, classpath, _):
        output = work / "monster-room.tsv"
        run_probe(work, classpath, ROOT / "tools/monster-room-oracle/MonsterRoomOracle.java",
                  "dev.mcspine.tools.MonsterRoomOracle", output,
                  extra_sources=[ROOT / "tools/vanilla-probe/VanillaBlockTags.java"])
        actual = output.read_bytes()
    if args.write:
        VECTORS.write_bytes(actual)
    elif actual != VECTORS.read_bytes():
        raise SystemExit("monster room oracle: native vectors differ")
    print("monster room oracle: vectors " + ("written" if args.write else "match"))


if __name__ == "__main__":
    main()
