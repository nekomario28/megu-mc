#!/usr/bin/env python3
"""Unit tests for the stdlib-only real-binary playability gate."""

import tempfile
import unittest
from pathlib import Path
import zipfile
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from tools import ci_playability_gate as gate


class PlayabilityGateTest(unittest.TestCase):
    def test_synthetic_mod_creation_is_deterministic_and_multi_mod(self):
        with tempfile.TemporaryDirectory() as tmp:
            mods = Path(tmp) / "game" / "mods"
            gate.write_synthetic_mods(mods)
            first = {path.name: path.read_bytes() for path in sorted(mods.iterdir())}
            gate.write_synthetic_mods(mods)
            second = {path.name: path.read_bytes() for path in sorted(mods.iterdir())}
        self.assertEqual(first, second)
        self.assertEqual(set(first), {"mini_create_addon.jar", "mini_food.jar"})

    def test_synthetic_jars_are_valid_zips_with_expected_members(self):
        with tempfile.TemporaryDirectory() as tmp:
            mods = Path(tmp) / "mods"
            gate.write_synthetic_mods(mods)
            with zipfile.ZipFile(mods / "mini_create_addon.jar") as jar:
                self.assertTrue(jar.testzip() is None)
                self.assertEqual(
                    set(jar.namelist()),
                    {
                        "fabric.mod.json",
                        "assets/mini_create/blockstates/mini_shaft.json",
                        "assets/mini_create/textures/block/mini_shaft.png",
                        "data/mini_create/recipes/pressing/plate.json",
                    },
                )
                texture = jar.read("assets/mini_create/textures/block/mini_shaft.png")
                self.assertTrue(texture.startswith(b"\x89PNG\r\n\x1a\n"))
            with zipfile.ZipFile(mods / "mini_food.jar") as jar:
                self.assertIsNone(jar.testzip())
                self.assertIn("assets/mini_food/lang/en_us.json", jar.namelist())
                self.assertIn("assets/mini_food/lang/ja_jp.json", jar.namelist())
                self.assertIn("data/mini_food/recipes/cooking.json", jar.namelist())

    def test_missing_binary_fails_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaisesRegex(gate.GateError, "missing megu-client binary"):
                gate.require_binary(Path(tmp) / "missing", "megu-client")

    def test_nonzero_command_fails_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            script = Path(tmp) / "fail.py"
            script.write_text("raise SystemExit(7)\n", encoding="utf-8")
            with self.assertRaisesRegex(gate.GateError, "exited 7, expected 0"):
                gate.run_checked(
                    [__import__("sys").executable, script], label="synthetic failing command"
                )

    def test_clean_room_and_full_fixture_mode_selection(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.assertFalse(gate.has_full_fixtures(root))
            (root / "config_packets.bin").write_bytes(b"fixture")
            (root / "assets").mkdir()
            (root / "assets/block_states_1211.json").write_text("{}", encoding="utf-8")
            self.assertTrue(gate.has_full_fixtures(root))


if __name__ == "__main__":
    unittest.main(verbosity=2)
