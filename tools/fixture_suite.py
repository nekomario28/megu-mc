#!/usr/bin/env python3
"""Run the fixtures a manifest declares through the existing comparators, and inventory every row.

  tools/fixture_suite.py <manifest.json> --out <empty-dir> [--timeout SECONDS]
                                  [--chunk-tool PATH] [--sculk-tool PATH]

This is a thin orchestration layer, not a second comparator. `kind: chunk` shells out to
`tools/chunk_triage.py` (`--port-bin --n --bin-ox/--bin-oz/--bin-n --min-y --height
--preset --fail-on-diff --json-out`) and `kind: sculk` to `tools/sculk_gate.py`
(`--require-match --json-out`). Both children are executed as an argument list with
`shell=False`; the suite stores their raw stdout, stderr and exit code and reads the
*child's own JSON* for the counts. Nothing is re-implemented and no count is typed in by
hand.

What a row must prove before its comparison is accepted (no green by silence):

* the fixture's `boundary.json` (written by `tools/carve-agent/native_fixture.sh`) exists
  and records `mode: frozen`, `runtime_writes: 0`, a declared generation `stage`, and a
  `seed`/`dimension`/`min_y`/`height`/`chunks` window;
* the row declares its reference and port generation stages, they are equal, and the
  reference stage matches the fixture's recorded one - a stage is part of the comparison
  identity (`evidence/gameplay/feature-parity/light-and-lost-writes-20261116.md`);
* `level.dat`'s `WorldGenSettings.seed` equals the declared seed, and the row's grid
  equals the fixture's `min_y`/`height`;
* every input the comparison actually reads - `level.dat`, each region file of the
  window, the port dump - is listed in `inputs.sha256` and the file on disk still hashes
  to the declared value. A stale or replaced artifact cannot pass;
* the child itself compared all `n*n` chunks, skipped none, and compared a non-zero
  number of cells. A child that exits 0 with a truncated JSON, a partial grid or zero
  comparisons is INVALID, not MATCH.

Missing evidence is never repaired by inference: a row with no `writer_trace` reports
`writer.status = "UNKNOWN"` and is still a valid MATCH when everything else is bound.
Nothing here classifies a block name into a writer or a "first divergent stage".

Scope: **all rows of the manifest you pass**, and only those. There is no discovery of
other repository data, no capture, no dump build, no world regeneration, and no network.
A row is one saved native artifact against one saved port artifact; see
`specs/project-convergence/fixture-suite.md` for the manifest schema.

Exit codes:
  0  every declared row is a valid MATCH with a positive comparison
  1  at least one valid row has differences (and none is invalid)
  2  invalid/missing/unknown input, a child crash/contract violation, a zero comparison,
     or a manifest that declares no rows
"""
import argparse
import datetime
import gzip
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TOOLS = ROOT / "tools"
sys.path.insert(0, str(TOOLS))
from vanilla_region_blocks import parse_nbt  # noqa: E402
import chunk_triage  # noqa: E402
from chunk_triage import TriageError, resolve_dimension_world, resolve_level_dat  # noqa: E402

DEFAULT_CHUNK_TOOL = TOOLS / "chunk_triage.py"
DEFAULT_SCULK_TOOL = TOOLS / "sculk_gate.py"
STAGES = ("terrain", "carvers", "features", "structures")
# The fixture's own dimension spelling -> the `--preset` name `chunk_triage` accepts.
DIMENSION_PRESETS = {
    "minecraft:overworld": "overworld", "overworld": "overworld",
    "minecraft:the_nether": "nether", "the_nether": "nether",
    "minecraft:the_end": "end", "the_end": "end",
}
# `sculk_gate` reads the saved Overworld at the full dimension height; a fixture of any
# other window cannot be handed to it.
SCULK_WINDOW = ({"min_y": -64, "height": 384}, "overworld")
ROW_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
SHA_RE = re.compile(r"^[0-9a-f]{64}$")
STATUSES = ("MATCH", "DIFF", "INVALID", "NOT_RUN")

