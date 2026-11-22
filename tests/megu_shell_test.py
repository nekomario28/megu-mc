#!/usr/bin/env python3
"""Unit tests for tools/megu_shell.py — pure stdlib, no PTY required.

LineEditor is a deterministic state machine, so every editing key (Tab
completion, Ctrl+R search, history navigation, ghost autosuggestion) is
exercised by feeding symbolic keys and asserting on buffer/cursor/effects.
The only subprocess case is the --plain passthrough, which runs the module
against a fake server to prove stdin/stdout forwarding and exit-code
preservation.
"""

import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

HERE = Path(__file__).resolve()
ROOT = HERE.parents[1]
SCRIPT = ROOT / "tools/megu_shell.py"

spec = importlib.util.spec_from_file_location("megu_shell", SCRIPT)
shell = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shell)


def types(editor, text):
    effects = []
    for ch in text:
        effects.extend(editor.feed(ch))
    return effects


class CommandCompleterTest(unittest.TestCase):
    def setUp(self):
        self.c = shell.CommandCompleter()

    def test_root_prefix(self):
        self.assertEqual(self.c.complete("gamem"), ["gamemode"])
        self.assertIn("gamemode", self.c.complete("gam"))
        self.assertIn("gamerule", self.c.complete("gam"))
        self.assertIn("megu", self.c.complete("me"))

    def test_slash_prefix(self):
        self.assertEqual(self.c.complete("/gamem"), ["gamemode"])
        self.assertEqual(self.c.complete("/sto"), ["stop", "stopsound"])
        self.assertEqual(self.c.complete("/stop"), ["stop", "stopsound"])

    def test_empty_line_lists_roots(self):
        roots = self.c.complete("")
        self.assertIn("stop", roots)
        self.assertIn("gamerule", roots)

    def test_time_subcommands(self):
        self.assertEqual(
            self.c.complete("time "), ["add", "get", "query", "set"])
        self.assertEqual(self.c.complete("time s"), ["set"])
        self.assertEqual(
            self.c.complete("time set "),
            ["day", "midnight", "night", "noon", "sunset"])
        self.assertEqual(self.c.complete("time set n"), ["night", "noon"])
        self.assertEqual(
            self.c.complete("time query "), ["day", "daytime", "gametime"])

    def test_weather_and_difficulty(self):
        self.assertEqual(
            self.c.complete("weather "), ["clear", "rain", "thunder"])
        self.assertEqual(
            self.c.complete("difficulty "),
            ["easy", "hard", "normal", "peaceful"])

    def test_gamemode_then_players(self):
        self.assertEqual(
            self.c.complete("gamemode "),
            ["adventure", "creative", "spectator", "survival"])
        self.c.add_player("Steve")
        self.assertEqual(self.c.complete("gamemode creative "), ["Steve"])
        self.assertEqual(
            self.c.complete("defaultgamemode c"), ["creative"])

    def test_gamerule_names_and_bool_values(self):
        self.assertEqual(self.c.complete("gamerule keepI"), ["keepInventory"])
        self.assertEqual(
            self.c.complete("gamerule keepInventory "), ["false", "true"])
        self.assertIn("mobGriefing", self.c.complete("gamerule "))
        # Integer rules offer no value candidates.
        self.assertEqual(
            self.c.complete("gamerule randomTickSpeed "), [])

    def test_whitelist_and_megu(self):
        self.assertEqual(
            self.c.complete("whitelist "),
            ["add", "list", "off", "on", "reload", "remove"])
        self.assertEqual(
            self.c.complete("megu "),
            ["mods", "policy", "reload", "status"])
        self.assertEqual(
            self.c.complete("megu policy "),
            ["get", "list", "reset", "set"])

    def test_player_tracking(self):
        self.c.add_player("Alex")
        self.c.add_player("Steve")
        self.assertEqual(self.c.complete("kick "), ["Alex", "Steve"])
        self.assertEqual(self.c.complete("tp S"), ["Steve"])
        self.assertEqual(self.c.complete("msg "), ["Alex", "Steve"])
        self.assertEqual(self.c.complete("whitelist add "), ["Alex", "Steve"])
        self.c.remove_player("Alex")
        self.assertEqual(self.c.complete("kick "), ["Steve"])

    def test_log_line_tracking(self):
        self.c.feed_log_line(
            "[spine] Alex joined from /127.0.0.1:5000 (uuid deadbeef)")
        self.assertEqual(self.c.complete("kick "), ["Alex"])
        self.c.feed_log_line("[spine] Alex disconnected: Timed out")
        self.assertEqual(self.c.complete("kick "), [])
        self.c.feed_log_line("Steve joined the game")
        self.assertEqual(self.c.complete("ban "), ["Steve"])
        self.c.feed_log_line("Steve left the game")
        self.assertEqual(self.c.complete("ban "), [])

    def test_common_prefix(self):
        self.assertEqual(
            shell.common_prefix(["gamemode", "gamerule"]), "game")
        self.assertEqual(shell.common_prefix(["stop"]), "stop")
        self.assertEqual(shell.common_prefix([]), "")

    def test_cursor_midline(self):
        self.assertEqual(
            self.c.complete("gamem x", cursor=4), ["gamemode", "gamerule"])

    def test_no_match(self):
        self.assertEqual(self.c.complete("zzz "), [])
        self.assertEqual(self.c.complete("weather z"), [])


class HistoryStoreTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = Path(self.tmp.name) / ".mc-spine-history"
        self.h = shell.HistoryStore(self.path)

    def test_round_trip(self):
        self.h.add("list")
        self.h.add("time set day")
        self.h.save()
        other = shell.HistoryStore(self.path)
        other.load()
        self.assertEqual(other.entries, ["list", "time set day"])

    def test_cap_enforced(self):
        h = shell.HistoryStore(self.path, cap=5)
        for i in range(10):
            h.add(f"cmd{i}")
        self.assertEqual(len(h.entries), 5)
        self.assertEqual(h.entries[0], "cmd5")

    def test_adjacent_dedup(self):
        self.h.add("list")
        self.h.add("list")
        self.h.add("stop")
        self.h.add("list")
        self.assertEqual(self.h.entries, ["list", "stop", "list"])

    def test_skips_empty_and_secrets(self):
        self.h.add("")
        self.h.add("   ")
        self.h.add("login admin password=hunter2")
        self.h.add("token abc123")
        self.assertEqual(self.h.entries, [])

    def test_save_is_atomic_and_complete(self):
        self.h.add("seed")
        self.h.save()
        self.assertTrue(self.path.exists())
        self.assertFalse((self.path.parent / ".mc-spine-history.tmp").exists())
        self.assertEqual(self.path.read_text().splitlines(), ["seed"])

    def test_suggest_most_recent_prefix(self):
        self.h.add("time set day")
        self.h.add("list")
        self.h.add("time set night")
        self.assertEqual(self.h.suggest("time"), "time set night")
        self.assertIsNone(self.h.suggest("time set night"))
        self.assertIsNone(self.h.suggest(""))
        self.assertIsNone(self.h.suggest("xyz"))

    def test_reverse_search(self):
        self.h.add("weather rain")
        self.h.add("list")
        self.h.add("weather clear")
        idx, entry = self.h.search("weather")
        self.assertEqual(entry, "weather clear")
        idx2, entry2 = self.h.search("weather", before=idx)
        self.assertEqual(entry2, "weather rain")
        self.assertEqual(self.h.search("nonexistent"), (None, None))
        self.assertEqual(self.h.search(""), (None, None))

    def test_load_missing_file(self):
        h = shell.HistoryStore(Path(self.tmp.name) / "nope")
        h.load()
        self.assertEqual(h.entries, [])


class HighlightTest(unittest.TestCase):
    def test_known_root_green(self):
        out = shell.highlight_command("gamemode creative")
        self.assertIn(shell.GREEN + "gamemode" + shell.RESET, out)

    def test_unknown_root_red(self):
        out = shell.highlight_command("gaemmode creative")
        self.assertIn(shell.RED + "gaemmode" + shell.RESET, out)

    def test_subcommand_cyan(self):
        out = shell.highlight_command("time set day")
        self.assertIn(shell.CYAN + "set" + shell.RESET, out)
        self.assertIn(shell.CYAN + "day" + shell.RESET, out)

    def test_number_and_coord_yellow(self):
        out = shell.highlight_command("time set 100")
        self.assertIn(shell.YELLOW + "100" + shell.RESET, out)
        out = shell.highlight_command("tp ~ ~5 ~-3")
        self.assertIn(shell.YELLOW + "~5" + shell.RESET, out)

    def test_plain_returns_identical(self):
        line = "gamemode creative Steve"
        self.assertEqual(shell.highlight_command(line, color=False), line)

    def test_no_color_env_has_zero_ansi(self):
        line = "gamemode creative"
        with mock.patch.dict(os.environ, {"NO_COLOR": "1"}):
            out = shell.highlight_command(line)
        self.assertEqual(out, line)
        self.assertNotIn("\x1b", out)


