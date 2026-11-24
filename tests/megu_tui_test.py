#!/usr/bin/env python3
"""Regression tests for tools/megu_tui.py.

Covers sparkline/text rendering, the no-side-effects fallback selection,
telemetry-vs-event log filtering, the MetricsParser against realistic megu
log lines (including hotspot/quarantine and sleeping states), deterministic
frame layout at compact and wide terminal sizes, keyboard views/history, and
end-to-end plain-mode passthrough of a stub subprocess.
"""

import argparse
import importlib.util
import io
import os
from pathlib import Path
import re
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
    "megu v1.21.1 listening 127.0.0.1:25565 (offline identities; no Mojang authentication)",
    "[live-mods] 5 jar(s): 1 auto-skipped, 2 native-bundle, 1 tier2-shim, 1 tier3",
    "[live-mods] dual-loader: 2 unified, 1 client-only, 0 superseded, 0 rejected, 0 duplicate ids, 0 missing deps, 3 verified damage hooks",
    "[live-mods] weirdmod.jar (weirdmod) tier3: 412 classes, 9 site(s) - mixin target missing",
    "[live-mods] liftcraft.jar (liftcraft) tier2 shim: 208 classes, 4 site(s) - damage hooks lifted",
    "[tick] observed_tps=19.97 avg_work_ms=8.21 max_work_ms=21.44 over_budget=0/100",
    "[spine] Alice joined from 127.0.0.1:51112 (uuid 0123456789abcdef0123456789abcdef)",
    "[spine] Bob joined from 127.0.0.1:51130 (uuid fedcba9876543210fedcba9876543210)",
    "[chat] Alice: welcome aboard",
    "[sz] t=400 VmRSS: 523264 kB | mobs=42 drops=7 xps=3 arrows=0 bob=1 pearl=0 primed=0 chests=18 furn=6 crops=11 world=0 inv=2 clients=2",
    "[chunkprof] window=60s chunks=1284 (4.2 ms/chunk) map=15.8ms entities=91",
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


class FakeProc:
    def __init__(self):
        self.stdin = io.BytesIO()

    def poll(self):
        return None


def parse_lines(lines):
    parser = megu_tui.MetricsParser()
    for line in lines:
        parser.feed(line)
    return parser


class TextUtilsTest(unittest.TestCase):
    def test_vlen_ignores_ansi(self):
        painted = megu_tui.paint("hello", "green", True)
        self.assertEqual(megu_tui.vlen(painted), 5)
        self.assertEqual(megu_tui.strip_ansi(painted), "hello")

    def test_clip_exact_width_plain_and_colored(self):
        self.assertEqual(megu_tui.clip("hello", 10), "hello     ")
        self.assertEqual(megu_tui.clip("hello world", 5), "hello")
        colored = megu_tui.paint("hello", "red", True)
        clipped = megu_tui.clip(colored, 10)
        self.assertEqual(megu_tui.vlen(clipped), 10)

    def test_asciify_keeps_output_ascii(self):
        text = megu_tui.asciify("● ⚠ ▇ café")
        self.assertTrue(text.isascii())
        self.assertIn("?", text)


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


class _FakeStream:
    def __init__(self, is_tty):
        self.is_tty = is_tty

    def isatty(self):
        return self.is_tty


class ModeSelectionTest(unittest.TestCase):
    def namespace(self, **overrides):
        values = {"plain": False, "tui": False, "no_tui": False}
        values.update(overrides)
        return argparse.Namespace(**values)

    def test_want_tui_auto_detect_and_explicit_tui(self):
        expected = (megu_tui.termios is not None and megu_tui.tty is not None
                    and megu_tui.select is not None and os.name != "nt")
        env = {"TERM": "xterm-256color"}
        self.assertEqual(megu_tui.want_tui(
            self.namespace(), env, _FakeStream(True), _FakeStream(True)), expected)
        self.assertEqual(megu_tui.want_tui(
            self.namespace(tui=True), env,
            _FakeStream(True), _FakeStream(True)), expected)
        self.assertFalse(megu_tui.want_tui(
            self.namespace(tui=True), env,
            _FakeStream(False), _FakeStream(True)))

    def test_plain_aliases_win_over_tui(self):
        env = {"TERM": "xterm-256color"}
        for name in ("plain", "no_tui"):
            self.assertFalse(megu_tui.want_tui(
                self.namespace(**{name: True, "tui": True}), env,
                _FakeStream(True), _FakeStream(True)))

    def test_cli_aliases_parse_to_plain(self):
        parser = megu_tui.build_parser()
        for flag in ("--plain", "--no-tui", "--cli", "--nogui"):
            with self.subTest(flag=flag):
                args = parser.parse_args([flag, "--server-bin", "megu"])
                self.assertTrue(args.plain)
                self.assertFalse(args.tui)

    def test_explicit_tui_flag_parses(self):
        args = megu_tui.build_parser().parse_args(["--tui"])
        self.assertTrue(args.tui)
        self.assertFalse(args.plain)

    def test_log_filter_default_and_choices(self):
        parser = megu_tui.build_parser()
        self.assertEqual(parser.parse_args([]).log_filter, "events")
        for mode in megu_tui.LOG_FILTERS:
            self.assertEqual(
                parser.parse_args(["--log-filter", mode]).log_filter, mode)


