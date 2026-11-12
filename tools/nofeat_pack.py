#!/usr/bin/env python3
"""Write a datapack that strips features and carvers from every biome.

  tools/nofeat_pack.py <vanilla_datapack_dir> <out_dir>

Why this exists: comparing a *terrain* chunk needs a world where generation stopped after
the density/aquifer/material stage. A server cannot be asked for that, but it can be given
a datapack that empties every biome's `features` and `carvers`: with nothing to place,
generation produces terrain only, and the saved chunk is the stage the port's
`terrain_cell` writes. The End and Nether corpora were captured that way; this rebuilds the
pack from the pinned vanilla data instead of hand-editing it.

The counts are printed so the run is auditable: every dropped reference is a feature the
comparison will not see.
"""
import argparse
import json
import sys
from pathlib import Path

STEPS = 11  # GenerationStep.Decorated .. TopLayer, each a list of placed features
# 1.21.1's *data* pack format is 48; 34 is the resource-pack format for the same release.
# A wrong number still loads (the server reported "Found new data pack … loading it
# automatically" and the stripped world had no ores), but it is the wrong declaration.
DATA_PACK_FORMAT_1211 = 48


def strip(biome):
    dropped = 0
    for step in biome.get("features") or []:
        dropped += len(step or [])
    biome["features"] = [[] for _ in range(STEPS)]
    dropped_carvers = sum(len(v or []) for v in (biome.get("carvers") or {}).values())
    biome["carvers"] = {}
    return dropped, dropped_carvers


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("datapack")
    ap.add_argument("out")
    args = ap.parse_args()
    src = Path(args.datapack)
    out = Path(args.out)
    biomes = sorted((src / "data/minecraft/worldgen/biome").glob("*.json"))
    if not biomes:
        print(f"no biome json under {src}", file=sys.stderr)
        return 2
    target = out / "data/minecraft/worldgen/biome"
    target.mkdir(parents=True, exist_ok=True)
    dropped = dropped_carvers = 0
    for path in biomes:
        biome = json.loads(path.read_text())
        d, dc = strip(biome)
        dropped += d
        dropped_carvers += dc
        (target / path.name).write_text(json.dumps(biome, separators=(",", ":")))
    (out / "pack.mcmeta").write_text(
        json.dumps(
            {
                "pack": {
                    "pack_format": DATA_PACK_FORMAT_1211,
                    "description": "terrain only: features and carvers removed",
                }
            }
        )
    )
    print(
        f"wrote {len(biomes)} biomes to {out} "
        f"(dropped {dropped} placed-feature references, dropped {dropped_carvers} carver entries)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
