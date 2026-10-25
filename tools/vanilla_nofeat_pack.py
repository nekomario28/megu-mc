#!/usr/bin/env python3
"""Write a vanilla data pack that disables features and carvers.

    tools/vanilla_nofeat_pack.py <1.21.1 server.jar> <world-dir>

`<server.jar>` may be either the self-extracting bundle (the one with
`libraries/` next to it) or the nested `server-1.21.1.jar`. The biome files are
read from the jar itself, not from `datapacks/vanilla`, so the comparison pack is
self-contained and does not depend on whether that generated directory is current.

The pack overrides every biome with `"features": []` and, unless
`--keep-carvers` is passed, `"carvers": {}`. With `--keep-carvers` the world
holds exactly the caves and canyons that the carvers produce - the fixture for
cave parity, which a features-and-carvers-off world cannot say anything about. Load it
in a **fresh** world (`level-name=<new dir>`), because generated chunks are not
regenerated. Then run mc-spine with `MC_SPINE_NOFEAT=1` and diff the two chunks:
what remains is density, the surface material rule and the aquifer.
"""
import io
import json
import os
import sys
import zipfile

NESTED = "META-INF/versions/1.21.1/server-1.21.1.jar"
PACK_FORMAT = 48  # 1.21.1 data pack


def open_server_jar(path):
    z = zipfile.ZipFile(path)
    if NESTED in z.namelist():
        return zipfile.ZipFile(io.BytesIO(z.read(NESTED)))
    return z


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if len(args) != 2:
        print(__doc__)
        return 2
    jar, world = args[0], args[1]
    keep_carvers = "--keep-carvers" in sys.argv
    src = f"{world}/datapacks/nofeat"
    data = f"{src}/data/minecraft/worldgen/biome"
    os.makedirs(data, exist_ok=True)
    with open(f"{src}/pack.mcmeta", "w") as f:
        json.dump({"pack": {"pack_format": PACK_FORMAT,
                            "description": "no features" + ("" if keep_carvers else " or carvers")}}, f)

    z = open_server_jar(jar)
    names = [n for n in z.namelist()
             if n.startswith("data/minecraft/worldgen/biome/") and n.endswith(".json")]
    if not names:
        print(f"{jar}: no biome data; pass the 1.21.1 server jar", file=sys.stderr)
        return 1
    removed = 0
    for name in names:
        biome = json.loads(z.read(name))
        if "attributes" in biome:
            print(f"{name}: not a 1.21.1 biome file", file=sys.stderr)
            return 1
        removed += sum(len(step) for step in biome["features"])
        biome["features"] = [[] for _ in biome["features"]]
        if not keep_carvers:
            biome["carvers"] = {}
        with open(os.path.join(data, os.path.basename(name)), "w") as f:
            json.dump(biome, f)
    print(f"wrote {len(names)} biomes to {src} "
          f"(dropped {removed} placed-feature references"
          f"{', kept carvers' if keep_carvers else ', dropped carvers'})")
    print(f"set level-name to {os.path.basename(world.rstrip('/'))} in server.properties")
    return 0


if __name__ == "__main__":
    sys.exit(main())