class MetricsParserTest(unittest.TestCase):
    def parse(self, lines=REALISTIC_LOG):
        return parse_lines(lines).snapshot()

    def test_realistic_session(self):
        snap = self.parse()
        self.assertTrue(snap["ready"])
        self.assertEqual(snap["version"], "1.21.1")
        self.assertEqual(snap["bind"], "127.0.0.1")
        self.assertEqual(snap["port"], 25565)
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
        self.assertEqual(snap["mod_warnings"], 1)
        self.assertEqual(snap["panics"], 1)
        self.assertEqual(snap["errors"], 1)
        self.assertEqual(snap["warnings"], 1)
        self.assertEqual(snap["uptime_ticks"], 400)
        self.assertEqual(snap["clients"], 2)
        self.assertFalse(snap["sleeping"])
        self.assertFalse(snap["hotspot_alert"])
        self.assertEqual(snap["lines_seen"], len(REALISTIC_LOG))
        self.assertEqual(len(snap["log"]), len(REALISTIC_LOG))
        self.assertEqual(len(snap["event_log"]), len(REALISTIC_LOG))

    def test_feed_returns_kinds(self):
        parser = megu_tui.MetricsParser()
        self.assertEqual(parser.feed(
            "megu v1.21.1 listening 127.0.0.1:25565 (x)"), "ready")
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

    def test_ring_buffer_bound_and_categories_stay_aligned(self):
        parser = megu_tui.MetricsParser(log_lines=10)
        for index in range(30):
            parser.feed(f"line {index}")
        snap = parser.snapshot()
        self.assertEqual(len(snap["log"]), 10)
        self.assertEqual(len(snap["log_categories"]), 10)
        self.assertEqual(snap["log"][-1], "line 29")
        self.assertEqual(snap["lines_seen"], 30)

    def test_vmrss_feeds_memory_history(self):
        parser = megu_tui.MetricsParser()
        parser.feed("[sz] t=20 VmRSS: 1048576 kB | mobs=0 clients=0")
        self.assertAlmostEqual(parser.snapshot()["mem_cur"], 1024.0)

    def test_telemetry_is_parsed_and_tagged(self):
        parser = megu_tui.MetricsParser()
        self.assertEqual(parser.feed(
            "[tick] observed_tps=18.25 avg_work_ms=31.0 max_work_ms=40.0 "
            "over_budget=2/100"), "tick")
        self.assertEqual(parser.feed(
            "[sz] t=600 VmRSS: 262144 kB | clients=1"), "size")
        self.assertEqual(parser.feed(
            "[chunkprof] window=60s chunks=1284 (4.2 ms/chunk) map=15.8ms"),
            "ms_per_chunk")
        snap = parser.snapshot()
        self.assertEqual(snap["log_categories"],
                         ["telemetry", "telemetry", "telemetry"])
        self.assertAlmostEqual(snap["tps_cur"], 18.25)
        self.assertAlmostEqual(snap["mspt_cur"], 31.0)
        self.assertAlmostEqual(snap["mem_cur"], 256.0)
        self.assertAlmostEqual(snap["ms_per_chunk_cur"], 4.2)
        self.assertEqual(snap["uptime_ticks"], 600)
        self.assertEqual(snap["clients"], 1)

    def test_event_filter_hides_telemetry_but_keeps_metrics(self):
        snap = self.parse()
        events = megu_tui.filtered_log_lines(snap, "events")
        self.assertTrue(any("joined" in line for line in events))
        self.assertTrue(any("[chat]" in line for line in events))
        self.assertTrue(any("[security]" in line for line in events))
        for spam in ("[tick]", "[sz]", "[chunkprof]"):
            self.assertFalse(any(line.startswith(spam) for line in events),
                             spam)
        self.assertAlmostEqual(snap["tps_cur"], 19.4)
        self.assertAlmostEqual(snap["mspt_cur"], 44.1)
        self.assertAlmostEqual(snap["mem_cur"], 511.0)

    def test_all_filter_keeps_raw_telemetry(self):
        snap = self.parse()
        full = megu_tui.filtered_log_lines(snap, "all")
        self.assertEqual(len(full), len(REALISTIC_LOG))
        for spam in ("[tick]", "[sz]", "[chunkprof]"):
            self.assertTrue(any(line.startswith(spam) for line in full), spam)

    def test_alerts_filter_only_alerts(self):
        snap = self.parse()
        alerts = megu_tui.filtered_log_lines(snap, "alerts")
        self.assertTrue(any("[security]" in line for line in alerts))
        self.assertTrue(any("[FATAL]" in line for line in alerts))
        self.assertFalse(any("joined" in line for line in alerts))
        self.assertFalse(any(line.startswith("[tick]") for line in alerts))
        self.assertFalse(any("[chat]" in line for line in alerts))
        entries = megu_tui.selected_log_entries(snap, "alerts")
        self.assertTrue(all(entry["category"] == "alert" for entry in entries))

    def test_log_filter_cycle_and_normalization(self):
        self.assertEqual(megu_tui.cycle_log_filter("events"), "alerts")
        self.assertEqual(megu_tui.cycle_log_filter("alerts"), "all")
        self.assertEqual(megu_tui.cycle_log_filter("all"), "events")
        self.assertEqual(megu_tui.normalize_log_filter("bogus"), "events")
        self.assertEqual(megu_tui.normalize_log_filter("all"), "all")

    def test_injected_banner_is_categorized_and_not_an_alert(self):
        parser = megu_tui.MetricsParser()
        self.assertTrue(parser.inject_banner())
        self.assertFalse(parser.inject_banner())
        parser.feed("[config] first server line")
        snap = parser.snapshot()
        banner = [entry for entry in snap["event_log"]
                  if entry["category"] == "banner"]
        self.assertEqual(len(banner), len(megu_tui.LOGO))
        self.assertEqual([entry["line"] for entry in banner],
                         list(megu_tui.LOGO))
        self.assertEqual(snap["event_log"][0]["category"], "banner")
        self.assertEqual(snap["lines_seen"], 1)
        self.assertEqual(snap["warnings"], 0)
        self.assertEqual(snap["errors"], 0)
        self.assertEqual(snap["panics"], 0)
        alerts = megu_tui.filtered_log_lines(snap, "alerts")
        self.assertFalse(any(line in megu_tui.LOGO for line in alerts))
        events = megu_tui.filtered_log_lines(snap, "events")
        self.assertEqual(events[:len(megu_tui.LOGO)], list(megu_tui.LOGO))

    def test_captured_banner_lines_are_recognized_without_counting(self):
        parser = megu_tui.MetricsParser()
        for line in megu_tui.LOGO_ASCII:
            parser.feed(line)
        snap = parser.snapshot()
        self.assertEqual(snap["log_categories"],
                         ["banner"] * len(megu_tui.LOGO_ASCII))
        self.assertEqual(snap["warnings"], 0)
        self.assertEqual(snap["errors"], 0)

    def test_hotspot_line_sets_guard_alert(self):
        parser = megu_tui.MetricsParser()
        parser.feed("Players: 2/20 (Bob, Carol)")
        parser.feed("Loaded chunks: 1284")
        self.assertEqual(parser.feed(
            "[guard] chunk hotspot (12,-4) throttled after 41 ms"),
            "hotspot")
        snap = parser.snapshot()
        self.assertTrue(snap["hotspot_alert"])
        self.assertEqual(snap["hotspots"][-1]["cx"], 12)
        self.assertEqual(snap["hotspots"][-1]["cz"], -4)
        self.assertEqual(snap["warnings"], 1)
        text = "\n".join(megu_tui.strip_ansi(line) for line in
                         megu_tui.render_frame(snap, 100, 24))
        self.assertIn("Hotspot (12,-4)", text)
        self.assertIn("throttled", text)

    def test_quarantine_line_sets_guard_alert(self):
        parser = megu_tui.MetricsParser()
        parser.feed("Loaded chunks: 512")
        self.assertEqual(parser.feed(
            "[quarantine] chunk (7,9) frozen after 3 consecutive faults"),
            "quarantine")
        snap = parser.snapshot()
        self.assertTrue(snap["hotspot_alert"])
        self.assertEqual(snap["quarantine_alerts"], 1)
        self.assertEqual(snap["hotspots"][-1]["cx"], 7)
        self.assertEqual(snap["hotspots"][-1]["cz"], 9)
        text = "\n".join(megu_tui.strip_ansi(line) for line in
                         megu_tui.render_frame(snap, 100, 24))
        self.assertIn("Quarantine (7,9)", text)

    def test_sleeping_and_wake_lines(self):
        parser = megu_tui.MetricsParser()
        parser.feed("megu v1.21.1 listening 127.0.0.1:25565 (offline)")
        parser.feed("[sleep] SLEEPING until a player joins (wake-on-join)")
        snap = parser.snapshot()
        self.assertTrue(snap["sleeping"])
        self.assertEqual(megu_tui._status_word(snap), "SLEEPING")
        parser.feed("[sleep] woke from sleep on player join")
        snap = parser.snapshot()
        self.assertFalse(snap["sleeping"])
        self.assertEqual(megu_tui._status_word(snap), "ONLINE")

    def test_join_wakes_sleeping_server(self):
        parser = megu_tui.MetricsParser()
        parser.feed("megu v1.21.1 listening 127.0.0.1:25565 (offline)")
        parser.feed("[sleep] SLEEPING until a player joins")
        self.assertTrue(parser.snapshot()["sleeping"])
        parser.feed("[spine] Zoe joined from 127.0.0.1:9 (uuid aaaa)")
        self.assertFalse(parser.snapshot()["sleeping"])

    def test_mod_warnings_are_counted_in_plain_language(self):
        snap = self.parse()
        self.assertEqual(snap["mod_warnings"], 1)
        text = "\n".join(megu_tui.strip_ansi(line) for line in
                         megu_tui.render_frame(snap, 120, 30))
        self.assertIn("Mods: 3 active", text)
        self.assertIn("1 fallback", text)
        self.assertIn("1 warning", text)

    def test_status_word_bands(self):
        def snap(tps=None, mspt=None, **extra):
            base = {"tps_cur": tps, "mspt_cur": mspt, "panics": 0,
                    "errors": 0, "hotspot_alert": False, "sleeping": False,
                    "players_online": 1, "ready": True, "status": "Running"}
            base.update(extra)
            return base
        self.assertEqual(megu_tui._status_word(snap(19.9, 10.0)), "ONLINE")
        self.assertEqual(megu_tui._status_word(snap(19.0, 10.0)), "BUSY")
        self.assertEqual(megu_tui._status_word(snap(17.0, 10.0)), "ALERT")
        self.assertEqual(megu_tui._status_word(snap(19.9, 46.0)), "ALERT")
        self.assertEqual(megu_tui._status_word(
            snap(19.9, 10.0, sleeping=True, players_online=0)), "SLEEPING")
        self.assertEqual(megu_tui._status_word(
            snap(19.9, 10.0, panics=1)), "ALERT")
        self.assertEqual(megu_tui._status_word(
            snap(19.9, 10.0, errors=1)), "ALERT")
        self.assertEqual(megu_tui._status_word(
            snap(19.9, 10.0, hotspot_alert=True)), "ALERT")
        self.assertEqual(megu_tui._status_word(
            {"ready": False, "status": "Booting"}), "BOOTING")


