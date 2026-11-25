#!/usr/bin/env python3
"""Offline guards for the pinned aquifer corpus; no Java, jar or world is run."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
import aquifer_oracle  # noqa: E402

FIXTURE = ROOT / "evidence" / "foundation" / "aquifer" / "vanilla.json"


def fixture():
    return json.loads(FIXTURE.read_text())


def rejected(mutator):
    data = fixture()
    mutator(data)
    try:
        aquifer_oracle.validate_fixture(data)
    except (AssertionError, KeyError, ValueError, TypeError):
        return
    raise AssertionError("malformed corpus was accepted")


def main():
    data = fixture()
    counts = aquifer_oracle.validate_fixture(data)
    assert counts == {"prelim": 64, "state": 150}
    assert len(data["result"]["density"]) == len({
        (x & ~3, z & ~3) for x, z in data["request"]["prelim"]}) * 49
    rejected(lambda d: d["request"]["state"].pop())
    rejected(lambda d: d["result"]["prelim"].append([0, 0, 0]))
    rejected(lambda d: d["result"]["prelim"].__setitem__(0, [64, 16, True]))
    rejected(lambda d: d["result"]["density"][0].__setitem__(3, "0"))
    rejected(lambda d: d["result"]["density"][0].__setitem__(3, "zzzzzzzzzzzzzzzz"))
    rejected(lambda d: d["result"]["state"][0].__setitem__(3, True))
    one = next(i for i, row in enumerate(data["request"]["state"]) if row[3] == 1.0)
    rejected(lambda d: d["result"]["state"][one].__setitem__(3, True))
    rejected(lambda d: d["result"]["state"][0].__setitem__(4, "Block{minecraft:stone}[level=0]"))
    rejected(lambda d: d.__setitem__("server_sha1", "0" * 40))
    rejected(lambda d: d.__setitem__("oracle_sha256", "0" * 64))
    rejected(lambda d: d.__setitem__("datapack", "synthetic"))
    rejected(lambda d: d.__setitem__("extra", 1))
    # Extra result key must fail even if every requested row is present.
    rejected(lambda d: d["result"].__setitem__("extra", []))
    # A wrong jar is rejected before Java and must not overwrite an existing output.
    with tempfile.TemporaryDirectory() as tmp:
        fake = Path(tmp) / "server.jar"
        fake.write_bytes(b"not the pinned jar")
        out = Path(tmp) / "out.json"
        out.write_text("keep me")
        before = hashlib.sha256(out.read_bytes()).hexdigest()
        run = subprocess.run(
            [sys.executable, str(ROOT / "tools" / "aquifer_oracle.py"),
             "--jar", str(fake), "--output", str(out)],
            capture_output=True, text=True)
        assert run.returncode == 2, (run.returncode, run.stderr)
        assert hashlib.sha256(out.read_bytes()).hexdigest() == before
    print("aquifer oracle guards passed")


if __name__ == "__main__":
    main()
