#!/usr/bin/env python3
"""One repeatable recipe: (structure id, seed, chunk, dimension) -> native capture + triage + manifest row.

    tools/structure_capture.py --structure minecraft:igloo --seed 4242 --dimension overworld \
        --root <captures-dir> --tag nat-4242-igloo

Why this exists: every structure row in `captures/matrix-v1|v2` was produced by the same
six manual steps, and each one was a place to drift: pick a chunk where the structure
*actually* places a start (not merely one whose coordinates look right), capture the
pinned server at the `structures` stage with the decorate set recorded, dump the port,
triage against the recorded window, and bind every artifact the comparison reads into a
manifest row. The steps are already owned by other tools:

  1. **admission** - `mc-spine --worldgen-structure-candidates <seed> <cx> <cz>`, the port's
     own structural candidate/admit probe (`src/main.rs`). It reports, per structure set,
     the spread candidate of the chunk's grid cell, whether the frequency reducer allows
     it, and whether an exclusion zone is involved. It is *necessary*, not sufficient:
     the biome gate and piece generation are unchecked (`biome_and_pieces_unchecked=true`),
     so the native capture is the authority and step 3 verifies against it.
  2. **capture** - `tools/carve-agent/native_fixture.sh` (frozen, `MC_FIXTURE_STAGE=structures`,
     its own `order.tsv` decorate set, `boundary.json`, `native.bin`, `run.log`).
  3. **start verification** - the captured chunk's own `structures.starts` NBT. A capture
     whose saved chunk does not carry a start for the requested structure is REFUTED and
     the next candidate is tried; it is never silently promoted.
  4. **port comparison** - `tools/port_triage.py --height-window ...` (build, env, dump and
     triage in one fail-closed command), with the capture's `order.tsv` available as the
     `--pop-order` input.
  5. **manifest row** - `inputs.sha256` for `level.dat`, every region file of the window,
     `dump.bin`, `run.log` and `order.tsv`, plus `inputs.expected_runtime_writes` derived
     from the capture's own `run.log` through `tools/fixture_suite.py`'s reader (one owner:
     the row is built with the same code that later verifies it).

`--capture-dir <existing capture>` skips steps 1-2, which is how this recipe is validated:
capturing a row that already exists and diffing the new `native.bin` against the old one
proves the recipe reproduces the reference instead of merely producing *a* capture.

Nothing here writes to `src/`, edits a manifest that already exists, or removes a failed
capture. A refuted capture keeps its own directory and its reason (`refuted.txt`).

Exit codes:
  0  capture (or reuse) verified, port triage ran, row emitted
  1  port vs native differ (the row is still emitted, with the measured counts)
  2  invalid input, missing pinned jar/JDK, capture or triage failure
  3  no admitted candidate in the scanned range, or every candidate was refuted
     (no capture of the requested structure exists, so nothing was measured)
"""
import argparse
import difflib
import json
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TOOLS = ROOT / "tools"
sys.path.insert(0, str(TOOLS))
import chunk_triage  # noqa: E402
import fixture_suite  # noqa: E402

JAR_SHA1 = "59353fb40c36d304f2035d51e7d6e6baa98dc05c"
DIMENSIONS = {
    "overworld": {
        "vanilla": "minecraft:overworld",
        "preset": "overworld",
        "prefix": "ow",
        "window": {"noise": (-64, 384), "dimension": (-64, 384)},
    },
    "nether": {
        "vanilla": "minecraft:the_nether",
        "preset": "nether",
        "prefix": "nether",
        "window": {"noise": (0, 128), "dimension": (0, 256)},
    },
    "end": {
        "vanilla": "minecraft:the_end",
        "preset": "end",
        "prefix": "end",
        "window": {"noise": (0, 128), "dimension": (0, 256)},
    },
}
DIMENSION_ALIASES = {
    "minecraft:overworld": "overworld", "overworld": "overworld",
    "minecraft:the_nether": "nether", "the_nether": "nether", "nether": "nether",
    "minecraft:the_end": "end", "the_end": "end", "end": "end",
}


class RecipeError(Exception):
    """Fail-closed error: invalid input, or a stage that could not be completed."""


def log(message):
    print(message, flush=True)


# --------------------------------------------------------------------------- datapack


def datapack_files(subdir):
    """`<ns>:<name>` -> bytes for one `worldgen/<subdir>` directory, as `Gen::dp_files` reads it.

    The port resolves registry names from the pack tree, so the recipe resolves the
    structure -> set -> biome-tag graph from the same files instead of carrying a copy.
    Later packs win within a name, exactly like `dp_files`.
    """
    found = {}
    for pack in sorted((ROOT / "datapacks").glob("*")):
        data = pack / "data"
        if not data.is_dir():
            continue
        for namespace in sorted(data.iterdir()):
            base = namespace / "worldgen" / subdir
            if not base.is_dir():
                continue
            for path in sorted(base.rglob("*.json")):
                rel = path.relative_to(base).with_suffix("")
                found[f"{namespace.name}:{rel.as_posix()}"] = path.read_bytes()
    return found


