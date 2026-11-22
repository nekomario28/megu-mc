#!/usr/bin/env python3
"""Terminal-art dashboard and live telemetry wrapper for the mc-spine server CLI.

Three modes:

    python3 tools/megu_tui.py --server-bin ./target/release/mc-spine -- <server-args...>
        Spawn the server under a live dashboard: banner, TPS/MSPT/RSS
        sparklines, status cards, a scrolling log tail, and a '/' command
        line that forwards operator commands to the server's stdin console.

    python3 tools/megu_tui.py --demo-frame [--ascii] [--cols N --rows M]
        Render one deterministic demo frame and exit 0. Used for tests and
        screenshots: the output depends only on the arguments, never on the
        clock or on randomness.

    python3 tools/megu_tui.py --render-snapshot <logfile|->
        Parse a captured server log and render one frame from it ('-' reads
        stdin). Also deterministic.

Fallback contract (zero side effects in pipes, CI, and headless logs):
  * stdout not a TTY, TERM unset/dumb/unknown, NO_COLOR set, or an explicit
    --plain / --no-tui flag -> plain stdio passthrough. The child's stdin,
    stdout, and stderr are inherited untouched, this wrapper emits no ANSI,
    and the wrapper exits with the server's exact exit code (128+signal for
    signal deaths).
  * --ascii renders the dashboard with pure-ASCII glyphs and no color.

Shutdown contract:
  * 'q', Ctrl+C, or SIGTERM first sends a clean "stop" line to the server's
    stdin console (the server dispatches console lines as level-4 commands,
    so /stop saves the world and exits), waits up to --stop-timeout seconds,
    then escalates SIGTERM, then SIGKILL as a last resort.
  * Terminal state (cbreak mode, cursor visibility, alternate screen) is
    always restored from a finally block, including on exceptions.

Pure Python 3 standard library: no third-party dependencies, no files
written, no network use.
"""

import argparse
import collections
import os
import re
import shutil
import subprocess
import sys
import threading
import time

try:
    import select
except ImportError:  # pragma: no cover - always present on POSIX
    select = None
try:
    import signal
except ImportError:  # pragma: no cover
    signal = None
try:
    import termios
    import tty
except ImportError:  # Windows: the interactive mode falls back to passthrough.
    termios = None
    tty = None

SPARK_BLOCKS = "▁▂▃▄▅▆▇█"
SPARK_ASCII = "._-=+*#@"

ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")

COLORS = {
    "reset": "\x1b[0m",
    "bold": "\x1b[1m",
    "dim": "\x1b[2m",
    "purple": "\x1b[38;5;141m",
    "pink": "\x1b[38;5;213m",
    "cyan": "\x1b[38;5;87m",
    "green": "\x1b[38;5;82m",
    "yellow": "\x1b[38;5;221m",
    "red": "\x1b[38;5;203m",
    "grey": "\x1b[38;5;245m",
    "white": "\x1b[38;5;255m",
}

MC_VERSIONS = "1.21.1 / 26.3"
DEFAULT_COLS = 80
DEFAULT_ROWS = 24
STOP_COMMAND = "stop"
ESCALATE_GRACE = 4.0

LOGO = (
    "███╗   ███╗███████╗ ██████╗ ██╗   ██╗",
    "████╗ ████║██╔════╝██╔════╝ ██║   ██║",
    "██╔████╔██║█████╗  ██║  ███╗██║   ██║",
    "██║╚██╔╝██║██╔══╝  ██║   ██║██║   ██║",
    "██║ ╚═╝ ██║███████╗╚██████╔╝╚██████╔╝",
    "╚═╝     ╚═╝╚══════╝ ╚═════╝  ╚═════╝ ",
)
LOGO_ASCII = (
    " __  __  _____  ____   _   _ ",
    "|  \\/  || ____|/ ___| | | | |",
    "| |\\/| ||  _|  | |  _ | | | |",
    "| |  | || |___ | |_| || |_| |",
    "|_|  |_||_____| \\____| \\___/ ",
)
TAGLINE = " mc-spine · server telemetry console"
TAGLINE_ASCII = " mc-spine - server telemetry console"
WORDMARK = "═╡ MEGU · mc-spine ╞═"
WORDMARK_ASCII = "==[ MEGU mc-spine ]=="


# ---------------------------------------------------------------- text utils

def strip_ansi(text):
    return ANSI_RE.sub("", text)


def vlen(text):
    """Visible column count of a string that may carry ANSI sequences."""
    return len(strip_ansi(text))


def clip(text, width):
    """Fit `text` to exactly `width` visible columns, preserving ANSI codes."""
    if width <= 0:
        return ""
    out = []
    visible = 0
    pos = 0
    for match in ANSI_RE.finditer(text):
        segment = text[pos:match.start()]
        if visible + len(segment) > width:
            out.append(segment[:width - visible])
            visible = width
            break
        out.append(segment)
        visible += len(segment)
        out.append(match.group(0))
        pos = match.end()
    else:
        segment = text[pos:]
        out.append(segment[:width - visible])
        visible += min(len(segment), max(0, width - visible))
    if visible < width:
        out.append(" " * (width - visible))
    line = "".join(out)
    # Truncation can drop a trailing reset; re-close so colors cannot bleed.
    if "\x1b[" in line and not line.endswith(COLORS["reset"]):
        line += COLORS["reset"]
    return line


def fit_plain(text, width, fill=" "):
    """Truncate or pad a plain (no-ANSI) string to `width` columns."""
    if len(text) >= width:
        return text[:width]
    return text + fill * (width - len(text))


def center_vis(text, width):
    """Center a possibly-colored string inside `width` visible columns."""
    pad = width - vlen(text)
    if pad <= 0:
        return clip(text, width).rstrip()
    left = pad // 2
    return " " * left + text + " " * (pad - left)


def asciify(text):
    return text.encode("ascii", "replace").decode("ascii")


def paint(text, name, color):
    if not color or name is None:
        return text
    return COLORS[name] + text + COLORS["reset"]


def finite(value):
    return (isinstance(value, (int, float)) and not isinstance(value, bool)
            and value == value
            and value not in (float("inf"), float("-inf")))


def fmt(value, digits=2, na="n/a"):
    if value is None:
        return na
    return f"{value:.{digits}f}"


