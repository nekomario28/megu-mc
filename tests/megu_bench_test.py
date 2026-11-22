#!/usr/bin/env python3
"""Unit and real loopback protocol-767 tests for the MEGU self-benchmark."""

import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "tools" / "megu_bench.py"
SPEC = importlib.util.spec_from_file_location("megu_bench", SCRIPT)
bench = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bench)


FAKE_SERVER = r'''#!/usr/bin/env python3
import json
import os
import socket
import sys
import threading


def vint(value):
    out = bytearray()
    while True:
        part = value & 127
        value >>= 7
        out.append(part | (128 if value else 0))
        if not value:
            return bytes(out)


def exact(sock, size):
    content = bytearray()
    while len(content) < size:
        part = sock.recv(size - len(content))
        if not part:
            raise RuntimeError("unexpected EOF")
        content.extend(part)
    return bytes(content)


def read_vint(sock):
    value = 0
    for shift in range(0, 35, 7):
        part = exact(sock, 1)[0]
        value |= (part & 127) << shift
        if not part & 128:
            return value
    raise RuntimeError("invalid VarInt")


def read_packet(sock):
    length = read_vint(sock)
    data = exact(sock, length)
    return data[0], data[1:]


def frame(packet_id, data=b""):
    body = vint(packet_id) + data
    return vint(len(body)) + body


def stop_on_stdin():
    for line in sys.stdin:
        if line.strip().lstrip("/") == "stop":
            with open(os.environ["FAKE_STOP_MARKER"], "ab") as marker:
                marker.write(b"stop\n")
                marker.flush()
            os._exit(0)


port = int(os.environ["MEGU_PORT"])
assert os.environ["MEGU_BIND_ADDR"] == "127.0.0.1"
assert os.environ["TMPDIR"] == os.getcwd()
assert os.environ["TEMP"] == os.getcwd()
threading.Thread(target=stop_on_stdin, daemon=True).start()
with socket.socket() as server:
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", port))
    server.listen(5)
    while True:
        client, _ = server.accept()
        with client:
            client.settimeout(4)
            packet_id, handshake = read_packet(client)
            assert packet_id == 0
            assert handshake[:2] == b"\xff\x05"  # Protocol 767 VarInt.
            packet_id, request = read_packet(client)
            assert packet_id == 0 and request == b""
            response = json.dumps({"version": {"name": "1.21.1", "protocol": 767},
                                   "players": {"max": 20, "online": 0},
                                   "description": {"text": "fake megu test"}}).encode()
            client.sendall(frame(0, vint(len(response)) + response))
            packet_id, payload = read_packet(client)
            assert packet_id == 1 and len(payload) == 8
            client.sendall(frame(1, payload))
'''


