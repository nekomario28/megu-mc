#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Multiplayer wire stress test for megu.

Launches an megu server on an ephemeral loopback port, connects N headless
megu-client bots concurrently, and fail-closed asserts on every client's
JSON summary plus server health. Prints a JSON summary to stdout.

Usage:
    python3 tests/multiplayer_stress_wire.py [server_binary] [--clients N]
        [--ticks N] [--compression-threshold N] [--client-bin PATH]
        [--build] [--self-test]
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_SERVER_BIN = "target/debug/megu"
DEFAULT_CLIENT_BIN = ROOT / "client-app/target/release/megu-client"
SCRIPTS = ("walk", "jump", "sprint", "survey")
SERVER_READY_TIMEOUT_S = 30.0
CLIENTS_TIMEOUT_S = 120.0
SERVER_STOP_TIMEOUT_S = 10.0
MIN_CHUNKS_DECODED = 25
VIEW_DISTANCE = 2


class StressFailure(Exception):
    pass


def log(msg: str) -> None:
    print(f"[stress] {msg}", file=sys.stderr, flush=True)


# ---------------------------------------------------------------- CLI ----

def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="multiplayer_stress_wire.py",
        description="Concurrent headless-client wire stress test for megu.",
    )
    p.add_argument("server_binary", nargs="?", default=DEFAULT_SERVER_BIN,
                   help=f"megu server binary (default: {DEFAULT_SERVER_BIN})")
    p.add_argument("--clients", type=int, default=8, help="concurrent clients (default: 8)")
    p.add_argument("--ticks", type=int, default=40, help="ticks per client (default: 40)")
    p.add_argument("--compression-threshold", type=int, default=64,
                   help="MEGU_COMPRESSION_THRESHOLD; negative leaves it unset (default: 64)")
    p.add_argument("--client-bin", type=Path, default=None,
                   help=f"megu-client binary (default: {DEFAULT_CLIENT_BIN})")
    p.add_argument("--build", action="store_true", help="force rebuilding the client")
    p.add_argument("--self-test", action="store_true", help="run offline self-tests and exit")
    return p


def parse_args(argv: Optional[Sequence[str]] = None) -> argparse.Namespace:
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.self_test:
        return args
    if args.clients < 1:
        parser.error("--clients must be >= 1")
    if args.ticks < 1:
        parser.error("--ticks must be >= 1")
    return args


def resolve_path(path: Path | str) -> Path:
    p = Path(path)
    return p if p.is_absolute() else ROOT / p


def script_for(index: int) -> str:
    return SCRIPTS[index % len(SCRIPTS)]


# ---------------------------------------------------------- /proc parse --

def parse_proc_stat(text: str, clk_tck: int) -> float:
    """Return user+sys CPU seconds from a /proc/<pid>/stat line."""
    rparen = text.rfind(")")
    if rparen < 0:
        raise ValueError("malformed /proc stat: missing ')'")
    fields = text[rparen + 1:].split()
    # fields[0] is field 3 (state); utime is field 14, stime field 15.
    if len(fields) < 13:
        raise ValueError("malformed /proc stat: too few fields")
    if clk_tck <= 0:
        raise ValueError("invalid clock ticks")
    utime = int(fields[11])
    stime = int(fields[12])
    return (utime + stime) / clk_tck


def parse_proc_status(text: str) -> Dict[str, Optional[float]]:
    """Return {'rss_mib', 'threads'} from /proc/<pid>/status text."""
    rss_mib: Optional[float] = None
    threads: Optional[int] = None
    for line in text.splitlines():
        key, _, value = line.partition(":")
        parts = value.split()
        if key == "VmRSS" and parts:
            kib = int(parts[0])
            unit = parts[1].lower() if len(parts) > 1 else "kb"
            if unit != "kb":
                raise ValueError(f"unexpected VmRSS unit {parts[1]!r}")
            rss_mib = kib / 1024.0
        elif key == "Threads" and parts:
            threads = int(parts[0])
    return {"rss_mib": rss_mib, "threads": threads}


