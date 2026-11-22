#!/usr/bin/env python3
"""Record/check pinned 1.21.1 weird-sampler rarity and NormalNoise vectors.

The Java probe constructs the real name-seeded NormalNoise values used by the
vanilla weird-scaled sampler.  These references pin transform/coordinate
semantics; they do not independently prove that megu implements the same
NormalNoise construction.  No server, world, downloads, or extra dependencies.
"""
import argparse
import hashlib
import io
import json
import math
from pathlib import Path
import struct
import subprocess
import zipfile

from vanilla_java import DEFAULT_JAR, JAR_SHA1, pinned_runtime, run_probe

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "tools/weird-sampler-oracle/WeirdSamplerOracle.java"
FIXTURE = ROOT / "evidence/foundation/weird-sampler/vanilla.json"
NOISE_DIR = ROOT / "datapacks/vanilla/data/minecraft/worldgen/noise"
# The three sampler noises plus multi-octave/sparse native-constructor guards.
NOISE_NAMES = ("minecraft:spaghetti_2d", "minecraft:spaghetti_3d_1",
               "minecraft:spaghetti_3d_2", "minecraft:temperature",
               "minecraft:continentalness", "minecraft:offset")
THRESHOLDS = {"type_1": (-0.5, 0.0, 0.5),
              "type_2": (-0.75, -0.5, 0.5, 0.75)}
NAN_BITS = "7ff8000000000001"
LOWER_HEX = frozenset("0123456789abcdef")

# One finite input per reachable rarity branch. Native output scales are
# recorded by the actual enum functions, not inferred from branch ordinals.
SAMPLE_INPUTS = {
    "type_1": (-1.0, -0.25, 0.25, 1.0),
    "type_2": (-1.0, -0.625, 0.0, 0.625, 1.0),
}
POSITIONS = ([0, 0, 0], [0, 64, 0], [-1, -64, -1], [17, 120, -31],
             [-30000000, 0, -30000000], [30000000, 300, -30000000],
             [-123456, 200, 654321], [123456, -256, -654321])
INTERIORS = {"type_1": (-1.5, -0.25, 0.25, 1.5),
             "type_2": (-1.25, -0.625, 0.0, 0.625, 1.25)}
EXPECTED_FIELDS = {"mapper": {"rarity_bits"},
                   "sample": {"rarity_bits", "noise_bits", "value_bits", "scaled_bits"}}


def f64_bits(value):
    """Lowercase 16 hex digits of the raw IEEE-754 binary64 encoding."""
    return struct.pack(">d", float(value)).hex()


def _mapper_inputs(mapper):
    values = []
    for threshold in THRESHOLDS[mapper]:
        values.extend([math.nextafter(threshold, -math.inf), threshold,
                       math.nextafter(threshold, math.inf)])
    values.extend([0.0, -0.0, math.inf, -math.inf])
    values.extend(INTERIORS[mapper])
    # NaN is a bit pattern, never a non-finite JSON number.
    return list(dict.fromkeys([f64_bits(value) for value in values] + [NAN_BITS]))


def _mapper_cases():
    return [{"op": "mapper", "mapper": mapper, "input_bits": bits}
            for mapper in THRESHOLDS for bits in _mapper_inputs(mapper)]


def _sample_cases():
    rows = []
    for mapper in THRESHOLDS:
        for seed in (0, 42):
            for noise in NOISE_NAMES:
                for value in SAMPLE_INPUTS[mapper]:
                    for _ in range(2):
                        rows.append({"op": "sample", "mapper": mapper,
                                     "input_bits": f64_bits(value),
                                     "pos": list(POSITIONS[len(rows) % len(POSITIONS)]),
                                     "seed": seed, "noise": noise})
    return rows


def cases():
    """Deterministic request corpus; no Java or jar access."""
    rows = _mapper_cases() + _sample_cases()
    for i, row in enumerate(rows):
        row["id"] = f"weird-{i:03d}"
    return rows


def _hex16(value):
    return (type(value) is str and len(value) == 16
            and all(ch in LOWER_HEX for ch in value))


def _validate_request(request):
    if type(request) is not dict:
        raise ValueError("weird sampler request is not an object")
    ident, op = request.get("id"), request.get("op")
    if (type(ident) is not str or not ident.startswith("weird-")
            or len(ident) != 9 or not all(ch in "0123456789" for ch in ident[6:])):
        raise ValueError("invalid weird sampler id")
    if type(op) is not str or op not in EXPECTED_FIELDS:
        raise ValueError("invalid weird sampler operation: " + ident)
    mapper = request.get("mapper")
    if type(mapper) is not str or mapper not in THRESHOLDS:
        raise ValueError("invalid weird sampler mapper: " + ident)
    if not _hex16(request.get("input_bits")):
        raise ValueError("invalid weird sampler input bits: " + ident)
    keys = {"id", "op", "mapper", "input_bits"}
    if op == "sample":
        keys |= {"pos", "seed", "noise"}
    if set(request) != keys:
        raise ValueError("changed weird sampler request fields: " + ident)
    if op == "sample":
        pos = request.get("pos")
        if (type(pos) is not list or len(pos) != 3
                or any(type(value) is not int for value in pos)):
            raise ValueError("invalid weird sampler position: " + ident)
        seed = request.get("seed")
        if type(seed) is not int or seed not in (0, 42):
            raise ValueError("invalid weird sampler seed: " + ident)
        noise = request.get("noise")
        if type(noise) is not str or noise not in NOISE_NAMES:
            raise ValueError("invalid weird sampler noise: " + ident)


