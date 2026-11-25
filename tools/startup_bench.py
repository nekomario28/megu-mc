#!/usr/bin/env python3
"""Cold-start benchmark for the megu server: exec -> `megu v... listening` ms.

The server's cold-start contract is that the game port is bound and the
`listening` banner is printed before the seed/worldgen/save datapack work runs
(see the "cold-start fast path" in `fn main`), so this tool measures wall-clock
from `Popen` to the first stderr line matching `LISTENING_RE`, across N runs.

Usage:
    python3 tools/startup_bench.py [--binary target/release/megu] [--runs 5]
        [--run-dir .] [--env K=V ...] [--args ARG ...] [--cold] [--json]
        [--max-ms MILLIS] [--timeout SECONDS]

`--max-ms` fails (exit 1) when the median exec->listening time exceeds the
threshold; `--json` emits the full report on stdout for receipts. `--cold`
advises the kernel to drop the benchmarked files from the page cache
(`posix_fadvise(DONTNEED)`, best-effort) before each run — an approximation of
a cold start that does not need root.

The tool is careful about the run directory: it only ever *creates* a missing
`eula.txt` (the EULA gate otherwise refuses the boot) and never writes
anything else; world saves the server itself writes stay in `--run-dir`.
"""
import argparse
import json
import math
import os
import re
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

LISTENING_RE = re.compile(rb"megu v\S+ listening \S+:\d+ ")
STAGE_RE = re.compile(
    rb"^\[startup\]\s+(?P<total>[0-9.]+) ms \(\+\s*(?P<delta>[0-9.]+)\) (?P<stage>.+?)\s*$"
)
PARALLEL_STAGE_RE = re.compile(
    rb"^\[startup\]\s+parallel (?P<stage>.+?):\s+(?P<ms>[0-9.]+) ms\s*$"
)

DEFAULT_BINARY = Path(__file__).resolve().parents[1] / "target" / "release" / "megu"


def free_port() -> int:
    """A port the kernel just confirmed free; the usual TOCTOU race is fine
    for a benchmark run that would only lose to another process binding first."""
    import socket

    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def drop_file_cache(path: Path) -> None:
    """Best-effort page-cache eviction for one file via posix_fadvise.

    DONTNEED drops clean pages of the named file so the next exec/read pays
    real storage latency again — the closest a cold start gets without root.
    Any failure (unsupported fs, missing file) is ignored by design.
    """
    fadvise = getattr(os, "posix_fadvise", None)
    if fadvise is None:
        return
    try:
        fd = os.open(path, os.O_RDONLY)
    except OSError:
        return
    try:
        fadvise(fd, 0, 0, getattr(os, "POSIX_FADV_DONTNEED", 4))
    except OSError:
        pass
    finally:
        os.close(fd)


def drop_tree_cache(root: Path, limit: int = 20000) -> None:
    """Evict every file under `root` (capped so a huge tree cannot stall the
    benchmark itself)."""
    seen = 0
    for dirpath, _dirs, files in os.walk(root):
        for name in files:
            drop_file_cache(Path(dirpath) / name)
            seen += 1
            if seen >= limit:
                return


def parse_stage(line: bytes):
    """One `MEGU_STARTUP_PROFILE` line -> (stage, total_ms, delta_ms) or a
    parallel-stage tuple (stage, ms, None), else None."""
    m = STAGE_RE.match(line)
    if m:
        return (m["stage"].decode(), float(m["total"]), float(m["delta"]))
    m = PARALLEL_STAGE_RE.match(line)
    if m:
        return (m["stage"].decode(), float(m["ms"]), None)
    return None


class BootSample:
    """One measured run: exec->listening ms plus the server's own stage
    breakdown when MEGU_STARTUP_PROFILE=1 was armed."""

    def __init__(self):
        self.listening_ms = None
        self.exit_code = None
        self.timed_out = False
        self.stages = []  # [(stage, total_ms, delta_ms|None)]
        self.stderr_tail = []

    def to_dict(self):
        return {
            "listening_ms": self.listening_ms,
            "exit_code": self.exit_code,
            "timed_out": self.timed_out,
            "stages": [
                {"stage": s, "total_ms": t, "delta_ms": d} for s, t, d in self.stages
            ],
        }