def sample_process(pid: int) -> Dict[str, Optional[float]]:
    sample: Dict[str, Optional[float]] = {"cpu_s": None, "rss_mib": None, "threads": None}
    try:
        stat = Path(f"/proc/{pid}/stat").read_text()
        sample["cpu_s"] = parse_proc_stat(stat, os.sysconf("SC_CLK_TCK"))
    except (OSError, ValueError) as exc:
        log(f"warning: cannot sample /proc/{pid}/stat: {exc}")
    try:
        sample.update(parse_proc_status(Path(f"/proc/{pid}/status").read_text()))
    except (OSError, ValueError) as exc:
        log(f"warning: cannot sample /proc/{pid}/status: {exc}")
    return sample


# ---------------------------------------------------------- assertions --

def extract_json_summary(stdout: str) -> Optional[Dict[str, Any]]:
    """Find the client's JSON summary object (last parseable JSON-object line)."""
    for line in reversed(stdout.splitlines()):
        line = line.strip()
        if line.startswith("{") and line.endswith("}"):
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(obj, dict):
                return obj
    try:
        obj = json.loads(stdout)
    except json.JSONDecodeError:
        return None
    return obj if isinstance(obj, dict) else None


def _is_int(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def validate_client(name: str, exit_code: Optional[int],
                    summary: Optional[Dict[str, Any]]) -> List[str]:
    """Return a list of assertion failures (empty means pass). Fail-closed."""
    failures: List[str] = []
    if exit_code != 0:
        failures.append(f"{name}: exit code {exit_code!r} != 0")
    if summary is None:
        failures.append(f"{name}: missing or unparseable JSON summary")
        return failures
    for key in ("login_ok", "config_ok", "play_ok"):
        if summary.get(key) is not True:
            failures.append(f"{name}: {key} = {summary.get(key)!r}, expected True")
    ints: Dict[str, int] = {}
    for key in ("chunks_decoded", "total_quads", "packed_vertices", "corrections_0x40"):
        value = summary.get(key)
        if not _is_int(value):
            failures.append(f"{name}: {key} = {value!r}, expected integer")
        else:
            ints[key] = value
    if "chunks_decoded" in ints and ints["chunks_decoded"] < MIN_CHUNKS_DECODED:
        failures.append(
            f"{name}: chunks_decoded = {ints['chunks_decoded']} < {MIN_CHUNKS_DECODED}")
    if "total_quads" in ints and ints["total_quads"] <= 0:
        failures.append(f"{name}: total_quads = {ints['total_quads']}, expected > 0")
    if "total_quads" in ints and "packed_vertices" in ints \
            and ints["packed_vertices"] != ints["total_quads"] * 4:
        failures.append(
            f"{name}: packed_vertices = {ints['packed_vertices']} != "
            f"total_quads * 4 = {ints['total_quads'] * 4}")
    if "corrections_0x40" in ints and ints["corrections_0x40"] != 0:
        failures.append(f"{name}: corrections_0x40 = {ints['corrections_0x40']}, expected 0")
    return failures


def validate_server_log(text: str) -> List[str]:
    if "panicked at" in text:
        line = next((ln for ln in text.splitlines() if "panicked at" in ln), "")
        return [f"server: panic detected: {line.strip()}"]
    return []


def aggregate(args: argparse.Namespace, per_client: List[Dict[str, Any]],
              server_sample: Dict[str, Optional[float]], wall_s: float) -> Dict[str, Any]:
    def total(key: str) -> int:
        return sum(int(c.get(key) or 0) for c in per_client)

    rss = server_sample.get("rss_mib")
    cpu = server_sample.get("cpu_s")
    return {
        "clients": args.clients,
        "ticks": args.ticks,
        "compression_threshold": args.compression_threshold,
        "total_chunks_decoded": total("chunks_decoded"),
        "total_quads": total("total_quads"),
        "total_packed_vertices": total("packed_vertices"),
        "max_corrections_0x40": max((int(c.get("corrections_0x40") or 0) for c in per_client),
                                    default=0),
        "server_cpu_s": round(cpu, 3) if cpu is not None else None,
        "server_rss_mib": round(rss, 1) if rss is not None else None,
        "server_threads": server_sample.get("threads"),
        "wall_s": round(wall_s, 3),
        "per_client": per_client,
    }


# ------------------------------------------------------------ runtime --

def server_env(port: int, compression_threshold: int,
               base: Optional[Dict[str, str]] = None) -> Dict[str, str]:
    env = dict(os.environ if base is None else base)
    env["MEGU_PORT"] = str(port)
    env["MEGU_BIND_ADDR"] = "127.0.0.1"
    env["MEGU_SEED"] = "0"
    env["MEGU_RESOURCE_PACK_URL"] = ""
    if compression_threshold >= 0:
        env["MEGU_COMPRESSION_THRESHOLD"] = str(compression_threshold)
    else:
        env.pop("MEGU_COMPRESSION_THRESHOLD", None)
    return env


def client_argv(client_bin: Path, index: int, port: int, ticks: int) -> List[str]:
    return [
        str(client_bin),
        "--username", f"Bot{index}",
        "--headless",
        "--quickPlayMultiplayer", f"127.0.0.1:{port}",
        "--ticks", str(ticks),
        "--view-distance", str(VIEW_DISTANCE),
        "--script", script_for(index),
        "--json-summary",
    ]


def pick_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def make_world_dir() -> Path:
    tmpdir = os.environ.get("TMPDIR")
    base = Path(tmpdir) if tmpdir else ROOT / "target/tmp"
    base.mkdir(parents=True, exist_ok=True)
    world = Path(tempfile.mkdtemp(prefix="mp-stress-wire-", dir=str(base)))
    for name in ("assets", "datapacks"):
        src = ROOT / name
        if not src.exists():
            raise StressFailure(f"required directory missing: {src}")
        (world / name).symlink_to(src)
    cfg = ROOT / "config_packets.bin"
    if cfg.exists():
        (world / "config_packets.bin").symlink_to(cfg)
    return world


def ensure_client(client_bin: Path, force_build: bool) -> Path:
    if force_build or not client_bin.exists():
        cmd = ["cargo", "build", "--manifest-path", "client-app/Cargo.toml",
               "--release", "--offline", "--locked", "-j", "2"]
        log("building client: " + " ".join(cmd))
        rc = subprocess.run(cmd, cwd=ROOT, stdout=sys.stderr).returncode
        if rc != 0:
            raise StressFailure(f"client build failed with exit code {rc}")
    if not client_bin.exists():
        raise StressFailure(f"client binary not found: {client_bin}")
    return client_bin


class OutputPump:
    """Drains a pipe in a background thread, signalling on a readiness token."""

    def __init__(self, stream, token: str, ready: threading.Event) -> None:
        self.lines: List[str] = []
        self._lock = threading.Lock()
        self._thread = threading.Thread(target=self._run, args=(stream, token, ready),
                                        daemon=True)
        self._thread.start()

    def _run(self, stream, token: str, ready: threading.Event) -> None:
        for raw in iter(stream.readline, b""):
            line = raw.decode("utf-8", errors="replace")
            with self._lock:
                self.lines.append(line)
            if token in line.lower():
                ready.set()
        stream.close()

    def text(self) -> str:
        with self._lock:
            return "".join(self.lines)

    def join(self, timeout: float) -> None:
        self._thread.join(timeout)


def wait_server_ready(proc: subprocess.Popen, port: int, ready: threading.Event) -> None:
    deadline = time.monotonic() + SERVER_READY_TIMEOUT_S
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise StressFailure(f"server exited early with code {proc.returncode}")
        if ready.is_set():
            return
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.5):
                return
        except OSError:
            pass
        ready.wait(0.2)
    raise StressFailure(f"server not ready within {SERVER_READY_TIMEOUT_S:.0f}s")