class LineEditorTest(unittest.TestCase):
    def setUp(self):
        self.c = shell.CommandCompleter()
        self.h = shell.HistoryStore(None)
        self.e = shell.LineEditor(self.c, self.h)

    def submit(self, text):
        types(self.e, text)
        effects = self.e.feed("enter")
        self.assertEqual(effects, [("submit", text)])

    def test_printable_and_backspace(self):
        types(self.e, "lis")
        self.assertEqual(self.e.buffer, "lis")
        self.e.feed("backspace")
        self.assertEqual(self.e.buffer, "li")
        self.assertEqual(self.e.cursor, 2)

    def test_ctrl_a_e_home_end(self):
        types(self.e, "list")
        self.e.feed("ctrl_a")
        self.assertEqual(self.e.cursor, 0)
        self.e.feed("ctrl_e")
        self.assertEqual(self.e.cursor, 4)
        self.e.feed("home")
        self.assertEqual(self.e.cursor, 0)
        self.e.feed("end")
        self.assertEqual(self.e.cursor, 4)

    def test_ctrl_u_kills_to_start(self):
        types(self.e, "time set day")
        self.e.feed("ctrl_a")
        self.e.cursor = 5
        self.e.feed("ctrl_u")
        self.assertEqual(self.e.buffer, "set day")
        self.assertEqual(self.e.cursor, 0)

    def test_ctrl_k_kills_to_end(self):
        types(self.e, "time set day")
        self.e.cursor = 8
        self.e.feed("ctrl_k")
        self.assertEqual(self.e.buffer, "time set")

    def test_ctrl_w_deletes_word(self):
        types(self.e, "time set day")
        self.e.feed("ctrl_w")
        self.assertEqual(self.e.buffer, "time set ")
        self.e.feed("ctrl_w")
        self.assertEqual(self.e.buffer, "time ")

    def test_left_right_delete(self):
        types(self.e, "ab")
        self.e.feed("left")
        self.e.feed("delete")
        self.assertEqual(self.e.buffer, "a")
        self.e.feed("right")
        self.assertEqual(self.e.feed("right"), [("bell",)])

    def test_enter_submits_and_records_history(self):
        self.submit("list")
        self.assertEqual(self.h.entries, ["list"])
        self.assertEqual(self.e.buffer, "")

    def test_right_accepts_ghost(self):
        self.h.add("time set day")
        types(self.e, "time")
        self.assertEqual(self.e.ghost(), " set day")
        self.e.feed("right")
        self.assertEqual(self.e.buffer, "time set day")
        self.assertEqual(self.e.cursor, len("time set day"))

    def test_end_accepts_ghost(self):
        self.h.add("weather rain")
        types(self.e, "wea")
        self.e.feed("end")
        self.assertEqual(self.e.buffer, "weather rain")

    def test_no_ghost_midline(self):
        self.h.add("list all")
        types(self.e, "lis")
        self.e.feed("left")
        self.assertEqual(self.e.ghost(), "")
        self.assertEqual(self.e.feed("right"), [])

    def test_up_down_history(self):
        self.h.add("list")
        self.h.add("seed")
        self.e.feed("up")
        self.assertEqual(self.e.buffer, "seed")
        self.e.feed("up")
        self.assertEqual(self.e.buffer, "list")
        self.assertEqual(self.e.feed("up"), [("bell",)])
        self.e.feed("down")
        self.assertEqual(self.e.buffer, "seed")
        self.e.feed("down")
        self.assertEqual(self.e.buffer, "")

    def test_prefix_aware_history(self):
        self.h.add("list")
        self.h.add("gamemode creative")
        self.h.add("seed")
        types(self.e, "ga")
        self.e.feed("up")
        self.assertEqual(self.e.buffer, "gamemode creative")
        self.e.feed("down")
        self.assertEqual(self.e.buffer, "ga")

    def test_tab_single_completion(self):
        types(self.e, "see")
        effects = self.e.feed("tab")
        self.assertEqual(effects, [])
        self.assertEqual(self.e.buffer, "seed ")

    def test_tab_common_prefix_then_cycle(self):
        types(self.e, "gam")
        effects = self.e.feed("tab")
        self.assertEqual(self.e.buffer, "game")
        kinds = [e[0] for e in effects]
        self.assertIn("candidates", kinds)
        cands = effects[0][1]
        self.assertEqual(cands, ["gamemode", "gamerule"])
        self.e.feed("tab")
        self.assertEqual(self.e.buffer, "gamemode")
        self.e.feed("tab")
        self.assertEqual(self.e.buffer, "gamerule")
        self.e.feed("tab")
        self.assertEqual(self.e.buffer, "gamemode")

    def test_tab_argument_completion(self):
        types(self.e, "time ")
        effects = self.e.feed("tab")
        self.assertEqual(effects[0][0], "candidates")
        self.assertIn("set", effects[0][1])

    def test_tab_no_candidates_bells(self):
        types(self.e, "zzz")
        self.assertEqual(self.e.feed("tab"), [("bell",)])

    def test_ctrl_r_search_accept(self):
        self.h.add("weather rain")
        self.h.add("list")
        self.e.feed("ctrl_r")
        types(self.e, "wea")
        self.assertEqual(self.e.buffer, "weather rain")
        effects = self.e.feed("enter")
        self.assertEqual(effects, [("accept",)])
        self.assertEqual(self.e.buffer, "weather rain")

    def test_ctrl_r_cycle_older(self):
        self.h.add("weather rain")
        self.h.add("weather clear")
        self.e.feed("ctrl_r")
        types(self.e, "weather")
        self.assertEqual(self.e.buffer, "weather clear")
        self.e.feed("ctrl_r")
        self.assertEqual(self.e.buffer, "weather rain")

    def test_ctrl_r_cancel_restores(self):
        self.h.add("list")
        types(self.e, "see")
        self.e.feed("ctrl_r")
        types(self.e, "li")
        self.assertEqual(self.e.buffer, "list")
        self.e.feed("escape")
        self.assertEqual(self.e.buffer, "see")
        self.assertEqual(self.e.cursor, 3)

    def test_ctrl_c_clears_line(self):
        types(self.e, "partial")
        effects = self.e.feed("ctrl_c")
        self.assertEqual(effects, [("cancel",)])
        self.assertEqual(self.e.buffer, "")

    def test_ctrl_c_empty_stops(self):
        self.assertEqual(self.e.feed("ctrl_c"), [("stop",)])

    def test_ctrl_d_empty_stops(self):
        self.assertEqual(self.e.feed("ctrl_d"), [("stop",)])

    def test_ctrl_d_deletes_forward(self):
        types(self.e, "ab")
        self.e.feed("ctrl_a")
        self.e.feed("ctrl_d")
        self.assertEqual(self.e.buffer, "b")