SCOPE = {
    "covers": "every row declared in the manifest passed to this run; no other repository data is discovered or guessed",
    "chunk": (
        "chunk_triage compares canonical block-state names (minecraft:air vs cave_air vs void_air "
        "kept distinct) for every cell of the declared n*n window; heightmaps, block entities and "
        "structures are reported by the child but are not accepted here - this is not a whole-canonical-state proof"
    ),
    "sculk": (
        "sculk_gate compares canonical state ids of the sculk family (sculk, sculk_vein, "
        "sculk_catalyst, sculk_shrieker) in the saved reference region; a same-name/different-property "
        "cell is a difference, and this is not whole-world parity"
    ),
    "stage": (
        "a MATCH additionally requires frozen fixture provenance, equal declared reference/port stages, "
        "matched seed/dimension/grid, and sha256 bindings that still match the files read"
    ),
    "not_covered": [
        "no capture, dump build or world regeneration in this run",
        "no automatic fixture discovery outside the manifest",
        "no writer or first-divergent-stage attribution beyond an attribution the manifest itself declares",
    ],
}


class SuiteError(Exception):
    """A manifest- or run-level problem that stops the suite (exit 2)."""


class Invalid(Exception):
    """One row's inputs cannot be accepted; the row is inventoried as INVALID."""


class ChildInvalid(Invalid):
    """A comparator ran but its output cannot be accepted; its raw record stays on the row."""

    def __init__(self, message, command=None):
        super().__init__(message)
        self.command = command


def utcnow():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def load_manifest(path):
    try:
        raw = path.read_bytes()
    except OSError as exc:
        raise SuiteError(f"cannot read manifest {path}: {exc}") from exc
    try:
        doc = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise SuiteError(f"manifest {path} is not valid JSON: {exc}") from exc
    if not isinstance(doc, dict):
        raise SuiteError("manifest root must be a JSON object")
    if doc.get("version") != 1:
        raise SuiteError(f"unsupported manifest version {doc.get('version')!r}; this tool reads version 1")
    rows = doc.get("rows")
    if not isinstance(rows, list) or not rows:
        raise SuiteError("manifest needs a non-empty 'rows' list")
    return raw, doc, rows


def declared_ids(rows):
    """The row ids in declaration order, so a failed run still inventories them."""
    ids = []
    for row in rows:
        rid = row.get("id") if isinstance(row, dict) else None
        ids.append(rid if isinstance(rid, str) else None)
    return ids


def resolve_path(base, value, what):
    if not isinstance(value, str) or not value.strip():
        raise Invalid(f"{what} must be a non-empty string")
    path = Path(value)
    if not path.is_absolute():
        path = base / path
    return path.resolve()


def require_int(value, what, *, minimum=None):
    if isinstance(value, bool) or not isinstance(value, int):
        raise Invalid(f"{what} must be an integer, got {value!r}")
    if minimum is not None and value < minimum:
        raise Invalid(f"{what} must be >= {minimum}, got {value}")
    return value


def read_level_seed(path):
    raw = path.read_bytes()
    if raw[:2] == b"\x1f\x8b":
        raw = gzip.decompress(raw)
    try:
        nbt, _ = parse_nbt(raw)
    except Exception as exc:  # a corrupt level.dat is an input error, not a traceback
        raise Invalid(f"cannot parse {path}: {exc}") from exc
    seed = ((nbt.get("Data") or {}).get("WorldGenSettings") or {}).get("seed")
    if isinstance(seed, bool) or not isinstance(seed, int):
        raise Invalid(f"{path} has no integer Data.WorldGenSettings.seed; the declared seed is unverified")
    return seed


def read_boundary(fixture):
    path = fixture / "boundary.json"
    if not path.is_file():
        raise Invalid(
            f"fixture {fixture} has no boundary.json: the declared generation stage and window are "
            "unverified, so this row cannot be green"
        )
    try:
        boundary = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as exc:
        raise Invalid(f"cannot read {path}: {exc}") from exc
    if not isinstance(boundary, dict):
        raise Invalid(f"{path} is not a JSON object")
    for key in ("mode", "dimension", "min_y", "height", "seed", "stage", "runtime_writes", "chunks"):
        if key not in boundary:
            raise Invalid(f"{path} is missing {key!r}; the fixture provenance is incomplete")
    if boundary["mode"] != "frozen":
        raise Invalid(f"fixture mode is {boundary['mode']!r}, not 'frozen'")
    if boundary["runtime_writes"] != 0:
        raise Invalid(
            f"fixture records {boundary['runtime_writes']} runtime write(s): the saved world is not the "
            "declared generation stage (a block name does not attribute the writer)"
        )
    if boundary["stage"] not in STAGES:
        raise Invalid(f"fixture stage {boundary['stage']!r} is not one of {list(STAGES)}")
    return boundary, path


