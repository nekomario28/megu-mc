#!/usr/bin/env python3
"""Real-chunk fossil acceptance: replay the port's own generation of the chunk a pinned
native 1.21.1 server decorated with `minecraft:nether_fossil`, and compare every channel
P1-4 names - all block states, the saved heightmap, and the pending-tick record.

    tools/fossil_chunk_gate.py [--binary target/debug/megu] [--repeat N] [--json-out PATH]

Why this exists: `tools/fossil_oracle.py` proves the FossilFeature against scripted
worlds, and `tools/structure_capture.py`/`tools/port_triage.py` proved this chunk once
against a local capture (`evidence/gameplay/feature-parity/template-fossil-p1-20261120.md`,
65,536 cells, 0 diffs). Neither is a gate: the oracle never generates a real chunk, and
the triage needs the unredistributable native world. This gate reads the committed
fixtures under `tests/data/nether_fossil_4242_*` (produced by
`tools/native_chunk_capture.py` from the frozen capture) plus the recorded population
order, drives the real `MEGU_WG_DUMP`/`MEGU_POP_ORDER` production path, and compares:

  1. **states** - all 65,536 cells of chunk (-22,20) at y 0..255 as canonical
     `name[prop=value,...]` strings, so a wrong stair facing or bone axis fails;
  2. **heightmap** - all 256 `OCEAN_FLOOR` columns (`MEGU_WG_DUMP_HM`) against the
     chunk's saved `Heightmaps.OCEAN_FLOOR`;
  3. **pending ticks** - every fluid tick the whole 9-chunk decorate pass leaves
     pending (`MEGU_WG_FLUID_TICKS`) against the saved `fluid_ticks` of all nine
     populated chunks (79 lava ticks, 0 block ticks, 0 block entities).

The fossil-positive property is a precondition, not a lucky sample: the fixture's own
`structures.starts["minecraft:nether_fossil"]` child `nefos` (template
`minecraft:nether_fossils/fossil_14`, BB [-356,55,319,-350,61,324]) and its 15
`bone_block` cells inside the chunk are verified from the fixture before any
comparison is accepted.

Exit codes:
  0  MATCH - every arm compared nonzero inputs and found zero differences
  1  DIFF  - at least one arm has differences (report names them)
  2  INVALID - fixture/inputs/run failed or produced nothing comparable
"""
import argparse
import gzip
import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
DATA = ROOT / "tests" / "data"
CANON = DATA / "nether_fossil_4242_chunk_-22_20_1211.tsv.gz"
META = DATA / "nether_fossil_4242_chunk_-22_20_1211.meta.json"
ORDER = DATA / "nether_fossil_4242_pop_order.tsv"

# The environment knobs this gate sets, kept in one place so a stray MEGU_* in the
# caller's shell cannot silently change the measurement (same policy as
# `tools/port_triage.py`'s STRIP_ENV_PREFIXES).
STRIP_ENV_PREFIXES = ("MEGU_", "SPC_", "FSEED_", "BIOMEF_", "FEATURE_")
SEED = 4242
CX, CZ = -22, 20
DUMP_N = 1
PRESET = "nether"
DUMP_WINDOW = "world"  # the Nether dimension_type window: min_y 0, height 256


class GateError(Exception):
    """Fail-closed input/fixture/run error (exit 2), never a counted difference."""


def sha256_file(path):
    import hashlib
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def load_canon_tsv(path):
    """Canonical-state TSV -> {(x,y,z): state}. The header is validated, not skipped."""
    opener = gzip.open if str(path).endswith(".gz") else open
    with opener(path, "rt", encoding="utf-8") as fh:
        lines = fh.read().splitlines()
    header = [line for line in lines if line.startswith("#")]
    if len(header) != 1 or not header[0].startswith("# chunk "):
        raise GateError(f"{path}: missing '# chunk cx,cz min_y .. height .. cells ..' header")
    rows = {}
    for i, line in enumerate(lines):
        if line.startswith("#"):
            continue
        fields = line.split("\t")
        if len(fields) != 4:
            raise GateError(f"{path}:{i + 1}: expected 4 TSV fields, got {len(fields)}")
        x, y, z = (int(fields[0]), int(fields[1]), int(fields[2]))
        if (x, y, z) in rows:
            raise GateError(f"{path}:{i + 1}: duplicate cell ({x},{y},{z})")
        rows[(x, y, z)] = fields[3]
    if not rows:
        raise GateError(f"{path}: zero state rows")
    declared = header[0].rsplit("cells ", 1)[-1]
    if declared.isdigit() and int(declared) != len(rows):
        raise GateError(f"{path}: header declares {declared} cells, file has {len(rows)}")
    return rows, header[0]