def percentile(values, pct):
    """Nearest-rank percentile over the finite values; None when empty."""
    vals = sorted(v for v in values if finite(v))
    if not vals:
        return None
    rank = int(len(vals) * pct / 100.0 + 0.5) - 1
    rank = 0 if rank < 0 else len(vals) - 1 if rank >= len(vals) else rank
    return vals[rank]


def sparkline(values, width, low=None, high=None, ascii_mode=False):
    """Render the last `width` values as a sparkline of exactly `width` cells.

    `low`/`high` fix the scale; None derives that bound from the finite data.
    Values clamp to the ends, NaN renders as a gap, +Inf/-Inf pin the top and
    bottom cells, and a flat series sits on the middle rung so "no variation"
    does not look like "zero".
    """
    chars = SPARK_ASCII if ascii_mode else SPARK_BLOCKS
    if width <= 0:
        return ""
    vals = list(values)[-width:]
    out = [" "] * (width - len(vals))
    numbers = [float(v) for v in vals if finite(v)]
    if low is None:
        low = min(numbers) if numbers else 0.0
    if high is None:
        high = max(numbers) if numbers else low
    span = high - low
    mid = len(chars) // 2
    for value in vals:
        try:
            v = float(value)
        except (TypeError, ValueError):
            out.append(" ")
            continue
        if v != v:
            out.append(" ")
        elif v == float("inf"):
            out.append(chars[-1])
        elif v == float("-inf"):
            out.append(chars[0])
        elif span <= 0:
            out.append(chars[mid])
        else:
            index = int((v - low) / span * (len(chars) - 1))
            index = 0 if index < 0 else len(chars) - 1 if index > len(chars) - 1 else index
            out.append(chars[index])
    return "".join(out)


# ---------------------------------------------------------------- log parser

RE_READY = re.compile(
    r"mc-spine v([0-9]+(?:\.[0-9]+)*)\s+listening\s+(\S+):(\d+)")
RE_TICK = re.compile(
    r"\[tick\]\s+observed_tps=([0-9.]+)\s+avg_work_ms=([0-9.]+)\s+"
    r"max_work_ms=([0-9.]+)\s+over_budget=(\d+)/(\d+)")
RE_VMRSS = re.compile(r"VmRSS:\s*(\d+)\s*kB")
RE_SZ_TIME = re.compile(r"\[sz\]\s+t=(\d+)")
RE_CLIENTS = re.compile(r"\bclients=(\d+)")
RE_JOIN = re.compile(
    r"\[spine\]\s+(\S+)\s+joined from\s+(\S+)\s+\(uuid\s+([0-9a-fA-F]+)\)")
RE_LEFT = re.compile(r"\[spine\]\s+(\S+)\s+left\s+\(from\s+\S+\)")
RE_DISCONNECTED = re.compile(r"\[spine\]\s+(\S+)\s+disconnected:")
RE_MODS_SUMMARY = re.compile(
    r"\[live-mods\]\s+(\d+)\s+jar\(s\):\s+(\d+)\s+auto-skipped,\s+"
    r"(\d+)\s+native-bundle,\s+(\d+)\s+tier2-shim,\s+(\d+)\s+tier3")
RE_DUAL_LOADER = re.compile(
    r"\[live-mods\]\s+dual-loader:\s+(\d+)\s+unified,\s+(\d+)\s+client-only,\s+"
    r"(\d+)\s+superseded,\s+(\d+)\s+rejected,\s+(\d+)\s+duplicate ids,\s+"
    r"(\d+)\s+missing deps,\s+(\d+)\s+verified damage hooks")
RE_MOD_DETAIL = re.compile(
    r"\[live-mods\]\s+(\S+)\s+\((\S+)\)\s+(tier2 shim|tier3)\s*:\s*(.*)")
RE_MODS_SCANNED = re.compile(
    r"\[live-mods\]\s+mods dir scanned:\s+(\d+)\s+jar\(s\)")
RE_STATUS = re.compile(
    r"^Status:\s+(\w+)\s+\(([0-9.]+)\s+TPS,\s*([0-9.]+)\s+ms/tick\)")
RE_PLAYERS = re.compile(r"^Players:\s+(\d+)/(\d+)(?:\s+\(([^)]*)\))?")
RE_CHUNKS = re.compile(r"^Loaded chunks:\s+(\d+)")
RE_MEMORY = re.compile(r"^Memory:\s+(\d+)\s+MB")
RE_MODS_STATUS = re.compile(
    r"^Mods:\s+(\d+)\s+native(?:\s*,\s*(\d+)\s+compatibility mode)?")
RE_LAN = re.compile(r"\[lan\]\s+advertising\s+\"([^\"]*)\"\s+on port\s+(\d+)")
RE_FATAL = re.compile(r"\[FATAL\]|panicked")
RE_ERROR = re.compile(r"\berror\b|\bfailed\b|FAILED|refusing")
RE_WARN = re.compile(r"\[security\]|refused|rejected|\bwarn\b|skipped|missing")
RE_MS_PER_CHUNK_KV = re.compile(
    r"\bms/chunk\s*[=:]\s*([0-9]+(?:\.[0-9]+)?)")
RE_MS_PER_CHUNK_PAREN = re.compile(
    r"\(([0-9]+(?:\.[0-9]+)?)\s*ms/chunk\)")
RE_CHUNKPROF_MS = re.compile(
    r"\bCHUNKPROF\b.*?\bmap=([0-9]+(?:\.[0-9]+)?)\s*ms\b", re.IGNORECASE)


def ms_per_chunk_from(line):
    """ms-per-chunk figure from a profiling line; None when none is present."""
    for pattern in (RE_MS_PER_CHUNK_KV, RE_MS_PER_CHUNK_PAREN,
                    RE_CHUNKPROF_MS):
        match = pattern.search(line)
        if match:
            return float(match.group(1))
    return None