class MeguBenchTests(unittest.TestCase):
    def cli(self, *args, env=None):
        return subprocess.run([sys.executable, str(SCRIPT), *map(str, args)],
                              capture_output=True, text=True, timeout=30,
                              env=env, check=False)

    def test_dry_run_json_has_exact_plan_and_no_output_file(self):
        with tempfile.TemporaryDirectory(prefix="megu-plan-test-") as directory:
            fake = Path(directory) / "fake.py"
            fake.write_text("pass\n")
            receipt = Path(directory) / "receipt.json"
            result = self.cli("--binary", fake, "--runs", "2", "--port", "0",
                              "--format", "json", "--dry-run", "--out", receipt)
            self.assertEqual(result.returncode, 0, result.stderr)
            plan = json.loads(result.stdout)
            self.assertEqual(plan["schema"], "megu-bench-plan/1")
            self.assertTrue(plan["dry_run"])
            self.assertEqual(plan["runs"], 2)
            self.assertEqual(plan["pings_per_run"], 10)
            self.assertEqual(plan["protocol"], 767)
            self.assertEqual(plan["command"], [sys.executable, str(fake.resolve())])
            self.assertEqual(plan["environment_overrides"]["MEGU_PORT"],
                             "<ephemeral per run>")
            self.assertFalse(receipt.exists())

    def test_invalid_argument_rejection(self):
        with tempfile.TemporaryDirectory(prefix="megu-args-test-") as directory:
            binary = Path(directory) / "fake.py"
            binary.write_text("pass\n")
            for args in (("--runs", "0"), ("--runs", "99"),
                         ("--port", "65536"), ("--format", "unknown")):
                with self.subTest(args=args):
                    result = self.cli("--binary", binary, "--dry-run", *args)
                    self.assertEqual(result.returncode, 2, result.stderr)
            missing = self.cli("--binary", Path(directory) / "missing", "--dry-run")
            self.assertEqual(missing.returncode, 2)
            no_argument = self.cli("--dry-run")
            self.assertEqual(no_argument.returncode, 2)

    def test_dry_run_never_launches_a_process(self):
        with tempfile.TemporaryDirectory(prefix="megu-no-spawn-test-") as directory:
            binary = Path(directory) / "fake.py"
            binary.write_text("pass\n")
            output = io.StringIO()
            with mock.patch.object(bench.subprocess, "Popen", side_effect=AssertionError("spawn")), \
                 mock.patch.object(bench.subprocess, "run", side_effect=AssertionError("spawn")), \
                 redirect_stdout(output):
                exit_code = bench.main(["--binary", str(binary), "--dry-run", "--format", "json"])
            self.assertEqual(exit_code, 0)
            plan = json.loads(output.getvalue())
            self.assertIn("hardware", plan)
            self.assertIn("ram_mib", plan["hardware"])

    def test_proc_status_parser(self):
        parsed = bench.parse_proc_status("Name:\tfake\nVmRSS:\t  25600 kB\n"
                                         "VmSize:\t60000 kB\nThreads:\t9\n")
        self.assertEqual(parsed, {"rss_mib": 25, "threads": 9})
        self.assertEqual(bench.parse_proc_status("Threads: unknown\nVmRSS: nope kB"),
                         {"rss_mib": None, "threads": None})
        self.assertEqual(bench.parse_proc_status("VmRSS: 42 kB\n"),
                         {"rss_mib": 42 / 1024, "threads": None})

    def test_process_metrics_unavailable_returns_null(self):
        with mock.patch.object(bench.sys, "platform", "win32"):
            self.assertEqual(bench.process_metrics(1234),
                             {"rss_mib": None, "threads": None})

    def test_aggregate_missing_metrics_and_median(self):
        runs = [{name: None for name in bench.METRICS} for _ in range(3)]
        for index, value in enumerate((8, 2, 12)):
            runs[index]["startup_ms"] = value
        measured = bench.aggregate(runs)
        self.assertEqual(measured["startup_ms"], {"min": 2, "median": 8, "max": 12})
        self.assertIsNone(measured["idle_rss_mib"])

    def test_text_markdown_json_renderers(self):
        run = {"startup_ms": 10.25, "idle_rss_mib": 12.0,
               "active_rss_mib": 13.0, "idle_threads": 2,
               "active_threads": 3, "ping_rtt_ms": 0.5}
        receipt = {"schema": bench.SCHEMA, "binary": "/example/server", "runs": [run],
                   "flags": {"MEGU_BIND_ADDR": "127.0.0.1"},
                   "hardware": {"cpu": "Example CPU", "logical_cpus": 4,
                                "ram_mib": 8192.0, "os": "Example OS"},
                   "metrics": bench.aggregate([run]), "note": bench.NOTE}
        output = bench.render_text(receipt)
        self.assertIn("Example CPU", output)
        self.assertIn("8192.00 MiB", output)
        self.assertIn("10.25", output)
        self.assertIn("Status-active RSS", output)
        self.assertIn(bench.NOTE, output)
        markdown = bench.render_markdown(receipt)
        self.assertIn("| Metric | Min | Median | Max |", markdown)
        self.assertIn("**10.25**", markdown)
        self.assertIn(bench.NOTE, markdown)
        parsed = json.loads(bench.render_json(receipt))
        self.assertEqual(parsed["schema"], "megu-bench/1")
        self.assertEqual(parsed["metrics"]["startup_ms"]["median"], 10.25)

    def test_real_loopback_status_ping_and_clean_stop(self):
        with tempfile.TemporaryDirectory(prefix="megu-e2e-test-") as directory:
            root = Path(directory)
            fake = root / "fake_server.py"
            fake.write_text(FAKE_SERVER, encoding="utf-8")
            marker = root / "stopped.marker"
            receipt_path = root / "receipt.json"
            environment = os.environ.copy()
            environment["TMPDIR"] = directory
            environment["FAKE_STOP_MARKER"] = str(marker)
            result = self.cli("--binary", fake, "--runs", "2", "--format", "json",
                              "--out", receipt_path, env=environment)
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads(result.stdout)
            self.assertEqual(report["schema"], "megu-bench/1")
            self.assertEqual(report, json.loads(receipt_path.read_text()))
            self.assertEqual(len(report["runs"]), 2)
            for run in report["runs"]:
                self.assertGreater(run["startup_ms"], 0)
                self.assertEqual(len(run["ping_rtt_samples_ms"]), 10)
                self.assertTrue(all(value > 0 for value in run["ping_rtt_samples_ms"]))
                self.assertGreater(run["ping_rtt_ms"], 0)
                self.assertGreater(run["port"], 0)
                if sys.platform.startswith("linux"):
                    self.assertGreater(run["idle_rss_mib"], 0)
                    self.assertGreater(run["idle_threads"], 0)
                    self.assertGreater(run["active_rss_mib"], 0)
                    self.assertGreater(run["active_threads"], 0)
            self.assertEqual(marker.read_bytes(), b"stop\nstop\n")
            self.assertEqual(list(root.glob("megu-bench-*")), [])
            self.assertEqual(report["flags"]["MEGU_BIND_ADDR"], "127.0.0.1")


if __name__ == "__main__":
    unittest.main()
