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
