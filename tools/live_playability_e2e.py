#!/usr/bin/env python3
"""Live Playability & Mod Integration E2E gate.

Boots the REAL compiled ``megu`` server and ``megu-client`` (``--headless``)
end-to-end through the same ``--quickPlaySingleplayer`` integrated launch a
player uses, and fails closed on the failure modes the unit tests missed:

* broken relative ``--server-bin`` symlinks in the per-world run directory
* repository test datapacks (``testpack``/``modtest``/``rectest``/
  ``spinetest``/``tagtest``) leaking into a player world
* the audio subprocess (``pw-cat``/``aplay``) dying mid-stream
* ``world_overlay.bin`` never written on disconnect / corrupt on reload
* pre-join chunk stalls (``MEGU_CHUNK_PROFILE=1`` ``band lookups=0`` shape)
* ``unsupported fluid shape`` runtime errors (``minecraft:glow_lichen``)
* ``MEGU_MODS_DIR`` not forwarded to the integrated server
* Create not armed/saved, live mobs absent, or GPU headless-probe failing

Standard library only, Python 3.9+. Never touches the network: the client
talks to its own integrated loopback server.

Checks
  C1  Clean bootstrap & relative-path resilience — no broken/looping
      symlinks under ``<gameDir>/saves/<world>`` and no test datapacks in
      the world dir or the server log.
  C2  Login readiness & performance budget — time from launch to
      ``[spine] <user> joined`` must stay under ``--max-join-secs``
      (default 8.0 s, ``--strict`` 3.0 s); CHUNKPROF counters are captured.
  C3  Clean stderr gate — no ``unsupported fluid shape``, no
      ``exited while streaming``, no ``panicked at``, no ``No such file or
      directory``, no ``Too many levels of symbolic links``, no save or
      corruption failures.
  C4  Persistence & restart round-trip — ``world_overlay.bin`` exists and
      is non-empty after the session ends, and a second launch of the same
      world reloads cleanly.
  C5  Synthetic mod JAR end-to-end — ``<gameDir>/mods/e2e_test_mod.jar`` is
      discovered by the live mod scan (``[live-mods]`` report) instead of
      being silently ignored.
  C6  Create live runtime — a synthetic Create-family JAR arms and persists
      the Create runtime across restart while mobs are observed; an optional
      GPU headless probe accepts that same modded ``--gameDir``.
  G1  GPU client probe — ``megu-client-gpu --headless-probe`` exits 0 when
      ``--gpu-client-bin`` is given.

Usage
  python3 tools/live_playability_e2e.py \
      --server-bin ./target/release/megu \
      --client-bin client-app/target/release/megu-client \
      [--gpu-client-bin client-gpu/target/release/megu-client-gpu] \
      [--mode vanilla|modded|all] [--strict] [--json] [--keep-tmp]

Exit status: 0 = every check passed, 1 = at least one check failed,
2 = the gate itself could not run (binary/argument problems).
"""
import argparse
import errno
import json
import os
import re
import struct
import subprocess
import sys
import tempfile
import threading
import time
import zipfile
import zlib
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

WORLD_NAME = "E2EWorld"
USERNAME = "E2EGate"
MOD_JAR_NAME = "e2e_test_mod.jar"
MOD_ID = "e2emod"
CREATE_E2E_JAR_NAME = "create_e2e.jar"
CREATE_E2E_MOD_ID = "create_e2e"

# Repository test datapacks that must never reach a player world. Keep this
# list aligned with datapacks/ minus the generated vanilla pack.
TEST_DATAPACK_NAMES = ("testpack", "modtest", "rectest", "spinetest", "tagtest")

# stderr lines that always mean a real-player regression. Substring match so
# platform/prefix variation still trips the gate.
FORBIDDEN_STDERR = (
    "unsupported fluid shape",
    "exited while streaming",
    "panicked at",
    "No such file or directory",
    "Too many levels of symbolic links",
    "WORLD SAVE FAILED",
    "refusing corrupt world save",
    "delta WAL recovery failed",
)

JOINED_RE = re.compile(r"\[spine\] (?P<user>\S+) joined from (?P<peer>\S+)")
CHUNKPROF_N_RE = re.compile(r"^CHUNKPROF n=(?P<n>\d+)")
CHUNKPROF_BAND_RE = re.compile(
    r"band lookups=(?P<lookups>[\d.]+) new=(?P<new>[\d.]+) "
    r"build=(?P<build>[\d.]+)ms cells=(?P<cells>[\d.]+) "
    r"\(per new=(?P<per_new>[\d.]+)ms\)"
)
CHUNKPROF_PARTS_RE = re.compile(
    r"band parts: range=(?P<range>[\d.]+)ms sections=(?P<sections>[\d.]+)ms "
    r"cols_miss=(?P<cols_miss>[\d.]+) epoch_bumps=(?P<epoch>[\d.]+)/chunk"
)
CHUNKPROF_FLOORS_RE = re.compile(
    r"floors: calls=(?P<calls>[\d.]+)/chunk cells=(?P<cells>[\d.]+)/chunk "
    r"starts=(?P<starts>[\d.]+)ms/chunk"
)
MOBS_RE = re.compile(r"\bmobs=(\d+)")
LIVE_MODS_SUMMARY_RE = re.compile(
    r"\[live-mods\] (?P<jars>\d+) jar\(s\): (?P<skipped>\d+) auto-skipped, "
    r"(?P<native>\d+) native-bundle, (?P<tier2>\d+) tier2-shim, "
    r"(?P<tier3>\d+) tier3"
)
LIVE_MODS_SCANNED_RE = re.compile(r"\[live-mods\] mods dir scanned: (?P<jars>\d+) jar\(s\)")


# --------------------------------------------------------------------------
# Parsers
# --------------------------------------------------------------------------

