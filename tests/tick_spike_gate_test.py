#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Unit tests for tools/tick_spike_gate.py — parser and thresholds only.

Every fixture is canned log text fed through a file or stdin; no server
process is ever spawned, keeping this gate inside the fast-gates budget.
"""
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
GATE = ROOT / "tools" / "tick_spike_gate.py"

GOOD = """\
megu v1.21.1 listening 127.0.0.1:25565 (offline identities; no Mojang authentication)
[tick] observed_tps=19.90 avg_work_ms=11.50 max_work_ms=15.77 over_budget=0/100
[tick] observed_tps=20.00 avg_work_ms=9.01 max_work_ms=31.20 over_budget=0/100
[sz] t=500 VmRSS: 524288 kB | mobs=44 drops=8
"""

SPIKE = """\
[tick] observed_tps=19.20 avg_work_ms=40.10 max_work_ms=312.44 over_budget=2/100
"""

SLOW = """\
[tick] observed_tps=14.31 avg_work_ms=70.00 max_work_ms=140.00 over_budget=6/100
"""

CHATTER = """\
megu v1.21.1 listening 127.0.0.1:25565
[spine] Bob joined from 127.0.0.1:51130
[chunkprof] window=60s chunks=1284 (4.2 ms/chunk)
tick without the bracketed prefix is not telemetry
"""


def run(*args, stdin=None):
    return subprocess.run([sys.executable, str(GATE), *map(str, args)],
                          input=stdin, capture_output=True, text=True,
                          check=False)


def log(text):
    handle = tempfile.NamedTemporaryFile(
        "w", suffix=".log", delete=False, encoding="utf-8")
    with handle:
        handle.write(text)
    return handle.name


class TickSpikeGateTests(unittest.TestCase):
    def test_clean_log_passes(self):
        result = run(log(GOOD))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("2 sample(s)", result.stdout)

    def test_work_ms_spike_fails(self):
        result = run(log(GOOD + SPIKE))
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("max_work_ms=312.44", result.stdout)

    def test_low_tps_fails(self):
        result = run(log(GOOD + SLOW))
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("observed_tps=14.31", result.stdout)

    def test_zero_samples_fails_closed(self):
        result = run(log(CHATTER))
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("0 [tick] sample(s)", result.stdout)

    def test_stdin_mode(self):
        result = run("-", stdin=GOOD)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_custom_thresholds_rescue_borderline_samples(self):
        path = log(GOOD + SPIKE)
        strict = run(path)
        self.assertEqual(strict.returncode, 1, strict.stdout + strict.stderr)
        relaxed = run(path, "--max-work-ms-ceiling", "400")
        self.assertEqual(relaxed.returncode, 0,
                         relaxed.stdout + relaxed.stderr)

    def test_min_samples_ratchet(self):
        result = run(log(GOOD), "--min-samples", "5")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("only 2 [tick] sample(s)", result.stdout)

    def test_missing_log_is_usage_error(self):
        result = run("definitely/not/a/log.txt")
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)

    def test_no_arguments_is_usage_error(self):
        result = run()
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)

    def test_malformed_and_partial_lines_are_ignored(self):
        messy = GOOD + (
            "[tick] observed_tps=oops avg_work_ms=1 max_work_ms=2 over_budget=0/1\n"
            "[tick] incomplete=\n")
        result = run(log(messy))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("2 sample(s)", result.stdout)


if __name__ == "__main__":
    unittest.main()
