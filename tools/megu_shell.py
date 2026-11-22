#!/usr/bin/env python3
"""Minimal Zsh-like interactive console for megu servers.

  tools/megu_shell.py --server-bin ./target/release/megu -- <server-args...>
  tools/megu_shell.py --complete "time set "

Interactive mode (TTY only) gives the server console smart Tab completion,
inline history autosuggestions (accept with Right/End), live syntax
highlighting, emacs-style line editing, and Ctrl+R reverse history search.
The command tables mirror `src/commands_core.rs` (`native_command_dispatcher`
and `COMMAND_GAMERULE_DEFAULTS`).

Non-TTY, `--plain`, or `NO_COLOR` output is a transparent byte passthrough:
no ANSI, no history writes, and the exact server exit code.

History lives in `<server_dir>/.megu-history` (bounded, adjacent-deduped,
atomic save; empty lines and secret-looking entries are never written).
"""

import argparse
import os
from pathlib import Path
import re
import select
import subprocess
import sys
import threading

try:
    import termios
    import tty
except ImportError:  # Windows and other non-POSIX platforms.
    termios = None
    tty = None


# ---------- command tables (mirrors src/commands_core.rs) ----------

# Root literals `native_command_dispatcher` registers across all op levels.
COMMANDS = (
    "advancement", "attribute", "ban", "ban-ip", "banlist", "bossbar",
    "clear", "clone", "counts", "damage", "data", "datapack", "debug",
    "defaultgamemode", "deop", "difficulty", "effect", "enchant", "execute",
    "experience", "fill", "fillbiome", "forceload", "function", "gamemode",
    "gamerule", "give", "help", "item", "jfr", "kick", "kill", "list",
    "locate", "loot", "me", "megu", "msg", "op", "pardon", "pardon-ip",
    "particle", "perf", "place", "player", "playsound", "profile", "publish",
    "random", "recipe", "reload", "return", "ride", "save", "save-all",
    "save-off", "save-on", "say", "schedule", "scoreboard", "seed",
    "setblock", "setidletimeout", "setworldspawn", "spawnpoint", "spectate",
    "spreadplayers", "stop", "stopsound", "summon", "tag", "team", "teammsg",
    "teleport", "tell", "tellraw", "tick", "time", "title", "tm", "tp",
    "transfer", "trigger", "w", "weather", "whitelist", "worldborder", "xp",
    "?",
)

# `COMMAND_GAMERULE_DEFAULTS` (name, is_bool). Order matches the Rust table.
_GAMERULE_ROWS = (
    ("announceAdvancements", True), ("blockExplosionDropDecay", True),
    ("commandBlockOutput", True), ("commandModificationBlockLimit", False),
    ("disableElytraMovementCheck", True), ("disableRaids", True),
    ("doDaylightCycle", True), ("doEntityDrops", True), ("doFireTick", True),
    ("doImmediateRespawn", True), ("doInsomnia", True),
    ("doLimitedCrafting", True), ("doMobLoot", True), ("doMobSpawning", True),
    ("doPatrolSpawning", True), ("doTileDrops", True),
    ("doTraderSpawning", True), ("doVinesSpread", True),
    ("doWardenSpawning", True), ("doWeatherCycle", True),
    ("drowningDamage", True), ("enderPearlsVanishOnDeath", True),
    ("fallDamage", True), ("fireDamage", True), ("forgiveDeadPlayers", True),
    ("freezeDamage", True), ("globalSoundEvents", True),
    ("keepInventory", True), ("lavaSourceConversion", True),
    ("logAdminCommands", True), ("maxCommandChainLength", False),
    ("maxCommandForkCount", False), ("maxEntityCramming", False),
    ("mobExplosionDropDecay", True), ("mobGriefing", True),
    ("naturalRegeneration", True), ("playersNetherPortalCreativeDelay", False),
    ("playersNetherPortalDefaultDelay", False),
    ("playersSleepingPercentage", False), ("projectilesCanBreakBlocks", True),
    ("randomTickSpeed", False), ("reducedDebugInfo", True),
    ("sendCommandFeedback", True), ("showDeathMessages", True),
    ("snowAccumulationHeight", False), ("spawnChunkRadius", False),
    ("spawnRadius", False), ("spectatorsGenerateChunks", True),
    ("tntExplosionDropDecay", True), ("universalAnger", True),
    ("waterSourceConversion", True),
)
GAMERULES = tuple(name for name, _ in _GAMERULE_ROWS)
BOOL_GAMERULES = frozenset(name for name, is_bool in _GAMERULE_ROWS if is_bool)

