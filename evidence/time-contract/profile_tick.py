#!/usr/bin/env python3
"""Measure the production tick fixture, without starting listeners or touching a user world."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not 1 <= args.runs <= 10:
        parser.error("runs must be between 1 and 10")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    command = ["cargo", "test", "--release", "--offline", "--locked", "-j", "2", "--no-run", "--message-format=json"]
    cadence = subprocess.check_output(["git", "grep", "-h", "const TICK_MS: u64", "--", "src/main.rs"], cwd=ROOT, text=True).strip()
    TICKS_PER_SECOND_LABEL = cadence.split("=")[-1].strip().rstrip(";") + "ms tick period"
    build = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, timeout=240)
    (output / "build.log").write_text(build.stderr)
    if build.returncode:
        raise RuntimeError(f"release test build failed; see {output / 'build.log'}")
    artifacts = [json.loads(line) for line in build.stdout.splitlines() if line.startswith("{")]
    binary = next(Path(item["executable"]) for item in artifacts
                  if item.get("reason") == "compiler-artifact" and item.get("executable")
                  and item["target"]["name"] == "mc-spine")
    results = []
    for index in range(args.runs):
        log = output / f"run-{index}.log"
        with tempfile.TemporaryDirectory(prefix="mc-spine-tick-profile-") as scratch:
            pid = os.fork()
            if pid == 0:
                try:
                    os.chdir(scratch)
                    env = dict(os.environ, MC_SPINE_TICK_PROFILE_DIR=scratch)
                    with log.open("wb") as stream:
                        os.dup2(stream.fileno(), 1)
                        os.dup2(stream.fileno(), 2)
                        signal.alarm(120)
                        os.execve(binary, [str(binary), "tick_tests::profile_game_tick", "--exact", "--ignored", "--nocapture", "--test-threads=1"], env)
                except BaseException:
                    os._exit(127)
            _, status, usage = os.wait4(pid, 0)
            if os.waitstatus_to_exitcode(status) != 0:
                raise RuntimeError(f"profile child failed (status {status}); see {log}")
        scenarios = {}
        for line in log.read_text().splitlines():
            marker = line.find("PROFILE\t")
            if marker < 0:
                continue
            _, scenario, stage, *values = line[marker:].split("\t")
            values = list(map(int, values))
            item = scenarios.setdefault(scenario, {})
            if stage == "tick":
                item["tick"] = dict(zip(("samples", "total_ns", "p50_ns", "p95_ns", "p99_ns"), values))
            else:
                item.setdefault("stages", {})[stage] = dict(zip(("calls", "total_ns", "max_ns"), values))
        assert set(scenarios) == {"empty", "idle_actor", "idle_furnaces", "active_furnaces", "crowded_actors", "dense_actors"}
        for item in scenarios.values():
            assert item["tick"]["samples"] == 1000
            assert len(item["stages"]) == 15
            assert all(value["calls"] == (167 if name == "lava" else 1000)
                       for name, value in item["stages"].items())
        results.append({"run": index, "cpu_user_seconds": usage.ru_utime,
                        "cpu_system_seconds": usage.ru_stime, "peak_rss_kib": usage.ru_maxrss,
                        "scenarios": scenarios})
    source = [str(path.relative_to(ROOT)) for path in sorted((ROOT / "src").glob("*.rs"))]
    source += ["Cargo.toml", "Cargo.lock", "evidence/time-contract/profile_tick.py"]
    report = {
        "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "working_diff_sha256": hashlib.sha256(subprocess.check_output(["git", "diff", "HEAD"], cwd=ROOT)).hexdigest(),
        "source_sha256": {name: hashlib.sha256((ROOT / name).read_bytes()).hexdigest() for name in source},
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        "build_command": command,
        "cadence": f"{TICKS_PER_SECOND_LABEL} semantics; ticks are executed back to back without sleep",
        "per_tick_unit": "microseconds, total_ns over 1000 ticks divided by 1000 then by 1000",
        "scope": "flat headless fixture; 100 warmup + 1000 measured ticks per scenario; no clients, mobs, fluids, worldgen, Java mods",
        "cpu_rss_scope": "whole child: all six scenarios, fixture setup, warmup and output; Linux RSS in KiB",
        "results": results,
    }
    (output / "results.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
