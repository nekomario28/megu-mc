#!/usr/bin/env python3
"""Contracts for tools/chunk_triage.py: air-kind fidelity, fail-closed bounds, and structure attribution."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import struct
import sys
import tempfile
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
        snowy_true = grass["minStateId"]   # snowy=true
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


if __name__ == "__main__":
    unittest.main()