_PLAYERS = "@players"
_GAMERULE_NAMES = "@gamerules"
_COMMAND_NAMES = "@commands"
BOOL_VALUES = ("false", "true")
_GAMEMODES = ("survival", "creative", "adventure", "spectator")

# First argument is an online player.
PLAYER_TARGET_COMMANDS = frozenset((
    "ban", "deop", "give", "kick", "kill", "msg", "op", "pardon",
    "spectate", "tell", "teleport", "tp", "w",
))

# Per-command argument completions keyed by the tuple of already-entered
# arguments. Dynamic markers resolve through CommandCompleter._resolve.
ARG_TABLES = {
    "time": {
        (): ["add", "get", "query", "set"],
        ("set",): ["day", "noon", "sunset", "night", "midnight"],
        ("query",): ["daytime", "gametime", "day"],
        ("get",): ["daytime", "gametime", "day"],
    },
    "weather": {(): ["clear", "rain", "thunder"]},
    "difficulty": {(): ["peaceful", "easy", "normal", "hard"]},
    "gamemode": {(): list(_GAMEMODES),
                 **{(m,): [_PLAYERS] for m in _GAMEMODES}},
    "defaultgamemode": {(): list(_GAMEMODES)},
    "gamerule": {(): [_GAMERULE_NAMES]},
    "whitelist": {
        (): ["on", "off", "list", "add", "remove", "reload"],
        ("add",): [_PLAYERS],
        ("remove",): [_PLAYERS],
    },
    "megu": {
        (): ["policy", "status", "mods", "reload"],
        ("policy",): ["list", "get", "set", "reset"],
    },
    "datapack": {
        (): ["list", "enable", "disable"],
        ("list",): ["enabled", "available"],
    },
    "tick": {(): ["query", "rate", "freeze", "unfreeze", "step", "sprint"]},
    "banlist": {(): ["ips", "players"]},
    "help": {(): [_COMMAND_NAMES]},
    "?": {(): [_COMMAND_NAMES]},
    "effect": {
        (): ["give", "clear"],
        ("give",): [_PLAYERS],
        ("clear",): [_PLAYERS],
    },
}


# ---------- completion ----------

_JOIN_PATTERNS = (
    re.compile(r"\[spine\] (\S+) joined from"),
    re.compile(r"(\S+) joined the game"),
)
_LEAVE_PATTERNS = (
    re.compile(r"\[spine\] (\S+) disconnected"),
    re.compile(r"(\S+) left the game"),
    re.compile(r"(\S+) lost connection"),
)


def common_prefix(candidates):
    """Longest shared prefix of `candidates` (empty string when none)."""
    if not candidates:
        return ""
    prefix = candidates[0]
    for cand in candidates[1:]:
        while not cand.startswith(prefix):
            prefix = prefix[:-1]
            if not prefix:
                return ""
    return prefix