def load_order_tsv(path):
    order = []
    for i, line in enumerate(Path(path).read_text().splitlines()):
        fields = line.split()
        if len(fields) != 2:
            raise GateError(f"{path}:{i + 1}: expected 'cx cz', got {line!r}")
        order.append((int(fields[0]), int(fields[1])))
    if len(order) != len(set(order)):
        raise GateError(f"{path}: duplicate source chunk")
    return order


def load_meta(path):
    meta = json.loads(Path(path).read_text())
    required = {
        "native": ("server_sha1", "server_sha256", "level_dat_sha256",
                   "region_sha256", "order_tsv_sha256", "boundary"),
        "chunk": ("cx", "cz", "min_y", "height", "Status", "census",
                  "block_entities", "block_ticks", "heightmaps"),
        "pending_ticks": ("fluid_id_map", "fluid_ticks"),
        "fossil_positive": ("structure", "verified_start", "child_bb",
                            "bone_block_cells_in_chunk", "bone_block_cells"),
        "population": ("order_file", "scope", "source_chunks"),
    }
    for section, keys in required.items():
        if not isinstance(meta.get(section), dict):
            raise GateError(f"{path}: missing section {section!r}")
        for key in keys:
            if key not in meta[section]:
                raise GateError(f"{path}: missing {section}.{key}")
    for section, key in (("native", k) for k in
                         ("server_sha256", "level_dat_sha256", "region_sha256",
                          "order_tsv_sha256", "run_log_sha256", "boundary_json_sha256")):
        value = meta[section].get(key, "")
        if len(value) != 64 or any(c not in "0123456789abcdef" for c in value):
            raise GateError(f"{path}: {section}.{key} is not a sha256")
    if meta["native"].get("server_sha1") != "59353fb40c36d304f2035d51e7d6e6baa98dc05c":
        raise GateError(f"{path}: fixture is not bound to the pinned 1.21.1 server")
    boundary = meta["native"]["boundary"]
    if boundary.get("mode") != "frozen" or boundary.get("stage") != "structures":
        raise GateError(f"{path}: capture is not a frozen 'structures' stage fixture")
    if boundary.get("runtime_writes") != 0:
        raise GateError(f"{path}: capture recorded runtime writes; this gate compares the frozen stage only")
    if boundary.get("seed") != SEED:
        raise GateError(f"{path}: capture seed {boundary.get('seed')} != {SEED}")
    if meta["chunk"].get("Status") not in ("minecraft:full", "full"):
        raise GateError(f"{path}: native chunk is not status=full")
    return meta


def census_of(rows, cx, cz, min_y, height):
    """Recompute the air census the meta records, over the full chunk grid."""
    air = cave = void = 0
    expected = 16 * 16 * height
    seen = set()
    for (x, y, z), _state in rows.items():
        if not (cx * 16 <= x < (cx + 1) * 16 and cz * 16 <= z < (cz + 1) * 16):
            raise GateError(f"fixture cell ({x},{y},{z}) escapes chunk ({cx},{cz})")
        if not (min_y <= y < min_y + height):
            raise GateError(f"fixture cell ({x},{y},{z}) escapes y window {min_y}..{min_y + height}")
        seen.add((x, y, z))
    for state in rows.values():
        if state == "minecraft:cave_air":
            cave += 1
        elif state == "minecraft:void_air":
            void += 1
    air = expected - len(rows)
    return {"air": air, "cave_air": cave, "void_air": void,
            "non_air": len(rows) - cave - void, "total": expected}


