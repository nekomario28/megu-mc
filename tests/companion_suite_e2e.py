#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
"""End-to-end companion suite smoke test.

Proves the five pure-Rust companion CLI binaries and the multi-binary
release bundler work together through the complete operator lifecycle:

  1. mc-spine-init      (server-files)  — the flag-mode bootstrap
                        (--dir/--mc-version 26.3/--accept-eula/--json)
                        fails closed on the EULA, then the legacy
                        init/doctor/op/whitelist/ban/claims subcommands
                        (re-exec'd to a sibling mc-spine-admin): doctor
                        gating on the EULA (rcon.password never printed),
                        offline admin, and claims with eula.txt
                        byte-unchanged throughout.
  2. mc-spine-mod-check (mod-adapter-core) — pre-flight a real ZIP_DEFLATED
                        .mrpack; its --json report must match
                        tools/mod_launcher.py --modpack byte-for-byte, and a
                        mismatched --mc-version must exit 2. Then
                        --scaffold-native emits a validated
                        mc-spine-mod-bundle/1 starter bundle, failing
                        closed on a malformed mod id.
  3. mc-spine-world-migrate (state-store) — migrate a 1.21.1 (DataVersion
                        3955) level.dat-style JSON document + chunk palette
                        to 26.3 (5023), refuse an unapproved downgrade
                        (exit 2), and restore 3955 with --allow-downgrade.
  4. mc-spine-client    (client-app)    — --ping a mock status server
                        speaking Protocol 777 (26.3): handshake, status
                        response JSON, ping/pong RTT.
  5. mc-spine-voice-relay (proto)       — --self-test, a --bench-packets
                        200 --json routing benchmark (routed == 200,
                        packets_per_second > 0), and a public --bind
                        refused with exit 2 without --allow-public-bind.
  6. tools/release_bundle.py            — build a --binary + 5x
                        --extra-binary release bundle twice, prove the
                        .tar.gz is byte-identical, and pass verify.

Usage: python3 tests/companion_suite_e2e.py [--self-test]
                                            [--target-dir DIR]
                                            [--init-bin PATH]
                                            [--mod-check-bin PATH]
                                            [--world-migrate-bin PATH]
                                            [--client-bin PATH]
                                            [--voice-relay-bin PATH]

`--target-dir` (default: $CARGO_TARGET_DIR, else repo target/companion-e2e)
receives the cargo build output for whichever binaries are not handed in
prebuilt; every cargo invocation sets CARGO_TARGET_DIR so no stray per-crate
target/ directories are left behind. `--self-test` runs the offline checks
(no cargo, no binaries) and exits.
"""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tarfile
import tempfile
import threading
import zipfile

ROOT = Path(__file__).resolve().parent.parent
RELEASE_BUNDLE = ROOT / "tools" / "release_bundle.py"
MOD_LAUNCHER = ROOT / "tools" / "mod_launcher.py"

# (key, crate directory, cargo --bin name, CLI flag attribute)
BINARIES = (
    ("init", "server-files", "mc-spine-init", "init_bin"),
    ("mod_check", "mod-adapter-core", "mc-spine-mod-check", "mod_check_bin"),
    ("world_migrate", "state-store", "mc-spine-world-migrate", "world_migrate_bin"),
    ("client", "client-app", "mc-spine-client", "client_bin"),
    ("voice_relay", "proto", "mc-spine-voice-relay", "voice_relay_bin"),
)

# Extra bins a companion's crate must leave in the same target dir without
# being stepped individually: mc-spine-init's legacy subcommands (init,
# doctor, op, whitelist, ban, claims) re-exec a sibling mc-spine-admin, so
# building init alone would leave step 1 delegating to a missing binary.
EXTRA_BINS = {
    "init": ("mc-spine-admin",),
}

CARGO_FLAGS = ["--offline", "--locked", "-j1"]
BUILD_TIMEOUT = 1800
CMD_TIMEOUT = 60

# Planted in server.properties to prove doctor never prints rcon.password.
RCON_SECRET = "e2e-rcon-s3cr3t-not-for-print"

# Fixed source sha recorded into both release bundles; what matters for this
# test is that the two builds record the SAME sha, not provenance.
BUNDLE_GIT_SHA = "0000000000000000000000000000000000000001"
BUNDLE_VERSION = "0.1.0"
BUNDLE_TARGET = "x86_64-unknown-linux-gnu"
BUNDLE_PREFIX = f"mc-spine-{BUNDLE_VERSION}-{BUNDLE_TARGET}"


class E2EFailure(AssertionError):
    pass


def check(condition, message):
    if not condition:
        raise E2EFailure(message)


def log(message):
    print(f"[companion-e2e] {message}", flush=True)


def tail(text, lines=25):
    return "\n".join((text or "").splitlines()[-lines:])


def run(argv, timeout=CMD_TIMEOUT, cwd=None, env=None):
    return subprocess.run(
        [str(a) for a in argv],
        cwd=cwd, env=env, stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        timeout=timeout, check=False,
        encoding="utf-8", errors="replace")


def expect(argv, code, timeout=CMD_TIMEOUT, cwd=None, what=None):
    """Run argv and require exit `code`; returns the CompletedProcess."""
    result = run(argv, timeout=timeout, cwd=cwd)
    label = what or " ".join(str(a) for a in argv[:2])
    check(result.returncode == code,
          f"{label}: expected exit {code}, got {result.returncode}\n"
          f"stdout:\n{tail(result.stdout)}\nstderr:\n{tail(result.stderr)}")
    return result


