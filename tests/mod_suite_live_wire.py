#!/usr/bin/env python3
"""Live Protocol 767 mod-suite boot and custom-payload E2E against a compiled server.

Uses only Python's standard library, loopback sockets, and a disposable world.
The existing protocol767 client handles login, configuration, and compression.
"""

import hashlib
import json
import math
import os
from pathlib import Path
import re
import struct
import subprocess
import sys
import tempfile
import time
import zipfile

from protocol767 import Client, ContractFailure, Reader, ROOT, require, string, vint
from wire_smoke import port, status


SYNC_CHANNEL = "mc-spine:mod_sync_v1"
EXHAUSTION_CHANNEL = "appleskin:exhaustion_sync"
SATURATION_CHANNEL = "appleskin:saturation_sync"
CUSTOM_PAYLOAD_ID = 0x19
DOWNLOAD_URL = "https://cdn.modrinth.com/data/test/versions/1/example.jar"
JAR_NAMES = ("farmersdelight-1.21.1.jar", "custom_hearth_delight-1.0.jar")
NAMESPACES = ("farmersdelight", "custom_hearth_delight")


def culinary_jar(path, namespace):
    """Create a real ZIP/JAR with one cooking and one cutting recipe."""
    cooking = {
        "type": f"{namespace}:cooking",
        "ingredients": [{"item": "minecraft:carrot"}],
        "result": {"id": f"{namespace}:test_soup", "count": 1},
        "container": {"item": "minecraft:bowl"},
        "cookingtime": 200,
        "experience": 0.35,
    }
    cutting = {
        "type": f"{namespace}:cutting",
        "ingredients": [{"item": f"{namespace}:test_soup"}],
        "tool": {"item": f"{namespace}:knife"},
        "result": [{"item": f"{namespace}:soup_slice", "count": 2}],
    }
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_STORED) as archive:
        archive.writestr(f"data/{namespace}/recipe/test_soup.json", json.dumps(cooking))
        archive.writestr(f"data/{namespace}/recipe/test_soup_slice.json", json.dumps(cutting))


def make_fixture(root):
    mods = root / "mods"
    mods.mkdir()
    # Deliberately invalid ZIP bytes: the performance-mod shortcut must run
    # before either culinary or bytecode parsing tries to open this file.
    (mods / "sodium-neoforge-0.6.0.jar").write_bytes(b"not a ZIP or a JAR\n")
    for name, namespace in zip(JAR_NAMES, NAMESPACES):
        culinary_jar(mods / name, namespace)
    jar_bytes = (mods / JAR_NAMES[0]).read_bytes()
    entry = {
        "mod_id": "farmersdelight",
        "file_name": "example.jar",
        "download_url": DOWNLOAD_URL,
        "sha256_hex": hashlib.sha256(jar_bytes).hexdigest(),
        "size_bytes": len(jar_bytes),
    }
    manifest = root / "mod-sync.json"
    manifest.write_text(json.dumps({"entries": [entry]}), encoding="utf-8")
    return mods, manifest, entry


def custom_payload(packet_id, body):
    require(packet_id == CUSTOM_PAYLOAD_ID, f"custom-payload-id: 0x{packet_id:02x}")
    reader = Reader(body)
    channel = reader.string()
    require(channel, "custom-payload-empty-channel")
    return channel, reader.take(len(body) - reader.pos)


def decode_msy1(data):
    reader = Reader(data)
    require(reader.take(4) == b"MSY1", "mod-sync-magic")
    count = reader.number(">H")
    require(0 < count <= 512, f"mod-sync-entry-count: {count}")
    entries = []
    for _ in range(count):
        mod_id = reader.take(reader.number("B")).decode("utf-8")
        file_name = reader.take(reader.number("B")).decode("utf-8")
        download_url = reader.take(reader.number(">H")).decode("utf-8")
        size_bytes = reader.number(">Q")
        sha256_hex = reader.take(32).hex()
        require(mod_id and file_name.endswith(".jar") and size_bytes > 0,
                "mod-sync-invalid-entry")
        require(download_url.startswith("https://"), "mod-sync-invalid-url")
        entries.append({
            "mod_id": mod_id,
            "file_name": file_name,
            "download_url": download_url,
            "sha256_hex": sha256_hex,
            "size_bytes": size_bytes,
        })
    require(reader.pos == len(data), "mod-sync-trailing-bytes")
    return entries