def validate_fixture(canon_rows, order, meta):
    """No green by silence: the fixture itself must prove it is the fossil-positive,
    fully recorded capture before the port is ever compared to it."""
    chunk = meta["chunk"]
    if (chunk["cx"], chunk["cz"]) != (CX, CZ):
        raise GateError("meta names a different chunk than this gate's")
    if (chunk["min_y"], chunk["height"]) != (0, 256):
        raise GateError("meta window is not the Nether dimension window 0..256")
    census = census_of(canon_rows, CX, CZ, chunk["min_y"], chunk["height"])
    recorded = chunk["census"]
    for key in ("air", "cave_air", "void_air", "non_air"):
        if int(recorded.get(key, -1)) != census[key]:
            raise GateError(
                f"fixture census mismatch: meta records {key}={recorded.get(key)}, "
                f"TSV contains {census[key]}")
    # The fossil must actually be in the fixture: the native start's own child BB and
    # every recorded bone_block cell must be present with their recorded states.
    fp = meta["fossil_positive"]
    if not fp["verified_start"] or fp["structure"] != "minecraft:nether_fossil":
        raise GateError("fixture does not record a verified nether_fossil start")
    bones = {(b["x"], b["y"], b["z"]): b["state"] for b in fp["bone_block_cells"]}
    if len(bones) != fp["bone_block_cells_in_chunk"] or not bones:
        raise GateError("fixture records no fossil bone_block cells")
    bb = fp["child_bb"]
    inside = [p for p in bones if bb[0] <= p[0] <= bb[3] and bb[1] <= p[1] <= bb[4]
              and bb[2] <= p[2] <= bb[5]]
    if len(inside) != len(bones):
        raise GateError("recorded bone_block cells escape the fossil child BB")
    for pos, state in bones.items():
        if canon_rows.get(pos) != state:
            raise GateError(f"fixture TSV disagrees with recorded fossil cell {pos}: {state}")
    # The population order must cover exactly the (n+2)^2 decoration halo the dump
    # requires, or the port replays a different FEATURES plan than native ran.
    halo = {(CX + dx, CZ + dz) for dx in range(-1, DUMP_N + 1) for dz in range(-1, DUMP_N + 1)}
    if set(order) != halo:
        raise GateError(f"order file covers {set(order)} != required halo {halo}")
    if meta["population"]["scope"] != "exact":
        raise GateError("fixture does not pin an exact population scope")
    if chunk["block_ticks"] != 0 or meta["pending_ticks"].get("block_ticks_total") != 0:
        raise GateError("fixture records native block ticks this gate does not model")
    for tick in meta["pending_ticks"]["fluid_ticks"]:
        if tick.get("i") not in meta["pending_ticks"]["fluid_id_map"].values():
            raise GateError(f"native fluid tick uses unmapped fluid {tick.get('i')!r}")
    return census


def build_env(workdir, order_path):
    env = {k: v for k, v in os.environ.items() if not k.startswith(STRIP_ENV_PREFIXES)}
    env.update({
        "MEGU_SEED": str(SEED),
        "MEGU_WG_PROFILE": "0",
        "MEGU_WG_PRESET": PRESET,
        "MEGU_WG_DUMP": str(workdir / "dump.bin"),
        "MEGU_WG_DUMP_X": str(CX),
        "MEGU_WG_DUMP_Z": str(CZ),
        "MEGU_WG_DUMP_N": str(DUMP_N),
        "MEGU_WG_DUMP_WINDOW": DUMP_WINDOW,
        "MEGU_WG_DUMP_CANON": str(workdir / "canon.tsv"),
        "MEGU_WG_DUMP_HM": str(workdir / "hm.bin"),
        "MEGU_WG_FLUID_TICKS": str(workdir / "fluid_ticks.tsv"),
        "MEGU_POP_ORDER": str(order_path),
        "MEGU_POP_ORDER_SCOPE": "exact",
    })
    return env


def run_dump(binary, env, log_path):
    proc = subprocess.run([str(binary)], cwd=ROOT, env=env,
                          capture_output=True, text=True)
    log_text = proc.stdout + proc.stderr
    log_path.write_text(log_text, encoding="utf-8")
    if proc.returncode != 0:
        tail = "\n".join(log_text.splitlines()[-25:])
        raise GateError(f"megu dump failed (exit {proc.returncode}):\n{tail}")
    # The plan must have executed every recorded source chunk; a silent skip reads
    # like a pass. `populate_plan` reports the count itself.
    if f"{len(load_order_tsv(env['MEGU_POP_ORDER']))} source chunks executed" not in log_text:
        raise GateError("dump log does not record the population plan executing")
    return log_text


def port_fluid_ticks(path, id_map):
    ticks = set()
    for i, line in enumerate(Path(path).read_text().splitlines()):
        fields = line.split("\t")
        if len(fields) != 4:
            raise GateError(f"{path}:{i + 1}: expected 'x y z fluid_id'")
        fluid = id_map.get(fields[3])
        if fluid is None:
            raise GateError(f"{path}:{i + 1}: unmapped port fluid id {fields[3]!r}")
        tick = (int(fields[0]), int(fields[1]), int(fields[2]), fluid)
        if tick in ticks:
            raise GateError(f"{path}:{i + 1}: duplicate pending tick {tick}")
        ticks.add(tick)
    return ticks


