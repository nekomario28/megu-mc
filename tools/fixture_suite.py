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
* a fixture that records runtime writes is INVALID unless the row declares
  `inputs.expected_runtime_writes` = `{count, blocks, writes_sha256}` and the capture's own
  `run.log` reproduces all three exactly: the number of `RUNTIME ... wrote=true` lines, their
  canonical block-state census, and the sha256 of the sorted RUNTIME line payloads (the log
  prefix is stripped, every `wrote=true`/`wrote=false` line is hashed). A count without a
  hash, an unknown member, a census that differs by one cell, or a hash that differs in any
  way is INVALID. The declaration does not classify the writes: it pins them, so a captured
  world is compared at the stage it declares instead of being excused by a block name;
  `boundary.json`'s count and the `run.log`'s own `wrote=true` count must also agree;
* the row declares its reference and port generation stages, they are equal, and the
  reference stage matches the fixture's recorded one - a stage is part of the comparison
  identity (`evidence/gameplay/feature-parity/light-and-lost-writes-20261116.md`);
* the row's `world` is the fixture's own world - `boundary.json` sits in the fixture and
  records a `world_subdirectory`, so `region_world` must be `fixture/<world_subdirectory>`
  and `world` must be the fixture's `world` (or that subdirectory); a byte-identical copy of
  the world elsewhere with every sha256 re-bound is still INVALID, because the provenance
  would then describe another tree;