class MetricsParser:
    """Fold raw mc-spine stdout/stderr lines into a rolling metrics snapshot.

    Every line lands in the `log` ring buffer; lines whose shape is known are
    additionally decoded into structured state. feed() returns the matched
    kind so tests and future consumers can route on it.
    """

    def __init__(self, history=120, log_lines=400):
        self.tps = collections.deque(maxlen=history)
        self.mspt = collections.deque(maxlen=history)
        self.mem_mib = collections.deque(maxlen=history)
        self.cpu_pct = collections.deque(maxlen=history)
        self.threads = None
        self.ms_per_chunk = collections.deque(maxlen=history)
        self.revision = 0
        self.players = {}
        self.players_online = None
        self.players_max = None
        self.loaded_chunks = None
        self.mods = {"auto_skipped": 0, "native": 0, "shim": 0, "fallback": 0}
        self.jars_seen = 0
        self.mod_details = []
        self.dual_loader = {}
        self.errors = 0
        self.warnings = 0
        self.panics = 0
        self.over_budget = 0
        self.ticks_sampled = 0
        self.uptime_ticks = None
        self.clients = None
        self.ready = False
        self.version = None
        self.bind = None
        self.port = None
        self.motd = None
        self.status_label = "Booting"
        self.log = collections.deque(maxlen=log_lines)
        self.lines_seen = 0

    def feed(self, line):
        line = strip_ansi(line).rstrip()
        self.lines_seen += 1
        self.revision += 1
        self.log.append(line)

        mspc = ms_per_chunk_from(line)
        if mspc is not None:
            self.ms_per_chunk.append(mspc)

        match = RE_READY.search(line)
        if match:
            self.ready = True
            self.status_label = "Running"
            self.version = match.group(1)
            self.bind = match.group(2)
            self.port = int(match.group(3))
            return "ready"

        match = RE_TICK.search(line)
        if match:
            self.tps.append(float(match.group(1)))
            self.mspt.append(float(match.group(2)))
            self.over_budget += int(match.group(4))
            self.ticks_sampled += int(match.group(5))
            return "tick"

        if line.startswith("[sz]"):
            match = RE_SZ_TIME.search(line)
            if match:
                self.uptime_ticks = int(match.group(1))
            match = RE_VMRSS.search(line)
            if match:
                self.mem_mib.append(int(match.group(1)) / 1024.0)
            match = RE_CLIENTS.search(line)
            if match:
                self.clients = int(match.group(1))
            return "size"

        match = RE_JOIN.search(line)
        if match:
            self.players[match.group(1)] = match.group(2)
            self.players_online = len(self.players)
            return "join"

        match = RE_LEFT.search(line) or RE_DISCONNECTED.search(line)
        if match:
            self.players.pop(match.group(1), None)
            self.players_online = len(self.players)
            return "leave"

        match = RE_MODS_SUMMARY.search(line)
        if match:
            self.jars_seen = int(match.group(1))
            self.mods = {
                "auto_skipped": int(match.group(2)),
                "native": int(match.group(3)),
                "shim": int(match.group(4)),
                "fallback": int(match.group(5)),
            }
            return "mods"

        match = RE_DUAL_LOADER.search(line)
        if match:
            self.jars_seen = self.jars_seen or int(match.group(1))
            self.dual_loader = {
                "unified": int(match.group(1)),
                "client_only": int(match.group(2)),
                "superseded": int(match.group(3)),
                "rejected": int(match.group(4)),
                "duplicate_ids": int(match.group(5)),
                "missing_deps": int(match.group(6)),
                "damage_hooks": int(match.group(7)),
            }
            return "mods"

        match = RE_MOD_DETAIL.search(line)
        if match:
            self.mod_details.append({
                "file": match.group(1),
                "mod_id": match.group(2),
                "tier": match.group(3),
                "detail": match.group(4).strip(),
            })
            return "mod"

        match = RE_MODS_SCANNED.search(line)
        if match:
            self.jars_seen = int(match.group(1))
            return "mods"

        match = RE_STATUS.search(line)
        if match:
            self.status_label = match.group(1)
            self.tps.append(float(match.group(2)))
            self.mspt.append(float(match.group(3)))
            return "status"

        match = RE_PLAYERS.search(line)
        if match:
            self.players_online = int(match.group(1))
            self.players_max = int(match.group(2))
            if match.group(3) is not None:
                names = [name.strip() for name in match.group(3).split(",")]
                self.players = {name: None for name in names if name}
            return "players"

        match = RE_CHUNKS.search(line)
        if match:
            self.loaded_chunks = int(match.group(1))
            return "chunks"

        match = RE_MEMORY.search(line)
        if match:
            self.mem_mib.append(float(match.group(1)))
            return "memory"

        match = RE_MODS_STATUS.search(line)
        if match:
            self.mods["native"] = int(match.group(1))
            if match.group(2) is not None:
                self.mods["shim"] = int(match.group(2))
            return "mods"

        match = RE_LAN.search(line)
        if match:
            self.motd = match.group(1)
            self.port = self.port or int(match.group(2))
            return "lan"

        if RE_FATAL.search(line):
            self.panics += 1
            self.errors += 1
            self.status_label = "Crash guard"
        elif RE_ERROR.search(line):
            self.errors += 1
        elif RE_WARN.search(line):
            self.warnings += 1
        return "ms_per_chunk" if mspc is not None else "log"

    def note_proc_metrics(self, rss_mib, threads, cpu_pct):
        """Record one /proc sample: RSS MiB, thread count, delta CPU %."""
        changed = False
        if finite(rss_mib) and rss_mib >= 0:
            self.mem_mib.append(float(rss_mib))
            changed = True
        if (isinstance(threads, int) and not isinstance(threads, bool)
                and threads >= 0):
            self.threads = threads
            changed = True
        if finite(cpu_pct) and cpu_pct >= 0:
            self.cpu_pct.append(float(cpu_pct))
            changed = True
        if changed:
            self.revision += 1

    def note_memory(self, mib):
        """Record an external RSS sample (from /proc/<pid>/status)."""
        self.note_proc_metrics(mib, None, None)

    def snapshot(self):
        tps = list(self.tps)
        mspt = list(self.mspt)
        mem = list(self.mem_mib)
        cpu = list(self.cpu_pct)
        mspc = list(self.ms_per_chunk)
        online = (self.players_online if self.players_online is not None
                  else len(self.players))
        return {
            "ready": self.ready,
            "version": self.version,
            "bind": self.bind,
            "port": self.port,
            "motd": self.motd,
            "status": self.status_label,
            "tps": tps,
            "tps_cur": tps[-1] if tps else None,
            "tps_min": min(tps) if tps else None,
            "tps_avg": (sum(tps) / len(tps)) if tps else None,
            "mspt": mspt,
            "mspt_cur": mspt[-1] if mspt else None,
            "mspt_p95": percentile(mspt, 95),
            "mem": mem,
            "mem_cur": mem[-1] if mem else None,
            "cpu": cpu,
            "cpu_cur": cpu[-1] if cpu else None,
            "threads": self.threads,
            "ms_per_chunk": mspc,
            "ms_per_chunk_cur": mspc[-1] if mspc else None,
            "revision": self.revision,
            "players": sorted(self.players),
            "players_online": online,
            "players_max": self.players_max,
            "loaded_chunks": self.loaded_chunks,
            "mods": dict(self.mods),
            "jars_seen": self.jars_seen,
            "mod_details": list(self.mod_details),
            "dual_loader": dict(self.dual_loader),
            "errors": self.errors,
            "warnings": self.warnings,
            "panics": self.panics,
            "over_budget": self.over_budget,
            "uptime_ticks": self.uptime_ticks,
            "clients": self.clients,
            "lines_seen": self.lines_seen,
            "log": list(self.log),
        }


