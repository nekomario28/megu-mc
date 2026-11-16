#!/usr/bin/env python3
"""Contracts for tools/sculk_gate.py: canonical state ids, full grid, and dump validation.

The control that matters is a false green the previous body produced: it compared block
*names* (`gn != wn`), so two `sculk_vein` cells with different `waterlogged`/direction
properties were reported as agreement, a chunk that was not `full` was silently skipped,
and a dump of only a few sections was accepted as long as its length divided evenly.
A missing world (or a world without `region/r.*.mca`) escaped as a `FileNotFoundError`
traceback instead of invalid input (exit 2).
`SCULK_GATE_UNDER_TEST=<path>` runs these cases against another copy of the script, so the
pre-change body can be shown failing them.

Fixtures are built from the shipped state table: the native chunk dict comes from
`tests/chunk_triage_test.make_chunk_nbt`, the region file uses the same Anvil layout as
`tests/nbt_region_test.write_region`, and every encoded chunk round-trips through
`vanilla_region_blocks.parse_nbt` before an assertion uses it.
"""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import struct
import sys
import tempfile
import unittest
import zlib

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
sys.path.insert(0, str(ROOT / "tests"))
import carver_diff  # noqa: E402
from chunk_triage_test import make_chunk_nbt  # noqa: E402
from vanilla_region_blocks import parse_nbt as real_parse_nbt  # noqa: E402

BLOCKS = json.loads((ROOT / "assets" / "blocks_1211.json").read_text())
STATE_TABLE = carver_diff.load_states()
CELLS = 24 * 4096


class LongArray(list):
    """A list the fixture encoder writes as NBT TAG_Long_Array (real Anvil type)."""


def all_props(name, **overrides):
    """Every property of `name`, defaulted from the shipped table, then overridden."""
    block = next(b for b in BLOCKS if b["name"] == name)
    props = {}
    for prop in block["states"]:
        allowed = prop.get("values") or ["true", "false"]
        props[prop["name"]] = str(overrides.get(prop["name"], allowed[0])).lower()
    return props


def block_state(name, props=None):
    """Canonical state id for `name` with a complete property map."""
    block = next(b for b in BLOCKS if b["name"] == name)
    values = []
    for prop in block["states"]:
        allowed = prop.get("values") or ["true", "false"]
        values.append(str((props or {}).get(prop["name"], allowed[0])).lower())
    return STATE_TABLE[(name, tuple(values))]


def _nbt_str(text):
    raw = text.encode("utf-8")
    return struct.pack(">H", len(raw)) + raw


def _nbt_tagged(key, value):
    name = _nbt_str(key)
    if isinstance(value, bool):
        return b"\x01" + name + struct.pack(">b", 1 if value else 0)
    if isinstance(value, LongArray):
        return b"\x0c" + name + struct.pack(">i", len(value)) + b"".join(
            struct.pack(">q", item) for item in value)
    if isinstance(value, int):
        if not -(1 << 31) <= value < (1 << 31):
            raise ValueError(f"{key}={value} does not fit TAG_Int; wrap it in LongArray")
        return b"\x03" + name + struct.pack(">i", value)
    if isinstance(value, str):
        return b"\x08" + name + _nbt_str(value)
    if isinstance(value, dict):
        body = b"".join(_nbt_tagged(k, v) for k, v in value.items())
        return b"\x0a" + name + body + b"\x00"
    if isinstance(value, list):
        if not value:
            return b"\x09" + name + b"\x00" + struct.pack(">i", 0)
        first = _nbt_tagged("", value[0])[0]
        body = b"".join(_nbt_tagged("", item)[3:] for item in value)
        return b"\x09" + name + bytes([first]) + struct.pack(">i", len(value)) + body
    raise TypeError(f"cannot encode {type(value).__name__} for {key!r}")


def nbt_encode(root):
    """Encode an Anvil-shaped chunk dict (the shapes these fixtures use)."""
    return b"\x0a" + _nbt_str("") + b"".join(_nbt_tagged(k, v) for k, v in root.items()) + b"\x00"


def chunk_payload(cells, cx, cz, *, status="minecraft:full", height=384):
    """Compressed chunk payload for `cells` = {(x, y, z): (name, {properties})}."""
    nbt = make_chunk_nbt(cx, cz, status=status, height=height, blocks=cells)
    for section in nbt["sections"]:
        data = section["block_states"].get("data")
        if data is not None:
            section["block_states"]["data"] = LongArray(data)
    for key in list(nbt["Heightmaps"]):
        nbt["Heightmaps"][key] = LongArray(nbt["Heightmaps"][key])
    raw = nbt_encode(nbt)
    # Prove the fixture is readable by the same parser the tools use before asserting on it.
    parsed, _ = real_parse_nbt(raw)
    assert parsed["Status"] == status
    assert len(parsed["sections"]) == height // 16, "fixture is not Anvil shaped"
    return zlib.compress(raw)