def parse_chunkprof(lines):
    """Fold `MEGU_CHUNK_PROFILE=1` stderr lines into one metrics dict.

    Later report blocks overwrite earlier ones so the result reflects the
    most recent server-side snapshot. `mobs_last` comes from the periodic
    `[sz]` status line, not CHUNKPROF.
    """
    metrics = {
        "chunkprof_reports": 0,
        "chunks_delivered": None,
        "band_lookups_per_chunk": None,
        "band_new_per_chunk": None,
        "band_ms_per_new": None,
        "epoch_bumps_per_chunk": None,
        "floors_calls_per_chunk": None,
        "floors_cells_per_chunk": None,
        "floors_ms_per_chunk": None,
        "mobs_last": None,
    }
    for line in lines:
        if line.startswith("CHUNKPROF"):
            metrics["chunkprof_reports"] += 1
        m = CHUNKPROF_N_RE.search(line)
        if m:
            metrics["chunks_delivered"] = int(m.group("n"))
        m = CHUNKPROF_BAND_RE.search(line)
        if m:
            metrics["band_lookups_per_chunk"] = float(m.group("lookups"))
            metrics["band_new_per_chunk"] = float(m.group("new"))
            metrics["band_ms_per_new"] = float(m.group("per_new"))
        m = CHUNKPROF_PARTS_RE.search(line)
        if m:
            metrics["epoch_bumps_per_chunk"] = float(m.group("epoch"))
        m = CHUNKPROF_FLOORS_RE.search(line)
        if m:
            metrics["floors_calls_per_chunk"] = float(m.group("calls"))
            metrics["floors_cells_per_chunk"] = float(m.group("cells"))
            metrics["floors_ms_per_chunk"] = float(m.group("starts"))
        m = MOBS_RE.search(line)
        if m:
            metrics["mobs_last"] = int(m.group(1))
    return metrics


def find_forbidden(lines, forbidden=FORBIDDEN_STDERR):
    """Return [(line_index, needle, line)] for every forbidden stderr line."""
    hits = []
    for idx, line in enumerate(lines):
        for needle in forbidden:
            if needle in line:
                hits.append((idx, needle, line))
                break
    return hits


def find_test_datapack_mentions(lines, names=TEST_DATAPACK_NAMES):
    """Return [(line_index, name, line)] when a repo test datapack name is
    mentioned in captured output. Word-boundary match so e.g. `datapacks`
    or `megu-mod-check` do not collide."""
    hits = []
    pattern = re.compile(
        r"(?<![A-Za-z0-9_.-])(" + "|".join(re.escape(n) for n in names) + r")"
        r"(?![A-Za-z0-9_.-])"
    )
    for idx, line in enumerate(lines):
        m = pattern.search(line)
        if m:
            hits.append((idx, m.group(1), line))
    return hits


def parse_live_mods_report(lines):
    """Parse the `[live-mods]` boot report. Returns the last summary dict or
    None when the scan never ran (MEGU_MODS_DIR absent or stub build)."""
    report = None
    for line in lines:
        m = LIVE_MODS_SUMMARY_RE.search(line)
        if m:
            report = {
                "jars": int(m.group("jars")),
                "auto_skipped": int(m.group("skipped")),
                "native_bundle": int(m.group("native")),
                "tier2_shim": int(m.group("tier2")),
                "tier3": int(m.group("tier3")),
                "line": line,
            }
        m = LIVE_MODS_SCANNED_RE.search(line)
        if m:
            report = {"jars": int(m.group("jars")), "none_classified": True,
                      "line": line}
    return report


# --------------------------------------------------------------------------
# Filesystem checks
# --------------------------------------------------------------------------

def inspect_world_dir(world_dir):
    """Walk the per-world run dir and classify symlink + datapack state.

    Returns (problems, info). `problems` is a list of human-readable
    failures: broken symlinks, symlink loops (ELOOP, the exact
    ``Too many levels of symbolic links`` failure), and datapack entries
    other than ``vanilla`` (the test-pack leak). `info` carries the
    observed layout for the report.
    """
    world_dir = Path(world_dir)
    problems = []
    info = {"exists": world_dir.is_dir(), "datapacks_entries": None,
            "datapacks_is_link": None, "symlinks": [], "entries": []}
    if not world_dir.is_dir():
        problems.append(f"world dir missing: {world_dir}")
        return problems, info
    for dirpath, dirnames, filenames in os.walk(world_dir, followlinks=False):
        for name in sorted(dirnames + filenames):
            path = Path(dirpath) / name
            rel = path.relative_to(world_dir).as_posix()
            info["entries"].append(rel)
            if path.is_symlink():
                info["symlinks"].append(rel)
                try:
                    os.stat(path)
                except FileNotFoundError:
                    problems.append(
                        f"broken symlink {rel} -> {os.readlink(path)}")
                except OSError as error:
                    if error.errno == errno.ELOOP:
                        problems.append(f"symlink loop {rel}")
                    else:
                        problems.append(f"symlink {rel} unreadable: {error}")
    datapacks = world_dir / "datapacks"
    if datapacks.is_symlink():
        info["datapacks_is_link"] = True
        if datapacks.is_dir():
            info["datapacks_entries"] = sorted(
                p.name for p in datapacks.iterdir())
        else:
            problems.append("datapacks/ is a dangling symlink")
    elif datapacks.is_dir():
        info["datapacks_is_link"] = False
        info["datapacks_entries"] = sorted(p.name for p in datapacks.iterdir())
    else:
        problems.append("datapacks/ missing in world dir")
    if info["datapacks_entries"] is not None:
        for entry in info["datapacks_entries"]:
            if entry in TEST_DATAPACK_NAMES:
                problems.append(f"test datapack linked into world: datapacks/{entry}")
            elif entry != "vanilla":
                problems.append(f"unexpected datapack linked into world: datapacks/{entry}")
    for required in ("assets", "config_packets.bin"):
        if not (world_dir / required).exists():
            problems.append(f"{required} missing or unresolvable in world dir")
    return problems, info


