#!/usr/bin/env python3
"""Contracts for the real-chunk fossil acceptance gate (P1-4).

`tools/fossil_chunk_gate.py` replays the port's generation of the Nether chunk that a
pinned native 1.21.1 server decorated with `minecraft:nether_fossil` (seed 4242,
chunk (-22,20), frozen `structures` stage) and compares every channel P1-4 names
against committed fixtures: all 65,536 canonical block states, the 256-column
OCEAN_FLOOR heightmap, and the 79 pending fluid ticks of the whole 9-chunk decorate
pass.

The cases below pin the two ways this gate could silently mean nothing:

* the fixture might not be the fossil-positive capture at all (no verified start, no
  bone blocks, a partial window, or a population order that does not cover the
  decoration halo the dump requires) - so fixture integrity is asserted before any
  port comparison is trusted;
* the comparator might pass on anything - so each arm is fed a planted difference and
  must report it.

The end-to-end case runs the real `target/debug/megu` binary through the production
`MEGU_WG_DUMP`/`MEGU_POP_ORDER` path; it skips when the binary is not built, the same
convention as `tests/client_launcher_test.py`.
"""
import importlib.util
import json
import struct
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))

spec = importlib.util.spec_from_file_location(
    "fossil_chunk_gate", ROOT / "tools" / "fossil_chunk_gate.py")
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)

CANON = gate.CANON
META = gate.META
ORDER = gate.ORDER
BINARY = ROOT / "target" / "debug" / "megu"


def load_fixture():
    rows, header = gate.load_canon_tsv(CANON)
    meta = gate.load_meta(META)
    order = gate.load_order_tsv(ORDER)
    return rows, meta, order, header


class FixtureIntegrity(unittest.TestCase):
    def test_fixture_is_the_fossil_positive_capture(self):
        rows, meta, order, _ = load_fixture()
        fp = meta["fossil_positive"]
        self.assertEqual(fp["structure"], "minecraft:nether_fossil")
        self.assertTrue(fp["verified_start"])
        self.assertGreaterEqual(fp["children"], 1)
        self.assertEqual(fp["child_piece"], "minecraft:nefos")
        # The recorded fossil cells must exist in the fixture with their recorded
        # states - this is what makes the comparison fossil-positive, not just any
        # Nether chunk that happens to pass.
        bones = {(b["x"], b["y"], b["z"]): b["state"] for b in fp["bone_block_cells"]}
        self.assertGreaterEqual(len(bones), 10)
        bb = fp["child_bb"]
        for (x, y, z), state in bones.items():
            self.assertTrue(bb[0] <= x <= bb[3] and bb[1] <= y <= bb[4]
                            and bb[2] <= z <= bb[5], (x, y, z))
            self.assertTrue(state.startswith("minecraft:bone_block"), state)
            self.assertEqual(rows.get((x, y, z)), state)
        self.assertEqual(gate.validate_fixture(rows, order, meta)["non_air"], 24688)

    def test_fixture_covers_the_full_dimension_window(self):
        rows, meta, _, header = load_fixture()
        chunk = meta["chunk"]
        self.assertEqual((chunk["cx"], chunk["cz"]), (-22, 20))
        self.assertEqual((chunk["min_y"], chunk["height"]), (0, 256))
        self.assertIn("chunk -22,20", header)
        # Every recorded census number is recomputed from the fixture, not trusted.
        census = gate.census_of(rows, -22, 20, 0, 256)
        recorded = chunk["census"]
        for key in ("air", "cave_air", "void_air", "non_air", "total"):
            self.assertEqual(int(recorded[key]), census[key], key)
        self.assertEqual(census["total"], 65536)

    def test_population_order_covers_the_decoration_halo(self):
        _, meta, order, _ = load_fixture()
        halo = {(-22 + dx, 20 + dz) for dx in (-1, 0, 1) for dz in (-1, 0, 1)}
        self.assertEqual(set(order), halo)
        self.assertEqual(meta["population"]["scope"], "exact")
        self.assertEqual(order, [tuple(c) for c in meta["population"]["source_chunks"]])

    def test_provenance_binds_the_pinned_capture(self):
        _, meta, _, _ = load_fixture()
        native = meta["native"]
        self.assertEqual(native["server_sha1"], "59353fb40c36d304f2035d51e7d6e6baa98dc05c")
        for key in ("server_sha256", "level_dat_sha256", "region_sha256",
                    "order_tsv_sha256", "run_log_sha256", "boundary_json_sha256"):
            self.assertRegex(native[key], r"^[0-9a-f]{64}$", key)
        boundary = native["boundary"]
        self.assertEqual(boundary["mode"], "frozen")
        self.assertEqual(boundary["stage"], "structures")
        self.assertEqual(boundary["seed"], 4242)
        self.assertEqual(boundary["dimension"], "minecraft:the_nether")
        self.assertEqual(boundary["runtime_writes"], 0)
        self.assertEqual(meta["chunk"]["Status"], "minecraft:full")
        self.assertEqual(meta["chunk"]["DataVersion"], 3955)
        # Two independent native runs produced identical chunk content; the fixture
        # must keep that record so the gate's 0-diff requirement is justified.
        self.assertIn("second_run", native)

    def test_pending_tick_record_is_complete(self):
        _, meta, _, _ = load_fixture()
        ticks = meta["pending_ticks"]["fluid_ticks"]
        self.assertEqual(len(ticks), meta["pending_ticks"]["fluid_ticks_total"])
        self.assertEqual(len(ticks), 79)
        for tick in ticks:
            self.assertEqual(tick["i"], "minecraft:lava")
            self.assertTrue(-368 <= tick["x"] <= -321 and 304 <= tick["z"] <= 351,
                            tick)
        # The native record covers every populated chunk, not only the target.
        chunks = {(t["x"] >> 4, t["z"] >> 4) for t in ticks}
        self.assertEqual(chunks, {(-23, 19), (-23, 20), (-23, 21),
                                  (-22, 19), (-22, 20), (-22, 21),
                                  (-21, 19), (-21, 20), (-21, 21)})
        self.assertEqual(meta["pending_ticks"]["block_ticks_total"], 0)
        self.assertEqual(meta["chunk"]["block_entities"], 0)