def write_region_many(world, payloads, *, file_size=16 * 1024 * 1024):
    """Several chunks into one region file; same layout as nbt_region_test.write_region."""
    region_dir = Path(world) / "region"
    region_dir.mkdir(parents=True, exist_ok=True)
    groups = {}
    for (cx, cz), body in payloads.items():
        groups.setdefault((cx >> 5, cz >> 5), []).append(((cx, cz), body))
    for (rx, rz), entries in groups.items():
        mca = region_dir / f"r.{rx}.{rz}.mca"
        with mca.open("wb") as file:
            file.truncate(file_size)
            sector = 3
            for (cx, cz), body in entries:
                file.seek(((cx & 31) + (cz & 31) * 32) * 4)
                file.write(sector.to_bytes(3, "big") + bytes([1]))
                file.seek(sector * 4096)
                file.write(struct.pack(">I", len(body) + 1) + bytes([2]) + body)
                sector += 2
    return mca


def make_world(root, chunks, *, height=384, statuses=None):
    """chunks: {(cx, cz): {(x, y, z): (name, props)}} -> world dir with its region file.

    `statuses` overrides the chunk Status so a fixture can hold an unfinished chunk.
    """
    world = Path(root) / "world"
    statuses = statuses or {}
    payloads = {coord: chunk_payload(cells, *coord, height=height,
                                     status=statuses.get(coord, "minecraft:full"))
                for coord, cells in chunks.items()}
    write_region_many(world, payloads)
    return world


def chunk_cells(cx, cz, name="minecraft:sculk", props=None):
    """Four sculk cells at y=-60 inside chunk (cx, cz)."""
    return {(cx * 16 + x, -60, cz * 16 + z): (name, props or {})
            for x in (0, 1) for z in (0, 1)}


def cell_offset(cx, cz, x, y, z):
    assert cx * 16 <= x < cx * 16 + 16 and cz * 16 <= z < cz * 16 + 16, "cell outside chunk"
    return ((y + 64) >> 4) * 4096 + ((y + 64) & 15) * 256 + (z & 15) * 16 + (x & 15)


def make_dump(path, n, cells_by_chunk, *, cells=CELLS):
    """cells_by_chunk: {(dx, dz): {cell_offset: state_id}} -> n*n chunk-major u32 dump."""
    raw = bytearray(n * n * cells * 4)
    for (dx, dz), entries in cells_by_chunk.items():
        base = (dz * n + dx) * cells
        for offset, state in entries.items():
            raw[(base + offset) * 4:(base + offset) * 4 + 4] = struct.pack("<I", state)
    path = Path(path)
    path.write_bytes(bytes(raw))
    return path


def load_gate():
    path = Path(os.environ.get("SCULK_GATE_UNDER_TEST", str(ROOT / "tools/sculk_gate.py"))).resolve()
    spec = importlib.util.spec_from_file_location("sculk_gate_under_test", path)
    module = importlib.util.module_from_spec(spec)
    sys.path.insert(0, str(ROOT / "tools"))
    spec.loader.exec_module(module)
    return module


def run_gate(module, argv):
    out, err = io.StringIO(), io.StringIO()
    code = None
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        old_argv = sys.argv
        sys.argv = ["sculk_gate.py", *argv]
        try:
            code = module.main()
        except SystemExit as exc:  # the stand-alone error path (usage / bad input)
            code = exc.code if isinstance(exc.code, int) else 1
        except Exception as exc:  # a crash is not an exit code; surface it as one
            code = f"crash: {type(exc).__name__}: {exc}"
        finally:
            sys.argv = old_argv
    return code, out.getvalue(), err.getvalue()