def runtime_root_for(server_bin, cwd):
    """Mirror integrated.rs find_runtime_root: first ancestor of the
    resolved server binary (then of `cwd`) that contains
    config_packets.bin + datapacks/ + assets/block_states_1211.json."""
    server_bin = Path(server_bin)
    resolved = server_bin if server_bin.is_absolute() else Path(cwd) / server_bin
    try:
        resolved = resolved.resolve()
    except OSError:
        pass
    bases = list(resolved.parents[:8]) + list(Path(cwd).resolve().parents[:8])
    seen = set()
    for base in bases:
        if base in seen:
            continue
        seen.add(base)
        if ((base / "config_packets.bin").is_file()
                and (base / "datapacks").is_dir()
                and (base / "assets" / "block_states_1211.json").is_file()):
            return base
    return None


def resolve_arg_path(arg, base):
    """Resolve a possibly-relative CLI path against `base` without requiring
    it to exist (canonicalize-best-effort, like the Rust side)."""
    path = Path(arg)
    if not path.is_absolute():
        path = Path(base) / path
    return Path(os.path.normpath(path))


def relative_server_arg(server_bin, run_cwd):
    """Express `server_bin` relative to `run_cwd` so the child must do real
    relative-path resolution. Falls back to the absolute path when the two
    share no anchor."""
    try:
        rel = os.path.relpath(server_bin, run_cwd)
    except ValueError:
        return str(server_bin)
    if rel.startswith(".."):
        return str(server_bin)
    return "." + os.sep + rel


# --------------------------------------------------------------------------
# Synthetic mod JAR
# --------------------------------------------------------------------------

def _png_chunk(tag, payload):
    body = tag + payload
    return struct.pack(">I", len(payload)) + body + struct.pack(">I", zlib.crc32(body))


def png_1x1(rgba=(0x3E, 0xC4, 0x5A, 0xFF)):
    """A valid 1x1 RGBA PNG, stdlib-built (no fixtures needed)."""
    ihdr = struct.pack(">IIBBBBB", 1, 1, 8, 6, 0, 0, 0)
    scanline = b"\x00" + bytes(rgba)
    idat = zlib.compress(scanline)
    return (b"\x89PNG\r\n\x1a\n" + _png_chunk(b"IHDR", ihdr)
            + _png_chunk(b"IDAT", idat) + _png_chunk(b"IEND", b""))


SYNTHETIC_MOD_MANIFEST = """modLoader = "javafml"
loaderVersion = "[1,)"
license = "MIT"

[[mods]]
modId = "{mod_id}"
version = "0.0.0-e2e"

[[dependencies.{mod_id}]]
modId = "minecraft"
type = "required"
versionRange = "[1.21,1.22)"
side = "BOTH"

[[dependencies.{mod_id}]]
modId = "neoforge"
type = "required"
versionRange = "[21.1,)"
side = "BOTH"
"""


def synthetic_mod_members(mod_id=MOD_ID):
    """The member map for the minimal synthetic mod JAR."""
    manifest = SYNTHETIC_MOD_MANIFEST.format(mod_id=mod_id)
    blockstate = {
        "variants": {"": {"model": f"{mod_id}:block/e2e_block"}},
    }
    model = {
        "parent": "minecraft:block/cube_all",
        "textures": {"all": f"{mod_id}:block/e2e_block"},
    }
    recipe = {
        "type": "minecraft:crafting_shapeless",
        "ingredients": [{"item": "minecraft:flint"}],
        "result": {"id": f"{mod_id}:e2e_shard", "count": 2},
    }
    lang = {"block.%s.e2e_block" % mod_id: "E2E Block"}
    return {
        "META-INF/neoforge.mods.toml": manifest.encode("utf-8"),
        "pack.mcmeta": json.dumps(
            {"pack": {"pack_format": 48, "description": "megu e2e synthetic mod"}},
            sort_keys=True).encode("utf-8"),
        f"assets/{mod_id}/blockstates/e2e_block.json": json.dumps(
            blockstate, sort_keys=True).encode("utf-8"),
        f"assets/{mod_id}/models/block/e2e_block.json": json.dumps(
            model, sort_keys=True).encode("utf-8"),
        f"assets/{mod_id}/textures/block/e2e_block.png": png_1x1(),
        f"assets/{mod_id}/lang/en_us.json": json.dumps(
            lang, sort_keys=True).encode("utf-8"),
        f"data/{mod_id}/recipes/e2e_shard.json": json.dumps(
            recipe, sort_keys=True).encode("utf-8"),
    }


def build_synthetic_mod_jar(path, mod_id=MOD_ID):
    """Write the minimal valid mod JAR: real ZIP, loader manifest, assets
    and data. Pure-declarative content means the live adapter classifies it
    as a native bundle — the test asserts exactly that discovery."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for name, payload in sorted(synthetic_mod_members(mod_id).items()):
            archive.writestr(name, payload)
    return path


def build_synthetic_create_jar(path):
    """Write a tiny valid NeoForge JAR with the Create resource paths C6
    exercises. The filename intentionally contains ``create`` because the
    server's Create gate recognizes Create-family JAR names."""
    manifest = SYNTHETIC_MOD_MANIFEST.format(mod_id=CREATE_E2E_MOD_ID)
    members = {
        "META-INF/neoforge.mods.toml": manifest.encode("utf-8"),
        "assets/create/blockstates/shaft.json": json.dumps(
            {"variants": {"axis=x": {"model": "create:block/shaft"}}},
            sort_keys=True).encode("utf-8"),
        "assets/create/textures/block/shaft.png": png_1x1(),
        "assets/create/lang/ja_jp.json": json.dumps(
            {"block.create.shaft": "シャフト"}, ensure_ascii=False,
            sort_keys=True).encode("utf-8"),
    }
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for name, payload in sorted(members.items()):
            archive.writestr(name, payload)
    return path


# --------------------------------------------------------------------------
# Session runner
# --------------------------------------------------------------------------

@dataclass
class SessionResult:
    label: str
    mode: str
    cmd: list = field(default_factory=list)
    cwd: str = ""
    exit_code: object = None
    spawn_error: str = ""
    join_seen: bool = False
    join_secs: object = None
    session_timed_out: bool = False
    join_timed_out: bool = False
    wall_secs: float = 0.0
    stdout_text: str = ""
    stderr_lines: list = field(default_factory=list)
    client_summary: object = None
    chunkprof: dict = field(default_factory=dict)
    world_dir: str = ""
    overlay_exists: bool = False
    overlay_bytes: int = 0
    overlay_waited: bool = False
    create_factory_bytes: int = 0
    create_factory_valid: bool = False

    def combined_lines(self):
        return self.stderr_lines + self.stdout_text.splitlines()


