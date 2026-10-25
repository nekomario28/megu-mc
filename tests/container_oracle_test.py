#!/usr/bin/env python3
"""No-JDK guards for oracle coverage, provenance and comparison completeness."""
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
from container_oracle import FIXTURE, SOURCE, cases, require_equal  # noqa: E402
from vanilla_java import JAR_SHA1  # noqa: E402


class ContainerOracleTests(unittest.TestCase):
    def setUp(self):
        self.data = json.loads(FIXTURE.read_text())

    def test_all_requested_cases_have_reference_results(self):
        rows = self.data["cases"]
        self.assertTrue(rows)
        self.assertEqual([{k: v for k, v in row.items() if k != "expected"} for row in rows], cases())
        self.assertEqual(len({row["id"] for row in rows}), len(rows))
        for row in rows:
            self.assertTrue(row["expected"])
            if row["op"] == "insert":
                self.assertEqual(set(row["expected"]), {"target", "source"})
            else:
                self.assertEqual(set(row["expected"]), {"slots"})
        self.assertEqual(self.data["minecraft"], "1.21.1")
        self.assertEqual(self.data["server_sha1"], JAR_SHA1)
        self.assertEqual(self.data["oracle_sha256"], hashlib.sha256(SOURCE.read_bytes()).hexdigest())
        require_equal(self.data, self.data)

    def test_empty_truncated_extended_and_changed_reference_fail(self):
        for fault in ["empty", "tail", "extra", "result", "version"]:
            actual = copy.deepcopy(self.data)
            if fault == "empty":
                actual["cases"] = []
            elif fault == "tail":
                actual["cases"].pop()
            elif fault == "extra":
                actual["cases"].append(actual["cases"][0])
            elif fault == "result":
                actual["cases"][0]["expected"] = {}
            else:
                actual["server_sha1"] = "foreign"
            with self.subTest(fault=fault), self.assertRaises(ValueError):
                require_equal(self.data, actual)
        with self.assertRaises(ValueError):
            require_equal({"cases": []}, {"cases": []})

    def test_wrong_jar_cli_preserves_existing_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            jar, output = Path(tmp) / "wrong.jar", Path(tmp) / "vectors.json"
            jar.write_bytes(b"foreign jar")
            output.write_text("keep this reference\n")
            result = subprocess.run([sys.executable, str(ROOT / "tools/container_oracle.py"),
                                     "--jar", str(jar), "--output", str(output)],
                                    capture_output=True, text=True, timeout=10)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("not the pinned", result.stderr)
            self.assertEqual(output.read_text(), "keep this reference\n")


if __name__ == "__main__":
    unittest.main()