def decode_appleskin(data):
    require(len(data) == 4, f"appleskin-f32-length: {len(data)}")
    value = struct.unpack(">f", data)[0]
    require(math.isfinite(value) and value >= 0, f"appleskin-f32-value: {value}")
    return value


class ModClient(Client):
    """Capture early play payloads, including those consumed inside login()."""

    def __init__(self, game):
        super().__init__(game, "Alice")
        self.payloads = {}

    def read(self, timeout):
        packet_id, body = super().read(timeout)
        if self.phase == "play" and packet_id == CUSTOM_PAYLOAD_ID:
            channel, payload = custom_payload(packet_id, body)
            if channel == SYNC_CHANNEL:
                self.payloads.setdefault(channel, decode_msy1(payload))
            elif channel in (EXHAUSTION_CHANNEL, SATURATION_CHANNEL):
                self.payloads.setdefault(channel, decode_appleskin(payload))
        return packet_id, body


def self_test():
    with tempfile.TemporaryDirectory(prefix="mc-spine-mods-selftest-") as tmp:
        mods, manifest, entry = make_fixture(Path(tmp))
        require(json.loads(manifest.read_text())["entries"] == [entry],
                "selftest-json-manifest")
        require(len(entry["sha256_hex"]) == 64, "selftest-sha256-length")
        require(not zipfile.is_zipfile(mods / "sodium-neoforge-0.6.0.jar"),
                "selftest-sodium-must-be-garbage")
        for name, namespace in zip(JAR_NAMES, NAMESPACES):
            with zipfile.ZipFile(mods / name) as archive:
                require(archive.testzip() is None, "selftest-jar-corrupt")
                for kind, recipe in (("cooking", "test_soup"),
                                     ("cutting", "test_soup_slice")):
                    item = json.loads(archive.read(
                        f"data/{namespace}/recipe/{recipe}.json"))
                    require(item["type"] == f"{namespace}:{kind}",
                            "selftest-jar-recipe")

        # Independent encoding of the MSY1 wire contract, followed by the
        # same 0x19 channel parser used by the live client.
        raw_id = entry["mod_id"].encode()
        raw_name = entry["file_name"].encode()
        raw_url = entry["download_url"].encode()
        wire = (b"MSY1" + struct.pack(">H", 1)
                + bytes([len(raw_id)]) + raw_id
                + bytes([len(raw_name)]) + raw_name
                + struct.pack(">H", len(raw_url)) + raw_url
                + struct.pack(">Q", entry["size_bytes"])
                + bytes.fromhex(entry["sha256_hex"]))
        channel, body = custom_payload(CUSTOM_PAYLOAD_ID, string(SYNC_CHANNEL) + wire)
        require(channel == SYNC_CHANNEL and decode_msy1(body) == [entry],
                "selftest-msy1-roundtrip")
        for bad in (wire[:3], b"BAD1" + wire[4:], wire[:-1], wire + b"extra"):
            try:
                decode_msy1(bad)
            except (AssertionError, struct.error, UnicodeError):
                pass
            else:
                raise ContractFailure("selftest-invalid-msy1-accepted")
        for channel in (EXHAUSTION_CHANNEL, SATURATION_CHANNEL):
            packet_channel, payload = custom_payload(
                CUSTOM_PAYLOAD_ID, string(channel) + struct.pack(">f", 5.25))
            require(packet_channel == channel and decode_appleskin(payload) == 5.25,
                    "selftest-appleskin-f32")
        for bad in (b"", b"\0\0", struct.pack(">f", float("nan"))):
            try:
                decode_appleskin(bad)
            except AssertionError:
                pass
            else:
                raise ContractFailure("selftest-invalid-appleskin-accepted")
    print("PASS offline synthetic JAR, MSY1 and AppleSkin 0x19 parsers", flush=True)


def log_tail(path):
    return path.read_text(errors="replace")[-6500:] if path.exists() else "(no log)"