def _scrub_env(extra):
    """Drop every inherited MEGU_* knob so the gate sees the exact
    environment a player gets, then apply the explicit overrides."""
    env = {k: v for k, v in os.environ.items() if not k.startswith("MEGU_")}
    env.update(extra or {})
    return env


def valid_create_factory(data):
    return len(data) >= 9 and data[:4] in (b"MCF1", b"MCF2")


def run_session(*, label, mode, client_bin, server_bin_arg, run_cwd, game_dir,
                world, username, ticks, view_distance, seed, join_timeout,
                exit_timeout, save_timeout, env_extra=None):
    """Drive one real `megu-client --headless --quickPlaySingleplayer`
    session: integrated server spawn, join-marker timing, stderr capture,
    graceful exit, then the world_overlay.bin wait."""
    result = SessionResult(label=label, mode=mode)
    game_dir = Path(game_dir)
    world_dir = game_dir / "saves" / world
    result.world_dir = str(world_dir)
    overlay = world_dir / "world_overlay.bin"
    cmd = [
        str(client_bin),
        "--username", username,
        "--gameDir", str(game_dir),
        "--quickPlaySingleplayer", world,
        "--server-bin", str(server_bin_arg),
        "--headless",
        "--ticks", str(ticks),
        "--script", "idle",
        "--view-distance", str(view_distance),
        "--json-summary",
    ]
    if seed is not None:
        cmd += ["--seed", str(seed)]
    result.cmd = cmd
    result.cwd = str(run_cwd)
    env = _scrub_env(env_extra)
    env["MEGU_CHUNK_PROFILE"] = "1"

    t0 = time.monotonic()
    try:
        proc = subprocess.Popen(
            cmd, cwd=str(run_cwd), env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, bufsize=1,
        )
    except OSError as error:
        result.spawn_error = f"{error}"
        return result

    stderr_lines = []
    stdout_parts = []

    def read_stderr():
        for raw in proc.stderr:
            stderr_lines.append((time.monotonic() - t0, raw.rstrip("\n")))

    def read_stdout():
        stdout_parts.append(proc.stdout.read())

    threads = [
        threading.Thread(target=read_stderr, daemon=True),
        threading.Thread(target=read_stdout, daemon=True),
    ]
    for thread in threads:
        thread.start()

    joined_re = JOINED_RE
    deadline = t0 + join_timeout
    scanned = 0
    while proc.poll() is None and time.monotonic() < deadline:
        while scanned < len(stderr_lines):
            stamp, line = stderr_lines[scanned]
            scanned += 1
            if joined_re.search(line):
                result.join_seen = True
                result.join_secs = stamp
                break
        if result.join_seen:
            break
        time.sleep(0.05)
    if not result.join_seen and proc.poll() is None:
        result.join_timed_out = True

    try:
        result.exit_code = proc.wait(timeout=exit_timeout)
    except subprocess.TimeoutExpired:
        result.session_timed_out = True
        proc.kill()
        try:
            result.exit_code = proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            result.exit_code = None

    for thread in threads:
        thread.join(timeout=5)
    proc.stdout.close()
    proc.stderr.close()
    result.wall_secs = time.monotonic() - t0
    result.stderr_lines = [line for _, line in stderr_lines]
    result.stdout_text = "".join(stdout_parts)
    # Recheck the marker in lines that landed during shutdown.
    if not result.join_seen:
        for stamp, line in stderr_lines:
            if joined_re.search(line):
                result.join_seen = True
                result.join_secs = stamp
                break
    result.chunkprof = parse_chunkprof(result.stderr_lines)
    for line in reversed(result.stdout_text.splitlines()):
        line = line.strip()
        if line.startswith("{") and line.endswith("}"):
            try:
                result.client_summary = json.loads(line)
            except json.JSONDecodeError:
                continue
            break
    # The integrated client SIGTERMs its server on exit; the overlay lands
    # during the server's shutdown save, which can lag client exit slightly.
    save_deadline = time.monotonic() + save_timeout
    while time.monotonic() < save_deadline:
        try:
            size = overlay.stat().st_size
            if size > 0:
                result.overlay_exists = True
                result.overlay_bytes = size
                break
        except OSError:
            pass
        time.sleep(0.1)
    else:
        result.overlay_waited = True
    if not result.overlay_exists:
        try:
            result.overlay_exists = overlay.stat().st_size > 0
            result.overlay_bytes = overlay.stat().st_size
        except OSError:
            pass
    try:
        create_factory = (world_dir / "create_factory.bin").read_bytes()
        result.create_factory_bytes = len(create_factory)
        result.create_factory_valid = valid_create_factory(create_factory)
    except OSError:
        pass
    return result


def run_gpu_probe(gpu_bin, run_cwd, timeout, env_extra=None, game_dir=None):
    """`megu-client-gpu --headless-probe`: adapters enumerate and exit 0
    even on headless machines. Returns a SessionResult-shaped record."""
    result = SessionResult(label="gpu-probe", mode="gpu")
    cmd = [str(gpu_bin), "--headless-probe"]
    if game_dir is not None:
        cmd += ["--gameDir", str(game_dir)]
    result.cmd = cmd
    result.cwd = str(run_cwd)
    t0 = time.monotonic()
    try:
        proc = subprocess.run(
            cmd, cwd=str(run_cwd), env=_scrub_env(env_extra),
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True, timeout=timeout,
        )
        result.exit_code = proc.returncode
        result.stdout_text = proc.stdout
        result.stderr_lines = proc.stderr.splitlines()
    except subprocess.TimeoutExpired:
        result.session_timed_out = True
    except OSError as error:
        result.spawn_error = f"{error}"
    result.wall_secs = time.monotonic() - t0
    return result


