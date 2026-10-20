#!/usr/bin/env python3
"""Write a vanilla data pack that leaves exactly one ore feature running.

    tools/vanilla_ore_isolate.py <server.jar> <world-dir> <placed-feature>

Every other `ore_*` placed feature is replaced with `count: 0`, and the biome
files are copied verbatim, so the step list keeps its length and order - which
matters, because a placed feature's seed is `population + index + 10000 * step`
and muting a feature by *removing* it from the list would renumber the rest.

Why: comparing an ore feature's cell set against a world that also contains
dirt, gravel and every other vein cannot say which cells came from which
feature. With one feature left, a chunk's blocks *are* that feature's output,
so vein positions and shapes can be read off directly (see
tools/vanilla_region_blocks.py for the reader).
"""
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from vanilla_nofeat_pack import open_server_jar, PACK_FORMAT  # noqa: E402

PACK = "oreisolate"


def main():
    if len(sys.argv) != 4:
        print(__doc__)
        return 2
    jar, world, keep = sys.argv[1], sys.argv[2], sys.argv[3]
    z = open_server_jar(jar)
    out = os.path.join(world, "datapacks", PACK)
    biome_dir = os.path.join(out, "data/minecraft/worldgen/biome")
    pf_dir = os.path.join(out, "data/minecraft/worldgen/placed_feature")
    os.makedirs(biome_dir, exist_ok=True)
    os.makedirs(pf_dir, exist_ok=True)
    with open(os.path.join(out, "pack.mcmeta"), "w") as f:
        json.dump({"pack": {"pack_format": PACK_FORMAT,
                            "description": f"only {keep}"}}, f)
    biomes = 0
    for name in z.namelist():
        if name.startswith("data/minecraft/worldgen/biome/") and name.endswith(".json"):
            with open(os.path.join(biome_dir, os.path.basename(name)), "wb") as f:
                f.write(z.read(name))
            biomes += 1
    muted = 0
    for name in z.namelist():
        if not name.startswith("data/minecraft/worldgen/placed_feature/") or not name.endswith(".json"):
            continue
        base = os.path.basename(name)[:-5]
        if not base.startswith("ore_") or base == keep:
            continue
        orig = json.loads(z.read(name))
        with open(os.path.join(pf_dir, base + ".json"), "w") as f:
            # Keep the configured-feature reference: the placed name and the
            # configured name differ for several ores (ore_coal_upper ->
            # minecraft:ore_coal), and a wrong reference fails registry load.
            json.dump({"feature": orig["feature"],
                       "placement": [{"type": "minecraft:count", "count": 0}]}, f)
        muted += 1
    print(f"wrote {biomes} biomes, muted {muted} placed features, kept {keep}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