def potential_inputs(row, base):
    """Paths a row names, resolved leniently: the output dir is checked against these.

    A row may fail validation before its inputs are proven, but the suite must still
    refuse to write its report into (or over) a directory the row pointed at.
    """
    paths = []
    if not isinstance(row, dict):
        return paths
    named = []
    for key in ("fixture", "world", "port"):
        named.append(row.get(key))
    inputs = row.get("inputs")
    if isinstance(inputs, dict):
        bindings = inputs.get("sha256")
        if isinstance(bindings, dict):
            named.extend(bindings.keys())
        for key in ("order", "writer_trace"):
            binding = inputs.get(key)
            if isinstance(binding, dict):
                named.append(binding.get("path"))
    for value in named:
        if isinstance(value, str) and value.strip():
            path = Path(value)
            paths.append(path if path.is_absolute() else base / path)
    return paths


def validate_row(row, ctx):
    """Resolve and prove one row's inputs; raise Invalid with the concrete reason."""
    if not isinstance(row, dict):
        raise Invalid("row must be a JSON object")
    rid = row.get("id")
    if not isinstance(rid, str) or not ROW_ID_RE.match(rid):
        raise Invalid(f"id must match {ROW_ID_RE.pattern}, got {rid!r}")
    if rid in ctx["duplicate_ids"]:
        raise Invalid(f"duplicate row id {rid!r} (ids must be distinct)")
    kind = row.get("kind")
    if kind not in ("chunk", "sculk"):
        raise Invalid(f"kind must be 'chunk' or 'sculk', got {kind!r}")

    n = require_int(row.get("n"), "n", minimum=1)
    origin = row.get("origin")
    if not isinstance(origin, list) or len(origin) != 2:
        raise Invalid(f"origin must be [cx, cz], got {origin!r}")
    cx, cz = (require_int(origin[0], "origin[0]"), require_int(origin[1], "origin[1]"))
    seed = require_int(row.get("seed"), "seed")
    grid = row.get("grid")
    if not isinstance(grid, dict):
        raise Invalid("grid must be an object {'min_y': int, 'height': int}")
    min_y = require_int(grid.get("min_y"), "grid.min_y")
    height = require_int(grid.get("height"), "grid.height", minimum=16)
    if height % 16 or min_y % 16:
        raise Invalid(f"grid must be section aligned, got min_y={min_y} height={height}")

    stage = row.get("stage")
    if not isinstance(stage, dict):
        raise Invalid("stage must be an object {'reference': ..., 'port': ...}")
    for side in ("reference", "port"):
        if stage.get(side) not in STAGES:
            raise Invalid(f"stage.{side} {stage.get(side)!r} is not one of {list(STAGES)}")
    if stage["reference"] != stage["port"]:
        raise Invalid(
            f"reference stage {stage['reference']!r} != port stage {stage['port']!r}: "
            "an unmatched generation stage is not a comparison"
        )

    base = ctx["manifest_dir"]
    fixture = resolve_path(base, row.get("fixture"), "fixture")
    if not fixture.is_dir():
        raise Invalid(f"fixture is not a directory: {fixture}")
    boundary, boundary_path = read_boundary(fixture)
    if stage["reference"] != boundary["stage"]:
        raise Invalid(
            f"declared reference stage {stage['reference']!r} != fixture stage {boundary['stage']!r} "
            f"({boundary_path})"
        )
    dimension = boundary["dimension"]
    preset = DIMENSION_PRESETS.get(dimension)
    if preset is None:
        raise Invalid(f"fixture dimension {dimension!r} is not one this suite maps to a chunk_triage preset")
    if require_int(boundary["seed"], "fixture seed") != seed:
        raise Invalid(f"declared seed {seed} != fixture seed {boundary['seed']}")
    for key, declared in (("min_y", min_y), ("height", height)):
        if boundary[key] != declared:
            raise Invalid(f"declared grid {key}={declared} != fixture {key}={boundary[key]}")
    if boundary["chunks"] != n * n:
        raise Invalid(f"declared window is {n}x{n} chunks but the fixture recorded {boundary['chunks']}")

    declared_preset = row.get("preset")
    if declared_preset is not None and declared_preset != preset:
        raise Invalid(f"declared preset {declared_preset!r} != fixture dimension preset {preset!r}")
    declared_dim = row.get("dim")
    if declared_dim is not None:
        try:
            dim_dir = chunk_triage.resolve_dimension_dir(fixture, dim=declared_dim)
        except TriageError as exc:
            raise Invalid(str(exc)) from exc
        expected_dir = chunk_triage.DIMENSION_ALIASES.get(preset)
        if dim_dir != expected_dir:
            raise Invalid(f"declared dim {declared_dim!r} resolves to {dim_dir!r}, fixture needs {expected_dir!r}")

    if kind == "sculk":
        window, sculk_preset = SCULK_WINDOW
        if preset != sculk_preset or (min_y, height) != (window["min_y"], window["height"]):
            raise Invalid(
                "sculk_gate reads the saved Overworld at the full dimension height "
                f"(preset={sculk_preset}, min_y={window['min_y']}, height={window['height']}); "
                f"this row is {preset} {min_y}/{height}"
            )

    world = resolve_path(base, row.get("world"), "world")
    if not world.is_dir():
        raise Invalid(f"world is not a directory: {world}")
    level_dat = resolve_level_dat(world)
    if level_dat is None:
        raise Invalid(f"{world} has no level.dat, so the world seed is unverified")
    level_dat = Path(level_dat).resolve()
    actual_seed = read_level_seed(level_dat)
    if actual_seed != seed:
        raise Invalid(f"level.dat seed {actual_seed} != declared seed {seed}")
    try:
        region_world = Path(resolve_dimension_world(str(world), preset=preset)).resolve()
    except TriageError as exc:
        raise Invalid(str(exc)) from exc

    port = resolve_path(base, row.get("port"), "port")
    if not port.is_file():
        raise Invalid(f"port dump is not a file: {port}")

    window_chunks = [(cx + dx, cz + dz) for dz in range(n) for dx in range(n)]
    region_files = sorted({(region_world / "region" / f"r.{x >> 5}.{z >> 5}.mca") for x, z in window_chunks})
    missing_regions = [str(p) for p in region_files if not p.is_file()]
    if missing_regions:
        raise Invalid(f"native region file(s) for the window are missing: {missing_regions}")

    # Every input the comparison reads has to be bound, and the bytes on disk have to
    # still match. Otherwise a replaced or stale artifact is indistinguishable from a pass.
    inputs = row.get("inputs")
    if not isinstance(inputs, dict) or not isinstance(inputs.get("sha256"), dict) or not inputs["sha256"]:
        raise Invalid("row needs inputs.sha256 {path: sha256}; an unbound input cannot be accepted")
    declared = {}
    for key, value in inputs["sha256"].items():
        bound = resolve_path(base, key, "inputs.sha256 key")
        if not isinstance(value, str) or not SHA_RE.match(value.lower()):
            raise Invalid(f"inputs.sha256[{key!r}] must be a hex sha256")
        if not bound.is_file():
            raise Invalid(f"bound input does not exist: {bound}")
        declared[bound] = value.lower()
    required = [level_dat, port, *region_files]
    actual = {path: sha256_file(path) for path in set(required) | set(declared)}
    unbound = [str(p) for p in required if p not in declared]
    if unbound:
        raise Invalid(f"no sha256 binding for input(s) the comparison reads: {unbound}")
    for path in sorted(declared, key=str):
        if declared[path] != actual[path]:
            raise Invalid(
                f"sha256 mismatch for {path}: declared {declared[path]}, actual {actual[path]} "
                "(the input is stale or was replaced)"
            )

    optional = {}
    for key in ("order", "writer_trace"):
        binding = inputs.get(key)
        if binding is None:
            continue
        if not isinstance(binding, dict):
            raise Invalid(f"inputs.{key} must be an object with path and sha256")
        path = resolve_path(base, binding.get("path"), f"inputs.{key}.path")
        declared_sha = binding.get("sha256")
        if not isinstance(declared_sha, str) or not SHA_RE.match(declared_sha.lower()):
            raise Invalid(f"inputs.{key}.sha256 must be a hex sha256")
        if not path.is_file():
            raise Invalid(f"inputs.{key}.path does not exist: {path}")
        real_sha = sha256_file(path)
        if real_sha != declared_sha.lower():
            raise Invalid(f"sha256 mismatch for inputs.{key} {path}: declared {declared_sha}, actual {real_sha}")
        entry = {"path": str(path), "sha256": real_sha}
        if key == "writer_trace":
            attribution = binding.get("attribution")
            if attribution is not None:
                if not isinstance(attribution, str) or not attribution.strip():
                    raise Invalid("inputs.writer_trace.attribution must be a non-empty string when present")
                entry["attribution"] = attribution
        optional[key] = entry

    if "writer_trace" in optional:
        writer = {
            "status": "DECLARED",
            "reason": "attribution is the manifest's declaration for the bound trace; the suite does not derive one",
            "trace": optional["writer_trace"],
            "attribution": optional["writer_trace"].get("attribution"),
        }
    else:
        writer = {
            "status": "UNKNOWN",
            "reason": "no writer trace bound in this row; a block name is not a writer",
        }

    facts = {
        "id": rid, "kind": kind, "n": n, "origin": [cx, cz], "seed": seed,
        "min_y": min_y, "height": height, "preset": preset,
        "dimension": dimension, "stage": {"reference": stage["reference"], "port": stage["port"],
                                         "fixture": boundary["stage"]},
        "fixture": fixture, "boundary": {"path": str(boundary_path), "sha256": sha256_file(boundary_path)},
        "seed_declared": seed, "seed_fixture": boundary["seed"], "seed_level_dat": actual_seed,
        "world": world, "region_world": region_world, "level_dat": level_dat, "port": port,
        "region_files": region_files, "input_sha256": {str(p): actual[p] for p in required},
        "order": optional.get("order"), "writer": writer,
    }
    return facts