def parse_json_stdout(result, what):
    try:
        return json.loads(result.stdout)
    except ValueError as error:
        raise E2EFailure(
            f"{what}: stdout is not a JSON document: {error}\n"
            f"{tail(result.stdout)}") from error


# ------------------------------------------------------------------ build

def ensure_binaries(args):
    """Resolve the five companion binaries: prebuilt paths win, the rest are
    cargo-built into --target-dir so no per-crate target/ tree appears."""
    target_dir = Path(args.target_dir or
                      os.environ.get("CARGO_TARGET_DIR",
                                     ROOT / "target" / "companion-e2e"))
    target_dir = target_dir.resolve()
    resolved = {}
    missing = []
    for key, crate, name, attr in BINARIES:
        prebuilt = getattr(args, attr)
        if prebuilt is not None:
            path = Path(prebuilt)
            check(path.is_file(), f"--{attr.replace('_', '-')}: {path} is not a file")
            check(os.access(path, os.X_OK),
                  f"--{attr.replace('_', '-')}: {path} is not executable")
            resolved[key] = path.resolve()
        else:
            missing.append((key, crate, name))
    if missing:
        target_dir.mkdir(parents=True, exist_ok=True)
        env = dict(os.environ)
        env["CARGO_TARGET_DIR"] = str(target_dir)
        for key, crate, name in missing:
            names = (name, *EXTRA_BINS.get(key, ()))
            log(f"building {' + '.join(names)} ({crate})")
            build = run(["cargo", "build",
                         "--manifest-path", ROOT / crate / "Cargo.toml",
                         *(flag for n in names for flag in ("--bin", n)),
                         *CARGO_FLAGS],
                        timeout=BUILD_TIMEOUT, cwd=ROOT, env=env)
            check(build.returncode == 0,
                  f"cargo build {' + '.join(names)} exited "
                  f"{build.returncode}\n"
                  f"{tail(build.stdout + build.stderr)}")
            for built in names:
                artifact = target_dir / "debug" / built
                check(artifact.is_file(), f"build did not produce {artifact}")
            resolved[key] = target_dir / "debug" / name
        # The contract: every crate's artifacts — the binaries' crates and
        # their path dependencies alike — live under target_dir, never in a
        # stray <crate>/target directory a later run could accidentally reuse.
        for manifest in ROOT.glob("*/Cargo.toml"):
            crate = manifest.parent
            check(not (crate / "target").is_dir(),
                  f"stray crate target dir left behind: {crate.name}/target")
    return resolved


# ------------------------------------------------------- step 1: mc-spine-init