def run_live(binary):
    require(binary.is_file(), f"compiled-binary-missing: {binary}")
    require(os.access(binary, os.X_OK), f"compiled-binary-not-executable: {binary}")
    with tempfile.TemporaryDirectory(prefix="mc-spine-mods-live-") as tmp:
        root = Path(tmp)
        mods, manifest, entry = make_fixture(root)
        for name in ("config_packets.bin", "assets"):
            source = ROOT / name
            require(source.exists(), f"server-fixture-missing: {source}")
            (root / name).symlink_to(source, target_is_directory=source.is_dir())

        game = port()
        env = os.environ.copy()
        for key in ("MC_SPINE_RCON_PORT", "MC_SPINE_RCON_PASS",
                    "MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS", "MC_SPINE_PROGRESSION",
                    "MC_SPINE_MOD_SYNC_MIRRORS"):
            env.pop(key, None)
        env.update(
            MC_SPINE_PORT=str(game), MC_SPINE_BIND_ADDR="127.0.0.1",
            MC_SPINE_MODS_DIR=str(mods), MC_SPINE_MOD_SYNC=str(manifest),
            MC_SPINE_WG="old", MC_SPINE_ENABLE_QUERY="0",
            MC_SPINE_ANVIL_WORLD_DIR="", MC_SPINE_RESOURCE_PACK_URL="",
            MC_SPINE_ANTICHEAT="off",
        )
        log_path = root / "server.log"
        try:
            with log_path.open("w") as output:
                proc = subprocess.Popen([str(binary)], cwd=root, env=env,
                                        stdin=subprocess.DEVNULL, stdout=output,
                                        stderr=subprocess.STDOUT)
                try:
                    deadline = time.monotonic() + 25
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

                    with ModClient(game) as alice:
                        alice.login()
                        expected_channels = {
                            SYNC_CHANNEL, EXHAUSTION_CHANNEL, SATURATION_CHANNEL,
                        }
                        if not expected_channels <= alice.payloads.keys():
                            alice.until(
                                lambda *_: expected_channels <= alice.payloads.keys(),
                                "live-mod-payloads-missing-" + repr(
                                    expected_channels - alice.payloads.keys()),
                            )
                        require(alice.payloads[SYNC_CHANNEL] == [entry],
                                f"live-mod-sync-entry: {alice.payloads[SYNC_CHANNEL]}")
                        print("PASS Protocol 767 Alice login -> configuration -> play", flush=True)
                        print("PASS 0x19 mc-spine:mod_sync_v1 MSY1 manifest (1 SHA-256 entry)",
                              flush=True)
                        print("PASS 0x19 AppleSkin exhaustion/saturation big-endian f32", flush=True)
                finally:
                    if proc.poll() is None:
                        proc.terminate()
                    try:
                        proc.wait(timeout=20)
                    except subprocess.TimeoutExpired as exc:
                        proc.kill()
                        proc.wait(timeout=5)
                        raise ContractFailure("server-graceful-shutdown-timeout") from exc

            logs = log_path.read_text(errors="replace")
            require(proc.returncode == 0,
                    f"server-graceful-shutdown-exit: {proc.returncode}")
            require("[live-mods] 3 jar(s): 1 superseded, 0 native, "
                    "2 data-only, 0 bounded-shim, 0 tier3" in logs,
                    "live-mod-boot-classification")
            require("[live-mods] culinary: 2 cooking, 2 cutting" in logs,
                    "live-mod-boot-culinary")
            require("[live-mods] mod sync manifest armed: 1 entry" in logs,
                    "live-mod-boot-sync")
            require(not re.search(r"(?i)panicked at|thread .* panicked|stack backtrace", logs),
                    "server-panic-in-logs")
            require("culinary scan skipped" not in logs and "mixin scan failed closed" not in logs,
                    "live-mod-jar-scan-failed")
            print("PASS live boot: sodium auto-skipped, 2 culinary JARs loaded "
                  "(2 cooking, 2 cutting), sync armed", flush=True)
            print("PASS graceful shutdown (exit 0, no panics, disposable files cleaned)",
                  flush=True)
        except BaseException:
            print("SERVER STDOUT/STDERR (tail)\n" + log_tail(log_path), file=sys.stderr)
            raise


def main():
    if len(sys.argv) == 2 and sys.argv[1] == "--self-test":
        self_test()
    elif len(sys.argv) == 2:
        run_live(Path(sys.argv[1]).resolve())
    else:
        raise SystemExit(f"usage: {sys.argv[0]} --self-test | /path/to/mc-spine-mod-suite")


if __name__ == "__main__":
    main()