FAKE_SERVER = r"""
import sys
for line in sys.stdin:
    sys.stdout.write("echo:" + line)
    sys.stdout.flush()
    if line.strip() == "stop":
        break
sys.exit(7)
"""


class PassthroughTest(unittest.TestCase):
    def run_shell(self, stdin_text, extra_args=("--plain",)):
        return subprocess.run(
            [sys.executable, str(SCRIPT), *extra_args,
             "--server-bin", sys.executable, "--", "-c", FAKE_SERVER],
            input=stdin_text.encode(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=30,
        )

    def test_plain_forwards_and_preserves_exit_code(self):
        proc = self.run_shell("list\nstop\n")
        self.assertEqual(proc.returncode, 7)
        self.assertIn(b"echo:list\n", proc.stdout)
        self.assertIn(b"echo:stop\n", proc.stdout)

    def test_passthrough_without_plain_on_pipe(self):
        proc = self.run_shell("seed\nstop\n", extra_args=())
        self.assertEqual(proc.returncode, 7)
        self.assertIn(b"echo:seed\n", proc.stdout)

    def test_passthrough_emits_no_ansi(self):
        proc = self.run_shell("list\nstop\n")
        self.assertNotIn(b"\x1b[", proc.stdout)
        self.assertNotIn(b"\x1b[", proc.stderr)


class CliTest(unittest.TestCase):
    def complete(self, line):
        proc = subprocess.run(
            [sys.executable, str(SCRIPT), "--complete", line],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=15)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        return proc.stdout.decode().splitlines()

    def test_complete_root(self):
        self.assertEqual(self.complete("gamem"), ["gamemode"])

    def test_complete_argument(self):
        self.assertEqual(
            self.complete("time set "),
            ["day", "midnight", "night", "noon", "sunset"])

    def test_complete_weather(self):
        self.assertEqual(
            self.complete("weather "), ["clear", "rain", "thunder"])


if __name__ == "__main__":
    unittest.main()
