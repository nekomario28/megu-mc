#!/usr/bin/env python3
"""Tick-spike gate: server `[tick]` telemetry must stay inside the work budget.

The server reports rolling tick health to stderr once every 100 ticks:

    [tick] observed_tps=19.90 avg_work_ms=11.50 max_work_ms=15.77 over_budget=0/100

Every sample must satisfy `max_work_ms <= --max-work-ms-ceiling` (default
250.0 ms) and `observed_tps >= --min-tps` (default 18.0). Zero samples fails
closed — a gate that never saw telemetry proves nothing.

Modes:

    python3 tools/tick_spike_gate.py run.log        # parse a captured log
    python3 tools/tick_spike_gate.py -              # read stdin
    python3 tools/tick_spike_gate.py --server-bin target/debug/megu \
        --live-seconds 25                          # bounded live run

Exit status: 0 clean, 1 findings, 2 usage/IO error.
"""
import argparse
import os
import re
import select
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

TICK = re.compile(
    r"\[tick\]\s+observed_tps=([0-9.]+)\s+avg_work_ms=([0-9.]+)\s+"
    r"max_work_ms=([0-9.]+)\s+over_budget=(\d+)/(\d+)")


def evaluate(lines, ceiling_ms, min_tps):
    """Score `[tick]` lines; return (samples, findings)."""
    samples = 0
    findings = []
    for lineno, line in enumerate(lines, 1):
        match = TICK.search(line)
        if not match:
            continue
        samples += 1
        tps, _avg, peak = float(match[1]), float(match[2]), float(match[3])
        if peak > ceiling_ms:
            findings.append(
                f"line {lineno}: max_work_ms={peak:.2f} exceeds "
                f"{ceiling_ms:.2f}ms ceiling ({line.strip()})")
        if tps < min_tps:
            findings.append(
                f"line {lineno}: observed_tps={tps:.2f} below "
                f"{min_tps:.2f} minimum ({line.strip()})")
    return samples, findings


def live_lines(server_bin, live_seconds, listen_deadline=30.0):
    """Run the server briefly on loopback; yield its stderr lines.

    Fails closed: raises RuntimeError when the server exits early or never
    prints its `listening` banner inside `listen_deadline`.
    """
    env = dict(os.environ)
    env["MEGU_BIND_ADDR"] = "127.0.0.1"
    for var in ("MEGU_PORT", "MEGU_RCON_PORT"):
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            env[var] = str(sock.getsockname()[1])
    with tempfile.TemporaryDirectory(prefix="megu-tickgate-") as workdir:
        proc = subprocess.Popen(
            [str(server_bin)], cwd=workdir, env=env,
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
            text=True, errors="replace")
        lines = []
        try:
            deadline = time.monotonic() + listen_deadline
            listening = False
            while time.monotonic() < deadline and not listening:
                ready, _, _ = select.select(
                    [proc.stderr], [], [],
                    max(0.0, deadline - time.monotonic()))
                if not ready:
                    break
                line = proc.stderr.readline()
                if line == "":
                    break
                lines.append(line)
                listening = " listening " in line
            if proc.poll() is not None:
                raise RuntimeError(
                    f"server exited before listening (code {proc.returncode})")
            if not listening:
                raise RuntimeError(
                    f"server produced no 'listening' banner in "
                    f"{listen_deadline:.0f}s")
            collect_until = time.monotonic() + live_seconds
            while True:
                remaining = collect_until - time.monotonic()
                if remaining <= 0:
                    break
                ready, _, _ = select.select(
                    [proc.stderr], [], [], remaining)
                if not ready:
                    if proc.poll() is not None:
                        raise RuntimeError(
                            f"server exited mid-run (code {proc.returncode})")
                    continue
                line = proc.stderr.readline()
                if line == "":
                    if proc.poll() is not None:
                        raise RuntimeError(
                            f"server exited mid-run (code {proc.returncode})")
                    continue
                lines.append(line)
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)
    return lines


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("log", nargs="?",
                        help="captured server log ('-' = stdin); required "
                             "unless --server-bin is given")
    parser.add_argument("--server-bin", type=Path,
                        help="run this server binary briefly and parse its "
                             "stderr telemetry")
    parser.add_argument("--live-seconds", type=float, default=25.0,
                        help="collection window after the listening banner "
                             "in --server-bin mode (default 25s)")
    parser.add_argument("--max-work-ms-ceiling", type=float, default=250.0,
                        help="largest acceptable max_work_ms (default 250.0)")
    parser.add_argument("--min-tps", type=float, default=18.0,
                        help="smallest acceptable observed_tps (default 18.0)")
    parser.add_argument("--min-samples", type=int, default=1,
                        help="fail when fewer than this many [tick] samples "
                             "are seen (default 1)")
    args = parser.parse_args(argv)

    try:
        if args.server_bin is not None:
            if not args.server_bin.is_file():
                raise ValueError(f"--server-bin {args.server_bin} not found")
            lines = live_lines(args.server_bin.resolve(), args.live_seconds)
            origin = str(args.server_bin)
        elif args.log == "-":
            lines = sys.stdin.readlines()
            origin = "<stdin>"
        elif args.log:
            lines = Path(args.log).read_text(
                encoding="utf-8", errors="replace").splitlines()
            origin = args.log
        else:
            parser.error("give a log file, '-', or --server-bin")
    except (OSError, RuntimeError, ValueError) as error:
        print(f"tick spike gate: {error}", file=sys.stderr)
        return 2

    samples, findings = evaluate(lines, args.max_work_ms_ceiling, args.min_tps)
    if samples < args.min_samples:
        findings.append(
            f"only {samples} [tick] sample(s) seen in {origin} "
            f"(need >= {args.min_samples}); telemetry missing or server dead")
    if findings:
        print(f"tick spike gate: FAIL ({samples} sample(s), "
              f"{len(findings)} finding(s))")
        for finding in findings:
            print(f"  {finding}")
        return 1
    print(f"tick spike gate: PASS ({samples} sample(s); max_work_ms "
          f"<= {args.max_work_ms_ceiling:.2f}ms, tps >= {args.min_tps:.2f})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
