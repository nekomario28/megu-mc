#!/usr/bin/env python3
"""Declarative Magic + VFX + Skeletal Animation + Combat Motion live E2E.

Clean-room wire check modeled on `tests/mod_suite_live_wire.py` and
`tests/multi_mod_e2e_wire.py` (Lane E2E-MAGIC-WIRE).  A synthetic Fabric JAR
carries an embedded `megu-bundle.json` — the member name
`auto_adapter::EMBEDDED_BUNDLE_MEMBER` installs verbatim as a
`megu-mod-bundle/1` document — declaring:

  * spells: an instant projectile, a cast-time AoE zone, and a channeled
    raycast beam, each delivering damage / apply_status / emit_vfx effects,
  * custom_statuses: a DoT burn and an absorption mana-shield,
  * rituals: one multiblock altar recipe (pattern + reagents + mana drain),
  * custom_channels: the four live magic wire channels (`megu:magic_v1`,
    `megu:vfx_v1`, `megu:anim_v1`, `megu:motion_v1`).

`--self-test` validates the fixtures offline, then runs
`megu-mod-check --adapt-mods-dir <mods> --out <dir> --json` three times:

  1. the magic JAR must adapt Tier 1 (`native_bundle`) with zero blockers
     and emit `<mod_id>.bundle.json` byte-equivalent to the embedded document;
  2. that emitted document re-embedded into a second JAR must adapt natively
     again and re-emit identically — the bundle JSON round-trips cleanly;
  3. two malformed magic JARs (a negative `mana_cost`, an unknown `schema`)
     must land in `tier3_fallback_required` with exact validation blockers —
     fail-closed, no crash, no emitted bundle files.

Default mode runs every `--self-test` check, then boots a real `megu`
binary built `--features mod-suite` against the adapted magic mods dir,
drives a Protocol 767 client through login -> configuration -> play, waits
for the spawn window and a 0x26 keep-alive, disconnects, and requires a
graceful exit 0 with the bundle registered in the boot log.

Standard library only; ephemeral ports; TemporaryDirectory everywhere.
`MC_PROBE_DEADLINE` is honored throughout.  Never touches rm -rf.
"""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time
import zipfile

from protocol767 import (
    Client, ContractFailure, Reader, ROOT, require, spawn_window, string,
)
from wire_smoke import port, status


BUNDLE_SCHEMA = "megu-mod-bundle/1"
EMBEDDED_BUNDLE_MEMBER = "megu-bundle.json"
ADAPTATION_REPORT_FILE = "adaptation-report.json"
REPORT_SCHEMA = "megu-mods-dir-adaptation/1"

MAGIC_MOD_ID = "starlight_magic"
MAGIC_JAR = "starlight-magic-1.0.jar"
ROUNDTRIP_JAR = "starlight-magic-roundtrip.jar"
BAD_MANA_JAR = "hexreaver-bad-mana.jar"
BAD_SCHEMA_JAR = "schemaforged-bad-schema.jar"

# The four declarative magic wire channels (MAG1/VFX1/ANM1/MOT1 payloads).
CUSTOM_CHANNELS = (
    "megu:magic_v1",
    "megu:vfx_v1",
    "megu:anim_v1",
    "megu:motion_v1",
)
CUSTOM_PAYLOAD_ID = 0x19
KEEP_ALIVE_ID = 0x26

CARGO_FLAGS = ["--offline", "--locked", "-j1"]
BUILD_TIMEOUT = 1800
# Server keep-alives ride a fixed 15 s cadence; the wait budget must clear it.
KEEPALIVE_BUDGET = 20.0


def _deadline():
    return float(os.environ.get("MC_PROBE_DEADLINE", "30"))


def fabric_manifest(mod_id):
    """Minimal valid `fabric.mod.json` declaring `mod_id` for all envs."""
    return json.dumps({
        "schemaVersion": 1,
        "id": mod_id,
        "version": "1.0.0",
        "environment": "*",
    })


def _sorted_pattern(blocks):
    """Ritual patterns emit in BTreeMap<(dx,dy,dz)> order; pre-sort so the
    fixture document is already in canonical emitted shape."""
    return sorted(blocks, key=lambda b: (b["dx"], b["dy"], b["dz"]))