# ------------------------------------------------------------------- frame

def _dot(ascii_mode):
    return "-" if ascii_mode else "·"


def _tps_band(cur):
    if cur is None:
        return "grey"
    if cur >= 19.0:
        return "green"
    if cur >= 15.0:
        return "yellow"
    return "red"


def _mspt_band(cur):
    if cur is None:
        return "grey"
    if cur < 20.0:
        return "green"
    if cur < 40.0:
        return "yellow"
    return "red"


def _cpu_band(cur):
    if cur is None:
        return "grey"
    if cur < 50.0:
        return "green"
    if cur < 100.0:
        return "yellow"
    return "red"


def _mem_band(cur):
    return "cyan" if cur is not None else "grey"


def _status_color(status, snap):
    if snap.get("panics"):
        return "red"
    label = (status or "").lower()
    if "running" in label or "smooth" in label:
        return "green"
    if "boot" in label:
        return "yellow"
    if "busy" in label or "overload" in label:
        return "yellow" if "busy" in label else "red"
    return "grey"


def _metric_line(name, stat, values, cols, band, ascii_mode, color,
                 low=None, high=None):
    prefix = " " + name + " "
    spark_width = max(0, cols - len(prefix) - vlen(stat) - 2)
    spark = sparkline(values, spark_width, low=low, high=high,
                      ascii_mode=ascii_mode)
    return (prefix + paint(stat, band, color) + "  "
            + paint(spark, band, color))


def _info_line(snap, ascii_mode, color):
    dot = " " + _dot(ascii_mode) + " "
    version = snap.get("version") or "?"
    port = snap.get("port")
    if port:
        address = f"{snap.get('bind') or 'server'}:{port}"
    else:
        address = "not listening yet"
    mods = snap.get("mods", {})
    mods_total = (mods.get("native", 0) + mods.get("shim", 0)
                  + mods.get("fallback", 0) + mods.get("auto_skipped", 0))
    status = snap.get("status") or "Booting"
    return (
        paint(f" mc-spine v{version}", "cyan", color)
        + dot + f"MC {MC_VERSIONS}"
        + dot + address
        + dot + f"mods {mods_total}"
        + dot + paint(status, _status_color(status, snap), color)
    )


def _card_block(entries, cols, ascii_mode):
    """Four bordered status cards in three lines; None when too narrow."""
    count = len(entries)
    card_w = (cols - (count - 1)) // count
    if card_w < 12:
        return None
    inner = card_w - 2
    tops, mids, bots = [], [], []
    for title, value in entries:
        if ascii_mode:
            tops.append("+" + fit_plain("- " + title + " ", inner, "-") + "+")
            mids.append("|" + center_vis(value, inner) + "|")
            bots.append("+" + "-" * inner + "+")
        else:
            tops.append("┌" + fit_plain("─ " + title + " ", inner, "─") + "┐")
            mids.append("│" + center_vis(value, inner) + "│")
            bots.append("└" + "─" * inner + "┘")
    return [" ".join(tops), " ".join(mids), " ".join(bots)]


def _cards(snap, cols, ascii_mode, color):
    players_max = snap.get("players_max")
    players_text = f"{snap.get('players_online', 0)}/{players_max if players_max is not None else '?'}"
    chunks = snap.get("loaded_chunks")
    chunks_text = f"{chunks:,}" if isinstance(chunks, int) else "n/a"
    mspc = snap.get("ms_per_chunk_cur")
    if mspc is not None:
        chunks_text += f" {_dot(ascii_mode)} {mspc:.1f}ms/ch"
    mods = snap.get("mods", {})
    mods_text = (f"N:{mods.get('native', 0)} "
                 f"S:{mods.get('shim', 0)} F:{mods.get('fallback', 0)}")
    panics = snap.get("panics", 0)
    guard_text = "OK" if panics == 0 else f"TRIPPED x{panics}"
    entries = [
        ("Players", paint(players_text, "white", color)),
        ("Chunks", paint(chunks_text, "white", color)),
        ("Adapted Mods", paint(mods_text, "cyan", color)),
        ("Guard", paint(guard_text, "green" if panics == 0 else "red", color)),
    ]
    block = _card_block(entries, cols, ascii_mode)
    if block is not None:
        return block
    dot = " " + _dot(ascii_mode) + " "
    inline = (" Players " + players_text + dot + "Chunks " + chunks_text + dot
              + "Mods " + mods_text.replace(":", "") + dot + "Guard " + guard_text)
    return [paint(inline, "grey", color)]


def _log_header(title, cols, ascii_mode, color):
    bar, dash = ("-", "--") if ascii_mode else ("─", "──")
    return paint(fit_plain(dash + " " + title + " ", cols, bar), "grey", color)


def _log_color(line, color):
    if not color:
        return line
    if RE_FATAL.search(line) or RE_ERROR.search(line):
        return paint(line, "red", color)
    if RE_WARN.search(line):
        return paint(line, "yellow", color)
    return line