def write_raw(raw_dir, rid, argv, completed, started, finished):
    stdout_path = raw_dir / f"{rid}.stdout"
    stderr_path = raw_dir / f"{rid}.stderr"
    stdout_path.write_bytes(completed.stdout or b"")
    stderr_path.write_bytes(completed.stderr or b"")
    command = {
        "argv": argv,
        "exit": completed.returncode,
        "started": started,
        "finished": finished,
        "stdout": str(stdout_path),
        "stderr": str(stderr_path),
    }
    (raw_dir / f"{rid}.command.json").write_text(json.dumps(command, indent=2) + "\n")
    return command, stdout_path, stderr_path


def run_child(facts, ctx):
    """Run the comparator for one validated row and return (status, reason, comparison, command)."""
    rid = facts["id"]
    n, (cx, cz) = facts["n"], facts["origin"]
    raw_dir = ctx["raw_dir"]
    json_path = raw_dir / f"{rid}.json"
    if facts["kind"] == "chunk":
        tool = ctx["chunk_tool"]
        argv = [
            str(ctx["python"]), str(tool), str(facts["world"]), str(cx), str(cz),
            "--port-bin", str(facts["port"]),
            "--n", str(n), "--bin-ox", str(cx), "--bin-oz", str(cz), "--bin-n", str(n),
            "--min-y", str(facts["min_y"]), "--height", str(facts["height"]),
            "--preset", facts["preset"],
            "--fail-on-diff", "--json-out", str(json_path),
        ]
    else:
        tool = ctx["sculk_tool"]
        argv = [
            str(ctx["python"]), str(tool), str(facts["world"]), str(facts["port"]),
            str(n), str(cx), str(cz), "--require-match", "--json-out", str(json_path),
        ]

    started = utcnow()
    try:
        completed = subprocess.run(argv, shell=False, capture_output=True, timeout=ctx["timeout"])
    except subprocess.TimeoutExpired as exc:
        partial = subprocess.CompletedProcess(argv, -1, exc.stdout, exc.stderr)
        command, _, _ = write_raw(raw_dir, rid, argv, partial, started, utcnow())
        raise ChildInvalid(f"comparator timed out after {ctx['timeout']}s", command=command) from exc
    except OSError as exc:
        raise Invalid(f"cannot execute comparator {tool}: {exc}") from exc
    finished = utcnow()
    command, stdout_path, stderr_path = write_raw(raw_dir, rid, argv, completed, started, finished)
    command["stdout"], command["stderr"] = str(stdout_path), str(stderr_path)
    command["json"] = str(json_path)

    stderr_tail = (completed.stderr or b"").decode("utf-8", "replace").strip().splitlines()[-3:]
    if completed.returncode not in (0, 1, 2):
        raise ChildInvalid(f"comparator crashed with exit {completed.returncode}: {stderr_tail}",
                           command=command)
    if completed.returncode == 2:
        raise ChildInvalid("comparator reported invalid input (exit 2): " + " | ".join(stderr_tail),
                           command=command)
    try:
        report = json.loads(json_path.read_text())
    except (OSError, json.JSONDecodeError) as exc:
        raise ChildInvalid(f"comparator JSON is missing or truncated ({json_path}): {exc}",
                           command=command) from exc
    if not isinstance(report, dict):
        raise ChildInvalid(f"comparator JSON is not an object: {json_path}", command=command)
    if "exit_code" in report and report["exit_code"] != completed.returncode:
        raise ChildInvalid(
            f"comparator JSON says exit {report['exit_code']} but the process exited {completed.returncode}",
            command=command,
        )

    if facts["kind"] == "chunk":
        # `chunk_triage --n 1` reports one chunk (`chunk`, `native_a_meta.total`) while
        # `--n > 1` reports a grid (`compared_chunks`, `totals.total_cells`). Both shapes
        # are the child's own measurement; read whichever this run produced.
        if "grid" in report:
            compared = report.get("compared_chunks")
            skipped = report.get("skipped") or []
            totals = report.get("totals") or {}
            diffs = totals.get("all_air_diffs")
            cells = totals.get("total_cells")
        else:
            if report.get("chunk") != [cx, cz]:
                raise ChildInvalid(
                    f"chunk_triage reported chunk {report.get('chunk')}, row asks ({cx},{cz})",
                    command=command,
                )
            compared = 1
            skipped = []
            totals = report.get("totals") or {}
            diffs = totals.get("all_air_diffs")
            cells = (report.get("native_a_meta") or {}).get("total")
        if not isinstance(compared, int) or not isinstance(diffs, int) or not isinstance(cells, int):
            raise ChildInvalid(f"chunk_triage JSON lacks compared/cell totals: {json_path}",
                               command=command)
        if skipped:
            raise ChildInvalid(f"chunk_triage skipped {len(skipped)} chunk(s): {skipped}",
                               command=command)
        if compared != n * n:
            raise ChildInvalid(f"chunk_triage compared {compared}/{n * n} chunks", command=command)
        if cells <= 0:
            raise ChildInvalid("chunk_triage compared zero cells", command=command)
        comparison = {"compared_chunks": compared, "scoped_cells": cells, "diffs": diffs,
                      "totals": totals, "skipped": skipped, "heightmaps": report.get("heightmaps")}
    else:
        compared = report.get("chunks_compared")
        expected = report.get("chunks_expected")
        counts = report.get("counts") or {}
        diffs = counts.get("diffs")
        reference = report.get("reference") or {}
        ref_total = reference.get("total")
        if not isinstance(compared, int) or not isinstance(diffs, int) or not isinstance(ref_total, int):
            raise ChildInvalid(f"sculk_gate JSON lacks chunks_compared/counts/reference: {json_path}",
                               command=command)
        if expected != n * n or compared != n * n:
            raise ChildInvalid(f"sculk_gate compared {compared}/{expected} chunks, row declares {n}x{n}",
                               command=command)
        if ref_total <= 0:
            raise ChildInvalid("sculk_gate found no sculk cells in the reference, so it proves nothing",
                               command=command)
        comparison = {"compared_chunks": compared, "positive_reference_cells": ref_total,
                      "port_cells": (report.get("port") or {}).get("total"),
                      "diffs": diffs, "counts": counts, "reference": reference,
                      "port": report.get("port")}

    if completed.returncode == 0 and diffs != 0:
        raise ChildInvalid(f"comparator exited 0 but its JSON reports {diffs} difference(s)",
                           command=command)
    if completed.returncode == 1 and diffs <= 0:
        raise ChildInvalid("comparator exited 1 but its JSON reports no differences", command=command)
    return ("MATCH" if completed.returncode == 0 else "DIFF"), comparison, command


