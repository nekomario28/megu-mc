#!/usr/bin/env python3
"""Write a data pack that isolates one deep-dark feature, for sculk comparisons.

    tools/vanilla_sculk_pack.py <1.21.1 server.jar> <world-dir> --variant veins
    tools/vanilla_sculk_pack.py <1.21.1 server.jar> <world-dir> --variant one-patch

Why: `sculk_vein` (multiface_growth, 204..250 per chunk) runs *before*
`sculk_patch_deep_dark` in step 7 of the deep-dark feature list and shares the same
decorator stream, so a draw-count difference in either one moves the other. Comparing
"all sculk cells" cannot say which. Two variants make the two separable:

- `veins`: the deep-dark biome's step 7 keeps only `sculk_vein`, so `sculk` blocks in
  the capture can only have come from the veil feature - and any `sculk` cell that *is*
  there is itself evidence the patch ran.
- `one-patch`: `sculk_patch_deep_dark` is overridden with `count: 1`, so one chunk
  carries at most one patch and its cells can be compared exactly.

The same override has to be applied to the port's own pack
(`datapacks/vanilla/data/minecraft/worldgen/...`); this script prints the two paths it
wrote so the caller can copy them, and never touches the port's pack itself.
"""
import io
import json
import os
import sys
import zipfile

NESTED = "META-INF/versions/1.21.1/server-1.21.1.jar"
PACK_FORMAT = 48  # 1.21.1 data pack
BIOME = "data/minecraft/worldgen/biome/deep_dark.json"
PLACED = "data/minecraft/worldgen/placed_feature/sculk_patch_deep_dark.json"
PATCH = "minecraft:sculk_patch_deep_dark"
VEIN = "minecraft:sculk_vein"


def open_server_jar(path):
    z = zipfile.ZipFile(path)
    if NESTED in z.namelist():
        return zipfile.ZipFile(io.BytesIO(z.read(NESTED)))
    return z


def main():
    argv = sys.argv[1:]
    if "--variant" not in argv:
        print(__doc__)
        return 2
    at = argv.index("--variant")
    variant = argv[at + 1] if at + 1 < len(argv) else ""
    args = [a for i, a in enumerate(argv) if i not in (at, at + 1)]
    if len(args) != 2:
        print(__doc__)
        return 2
    jar, world = args
    if variant not in ("veins", "one-patch"):
        print(f"unknown variant {variant}", file=sys.stderr)
        return 2

    src = f"{world}/datapacks/sculk-isolation"
    z = open_server_jar(jar)
    for name in (BIOME, PLACED):
        if name not in z.namelist():
            print(f"{jar}: {name} is missing; pass the 1.21.1 server jar", file=sys.stderr)
            return 1

    biome = json.loads(z.read(BIOME))
    if "attributes" in biome:
        print(f"{BIOME}: not a 1.21.1 biome file", file=sys.stderr)
        return 1
    step = None
    for i, entries in enumerate(biome["features"]):
        if PATCH in entries:
            step = i
    if step is None:
        print(f"{BIOME}: {PATCH} is not in the feature list", file=sys.stderr)
        return 1
    removed = 0
    if variant == "veins":
        before = len(biome["features"][step])
        biome["features"][step] = [f for f in biome["features"][step] if f != PATCH]
        removed = before - len(biome["features"][step])
        if not removed:
            print(f"{BIOME}: nothing removed", file=sys.stderr)
            return 1

    written = []
    data = f"{src}/data/minecraft/worldgen"
    os.makedirs(f"{data}/biome", exist_ok=True)
    with open(f"{src}/pack.mcmeta", "w") as f:
        json.dump({"pack": {"pack_format": PACK_FORMAT,
                            "description": f"sculk isolation: {variant}"}}, f)
    with open(f"{data}/biome/deep_dark.json", "w") as f:
        json.dump(biome, f)
    written.append(f"{data}/biome/deep_dark.json")

    if variant == "one-patch":
        placed = json.loads(z.read(PLACED))
        patched = False
        for mod in placed.get("placement", []):
            if mod.get("type") == "minecraft:count" and not isinstance(mod.get("count"), dict):
                mod["count"] = 1
                patched = True
        if not patched:
            print(f"{PLACED}: no plain count modifier to narrow", file=sys.stderr)
            return 1
        os.makedirs(f"{data}/placed_feature", exist_ok=True)
        with open(f"{data}/placed_feature/sculk_patch_deep_dark.json", "w") as f:
            json.dump(placed, f)
        written.append(f"{data}/placed_feature/sculk_patch_deep_dark.json")

    print(f"sculk isolation pack ({variant}) written under {src}")
    for path in written:
        print(f"  port copy: {path.removeprefix(src + '/')}")
    print("  load it in a fresh world; the port needs the same files under "
          "datapacks/vanilla/data/minecraft/worldgen/")
    return 0


if __name__ == "__main__":
    sys.exit(main())