def sample_differences(want, got, limit=12):
    diffs = []
    for pos in sorted(set(want) | set(got)):
        a, b = want.get(pos), got.get(pos)
        if a != b:
            diffs.append({"cell": list(pos), "native": a, "port": b})
            if len(diffs) >= limit:
                break
    return diffs


def compare_all(canon_rows, meta, workdir):
    report = {"status": "MATCH", "arms": {}}
    failures = []

    port_rows, port_header = load_canon_tsv(workdir / "canon.tsv")
    state_diffs = sample_differences(canon_rows, port_rows)
    n_state = sum(1 for pos in set(canon_rows) | set(port_rows)
                  if canon_rows.get(pos) != port_rows.get(pos))
    report["arms"]["states"] = {
        "native_cells": len(canon_rows), "port_cells": len(port_rows),
        "differing_cells": n_state, "samples": state_diffs,
        "window": [meta["chunk"]["min_y"], meta["chunk"]["height"]],
        "total_cells": 16 * 16 * meta["chunk"]["height"],
    }
    if n_state:
        failures.append(f"states: {n_state} differing cells")

    hm_raw = (workdir / "hm.bin").read_bytes()
    if len(hm_raw) != 256 * 4:
        raise GateError(f"hm.bin is {len(hm_raw)} bytes, expected 1024")
    port_hm = list(struct.unpack("<256i", hm_raw))
    native_hm = meta["chunk"]["heightmaps"].get("OCEAN_FLOOR")
    if not isinstance(native_hm, list) or len(native_hm) != 256:
        raise GateError("fixture meta lacks Heightmaps.OCEAN_FLOOR[256]")
    hm_diffs = [i for i in range(256) if port_hm[i] != native_hm[i]]
    report["arms"]["heightmap"] = {
        "name": "OCEAN_FLOOR", "columns": 256,
        "differing_columns": len(hm_diffs),
        "samples": [{"column": i, "native": native_hm[i], "port": port_hm[i]}
                    for i in hm_diffs[:12]],
    }
    if hm_diffs:
        failures.append(f"heightmap: {len(hm_diffs)} differing columns")

    id_map = meta["pending_ticks"]["fluid_id_map"]
    port_ticks = port_fluid_ticks(workdir / "fluid_ticks.tsv", id_map)
    native_ticks = {(t["x"], t["y"], t["z"], t["i"])
                    for t in meta["pending_ticks"]["fluid_ticks"]}
    tick_diffs = {
        "port_only": sorted(port_ticks - native_ticks),
        "native_only": sorted(native_ticks - port_ticks),
    }
    report["arms"]["pending_ticks"] = {
        "native_fluid_ticks": len(native_ticks),
        "port_fluid_ticks": len(port_ticks),
        "port_only": len(tick_diffs["port_only"]),
        "native_only": len(tick_diffs["native_only"]),
        "samples": {k: v[:12] for k, v in tick_diffs.items()},
        "block_ticks": {"native": meta["pending_ticks"]["block_ticks_total"],
                        "port": "not emitted by MEGU_WG_FLUID_TICKS"},
        "block_entities": {"native": meta["chunk"]["block_entities"],
                           "port": "not emitted by the offline dump"},
    }
    if tick_diffs["port_only"] or tick_diffs["native_only"]:
        failures.append(
            f"pending ticks: +{len(tick_diffs['port_only'])} port-only, "
            f"+{len(tick_diffs['native_only'])} native-only")

    if failures:
        report["status"] = "DIFF"
        report["failures"] = failures
    return report


def resolve_binary(args):
    if args.binary:
        binary = Path(args.binary).resolve()
        if not binary.is_file():
            raise GateError(f"--binary does not exist: {binary}")
        return binary
    binary = ROOT / "target" / "debug" / "megu"
    if binary.is_file():
        return binary
    if args.build:
        cmd = ["cargo", "build", "--offline", "--locked",
               "--message-format=json-render-diagnostics"]
        proc = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True)
        if proc.returncode != 0:
            tail = "\n".join((proc.stdout + proc.stderr).splitlines()[-25:])
            raise GateError(f"cargo build failed (exit {proc.returncode}):\n{tail}")
        sys.path.insert(0, str(ROOT / "tools"))
        import cargo_executable
        try:
            return cargo_executable.executable(proc.stdout.splitlines())
        except Exception as exc:
            raise GateError(f"failed to resolve cargo executable: {exc}") from exc
    raise GateError("target/debug/megu not built; pass --binary or --build")


