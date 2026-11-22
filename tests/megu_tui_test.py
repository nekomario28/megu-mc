#!/usr/bin/env python3
"""Regression tests for tools/megu_tui.py.

Covers sparkline rendering, the no-side-effects fallback detection, the
MetricsParser against realistic mc-spine log lines, deterministic frame
layout at several terminal sizes, and end-to-end plain-mode passthrough of
a stub subprocess (stdio forwarding + exact exit code preservation).
"""

import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
TOOL = ROOT / "tools" / "megu_tui.py"
SPEC = importlib.util.spec_from_file_location("megu_tui", TOOL)
megu_tui = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(megu_tui)

BLOCKS = megu_tui.SPARK_BLOCKS

REALISTIC_LOG = [
    "[config] loaded server.toml (profile default)",
    "mc-spine v0.6 listening 127.0.0.1:25599 (offline identities; no Mojang authentication)",
    "[live-mods] 5 jar(s): 1 auto-skipped, 2 native-bundle, 1 tier2-shim, 1 tier3",
    "[live-mods] dual-loader: 2 unified, 1 client-only, 0 superseded, 0 rejected, 0 duplicate ids, 0 missing deps, 3 verified damage hooks",
    "[live-mods] weirdmod.jar (weirdmod) tier3: 412 classes, 9 site(s) - mixin target missing",
    "[live-mods] liftcraft.jar (liftcraft) tier2 shim: 208 classes, 4 site(s) - damage hooks lifted",
    "[tick] observed_tps=19.97 avg_work_ms=8.21 max_work_ms=21.44 over_budget=0/100",
    "[spine] Alice joined from 127.0.0.1:51112 (uuid 0123456789abcdef0123456789abcdef)",
    "[spine] Bob joined from 127.0.0.1:51130 (uuid fedcba9876543210fedcba9876543210)",
    "[sz] t=400 VmRSS: 523264 kB | mobs=42 drops=7 xps=3 arrows=0 bob=1 pearl=0 primed=0 chests=18 furn=6 crops=11 world=0 inv=2 clients=2",
    "[tick] observed_tps=19.41 avg_work_ms=44.10 max_work_ms=58.02 over_budget=3/100",
    "[spine] Alice left (from 127.0.0.1:51112)",
    "Status: Busy (19.4 TPS, 44.1 ms/tick)",
    "Players: 2/20 (Bob, Carol)",
    "Loaded chunks: 1284",
    "Memory: 511 MB",
    "Mods: 2 native, 1 compatibility mode",
    "[security] velocity forwarding refused for Mallory from 10.0.0.9: missing forwarding secret",
    "[FATAL] tick thread panicked: boom; exiting so the server restarts",
]

STUB = (
    "import sys\n"
    "print('stub: ready', flush=True)\n"
    "line = sys.stdin.readline()\n"
    "print('stub: got ' + line.strip(), flush=True)\n"
    "sys.exit(7)\n"
)


