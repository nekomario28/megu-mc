#!/usr/bin/env python3
"""Live protocol-767, RCON and datapack regression contracts (E4-16).

All worlds, synthesized datapacks, logs and network listeners are isolated under
target/tmp. The test is deliberately an external client of the compiled binary.
"""

import contextlib
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
import zipfile

from container_wire import stack, wire_stack
from protocol767 import Client, Reader, ROOT, port, require, status, string, vint


BINARY = ROOT / "target/debug/mc-spine"
# Protocol 767 item registry IDs, pinned by src/main.rs:name_to_item.
COAL, STICK, TORCH = 803, 848, 291
RESULTS = []
LOGIN_PLAY_IDS = []
RCON_SECRET = "e4-wire-regression-secret"


def check(name, condition, detail=""):
    require(condition, f"{name}: {detail}")
    RESULTS.append(name)
    print(f"PASS {len(RESULTS):02d} {name}", flush=True)


def put(base, relative, content):
    path = base / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def synthetic_packs(root):
    packs = root / "datapacks"
    packs.mkdir()
    # Vanilla holds recipes/tags, but has no function and is not indexed in
    # /datapack list by the current server. Use an isolated wrapper to exercise
    # its data and make discovery visible without modifying the source pack.
    vanilla = packs / "vanilla"
    (vanilla / "data").mkdir(parents=True)
    (vanilla / "pack.mcmeta").symlink_to(ROOT / "datapacks/vanilla/pack.mcmeta")
    (vanilla / "data/minecraft").symlink_to(ROOT / "datapacks/vanilla/data/minecraft",
                                            target_is_directory=True)
    put(vanilla, "data/e4vanilla/function/wire_marker.mcfunction", "# discovery marker\n")
    for name in ("spinetest", "rectest", "tagtest", "modtest"):
        shutil.copytree(ROOT / "datapacks" / name, packs / name)
        # The loader's /datapack list indexes only packs with functions/load.
        # Existing recipe/tag-only packs need a function to appear in that list.
        put(packs / name, f"data/{name}/function/wire_marker.mcfunction", "# marker for E4-16\n")

    pack = packs / "e4live"
    put(pack, "pack.mcmeta", json.dumps({"pack": {"pack_format": 48, "description": "E4-16 live wire"}}))
    put(pack, "data/e4/tags/function/load.json", json.dumps({"values": ["e4:startup"]}))
    put(pack, "data/e4/tags/function/tick.json", json.dumps({"values": ["e4:heartbeat"]}))
    put(pack, "data/e4/function/startup.mcfunction", "\n".join((
        "scoreboard objectives add e4life dummy",
        "scoreboard players set Loaded e4life 1",
        "scoreboard players set Tick e4life 0",
        "setblock 24 80 24 minecraft:diamond_block",
    )) + "\n")
    put(pack, "data/e4/function/heartbeat.mcfunction", "scoreboard players add Tick e4life 1\n")
    put(pack, "data/e4/function/root.mcfunction", "function e4:nested/leaf\n")
    put(pack, "data/e4/function/nested/leaf.mcfunction", "scoreboard players set Nested e4life 7\nsetblock 25 80 24 minecraft:gold_block\n")
    put(pack, "data/e4/function/scheduled.mcfunction", "scoreboard players set Scheduled e4life 9\n")
    put(pack, "data/e4/tags/block/treasure.json", json.dumps({"values": ["minecraft:diamond_block"]}))
    put(pack, "data/e4/tags/item/coal_like.json", json.dumps({"values": ["minecraft:coal"]}))
    put(pack, "data/e4/recipe/coal_to_torches.json", json.dumps({
        "type": "minecraft:crafting_shapeless", "ingredients": [{"tag": "e4:coal_like"}],
        "result": {"id": "minecraft:torch", "count": 2},
    }))

    with zipfile.ZipFile(packs / "ziponly.zip", "w") as archive:
        archive.writestr("pack.mcmeta", '{"pack":{"pack_format":48,"description":"zip sentinel"}}')
        archive.writestr("data/e4zip/function/probe.mcfunction", "setblock 26 80 24 minecraft:emerald_block\n")
    return packs