def magic_bundle_document():
    """The `megu-mod-bundle/1` document embedded verbatim in the magic
    JAR — written in the canonical emitted shape (every validated field
    present, including defaulted ones) so the adapter's deterministic
    `bundle.to_json()` output compares equal to the embedded bytes."""
    return {
        "schema": BUNDLE_SCHEMA,
        "mod_id": MAGIC_MOD_ID,
        "mc_version_range": "[1.21,)",
        "portals": [],
        "progression_barriers": [],
        "biome_regions": [],
        "creatures": [],
        "custom_channels": list(CUSTOM_CHANNELS),
        "spells": [
            {
                "id": f"{MAGIC_MOD_ID}:spark_bolt",
                "mana_cost": 4.0,
                "base_cooldown_ticks": 20,
                "cast": {"kind": "instant"},
                "interrupt_on_damage": False,
                "interrupt_on_move": False,
                "steps": [{
                    "form": {"kind": "projectile", "speed": 1.5,
                             "gravity": 0.0, "max_ticks": 40},
                    "effects": [
                        {"kind": "damage", "school": f"{MAGIC_MOD_ID}:arcane",
                         "base_amount": 5.0},
                        {"kind": "emit_vfx",
                         "vfx_id": f"{MAGIC_MOD_ID}:spark_trail"},
                    ],
                }],
            },
            {
                "id": f"{MAGIC_MOD_ID}:ember_field",
                "mana_cost": 12.0,
                "base_cooldown_ticks": 120,
                "cast": {"kind": "cast_time", "total_ticks": 30},
                "interrupt_on_damage": True,
                "interrupt_on_move": True,
                "steps": [{
                    "form": {"kind": "aoe_zone", "radius": 5.0,
                             "duration_ticks": 100, "pulse_interval": 20},
                    "effects": [
                        {"kind": "damage", "school": f"{MAGIC_MOD_ID}:fire",
                         "base_amount": 2.0},
                        {"kind": "apply_status",
                         "status_id": f"{MAGIC_MOD_ID}:burning",
                         "amplifier": 1, "duration_ticks": 60},
                        {"kind": "emit_vfx",
                         "vfx_id": f"{MAGIC_MOD_ID}:ember_cloud"},
                    ],
                }],
            },
            {
                "id": f"{MAGIC_MOD_ID}:rift_lance",
                "mana_cost": 0.0,
                "base_cooldown_ticks": 200,
                "cast": {"kind": "channeled", "interval_ticks": 5,
                         "max_ticks": 60, "mana_per_pulse": 1.5},
                "interrupt_on_damage": True,
                "interrupt_on_move": False,
                "steps": [{
                    "form": {"kind": "raycast_beam", "range": 24.0,
                             "pierce_cap": 4},
                    "effects": [
                        {"kind": "damage", "school": f"{MAGIC_MOD_ID}:arcane",
                         "base_amount": 3.5},
                        {"kind": "emit_vfx",
                         "vfx_id": f"{MAGIC_MOD_ID}:rift_beam"},
                    ],
                }],
            },
        ],
        "custom_statuses": [
            {
                "id": f"{MAGIC_MOD_ID}:burning",
                "max_amplifier": 2,
                "pulse_interval_ticks": 20,
                "damage_per_pulse": 1.5,
                "heal_per_pulse": 0.0,
                "absorption_per_level": 0.0,
                "frozen": False,
                "rooted": False,
                "silenced": False,
                "disarmed": False,
                "attribute_modifiers": {},
                "vfx_aura": f"{MAGIC_MOD_ID}:burn_aura",
            },
            {
                "id": f"{MAGIC_MOD_ID}:mana_ward",
                "max_amplifier": 3,
                "pulse_interval_ticks": 0,
                "damage_per_pulse": 0.0,
                "heal_per_pulse": 0.0,
                "absorption_per_level": 4.0,
                "frozen": False,
                "rooted": False,
                "silenced": False,
                "disarmed": False,
                "attribute_modifiers": {
                    f"{MAGIC_MOD_ID}:knockback_resist": 0.5,
                },
                "vfx_aura": f"{MAGIC_MOD_ID}:ward_aura",
            },
        ],
        "rituals": [{
            "id": f"{MAGIC_MOD_ID}:ascension_altar",
            "pattern": _sorted_pattern([
                {"dx": 0, "dy": 0, "dz": 0, "block": f"{MAGIC_MOD_ID}:altar_core"},
                {"dx": -1, "dy": 0, "dz": 0, "block": "minecraft:crying_obsidian"},
                {"dx": 1, "dy": 0, "dz": 0, "block": "minecraft:crying_obsidian"},
                {"dx": 0, "dy": 0, "dz": -1, "block": "minecraft:crying_obsidian"},
                {"dx": 0, "dy": 0, "dz": 1, "block": "minecraft:crying_obsidian"},
                {"dx": -2, "dy": 0, "dz": -2, "block": "minecraft:gilded_blackstone"},
                {"dx": -2, "dy": 0, "dz": 2, "block": "minecraft:gilded_blackstone"},
                {"dx": 2, "dy": 0, "dz": -2, "block": "minecraft:gilded_blackstone"},
                {"dx": 2, "dy": 0, "dz": 2, "block": "minecraft:gilded_blackstone"},
            ]),
            "reagents": {
                f"{MAGIC_MOD_ID}:star_shard": 2,
                "minecraft:amethyst_shard": 4,
            },
            "duration_ticks": 200,
            "mana_per_tick": 0.75,
        }],
    }


