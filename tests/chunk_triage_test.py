#!/usr/bin/env python3
"""Contracts for tools/chunk_triage.py: air-kind fidelity, fail-closed bounds, and structure attribution."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import struct
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("chunk_triage", ROOT / "tools/chunk_triage.py")
ct = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ct)


def make_chunk_nbt(cx, cz, *, status="minecraft:full", min_y=-64, height=32,
                   blocks=None, references=None, starts=None, biome="minecraft:snowy_plains",
                   world_surface_val=None):
    """Build an in-memory Anvil-shaped chunk dict for testing."""
    sections = []
    num_sec = height // 16
    min_sy = min_y // 16
    blocks = blocks or {}
    for sidx in range(num_sec):
        sy = min_sy + sidx
        palette = [{"Name": "minecraft:air"}]
        pal_index = {("minecraft:air", ()): 0}
        cell_vals = [0] * 4096
        for i in range(4096):
            lx, lz, ly = i & 15, (i >> 4) & 15, i >> 8
            wx, wy, wz = cx * 16 + lx, sy * 16 + ly, cz * 16 + lz
            entry = blocks.get((wx, wy, wz))
            if entry is None:
                continue
            name, props = entry
            key = (name, tuple(sorted(props.items())))
            if key not in pal_index:
                pal_index[key] = len(palette)
                p = {"Name": name}
                if props:
                    p["Properties"] = dict(props)
                palette.append(p)
            cell_vals[i] = pal_index[key]
        bs = {"palette": palette}
        if len(palette) > 1:
            bits = max(4, (len(palette) - 1).bit_length())
            per = 64 // bits
            words = [0] * ((4096 + per - 1) // per)
            for i, v in enumerate(cell_vals):
                words[i // per] |= (v << ((i % per) * bits))
            bs["data"] = words
        sections.append({
            "Y": sy,
            "block_states": bs,
            "biomes": {"palette": [biome]},
        })

    hms = {}
    if world_surface_val == "auto":
        words = [0] * ((256 + 6) // 7)
        for lz in range(16):
            for lx in range(16):
                wx, wz = cx * 16 + lx, cz * 16 + lz
                top = max((wy for (bx, wy, bz) in blocks if bx == wx and bz == wz), default=min_y - 1)
                rel = (top + 1) - min_y
                idx = lz * 16 + lx
                words[idx // 7] |= (rel << ((idx % 7) * 9))
        hms["WORLD_SURFACE"] = words
        hms["OCEAN_FLOOR"] = words
    elif world_surface_val is not None:
        rel = world_surface_val - min_y
        words = [0] * ((256 + 6) // 7)
        for i in range(256):
            words[i // 7] |= (rel << ((i % 7) * 9))
        hms["WORLD_SURFACE"] = words
        hms["OCEAN_FLOOR"] = words

    return {
        "DataVersion": 3955,
        "Status": status,
        "xPos": cx,
        "yPos": min_y // 16,
        "zPos": cz,
        "LastUpdate": 1,
        "InhabitedTime": 0,
        "isLightOn": 1,
        "sections": sections,
        "Heightmaps": hms,
        "block_entities": [],
        "block_ticks": [],
        "fluid_ticks": [],
        "structures": {
            "References": references or {},
            "starts": starts or {},
        },
    }


def encode_chunk_pos_long(cx, cz):
    return (cx & 0xFFFFFFFF) | ((cz & 0xFFFFFFFF) << 32)


class ChunkTriageContracts(unittest.TestCase):
    def test_matching_layers_are_bulk_compared_without_hiding_native_b_changes(self):
        class BudgetedCells(list):
            """Count Python per-cell reads, while permitting bulk slice/equality reads."""
            def __init__(self, values, budget):
                super().__init__(values)
                self.budget = budget
                self.reads = 0

            def __getitem__(self, key):
                if isinstance(key, int):
                    self.reads += 1
                    if self.reads > self.budget:
                        raise AssertionError('matching layers were scanned cell by cell')
                return super().__getitem__(key)

        nbt_a = make_chunk_nbt(0, 0)
        nbt_b = make_chunk_nbt(0, 0, blocks={(0, -60, 0): ('minecraft:cave_air', {})})
        with patch.object(ct, 'read_chunk_nbt', return_value=nbt_a):
            a = ct.load_native_chunk('unused', 0, 0)
        with patch.object(ct, 'read_chunk_nbt', return_value=nbt_b):
            b = ct.load_native_chunk('unused-b', 0, 0)
        args = SimpleNamespace(halo=1, samples=5, hm_name='OCEAN_FLOOR')
        for reference_b, budget, expected_ab in [(None, 0, 0), (b, 256, 1)]:
            with self.subTest(world_b=reference_b is not None):
                port = {'cells': BudgetedCells(a['cells'], budget)}
                res = ct._compare_chunk(args, a, reference_b, port, {'pieces': []}, None)
                self.assertEqual(res['totals'], {
                    'all_air_diffs': 0, 'non_air_projection_diffs': 0,
                    'native_ab_all_diffs': expected_ab, 'native_ab_non_air_diffs': 0,
                    'consistent_diffs': 0, 'unstable_diffs': 0,
                })
                self.assertEqual(res['overall_pairs'], {})
                self.assertEqual(res['buckets'], {})

    def test_bulk_comparison_rejects_short_cell_arrays(self):
        with patch.object(ct, 'read_chunk_nbt', return_value=make_chunk_nbt(0, 0)):
            a = ct.load_native_chunk('unused', 0, 0)
        args = SimpleNamespace(halo=1, samples=5, hm_name='OCEAN_FLOOR')
        for side in ('native_a', 'native_b', 'port'):
            with self.subTest(side=side):
                inputs = {'native_a': dict(a), 'native_b': dict(a),
                          'port': {'cells': list(a['cells'])}}
                inputs[side]['cells'] = []
                with self.assertRaisesRegex(ct.TriageError, side + ' has 0 cells'):
                    ct._compare_chunk(args, inputs['native_a'], inputs['native_b'],
                                      inputs['port'], {'pieces': []}, None)

    def test_cave_air_and_void_air_are_never_collapsed(self):
        native = make_chunk_nbt(
            3, -2,
            blocks={
                (48, -60, -32): ("minecraft:cave_air", {}),
                (49, -60, -32): ("minecraft:void_air", {}),
                (50, -60, -32): ("minecraft:stone", {}),
            },
        )
        with tempfile.TemporaryDirectory() as tmp:
            tsv = Path(tmp) / "port.tsv"
            tsv.write_text("# chunk 3,-2 min_y -64 height 32 cells 1\n50\t-60\t-32\tminecraft:stone\n")
            with patch.object(ct, "read_chunk_nbt", return_value=native):
                out = io.StringIO()
                with contextlib.redirect_stdout(out):
                    rc = ct.main([tmp, "3", "-2", "--port-tsv", str(tsv), "--fail-on-diff"])
        self.assertEqual(rc, 1)
        txt = out.getvalue()
        self.assertIn("all_air_kinds=2 / 8192", txt)
        self.assertIn("(lossy non_air_projection=0)", txt)
        self.assertIn("minecraft:cave_air -> minecraft:air", txt)
        self.assertIn("minecraft:void_air -> minecraft:air", txt)

    def test_structure_child_bb_and_halo_attribution_with_negative_start_chunk(self):
        scx, scz = -4, 3
        target = make_chunk_nbt(
            3, -2,
            blocks={
                (63, -55, -20): ("minecraft:cave_air", {}),
                (63, -56, -20): ("minecraft:oak_planks", {}),
                (50, -50, -25): ("minecraft:diorite", {}),
                (50, -40, -25): ("minecraft:stone", {}),
            },
            references={"minecraft:mineshaft": [encode_chunk_pos_long(scx, scz)]},
        )
        start_chunk = make_chunk_nbt(
            scx, scz,
            starts={
                "minecraft:mineshaft": {
                    "Children": [
                        {"id": "minecraft:mscorridor", "BB": [62, -55, -21, 65, -53, -2]},
                    ]
                }
            },
        )

        def fake_reader(world, cx, cz):
            if (cx, cz) == (3, -2):
                return target
            if (cx, cz) == (scx, scz):
                return start_chunk
            raise AssertionError(f"unexpected chunk read {(cx, cz)}")

        with tempfile.TemporaryDirectory() as tmp:
            tsv = Path(tmp) / "port.tsv"
            tsv.write_text(
                "# chunk 3,-2 min_y -64 height 32 cells 2\n"
                "50\t-50\t-25\tminecraft:granite\n"
                "50\t-40\t-25\tminecraft:stone\n"
            )
            json_out = Path(tmp) / "report.json"
            with patch.object(ct, "read_chunk_nbt", side_effect=fake_reader):
                out = io.StringIO()
                with contextlib.redirect_stdout(out):
                    rc = ct.main([tmp, "3", "-2", "--port-tsv", str(tsv), "--json-out", str(json_out)])
            self.assertEqual(rc, 0)
            report = json.loads(json_out.read_text())
        self.assertEqual(report["totals"]["all_air_diffs"], 3)
        buckets = {entry["bucket"]: entry["count"] for entry in report["attribution"]}
        self.assertEqual(buckets["inside_structure_bb | minecraft:mineshaft | minecraft:mscorridor#0"], 1)
        self.assertEqual(buckets["near_structure_halo(d=1) | minecraft:mineshaft | minecraft:mscorridor#0"], 1)
        self.assertEqual(buckets["outside_structures | underground"], 1)

    def test_incomplete_native_chunk_fails_closed(self):
        partial = make_chunk_nbt(0, 0, status="minecraft:biomes")
        with tempfile.TemporaryDirectory() as tmp:
            tsv = Path(tmp) / "port.tsv"
            tsv.write_text("# chunk 0,0 min_y -64 height 32 cells 0\n")
            with patch.object(ct, "read_chunk_nbt", return_value=partial):
                err = io.StringIO()
                with contextlib.redirect_stderr(err):
                    rc = ct.main([tmp, "0", "0", "--port-tsv", str(tsv)])
        self.assertEqual(rc, 2)
        self.assertIn("minecraft:biomes", err.getvalue())

    def test_mismatched_tsv_chunk_header_or_coords_fails_closed(self):
        native = make_chunk_nbt(3, -2)
        with tempfile.TemporaryDirectory() as tmp:
            bad_hdr = Path(tmp) / "bad_hdr.tsv"
            bad_hdr.write_text("# chunk 0,0 min_y -64 height 32 cells 0\n")
            bad_coord = Path(tmp) / "bad_coord.tsv"
            bad_coord.write_text("# chunk 3,-2 min_y -64 height 32 cells 1\n0\t-64\t0\tminecraft:bedrock\n")
            with patch.object(ct, "read_chunk_nbt", return_value=native):
                for path in (bad_hdr, bad_coord):
                    err = io.StringIO()
                    with contextlib.redirect_stderr(err):
                        rc = ct.main([tmp, "3", "-2", "--port-tsv", str(path)])
                    self.assertEqual(rc, 2)

    def test_port_bin_slicing_and_world_b_noise_split(self):
        blocks_meta = json.loads((ROOT / "assets/blocks_1211.json").read_text())
        grass = next(b for b in blocks_meta if b["name"] == "grass_block")
        snowy_false = grass["minStateId"] + 1  # snowy=false
        stone = next(b for b in blocks_meta if b["name"] == "stone")["minStateId"]

        native_a = make_chunk_nbt(
            1, 0,
            blocks={
                (16, -60, 0): ("minecraft:grass_block", {"snowy": "true"}),
                (17, -60, 0): ("minecraft:stone", {}),
            },
            world_surface_val="auto",
        )
        native_b = make_chunk_nbt(
            1, 0,
            blocks={
                (16, -60, 0): ("minecraft:grass_block", {"snowy": "true"}),
                (17, -60, 0): ("minecraft:diorite", {}),
            },
            world_surface_val="auto",
        )

        def fake_reader(world, cx, cz):
            return native_a if world.endswith("/a") else native_b

        with tempfile.TemporaryDirectory() as tmp:
            # Build a 2x2 grid binary dump starting at (0, 0); slice (1, 0) = dx=1, dz=0 (chunk_idx = 1).
            chunk_cells = 2 * 4096
            grid = [0] * (4 * chunk_cells)
            base = 1 * chunk_cells
            # Local (lx=0, lz=0, y=-60 => sy=-4, ly=4) -> offset 4*256 + 0 = 1024
            grid[base + 1024] = snowy_false  # deliberate property diff vs snowy=true
            grid[base + 1025] = stone        # matches native A, differs from native B
            bin_path = Path(tmp) / "dump.bin"
            bin_path.write_bytes(struct.pack(f"<{len(grid)}I", *grid))

            hm_path = Path(tmp) / "hm.bin"
            hm_vals = [-64] * (4 * 256)
            hm_vals[256] = -58  # differs from computed -59 at (0,0) in chunk (1,0)
            hm_vals[257] = -59  # matches (1,0) in chunk (1,0)
            hm_path.write_bytes(struct.pack(f"<{len(hm_vals)}i", *hm_vals))

            json_out = Path(tmp) / "out.json"
            with patch.object(ct, "read_chunk_nbt", side_effect=fake_reader):
                out = io.StringIO()
                with contextlib.redirect_stdout(out):
                    rc = ct.main([
                        f"{tmp}/a", "1", "0",
                        "--world-b", f"{tmp}/b",
                        "--port-bin", str(bin_path),
                        "--bin-ox", "0", "--bin-oz", "0", "--bin-n", "2",
                        "--port-hm", str(hm_path), "--hm-name", "OCEAN_FLOOR",
                        "--json-out", str(json_out),
                    ])
            self.assertEqual(rc, 0)
            report = json.loads(json_out.read_text())
        self.assertEqual(report["totals"]["all_air_diffs"], 1)
        self.assertEqual(report["totals"]["consistent_diffs"], 1)
        self.assertEqual(report["totals"]["native_ab_all_diffs"], 1)
        self.assertEqual(report["heightmaps"]["port_vs_native_saved_diff_cols"], 1)
        self.assertEqual(report["heightmaps"]["WORLD_SURFACE_vs_non_air_top_mismatches"], 0)

    def test_multi_chunk_grid_bin_and_incomplete_handling(self):
        blocks_meta = json.loads((ROOT / "assets/blocks_1211.json").read_text())
        stone = next(b for b in blocks_meta if b["name"] == "stone")["minStateId"]

        chunks_a = {
            (0, 0): make_chunk_nbt(0, 0, blocks={(0, -60, 0): ("minecraft:stone", {})}),
            (1, 0): make_chunk_nbt(1, 0, blocks={(16, -60, 0): ("minecraft:diorite", {})}),
            (0, 1): make_chunk_nbt(0, 1, status="minecraft:biomes"),
            (1, 1): make_chunk_nbt(1, 1, blocks={(16, -60, 16): ("minecraft:stone", {})}),
        }

        def fake_reader(world, cx, cz):
            return chunks_a[(cx, cz)]

        with tempfile.TemporaryDirectory() as tmp:
            chunk_cells = 2 * 4096
            grid = [0] * (4 * chunk_cells)
            # (0,0) idx=0 matches stone; (1,0) idx=1 mismatches (stone vs diorite); (1,1) idx=3 matches stone.
            grid[0 * chunk_cells + 4 * 256 + 0] = stone
            grid[1 * chunk_cells + 4 * 256 + 0] = stone
            grid[3 * chunk_cells + 4 * 256 + 0] = stone
            bin_path = Path(tmp) / "dump.bin"
            bin_path.write_bytes(struct.pack(f"<{len(grid)}I", *grid))

            # Without --allow-incomplete, partial chunk (0,1) must fail closed.
            with patch.object(ct, "read_chunk_nbt", side_effect=fake_reader):
                err = io.StringIO()
                with contextlib.redirect_stderr(err):
                    rc_fail = ct.main([tmp, "0", "0", "--n", "2", "--port-bin", str(bin_path)])
            self.assertEqual(rc_fail, 2)
            self.assertIn("minecraft:biomes", err.getvalue())

            # With --allow-incomplete, (0,1) is loudly skipped and 3 complete chunks are triaged.
            json_out = Path(tmp) / "grid.json"
            with patch.object(ct, "read_chunk_nbt", side_effect=fake_reader):
                out = io.StringIO()
                with contextlib.redirect_stdout(out):
                    rc_ok = ct.main([
                        tmp, "0", "0", "--n", "2",
                        "--port-bin", str(bin_path),
                        "--allow-incomplete", "--per-chunk",
                        "--json-out", str(json_out),
                        "--fail-on-diff",
                    ])
            self.assertEqual(rc_ok, 1)
            txt = out.getvalue()
            report = json.loads(json_out.read_text())
        self.assertIn("skipped incomplete chunks (1): (0,1:native_a)", txt)
        self.assertIn("per-chunk scoreboard:", txt)
        self.assertEqual(report["compared_chunks"], 3)
        self.assertEqual(report["totals"]["all_air_diffs"], 1)
        self.assertEqual(len(report["skipped"]), 1)

    def test_multi_chunk_tsv_grid_and_missing_section_fail_closed(self):
        chunks = {
            (0, 0): make_chunk_nbt(0, 0, blocks={(0, -60, 0): ("minecraft:stone", {})}),
            (1, 0): make_chunk_nbt(1, 0, blocks={(16, -60, 0): ("minecraft:stone", {})}),
            (0, 1): make_chunk_nbt(0, 1, blocks={}),
            (1, 1): make_chunk_nbt(1, 1, blocks={}),
        }
        with tempfile.TemporaryDirectory() as tmp:
            tsv_ok = Path(tmp) / "ok.tsv"
            tsv_ok.write_text(
                "# chunk 0,0 min_y -64 height 32 cells 1\n"
                "0\t-60\t0\tminecraft:stone\n"
                "# chunk 1,0 min_y -64 height 32 cells 1\n"
                "16\t-60\t0\tminecraft:granite\n"
                "# chunk 0,1 min_y -64 height 32 cells 0\n"
                "# chunk 1,1 min_y -64 height 32 cells 0\n"
            )
            tsv_missing = Path(tmp) / "missing.tsv"
            tsv_missing.write_text("# chunk 0,0 min_y -64 height 32 cells 0\n")

            with patch.object(ct, "read_chunk_nbt", side_effect=lambda w, cx, cz: chunks[(cx, cz)]):
                out = io.StringIO()
                with contextlib.redirect_stdout(out):
                    rc = ct.main([tmp, "0", "0", "--n", "2", "--port-tsv", str(tsv_ok), "--fail-on-diff"])
                self.assertEqual(rc, 1)
                self.assertIn("grid diff summary: all_air_kinds=1 / 32768", out.getvalue())

                err = io.StringIO()
                with contextlib.redirect_stderr(err):
                    rc_bad = ct.main([tmp, "0", "0", "--n", "2", "--port-tsv", str(tsv_missing)])
                self.assertEqual(rc_bad, 2)

    def test_explicit_window_rejects_native_blocks_it_does_not_cover(self):
        # A nether/end chunk is 256 blocks tall in `dimension_type` while its noise
        # settings generate 0..128. Asking for the generated window must compare all of
        # it and must not silently drop a non-air section above it.
        inside = make_chunk_nbt(
            0, 0, min_y=0, height=208, blocks={(0, 100, 0): ("minecraft:netherrack", {})}
        )
        above = make_chunk_nbt(
            0, 0, min_y=0, height=208, blocks={(0, 200, 0): ("minecraft:netherrack", {})}
        )
        with patch.object(ct, "read_chunk_nbt", return_value=inside):
            chunk = ct.load_native_chunk("world", 0, 0, min_y_override=0, height_override=128)
        self.assertEqual((chunk["min_y"], chunk["height"]), (0, 128))
        self.assertEqual(chunk["cells"][(100 - 0) * 256], "minecraft:netherrack")
        with patch.object(ct, "read_chunk_nbt", return_value=above):
            with self.assertRaises(ct.TriageError) as raised:
                ct.load_native_chunk("world", 0, 0, min_y_override=0, height_override=128)
        self.assertIn("outside the comparison window", str(raised.exception))

    def test_preset_resolves_the_dimension_directory_of_a_native_world(self):
        # A native multi-dimension capture keeps `level.dat` at the root and the
        # Nether/End regions in `DIM-1`/`DIM1`. Reading `<world>/region` for a Nether
        # chunk would read the Overworld's file (or nothing), so the preset has to
        # select the directory before any chunk read.
        with tempfile.TemporaryDirectory() as tmp:
            world = Path(tmp) / "world"
            for rel in ("region", "DIM-1/region", "DIM1/region"):
                (world / rel).mkdir(parents=True)
            (world / "level.dat").write_bytes(b"")
            self.assertEqual(str(world / "DIM-1"), ct.resolve_dimension_world(str(world), preset="nether"))
            self.assertEqual(str(world / "DIM1"), ct.resolve_dimension_world(str(world), preset="end"))
            self.assertEqual(str(world), ct.resolve_dimension_world(str(world), preset="overworld"))
            self.assertEqual(str(world), ct.resolve_dimension_world(str(world)))
            # `--dim` wins over `--preset` and accepts the directory names.
            self.assertEqual(
                str(world / "DIM1"), ct.resolve_dimension_world(str(world), preset="nether", dim="DIM1")
            )
            self.assertEqual(
                str(world), ct.resolve_dimension_world(str(world), preset="nether", dim="overworld")
            )
            with self.assertRaises(ct.TriageError) as raised:
                ct.resolve_dimension_world(str(world), dim="mars")
            self.assertIn("unknown --dim", str(raised.exception))

            # The dimension directory itself keeps working: it has `region/` and no
            # `level.dat`, so the same preset resolves to it unchanged.
            for name, preset in (("DIM-1", "nether"), ("DIM1", "end")):
                self.assertEqual(
                    str(world / name),
                    ct.resolve_dimension_world(str(world / name), preset=preset),
                )

            # A multi-dimension root without the requested dimension must fail closed
            # instead of comparing the Overworld's chunks against the Nether preset.
            bare = Path(tmp) / "bare"
            (bare / "region").mkdir(parents=True)
            (bare / "level.dat").write_bytes(b"")
            with self.assertRaises(ct.TriageError) as raised:
                ct.resolve_dimension_world(str(bare), preset="nether")
            self.assertIn("DIM-1/region", str(raised.exception))
            # A directory that holds `region/` itself is taken at its word: the caller
            # named the dimension directory, which is the pre-existing call shape.
            self.assertEqual(
                str(world / "DIM-1"), ct.resolve_dimension_world(str(world / "DIM-1"), preset="overworld")
            )

    def test_level_dat_is_found_from_a_dimension_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            world = Path(tmp) / "world"
            (world / "DIM-1" / "region").mkdir(parents=True)
            (world / "level.dat").write_bytes(b"")
            self.assertEqual(world / "level.dat", ct.resolve_level_dat(world))
            self.assertEqual(world / "level.dat", ct.resolve_level_dat(world / "DIM-1"))
            # A directory that is not a dimension directory and has no `level.dat`
            # answers None rather than guessing a parent.
            (world / "DIM1" / "region").mkdir(parents=True)
            self.assertEqual(world / "level.dat", ct.resolve_level_dat(world / "DIM1"))
            self.assertIsNone(ct.resolve_level_dat(world / "DIM1" / "region"))

    def test_preset_selects_the_dimension_for_every_native_read(self):
        # Every native read has to come from the resolved directory: the target chunk,
        # and the neighbour start chunks `collect_structure_pieces` follows.
        native = make_chunk_nbt(
            0, 0, min_y=0, height=32,
            blocks={(0, 20, 0): ("minecraft:netherrack", {})},
            references={"minecraft:fortress": [encode_chunk_pos_long(1, 0)]},
        )
        neighbour = make_chunk_nbt(
            1, 0, min_y=0, height=32,
            starts={"minecraft:fortress": {"Children": [{"BB": [16, 10, 0, 24, 20, 8]}]}},
        )
        seen = []

        def fake_reader(world, cx, cz):
            seen.append((world, cx, cz))
            return neighbour if (cx, cz) == (1, 0) else native

        with tempfile.TemporaryDirectory() as tmp:
            world = Path(tmp) / "world"
            for rel in ("region", "DIM-1/region"):
                (world / rel).mkdir(parents=True)
            (world / "level.dat").write_bytes(b"")
            port = Path(tmp) / "port.bin"
            port.write_bytes(struct.pack("<8192I", *([0] * 8192)))
            with patch.object(ct, "read_chunk_nbt", side_effect=fake_reader):
                out = io.StringIO()
                with contextlib.redirect_stdout(out):
                    rc = ct.main([
                        str(world), "0", "0", "--preset", "nether", "--port-bin", str(port),
                        "--min-y", "0", "--height", "32",
                    ])
            self.assertEqual(rc, 0, out.getvalue())
            self.assertTrue(seen)
            dim_dir = str(world / "DIM-1")
            self.assertEqual({w for w, _, _ in seen}, {dim_dir})
            self.assertIn((dim_dir, 1, 0), seen)
            self.assertIn("DIM-1", out.getvalue())

            # Without the preset the same root reads the Overworld's `region/`.
            seen.clear()
            with patch.object(ct, "read_chunk_nbt", side_effect=fake_reader):
                with contextlib.redirect_stdout(io.StringIO()):
                    rc = ct.main([
                        str(world), "0", "0", "--port-bin", str(port),
                        "--min-y", "0", "--height", "32",
                    ])
            self.assertEqual(rc, 0)
            self.assertEqual({w for w, _, _ in seen}, {str(world)})

    def test_out_of_window_blocks_are_rejected_by_default_and_reportable(self):
        # An End City tower reaches y=141 in vanilla while the End's `noise_settings`
        # window stops at 128. The default must refuse to compare a subset; the opt-in
        # reports the exclusion as a counted gap so the window itself is still measured.
        tall = make_chunk_nbt(
            0, 0, min_y=0, height=208,
            blocks={(0, 200, 0): ("minecraft:purpur_block", {}),
                    (1, 200, 0): ("minecraft:purpur_block", {})},
        )
        with patch.object(ct, "read_chunk_nbt", return_value=tall):
            with self.assertRaises(ct.TriageError) as raised:
                ct.load_native_chunk("world", 0, 0, min_y_override=0, height_override=128)
            self.assertIn("outside the comparison window", str(raised.exception))
            self.assertIn("--out-of-window report", str(raised.exception))
            chunk = ct.load_native_chunk("world", 0, 0, min_y_override=0, height_override=128,
                                         out_of_window="report")
        self.assertEqual(chunk["out_of_window"]["cells"], 2)
        self.assertEqual(chunk["out_of_window"]["states"], {"minecraft:purpur_block": 2})
        self.assertEqual(chunk["out_of_window"]["sections"], [12])
        # Air-only sections outside the window are not an exclusion.
        air = make_chunk_nbt(0, 0, min_y=0, height=208)
        with patch.object(ct, "read_chunk_nbt", return_value=air):
            chunk = ct.load_native_chunk("world", 0, 0, min_y_override=0, height_override=128)
        self.assertEqual(chunk["out_of_window"]["cells"], 0)


if __name__ == "__main__":
    unittest.main()