class FrameLayoutTest(unittest.TestCase):
    def frame(self, cols, rows, **kwargs):
        parser = megu_tui.demo_parser()
        return megu_tui.render_frame(parser.snapshot(), cols, rows, **kwargs)

    def test_standard_sizes_never_overflow(self):
        for cols, rows in ((80, 24), (92, 22), (100, 30), (120, 36),
                           (140, 40)):
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
        for cols, rows in ((80, 24), (120, 36)):
            lines = self.frame(cols, rows, ascii_mode=True)
            for line in lines:
                self.assertTrue(line.isascii(), line)
                self.assertNotIn("\x1b", line)

    def test_colored_frame_keeps_widths(self):
        lines = self.frame(100, 30, color=True)
        self.assertIn("\x1b[", "\n".join(lines))
        for line in lines:
            self.assertEqual(megu_tui.vlen(line), 100)

    def test_logo_constants_are_six_line_banners(self):
        self.assertEqual(len(megu_tui.LOGO), 6)
        self.assertEqual(len(megu_tui.LOGO_ASCII), 6)
        for line in megu_tui.LOGO_ASCII:
            self.assertTrue(line.isascii(), line)
        # The unicode banner uses the classic block-letter wordmark.
        self.assertIn("█", megu_tui.LOGO[0])
        self.assertIn("╚", megu_tui.LOGO[-1])
        self.assertEqual(megu_tui.startup_banner(False), megu_tui.LOGO)
        self.assertEqual(megu_tui.startup_banner(True), megu_tui.LOGO_ASCII)

    def test_pinned_header_is_three_compact_lines(self):
        for cols, rows in ((80, 24), (120, 36)):
            plain = [megu_tui.strip_ansi(line)
                     for line in self.frame(cols, rows)]
            self.assertIn("MEGU 1.21.1", plain[0])
            self.assertIn("ONLINE", plain[0])
            self.assertIn("HEALTH", plain[1])
            self.assertIn("TPS", plain[1])
            self.assertIn("Players 2/20", plain[2])
            self.assertIn("Chunks 1,284", plain[2])
            self.assertIn("Guard: OK", plain[2])
            self.assertIn("Mods: 3 active", plain[2])
            # The logo is scrolling content, never pinned chrome.
            for line in plain[:3]:
                for logo_line in megu_tui.LOGO + megu_tui.LOGO_ASCII:
                    self.assertNotIn(logo_line.rstrip(), line)
            self.assertTrue(plain[3].lstrip().startswith("Events"), plain[3])

    def test_startup_banner_is_in_the_scrolling_log(self):
        plain = [megu_tui.strip_ansi(line).rstrip()
                 for line in self.frame(120, 36)]
        section = next(index for index, line in enumerate(plain)
                       if line.lstrip().startswith("Events"))
        positions = []
        for logo_line in megu_tui.LOGO:
            expected = logo_line.rstrip()
            index = plain.index(expected)
            self.assertGreater(index, section)
            positions.append(index)
        self.assertEqual(positions, sorted(positions))
        # ...and it appears before the first captured server line.
        first_server = next(index for index, line in enumerate(plain)
                            if line.startswith("[live-mods]"))
        self.assertLess(max(positions), first_server)

    def test_startup_banner_scrolls_off_naturally(self):
        parser = megu_tui.MetricsParser()
        parser.inject_banner()
        for index in range(3):
            parser.feed(f"[config] boot line {index}")
        early = "\n".join(megu_tui.strip_ansi(line) for line in
                          megu_tui.render_frame(parser.snapshot(), 80, 24))
        self.assertIn(megu_tui.LOGO[0].rstrip(), early)
        for index in range(40):
            parser.feed(f"[config] later line {index}")
        late = "\n".join(megu_tui.strip_ansi(line) for line in
                         megu_tui.render_frame(parser.snapshot(), 80, 24))
        for line in megu_tui.LOGO:
            self.assertNotIn(line.rstrip(), late)

    def test_startup_banner_is_hidden_under_alerts_filter(self):
        parser = megu_tui.demo_parser()
        alerts = "\n".join(megu_tui.strip_ansi(line) for line in
                           megu_tui.render_frame(parser.snapshot(), 120, 36,
                                                 log_filter="alerts"))
        for line in megu_tui.LOGO:
            self.assertNotIn(line.rstrip(), alerts)
        self.assertIn("[security]", alerts)
        self.assertNotIn("[live-mods]", alerts)

    def test_header_has_all_key_fields_at_80_and_120(self):
        for cols, rows in ((80, 24), (120, 36)):
            header = megu_tui.strip_ansi(self.frame(cols, rows)[0])
            self.assertIn("MEGU 1.21.1", header)
            self.assertIn("ONLINE", header)
            self.assertIn("127.0.0.1:25565", header)
            self.assertIn("Players 2/20", header)
            self.assertIn("TPS 19.9", header)
            self.assertIn("(11.5ms)", header)
            self.assertIn("RAM 523 MiB", header)

    def test_summary_is_compact_and_logs_get_most_rows(self):
        for cols, rows in ((80, 24), (120, 36)):
            lines = self.frame(cols, rows)
            plain = [megu_tui.strip_ansi(line) for line in lines]
            section_index = next(
                index for index, line in enumerate(plain)
                if line.lstrip().startswith("Events"))
            summary_rows = section_index - 1  # exclude the header line
            self.assertLessEqual(summary_rows, 3, (cols, rows, plain[:5]))
            self.assertGreaterEqual(rows - section_index - 1, rows * 0.6,
                                    (cols, rows, section_index))

    def test_health_strip_has_tps_mspt_ram_cpu_sparklines(self):
        for cols, rows in ((80, 24), (120, 36)):
            lines = self.frame(cols, rows)
            health = megu_tui.strip_ansi(lines[1])
            for label in ("HEALTH", "TPS", "MSPT", "RAM", "CPU"):
                self.assertIn(label, health)
            self.assertTrue(any(char in health for char in BLOCKS))

    def test_actionable_cards_use_human_language(self):
        lines = self.frame(120, 36)
        text = "\n".join(megu_tui.strip_ansi(line) for line in lines)
        self.assertIn("Players 2/20: Bob, Carol", text)
        self.assertIn("Chunks 1,284", text)
        self.assertIn("Guard: OK", text)
        self.assertIn("Mods: 3 active", text)
        self.assertIn("1 fallback", text)
        self.assertNotRegex(text, r"\bN:\d")
        self.assertNotRegex(text, r"\bS:\d")
        self.assertNotRegex(text, r"\bF:\d")

    def test_idle_server_card_says_idle(self):
        parser = megu_tui.MetricsParser()
        parser.feed("Players: 0/20")
        snap = parser.snapshot()
        text = "\n".join(megu_tui.strip_ansi(line) for line in
                         megu_tui.render_frame(snap, 120, 24))
        self.assertIn("Players 0/20 (idle)", text)

    def test_hotspot_alert_is_highlighted(self):
        parser = megu_tui.demo_parser()
        parser.feed("[guard] chunk hotspot (12,-4) throttled after 41 ms")
        snap = parser.snapshot()
        text = "\n".join(megu_tui.strip_ansi(line) for line in
                         megu_tui.render_frame(snap, 120, 30))
        self.assertIn("Hotspot (12,-4)", text)
        self.assertIn("throttled", text)
        self.assertNotIn("Guard: OK", text)
        self.assertIn("ALERT", text)
        ascii_text = "\n".join(megu_tui.render_frame(
            snap, 120, 30, ascii_mode=True))
        self.assertIn("! Hotspot (12,-4)", ascii_text)

    def test_footer_shows_filter_counts_and_shortcuts(self):
        lines = self.frame(120, 36)
        footer = megu_tui.strip_ansi(lines[-1])
        self.assertIn("filter:events", footer)
        self.assertIn("[p] players", footer)
        self.assertIn("[m] mods", footer)
        self.assertIn("[q] save & stop", footer)
        self.assertIn("1 warn", footer)
        self.assertIn("0 err", footer)
        self.assertIn("[/] cmd", footer)

    def test_footer_tracks_active_filter(self):
        lines = self.frame(120, 36, ui={"log_filter": "alerts"})
        footer = megu_tui.strip_ansi(lines[-1])
        self.assertIn("filter:alerts", footer)
        self.assertIn("Alerts", "\n".join(megu_tui.strip_ansi(l) for l in lines))

    def test_log_filters_change_only_the_log_body(self):
        snap = megu_tui.demo_parser().snapshot()
        events = "\n".join(megu_tui.strip_ansi(line) for line in
                           megu_tui.render_frame(snap, 120, 36,
                                                 log_filter="events"))
        all_log = "\n".join(megu_tui.strip_ansi(line) for line in
                            megu_tui.render_frame(snap, 120, 36,
                                                  log_filter="all"))
        alerts = "\n".join(megu_tui.strip_ansi(line) for line in
                           megu_tui.render_frame(snap, 120, 36,
                                                 log_filter="alerts"))
        self.assertNotIn("[tick]", events)
        self.assertNotIn("[sz]", events)
        self.assertNotIn("[chunkprof]", events)
        self.assertIn("[tick]", all_log)
        self.assertIn("[sz]", all_log)
        self.assertIn("[chunkprof]", all_log)
        self.assertEqual(events.splitlines()[0], all_log.splitlines()[0])
        self.assertIn("[security]", alerts)
        self.assertNotIn("[tick]", alerts)
        self.assertNotIn("joined", alerts)
        self.assertIn("Alerts", alerts)

    def test_players_view(self):
        lines = self.frame(120, 30, ui={"view": "players"})
        text = "\n".join(megu_tui.strip_ansi(line) for line in lines)
        self.assertIn("Players", text)
        self.assertIn("Status ONLINE", text)
        self.assertIn("uptime 500 ticks", text)
        self.assertIn("Bob", text)
        self.assertIn("127.0.0.1:51130", text)
        self.assertIn("[p] log", megu_tui.strip_ansi(lines[-1]))

    def test_players_view_when_empty(self):
        parser = megu_tui.MetricsParser()
        parser.feed("Players: 0/20")
        lines = megu_tui.render_frame(parser.snapshot(), 100, 24,
                                      ui={"view": "players"})
        text = "\n".join(megu_tui.strip_ansi(line) for line in lines)
        self.assertIn("No players online", text)

    def test_mods_view_is_human_readable(self):
        lines = self.frame(120, 30, ui={"view": "mods"})
        text = "\n".join(megu_tui.strip_ansi(line) for line in lines)
        self.assertIn("weirdmod.jar", text)
        self.assertIn("compatibility shim", text)
        self.assertIn("runtime adapter", text)
        self.assertNotIn("tier2", text.lower())
        self.assertNotIn("tier3", text.lower())
        self.assertIn("3 active", text)
        self.assertIn("[m] log", megu_tui.strip_ansi(lines[-1]))

    def test_legacy_ui_mods_key_still_opens_mods_view(self):
        lines = self.frame(100, 30, ui={"mods": True})
        text = "\n".join(megu_tui.strip_ansi(line) for line in lines)
        self.assertIn("weirdmod.jar", text)

    def test_empty_snapshot_does_not_crash(self):
        snap = megu_tui.MetricsParser().snapshot()
        lines = megu_tui.render_frame(snap, 80, 24)
        self.assertEqual(len(lines), 24)
        for line in lines:
            self.assertEqual(megu_tui.vlen(line), 80)
        self.assertIn("BOOTING", megu_tui.strip_ansi(lines[0]))
        self.assertIn("(idle)", "\n".join(megu_tui.strip_ansi(l) for l in lines))


