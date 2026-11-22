#!/usr/bin/env python3
"""Measure an isolated, loopback-only Minecraft protocol-767 server.

Only a status request and ten status pings are generated, with no players or
gameplay workload. Each run has an empty disposable server working directory.
"""

from __future__ import annotations

import argparse
import ctypes
import json
import os
from pathlib import Path
import platform
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import time


SCHEMA = "megu-bench/1"
PROTOCOL = 767
PING_COUNT = 10
STARTUP_TIMEOUT_S = 90
NOTE = "Measured on loopback with datapack-free/configured startup; compare like-for-like."
METRICS = {
    "startup_ms": "Startup to verified Pong (ms)",
    "idle_rss_mib": "Idle RSS (MiB)",
    "active_rss_mib": "Status-active RSS (MiB)",
    "idle_threads": "Idle threads",
    "active_threads": "Status-active threads",
    "ping_rtt_ms": "Status Ping RTT (ms)",
}


class BenchmarkError(Exception):
    """A real run could not obtain trustworthy measurements."""


def positive_runs(value: str) -> int:
    try:
        count = int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("--runs must be an integer from 1 to 10") from error
    if not 1 <= count <= 10:
        raise argparse.ArgumentTypeError("--runs must be an integer from 1 to 10")
    return count


def valid_port(value: str) -> int:
    try:
        port = int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("--port must be in 0..65535") from error
    if not 0 <= port <= 65535:
        raise argparse.ArgumentTypeError("--port must be in 0..65535")
    return port


def binary_command(binary: Path) -> list[str]:
    # Python fixture executables also work on Windows, which has no shebang support.
    if binary.suffix.lower() == ".py":
        return [sys.executable, str(binary)]
    return [str(binary)]


def hardware_context() -> dict:
    cpu = platform.machine() or "unknown"
    ram_bytes = None
    if sys.platform.startswith("linux"):
        try:
            for line in Path("/proc/cpuinfo").read_text().splitlines():
                if line.startswith("model name"):
                    cpu = line.partition(":")[2].strip()
                    break
            for line in Path("/proc/meminfo").read_text().splitlines():
                if line.startswith("MemTotal:"):
                    ram_bytes = int(line.split()[1]) * 1024
                    break
        except (OSError, ValueError, IndexError):
            pass
    elif sys.platform == "darwin":
        # sysctlbyname avoids launching a process, including during --dry-run.
        try:
            libc = ctypes.CDLL(None)
            model = ctypes.create_string_buffer(256)
            model_size = ctypes.c_size_t(ctypes.sizeof(model))
            if libc.sysctlbyname(b"machdep.cpu.brand_string", model,
                                 ctypes.byref(model_size), None, 0) == 0:
                cpu = model.value.decode("utf-8", errors="replace")
            memory = ctypes.c_uint64()
            memory_size = ctypes.c_size_t(ctypes.sizeof(memory))
            if libc.sysctlbyname(b"hw.memsize", ctypes.byref(memory),
                                 ctypes.byref(memory_size), None, 0) == 0:
                ram_bytes = memory.value
        except (AttributeError, OSError):
            pass
    elif os.name == "nt":
        try:
            class MemoryStatus(ctypes.Structure):
                _fields_ = [("length", ctypes.c_ulong), ("memory_load", ctypes.c_ulong),
                            ("total_phys", ctypes.c_ulonglong),
                            ("avail_phys", ctypes.c_ulonglong),
                            ("total_page", ctypes.c_ulonglong),
                            ("avail_page", ctypes.c_ulonglong),
                            ("total_virtual", ctypes.c_ulonglong),
                            ("avail_virtual", ctypes.c_ulonglong),
                            ("avail_extended_virtual", ctypes.c_ulonglong)]
            status = MemoryStatus()
            status.length = ctypes.sizeof(MemoryStatus)
            if ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(status)):
                ram_bytes = status.total_phys
        except (AttributeError, OSError):
            pass
    return {
        "os": f"{platform.system()} {platform.release()} ({platform.machine()})",
        "cpu": cpu,
        "logical_cpus": os.cpu_count(),
        "ram_mib": round(ram_bytes / (1024 * 1024), 1) if ram_bytes is not None else None,
    }


def parse_proc_status(contents: str) -> dict:
    """Parse VmRSS kB and Threads without guessing values for missing keys."""
    values = {"rss_mib": None, "threads": None}
    for line in contents.splitlines():
        key, separator, raw = line.partition(":")
        if not separator:
            continue
        pieces = raw.split()
        try:
            if key == "VmRSS" and len(pieces) == 2 and pieces[1] == "kB":
                values["rss_mib"] = int(pieces[0]) / 1024
            elif key == "Threads" and len(pieces) == 1:
                values["threads"] = int(pieces[0])
        except ValueError:
            continue
    return values