def _bottom_line(snap, cols, ascii_mode, color, ui):
    dot = " " + _dot(ascii_mode) + " "
    if ui.get("stopping"):
        text = " stopping mc-spine gracefully" + ("..." if ascii_mode else "…")
        return paint(text, "yellow", color)
    if ui.get("exited") is not None:
        text = f" mc-spine exited{dot}code {ui['exited']}"
        return paint(text, "grey", color)
    if ui.get("cmd_mode"):
        cursor = "_" if ascii_mode else "█"
        return " /" + ui.get("cmd_buf", "") + paint(cursor, "cyan", color)
    view = "mods" if ui.get("mods") else "log"
    text = (f" {snap.get('lines_seen', 0)} lines" + dot
            + f"{snap.get('warnings', 0)} warn" + dot
            + f"{snap.get('errors', 0)} err" + dot
            + f"view:{view}" + dot + "q=stop m=mods /=cmd")
    return paint(text, "grey", color)


def render_frame(snap, cols, rows, ascii_mode=False, color=False, ui=None):
    """Render one dashboard frame as a list of `rows` strings.

    Every returned line is exactly `cols` visible columns (ANSI sequences do
    not count). The frame is a pure function of `snap`, `cols`, `rows`, and
    the flags - no clock, no randomness - so --demo-frame and
    --render-snapshot output is deterministic.
    """
    cols = max(8, int(cols))
    rows = max(4, int(rows))
    ui = ui or {}
    dot = " " + _dot(ascii_mode) + " "

    head = []
    if rows >= 24 and cols >= 62:
        logo = LOGO_ASCII if ascii_mode else LOGO
        for index, row in enumerate(logo):
            head.append(paint(row, "purple" if index % 2 == 0 else "pink", color))
        head.append(paint(TAGLINE_ASCII if ascii_mode else TAGLINE, "cyan", color))
        head.append("")
    elif rows >= 15 and cols >= 26:
        mark = WORDMARK_ASCII if ascii_mode else WORDMARK
        head.append(paint(mark, "purple", color))

    head.append(_info_line(snap, ascii_mode, color))
    if rows >= 16:
        keys = (" [q] stop" + dot + "[m] mods" + dot + "[/] command" + dot
                + "[Ctrl+C] shutdown")
        head.append(paint(keys, "grey", color))
    if rows >= 20:
        head.append("")

    tps_stat = (f"{fmt(snap.get('tps_cur'))}  min {fmt(snap.get('tps_min'))}  "
                f"avg {fmt(snap.get('tps_avg'))}  [0..20]")
    mspt_stat = (f"{fmt(snap.get('mspt_cur'), 1)}ms  "
                 f"p95 {fmt(snap.get('mspt_p95'), 1)}ms  [0..50]")
    mem_stat = (f"{fmt(snap.get('mem_cur'), 0)} MiB"
                if snap.get("mem_cur") is not None else "n/a MiB")
    threads = snap.get("threads")
    if threads is not None:
        mem_stat += f"  thr {threads}"
    metrics = [
        _metric_line("TPS", tps_stat, snap.get("tps", []), cols,
                     _tps_band(snap.get("tps_cur")), ascii_mode, color,
                     low=0.0, high=20.0),
        _metric_line("MSPT", mspt_stat, snap.get("mspt", []), cols,
                     _mspt_band(snap.get("mspt_cur")), ascii_mode, color,
                     low=0.0, high=50.0),
        _metric_line("MEM", mem_stat, snap.get("mem", []), cols,
                     _mem_band(snap.get("mem_cur")), ascii_mode, color),
    ]
    cpu = snap.get("cpu", [])
    if rows >= 26 and cpu:
        cpu_stat = (f"{fmt(snap.get('cpu_cur'), 1)}%  "
                    f"thr {threads if threads is not None else '?'}")
        metrics.append(
            _metric_line("CPU", cpu_stat, cpu, cols,
                         _cpu_band(snap.get("cpu_cur")), ascii_mode, color,
                         low=0.0))

    cards = []
    if rows >= 17:
        cards = _cards(snap, cols, ascii_mode, color) or []
    blocks = head + metrics + cards

    avail = rows - len(blocks) - 1
    log_section = []
    if avail >= 1:
        if ui.get("mods"):
            log_section.append(_log_header("adapted mods (m: back to log)", cols,
                                           ascii_mode, color))
            details = snap.get("mod_details", [])
            pool = [f"{d['file']} ({d['mod_id']}) {d['tier']}: {d['detail']}"
                    for d in details] or ["no adapted mods recorded"]
        else:
            log_section.append(_log_header("server log", cols, ascii_mode, color))
            pool = snap.get("log", [])
        for line in pool[-(avail - 1):]:
            clean = strip_ansi(line).rstrip()
            if ascii_mode:
                clean = asciify(clean)
            log_section.append(_log_color(clean, color))

    lines = blocks + log_section
    if len(lines) > rows - 1:
        lines = lines[len(lines) - (rows - 1):]
    lines += [""] * (rows - 1 - len(lines))
    lines.append(_bottom_line(snap, cols, ascii_mode, color, ui))

    out = []
    for line in lines:
        if ascii_mode:
            line = asciify(strip_ansi(line))
        out.append(clip(line, cols))
    return out


# -------------------------------------------------------------- demo content

DEMO_LOG = """\
[config] loaded server.toml (profile default)
[anvil] loaded world: WorldSummary(chunks=1284, players=3)
[live-mods] auto-adapt: scanned 5 jar(s) for target 1.21.1
[live-mods] 5 jar(s): 1 auto-skipped, 2 native-bundle, 1 tier2-shim, 1 tier3
[live-mods] dual-loader: 2 unified, 1 client-only, 0 superseded, 0 rejected, 0 duplicate ids, 0 missing deps, 3 verified damage hooks
[live-mods] weirdmod.jar (weirdmod) tier3: 412 classes, 9 site(s) - mixin target missing
[live-mods] liftcraft.jar (liftcraft) tier2 shim: 208 classes, 4 site(s) - damage hooks lifted
[tick] observed_tps=19.97 avg_work_ms=8.21 max_work_ms=21.44 over_budget=0/100
mc-spine v0.6 listening 127.0.0.1:25599 (offline identities; no Mojang authentication)
[lan] advertising "Megu demo server" on port 25599
[spine] Alice joined from 127.0.0.1:51112 (uuid 0123456789abcdef0123456789abcdef)
[spine] Bob joined from 127.0.0.1:51130 (uuid fedcba9876543210fedcba9876543210)
[sz] t=400 VmRSS: 523264 kB | mobs=42 drops=7 xps=3 arrows=0 bob=1 pearl=0 primed=0 chests=18 furn=6 crops=11 world=0 inv=2 clients=2
[tick] observed_tps=19.41 avg_work_ms=44.10 max_work_ms=58.02 over_budget=3/100
[spine] Alice left (from 127.0.0.1:51112)
Status: Busy (19.4 TPS, 44.1 ms/tick)
Players: 2/20 (Bob, Carol)
Loaded chunks: 1284
Memory: 511 MB
Mods: 2 native, 1 compatibility mode
[tick] observed_tps=19.95 avg_work_ms=9.02 max_work_ms=15.77 over_budget=0/100
[security] velocity forwarding refused for Mallory from 10.0.0.9: missing forwarding secret
[sz] t=500 VmRSS: 524288 kB | mobs=44 drops=8 xps=4 arrows=0 bob=0 pearl=0 primed=0 chests=18 furn=6 crops=12 world=0 inv=2 clients=2
[chunkprof] window=60s chunks=1284 (4.2 ms/chunk) map=15.8ms entities=91
"""


