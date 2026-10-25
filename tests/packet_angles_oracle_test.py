#!/usr/bin/env python3
"""Exact angle-corpus/provenance guards; everyday consumers do not need Java."""
import copy
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
import packet_angles_oracle as oracle


class PacketAnglesReferenceTests(unittest.TestCase):
    def test_complete_cases_and_provenance(self):
        fixture = json.loads(oracle.FIXTURE.read_text())
        self.assertEqual(fixture["minecraft"], "1.21.1")
        self.assertEqual(fixture["server_sha1"], oracle.JAR_SHA1)
        self.assertEqual(fixture["oracle_sha256"], hashlib.sha256(oracle.SOURCE.read_bytes()).hexdigest())
        self.assertEqual([{k: v for k, v in row.items() if k != "expected"}
                          for row in fixture["cases"]], oracle.cases())
        self.assertIn(0x80000001, {r["yaw_bits"] for r in oracle.cases()})
        self.assertEqual({r["yaw_bits"] for r in oracle.cases()},
                         {r["pitch_bits"] for r in oracle.cases()})
        self.assertTrue(any(r["expected"]["spawn"][1] != r["expected"]["teleport"][0]
                            for r in fixture["cases"]), "floor and truncation need distinct coverage")
        self.assertTrue(any(r["expected"]["spawn"][1] != r["expected"]["spawn"][2]
                            for r in fixture["cases"]), "f32 body and f64 head need distinct coverage")
        for row in fixture["cases"]:
            result = row["expected"]
            self.assertEqual(set(result), {"spawn", "teleport", "head_update"})
            self.assertEqual(len(result["spawn"]), 3)
            self.assertEqual(len(result["teleport"]), 2)
            for value in result["spawn"] + result["teleport"] + [result["head_update"]]:
                self.assertIs(type(value), int)
                self.assertTrue(0 <= value <= 255)

    def test_comparison_rejects_empty_truncated_extra_changed_and_stale(self):
        fixture = json.loads(oracle.FIXTURE.read_text())
        oracle.require_equal(fixture, copy.deepcopy(fixture))
        mutations = [lambda v: v.update(cases=[]), lambda v: v["cases"].pop(),
                     lambda v: v["cases"].append(copy.deepcopy(v["cases"][0])),
                     lambda v: v["cases"][0].update(yaw_bits=1),
                     lambda v: v["cases"][0]["expected"].pop("head_update"),
                     lambda v: v["cases"][0]["expected"]["spawn"].append(0),
                     lambda v: v["cases"][0]["expected"].update(head_update=False),
                     lambda v: v.update(oracle_sha256="0" * 64),
                     lambda v: v.update(server_sha1="0" * 40),
                     lambda v: v.update(minecraft="1.20.1")]
        for mutate in mutations:
            changed = copy.deepcopy(fixture)
            mutate(changed)
            with self.assertRaises(ValueError):
                oracle.require_equal(fixture, changed)

    def test_wrong_jar_preserves_output_before_java(self):
        with tempfile.TemporaryDirectory() as tmp:
            jar, output = Path(tmp) / "wrong.jar", Path(tmp) / "output.json"
            jar.write_bytes(b"not the pinned jar")
            output.write_text("keep this")
            result = subprocess.run([sys.executable, str(ROOT / "tools/packet_angles_oracle.py"),
                                     "--jar", str(jar), "--output", str(output)],
                                    capture_output=True, text=True, timeout=10)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("not the pinned 1.21.1 server jar", result.stderr)
            self.assertEqual(output.read_text(), "keep this")


if __name__ == "__main__":
    unittest.main()