def step_init(bins, work):
    log("step 1/6: mc-spine-init server bootstrap + offline admin")
    binary = bins["init"]
    srv = work / "server"

    # The flag-mode bootstrap is fail-closed on the EULA: eula.txt is only
    # written eula=true when --accept-eula is passed, and an existing
    # eula=false is never upgraded without --force.
    boot = work / "bootstrap"
    refused = expect([binary, "--dir", boot, "--mc-version", "26.3",
                      "--json"], 0)
    report = parse_json_stdout(refused, "init --json without --accept-eula")
    check(report.get("eula_accepted") is False,
          "init must not report eula_accepted without --accept-eula")
    check(report.get("mc_version") == "26.3",
          f"init --json mc_version is {report.get('mc_version')!r}")
    check((boot / "world").is_dir() and (boot / "mods").is_dir(),
          "init did not create the world/mods directories")
    eula_path = boot / "eula.txt"
    check("eula=false" in eula_path.read_text(encoding="utf-8"),
          "init wrote eula=true without --accept-eula")

    kept = expect([binary, "--dir", boot, "--mc-version", "26.3",
                   "--accept-eula", "--json"], 0)
    check(parse_json_stdout(kept, "init --accept-eula without --force")
          .get("eula_accepted") is False,
          "init flipped an existing eula=false without --force")
    accepted = expect([binary, "--dir", boot, "--mc-version", "26.3",
                       "--accept-eula", "--force", "--json"], 0)
    check(parse_json_stdout(accepted, "init --accept-eula --force")
          .get("eula_accepted") is True,
          "init --accept-eula --force did not report eula_accepted")
    check("eula=true" in eula_path.read_text(encoding="utf-8"),
          "init --accept-eula --force did not write eula=true")
    check('mc_version = "26.3"'
          in (boot / "server.toml").read_text(encoding="utf-8"),
          "init did not record mc_version 26.3 in server.toml")

    # The legacy subcommands re-exec the sibling mc-spine-admin binary.
    expect([binary, "init", "--dir", srv,
            "--mc-version", "1.21.1",
            "--port", "25565",
            "--motd", "Alpha Test"], 0)
    props_path = srv / "server.properties"
    props = props_path.read_text(encoding="utf-8")
    check("server-port=25565" in props, "init did not write server-port=25565")
    check("motd=Alpha Test" in props, "init did not write the motd")
    eula = srv / "eula.txt"
    check("eula=false" in eula.read_text(encoding="utf-8"),
          "init must never pre-accept the EULA")

    # Plant a secret: doctor must prove the directory is healthy while never
    # printing rcon.password, in text or JSON.
    with props_path.open("a", encoding="utf-8") as stream:
        stream.write(f"enable-rcon=true\nrcon.password={RCON_SECRET}\n")

    refused = expect([binary, "doctor", "--dir", srv], 2)
    check(RCON_SECRET not in refused.stdout + refused.stderr,
          "doctor (eula gate) leaked rcon.password")
    check("eula" in refused.stdout.lower(),
          "doctor exit 2 did not explain the EULA action to take")

    eula.write_text(
        eula.read_text(encoding="utf-8").replace("eula=false", "eula=true"),
        encoding="utf-8")

    ready = expect([binary, "doctor", "--dir", srv, "--json"], 0)
    check(RCON_SECRET not in ready.stdout + ready.stderr,
          "doctor --json leaked rcon.password")
    report = parse_json_stdout(ready, "doctor --json")
    check(report.get("ready") is True, "doctor --json ready is not true")
    props_report = report.get("files", {}).get("server.properties", {})
    check(props_report.get("rcon_password_set") is True,
          "doctor --json should report rcon_password_set (not the password)")

    expect([binary, "op", "--dir", srv, "Alice", "--level", "4"], 0)
    expect([binary, "whitelist", "--dir", srv, "on"], 0)
    expect([binary, "whitelist", "--dir", srv, "add", "Alice"], 0)
    expect([binary, "ban", "--dir", srv, "Griefer", "--reason", "test"], 0)

    ops = json.loads((srv / "ops.json").read_text(encoding="utf-8"))
    check(any(e.get("name") == "Alice" and e.get("level") == 4 for e in ops),
          "ops.json lacks Alice at level 4")
    whitelist = json.loads((srv / "whitelist.json").read_text(encoding="utf-8"))
    check(any(e.get("name") == "Alice" for e in whitelist),
          "whitelist.json lacks Alice")
    bans = json.loads((srv / "banned-players.json").read_text(encoding="utf-8"))
    check(any(e.get("name") == "Griefer" and e.get("reason") == "test"
              for e in bans),
          "banned-players.json lacks Griefer (reason 'test')")
    props = props_path.read_text(encoding="utf-8")
    check("white-list=true" in props, "whitelist on did not set white-list=true")

    # Offline OpenPAC/FTB-Chunks administration: claims live in
    # openpac_claims.json / openpac_parties.json beside the admin files, so
    # eula.txt must stay byte-unchanged through the whole sequence.
    eula_bytes = eula.read_bytes()
    expect([binary, "claims", "--dir", srv, "server-claim",
            "overworld", "3", "-2", "--forceload"], 0)
    listing = expect([binary, "claims", "--dir", srv, "list", "--json"], 0)
    report = parse_json_stdout(listing, "claims list --json")
    check(report.get("claim_count") == 1,
          f"claims list reports {report.get('claim_count')} claims, not 1")
    check(report.get("forceload_count") == 1,
          "claims list reports no forceloaded claim")
    rows = report.get("claims") or []
    check(any(row.get("dimension") == "minecraft:overworld"
              and row.get("cx") == 3 and row.get("cz") == -2
              and row.get("owner") == "server"
              and row.get("forceloaded") is True
              for row in rows),
          f"claims list lost the forceloaded server claim or its "
          f"negative cz: {rows}")
    removed = expect([binary, "claims", "--dir", srv, "unclaim",
                      "overworld", "3", "-2", "--json"], 0)
    check(parse_json_stdout(removed, "claims unclaim --json").get("removed")
          is True, "claims unclaim did not confirm removal")
    gone = expect([binary, "claims", "--dir", srv, "list", "--json"], 0)
    check(parse_json_stdout(gone, "claims list after unclaim")
          .get("claim_count") == 0,
          "unclaimed chunk still listed")
    check(eula.read_bytes() == eula_bytes,
          "claims administration modified eula.txt")

    final = expect([binary, "doctor", "--dir", srv, "--json"], 0)
    check(parse_json_stdout(final, "final doctor").get("ready") is True,
          "admin-edited directory is not doctor-ready")
    log("  init/doctor/op/whitelist/ban/claims lifecycle verified")


# --------------------------------------------- step 2: mc-spine-mod-check

def mrpack_manifest():
    """A 1.21.1 NeoForge Modrinth index: a flagship native adapter mod, three
    built-in-superseded performance mods, and one client-side-only mod."""
    def jar(path, sha1, client="required", server="required", size=4096):
        return {
            "path": path,
            "hashes": {"sha1": sha1,
                       "sha512": hashlib.sha512(path.encode()).hexdigest()},
            "env": {"client": client, "server": server},
            "downloads": [f"https://example.invalid/{path.rsplit('/', 1)[-1]}"],
            "fileSize": size,
        }
    return {
        "formatVersion": 1,
        "game": "minecraft",
        "name": "E2E Operator Pack",
        "versionId": "1.0.0",
        "summary": "companion suite smoke-test pack",
        "dependencies": {"minecraft": "1.21.1", "neoforge": "21.1.215"},
        "files": [
            jar("mods/create-1.21.1-6.0.4.jar", "0" * 39 + "a", size=15_000_000),
            jar("mods/sodium-fabric-0.6.13+mc1.21.1.jar", "1" * 40,
                size=2_000_000),
            jar("mods/lithium-neoforge-0.15.0+mc1.21.1.jar", "2" * 40,
                size=1_000_000),
            jar("mods/DistantHorizons-2.3.2-b-1.21.1-fabric-neoforge.jar",
                "3" * 40, size=8_000_000),
            jar("mods/iris-neoforge-1.8.0-beta.4+1.21.1.jar", "4" * 40,
                server="unsupported", size=4_000_000),
        ],
    }


def build_mrpack(path):
    """Write `path` as a real ZIP_DEFLATED .mrpack archive."""
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("modrinth.index.json",
                         json.dumps(mrpack_manifest(), indent=2))


