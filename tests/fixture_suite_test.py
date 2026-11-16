#!/usr/bin/env python3
"""Contracts for tools/fixture_suite.py: manifest acceptance, child contract, and inventory.

What these cases pin, in the order the suite decides it:

* every declared row appears in the result - a failing row never removes the others;
* a child that exits 0 but reports zero/partial comparisons, a truncated JSON, or a
  diff count contradicting its exit code is INVALID, not MATCH;
* stale sha256 bindings, an unmatched generation stage, a seed/grid/dimension that the
  fixture does not record, a non-positive `n`, a missing binding and a duplicate id are
  all invalid input (exit 2), and the remaining rows still run and are inventoried;
* a `world` outside the fixture is invalid even when it is a byte-identical copy with every
  sha256 re-bound (the provenance describes the fixture's own world), and an unhashable or
  wrongly typed field in `boundary.json`/the row is INVALID instead of a `TypeError`;
* an unbound writer trace stays `UNKNOWN` and nothing is inferred from block names;
* the suite resolves manifest paths from the manifest's own directory, and refuses an
  output directory that overlaps an input or an earlier run;
* the *real* comparators are exercised as child processes with the suite's argument
  contract: `chunk_triage.py` produces a MATCH and then a DIFF for one edited cell, and
  `sculk_gate.py` catches a same-block-name/different-property cell.

Fixtures are synthetic and local (no capture, no network): the native chunk dict comes
from `tests/chunk_triage_test.make_chunk_nbt`, the region layout from
`tests/nbt_region_test.write_region`, the level.dat from `tests/port_triage_test`, and the
state ids from the shipped `assets/blocks_1211.json`.
"""
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
sys.path.insert(0, str(ROOT / "tests"))
from port_triage_test import make_level_dat  # noqa: E402
from sculk_gate_test import (  # noqa: E402
    all_props, block_state, cell_offset, chunk_cells, chunk_payload, make_dump, write_region_many,
)

_spec = importlib.util.spec_from_file_location("fixture_suite", ROOT / "tools/fixture_suite.py")
fs = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(fs)

FAKE_TOOL = '''#!/usr/bin/env python3
"""Test-only stand-in for a comparator: behaviour comes from FAKE_CHILD_SPEC."""
import json
import os
import sys
from pathlib import Path

out = Path(sys.argv[sys.argv.index("--json-out") + 1])
spec = json.loads(os.environ["FAKE_CHILD_SPEC"])
behaviour = spec.get(out.stem, spec.get("*", {}))
sys.stderr.write(behaviour.get("stderr", ""))
if behaviour.get("json") is not None:
    value = behaviour["json"]
    out.write_text(value if isinstance(value, str) else json.dumps(value) + "\\n")
print(behaviour.get("stdout", ""), end="")
sys.exit(behaviour.get("exit", 0))
'''


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def rel(root, path):
    return str(Path(path).resolve().relative_to(Path(root).resolve()))