def malformed_mana_document():
    """A bundle whose only defect is a negative `mana_cost` — the parser's
    `SpellValidationError::BadManaCost` must surface as a Tier 3 blocker."""
    return {
        "schema": BUNDLE_SCHEMA,
        "mod_id": "hexreaver",
        "mc_version_range": "[1.21,)",
        "portals": [],
        "progression_barriers": [],
        "biome_regions": [],
        "creatures": [],
        "custom_channels": [],
        "spells": [{
            "id": "hexreaver:cursed_bolt",
            "mana_cost": -9.5,
            "base_cooldown_ticks": 10,
            "cast": {"kind": "instant"},
            "steps": [{
                "form": {"kind": "self_cast"},
                "effects": [{"kind": "heal", "base_amount": 1.0}],
            }],
        }],
    }


def unknown_schema_document():
    """A bundle carrying a foreign schema tag — `UnknownSchema` must surface
    as a Tier 3 blocker, never a crash or a silent default."""
    return {
        "schema": "megu-mod-bundle/2",
        "mod_id": "schemaforged",
        "mc_version_range": "[1.21,)",
        "portals": [],
        "progression_barriers": [],
        "biome_regions": [],
        "creatures": [],
        "custom_channels": [],
    }


def write_jar(path, mod_id, bundle_doc):
    """One clean-room magic mod JAR: a loader manifest plus the embedded
    `megu-bundle.json` the adapter installs verbatim."""
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_STORED) as archive:
        archive.writestr("fabric.mod.json", fabric_manifest(mod_id))
        if bundle_doc is not None:
            archive.writestr(EMBEDDED_BUNDLE_MEMBER, json.dumps(bundle_doc))


def make_magic_fixture(mods):
    """Write the magic mod JAR into `mods`; returns the embedded document."""
    mods.mkdir()
    doc = magic_bundle_document()
    write_jar(mods / MAGIC_JAR, MAGIC_MOD_ID, doc)
    return doc


def make_malformed_fixture(mods):
    """Two JARs, each carrying an unusable embedded bundle: one a rejected
    spell parameter, one a foreign schema tag."""
    mods.mkdir()
    write_jar(mods / BAD_MANA_JAR, "hexreaver", malformed_mana_document())
    write_jar(mods / BAD_SCHEMA_JAR, "schemaforged", unknown_schema_document())


# ---------------------------------------------------------------------------
# Binary resolution — prebuilt paths win, cargo fills in the rest.
# ---------------------------------------------------------------------------

def _cargo_build(manifest, extra, target_dir, what):
    target_dir.mkdir(parents=True, exist_ok=True)
    build = subprocess.run(
        ["cargo", "build", "--manifest-path", str(manifest), *extra,
         "--target-dir", str(target_dir), *CARGO_FLAGS],
        cwd=ROOT, stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        timeout=BUILD_TIMEOUT, encoding="utf-8", errors="replace")
    require(build.returncode == 0,
            f"{what}-build-failed-{build.returncode}\n"
            + "\n".join(build.stdout.splitlines()[-25:]))
    return target_dir / "debug" / what