def structure_sets_for(structure_id):
    """The structure sets that can place `structure_id`, with their placement parameters."""
    sets = {}
    known = set()
    for name, raw in sorted(datapack_files("structure_set").items()):
        try:
            doc = json.loads(raw)
        except json.JSONDecodeError as exc:
            raise RecipeError(f"{name} is not valid JSON: {exc}") from exc
        members = [entry.get("structure") for entry in doc.get("structures") or []]
        known.update(m for m in members if m)
        if structure_id not in members:
            continue
        placement = doc.get("placement") or {}
        sets[name] = {
            "placement": placement,
            "members": sorted(m for m in members if m),
            "raw": doc,
        }
    if not sets and known:
        # A capture's *name* is not its structure id (`nat-4242-jungle-temple` is
        # `minecraft:jungle_pyramid`), so a near miss is reported with the real ids.
        raise RecipeError(
            f"no structure set lists {structure_id}; close ids: "
            f"{difflib.get_close_matches(structure_id, sorted(known), n=4)}"
        )
    return sets


def resolve_biome_reference(reference, depth=0):
    """Expand a structure's `biomes` value (a `#tag` or a biome id) into concrete biome ids."""
    if depth > 4:
        raise RecipeError(f"biome tag nesting deeper than 4 at {reference}")
    if not reference.startswith("#"):
        return {reference}
    tag = reference[1:]
    namespace, _, name = tag.partition(":")
    if not name:
        namespace, name = "minecraft", namespace
    out = set()
    seen = False
    for pack in sorted((ROOT / "datapacks").glob("*")):
        path = pack / "data" / namespace / "tags" / "worldgen" / "biome" / f"{name}.json"
        # A tag may also be spelled as a directory of files; only the flat form is used by
        # the vanilla pack for `has_structure/*`, and a missing tag is reported, not guessed.
        if not path.is_file():
            continue
        seen = True
        doc = json.loads(path.read_bytes())
        for value in doc.get("values") or []:
            if isinstance(value, dict):
                value = value.get("id")
            if not isinstance(value, str):
                continue
            out |= resolve_biome_reference(value, depth + 1)
    if not seen:
        raise RecipeError(
            f"biome tag {reference} has no file under datapacks/*/data/{namespace}/tags/"
            "worldgen/biome; the structure's biome gate cannot be predicted"
        )
    return out


def structure_biomes(structure_id):
    files = datapack_files("structure")
    raw = files.get(structure_id)
    if raw is None:
        raise RecipeError(
            f"{structure_id} is not in the pack's worldgen/structure tree "
            f"({len(files)} structures present); the id is probably misspelled"
        )
    doc = json.loads(raw)
    biomes = doc.get("biomes")
    if not isinstance(biomes, str):
        raise RecipeError(f"{structure_id} declares no string `biomes` field ({biomes!r})")
    return resolve_biome_reference(biomes), doc


# ------------------------------------------------------------------- admission probe


def parse_candidates(text):
    """Parse `--worldgen-structure-candidates` output into `{set: {...}}`.

    Two line shapes, both tab separated (`src/main.rs`):
      <set>\t<cx,cz>\tfrequency=<bool>\texclusion_unchecked=<bool>\tbiome_and_pieces_unchecked=true
      <set>\t[(cx, cz), ...]\tpreferred_biome_relocation_unchecked=true\tbiome_and_pieces_unchecked=true
    The ring form has no cell candidate and no frequency: its points *are* the placement.
    """
    rows = {}
    for line in text.splitlines():
        if not line.strip():
            continue
        fields = line.split("\t")
        if len(fields) < 3:
            raise RecipeError(f"unrecognised candidate-probe line: {line[:160]!r}")
        set_name, placement, rest = fields[0], fields[1], fields[2:]
        entry = {
            "frequency": any(f == "frequency=true" for f in rest),
            "exclusion": any(f == "exclusion_unchecked=true" for f in rest),
            "rings": None,
            "candidate": None,
            "raw": "\t".join(rest),
        }
        if placement.startswith("["):
            points = re.findall(r"\((-?\d+),\s*(-?\d+)\)", placement)
            if not points:
                raise RecipeError(f"ring placement with no points: {line[:160]!r}")
            entry["rings"] = [(int(x), int(z)) for x, z in points]
        else:
            x, _, z = placement.partition(",")
            try:
                entry["candidate"] = (int(x), int(z))
            except ValueError as exc:
                raise RecipeError(f"unparsable candidate {placement!r} in {set_name}") from exc
            if not rest or not rest[0].startswith("frequency="):
                raise RecipeError(f"spread set {set_name} printed no frequency verdict: {line[:160]!r}")
        rows[set_name] = entry
    if not rows:
        raise RecipeError("candidate probe printed nothing; the datapack did not load")
    return rows