def build_fixture(root, *, n=1, origin=(0, 0), height=32, seed=4242, stage="features",
                  dimension="minecraft:overworld", boundary_overrides=None,
                  cells_by_chunk=None, statuses=None):
    """A local capture directory: world/region + level.dat + boundary.json + port.bin."""
    root = Path(root)
    fixture = root / "capture"
    world = fixture / "world"
    cells_by_chunk = cells_by_chunk or {}
    statuses = statuses or {}
    payloads = {}
    for dz in range(n):
        for dx in range(n):
            cx, cz = origin[0] + dx, origin[1] + dz
            payloads[(cx, cz)] = chunk_payload(
                cells_by_chunk.get((cx, cz), {}), cx, cz, height=height,
                status=statuses.get((cx, cz), "minecraft:full"))
    write_region_many(world, payloads)
    (world / "level.dat").write_bytes(make_level_dat(seed=seed))
    boundary = {
        "mode": "frozen", "dimension": dimension, "min_y": -64, "height": height,
        "seed": seed, "stage": stage, "world_subdirectory": "world",
        "game_times": [2, 2], "chunks": n * n, "decorated_chunks": n * n, "runtime_writes": 0,
    }
    boundary.update(boundary_overrides or {})
    (fixture / "boundary.json").write_text(json.dumps(boundary, indent=2) + "\n")
    (fixture / "port.bin").write_bytes(bytes(n * n * (height // 16) * 4096 * 4))
    return fixture


def window_dump(path, origin, n, height, cells):
    """cells: {(x, y, z): canonical_state_id} across the window; air elsewhere."""
    by_chunk = {}
    for (x, y, z), state in cells.items():
        cx, cz = x >> 4, z >> 4
        by_chunk.setdefault((cx - origin[0], cz - origin[1]), {})[cell_offset(cx, cz, x, y, z)] = state
    return make_dump(path, n, by_chunk, cells=(height // 16) * 4096)


def window_regions(world, origin, n):
    regions = set()
    for dz in range(n):
        for dx in range(n):
            cx, cz = origin[0] + dx, origin[1] + dz
            regions.add(Path(world) / "region" / f"r.{cx >> 5}.{cz >> 5}.mca")
    return sorted(regions)


def make_row(root, fixture, *, rid="row-1", kind="chunk", n=1, origin=(0, 0), height=32,
             seed=4242, stage=("features", "features"), preset=None, bind=True,
             writer_trace=None):
    fixture = Path(fixture)
    world = fixture / "world"
    row = {
        "id": rid, "kind": kind,
        "fixture": rel(root, fixture), "world": rel(root, world), "port": rel(root, fixture / "port.bin"),
        "origin": [origin[0], origin[1]], "n": n, "grid": {"min_y": -64, "height": height},
        "seed": seed, "stage": {"reference": stage[0], "port": stage[1]},
    }
    if preset is not None:
        row["preset"] = preset
    if bind:
        paths = [world / "level.dat", fixture / "port.bin", *window_regions(world, origin, n)]
        inputs = {"sha256": {rel(root, path): sha256(path) for path in paths}}
        if writer_trace is not None:
            trace_path = Path(writer_trace["path"])
            entry = {"path": rel(root, trace_path), "sha256": sha256(trace_path)}
            if "attribution" in writer_trace:
                entry["attribution"] = writer_trace["attribution"]
            inputs["writer_trace"] = entry
        row["inputs"] = inputs
    return row


def write_manifest(root, rows, *, version=1):
    path = Path(root) / "manifest.json"
    path.write_text(json.dumps({"version": version, "rows": rows}, indent=2) + "\n")
    return path


def run_suite(argv, *, fake=None, cwd=None):
    """Call the suite's entry point; `fake` becomes the child behaviour spec."""
    previous = os.environ.get("FAKE_CHILD_SPEC")
    if fake is not None:
        os.environ["FAKE_CHILD_SPEC"] = json.dumps(fake)
    stdout, stderr = io.StringIO(), io.StringIO()
    old_cwd = os.getcwd()
    try:
        if cwd is not None:
            os.chdir(cwd)
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            code = fs.main([str(arg) for arg in argv])
    finally:
        os.chdir(old_cwd)
        if previous is None:
            os.environ.pop("FAKE_CHILD_SPEC", None)
        else:
            os.environ["FAKE_CHILD_SPEC"] = previous
    return code, stdout.getvalue() + stderr.getvalue()


def chunk_json(diffs=0, compared=1, cells=8192, skipped=None):
    return {"grid": {"origin": [0, 0], "n": 1}, "min_y": -64, "height": 32,
            "compared_chunks": compared, "skipped": skipped or [],
            "totals": {"all_air_diffs": diffs, "total_cells": cells}}


class FixtureSuiteContracts(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.root = Path(tempfile.mkdtemp(prefix="fixture-suite-test-"))
        cls.fake_tool = cls.root / "fake_tool.py"
        cls.fake_tool.write_text(FAKE_TOOL)
        # A real, local fixture: one overworld chunk, 2 sections, one stone cell.
        cls.fixture = build_fixture(cls.root / "fixtures/plain", n=1, height=32,
                                    cells_by_chunk={(0, 0): {(0, -60, 0): ("minecraft:stone", {})}})
        window_dump(cls.fixture / "port.bin", (0, 0), 1, 32,
                    {(0, -60, 0): block_state("stone")})

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.root, ignore_errors=True)

    def out(self, name):
        path = self.root / "out" / name
        shutil.rmtree(path, ignore_errors=True)
        return path

    def fake_argv(self, manifest, out):
        return [manifest, "--out", out, "--chunk-tool", self.fake_tool, "--sculk-tool", self.fake_tool]

    def test_two_rows_are_both_counted_one_diff_one_match_is_exit_1(self):
        manifest = write_manifest(self.root, [
            make_row(self.root, self.fixture, rid="row-match"),
            make_row(self.root, self.fixture, rid="row-diff"),
        ])
        out = self.out("two-rows")
        code, text = run_suite(self.fake_argv(manifest, out), fake={
            "row-match": {"json": chunk_json(diffs=0)},
            "row-diff": {"json": chunk_json(diffs=3), "exit": 1, "stdout": "3 differences\n"},
        })
        self.assertEqual(code, 1, text)
        result = json.loads((out / "result.json").read_text())
        self.assertEqual([row["status"] for row in result["rows"]], ["MATCH", "DIFF"])
        self.assertEqual(result["counts"], {"MATCH": 1, "DIFF": 1, "INVALID": 0, "NOT_RUN": 0})
        self.assertEqual(result["exit_code"], 1)
        diff_row = result["rows"][1]
        self.assertEqual(diff_row["comparison"]["diffs"], 3)   # the child's own count
        self.assertEqual(diff_row["command"]["exit"], 1)
        self.assertEqual(Path(diff_row["command"]["stdout"]).read_text(), "3 differences\n")
        summary = (out / "summary.md").read_text()
        self.assertIn("row-match", summary)
        self.assertIn("row-diff", summary)
        self.assertIn("no automatic fixture discovery", summary)

    def test_child_contract_violations_are_invalid_not_match(self):
        cases = {
            "child crash": ({"exit": 3}, "crashed with exit 3"),
            "child invalid input": ({"exit": 2, "stderr": "missing dump"}, "invalid input (exit 2)"),
            "truncated child json": ({"json": "{\"totals\": {", "exit": 0}, "missing or truncated"),
            "exit 0 with diffs": ({"json": chunk_json(diffs=5), "exit": 0}, "exited 0"),
            "zero comparisons": ({"json": chunk_json(diffs=0, compared=0, cells=0), "exit": 0},
                                 "compared 0/1"),
            "partial grid": ({"json": chunk_json(diffs=0, compared=0,
                                                 skipped=[{"chunk": [1, 0]}])}, "skipped 1 chunk"),
            "exit 1 without diffs": ({"json": chunk_json(diffs=0), "exit": 1}, "no differences"),
            "no json written": ({"exit": 0}, "missing or truncated"),
            "empty json file": ({"json": "", "exit": 0}, "missing or truncated"),
        }
        for label, (behaviour, reason) in cases.items():
            with self.subTest(label=label):
                manifest = write_manifest(self.root, [make_row(self.root, self.fixture, rid="row-1")])
                out = self.out("child-" + label.replace(" ", "-"))
                code, text = run_suite(self.fake_argv(manifest, out), fake={"row-1": behaviour})
                self.assertEqual(code, 2, text)
                result = json.loads((out / "result.json").read_text())
                row = result["rows"][0]
                self.assertEqual(row["status"], "INVALID", label)
                self.assertIn(reason, row["reason"], label)
                # The child's own record survives the rejection: raw stdout/stderr stay linked.
                self.assertTrue(Path(row["command"]["stderr"]).is_file(), label)
                self.assertEqual(result["counts"]["MATCH"], 0, label)

    def test_a_sculk_row_needs_a_positive_reference(self):
        fixture = build_fixture(self.root / "fixtures/sculk", n=1, height=384,
                                cells_by_chunk={(0, 0): chunk_cells(0, 0)})
        row = make_row(self.root, fixture, rid="sculk-row", kind="sculk", height=384)
        manifest = write_manifest(self.root, [row])
        out = self.out("sculk-zero")
        code, text = run_suite(self.fake_argv(manifest, out), fake={"sculk-row": {"json": {
            "chunks_compared": 1, "chunks_expected": 1, "counts": {"diffs": 0},
            "reference": {"total": 0}, "port": {"total": 0}, "exit_code": 0}}})
        self.assertEqual(code, 2, text)
        result = json.loads((out / "result.json").read_text())
        self.assertEqual(result["rows"][0]["status"], "INVALID")
        self.assertIn("no sculk cells", result["rows"][0]["reason"])

    def test_bad_inputs_are_invalid_and_other_rows_still_run(self):
        good = make_row(self.root, self.fixture, rid="good")
        cases = {}
        expected = {
            "stale sha binding": "sha256 mismatch",
            "missing binding": "no sha256 binding",
            "stage mismatch": "reference stage",
            "fixture stage mismatch": "fixture stage",
            "seed mismatch": "declared seed",
            "non-positive n": "n must be >= 1",
            "grid mismatch": "declared grid",
        }
        stale = json.loads(json.dumps(good))
        stale["id"] = "stale"
        key = next(iter(stale["inputs"]["sha256"]))
        stale["inputs"]["sha256"][key] = "0" * 64
        cases["stale sha binding"] = stale

        unbind = json.loads(json.dumps(good))
        unbind["id"] = "unbind"
        unbind["inputs"]["sha256"].pop(next(iter(unbind["inputs"]["sha256"])))
        cases["missing binding"] = unbind

        stage = json.loads(json.dumps(good))
        stage["id"] = "stage"
        stage["stage"] = {"reference": "features", "port": "structures"}
        cases["stage mismatch"] = stage

        fixture_stage = json.loads(json.dumps(good))
        fixture_stage["id"] = "fixture-stage"
        fixture_stage["stage"] = {"reference": "structures", "port": "structures"}
        cases["fixture stage mismatch"] = fixture_stage

        seed = json.loads(json.dumps(good))
        seed["id"] = "seed"
        seed["seed"] = 7
        cases["seed mismatch"] = seed

        zero = json.loads(json.dumps(good))
        zero["id"] = "zero-n"
        zero["n"] = 0
        cases["non-positive n"] = zero

        grid = json.loads(json.dumps(good))
        grid["id"] = "grid"
        grid["grid"] = {"min_y": -64, "height": 16}
        cases["grid mismatch"] = grid

        for label, row in cases.items():
            with self.subTest(label=label):
                manifest = write_manifest(self.root, [row, good])
                out = self.out("bad-" + label.replace(" ", "-"))
                code, text = run_suite(self.fake_argv(manifest, out), fake={"*": {"json": chunk_json()}})
                self.assertEqual(code, 2, text)
                result = json.loads((out / "result.json").read_text())
                self.assertEqual(len(result["rows"]), 2, "a bad row must not drop the other rows")
                self.assertEqual(result["rows"][0]["status"], "INVALID", label)
                self.assertIn(expected[label], result["rows"][0]["reason"], label)
                self.assertEqual(result["rows"][1]["status"], "MATCH", label)

    def test_a_world_outside_the_fixture_is_invalid_even_when_rebound(self):
        """`boundary.json` describes one fixture; a copied world is not that world.

        The failure this pins: a byte-identical copy of `world/` under another path, with
        every sha256 re-bound to the copy, was accepted as MATCH because only the file
        hashes were checked - nothing tied `world`/`region_world` back to the fixture
        whose boundary.json records the stage and window.
        """
        fixture = self.fixture
        unrelated = self.root / "unrelated" / "world"
        shutil.rmtree(unrelated.parent, ignore_errors=True)
        shutil.copytree(fixture / "world", unrelated)
        row = make_row(self.root, fixture, rid="foreign-world")
        row["world"] = rel(self.root, unrelated)
        row["inputs"]["sha256"] = {rel(self.root, path): sha256(path) for path in
                                   (unrelated / "level.dat", unrelated / "region/r.0.0.mca",
                                    fixture / "port.bin")}
        manifest = write_manifest(self.root, [row])
        out = self.out("foreign-world")
        code, text = run_suite(self.fake_argv(manifest, out), fake={"*": {"json": chunk_json()}})
        self.assertEqual(code, 2, text)
        result = json.loads((out / "result.json").read_text())
        row_result = result["rows"][0]
        self.assertEqual(row_result["status"], "INVALID")
        self.assertIn("not the fixture's world", row_result["reason"])
        self.assertNotIn("command", row_result, "the comparator must not run on a foreign world")

    def test_unhashable_boundary_fields_are_invalid_not_a_typeerror(self):
        """A typed field in boundary.json is checked before any dict/set lookup.

        The failure this pins: `dimension: {}` reached `DIMENSION_PRESETS.get(dimension)`
        and raised `TypeError: unhashable type: 'dict'` out of the suite instead of
        inventorying the row as INVALID with exit 2. A boolean is not an integer here
        either: `runtime_writes: true` and `chunks: true` used to satisfy `!= 0`/`== n*n`.
        """
        cases = {
            "dimension-object": ("dimension", {}, "dimension"),
            "dimension-list": ("dimension", [], "dimension"),
            "dimension-bool": ("dimension", True, "dimension"),
            "dimension-number": ("dimension", 7, "dimension"),
            "mode-object": ("mode", {}, "mode"),
            "stage-list": ("stage", [], "stage"),
            "runtime-writes-bool": ("runtime_writes", True, "runtime_writes"),
            "chunks-bool": ("chunks", True, "chunks"),
            "seed-bool": ("seed", True, "seed"),
            "min-y-string": ("min_y", "-64", "min_y"),
            "height-float": ("height", 32.0, "height"),
        }
        for label, (field, value, reason) in cases.items():
            with self.subTest(case=label):
                fixture = build_fixture(self.root / f"fixtures/boundary/{label}", n=1, height=32,
                                        cells_by_chunk={(0, 0): {(0, -60, 0): ("minecraft:stone", {})}})
                window_dump(fixture / "port.bin", (0, 0), 1, 32,
                            {(0, -60, 0): block_state("stone")})
                boundary = json.loads((fixture / "boundary.json").read_text())
                boundary[field] = value
                (fixture / "boundary.json").write_text(json.dumps(boundary))
                manifest = write_manifest(self.root, [make_row(self.root, fixture, rid="bad-boundary")])
                out = self.out("boundary-" + label)
                code, text = run_suite(self.fake_argv(manifest, out),
                                       fake={"*": {"json": chunk_json()}})
                self.assertEqual(code, 2, text)
                self.assertNotIn("Traceback", text)
                row_result = json.loads((out / "result.json").read_text())["rows"][0]
                self.assertEqual(row_result["status"], "INVALID", label)
                self.assertIn(reason, row_result["reason"], label)

    def test_unhashable_row_fields_are_invalid_not_a_typeerror(self):
        """The row's own typed fields are rejected before `in`/`get`/compare."""
        cases = {
            "kind-object": ({"kind": {}}, "kind"),
            "preset-object": ({"preset": {}}, "preset"),
            "dim-list": ({"dim": []}, "dim"),
            "stage-reference-object": ({"stage": {"reference": {}, "port": {}}}, "stage.reference"),
            "stage-port-list": ({"stage": {"reference": "features", "port": []}}, "stage.port"),
            "grid-height-bool": ({"grid": {"min_y": -64, "height": True}}, "grid.height"),
            "seed-bool": ({"seed": True}, "seed"),
        }
        for label, (mutation, reason) in cases.items():
            with self.subTest(case=label):
                row = make_row(self.root, self.fixture, rid="bad-row")
                row.update(mutation)
                manifest = write_manifest(self.root, [row])
                out = self.out("row-" + label)
                code, text = run_suite(self.fake_argv(manifest, out),
                                       fake={"*": {"json": chunk_json()}})
                self.assertEqual(code, 2, text)
                self.assertNotIn("Traceback", text)
                row_result = json.loads((out / "result.json").read_text())["rows"][0]
                self.assertEqual(row_result["status"], "INVALID", label)
                self.assertIn(reason, row_result["reason"], label)

    def test_duplicate_ids_and_truncated_manifests_are_invalid(self):
        row = make_row(self.root, self.fixture, rid="dup")
        manifest = write_manifest(self.root, [row, json.loads(json.dumps(row))])
        out = self.out("duplicate")
        code, text = run_suite(self.fake_argv(manifest, out), fake={"*": {"json": chunk_json()}})
        self.assertEqual(code, 2, text)
        result = json.loads((out / "result.json").read_text())
        self.assertEqual([r["status"] for r in result["rows"]], ["INVALID", "INVALID"])
        self.assertIn("duplicate row id", result["rows"][0]["reason"])

        broken = self.root / "truncated.json"
        broken.write_text('{"version": 1, "rows": [{"id": "x"')
        out = self.out("truncated")
        code, text = run_suite([broken, "--out", out, "--chunk-tool", self.fake_tool,
                                "--sculk-tool", self.fake_tool])
        self.assertEqual(code, 2, text)
        self.assertIn("not valid JSON", text)
        self.assertFalse((out / "result.json").exists())

    def test_unknown_writer_stays_unknown_and_a_declared_trace_is_passed_through(self):
        manifest = write_manifest(self.root, [make_row(self.root, self.fixture, rid="no-trace")])
        out = self.out("writer-unknown")
        code, text = run_suite(self.fake_argv(manifest, out), fake={"*": {"json": chunk_json()}})
        self.assertEqual(code, 0, text)
        row = json.loads((out / "result.json").read_text())["rows"][0]
        self.assertEqual(row["writer"]["status"], "UNKNOWN")
        self.assertIn("no writer trace", row["writer"]["reason"])
        self.assertNotIn("LIGHT", json.dumps(row))

        trace = self.root / "writer.trace"
        trace.write_text("SPATCH 0 -60 0\n")
        manifest = write_manifest(self.root, [make_row(
            self.root, self.fixture, rid="with-trace",
            writer_trace={"path": trace, "attribution": "sculk_patchFeature (declared)"})])
        out = self.out("writer-declared")
        code, text = run_suite(self.fake_argv(manifest, out), fake={"*": {"json": chunk_json()}})
        self.assertEqual(code, 0, text)
        row = json.loads((out / "result.json").read_text())["rows"][0]
        self.assertEqual(row["writer"]["status"], "DECLARED")
        self.assertEqual(row["writer"]["attribution"], "sculk_patchFeature (declared)")
        self.assertEqual(row["writer"]["trace"]["sha256"], sha256(trace))

    def test_manifest_paths_resolve_from_the_manifest_directory(self):
        elsewhere = self.root / "elsewhere"
        elsewhere.mkdir(exist_ok=True)
        manifest = write_manifest(self.root, [make_row(self.root, self.fixture, rid="cwd")])
        out = self.out("cwd")
        code, text = run_suite(self.fake_argv(manifest, out), fake={"*": {"json": chunk_json()}},
                               cwd=elsewhere)
        self.assertEqual(code, 0, text)
        self.assertEqual(json.loads((out / "result.json").read_text())["rows"][0]["status"], "MATCH")

    def test_output_directory_cannot_overlap_inputs_or_a_previous_run(self):
        manifest = write_manifest(self.root, [make_row(self.root, self.fixture, rid="safe")])
        code, text = run_suite(self.fake_argv(manifest, self.fixture / "world"),
                               fake={"*": {"json": chunk_json()}})
        self.assertEqual(code, 2, text)
        self.assertIn("output dir", text)
        self.assertFalse((self.fixture / "world" / "result.json").exists())

        occupied = self.out("occupied")
        occupied.mkdir(parents=True)
        (occupied / "keep.txt").write_text("earlier run\n")
        code, text = run_suite(self.fake_argv(manifest, occupied), fake={"*": {"json": chunk_json()}})
        self.assertEqual(code, 2, text)
        self.assertEqual((occupied / "keep.txt").read_text(), "earlier run\n")

    def test_real_chunk_triage_child_is_a_match_then_a_diff(self):
        fixture = build_fixture(self.root / "fixtures/real", n=1, height=32,
                                cells_by_chunk={(0, 0): {(0, -60, 0): ("minecraft:stone", {})}})
        dump = fixture / "port.bin"
        window_dump(dump, (0, 0), 1, 32, {(0, -60, 0): block_state("stone")})
        manifest = write_manifest(self.root, [make_row(self.root, fixture, rid="real-chunk")])
        out = self.out("real-match")
        code, text = run_suite([manifest, "--out", out])
        self.assertEqual(code, 0, text)
        result = json.loads((out / "result.json").read_text())
        row = result["rows"][0]
        self.assertEqual(row["status"], "MATCH", row.get("reason"))
        self.assertEqual(row["command"]["exit"], 0)
        self.assertEqual(row["comparison"]["compared_chunks"], 1)
        self.assertEqual(row["comparison"]["scoped_cells"], 8192)
        self.assertEqual(row["comparison"]["diffs"], 0)

        window_dump(dump, (0, 0), 1, 32, {(0, -60, 0): block_state("granite")})
        manifest = write_manifest(self.root, [make_row(self.root, fixture, rid="real-chunk")])
        out = self.out("real-diff")
        code, text = run_suite([manifest, "--out", out])
        self.assertEqual(code, 1, text)
        result = json.loads((out / "result.json").read_text())
        row = result["rows"][0]
        self.assertEqual(row["status"], "DIFF")
        self.assertEqual(row["comparison"]["diffs"], 1)
        self.assertEqual(row["command"]["exit"], 1)

    def test_real_sculk_gate_catches_a_same_name_property_difference(self):
        props = all_props("sculk_vein", waterlogged="false")
        cells = chunk_cells(0, 0, name="minecraft:sculk_vein", props=props)
        fixture = build_fixture(self.root / "fixtures/real-sculk", n=1, height=384,
                                cells_by_chunk={(0, 0): cells})
        dump = fixture / "port.bin"
        matching = {(x, y, z): block_state("sculk_vein", props) for (x, y, z) in cells}
        window_dump(dump, (0, 0), 1, 384, matching)
        manifest = write_manifest(self.root, [make_row(self.root, fixture, rid="real-sculk", kind="sculk", height=384)])
        out = self.out("real-sculk-match")
        code, text = run_suite([manifest, "--out", out])
        self.assertEqual(code, 0, text)
        row = json.loads((out / "result.json").read_text())["rows"][0]
        self.assertEqual(row["status"], "MATCH", row.get("reason"))
        self.assertEqual(row["comparison"]["positive_reference_cells"], 4)

        wrong = block_state("sculk_vein", all_props("sculk_vein", waterlogged="true"))
        changed = dict(matching)
        changed[(0, -60, 0)] = wrong
        window_dump(dump, (0, 0), 1, 384, changed)
        manifest = write_manifest(self.root, [make_row(self.root, fixture, rid="real-sculk", kind="sculk", height=384)])
        out = self.out("real-sculk-diff")
        code, text = run_suite([manifest, "--out", out])
        self.assertEqual(code, 1, text)
        row = json.loads((out / "result.json").read_text())["rows"][0]
        self.assertEqual(row["status"], "DIFF")
        self.assertEqual(row["comparison"]["diffs"], 1)
        self.assertEqual(row["comparison"]["counts"]["wrong_state"], 1)


if __name__ == "__main__":
    unittest.main()
