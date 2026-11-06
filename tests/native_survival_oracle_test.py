#!/usr/bin/env python3
"""Portable negative controls for full native canSurvive matrix validation."""
from pathlib import Path
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
from native_support_inventory import inventory
from native_survival_oracle import CONTEXTS, canonical, validate_matrix


class SurvivalMatrixGuardTest(unittest.TestCase):
    def setUp(self):
        self.states = list(inventory(scope="default"))[:2]
        self.rows = [
            f"{sid}\t{scene}\tT\t{canonical(entry)}\t"
            for sid, entry in self.states for scene in CONTEXTS
        ]

    def check(self, rows, strict=True):
        return validate_matrix((row + "\n" for row in rows), self.states, strict)

    def test_complete_matrix(self):
        self.assertEqual(self.check(self.rows), (len(CONTEXTS) * 2, 0))

    def test_missing_or_reordered_rows(self):
        for rows in (self.rows[:-1], self.rows[::-1]):
            with self.assertRaises(ValueError):
                self.check(rows)

    def test_unsupported_native_call_is_not_counted_as_pass(self):
        rows = list(self.rows)
        rows[0] = rows[0].replace("\tT\t", "\tU:UnsupportedOperationException\t")
        with self.assertRaises(ValueError):
            self.check(rows)
        self.assertEqual(self.check(rows, strict=False)[1], 1)

    def test_wrong_native_state_identity_is_rejected(self):
        rows = list(self.rows)
        rows[0] = rows[0].replace(canonical(self.states[0][1]), "minecraft:invalid_oracle_state")
        with self.assertRaises(ValueError):
            self.check(rows)

    def test_malformed_native_read_trace_is_rejected(self):
        rows = list(self.rows)
        rows[0] += "not-a-position"
        with self.assertRaises(ValueError):
            self.check(rows)

    def test_python_context_order_matches_java(self):
        import re
        source = (ROOT / "tools/structure-oracle/NativeSurvivalOracle.java").read_text()
        java_names = re.findall(r'new Context\("([^"]+)"', source)
        self.assertEqual(tuple(java_names), CONTEXTS)


if __name__ == "__main__":
    unittest.main()