def measure_once(binary, run_dir, env, timeout, args=()):
    """Spawn the server, read stderr until the listening banner, then stop it.

    Returns a BootSample. A boot that never prints the banner within `timeout`
    seconds is killed and reported with `timed_out=True` — the run is a
    measurement failure, not a crash of the harness.
    """
    sample = BootSample()
    t0 = time.perf_counter()
    proc = subprocess.Popen(
        [str(binary), *args],
        cwd=str(run_dir),
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    deadline = t0 + timeout
    lines = []

    def drain():
        try:
            for line in proc.stderr:
                lines.append(line)
        except (OSError, ValueError):
            pass

    reader = threading.Thread(target=drain, daemon=True)
    reader.start()
    consumed = 0
    try:
        while True:
            while consumed < len(lines):
                line = lines[consumed]
                consumed += 1
                stage = parse_stage(line)
                if stage:
                    sample.stages.append(stage)
                if LISTENING_RE.search(line):
                    sample.listening_ms = (time.perf_counter() - t0) * 1000.0
                    break
            if sample.listening_ms is not None:
                break
            if proc.poll() is not None and consumed >= len(lines):
                break
            if time.perf_counter() > deadline:
                sample.timed_out = True
                break
            time.sleep(0.0005)
    finally:
        stop_server(proc, sample)
    reader.join(timeout=2.0)
    if proc.stderr is not None:
        proc.stderr.close()
    for line in lines[consumed:]:
        stage = parse_stage(line)
        if stage:
            sample.stages.append(stage)
    sample.stderr_tail = [l.decode(errors="replace").rstrip() for l in lines[-25:]]
    return sample


def stop_server(proc, sample):
    """SIGTERM (vanilla's graceful stop), then SIGKILL past the deadline."""
    if proc.poll() is not None:
        sample.exit_code = proc.returncode
        return
    proc.terminate()
    try:
        sample.exit_code = proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        proc.kill()
        sample.exit_code = proc.wait()


def percentile(values, pct):
    """Nearest-rank percentile; `pct` in 0..100."""
    if not values:
        return None
    ordered = sorted(values)
    rank = max(1, math.ceil(pct * len(ordered) / 100))
    return ordered[min(rank, len(ordered)) - 1]


def summarize(samples):
    """Report statistics over the runs that produced a listening banner."""
    timings = [s.listening_ms for s in samples if s.listening_ms is not None]
    failures = len(samples) - len(timings)
    result = {
        "runs": len(samples),
        "measured": len(timings),
        "failures": failures,
        "median_ms": None,
        "p95_ms": None,
        "min_ms": None,
        "max_ms": None,
        "mean_ms": None,
        "timings_ms": timings,
    }
    if timings:
        result["median_ms"] = statistics.median(timings)
        result["p95_ms"] = percentile(timings, 95)
        result["min_ms"] = min(timings)
        result["max_ms"] = max(timings)
        result["mean_ms"] = statistics.mean(timings)
    return result


def ensure_run_dir(run_dir):
    """The EULA gate refuses a boot with a present-but-unaccepted eula.txt;
    a *missing* file is a pass, so only create the file when some configuration
    needs it — we create it unconditionally here to keep the contract simple
    and explicit. Never overwrites an existing file."""
    eula = run_dir / "eula.txt"
    if not eula.exists():
        eula.write_text("eula=true\n")


def build_env(overrides):
    env = dict(os.environ)
    for item in overrides:
        key, sep, value = item.partition("=")
        if not sep:
            raise SystemExit(f"--env expects KEY=VALUE, got {item!r}")
        env[key] = value
    return env


def format_report(report, samples, binary, run_dir):
    lines = [
        f"startup_bench: {binary} (run-dir {run_dir})",
        f"runs={report['runs']} measured={report['measured']} "
        f"failures={report['failures']}",
    ]
    for i, ms in enumerate(report["timings_ms"]):
        lines.append(f"  run {i}: {ms:.1f} ms")
    if report["measured"]:
        lines.append(
            f"median={report['median_ms']:.1f} ms p95={report['p95_ms']:.1f} ms "
            f"min={report['min_ms']:.1f} ms max={report['max_ms']:.1f} ms "
            f"mean={report['mean_ms']:.1f} ms"
        )
    else:
        tail = next((s.stderr_tail for s in samples if s.stderr_tail), [])
        for line in tail[-10:]:
            lines.append(f"  server: {line}")
    return "\n".join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Median/p95 exec->'listening' benchmark for the megu server."
    )
    parser.add_argument("--binary", default=str(DEFAULT_BINARY),
                        help="server binary to measure (default %(default)s)")
    parser.add_argument("--runs", type=int, default=5,
                        help="measured runs (default %(default)s)")
    parser.add_argument("--run-dir", default=".",
                        help="server working directory (default: cwd)")
    parser.add_argument("--env", action="append", default=[], metavar="K=V",
                        help="extra environment for the server (repeatable)")
    parser.add_argument("--args", action="append", default=[], metavar="ARG",
                        help="extra server CLI args (repeatable)")
    parser.add_argument("--cold", action="store_true",
                        help="posix_fadvise(DONTNEED) the binary + run-dir "
                             "before each run to approximate a cold cache")
    parser.add_argument("--max-ms", type=float, default=None,
                        help="fail when median exec->listening exceeds MILLIS")
    parser.add_argument("--timeout", type=float, default=30.0,
                        help="per-run seconds to wait for the banner "
                             "(default %(default)s)")
    parser.add_argument("--json", action="store_true",
                        help="emit the full report as JSON on stdout")
    ns = parser.parse_args(argv)

    binary = Path(ns.binary).resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        print(f"startup_bench: not an executable: {binary}", file=sys.stderr)
        return 2
    run_dir = Path(ns.run_dir).resolve()
    if ns.runs < 1:
        print("startup_bench: --runs must be >= 1", file=sys.stderr)
        return 2
    ensure_run_dir(run_dir)
    env = build_env(ns.env)
    env.setdefault("MEGU_BIND_ADDR", "127.0.0.1")
    env.setdefault("MEGU_PORT", str(free_port()))

    samples = []
    for _ in range(ns.runs):
        if ns.cold:
            drop_file_cache(binary)
            drop_tree_cache(run_dir)
        samples.append(measure_once(binary, run_dir, env, ns.timeout, ns.args))
    report = summarize(samples)
    report["binary"] = str(binary)
    report["run_dir"] = str(run_dir)
    report["cold"] = ns.cold
    report["samples"] = [s.to_dict() for s in samples]

    if ns.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(format_report(report, samples, binary, run_dir))

    if report["measured"] == 0:
        return 1
    if ns.max_ms is not None and report["median_ms"] > ns.max_ms:
        if not ns.json:
            print(f"FAIL: median {report['median_ms']:.1f} ms > "
                  f"--max-ms {ns.max_ms}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