def overall_code(rows):
    statuses = [row["status"] for row in rows]
    if not statuses:
        return 2
    if any(status in ("INVALID", "NOT_RUN") for status in statuses):
        return 2
    if all(status == "MATCH" for status in statuses):
        return 0
    if any(status == "DIFF" for status in statuses):
        return 1
    return 2


def source_state():
    record = {"root": str(ROOT), "head": None, "dirty": None, "dirty_paths": None}
    try:
        head = subprocess.run(["git", "-C", str(ROOT), "rev-parse", "HEAD"],
                              shell=False, capture_output=True, timeout=60)
        if head.returncode == 0:
            record["head"] = head.stdout.decode().strip() or None
        status = subprocess.run(["git", "-C", str(ROOT), "status", "--porcelain"],
                                shell=False, capture_output=True, timeout=60)
        if status.returncode == 0:
            lines = [line for line in status.stdout.decode().splitlines() if line.strip()]
            record["dirty"] = bool(lines)
            record["dirty_paths"] = len(lines)
    except (OSError, subprocess.TimeoutExpired) as exc:
        record["error"] = str(exc)
    return record


def ensure_output_dir(out, inputs):
    resolved = out.resolve()
    for path in inputs:
        path = Path(path).resolve()
        if resolved == path:
            raise SuiteError(f"output dir {resolved} is an input path")
        if resolved in path.parents:
            raise SuiteError(f"output dir {resolved} contains input {path}")
        if path in resolved.parents:
            raise SuiteError(f"output dir {resolved} is inside input {path}")
    if resolved.exists():
        if not resolved.is_dir():
            raise SuiteError(f"output path {resolved} exists and is not a directory")
        if any(resolved.iterdir()):
            raise SuiteError(f"output dir {resolved} is not empty; refusing to overwrite a previous run")
    return resolved