def resolve_mod_check(given):
    """`--mod-check-bin` wins; otherwise build the adapter CLI exactly the
    way the lane contract spells it out."""
    if given is not None:
        binary = Path(given).resolve()
    else:
        binary = _cargo_build(
            ROOT / "mod-adapter-core" / "Cargo.toml",
            ["--bin", "megu-mod-check"],
            ROOT / "target" / "mod-adapter-core",
            "megu-mod-check")
    require(binary.is_file(), f"mod-check-binary-missing: {binary}")
    require(os.access(binary, os.X_OK),
            f"mod-check-binary-not-executable: {binary}")
    return binary


def resolve_server(given):
    """`--server-bin` wins; otherwise build the mod-suite server binary the
    live boot needs (`MEGU_MODS_DIR` adaptation only exists behind that
    feature)."""
    if given is not None:
        binary = Path(given).resolve()
    else:
        binary = _cargo_build(
            ROOT / "Cargo.toml",
            ["--features", "mod-suite"],
            ROOT / "target" / "magic-e2e",
            "megu")
    require(binary.is_file(), f"compiled-binary-missing: {binary}")
    require(os.access(binary, os.X_OK),
            f"compiled-binary-not-executable: {binary}")
    return binary


def run_checked(argv, code, what):
    result = subprocess.run(
        [str(a) for a in argv], stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        timeout=120, encoding="utf-8", errors="replace")
    require(result.returncode == code,
            f"{what}-exit-{result.returncode}\n"
            f"stdout:\n{result.stdout[-2000:]}\nstderr:\n{result.stderr[-2000:]}")
    return result


def adapt_mods_dir(mod_check, mods, out, code, what):
    result = run_checked(
        [mod_check, "--adapt-mods-dir", mods, "--out", out, "--json"],
        code, what)
    try:
        return json.loads(result.stdout)
    except ValueError as error:
        raise ContractFailure(f"{what}-stdout-not-json: {error}") from error


def custom_payload(packet_id, body):
    require(packet_id == CUSTOM_PAYLOAD_ID,
            f"custom-payload-id: 0x{packet_id:02x}")
    reader = Reader(body)
    channel = reader.string()
    require(channel, "custom-payload-empty-channel")
    return channel, reader.take(len(body) - reader.pos)


def _jar_by_name(report, name):
    for jar in report["jars"]:
        if jar["jar"] == name:
            return jar
    raise ContractFailure(f"adapt-report-missing-jar: {name}")


def _check_report_shape(report, jars_found):
    require(report.get("schema") == REPORT_SCHEMA, "adapt-report-schema")
    require(report.get("jars_found") == jars_found, "adapt-report-jars-found")
    totals = report.get("totals", {})
    for tier in ("auto_skipped", "native_bundle", "tier2_lifted_shim",
                 "tier3_fallback_required"):
        require(tier in totals, f"adapt-report-totals-missing-{tier}")


# ---------------------------------------------------------------------------
# --self-test: offline fixture validation + three adapter runs
# ---------------------------------------------------------------------------