def step_mod_check(bins, work):
    log("step 2/6: mc-spine-mod-check .mrpack pre-flight (parity with mod_launcher.py)")
    binary = bins["mod_check"]
    pack = work / "e2e-pack.mrpack"
    build_mrpack(pack)

    rust = expect([binary, "--modpack", pack, "--mc-version", "1.21.1", "--json"], 0)
    python = expect([sys.executable, MOD_LAUNCHER,
                     "--modpack", pack, "--mc-version", "1.21.1", "--json"], 0)
    check(rust.stdout.encode() == python.stdout.encode(),
          "mc-spine-mod-check --json differs from tools/mod_launcher.py --json\n"
          f"rust:\n{tail(rust.stdout, 60)}\npython:\n{tail(python.stdout, 60)}")
    report = parse_json_stdout(rust, "mod-check --json")
    check(report.get("readiness") == "ready_with_auto_skips",
          f"unexpected readiness {report.get('readiness')!r}")
    check(report.get("native_mods") == ["create"],
          f"flagship native mods wrong: {report.get('native_mods')}")
    check(report.get("skipped_perf_mods") ==
          ["distanthorizons", "lithium", "sodium"],
          f"superseded perf mods wrong: {report.get('skipped_perf_mods')}")
    check(report.get("client_only_mods") == ["iris"],
          f"client-only mods wrong: {report.get('client_only_mods')}")

    rust_bad = expect([binary, "--modpack", pack, "--mc-version", "26.3", "--json"], 2)
    check(parse_json_stdout(rust_bad, "mod-check mismatch report").get("readiness")
          == "version_mismatch",
          "--mc-version 26.3 did not yield version_mismatch")
    python_bad = expect([sys.executable, MOD_LAUNCHER,
                         "--modpack", pack, "--mc-version", "26.3", "--json"], 2)
    check(rust_bad.stdout.encode() == python_bad.stdout.encode(),
          "version-mismatch reports differ between Rust and Python")

    # --scaffold-native emits a validated mc-spine-mod-bundle/1 starter for
    # native zero-JVM mods; a malformed mod id fails closed with exit 2.
    bundle_path = work / "demo_mod.bundle.json"
    expect([binary, "--scaffold-native", "demo_mod", "--out", bundle_path], 0)
    bundle = json.loads(bundle_path.read_text(encoding="utf-8"))
    check(bundle.get("schema") == "mc-spine-mod-bundle/1",
          f"scaffold bundle schema is {bundle.get('schema')!r}")
    check(bundle.get("mod_id") == "demo_mod",
          "scaffold bundle lost the requested mod id")
    check(set(bundle) == {"schema", "mod_id", "mc_version_range", "portals",
                          "progression_barriers", "biome_regions",
                          "creatures", "custom_channels"},
          f"scaffold bundle is not the closed v1 shape: {sorted(bundle)}")
    check(all(bundle[key] == [] for key in
              ("portals", "progression_barriers", "biome_regions",
               "creatures", "custom_channels")),
          "scaffold bundle should emit empty section lists")
    expect([binary, "--scaffold-native", "demo_mod:namespaced"], 2)
    log("  .mrpack report matches mod_launcher.py byte-for-byte; mismatch "
        "exits 2; --scaffold-native bundle validated")


# ------------------------------------------- step 3: mc-spine-world-migrate

def world_metadata_3955():
    """A 1.21.1 (DataVersion 3955) level.dat-style export: version metadata,
    a chunk-section palette mixing vanilla and modded namespaces, an item
    stack with vanilla + modded components, and a scalar pack_format."""
    return json.dumps({
        "DataVersion": 3955,
        "Data": {
            "DataVersion": 3955,
            "Version": {"Id": 3955, "Name": "1.21.1", "Snapshot": False},
            "RandomSeed": 8675309,
            "LevelName": "e2e-world",
        },
        "sections": [{
            "y": 0,
            "block_states": {
                "palette": [
                    {"id": "minecraft:grass",
                     "properties": {"snowy": "false"}},
                    {"Name": "minecraft:snow"},
                    {"id": "create:brass_casing"},
                ],
            },
            "biomes": {"palette": ["minecraft:plains", "aether:skyroot_meadow"]},
        }],
        "inventory": [{
            "id": "minecraft:boat",
            "count": 1,
            "components": {
                "minecraft:enchantments": {"minecraft:sharpness": 5},
                "create:schematic": {"progress": 4},
            },
        }],
        "pack": {"pack_format": 48, "description": "e2e world pack"},
        "mod_id": "create:shaft",
    }, indent=2)