@contextlib.contextmanager
def server(root, *, player_admin=False):
    for link in ("assets", "config_packets.bin"):
        (root / link).symlink_to(ROOT / link, target_is_directory=(ROOT / link).is_dir())
    game, rcon_port = port(), port()
    while rcon_port == game:
        rcon_port = port()
    env = os.environ.copy()
    env.update(MC_SPINE_PORT=str(game), MC_SPINE_BIND_ADDR="127.0.0.1",
               MC_SPINE_RCON_PORT=str(rcon_port), MC_SPINE_RCON_BIND_ADDR="127.0.0.1",
               MC_SPINE_RCON_PASS=RCON_SECRET, MC_SPINE_WG="old",
               MC_SPINE_RESOURCE_PACK_URL="")
    env.pop("MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS", None)
    if player_admin:
        env["MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS"] = "1"
    with (root / "server.log").open("w+") as log:
        process = subprocess.Popen([str(BINARY)], cwd=root, env=env, stdout=log, stderr=log)
        try:
            for _ in range(240):
                require(process.poll() is None, "server-exited-during-startup")
                try:
                    status(game)
                    with socket.create_connection(("127.0.0.1", rcon_port), timeout=0.5):
                        break
                except (OSError, AssertionError):
                    time.sleep(0.05)
            else:
                raise AssertionError("live listener startup deadline")
            yield game, rcon_port
        except BaseException:
            log.flush()
            log.seek(0)
            print("SERVER LOG (tail)\n" + log.read()[-5500:], file=sys.stderr)
            raise
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)


class Rcon:
    def __init__(self, game, password):
        self.sock = socket.create_connection(("127.0.0.1", game), timeout=10)
        self.sock.settimeout(10)
        self.number = 10
        rid, kind, _ = self.send(3, password)
        self.authorized = rid == 10 and kind == 2

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.sock.close()

    def send(self, kind, payload):
        data = struct.pack("<ii", self.number, kind) + payload.encode("utf-8") + b"\0\0"
        self.sock.sendall(struct.pack("<i", len(data)) + data)
        size = struct.unpack("<i", self.exact(4))[0]
        require(10 <= size <= 8192, "rcon-response-size")
        body = self.exact(size)
        require(body[-2:] == b"\0\0", "rcon-response-terminator")
        return (*struct.unpack("<ii", body[:8]), body[8:-2].decode("utf-8"))

    def exact(self, n):
        data = bytearray()
        while len(data) < n:
            part = self.sock.recv(n - len(data))
            if not part:
                raise EOFError("RCON unexpectedly closed")
            data.extend(part)
        return bytes(data)

    def command(self, command):
        require(self.authorized, "rcon-unauthorized-command-attempt")
        self.number += 1
        rid, kind, reply = self.send(2, command)
        require((rid, kind) == (self.number, 0), f"rcon-packet-{command}: {(rid, kind)}")
        return reply


class WireClient(Client):
    def __init__(self, game, name):
        super().__init__(game, name)
        self.play_ids = []
        self.slots = {}
        self.respawns = []

    def read(self, timeout):
        pid, body = super().read(timeout)
        if self.phase == "play":
            self.play_ids.append(pid)
            if pid == 0x47:
                self.respawns.append(self.dimension_name)
            elif pid == 0x15:
                reader = Reader(body)
                window = reader.number("B")
                reader.varint()
                slot_id = reader.number(">h")
                value = stack(reader)
                require(reader.pos == len(body), "complete-slot-packet")
                self.slots[(window, slot_id)] = value
        return pid, body

    def command(self, text, expected):
        before = len(self.messages)
        self.send(0x04, string(text))
        self.until(lambda *_: any(expected in m for m in self.messages[before:]),
                   f"player-command-feedback-{text}")
        # The command packet passes through the normal chat spam limiter.
        time.sleep(0.16)
        return next(m for m in self.messages[before:] if expected in m)

    def denied(self, text):
        # The current permission guard returns before chat feedback is sent.
        # Read a bounded window and assert that no command response appears.
        before = len(self.messages)
        self.send(0x04, string(text))
        deadline = time.monotonic() + 0.35
        while time.monotonic() < deadline:
            try:
                self.read(deadline - time.monotonic())
            except (TimeoutError, socket.timeout):
                break
        # A join advancement broadcast can arrive independently of commands.
        new_messages = [message for message in self.messages[before:]
                        if "has made the advancement" not in message]
        time.sleep(0.16)
        return new_messages

    def slot(self, slot_id, expected):
        self.until(lambda *_: self.slots.get((0, slot_id)) == expected,
                   f"craft-slot-{slot_id}-{expected}")

    def creative(self, slot_id, item, count):
        self.send(0x32, struct.pack(">h", slot_id) + wire_stack(item, count))
        self.slot(slot_id, (item, count))

    def click(self, slot_id):
        self.send(0x0e, bytes([0]) + vint(0) + struct.pack(">hB", slot_id, 0)
                  + vint(0) + vint(0) + wire_stack())