def process_metrics(pid: int) -> dict:
    unknown = {"rss_mib": None, "threads": None}
    if sys.platform.startswith("linux"):
        try:
            return parse_proc_status(Path(f"/proc/{pid}/status").read_text())
        except OSError:
            return unknown
    if sys.platform == "darwin":
        # Some BSD ps variants do not implement nlwp; keep RSS when possible.
        for columns in (("rss=", "nlwp="), ("rss=",)):
            try:
                result = subprocess.run(["ps", "-o", ",".join(columns), "-p", str(pid)],
                                        capture_output=True, text=True, check=True, timeout=2)
                parts = result.stdout.split()
                if len(parts) == len(columns):
                    return {"rss_mib": int(parts[0]) / 1024,
                            "threads": int(parts[1]) if len(parts) > 1 else None}
            except (OSError, ValueError, subprocess.SubprocessError):
                continue
    return unknown


def varint(number: int) -> bytes:
    output = bytearray()
    while True:
        part = number & 127
        number >>= 7
        output.append(part | (128 if number else 0))
        if not number:
            return bytes(output)


def read_exact(sock: socket.socket, length: int) -> bytes:
    data = bytearray()
    while len(data) < length:
        chunk = sock.recv(length - len(data))
        if not chunk:
            raise BenchmarkError("status server closed the connection early")
        data.extend(chunk)
    return bytes(data)


def read_varint(sock: socket.socket) -> int:
    value = 0
    for shift in range(0, 35, 7):
        byte = read_exact(sock, 1)[0]
        value |= (byte & 127) << shift
        if byte & 128 == 0:
            return value
    raise BenchmarkError("oversized status packet VarInt")


def packet(packet_id: int, body: bytes = b"") -> bytes:
    contents = varint(packet_id) + body
    return varint(len(contents)) + contents


def read_packet(sock: socket.socket) -> tuple[int, bytes]:
    length = read_varint(sock)
    if not 1 <= length <= 1_048_576:
        raise BenchmarkError(f"invalid status packet size {length}")
    contents = read_exact(sock, length)
    # Packet IDs are small in the Status state, so one byte suffices here.
    if contents[0] not in (0, 1):
        raise BenchmarkError("unexpected Status packet ID")
    return contents[0], contents[1:]


def decode_status(body: bytes) -> dict:
    # Decode the VarInt-prefixed JSON string in the Status Response.
    size = 0
    offset = 0
    for shift in range(0, 35, 7):
        if offset >= len(body):
            raise BenchmarkError("truncated status JSON length")
        part = body[offset]
        offset += 1
        size |= (part & 127) << shift
        if not part & 128:
            break
    else:
        raise BenchmarkError("oversized status JSON length")
    if len(body) - offset != size:
        raise BenchmarkError("invalid status JSON length")
    try:
        status = json.loads(body[offset:].decode("utf-8"))
        if status["version"]["protocol"] != PROTOCOL:
            raise BenchmarkError("status response has the wrong protocol version")
    except (UnicodeError, ValueError, TypeError, KeyError) as error:
        raise BenchmarkError("invalid protocol-767 status JSON") from error
    return status


def status_ping(port: int, timeout: float = 2.0, sample_active=None) -> float:
    """Perform an entire protocol-767 status exchange; time only Ping→Pong."""
    with socket.create_connection(("127.0.0.1", port), timeout=timeout) as sock:
        sock.settimeout(timeout)
        host = b"127.0.0.1"
        handshake = (varint(PROTOCOL) + varint(len(host)) + host
                     + struct.pack(">H", port) + varint(1))
        sock.sendall(packet(0, handshake) + packet(0))
        response_id, response = read_packet(sock)
        if response_id != 0:
            raise BenchmarkError("expected a Status Response packet")
        decode_status(response)
        # The server's status handler is still awaiting Ping on this socket.
        # Sample active process metrics outside the Ping RTT timing window.
        if sample_active is not None:
            sample_active()
        nonce = time.perf_counter_ns() & ((1 << 63) - 1)
        payload = struct.pack(">q", nonce)
        start = time.perf_counter_ns()
        sock.sendall(packet(1, payload))
        pong_id, pong = read_packet(sock)
        end = time.perf_counter_ns()
        if pong_id != 1 or pong != payload:
            raise BenchmarkError("invalid Status Pong payload")
        return (end - start) / 1_000_000


