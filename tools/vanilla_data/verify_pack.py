#!/usr/bin/env python3
"""Verify a generated `datapacks/vanilla` tree against the pinned 1.21.1 inventory.

  verify_pack.py [--pack DIR] [--jar-sha1 SHA1]

`gen_vanilla_data.sh` writes this tree from two sources: the vanilla data generator's
`--server` output (`worldgen/`, `tags/block/`, `dimension_type/`) and the jar's own members
(`structure/**/*.nbt`, `tags/worldgen/*`, `multi_noise_biome_tree.json`). Both halves can fail
*silently*: the previous version of the generator removed `datapacks/vanilla/data` and copied a
fixed list of seven worldgen directories, so a jar whose datagen output had moved - or a copy
that dropped a directory - produced a tree that still loaded and still ran, with different
terrain. Counting the inventory and hashing the members is what turns that into a failure.

The inventory below is the pinned version's, measured once and frozen: it is a claim about the
1.21.1 jar, and a different jar (or a corrupt one) breaks it. Every check here fails closed and
prints what it expected, so "verify" is not a pass on an empty tree.

Run it after generating (`gen_vanilla_data.sh` does), or on its own in CI: it needs no jar.
"""
import argparse
import hashlib
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
JAR_SHA1 = "59353fb40c36d304f2035d51e7d6e6baa98dc05c"

# What the data generator wrote for 1.21.1, by directory, relative to data/minecraft/.
DATAGEN = {
    "dimension_type": 4,
    "tags/block": 184,
    "worldgen/biome": 64,
    "worldgen/configured_carver": 4,
    "worldgen/configured_feature": 196,
    "worldgen/density_function": 35,
    "worldgen/flat_level_generator_preset": 9,
    "worldgen/multi_noise_biome_source_parameter_list": 2,
    "worldgen/noise": 60,
    "worldgen/noise_settings": 7,
    "worldgen/placed_feature": 233,
    "worldgen/processor_list": 40,
    "worldgen/structure": 34,
    "worldgen/structure_set": 20,
    "worldgen/template_pool": 186,
    "worldgen/world_preset": 6,
}
# The jar's own members, by prefix, with the exact count and suffix. `structure-provenance.json`
# records one hash per member, so the count is checked twice: here and against that manifest.
JAR = {
    "data/minecraft/structure": (".nbt", 1180),
    "data/minecraft/tags/worldgen/biome": (".json", 70),
    "data/minecraft/tags/worldgen/structure": (".json", 13),
}
PACK_FORMAT = 48


class Failure(Exception):
    pass


def require(condition, message):
    if not condition:
        raise Failure(message)


def count_files(directory, suffix):
    return sorted(p for p in directory.rglob("*" + suffix) if p.is_file())


def check_inventory(pack):
    data = pack / "data" / "minecraft"
    require(data.is_dir(), f"missing {data}")
    for relative, expected in DATAGEN.items():
        directory = data / relative
        require(directory.is_dir(), f"missing directory {relative}")
        found = count_files(directory, ".json")
        require(
            len(found) == expected,
            f"{relative}: {len(found)} json files, expected {expected}",
        )
        for path in found:
            try:
                json.loads(path.read_text())
            except json.JSONDecodeError as error:
                raise Failure(f"{path.relative_to(pack)} is not valid JSON: {error}")
    for prefix, (suffix, expected) in JAR.items():
        directory = pack / prefix
        require(directory.is_dir(), f"missing directory {prefix}")
        found = count_files(directory, suffix)
        require(
            len(found) == expected,
            f"{prefix}: {len(found)} {suffix} files, expected {expected}",
        )
        for path in found:
            if suffix == ".json":
                try:
                    json.loads(path.read_text())
                except json.JSONDecodeError as error:
                    raise Failure(f"{path.relative_to(pack)} is not valid JSON: {error}")

    # An unexpected directory is not harmless: it is either a stale version's data that the
    # consumer would now read, or a generator change this inventory has not been told about.
    expected_top = {"dimension_type", "structure", "tags", "worldgen"}
    got_top = {p.name for p in data.iterdir() if p.is_dir()}
    require(
        got_top == expected_top,
        f"data/minecraft holds {sorted(got_top)}, expected {sorted(expected_top)}",
    )
    expected_tags = {"block", "worldgen"}
    got_tags = {p.name for p in (data / "tags").iterdir() if p.is_dir()}
    require(
        got_tags == expected_tags,
        f"data/minecraft/tags holds {sorted(got_tags)}, expected {sorted(expected_tags)}",
    )
    expected_worldgen = {relative.split("/", 1)[1] for relative in DATAGEN if relative.startswith("worldgen/")}
    got_worldgen = {p.name for p in (data / "worldgen").iterdir() if p.is_dir()}
    require(
        got_worldgen == expected_worldgen,
        f"data/minecraft/worldgen holds {sorted(got_worldgen)}, expected {sorted(expected_worldgen)}",
    )
    stray = sorted(
        path.relative_to(pack).as_posix()
        for path in data.rglob("*")
        if path.is_file() and path.suffix not in (".json", ".nbt")
    )
    require(not stray, f"unexpected non-data files under data/: {stray[:5]}")
    return sum(len(count_files(data / relative, ".json")) for relative in DATAGEN)


