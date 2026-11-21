#!/usr/bin/env python3
"""Real compiled-server GS4, two-player equipment/commands, and Anvil E2E.

Only loopback sockets and disposable worlds are used. Python stdlib only.
The protocol-767 fixture handles Set Compression (0x03) transparently.
"""

import contextlib
from collections import deque
import hashlib
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import time

from protocol767 import Client, ContractFailure, Reader, ROOT, require, string, vint
from wire_smoke import port, status


MOTD = "mc-spine live parity E2E"
SESSION = b"\x19\x28\x37\x46"
MASKED_SESSION = bytes(b & 0x0f for b in SESSION)
GS4_KEYS = (
    "hostname", "gametype", "game_id", "version", "plugins", "map",
    "numplayers", "maxplayers", "hostport", "hostip",
)
STONE = 1
WHEAT = 854


def check(condition, label, detail=""):
    require(condition, f"{label}: {detail}")


def terminated(data, cursor):
    end = data.find(b"\0", cursor)
    check(end >= 0, "gs4-missing-terminator", data[cursor:].hex())
    return data[cursor:end].decode("utf-8"), end + 1


def gs4_request(packet_type, token=None, full=False):
    raw = b"\xfe\xfd" + bytes([packet_type]) + SESSION
    if token is not None:
        raw += struct.pack(">i", token)
        if full:
            raw += b"\0\0\0\0"
    return raw


def gs4_header(data, packet_type):
    check(len(data) >= 5 and data[:5] == bytes([packet_type]) + MASKED_SESSION,
          "gs4-response-header", data[:24].hex())
    return 5


def gs4_challenge(data):
    cursor = gs4_header(data, 0x09)
    value, end = terminated(data, cursor)
    check(end == len(data) and value.isascii() and value.isdecimal(),
          "gs4-challenge-decimal", repr(data))
    token = int(value)
    check(0 <= token <= 0x7fffffff, "gs4-challenge-range", token)
    return token


def gs4_basic(data):
    cursor = gs4_header(data, 0x00)
    names = ("hostname", "gametype", "map", "numplayers", "maxplayers")
    values = {}
    for name in names:
        values[name], cursor = terminated(data, cursor)
    check(cursor + 2 <= len(data), "gs4-basic-port-truncated", data.hex())
    values["hostport"] = str(struct.unpack_from("<H", data, cursor)[0])
    values["hostip"], cursor = terminated(data, cursor + 2)
    check(cursor == len(data), "gs4-basic-trailing", data[cursor:].hex())
    return values


def gs4_full(data):
    cursor = gs4_header(data, 0x00)
    marker = b"splitnum\0\x80\0"
    check(data[cursor:cursor + len(marker)] == marker,
          "gs4-full-splitnum", data[cursor:cursor + 24].hex())
    cursor += len(marker)
    values = {}
    for expected_key in GS4_KEYS:
        key, cursor = terminated(data, cursor)
        check(key == expected_key, "gs4-full-key-order", (expected_key, key))
        value, cursor = terminated(data, cursor)
        values[key] = value
    player_marker = b"\0\x01player_\0\0"
    check(data[cursor:cursor + len(player_marker)] == player_marker,
          "gs4-full-player-marker", data[cursor:cursor + 24].hex())
    cursor += len(player_marker)
    players = []
    while cursor < len(data):
        player, cursor = terminated(data, cursor)
        if not player:
            break
        players.append(player)
    check(cursor == len(data), "gs4-full-trailing", data[cursor:].hex())
    return values, players


def equipment_packet(body):
    """Decode the packet's entity id and exact slot/item entries (empty components)."""
    reader = Reader(body)
    entity_id = reader.varint()
    slots = {}
    for _ in range(6):
        flag = reader.number("B")
        slot = flag & 0x7f
        check(slot <= 5 and slot not in slots, "equipment-slot", body.hex())
        count = reader.varint()
        check(0 <= count <= 127, "equipment-count", body.hex())
        item = reader.varint() if count else 0
        if count:
            check(reader.varint() == 0 and reader.varint() == 0,
                  "equipment-components", body.hex())
        slots[slot] = (item, count)
        if not flag & 0x80:
            check(reader.pos == len(body), "equipment-trailing", body.hex())
            return entity_id, slots
    raise ContractFailure(f"equipment-unterminated: {body.hex()}")


