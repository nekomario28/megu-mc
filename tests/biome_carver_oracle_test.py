#!/usr/bin/env python3
"""Offline guards for the pinned biome-carver corpus; no Java or jar is run."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
import biome_carver_oracle  # noqa: E402

FIXTURE = ROOT / "evidence" / "foundation" / "biome-carvers" / "vanilla.json"


def fixture():
    return json.loads(FIXTURE.read_text())


def rejected(mutator):
    data = fixture()
    mutator(data)
    try:
        biome_carver_oracle.validate(data)
    except (AssertionError, KeyError, TypeError, ValueError):
        return
    raise AssertionError("malformed carver corpus was accepted")


def main():
    data = fixture()
    counts = biome_carver_oracle.validate(data)
    assert counts == {"biomes": 64, "lists": 58}
    rejected(lambda d: d["biomes"].pop("minecraft:forest"))
    rejected(lambda d: d["biomes"].__setitem__("minecraft:extra", []))
    rejected(lambda d: d["biomes"].__setitem__(
        "minecraft:forest", list(reversed(d["biomes"]["minecraft:forest"]))))
    rejected(lambda d: d.__setitem__("server_sha1", "0" * 40))
    rejected(lambda d: d.__setitem__("oracle_sha256", "0" * 64))
    rejected(lambda d: d["biomes"].__setitem__("minecraft:plains", "minecraft:cave"))
    rejected(lambda d: d["biomes"].__setitem__("minecraft:plains", ["bad"]))
    with tempfile.TemporaryDirectory() as tmp:
        fake = Path(tmp) / "server.jar"
        fake.write_bytes(b"not the pinned jar")
        out = Path(tmp) / "out.json"
        out.write_text("keep me")
        before = hashlib.sha256(out.read_bytes()).hexdigest()
        run = subprocess.run(
            [sys.executable, str(ROOT / "tools" / "biome_carver_oracle.py"),
             "--jar", str(fake), "--output", str(out)],
            capture_output=True, text=True)
        assert run.returncode == 2, (run.returncode, run.stderr)
        assert hashlib.sha256(out.read_bytes()).hexdigest() == before
    print("biome carver oracle guards passed")


if __name__ == "__main__":
    main()