def _validate_expected(request, expected):
    ident, op = request["id"], request["op"]
    if type(expected) is not dict or set(expected) != EXPECTED_FIELDS[op]:
        raise ValueError("changed weird sampler result fields: " + ident)
    if not _hex16(expected.get("rarity_bits")):
        raise ValueError("invalid weird sampler rarity bits: " + ident)
    if op == "sample":
        for field in ("noise_bits", "value_bits"):
            if not _hex16(expected.get(field)):
                raise ValueError("invalid weird sampler " + field + ": " + ident)
        scaled = expected.get("scaled_bits")
        if type(scaled) is not list or len(scaled) != 3 or not all(map(_hex16, scaled)):
            raise ValueError("invalid weird sampler scaled bits: " + ident)


def validate(rows, requested):
    """Fail closed on empty/truncated/extra/reordered/changed/ill-typed rows."""
    if type(rows) is not list or not rows or len(rows) != len(requested):
        raise ValueError("empty/truncated weird sampler oracle")
    for request, row in zip(requested, rows, strict=True):
        _validate_request(request)
        if type(row) is not dict or set(row) != set(request) | {"expected"}:
            raise ValueError("changed weird sampler request: " + request["id"])
        actual_request = {key: value for key, value in row.items() if key != "expected"}
        _validate_request(actual_request)
        if actual_request != request:
            raise ValueError("changed weird sampler request: " + request["id"])
        _validate_expected(request, row.get("expected"))


def require_equal(expected, actual):
    # Compare the whole canonical object; an empty/missing tail is not a match.
    # Canonical JSON also keeps True/False distinct from 1/0.
    if (type(expected) is not dict or type(actual) is not dict
            or type(actual.get("cases")) is not list or not actual["cases"]
            or json.dumps(expected, sort_keys=True, separators=(",", ":"), allow_nan=False)
            != json.dumps(actual, sort_keys=True, separators=(",", ":"), allow_nan=False)):
        raise ValueError("weird sampler reference differs, is incomplete, "
                         "or has stale provenance")


def _noise_payloads(inner):
    payloads = {}
    with zipfile.ZipFile(io.BytesIO(inner)) as game:
        for name in NOISE_NAMES:
            path = "data/minecraft/worldgen/noise/" + name.split(":", 1)[1] + ".json"
            try:
                payloads[name] = game.read(path)
            except KeyError as error:
                raise ValueError("pinned jar has no noise: " + name) from error
    for name, payload in payloads.items():
        local = NOISE_DIR / (name.split(":", 1)[1] + ".json")
        if not local.is_file():
            raise ValueError("installed noise is missing: " + name)
        if json.loads(payload) != json.loads(local.read_bytes()):
            raise ValueError("installed noise differs from the pinned jar: " + name)
    return payloads


def reference(jar):
    requested = cases()
    with pinned_runtime(jar) as (work, classpath, inner):
        payloads = _noise_payloads(inner)
        noises = {name: json.loads(payload) for name, payload in payloads.items()}
        source, destination = work / "input.json", work / "output.json"
        source.write_text(json.dumps({"cases": requested, "noises": noises}))
        run_probe(work, classpath, SOURCE, "dev.megu.tools.WeirdSamplerOracle",
                  source, destination)
        rows = json.loads(destination.read_text())
    validate(rows, requested)
    return {"minecraft": "1.21.1", "server_sha1": JAR_SHA1,
            "oracle_sha256": hashlib.sha256(SOURCE.read_bytes()).hexdigest(),
            "noise_sha256": {name: hashlib.sha256(payload).hexdigest()
                             for name, payload in payloads.items()},
            "cases": rows}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jar", type=Path, default=DEFAULT_JAR)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--output", type=Path,
                      help="record only after a complete successful run")
    mode.add_argument("--check", type=Path, default=FIXTURE,
                      help="default: check committed vectors")
    args = parser.parse_args()
    try:
        actual = reference(args.jar)
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(actual, indent=2, sort_keys=True) + "\n")
        else:
            require_equal(json.loads(args.check.read_text()), actual)
        print(f"weird sampler: {len(actual['cases'])} actual-class cases, pinned 1.21.1")
    except (OSError, ValueError, KeyError, TypeError,
            subprocess.SubprocessError, zipfile.BadZipFile) as error:
        parser.exit(1, f"weird sampler: {error}\n")


if __name__ == "__main__":
    main()