# --------------------------------------------------------------------------
# Checks
# --------------------------------------------------------------------------

@dataclass
class CheckResult:
    cid: str
    title: str
    status: str = "pass"
    details: list = field(default_factory=list)
    metrics: dict = field(default_factory=dict)

    def fail(self, detail):
        self.status = "fail"
        self.details.append(detail)
        return self

    def note(self, detail):
        self.details.append(detail)
        return self

    def to_json(self):
        return {"id": self.cid, "title": self.title, "status": self.status,
                "details": self.details, "metrics": self.metrics}


def check_bootstrap(session, runtime_root):
    """C1 — clean bootstrap + relative-path resilience for one session."""
    check = CheckResult(
        cid="C1", title="Clean bootstrap & relative-path resilience")
    check.metrics["server_bin_arg"] = session.cmd[
        session.cmd.index("--server-bin") + 1] if "--server-bin" in session.cmd else None
    if session.spawn_error:
        return check.fail(f"client spawn failed: {session.spawn_error}")
    problems, info = inspect_world_dir(session.world_dir)
    check.metrics["world_dir_entries"] = info["entries"]
    check.metrics["datapacks_entries"] = info["datapacks_entries"]
    for problem in problems:
        check.fail(problem)
    for _, name, line in find_test_datapack_mentions(session.combined_lines()):
        check.fail(f"test datapack '{name}' in logs: {line.strip()}")
    if runtime_root is not None:
        check.note(f"runtime root: {runtime_root}")
        if not (runtime_root / "datapacks" / "vanilla").is_dir():
            check.note("runtime root has no generated datapacks/vanilla; "
                       "whole-dir fallback would link test packs")
    if not problems:
        check.note("world dir links resolve cleanly; only vanilla datapack present")
    return check


def check_join_budget(session, budget):
    """C2 — login readiness within the join-time budget + CHUNKPROF capture."""
    check = CheckResult(
        cid="C2", title="Login readiness & performance budget")
    check.metrics.update(session.chunkprof)
    check.metrics["join_budget_secs"] = budget
    if session.spawn_error:
        return check.fail(f"client spawn failed: {session.spawn_error}")
    if not session.join_seen:
        detail = "join marker '[spine] <user> joined' never appeared"
        if session.join_timed_out:
            detail += " (join wait timed out)"
        if session.exit_code not in (None, 0):
            detail += f" (client exit {session.exit_code})"
        return check.fail(detail)
    check.metrics["join_secs"] = session.join_secs
    if session.join_secs is not None and session.join_secs > budget:
        check.fail(f"join took {session.join_secs:.2f}s > {budget:.2f}s budget")
    else:
        check.note(f"joined in {session.join_secs:.2f}s (budget {budget:.2f}s)")
    lookups = session.chunkprof.get("band_lookups_per_chunk")
    if (lookups == 0.0
            and (session.chunkprof.get("chunks_delivered") or 0) > 0):
        check.note("CHUNKPROF band lookups=0 with chunks delivered "
                   "(pre-join stall signature)")
    return check


def check_stderr_gate(session):
    """C3 — forbidden runtime regressions anywhere in captured output."""
    check = CheckResult(cid="C3", title="Clean stderr gate")
    hits = find_forbidden(session.combined_lines())
    for _, needle, line in hits:
        check.fail(f"'{needle}': {line.strip()}")
    if session.session_timed_out:
        check.fail("session timed out and was killed")
    if session.exit_code not in (0, None):
        check.fail(f"client exited {session.exit_code}")
    if not hits and session.exit_code == 0:
        check.note(f"stderr clean ({len(session.stderr_lines)} lines), exit 0")
    return check


def check_persistence(first, restart):
    """C4 — world_overlay.bin written on disconnect + clean reload."""
    check = CheckResult(cid="C4", title="Persistence & restart round-trip")
    check.metrics["overlay_bytes_first"] = first.overlay_bytes
    if not first.overlay_exists:
        check.fail(f"{first.world_dir}/world_overlay.bin missing or empty "
                   "after disconnect")
    else:
        check.note(f"world_overlay.bin written ({first.overlay_bytes} bytes)")
    if restart.spawn_error:
        return check.fail(f"restart spawn failed: {restart.spawn_error}")
    if not restart.join_seen:
        check.fail("restart: join marker never appeared")
    else:
        check.note(f"rejoined in {restart.join_secs:.2f}s")
        check.metrics["rejoin_secs"] = restart.join_secs
    hits = find_forbidden(restart.combined_lines())
    for _, needle, line in hits:
        check.fail(f"restart stderr '{needle}': {line.strip()}")
    if restart.exit_code != 0:
        check.fail(f"restart client exit {restart.exit_code}")
    check.metrics["overlay_bytes_restart"] = restart.overlay_bytes
    if not restart.overlay_exists:
        check.fail("world_overlay.bin missing after restart session")
    return check


def check_mod_jar(session, jar_path):
    """C5 — synthetic mod JAR discovered by the live mod scan."""
    check = CheckResult(cid="C5", title="Synthetic mod JAR end-to-end")
    check.metrics["jar"] = str(jar_path)
    check.note(f"jar under test: {jar_path}")
    if session.spawn_error:
        return check.fail(f"client spawn failed: {session.spawn_error}")
    lines = session.combined_lines()
    if any("MEGU_MODS_DIR" in line and "not a directory" in line
           for line in lines):
        check.fail("server reports MEGU_MODS_DIR is not a directory")
    report = parse_live_mods_report(lines)
    check.metrics["live_mods_report"] = report
    if report is None:
        return check.fail(
            "no [live-mods] scan output — MEGU_MODS_DIR not forwarded or "
            "server built without mod-suite (mod JAR silently ignored)")
    if report.get("none_classified"):
        return check.fail(
            f"mods dir scanned but nothing classified: {report['line']}")
    if report["jars"] < 1:
        return check.fail(f"live scan saw {report['jars']} jars — "
                          "synthetic JAR silently ignored")
    if report["tier3"] > 0:
        check.fail(f"{report['tier3']} jar(s) fell back to tier3")
    if report["native_bundle"] < 1 and report["tier2_shim"] < 1 \
            and report["auto_skipped"] < 1:
        check.fail(f"live scan classified no jars: {report['line']}")
    named = MOD_JAR_NAME in "\n".join(lines) or MOD_ID in "\n".join(lines)
    if not named:
        check.note(f"mod id {MOD_ID} not named in logs; classification "
                   f"still reported {report['jars']} jar(s)")
    check.note(report["line"].strip())
    return check