class ComparatorBites(unittest.TestCase):
    """Each comparison arm must see a planted difference; a gate that cannot go red
    on a wrong cell is a gate that cannot go green for the right reason either."""

    @classmethod
    def setUpClass(cls):
        if not BINARY.is_file():
            raise unittest.SkipTest("build target/debug/megu to run the bite probes")
        tmp = tempfile.TemporaryDirectory(prefix="fossil-gate-real-")
        cls.addClassCleanup(tmp.cleanup)
        env = gate.build_env(Path(tmp.name), ORDER)
        gate.run_dump(BINARY, env, Path(tmp.name) / "dump.log")
        cls.canon_text = (Path(tmp.name) / "canon.tsv").read_text()
        cls.hm_bytes = (Path(tmp.name) / "hm.bin").read_bytes()
        cls.ticks_text = (Path(tmp.name) / "fluid_ticks.tsv").read_text()

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="fossil-gate-test-")
        self.addCleanup(self.tmp.cleanup)

    def compare(self, canon, hm, ticks):
        workdir = Path(self.tmp.name)
        (workdir / "canon.tsv").write_text(canon, encoding="utf-8")
        (workdir / "hm.bin").write_bytes(hm)
        (workdir / "fluid_ticks.tsv").write_text(ticks, encoding="utf-8")
        meta = json.loads(META.read_text())
        rows, _ = gate.load_canon_tsv(CANON)
        return gate.compare_all(rows, meta, workdir)

    def test_clean_dump_reports_match(self):
        report = self.compare(self.canon_text, self.hm_bytes, self.ticks_text)
        self.assertEqual(report["status"], "MATCH")
        self.assertEqual(report["arms"]["states"]["differing_cells"], 0)
        self.assertEqual(report["arms"]["heightmap"]["differing_columns"], 0)
        self.assertEqual(report["arms"]["pending_ticks"]["port_only"], 0)
        self.assertEqual(report["arms"]["pending_ticks"]["native_only"], 0)

    def test_one_wrong_state_is_counted(self):
        lines = self.canon_text.splitlines()
        for i, line in enumerate(lines):
            if not line.startswith("#") and "\t" in line:
                fields = line.split("\t")
                fields[3] = "minecraft:dirt"
                lines[i] = "\t".join(fields)
                break
        report = self.compare("\n".join(lines) + "\n", self.hm_bytes, self.ticks_text)
        self.assertEqual(report["status"], "DIFF")
        self.assertEqual(report["arms"]["states"]["differing_cells"], 1)
        self.assertEqual(report["arms"]["states"]["samples"][0]["port"], "minecraft:dirt")

    def test_one_missing_cell_is_counted(self):
        lines = self.canon_text.splitlines()
        header = lines[0]
        declared = int(header.rsplit("cells ", 1)[-1])
        lines[0] = header.rsplit("cells ", 1)[0] + f"cells {declared - 1}"
        for i, line in enumerate(lines):
            if not line.startswith("#"):
                del lines[i]
                break
        report = self.compare("\n".join(lines) + "\n", self.hm_bytes, self.ticks_text)
        self.assertEqual(report["status"], "DIFF")
        self.assertEqual(report["arms"]["states"]["differing_cells"], 1)

    def test_one_heightmap_column_is_counted(self):
        cols = list(struct.unpack("<256i", self.hm_bytes))
        cols[7] += 1
        report = self.compare(self.canon_text,
                              struct.pack("<256i", *cols), self.ticks_text)
        self.assertEqual(report["status"], "DIFF")
        self.assertEqual(report["arms"]["heightmap"]["differing_columns"], 1)
        self.assertEqual(report["arms"]["heightmap"]["samples"][0]["column"], 7)

    def test_one_pending_tick_is_counted(self):
        kept = self.ticks_text.splitlines()[1:]
        report = self.compare(self.canon_text, self.hm_bytes, "\n".join(kept) + "\n")
        self.assertEqual(report["status"], "DIFF")
        self.assertEqual(report["arms"]["pending_ticks"]["native_only"], 1)

    def test_an_extra_pending_tick_is_counted(self):
        extra = self.ticks_text + "-300\t70\t300\t2\n"
        report = self.compare(self.canon_text, self.hm_bytes, extra)
        self.assertEqual(report["status"], "DIFF")
        self.assertEqual(report["arms"]["pending_ticks"]["port_only"], 1)