class CommandCompleter:
    """Context-aware completion over the server's real command tree.

    `players` tracks the live online list parsed from server log lines so
    player arguments complete against who is actually connected.
    """

    def __init__(self):
        self.players = set()

    def add_player(self, name):
        if name:
            self.players.add(name)

    def remove_player(self, name):
        self.players.discard(name)

    def feed_log_line(self, line):
        """Update the online set from one server log line."""
        for pattern in _JOIN_PATTERNS:
            match = pattern.search(line)
            if match:
                self.add_player(match.group(1))
                return
        for pattern in _LEAVE_PATTERNS:
            match = pattern.search(line)
            if match:
                self.remove_player(match.group(1))
                return

    def _resolve(self, entry):
        if entry is None:
            return []
        out = []
        for item in entry:
            if item == _PLAYERS:
                out.extend(sorted(self.players))
            elif item == _GAMERULE_NAMES:
                out.extend(GAMERULES)
            elif item == _COMMAND_NAMES:
                out.extend(COMMANDS)
            else:
                out.append(item)
        return out

    def _arg_candidates(self, root, prev_args):
        table = ARG_TABLES.get(root)
        if table is None:
            if root in PLAYER_TARGET_COMMANDS and not prev_args:
                return sorted(self.players)
            return []
        entry = table.get(tuple(prev_args))
        if entry is None and root == "gamerule" and len(prev_args) == 1 \
                and prev_args[0] in BOOL_GAMERULES:
            entry = BOOL_VALUES
        return self._resolve(entry)

    def complete(self, line, cursor=None):
        """Candidates for the token under `cursor` (default: end of line)."""
        text = line if cursor is None else line[:cursor]
        text = text.lstrip()
        if text.startswith("/"):
            text = text[1:]
        if not text or text.endswith(" "):
            tokens = text.split()
            prefix = ""
        else:
            tokens = text.split()
            prefix = tokens.pop()
        if not tokens:
            pool = list(COMMANDS)
        else:
            pool = self._arg_candidates(tokens[0], tuple(tokens[1:]))
        return sorted({cand for cand in pool if cand.startswith(prefix)})


# ---------- history ----------

SECRET_KEY = re.compile(
    r"password|secret|token|credential|private[._-]?key|authorization", re.I)
HISTORY_CAP = 1000


class HistoryStore:
    """Bounded, deduplicated, atomically persisted command history."""

    def __init__(self, path, cap=HISTORY_CAP):
        self.path = Path(path) if path is not None else None
        self.cap = cap
        self.entries = []

    def load(self):
        self.entries = []
        if self.path is None or not self.path.exists():
            return
        try:
            for raw in self.path.read_text(encoding="utf-8").splitlines():
                line = raw.rstrip("\r")
                if line:
                    self.entries.append(line)
        except OSError:
            self.entries = []
        del self.entries[:-self.cap]

    def add(self, line):
        line = line.strip()
        if not line or SECRET_KEY.search(line):
            return
        if self.entries and self.entries[-1] == line:
            return
        self.entries.append(line)
        del self.entries[:-self.cap]

    def save(self):
        if self.path is None:
            return
        tmp = self.path.with_name(self.path.name + ".tmp")
        try:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            tmp.write_text("\n".join(self.entries[-self.cap:]) + "\n",
                           encoding="utf-8")
            os.replace(tmp, self.path)
        except OSError:
            try:
                tmp.unlink()
            except OSError:
                pass

    def suggest(self, prefix):
        """Most recent entry extending `prefix`; the autosuggestion source."""
        if not prefix:
            return None
        for entry in reversed(self.entries):
            if entry.startswith(prefix) and entry != prefix:
                return entry
        return None

    def search(self, query, before=None):
        """Most recent entry containing `query` before index `before`.

        Returns `(index, entry)` or `(None, None)`.
        """
        if not query:
            return (None, None)
        indices = range(len(self.entries) - 1, -1, -1)
        if before is not None:
            indices = range(min(before, len(self.entries)) - 1, -1, -1)
        for idx in indices:
            if query in self.entries[idx]:
                return (idx, self.entries[idx])
        return (None, None)


# ---------- highlighting ----------

RED = "\x1b[31m"
GREEN = "\x1b[32m"
YELLOW = "\x1b[33m"
CYAN = "\x1b[36m"
DIM = "\x1b[90m"
RESET = "\x1b[0m"

_NUMBER = re.compile(r"-?\d+(\.\d+)?")
_COORD = re.compile(r"[~^]-?\d*(\.\d+)?")


def color_enabled(color_flag):
    return color_flag and "NO_COLOR" not in os.environ


