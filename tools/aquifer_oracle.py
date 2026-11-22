#!/usr/bin/env python3
"""Compare the pinned 1.21.1 NoiseChunk/Aquifer through actual classes.

The probe loads the server jar's own datapack through RegistryDataLoader and
constructs an ordinary ProtoChunk, NoiseChunk and Aquifer. Default mode reruns
Java and requires complete request/result/provenance equality with the fixture;
only an explicit --output writes it. A wrong jar is rejected before Java.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from vanilla_java import DEFAULT_JAR, JAR_SHA1, pinned_runtime, run_probe  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "tools" / "aquifer-oracle" / "AquiferOracle.java"
FIXTURE = ROOT / "evidence" / "foundation" / "aquifer" / "vanilla.json"
SEED = 0
CHUNK = [7, 4]


def request():
    """Chunk-local cases only: a vanilla aquifer is scoped to its own chunk."""
    x0, z0 = CHUNK[0] * 16, CHUNK[1] * 16
    prelim = []
    for x in (x0 - 48, x0 - 17, x0 - 16, x0 - 1, x0, x0 + 15, x0 + 16, x0 + 32):
        for z in (z0 - 48, z0 - 17, z0 - 16, z0 - 1, z0, z0 + 15, z0 + 16, z0 + 32):
            prelim.append([x, z])
    state = []
    ys = [-64, -60, -56, -55, -54, -53, -48, -40, -20, 0, 20, 40, 55, 60, 62, 63,
          64, 80, 100, 160, 240, 300, 319]
    for x, z in ((x0, z0), (x0 + 4, z0 + 8), (x0 + 8, z0 + 8), (x0 + 15, z0 + 15),
                 (x0 + 15, z0), (x0, z0 + 15)):
        for y in ys:
            state.append([x, y, z, 0.0])
    for density in (-1.0, 1.0):
        for y in (-60, -54, 0, 62, 64, 200):
            state.append([x0 + 4, y, z0 + 8, density])
    # Raw initial_density_without_jaggedness at every section-aligned column
    # the preliminary-surface probe reads, so the loop and the DF are separated.
    density_points = []
    for x, z in sorted({(x & ~3, z & ~3) for x, z in prelim}):
        for y in range(-64, 321, 8):
            density_points.append([x, y, z])
    return {"seed": SEED, "chunk": CHUNK, "prelim": prelim, "state": state,
            "density": density_points}


def state_name(raw):
    if raw is None:
        return None
    match = re.fullmatch(r"Block\{minecraft:([a-z0-9_]+)\}(?:\[.*\])?", raw)
    if not match:
        raise ValueError("unexpected BlockState rendering: " + raw)
    return "minecraft:" + match.group(1)


def validate(req, result):
    assert set(req) == {"seed", "chunk", "prelim", "state", "density"}
    assert set(result) == {"prelim", "state", "density"}, set(result)
    assert req["seed"] == SEED and req["chunk"] == CHUNK
    assert len(req["prelim"]) == 64 and len(req["state"]) == 150
    aligned = {(x & ~3, z & ~3) for x, z in req["prelim"]}
    assert len(req["density"]) == len(aligned) * 49
    assert len(result["density"]) == len(req["density"])
    assert len(result["prelim"]) == len(req["prelim"])
    assert len(result["state"]) == len(req["state"])
    columns = {(x, z) for x, z, _ in result["prelim"]}
    assert len(columns) == len(result["prelim"])
    assert all(type(level) is int for _, _, level in result["prelim"])
    seen = set()
    for want, got in zip(req["density"], result["density"]):
        assert len(got) == 4 and got[:3] == want
        assert re.fullmatch(r"[0-9a-f]{16}", got[3]), got[3]
    for want, got in zip(req["state"], result["state"]):
        assert len(got) == 5 and got[:3] == want[:3]
        # Java prints the f64 argument; these are exact binary64 literals.
        assert type(want[3]) is float and type(got[3]) is float
        assert got[3] == want[3]
        assert got[4] is None or state_name(got[4]) in {
            "minecraft:air", "minecraft:water", "minecraft:lava"}, got[4]
        seen.add(state_name(got[4]) if got[4] is not None else None)
    # Positive coverage: a corpus of one answer cannot prove a branch.
    assert seen == {None, "minecraft:air", "minecraft:water", "minecraft:lava"}, seen
    return {"prelim": len(result["prelim"]), "state": len(result["state"])}


def validate_fixture(data):
    assert set(data) == {"minecraft", "server_sha1", "inner_game_sha256", "oracle_sha256",
                         "datapack", "request", "result"}, set(data)
    assert data["minecraft"] == "1.21.1" and data["server_sha1"] == JAR_SHA1
    assert re.fullmatch(r"[0-9a-f]{64}", data["inner_game_sha256"])
    assert re.fullmatch(r"[0-9a-f]{64}", data["oracle_sha256"])
    assert data["oracle_sha256"] == hashlib.sha256(SOURCE.read_bytes()).hexdigest()
    assert data["datapack"] == "server jar data/minecraft/worldgen loaded by RegistryDataLoader"
    assert set(data["result"]) == {"prelim", "state", "density"}
    return validate(data["request"], data["result"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--check", type=Path, default=FIXTURE)
    group.add_argument("--output", type=Path)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    args = parser.parse_args()
    if args.jar.exists() and hashlib.sha1(args.jar.read_bytes()).hexdigest() != JAR_SHA1:
        parser.exit(2, "not the pinned 1.21.1 server jar: %s\n" % args.jar)
    req = request()
    work_file = Path("/tmp/mc-aquifer-request.json")
    work_file.write_text(json.dumps(req, sort_keys=True) + "\n")
    with pinned_runtime(args.jar) as (work, classpath, inner):
        out_file = work / "aquifer-result.json"
        run_probe(work, classpath, SOURCE, "dev.megu.tools.AquiferOracle", work_file, out_file)
        result = json.loads(out_file.read_text())
        observed = {"prelim": result["prelim"], "state": result["state"],
                    "density": result["density"]}
    fixture = {
        "minecraft": "1.21.1",
        "server_sha1": JAR_SHA1,
        "inner_game_sha256": hashlib.sha256(inner).hexdigest(),
        "oracle_sha256": hashlib.sha256(SOURCE.read_bytes()).hexdigest(),
        "datapack": "server jar data/minecraft/worldgen loaded by RegistryDataLoader",
        "request": req,
        "result": observed,
    }
    counts = validate(req, observed)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(fixture, indent=2, sort_keys=True) + "\n")
        print("wrote", args.output, counts)
        return
    expected = json.loads(args.check.read_text())
    validate_fixture(expected)
    if expected != fixture:
        for key in ("minecraft", "server_sha1", "inner_game_sha256", "oracle_sha256",
                    "datapack", "request", "result"):
            if expected.get(key) != fixture.get(key):
                print("fixture mismatch:", key, file=sys.stderr)
        raise SystemExit(1)
    print("aquifer oracle: %d prelim, %d state cases match the pinned classes" %
          (counts["prelim"], counts["state"]))


if __name__ == "__main__":
    main()
