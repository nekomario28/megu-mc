#!/usr/bin/env python3
"""Contracts for tools/structure_capture.py: admission, start verification, and the row it emits.

Two of these cases are regressions of mistakes made while writing the recipe, both of which
produced a recipe that *looked* like it worked:

* the candidate scan queried one cell corner and then required the probe's answer to equal the
  corner. The probe answers "which chunk is this chunk's cell candidate", so that comparison can
  only ever be true when a cell's random offset happens to be zero - the scan reported "no
  candidates" for a structure (the igloo) that has one in almost every 32x32 chunk cell. The
  fake probe below answers with a candidate that is *not* the queried chunk, so the old
  comparison would fail this test.
* `--worldgen-structure-candidates` prints two line shapes (a spread cell and a concentric-ring
  point list), and parsing only the first made every scan of any structure crash on the
  strongholds line - the ring form has no `frequency=` field at all.

The rest pin the fail-closed edges the recipe must not paper over: a structure id that is not in
the pack, a capture whose saved chunk carries no start for the requested structure, and a row
that claims an artifact it cannot bind.
"""
import contextlib
import importlib.util
import io
import json
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests"))
from chunk_triage_test import encode_chunk_pos_long, make_chunk_nbt  # noqa: E402

spec = importlib.util.spec_from_file_location("structure_capture", ROOT / "tools/structure_capture.py")
sc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sc)

SPREAD_LINE = "minecraft:igloos\t5,23\tfrequency=true\texclusion_unchecked=false\tbiome_and_pieces_unchecked=true"
RING_LINE = ("minecraft:strongholds\t[(69, 58), (-92, 33)]\t"
             "preferred_biome_relocation_unchecked=true\tbiome_and_pieces_unchecked=true")


class CandidateParsing(unittest.TestCase):
    def test_both_probe_line_shapes_parse(self):
        rows = sc.parse_candidates(SPREAD_LINE + "\n" + RING_LINE + "\n")
        self.assertEqual(rows["minecraft:igloos"]["candidate"], (5, 23))
        self.assertTrue(rows["minecraft:igloos"]["frequency"])
        self.assertFalse(rows["minecraft:igloos"]["exclusion"])
        # The ring form is not a cell candidate: its points are the placement, there is no
        # frequency reducer, and reading `rings` as a candidate would silently mis-admit.
        self.assertIsNone(rows["minecraft:strongholds"]["candidate"])
        self.assertEqual(rows["minecraft:strongholds"]["rings"], [(69, 58), (-92, 33)])
        self.assertFalse(rows["minecraft:strongholds"]["frequency"])

    def test_empty_output_is_an_error_not_an_empty_scan(self):
        with self.assertRaises(sc.RecipeError):
            sc.parse_candidates("\n\n")

    def test_a_spread_set_without_a_frequency_verdict_is_refused(self):
        broken = "minecraft:igloos\t5,23\tbiome_and_pieces_unchecked=true"
        with self.assertRaises(sc.RecipeError):
            sc.parse_candidates(broken)