def demo_parser():
    """A deterministic snapshot: canned realistic log plus a synthetic wave."""
    parser = MetricsParser()
    for line in DEMO_LOG.splitlines():
        parser.feed(line)
    parser.threads = 14
    for i in range(72):
        crest = abs((i % 24) - 12) / 12.0
        dip = max(0.0, 1.0 - abs(i - 52) / 9.0)
        parser.tps.append(round(19.2 + 0.8 * crest - 4.5 * dip, 2))
        parser.mspt.append(round(11.0 + 6.0 * (1.0 - crest) + 41.0 * dip, 2))
        parser.mem_mib.append(round(468.0 + i * 0.7 + (i % 6), 1))
        parser.cpu_pct.append(round(14.0 + 26.0 * crest + 48.0 * dip, 1))
        parser.ms_per_chunk.append(
            round(2.8 + 1.4 * (1.0 - crest) + 5.6 * dip, 2))
    return parser


def snapshot_from_file(path):
    parser = MetricsParser()
    try:
        if path == "-":
            lines = sys.stdin.read().splitlines()
        else:
            with open(path, "r", encoding="utf-8", errors="replace") as handle:
                lines = handle.read().splitlines()
    except OSError as error:
        print(f"megu-tui: cannot read {path!r}: {error}", file=sys.stderr)
        return None
    for line in lines:
        parser.feed(line)
    return parser


# -------------------------------------------------------- process management

def exit_code(returncode):
    """Shell-style exit code: 128+signal for signal deaths."""
    if returncode is None:
        return 1
    return returncode if returncode >= 0 else 128 + (-returncode)


def wait_for(proc, seconds):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        code = proc.poll()
        if code is not None:
            return code
        time.sleep(0.05)
    return proc.poll()


def escalate(proc):
    """SIGTERM, brief grace, SIGKILL. Returns the final returncode."""
    try:
        proc.terminate()
    except OSError:
        pass
    code = wait_for(proc, ESCALATE_GRACE)
    if code is None:
        try:
            proc.kill()
        except OSError:
            pass
        code = proc.wait()
    return code


def send_line(proc, text):
    if proc.stdin is None:
        return
    try:
        proc.stdin.write(text.encode("utf-8") + b"\n")
        proc.stdin.flush()
    except (OSError, ValueError):
        pass


def send_stop(proc, state):
    send_line(proc, STOP_COMMAND)
    state["stop_sent"] = True


def finish_stop(proc, state, stop_timeout):
    """Graceful 'stop' line, then escalate. No-op once the process is dead."""
    if proc.poll() is not None:
        return
    if not state.get("stop_sent"):
        send_stop(proc, state)
    if wait_for(proc, stop_timeout) is None:
        escalate(proc)


def parse_proc_status(text):
    """(rss_mib, threads) from /proc/<pid>/status text; fields may be None."""
    rss_mib = None
    threads = None
    for line in text.splitlines():
        parts = line.split()
        if len(parts) < 2:
            continue
        if parts[0] == "VmRSS:":
            try:
                rss_mib = int(parts[1]) / 1024.0
            except ValueError:
                pass
        elif parts[0] == "Threads:":
            try:
                threads = int(parts[1])
            except ValueError:
                pass
    return rss_mib, threads


def parse_proc_stat_jiffies(text):
    """utime+stime jiffies from /proc/<pid>/stat text; None when unparsable.

    The comm field may contain spaces and parentheses, so fields are counted
    back from the last ')' instead of split naively.
    """
    rparen = text.rfind(")")
    if rparen < 0:
        return None
    fields = text[rparen + 1:].split()
    # fields[0] is state (field 3); utime/stime are fields 14 and 15.
    if len(fields) < 13:
        return None
    try:
        return int(fields[11]) + int(fields[12])
    except ValueError:
        return None


def cpu_pct_delta(prev_sample, cur_sample, hz):
    """CPU % between two (jiffies, wall_s) samples; None when not computable."""
    if prev_sample is None or cur_sample is None or hz <= 0:
        return None
    delta_jiffies = cur_sample[0] - prev_sample[0]
    delta_wall = cur_sample[1] - prev_sample[1]
    if delta_jiffies < 0 or delta_wall <= 0:
        return None
    return 100.0 * delta_jiffies / (hz * delta_wall)


def _clock_ticks_hz():
    try:
        return os.sysconf("SC_CLK_TCK")
    except (AttributeError, OSError, ValueError):
        return 100


def sample_proc_metrics(pid, prev_cpu_sample=None, now=None):
    """One-shot /proc telemetry: VmRSS MiB, thread count, delta CPU %.

    Reads /proc/<pid>/status and /proc/<pid>/stat once each. `prev_cpu_sample`
    is the `cpu_sample` value returned by the previous call for the same pid;
    `cpu_pct` is None on the first sample, off-Linux, or when the wall-clock
    delta is not positive. Every metric degrades to None on read failure.
    """
    now = time.monotonic() if now is None else now
    rss_mib = threads = None
    try:
        with open(f"/proc/{pid}/status", "r", encoding="ascii",
                  errors="replace") as handle:
            rss_mib, threads = parse_proc_status(handle.read())
    except OSError:
        pass
    cpu_sample = prev_cpu_sample
    cpu_pct = None
    try:
        with open(f"/proc/{pid}/stat", "r", encoding="ascii",
                  errors="replace") as handle:
            jiffies = parse_proc_stat_jiffies(handle.read())
    except OSError:
        jiffies = None
    if jiffies is not None:
        cpu_sample = (jiffies, now)
        cpu_pct = cpu_pct_delta(prev_cpu_sample, cpu_sample,
                                _clock_ticks_hz())
    return {"rss_mib": rss_mib, "threads": threads,
            "cpu_pct": cpu_pct, "cpu_sample": cpu_sample}