class SparklineTest(unittest.TestCase):
    def test_empty_and_zero_width(self):
        self.assertEqual(megu_tui.sparkline([], 6), " " * 6)
        self.assertEqual(megu_tui.sparkline([], 0), "")
        self.assertEqual(megu_tui.sparkline([1.0, 2.0], 0), "")
        self.assertEqual(megu_tui.sparkline([1.0], -3), "")

    def test_flat_series_sits_on_one_rung(self):
        line = megu_tui.sparkline([5.0] * 6, 6)
        self.assertEqual(len(line), 6)
        self.assertEqual(len(set(line)), 1)
        self.assertNotEqual(line, " " * 6)

    def test_ramp_is_monotonic(self):
        line = megu_tui.sparkline(range(8), 8)
        self.assertEqual(line[0], BLOCKS[0])
        self.assertEqual(line[-1], BLOCKS[-1])
        indexes = [BLOCKS.index(char) for char in line]
        self.assertEqual(indexes, sorted(indexes))

    def test_spike_and_clamping(self):
        line = megu_tui.sparkline([1, 1, 1, 50, 1], 5, low=0, high=10)
        self.assertEqual(line[3], BLOCKS[-1])
        line = megu_tui.sparkline([-4, 0, 5, 20], 4, low=0, high=10)
        self.assertEqual(line[0], BLOCKS[0])
        self.assertEqual(line[1], BLOCKS[0])
        self.assertEqual(line[3], BLOCKS[-1])

    def test_nan_and_inf(self):
        line = megu_tui.sparkline(
            [float("nan"), float("inf"), float("-inf"), 5.0], 4, low=0, high=10)
        self.assertEqual(line[0], " ")
        self.assertEqual(line[1], BLOCKS[-1])
        self.assertEqual(line[2], BLOCKS[0])
        self.assertEqual(line[3], BLOCKS[3])

    def test_ascii_charset(self):
        line = megu_tui.sparkline([0.0, 7.0], 2, ascii_mode=True)
        self.assertTrue(line.isascii())
        self.assertEqual(line, megu_tui.SPARK_ASCII[0] + megu_tui.SPARK_ASCII[-1])

    def test_tail_window(self):
        line = megu_tui.sparkline(range(10), 4)
        self.assertEqual(len(line), 4)
        self.assertEqual(line[-1], BLOCKS[-1])

    def test_left_padding_for_short_series(self):
        line = megu_tui.sparkline([0.0, 7.0], 6)
        self.assertEqual(len(line), 6)
        self.assertEqual(line[:4], " " * 4)


class FallbackDetectionTest(unittest.TestCase):
    def test_non_tty_forces_plain(self):
        env = {"TERM": "xterm-256color"}
        self.assertFalse(megu_tui.tui_capable(env, False, True))
        self.assertFalse(megu_tui.tui_capable(env, True, False))
        self.assertFalse(megu_tui.tui_capable(env, False, False))

    def test_no_color_env_disables_tui(self):
        env = {"NO_COLOR": "1", "TERM": "xterm-256color"}
        self.assertFalse(megu_tui.tui_capable(env, True, True))

    def test_dumb_or_missing_term_disables_tui(self):
        for term in ("", "dumb", "unknown"):
            self.assertFalse(megu_tui.tui_capable({"TERM": term}, True, True))
        self.assertFalse(megu_tui.tui_capable({}, True, True))

    def test_real_tty_is_eligible(self):
        expected = (megu_tui.termios is not None and megu_tui.tty is not None
                    and megu_tui.select is not None and os.name != "nt")
        env = {"TERM": "xterm-256color"}
        self.assertEqual(megu_tui.tui_capable(env, True, True), expected)

    def test_color_policy(self):
        self.assertFalse(megu_tui.color_ok({"NO_COLOR": ""}))
        self.assertFalse(megu_tui.color_ok({"TERM": "dumb"}))
        self.assertFalse(megu_tui.color_ok({}))
        self.assertTrue(megu_tui.color_ok({"TERM": "xterm"}))