def self_test(mod_check, tmp):
    root = Path(tmp)

    # --- fixture sanity: real ZIP, manifest id, embedded canonical doc. ---
    mods = root / "mods"
    embedded = make_magic_fixture(mods)
    require(zipfile.is_zipfile(mods / MAGIC_JAR), "selftest-magic-not-jar")
    with zipfile.ZipFile(mods / MAGIC_JAR) as archive:
        require(archive.testzip() is None, "selftest-magic-jar-corrupt")
        require(json.loads(archive.read("fabric.mod.json"))["id"] == MAGIC_MOD_ID,
                "selftest-magic-manifest")
        require(json.loads(archive.read(EMBEDDED_BUNDLE_MEMBER)) == embedded
                and embedded["schema"] == BUNDLE_SCHEMA,
                "selftest-embedded-bundle")
    require({s["cast"]["kind"] for s in embedded["spells"]}
            == {"instant", "cast_time", "channeled"},
            "selftest-spell-cast-kinds")
    require({s["steps"][0]["form"]["kind"] for s in embedded["spells"]}
            == {"projectile", "aoe_zone", "raycast_beam"},
            "selftest-spell-form-kinds")
    require(embedded["custom_channels"] == list(CUSTOM_CHANNELS)
            and len(embedded["custom_statuses"]) == 2
            and len(embedded["rituals"]) == 1,
            "selftest-magic-sections")

    # The same 0x19 channel split the live client applies, exercised over a
    # magic channel frame.
    packet_channel, payload = custom_payload(
        CUSTOM_PAYLOAD_ID, string(CUSTOM_CHANNELS[0]) + b"MAG1body")
    require(packet_channel == CUSTOM_CHANNELS[0] and payload == b"MAG1body",
            "selftest-0x19-channel-split")

    # --- run 1: the magic JAR adapts Tier 1 with zero blockers, and the ---
    # --- emitted bundle preserves every magic section verbatim.        ---
    out = root / "adapted"
    report = adapt_mods_dir(mod_check, mods, out, 0, "adapt-magic")
    _check_report_shape(report, 1)
    require(report["verdict"] == "adapted", "adapt-magic-verdict")
    require(report["totals"]["native_bundle"] == 1
            and report["totals"]["tier3_fallback_required"] == 0,
            "adapt-magic-totals")
    jar = _jar_by_name(report, MAGIC_JAR)
    require(jar["tier"] == "native_bundle", "adapt-magic-tier")
    require(jar["blockers"] == [], "adapt-magic-blockers")
    require(jar["mod_id"] == MAGIC_MOD_ID, "adapt-magic-mod-id")
    emitted_path = out / f"{MAGIC_MOD_ID}.bundle.json"
    require(emitted_path.is_file(), "adapt-magic-bundle-file-missing")
    emitted = json.loads(emitted_path.read_text(encoding="utf-8"))
    require(emitted == embedded, "adapt-magic-bundle-roundtrip")
    # The report file mirrors the --json stdout document byte for byte.
    report_file = out / ADAPTATION_REPORT_FILE
    require(report_file.is_file(), "adapt-magic-report-file-missing")
    require(json.loads(report_file.read_text(encoding="utf-8")) == report,
            "adapt-magic-report-file-mismatch")

    # --- run 2: the emitted document re-embedded in a fresh JAR must   ---
    # --- adapt natively again and re-emit identically — a real         ---
    # --- bundle-JSON round-trip through the same fail-closed parser.   ---
    rt_mods = root / "roundtrip_mods"
    rt_mods.mkdir()
    rt_doc = json.loads(emitted_path.read_text(encoding="utf-8"))
    write_jar(rt_mods / ROUNDTRIP_JAR, MAGIC_MOD_ID, rt_doc)
    rt_out = root / "roundtrip_out"
    rt_report = adapt_mods_dir(mod_check, rt_mods, rt_out, 0, "adapt-roundtrip")
    _check_report_shape(rt_report, 1)
    require(rt_report["verdict"] == "adapted"
            and rt_report["totals"]["native_bundle"] == 1,
            "adapt-roundtrip-verdict")
    rt_emitted = json.loads(
        (rt_out / f"{MAGIC_MOD_ID}.bundle.json").read_text(encoding="utf-8"))
    require(rt_emitted == emitted, "adapt-roundtrip-deterministic")

    # --- run 3: malformed magic bundles fail closed into Tier 3 — the  ---
    # --- exact validation reason lands in blockers, nothing crashes,   ---
    # --- and no bundle file is emitted for a rejected JAR.             ---
    bad_mods = root / "bad_mods"
    make_malformed_fixture(bad_mods)
    bad_out = root / "bad_out"
    bad = adapt_mods_dir(mod_check, bad_mods, bad_out, 2, "adapt-malformed")
    _check_report_shape(bad, 2)
    require(bad["verdict"] == "requires_fallback", "adapt-malformed-verdict")
    require(bad["totals"]["tier3_fallback_required"] == 2
            and bad["totals"]["native_bundle"] == 0,
            "adapt-malformed-totals")
    mana = _jar_by_name(bad, BAD_MANA_JAR)
    require(mana["tier"] == "tier3_fallback_required", "adapt-mana-tier")
    require(any("failed megu-mod-bundle/1 validation" in b
                and "BadManaCost" in b for b in mana["blockers"]),
            "adapt-mana-blocker")
    schema = _jar_by_name(bad, BAD_SCHEMA_JAR)
    require(schema["tier"] == "tier3_fallback_required", "adapt-schema-tier")
    require(any("unknown bundle schema" in b for b in schema["blockers"]),
            "adapt-schema-blocker")
    require(list(bad_out.glob("*.bundle.json")) == [],
            "adapt-malformed-emitted-bundle")

    print("PASS offline synthetic magic JAR + 0x19 channel split", flush=True)
    print("PASS adapt-mods-dir: Tier 1 native_bundle, 0 blockers, emitted "
          "bundle preserves spells/custom_statuses/rituals/custom_channels",
          flush=True)
    print("PASS bundle JSON round-trip: re-embedded emitted document adapts "
          "natively and re-emits identically", flush=True)
    print("PASS fail-closed: invalid mana_cost + unknown schema land in "
          "tier3_fallback_required (exit 2, no bundle emitted)", flush=True)