def sample_rss_mib(pid):
    """Resident memory of `pid` in MiB from /proc; None off-Linux."""
    return sample_proc_metrics(pid)["rss_mib"]


# ------------------------------------------------------------ plain passthru

def run_plain(command, args):
    """Inherit stdin/stdout/stderr untouched; exit with the child's code."""
    try:
        proc = subprocess.Popen(command, shell=False)
    except OSError as error:
        print(f"megu-tui: cannot start {command[0]!r}: {error}", file=sys.stderr)
        return 2
    previous = None
    if signal is not None:
        def _forward(_signum, _frame):
            try:
                proc.terminate()
            except OSError:
                pass
        try:
            previous = signal.signal(signal.SIGTERM, _forward)
        except (ValueError, OSError):
            previous = None
    try:
        code = proc.wait()
    except KeyboardInterrupt:
        # SIGINT reached the whole process group: the server is already doing
        # its own graceful shutdown. Wait for it, then escort it if needed.
        code = wait_for(proc, args.stop_timeout)
        if code is None:
            code = escalate(proc)
    finally:
        if previous is not None:
            try:
                signal.signal(signal.SIGTERM, previous)
            except (ValueError, OSError):
                pass
    return exit_code(code)


# ------------------------------------------------------------- interactive

class Terminal:
    """Alternate-screen + cbreak terminal with unconditional restore."""

    def __init__(self, stdin, stdout):
        self.in_fd = stdin.fileno()
        self.stdout = stdout
        self.saved = None
        self.active = False

    def enter(self):
        if termios is not None and tty is not None:
            try:
                self.saved = termios.tcgetattr(self.in_fd)
                tty.setcbreak(self.in_fd)
            except (OSError, termios.error):
                self.saved = None
        self.write("\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H")
        self.active = True

    def restore(self):
        if not self.active:
            return
        self.active = False
        if self.saved is not None and termios is not None:
            try:
                termios.tcsetattr(self.in_fd, termios.TCSANOW, self.saved)
            except (OSError, termios.error):
                pass
        self.write("\x1b[?25h\x1b[?1049l")

    def write(self, text):
        try:
            self.stdout.buffer.write(text.encode("utf-8", "replace"))
            self.stdout.buffer.flush()
        except (AttributeError, OSError, ValueError):
            try:
                self.stdout.write(text)
                self.stdout.flush()
            except (OSError, ValueError):
                pass


def read_stdin(fd, timeout):
    if select is None:
        return b""
    try:
        ready, _, _ = select.select([fd], [], [], timeout)
    except (OSError, ValueError):
        return b""
    if not ready:
        return b""
    try:
        return os.read(fd, 64)
    except OSError:
        return b""


def handle_keys(data, state, proc):
    for byte in data:
        if state["cmd_mode"]:
            if byte in (0x0D, 0x0A):
                send_line(proc, state["cmd_buf"])
                state["cmd_buf"] = ""
                state["cmd_mode"] = False
            elif byte in (0x7F, 0x08):
                state["cmd_buf"] = state["cmd_buf"][:-1]
            elif byte == 0x1B:
                state["cmd_mode"] = False
                state["cmd_buf"] = ""
            elif 0x20 <= byte < 0x7F:
                state["cmd_buf"] += chr(byte)
        else:
            if byte == 0x03 or chr(byte) == "q":
                state["quit"] = True
            elif chr(byte) == "m":
                state["mods"] = not state["mods"]
            elif chr(byte) == "/":
                state["cmd_mode"] = True
                state["cmd_buf"] = ""


def render_key(parser, state, cols, rows):
    """Compact dirty-frame key: a frame is redrawn only while this changes."""
    return (parser.revision, state.get("mods"), state.get("cmd_mode"),
            state.get("cmd_buf"), state.get("stopping"), cols, rows)


def tui_loop(proc, parser, lock, threads, state, term, args, color):
    """Render frames until the server exits or a stop is requested."""
    stop_deadline = None
    escalate_deadline = None
    next_rss = 0.0
    frame_at = 0.0
    prev_key = None
    while True:
        now = time.monotonic()
        data = read_stdin(term.in_fd, 0.2)
        if data:
            handle_keys(data, state, proc)
        if (state["quit"] or state["sigterm"]) and not state["stopping"]:
            state["stopping"] = True
            send_stop(proc, state)
            stop_deadline = now + args.stop_timeout
        if proc.poll() is not None:
            break
        if state["stopping"]:
            if stop_deadline is not None and now >= stop_deadline:
                try:
                    proc.terminate()
                except OSError:
                    pass
                escalate_deadline = now + ESCALATE_GRACE
                stop_deadline = None
            elif escalate_deadline is not None and now >= escalate_deadline:
                try:
                    proc.kill()
                except OSError:
                    pass
                escalate_deadline = None
        elif now >= next_rss:
            metrics = sample_proc_metrics(proc.pid, state.get("cpu_sample"))
            state["cpu_sample"] = metrics["cpu_sample"]
            if (metrics["rss_mib"] is not None
                    or metrics["threads"] is not None
                    or metrics["cpu_pct"] is not None):
                with lock:
                    parser.note_proc_metrics(metrics["rss_mib"],
                                             metrics["threads"],
                                             metrics["cpu_pct"])
            next_rss = now + 1.0
        if now >= frame_at or data:
            frame_at = now + 0.25
            size = shutil.get_terminal_size(fallback=(100, 30))
            with lock:
                key = render_key(parser, state, size.columns, size.lines)
                if key != prev_key:
                    snap = parser.snapshot()
                    frame = render_frame(snap, size.columns, size.lines,
                                         ascii_mode=args.ascii_mode,
                                         color=color, ui=state)
                    term.write("\x1b[H" + "\r\n".join(frame) + "\x1b[J")
                    prev_key = key
    for thread in threads:
        thread.join(timeout=0.4)