class AdmissionScan(unittest.TestCase):
    """The scan enumerates *cell candidates*, not "chunks whose probe answer is themselves"."""

    def fake_probe(self, spacing, offset):
        def probe(binary, seed, chunks):
            out = []
            for cx, cz in chunks:
                # Every chunk of a cell answers with the same candidate, which sits `offset`
                # chunks into the cell - never on the queried corner.
                u, v = cx // spacing, cz // spacing
                line = f"minecraft:igloos\t{u * spacing + offset},{v * spacing + offset}\t" \
                       "frequency=true\texclusion_unchecked=false\tbiome_and_pieces_unchecked=true"
                out.append((cx, cz, sc.parse_candidates(line)))
            return out
        return probe

    def test_scan_returns_the_cell_candidate(self):
        sets = {"minecraft:igloos": {"placement": {"type": "minecraft:random_spread",
                                                   "spacing": 32, "separation": 8, "salt": 1},
                                        "members": ["minecraft:igloo"]}}
        with patch.object(sc, "probe_candidates", side_effect=self.fake_probe(32, 5)):
            found = sc.admitted_candidates(Path("/bin/true"), 4242, "minecraft:igloo", sets, (0, 0), 32)
        chunks = sorted(tuple(entry["chunk"]) for entry in found)
        self.assertEqual(chunks, [(-27, -27), (-27, 5), (5, -27), (5, 5)])
        self.assertTrue(all(entry["set"] == "minecraft:igloos" for entry in found))

    def test_the_scan_drops_frequency_rejects_and_exclusion_members(self):
        sets = {"minecraft:igloos": {"placement": {"type": "minecraft:random_spread",
                                                   "spacing": 32, "separation": 8, "salt": 1},
                                        "members": ["minecraft:igloo"]}}

        def probe(binary, seed, chunks):
            out = []
            for cx, cz in chunks:
                # Verdicts keyed on the *candidate*, not the queried chunk: the scan's own
                # radius filter drops candidates outside the box, so a verdict keyed on the
                # queried corner would make the flags unobservable.
                freq = "true" if (cz + 4) <= 0 else "false"
                excl = "true" if (cx + 4) > 0 else "false"
                line = f"minecraft:igloos\t{cx + 4},{cz + 4}\tfrequency={freq}\texclusion_unchecked={excl}\tbiome_and_pieces_unchecked=true"
                out.append((cx, cz, sc.parse_candidates(line)))
            return out

        with patch.object(sc, "probe_candidates", side_effect=probe):
            found = sc.admitted_candidates(Path("/bin/true"), 0, "minecraft:igloo", sets, (0, 0), 32)
        # The scan reports both verdicts alongside the candidate; `main` applies them in one
        # place, so a rejected cell cannot be dropped by one stage and admitted by another.
        # 3x3 cells for spacing 32 around (0,0); the corner cells' candidates (x or z = 36)
        # fall outside --scan-radius 32, which is why four of the nine come back.
        self.assertEqual(len(found), 4)
        self.assertTrue(any(not entry["frequency"] for entry in found))
        self.assertTrue(any(entry["exclusion"] for entry in found))
        self.assertTrue(any(entry["frequency"] and not entry["exclusion"] for entry in found))


class DatapackResolution(unittest.TestCase):
    def test_structure_sets_are_found_from_the_pack(self):
        sets = sc.structure_sets_for("minecraft:igloo")
        self.assertIn("minecraft:igloos", sets)
        self.assertEqual(sets["minecraft:igloos"]["placement"]["spacing"], 32)

    def test_a_misspelled_structure_reports_close_ids_instead_of_scanning(self):
        with self.assertRaises(sc.RecipeError) as caught:
            sc.structure_sets_for("minecraft:igloo_typo")
        self.assertIn("close ids", str(caught.exception))

    def test_biome_tags_resolve_to_concrete_ids(self):
        biomes, doc = sc.structure_biomes("minecraft:igloo")
        self.assertIn("minecraft:snowy_taiga", biomes)
        self.assertEqual(doc["type"], "minecraft:igloo")
        self.assertFalse(any(b.startswith("#") for b in biomes))


class StartVerification(unittest.TestCase):
    def test_the_captured_chunk_must_carry_the_start(self):
        chunk = make_chunk_nbt(3, 4, starts={"minecraft:igloo": {"Children": [{"BB": [1, 2, 3, 4, 5, 6]}]}},
                              references={"minecraft:igloo": [encode_chunk_pos_long(3, 4)]})
        with patch.object(sc.chunk_triage, "read_chunk_nbt", return_value=chunk):
            starts = sc.native_start(Path("/nonexistent"), "minecraft:igloo", (3, 4), 1, "overworld")
        self.assertEqual(len(starts), 2)  # one child BB plus the References entry
        self.assertEqual(starts[0]["BB"], [1, 2, 3, 4, 5, 6])
        self.assertEqual(tuple(starts[1]["reference"]), (3, 4))

    def test_a_chunk_without_the_start_verifies_nothing(self):
        chunk = make_chunk_nbt(3, 4, starts={"minecraft:village_plains": {"Children": []}})
        with patch.object(sc.chunk_triage, "read_chunk_nbt", return_value=chunk):
            starts = sc.native_start(Path("/nonexistent"), "minecraft:igloo", (3, 4), 1, "overworld")
        self.assertEqual(starts, [])