class SculkGateContracts(unittest.TestCase):
    def setUp(self):
        self.gate = load_gate()

    def run_pair(self, chunks, dump_cells, *, n=1, origin=(0, 0), extra=()):
        with tempfile.TemporaryDirectory() as tmp:
            world = make_world(tmp, chunks)
            dump = make_dump(Path(tmp) / "port.bin", n, dump_cells)
            return run_gate(self.gate, [str(world), str(dump), str(n), str(origin[0]),
                                        str(origin[1]), *extra])

    def test_same_block_name_with_different_properties_is_a_difference(self):
        """The old `gn != wn` compared names: this exact pair used to be a false PASS."""
        vein = all_props("sculk_vein", waterlogged="false")
        native = block_state("sculk_vein", vein)
        port_wrong = block_state("sculk_vein", all_props("sculk_vein", waterlogged="true"))
        self.assertNotEqual(native, port_wrong)
        chunk = {(x, -60, z): ("minecraft:sculk_vein", vein) for x in (0, 1) for z in (0, 1)}
        code, out, err = self.run_pair(
            {(0, 0): chunk},
            {(0, 0): {cell_offset(0, 0, 0, -60, 0): port_wrong,
                      **{cell_offset(0, 0, x, -60, z): native
                         for x in (0, 1) for z in (0, 1) if (x, z) != (0, 0)}}},
            extra=("--require-match",),
        )
        self.assertEqual(code, 1, out + err)
        self.assertIn("wrong-state", out)
        self.assertIn("1 sculk cells disagree", out)

    def test_identical_sculk_cells_are_green_and_counted(self):
        state = block_state("sculk")
        chunk = {(x, -60, z): ("minecraft:sculk", {}) for x in (0, 1) for z in (0, 1)}
        with tempfile.TemporaryDirectory() as tmp:
            world = make_world(tmp, {(0, 0): chunk})
            dump = make_dump(Path(tmp) / "port.bin", 1, {(0, 0): {
                cell_offset(0, 0, x, -60, z): state for x in (0, 1) for z in (0, 1)}})
            report_path = Path(tmp) / "report.json"
            code, out, err = run_gate(self.gate, [str(world), str(dump), "1", "0", "0",
                                                 "--require-match", "--json-out", str(report_path)])
            self.assertEqual(code, 0, out + err)
            report = json.loads(report_path.read_text())
            self.assertEqual(report["chunks_compared"], 1)
            self.assertEqual(report["reference"]["total"], 4)
            self.assertEqual(report["counts"]["diffs"], 0)
            self.assertEqual(report["exit_code"], 0)

    def test_a_non_full_chunk_in_the_grid_is_invalid_not_skipped(self):
        """The old body `continue`d on a non-full chunk, so a partial grid could PASS."""
        state = block_state("sculk")
        chunks = {(cx, cz): chunk_cells(cx, cz) for cx in (0, 1) for cz in (0, 1)}
        with tempfile.TemporaryDirectory() as tmp:
            world = make_world(tmp, chunks, statuses={(1, 0): "minecraft:biomes"})
            dump = make_dump(Path(tmp) / "port.bin", 2, {
                (dx, dz): {cell_offset(cx, cz, x, y, z): state for (x, y, z) in cells}
                for dz, cz in ((0, 0), (1, 1)) for dx, cx in ((0, 0), (1, 1))
                for cells in (chunks[(cx, cz)],)
            })
            code, out, err = run_gate(self.gate, [str(world), str(dump), "2", "0", "0", "--require-match"])
        self.assertEqual(code, 2, out + err)
        self.assertIn("not full", err)
        self.assertIn("(1, 0)", err)
        self.assertNotIn("PASS", out)

    def test_absent_chunk_is_invalid_input_not_a_traceback(self):
        state = block_state("sculk")
        code, out, err = self.run_pair(
            {(0, 0): {(0, -60, 0): ("minecraft:sculk", {})}},
            {(0, 0): {cell_offset(0, 0, 0, -60, 0): state}},
            n=2,
        )
        self.assertEqual(code, 2, out + err)
        self.assertIn("not present", err)

    def test_negative_or_zero_n_is_invalid(self):
        with tempfile.TemporaryDirectory() as tmp:
            world = make_world(tmp, {(0, 0): {(0, -60, 0): ("minecraft:sculk", {})}})
            dump = Path(tmp) / "port.bin"
            dump.write_bytes(bytes(CELLS * 4))
            for n in (0, -1):
                with self.subTest(n=n):
                    code, out, err = run_gate(self.gate, [str(world), str(dump), str(n), "0", "0"])
                    self.assertEqual(code, 2, out + err)
                    self.assertIn("n must be positive", err)

    def test_truncated_dump_height_is_invalid(self):
        """The old body accepted any multiple of 4096 cells; only 24 sections are supported."""
        state = block_state("sculk")
        with tempfile.TemporaryDirectory() as tmp:
            world = make_world(tmp, {(0, 0): {(0, -60, 0): ("minecraft:sculk", {})}})
            short = make_dump(Path(tmp) / "short.bin", 1,
                              {(0, 0): {cell_offset(0, 0, 0, -60, 0): state}}, cells=5 * 4096)
            code, out, err = run_gate(self.gate, [str(world), str(short), "1", "0", "0", "--require-match"])
        self.assertEqual(code, 2, out + err)
        self.assertIn("full 24-section", err)
        self.assertNotIn("PASS", out)

    def test_dump_length_that_is_not_whole_cells_is_invalid(self):
        with tempfile.TemporaryDirectory() as tmp:
            world = make_world(tmp, {(0, 0): {(0, -60, 0): ("minecraft:sculk", {})}})
            broken = Path(tmp) / "port.bin"
            broken.write_bytes(bytes(CELLS * 4 + 2))
            code, out, err = run_gate(self.gate, [str(world), str(broken), "1", "0", "0"])
        self.assertEqual(code, 2, out + err)
        self.assertIn("not n*n", err)

    def test_a_missing_world_is_invalid_input_not_a_traceback(self):
        """`vanilla_chunk` opens `<world>/region/r.*.mca`; a missing world used to escape.

        The failure this pins: `FileNotFoundError` propagated out of `compare` (the
        `except (SystemExit, ValueError)` did not cover `OSError`), so the gate died with a
        traceback instead of reporting invalid input.
        """
        with tempfile.TemporaryDirectory() as tmp:
            dump = make_dump(Path(tmp) / "port.bin", 1, {})
            code, out, err = run_gate(self.gate, [str(Path(tmp) / "absent-world"), str(dump),
                                                  "1", "0", "0", "--require-match"])
        self.assertEqual(code, 2, out + err)
        self.assertIn("not a directory", err)
        self.assertNotIn("Traceback", out + err)
        self.assertNotIn("PASS", out)

    def test_a_world_without_the_region_file_is_invalid_input_not_a_traceback(self):
        """A world directory that exists but holds no `region/r.*.mca` is invalid input."""
        with tempfile.TemporaryDirectory() as tmp:
            world = Path(tmp) / "world"
            (world / "region").mkdir(parents=True)
            dump = make_dump(Path(tmp) / "port.bin", 1, {})
            code, out, err = run_gate(self.gate, [str(world), str(dump), "1", "0", "0",
                                                 "--require-match"])
        self.assertEqual(code, 2, out + err)
        self.assertIn("cannot read chunk", err)
        self.assertNotIn("Traceback", out + err)
        self.assertNotIn("PASS", out)

    def test_a_reference_without_sculk_cannot_pass(self):
        with tempfile.TemporaryDirectory() as tmp:
            world = make_world(tmp, {(0, 0): {(0, -60, 0): ("minecraft:stone", {})}})
            dump = make_dump(Path(tmp) / "port.bin", 1, {(0, 0): {}})
            code, out, err = run_gate(self.gate, [str(world), str(dump), "1", "0", "0", "--require-match"])
        self.assertEqual(code, 2, out + err)
        self.assertIn("no sculk cells", out)

    def test_out_of_range_state_id_is_input_error(self):
        code, out, err = self.run_pair(
            {(0, 0): {(0, -60, 0): ("minecraft:sculk", {})}},
            {(0, 0): {0: 0xFFFFFF}},
            extra=("--require-match",),
        )
        self.assertEqual(code, 2, out + err)
        self.assertIn("unmappable block state id", err)

    def test_report_mode_still_reports_without_requiring_match(self):
        wrong = block_state("sculk_catalyst", {"bloom": "false"})
        chunk = {(0, -60, 0): ("minecraft:sculk_catalyst", {"bloom": "true"})}
        with tempfile.TemporaryDirectory() as tmp:
            world = make_world(tmp, {(0, 0): chunk})
            dump = make_dump(Path(tmp) / "port.bin", 1, {(0, 0): {cell_offset(0, 0, 0, -60, 0): wrong}})
            report_path = Path(tmp) / "report.json"
            code, out, err = run_gate(self.gate, [str(world), str(dump), "1", "0", "0",
                                                 "--json-out", str(report_path)])
            self.assertEqual(code, 0, out + err)  # report-only contract is unchanged
            report = json.loads(report_path.read_text())
            self.assertEqual(report["counts"]["wrong_state"], 1)
            self.assertEqual(report["exit_code"], 0)
            self.assertIn("REPORT ONLY", out)


if __name__ == "__main__":
    unittest.main()
