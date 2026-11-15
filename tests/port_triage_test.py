#!/usr/bin/env python3
"""Contracts for tools/port_triage.py: level.dat seed/stage guards, clean env, and grid execution."""
import contextlib
import gzip
import importlib.util
import io
import json
import os
from pathlib import Path
import struct
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests"))
from chunk_triage_test import make_chunk_nbt  # noqa: E402

pt_spec = importlib.util.spec_from_file_location("port_triage", ROOT / "tools/port_triage.py")
pt = importlib.util.module_from_spec(pt_spec)
pt_spec.loader.exec_module(pt)


def _nbt_str(s):
    raw = s.encode("utf-8")
    return struct.pack(">H", len(raw)) + raw


def make_level_dat(seed=4242, generate_features=1, enabled_packs=("vanilla",)):
    # Minimal big-endian NBT: Compound "" { Compound "Data" { Compound "WorldGenSettings" { Long "seed", Byte "generate_features" }, Compound "DataPacks" { List<String> "Enabled" } } }
    wgs = (
        b"\x04" + _nbt_str("seed") + struct.pack(">q", seed)
        + b"\x01" + _nbt_str("generate_features") + struct.pack(">b", generate_features)
        + b"\x00"
    )
    pack_items = b"".join(_nbt_str(p) for p in enabled_packs)
    dp = (
        b"\x09" + _nbt_str("Enabled") + b"\x08" + struct.pack(">i", len(enabled_packs)) + pack_items
        + b"\x00"
    )
    data = (
        b"\x0a" + _nbt_str("WorldGenSettings") + wgs
        + b"\x0a" + _nbt_str("DataPacks") + dp
        + b"\x00"
    )
    root = b"\x0a" + _nbt_str("") + b"\x0a" + _nbt_str("Data") + data + b"\x00"
    return gzip.compress(root)