def execute(args):
    binary = resolve_binary(args)
    canon_rows, _ = load_canon_tsv(args.data / CANON.name)
    order = load_order_tsv(args.data / ORDER.name)
    meta = load_meta(args.data / META.name)
    census = validate_fixture(canon_rows, order, meta)

    if args.out_dir:
        workdir = args.out_dir.resolve()
        workdir.mkdir(parents=True, exist_ok=True)
        return measure(binary, canon_rows, meta, order, census, workdir, args)
    with tempfile.TemporaryDirectory(prefix="fossil-gate-") as tmp:
        return measure(binary, canon_rows, meta, order, census, Path(tmp), args)


def measure(binary, canon_rows, meta, order, census, workdir, args):
    reports = []
    artifact_hashes = None
    for attempt in range(args.repeat):
        run_dir = workdir / f"run{attempt}"
        run_dir.mkdir(parents=True, exist_ok=True)
        env = build_env(run_dir, args.data / ORDER.name)
        run_dump(binary, env, run_dir / f"dump{attempt}.log")
        hashes = {name: sha256_file(run_dir / name)
                  for name in ("dump.bin", "canon.tsv", "hm.bin", "fluid_ticks.tsv")}
        if artifact_hashes is None:
            artifact_hashes = hashes
        elif hashes != artifact_hashes:
            reports.append({"status": "DIFF",
                            "failures": [f"run {attempt} output differs from run 0 "
                                         f"(hashes {hashes} != {artifact_hashes})"]})
            break
        if attempt == 0:
            reports.append(compare_all(canon_rows, meta, run_dir))
    report = reports[0] if len(reports) == 1 else {
        "status": "DIFF" if any(r["status"] != "MATCH" for r in reports) else "MATCH",
        "runs": reports,
    }
    report.update({
        "fixture_id": meta["id"],
        "binary": str(binary),
        "binary_sha256": sha256_file(binary),
        "seed": SEED, "chunk": [CX, CZ], "preset": PRESET,
        "window": DUMP_WINDOW, "pop_order_chunks": len(order),
        "repeat": args.repeat,
        "census": census,
        "artifacts": artifact_hashes or {},
    })
    return report


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--binary", help="megu server binary (default: target/debug/megu)")
    ap.add_argument("--build", action="store_true",
                    help="cargo build --offline --locked when target/debug/megu is missing")
    ap.add_argument("--data", type=Path, default=DATA,
                    help="fixture directory (default: tests/data)")
    ap.add_argument("--repeat", type=int, default=1,
                    help="run the whole dump N times and require byte-identical outputs "
                         "(determinism arm; default: 1)")
    ap.add_argument("--out-dir", type=Path,
                    help="keep dump artifacts here (default: a temporary directory)")
    ap.add_argument("--json-out", type=Path, help="write the JSON report here")
    args = ap.parse_args(argv)

    if args.repeat < 1:
        print("INVALID  --repeat must be >= 1", file=sys.stderr)
        return 2
    try:
        report = execute(args)
    except (GateError, OSError, ValueError) as exc:
        report = {"status": "INVALID", "error": str(exc)}

    if args.json_out:
        args.json_out.write_text(json.dumps(report, indent=1) + "\n")
    status = report["status"]
    if status == "MATCH":
        arms = report.get("arms", {})
        print(f"fossil chunk gate: MATCH "
              f"({arms.get('states', {}).get('total_cells', '?')} cells, "
              f"{arms.get('heightmap', {}).get('columns', '?')} heightmap columns, "
              f"{arms.get('pending_ticks', {}).get('native_fluid_ticks', '?')} pending ticks, "
              f"repeat={report.get('repeat')})")
        return 0
    if status == "DIFF":
        print("fossil chunk gate: DIFF", file=sys.stderr)
        for failure in report.get("failures", []):
            print(f"  {failure}", file=sys.stderr)
        if "runs" in report:
            print(json.dumps(report["runs"], indent=1)[:4000], file=sys.stderr)
        else:
            print(json.dumps(report.get("arms", {}), indent=1)[:4000], file=sys.stderr)
        return 1
    print(f"fossil chunk gate: INVALID  {report.get('error')}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
