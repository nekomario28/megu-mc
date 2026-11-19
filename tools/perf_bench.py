#!/usr/bin/env python3
"""Reproducible 64-chunk worldgen benchmark and M1-M6 counter harness.

Automates the ``MC_SPINE_WG_DUMP_N=8`` release dump measurements documented in
``evidence/foundation/performance/README.md``.  No cargo and no Rust source
build is involved: the harness drives an existing binary with the environment
variables the binary already reads.

What it measures, per (arm, jobs) cell:

* wall clock -- ``time.monotonic()`` around the child process,
* phase time -- the binary's own ``[wgdump] ... in <T>`` line (the generation
  loop only; this is the number used in the README),
* child CPU -- ``RUSAGE_CHILDREN`` user+sys delta (/usr/bin/time is absent here),
* sha256 -- of the dump bytes, optionally checked against the recorded digests,
* M1-M6 -- parsed from the exact ``[m3]`` lines ``m3_report()`` prints when
  ``MC_SPINE_M3_PROFILE=1`` is set.

Runs are interleaved (round -> arm -> jobs -> binary) so host-load drift hits
every arm equally.  With ``--compare-binary`` both binaries run in the same
session, in that same interleaved order, and a delta % and speedup ratio are
reported per cell.  Dump files are written under ``--tmpdir`` (never /tmp) and
removed once hashed.

Usage::

    tools/perf_bench.py --markdown --json target/perf/latest.json
    tools/perf_bench.py --compare-binary target/release/candidate \\
        --profile-counters --cpu-quota 400% --memory-max 5G --markdown
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import resource
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, Dict, Iterable, List, Mapping, Optional, Sequence, Tuple

# ---------------------------------------------------------------------------
# Constants that mirror the binary's env contract and the recorded digests.
# ---------------------------------------------------------------------------

DEFAULT_BINARY = "./target/release/mc-spine"
ARMS: Tuple[str, ...] = ("full", "nofeat", "terrain")

ARM_ENV: Dict[str, Dict[str, str]] = {
    # full: features on, nothing disabled
    "full": {},
    # terrain + carvers
    "nofeat": {"MC_SPINE_NOFEAT": "1"},
    # terrain only
    "terrain": {"MC_SPINE_NOFEAT": "1", "MC_SPINE_NOCARVE": "1"},
}

# sha256 of the 25,165,824-byte seed-0 N=8 block dump (see the README).
EXPECTED_SHA_DEFAULT: Dict[str, str] = {
    "full": "670fdfe3472812afeeba823a30e08f472ea8eb69965d6ba0c54b30a2e9949def",
    "nofeat": "57101c434d49edf21f7efdd4207beca8cae34ff74cbf4ffdc009b680cb5d5a48",
    "terrain": "bb499dfb6c53128734a23f8722534a5f1882e7acbb53a9885a604ce74352beff",
}

_SHA_RE = re.compile(r"^[0-9a-fA-F]{64}$")

# ---------------------------------------------------------------------------
# Parsers.  These are deliberately pure functions so the unit test can check
# them against the real stderr formats without running the Rust binary.
# ---------------------------------------------------------------------------

# Rust's Duration Debug format: 8.273s, 557.3421ms, 1.234µs, 12ns, ...
_DURATION_RE = re.compile(r"^(\d+(?:\.\d+)?)\s*(ns|µs|us|ms|s)$", re.IGNORECASE)

# [wgdump] 64 chunks -> /path/d.bin (25165824 bytes) in 8.273123s
_WGDUMP_RE = re.compile(
    r"\[wgdump\]\s+(?P<chunks>\d+)\s+chunks\s+->\s+(?P<path>\S+)\s+"
    r"\((?P<bytes>\d+)\s+bytes\)\s+in\s+"
    r"(?P<dur>\d+(?:\.\d+)?\s*(?:ns|µs|us|ms|s))",
    re.MULTILINE,
)

_WGPROF_EVAL_RE = re.compile(
    r"\[wgprof\] eval_c calls: (?P<calls>\d+) \((?P<per>\d+) per chunk\)"
)

_M3_HEADER_RE = re.compile(
    r"\[m3\] terrain_store_bound=(?P<bound>\d+) jobs=(?P<jobs>\d+)"
)

_M3_M1_RE = re.compile(
    r"\[m3\] M1 chunk_starts owner=(?P<cs_owner>\d+) worker=(?P<cs_worker>\d+)"
    r" \| cols built owner=(?P<cb_owner>\d+) worker=(?P<cb_worker>\d+)"
    r" hit owner=(?P<ch_owner>\d+) worker=(?P<ch_worker>\d+)"
    r" \| beard resolved owner=(?P<br_owner>\d+) worker=(?P<br_worker>\d+)"
)

_M3_M2_RE = re.compile(
    r"\[m3\] M2 terr_c hit=(?P<hit>\d+) miss=(?P<miss>\d+) evict=(?P<evict>\d+)"
    r" install=(?P<install>\d+) install_dup=(?P<dup>\d+) bound=(?P<bound>\d+)"
)

_M3_M3_RE = re.compile(
    r"\[m3\] M3 feature reads rc_hit=(?P<rc_hit>\d+)"
    r" prefetch_hit=(?P<prefetch_hit>\d+) prefetch_miss=(?P<prefetch_miss>\d+)"
    r" \| fallback density=(?P<fb_density>\d+) deep=(?P<fb_deep>\d+)"
    r" aquifer=(?P<fb_aquifer>\d+) surface=(?P<fb_surface>\d+)"
    r" \| deep_memo hit=(?P<deep_hit>\d+) miss=(?P<deep_miss>\d+)"
    r" aqlv hit=(?P<aqlv_hit>\d+) miss=(?P<aqlv_miss>\d+)"
    r" prelim hit=(?P<prelim_hit>\d+) miss=(?P<prelim_miss>\d+)"
)

_M3_M4_RE = re.compile(
    r"\[m3\] M4 sections worker=(?P<worker>\d+) owner=(?P<owner>\d+)"
    r" evicted_unconsumed=(?P<evicted>\d+)"
)

_M3_M6_RE = re.compile(r"\[m3\] M6 df4 vec=(?P<vec>\d+) bail=(?P<bail>\d+)")

_M3_M5_RE = re.compile(
    r"\[m3\] M5 cols=(?P<cols_ms>\d+(?:\.\d+)?)ms/(?P<cols_n>\d+)"
    r" beard=(?P<beard_ms>\d+(?:\.\d+)?)ms/(?P<beard_n>\d+)"
    r" fill=(?P<fill_ms>\d+(?:\.\d+)?)ms/(?P<fill_n>\d+)"
    r" aquifer_cells=(?P<aquifer_cells>\d+) surface_cells=(?P<surface_cells>\d+)"
    r" carver=(?P<carver_ms>\d+(?:\.\d+)?)ms/(?P<carver_n>\d+)"
    r" feature=(?P<feature_ms>\d+(?:\.\d+)?)ms/(?P<feature_n>\d+)"
)

_DURATION_UNITS = {"ns": 1e-9, "µs": 1e-6, "us": 1e-6, "ms": 1e-3, "s": 1.0}


def parse_duration_seconds(text: str) -> float:
    """Convert a Rust ``Duration`` Debug string to seconds."""
    m = _DURATION_RE.match(text.strip())
    if not m:
        raise ValueError("unrecognized duration: %r" % (text,))
    return float(m.group(1)) * _DURATION_UNITS[m.group(2).lower()]


def parse_wgdump(stderr: str) -> Optional[Dict[str, Any]]:
    """Parse the binary's ``[wgdump] N chunks -> ... in T`` phase line."""
    m = _WGDUMP_RE.search(stderr)
    if not m:
        return None
    return {
        "chunks": int(m.group("chunks")),
        "path": m.group("path"),
        "bytes": int(m.group("bytes")),
        "duration_text": m.group("dur"),
        "seconds": parse_duration_seconds(m.group("dur")),
    }