def step_world_migrate(bins, work):
    log("step 3/6: mc-spine-world-migrate 1.21.1 -> 26.3 -> 1.21.1")
    binary = bins["world_migrate"]
    src = work / "level-1.21.1.json"
    src.write_text(world_metadata_3955(), encoding="utf-8")
    up = work / "level-26.3.json"
    down = work / "level-back.json"
    up_again = work / "level-again-26.3.json"

    result = expect([binary, "migrate-json", "--in", src, "--out", up,
                     "--to", "26.3", "--json"], 0)
    report = parse_json_stdout(result, "migrate-json report")
    check(report.get("original_data_version") == 3955,
          "original_data_version is not 3955")
    check(report.get("final_data_version") == 5023,
          "final_data_version is not 5023")
    doc = json.loads(up.read_text(encoding="utf-8"))
    check(doc.get("DataVersion") == 5023, "migrated doc DataVersion is not 5023")
    check(doc["Data"]["Version"]["Name"] == "26.3",
          "migrated doc Version.Name is not 26.3")
    palette = [e["Name"]
               for e in doc["sections"][0]["block_states"]["palette"]]
    check(palette == ["minecraft:short_grass", "minecraft:snow_layer",
                      "create:brass_casing"],
          f"palette not migrated/preserved as expected: {palette}")
    check("aether:skyroot_meadow" in doc["sections"][0]["biomes"]["palette"],
          "modded biome namespace was not preserved")
    item = doc["inventory"][0]
    check(item["id"] == "minecraft:oak_boat",
          "minecraft:boat was not renamed to minecraft:oak_boat")
    check("create:schematic" in item["components"],
          "modded item component was dropped")
    check(doc["pack"]["min_format"] == [121, 0],
          "pack_format was not lifted to the 26.x min_format tuple")

    expect([binary, "migrate-json", "--in", up, "--out", down,
            "--to", "1.21.1"], 2)
    check(not down.exists(),
          "refused downgrade must not write an output file")

    result = expect([binary, "migrate-json", "--in", up, "--out", down,
                     "--to", "1.21.1", "--allow-downgrade", "--json"], 0)
    check(parse_json_stdout(result, "downgrade report").get("final_data_version")
          == 3955, "downgrade did not restore DataVersion 3955")
    restored = json.loads(down.read_text(encoding="utf-8"))
    check(restored.get("DataVersion") == 3955,
          "restored doc DataVersion is not 3955")
    check(restored["inventory"][0]["id"] == "minecraft:boat",
          "downgrade did not restore minecraft:boat")
    check(restored["pack"]["pack_format"] == 48,
          "downgrade did not restore pack_format 48")

    # The pipeline pair is a fixed point: re-upgrading the restored document
    # reproduces the earlier 26.3 output byte-for-byte.
    expect([binary, "migrate-json", "--in", down, "--out", up_again,
            "--to", "26.3"], 0)
    check(up_again.read_bytes() == up.read_bytes(),
          "downgrade -> upgrade roundtrip is not byte-identical")
    log("  3955 -> 5023 migration, namespace preservation, downgrade gate verified")


# ------------------------------------------------- step 4: mc-spine-client

def _write_varint(value):
    out = bytearray()
    value &= 0xFFFFFFFF
    while True:
        byte = value & 0x7F
        value >>= 7
        if value:
            out.append(byte | 0x80)
        else:
            out.append(byte)
            return bytes(out)


def _take_varint(buf, pos):
    value = 0
    for shift in range(0, 35, 7):
        byte = buf[pos]
        pos += 1
        value |= (byte & 0x7F) << shift
        if not byte & 0x80:
            return value, pos
    raise ValueError("varint too long")


def _read_varint(stream):
    value = 0
    for shift in range(0, 35, 7):
        raw = stream.read(1)
        if len(raw) != 1:
            raise ConnectionError("eof inside varint")
        value |= (raw[0] & 0x7F) << shift
        if not raw[0] & 0x80:
            return value
    raise ValueError("varint too long")


def _read_frame(stream):
    """One uncompressed `VarInt length | VarInt pid | payload` frame."""
    length = _read_varint(stream)
    body = stream.read(length)
    if len(body) != length:
        raise ConnectionError(f"eof inside frame: wanted {length}, got {len(body)}")
    pid, pos = _take_varint(body, 0)
    return pid, body[pos:]


def _frame(pid, payload):
    body = _write_varint(pid) + payload
    return _write_varint(len(body)) + body


def _parse_handshake(payload):
    """(protocol, host, port, next_state) out of a handshake packet body."""
    protocol, pos = _take_varint(payload, 0)
    host_len, pos = _take_varint(payload, pos)
    host = payload[pos:pos + host_len].decode("utf-8")
    pos += host_len
    port = int.from_bytes(payload[pos:pos + 2], "big")
    next_state, _ = _take_varint(payload, pos + 2)
    return protocol, host, port, next_state


def status_response_json(motd, online, max_players, sample, protocol=777,
                         version_name="26.3"):
    """The status_response JSON document our mock server answers with."""
    return json.dumps({
        "version": {"name": version_name, "protocol": protocol},
        "players": {
            "online": online,
            "max": max_players,
            "sample": [{"name": name, "id": "00000000-0000-0000-0000-000000000000"}
                       for name in sample],
        },
        "description": {"text": motd},
    }, separators=(",", ":"))


def mock_status_exchange(conn, status_json, observed):
    """Serve one protocol status exchange on `conn`, recording the handshake.

    Mirrors the vanilla server-list sequence mc-spine-client --ping drives:
    handshake (next_state 1) -> status_request -> status_response ->
    ping_request -> pong_response.
    """
    stream = conn.makefile("rb")
    pid, payload = _read_frame(stream)
    check(pid == 0, f"expected handshake packet 0x00, got {pid:#04x}")
    protocol, host, port, next_state = _parse_handshake(payload)
    observed.update(protocol=protocol, host=host,
                    port=port, next_state=next_state)
    pid, payload = _read_frame(stream)
    check(pid == 0 and payload == b"",
          f"expected empty status_request, got pid {pid:#04x} + {len(payload)}B")
    raw = status_json.encode("utf-8")
    conn.sendall(_frame(0, _write_varint(len(raw)) + raw))
    pid, payload = _read_frame(stream)
    check(pid == 1 and len(payload) == 8,
          f"expected ping_request 0x01 with an i64, got pid {pid:#04x} "
          f"+ {len(payload)}B")
    conn.sendall(_frame(1, payload))
    observed["pong_echoed"] = int.from_bytes(payload, "big", signed=True)