def check_create_live(first, restart, create_jar, gpu_probe=None):
    """C6 — Create arms in the live integrated server and survives reload;
    require observed mobs and optionally check the GPU probe with this gameDir."""
    check = CheckResult(cid="C6", title="Create live runtime & mob spawn")
    check.metrics["create_jar"] = str(create_jar)
    check.note(f"Create-family JAR under test: {create_jar}")
    check.metrics["gpu_probe_with_modded_game_dir"] = gpu_probe is not None
    create_errors = re.compile(r"\b(?:ERROR|FATAL|PANIC)\b", re.IGNORECASE)
    for label, session in (("first", first), ("restart", restart)):
        lines = session.combined_lines()
        check.metrics[f"stderr_lines_{label}"] = len(session.stderr_lines)
        errors = [line for line in session.stderr_lines if create_errors.search(line)]
        check.metrics[f"stderr_errors_{label}"] = len(errors)
        if errors:
            check.fail(f"{label} boot has stderr error(s): " + " | ".join(errors))
        if session.spawn_error:
            check.fail(f"{label} client spawn failed: {session.spawn_error}")
        if session.exit_code != 0:
            check.fail(f"{label} client exit {session.exit_code}")
        if not session.join_seen:
            check.fail(f"{label} integrated session never joined")
        if session.session_timed_out:
            check.fail(f"{label} integrated session timed out")
        for _, needle, line in find_forbidden(lines):
            check.fail(f"{label} stderr '{needle}': {line.strip()}")
        overlay = Path(session.world_dir) / "world_overlay.bin"
        check.metrics[f"world_overlay_bytes_{label}"] = session.overlay_bytes
        if not session.overlay_exists or session.overlay_bytes <= 0 or not overlay.is_file():
            check.fail(f"{label} world_overlay.bin missing or empty after disconnect")
        check.metrics[f"create_factory_bytes_{label}"] = session.create_factory_bytes
        if not session.create_factory_valid:
            check.fail(f"{label} Create runtime sidecar missing or invalid; Create was not observably armed")
        mods_report = parse_live_mods_report(lines)
        check.metrics[f"live_mods_report_{label}"] = mods_report
        if mods_report is None or mods_report.get("jars", 0) < 1:
            check.fail(f"{label} server did not report scanning the gameDir mods directory")
        if (CREATE_E2E_JAR_NAME not in "\n".join(lines)
                and CREATE_E2E_MOD_ID not in "\n".join(lines)):
            check.fail(f"{label} live mod scan did not identify {CREATE_E2E_JAR_NAME}")
    mobs = first.chunkprof.get("mobs_last")
    check.metrics["mobs_last"] = mobs
    if mobs is None or mobs <= 0:
        check.fail(f"no live mobs observed in status output (mobs={mobs})")
    else:
        check.note("live mob count observed using MEGU_DEBUG_SEED_MOBS=1; this verifies entity spawn/reporting, not natural spawn selection")
        check.note("modded sessions run at least 120 client ticks to capture the periodic mob status line")
    if gpu_probe is not None:
        check.metrics["gpu_probe_exit_code"] = gpu_probe.exit_code
        check.metrics["gpu_probe_stderr_lines"] = len(gpu_probe.stderr_lines)
        if gpu_probe.spawn_error:
            check.fail(f"GPU probe spawn failed: {gpu_probe.spawn_error}")
        if gpu_probe.session_timed_out or gpu_probe.exit_code != 0:
            check.fail(f"GPU --headless-probe with modded --gameDir failed (exit {gpu_probe.exit_code})")
        gpu_errors = [line for line in gpu_probe.stderr_lines if create_errors.search(line)]
        if gpu_errors:
            check.fail("GPU probe stderr error(s): " + " | ".join(gpu_errors))
        if check.status == "pass":
            check.note("GPU headless probe accepted the modded gameDir and exited cleanly; this probe does not load rendered mod assets")
    else:
        check.note("GPU probe not run because --gpu-client-bin was not supplied")
    check.note("Create arm is evidenced by a valid MCF1/MCF2 sidecar on both saves; restart join with Create enabled verifies sidecar reload")
    return check


def check_gpu_probe(result):
    """G1 — megu-client-gpu --headless-probe exits cleanly."""
    check = CheckResult(cid="G1", title="GPU client headless probe")
    if result.spawn_error:
        return check.fail(f"gpu client spawn failed: {result.spawn_error}")
    if result.session_timed_out:
        return check.fail("gpu probe timed out")
    if result.exit_code != 0:
        check.fail(f"gpu probe exit {result.exit_code}")
    for _, needle, line in find_forbidden(result.stderr_lines):
        check.fail(f"'{needle}': {line.strip()}")
    if check.status == "pass":
        check.note("adapter probe exited 0")
    return check


# --------------------------------------------------------------------------
# Gate orchestration
# --------------------------------------------------------------------------

