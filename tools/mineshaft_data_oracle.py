#!/usr/bin/env python3
"""Check actual native spawner and deferred chest-minecart loot NBT; never ticks a world."""
import argparse
import json
from pathlib import Path
from vanilla_java import DEFAULT_JAR, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parent.parent
FILES = {"spawner.nbt": "mineshaft_spawner_1211.nbt",
         "chest-minecart.nbt": "mineshaft_chest_minecart_1211.nbt",
         "rng.json": "mineshaft_data_1211.json"}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    p.add_argument("--output-dir", type=Path, help="explicit fixture destination; default only compares")
    p.add_argument("--reference-dir", type=Path, default=ROOT / "tests/data", help="read-only fixture reference")
    a = p.parse_args()
    with pinned_runtime(a.jar) as (work, cp, _):
        output = work / "data"
        run_probe(work, cp, ROOT / "tools/structure-oracle/MineshaftDataOracle.java",
                  "dev.megu.tools.MineshaftDataOracle", output,
                  extra_sources=[ROOT / "tools/structure-oracle/NativeHarness.java"])
        files = {target: (output / source).read_bytes() for source, target in FILES.items()}
    meta = json.loads(files["mineshaft_data_1211.json"])
    if meta["before"] != meta["after"]:
        raise SystemExit("native fresh spawner setter consumed placement RNG")
    if a.output_dir:
        a.output_dir.mkdir(parents=True, exist_ok=True)
        for name, data in files.items():
            (a.output_dir / name).write_bytes(data)
    elif any((a.reference_dir / name).read_bytes() != data for name, data in files.items()):
        raise SystemExit("mineshaft entity data differ (no files changed)")
    print("native mineshaft NBT and setter RNG: " + ("written" if a.output_dir else "match"))


if __name__ == "__main__":
    main()