def check_structure_provenance(pack, jar_sha1):
    path = pack / "structure-provenance.json"
    require(path.is_file(), "missing structure-provenance.json")
    provenance = json.loads(path.read_text())
    require(
        provenance.get("server_sha1") == jar_sha1,
        f"structure-provenance records jar {provenance.get('server_sha1')}, expected {jar_sha1}",
    )
    members = provenance.get("members_sha256") or {}
    require(len(members) == sum(count for _, count in JAR.values()),
            f"structure-provenance records {len(members)} members, expected "
            f"{sum(count for _, count in JAR.values())}")
    for name, expected in sorted(members.items()):
        target = pack / name
        require(target.is_file(), f"member {name} is recorded but missing")
        got = hashlib.sha256(target.read_bytes()).hexdigest()
        require(got == expected, f"member {name} hash {got} != recorded {expected}")
    installed = {
        path.relative_to(pack).as_posix()
        for prefix in JAR
        for path in (pack / prefix).rglob("*")
        if path.is_file()
    }
    require(
        installed == set(members),
        f"the tree and the provenance disagree on membership: "
        f"{sorted(installed ^ set(members))[:5]}",
    )
    return len(members)


def check_biome_tree(pack, jar_sha1):
    path = pack / "multi_noise_biome_tree.json"
    require(path.is_file(), "missing multi_noise_biome_tree.json")
    tree = json.loads(path.read_text())
    require(
        {"overworld", "nether"} <= set(tree),
        f"biome tree presets are {sorted(set(tree))}, expected overworld and nether",
    )
    # src/worldgen.rs reads `tree[preset]`, and `_provenance` must not shadow a preset name.
    require("_provenance" in tree, "biome tree carries no _provenance block")
    require(
        tree["_provenance"].get("server_sha1") == jar_sha1,
        f"biome tree records jar {tree['_provenance'].get('server_sha1')}, expected {jar_sha1}",
    )
    return path.stat().st_size


def check_pack_mcmeta(pack):
    path = pack / "pack.mcmeta"
    require(path.is_file(), "missing pack.mcmeta")
    meta = json.loads(path.read_text())
    require(
        meta.get("pack", {}).get("pack_format") == PACK_FORMAT,
        f"pack.mcmeta pack_format {meta.get('pack', {}).get('pack_format')}, expected {PACK_FORMAT}",
    )


def digest(pack):
    lines = []
    for path in sorted(p for p in pack.rglob("*") if p.is_file()):
        got = hashlib.sha256(path.read_bytes()).hexdigest()
        lines.append(f"{got}  {path.relative_to(pack).as_posix()}")
    return hashlib.sha256("\n".join(lines).encode()).hexdigest(), len(lines)


def verify(pack, jar_sha1):
    count = check_inventory(pack)
    members = check_structure_provenance(pack, jar_sha1)
    size = check_biome_tree(pack, jar_sha1)
    check_pack_mcmeta(pack)
    combined, files = digest(pack)
    print(
        f"vanilla pack: {count} datagen json + {members} jar members, "
        f"biome tree {size} bytes, {files} files, digest {combined}"
    )
    return combined


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--pack", type=Path, default=ROOT / "datapacks/vanilla")
    parser.add_argument("--jar-sha1", default=JAR_SHA1)
    args = parser.parse_args()
    try:
        verify(args.pack, args.jar_sha1)
    except Failure as error:
        print(f"vanilla pack: FAILED {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