def _static_arg_candidates(root, prev_args):
    """Static (non-dynamic) argument candidates, for highlighting only."""
    entry = ARG_TABLES.get(root, {}).get(tuple(prev_args))
    if entry is None:
        if root == "gamerule":
            if not prev_args:
                return list(GAMERULES)
            if len(prev_args) == 1 and prev_args[0] in BOOL_GAMERULES:
                return list(BOOL_VALUES)
        return []
    return [item for item in entry if not item.startswith("@")]


def highlight_command(line, color=True):
    """Color a command line; returns it byte-identical when color is off.

    Root token green when known, red when not; known subcommands cyan;
    numbers and `~`/`^` coordinates yellow. With `color` false or `NO_COLOR`
    set the output contains no ANSI bytes at all.
    """
    if not color_enabled(color):
        return line
    tokens = [(m.group(0), m.start(), m.end())
              for m in re.finditer(r"\S+", line)]
    if not tokens:
        return line
    root = tokens[0][0].lstrip("/")
    out = []
    pos = 0
    for index, (token, start, end) in enumerate(tokens):
        out.append(line[pos:start])
        if index == 0:
            code = GREEN if root in COMMANDS else RED
        else:
            prev = tuple(t[0] for t in tokens[1:index])
            if token in _static_arg_candidates(root, prev):
                code = CYAN
            elif _NUMBER.fullmatch(token) or _COORD.fullmatch(token):
                code = YELLOW
            else:
                code = None
        out.append(f"{code}{token}{RESET}" if code else token)
        pos = end
    out.append(line[pos:])
    return "".join(out)


# ---------- line editor ----------

CTRL_KEYS = {
    "\x01": "ctrl_a", "\x03": "ctrl_c", "\x04": "ctrl_d", "\x05": "ctrl_e",
    "\x0b": "ctrl_k", "\x12": "ctrl_r", "\x15": "ctrl_u", "\x17": "ctrl_w",
}
WORD_BREAK = re.compile(r"\s+")