def parse_eval_calls(stderr: str) -> Optional[Dict[str, int]]:
    """Parse the optional ``WG_COUNT`` line."""
    m = _WGPROF_EVAL_RE.search(stderr)
    if not m:
        return None
    return {"calls": int(m.group("calls")), "per_chunk": int(m.group("per"))}


def _ints(m: re.Match[str], *names: str) -> Dict[str, int]:
    return {name: int(m.group(name)) for name in names}


def parse_m3(stderr: str) -> Dict[str, Any]:
    """Parse the M1-M6 ``[m3]`` lines printed by ``worldgen::m3_report()``.

    Returns a dict keyed ``m1``..``m6`` plus ``header``.  A metric whose line
    is absent is simply omitted (counters are off unless the env flag is set),
    so this is safe to call on any stderr.
    """
    out: Dict[str, Any] = {}

    m = _M3_HEADER_RE.search(stderr)
    if m:
        out["header"] = _ints(m, "bound", "jobs")

    m = _M3_M1_RE.search(stderr)
    if m:
        out["m1"] = {
            "chunk_starts": {"owner": int(m.group("cs_owner")), "worker": int(m.group("cs_worker"))},
            "cols_built": {"owner": int(m.group("cb_owner")), "worker": int(m.group("cb_worker"))},
            "cols_hit": {"owner": int(m.group("ch_owner")), "worker": int(m.group("ch_worker"))},
            "beard_resolved": {"owner": int(m.group("br_owner")), "worker": int(m.group("br_worker"))},
        }

    m = _M3_M2_RE.search(stderr)
    if m:
        out["m2"] = {
            "hit": int(m.group("hit")),
            "miss": int(m.group("miss")),
            "evict": int(m.group("evict")),
            "install": int(m.group("install")),
            "install_dup": int(m.group("dup")),
            "bound": int(m.group("bound")),
        }

    m = _M3_M3_RE.search(stderr)
    if m:
        out["m3"] = {
            "rc_hit": int(m.group("rc_hit")),
            "prefetch_hit": int(m.group("prefetch_hit")),
            "prefetch_miss": int(m.group("prefetch_miss")),
            "fallback": {
                "density": int(m.group("fb_density")),
                "deep": int(m.group("fb_deep")),
                "aquifer": int(m.group("fb_aquifer")),
                "surface": int(m.group("fb_surface")),
            },
            "deep_memo": {"hit": int(m.group("deep_hit")), "miss": int(m.group("deep_miss"))},
            "aqlv": {"hit": int(m.group("aqlv_hit")), "miss": int(m.group("aqlv_miss"))},
            "prelim": {"hit": int(m.group("prelim_hit")), "miss": int(m.group("prelim_miss"))},
        }

    m = _M3_M4_RE.search(stderr)
    if m:
        out["m4"] = {
            "sections": {"worker": int(m.group("worker")), "owner": int(m.group("owner"))},
            "evicted_unconsumed": int(m.group("evicted")),
        }

    m = _M3_M6_RE.search(stderr)
    if m:
        out["m6"] = {"vec": int(m.group("vec")), "bail": int(m.group("bail"))}

    m = _M3_M5_RE.search(stderr)
    if m:
        def stage(name: str) -> Dict[str, float]:
            return {"ms": float(m.group(name + "_ms")), "n": int(m.group(name + "_n"))}

        out["m5"] = {
            "cols": stage("cols"),
            "beard": stage("beard"),
            "fill": stage("fill"),
            "carver": stage("carver"),
            "feature": stage("feature"),
            "aquifer_cells": int(m.group("aquifer_cells")),
            "surface_cells": int(m.group("surface_cells")),
        }

    return out


