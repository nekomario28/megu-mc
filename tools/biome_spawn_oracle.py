#!/usr/bin/env python3
"""Dump/verify pinned 1.21.1 biome MobSpawnSettings through the actual classes."""
import argparse
import hashlib
import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from vanilla_java import DEFAULT_JAR, JAR_SHA1, pinned_runtime, run_probe  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "tools" / "biome-spawn-oracle" / "BiomeSpawnOracle.java"
FIXTURE = ROOT / "evidence" / "foundation" / "biome-spawners" / "vanilla.json"


def validate(data):
    assert set(data) == {"minecraft", "server_sha1", "inner_game_sha256",
                         "oracle_sha256", "datapack", "biomes"}
    assert data["minecraft"] == "1.21.1" and data["server_sha1"] == JAR_SHA1
    assert data["oracle_sha256"] == hashlib.sha256(SOURCE.read_bytes()).hexdigest()
    biomes = data["biomes"]
    assert isinstance(biomes, dict) and len(biomes) == 64
    assert biomes["minecraft:plains"]["spawners"]["monster"][1] == {
        "type": "minecraft:zombie", "min": 4, "max": 4, "weight": 95}
    assert len(biomes["minecraft:plains"]["spawners"]["monster"]) == 8
    assert biomes["minecraft:soul_sand_valley"]["spawn_costs"]["minecraft:enderman"] == {
        "energy_budget": 0.15, "charge": 0.7}
    assert len(biomes["minecraft:soul_sand_valley"]["spawn_costs"]) == 4
    # positive coverage of every data shape, including empty categories
    assert any(v["spawners"]["monster"] for v in biomes.values())
    assert any(not v["spawners"]["water_ambient"] for v in biomes.values())
    assert all(set(v) == {"spawners", "spawn_costs", "creature_generation_probability"}
               for v in biomes.values())
    for row in biomes.values():
        assert isinstance(row["creature_generation_probability"], float)
        assert set(row["spawners"]) == {
            "monster", "creature", "ambient", "axolotls",
            "underground_water_creature", "water_creature", "water_ambient", "misc"}
        for entries in row["spawners"].values():
            for e in entries:
                assert set(e) == {"type", "min", "max", "weight"}
                assert all(type(e[k]) is int for k in ("min", "max", "weight"))
                assert e["type"].startswith("minecraft:")
        for cost in row["spawn_costs"].values():
            assert set(cost) == {"energy_budget", "charge"}
            assert all(type(cost[k]) is float for k in cost)
    return {"biomes": len(biomes), "spawner_entries": sum(
        len(v) for row in biomes.values() for v in row["spawners"].values()),
        "spawn_costs": sum(len(row["spawn_costs"]) for row in biomes.values())}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--check", type=Path, default=FIXTURE)
    group.add_argument("--output", type=Path)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    args = parser.parse_args()
    if args.jar.exists() and hashlib.sha1(args.jar.read_bytes()).hexdigest() != JAR_SHA1:
        parser.exit(2, "not the pinned 1.21.1 server jar: %s\n" % args.jar)
    with pinned_runtime(args.jar) as (work, classpath, inner):
        out = work / "spawns.json"
        run_probe(work, classpath, SOURCE, "dev.megu.tools.BiomeSpawnOracle", out)
        biomes = json.loads(out.read_text())
    fixture = {"minecraft": "1.21.1", "server_sha1": JAR_SHA1,
               "inner_game_sha256": hashlib.sha256(inner).hexdigest(),
               "oracle_sha256": hashlib.sha256(SOURCE.read_bytes()).hexdigest(),
               "datapack": "server jar data/minecraft/worldgen loaded by RegistryDataLoader",
               "biomes": biomes}
    counts = validate(fixture)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(fixture, indent=2, sort_keys=True) + "\n")
        print("wrote", args.output, counts)
        return
    expected = json.loads(args.check.read_text())
    validate(expected)
    if expected != fixture:
        for key in fixture:
            if expected.get(key) != fixture.get(key):
                print("fixture mismatch:", key, file=sys.stderr)
        raise SystemExit(1)
    print("biome spawn oracle: %d biomes, %d spawner entries, %d costs match the pinned classes" %
          (counts["biomes"], counts["spawner_entries"], counts["spawn_costs"]))


if __name__ == "__main__":
    main()