@dataclass
class Gate:
    checks: list = field(default_factory=list)
    sessions: list = field(default_factory=list)
    started_at: float = field(default_factory=time.time)

    def add(self, check):
        self.checks.append(check)
        return check

    @property
    def failed(self):
        return [c for c in self.checks if c.status == "fail"]

    def report(self, config):
        return {
            "tool": "live_playability_e2e",
            "schema": 1,
            "started_at": self.started_at,
            "config": config,
            "checks": [c.to_json() for c in self.checks],
            "sessions": [self._session_json(s) for s in self.sessions],
            "overall": "fail" if self.failed else "pass",
        }

    @staticmethod
    def _session_json(s):
        return {
            "label": s.label, "mode": s.mode, "cmd": s.cmd, "cwd": s.cwd,
            "exit_code": s.exit_code, "spawn_error": s.spawn_error,
            "join_seen": s.join_seen, "join_secs": s.join_secs,
            "session_timed_out": s.session_timed_out,
            "join_timed_out": s.join_timed_out,
            "wall_secs": round(s.wall_secs, 3),
            "chunkprof": s.chunkprof,
            "client_summary": s.client_summary,
            "stderr_tail": s.stderr_lines[-40:],
            "stderr_line_count": len(s.stderr_lines),
            "world_dir": s.world_dir,
            "overlay_exists": s.overlay_exists,
            "overlay_bytes": s.overlay_bytes,
            "create_factory_bytes": s.create_factory_bytes,
            "create_factory_valid": s.create_factory_valid,
        }

    def print_table(self, out):
        width = max((len(c.cid) for c in self.checks), default=2)
        for check in self.checks:
            status = "PASS" if check.status == "pass" else "FAIL"
            out.write(f"[{status}] {check.cid:<{width}}  {check.title}\n")
            for detail in check.details:
                out.write(f"       {' ' * width}  - {detail}\n")
        verdict = "PASS" if not self.failed else "FAIL"
        out.write(f"{verdict}: {len(self.checks) - len(self.failed)}/"
                  f"{len(self.checks)} checks passed\n")


def _autodetect(candidates):
    for candidate in candidates:
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate
    return None


def find_default_server_bin():
    return _autodetect(
        [ROOT / "target" / "release" / "megu",
         ROOT / "target" / "debug" / "megu"])


def find_default_client_bin():
    return _autodetect(
        [ROOT / "client-app" / "target" / "release" / "megu-client",
         ROOT / "client-app" / "target" / "debug" / "megu-client",
         ROOT / "target" / "release" / "megu-client",
         ROOT / "target" / "debug" / "megu-client"])


def find_default_gpu_bin():
    return _autodetect(
        [ROOT / "client-gpu" / "target" / "release" / "megu-client-gpu",
         ROOT / "client-gpu" / "target" / "debug" / "megu-client-gpu",
         ROOT / "target" / "release" / "megu-client-gpu",
         ROOT / "target" / "debug" / "megu-client-gpu"])


def _run_mode(gate, mode, base_dir, cfg):
    """Run the two legs (fresh launch + restart) for one mode."""
    mode_dir = Path(base_dir) / mode
    game_dir = mode_dir / "game"
    mods_dir = game_dir / "mods"
    if mode == "modded":
        jar = build_synthetic_mod_jar(mods_dir / MOD_JAR_NAME)
        create_jar = build_synthetic_create_jar(mods_dir / CREATE_E2E_JAR_NAME)
    else:
        jar = None
        create_jar = None

    rel_arg = relative_server_arg(cfg["server_bin"], cfg["run_cwd"])
    abs_arg = str(cfg["server_bin"])

    first = run_session(
        label=f"{mode}/launch-relative", mode=mode,
        client_bin=cfg["client_bin"], server_bin_arg=rel_arg,
        run_cwd=cfg["run_cwd"], game_dir=game_dir, world=cfg["world"],
        username=cfg["username"],
        ticks=max(cfg["ticks"], 120) if mode == "modded" else cfg["ticks"],
        view_distance=cfg["view_distance"], seed=cfg["seed"],
        join_timeout=cfg["join_timeout"], exit_timeout=cfg["exit_timeout"],
        save_timeout=cfg["save_timeout"],
        env_extra=({"MEGU_ENABLE_CREATE": "1",
                    "MEGU_DEBUG_SEED_MOBS": "1"} if mode == "modded" else None))
    gate.sessions.append(first)

    c1 = gate.add(check_bootstrap(first, cfg["runtime_root"]))
    c1.title += f" [{mode}]"
    c2 = gate.add(check_join_budget(first, cfg["max_join_secs"]))
    c2.title += f" [{mode}]"
    c3 = gate.add(check_stderr_gate(first))
    c3.title += f" [{mode}]"
    if mode == "modded":
        gate.add(check_mod_jar(first, jar))

    restart = run_session(
        label=f"{mode}/restart-absolute", mode=mode,
        client_bin=cfg["client_bin"], server_bin_arg=abs_arg,
        run_cwd=cfg["run_cwd"], game_dir=game_dir, world=cfg["world"],
        username=cfg["username"],
        ticks=max(cfg["ticks"], 120) if mode == "modded" else cfg["ticks"],
        view_distance=cfg["view_distance"], seed=cfg["seed"],
        join_timeout=cfg["join_timeout"], exit_timeout=cfg["exit_timeout"],
        save_timeout=cfg["save_timeout"],
        env_extra=({"MEGU_ENABLE_CREATE": "1",
                    "MEGU_DEBUG_SEED_MOBS": "1"} if mode == "modded" else None))
    gate.sessions.append(restart)
    c4 = gate.add(check_persistence(first, restart))
    c4.title += f" [{mode}]"
    c3r = gate.add(check_stderr_gate(restart))
    c3r.cid = "C3r"
    c3r.title += f" (restart) [{mode}]"
    if mode == "modded":
        gpu_probe = None
        if cfg["gpu_client_bin"] is not None:
            gpu_probe = run_gpu_probe(
                cfg["gpu_client_bin"], cfg["run_cwd"], timeout=60.0,
                game_dir=game_dir)
            gate.sessions.append(gpu_probe)
        gate.add(check_create_live(first, restart, create_jar, gpu_probe))