# ---------------------------------------------------------------------------
# Small pure helpers (also unit-tested directly).
# ---------------------------------------------------------------------------

class BenchError(Exception):
    """Expected, reportable harness failure."""


def parse_arms(spec: str) -> List[str]:
    arms = [a.strip().lower() for a in spec.split(",") if a.strip()]
    if not arms:
        raise BenchError("--arms selected no arms")
    unknown = [a for a in arms if a not in ARMS]
    if unknown:
        raise BenchError("unknown arm(s): %s (valid: %s)" % (", ".join(unknown), ", ".join(ARMS)))
    # de-duplicate, preserving the user's order
    return list(dict.fromkeys(arms))


def parse_jobs(spec: str) -> List[int]:
    jobs: List[int] = []
    for part in spec.split(","):
        part = part.strip()
        if not part:
            continue
        try:
            value = int(part)
        except ValueError:
            raise BenchError("--jobs expects integers, got %r" % (part,)) from None
        if value <= 0:
            raise BenchError("--jobs values must be positive, got %d" % (value,))
        if value not in jobs:
            jobs.append(value)
    if not jobs:
        raise BenchError("--jobs selected no worker counts")
    return jobs


def parse_expected_sha(spec: str, dump_n: int) -> Tuple[Dict[str, str], bool]:
    """Return (expected digests, applicable).

    ``default`` loads the recorded N=8 seed-0 digests; those are not applicable
    to another window size, in which case verification is skipped (reported)
    rather than producing a guaranteed failure.  An explicit JSON object or a
    path to one is always applied.
    """
    is_default = spec.strip().lower() in ("", "default")
    if is_default:
        return dict(EXPECTED_SHA_DEFAULT), dump_n == 8
    if spec.lstrip().startswith("{"):
        try:
            raw = json.loads(spec)
        except json.JSONDecodeError as exc:
            raise BenchError("--expected-sha inline JSON is invalid: %s" % (exc,)) from None
    else:
        path = Path(spec).expanduser()
        try:
            raw = json.loads(path.read_text())
        except (OSError, json.JSONDecodeError) as exc:
            raise BenchError("cannot read --expected-sha JSON %s: %s" % (path, exc)) from None
    if not isinstance(raw, dict):
        raise BenchError("--expected-sha JSON must be an object mapping arm to sha256")
    expected: Dict[str, str] = {}
    for key, value in raw.items():
        key = str(key).strip().lower()
        if not isinstance(value, str) or not _SHA_RE.match(value):
            raise BenchError("--expected-sha value for %r is not a 64-hex sha256" % (key,))
        expected[key] = value.lower()
    return expected, True