class LineEditor:
    """Deterministic editing state machine — testable without a PTY.

    `feed(key)` takes a printable character or a symbolic key name and
    returns effect tuples: ("submit", line), ("cancel",), ("stop",),
    ("bell",), ("candidates", [...]).
    """

    def __init__(self, completer=None, history=None):
        self.completer = completer or CommandCompleter()
        self.history = history
        self.buffer = ""
        self.cursor = 0
        self._hist_index = None
        self._hist_filter = ""
        self._hist_saved = ""
        self._search = None
        self._tab = None

    # ---- queries ----

    def ghost(self):
        """History autosuggestion suffix shown after the cursor."""
        if self.history is None or self.cursor != len(self.buffer):
            return ""
        entry = self.history.suggest(self.buffer)
        return entry[len(self.buffer):] if entry else ""

    # ---- internal helpers ----

    def _token_span(self):
        """(start, end) of the whitespace-delimited token under the cursor."""
        start = self.cursor
        while start > 0 and self.buffer[start - 1] not in " \t":
            start -= 1
        end = self.cursor
        while end < len(self.buffer) and self.buffer[end] not in " \t":
            end += 1
        return start, end

    def _reset_transient(self):
        self._hist_index = None
        self._tab = None

    def _insert(self, text):
        self.buffer = self.buffer[:self.cursor] + text + self.buffer[self.cursor:]
        self.cursor += len(text)

    def _history_step(self, backwards):
        entries = self.history.entries if self.history else []
        if self._hist_index is None:
            self._hist_filter = self.buffer
            self._hist_saved = self.buffer
            self._hist_index = len(entries)
        step = -1 if backwards else 1
        idx = self._hist_index + step
        while 0 <= idx < len(entries):
            if entries[idx].startswith(self._hist_filter):
                self._hist_index = idx
                self.buffer = entries[idx]
                self.cursor = len(self.buffer)
                return True
            idx += step
        if not backwards:
            self._hist_index = None
            self.buffer = self._hist_saved
            self.cursor = len(self.buffer)
            return True
        return False

    def _search_feed(self, key):
        """Handle one key while Ctrl+R reverse-search is active."""
        state = self._search
        if key == "enter":
            self._search = None
            return [("accept",)]
        if key in ("escape", "ctrl_c"):
            self.buffer = state["saved"]
            self.cursor = state["saved_cursor"]
            self._search = None
            return [("cancel",)]
        if key == "ctrl_r":
            idx, entry = self.history.search(state["query"],
                                           before=state.get("match"))
            if entry is None:
                return [("bell",)]
            state["match"] = idx
            self.buffer = entry
            self.cursor = len(entry)
            return []
        if key == "backspace":
            state["query"] = state["query"][:-1]
        elif len(key) == 1 and key >= " ":
            state["query"] += key
        else:
            return [("bell",)]
        idx, entry = self.history.search(state["query"])
        if entry is None:
            state["match"] = None
            return [("bell",)]
        state["match"] = idx
        self.buffer = entry
        self.cursor = len(entry)
        return []

    def _complete(self):
        cands = self.completer.complete(self.buffer, self.cursor)
        if self._tab is not None:
            state = self._tab
            state["idx"] = (state["idx"] + 1) % len(state["cands"])
            start, end = state["start"], self.cursor
            while end < len(self.buffer) and self.buffer[end] not in " \t":
                end += 1
            self.buffer = (self.buffer[:start] + state["cands"][state["idx"]]
                           + self.buffer[end:])
            self.cursor = start + len(state["cands"][state["idx"]])
            return []
        if not cands:
            return [("bell",)]
        start, end = self._token_span()
        token = self.buffer[start:end]
        if len(cands) == 1:
            insertion = cands[0] + " "
            self.buffer = self.buffer[:start] + insertion + self.buffer[end:]
            self.cursor = start + len(insertion)
            return []
        prefix = common_prefix(cands)
        if len(prefix) > len(token):
            self.buffer = self.buffer[:start] + prefix + self.buffer[end:]
            self.cursor = start + len(prefix)
        self._tab = {"start": start, "cands": cands, "idx": -1}
        return [("candidates", cands)]

    # ---- public entry point ----

    def feed(self, key):
        if key in CTRL_KEYS:
            key = CTRL_KEYS[key]
        if self._search is not None:
            self._tab = None
            return self._search_feed(key)
        if key != "tab":
            self._tab = None
        if key not in ("up", "down"):
            self._hist_index = None

        if key == "enter":
            line = self.buffer
            if self.history is not None:
                self.history.add(line)
            self.buffer = ""
            self.cursor = 0
            self._reset_transient()
            return [("submit", line)]
        if key == "ctrl_c":
            if self.buffer:
                self.buffer = ""
                self.cursor = 0
                self._reset_transient()
                return [("cancel",)]
            return [("stop",)]
        if key == "ctrl_d":
            if not self.buffer:
                return [("stop",)]
            if self.cursor < len(self.buffer):
                self.buffer = self.buffer[:self.cursor] + self.buffer[self.cursor + 1:]
                return []
            return [("bell",)]
        if key == "escape":
            self._reset_transient()
            return []
        if key == "backspace":
            if self.cursor == 0:
                return [("bell",)]
            self.buffer = self.buffer[:self.cursor - 1] + self.buffer[self.cursor:]
            self.cursor -= 1
            return []
        if key == "delete":
            if self.cursor >= len(self.buffer):
                return [("bell",)]
            self.buffer = self.buffer[:self.cursor] + self.buffer[self.cursor + 1:]
            return []
        if key == "left":
            if self.cursor == 0:
                return [("bell",)]
            self.cursor -= 1
            return []
        if key == "right":
            if self.cursor == len(self.buffer):
                suggestion = self.ghost()
                if suggestion:
                    self.buffer += suggestion
                    self.cursor = len(self.buffer)
                    return []
                return [("bell",)]
            self.cursor += 1
            return []
        if key in ("home", "ctrl_a"):
            self.cursor = 0
            return []
        if key in ("end", "ctrl_e"):
            suggestion = self.ghost()
            if suggestion:
                self.buffer += suggestion
            self.cursor = len(self.buffer)
            return []
        if key == "ctrl_u":
            self.buffer = self.buffer[self.cursor:]
            self.cursor = 0
            return []
        if key == "ctrl_k":
            self.buffer = self.buffer[:self.cursor]
            return []
        if key == "ctrl_w":
            start = self.cursor
            while start > 0 and self.buffer[start - 1] in " \t":
                start -= 1
            while start > 0 and self.buffer[start - 1] not in " \t":
                start -= 1
            self.buffer = self.buffer[:start] + self.buffer[self.cursor:]
            self.cursor = start
            return []
        if key == "up":
            if self._history_step(True):
                return []
            return [("bell",)]
        if key == "down":
            if self._hist_index is None:
                return [("bell",)]
            self._history_step(False)
            return []
        if key == "tab":
            return self._complete()
        if key == "ctrl_r":
            if self.history is None:
                return [("bell",)]
            self._search = {"query": "", "saved": self.buffer,
                            "saved_cursor": self.cursor, "match": None}
            return []
        if len(key) == 1 and key >= " ":
            self._insert(key)
            return []
        return [("bell",)]


