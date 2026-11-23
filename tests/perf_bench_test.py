#!/usr/bin/env python3
"""Hermetic unit tests for tools/perf_bench.py.

No cargo, no release binary and no network: a tiny Python script stands in for
``megu``.  It writes a deterministic dump, prints a real-format ``[wgdump]``
line, logs its invocation order, and emits the exact ``[m3]`` lines that
``worldgen::m3_report()`` produces.

Run with::

    PYTHONDONTWRITEBYTECODE=1 python3 tests/perf_bench_test.py
"""

from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
_spec = importlib.util.spec_from_file_location("perf_bench", ROOT / "tools" / "perf_bench.py")
pb = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(pb)

# 64-chunk seed-0 Overworld digests of current main 28e5977, proven
# bit-identical across Wave 5 a82df1a/cfffac7 and Wave 7A cb26645/92a9749.
PINNED_SHA_DEFAULT = {
    "full": "368ab50dc931faec43469cd4fd9620f56c1f8c0931f0708c3bfd9e9b3e8297b7",
    "nofeat": "16dd14904130aa56a887cb4fb7e18b2492784cb147a08325cdc46df64f91b766",
    "terrain": "c7fa585e8ec40df5825325c6d39fc6f6d17d5c01e3e80f2f4e4b3bae9b50c3e0",
}

# Superseded defaults that must no longer be pinned.
STALE_SHA_DEFAULT = (
    "670fdfe3472812afeeba823a30e08f472ea8eb69965d6ba0c54b30a2e9949def",
    "57101c434d49edf21f7efdd4207beca8cae34ff74cbf4ffdc009b680cb5d5a48",
    "bb499dfb6c53128734a23f8722534a5f1882e7acbb53a9885a604ce74352beff",
)

# Copied verbatim from the m3_report() format in src/worldgen.rs (with plausible
# numbers from evidence/foundation/performance/README.md).
M3_STDERR = """[m3] terrain_store_bound=512 jobs=5
[m3] M1 chunk_starts owner=170 worker=111 | cols built owner=303 worker=117 hit owner=133095 worker=54 | beard resolved owner=164 worker=10
[m3] M2 terr_c hit=1536 miss=0 evict=1024 install=1536 install_dup=0 bound=512
[m3] M3 feature reads rc_hit=1 prefetch_hit=178699 prefetch_miss=978534 | fallback density=2 deep=3 aquifer=4 surface=5 | deep_memo hit=10985 miss=665889 aqlv hit=277262 miss=1419 prelim hit=1903 miss=9
[m3] M4 sections worker=1536 owner=0 evicted_unconsumed=0
[m3] M6 df4 vec=7726111 bail=38227
[m3] M5 cols=6706.0ms/481 beard=3282.8ms/234 fill=1446.5ms/1536 aquifer_cells=5296614 surface_cells=39 carver=1864.4ms/1089 feature=632.6ms/121
"""

FAKE_TEMPLATE = '''#!/usr/bin/env python3
"""Stand-in for target/release/megu: no Rust, no cargo, <1 ms."""
import os, sys
TAG = __TAG__
log = os.environ.get("PERF_FAKE_LOG")
arm = "full"
if os.environ.get("MEGU_NOFEAT"):
    arm = "nofeat"
    if os.environ.get("MEGU_NOCARVE"):
        arm = "terrain"
jobs = os.environ.get("MEGU_GEN_JOBS", "?")
if log:
    with open(log, "a", encoding="utf-8") as fh:
        fh.write("%s|%s|%s\\n" % (TAG, arm, jobs))
n = int(os.environ.get("MEGU_WG_DUMP_N", "8"))
size = 64 * n * n
data = bytes(((i * 31 + len(arm)) & 0xff) for i in range(size))
with open(os.environ["MEGU_WG_DUMP"], "wb") as fh:
    fh.write(data)
dur = os.environ.get("PERF_FAKE_DUR_" + TAG, os.environ.get("PERF_FAKE_DUR", "1.500s"))
sys.stderr.write("[wgdump] %d chunks -> dump.bin (%d bytes) in %s\\n" % (n * n, size, dur))
if os.environ.get("MEGU_M3_PROFILE"):
    sys.stderr.write(__M3_TEXT__)
'''