def summarize(values: Iterable[float]) -> Dict[str, float]:
    vals = [float(v) for v in values]
    if not vals:
        return {"n": 0}
    lo, hi = min(vals), max(vals)
    return {
        "n": len(vals),
        "min": lo,
        "max": hi,
        "median": statistics.median(vals),
        "mean": statistics.fmean(vals),
        "spread": hi - lo,
    }


def ab_delta(base: Optional[float], candidate: Optional[float]) -> Dict[str, Optional[float]]:
    """A/B summary.  Negative delta_pct and speedup > 1 mean candidate is faster."""
    if base is None or candidate is None or base == 0:
        return {"base": base, "candidate": candidate, "delta_pct": None, "speedup": None}
    return {
        "base": base,
        "candidate": candidate,
        "delta_pct": (candidate - base) / base * 100.0,
        "speedup": base / candidate,
    }


def host_cpu_info() -> Dict[str, Any]:
    info: Dict[str, Any] = {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "processor": platform.processor(),
        "cpu_count": os.cpu_count(),
    }
    try:
        info["available_parallelism"] = os.process_cpu_count()
    except AttributeError:  # Python < 3.13
        pass
    try:
        with open("/proc/cpuinfo", encoding="utf-8", errors="replace") as fh:
            for line in fh:
                if line.lower().startswith("model name"):
                    info["model_name"] = line.split(":", 1)[1].strip()
                    break
    except OSError:
        pass
    return info


def git_rev(repo_root: Path) -> str:
    try:
        proc = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=str(repo_root),
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            check=False,
        )
    except OSError:
        return "unknown"
    return proc.stdout.strip() if proc.returncode == 0 else "unknown"


def systemd_prefix(cpu_quota: Optional[str], memory_max: Optional[str]) -> List[str]:
    """Build the systemd-run scope wrapper, or fail cleanly if unavailable."""
    if not cpu_quota and not memory_max:
        return []
    if shutil.which("systemd-run") is None:
        raise BenchError(
            "--cpu-quota/--memory-max require systemd-run, which is not on PATH"
        )
    cmd = ["systemd-run", "--user", "--scope"]
    if cpu_quota:
        quota = cpu_quota if "%" in cpu_quota else cpu_quota + "%"
        cmd += ["-p", "CPUQuota=" + quota]
    if memory_max:
        cmd += ["-p", "MemoryMax=" + memory_max]
    cmd.append("--collect")
    return cmd


def _child_cpu_seconds() -> float:
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    return usage.ru_utime + usage.ru_stime


# ---------------------------------------------------------------------------
# Execution.
# ---------------------------------------------------------------------------

