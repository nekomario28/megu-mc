#!/usr/bin/env python3
"""The receipt producer must reject the shapes that produced a false green.

Measured 2026-11-16 in a readiness run: `Executable` was never parsed, so `"" --list`
exited 127 with an empty binary and empty SHA, `tests_total=0`, and the run still left
a green-looking receipt. These cases pin each part of that shape, plus a stale source
tree and a zero-test list, as FAIL; the matching-source case is the positive control.
"""
from pathlib import Path
import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parents[1]
TOOL = ROOT / "tools" / "evidence_receipt.py"
sys.path.insert(0, str(ROOT / "tools"))
from cargo_executable import executable, test_executable


class CargoResolverTests(unittest.TestCase):
    """`cargo test --no-run` reports the harness with test:true; `cargo build` does not."""

    def lines(self, harness):
        return [json.dumps({"reason": "compiler-artifact",
                            "target": {"name": "mc-spine", "kind": ["bin"]},
                            "profile": {"test": harness},
                            "executable": str(Path(__file__).resolve())})]

    def test_both_harness_kinds_resolve_and_cross_do_not(self):
        self.assertEqual(test_executable(self.lines(True)), Path(__file__).resolve())
        self.assertEqual(executable(self.lines(False)), Path(__file__).resolve())
        with self.assertRaises(ValueError):
            executable(self.lines(True))
        with self.assertRaises(ValueError):
            test_executable(self.lines(False))


class ReceiptProducerTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="spine-receipt-")
        self.root = Path(self.tmp.name)
        subprocess.run(["git", "init", "-q"], cwd=self.root, check=True)
        subprocess.run(["git", "config", "user.email", "t@example.invalid"], cwd=self.root, check=True)
        subprocess.run(["git", "config", "user.name", "t"], cwd=self.root, check=True)
        (self.root / "src").write_text("x\n")
        subprocess.run(["git", "add", "src"], cwd=self.root, check=True)
        subprocess.run(["git", "commit", "-qm", "base"], cwd=self.root, check=True)
        self.head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=self.root,
                                   capture_output=True, text=True, check=True).stdout.strip()
        self.binary = self.root / "fake-mc-spine"
        self.binary.write_text(textwrap.dedent("""\
            #!/bin/sh
            case "$FAKE_BIN_MODE" in
              list127) exit 127 ;;
              zero) echo "0 tests, 0 benchmarks"; exit 0 ;;
              ok) if [ "$1" = --list ]; then
                    echo "feature::tests::a: test"
                    echo "feature::tests::b: test"
                    echo "2 tests, 0 benchmarks"
                  else
                    echo "test feature::tests::a ... ok"
                    echo "test result: ok. 1 passed; 0 failed; 0 ignored; 1 filtered out"
                  fi; exit 0 ;;
              red) echo "test feature::tests::a ... FAILED"
                   echo "test result: FAILED. 0 passed; 1 failed; 0 ignored"; exit 101 ;;
            esac
            """))
        self.binary.chmod(0o755)
        self.cargo = self.root / "fake-cargo"
        self.cargo.write_text(textwrap.dedent(f"""\
            #!/bin/sh
            if [ "$FAKE_CARGO_MODE" = noexec ]; then
              echo '{{"reason":"build-finished","success":true}}'
              exit 0
            fi
            printf '%s\\n' '{{"reason":"compiler-artifact","target":{{"name":"mc-spine","kind":["bin"]}},"profile":{{"test":true}},"executable":"{self.binary}"}}'
            """))
        self.cargo.chmod(0o755)

    def tearDown(self):
        self.tmp.cleanup()

    def produce(self, expect_head=None, cargo_mode="", bin_mode="ok", out="out"):
        env = dict(os.environ, FAKE_CARGO_MODE=cargo_mode, FAKE_BIN_MODE=bin_mode)
        result = subprocess.run(
            [sys.executable, str(TOOL), "--root", str(self.root), "--out", str(self.root / out),
             "--expect-head", expect_head or self.head, "--cargo", str(self.cargo),
             "--filter", "feature::tests::a"], env=env, capture_output=True, text=True)
        receipt = json.loads((self.root / out / "receipt.json").read_text())
        return result, receipt

    def test_matching_source_and_one_running_test_passes(self):
        result, receipt = self.produce()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(receipt["status"], "PASS")
        self.assertEqual(receipt["tests_total"], 2)
        self.assertGreater(receipt["binary"]["bytes"], 0)
        self.assertEqual(len(receipt["binary"]["sha256"]), 64)

    def test_missing_executable_shape_is_rejected(self):
        # No compiler-artifact executable at all: the observed `bin=` / `sha256=` shape.
        result, receipt = self.produce(cargo_mode="noexec")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(receipt["status"], "FAIL")
        self.assertNotIn("binary", receipt)
        self.assertTrue(any("no unique Cargo executable" in p for p in receipt["problems"]))

    def test_list_exit_127_and_zero_tests_is_rejected(self):
        result, receipt = self.produce(bin_mode="list127")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(receipt["status"], "FAIL")
        self.assertEqual(receipt["tests_total"], 0)
        self.assertTrue(any("--list exited 127" in p for p in receipt["problems"]))
        self.assertTrue(any("zero tests" in p for p in receipt["problems"]))

    def test_list_with_zero_tests_is_rejected(self):
        result, receipt = self.produce(bin_mode="zero")
        self.assertEqual(result.returncode, 1)
        self.assertTrue(any("zero tests" in p for p in receipt["problems"]))

    def test_a_red_selected_filter_is_rejected(self):
        result, receipt = self.produce(bin_mode="red")
        self.assertEqual(result.returncode, 1)
        self.assertTrue(any("exited 101" in p for p in receipt["problems"]))

    def test_stale_source_head_is_rejected_before_it_can_certify(self):
        result, receipt = self.produce(expect_head="0" * 40)
        self.assertEqual(result.returncode, 1)
        self.assertTrue(any("stale source" in p for p in receipt["problems"]))
        self.assertEqual(receipt["status"], "FAIL")


if __name__ == "__main__":
    unittest.main()