# ---------------------------------------------------------------------------
# Live mode: Protocol 767 boot + login + keep-alive + graceful shutdown
# ---------------------------------------------------------------------------

class MagicClient(Client):
    """Capture play-phase custom payloads and keep-alive frames."""

    def __init__(self, game):
        super().__init__(game, "Alice")
        self.payloads = {}
        self.keepalives = []

    def read(self, timeout):
        packet_id, body = super().read(timeout)
        if self.phase == "play":
            if packet_id == CUSTOM_PAYLOAD_ID:
                channel, payload = custom_payload(packet_id, body)
                self.payloads.setdefault(channel, []).append(payload)
            elif packet_id == KEEP_ALIVE_ID:
                self.keepalives.append(body)
        return packet_id, body


def await_keepalive(client, budget):
    """Read until one server keep-alive (0x26, auto-echoed by the client's
    reader) has arrived; the cadence is a fixed 15 s, so the budget must
    clear that interval."""
    deadline = time.monotonic() + max(budget, KEEPALIVE_BUDGET)
    while not client.keepalives:
        remaining = deadline - time.monotonic()
        require(remaining > 0, "live-keep-alive-timeout")
        try:
            client.read(min(remaining, 1.0))
        except TimeoutError:
            continue
    require(len(client.keepalives) >= 1, "live-keep-alive-count")


def log_tail(path):
    return path.read_text(errors="replace")[-6500:] if path.exists() else "(no log)"