def run_one(
    binary: str,
    label: str,
    arm: str,
    jobs: int,
    round_no: int,
    dump_n: int,
    tmpdir: Path,
    wrap: Sequence[str],
    profile: bool,
    base_env: Optional[Mapping[str, str]] = None,
) -> Dict[str, Any]:
    """Run one dump, hash and delete it, and return timing/parsed metadata."""
    env = dict(base_env if base_env is not None else os.environ)
    env.update(ARM_ENV[arm])
    env["MC_SPINE_WG_PROFILE"] = "0"
    env["MC_SPINE_WG_DUMP_N"] = str(dump_n)
    env["MC_SPINE_WG_DUMP_X"] = "0"
    env["MC_SPINE_WG_DUMP_Z"] = "0"
    env["MC_SPINE_GEN_JOBS"] = str(jobs)
    if profile:
        env["MC_SPINE_M3_PROFILE"] = "1"
    else:
        env.pop("MC_SPINE_M3_PROFILE", None)

    fd, dump_path = tempfile.mkstemp(
        prefix="perf-%s-r%d-%s-j%d-" % (label, round_no, arm, jobs),
        suffix=".bin",
        dir=str(tmpdir),
    )
    os.close(fd)
    env["MC_SPINE_WG_DUMP"] = dump_path

    cmd = list(wrap) + [binary]
    cpu_before = _child_cpu_seconds()
    t0 = time.monotonic()
    try:
        proc = subprocess.run(
            cmd,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            errors="replace",
            check=False,
        )
        wall = time.monotonic() - t0
        cpu = _child_cpu_seconds() - cpu_before

        digest = hashlib.sha256()
        with open(dump_path, "rb") as fh:
            for block in iter(lambda: fh.read(1 << 20), b""):
                digest.update(block)
        size = os.path.getsize(dump_path)
    finally:
        try:
            os.unlink(dump_path)
        except OSError:
            pass

    if proc.returncode != 0:
        tail = "\n".join(proc.stderr.strip().splitlines()[-12:])
        raise BenchError(
            "%s failed (exit %d) for arm=%s jobs=%d:\n%s"
            % (binary, proc.returncode, arm, jobs, tail)
        )

    phase = parse_wgdump(proc.stderr)
    return {
        "round": round_no,
        "arm": arm,
        "jobs": jobs,
        "binary": label,
        "binary_path": binary,
        "profile": profile,
        "wall_s": wall,
        "phase_s": phase["seconds"] if phase else None,
        "cpu_s": cpu,
        "dump_bytes": size,
        "sha256": digest.hexdigest(),
        "phase": phase,
        "eval_c": parse_eval_calls(proc.stderr),
        "counters": parse_m3(proc.stderr) if profile else None,
    }


def build_summary(
    samples: Sequence[Mapping[str, Any]],
    arms: Sequence[str],
    jobs_list: Sequence[int],
    labels: Sequence[str],
) -> Dict[str, Any]:
    summary: Dict[str, Any] = {}
    for label in labels:
        summary[label] = {}
        for arm in arms:
            summary[label][arm] = {}
            for jobs in jobs_list:
                group = [
                    s
                    for s in samples
                    if s["binary"] == label and s["arm"] == arm and s["jobs"] == jobs
                    and not s["profile"]
                ]
                summary[label][arm][str(jobs)] = {
                    "wall": summarize(s["wall_s"] for s in group),
                    "phase": summarize(s["phase_s"] for s in group if s["phase_s"] is not None),
                    "cpu": summarize(s["cpu_s"] for s in group),
                    "sha256": sorted({s["sha256"] for s in group}),
                }
    return summary