def self_test():
    check(len(gs4_request(0x09)) == 7, "selftest-gs4-handshake-length")
    check(len(gs4_request(0x00, 123)) == 11, "selftest-gs4-basic-length")
    check(len(gs4_request(0x00, 123, full=True)) == 15,
          "selftest-gs4-full-length")
    check(gs4_challenge(b"\x09" + MASKED_SESSION + b"12345\0") == 12345,
          "selftest-gs4-challenge")
    basic = (b"\x00" + MASKED_SESSION + b"hello\0SMP\0world\0" + b"0\0" + b"20\0"
             + struct.pack("<H", 25565) + b"127.0.0.1\0")
    check(gs4_basic(basic)["hostport"] == "25565", "selftest-gs4-basic")
    pairs = (b"".join(k.encode() + b"\0" + v.encode() + b"\0" for k, v in zip(
        GS4_KEYS, ("hello", "SMP", "MINECRAFT", "1.21.1", "", "world",
                   "2", "20", "25565", "127.0.0.1"))))
    full = (b"\x00" + MASKED_SESSION + b"splitnum\0\x80\0" + pairs
            + b"\0\x01player_\0\0Alice\0Bob\0\0")
    values, players = gs4_full(full)
    check(values["numplayers"] == "2" and players == ["Alice", "Bob"],
          "selftest-gs4-full")
    equipped = vint(42) + b"\0" + vint(1) + vint(WHEAT) + b"\0\0"
    check(equipment_packet(equipped) == (42, {0: (WHEAT, 1)}),
          "selftest-equipment-roundtrip")
    for invalid in (equipped[:-1], equipped + b"\0", vint(42) + b"\x06\0"):
        try:
            equipment_packet(invalid)
        except (AssertionError, struct.error):
            continue
        raise ContractFailure(f"selftest-equipment-invalid-accepted: {invalid.hex()}")
    print("PASS offline GS4 codec and 0x5b packet assertions", flush=True)


def offline_uuid(name):
    raw = bytearray(hashlib.md5(("OfflinePlayer:" + name).encode()).digest())
    raw[6] = (raw[6] & 15) | 48
    raw[8] = (raw[8] & 63) | 128
    return bytes(raw)


def prepare_server(root):
    root.mkdir(parents=True, exist_ok=True)
    for name in ("config_packets.bin", "assets"):
        link = root / name
        if not link.exists():
            target = ROOT / name
            link.symlink_to(target, target_is_directory=target.is_dir())


def log_tail(root):
    log = root / "server.log"
    return log.read_text(errors="replace")[-6500:] if log.exists() else "(no log)"


@contextlib.contextmanager
def server(binary, root, overrides=None):
    """Separate working world, loopback listener, logs, and graceful SIGTERM."""
    prepare_server(root)
    game = port()
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
        udp.bind(("127.0.0.1", 0))
        query = udp.getsockname()[1]
    env = os.environ.copy()
    env.pop("MC_SPINE_RCON_PORT", None)
    env.pop("MC_SPINE_RCON_PASS", None)
    env.pop("MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS", None)
    env.update(MC_SPINE_PORT=str(game), MC_SPINE_BIND_ADDR="127.0.0.1",
               MC_SPINE_QUERY_PORT=str(query), MC_SPINE_ENABLE_QUERY="0",
               MC_SPINE_ANVIL_WORLD_DIR="", MC_SPINE_MOTD=MOTD,
               MC_SPINE_WG="old", MC_SPINE_RESOURCE_PACK_URL="",
               MC_SPINE_ANTICHEAT="off")
    if overrides:
        env.update(overrides)
    failure = False
    with (root / "server.log").open("w") as output:
        proc = subprocess.Popen([str(binary)], cwd=root, env=env,
                                stdin=subprocess.DEVNULL, stdout=output,
                                stderr=subprocess.STDOUT)
        try:
            for _ in range(160):
                check(proc.poll() is None, "server-startup-exit", proc.returncode)
                try:
                    status(game)
                    break
                except (OSError, AssertionError):
                    time.sleep(0.05)
            else:
                raise ContractFailure("server-listener-start-timeout")
            yield game, query, proc
        except BaseException:
            failure = True
            output.flush()
            print("SERVER STDERR/STDOUT (tail)\n" + log_tail(root), file=sys.stderr)
            raise
        finally:
            if proc.poll() is None:
                proc.terminate()
            try:
                proc.wait(timeout=20)
            except subprocess.TimeoutExpired as exc:
                proc.kill()
                proc.wait(timeout=5)
                raise ContractFailure("server-graceful-shutdown-timeout: " + log_tail(root)) from exc
            if not failure:
                check(proc.returncode == 0, "server-graceful-shutdown-exit",
                      f"{proc.returncode}\n{log_tail(root)}")