def run_live(binary, tmp):
    root = Path(tmp)
    live = root / "live"
    live.mkdir()
    mods = live / "mods"
    make_magic_fixture(mods)
    for name in ("config_packets.bin", "assets"):
        source = ROOT / name
        require(source.exists(), f"server-fixture-missing: {source}")
        (live / name).symlink_to(source, target_is_directory=source.is_dir())

    game = port()
    env = os.environ.copy()
    for key in ("MEGU_RCON_PORT", "MEGU_RCON_PASS",
                "MEGU_ALLOW_PLAYER_ADMIN_COMMANDS", "MEGU_PROGRESSION",
                "MEGU_MOD_SYNC_MIRRORS"):
        env.pop(key, None)
    env.update(
        MEGU_PORT=str(game), MEGU_BIND_ADDR="127.0.0.1",
        MEGU_MODS_DIR=str(mods),
        MEGU_WG="old", MEGU_ENABLE_QUERY="0",
        MEGU_ANVIL_WORLD_DIR="", MEGU_RESOURCE_PACK_URL="",
        MEGU_ANTICHEAT="off",
    )
    env.pop("MEGU_MOD_SYNC", None)
    log_path = live / "server.log"
    try:
        with log_path.open("w") as output:
            proc = subprocess.Popen([str(binary)], cwd=live, env=env,
                                    stdin=subprocess.DEVNULL, stdout=output,
                                    stderr=subprocess.STDOUT)
            try:
                deadline = time.monotonic() + _deadline()
                while time.monotonic() < deadline:
                    require(proc.poll() is None,
                            f"server-exited-during-boot: {proc.returncode}")
                    try:
                        status(game)
                        break
                    except (OSError, AssertionError):
                        time.sleep(0.1)
                else:
                    raise ContractFailure("server-listener-start-timeout")

                with MagicClient(game) as alice:
                    alice.login()
                    print("PASS Protocol 767 Alice login -> configuration "
                          "-> play (magic bundle mods dir)", flush=True)
                    alice.until_chunks(spawn_window(), "live-spawn-chunks")
                    print(f"PASS spawn chunk window "
                          f"({len(alice.chunks)} chunks delivered)", flush=True)
                    await_keepalive(alice, _deadline())
                    print("PASS server 0x26 keep-alive answered (client "
                          "echoed 0x18)", flush=True)
            finally:
                if proc.poll() is None:
                    proc.terminate()
                try:
                    proc.wait(timeout=_deadline())
                except subprocess.TimeoutExpired as exc:
                    proc.kill()
                    proc.wait(timeout=5)
                    raise ContractFailure("server-graceful-shutdown-timeout") from exc

        logs = log_path.read_text(errors="replace")
        require(proc.returncode == 0,
                f"server-graceful-shutdown-exit: {proc.returncode}")

        # The declarative magic JAR adapts Tier 1 at boot, its embedded
        # bundle registers into the unified runtime with all four magic
        # channels declared, and the dual loader plans it as one unified mod.
        require("[live-mods] 1 jar(s): 0 auto-skipped, 1 native-bundle, "
                "0 tier2-shim, 0 tier3" in logs,
                "live-magic-boot-classification")
        require(f"[live-mods] native bundle {MAGIC_MOD_ID} ({MAGIC_JAR}): "
                "0 portals, 0 progression barriers, 0 regions, 0 creatures, "
                "4 channels" in logs, "live-magic-bundle-registered")
        require("[live-mods] dual-loader: 1 unified, 0 client-only, "
                "0 superseded, 0 rejected, 0 duplicate ids, "
                "0 missing deps, 0 verified damage hooks" in logs,
                "live-magic-dual-loader-plan")
        require(re.search(r"\[live-mods\] rejected \S", logs) is None
                and re.search(r"\[live-mods\] bundle \S+ rejected", logs) is None,
                "live-magic-unexpected-rejection")
        require(re.search(r"\[live-mods\] \S+ \(\S+\) tier3:", logs) is None,
                "live-magic-unexpected-tier3")
        require(not re.search(
            r"(?i)panicked at|thread .* panicked|stack backtrace", logs),
            "server-panic-in-logs")
        require("culinary scan skipped" not in logs
                and "mixin scan failed closed" not in logs,
                "live-magic-jar-scan-failed")
        print("PASS live boot: declarative magic JAR adapted Tier 1; "
              "starlight_magic bundle registered (4 magic channels)", flush=True)
        print("PASS graceful shutdown (exit 0, no panics, disposable files "
              "cleaned)", flush=True)
    except BaseException:
        print("SERVER STDOUT/STDERR (tail)\n" + log_tail(log_path),
              file=sys.stderr)
        raise


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--self-test", action="store_true",
                        help="run only the offline fixture + adapter checks "
                             "(no server boot)")
    parser.add_argument("--server-bin", type=Path, default=None,
                        help="prebuilt megu server binary built with "
                             "--features mod-suite (skips the cargo build)")
    parser.add_argument("--mod-check-bin", type=Path, default=None,
                        help="prebuilt megu-mod-check binary "
                             "(skips the cargo build)")
    parser.add_argument("--keep-tmp", action="store_true",
                        help="keep the temporary working directory and "
                             "print its path")
    args = parser.parse_args()

    mod_check = resolve_mod_check(args.mod_check_bin)
    tmp = tempfile.TemporaryDirectory(
        prefix="megu-magic-wire-", delete=not args.keep_tmp)
    try:
        self_test(mod_check, tmp.name)
        if not args.self_test:
            binary = resolve_server(args.server_bin)
            run_live(binary, tmp.name)
            print("PASS live wire: magic bundle loaded at boot, Protocol 767 "
                  "play session, keep-alive, clean shutdown", flush=True)
    finally:
        if args.keep_tmp:
            print(f"kept tmp dir: {tmp.name}", flush=True)
        else:
            tmp.cleanup()


if __name__ == "__main__":
    main()