def markdown_table(result: Mapping[str, Any]) -> str:
    """A README-ready table of medians (interleaved samples, spread = max-min)."""
    arms = result["config"]["arms"]
    jobs_list = result["config"]["jobs"]
    labels = result["config"]["binaries"]
    summary = result["summary"]
    ab = result["ab"]
    dump_n = result["config"]["dump_n"]
    chunks = dump_n * dump_n
    header = [
        "workload", "jobs", "binary", "wall median (s)", "wall spread (s)",
        "phase median (s)", "cpu median (s)", "ms/chunk", "sha256",
    ]
    if "candidate" in labels:
        header += ["Δ wall %", "speedup"]
    lines = ["| " + " | ".join(header) + " |", "|" + "|".join(["---"] * len(header)) + "|"]

    def fmt(value: Any, digits: int = 3) -> str:
        return "—" if value is None else ("%.*f" % (digits, value))

    for label in labels:
        for arm in arms:
            for jobs in jobs_list:
                cell = summary[label][arm][str(jobs)]
                wall = cell["wall"].get("median")
                phase = cell["phase"].get("median")
                cpu = cell["cpu"].get("median")
                sha = cell["sha256"][0][:12] if cell["sha256"] else "—"
                row = [
                    arm, str(jobs), label, fmt(wall), fmt(cell["wall"].get("spread")),
                    fmt(phase), fmt(cpu), fmt(wall * 1000.0 / chunks, 1) if wall is not None else "—",
                    sha,
                ]
                if "candidate" in labels:
                    if label == "candidate":
                        delta = ab.get(arm, {}).get(str(jobs), {}).get("wall", {})
                        row += [
                            fmt(delta.get("delta_pct"), 1),
                            fmt(delta.get("speedup"), 2),
                        ]
                    else:
                        row += ["—", "—"]
                lines.append("| " + " | ".join(row) + " |")

    counters = result.get("counters", {})
    if counters:
        lines += ["", "### M1–M6 counters (one profile pass per cell)", ""]
        cheader = [
            "workload", "jobs", "binary", "M1 starts o/w", "M2 evict",
            "M4 worker sections", "M6 vec/bail", "M5 cols ms/n",
        ]
        lines.append("| " + " | ".join(cheader) + " |")
        lines.append("|" + "|".join(["---"] * len(cheader)) + "|")
        for label in labels:
            for arm in arms:
                for jobs in jobs_list:
                    c = counters.get(label, {}).get(arm, {}).get(str(jobs), {})
                    m1 = c.get("m1", {}).get("chunk_starts", {})
                    m2 = c.get("m2", {})
                    m4 = c.get("m4", {})
                    m6 = c.get("m6", {})
                    m5 = c.get("m5", {}).get("cols", {})
                    pair = lambda d: "%s/%s" % (d.get("owner", "—"), d.get("worker", "—"))
                    lines.append(
                        "| %s | %d | %s | %s | %s | %s | %s/%s | %s/%s |"
                        % (
                            arm, jobs, label,
                            pair(m1),
                            m2.get("evict", "—"),
                            m4.get("sections", {}).get("worker", "—"),
                            m6.get("vec", "—"), m6.get("bail", "—"),
                            m5.get("ms", "—"), m5.get("n", "—"),
                        )
                    )
    return "\n".join(lines)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Reproducible worldgen 64-chunk dump benchmark + M1-M6 counter harness.",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    parser.add_argument("--binary", default=DEFAULT_BINARY, help="binary under test")
    parser.add_argument("--arms", default=",".join(ARMS), help="comma-separated subset of full,nofeat,terrain")
    parser.add_argument("--jobs", default="1,5,8", help="comma-separated MC_SPINE_GEN_JOBS values")
    parser.add_argument("--rounds", type=int, default=3, help="interleaved rounds per (arm, jobs) pair")
    parser.add_argument("--dump-n", type=int, default=8, help="dump window N (N x N chunks)")
    parser.add_argument(
        "--profile-counters", action="store_true",
        help="also run one pass per cell with MC_SPINE_M3_PROFILE=1 and parse M1-M6",
    )
    parser.add_argument(
        "--compare-binary", metavar="PATH",
        help="second binary, interleaved with the first as base vs candidate",
    )
    parser.add_argument(
        "--expected-sha", metavar="JSON_OR_DEFAULT", default="default",
        help="digest map (inline JSON or file path), or 'default' for the recorded N=8 digests",
    )
    parser.add_argument(
        "--no-verify-sha", action="store_true", help="do not fail on a dump digest mismatch",
    )
    parser.add_argument("--cpu-quota", metavar="PCT", help="wrap runs in systemd-run with CPUQuota=PCT")
    parser.add_argument("--memory-max", metavar="SIZE", help="wrap runs in systemd-run with MemoryMax=SIZE")
    parser.add_argument("--tmpdir", metavar="DIR", help="dump scratch directory (default <repo>/target/tmp)")
    parser.add_argument("--json", metavar="PATH", help="write a machine-readable JSON summary")
    parser.add_argument("--markdown", action="store_true", help="print a README-ready Markdown table to stdout")
    return parser