def dump_bytes(arm: str, n: int = 8) -> bytes:
    """Same bytes the fake binary writes, for expected-sha tests."""
    size = 64 * n * n
    return bytes(((i * 31 + len(arm)) & 0xFF) for i in range(size))


class PerfBenchTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="perf-bench-test-")
        self.root = Path(self._tmp.name)
        self.dumps = self.root / "dumps"
        self.dumps.mkdir()

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def fake_binary(self, name: str, tag: str) -> str:
        path = self.root / name
        path.write_text(
            FAKE_TEMPLATE.replace("__TAG__", repr(tag)).replace("__M3_TEXT__", repr(M3_STDERR)),
            encoding="utf-8",
        )
        path.chmod(0o755)
        return str(path)

    def invoke(self, argv, extra_env=None):
        env = dict(os.environ)
        env["PYTHONDONTWRITEBYTECODE"] = "1"
        env.update(extra_env or {})
        out, err = io.StringIO(), io.StringIO()
        with patch.dict(os.environ, env, clear=True), contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = pb.main(argv)
        return code, out.getvalue(), err.getvalue()

    def base_argv(self, binary, *extra):
        return [
            "--binary", binary,
            "--tmpdir", str(self.dumps),
            "--no-verify-sha",
            *extra,
        ]

    # ------------------------------------------------------------------
    # Parsing of the real stderr formats.
    # ------------------------------------------------------------------

    def test_parse_wgdump_real_output(self):
        for text, want in (
            ("8.273123s", 8.273123),
            ("557.3421ms", 0.5573421),
            ("1.234µs", 1.234e-6),
            ("12ns", 12e-9),
        ):
            self.assertAlmostEqual(pb.parse_duration_seconds(text), want, places=12)
        stderr = (
            "[wgprof] 8x8 chunks in 8.30s (129.7 ms/chunk)\n"
            "[wgprof] eval_c calls: 114688 (1792 per chunk)\n"
            "[wgdump] 64 chunks -> /tmp/megu/target/tmp/d.bin "
            "(25165824 bytes) in 8.273123s\n"
        )
        parsed = pb.parse_wgdump(stderr)
        self.assertEqual(parsed["chunks"], 64)
        self.assertEqual(parsed["bytes"], 25165824)
        self.assertAlmostEqual(parsed["seconds"], 8.273123, places=6)
        self.assertEqual(pb.parse_eval_calls(stderr), {"calls": 114688, "per_chunk": 1792})
        self.assertIsNone(pb.parse_wgdump("[m3] counters only\n"))

    def test_parse_m3_real_counter_lines(self):
        counters = pb.parse_m3(M3_STDERR)
        self.assertEqual(counters["header"], {"bound": 512, "jobs": 5})
        self.assertEqual(counters["m1"]["chunk_starts"], {"owner": 170, "worker": 111})
        self.assertEqual(counters["m1"]["cols_hit"], {"owner": 133095, "worker": 54})
        self.assertEqual(counters["m1"]["beard_resolved"]["worker"], 10)
        self.assertEqual(counters["m2"]["evict"], 1024)
        self.assertEqual(counters["m2"]["install_dup"], 0)
        self.assertEqual(counters["m3"]["fallback"], {"density": 2, "deep": 3, "aquifer": 4, "surface": 5})
        self.assertEqual(counters["m3"]["deep_memo"], {"hit": 10985, "miss": 665889})
        self.assertEqual(counters["m4"]["sections"], {"worker": 1536, "owner": 0})
        self.assertEqual(counters["m4"]["evicted_unconsumed"], 0)
        self.assertEqual(counters["m6"], {"vec": 7726111, "bail": 38227})
        self.assertEqual(counters["m5"]["cols"], {"ms": 6706.0, "n": 481})
        self.assertEqual(counters["m5"]["aquifer_cells"], 5296614)
        self.assertEqual(counters["m5"]["surface_cells"], 39)
        self.assertEqual(counters["m5"]["feature"], {"ms": 632.6, "n": 121})
        self.assertEqual(pb.parse_m3(""), {})

    # ------------------------------------------------------------------
    # Scheduling, aggregation and A/B math.
    # ------------------------------------------------------------------

    def test_interleaved_execution_order(self):
        fake = self.fake_binary("fake.py", "BASE")
        log = self.root / "order.log"
        out_json = self.root / "order.json"
        code, _, _ = self.invoke(
            self.base_argv(
                fake, "--arms", "full,nofeat,terrain", "--jobs", "1,5",
                "--rounds", "2", "--json", str(out_json),
            ),
            {"PERF_FAKE_LOG": str(log)},
        )
        self.assertEqual(code, 0)
        wanted = [
            "BASE|%s|%d" % (arm, jobs)
            for _ in range(2)
            for arm in ("full", "nofeat", "terrain")
            for jobs in (1, 5)
        ]
        self.assertEqual(log.read_text().strip().splitlines(), wanted)
        doc = json.loads(out_json.read_text())
        self.assertEqual(
            [(s["arm"], s["jobs"], s["round"]) for s in doc["samples"]],
            [(arm, jobs, r) for r in (1, 2) for arm in ("full", "nofeat", "terrain") for jobs in (1, 5)],
        )
        # no leftover dump files after hashing
        self.assertEqual(list(self.dumps.glob("*.bin")), [])

    def test_median_spread_and_json_summary(self):
        s = pb.summarize([1.0, 3.0, 2.0])
        self.assertEqual(s["n"], 3)
        self.assertEqual(s["min"], 1.0)
        self.assertEqual(s["max"], 3.0)
        self.assertEqual(s["median"], 2.0)
        self.assertEqual(s["spread"], 2.0)
        even = pb.summarize([4.0, 1.0, 3.0, 2.0])
        self.assertEqual(even["median"], 2.5)
        self.assertEqual(even["spread"], 3.0)
        self.assertEqual(pb.summarize([]), {"n": 0})

        fake = self.fake_binary("fake.py", "BASE")
        out_json = self.root / "summary.json"
        code, _, _ = self.invoke(
            self.base_argv(fake, "--arms", "full", "--jobs", "1", "--rounds", "3", "--json", str(out_json), "--markdown"),
            {"PERF_FAKE_DUR": "2.5s"},
        )
        self.assertEqual(code, 0)
        doc = json.loads(out_json.read_text())
        cell = doc["summary"]["base"]["full"]["1"]
        self.assertEqual(cell["wall"]["n"], 3)
        self.assertEqual(cell["phase"]["median"], 2.5)
        self.assertEqual(cell["phase"]["spread"], 0.0)
        self.assertEqual(cell["cpu"]["n"], 3)
        self.assertEqual(doc["samples"][0]["dump_bytes"], 64 * 64)
        self.assertEqual(doc["config"]["dump_n"], 8)

    def test_ab_delta_and_interleaved_pair(self):
        d = pb.ab_delta(10.0, 8.0)
        self.assertAlmostEqual(d["delta_pct"], -20.0)
        self.assertAlmostEqual(d["speedup"], 1.25)
        self.assertIsNone(pb.ab_delta(0.0, 5.0)["delta_pct"])

        base = self.fake_binary("base.py", "BASE")
        cand = self.fake_binary("cand.py", "CAND")
        log = self.root / "ab.log"
        out_json = self.root / "ab.json"
        code, out, _ = self.invoke(
            self.base_argv(
                base, "--compare-binary", cand, "--arms", "full", "--jobs", "1",
                "--rounds", "2", "--json", str(out_json), "--markdown",
            ),
            {
                "PERF_FAKE_LOG": str(log),
                "PERF_FAKE_DUR_BASE": "2.0s",
                "PERF_FAKE_DUR_CAND": "1.0s",
            },
        )
        self.assertEqual(code, 0)
        self.assertEqual(log.read_text().strip().splitlines(), ["BASE|full|1", "CAND|full|1"] * 2)
        doc = json.loads(out_json.read_text())
        delta = doc["ab"]["full"]["1"]["phase"]
        self.assertAlmostEqual(delta["delta_pct"], -50.0)
        self.assertAlmostEqual(delta["speedup"], 2.0)
        self.assertIn("| workload | jobs | binary |", out)
        self.assertIn("speedup", out)

    # ------------------------------------------------------------------
    # Fail-closed digest verification and the profile pass.
    # ------------------------------------------------------------------

    def test_default_expected_sha_pins_current_main(self):
        # 64-chunk seed-0 Overworld digests from main 28e5977 (bit-identical
        # across Wave 5 a82df1a/cfffac7 and Wave 7A cb26645/92a9749).
        self.assertEqual(pb.EXPECTED_SHA_DEFAULT, PINNED_SHA_DEFAULT)
        self.assertEqual(set(pb.EXPECTED_SHA_DEFAULT), set(pb.ARMS))
        for arm, digest in pb.EXPECTED_SHA_DEFAULT.items():
            self.assertRegex(digest, r"^[0-9a-f]{64}$", arm)
        self.assertEqual(len(set(pb.EXPECTED_SHA_DEFAULT.values())), len(pb.ARMS))
        for stale in STALE_SHA_DEFAULT:
            self.assertNotIn(stale, pb.EXPECTED_SHA_DEFAULT.values())

        for spec in ("default", "DEFAULT", " default ", ""):
            expected, applicable = pb.parse_expected_sha(spec, 8)
            self.assertEqual(expected, PINNED_SHA_DEFAULT)
            self.assertTrue(applicable)
        expected, applicable = pb.parse_expected_sha("default", 4)
        self.assertEqual(expected, PINNED_SHA_DEFAULT)
        self.assertFalse(applicable)
        # callers get a copy; the module constant cannot be mutated through it
        expected["full"] = "0" * 64
        self.assertEqual(pb.EXPECTED_SHA_DEFAULT["full"], PINNED_SHA_DEFAULT["full"])

    def test_default_expected_sha_end_to_end(self):
        fake = self.fake_binary("fake.py", "BASE")
        # N=8: the pinned digests are applied (and the fake's bytes do not match)
        out_json = self.root / "default8.json"
        code, _, err = self.invoke(
            ["--binary", fake, "--tmpdir", str(self.dumps), "--arms", "terrain",
             "--jobs", "1", "--rounds", "1", "--json", str(out_json)],
        )
        self.assertEqual(code, 1)
        self.assertIn("sha256 mismatch for arm=terrain", err)
        self.assertIn("expected " + PINNED_SHA_DEFAULT["terrain"], err)
        self.assertFalse(out_json.exists())

        # --no-verify-sha: run succeeds, config records no verification
        code, _, _ = self.invoke(
            self.base_argv(fake, "--arms", "full", "--jobs", "1", "--rounds", "1", "--json", str(out_json)),
        )
        self.assertEqual(code, 0)
        doc = json.loads(out_json.read_text())
        self.assertFalse(doc["config"]["verify_sha"])
        self.assertEqual(doc["config"]["expected_sha"], {})

        # N != 8: recorded digests are skipped with a note, not failed
        out4 = self.root / "default4.json"
        code, _, err = self.invoke(
            ["--binary", fake, "--tmpdir", str(self.dumps), "--arms", "full",
             "--jobs", "1", "--rounds", "1", "--dump-n", "4", "--json", str(out4)],
        )
        self.assertEqual(code, 0)
        self.assertIn("skipping sha check at N=4", err)
        doc = json.loads(out4.read_text())
        self.assertFalse(doc["config"]["verify_sha"])
        self.assertEqual(doc["samples"][0]["dump_bytes"], 64 * 4 * 4)

        # the pinned map, passed explicitly, is what config records when verifying
        good = hashlib.sha256(dump_bytes("full")).hexdigest()
        pinned = dict(PINNED_SHA_DEFAULT, full=good)
        ok_json = self.root / "pinned.json"
        code, _, _ = self.invoke(
            ["--binary", fake, "--tmpdir", str(self.dumps), "--arms", "full",
             "--jobs", "1", "--rounds", "1", "--expected-sha", json.dumps(pinned),
             "--json", str(ok_json)],
        )
        self.assertEqual(code, 0)
        doc = json.loads(ok_json.read_text())
        self.assertTrue(doc["config"]["verify_sha"])
        self.assertEqual(doc["config"]["expected_sha"], pinned)

    def test_sha256_mismatch_is_fail_closed(self):
        fake = self.fake_binary("fake.py", "BASE")
        good = hashlib.sha256(dump_bytes("full")).hexdigest()
        expected = json.dumps({"full": good, "nofeat": "0" * 64, "terrain": "0" * 64})

        ok_json = self.root / "ok.json"
        code, _, _ = self.invoke(
            ["--binary", fake, "--tmpdir", str(self.dumps), "--arms", "full",
             "--jobs", "1", "--rounds", "1", "--expected-sha", expected, "--json", str(ok_json)],
        )
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(ok_json.read_text())["samples"][0]["sha256"], good)

        bad_json = self.root / "bad.json"
        code, _, err = self.invoke(
            ["--binary", fake, "--tmpdir", str(self.dumps), "--arms", "full",
             "--jobs", "1", "--rounds", "1", "--expected-sha", json.dumps({"full": "0" * 64}),
             "--json", str(bad_json)],
        )
        self.assertEqual(code, 1)
        self.assertIn("sha256 mismatch", err)
        self.assertFalse(bad_json.exists())

        # explicit opt-out permits the same bad digest
        code, _, _ = self.invoke(
            ["--binary", fake, "--tmpdir", str(self.dumps), "--arms", "full",
             "--jobs", "1", "--rounds", "1", "--expected-sha", json.dumps({"full": "0" * 64}),
             "--no-verify-sha"],
        )
        self.assertEqual(code, 0)

    def test_profile_counters_end_to_end(self):
        fake = self.fake_binary("fake.py", "BASE")
        out_json = self.root / "profile.json"
        code, _, err = self.invoke(
            self.base_argv(
                fake, "--arms", "full", "--jobs", "1", "--rounds", "1",
                "--profile-counters", "--json", str(out_json), "--markdown",
            ),
        )
        self.assertEqual(code, 0)
        self.assertIn("parsed 7 M3 groups", err)
        doc = json.loads(out_json.read_text())
        counters = doc["counters"]["base"]["full"]["1"]
        self.assertEqual(counters["m6"], {"vec": 7726111, "bail": 38227})
        self.assertEqual(counters["m1"]["chunk_starts"], {"owner": 170, "worker": 111})
        self.assertEqual(counters["m5"]["cols"], {"ms": 6706.0, "n": 481})
        self.assertTrue(doc["samples"][0]["profile"] is False)

    def test_help(self):
        env = dict(os.environ)
        env["PYTHONDONTWRITEBYTECODE"] = "1"
        proc = subprocess.run(
            [sys.executable, str(ROOT / "tools" / "perf_bench.py"), "--help"],
            capture_output=True, text=True, env=env, check=False,
        )
        self.assertEqual(proc.returncode, 0)
        for flag in (
            "--binary", "--arms", "--jobs", "--rounds", "--dump-n",
            "--profile-counters", "--compare-binary", "--expected-sha",
            "--no-verify-sha", "--cpu-quota", "--memory-max", "--tmpdir",
            "--json", "--markdown",
        ):
            self.assertIn(flag, proc.stdout)

    def test_pgo_build_script_cli(self):
        script = ROOT / "tools" / "pgo_build.sh"
        self.assertTrue(script.is_file(), "tools/pgo_build.sh must exist")
        self.assertTrue(
            os.access(script, os.X_OK), "tools/pgo_build.sh must be executable"
        )

        def run(argv):
            return subprocess.run(
                [str(script)] + argv, capture_output=True, text=True, check=False,
            )

        # --help is accepted and documents the supported flags.
        proc = run(["--help"])
        self.assertEqual(proc.returncode, 0)
        for flag in ("--out", "--dry-run", "--jobs"):
            self.assertIn(flag, proc.stdout)

        # Unknown flags are rejected (exit 2).
        proc = run(["--frobnicate"])
        self.assertEqual(proc.returncode, 2)
        self.assertIn("unknown argument", proc.stderr)

        # --dry-run parses arguments and reports the plan; it exits 0 when
        # llvm-profdata is discoverable, otherwise with the actionable message.
        proc = run(["--dry-run", "--out", "target/pgo/megu", "--jobs", "2"])
        if proc.returncode == 0:
            self.assertIn("target/pgo/megu", proc.stdout)
            self.assertIn("llvm-profdata", proc.stdout)
            self.assertIn("step 1 instrumented build", proc.stdout)
            self.assertIn("step 5", proc.stdout)
        else:
            self.assertEqual(proc.returncode, 1)
            self.assertIn("rustup component add llvm-tools", proc.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