class TraceClient(Client):
    def __init__(self, game, name):
        super().__init__(game, name)
        self.events = deque(maxlen=40)
        self.spawns = {}
        self.equipment = []
        self.cameras = []
        self.entity_id = None

    def read(self, timeout):
        pid, body = super().read(timeout)
        if self.phase == "play":
            self.events.append(f"0x{pid:02x} len={len(body)} data={body[:60].hex()}")
            if pid == 0x2b:
                self.entity_id = Reader(body).number(">i")
            elif pid == 0x01:
                reader = Reader(body)
                eid = reader.varint()
                self.spawns[reader.take(16)] = eid
            elif pid == 0x5b:
                self.equipment.append(equipment_packet(body))
            elif pid == 0x52:
                reader = Reader(body)
                self.cameras.append(reader.varint())
                check(reader.pos == len(body), "camera-packet-trailing", body.hex())
        return pid, body

    def command(self, text, feedback):
        before = len(self.messages)
        self.send(0x04, string(text))
        try:
            self.until(lambda *_: any(feedback in reply for reply in self.messages[before:]),
                       f"command-{text}-feedback-{feedback}-actual-{self.messages[before:]}")
        finally:
            # Match the existing command wire probe's spam-limiter pacing.
            time.sleep(0.18)


def check_stat_values(values, game, count):
    check(values["hostname"] == MOTD, "gs4-motd", values)
    check(values["gametype"] == "SMP", "gs4-gametype", values)
    if "version" in values:
        check(values["version"] == "1.21.1", "gs4-version", values)
    check(int(values["numplayers"]) == count, "gs4-numplayers", values)
    check(int(values["maxplayers"]) >= 2, "gs4-maxplayers", values)
    check(values["hostport"] == str(game), "gs4-hostport", values)
    check(values["hostip"] == "127.0.0.1", "gs4-hostip", values)


def query_live(game, query, count, expected_players):
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
        udp.bind(("127.0.0.1", 0))
        udp.settimeout(2)
        address = ("127.0.0.1", query)
        udp.sendto(gs4_request(0x09), address)
        token = gs4_challenge(udp.recvfrom(4096)[0])
        udp.sendto(gs4_request(0x00, token), address)
        basic = gs4_basic(udp.recvfrom(4096)[0])
        check_stat_values(basic, game, count)
        udp.sendto(gs4_request(0x00, token, full=True), address)
        values, players = gs4_full(udp.recvfrom(4096)[0])
        check(tuple(values) == GS4_KEYS, "gs4-ten-kv-keys", values)
        check_stat_values(values, game, count)
        check(values["game_id"] == "MINECRAFT" and values["plugins"] == "",
              "gs4-vanilla-fields", values)
        check(values["map"] == basic["map"], "gs4-map", (basic, values))
        check(set(players) == set(expected_players) and len(players) == count,
              "gs4-player-list", players)
        check(all(basic[key] == values[key] for key in basic),
              "gs4-basic-full-consistency", (basic, values))
        udp.settimeout(0.3)
        invalid = token + 1 if token < 0x7fffffff else token - 1
        udp.sendto(gs4_request(0x00, invalid), address)
        try:
            unexpected = udp.recvfrom(4096)[0]
        except socket.timeout:
            pass
        else:
            raise ContractFailure(f"gs4-invalid-token-accepted: {unexpected.hex()}")


def creative_slot(client, slot, item):
    client.send(0x32, struct.pack(">h", slot) + vint(1) + vint(item) + b"\0\0")

    def acknowledged(pid, body):
        if pid != 0x15:
            return False
        reader = Reader(body)
        window = reader.number("B")
        reader.varint()  # state id
        sent_slot = reader.number(">h")
        count = reader.varint()
        sent_item = reader.varint() if count else 0
        if count:
            check(reader.varint() == 0 and reader.varint() == 0,
                  "creative-slot-components", body.hex())
        check(reader.pos == len(body), "creative-slot-trailing", body.hex())
        return window == 0 and sent_slot == slot and sent_item == item and count == 1

    client.until(acknowledged, f"creative-slot-{slot}-{item}-ack")


def equipment_seen(observer, uuid, item):
    eid = observer.spawns.get(uuid)
    return eid is not None and any(
        entity == eid and slots.get(0) == (item, 1)
        for entity, slots in observer.equipment
    )