def step_client_ping(bins, work):
    log("step 4/6: mc-spine-client --ping against a Protocol 777 mock server")
    binary = bins["client"]
    motd = "Alpha Test Server"
    status_json = status_response_json(motd, online=2, max_players=20,
                                       sample=["Alice", "Bob"])

    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    port = listener.getsockname()[1]
    observed = {}
    errors = []

    def serve():
        try:
            conn, _ = listener.accept()
            with conn:
                mock_status_exchange(conn, status_json, observed)
        except (OSError, E2EFailure, ConnectionError, ValueError) as error:
            errors.append(error)

    server = threading.Thread(target=serve, daemon=True)
    server.start()
    try:
        result = expect([binary, "--ping", f"127.0.0.1:{port}",
                         "--mc-version", "26.3", "--json"], 0,
                        timeout=30, cwd=work)
    finally:
        server.join(timeout=15)
        listener.close()
    if errors:
        raise E2EFailure(f"mock status server failed: {errors[0]!r}")
    check(observed.get("protocol") == 777,
          f"handshake carried protocol {observed.get('protocol')}, not 777")
    check(observed.get("next_state") == 1,
          "handshake next_state is not 1 (status)")
    check(observed.get("pong_echoed") is not None,
          "server never received the ping request")

    doc = parse_json_stdout(result, "client --ping --json")
    check(doc.get("status") == "ok", "ping status is not ok")
    check(doc.get("motd") == motd, f"motd {doc.get('motd')!r} != {motd!r}")
    check(doc.get("players_online") == 2 and doc.get("players_max") == 20,
          "player counts wrong")
    check(doc.get("player_sample") == ["Alice", "Bob"],
          "player sample wrong")
    check(doc.get("protocol") == 777 and doc.get("requested_protocol") == 777,
          "protocol fields wrong")
    check(doc.get("protocol_match") is True, "protocol_match is not true")
    check(isinstance(doc.get("rtt_ms"), (int, float))
          and not isinstance(doc.get("rtt_ms"), bool) and doc["rtt_ms"] > 0,
          f"rtt_ms is not positive: {doc.get('rtt_ms')!r}")
    log(f"  status exchange verified (motd, players, protocol 777, "
        f"rtt {doc['rtt_ms']:.3f} ms)")


# ----------------------------------------- step 5: mc-spine-voice-relay

def step_voice_relay(bins, work):
    log("step 5/6: mc-spine-voice-relay self-test, bench & bind posture")
    binary = bins["voice_relay"]

    expect([binary, "--self-test"], 0, cwd=work)

    bench = expect([binary, "--bench-packets", "200", "--json"], 0, cwd=work)
    report = parse_json_stdout(bench, "voice-relay --bench-packets --json")
    check(report.get("frames") == 200 and report.get("routed") == 200,
          f"bench routed {report.get('routed')}/{report.get('frames')} "
          f"frames, expected 200/200")
    pps = report.get("packets_per_second")
    check(isinstance(pps, (int, float)) and not isinstance(pps, bool)
          and pps > 0, f"packets_per_second is not positive: {pps!r}")
    lat = report.get("latency_ns") or {}
    check(all(isinstance(lat.get(k), int) and lat[k] >= 0
              for k in ("p50", "p99", "mean", "max")),
          f"latency_ns percentiles missing or malformed: {lat!r}")

    refused = expect([binary, "--bind", "0.0.0.0:24454"], 2, cwd=work)
    check("allow-public-bind" in refused.stderr,
          "public-bind refusal did not name --allow-public-bind")
    log(f"  loopback self-test, {report['routed']} routed frames "
        f"({pps:,.0f} packets/s), public bind refused with exit 2")


# ------------------------------------------------- step 6: release bundle

def _bundle_argv(out_dir, primary, companions):
    argv = [sys.executable, RELEASE_BUNDLE, "build",
            "--binary", primary,
            "--out-dir", out_dir,
            "--version", BUNDLE_VERSION,
            "--target", BUNDLE_TARGET,
            "--repo-root", ROOT,
            "--git-sha", BUNDLE_GIT_SHA]
    for binary in companions:
        argv += ["--extra-binary", binary]
    return argv


def step_release_bundle(bins, work):
    log("step 6/6: release_bundle.py multi-binary build x2 + verify")
    primary = work / "bin" / "mc-spine"
    primary.parent.mkdir(parents=True, exist_ok=True)
    primary.write_bytes(b"#!/bin/sh\n# e2e stand-in for the mc-spine server binary\n")
    primary.chmod(0o755)
    companions = [bins[key] for key, _, _, _ in BINARIES]

    out_a, out_b = work / "bundle-a", work / "bundle-b"
    expect(_bundle_argv(out_a, primary, companions), 0)
    expect(_bundle_argv(out_b, primary, companions), 0)

    tar_name = f"{BUNDLE_PREFIX}.tar.gz"
    for name in (tar_name, "release-manifest.json", "SHA256SUMS"):
        check((out_a / name).read_bytes() == (out_b / name).read_bytes(),
              f"release bundle {name} is not byte-identical across builds")
    tar_path = out_a / tar_name
    manifest_path = out_a / "release-manifest.json"
    sums_path = out_a / "SHA256SUMS"
    expect([sys.executable, RELEASE_BUNDLE, "verify",
            "--bundle", tar_path, "--manifest", manifest_path,
            "--sums", sums_path], 0)

    with tarfile.open(tar_path, "r:gz") as archive:
        members = {member.name for member in archive.getmembers()}
    expected = {f"{BUNDLE_PREFIX}/{name}"
                for name in ("mc-spine", "mc-spine-init", "mc-spine-mod-check",
                             "mc-spine-world-migrate", "mc-spine-client",
                             "mc-spine-voice-relay")}
    check(expected <= members,
          f"bundle is missing suite binaries: {sorted(expected - members)}")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    check(manifest.get("files") and len(manifest["files"]) == len(members),
          "manifest.files does not cover every tar member")
    log(f"  {tar_name} deterministic (byte-identical) and verify-clean")