class KeyboardTest(unittest.TestCase):
    def state(self):
        return {"quit": False, "stopping": False, "cmd_mode": False,
                "cmd_buf": "", "view": "log", "log_filter": "events",
                "history": [], "history_index": 0}

    def test_f_cycles_log_filter(self):
        state = self.state()
        proc = FakeProc()
        for expected in ("alerts", "all", "events"):
            megu_tui.handle_keys(b"f", state, proc)
            self.assertEqual(state["log_filter"], expected)

    def test_p_and_m_toggle_views(self):
        state = self.state()
        proc = FakeProc()
        megu_tui.handle_keys(b"p", state, proc)
        self.assertEqual(state["view"], "players")
        megu_tui.handle_keys(b"p", state, proc)
        self.assertEqual(state["view"], "log")
        megu_tui.handle_keys(b"m", state, proc)
        self.assertEqual(state["view"], "mods")
        megu_tui.handle_keys(b"m", state, proc)
        self.assertEqual(state["view"], "log")

    def test_q_and_ctrl_c_set_quit(self):
        for key in (b"q", b"\x03"):
            state = self.state()
            megu_tui.handle_keys(key, state, FakeProc())
            self.assertTrue(state["quit"])

    def test_command_prompt_sends_line_and_records_history(self):
        state = self.state()
        proc = FakeProc()
        megu_tui.handle_keys(b"/", state, proc)
        self.assertTrue(state["cmd_mode"])
        megu_tui.handle_keys(b"list", state, proc)
        megu_tui.handle_keys(b"\r", state, proc)
        self.assertFalse(state["cmd_mode"])
        self.assertEqual(proc.stdin.getvalue(), b"list\n")
        self.assertEqual(state["history"], ["list"])

    def test_up_and_down_arrow_history(self):
        state = self.state()
        proc = FakeProc()
        megu_tui.handle_keys(b"/first\r", state, proc)
        megu_tui.handle_keys(b"/second\r", state, proc)
        megu_tui.handle_keys(b"/", state, proc)
        megu_tui.handle_keys(b"\x1b[A", state, proc)
        self.assertEqual(state["cmd_buf"], "second")
        megu_tui.handle_keys(b"\x1b[A", state, proc)
        self.assertEqual(state["cmd_buf"], "first")
        megu_tui.handle_keys(b"\x1b[B", state, proc)
        self.assertEqual(state["cmd_buf"], "second")
        megu_tui.handle_keys(b"\x1b[B", state, proc)
        self.assertEqual(state["cmd_buf"], "")

    def test_escape_and_backspace_in_command_mode(self):
        state = self.state()
        proc = FakeProc()
        megu_tui.handle_keys(b"/list", state, proc)
        megu_tui.handle_keys(b"\x7f", state, proc)
        self.assertEqual(state["cmd_buf"], "lis")
        megu_tui.handle_keys(b"\x1b", state, proc)
        self.assertFalse(state["cmd_mode"])
        self.assertEqual(state["cmd_buf"], "")

    def test_empty_command_is_not_sent(self):
        state = self.state()
        proc = FakeProc()
        megu_tui.handle_keys(b"/\r", state, proc)
        self.assertEqual(proc.stdin.getvalue(), b"")
        self.assertEqual(state["history"], [])

    def test_render_key_changes_on_ui_state(self):
        parser = megu_tui.MetricsParser()
        state = self.state()
        base = megu_tui.render_key(parser, state, 80, 24)
        parser.snapshot()
        self.assertEqual(base, megu_tui.render_key(parser, state, 80, 24))
        parser.feed("[tick] observed_tps=20.0 avg_work_ms=1.0 "
                    "max_work_ms=2.0 over_budget=0/10")
        self.assertNotEqual(base, megu_tui.render_key(parser, state, 80, 24))
        for changed in ({"log_filter": "alerts"}, {"view": "players"},
                        {"cmd_mode": True}, {"cmd_buf": "/list"},
                        {"stopping": True}):
            self.assertNotEqual(base, megu_tui.render_key(
                parser, dict(state, **changed), 80, 24))