def stop_server(proc: subprocess.Popen) -> Optional[int]:
    if proc.poll() is not None:
        return proc.returncode
    proc.send_signal(signal.SIGTERM)
    try:
        return proc.wait(timeout=SERVER_STOP_TIMEOUT_S)
    except subprocess.TimeoutExpired:
        log("server ignored SIGTERM; sending SIGKILL")
        proc.kill()
        return proc.wait(timeout=SERVER_STOP_TIMEOUT_S)


def run_clients(client_bin: Path, port: int, args: argparse.Namespace,
                world: Path) -> List[Dict[str, Any]]:
    procs = []
    for i in range(args.clients):
        out = open(world / f"Bot{i}.stdout", "wb")
        err = open(world / f"Bot{i}.stderr", "wb")
        p = subprocess.Popen(client_argv(client_bin, i, port, args.ticks), cwd=ROOT,
                             stdin=subprocess.DEVNULL, stdout=out, stderr=err)
        procs.append((i, p, out, err, time.monotonic()))

    deadline = time.monotonic() + CLIENTS_TIMEOUT_S
    results: List[Dict[str, Any]] = []
    for i, p, out, err, started in procs:
        timed_out = False
        try:
            p.wait(timeout=max(0.0, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            timed_out = True
            p.kill()
            p.wait()
        elapsed = time.monotonic() - started
        out.close()
        err.close()
        stdout = (world / f"Bot{i}.stdout").read_text(errors="replace")
        results.append({
            "name": f"Bot{i}",
            "script": script_for(i),
            "exit_code": None if timed_out else p.returncode,
            "timed_out": timed_out,
            "elapsed_s": round(elapsed, 3),
            "summary": extract_json_summary(stdout),
        })
    return results


def run(args: argparse.Namespace) -> int:
    server_bin = resolve_path(args.server_binary)
    if not server_bin.exists():
        raise StressFailure(f"server binary not found: {server_bin}")
    client_bin = ensure_client(
        resolve_path(args.client_bin) if args.client_bin else DEFAULT_CLIENT_BIN, args.build)

    world = make_world_dir()
    port = pick_port()
    log(f"world={world} port={port} clients={args.clients} ticks={args.ticks} "
        f"compression_threshold={args.compression_threshold}")

    ready = threading.Event()
    start = time.monotonic()
    proc = subprocess.Popen([str(server_bin)], cwd=world,
                            env=server_env(port, args.compression_threshold),
                            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE)
    out_pump = OutputPump(proc.stdout, "listening", ready)
    err_pump = OutputPump(proc.stderr, "listening", ready)
    failures: List[str] = []
    results: List[Dict[str, Any]] = []
    sample: Dict[str, Optional[float]] = {"cpu_s": None, "rss_mib": None, "threads": None}
    try:
        wait_server_ready(proc, port, ready)
        log("server ready")
        results = run_clients(client_bin, port, args, world)
        if proc.poll() is None:
            sample = sample_process(proc.pid)
        else:
            failures.append(f"server exited during run with code {proc.returncode}")
    finally:
        stop_server(proc)
        out_pump.join(5.0)
        err_pump.join(5.0)
    wall_s = time.monotonic() - start

    server_log = err_pump.text() + out_pump.text()
    (world / "server.stderr").write_text(err_pump.text())
    (world / "server.stdout").write_text(out_pump.text())

    per_client: List[Dict[str, Any]] = []
    for r in results:
        if r["timed_out"]:
            failures.append(f"{r['name']}: timed out after {CLIENTS_TIMEOUT_S:.0f}s")
        failures.extend(validate_client(r["name"], r["exit_code"], r["summary"]))
        s = r["summary"] or {}
        per_client.append({
            "name": r["name"],
            "script": r["script"],
            "exit_code": r["exit_code"],
            "elapsed_s": r["elapsed_s"],
            **{k: s.get(k) for k in ("login_ok", "config_ok", "play_ok", "chunks_decoded",
                                     "total_quads", "packed_vertices", "corrections_0x40")},
        })
    if len(results) != args.clients:
        failures.append(f"expected {args.clients} client results, got {len(results)}")
    failures.extend(validate_server_log(server_log))

    if failures:
        for f in failures:
            log(f"FAIL {f}")
        log(f"artifacts kept in {world}")
        return 1

    print(json.dumps(aggregate(args, per_client, sample, wall_s), indent=2, sort_keys=False))
    shutil.rmtree(world, ignore_errors=True)
    return 0


# ---------------------------------------------------------- self-test --

def self_test() -> int:
    import unittest

    good = {"login_ok": True, "config_ok": True, "play_ok": True, "chunks_decoded": 25,
            "total_quads": 100, "packed_vertices": 400, "corrections_0x40": 0}

    class T(unittest.TestCase):
        def test_cli_defaults(self):
            a = parse_args([])
            self.assertEqual(a.server_binary, DEFAULT_SERVER_BIN)
            self.assertEqual((a.clients, a.ticks, a.compression_threshold), (8, 40, 64))
            self.assertIsNone(a.client_bin)
            self.assertFalse(a.build)
            self.assertFalse(a.self_test)

        def test_cli_overrides(self):
            a = parse_args(["/x/srv", "--clients", "3", "--ticks", "7",
                            "--compression-threshold", "-1", "--client-bin", "/y/cli",
                            "--build"])
            self.assertEqual(a.server_binary, "/x/srv")
            self.assertEqual((a.clients, a.ticks, a.compression_threshold), (3, 7, -1))
            self.assertEqual(a.client_bin, Path("/y/cli"))
            self.assertTrue(a.build)

        def test_cli_rejects_bad_values(self):
            import contextlib
            import io
            for argv in (["--clients", "0"], ["--ticks", "0"], ["--clients", "x"]):
                with contextlib.redirect_stderr(io.StringIO()), \
                        self.assertRaises(SystemExit):
                    parse_args(argv)

        def test_client_argv_and_scripts(self):
            self.assertEqual([script_for(i) for i in range(5)],
                             ["walk", "jump", "sprint", "survey", "walk"])
            argv = client_argv(Path("/c"), 2, 12345, 40)
            self.assertEqual(argv[argv.index("--username") + 1], "Bot2")
            self.assertEqual(argv[argv.index("--script") + 1], "sprint")
            self.assertEqual(argv[argv.index("--quickPlayMultiplayer") + 1],
                             "127.0.0.1:12345")
            self.assertEqual(argv[argv.index("--view-distance") + 1], "2")
            self.assertIn("--headless", argv)
            self.assertIn("--json-summary", argv)

        def test_server_env(self):
            env = server_env(4242, 64, {"MEGU_COMPRESSION_THRESHOLD": "9"})
            self.assertEqual(env["MEGU_PORT"], "4242")
            self.assertEqual(env["MEGU_BIND_ADDR"], "127.0.0.1")
            self.assertEqual(env["MEGU_SEED"], "0")
            self.assertEqual(env["MEGU_RESOURCE_PACK_URL"], "")
            self.assertEqual(env["MEGU_COMPRESSION_THRESHOLD"], "64")
            env = server_env(4242, -1, {"MEGU_COMPRESSION_THRESHOLD": "9"})
            self.assertNotIn("MEGU_COMPRESSION_THRESHOLD", env)
            self.assertEqual(server_env(1, 0, {})["MEGU_COMPRESSION_THRESHOLD"], "0")

        def test_proc_stat(self):
            line = ("1234 (mc spine) (x)) S 1 1234 1234 0 -1 4194560 100 0 0 0 "
                    "250 150 0 0 20 0 9 0 5000 1000000 500 18446744073709551615")
            self.assertAlmostEqual(parse_proc_stat(line, 100), 4.0)
            with self.assertRaises(ValueError):
                parse_proc_stat("garbage", 100)
            with self.assertRaises(ValueError):
                parse_proc_stat("1 (a) S 1 2", 100)

        def test_proc_status(self):
            text = "Name:\tmegu\nVmRSS:\t  204800 kB\nThreads:\t17\n"
            self.assertEqual(parse_proc_status(text), {"rss_mib": 200.0, "threads": 17})
            self.assertEqual(parse_proc_status("Name: x\n"),
                             {"rss_mib": None, "threads": None})

        def test_sample_self(self):
            if Path("/proc/self/stat").exists():
                s = sample_process(os.getpid())
                self.assertIsNotNone(s["cpu_s"])
                self.assertGreater(s["rss_mib"], 0)
                self.assertGreaterEqual(s["threads"], 1)

        def test_extract_summary(self):
            out = "log line\n{not json}\n" + json.dumps(good) + "\n"
            self.assertEqual(extract_json_summary(out), good)
            self.assertEqual(extract_json_summary(json.dumps(good, indent=2)), good)
            self.assertIsNone(extract_json_summary("no summary here\n"))
            self.assertIsNone(extract_json_summary("[1, 2]"))

        def test_validate_pass(self):
            self.assertEqual(validate_client("Bot0", 0, dict(good)), [])

        def test_validate_failures(self):
            cases = [
                (1, {}, "exit code"),
                (None, {}, "exit code"),
                (0, {"login_ok": False}, "login_ok"),
                (0, {"config_ok": 1}, "config_ok"),
                (0, {"play_ok": None}, "play_ok"),
                (0, {"chunks_decoded": 24}, "chunks_decoded"),
                (0, {"chunks_decoded": True}, "chunks_decoded"),
                (0, {"total_quads": 0, "packed_vertices": 0}, "total_quads"),
                (0, {"packed_vertices": 399}, "packed_vertices"),
                (0, {"corrections_0x40": 1}, "corrections_0x40"),
                (0, {"corrections_0x40": "0"}, "corrections_0x40"),
            ]
            for rc, patch, needle in cases:
                summary = dict(good, **patch)
                fails = validate_client("Bot1", rc, summary)
                self.assertTrue(any(needle in f for f in fails), (rc, patch, fails))
            for key in good:
                summary = dict(good)
                del summary[key]
                self.assertTrue(validate_client("Bot1", 0, summary), key)
            self.assertTrue(validate_client("Bot1", 0, None))

        def test_validate_server_log(self):
            self.assertEqual(validate_server_log("listening on 127.0.0.1:1\n"), [])
            fails = validate_server_log("ok\nthread 'main' panicked at src/x.rs:1:1:\n")
            self.assertEqual(len(fails), 1)
            self.assertIn("panicked at", fails[0])

        def test_aggregate(self):
            a = parse_args(["--clients", "2"])
            pc = [dict(good, name="Bot0"),
                  dict(good, name="Bot1", chunks_decoded=30, corrections_0x40=0)]
            s = aggregate(a, pc, {"cpu_s": 1.23456, "rss_mib": 99.99, "threads": 5}, 2.5)
            self.assertEqual(s["total_chunks_decoded"], 55)
            self.assertEqual(s["total_quads"], 200)
            self.assertEqual(s["total_packed_vertices"], 800)
            self.assertEqual(s["max_corrections_0x40"], 0)
            self.assertEqual((s["server_cpu_s"], s["server_rss_mib"], s["server_threads"]),
                             (1.235, 100.0, 5))
            self.assertEqual(set(s), {"clients", "ticks", "compression_threshold",
                                      "total_chunks_decoded", "total_quads",
                                      "total_packed_vertices", "max_corrections_0x40",
                                      "server_cpu_s", "server_rss_mib", "server_threads",
                                      "wall_s", "per_client"})
            json.dumps(s)

        def test_pick_port(self):
            port = pick_port()
            self.assertTrue(0 < port < 65536)

    suite = unittest.defaultTestLoader.loadTestsFromTestCase(T)
    result = unittest.TextTestRunner(stream=sys.stderr, verbosity=2).run(suite)
    return 0 if result.wasSuccessful() else 1


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = parse_args(argv)
    if args.self_test:
        return self_test()
    try:
        return run(args)
    except StressFailure as exc:
        log(f"FAIL {exc}")
        return 1


if __name__ == "__main__":
    sys.exit(main())