# ---------------------------------------------------------------- self-test

def self_test():
    checks = []

    def check_(label, cond):
        checks.append((label, bool(cond)))
        return cond

    def expect_failure(label, function, *args, exc=E2EFailure, **kwargs):
        try:
            function(*args, **kwargs)
        except exc:
            return check_(label, True)
        except Exception:  # noqa: BLE001 - the wrong exception is a fail
            pass
        return check_(label, False)

    # varint codec vectors + roundtrip.
    vectors = {0: b"\x00", 1: b"\x01", 127: b"\x7f", 128: b"\x80\x01",
               255: b"\xff\x01", 777: b"\x89\x06", 25565: b"\xdd\xc7\x01"}
    check_("varint: known vectors encode correctly",
           all(_write_varint(v) == raw for v, raw in vectors.items()))
    check_("varint: vectors decode back to the value",
           all(_take_varint(raw + b"tail", 0) == (v, len(raw))
               for v, raw in vectors.items()))
    check_("varint: stream reader round-trips frames",
           _read_varint(io.BytesIO(_write_varint(2097151))) == 2097151)
    expect_failure("varint: a sixth continuation byte is rejected",
                   _read_varint, io.BytesIO(b"\x80" * 6), exc=ValueError)

    # frame codec roundtrip + handshake parser on a synthetic packet.
    framed = _frame(0x01, b"\x01\x02\x03\x04\x05\x06\x07\x08")
    pid, payload = _read_frame(io.BytesIO(framed))
    check_("frame: pid/payload roundtrip", pid == 1 and len(payload) == 8)
    hs = (_write_varint(777) + _write_varint(9) + b"127.0.0.1"
          + (25565).to_bytes(2, "big") + _write_varint(1))
    check_("handshake: protocol/host/port/next_state parse",
           _parse_handshake(hs) == (777, "127.0.0.1", 25565, 1))

    # status document shape the client expects.
    doc = json.loads(status_response_json("M", 1, 20, ["A"]))
    check_("status json: version.protocol 777 + description text",
           doc["version"]["protocol"] == 777
           and doc["description"]["text"] == "M"
           and doc["players"]["sample"][0]["name"] == "A")

    # full mock exchange over a socket pair: the same code path the E2E uses,
    # driven by a tiny Python client instead of the Rust binary.
    status_json = status_response_json("Offline Mock", 3, 40, ["E2E"], protocol=777)
    client_sock, server_sock = socket.socketpair()
    observed, errors = {}, []

    def serve():
        try:
            with server_sock:
                mock_status_exchange(server_sock, status_json, observed)
        except (OSError, E2EFailure, ConnectionError, ValueError) as error:
            errors.append(error)

    thread = threading.Thread(target=serve, daemon=True)
    thread.start()
    with client_sock:
        host = b"127.0.0.1"
        handshake = (_write_varint(777) + _write_varint(len(host)) + host
                     + (0).to_bytes(2, "big") + _write_varint(1))
        client_sock.sendall(_frame(0, handshake))
        client_sock.sendall(_frame(0, b""))
        stream = client_sock.makefile("rb")
        pid, payload = _read_frame(stream)
        replied = json.loads(payload[_take_varint(payload, 0)[1]:]) if pid == 0 else {}
        nonce = 0x1122334455667788
        client_sock.sendall(_frame(1, nonce.to_bytes(8, "big")))
        pong_pid, pong = _read_frame(stream)
    thread.join(timeout=10)
    check_("mock exchange: no server-side errors", not errors)
    check_("mock exchange: status_response parsed",
           pid == 0 and replied.get("version", {}).get("protocol") == 777)
    check_("mock exchange: pong echoes the nonce",
           pong_pid == 1 and int.from_bytes(pong, "big", signed=True) == nonce)
    check_("mock exchange: handshake observed",
           observed.get("protocol") == 777 and observed.get("next_state") == 1
           and observed.get("host") == "127.0.0.1")

    # .mrpack builder: a real ZIP_DEFLATED archive both parsers accept.
    with tempfile.TemporaryDirectory(prefix="companion-e2e-selftest-") as tmp:
        server_dir = Path(tmp) / "server"
        start = run([
            sys.executable, ROOT / "tools" / "megu_start.py",
            "--dir", server_dir, "--dry-run", "--json", "--lang", "en",
            "--accept-eula",
        ])
        try:
            start_plan = json.loads(start.stdout)
        except ValueError:
            start_plan = {}
        check_("onboarding: dry-run returns a JSON plan without starting a server",
               start.returncode in (0, 2) and not start.stderr
               and isinstance(start_plan, dict)
               and start_plan.get("schema") == "megu-start-plan/1"
               and start_plan.get("dry_run") is True
               and isinstance(start_plan.get("ready"), bool)
               and isinstance(start_plan.get("action_items"), list)
               and isinstance(start_plan.get("launch_argv"), list)
               and "mc-spine" in start_plan["launch_argv"])
        check_("onboarding: dry-run only creates the server bootstrap files",
               {path.relative_to(Path(tmp)).as_posix()
                for path in Path(tmp).rglob("*")}
               == {"server", "server/eula.txt", "server/server.properties"}
               and (server_dir / "eula.txt").read_text(encoding="utf-8")
               == "eula=true\n")

        pack = Path(tmp) / "pack.mrpack"
        build_mrpack(pack)
        with zipfile.ZipFile(pack) as archive:
            member = archive.getinfo("modrinth.index.json")
            index = json.loads(archive.read("modrinth.index.json"))
            deflated = member.compress_type == zipfile.ZIP_DEFLATED
        check_("mrpack: modrinth.index.json is ZIP_DEFLATED", deflated)
        check_("mrpack: manifest parses with 5 files on 1.21.1 NeoForge",
               index["formatVersion"] == 1
               and index["dependencies"]["minecraft"] == "1.21.1"
               and index["dependencies"]["neoforge"] == "21.1.215"
               and len(index["files"]) == 5)
        check_("mrpack: the client-only entry is server-unsupported",
               any(f["env"]["server"] == "unsupported" for f in index["files"]))
        src = Path(tmp) / "level.json"
        src.write_text(world_metadata_3955(), encoding="utf-8")
        doc = json.loads(src.read_text(encoding="utf-8"))
        check_("world doc: DataVersion 3955 with modded namespaces",
               doc["DataVersion"] == 3955
               and any("create:" in p.get("id", "")
                       for p in doc["sections"][0]["block_states"]["palette"]))
        dummy = Path(tmp) / "bin" / "mc-spine"
        dummy.parent.mkdir(parents=True)
        dummy.write_bytes(b"x")
        argv = _bundle_argv(Path(tmp) / "out", dummy, [Path("/a/b")])
        check_("bundle argv: --binary once + --extra-binary repeat",
               argv.count("--binary") == 1 and argv.count("--extra-binary") == 1)

    # the companions table is the single source of truth for what builds,
    # what the bundle ships, and which --*-bin flags exist.
    check_("binaries table: five companions, unique keys/names/flags",
           len(BINARIES) == 5
           and len({key for key, _, _, _ in BINARIES}) == 5
           and len({name for _, _, name, _ in BINARIES}) == 5
           and len({attr for _, _, _, attr in BINARIES}) == 5)
    check_("binaries table: voice relay ships from proto",
           ("voice_relay", "proto", "mc-spine-voice-relay", "voice_relay_bin")
           in BINARIES)
    check_("extra bins: delegation helpers sit beside a listed companion",
           set(EXTRA_BINS) <= {key for key, _, _, _ in BINARIES}
           and not ({name for names in EXTRA_BINS.values() for name in names}
                    & {name for _, _, name, _ in BINARIES}))

    # the assertion helpers themselves must be able to fail.
    expect_failure("check(): false is a failure", check, False, "boom")
    expect_failure("expect(): wrong exit code is a failure", expect,
                   [sys.executable, "-c", "import sys; sys.exit(3)"], 0)

    failed = [label for label, ok in checks if not ok]
    for label, ok in checks:
        print(f"  {'ok  ' if ok else 'FAIL'} {label}")
    if failed:
        print(f"self-test: {len(checks) - len(failed)}/{len(checks)} passed, "
              f"{len(failed)} FAILED: {failed}")
        return 1
    print(f"self-test: all {len(checks)} checks passed")
    return 0


