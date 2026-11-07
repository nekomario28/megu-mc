#!/usr/bin/env python3
"""Fail-closed gate contract tests; no Rust compiler or JAR required."""
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
from native_cargo_gate import NAMES, validate_output


class NativeCargoGateTests(unittest.TestCase):
    def test_all_three_explicit_targets(self):
        self.assertEqual(len(NAMES), 3)

    def test_exactly_one_named_test_and_summary(self):
        for name in NAMES:
            with self.subTest(name=name):
                output = (f"test tick_tests::{name} ... ok\n"
                          "test result: ok. 1 passed; 0 failed; 0 ignored; 396 filtered out\n")
                validate_output(output, name)

    def test_zero_matches_cannot_be_a_success(self):
        for name in NAMES:
            with self.subTest(name=name), self.assertRaises(ValueError):
                validate_output("running 0 tests\n"
                                "test result: ok. 0 passed; 0 failed; 0 ignored; 397 filtered out\n", name)

    def test_other_target_and_ignored_do_not_count(self):
        names = sorted(NAMES)
        output = (f"test tick_tests::{names[0]} ... ok\n"
                  "test result: ok. 1 passed; 0 failed; 0 ignored; 396 filtered out\n")
        with self.assertRaises(ValueError):
            validate_output(output, names[1])
        for name in NAMES:
            with self.assertRaises(ValueError):
                validate_output(
                    f"test tick_tests::{name} ... ignored\n"
                    "test result: ok. 0 passed; 0 failed; 1 ignored; 396 filtered out\n",
                    name,
                )

    def test_one_named_line_without_correct_summary_is_not_enough(self):
        name = next(iter(NAMES))
        with self.assertRaises(ValueError):
            validate_output(f"test tick_tests::{name} ... ok\n"
                            "test result: ok. 2 passed; 0 failed; 0 ignored; 395 filtered out\n", name)


if __name__ == "__main__":
    unittest.main()
