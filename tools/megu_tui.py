#!/usr/bin/env python3
"""Smart terminal dashboard and live telemetry wrapper for the megu server.

Three modes:

    python3 tools/megu_tui.py --server-bin ./target/release/megu -- <server-args...>
        Spawn the server under a compact, high-signal dashboard: a one-line
        status header, health/perf sparklines, actionable status cards, a
        noise-filtered event log, a '/' command line with history, and quick
        players/mods views.

    python3 tools/megu_tui.py --demo-frame [--ascii] [--cols N --rows M]
        Render one deterministic demo frame and exit 0. Used for tests and
        screenshots: the output depends only on the arguments, never on the
        clock or on randomness.

    python3 tools/megu_tui.py --render-snapshot <logfile|->
        Parse a captured server log and render one frame from it ('-' reads
        stdin). Also deterministic.

Log filters (also available interactively with 'f'):
    events   human events + warnings/errors/alerts; tick/size/chunkprof
             telemetry is folded into the metrics strip and hidden (default)
    alerts   warnings, errors, security, quarantine and chunk-hotspot alerts
    all      the raw server log, telemetry lines included

Startup banner:
    The classic six-line MEGU logo is written once at the top of the scrolling
    log (never pinned to the three-line HUD) so it scrolls away as events
    arrive, like Mohist / Paper / Purpur. `--no-banner` (or MEGU_NO_BANNER=1 /
    NO_BANNER=1) skips it; `--log-filter alerts` never shows it.

Fallback contract (zero side effects in pipes, CI, and headless logs):
  * stdout not a TTY, TERM unset/dumb/unknown, NO_COLOR set, or an explicit
    --plain / --no-tui / --cli / --nogui flag -> plain stdio passthrough.
    The child's stdin, stdout, and stderr are inherited untouched, this
    wrapper emits no ANSI, and the wrapper exits with the server's exact exit
    code (128+signal for signal deaths).
  * --tui explicitly requests the dashboard, but the same safety checks still
    apply; a pipe or dumb terminal always falls back to passthrough.
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

# The Minecraft data version the server targets. This is deliberately the
# only version string in the UI: server build numbers belong in the log.
MC_VERSIONS = "1.21.1"
DEFAULT_COLS = 80
DEFAULT_ROWS = 24
STOP_COMMAND = "stop"
ESCALATE_GRACE = 4.0
LOG_FILTERS = ("events", "alerts", "all")
LOG_FILTER_TITLES = {"events": "Events", "alerts": "Alerts", "all": "All log"}

# The classic six-line MEGU startup banner. It is written once into the
# scrolling log at boot (never pinned to the HUD) so it scrolls away like the
# startup art of Mohist / Paper / Purpur.
LOGO = (
    "███╗   ███╗███████╗ ██████╗ ██╗   ██╗",
    "████╗ ████║██╔════╝██╔════╝ ██║   ██║",
    "██╔████╔██║█████╗  ██║  ███╗██║   ██║",
    "██║╚██╔╝██║██╔══╝  ██║   ██║██║   ██║",
    "██║ ╚═╝ ██║███████╗╚██████╔╝╚██████╔╝",
    "╚═╝     ╚═╝╚══════╝ ╚═════╝  ╚═════╝ ",
)
LOGO_ASCII = (
    "##   ## ######  ####  ##   ##",
    "### ### ##     ##  ## ##   ##",
    "## # ## ####   ##     ##   ##",
    "##   ## ##     ## ### ##   ##",
    "##   ## ##     ##  ##  ## ## ",
    "##   ## ######  ####    ###  ",
)
BANNER_LINE_SET = frozenset(line.rstrip() for line in LOGO + LOGO_ASCII)


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
    r"megu v([0-9]+(?:\.[0-9]+)*)\s+listening\s+(\S+):(\d+)")
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
RE_WORLD_CHUNKS = re.compile(r"WorldSummary\(chunks=(\d+)")
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

# A hotspot is a chunk coordinate pair attached to a throttling, slow-chunk,
# or quarantine diagnostic. Both word orders are common in operator logs.
RE_HOTSPOT = re.compile(
    r"\b(?:chunk\s+)?(?:hotspot|slow\s+chunk)\b[^()\n]*"
    r"\(\s*(-?\d+)\s*,\s*(-?\d+)\s*\)", re.IGNORECASE)
RE_HOTSPOT_CHUNK_FIRST = re.compile(
    r"\bchunk\b[^()\n]*\(\s*(-?\d+)\s*,\s*(-?\d+)\s*\)"
    r"[^\n]*(?:hotspot|slow|throttl|quarantin|frozen|froze)", re.IGNORECASE)
RE_HOTSPOT_KV = re.compile(
    r"\bcx\s*=\s*(-?\d+)\s*[, ]\s*cz\s*=\s*(-?\d+)", re.IGNORECASE)
RE_HOTSPOT_COMMA = re.compile(
    r"\b(?:chunk|hotspot)\s+(-?\d+)\s*,\s*(-?\d+)\b", re.IGNORECASE)
RE_SLEEP = re.compile(
    r"\bSLEEPING\b|\bserver\s+(?:is\s+)?(?:sleeping|asleep|idle)\b|"
    r"\bsleeping\s+until\b|\bidle\s+sleep\b", re.IGNORECASE)
RE_WAKE = re.compile(
    r"\bwoke\b|\bwake[- ]on[- ]join\b|\bresumed\s+from\s+sleep\b|"
    r"\bwaking\b", re.IGNORECASE)


def ms_per_chunk_from(line):
    """ms-per-chunk figure from a profiling line; None when none is present."""
    for pattern in (RE_MS_PER_CHUNK_KV, RE_MS_PER_CHUNK_PAREN,
                    RE_CHUNKPROF_MS):
        match = pattern.search(line)
        if match:
            return float(match.group(1))
    return None


def hotspot_coords(line):
    """(cx, cz) from a hotspot/slow-chunk/quarantine line, else None."""
    for pattern in (RE_HOTSPOT, RE_HOTSPOT_CHUNK_FIRST, RE_HOTSPOT_KV,
                    RE_HOTSPOT_COMMA):
        match = pattern.search(line)
        if match:
            return int(match.group(1)), int(match.group(2))
    return None


def startup_banner(ascii_mode=False):
    """The six startup banner rows for the requested glyph set."""
    return LOGO_ASCII if ascii_mode else LOGO


def is_banner_line(line):
    """Whether a captured log line is one of the startup banner rows."""
    return line.rstrip() in BANNER_LINE_SET


def banner_disabled(args, env):
    """True when --no-banner or MEGU_NO_BANNER/NO_BANNER asks to skip it."""
    if getattr(args, "no_banner", False):
        return True
    for name in ("MEGU_NO_BANNER", "NO_BANNER"):
        value = env.get(name)
        if value is None:
            continue
        if value.strip().lower() not in ("", "0", "false", "no", "off"):
            return True
    return False


def _looks_telemetry(line):
    stripped = line.lstrip()
    lower = stripped.lower()
    return (stripped.startswith("[tick]")
            or stripped.startswith("[sz]")
            or stripped.startswith("[chunkprof]")
            or "chunkprof" in lower
            or "observed_tps=" in lower
            or "vmrss:" in lower)


def classify_line(line):
    """Best-effort category for a raw line: banner, event, alert, telemetry."""
    lower = line.lower()
    if is_banner_line(line):
        return "banner"
    if _looks_telemetry(line):
        return "telemetry"
    if (RE_FATAL.search(line) or RE_ERROR.search(line) or RE_WARN.search(line)
            or "quarantin" in lower or "hotspot" in lower
            or "slow chunk" in lower):
        return "alert"
    return "event"


def classify_kind(line, kind):
    """Category for a parsed log line, using the decoder's match kind."""
    if kind == "banner" or is_banner_line(line):
        return "banner"
    if kind in ("tick", "size", "ms_per_chunk") or _looks_telemetry(line):
        return "telemetry"
    if kind in ("hotspot", "quarantine"):
        return "alert"
    if kind in ("ready", "join", "leave", "lan", "mods", "mod", "status",
                "players", "chunks", "memory", "sleep", "wake"):
        return "event"
    return classify_line(line)