class RowAssembly(unittest.TestCase):
    """A row may not claim an artifact it cannot bind, and it binds what the comparator reads."""

    def setUp(self):
        import tempfile
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        capture = self.root / "nat-4242-igloo"
        (capture / "world" / "region").mkdir(parents=True)
        (capture / "world" / "level.dat").write_bytes(b"\x00")
        (capture / "world" / "region" / "r.1.-16.mca").write_bytes(b"\x01")
        (capture / "run.log").write_text("")
        (capture / "order.tsv").write_text("1\t-16\n")
        (capture / "boundary.json").write_text(json.dumps({
            "mode": "frozen", "dimension": "minecraft:overworld", "min_y": -64, "height": 384,
            "seed": 4242, "stage": "structures", "world_subdirectory": "world",
            "chunks": 1, "runtime_writes": 0,
        }))
        (self.root / "dumps").mkdir()
        (self.root / "dumps" / "dump.bin").write_bytes(b"\x02")
        self.capture = capture

    def tearDown(self):
        self.tmp.cleanup()

    def row(self):
        boundary = sc.read_boundary(self.capture)
        return sc.build_row(None, self.capture, boundary, "minecraft:igloo",
                            self.root / "dumps" / "dump.bin", (37, -512), 1, "noise",
                            self.root, False)

    def test_row_binds_level_dat_region_dump_log_and_order(self):
        row = self.row()
        self.assertEqual(sorted(row["inputs"]["sha256"]),
                         ["dumps/dump.bin", "nat-4242-igloo/order.tsv",
                          "nat-4242-igloo/run.log", "nat-4242-igloo/world/level.dat",
                          "nat-4242-igloo/world/region/r.1.-16.mca"])
        self.assertEqual(row["id"], "ow-4242-structures-37--512--64-384")
        self.assertEqual(row["stage"], {"reference": "structures", "port": "structures"})
        self.assertEqual(row["inputs"]["order"]["path"], "nat-4242-igloo/order.tsv")
        self.assertNotIn("expected_runtime_writes", row["inputs"])

    def test_a_capture_with_runtime_writes_must_declare_them(self):
        (self.capture / "boundary.json").write_text(json.dumps({
            "mode": "frozen", "dimension": "minecraft:overworld", "min_y": -64, "height": 384,
            "seed": 4242, "stage": "structures", "world_subdirectory": "world",
            "chunks": 1, "runtime_writes": 4,
        }))
        boundary = sc.read_boundary(self.capture)
        row = sc.build_row(None, self.capture, boundary, "minecraft:igloo",
                           self.root / "dumps" / "dump.bin", (37, -512), 1, "noise",
                           self.root, True)
        declared = row["inputs"]["expected_runtime_writes"]
        self.assertEqual(declared["count"], 0)  # the empty run.log records no wrote=true line
        self.assertEqual(declared["blocks"], {})

    def test_a_missing_region_file_is_an_error_not_an_unbound_row(self):
        (self.capture / "world" / "region" / "r.1.-16.mca").unlink()
        with self.assertRaises(sc.RecipeError):
            self.row()

    def test_row_paths_are_relative_to_the_manifest_it_is_written_into(self):
        boundary = sc.read_boundary(self.capture)
        row = sc.build_row(None, self.capture, boundary, "minecraft:igloo",
                           self.root / "dumps" / "dump.bin", (37, -512), 1, "noise",
                           self.root / "elsewhere", False)
        self.assertTrue(row["fixture"].startswith("../nat-4242-igloo"))
        self.assertEqual(row["world"], "../nat-4242-igloo/world")


class CaptureRefusesToOverwrite(unittest.TestCase):
    def test_an_existing_capture_directory_is_never_reused_as_a_target(self):
        # `native_fixture.sh` itself refuses; the recipe must not work around that with a
        # rename, because a capture is the evidence a row cites.
        text = (ROOT / "tools" / "structure_capture.py").read_text()
        self.assertIn("refusing to overwrite capture", text)
        self.assertNotIn("shutil.rmtree", text)


if __name__ == "__main__":
    with contextlib.redirect_stdout(io.StringIO()):
        unittest.main(verbosity=2)
