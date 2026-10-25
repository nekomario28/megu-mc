#!/usr/bin/env python3
"""Pin 1.21.1 biome carver lists from the server jar's own datapack."""
import argparse
import hashlib
import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from vanilla_java import DEFAULT_JAR, JAR_SHA1, pinned_runtime, run_probe  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "tools" / "biome-carver-oracle" / "BiomeCarverOracle.java"
FIXTURE = ROOT / "evidence" / "foundation" / "biome-carvers" / "vanilla.json"


def validate(data):
    assert set(data) == {"minecraft", "server_sha1", "inner_game_sha256",
                         "oracle_sha256", "datapack", "biomes"}
    assert data["minecraft"] == "1.21.1" and data["server_sha1"] == JAR_SHA1
    assert data["oracle_sha256"] == hashlib.sha256(SOURCE.read_bytes()).hexdigest()
    biomes = data["biomes"]
    assert isinstance(biomes, dict) and len(biomes) == 64
    assert biomes["minecraft:forest"] == [
        "minecraft:cave", "minecraft:cave_extra_underground", "minecraft:canyon"]
    assert any(v == [] for v in biomes.values()), "empty carver list coverage"
    assert any(len(v) == 3 for v in biomes.values()), "multi-carver coverage"
    for name, carvers in biomes.items():
        assert name.startswith("minecraft:")
        assert isinstance(carvers, list) and all(
            isinstance(c, str) and c.startswith("minecraft:") for c in carvers)
    return {"biomes": len(biomes), "lists": sum(bool(v) for v in biomes.values())}


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
        out = work / "carvers.json"
        run_probe(work, classpath, SOURCE, "dev.mcspine.tools.BiomeCarverOracle", out)
        biomes = json.loads(out.read_text())
    fixture = {
        "minecraft": "1.21.1", "server_sha1": JAR_SHA1,
        "inner_game_sha256": hashlib.sha256(inner).hexdigest(),
        "oracle_sha256": hashlib.sha256(SOURCE.read_bytes()).hexdigest(),
        "datapack": "server jar data/minecraft/worldgen loaded by RegistryDataLoader",
        "biomes": biomes,
    }
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
    print("biome carver oracle: %d biomes match the pinned classes" % counts["biomes"])


if __name__ == "__main__":
    main()
