#!/usr/bin/env python3
"""Offline guards for the pinned biome spawner/cost corpus."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
import biome_spawn_oracle  # noqa: E402


def fixture():
    return json.loads(biome_spawn_oracle.FIXTURE.read_text())


def rejected(mutator):
    data = fixture()
    mutator(data)
    try:
        biome_spawn_oracle.validate(data)
    except (AssertionError, KeyError, TypeError, ValueError):
        return
    raise AssertionError("malformed spawn corpus was accepted")


def main():
    data = fixture()
    counts = biome_spawn_oracle.validate(data)
    assert counts == {"biomes": 64, "spawner_entries": 746, "spawn_costs": 5}
    rejected(lambda d: d["biomes"]["minecraft:plains"]["spawners"]["monster"].pop())
    rejected(lambda d: d["biomes"]["minecraft:plains"]["spawners"]["monster"][0].__setitem__("min", True))
    rejected(lambda d: d["biomes"]["minecraft:plains"]["spawners"]["monster"][0].__setitem__("weight", "100"))
    rejected(lambda d: d["biomes"]["minecraft:soul_sand_valley"]["spawn_costs"].pop("minecraft:enderman"))
    rejected(lambda d: d["biomes"]["minecraft:soul_sand_valley"]["spawn_costs"]["minecraft:enderman"].__setitem__("charge", 1))
    rejected(lambda d: d.__setitem__("server_sha1", "0" * 40))
    rejected(lambda d: d.__setitem__("oracle_sha256", "0" * 64))
    rejected(lambda d: d["biomes"].pop("minecraft:plains"))
    with tempfile.TemporaryDirectory() as tmp:
        fake = Path(tmp) / "server.jar"
        fake.write_bytes(b"not the pinned jar")
        out = Path(tmp) / "out.json"
        out.write_text("keep me")
        before = hashlib.sha256(out.read_bytes()).hexdigest()
        run = subprocess.run(
            [sys.executable, str(ROOT / "tools" / "biome_spawn_oracle.py"),
             "--jar", str(fake), "--output", str(out)],
            capture_output=True, text=True)
        assert run.returncode == 2, (run.returncode, run.stderr)
        assert hashlib.sha256(out.read_bytes()).hexdigest() == before
    print("biome spawn oracle guards passed")


if __name__ == "__main__":
    main()