def select_port(requested: int) -> int:
    # An ephemeral port is allocated locally and released for the child to bind.
    # The bind also rejects already occupied explicitly requested ports.
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", requested))
        return sock.getsockname()[1]


def stop_server(process: subprocess.Popen) -> None:
    if process.poll() is not None:
        return
    try:
        if process.stdin:
            process.stdin.write(b"/stop\n")
            process.stdin.flush()
        process.wait(timeout=3)
        return
    except (OSError, ValueError, subprocess.TimeoutExpired):
        pass
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)


def benchmark_run(command: list[str], requested_port: int) -> dict:
    with tempfile.TemporaryDirectory(prefix="megu-bench-") as temporary:
        directory = Path(temporary)
        port = select_port(requested_port)
        environment = {key: value for key, value in os.environ.items()
                       if not key.startswith("MEGU_")}
        environment.update(MEGU_BIND_ADDR="127.0.0.1", MEGU_PORT=str(port),
                           TMPDIR=temporary, TMP=temporary, TEMP=temporary)
        log = directory / "server.log"
        with log.open("wb") as stream:
            started = time.perf_counter()
            try:
                process = subprocess.Popen(command, cwd=directory, env=environment,
                                           stdin=subprocess.PIPE, stdout=stream,
                                           stderr=subprocess.STDOUT)
            except OSError as error:
                raise BenchmarkError(f"cannot launch benchmark binary: {error}") from error
            try:
                deadline = started + STARTUP_TIMEOUT_S
                while True:
                    if process.poll() is not None:
                        raise BenchmarkError(f"server exited before ready ({process.returncode})")
                    if time.perf_counter() >= deadline:
                        raise BenchmarkError("server did not complete Status Ping before timeout")
                    try:
                        status_ping(port, timeout=min(0.5, max(0.05, deadline - time.perf_counter())))
                        startup_ms = (time.perf_counter() - started) * 1000
                        break
                    except (OSError, BenchmarkError):
                        time.sleep(0.03)
                idle = process_metrics(process.pid)
                samples = []
                active = []
                for _ in range(PING_COUNT):
                    samples.append(status_ping(
                        port, sample_active=lambda: active.append(process_metrics(process.pid))))
                return {
                    "port": port,
                    "startup_ms": startup_ms,
                    "idle_rss_mib": idle["rss_mib"],
                    "active_rss_mib": max((item["rss_mib"] for item in active
                                            if item["rss_mib"] is not None), default=None),
                    "idle_threads": idle["threads"],
                    "active_threads": max((item["threads"] for item in active
                                           if item["threads"] is not None), default=None),
                    "ping_rtt_ms": statistics.median(samples),
                    "ping_rtt_samples_ms": samples,
                }
            except (OSError, BenchmarkError) as error:
                stream.flush()
                tail = log.read_bytes()[-1500:].decode("utf-8", errors="replace")
                raise BenchmarkError(f"{error}\nServer log tail:\n{tail}") from error
            finally:
                stop_server(process)
                if process.stdin:
                    process.stdin.close()


def aggregate(runs: list[dict]) -> dict:
    result = {}
    for name in METRICS:
        numbers = [run[name] for run in runs if run[name] is not None]
        result[name] = {"min": min(numbers), "median": statistics.median(numbers),
                        "max": max(numbers)} if numbers else None
    return result


def benchmark_plan(binary: Path, command: list[str], runs: int, port: int) -> dict:
    return {
        "schema": "megu-bench-plan/1",
        "dry_run": True,
        "binary": str(binary),
        "command": command,
        "runs": runs,
        "port": port if port else "ephemeral per run",
        "bind": "127.0.0.1",
        "protocol": PROTOCOL,
        "pings_per_run": PING_COUNT,
        "startup_timeout_seconds": STARTUP_TIMEOUT_S,
        "environment_overrides": {
            "MEGU_BIND_ADDR": "127.0.0.1",
            "MEGU_PORT": str(port) if port else "<ephemeral per run>",
            "TMPDIR": "<fresh isolated workdir per run>",
            "TMP": "<fresh isolated workdir per run>",
            "TEMP": "<fresh isolated workdir per run>",
        },
        "measures": list(METRICS),
        "shutdown": "/stop on stdin, then terminate/kill if needed",
        "note": NOTE,
        "hardware": hardware_context(),
    }


