#!/usr/bin/env python3
"""Everyday fixture provenance/completeness checks; no Java installation required."""
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
import surface_oracle as oracle


class SurfaceReferenceTests(unittest.TestCase):
    def test_exact_cases_and_provenance(self):
        fixture = json.loads(oracle.FIXTURE.read_text())
        self.assertEqual(fixture["minecraft"], "1.21.1")
        self.assertEqual(fixture["server_sha1"], oracle.JAR_SHA1)
        self.assertEqual(fixture["oracle_sha256"], hashlib.sha256(oracle.SOURCE.read_bytes()).hexdigest())
        self.assertEqual(fixture["settings_sha256"], hashlib.sha256(oracle.SETTINGS.read_bytes()).hexdigest())
        self.assertEqual([{k: v for k, v in r.items() if k != "expected"}
                          for r in fixture["cases"]], oracle.cases())
        shapes = {"surface_rule": {"decoded"}, "state": {"state_id"},
                  "simplex": {"value_bits"}, "temperature": {"temperature_bits", "cold"}}
        for row in fixture["cases"]:
            self.assertEqual(set(row["expected"]), shapes[row["op"]])
        self.assertTrue(fixture["cases"][0]["expected"]["decoded"])
        temps = [r for r in fixture["cases"] if r["op"] == "temperature"]
        self.assertEqual({r["expected"]["cold"] for r in temps}, {True, False})
        self.assertEqual({r["frozen"] for r in temps}, {True, False})

    def test_comparator_rejects_missing_extra_changed_inputs_and_results(self):
        fixture = json.loads(oracle.FIXTURE.read_text())
        oracle.require_equal(fixture, copy.deepcopy(fixture))
        bad = [copy.deepcopy(fixture) for _ in range(9)]
        bad[0]["cases"] = []
        bad[1]["cases"].pop()
        bad[2]["cases"].append(copy.deepcopy(fixture["cases"][0]))
        bad[3]["cases"][0]["expected"]["decoded"] = False
        bad[4]["cases"][1]["state"]["Name"] = "minecraft:dirt"
        bad[5]["minecraft"] = "26.3"
        bad[6]["oracle_sha256"] = "0" * 64
        bad[7]["settings_sha256"] = "0" * 64
        bad[8]["cases"][-1]["expected"].pop("cold")
        for changed in bad:
            with self.subTest(changed=changed["cases"][:1]):
                with self.assertRaises(ValueError):
                    oracle.require_equal(fixture, changed)

    def test_wrong_jar_fails_before_java_and_preserves_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            jar, output = Path(tmp) / "bad.jar", Path(tmp) / "output.json"
            jar.write_bytes(b"not the pinned jar")
            output.write_text("keep this")
            result = subprocess.run([sys.executable, str(ROOT / "tools/surface_oracle.py"),
                                     "--jar", str(jar), "--output", str(output)],
                                    capture_output=True, text=True, timeout=10)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("not the pinned 1.21.1 server jar", result.stderr)
            self.assertEqual(output.read_text(), "keep this")


if __name__ == "__main__":
    unittest.main()