def main(argv: Optional[Sequence[str]] = None, base_env: Optional[Mapping[str, str]] = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        arms = parse_arms(args.arms)
        jobs_list = parse_jobs(args.jobs)
        if args.rounds < 1:
            raise BenchError("--rounds must be >= 1")
        if args.dump_n < 1:
            raise BenchError("--dump-n must be >= 1")

        expected, sha_applicable = parse_expected_sha(args.expected_sha, args.dump_n)
        verify = sha_applicable and not args.no_verify_sha
        if not sha_applicable and not args.no_verify_sha:
            print(
                "[perf_bench] note: recorded digests are for N=8; skipping sha check at N=%d"
                % args.dump_n,
                file=sys.stderr,
            )

        binaries = {"base": args.binary}
        if args.compare_binary:
            binaries["candidate"] = args.compare_binary
        labels = list(binaries)
        for label, path in binaries.items():
            if not Path(path).is_file():
                raise BenchError("%s binary %r is not a file" % (label, path))

        repo_root = Path(__file__).resolve().parent.parent
        tmpdir = Path(args.tmpdir).expanduser() if args.tmpdir else repo_root / "target" / "tmp"
        if not tmpdir.is_absolute():
            tmpdir = Path.cwd() / tmpdir
        tmpdir.mkdir(parents=True, exist_ok=True)

        wrap = systemd_prefix(args.cpu_quota, args.memory_max)

        def check_sha(sample: Mapping[str, Any]) -> None:
            want = expected.get(sample["arm"])
            if want and verify and sample["sha256"] != want:
                raise BenchError(
                    "sha256 mismatch for arm=%s jobs=%d %s: got %s, expected %s"
                    % (
                        sample["arm"], sample["jobs"], sample["binary"],
                        sample["sha256"], want,
                    )
                )

        samples: List[Dict[str, Any]] = []
        for r in range(1, args.rounds + 1):
            for arm in arms:
                for jobs in jobs_list:
                    for label, path in binaries.items():
                        s = run_one(path, label, arm, jobs, r, args.dump_n, tmpdir, wrap, False, base_env)
                        check_sha(s)
                        samples.append(s)
                        print(
                            "[perf_bench] r%d/%d %s j%d %s wall=%.3fs phase=%s cpu=%.3fs"
                            % (
                                r, args.rounds, arm, jobs, label, s["wall_s"],
                                "—" if s["phase_s"] is None else "%.3fs" % s["phase_s"],
                                s["cpu_s"],
                            ),
                            file=sys.stderr,
                        )

        counters: Dict[str, Any] = {label: {arm: {} for arm in arms} for label in labels}
        if args.profile_counters:
            for arm in arms:
                for jobs in jobs_list:
                    for label, path in binaries.items():
                        s = run_one(path, label, arm, jobs, 0, args.dump_n, tmpdir, wrap, True, base_env)
                        check_sha(s)
                        counters[label][arm][str(jobs)] = s["counters"] or {}
                        print(
                            "[perf_bench] profile %s j%d %s: parsed %d M3 groups"
                            % (arm, jobs, label, len(s["counters"] or {})),
                            file=sys.stderr,
                        )

        summary = build_summary(samples, arms, jobs_list, labels)
        ab: Dict[str, Any] = {}
        if "candidate" in binaries:
            for arm in arms:
                ab[arm] = {}
                for jobs in jobs_list:
                    key = str(jobs)
                    b = summary["base"][arm][key]
                    c = summary["candidate"][arm][key]
                    ab[arm][key] = {
                        metric: ab_delta(b[metric].get("median"), c[metric].get("median"))
                        for metric in ("wall", "phase", "cpu")
                    }

        result = {
            "tool": "perf_bench",
            "schema": 1,
            "git_rev": git_rev(repo_root),
            "host": host_cpu_info(),
            "config": {
                "arms": arms,
                "jobs": jobs_list,
                "rounds": args.rounds,
                "dump_n": args.dump_n,
                "verify_sha": verify,
                "expected_sha": expected if verify else {},
                "cpu_quota": args.cpu_quota,
                "memory_max": args.memory_max,
                "systemd_wrap": wrap,
                "tmpdir": str(tmpdir),
                "binaries": labels,
                "binary_paths": binaries,
            },
            "samples": samples,
            "summary": summary,
            "counters": counters,
            "ab": ab,
        }

        if args.json:
            path = Path(args.json).expanduser()
            if not path.is_absolute():
                path = Path.cwd() / path
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
            print("[perf_bench] json -> %s" % (path,), file=sys.stderr)
        if args.markdown:
            print(markdown_table(result))
        return 0
    except BenchError as exc:
        print("[perf_bench] error: %s" % (exc,), file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