class MetricsParser:
    """Fold raw megu stdout/stderr lines into a rolling metrics snapshot.

    Every line lands in the `log` ring buffer together with a category
    (`event`, `alert`, or `telemetry`). Lines whose shape is known are also
    decoded into structured state. feed() returns the matched kind so tests
    and future consumers can route on it.
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
        self.sleeping = False
        self.hotspots = collections.deque(maxlen=20)
        self.quarantine_alerts = 0
        self.log = collections.deque(maxlen=log_lines)
        self.log_categories = collections.deque(maxlen=log_lines)
        self.lines_seen = 0
        self.banner_added = False

    def inject_banner(self, ascii_mode=False):
        """Write the startup banner once at the top of the scrolling log.

        Banner lines are not fed through the decoder: they are decorative,
        never counted as warnings/errors, and carry the dedicated `banner`
        category so `--log-filter alerts` stays alerts-only.
        """
        if self.banner_added:
            return False
        for line in startup_banner(ascii_mode):
            self.log.append(line)
            self.log_categories.append("banner")
        self.banner_added = True
        self.revision += 1
        return True

    # -- decoding ---------------------------------------------------------

    def feed(self, line):
        line = strip_ansi(line).rstrip()
        self.lines_seen += 1
        self.revision += 1
        kind = self._decode(line)
        category = classify_kind(line, kind)
        self.log.append(line)
        self.log_categories.append(category)
        return kind

    def _decode(self, line):
        if is_banner_line(line):
            return "banner"

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

        if RE_SLEEP.search(line) and not line.lstrip().startswith("[live-mods]"):
            self.sleeping = True
            self.status_label = "Sleeping"
            return "sleep"
        if RE_WAKE.search(line):
            self.sleeping = False
            if self.status_label == "Sleeping":
                self.status_label = "Running"
            return "wake"

        lower = line.lower()
        if ("hotspot" in lower or "slow chunk" in lower
                or ("throttl" in lower and "chunk" in lower)):
            coords = hotspot_coords(line)
            cx = coords[0] if coords else None
            cz = coords[1] if coords else None
            self.hotspots.append({"cx": cx, "cz": cz, "kind": "hotspot",
                                  "line": line})
            self.warnings += 1
            return "hotspot"
        if ("quarantin" in lower
                and ("chunk" in lower
                     or line.lstrip().lower().startswith("[quarantine"))):
            coords = hotspot_coords(line)
            cx = coords[0] if coords else None
            cz = coords[1] if coords else None
            self.hotspots.append({"cx": cx, "cz": cz, "kind": "quarantine",
                                  "line": line})
            self.quarantine_alerts += 1
            self.warnings += 1
            return "quarantine"

        match = RE_JOIN.search(line)
        if match:
            self.players[match.group(1)] = match.group(2)
            self.players_online = len(self.players)
            self.sleeping = False
            if self.status_label == "Sleeping":
                self.status_label = "Running"
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
                # Keep addresses already learned from join lines; the status
                # snapshot only reports names.
                self.players = {
                    name: self.players.get(name)
                    for name in names if name
                }
            return "players"

        match = RE_CHUNKS.search(line) or RE_WORLD_CHUNKS.search(line)
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

    # -- snapshot ---------------------------------------------------------

    def _mod_warnings(self):
        dual = self.dual_loader or {}
        count = int(dual.get("rejected", 0)) + int(dual.get("missing_deps", 0))
        for detail in self.mod_details:
            low = detail.get("detail", "").lower()
            if "missing" in low or "damaged" in low:
                count += 1
        return count

    def snapshot(self):
        tps = list(self.tps)
        mspt = list(self.mspt)
        mem = list(self.mem_mib)
        cpu = list(self.cpu_pct)
        mspc = list(self.ms_per_chunk)
        online = (self.players_online if self.players_online is not None
                  else len(self.players))
        categories = list(self.log_categories)
        lines = list(self.log)
        entries = [
            {"category": categories[index] if index < len(categories) else "event",
             "line": line}
            for index, line in enumerate(lines)
        ]
        hotspots = [dict(hotspot) for hotspot in self.hotspots]
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
            "player_addrs": {name: address
                             for name, address in self.players.items()},
            "players_online": online,
            "players_max": self.players_max,
            "loaded_chunks": self.loaded_chunks,
            "mods": dict(self.mods),
            "jars_seen": self.jars_seen,
            "mod_details": list(self.mod_details),
            "dual_loader": dict(self.dual_loader),
            "mod_warnings": self._mod_warnings(),
            "errors": self.errors,
            "warnings": self.warnings,
            "panics": self.panics,
            "over_budget": self.over_budget,
            "uptime_ticks": self.uptime_ticks,
            "clients": self.clients,
            "sleeping": self.sleeping,
            "hotspots": hotspots,
            "hotspot_alert": bool(hotspots),
            "quarantine_alerts": self.quarantine_alerts,
            "lines_seen": self.lines_seen,
            "log": lines,
            "log_categories": categories,
            "event_log": entries,
        }


# ------------------------------------------------------------------- frame

def _divider(ascii_mode):
    return " - " if ascii_mode else " · "


def _warning_mark(ascii_mode):
    return "!" if ascii_mode else "⚠"


def _status_bullet(ascii_mode):
    return "*" if ascii_mode else "●"


def _tps_band(cur):
    if cur is None:
        return "grey"
    if cur >= 19.5:
        return "green"
    if cur >= 18.0:
        return "yellow"
    return "red"


def _mspt_band(cur):
    if cur is None:
        return "grey"
    if cur < 30.0:
        return "green"
    if cur < 45.0:
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
    if cur is None:
        return "grey"
    if cur < 1024.0:
        return "green"
    if cur < 2048.0:
        return "yellow"
    return "red"


def _status_word(snap):
    """ALERT / SLEEPING / BUSY / ONLINE / BOOTING, most urgent first."""
    tps = snap.get("tps_cur")
    mspt = snap.get("mspt_cur")
    if (snap.get("panics") or snap.get("hotspot_alert")
            or snap.get("errors")):
        return "ALERT"
    if snap.get("sleeping") and not snap.get("players_online"):
        return "SLEEPING"
    if ((tps is not None and tps < 18.0)
            or (mspt is not None and mspt >= 45.0)):
        return "ALERT"
    label = (snap.get("status") or "").lower()
    if ("busy" in label
            or (tps is not None and tps < 19.5)
            or (mspt is not None and mspt >= 30.0)):
        return "BUSY"
    if snap.get("ready"):
        return "ONLINE"
    return "BOOTING"


def _status_color(word):
    return {
        "ONLINE": "green",
        "SLEEPING": "cyan",
        "BUSY": "yellow",
        "ALERT": "red",
        "BOOTING": "yellow",
    }.get(word, "grey")


def _header_line(snap, cols, ascii_mode, color):
    players_max = snap.get("players_max")
    max_text = str(players_max) if players_max is not None else "?"
    port = snap.get("port")
    address = (f"{snap.get('bind') or 'server'}:{port}"
               if port else "not listening")
    tps = snap.get("tps_cur")
    mspt = snap.get("mspt_cur")
    tps_text = (f"{fmt(tps, 1)} ({fmt(mspt, 1)}ms)"
                if tps is not None else "n/a")
    mem = snap.get("mem_cur")
    ram_text = f"{fmt(mem, 0)} MiB" if mem is not None else "n/a MiB"
    status = _status_word(snap)
    fields = [
        paint("MEGU " + MC_VERSIONS, "cyan", color),
        paint(_status_bullet(ascii_mode) + " " + status,
              _status_color(status), color),
        paint(address, "grey", color),
        paint(f"Players {snap.get('players_online', 0)}/{max_text}",
              "white", color),
        paint("TPS " + tps_text, _tps_band(tps), color),
        paint("RAM " + ram_text, _mem_band(mem), color),
    ]
    # Keep the header on one line. Wide terminals get two-space separators;
    # 80-column terminals use one space; then optional RAM/address fields drop.
    for count in (6, 5, 4, 3, 2):
        subset = fields[:count]
        for separator in ("  ", " "):
            line = separator.join(subset)
            if vlen(line) <= cols:
                return line
    return clip(" ".join(fields), cols)


def _health_entries(snap):
    tps = snap.get("tps_cur")
    mspt = snap.get("mspt_cur")
    mem = snap.get("mem_cur")
    cpu = snap.get("cpu_cur")
    return [
        ("TPS", fmt(tps, 1), _tps_band(tps), snap.get("tps", []), 0.0, 20.0),
        ("MSPT", fmt(mspt, 1) + "ms", _mspt_band(mspt), snap.get("mspt", []),
         0.0, 50.0),
        ("RAM", (fmt(mem, 0) + "M") if mem is not None else "n/a",
         _mem_band(mem), snap.get("mem", []), None, None),
        ("CPU", (fmt(cpu, 0) + "%") if cpu is not None else "n/a",
         _cpu_band(cpu), snap.get("cpu", []), 0.0, None),
    ]


def _health_line(snap, cols, ascii_mode, color):
    entries = _health_entries(snap)

    def render(spark_width, active):
        parts = [paint("HEALTH", "grey", color)]
        for name, value, band, values, low, high in active:
            segment = (paint(name, "grey", color) + " "
                       + paint(value, band, color))
            if spark_width > 0:
                segment += " " + paint(
                    sparkline(values, spark_width, low=low, high=high,
                              ascii_mode=ascii_mode), band, color)
            parts.append(segment)
        return "  ".join(parts)

    # Prefer showing all four metrics; shrink the sparklines before dropping
    # a metric so an 80-column terminal still sees TPS, MSPT, RAM and CPU.
    for active in (entries, entries[:3], entries[:2], entries[:1]):
        for spark_width in (8, 6, 5, 4, 3, 2, 0):
            line = render(spark_width, active)
            if vlen(line) <= cols:
                return line
    return clip(render(0, entries[:1]), cols)


def _player_card(snap, ascii_mode, color):
    online = snap.get("players_online", 0)
    players_max = snap.get("players_max")
    max_text = str(players_max) if players_max is not None else "?"
    if not online:
        return paint(f"Players 0/{max_text} (idle)", "grey", color)
    names = [name for name in snap.get("players", []) if name]
    if not names:
        return paint(f"Players {online}/{max_text}", "white", color)
    shown = names[:6]
    text = ", ".join(shown)
    if len(names) > len(shown):
        text += f" +{len(names) - len(shown)}"
    return paint(f"Players {online}/{max_text}: {text}", "white", color)


def _guard_card(snap, cols, ascii_mode, color):
    chunks = snap.get("loaded_chunks")
    chunks_text = f"Chunks {chunks:,}" if isinstance(chunks, int) else "Chunks n/a"
    divider = _divider(ascii_mode)
    hotspots = snap.get("hotspots") or []
    if hotspots:
        hotspot = hotspots[-1]
        cx, cz = hotspot.get("cx"), hotspot.get("cz")
        kind = hotspot.get("kind", "hotspot")
        if cx is not None and cz is not None:
            if kind == "quarantine":
                badge = f"{_warning_mark(ascii_mode)} Quarantine ({cx},{cz})"
            else:
                badge = f"{_warning_mark(ascii_mode)} Hotspot ({cx},{cz}) throttled"
        elif kind == "quarantine":
            badge = f"{_warning_mark(ascii_mode)} Chunk quarantined"
        else:
            badge = f"{_warning_mark(ascii_mode)} Chunk hotspot detected"
        return (paint(chunks_text, "white", color) + divider
                + paint(badge, "red", color))
    if snap.get("panics"):
        return (paint(chunks_text, "white", color) + divider
                + paint("Guard: TRIPPED", "red", color))
    return (paint(chunks_text, "white", color) + divider
            + paint("Guard: OK", "green", color))


def _mods_card(snap, ascii_mode, color):
    mods = snap.get("mods", {})
    active = int(mods.get("native", 0)) + int(mods.get("shim", 0))
    fallback = int(mods.get("fallback", 0))
    warnings = int(snap.get("mod_warnings", 0))
    divider = _divider(ascii_mode)
    text = f"Mods: {active} active"
    extras = []
    if fallback:
        extras.append(f"{fallback} fallback")
    if warnings:
        extras.append(f"{warnings} warning" + ("s" if warnings != 1 else ""))
    if extras:
        text += divider + divider.join(extras)
    return paint(text, "cyan", color)


def _pack_segments(segments, cols):
    """Pack colored segments into one or more lines without splitting words."""
    if not segments:
        return [""]
    for separator in ("  ", " "):
        total = sum(vlen(segment) for segment in segments)
        total += len(separator) * (len(segments) - 1)
        if total <= cols:
            return [separator.join(segments)]
    lines = []
    current = ""
    for segment in segments:
        if vlen(segment) > cols:
            segment = clip(segment, cols)
        if not current:
            current = segment
        elif vlen(current) + 2 + vlen(segment) <= cols:
            current += "  " + segment
        else:
            lines.append(current)
            current = segment
    lines.append(current)
    return lines


def _player_card_compact(snap, color):
    online = snap.get("players_online", 0)
    players_max = snap.get("players_max")
    max_text = str(players_max) if players_max is not None else "?"
    text = f"Players {online}/{max_text}"
    if not online:
        text += " (idle)"
    return paint(text, "white" if online else "grey", color)


def _mods_card_compact(snap, color):
    mods = snap.get("mods", {})
    active = int(mods.get("native", 0)) + int(mods.get("shim", 0))
    return paint(f"Mods: {active} active", "cyan", color)


def _cards(snap, cols, ascii_mode, color):
    segments = [
        _player_card(snap, ascii_mode, color),
        _guard_card(snap, cols, ascii_mode, color),
        _mods_card(snap, ascii_mode, color),
    ]
    packed = _pack_segments(segments, cols)
    if len(packed) == 1:
        return packed
    # The pinned HUD is strictly three lines: keep the summary on one line by
    # dropping optional detail (player names, fallback/warning counts).
    compact = [
        _player_card_compact(snap, color),
        _guard_card(snap, cols, ascii_mode, color),
        _mods_card_compact(snap, color),
    ]
    return _pack_segments(compact, cols)


def _log_category_entries(snap):
    entries = snap.get("event_log")
    if entries:
        return entries
    lines = snap.get("log", [])
    categories = snap.get("log_categories") or []
    return [
        {"category": (categories[index] if index < len(categories)
                      else classify_line(line)),
         "line": line}
        for index, line in enumerate(lines)
    ]


def normalize_log_filter(name):
    return name if name in LOG_FILTERS else "events"


def cycle_log_filter(name):
    try:
        index = LOG_FILTERS.index(normalize_log_filter(name))
    except ValueError:
        index = 0
    return LOG_FILTERS[(index + 1) % len(LOG_FILTERS)]


def selected_log_entries(snap, log_filter):
    mode = normalize_log_filter(log_filter)
    entries = _log_category_entries(snap)
    if mode == "all":
        return entries
    if mode == "alerts":
        return [entry for entry in entries
                if entry.get("category") == "alert"]
    return [entry for entry in entries
            if entry.get("category") != "telemetry"]


def filtered_log_lines(snap, log_filter):
    return [entry.get("line", "") for entry in
            selected_log_entries(snap, log_filter)]


# Alias kept for callers that describe the operation as selecting lines.
select_log_lines = filtered_log_lines


def _section_header(title, cols, ascii_mode, color, detail=None):
    bar = "-" if ascii_mode else "─"
    label = " " + title
    if detail:
        label += (" - " if ascii_mode else "  ·  ") + detail
    return paint(fit_plain(label + " ", cols, bar), "grey", color)


def _color_log_line(entry, color):
    line = entry.get("line", "")
    if not color:
        return line
    category = entry.get("category")
    if category == "banner":
        return paint(line, "purple", color)
    if category == "alert":
        name = "red" if (RE_FATAL.search(line) or RE_ERROR.search(line)) \
            else "yellow"
        return paint(line, name, color)
    if re.search(r"\bjoined\b", line, re.IGNORECASE):
        return paint(line, "green", color)
    if (re.search(r"\bleft\b", line, re.IGNORECASE)
            or re.search(r"\bdisconnected\b", line, re.IGNORECASE)):
        return paint(line, "grey", color)
    if line.lstrip().startswith("[chat]") or re.match(r"<\S+>", line.lstrip()):
        return paint(line, "cyan", color)
    if line.lstrip().startswith("[cmd]") or line.lstrip().startswith("[console]"):
        return paint(line, "yellow", color)
    return line


def _log_section(snap, body_rows, cols, ascii_mode, color, log_filter, dot):
    if body_rows <= 0:
        return []
    mode = normalize_log_filter(log_filter)
    entries = selected_log_entries(snap, mode)
    if mode == "events":
        hidden = sum(1 for entry in _log_category_entries(snap)
                     if entry.get("category") == "telemetry")
        detail = "filter:events"
        if hidden:
            detail += f"{dot}{hidden} telemetry hidden"
    elif mode == "alerts":
        detail = f"filter:alerts{dot}{len(entries)} alert" + (
            "" if len(entries) == 1 else "s")
    else:
        detail = f"filter:all{dot}{len(entries)} lines"
    out = [_section_header(LOG_FILTER_TITLES[mode], cols, ascii_mode, color,
                           detail)]
    tail = entries[-(body_rows - 1):] if body_rows > 1 else []
    if not tail and body_rows > 1:
        message = ("no alerts" if mode == "alerts" else "no events yet")
        out.append(paint("  " + message, "grey", color))
    else:
        out.extend(_color_log_line(entry, color) for entry in tail)
    return out


def _players_section(snap, body_rows, cols, ascii_mode, color, dot):
    if body_rows <= 0:
        return []
    online = snap.get("players_online", 0)
    uptime = snap.get("uptime_ticks")
    chunks = snap.get("loaded_chunks")
    detail = f"Status {_status_word(snap)}"
    detail += f"{dot}{online} online"
    if uptime is not None:
        detail += f"{dot}uptime {uptime} ticks"
    if isinstance(chunks, int):
        detail += f"{dot}{chunks:,} chunks"
    out = [_section_header("Players", cols, ascii_mode, color, detail)]
    names = [name for name in snap.get("players", []) if name]
    addresses = snap.get("player_addrs") or {}
    if not online:
        out.append(paint("  No players online (server idle)", "grey", color))
    elif not names:
        out.append(paint(f"  {online} player(s) online (names not reported)",
                         "grey", color))
    else:
        for name in names[:max(0, body_rows - 1)]:
            address = addresses.get(name) or "connected"
            out.append(paint(f"  {name}{dot}{address}", "white", color))
    return out


def _tier_label(tier):
    low = (tier or "").lower()
    if "tier2" in low:
        return "compatibility shim"
    if "tier3" in low:
        return "runtime adapter"
    return tier or "adapted"


def _mods_section(snap, body_rows, cols, ascii_mode, color, dot):
    if body_rows <= 0:
        return []
    mods = snap.get("mods", {})
    active = int(mods.get("native", 0)) + int(mods.get("shim", 0))
    fallback = int(mods.get("fallback", 0))
    warnings = int(snap.get("mod_warnings", 0))
    detail = f"{active} active"
    if fallback:
        detail += f"{dot}{fallback} fallback"
    if warnings:
        detail += f"{dot}{warnings} warning" + ("" if warnings == 1 else "s")
    out = [_section_header("Mods", cols, ascii_mode, color, detail)]
    details = snap.get("mod_details", [])
    if not details:
        out.append(paint("  No adapted mods recorded", "grey", color))
        return out
    for item in details[:max(0, body_rows - 1)]:
        line = (f"  {item.get('file', '?')}{dot}{item.get('mod_id', '?')}"
                f"{dot}{_tier_label(item.get('tier'))}"
                f"{dot}{item.get('detail', '')}")
        out.append(paint(line, "white", color))
    return out


def _body(snap, body_rows, cols, ascii_mode, color, ui, log_filter):
    dot = _divider(ascii_mode)
    view = ui.get("view")
    if view is None:
        view = "mods" if ui.get("mods") else "log"
    if view == "players":
        return _players_section(snap, body_rows, cols, ascii_mode, color, dot)
    if view == "mods":
        return _mods_section(snap, body_rows, cols, ascii_mode, color, dot)
    return _log_section(snap, body_rows, cols, ascii_mode, color, log_filter,
                        dot)


def _footer_line(snap, cols, ascii_mode, color, ui, log_filter):
    if ui.get("stopping"):
        text = " stopping megu gracefully" + ("..." if ascii_mode else "…")
        return paint(clip(text, cols), "yellow", color)
    if ui.get("exited") is not None:
        dot = _divider(ascii_mode)
        text = f" megu exited{dot}code {ui['exited']}"
        return paint(clip(text, cols), "grey", color)
    if ui.get("cmd_mode"):
        cursor = "_" if ascii_mode else "█"
        prompt = " /" + ui.get("cmd_buf", "") + cursor
        return paint(clip(prompt, cols), "cyan", color)
    mode = normalize_log_filter(log_filter)
    view = ui.get("view") or ("mods" if ui.get("mods") else "log")
    player_key = "[p] log" if view == "players" else "[p] players"
    mods_key = "[m] log" if view == "mods" else "[m] mods"
    segments = [
        paint("[/] cmd", "white", color),
        paint(f"[f] filter:{mode}", "cyan", color),
        paint(player_key, "white", color),
        paint(mods_key, "white", color),
        paint("[q] save & stop", "white", color),
    ]
    warnings = int(snap.get("warnings", 0))
    errors = int(snap.get("errors", 0))
    counts = (paint(f"{warnings} warn", "yellow" if warnings else "grey", color)
              + paint(_divider(ascii_mode), "grey", color)
              + paint(f"{errors} err", "red" if errors else "grey", color))
    for separator in ("  ", " "):
        line = separator.join(segments) + separator + counts
        if vlen(line) <= cols:
            return line
    compact = (paint(f"{warnings}w {errors}e", "grey", color))
    for separator in ("  ", " "):
        line = separator.join(segments) + separator + compact
        if vlen(line) <= cols:
            return line
    line = " ".join(segments) + " " + compact
    return clip(line, cols)


def render_frame(snap, cols, rows, ascii_mode=False, color=False, ui=None,
                 log_filter=None):
    """Render one dashboard frame as a list of `rows` strings.

    Every returned line is exactly `cols` visible columns (ANSI sequences do
    not count). The frame is a pure function of `snap`, `cols`, `rows`, and
    the flags - no clock, no randomness - so --demo-frame and
    --render-snapshot output is deterministic.

    Layout: one-line header, a 1-3 row health/cards summary bar, then the
    rest of the screen is clean log content, with a one-line footer.
    """
    cols = max(8, int(cols))
    rows = max(4, int(rows))
    ui = dict(ui or {})
    mode = normalize_log_filter(
        log_filter if log_filter is not None
        else ui.get("log_filter", "events"))

    blocks = [_header_line(snap, cols, ascii_mode, color)]
    if rows >= 8:
        blocks.append(_health_line(snap, cols, ascii_mode, color))
    if rows >= 10:
        blocks.extend(_cards(snap, cols, ascii_mode, color))

    # Keep at least one body line and the footer even on tiny terminals.
    max_summary = max(1, rows - 2)
    if len(blocks) > max_summary:
        blocks = blocks[:max_summary]

    body_rows = rows - len(blocks) - 1
    body = []
    if body_rows > 0:
        body = _body(snap, body_rows, cols, ascii_mode, color, ui, mode)

    lines = blocks + body[:body_rows]
    if len(lines) > rows - 1:
        lines = lines[:rows - 1]
    lines += [""] * (rows - 1 - len(lines))
    lines.append(_footer_line(snap, cols, ascii_mode, color, ui, mode))

    out = []
    for line in lines:
        if ascii_mode:
            line = asciify(strip_ansi(line))
        out.append(clip(line, cols))
    return out


# -------------------------------------------------------------- demo content

DEMO_LOG = """\
[live-mods] 5 jar(s): 1 auto-skipped, 2 native-bundle, 1 tier2-shim, 1 tier3
[live-mods] weirdmod.jar (weirdmod) tier3: 412 classes, 9 site(s) - mixin target missing
[live-mods] liftcraft.jar (liftcraft) tier2 shim: 208 classes, 4 site(s) - damage hooks lifted
[anvil] loaded world: WorldSummary(chunks=1284, players=3)
[tick] observed_tps=19.90 avg_work_ms=11.50 max_work_ms=15.77 over_budget=0/100
megu v1.21.1 listening 127.0.0.1:25565 (offline identities; no Mojang authentication)
[spine] Bob joined from 127.0.0.1:51130 (uuid fedcba9876543210fedcba9876543210)
[sz] t=500 VmRSS: 524288 kB | mobs=44 drops=8 xps=4 arrows=0 bob=0 pearl=0 primed=0 chests=18 furn=6 crops=12 world=0 inv=2 clients=2
Players: 2/20 (Bob, Carol)
[security] velocity forwarding refused for Mallory from 10.0.0.9: missing forwarding secret
[chunkprof] window=60s chunks=1284 (4.2 ms/chunk) map=15.8ms entities=91
"""


def demo_parser(include_banner=True, ascii_banner=False):
    """A deterministic snapshot: canned realistic log plus a synthetic wave.

    The startup banner is written before the first server line unless the
    caller (or --no-banner) opts out. It is ordinary scrolling log content,
    not pinned chrome, so a long session naturally scrolls it away.
    """
    parser = MetricsParser()
    if include_banner:
        parser.inject_banner(ascii_mode=ascii_banner)
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


def _toggle_view(state, name):
    current = state.get("view")
    if current is None:
        current = "mods" if state.get("mods") else "log"
    state["view"] = "log" if current == name else name


def handle_keys(data, state, proc):
    """Apply raw terminal bytes to dashboard state; forward submitted commands."""
    if "log_filter" not in state:
        state["log_filter"] = "events"
    if "history" not in state:
        state["history"] = []
    if "history_index" not in state:
        state["history_index"] = len(state["history"])
    index = 0
    while index < len(data):
        byte = data[index]
        if state["cmd_mode"]:
            if byte == 0x1B:
                sequence = data[index + 1:index + 3]
                if sequence == b"[A":
                    history = state["history"]
                    if history:
                        position = max(0, state["history_index"] - 1)
                        state["history_index"] = position
                        state["cmd_buf"] = history[position]
                    index += 3
                    continue
                if sequence == b"[B":
                    history = state["history"]
                    position = min(len(history), state["history_index"] + 1)
                    state["history_index"] = position
                    state["cmd_buf"] = (history[position]
                                        if position < len(history) else "")
                    index += 3
                    continue
                if sequence in (b"[C", b"[D"):
                    index += 3
                    continue
                state["cmd_mode"] = False
                state["cmd_buf"] = ""
                index += 1
                continue
            if byte in (0x0D, 0x0A):
                command = state["cmd_buf"]
                if command.strip():
                    send_line(proc, command)
                    history = state["history"]
                    if not history or history[-1] != command:
                        history.append(command)
                    del history[:-50]
                state["cmd_buf"] = ""
                state["cmd_mode"] = False
                state["history_index"] = len(state["history"])
            elif byte in (0x7F, 0x08):
                state["cmd_buf"] = state["cmd_buf"][:-1]
                state["history_index"] = len(state["history"])
            elif 0x20 <= byte < 0x7F:
                state["cmd_buf"] += chr(byte)
                state["history_index"] = len(state["history"])
            index += 1
            continue

        char = chr(byte)
        if byte == 0x03 or char == "q":
            state["quit"] = True
        elif char == "f":
            state["log_filter"] = cycle_log_filter(
                state.get("log_filter", "events"))
        elif char == "p":
            _toggle_view(state, "players")
        elif char == "m":
            _toggle_view(state, "mods")
        elif char == "/":
            state["cmd_mode"] = True
            state["cmd_buf"] = ""
            state["history_index"] = len(state.get("history", []))
        index += 1


def render_key(parser, state, cols, rows):
    """Compact dirty-frame key: a frame is redrawn only while this changes."""
    view = state.get("view")
    if view is None:
        view = "mods" if state.get("mods") else "log"
    return (parser.revision, view, state.get("cmd_mode"),
            state.get("cmd_buf"), state.get("history_index"),
            normalize_log_filter(state.get("log_filter", "events")),
            state.get("stopping"), state.get("exited"), cols, rows)


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
                    frame = render_frame(
                        snap, size.columns, size.lines,
                        ascii_mode=args.ascii_mode, color=color, ui=state,
                        log_filter=state.get("log_filter", args.log_filter))
                    term.write("\x1b[H" + "\r\n".join(frame) + "\x1b[J")
                    prev_key = key
    for thread in threads:
        thread.join(timeout=0.4)


def report_exit(code, parser):
    """After the alt screen is left, leave a short tail for scrollback."""
    out = [f"megu-tui: megu exited, code {exit_code(code)}"]
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
    if not getattr(args, "no_banner", False):
        parser.inject_banner(ascii_mode=getattr(args, "ascii_mode", False))
    lock = threading.Lock()
    state = {"quit": False, "stopping": False, "stop_sent": False,
             "view": "log", "cmd_mode": False, "cmd_buf": "",
             "history": [], "history_index": 0,
             "log_filter": normalize_log_filter(args.log_filter),
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
    """Select the dashboard: explicit --tui still respects terminal safety."""
    if args.plain or getattr(args, "no_tui", False):
        return False
    capable = tui_capable(env, stdin.isatty(), stdout.isatty())
    if getattr(args, "tui", False):
        return capable
    return capable


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
        description="megu smart server dashboard / safe passthrough wrapper",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "examples:\n"
            "  %(prog)s --server-bin ./target/release/megu -- --port 25565\n"
            "  %(prog)s --demo-frame --cols 92 --rows 22\n"
            "  %(prog)s --demo-frame --log-filter all --ascii\n"
            "  %(prog)s --demo-frame --no-banner --ascii\n"
            "  %(prog)s --cli --server-bin ./megu -- --port 25565\n"
            "  %(prog)s --render-snapshot server.log --log-filter alerts\n"
        ),
    )
    parser.add_argument("--server-bin", metavar="PATH",
                        help="megu binary to launch")
    parser.add_argument("--cols", type=int, default=0,
                        help="frame width (default: terminal size or 80)")
    parser.add_argument("--rows", type=int, default=0,
                        help="frame height (default: terminal size or 24)")
    parser.add_argument("--plain", "--no-tui", "--cli", "--nogui",
                        dest="plain", action="store_true",
                        help="force plain-text passthrough, never a TUI "
                             "(--no-tui/--cli/--nogui are aliases)")
    parser.add_argument("--tui", action="store_true",
                        help="explicitly request the dashboard on an "
                             "interactive TTY (auto-detected by default)")
    parser.add_argument("--ascii", dest="ascii_mode", action="store_true",
                        help="pure-ASCII glyphs and no color")
    parser.add_argument("--color", action="store_true",
                        help="force ANSI color in frame modes even when piped")
    parser.add_argument("--log-filter", choices=LOG_FILTERS, default="events",
                        help="event log filter: events (default), alerts, all")
    parser.add_argument("--no-banner", action="store_true",
                        help="skip the startup MEGU banner in the scrolling log"
                             " (also MEGU_NO_BANNER=1 / NO_BANNER=1)")
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
    args.no_banner = banner_disabled(args, os.environ)

    size = shutil.get_terminal_size(fallback=(DEFAULT_COLS, DEFAULT_ROWS))
    cols = args.cols if args.cols > 0 else size.columns
    rows = args.rows if args.rows > 0 else size.lines

    if args.demo_frame or args.render_snapshot is not None:
        if args.demo_frame:
            parser = demo_parser(
                include_banner=not args.no_banner,
                ascii_banner=args.ascii_mode or args.plain)
        else:
            parser = snapshot_from_file(args.render_snapshot)
        if parser is None:
            return 2
        frame = render_frame(
            parser.snapshot(), cols, rows,
            ascii_mode=args.ascii_mode or args.plain,
            color=frame_color(args, os.environ),
            log_filter=args.log_filter)
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