def score(rcon, holder):
    return rcon.command(f"scoreboard players get {holder} e4life")


def expect(rcon, command, fragment, name):
    reply = rcon.command(command)
    check(name, fragment in reply, f"{command!r}: {reply!r} (expected {fragment!r})")
    return reply


def datapack_cases(rcon):
    listed = rcon.command("datapack list")
    for name in ("vanilla", "spinetest", "rectest", "tagtest", "modtest", "e4live"):
        check(f"datapack-listed-{name}", name in listed, listed)
    check("zip-listed", "ziponly.zip" in listed, listed)
    check("startup-load-function", score(rcon, "Loaded") == "Loaded has 1 in e4life")
    expect(rcon, "execute if block 24 80 24 minecraft:diamond_block", "Test passed", "startup-load-world-effect")
    start = score(rcon, "Tick")
    time.sleep(0.3)
    end = score(rcon, "Tick")
    check("tick-tag-repeats", start != end and "Tick has " in end, f"{start!r} -> {end!r}")
    expect(rcon, "function e4:root", "ran 2 commands", "nested-mcfunction-dispatch")
    expect(rcon, "scoreboard players get Nested e4life", "7", "nested-mcfunction-effect")
    expect(rcon, "execute if block 25 80 24 minecraft:gold_block", "Test passed", "nested-function-world-effect")
    expect(rcon, "schedule function e4:scheduled 2t", "in 2 ticks", "schedule-2t-accepted")
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        if "9" in score(rcon, "Scheduled"):
            break
        time.sleep(0.06)
    else:
        raise AssertionError("scheduled mcfunction never ran")
    check("scheduled-function-executed", "9" in score(rcon, "Scheduled"))
    expect(rcon, "execute if block 24 80 24 #e4:treasure", "Test passed", "synthetic-block-tag-match")
    expect(rcon, "execute if block 24 80 24 #tagtest:precious", "Test passed", "existing-tagtest-block-tag-match")
    expect(rcon, "function spine:hello", "ran 2 commands", "existing-spinetest-function")
    expect(rcon, "function e4zip:probe", "ran 1 commands", "zip-function-executed")
    expect(rcon, "execute if block 26 80 24 minecraft:emerald_block", "Test passed", "zip-world-effect-present")
    expect(rcon, "datapack disable e4live", "Disabled datapack 'e4live'", "datapack-disable-live")
    expect(rcon, "datapack enable e4live", "Enabled datapack 'e4live'", "datapack-enable-live")
    expect(rcon, "reload", "Reloading!", "reload-live")
    expect(rcon, "datapack list", "e4live", "boundary-commands-keep-server-live")
    expect(rcon, "counts", "recipes=", "loaded-recipe-registry")