def two_players(game, query):
    alice = TraceClient(game, "Alice")
    bob = TraceClient(game, "Bob")
    try:
        with alice, bob:
            alice.login(view=2)
            alice.command("gamemode creative", "gamemode")
            creative_slot(alice, 36, STONE)
            creative_slot(alice, 37, WHEAT)
            bob.login(view=2)
            bob.until(lambda *_: equipment_seen(bob, offline_uuid("Alice"), STONE),
                      "bob-spawn-alice-set-equipment-0x5b")
            alice.send(0x2f, struct.pack(">h", 1))
            bob.until(lambda *_: equipment_seen(bob, offline_uuid("Alice"), WHEAT),
                      "bob-hotbar-switch-alice-set-equipment-0x5b")
            query_live(game, query, 2, ("Alice", "Bob"))
            print("PASS live 767 two players and spawn/hotbar SetEquipment 0x5b", flush=True)

            alice.command("weather clear 30s", "Weather: clear")
            alice.command("time add 1d", "Set the time to")
            alice.command("fillbiome 0 80 0 3 83 3 plains", "Changed biome of 1 cells")
            alice.command("forceload query", "forceloaded chunk(s)")
            alice.command("ride Bob mount Alice", "Now riding")
            alice.command("ride Bob dismount", "No longer riding")
            alice.command("gamemode spectator", "gamemode")
            alice.command("spectate Bob", "Now spectating")
            check(bob.entity_id is not None, "bob-entity-id")
            if bob.entity_id not in alice.cameras:
                alice.until(lambda *_: bob.entity_id in alice.cameras,
                            "spectate-camera-packet-0x52")
            check(alice.socket.fileno() >= 0 and bob.socket.fileno() >= 0,
                  "both-player-sockets-open")
            print("PASS live commands weather/time/fillbiome/spectate/ride/forceload", flush=True)
    except BaseException:
        for client in (alice, bob):
            print(f"{client.name} packet trace:\n" + "\n".join(client.events),
                  file=sys.stderr)
            print(f"{client.name} feedback: {client.messages[-12:]}", file=sys.stderr)
        raise


def no_staging_files(directory):
    stale = sorted(str(p.relative_to(directory)) for p in directory.rglob("*")
                   if p.is_file() and (p.suffix == ".tmp" or p.name == "level.dat_new"))
    check(not stale, "anvil-stray-staging-files", stale)


def anvil_roundtrip(binary, root):
    world = root / "anvil-world"
    first = root / "anvil-server-1"
    second = root / "anvil-server-2"
    with server(binary, first, {"MC_SPINE_ANVIL_WORLD_DIR": str(world)}):
        pass
    level = world / "level.dat"
    check(level.is_file() and level.stat().st_size > 0,
          "anvil-first-graceful-save-level-dat", log_tail(first))
    check("[anvil] saved" in log_tail(first), "anvil-first-save-log", log_tail(first))
    no_staging_files(world)
    with server(binary, second, {"MC_SPINE_ANVIL_WORLD_DIR": str(world)}):
        pass
    second_log = log_tail(second)
    check("[anvil] loaded" in second_log and "level_loaded: true" in second_log,
          "anvil-second-loaded-from-disk", second_log)
    check("[anvil] saved" in second_log and "backup_rotated: true" in second_log,
          "anvil-second-rotated-save", second_log)
    check(level.is_file() and level.stat().st_size > 0, "anvil-second-level-dat", second_log)
    backup = world / "level.dat_old"
    check(backup.is_file() and backup.stat().st_size > 0,
          "anvil-second-backup-level-dat-old", second_log)
    no_staging_files(world)
    print("PASS native Anvil two-process boot/reload/atomic rotation and staging cleanup",
          flush=True)


def main():
    if len(sys.argv) == 2 and sys.argv[1] == "--self-test":
        self_test()
        return
    if len(sys.argv) != 2:
        raise SystemExit("usage: live_parity_e2e_wire.py --self-test | /path/to/mc-spine")
    binary = Path(sys.argv[1]).resolve()
    check(binary.is_file() and os.access(binary, os.X_OK), "compiled-binary-missing", binary)
    os.environ.setdefault("MC_PROBE_DEADLINE", "20")
    with tempfile.TemporaryDirectory(prefix="mc-spine-live-parity-") as temporary:
        root = Path(temporary)
        game_root = root / "network-server"
        op = offline_uuid("Alice").hex()
        uuid = f"{op[:8]}-{op[8:12]}-{op[12:16]}-{op[16:20]}-{op[20:]}"
        prepare_server(game_root)
        (game_root / "ops.json").write_text(
            '[{"uuid":"' + uuid + '","name":"Alice","level":4,'
            '"bypassesPlayerLimit":false}]', encoding="utf-8")
        with server(binary, game_root, {"MC_SPINE_ENABLE_QUERY": "1"}) as (game, query, _):
            query_live(game, query, 0, ())
            print("PASS live GS4 UDP challenge/basic/full/10 keys/invalid token", flush=True)
            two_players(game, query)
        anvil_roundtrip(binary, root)


if __name__ == "__main__":
    try:
        main()
    except (AssertionError, OSError, ValueError, UnicodeError, struct.error) as error:
        print(f"LIVE PARITY E2E FAIL: {type(error).__name__}: {error}", file=sys.stderr)
        raise SystemExit(1) from error