class ProcMetricsTest(unittest.TestCase):
    STATUS_TEXT = (
        "Name:\tmegu\n"
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

    def test_profiling_lines_are_telemetry(self):
        parser = megu_tui.MetricsParser()
        parser.feed("[chunk] window stats ms/chunk=4.2")
        parser.feed("CHUNKPROF region=world map=15.8ms entities=91")
        self.assertEqual(parser.snapshot()["log_categories"],
                         ["telemetry", "telemetry"])


class DemoFrameCliTest(unittest.TestCase):
    def run_tool(self, argv, env=None, input_text=None):
        merged = dict(os.environ)
        merged.update(env or {})
        return subprocess.run(
            [sys.executable, str(TOOL)] + argv,
            input=input_text, capture_output=True, text=True, encoding="utf-8",
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

    def test_demo_frame_shows_startup_banner_in_log(self):
        result = self.run_tool(
            ["--demo-frame", "--cols", "120", "--rows", "36"])
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = [line.rstrip() for line in result.stdout.splitlines()]
        for logo_line in megu_tui.LOGO:
            self.assertIn(logo_line.rstrip(), lines)
        header = lines[0]
        self.assertIn("MEGU 1.21.1", header)
        for logo_line in megu_tui.LOGO:
            self.assertNotIn(logo_line.rstrip(), header)

    def test_no_banner_flag_and_env_suppress_banner(self):
        variants = (
            (["--no-banner"], {}),
            ([], {"MEGU_NO_BANNER": "1"}),
            ([], {"NO_BANNER": "1"}),
        )
        for extra, env in variants:
            with self.subTest(extra=extra, env=env):
                result = self.run_tool(
                    ["--demo-frame", "--ascii", "--cols", "120", "--rows",
                     "36"] + extra, env=env)
                self.assertEqual(result.returncode, 0, result.stderr)
                for logo_line in megu_tui.LOGO + megu_tui.LOGO_ASCII:
                    self.assertNotIn(logo_line.rstrip(), result.stdout)

    def test_ascii_banner_respects_exact_frame_bounds(self):
        for cols, rows in ((80, 24), (92, 22), (120, 36)):
            parser = megu_tui.MetricsParser()
            parser.inject_banner(ascii_mode=True)
            parser.feed("[config] loaded server.toml")
            lines = megu_tui.render_frame(parser.snapshot(), cols, rows,
                                          ascii_mode=True)
            self.assertEqual(len(lines), rows, (cols, rows))
            for line in lines:
                self.assertTrue(line.isascii(), line)
                self.assertEqual(megu_tui.vlen(line), cols, (cols, rows, line))
            self.assertIn(megu_tui.LOGO_ASCII[0].rstrip(),
                          [line.rstrip() for line in lines])

    def test_demo_frame_92x22_sample_is_exact(self):
        result = self.run_tool(
            ["--demo-frame", "--ascii", "--cols", "92", "--rows", "22"])
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = result.stdout.splitlines()
        self.assertEqual(len(lines), 22)
        for line in lines:
            self.assertEqual(len(line), 92)
        self.assertIn("MEGU 1.21.1", lines[0])
        self.assertIn("ONLINE", lines[0])
        self.assertIn("25565", lines[0])

    def test_demo_frame_cli_alias_is_plain_ascii(self):
        result = self.run_tool(
            ["--demo-frame", "--cli", "--cols", "80", "--rows", "24"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(result.stdout.isascii())
        self.assertNotIn("\x1b", result.stdout)

    def test_demo_frame_log_filter_events_hides_telemetry(self):
        result = self.run_tool(
            ["--demo-frame", "--log-filter", "events",
             "--cols", "120", "--rows", "36"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("[tick]", result.stdout)
        self.assertNotIn("[sz]", result.stdout)
        self.assertNotIn("[chunkprof]", result.stdout)
        self.assertIn("TPS 19.9", result.stdout)

    def test_demo_frame_log_filter_all_shows_telemetry(self):
        result = self.run_tool(
            ["--demo-frame", "--log-filter", "all",
             "--cols", "120", "--rows", "36"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("[tick]", result.stdout)
        self.assertIn("[sz]", result.stdout)
        self.assertIn("[chunkprof]", result.stdout)
        self.assertIn("All log", result.stdout)

    def test_demo_frame_log_filter_alerts_only(self):
        result = self.run_tool(
            ["--demo-frame", "--log-filter", "alerts",
             "--cols", "120", "--rows", "36"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("[security]", result.stdout)
        self.assertNotIn("[tick]", result.stdout)
        self.assertNotIn("joined", result.stdout)
        self.assertIn("Alerts", result.stdout)

    def test_demo_frame_log_filter_is_deterministic(self):
        args = ["--demo-frame", "--log-filter", "alerts",
                "--cols", "92", "--rows", "22"]
        first = self.run_tool(args)
        second = self.run_tool(args)
        self.assertEqual(first.returncode, 0)
        self.assertEqual(first.stdout, second.stdout)

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
        self.assertIn("25565", result.stdout)
        self.assertIn("TPS", result.stdout)
        self.assertEqual(len(result.stdout.splitlines()), 24)

    def test_render_snapshot_does_not_duplicate_captured_banner(self):
        captured = list(megu_tui.LOGO) + REALISTIC_LOG
        with tempfile.NamedTemporaryFile(
                "w", suffix=".log", delete=False, encoding="utf-8") as handle:
            handle.write("\n".join(captured) + "\n")
            path = handle.name
        try:
            result = self.run_tool(
                ["--render-snapshot", path, "--cols", "120", "--rows",
                 "36"])
        finally:
            os.unlink(path)
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = [line.rstrip() for line in result.stdout.splitlines()]
        for logo_line in megu_tui.LOGO:
            self.assertEqual(lines.count(logo_line.rstrip()), 1, logo_line)

    def test_render_snapshot_from_stdin(self):
        result = self.run_tool(
            ["--render-snapshot", "-", "--cols", "80", "--rows", "24"],
            input_text="\n".join(REALISTIC_LOG) + "\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("25565", result.stdout)
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

    def test_explicit_tui_still_falls_back_without_a_tty(self):
        result = self.run_server(["--tui"])
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertIn("stub: ready", result.stdout)
        self.assertNotIn("\x1b", result.stdout)

    def test_plain_aliases_all_passthrough(self):
        for flag in ("--no-tui", "--cli", "--nogui"):
            with self.subTest(flag=flag):
                result = self.run_server([flag])
                self.assertEqual(result.returncode, 7, result.stderr)
                self.assertIn("stub: got list", result.stdout)
                self.assertNotIn("\x1b", result.stdout)

    def test_no_color_env_also_plain(self):
        result = self.run_server([], env={"NO_COLOR": "1"})
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertNotIn("\x1b", result.stdout)

    def test_server_args_after_double_dash_are_forwarded(self):
        result = self.run_server(["--cli"])
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertIn("stub: got list", result.stdout)

    def test_missing_binary_is_a_clean_error(self):
        result = subprocess.run(
            [sys.executable, str(TOOL), "--cli", "--server-bin",
             str(ROOT / "no-such-binary-xyz"), "--"],
            capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 2)
        self.assertIn("megu-tui", result.stderr)

    def test_stop_timeout_is_accepted(self):
        result = self.run_server(["--cli", "--stop-timeout", "2.5"])
        self.assertEqual(result.returncode, 7, result.stderr)


if __name__ == "__main__":
    unittest.main()