def command_cases(rcon, client):
    expect(rcon, "help", "commands: help", "rcon-help")
    expect(rcon, "scoreboard objectives add e4cmd dummy", "e4cmd", "scoreboard-objective")
    expect(rcon, "scoreboard players set Holder e4cmd 4", "4", "scoreboard-set")
    expect(rcon, "execute store result score Stored e4cmd run execute if entity @a", "Stored", "execute-store-result-score")
    expect(rcon, "scoreboard players get Stored e4cmd", "1", "execute-stored-score-readback")
    expect(rcon, "setblock 31 80 31 minecraft:stone", "Set block at 31 80 31", "setblock-live")
    expect(rcon, "execute if block 31 80 31 minecraft:stone", "Test passed", "execute-if-block")
    expect(rcon, "execute unless block 31 80 31 minecraft:dirt", "Test passed", "execute-unless-block")
    expect(rcon, "execute if entity @a", "Test passed", "execute-if-entity")
    expect(rcon, "execute unless entity @a", "Test failed", "execute-unless-entity")
    expect(rcon, "execute positioned 31 80 31 if block ~ ~ ~ minecraft:stone", "Test passed", "execute-positioned")
    expect(rcon, "execute at @a if entity @a", "Test passed", "execute-at-selector")
    expect(rcon, "execute as @a run time query daytime", "executed 1 command(s)", "execute-as-selector")
    expect(rcon, "execute in minecraft:the_nether run setblock 33 80 33 minecraft:gold_block", "executed 1 command(s)", "execute-in-nether")
    # Current `execute if block` calls the overworld-only block_at().
    # Preserve this concrete gap as a regression boundary until it is fixed.
    expect(rcon, "execute in minecraft:the_nether if block 33 80 33 minecraft:gold_block",
           "Test failed", "execute-in-dimension-block-condition-boundary")
    expect(rcon, "forceload add 32 32", "(1 chunks)", "forceload-add")
    expect(rcon, "forceload query", "1 forceloaded", "forceload-query")
    expect(rcon, "forceload remove 32 32", "(1 chunks)", "forceload-remove")
    expect(rcon, "forceload query", "0 forceloaded", "forceload-empty")
    expect(rcon, "setblock 35 80 35 minecraft:chest", "Set block", "data-block-fixture")
    expect(rcon, "data get block 35 80 35 Items", "Storage has", "data-block-get")
    expect(rcon, "data remove block 35 80 35 Items", "Removed data", "data-block-remove")
    expect(rcon, "gamerule keepInventory true", "keepInventory", "gamerule-set")
    expect(rcon, "gamerule keepInventory", "keepInventory", "gamerule-readback")
    expect(rcon, "summon minecraft:cow 38 80 38", "Summoned", "summon-live")
    expect(rcon, "execute if entity @e[type=minecraft:cow]", "Test passed", "selector-mob-type")
    expect(rcon, "kill @e[type=minecraft:cow]", "Killed", "kill-mob-selector")
    expect(rcon, "tag @a add e4wire", "1 target(s)", "tag-player-add")
    expect(rcon, "tag @a list", "e4wire", "tag-player-readback")
    expect(rcon, "tag @a remove e4wire", "1 target(s)", "tag-player-remove")
    expect(rcon, "execute in minecraft:the_nether run tp @a 2 82 2", "executed 1 command(s)", "tp-cross-dimension-command")
    client.until(lambda *_: "minecraft:the_nether" in client.respawns, "cross-dimension-respawn")
    check("tp-cross-dimension-respawn-0x47", "minecraft:the_nether" in client.respawns)
    expect(rcon, "execute in minecraft:overworld run tp @a 4 82 4", "executed 1 command(s)", "tp-return-overworld-command")
    client.until(lambda *_: "minecraft:overworld" in client.respawns, "return-dimension-respawn")
    check("tp-return-overworld-respawn-0x47", "minecraft:overworld" in client.respawns)


def craft_cases(client, rcon):
    # These are real window=0 creative inventory updates and click transactions.
    # The result slot is calculated by the server's JSON recipe matcher.
    coal, stick, torch = COAL, STICK, TORCH
    client.creative(36, coal, 1)
    client.creative(37, stick, 1)
    client.click(36)
    client.slot(36, (0, 0))
    client.click(1)
    client.slot(1, (coal, 1))
    client.click(37)
    client.slot(37, (0, 0))
    client.click(2)
    client.slot(0, (torch, 4))
    check("rectest-shapeless-crafting-result", client.slots[(0, 0)] == (torch, 4))
    client.click(0)
    client.slot(1, (0, 0))
    client.slot(2, (0, 0))
    client.click(38)
    client.slot(38, (torch, 4))
    check("rectest-crafting-consumes-ingredients", client.slots[(0, 1)] == (0, 0) and
          client.slots[(0, 2)] == (0, 0))
    client.creative(36, coal, 1)
    client.click(36)
    client.slot(36, (0, 0))
    client.click(1)
    client.slot(0, (torch, 2))
    check("synthetic-item-tag-crafting-result", client.slots[(0, 0)] == (torch, 2))
    client.click(0)
    client.slot(1, (0, 0))
    client.click(39)
    client.slot(39, (torch, 2))
    check("synthetic-item-tag-crafting-consumed", client.slots[(0, 1)] == (0, 0))