def report_exit(code, parser):
    """After the alt screen is left, leave a short tail for scrollback."""
    out = [f"megu-tui: mc-spine exited, code {exit_code(code)}"]
    tail = [line for line in parser.log if line.strip()][-8:]
    if tail:
        out.append("megu-tui: last server log lines:")
        out.extend("  " + line for line in tail)
    write_stdout("\n".join(out) + "\n")


def write_stdout(text):
    try:
        sys.stdout.buffer.write(text.encode("utf-8", "replace"))
        sys.stdout.buffer.flush()
    except (AttributeError, OSError, ValueError):
        try:
            sys.stdout.write(text)
            sys.stdout.flush()
        except (OSError, ValueError):
            pass


def run_tui(command, args):
    try:
        proc = subprocess.Popen(command, shell=False, stdin=subprocess.PIPE,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                bufsize=0)
    except OSError as error:
        print(f"megu-tui: cannot start {command[0]!r}: {error}", file=sys.stderr)
        return 2

    parser = MetricsParser()
    lock = threading.Lock()
    state = {"quit": False, "stopping": False, "stop_sent": False,
             "mods": False, "cmd_mode": False, "cmd_buf": "",
             "sigterm": False, "cpu_sample": None}

    def reader(pipe):
        try:
            for raw in iter(pipe.readline, b""):
                line = raw.decode("utf-8", "replace")
                with lock:
                    parser.feed(line)
        except (OSError, ValueError):
            pass
        finally:
            try:
                pipe.close()
            except OSError:
                pass

    threads = [
        threading.Thread(target=reader, args=(proc.stdout,), daemon=True),
        threading.Thread(target=reader, args=(proc.stderr,), daemon=True),
    ]
    for thread in threads:
        thread.start()

    if signal is not None:
        def _flag(_signum, _frame):
            state["sigterm"] = True
        try:
            signal.signal(signal.SIGTERM, _flag)
        except (ValueError, OSError):
            pass

    term = Terminal(sys.stdin, sys.stdout)
    color = not args.ascii_mode and "NO_COLOR" not in os.environ
    try:
        term.enter()
        tui_loop(proc, parser, lock, threads, state, term, args, color)
    except KeyboardInterrupt:
        state["quit"] = True
    finally:
        term.restore()
        finish_stop(proc, state, args.stop_timeout)
        code = proc.wait()
    report_exit(code, parser)
    return exit_code(code)


# ------------------------------------------------------------------ fallback

def tui_capable(env, stdin_tty, stdout_tty):
    """Whether the interactive dashboard may run at all on this terminal."""
    if not (stdin_tty and stdout_tty):
        return False
    if "NO_COLOR" in env:
        return False
    if env.get("TERM", "") in ("", "dumb", "unknown"):
        return False
    if os.name == "nt" or termios is None or tty is None or select is None:
        return False
    return True


def color_ok(env):
    """Whether ANSI color may appear in output (frame modes)."""
    if "NO_COLOR" in env:
        return False
    return env.get("TERM") not in (None, "", "dumb", "unknown")


def want_tui(args, env, stdin, stdout):
    if args.plain or args.no_tui:
        return False
    return tui_capable(env, stdin.isatty(), stdout.isatty())


def frame_color(args, env):
    if args.plain or args.ascii_mode:
        return False
    if args.color:
        return True
    return sys.stdout.isatty() and color_ok(env)


# ----------------------------------------------------------------------- cli

def build_parser():
    parser = argparse.ArgumentParser(
        prog="megu_tui",
        description="mc-spine server terminal dashboard / safe passthrough wrapper",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "examples:\n"
            "  %(prog)s --server-bin ./target/release/mc-spine -- --port 25599\n"
            "  %(prog)s --demo-frame --cols 100 --rows 28\n"
            "  %(prog)s --render-snapshot server.log\n"
        ),
    )
    parser.add_argument("--server-bin", metavar="PATH",
                        help="mc-spine binary to launch")
    parser.add_argument("--cols", type=int, default=0,
                        help="frame width (default: terminal size or 80)")
    parser.add_argument("--rows", type=int, default=0,
                        help="frame height (default: terminal size or 24)")
    parser.add_argument("--plain", action="store_true",
                        help="force plain-text passthrough, never a TUI")
    parser.add_argument("--no-tui", action="store_true",
                        help="alias for --plain")
    parser.add_argument("--ascii", dest="ascii_mode", action="store_true",
                        help="pure-ASCII glyphs and no color")
    parser.add_argument("--color", action="store_true",
                        help="force ANSI color in frame modes even when piped")
    parser.add_argument("--demo-frame", action="store_true",
                        help="render one deterministic demo frame and exit")
    parser.add_argument("--render-snapshot", metavar="FILE",
                        help="render one frame from a captured server log "
                             "('-' reads stdin)")
    parser.add_argument("--stop-timeout", type=float, default=15.0,
                        help="seconds to wait for graceful 'stop' before SIGTERM")
    return parser


def split_argv(argv):
    """Split our flags from the server's args at the first '--'."""
    if "--" in argv:
        index = argv.index("--")
        return argv[:index], argv[index + 1:]
    return argv, []


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    own_args, server_args = split_argv(argv)
    args = build_parser().parse_args(own_args)

    size = shutil.get_terminal_size(fallback=(DEFAULT_COLS, DEFAULT_ROWS))
    cols = args.cols if args.cols > 0 else size.columns
    rows = args.rows if args.rows > 0 else size.lines

    if args.demo_frame or args.render_snapshot is not None:
        parser = demo_parser() if args.demo_frame else snapshot_from_file(
            args.render_snapshot)
        if parser is None:
            return 2
        frame = render_frame(
            parser.snapshot(), cols, rows,
            ascii_mode=args.ascii_mode or args.plain,
            color=frame_color(args, os.environ))
        write_stdout("\n".join(frame) + "\n")
        return 0

    if not args.server_bin:
        build_parser().print_usage(sys.stderr)
        print("megu-tui: --server-bin is required (or use --demo-frame / "
              "--render-snapshot)", file=sys.stderr)
        return 2

    command = [args.server_bin] + server_args
    if want_tui(args, os.environ, sys.stdin, sys.stdout):
        return run_tui(command, args)
    return run_plain(command, args)


if __name__ == "__main__":
    sys.exit(main())