def parse_args(argv=None):
    parser = argparse.ArgumentParser(
        description="Live playability & mod integration E2E gate: boots the "
                    "real megu server + megu-client and fails closed on the "
                    "real-player failure modes unit tests missed.")
    parser.add_argument("--server-bin",
                        help="megu server binary; relative paths like "
                             "./target/release/megu are exercised verbatim "
                             "(auto-detected under target/{release,debug} "
                             "when omitted)")
    parser.add_argument("--client-bin",
                        help="megu-client binary (auto-detected under "
                             "client-app/target/{release,debug} when omitted)")
    parser.add_argument("--gpu-client-bin",
                        help="optional megu-client-gpu binary; runs "
                             "--headless-probe when given")
    parser.add_argument("--mode", choices=("vanilla", "modded", "all"),
                        default="vanilla",
                        help="vanilla = clean gameDir; modded = synthetic "
                             "JAR in <gameDir>/mods; all = both (default: vanilla)")
    parser.add_argument("--max-join-secs", type=float, default=None,
                        help="join-time budget in seconds "
                             "(default 8.0; --strict makes it 3.0)")
    parser.add_argument("--strict", action="store_true",
                        help="tighten the join budget to 3.0 s")
    parser.add_argument("--ticks", type=int, default=20,
                        help="headless session ticks after join (default 20)")
    parser.add_argument("--view-distance", type=int, default=4,
                        help="client --view-distance (default 4)")
    parser.add_argument("--seed", type=int, default=12026,
                        help="world seed for a new world (default 12026)")
    parser.add_argument("--username", default=USERNAME,
                        help=f"player name (default {USERNAME})")
    parser.add_argument("--world", default=WORLD_NAME,
                        help=f"world name (default {WORLD_NAME})")
    parser.add_argument("--join-timeout", type=float, default=None,
                        help="hard cap waiting for the join marker "
                             "(default max(30, 4x budget))")
    parser.add_argument("--exit-timeout", type=float, default=120.0,
                        help="cap for the session after join (default 120 s)")
    parser.add_argument("--save-timeout", type=float, default=20.0,
                        help="cap waiting for world_overlay.bin (default 20 s)")
    parser.add_argument("--run-cwd", default=None,
                        help="cwd for client launches; relative --server-bin "
                             "resolves from here (default: repo root)")
    parser.add_argument("--keep-tmp", action="store_true",
                        help="keep the temporary gameDir tree for inspection")
    parser.add_argument("--json", action="store_true",
                        help="emit the machine-readable report instead of "
                             "the summary table")
    return parser.parse_args(argv)


def main(argv=None):
    args = parse_args(argv)
    run_cwd = Path(args.run_cwd).resolve() if args.run_cwd else ROOT

    if args.server_bin:
        server_bin = resolve_arg_path(args.server_bin, run_cwd)
        if not server_bin.is_file():
            print(f"error: --server-bin '{args.server_bin}' does not resolve "
                  f"to a file from {run_cwd}", file=sys.stderr)
            return 2
    else:
        server_bin = find_default_server_bin()
        if server_bin is None:
            print("error: no --server-bin and no megu binary under "
                  "target/{release,debug}", file=sys.stderr)
            return 2

    if args.client_bin:
        client_bin = resolve_arg_path(args.client_bin, run_cwd)
        if not client_bin.is_file():
            print(f"error: --client-bin '{args.client_bin}' does not resolve "
                  f"to a file from {run_cwd}", file=sys.stderr)
            return 2
    else:
        client_bin = find_default_client_bin()
        if client_bin is None:
            print("error: no --client-bin and no megu-client under "
                  "client-app/target/{release,debug}", file=sys.stderr)
            return 2

    gpu_bin = None
    if args.gpu_client_bin:
        gpu_bin = resolve_arg_path(args.gpu_client_bin, run_cwd)
        if not gpu_bin.is_file():
            print(f"error: --gpu-client-bin '{args.gpu_client_bin}' does "
                  f"not resolve to a file from {run_cwd}", file=sys.stderr)
            return 2

    budget = args.max_join_secs
    if budget is None:
        budget = 3.0 if args.strict else 8.0
    join_timeout = args.join_timeout
    if join_timeout is None:
        join_timeout = max(30.0, budget * 4.0)

    runtime_root = runtime_root_for(server_bin, run_cwd)
    cfg = {
        "server_bin": server_bin,
        "client_bin": client_bin,
        "gpu_client_bin": gpu_bin,
        "run_cwd": run_cwd,
        "runtime_root": runtime_root,
        "max_join_secs": budget,
        "join_timeout": join_timeout,
        "exit_timeout": args.exit_timeout,
        "save_timeout": args.save_timeout,
        "ticks": args.ticks,
        "view_distance": args.view_distance,
        "seed": args.seed,
        "username": args.username,
        "world": args.world,
        "mode": args.mode,
    }
    # Normalize for JSON/report use.
    report_cfg = dict(cfg)
    report_cfg.update({
        "server_bin": str(server_bin),
        "client_bin": str(client_bin),
        "gpu_client_bin": str(gpu_bin) if gpu_bin else None,
        "run_cwd": str(run_cwd),
        "runtime_root": str(runtime_root) if runtime_root else None,
    })

    if runtime_root is None:
        print(f"warning: no megu runtime root (config_packets.bin + "
              f"datapacks/ + assets/block_states_1211.json) found near "
              f"{server_bin}; the integrated launch will likely fail",
              file=sys.stderr)

    gate = Gate()
    tmp = tempfile.TemporaryDirectory(prefix="megu-e2e-")
    base_dir = Path(tmp.name) if not args.keep_tmp else Path(
        tempfile.mkdtemp(prefix="megu-e2e-keep-"))
    try:
        modes = ("vanilla", "modded") if args.mode == "all" else (args.mode,)
        for mode in modes:
            _run_mode(gate, mode, base_dir, cfg)
        if gpu_bin is not None:
            probe = next((s for s in gate.sessions
                          if s.label == "gpu-probe" and s.mode == "gpu"), None)
            if probe is None:
                probe = run_gpu_probe(gpu_bin, run_cwd, timeout=60.0)
                gate.sessions.append(probe)
            gate.add(check_gpu_probe(probe))
    finally:
        if args.keep_tmp:
            print(f"kept temp tree: {base_dir}", file=sys.stderr)
        else:
            tmp.cleanup()

    if args.json:
        json.dump(gate.report(report_cfg), sys.stdout, indent=2)
        sys.stdout.write("\n")
    else:
        gate.print_table(sys.stdout)
    return 1 if gate.failed else 0


if __name__ == "__main__":
    sys.exit(main())