# ---------- terminal plumbing ----------

_ESCAPE_KEYS = {
    "A": "up", "B": "down", "C": "right", "D": "left",
    "H": "home", "F": "end", "Z": "shift_tab",
}
_TILDE_KEYS = {
    "1": "home", "3": "delete", "4": "end", "5": "up", "6": "down",
    "7": "home", "8": "end",
}


def _read_key(fd):
    """Read one logical keypress from a raw-mode file descriptor."""
    ch = os.read(fd, 1)
    if not ch:
        return "ctrl_d"
    ch = ch.decode("utf-8", "replace")
    if ch != "\x1b":
        if ch in ("\r", "\n"):
            return "enter"
        if ch == "\t":
            return "tab"
        if ch in ("\x7f", "\x08"):
            return "backspace"
        return ch
    ready, _, _ = select.select([fd], [], [], 0.03)
    if not ready:
        return "escape"
    seq = os.read(fd, 1).decode("ascii", "replace")
    if seq == "O":
        ready, _, _ = select.select([fd], [], [], 0.03)
        if ready:
            return _ESCAPE_KEYS.get(os.read(fd, 1).decode("ascii", "replace"),
                                    "escape")
        return "escape"
    if seq != "[":
        return "escape"
    data = ""
    while len(data) < 8:
        ready, _, _ = select.select([fd], [], [], 0.03)
        if not ready:
            break
        c = os.read(fd, 1).decode("ascii", "replace")
        data += c
        if c.isalpha() or c == "~":
            break
    if data.endswith("~"):
        return _TILDE_KEYS.get(data[:-1], "escape")
    return _ESCAPE_KEYS.get(data[:1], "escape")


def _render(prompt, editor, color):
    shown = highlight_command(editor.buffer, color=color)
    ghost = editor.ghost()
    if ghost and color_enabled(color):
        shown += DIM + ghost + RESET
    line = "\r\x1b[K" + prompt + shown
    column = len(prompt) + editor.cursor
    line += "\r" + (f"\x1b[{column}C" if column else "")
    return line


def _spawn_server(server_bin, server_args, server_dir):
    return subprocess.Popen(
        [server_bin, *server_args],
        cwd=server_dir,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )


def _send(server, line):
    try:
        server.stdin.write(line.encode("utf-8") + b"\n")
        server.stdin.flush()
        return True
    except (BrokenPipeError, OSError, ValueError):
        return False


def run_passthrough(server_bin, server_args, server_dir):
    """Transparent stdin->server / server->stdout pipe. Zero ANSI bytes."""
    server = _spawn_server(server_bin, server_args, server_dir)

    def pump():
        try:
            for chunk in iter(lambda: server.stdout.read1(65536), b""):
                sys.stdout.buffer.write(chunk)
                sys.stdout.buffer.flush()
        except (OSError, ValueError):
            pass

    reader = threading.Thread(target=pump, daemon=True)
    reader.start()
    try:
        while True:
            chunk = sys.stdin.buffer.read(65536)
            if not chunk:
                break
            if server.stdin is None:
                break
            try:
                server.stdin.write(chunk)
                server.stdin.flush()
            except (BrokenPipeError, OSError, ValueError):
                break
    except KeyboardInterrupt:
        pass
    finally:
        try:
            if server.stdin:
                server.stdin.close()
        except OSError:
            pass
    code = server.wait()
    reader.join(timeout=2)
    return code