def summary_markdown(result):
    rows = result["rows"]
    lines = [
        "# Fixture suite result",
        "",
        f"- manifest: `{result['manifest']['path']}` (sha256 `{result['manifest']['sha256']}`)",
        f"- rows declared: {result['manifest']['rows_declared']}",
        f"- source HEAD: `{result['source']['head']}` dirty: {result['source']['dirty']}",
        f"- started: {result['started']}  finished: {result['finished']}",
        f"- exit code: {result['exit_code']}",
        f"- counts: {json.dumps(result['counts'], sort_keys=True)}",
        "",
        "## Rows",
        "",
        "| id | kind | status | diffs | compared | scope |",
        "| --- | --- | --- | --- | --- | --- |",
    ]
    for row in rows:
        cmp = row.get("comparison") or {}
        lines.append(
            f"| {row.get('id')} | {row.get('kind')} | {row['status']} | "
            f"{cmp.get('diffs', '-')} | {cmp.get('compared_chunks', '-')} | {row.get('reason') or 'ok'} |"
        )
    lines += ["", "## Scope", ""]
    lines.append(f"- covers: {SCOPE['covers']}")
    lines.append(f"- chunk rows: {SCOPE['chunk']}")
    lines.append(f"- sculk rows: {SCOPE['sculk']}")
    lines.append(f"- acceptance: {SCOPE['stage']}")
    for item in SCOPE["not_covered"]:
        lines.append(f"- not covered: {item}")
    lines += ["", "## Per-row evidence", ""]
    for row in rows:
        lines.append(f"### {row.get('id')}")
        lines.append("")
        if row.get("reason"):
            lines.append(f"- status reason: {row['reason']}")
        stage = row.get("stage") or {}
        if stage:
            lines.append(f"- stage: reference={stage.get('reference')} port={stage.get('port')} fixture={stage.get('fixture')}")
        writer = row.get("writer") or {}
        if writer:
            lines.append(f"- writer: {writer.get('status')} ({writer.get('reason')})")
        window = row.get("window") or {}
        if window:
            lines.append(f"- window: origin={window.get('origin')} n={window.get('n')} grid={window.get('min_y')}/{window.get('height')}")
        for name, path in (row.get("inputs") or {}).get("sha256", {}).items():
            lines.append(f"- input sha256 `{name}` = `{path}`")
        command = row.get("command")
        if command:
            lines.append(f"- command exit: {command.get('exit')} ({command.get('started')} .. {command.get('finished')})")
            lines.append(f"- raw stdout/stderr: `{command.get('stdout')}` `{command.get('stderr')}`")
        lines.append("")
    return "\n".join(lines) + "\n"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("manifest", type=Path, help="manifest JSON (version 1)")
    parser.add_argument("--out", required=True, type=Path,
                        help="output directory; must be empty and outside every input")
    parser.add_argument("--timeout", type=int, default=300, help="per-comparator timeout (default 300s)")
    parser.add_argument("--chunk-tool", type=Path, default=DEFAULT_CHUNK_TOOL,
                        help="chunk comparator to execute (default tools/chunk_triage.py)")
    parser.add_argument("--sculk-tool", type=Path, default=DEFAULT_SCULK_TOOL,
                        help="sculk comparator to execute (default tools/sculk_gate.py)")
    args = parser.parse_args(argv)
    started = utcnow()

    try:
        raw, doc, rows = load_manifest(args.manifest.expanduser().resolve())
    except SuiteError as exc:
        print(f"fixture_suite: ERROR: {exc}", file=sys.stderr)
        return 2
    manifest_path = args.manifest.expanduser().resolve()
    ids = declared_ids(rows)
    seen = {}
    for rid in ids:
        if isinstance(rid, str):
            seen[rid] = seen.get(rid, 0) + 1
    duplicate_ids = {rid for rid, count in seen.items() if count > 1}
    ctx = {
        "manifest_dir": manifest_path.parent,
        "duplicate_ids": duplicate_ids,
        "python": Path(sys.executable),
        "chunk_tool": args.chunk_tool.expanduser().resolve(),
        "sculk_tool": args.sculk_tool.expanduser().resolve(),
        "timeout": args.timeout,
    }

    results = []
    facts_list = []
    for index, row in enumerate(rows):
        rid = ids[index]
        base = {"id": rid, "kind": row.get("kind") if isinstance(row, dict) else None,
                "status": "INVALID", "reason": None}
        try:
            facts = validate_row(row, ctx)
        except Invalid as exc:
            base["reason"] = str(exc)
            results.append(base)
            continue
        except (OSError, TriageError) as exc:
            base["reason"] = f"input error: {exc}"
            results.append(base)
            continue
        base["reason"] = None
        facts_list.append((len(results), facts))
        results.append(base)  # placeholder, filled after the child runs

    inputs = [manifest_path, ctx["chunk_tool"], ctx["sculk_tool"]]
    for row in rows:
        inputs.extend(potential_inputs(row, ctx["manifest_dir"]))
    try:
        out = ensure_output_dir(args.out, inputs)
    except SuiteError as exc:
        print(f"fixture_suite: ERROR: {exc}", file=sys.stderr)
        # No row ran and no artifact can be written, so the declared rows are still
        # inventoried instead of disappearing from the report.
        print(f"fixture_suite: NOT_RUN rows: {[rid for rid in ids]}", file=sys.stderr)
        return 2
    raw_dir = out / "raw"
    try:
        raw_dir.mkdir(parents=True)
    except OSError as exc:
        print(f"fixture_suite: ERROR: cannot create {raw_dir}: {exc}", file=sys.stderr)
        print(f"fixture_suite: NOT_RUN rows: {[fact['id'] for _, fact in facts_list]}", file=sys.stderr)
        return 2
    ctx["raw_dir"] = raw_dir

    for index, facts in facts_list:
        row_result = results[index]
        row_result.update({
            "kind": facts["kind"],
            "stage": facts["stage"],
            "writer": facts["writer"],
            "seed": {"declared": facts["seed_declared"], "fixture": facts["seed_fixture"],
                     "level_dat": facts["seed_level_dat"]},
            "window": {"origin": facts["origin"], "n": facts["n"],
                       "min_y": facts["min_y"], "height": facts["height"]},
            "provenance": {"fixture": str(facts["fixture"]), "boundary": facts["boundary"],
                           "dimension": facts["dimension"], "preset": facts["preset"]},
            "inputs": {"world": str(facts["world"]), "region_world": str(facts["region_world"]),
                       "port": str(facts["port"]),
                       "sha256": facts["input_sha256"]},
        })
        try:
            status, comparison, command = run_child(facts, ctx)
        except Invalid as exc:
            row_result["status"] = "INVALID"
            row_result["reason"] = str(exc)
            command = getattr(exc, "command", None)
            if command is not None:
                row_result["command"] = command
            continue
        row_result["status"] = status
        row_result["comparison"] = comparison
        row_result["command"] = command

    counts = {status: 0 for status in STATUSES}
    for row in results:
        counts[row["status"]] = counts.get(row["status"], 0) + 1
    code = overall_code(results)
    result = {
        "tool": "fixture_suite",
        "version": 1,
        "manifest": {"path": str(manifest_path), "sha256": hashlib.sha256(raw).hexdigest(),
                     "rows_declared": len(rows)},
        "source": source_state(),
        "tools": {
            "python": sys.version.split()[0],
            "chunk": {"path": str(ctx["chunk_tool"]), "sha256": sha256_file(ctx["chunk_tool"])
                      if ctx["chunk_tool"].is_file() else None},
            "sculk": {"path": str(ctx["sculk_tool"]), "sha256": sha256_file(ctx["sculk_tool"])
                      if ctx["sculk_tool"].is_file() else None},
        },
        "started": started,
        "finished": utcnow(),
        "out": str(out),
        "scope": SCOPE,
        "rows": results,
        "counts": counts,
        "exit_code": code,
    }
    try:
        (out / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        (out / "summary.md").write_text(summary_markdown(result))
    except OSError as exc:
        print(f"fixture_suite: ERROR: cannot write the report in {out}: {exc}", file=sys.stderr)
        return 2
    print(f"fixture_suite: {json.dumps(counts, sort_keys=True)} -> {out / 'result.json'}")
    return code


if __name__ == "__main__":
    sys.exit(main())