# ------------------------------------------------------------------- main

def e2e(args):
    bins = ensure_binaries(args)
    for key, _, name, _ in BINARIES:
        log(f"using {name}: {bins[key]}")
    with tempfile.TemporaryDirectory(prefix="companion-suite-e2e-") as tmp:
        work = Path(tmp)
        step_init(bins, work)
        step_mod_check(bins, work)
        step_world_migrate(bins, work)
        step_client_ping(bins, work)
        step_voice_relay(bins, work)
        step_release_bundle(bins, work)
    log("PASS — all 6 companion-suite steps verified")


def main():
    parser = argparse.ArgumentParser(
        description="End-to-end multi-binary companion operator & release "
                    "suite smoke test")
    parser.add_argument("--self-test", action="store_true",
                        help="run the offline self-checks (no cargo) and exit")
    parser.add_argument("--target-dir", type=Path, default=None,
                        help="cargo target dir for companion builds "
                             "(default: $CARGO_TARGET_DIR or repo "
                             "target/companion-e2e)")
    parser.add_argument("--init-bin", type=Path, default=None,
                        help="prebuilt mc-spine-init binary (skip building it)")
    parser.add_argument("--mod-check-bin", type=Path, default=None,
                        help="prebuilt mc-spine-mod-check binary")
    parser.add_argument("--world-migrate-bin", type=Path, default=None,
                        help="prebuilt mc-spine-world-migrate binary")
    parser.add_argument("--client-bin", type=Path, default=None,
                        help="prebuilt mc-spine-client binary")
    parser.add_argument("--voice-relay-bin", type=Path, default=None,
                        help="prebuilt mc-spine-voice-relay binary")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    try:
        e2e(args)
    except E2EFailure as error:
        print(f"[companion-e2e] FAIL: {error}", file=sys.stderr)
        return 1
    except subprocess.TimeoutExpired as error:
        print(f"[companion-e2e] FAIL: timed out: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