class MetricsParserTest(unittest.TestCase):
    def parse(self, lines=REALISTIC_LOG):
        parser = megu_tui.MetricsParser()
        for line in lines:
            parser.feed(line)
        return parser.snapshot()

    def test_realistic_session(self):
        snap = self.parse()
        self.assertTrue(snap["ready"])
        self.assertEqual(snap["version"], "0.6")
        self.assertEqual(snap["bind"], "127.0.0.1")
        self.assertEqual(snap["port"], 25599)
        self.assertIn(19.97, snap["tps"])
        self.assertAlmostEqual(snap["tps_cur"], 19.4)
        self.assertIn(8.21, snap["mspt"])
        self.assertAlmostEqual(snap["mspt_p95"], 44.1)
        self.assertAlmostEqual(snap["mem_cur"], 511.0)
        self.assertEqual(snap["players_online"], 2)
        self.assertEqual(snap["players_max"], 20)
        self.assertEqual(snap["players"], ["Bob", "Carol"])
        self.assertEqual(snap["loaded_chunks"], 1284)
        self.assertEqual(snap["mods"],
                         {"auto_skipped": 1, "native": 2, "shim": 1, "fallback": 1})
        self.assertEqual(snap["jars_seen"], 5)
        self.assertEqual(snap["dual_loader"]["unified"], 2)
        self.assertEqual(snap["dual_loader"]["damage_hooks"], 3)
        self.assertEqual(len(snap["mod_details"]), 2)
        self.assertEqual(snap["mod_details"][0]["tier"], "tier3")
        self.assertEqual(snap["panics"], 1)
        self.assertEqual(snap["errors"], 1)
        self.assertEqual(snap["warnings"], 1)
        self.assertEqual(snap["uptime_ticks"], 400)
        self.assertEqual(snap["clients"], 2)
        self.assertEqual(snap["lines_seen"], len(REALISTIC_LOG))
        self.assertEqual(len(snap["log"]), len(REALISTIC_LOG))

    def test_feed_returns_kinds(self):
        parser = megu_tui.MetricsParser()
        self.assertEqual(parser.feed(
            "mc-spine v0.6 listening 127.0.0.1:25599 (x)"), "ready")
        self.assertEqual(parser.feed(
            "[tick] observed_tps=20.00 avg_work_ms=1.0 max_work_ms=2.0 "
            "over_budget=0/100"), "tick")
        self.assertEqual(parser.feed(
            "[spine] Zoe joined from 127.0.0.1:9 (uuid aaaa)"), "join")
        self.assertEqual(parser.feed("totally unrecognized chatter"), "log")

    def test_join_leave_tracking(self):
        parser = megu_tui.MetricsParser()
        parser.feed("[spine] Zoe joined from 127.0.0.1:9 (uuid 0123456789abcdef0123456789abcdef)")
        self.assertEqual(parser.snapshot()["players_online"], 1)
        parser.feed("[spine] Zoe left (from 127.0.0.1:9)")
        self.assertEqual(parser.snapshot()["players_online"], 0)

    def test_ring_buffer_bound(self):
        parser = megu_tui.MetricsParser(log_lines=10)
        for index in range(30):
            parser.feed(f"line {index}")
        snap = parser.snapshot()
        self.assertEqual(len(snap["log"]), 10)
        self.assertEqual(snap["log"][-1], "line 29")
        self.assertEqual(snap["lines_seen"], 30)

    def test_vmrss_feeds_memory_history(self):
        parser = megu_tui.MetricsParser()
        parser.feed("[sz] t=20 VmRSS: 1048576 kB | mobs=0 clients=0")
        self.assertAlmostEqual(parser.snapshot()["mem_cur"], 1024.0)


class FrameLayoutTest(unittest.TestCase):
    def frame(self, cols, rows, **kwargs):
        parser = megu_tui.demo_parser()
        return megu_tui.render_frame(parser.snapshot(), cols, rows, **kwargs)

    def test_standard_sizes_never_overflow(self):
        for cols, rows in ((80, 24), (100, 30), (140, 40), (40, 12)):
            lines = self.frame(cols, rows)
            self.assertEqual(len(lines), rows, (cols, rows))
            for line in lines:
                self.assertEqual(megu_tui.vlen(line), cols, (cols, rows, line))

    def test_tiny_frames_do_not_crash(self):
        for cols, rows in ((40, 12), (20, 6), (12, 4), (8, 4)):
            lines = self.frame(cols, rows)
            self.assertEqual(len(lines), rows)
            for line in lines:
                self.assertLessEqual(megu_tui.vlen(line), cols)

    def test_ascii_frame_is_pure_ascii(self):
        lines = self.frame(80, 24, ascii_mode=True)
        for line in lines:
            self.assertTrue(line.isascii(), line)
            self.assertNotIn("\x1b", line)

    def test_colored_frame_keeps_widths(self):
        lines = self.frame(100, 30, color=True)
        self.assertIn("\x1b[", "\n".join(lines))
        for line in lines:
            self.assertEqual(megu_tui.vlen(line), 100)

    def test_frame_contains_dashboard_sections(self):
        lines = self.frame(100, 30)
        text = "\n".join(megu_tui.strip_ansi(line) for line in lines)
        self.assertIn("TPS", text)
        self.assertIn("MSPT", text)
        self.assertIn("MEM", text)
        self.assertIn("Players", text)
        self.assertIn("Chunks", text)
        self.assertIn("Adapted Mods", text)
        self.assertIn("Guard", text)
        self.assertIn("1.21.1", text)
        self.assertIn("26.3", text)
        self.assertIn("server log", text.lower())
        self.assertIn("/]", text)  # the '/' command keybinding hint

    def test_empty_snapshot_does_not_crash(self):
        snap = megu_tui.MetricsParser().snapshot()
        lines = megu_tui.render_frame(snap, 80, 24)
        self.assertEqual(len(lines), 24)

    def test_mod_details_view(self):
        parser = megu_tui.demo_parser()
        snap = parser.snapshot()
        lines = megu_tui.render_frame(snap, 100, 30,
                                      ui={"mods": True})
        text = "\n".join(megu_tui.strip_ansi(line) for line in lines)
        self.assertIn("adapted mods", text.lower())
        self.assertIn("weirdmod.jar", text)


