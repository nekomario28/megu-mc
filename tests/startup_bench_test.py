#!/usr/bin/env python3
"""Unit tests for tools/startup_bench.py.

Every case is hermetic: the "server" under test is a tiny fake executable
(script on a shebang) that emits canned stderr — a delayed `listening`
banner, `MEGU_STARTUP_PROFILE` stage lines, or nothing at all — so the tests
pin the harness's parsing, statistics and exit contract without needing a
built megu binary.
"""
import importlib.util
import io
import os
import stat
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path

HERE = Path(__file__).resolve()
ROOT = HERE.parents[1]
SCRIPT = ROOT / "tools" / "startup_bench.py"

spec = importlib.util.spec_from_file_location("startup_bench", SCRIPT)
sb = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sb)

FAKE_SERVER = """#!/usr/bin/env python3
import sys, time
print("[startup]     0.100 ms (+  0.100) cli-flags", file=sys.stderr, flush=True)
print("[startup]   parallel block-registry:   15.236 ms", file=sys.stderr, flush=True)
time.sleep(float(sys.argv[1]) if len(sys.argv) > 1 else 0.05)
print("megu v0.6 listening 127.0.0.1:25565 (offline identities; no Mojang authentication)",
      file=sys.stderr, flush=True)
print("[startup]    60.000 ms (+ 59.900) ready", file=sys.stderr, flush=True)
time.sleep(60)
"""

FAKE_SERVER_SILENT = """#!/usr/bin/env python3
import sys, time
print("[spine] booting without a banner", file=sys.stderr, flush=True)
time.sleep(60)
"""


def make_executable(path: Path, body: str) -> Path:
    path.write_text(body)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return path


class ParseStageTest(unittest.TestCase):
    def test_timeline_stage(self):
        line = b"[startup]    12.345 ms (+  3.210) worldgen-init\n"
        self.assertEqual(sb.parse_stage(line), ("worldgen-init", 12.345, 3.210))

    def test_parallel_stage(self):
        line = b"[startup]   parallel block-registry:   15.236 ms\n"
        self.assertEqual(sb.parse_stage(line), ("block-registry", 15.236, None))

    def test_non_stage_line(self):
        self.assertIsNone(sb.parse_stage(b"megu v0.6 listening 127.0.0.1:1 (x)\n"))
        self.assertIsNone(sb.parse_stage(b"[spine] loaded 0 furnaces\n"))


class StatisticsTest(unittest.TestCase):
    def test_percentile_nearest_rank(self):
        self.assertEqual(sb.percentile([10, 20, 30, 40, 50], 95), 50)
        self.assertEqual(sb.percentile([10, 20, 30, 40, 50], 50), 30)
        self.assertIsNone(sb.percentile([], 95))

    def test_summarize_counts_failures(self):
        def sample(ms):
            s = sb.BootSample()
            s.listening_ms = ms
            return s

        report = sb.summarize([sample(10.0), sample(20.0), sample(30.0), sb.BootSample()])
        self.assertEqual(report["runs"], 4)
        self.assertEqual(report["measured"], 3)
        self.assertEqual(report["failures"], 1)
        self.assertEqual(report["median_ms"], 20.0)
        self.assertEqual(report["p95_ms"], 30.0)
        self.assertEqual(report["min_ms"], 10.0)

    def test_summarize_all_failed(self):
        report = sb.summarize([sb.BootSample()])
        self.assertEqual(report["measured"], 0)
        self.assertIsNone(report["median_ms"])


class MeasureOnceTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="startup-bench-test-"))

    def test_records_listening_and_stages(self):
        binary = make_executable(self.tmp / "fake-server", FAKE_SERVER)
        sample = sb.measure_once(binary, self.tmp, dict(os.environ), timeout=10, args=["0.02"])
        self.assertFalse(sample.timed_out)
        self.assertIsNotNone(sample.listening_ms)
        self.assertGreater(sample.listening_ms, 0)
        stages = [s[0] for s in sample.stages]
        self.assertIn("cli-flags", stages)
        self.assertIn("block-registry", stages)

    def test_timeout_marks_unmeasured(self):
        binary = make_executable(self.tmp / "fake-silent", FAKE_SERVER_SILENT)
        sample = sb.measure_once(binary, self.tmp, dict(os.environ), timeout=1.0)
        self.assertTrue(sample.timed_out)
        self.assertIsNone(sample.listening_ms)

    def test_exit_before_banner(self):
        body = "#!/bin/sh\necho 'dying' >&2\nexit 3\n"
        binary = make_executable(self.tmp / "fake-dead", body)
        sample = sb.measure_once(binary, self.tmp, dict(os.environ), timeout=10)
        self.assertFalse(sample.timed_out)
        self.assertIsNone(sample.listening_ms)
        self.assertEqual(sample.exit_code, 3)


class MainContractTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="startup-bench-main-"))
        self.binary = make_executable(self.tmp / "fake-server", FAKE_SERVER)

    def run_main(self, *argv):
        out = io.StringIO()
        with redirect_stdout(out):
            code = sb.main(list(argv))
        return code, out.getvalue()

    def test_json_report(self):
        code, out = self.run_main(
            "--binary", str(self.binary), "--runs", "2",
            "--run-dir", str(self.tmp), "--json",
        )
        self.assertEqual(code, 0)
        import json

        report = json.loads(out)
        self.assertEqual(report["runs"], 2)
        self.assertEqual(report["measured"], 2)
        self.assertIsNotNone(report["median_ms"])
        self.assertEqual(len(report["samples"]), 2)

    def test_max_ms_fails(self):
        code, _ = self.run_main(
            "--binary", str(self.binary), "--runs", "1",
            "--run-dir", str(self.tmp), "--max-ms", "0.000001",
        )
        self.assertEqual(code, 1)

    def test_max_ms_passes(self):
        code, _ = self.run_main(
            "--binary", str(self.binary), "--runs", "1",
            "--run-dir", str(self.tmp), "--max-ms", "60000",
        )
        self.assertEqual(code, 0)

    def test_missing_binary_is_usage_error(self):
        code, _ = self.run_main(
            "--binary", str(self.tmp / "nope"), "--runs", "1",
            "--run-dir", str(self.tmp),
        )
        self.assertEqual(code, 2)

    def test_ensure_run_dir_creates_eula_once(self):
        run_dir = self.tmp / "world"
        run_dir.mkdir()
        sb.ensure_run_dir(run_dir)
        eula = run_dir / "eula.txt"
        self.assertEqual(eula.read_text(), "eula=true\n")
        eula.write_text("eula=false\n")
        sb.ensure_run_dir(run_dir)
        self.assertEqual(eula.read_text(), "eula=false\n")

    def test_free_port(self):
        port = sb.free_port()
        self.assertIsInstance(port, int)
        self.assertTrue(0 < port < 65536)


if __name__ == "__main__":
    unittest.main()