* every typed field (in `boundary.json` and in the row) is checked before it is used as a
  key or compared, so `dimension: {}` or a boolean `runtime_writes` is INVALID input
  rather than a `TypeError` out of the suite;
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
from collections import Counter
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
# The capture's agent prints one line per attempted runtime write, through the server's own
# logger, so the recorded line carries a timestamp and a level prefix before this payload.
RUNTIME_MARKER = "RUNTIME "
RUNTIME_STATE_RE = re.compile(r"\bstate=(\d+)\b")
EXPECTED_WRITES_KEYS = ("count", "blocks", "writes_sha256")

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
    "runtime_writes": (
        "a fixture with runtime writes is only accepted through a declared inputs.expected_runtime_writes "
        "{count, blocks, writes_sha256} that the capture's own run.log reproduces exactly (wrote=true line "
        "count, canonical block-state census, sha256 of the sorted RUNTIME line payloads); an undeclared "
        "fixture with any write stays INVALID"
    ),
    "not_covered": [
        "no capture, dump build or world regeneration in this run",
        "no automatic fixture discovery outside the manifest",
        "no writer or first-divergent-stage attribution beyond an attribution the manifest itself declares",
        "a declared runtime write is pinned, not classified: this suite does not decide whether the writes "
        "belong to generation or are post-generation ticks",
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
    # Types are checked before any dict/set lookup or comparison: `dimension` is used as a
    # `DIMENSION_PRESETS` key, so an object/list there is an invalid input, not a traceback.
    for key in ("mode", "dimension", "stage"):
        if not isinstance(boundary[key], str):
            raise Invalid(f"{path} field {key!r} must be a string, got {boundary[key]!r}")
    if boundary["mode"] != "frozen":
        raise Invalid(f"fixture mode is {boundary['mode']!r}, not 'frozen'")
    runtime_writes = require_int(boundary["runtime_writes"], f"{path} field 'runtime_writes'")
    for key in ("min_y", "height", "chunks", "seed"):
        require_int(boundary[key], f"{path} field {key!r}")
    subdir = boundary.get("world_subdirectory")
    if subdir is not None and (not isinstance(subdir, str) or not subdir.strip()):
        raise Invalid(f"{path} field 'world_subdirectory' must be a non-empty string, got {subdir!r}")
    if boundary["stage"] not in STAGES:
        raise Invalid(f"fixture stage {boundary['stage']!r} is not one of {list(STAGES)}")
    return boundary, path


def runtime_write_record(fixture):
    """The capture's own runtime-write record, or None when the fixture has no `run.log`.

    A capture's agent prints one `RUNTIME x y z state=<id> wrote=<true|false> stack=...`
    line per attempted write through the server's logger. `count`/`blocks` describe the
    lines with `wrote=true` (an attempted-but-refused write did not change the world), and
    `writes_sha256` pins *every* RUNTIME line so a recapture cannot keep the count and the
    census while changing the cells, the order or the caller. The payload is normalized to
    the text starting at `RUNTIME ` so the log's timestamp does not enter the hash: the two
    independent fortress captures (matrix-v1 `nat-4242-nether-fortress` and matrix-v2
    `nat-4242-nether-fortress-256`) hash identically after that strip and differently
    before it.
    """
    path = fixture / "run.log"
    if not path.is_file():
        return None
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError as exc:
        raise Invalid(f"cannot read {path}: {exc}") from exc
    lines = []
    for line in text.splitlines():
        index = line.find(RUNTIME_MARKER)
        if index < 0:
            continue
        payload = line[index:].rstrip()
        if payload:
            lines.append(payload)
    written = [line for line in lines if " wrote=true" in line]
    census = Counter()
    if written:
        try:
            table = chunk_triage.load_block_state_table()
        except TriageError as exc:
            raise Invalid(f"cannot name the runtime write states: {exc}") from exc
    for line in written:
        match = RUNTIME_STATE_RE.search(line)
        if match is None:
            raise Invalid(
                f"runtime write line in {path} records no state id, so its block cannot be named: "
                f"{line[:120]!r}"
            )
        state = int(match.group(1))
        name = table.get(state)
        if name is None:
            raise Invalid(
                f"runtime write state id {state} ({path}) is not in assets/blocks_1211.json; "
                "the census cannot be named, so no declaration can be verified"
            )
        census[name] += 1
    payload = "".join(line + "\n" for line in sorted(lines))
    return {
        "log": path,
        "count": len(written),
        "blocks": dict(sorted(census.items())),
        "writes_sha256": hashlib.sha256(payload.encode()).hexdigest(),
    }


def declared_runtime_writes(row):
    """The row's `inputs.expected_runtime_writes`, or None when it declares nothing."""
    inputs = row.get("inputs")
    if isinstance(inputs, dict) and "expected_runtime_writes" in inputs:
        return inputs["expected_runtime_writes"]
    return None


def check_runtime_writes(row, fixture, boundary, boundary_path, runtime):
    """Accept the fixture's runtime writes only through an exact, fail-closed declaration.

    Two things are decided here, in this order:

    1. the provenance must agree with itself - `boundary.json`'s `runtime_writes` is what the
       capture script counted, so it must equal the `run.log`'s own `wrote=true` count. This
       holds for every row, declared or not; a fixture whose count and log disagree is
       INVALID, because neither number can be trusted afterwards.
    2. a fixture with writes needs `inputs.expected_runtime_writes` = `{count, blocks,
       writes_sha256}` and the record must equal it in all three. No member is optional and no
       unknown member is ignored: a declaration this suite cannot verify is not a declaration.

    An undeclared fixture with zero writes stays on the default path; an undeclared fixture
    with a write is INVALID, exactly as before this rule existed.
    """
    recorded = boundary["runtime_writes"]
    declared = declared_runtime_writes(row)
    facts = {
        "boundary": recorded,
        "log": str(runtime["log"]) if runtime is not None else None,
        "count": runtime["count"] if runtime is not None else None,
        "blocks": runtime["blocks"] if runtime is not None else None,
        "writes_sha256": runtime["writes_sha256"] if runtime is not None else None,
        "declared": declared is not None,
        "verified": False,
    }
    if runtime is not None and runtime["count"] != recorded:
        raise Invalid(
            f"{boundary_path} records runtime_writes={recorded} but {runtime['log']} holds "
            f"{runtime['count']} RUNTIME line(s) with wrote=true; the capture's provenance "
            "disagrees with its own log, so neither count is usable"
        )
    if declared is None:
        if recorded != 0:
            raise Invalid(
                f"fixture records {recorded} runtime write(s): the saved world is not the "
                "declared generation stage (a block name does not attribute the writer) and this "
                "row declares no inputs.expected_runtime_writes {count, blocks, writes_sha256}"
            )
        return facts

    what = "inputs.expected_runtime_writes"
    if not isinstance(declared, dict):
        raise Invalid(f"{what} must be an object {{count, blocks, writes_sha256}}, got {declared!r}")
    unknown = sorted(key for key in declared if key not in EXPECTED_WRITES_KEYS)
    if unknown:
        raise Invalid(
            f"{what} carries key(s) this suite cannot verify: {unknown}; only "
            f"{list(EXPECTED_WRITES_KEYS)} are part of the rule"
        )
    missing = [key for key in EXPECTED_WRITES_KEYS if key not in declared]
    if missing:
        raise Invalid(
            f"{what} is missing {missing}: a declaration is {list(EXPECTED_WRITES_KEYS)} and "
            "a count alone cannot pin which writes they were"
        )
    count = require_int(declared["count"], f"{what}.count", minimum=0)
    blocks = declared["blocks"]
    if not isinstance(blocks, dict) or not blocks:
        raise Invalid(f"{what}.blocks must be a non-empty object {{canonical state name: count}}, got {blocks!r}")
    for name, value in blocks.items():
        if not isinstance(name, str) or not name.strip():
            raise Invalid(f"{what}.blocks key must be a non-empty string, got {name!r}")
        require_int(value, f"{what}.blocks[{name!r}]", minimum=1)
    writes_sha = declared["writes_sha256"]
    if not isinstance(writes_sha, str) or not SHA_RE.match(writes_sha.lower()):
        raise Invalid(f"{what}.writes_sha256 must be a hex sha256, got {writes_sha!r}")
    if runtime is None:
        raise Invalid(
            f"{what} is declared but {fixture} has no run.log, so the declared writes cannot be "
            "verified against the capture's own record"
        )
    if count != recorded or count != runtime["count"]:
        raise Invalid(
            f"{what}.count={count} != {boundary_path} runtime_writes={recorded} / "
            f"{runtime['log']} wrote=true line count={runtime['count']}: the declaration does "
            "not describe this fixture's runtime writes"
        )
    if blocks != runtime["blocks"]:
        raise Invalid(
            f"{what}.blocks {json.dumps(blocks, sort_keys=True)} != the capture's census "
            f"{json.dumps(runtime['blocks'], sort_keys=True)} ({runtime['log']}); a declared row "
            "whose writes differ in any way is not comparable"
        )
    if writes_sha.lower() != runtime["writes_sha256"]:
        raise Invalid(
            f"{what}.writes_sha256={writes_sha} != the sha256 of the sorted RUNTIME lines in "
            f"{runtime['log']} ({runtime['writes_sha256']}); the declared writes are not the "
            "captured ones"
        )
    facts["verified"] = True
    return facts


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
    if not isinstance(kind, str):
        raise Invalid(f"kind must be a string ('chunk' or 'sculk'), got {kind!r}")
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
        value = stage.get(side)
        if not isinstance(value, str):
            raise Invalid(f"stage.{side} must be a string, got {value!r}")
        if value not in STAGES:
            raise Invalid(f"stage.{side} {value!r} is not one of {list(STAGES)}")
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
    runtime = check_runtime_writes(row, fixture, boundary, boundary_path, runtime_write_record(fixture))
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
    if declared_preset is not None:
        if not isinstance(declared_preset, str):
            raise Invalid(f"declared preset must be a string, got {declared_preset!r}")
        if declared_preset != preset:
            raise Invalid(f"declared preset {declared_preset!r} != fixture dimension preset {preset!r}")
    declared_dim = row.get("dim")
    if declared_dim is not None:
        if not isinstance(declared_dim, str):
            raise Invalid(f"declared dim must be a string, got {declared_dim!r}")
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

    # `boundary.json` describes one fixture, so the world must be that fixture's world: a
    # byte-identical copy elsewhere (with every sha256 re-bound to the copy) is a different
    # world, and accepting it would let the recorded stage/window describe another tree.
    subdir = boundary.get("world_subdirectory")
    if subdir is not None:
        expected_region_world = (fixture / subdir).resolve()
        if not expected_region_world.is_relative_to(fixture):
            raise Invalid(
                f"{boundary_path} records world_subdirectory {subdir!r}, which resolves outside "
                f"the fixture {fixture}"
            )
        if region_world != expected_region_world:
            raise Invalid(
                f"row world {world} is not the fixture's world: it holds the {preset} regions in "
                f"{region_world}, but {boundary_path} records {expected_region_world}"
            )
        allowed_worlds = {(fixture / "world").resolve(), expected_region_world}
        if world not in allowed_worlds:
            allowed = " or ".join(str(path) for path in sorted(allowed_worlds, key=str))
            raise Invalid(
                f"row world {world} is not the fixture's world: {boundary_path} belongs to "
                f"{fixture}, so world must be {allowed}"
            )
    else:
        fixture_world = (fixture / "world").resolve()
        if world != fixture_world:
            raise Invalid(
                f"row world {world} is not the fixture's world {fixture_world}: {boundary_path} "
                "records no world_subdirectory, so only the fixture's own world can be compared"
            )
        if not region_world.is_relative_to(world):
            raise Invalid(f"region world {region_world} is not inside the fixture's world {world}")

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
        "order": optional.get("order"), "writer": writer, "runtime_writes": runtime,
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
    lines.append(f"- runtime writes: {SCOPE['runtime_writes']}")
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
        writes = (row.get("provenance") or {}).get("runtime_writes") or {}
        if writes:
            lines.append(
                f"- runtime writes: fixture={writes.get('boundary')} log_wrote_true={writes.get('count')} "
                f"declared={writes.get('declared')} verified={writes.get('verified')}"
            )
            if writes.get("blocks"):
                lines.append(f"- runtime write census: {json.dumps(writes.get('blocks'), sort_keys=True)}")
            if writes.get("writes_sha256"):
                lines.append(f"- runtime write lines sha256: `{writes.get('writes_sha256')}`")
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
                           "dimension": facts["dimension"], "preset": facts["preset"],
                           "runtime_writes": facts["runtime_writes"]},
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