class PortTriageContracts(unittest.TestCase):
    def test_level_dat_seed_and_stage_guards_fail_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            world = Path(tmp) / "world"
            world.mkdir()
            (world / "level.dat").write_bytes(
                make_level_dat(seed=4242, generate_features=1, enabled_packs=("vanilla", "file/nofeat"))
            )
            native = make_chunk_nbt(0, 0)
            with patch.object(pt.chunk_triage, "read_chunk_nbt", return_value=native):
                # file/nofeat + --mode full must fail closed before building.
                err = io.StringIO()
                with contextlib.redirect_stderr(err):
                    rc = pt.main([str(world), "0", "0"])
                self.assertEqual(rc, 2)
                self.assertIn("nofeat", err.getvalue())

                # Explicit seed mismatch must fail closed.
                err = io.StringIO()
                with contextlib.redirect_stderr(err):
                    rc = pt.main([str(world), "0", "0", "--mode", "carvers", "--seed", "7"])
                self.assertEqual(rc, 2)
                self.assertIn("seed", err.getvalue())

                # No level.dat and no --seed must fail closed.
                world_nolvl = Path(tmp) / "nolvl"
                world_nolvl.mkdir()
                err = io.StringIO()
                with contextlib.redirect_stderr(err):
                    rc = pt.main([str(world_nolvl), "0", "0"])
                self.assertEqual(rc, 2)
                self.assertIn("auto-detect seed", err.getvalue())

    def test_preflight_incomplete_chunk_stops_before_build(self):
        with tempfile.TemporaryDirectory() as tmp:
            world = Path(tmp) / "world"
            world.mkdir()
            (world / "level.dat").write_bytes(make_level_dat(seed=0))
            partial = make_chunk_nbt(0, 0, status="minecraft:biomes")
            with patch.object(pt.chunk_triage, "read_chunk_nbt", return_value=partial), \
                 patch.object(pt, "resolve_binary", side_effect=AssertionError("must not build before preflight")):
                err = io.StringIO()
                with contextlib.redirect_stderr(err):
                    rc = pt.main([str(world), "0", "0"])
            self.assertEqual(rc, 2)
            self.assertIn("minecraft:biomes", err.getvalue())

    def test_end_to_end_fake_binary_clean_env_grid_and_provenance(self):
        blocks_meta = json.loads((ROOT / "assets/blocks_1211.json").read_text())
        stone = next(b for b in blocks_meta if b["name"] == "stone")["minStateId"]
        granite = next(b for b in blocks_meta if b["name"] == "granite")["minStateId"]

        chunks = {
            (0, 0): make_chunk_nbt(0, 0, blocks={(0, -60, 0): ("minecraft:stone", {})}, world_surface_val="auto"),
            (1, 0): make_chunk_nbt(1, 0, blocks={(16, -60, 0): ("minecraft:granite", {})}, world_surface_val="auto"),
            (0, 1): make_chunk_nbt(0, 1, world_surface_val="auto"),
            (1, 1): make_chunk_nbt(1, 1, world_surface_val="auto"),
        }
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            world = tmp_path / "world"
            world.mkdir()
            (world / "level.dat").write_bytes(make_level_dat(seed=4242))
            pop_order = tmp_path / "order.tsv"
            pop_order.write_text("0\t0\n")

            fake_bin = tmp_path / "fake-mc-spine"
            env_log = tmp_path / "captured_env.json"
            fake_bin.write_text(f'''#!{sys.executable}
import json, os, struct
from pathlib import Path
env = dict(os.environ)
Path({str(env_log)!r}).write_text(json.dumps({{k: v for k, v in env.items() if k.startswith(("MC_SPINE_", "FSEED_", "FEATURE_"))}}))
n = int(env["MC_SPINE_WG_DUMP_N"])
chunk_cells = 2 * 4096
grid = [0] * (n * n * chunk_cells)
# Match (0,0) stone and mismatch (1,0) stone vs native granite
grid[0 * chunk_cells + 4 * 256 + 0] = {stone}
grid[1 * chunk_cells + 4 * 256 + 0] = {stone}
Path(env["MC_SPINE_WG_DUMP"]).write_bytes(struct.pack(f"<{{len(grid)}}I", *grid))
if "MC_SPINE_WG_DUMP_HM" in env:
    hm = [-64] * (n * n * 256)
    hm[0] = -59  # matches (0,0) top + 1
    Path(env["MC_SPINE_WG_DUMP_HM"]).write_bytes(struct.pack(f"<{{len(hm)}}i", *hm))
if "MC_SPINE_WG_DUMP_CANON" in env:
    Path(env["MC_SPINE_WG_DUMP_CANON"]).write_text("# chunk 0,0 min_y -64 height 32 cells 0\\n")
''')
            fake_bin.chmod(0o755)

            out_dir = tmp_path / "artifacts"
            json_out = tmp_path / "report.json"

            # Pollute caller env with stale MC_SPINE_* / FSEED_* vars to verify stripping.
            with patch.dict(os.environ, {"MC_SPINE_NOFEAT": "1", "FSEED_ARM": "0,0,0,0", "FEATURE_TRACE": "1"}), \
                 patch.object(pt.chunk_triage, "read_chunk_nbt", side_effect=lambda w, cx, cz: chunks[(cx, cz)]):
                out = io.StringIO()
                with contextlib.redirect_stdout(out):
                    rc = pt.main([
                        str(world), "0", "0", "--n", "2",
                        "--binary", str(fake_bin),
                        # the synthetic chunk is two sections tall; the port-side window
                        # is pinned to it instead of the preset's 384-block window
                        "--min-y", "-64", "--height", "32",
                        "--mode", "terrain", "--noaq",
                        "--pop-order", str(pop_order), "--pop-order-scope", "recorded",
                        "--port-hm", "--canon",
                        "--out-dir", str(out_dir),
                        "--json-out", str(json_out),
                        "--fail-on-diff",
                    ])
            self.assertEqual(rc, 1)
            captured = json.loads(env_log.read_text())
            self.assertEqual(captured["MC_SPINE_SEED"], "4242")
            self.assertEqual(captured["MC_SPINE_WG_PROFILE"], "0")
            self.assertEqual(captured["MC_SPINE_NOFEAT"], "1")
            self.assertEqual(captured["MC_SPINE_NOCARVE"], "1")
            self.assertEqual(captured["MC_SPINE_NOAQ"], "1")
            self.assertEqual(captured["MC_SPINE_POP_ORDER_SCOPE"], "recorded")
            self.assertNotIn("FSEED_ARM", captured)
            self.assertNotIn("FEATURE_TRACE", captured)
            self.assertTrue((out_dir / "dump.bin").is_file())
            self.assertTrue((out_dir / "hm.bin").is_file())
            self.assertTrue((out_dir / "canon.tsv").is_file())
            report = json.loads(json_out.read_text())
            self.assertEqual(report["port_provenance"]["seed"], 4242)
            self.assertEqual(report["port_provenance"]["mode"], "terrain")
            self.assertEqual(report["totals"]["all_air_diffs"], 1)

    def test_pop_order_accepts_sculk_trace_and_refuses_game_log(self):
        blocks_meta = json.loads((ROOT / "assets/blocks_1211.json").read_text())
        stone = next(b for b in blocks_meta if b["name"] == "stone")["minStateId"]
        chunk = make_chunk_nbt(0, 0, blocks={(0, -60, 0): ("minecraft:stone", {})}, world_surface_val="auto")
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            world = tmp_path / "world"
            world.mkdir()
            (world / "level.dat").write_bytes(make_level_dat(seed=4242))

            # Ordered SculkLog trace with POP_CHUNK and SPASS (POP_CHUNK preferred, dedup preserving order)
            trace = tmp_path / "sculk-trace.txt"
            trace.write_text(
                "[w=Worker-Main-1] SDRAW_SCOPE pass=all\n"
                "[w=Worker-Main-1] POP_CHUNK 1 0\n"
                "[w=Worker-Main-1] SPASS 1 0\n"
                "[w=Worker-Main-2] POP_CHUNK 0 0\n"
                "[w=Worker-Main-1] POP_CHUNK 1 0\n"
            )
            fake_bin = tmp_path / "fake-mc-spine"
            captured_order = tmp_path / "captured_order.txt"
            fake_bin.write_text(f'''#!{sys.executable}
import os, struct
from pathlib import Path
env = dict(os.environ)
Path({str(captured_order)!r}).write_text(Path(env["MC_SPINE_POP_ORDER"]).read_text())
grid = [0] * (2 * 4096)
grid[4 * 256 + 0] = {stone}
Path(env["MC_SPINE_WG_DUMP"]).write_bytes(struct.pack(f"<{{len(grid)}}I", *grid))
''')
            fake_bin.chmod(0o755)
            out_dir = tmp_path / "out"
            with patch.object(pt.chunk_triage, "read_chunk_nbt", return_value=chunk):
                out = io.StringIO()
                with contextlib.redirect_stdout(out):
                    rc = pt.main([
                        str(world), "0", "0",
                        "--binary", str(fake_bin),
                        "--min-y", "-64", "--height", "32",
                        "--pop-order", str(trace),
                        "--pop-order-scope", "recorded",
                        "--out-dir", str(out_dir),
                    ])
            self.assertEqual(rc, 0)
            self.assertEqual(captured_order.read_text(), "1\t0\n0\t0\n")
            self.assertEqual((out_dir / "pop_order.tsv").read_text(), "1\t0\n0\t0\n")

            # Unordered log4j game log must fail closed before building.
            game_log = tmp_path / "run.log"
            game_log.write_text("[12:00:00] [Worker-Main-1/INFO]: [STDOUT]: DECO 0 0\n")
            with patch.object(pt.chunk_triage, "read_chunk_nbt", return_value=chunk), \
                 patch.object(pt, "resolve_binary", side_effect=AssertionError("must not build on unordered pop-order")):
                err = io.StringIO()
                with contextlib.redirect_stderr(err):
                    rc = pt.main([str(world), "0", "0", "--pop-order", str(game_log)])
            self.assertEqual(rc, 2)
            self.assertIn("not program order", err.getvalue())

            # Empty / markerless trace must fail closed before building.
            empty_trace = tmp_path / "empty.txt"
            empty_trace.write_text("[w=Worker-Main-1] SDRAW_SCOPE pass=all\n")
            with patch.object(pt.chunk_triage, "read_chunk_nbt", return_value=chunk), \
                 patch.object(pt, "resolve_binary", side_effect=AssertionError("must not build on empty pop-order")):
                err = io.StringIO()
                with contextlib.redirect_stderr(err):
                    rc = pt.main([str(world), "0", "0", "--pop-order", str(empty_trace)])
            self.assertEqual(rc, 2)
            self.assertIn("POP_CHUNK/SPASS/DECO", err.getvalue())

    def test_full_mode_nether_uses_the_noise_settings_window(self):
        # `--preset nether --mode full` used to be refused outright. It now decorates,
        # and the comparison window is the preset's `noise_settings` window (0..128),
        # not an inference from which sections this native chunk happens to fill: the
        # chunk below carries `yPos=0` and only 4 non-empty sections.
        native = make_chunk_nbt(
            0, 0, min_y=0, height=64,
            blocks={(0, 60, 0): ("minecraft:netherrack", {})},
            biome="minecraft:nether_wastes",
        )
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            world = tmp_path / "world"
            world.mkdir()
            (world / "level.dat").write_bytes(make_level_dat(seed=4242))
            # A native multi-dimension capture keeps the Nether's regions in
            # `DIM-1/`, not in the root's `region/`.
            (world / "DIM-1" / "region").mkdir(parents=True)
            env_log = tmp_path / "env.json"
            fake_bin = tmp_path / "fake-mc-spine"
            # 8 sections of all-air for the 0..128 window the port generates.
            fake_bin.write_text(f'''#!{sys.executable}
import json, os, struct
from pathlib import Path
env = dict(os.environ)
Path({str(env_log)!r}).write_text(json.dumps(env))
n = int(env["MC_SPINE_WG_DUMP_N"])
Path(env["MC_SPINE_WG_DUMP"]).write_bytes(struct.pack(f"<{{n * 8 * 4096}}I", *([0] * (n * 8 * 4096))))
''')
            fake_bin.chmod(0o755)
            with patch.object(pt.chunk_triage, "read_chunk_nbt", return_value=native):
                out = io.StringIO()
                with contextlib.redirect_stdout(out):
                    rc = pt.main([
                        str(world), "0", "0",
                        "--preset", "nether", "--mode", "full",
                        "--binary", str(fake_bin),
                    ])
            self.assertEqual(rc, 0, out.getvalue())
            env = json.loads(env_log.read_text())
            self.assertEqual(env["MC_SPINE_WG_PRESET"], "nether")
            # full decoration: neither the feature layer nor the carver pass is skipped
            self.assertNotIn("MC_SPINE_NOFEAT", env)
            self.assertNotIn("MC_SPINE_NOCARVE", env)

            # A native block above the generated window must fail closed instead of
            # being silently left out of the comparison.
            tall = make_chunk_nbt(
                0, 0, min_y=0, height=208,
                blocks={(0, 200, 0): ("minecraft:netherrack", {})},
                biome="minecraft:nether_wastes",
            )
            with patch.object(pt.chunk_triage, "read_chunk_nbt", return_value=tall), \
                 patch.object(pt, "resolve_binary", side_effect=AssertionError("must not build")):
                err = io.StringIO()
                with contextlib.redirect_stderr(err):
                    rc = pt.main([
                        str(world), "0", "0",
                        "--preset", "nether", "--mode", "full",
                    ])
            self.assertEqual(rc, 2)
            self.assertIn("outside the comparison window", err.getvalue())

    def test_preset_window_defaults_and_overrides(self):
        for preset, expected in (("overworld", (-64, 384)), ("nether", (0, 128)), ("end", (0, 128))):
            self.assertEqual(pt.PRESET_NOISE_WINDOW[preset], expected)
        chunk = make_chunk_nbt(0, 0, blocks={(0, -60, 0): ("minecraft:stone", {})})
        with tempfile.TemporaryDirectory() as tmp:
            world = Path(tmp) / "world"
            world.mkdir()
            (world / "level.dat").write_bytes(make_level_dat(seed=4242))
            with patch.object(pt.chunk_triage, "read_chunk_nbt", return_value=chunk):
                for overrides, message in (
                    (["--height", "12"], "--height"),
                    (["--min-y", "5"], "--min-y"),
                ):
                    err = io.StringIO()
                    with contextlib.redirect_stderr(err):
                        rc = pt.main([str(world), "0", "0", *overrides])
                    self.assertEqual(rc, 2)
                    self.assertIn(message, err.getvalue())

    def test_nether_preset_reads_dim_minus_one_and_its_root_level_dat(self):
        # A native Nether capture has `world/level.dat` plus `world/DIM-1/region`;
        # the root's `region/` is the Overworld. The preflight has to resolve the
        # dimension directory *and* still read the seed from the root's level.dat,
        # otherwise the port dumps the Overworld seed against Nether chunks.
        native = make_chunk_nbt(
            0, 0, min_y=0, height=32,
            blocks={(0, 20, 0): ("minecraft:netherrack", {})},
            biome="minecraft:nether_wastes",
        )
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            world = tmp_path / "world"
            for rel in ("region", "DIM-1/region"):
                (world / rel).mkdir(parents=True)
            (world / "level.dat").write_bytes(make_level_dat(seed=4242))
            args = pt.build_parser().parse_args(
                [str(world), "0", "0", "--preset", "nether", "--binary", str(tmp_path / "missing")]
            )
            with patch.object(pt.chunk_triage, "read_chunk_nbt", return_value=native):
                seed, meta = pt.preflight(args)
            self.assertEqual(seed, 4242)
            self.assertEqual(meta["path"], str(world / "level.dat"))
            self.assertEqual(args.world, str(world / "DIM-1"))

            # The dimension directory itself stays a valid argument.
            args = pt.build_parser().parse_args(
                [str(world / "DIM-1"), "0", "0", "--preset", "nether", "--binary", str(tmp_path / "missing")]
            )
            with patch.object(pt.chunk_triage, "read_chunk_nbt", return_value=native):
                seed, meta = pt.preflight(args)
            self.assertEqual(seed, 4242)
            self.assertEqual(args.world, str(world / "DIM-1"))

            # A root without the requested dimension fails closed before any build.
            (world / "DIM-1" / "region").rmdir()
            args = pt.build_parser().parse_args(
                [str(world), "0", "0", "--preset", "nether", "--binary", str(tmp_path / "missing")]
            )
            with self.assertRaises(pt.PortTriageError) as raised:
                pt.preflight(args)
            self.assertIn("DIM-1/region", str(raised.exception))

    def test_out_of_window_flag_defaults_to_reject(self):
        # The End City capture has 273 native cells above the End noise window; the
        # default stays fail-closed and the report mode is an explicit opt-in.
        self.assertEqual(
            pt.build_parser().parse_args(["w", "0", "0"]).out_of_window, "reject")
        self.assertEqual(
            pt.build_parser().parse_args(["w", "0", "0", "--out-of-window", "report"]).out_of_window,
            "report")


if __name__ == "__main__":
    unittest.main()