class DemoFrameCliTest(unittest.TestCase):
    def run_tool(self, argv, env=None, input_text=None):
        merged = dict(os.environ)
        merged.update(env or {})
        return subprocess.run(
            [sys.executable, str(TOOL)] + argv,
            input=input_text, capture_output=True, text=True,
            timeout=30, env=merged)

    def test_demo_frame_piped_has_no_ansi(self):
        for env in ({}, {"NO_COLOR": "1"}, {"TERM": "dumb"}):
            result = self.run_tool(
                ["--demo-frame", "--cols", "80", "--rows", "24"], env=env)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertNotIn("\x1b", result.stdout)
            lines = result.stdout.splitlines()
            self.assertEqual(len(lines), 24)
            for line in lines:
                self.assertEqual(len(line), 80)

    def test_demo_frame_is_deterministic(self):
        args = ["--demo-frame", "--cols", "100", "--rows", "28"]
        first = self.run_tool(args)
        second = self.run_tool(args)
        self.assertEqual(first.returncode, 0)
        self.assertEqual(first.stdout, second.stdout)

    def test_demo_frame_ascii_is_pure_ascii(self):
        result = self.run_tool(
            ["--demo-frame", "--ascii", "--cols", "80", "--rows", "24"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(result.stdout.isascii())
        self.assertNotIn("\x1b", result.stdout)

    def test_render_snapshot_from_log_file(self):
        with tempfile.NamedTemporaryFile(
                "w", suffix=".log", delete=False, encoding="utf-8") as handle:
            handle.write("\n".join(REALISTIC_LOG) + "\n")
            path = handle.name
        try:
            result = self.run_tool(
                ["--render-snapshot", path, "--cols", "80", "--rows", "24"])
        finally:
            os.unlink(path)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("25599", result.stdout)
        self.assertIn("TPS", result.stdout)
        self.assertEqual(len(result.stdout.splitlines()), 24)

    def test_render_snapshot_missing_file(self):
        result = self.run_tool(["--render-snapshot", str(ROOT / "no-such.log")])
        self.assertEqual(result.returncode, 2)
        self.assertIn("megu-tui", result.stderr)


class ProcMetricsTest(unittest.TestCase):
    STATUS_TEXT = (
        "Name:\tmc-spine\n"
        "State:\tR (running)\n"
        "Pid:\t4242\n"
        "Threads:\t14\n"
        "VmPeak:\t  801200 kB\n"
        "VmRSS:\t  523264 kB\n"
        "VmHWM:\t  600064 kB\n"
    )
    # Fields after the last ')': state, ppid, pgrp, session, tty_nr, tpgid,
    # flags, minflt, cminflt, majflt, cmajflt, utime, stime, ...
    STAT_TEXT = "4242 (mc spine) weird) S 1 2 3 4 5 6 7 8 9 10 300 200 11 12\n"

    def test_parse_proc_status_reads_rss_and_threads(self):
        rss_mib, threads = megu_tui.parse_proc_status(self.STATUS_TEXT)
        self.assertAlmostEqual(rss_mib, 511.0)
        self.assertEqual(threads, 14)

    def test_parse_proc_status_missing_fields(self):
        rss_mib, threads = megu_tui.parse_proc_status(
            "Name:\tx\nState:\tS (sleeping)\n")
        self.assertIsNone(rss_mib)
        self.assertIsNone(threads)

    def test_parse_proc_stat_jiffies_with_tricky_comm(self):
        # The comm field contains a space and a ')' - field positions must be
        # counted back from the last ')' so utime+stime = 300+200.
        self.assertEqual(
            megu_tui.parse_proc_stat_jiffies(self.STAT_TEXT), 500)

    def test_parse_proc_stat_jiffies_rejects_garbage(self):
        self.assertIsNone(megu_tui.parse_proc_stat_jiffies("no parens"))
        self.assertIsNone(megu_tui.parse_proc_stat_jiffies("1 (x) S 1 2"))
        self.assertIsNone(megu_tui.parse_proc_stat_jiffies(
            "1 (x) S 1 2 3 4 5 6 7 8 9 10 zz 200 11"))

    def test_cpu_pct_delta_math(self):
        # 10 jiffies over 0.5 s at 100 Hz = 20% of one CPU.
        self.assertAlmostEqual(
            megu_tui.cpu_pct_delta((1000, 10.0), (1010, 10.5), 100), 20.0)
        self.assertAlmostEqual(
            megu_tui.cpu_pct_delta((0, 0.0), (50, 0.25), 100), 200.0)

    def test_cpu_pct_delta_edge_cases(self):
        self.assertIsNone(megu_tui.cpu_pct_delta(None, (10, 1.0), 100))
        self.assertIsNone(megu_tui.cpu_pct_delta((10, 1.0), None, 100))
        self.assertIsNone(megu_tui.cpu_pct_delta((10, 1.0), (20, 1.0), 100))
        self.assertIsNone(megu_tui.cpu_pct_delta((10, 1.0), (5, 2.0), 100))
        self.assertIsNone(megu_tui.cpu_pct_delta((10, 1.0), (20, 2.0), 0))

    @unittest.skipUnless(os.path.exists("/proc/self/stat"), "needs /proc")
    def test_sample_proc_metrics_live(self):
        first = megu_tui.sample_proc_metrics(os.getpid())
        self.assertIsNone(first["cpu_pct"])  # first sample: no delta yet
        self.assertIsNotNone(first["cpu_sample"])
        self.assertGreater(first["rss_mib"], 0)
        self.assertGreaterEqual(first["threads"], 1)
        second = megu_tui.sample_proc_metrics(
            os.getpid(), first["cpu_sample"],
            now=first["cpu_sample"][1] + 0.25)
        self.assertIsNotNone(second["cpu_pct"])
        self.assertGreaterEqual(second["cpu_pct"], 0.0)
        self.assertGreater(megu_tui.sample_rss_mib(os.getpid()), 0)

    def test_sample_proc_metrics_missing_pid(self):
        metrics = megu_tui.sample_proc_metrics(1 << 30)
        self.assertIsNone(metrics["rss_mib"])
        self.assertIsNone(metrics["threads"])
        self.assertIsNone(metrics["cpu_pct"])
        self.assertIsNone(megu_tui.sample_rss_mib(1 << 30))


class MsPerChunkParseTest(unittest.TestCase):
    def test_all_three_forms(self):
        parser = megu_tui.MetricsParser()
        parser.feed("[chunk] window stats ms/chunk=4.2")
        parser.feed("[anvil] chunks=1284 (4.2 ms/chunk)")
        parser.feed("CHUNKPROF region=world map=15.8ms entities=91")
        snap = parser.snapshot()
        self.assertEqual(snap["ms_per_chunk"], [4.2, 4.2, 15.8])
        self.assertAlmostEqual(snap["ms_per_chunk_cur"], 15.8)

    def test_feed_returns_ms_per_chunk_kind(self):
        parser = megu_tui.MetricsParser()
        self.assertEqual(parser.feed("profiler ms/chunk: 7.5"),
                         "ms_per_chunk")
        self.assertEqual(parser.feed("ordinary chatter"), "log")

    def test_ms_per_chunk_alongside_other_matches(self):
        parser = megu_tui.MetricsParser()
        kind = parser.feed(
            "[sz] t=60 VmRSS: 262144 kB ms/chunk=3.1 clients=1")
        self.assertEqual(kind, "size")
        self.assertAlmostEqual(parser.snapshot()["ms_per_chunk_cur"], 3.1)


class TelemetryRenderTest(unittest.TestCase):
    def parser(self):
        parser = megu_tui.MetricsParser()
        parser.feed("Loaded chunks: 1284")
        parser.feed("stats ms/chunk=4.2")
        parser.note_proc_metrics(512.0, 14, 30.0)
        parser.note_proc_metrics(513.0, 14, 41.5)
        return parser

    def test_cpu_row_shown_on_tall_frames(self):
        lines = megu_tui.render_frame(self.parser().snapshot(), 100, 28)
        text = "\n".join(megu_tui.strip_ansi(line) for line in lines)
        self.assertIn("CPU 41.5%  thr 14", text)
        self.assertIn("thr 14", text)
        self.assertIn("1,284 · 4.2ms/ch", text)
        for line in lines:
            self.assertEqual(megu_tui.vlen(line), 100)

    def test_cpu_row_hidden_on_80x24_but_threads_kept(self):
        lines = megu_tui.render_frame(self.parser().snapshot(), 80, 24)
        text = "\n".join(megu_tui.strip_ansi(line) for line in lines)
        self.assertNotIn("CPU", text)          # sparkline row gracefully hidden
        self.assertIn("thr 14", text)          # threads still on the MEM line
        self.assertIn("1,284 · 4.2ms/ch", text)
        for line in lines:
            self.assertEqual(megu_tui.vlen(line), 80)

    def test_ascii_mode_renders_cleanly(self):
        for cols, rows in ((80, 24), (100, 28)):
            lines = megu_tui.render_frame(
                self.parser().snapshot(), cols, rows, ascii_mode=True)
            text = "\n".join(lines)
            self.assertTrue(text.isascii())
            self.assertIn("1,284 - 4.2ms/ch", text)
            self.assertIn("thr 14", text)
            self.assertEqual("CPU" in text, rows >= 26)
            for line in lines:
                self.assertEqual(len(line), cols)


class FixedScaleTest(unittest.TestCase):
    def test_tps_scale_is_zero_to_twenty(self):
        self.assertEqual(megu_tui.sparkline([20.0], 4, low=0.0, high=20.0)[-1],
                         BLOCKS[-1])
        # 19.9 lands one rung under the top (truncated scale): near-full, not
        # the crash-looking drop a self-scaled sparkline would show.
        index = BLOCKS.index(
            megu_tui.sparkline([19.9], 4, low=0.0, high=20.0)[-1])
        self.assertEqual(index, len(BLOCKS) - 2)
        tiny = megu_tui.sparkline([19.8, 19.9], 8, low=0.0, high=20.0)
        self.assertEqual(tiny[0], tiny[1])  # 19.8 vs 19.9: same rung, no drama

    def test_mspt_scale_is_zero_to_fifty_budget(self):
        low = megu_tui.sparkline([10.0], 4, low=0.0, high=50.0)[-1]
        high = megu_tui.sparkline([45.0], 4, low=0.0, high=50.0)[-1]
        spike = megu_tui.sparkline([80.0], 4, low=0.0, high=50.0)[-1]
        self.assertLessEqual(BLOCKS.index(low), 1)      # 10 ms sits low
        self.assertGreaterEqual(BLOCKS.index(high), 5)  # 45 ms sits high
        self.assertEqual(spike, BLOCKS[-1])             # >50 clamps to full bar

    def test_tps_band_thresholds(self):
        self.assertEqual(megu_tui._tps_band(None), "grey")
        self.assertEqual(megu_tui._tps_band(19.9), "green")
        self.assertEqual(megu_tui._tps_band(17.0), "yellow")
        self.assertEqual(megu_tui._tps_band(14.9), "red")

    def test_mspt_band_thresholds(self):
        self.assertEqual(megu_tui._mspt_band(None), "grey")
        self.assertEqual(megu_tui._mspt_band(10.0), "green")
        self.assertEqual(megu_tui._mspt_band(30.0), "yellow")
        self.assertEqual(megu_tui._mspt_band(45.0), "red")

    def _mspt_line(self, mspt):
        parser = megu_tui.MetricsParser()
        parser.mspt.append(mspt)
        lines = megu_tui.render_frame(parser.snapshot(), 80, 24, color=True)
        return next(line for line in lines if " MSPT " in line)

    def test_mspt_rendered_colors(self):
        self.assertIn(megu_tui.COLORS["green"], self._mspt_line(10.0))
        self.assertIn(megu_tui.COLORS["yellow"], self._mspt_line(30.0))
        self.assertIn(megu_tui.COLORS["red"], self._mspt_line(45.0))

    def test_tps_rendered_green_and_near_top(self):
        parser = megu_tui.MetricsParser()
        parser.tps.append(19.9)
        lines = megu_tui.render_frame(parser.snapshot(), 80, 24, color=True)
        tps_line = next(line for line in lines if " TPS " in line)
        self.assertIn(megu_tui.COLORS["green"], tps_line)
        self.assertIn(BLOCKS[-2], megu_tui.strip_ansi(tps_line))


class DirtyFrameTest(unittest.TestCase):
    STATE = {"mods": False, "cmd_mode": False, "cmd_buf": "",
             "stopping": False}

    def key(self, parser, state=None, cols=80, rows=24):
        return megu_tui.render_key(
            parser, dict(self.STATE, **(state or {})), cols, rows)

    def test_key_stable_when_nothing_changes(self):
        parser = megu_tui.MetricsParser()
        before = self.key(parser)
        parser.snapshot()  # reads must not mark the frame dirty
        self.assertEqual(before, self.key(parser))

    def test_key_changes_on_new_log_line(self):
        parser = megu_tui.MetricsParser()
        before = self.key(parser)
        parser.feed("[tick] observed_tps=20.0 avg_work_ms=1.0 "
                    "max_work_ms=2.0 over_budget=0/10")
        self.assertNotEqual(before, self.key(parser))

    def test_key_changes_on_proc_sample(self):
        parser = megu_tui.MetricsParser()
        before = self.key(parser)
        parser.note_proc_metrics(512.0, 14, 37.5)
        self.assertNotEqual(before, self.key(parser))

    def test_empty_proc_sample_still_marks_dirty_once(self):
        parser = megu_tui.MetricsParser()
        before = self.key(parser)
        parser.note_proc_metrics(None, None, None)  # all failed: no change
        self.assertEqual(before, self.key(parser))
        parser.note_proc_metrics(None, 7, None)
        self.assertNotEqual(before, self.key(parser))

    def test_key_changes_on_ui_and_geometry(self):
        parser = megu_tui.MetricsParser()
        base = self.key(parser)
        for changed in ({"mods": True}, {"cmd_mode": True},
                        {"cmd_buf": "/list"}, {"stopping": True}):
            self.assertNotEqual(base, self.key(parser, changed))
        self.assertNotEqual(base, self.key(parser, cols=100))
        self.assertNotEqual(base, self.key(parser, rows=30))


class PassthroughTest(unittest.TestCase):
    def run_server(self, extra_args, env=None):
        merged = dict(os.environ)
        merged.update(env or {})
        return subprocess.run(
            [sys.executable, str(TOOL)] + extra_args
            + ["--server-bin", sys.executable, "--", "-c", STUB],
            input="list\n", capture_output=True, text=True,
            timeout=30, env=merged)

    def test_plain_mode_forwards_stdio_and_exit_code(self):
        result = self.run_server(["--plain"])
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertIn("stub: ready", result.stdout)
        self.assertIn("stub: got list", result.stdout)
        self.assertNotIn("\x1b", result.stdout)

    def test_non_tty_auto_falls_back_to_plain(self):
        result = self.run_server([])
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertIn("stub: ready", result.stdout)
        self.assertIn("stub: got list", result.stdout)
        self.assertNotIn("\x1b", result.stdout)

    def test_no_tui_alias(self):
        result = self.run_server(["--no-tui"])
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertNotIn("\x1b", result.stdout)

    def test_no_color_env_also_plain(self):
        result = self.run_server([], env={"NO_COLOR": "1"})
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertNotIn("\x1b", result.stdout)

    def test_missing_binary_is_a_clean_error(self):
        result = subprocess.run(
            [sys.executable, str(TOOL), "--plain", "--server-bin",
             str(ROOT / "no-such-binary-xyz"), "--"],
            capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 2)
        self.assertIn("megu-tui", result.stderr)


if __name__ == "__main__":
    unittest.main()