def run_interactive(server_bin, server_args, server_dir, prompt="megu> ",
                    color=True):
    """Raw-mode console: completion, ghost hints, highlighting, history."""
    history = HistoryStore(Path(server_dir) / ".megu-history")
    history.load()
    completer = CommandCompleter()
    editor = LineEditor(completer, history)
    server = _spawn_server(server_bin, server_args, server_dir)
    fd = sys.stdin.fileno()
    out = sys.stdout
    lock = threading.Lock()
    done = threading.Event()

    def pump():
        try:
            for raw in iter(server.stdout.readline, b""):
                text = raw.decode("utf-8", "replace").rstrip("\r\n")
                completer.feed_log_line(text)
                with lock:
                    out.write("\r\x1b[K" + text + "\n"
                              + _render(prompt, editor, color))
                    out.flush()
        except (OSError, ValueError):
            pass
        finally:
            done.set()

    reader = threading.Thread(target=pump, daemon=True)
    reader.start()
    old = termios.tcgetattr(fd)
    stop_sent = False
    try:
        tty.setraw(fd)
        with lock:
            out.write(_render(prompt, editor, color))
            out.flush()
        while not done.is_set():
            ready, _, _ = select.select([fd], [], [], 0.1)
            if not ready:
                if server.poll() is not None:
                    break
                continue
            key = _read_key(fd)
            effects = editor.feed(key)
            for effect in effects:
                kind = effect[0]
                if kind == "submit":
                    _send(server, effect[1])
                elif kind == "stop":
                    _send(server, "stop")
                    stop_sent = True
                elif kind == "cancel":
                    with lock:
                        out.write("\r\x1b[K" + prompt + "^C\n")
                elif kind == "candidates":
                    with lock:
                        out.write("\r\x1b[K" + "  ".join(effect[1]) + "\n")
                elif kind == "bell":
                    with lock:
                        out.write("\a")
            with lock:
                out.write(_render(prompt, editor, color))
                out.flush()
            if stop_sent or server.poll() is not None:
                break
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, old)
        history.save()
    code = server.wait()
    reader.join(timeout=2)
    with lock:
        out.write("\r\x1b[K")
        out.flush()
    return code


def main(argv=None):
    parser = argparse.ArgumentParser(
        prog="megu_shell",
        description="Minimal Zsh-like interactive console for megu.")
    parser.add_argument("--server-bin",
                        help="path to the megu server binary")
    parser.add_argument("--server-dir", default=".",
                        help="server working directory (history lives here)")
    parser.add_argument("--plain", action="store_true",
                        help="disable colors; non-TTY becomes pure passthrough")
    parser.add_argument("--complete", metavar="LINE",
                        help="print completions for LINE and exit")
    argv = list(sys.argv[1:] if argv is None else argv)
    if "--" in argv:
        sep = argv.index("--")
        server_args = argv[sep + 1:]
        argv = argv[:sep]
    else:
        server_args = []
    args = parser.parse_args(argv)

    if args.complete is not None:
        for cand in CommandCompleter().complete(args.complete):
            print(cand)
        return 0

    if not args.server_bin:
        parser.error("--server-bin is required unless --complete is used")

    color = not args.plain
    interactive = (
        not args.plain
        and termios is not None
        and sys.stdin.isatty()
        and sys.stdout.isatty()
    )
    if interactive:
        return run_interactive(args.server_bin, server_args,
                               args.server_dir, color=color)
    return run_passthrough(args.server_bin, server_args, args.server_dir)


if __name__ == "__main__":
    sys.exit(main())