class EndToEnd(unittest.TestCase):
    @unittest.skipUnless(BINARY.is_file(),
                         "build target/debug/megu to run the fossil acceptance gate")
    def test_gate_replays_the_native_fossil_chunk(self):
        with tempfile.TemporaryDirectory(prefix="fossil-gate-e2e-") as tmp:
            out = Path(tmp) / "report.json"
            proc = subprocess.run(
                [sys.executable, str(ROOT / "tools" / "fossil_chunk_gate.py"),
                 "--binary", str(BINARY), "--repeat", "2", "--json-out", str(out)],
                cwd=ROOT, capture_output=True, text=True)
            self.assertEqual(proc.returncode, 0,
                             f"gate exited {proc.returncode}:\n{proc.stderr[-2000:]}")
            report = json.loads(out.read_text())
            self.assertEqual(report["status"], "MATCH")
            self.assertEqual(report["seed"], 4242)
            self.assertEqual(report["repeat"], 2)
            arms = report["arms"]
            self.assertEqual(arms["states"]["total_cells"], 65536)
            self.assertEqual(arms["states"]["differing_cells"], 0)
            self.assertEqual(arms["states"]["native_cells"], 25336)
            self.assertEqual(arms["heightmap"]["differing_columns"], 0)
            self.assertEqual(arms["pending_ticks"]["native_fluid_ticks"], 79)
            self.assertEqual(arms["pending_ticks"]["port_only"], 0)
            self.assertEqual(arms["pending_ticks"]["native_only"], 0)
            self.assertEqual(report["artifacts"]["dump.bin"],
                             "b6628d89652caa1ba6cb1ffc66f2ea1781afea501f87e7e0f3edb9b64a1f0f90")


if __name__ == "__main__":
    unittest.main()
