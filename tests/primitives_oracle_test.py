#!/usr/bin/env python3
"""Offline corpus identity, coverage and fail-closed primitive-oracle guards."""
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
import primitives_oracle as oracle


class PrimitivesReferenceTests(unittest.TestCase):
    def test_complete_cases_and_provenance(self):
        fixture = json.loads(oracle.FIXTURE.read_text())
        self.assertEqual(fixture["minecraft"], "1.21.1")
        self.assertEqual(fixture["server_sha1"], oracle.JAR_SHA1)
        self.assertEqual(fixture["oracle_sha256"], hashlib.sha256(oracle.SOURCE.read_bytes()).hexdigest())
        oracle.validate(fixture["cases"], oracle.cases())
        self.assertEqual([{k: v for k, v in row.items() if k != "expected"}
                          for row in fixture["cases"]], oracle.cases())
        sections = [r for r in fixture["cases"] if r["kind"] == "section"]
        for n, bits in [(1, 0), (16, 4), (17, 5), (256, 8), (257, 15), (4096, 15)]:
            self.assertTrue(any(len(r["states"]) == n and r["expected"]["block_bits"] == bits
                                for r in sections), (n, bits))
        for n, bits in [(1, 0), (8, 3), (9, 6), (64, 6)]:
            self.assertTrue(any(len(r["biomes"]) == n and r["expected"]["biome_bits"] == bits
                                for r in sections), (n, bits))
        for state in [12958, 12959]:
            self.assertTrue(any(r["states"] == [state] and r["expected"]["non_air"] == 0
                                for r in sections))
        clips = [r for r in fixture["cases"] if r["kind"] == "clip"]
        self.assertTrue(any(r["expected"]["hit_bits"] for r in clips))
        self.assertTrue(any(not r["expected"]["hit_bits"] for r in clips))
        for units in [[0], [0xd83d, 0xdca3], [0xd800], [0xdc00]]:
            self.assertTrue(any(r["kind"] == "string" and r["units"] == units
                                for r in fixture["cases"]))

    def test_comparison_rejects_empty_truncated_extra_changed_and_stale(self):
        fixture = json.loads(oracle.FIXTURE.read_text())
        oracle.require_equal(fixture, copy.deepcopy(fixture))
        mutations = [lambda v: v.update(cases=[]), lambda v: v["cases"].pop(),
                     lambda v: v["cases"].append(copy.deepcopy(v["cases"][0])),
                     lambda v: v["cases"][0].update(states=[1]),
                     lambda v: v["cases"][0]["expected"].pop("sha1"),
                     lambda v: v["cases"][0]["expected"].update(extra=0),
                     lambda v: v["cases"][0]["expected"].update(non_air=False),
                     lambda v: v.update(oracle_sha256="0" * 64),
                     lambda v: v.update(server_sha1="0" * 40),
                     lambda v: v.update(minecraft="1.20.1")]
        for mutate in mutations:
            changed = copy.deepcopy(fixture)
            mutate(changed)
            with self.assertRaises(ValueError):
                oracle.require_equal(fixture, changed)

    def test_validation_rejects_missing_results_and_bool_integers(self):
        rows = json.loads(oracle.FIXTURE.read_text())["cases"]
        for field, value in [("length", False), ("sha1", "not a hash"), ("extra", 0)]:
            changed = copy.deepcopy(rows)
            changed[0]["expected"][field] = value
            with self.assertRaises(ValueError):
                oracle.validate(changed, oracle.cases())
        with self.assertRaises(ValueError):
            oracle.validate([], oracle.cases())

    def test_wrong_jar_preserves_output_before_java(self):
        with tempfile.TemporaryDirectory() as tmp:
            jar, output = Path(tmp) / "wrong.jar", Path(tmp) / "output.json"
            jar.write_bytes(b"not the pinned jar")
            output.write_text("keep this")
            result = subprocess.run([sys.executable, str(ROOT / "tools/primitives_oracle.py"),
                                     "--jar", str(jar), "--output", str(output)],
                                    capture_output=True, text=True, timeout=10)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("not the pinned 1.21.1 server jar", result.stderr)
            self.assertEqual(output.read_text(), "keep this")


if __name__ == "__main__":
    unittest.main()
