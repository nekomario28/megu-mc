#!/usr/bin/env python3
"""A nested '1 passed' must not hide the terminal 194/24/1 migration result."""
from pathlib import Path
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
from suite_compare import summary, compare


def log(passed, failures):
    return ("test result: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out\n"
            + ("\nfailures:\n\n" + "".join(f"    {name}\n" for name in failures) if failures else "")
            + f"\ntest result: {'FAILED' if failures else 'ok'}. {passed} passed; {len(failures)} failed; 1 ignored; 0 filtered out\n")


class SuiteCompareTests(unittest.TestCase):
    def test_last_result_not_nested_child(self):
        names = [f"area::test_{i:02}" for i in range(24)]
        result = summary(log(194, names))
        self.assertEqual((result["passed"], result["failed"], result["ignored"]), (194, 24, 1))
        self.assertEqual(result["failures"], names)

    def test_incomplete_and_duplicate_rosters_are_not_results(self):
        for text in ["", log(3, ["a", "b"]).replace("    b\n", ""),
                     log(3, ["a", "a"])]:
            with self.assertRaises(ValueError):
                summary(text)

    def test_same_count_different_failure_is_not_same_baseline(self):
        result = compare(log(188, ["a"]), log(194, ["b"]), 6)
        self.assertFalse(result["unchanged_failure_baseline"])
        self.assertEqual(result["added_failures"], ["b"])

    def test_exact_delta_does_not_claim_green_suite(self):
        result = compare(log(188, ["a"]), log(194, ["a"]), 6)
        self.assertTrue(result["unchanged_failure_baseline"])
        self.assertEqual(result["current"]["failed"], 1)
        self.assertFalse(compare(log(188, ["a"]), log(193, ["a"]), 6)["unchanged_failure_baseline"])


if __name__ == "__main__":
    unittest.main()