def fmt(value: float | int | None) -> str:
    if value is None:
        return "N/A"
    return f"{value:.2f}" if isinstance(value, float) else str(value)


def hardware_line(hardware: dict) -> str:
    cpus = hardware["logical_cpus"]
    ram = hardware["ram_mib"]
    return (f"{hardware['cpu']} ({cpus if cpus is not None else 'unknown'} logical CPUs), "
            f"RAM {fmt(ram)} MiB, {hardware['os']}")


def render_text(receipt: dict) -> str:
    if receipt.get("dry_run"):
        return ("MEGU benchmark dry run\n" + json.dumps(receipt, indent=2)
                + "\nNo server spawned.")
    lines = ["MEGU local status benchmark", hardware_line(receipt["hardware"]),
             f"Binary: {receipt['binary']}",
             f"Flags: {receipt['flags']}", f"Runs: {len(receipt['runs'])}; "
             f"Protocol: {PROTOCOL}; pings/run: {PING_COUNT}",
             f"{'Metric':<30} {'Min':>12} {'Median':>12} {'Max':>12}"]
    for name, label in METRICS.items():
        values = receipt["metrics"][name]
        display = [fmt(values[key]) if values else "N/A" for key in ("min", "median", "max")]
        lines.append(f"{label:<30} {display[0]:>12} {display[1]:>12} {display[2]:>12}")
    return "\n".join([*lines, NOTE])


def render_markdown(receipt: dict) -> str:
    if receipt.get("dry_run"):
        return "### MEGU dry-run plan\n\n```json\n" + json.dumps(receipt, indent=2) + "\n```"
    lines = ["### MEGU local status benchmark", "",
             f"**Machine** — {hardware_line(receipt['hardware'])}",
             f"**Binary** — `{receipt['binary']}`  ",
             f"**Measured flags** — `{receipt['flags']}`  ",
             f"**Runs** — {len(receipt['runs'])}, protocol {PROTOCOL}, {PING_COUNT} pings/run",
             "", "| Metric | Min | Median | Max |", "| :-- | --: | --: | --: |"]
    for name, label in METRICS.items():
        values = receipt["metrics"][name]
        parts = [fmt(values[key]) if values else "N/A" for key in ("min", "median", "max")]
        lines.append(f"| {label} | {parts[0]} | **{parts[1]}** | {parts[2]} |")
    return "\n".join([*lines, "", NOTE])


def render_json(receipt: dict) -> str:
    return json.dumps(receipt, indent=2, ensure_ascii=False)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Isolated local Protocol-767 status benchmark")
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--runs", type=positive_runs, default=3)
    parser.add_argument("--port", type=valid_port, default=0)
    parser.add_argument("--format", choices=("text", "markdown", "json"), default="text")
    parser.add_argument("--out", type=Path, help="write canonical JSON receipt")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args(argv)
    binary = args.binary.expanduser().resolve()
    if not binary.is_file():
        parser.error("--binary must name an existing regular file")
    if os.name != "nt" and not os.access(binary, os.X_OK) and binary.suffix.lower() != ".py":
        parser.error("--binary must be executable")
    command = binary_command(binary)
    if args.dry_run:
        report = benchmark_plan(binary, command, args.runs, args.port)
    else:
        flags = {"argv": command, "MEGU_BIND_ADDR": "127.0.0.1",
                 "MEGU_PORT": args.port if args.port else "ephemeral per run",
                 "TMPDIR": "fresh isolated workdir per run", "TMP": "same as TMPDIR",
                 "TEMP": "same as TMPDIR", "other_MEGU_vars": "cleared"}
        try:
            runs = [benchmark_run(command, args.port) for _ in range(args.runs)]
        except (OSError, BenchmarkError) as error:
            print(f"megu-bench: {error}", file=sys.stderr)
            return 1
        report = {"schema": SCHEMA, "binary": str(binary), "hardware": hardware_context(),
                  "protocol": PROTOCOL, "pings_per_run": PING_COUNT, "flags": flags,
                  "runs": runs, "metrics": aggregate(runs),
                  "measurement_scope": "fresh empty world, loopback status only, no players",
                  "note": NOTE}
        if args.out:
            try:
                args.out.write_text(render_json(report) + "\n", encoding="utf-8")
            except OSError as error:
                print(f"megu-bench: cannot write receipt: {error}", file=sys.stderr)
                return 1
    renderers = {"text": render_text, "markdown": render_markdown, "json": render_json}
    print(renderers[args.format](report))
    return 0


if __name__ == "__main__":
    sys.exit(main())