def player_cases(client, rcon=None, *, admin):
    if admin:
        reply = client.command("time set 1234", "Set time to 1234")
        check("player-admin-override-command-allowed", "Set time to 1234" in reply, reply)
        readback = client.command("time query daytime", "The time is")
        current = int(readback.split()[-1])
        check("player-admin-override-readback", 0 <= current - 1234 < 20, readback)
        return
    for command, response in (("help", "commands:"), ("list", "players:"),
                              ("time query daytime", "The time is")):
        actual = client.command(command, response)
        check("player-allow-" + command.replace(" ", "-"), response in actual, actual)
    prior_time = int(rcon.command("time query daytime").split()[-1])
    for name, command in (("time-set", "time set night"),
                          ("setblock", "setblock 40 80 40 minecraft:diamond_block"),
                          ("scoreboard", "scoreboard objectives add illegal dummy"),
                          ("datapack", "datapack list"), ("reload", "reload")):
        actual = client.command(command, "You do not have permission to run that command")
        check("player-deny-" + name,
              "You do not have permission to run that command" in actual,
              f"unexpected feedback for {command!r}: {actual!r}")
    current_time = int(rcon.command("time query daytime").split()[-1])
    check("player-deny-time-unchanged", 0 <= (current_time - prior_time) % 24000 < 200,
          f"{prior_time} -> {current_time}")
    expect(rcon, "execute unless block 40 80 40 minecraft:diamond_block", "Test passed",
           "player-deny-setblock-unchanged")
    check("player-deny-scoreboard-unchanged", "illegal" not in rcon.command("scoreboard objectives list"))
    expect(rcon, "datapack list", "e4live", "player-deny-server-responsive")


def main():
    require(BINARY.is_file(), f"compiled-binary-missing-{BINARY}")
    (ROOT / "target/tmp").mkdir(parents=True, exist_ok=True)
    require(Path(os.environ.get("TMPDIR", "")).resolve() == (ROOT / "target/tmp").resolve(),
            "TMPDIR must be target/tmp")
    with tempfile.TemporaryDirectory(prefix="e4-16-", dir=ROOT / "target/tmp") as tmp:
        work = Path(tmp)
        first = work / "default"
        first.mkdir()
        synthetic_packs(first)
        with server(first) as (game, rcon_port):
            with Rcon(rcon_port, "intentionally-wrong") as bad:
                check("rcon-rejects-invalid-password", not bad.authorized)
            with Rcon(rcon_port, RCON_SECRET) as rcon:
                check("rcon-accepts-correct-password", rcon.authorized)
                datapack_cases(rcon)
                with WireClient(game, "E4Tester") as client:
                    client.login()
                    if client.pos is None:
                        client.until_id(0x40, "login-initial-position")
                    LOGIN_PLAY_IDS.extend(client.play_ids)
                    check("protocol-767-login", client.pos is not None)
                    # Capture play traffic through the first chunk batch and
                    # initial position. An idle server need not send another.
                    check("login-commands-packet-0x10-present", 0x10 in client.play_ids,
                          f"observed login play IDs {client.play_ids!r}")
                    player_cases(client, rcon, admin=False)
                    command_cases(rcon, client)
                    craft_cases(client, rcon)

        override = work / "override"
        override.mkdir()
        synthetic_packs(override)
        with server(override, player_admin=True) as (game, _):
            with WireClient(game, "E4AdminTester") as client:
                client.login()
                if client.pos is None:
                    client.until_id(0x40, "admin-login-initial-position")
                player_cases(client, admin=True)

    print(json.dumps({"suite": "E4-16", "protocol": 767, "binary": str(BINARY),
                      "sha256": hashlib.sha256(BINARY.read_bytes()).hexdigest(),
                      "passed": len(RESULTS), "failed": 0, "cases": RESULTS,
                      "login_play_ids_hex": [f"0x{pid:02x}" for pid in LOGIN_PLAY_IDS]},
                     sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"E4-16 FAIL after {len(RESULTS)} passes: {error}", file=sys.stderr)
        raise