def probe_candidates(binary, seed, chunks):
    """Run the port's candidate probe once per chunk and return `[rows, ...]`."""
    out = []
    for cx, cz in chunks:
        proc = subprocess.run(
            [str(binary), "--worldgen-structure-candidates", str(seed), str(cx), str(cz)],
            cwd=ROOT, capture_output=True, text=True, check=False,
        )
        if proc.returncode != 0:
            tail = "\n".join((proc.stdout + proc.stderr).splitlines()[-15:])
            raise RecipeError(f"candidate probe failed for chunk ({cx},{cz}):\n{tail}")
        out.append((cx, cz, parse_candidates(proc.stdout)))
    return out


def admitted_candidates(binary, seed, structure_id, sets, origin, radius):
    """Chunk candidates, inside the scan box, of the sets that can place `structure_id`.

    The probe answers "which chunk is *this* chunk's cell candidate", and
    `floorDiv(cell * spacing, spacing) == cell` for negative coordinates too, so querying
    one cell corner enumerates that whole cell. The answer is therefore the candidate of
    the corner's cell, not the corner itself - a caller that compares them would only ever
    accept a cell whose offset happens to be zero.
    """
    found = []
    seen = set()
    ox, oz = origin
    for set_name, info in sorted(sets.items()):
        placement = info["placement"]
        if placement.get("type") != "minecraft:random_spread":
            continue
        spacing = int(placement["spacing"])
        lo = ((ox - radius) // spacing, (oz - radius) // spacing)
        hi = ((ox + radius) // spacing, (oz + radius) // spacing)
        cells = [(u, v) for v in range(lo[1], hi[1] + 1) for u in range(lo[0], hi[0] + 1)]
        rows = probe_candidates(binary, seed, [(u * spacing, v * spacing) for u, v in cells])
        for (u, v), (_, _, table) in zip(cells, rows):
            entry = table.get(set_name)
            if entry is None:
                raise RecipeError(f"candidate probe did not report {set_name}; the set list changed")
            chunk = entry["candidate"]
            if chunk is None or (set_name, chunk) in seen:
                continue
            seen.add((set_name, chunk))
            if not (ox - radius <= chunk[0] <= ox + radius):
                continue
            if not (oz - radius <= chunk[1] <= oz + radius):
                continue
            found.append({
                "set": set_name,
                "chunk": chunk,
                "cell": (u, v),
                "frequency": entry["frequency"],
                "exclusion": entry["exclusion"],
            })
    # Ring sets (strongholds) answer with their whole point list, which is not a cell
    # candidate: adding them to the spread scan would mix two placement families.
    return found


def verify_chunk_is_candidate(binary, seed, structure_id, sets, cx, cz):
    """Re-run the probe for one explicit chunk: is it a candidate for the requested structure?"""
    _, _, table = probe_candidates(binary, seed, [(cx, cz)])[0]
    verdict = []
    for set_name in sorted(sets):
        entry = table.get(set_name)
        if entry is None:
            raise RecipeError(f"candidate probe did not report {set_name}")
        if entry["rings"] is not None:
            ok = (cx, cz) in entry["rings"]
        else:
            ok = entry["candidate"] == (cx, cz)
        verdict.append({
            "set": set_name,
            "start_chunk_of_cell": entry["candidate"],
            "rings": entry["rings"],
            "is_own_candidate": ok,
            "frequency": entry["frequency"],
            "exclusion": entry["exclusion"],
        })
    return verdict


# -------------------------------------------------------------------------- biome hint


BIOMEAT_RE = re.compile(r"^BIOMEAT (-?\d+) (-?\d+) (-?\d+) blended=(\S+) container=(\S+)$")


def port_biomes(binary, seed, preset, positions):
    """`MC_SPINE_BIOME_AT` for a batch of block positions -> `{(x,y,z): (blended, container)}`."""
    if not positions:
        return {}
    spec = ";".join(f"{x},{y},{z}" for x, y, z in positions)
    env = {k: v for k, v in os.environ.items() if not k.startswith("MC_SPINE_")}
    env.update({
        "MC_SPINE_SEED": str(seed),
        "MC_SPINE_WG_PRESET": preset,
        "MC_SPINE_WG_PROFILE": "0",
        "MC_SPINE_BIOME_AT": spec,
    })
    proc = subprocess.run([str(binary)], cwd=ROOT, env=env, capture_output=True, text=True,
                             check=False)
    if proc.returncode != 0:
        tail = "\n".join((proc.stdout + proc.stderr).splitlines()[-15:])
        raise RecipeError(f"biome probe failed:\n{tail}")
    out = {}
    for line in (proc.stdout + proc.stderr).splitlines():
        match = BIOMEAT_RE.match(line.strip())
        if match is None:
            continue
        key = (int(match.group(1)), int(match.group(2)), int(match.group(3)))
        out[key] = (match.group(4), match.group(5))
    if len(out) != len(positions):
        raise RecipeError(
            f"biome probe answered {len(out)} of {len(positions)} positions; "
            "the hint would rank candidates on missing data"
        )
    return out


def biome_hint(binary, seed, preset, candidates, allowed):
    """Annotate candidates with the port's quart-container biome at the chunk's centre.

    A hint, not a verdict: the structure's own piece finder decides whether a start exists
    (`biome_and_pieces_unchecked=true` in the probe), and stage 3 measures that. The hint
    only orders the attempts so a capture is not spent on a deep-ocean-free candidate.
    """
    positions = [(cx * 16 + 8, 64, cz * 16 + 8) for cx, cz in candidates]
    answer = port_biomes(binary, seed, preset, positions)
    hinted = []
    for (cx, cz), pos in zip(candidates, positions):
        blended, container = answer[pos]
        hinted.append({"chunk": (cx, cz), "blended": blended, "container": container,
                       "passes": container in allowed})
    return hinted


# ----------------------------------------------------------------------------- capture


def run_capture(capture_dir, chunk, n, dimension, seed, height_window, jar_base, mem_max):
    """Run `native_fixture.sh` once. Returns the subprocess result (its stdout is the log)."""
    env = {k: v for k, v in os.environ.items() if not k.startswith("MC_FIXTURE_")}
    env.update({
        "MC_FIXTURE_DIMENSION": DIMENSIONS[dimension]["vanilla"],
        "MC_FIXTURE_STAGE": "structures",
        "MC_FIXTURE_HEIGHT_WINDOW": height_window,
        "MC_SPINE_SEED": str(seed),
        "CARVER_FIXTURE_BASE": str(jar_base),
    })
    command = ["bash", str(TOOLS / "carve-agent" / "native_fixture.sh"),
               str(capture_dir), str(chunk[0]), str(chunk[1]), str(n), "frozen"]
    if mem_max:
        # The capture's own JVM is 1 GiB; the scope bounds the shell + JVM so a runaway
        # world save cannot take the host (the fixture contract says "run inside a
        # MemoryMax scope", and this is that scope).
        command = ["systemd-run", "--scope", "--user", "-p", f"MemoryMax={mem_max}",
                   "--unit", f"scap-{capture_dir.name}-{os.getpid()}"] + command
    return subprocess.run(command, cwd=ROOT, env=env, capture_output=True, text=True,
                          check=False)


def native_start(native_world, structure_id, origin, n, preset):
    """Does the captured window carry a start for `structure_id`? Read from the world itself."""
    found = []
    cache = {}
    for dz in range(n):
        for dx in range(n):
            cx, cz = origin[0] + dx, origin[1] + dz
            nbt = chunk_triage.cached_read_chunk_nbt(native_world, cx, cz, cache)
            structs = nbt.get("structures") or nbt.get("Structures") or {}
            starts = structs.get("starts") or {}
            refs = structs.get("References") or {}
            start = starts.get(structure_id)
            if not isinstance(start, dict) or start.get("id") == "INVALID":
                continue
            for index, child in enumerate(start.get("Children") or []):
                found.append({
                    "chunk": [cx, cz],
                    "child_index": index,
                    "BB": list(child.get("BB") or []),
                })
            for packed in refs.get(structure_id) or []:
                found.append({"chunk": [cx, cz], "reference": chunk_triage.decode_chunk_pos_long(packed)})
    return found


# ----------------------------------------------------------------------------- manifest


def region_files(world, origin, n):
    """The region file of every chunk in the window (deduplicated, sorted)."""
    paths = set()
    for dz in range(n):
        for dx in range(n):
            cx, cz = origin[0] + dx, origin[1] + dz
            path = world / "region" / f"r.{cx >> 5}.{cz >> 5}.mca"
            paths.add(path)
    return sorted(paths)


def sha256_file(path):
    return chunk_triage.sha256_file(path)


def build_row(row_id, capture_dir, boundary, structure_id, dump_path, origin, n,
              height_window, manifest_dir, with_runtime_writes):
    """Assemble one `fixture_suite` row with every artifact the comparison reads bound."""
    dimension = DIMENSION_ALIASES[boundary["dimension"]]
    world = capture_dir / boundary["world_subdirectory"]

    def rel(path):
        return os.path.relpath(path.resolve(), manifest_dir.resolve()).replace(os.sep, "/")

    inputs = {"sha256": {}}
    bound = {
        rel(capture_dir / "world" / "level.dat"): capture_dir / "world" / "level.dat",
        rel(dump_path): dump_path,
    }
    for region in region_files(world, origin, n):
        bound[rel(region)] = region
    for extra in ("run.log", "order.tsv"):
        path = capture_dir / extra
        if path.is_file():
            bound[rel(path)] = path
    for key, path in sorted(bound.items()):
        if not path.is_file():
            raise RecipeError(f"the row would bind {key} but {path} does not exist")
        inputs["sha256"][key] = sha256_file(path)

    order = capture_dir / "order.tsv"
    if order.is_file():
        inputs["order"] = {"path": rel(order), "sha256": sha256_file(order)}

    if with_runtime_writes:
        record = fixture_suite.runtime_write_record(capture_dir)
        if record is None:
            raise RecipeError(f"{capture_dir} records {boundary['runtime_writes']} runtime writes "
                              "but has no run.log to declare them from")
        inputs["expected_runtime_writes"] = {
            "count": record["count"],
            "blocks": record["blocks"],
            "writes_sha256": record["writes_sha256"],
        }

    min_y, height = boundary["min_y"], boundary["height"]
    # Same shape as the committed rows (`ow-4242-structures-74--41--64-384`): each field is
    # joined with `-` and keeps its own sign, so the id of a window is a function of it.
    row_id = row_id or (
        f"{DIMENSIONS[dimension]['prefix']}-{boundary['seed']}-structures-"
        f"{origin[0]}-{origin[1]}-{min_y}-{height}" + (f"-n{n}" if n > 1 else "")
    )
    return {
        "id": row_id,
        "kind": "chunk",
        "fixture": rel(capture_dir),
        "world": rel(capture_dir / "world"),
        "port": rel(dump_path),
        "origin": [origin[0], origin[1]],
        "n": n,
        "grid": {"min_y": min_y, "height": height},
        "seed": boundary["seed"],
        "stage": {"reference": boundary["stage"], "port": boundary["stage"]},
        "inputs": inputs,
    }


def read_boundary(capture_dir):
    path = capture_dir / "boundary.json"
    if not path.is_file():
        raise RecipeError(f"{capture_dir} has no boundary.json; not a native_fixture.sh capture")
    try:
        boundary = json.loads(path.read_text())
    except json.JSONDecodeError as exc:
        raise RecipeError(f"{path} is not valid JSON: {exc}") from exc
    for field in ("mode", "dimension", "min_y", "height", "seed", "stage",
                  "world_subdirectory", "chunks", "runtime_writes"):
        if field not in boundary:
            raise RecipeError(f"{path} does not record {field!r}")
    return boundary


# -------------------------------------------------------------------------------- main


def resolve_binary(args):
    if args.binary:
        path = Path(args.binary)
        if not path.is_file():
            raise RecipeError(f"--binary does not exist: {path}")
        return path.resolve()
    path = ROOT / "target" / "debug" / "mc-spine"
    if not path.is_file():
        raise RecipeError(
            f"no mc-spine binary at {path}; build it first "
            "(cargo build --offline --locked) or pass --binary"
        )
    return path.resolve()


def check_jar_base(jar_base):
    jar = Path(jar_base) / "server.jar"
    if not jar.is_file():
        raise RecipeError(f"no server.jar under {jar_base}")
    got = "".join(subprocess.run(["sha1sum", str(jar)], capture_output=True, text=True, check=False)
                  .stdout.split()[:1])
    if got != JAR_SHA1:
        raise RecipeError(f"{jar} is sha1 {got}, not the pinned {JAR_SHA1}")
    for sub in ("libraries", "versions"):
        if not (Path(jar_base) / sub).is_dir():
            raise RecipeError(f"{jar_base} has no {sub}/; native_fixture.sh symlinks them")


def main(argv=None):
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--structure", required=True, help="structure id, e.g. minecraft:igloo")
    ap.add_argument("--seed", type=int, required=True)
    ap.add_argument("--dimension", default="overworld",
                    help="overworld|nether|end (default overworld)")
    ap.add_argument("--root", required=True,
                    help="captures root: the capture, its dump and manifest.json live here")
    ap.add_argument("--tag", help="capture directory name under --root (default from the chunk)")
    ap.add_argument("--chunk", nargs=2, type=int, metavar=("CX", "CZ"),
                    help="explicit chunk; it is checked against the probe before use")
    ap.add_argument("--scan-origin", nargs=2, type=int, default=[0, 0], metavar=("CX", "CZ"),
                    help="centre of the candidate scan in chunk coordinates (default 0 0)")
    ap.add_argument("--scan-radius", type=int, default=96,
                    help="scan this many chunks either side of --scan-origin (default 96)")
    ap.add_argument("--n", type=int, default=1, help="chunks per side in the window (default 1)")
    ap.add_argument("--height-window", choices=("noise", "dimension"),
                    help="y window for the capture and the triage (default: the dimension's "
                         "recorded choice: noise for overworld, dimension for nether/end)")
    ap.add_argument("--capture-dir",
                    help="reuse an existing capture instead of running native_fixture.sh")
    ap.add_argument("--jar-base", default=os.environ.get("CARVER_FIXTURE_BASE", ""),
                    help="installed 1.21.1 tree (server.jar+libraries+versions); "
                         "default $CARVER_FIXTURE_BASE")
    ap.add_argument("--binary", help="mc-spine binary (default target/debug/mc-spine)")
    ap.add_argument("--mem-max", default="3G",
                    help="MemoryMax for the native capture scope (default 3G; '' disables)")
    ap.add_argument("--max-attempts", type=int, default=4,
                    help="how many admitted candidates to capture before giving up (default 4)")
    ap.add_argument("--no-biome-gate", action="store_true",
                    help="do not order candidates by the port's biome hint")
    ap.add_argument("--use-pop-order", action="store_true",
                    help="pass the capture's order.tsv to port_triage as --pop-order")
    ap.add_argument("--world-b",
                    help="a second independent native run of the same window: port_triage "
                         "then reports native A vs B as the run-to-run noise floor")
    ap.add_argument("--row-id", help="manifest row id (default derived from the window)")
    ap.add_argument("--manifest",
                    help="append/replace the row in this manifest instead of <root>/manifest.json; "
                         "every path in the row is written relative to its directory (this is how "
                         "a measured row is promoted into captures/matrix-v2/manifest-promotable.json)")
    ap.add_argument("--dry-run", action="store_true",
                    help="report the admitted candidates and stop before any capture")
    ap.add_argument("--json-out", help="write the recipe's own report here")
    args = ap.parse_args(argv)

    if args.dimension not in DIMENSIONS:
        raise RecipeError(f"--dimension must be one of {sorted(DIMENSIONS)}")
    if args.n != 1:
        raise RecipeError(
            "this recipe captures a 1x1 window: a 3x3 village window needs its own boundary "
            "declaration, and matrix-v1's village row was captured that way by hand")
    dim = DIMENSIONS[args.dimension]
    height_window = args.height_window or ("noise" if args.dimension == "overworld" else "dimension")
    preset = dim["preset"]
    binary = resolve_binary(args)

    sets = structure_sets_for(args.structure)
    if not sets:
        raise RecipeError(
            f"no structure set in datapacks/*/worldgen/structure_set lists {args.structure}; "
            "the set is the only thing that can place a start"
        )
    allowed_biomes, structure_doc = structure_biomes(args.structure)
    log(f"structure {args.structure} (type {structure_doc.get('type')}, "
        f"biomes {len(allowed_biomes)} ids) in sets {sorted(sets)}")

    report = {
        "structure": args.structure,
        "seed": args.seed,
        "dimension": args.dimension,
        "height_window": height_window,
        "binary": str(binary),
        "binary_sha256": sha256_file(binary),
        "sets": {name: {"spacing": info["placement"].get("spacing"),
                        "separation": info["placement"].get("separation"),
                        "salt": info["placement"].get("salt"),
                        "members": info["members"]}
                 for name, info in sets.items()},
        "allowed_biomes": sorted(allowed_biomes),
    }

    # ---- stage 1: admission
    if args.chunk:
        verdict = verify_chunk_is_candidate(binary, args.seed, args.structure, sets,
                                            args.chunk[0], args.chunk[1])
        report["admission"] = {"mode": "explicit", "chunk": list(args.chunk), "verdict": verdict}
        if not any(entry["is_own_candidate"] for entry in verdict):
            raise RecipeError(
                f"chunk {tuple(args.chunk)} is not its own cell candidate for any set of "
                f"{args.structure}: {json.dumps(verdict, default=str)}"
            )
        candidates = [tuple(args.chunk)]
    else:
        admitted = admitted_candidates(binary, args.seed, args.structure, sets,
                                       tuple(args.scan_origin), args.scan_radius)
        admitted = [c for c in admitted if c["frequency"] and not c["exclusion"]]
        candidates = [tuple(c["chunk"]) for c in admitted]
        report["admission"] = {
            "mode": "scan",
            "origin": list(args.scan_origin),
            "radius": args.scan_radius,
            "admitted": [{**c, "chunk": list(c["chunk"])} for c in admitted],
        }
        if not candidates:
            raise RecipeError(
                f"no admitted candidate for {args.structure} (seed {args.seed}) within "
                f"{args.scan_radius} chunks of {tuple(args.scan_origin)}: increase --scan-radius "
                "or capture a different seed"
            )
    log(f"admitted candidates: {len(candidates)} (first: {candidates[:6]})")

    if not args.no_biome_gate and not args.capture_dir:
        hinted = biome_hint(binary, args.seed, preset, candidates, allowed_biomes)
        report["biome_hint"] = [{**h, "chunk": list(h["chunk"])} for h in hinted]
        ordered = [h["chunk"] for h in hinted if h["passes"]]
        ordered += [h["chunk"] for h in hinted if not h["passes"]]
        if not any(h["passes"] for h in hinted):
            log("biome hint: no candidate's container biome is in the structure's biome set; "
                "capturing in admission order and letting the world decide")
        candidates = ordered

    if args.dry_run:
        if args.json_out:
            Path(args.json_out).parent.mkdir(parents=True, exist_ok=True)
            Path(args.json_out).write_text(json.dumps(report, indent=2, default=str) + "\n")
        log(json.dumps(report, indent=2, default=str))
        return 0

    # ---- stage 2-3: capture, then verify the start from the capture itself
    root = Path(args.root).resolve()
    root.mkdir(parents=True, exist_ok=True)
    capture = None
    attempts = []
    if args.capture_dir:
        if not args.chunk:
            raise RecipeError(
                "--capture-dir needs --chunk: boundary.json records the window's size but "
                "not its origin")
        capture = Path(args.capture_dir).resolve()
        boundary = read_boundary(capture)
        starts = native_start(root_of_dimension(capture, boundary), args.structure,
                              tuple(args.chunk), args.n, preset)
        attempts.append({"capture": str(capture), "reused": True,
                         "chunk": list(args.chunk), "starts": starts})
        report["capture"] = {"reused": str(capture)}
        if not starts:
            raise RecipeError(
                f"{capture} carries no start for {args.structure}: the reused capture does not "
                "measure this structure"
            )
    else:
        if not args.jar_base:
            raise RecipeError("--jar-base (or $CARVER_FIXTURE_BASE) is required to capture")
        check_jar_base(args.jar_base)
        for index, chunk in enumerate(candidates[: args.max_attempts]):
            tag = args.tag or (
                f"nat-{args.seed}-{args.structure.split(':')[1]}-{chunk[0]}-{chunk[1]}")
            if index:
                tag = f"{tag}-try{index + 1}"
            target = root / tag
            if target.exists():
                raise RecipeError(
                    f"refusing to overwrite capture {target}; a capture is evidence, move it aside"
                )
            log(f"capture {target} (chunk {chunk}, {args.dimension}/{height_window}, seed {args.seed})")
            proc = run_capture(target, chunk, args.n, args.dimension, args.seed,
                               height_window, args.jar_base, args.mem_max)
            record = {
                "capture": str(target),
                "chunk": list(chunk),
                "exit": proc.returncode,
                "stdout_tail": "\n".join(proc.stdout.splitlines()[-12:]),
                "stderr_tail": "\n".join(proc.stderr.splitlines()[-12:]),
            }
            if proc.returncode != 0 or not (target / "boundary.json").is_file():
                (target / "refuted.txt").write_text(
                    f"capture failed (exit {proc.returncode}) for {args.structure} at {chunk}\n")
                attempts.append(record)
                log(f"capture FAILED (exit {proc.returncode}); see {target / 'run.log'}")
                continue
            boundary = read_boundary(target)
            world = target / boundary["world_subdirectory"]
            starts = native_start(world, args.structure, tuple(chunk), args.n, preset)
            record["starts"] = starts
            attempts.append(record)
            if not starts:
                (target / "refuted.txt").write_text(
                    f"no start for {args.structure} in chunk {chunk} "
                    f"({boundary['dimension']}, seed {args.seed}); refuted by the capture's own "
                    "structures.starts\n")
                log(f"REFUTED: {target} has no {args.structure} start")
                continue
            capture = target
            report["capture"] = {"dir": str(capture), "chunk": list(chunk), "starts": starts}
            log(f"verified start: {json.dumps(starts[:4])}")
            break
        report["attempts"] = attempts
        if capture is None:
            log(json.dumps(report, indent=2, default=str))
            raise RecipeError(
                f"every candidate was refuted for {args.structure}; the failed captures are kept "
                "with refuted.txt. No comparison was made."
            )

    boundary = read_boundary(capture)
    if boundary["stage"] != "structures":
        raise RecipeError(f"{capture} was captured at stage {boundary['stage']!r}, not 'structures'")
    if boundary["seed"] != args.seed:
        raise RecipeError(f"{capture} recorded seed {boundary['seed']}, not {args.seed}")
    if boundary["min_y"] != dim["window"][height_window][0] or \
            boundary["height"] != dim["window"][height_window][1]:
        raise RecipeError(
            f"{capture} recorded window {boundary['min_y']}/{boundary['height']} but "
            f"{args.dimension}/{height_window} is {dim['window'][height_window]}"
        )
    if boundary["mode"] != "frozen":
        raise RecipeError(f"{capture} was captured in mode {boundary['mode']!r}, not 'frozen'")

    # ---- stage 4: port dump + triage
    origin = report["capture"].get("chunk") or (args.chunk and list(args.chunk))
    if origin is None:
        raise RecipeError("the capture's origin is unknown; pass --chunk")
    origin = [int(origin[0]), int(origin[1])]
    dump_dir = root / "dumps" / capture.name
    dump_dir.mkdir(parents=True, exist_ok=True)
    triage_cmd = [
        sys.executable, str(TOOLS / "port_triage.py"), str(capture / "world"),
        str(origin[0]), str(origin[1]), "--n", str(args.n),
        "--preset", preset, "--height-window", height_window,
        "--out-dir", str(dump_dir), "--binary", str(binary),
        "--json-out", str(dump_dir / "triage.json"),
    ]
    order = capture / "order.tsv"
    if args.use_pop_order:
        if not order.is_file():
            raise RecipeError(f"--use-pop-order but {order} does not exist")
        triage_cmd += ["--pop-order", str(order)]
    if args.world_b:
        triage_cmd += ["--world-b", str(Path(args.world_b).resolve())]
    log("port_triage: " + " ".join(triage_cmd))
    proc = subprocess.run(triage_cmd, cwd=ROOT, capture_output=True, text=True, check=False)
    (dump_dir / "port_triage.log").write_text(proc.stdout + proc.stderr)
    report["port_triage"] = {
        "cmd": triage_cmd, "exit": proc.returncode,
        "tail": "\n".join((proc.stdout + proc.stderr).splitlines()[-20:]),
    }
    if proc.returncode not in (0, 1):
        log(json.dumps(report, indent=2, default=str))
        raise RecipeError(f"port_triage failed (exit {proc.returncode}); see {dump_dir}")
    triage = json.loads((dump_dir / "triage.json").read_text())
    diffs = triage["totals"]["all_air_diffs"]
    # The report counts chunks or (for 1x1) names the chunk; every compared chunk carries the
    # declared rectangle 16 x 16 x height, which is the "0 / 98,304" denominator.
    per_chunk = 256 * int(boundary["height"])
    if "grid" in triage:
        chunks_compared = triage["compared_chunks"]
        skipped = len(triage.get("skipped") or [])
    else:
        chunks_compared = 1
        skipped = 0
    compared = chunks_compared * per_chunk
    report["diff"] = {
        "chunks_compared": chunks_compared,
        "chunks_skipped": skipped,
        "cells": compared,
        "all_air_diffs": diffs,
        "exit": proc.returncode,
        "port_dump_sha256": (triage.get("port_meta") or {}).get("sha256"),
        "native_non_air": (triage.get("native_a_meta") or {}).get("non_air"),
        "port_non_air": (triage.get("port_meta") or {}).get("non_air"),
        "heightmap_diff_cols": (triage.get("heightmaps") or {}).get("port_vs_native_saved_diff_cols"),
    }
    log(f"port vs native: {diffs} differing cell(s) / {compared}")

    # ---- stage 5: manifest row
    manifest_path = Path(args.manifest) if args.manifest else root / "manifest.json"
    manifest_path.parent.mkdir(parents=True, exist_ok=True)
    row = build_row(args.row_id, capture, boundary, args.structure, dump_dir / "dump.bin",
                    origin, args.n, height_window, manifest_path.parent,
                    boundary["runtime_writes"] > 0)
    manifest = {"version": 1, "rows": []}
    if manifest_path.is_file():
        manifest = json.loads(manifest_path.read_text())
    manifest["rows"] = [existing for existing in manifest.get("rows") or []
                        if existing.get("id") != row["id"]]
    manifest["rows"].append(row)
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    report["row"] = row
    report["manifest"] = str(manifest_path)
    log(f"row {row['id']} written to {manifest_path}")

    if args.json_out:
        Path(args.json_out).parent.mkdir(parents=True, exist_ok=True)
        Path(args.json_out).write_text(json.dumps(report, indent=2, default=str) + "\n")
    return 1 if diffs else 0


def root_of_dimension(capture, boundary):
    """The dimension directory a capture's window lives in."""
    return capture / boundary["world_subdirectory"]


if __name__ == "__main__":
    try:
        sys.exit(main())
    except RecipeError as exc:
        print(f"structure_capture: ERROR: {exc}", file=sys.stderr)
        sys.exit(2)
