#!/usr/bin/env python3
"""Loopback protocol 767 and RCON coverage for commands, operators and ZIP packs."""

import contextlib
import io
import json
import os
import socket
import struct
import subprocess
import sys
import tempfile
import time
import zipfile
from pathlib import Path

from protocol767 import ROOT, Client, Reader, string, vint
from wire_smoke import port, rcon_frame, rcon_response, status

COMMANDS = 0x11
SUGGEST = 0x0B
SUGGESTIONS = 0x10
PUBLIC_COMMANDS = {"help", "list", "me", "msg", "tell", "time", "w",
                   "teammsg", "tm", "trigger"}
PLAYER = "BrigadierProbe"
SECRET = "local-brigadier-wire-test"


@contextlib.contextmanager
def server(binary, root):
    game, rcon = port(), port()
    while rcon == game:
        rcon = port()
    env = dict(
        os.environ,
        MEGU_PORT=str(game),
        MEGU_BIND_ADDR="127.0.0.1",
        MEGU_RCON_PORT=str(rcon),
        MEGU_RCON_BIND_ADDR="127.0.0.1",
        MEGU_RCON_PASS=SECRET,
        MEGU_WG="old",
        MEGU_RESOURCE_PACK_URL="",
    )
    env.pop("MEGU_ALLOW_PLAYER_ADMIN_COMMANDS", None)
    with (root / "server.log").open("w+") as log:
        proc = subprocess.Popen(
            [str(binary)], cwd=root, env=env, stdout=log, stderr=log
        )
        try:
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                assert proc.poll() is None, "server exited at startup"
                try:
                    status(game)
                    with socket.create_connection(("127.0.0.1", rcon), timeout=2):
                        break
                except OSError:
                    time.sleep(0.05)
            else:
                raise AssertionError("listeners did not become ready")
            yield game, rcon
        except Exception:
            log.seek(0)
            print(log.read()[-6000:], file=sys.stderr)
            raise
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)


def rcon_command(port_number, text):
    with socket.create_connection(("127.0.0.1", port_number), timeout=5) as connection:
        connection.sendall(rcon_frame(1, 3, SECRET))
        assert rcon_response(connection) == struct.pack("<ii", 1, 2) + b"\0\0"
        connection.sendall(rcon_frame(2, 2, text))
        reply = rcon_response(connection)
        assert reply[:8] == struct.pack("<ii", 2, 0) and reply.endswith(b"\0\0"), reply
        return reply[8:-2].decode()


def decode_commands(body):
    reader = Reader(body)
    count = reader.varint()
    assert 1 <= count < 1024, count
    nodes = []
    for _ in range(count):
        flags = reader.number("B")
        kind = flags & 3
        assert flags <= 0x1F and kind != 3, flags
        child_count = reader.varint()
        assert child_count < count, child_count
        children = [reader.varint() for _ in range(child_count)]
        assert len(children) == len(set(children)) and all(i < count for i in children)
        assert not flags & 8 or reader.varint() < count
        name = reader.string() if kind != 0 else ""
        parser = None
        if kind == 2:
            parser = reader.varint()
            assert parser in range(6), parser
            if parser in (1, 2, 3, 4):
                bounds = reader.number("B")
                assert bounds <= 3
                for bit in (1, 2):
                    if bounds & bit:
                        reader.number({1: ">f", 2: ">d", 3: ">i", 4: ">q"}[parser])
            elif parser == 5:
                assert reader.varint() in range(3)
            if flags & 0x10:
                assert reader.string() == "minecraft:ask_server"
        else:
            assert not flags & 0x10
        nodes.append((name, kind, bool(flags & 4), parser, children))
    root = reader.varint()
    assert reader.pos == len(body) and root < count and nodes[root][1] == 0
    seen = set()

    def decode(index):
        assert index not in seen, "duplicate or cyclic command node"
        seen.add(index)
        name, kind, executable, parser, children = nodes[index]
        child_nodes = dict(decode(i) for i in children)
        assert len(child_nodes) == len(children), f"duplicate child name under {name!r}"
        return name, (kind, executable, parser, child_nodes)

    _, (_, _, _, tree) = decode(root)
    assert len(seen) == count, "unreachable command node"
    return tree


class CommandClient(Client):
    play_prefix = None
    initial_tree = None

    def read(self, timeout):
        pid, body = super().read(timeout)
        if self.phase == "play" and self.initial_tree is None:
            if self.play_prefix is None:
                self.play_prefix = []
            self.play_prefix.append(pid)
            if pid == COMMANDS:
                self.initial_tree = decode_commands(body)
        return pid, body

    def login(self):
        super().login()
        # Join Game, ServerData, then the vanilla view-distance pair
        # (SetChunkCacheRadius, SetSimulationDistance), then the command tree.
        assert self.play_prefix == [0x2B, 0x4B, 0x55, 0x62, COMMANDS], self.play_prefix
        self.until_id(0x40)

    def suggest(self, transaction, text):
        self.send(SUGGEST, vint(transaction) + string(text))
        reader = Reader(self.until_id(SUGGESTIONS, f"suggestions for {text!r}"))
        assert reader.varint() == transaction, "suggestion transaction mismatch"
        start, length, count = reader.varint(), reader.varint(), reader.varint()
        assert start + length <= len(text.encode("utf-16-le")) // 2 and count < 1024
        suggestions = set()
        for _ in range(count):
            suggestions.add(reader.string())
            assert reader.number("B") == 0, "unexpected suggestion tooltip"
        assert len(suggestions) == count and reader.pos == len(reader.data)
        return start, length, suggestions

    def apply_command(self, text, expected, packet=0x04):
        before = len(self.messages)
        if packet == 0x06:
            self.chat("/" + text)
        else:
            suffix = (
                struct.pack(">qq", int(time.time() * 1000), 0) + bytes(5)
                if packet == 0x05
                else b""
            )
            self.send(packet, string(text) + suffix)
        self.until(
            lambda *_: expected in self.messages[before:],
            f"chat command {text!r}: {expected!r}",
        )

    def next_tree(self):
        return decode_commands(self.until_id(COMMANDS, "updated command tree"))


def assert_tree(tree, admin):
    assert {"help", "list", "time"} <= tree.keys(), tree.keys()
    assert tree["help"][1] and tree["list"][1]
    time_subcommands = tree["time"][3]
    assert {"query", "get"} <= time_subcommands.keys()
    assert set(time_subcommands["query"][3]) == {"day", "daytime", "gametime"}
    if admin:
        assert {
            "op",
            "deop",
            "reload",
            "datapack",
            "give",
            "scoreboard",
            "stop",
        } <= tree.keys()
        assert tree["op"][3]["player"][:3] == (2, True, 5)
        assert set(tree["datapack"][3]) == {"list", "enable", "disable"}
        assert time_subcommands["set"][3]["ticks"][:3] == (2, True, 4)
    else:
        assert set(tree) == PUBLIC_COMMANDS, tree.keys()
        assert set(time_subcommands) == {"query", "get"}


def assert_initial_permissions(client, rcon):
    assert_tree(client.initial_tree, False)
    assert client.suggest(1, "/") == (1, 0, PUBLIC_COMMANDS)
    assert client.suggest(2, "time q") == (5, 1, {"query"})
    assert client.suggest(3, "/time s")[2] == set()
    client.send(SUGGEST, vint(4) + string("/op ") + b"\0")
    assert client.suggest(5, "/hel") == (1, 3, {"help"})
    denied = "You do not have permission to run that command"
    client.apply_command("time set 12345", denied)
    client.apply_command(f"op {PLAYER}", denied)
    assert rcon_command(rcon, "gamerule doDaylightCycle false").endswith("false")
    before = rcon_command(rcon, "time query gametime")
    client.apply_command("time set 54321", denied, 0x05)
    assert rcon_command(rcon, "time query gametime") == before
    assert rcon_command(rcon, f"op {PLAYER}") == f"Made {PLAYER} a server operator"
    assert_tree(client.next_tree(), True)
    assert client.suggest(6, "/op Brig") == (4, 4, {PLAYER})
    assert client.suggest(7, "/time set mi") == (10, 2, {"midnight"})
    assert client.suggest(8, "/datapack d") == (10, 1, {"disable"})
    client.apply_command("time set 12345", "Set time to 12345", 0x05)
    assert rcon_command(rcon, "time query gametime") == "The gametime is 12345"


def create_zip(root):
    class StreamingArchive(io.BytesIO):
        def seekable(self):
            return False

        def seek(self, *args):
            raise io.UnsupportedOperation("streaming ZIP")

    packs = root / "datapacks"
    packs.mkdir()
    stream = StreamingArchive()
    with zipfile.ZipFile(stream, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        archive.writestr(
            "pack.mcmeta", '{"pack":{"pack_format":48,"description":"wire test"}}'
        )
        archive.writestr(
            "data/minecraft/tags/function/load.json", '{"values":["zip:load"]}'
        )
        archive.writestr(
            "data/minecraft/tags/function/tick.json", '{"values":["zip:tick"]}'
        )
        archive.writestr(
            "data/zip/function/load.mcfunction",
            "scoreboard objectives add zip_ticks dummy\n"
            "scoreboard players set startup zip_ticks 701\n"
            "scoreboard players set heartbeat zip_ticks 0\n",
        )
        archive.writestr(
            "data/zip/function/tick.mcfunction",
            "scoreboard players add heartbeat zip_ticks 1\n",
        )
        archive.writestr(
            "data/zip/function/ping.mcfunction",
            "scoreboard players set called zip_ticks 767\n",
            compress_type=zipfile.ZIP_STORED,
        )
    (packs / "wire.zip").write_bytes(stream.getvalue())


def until_value(read, target):
    deadline = time.monotonic() + 5
    while True:
        value = read()
        if value >= target:
            return value
        assert time.monotonic() < deadline, f"last value {value} did not reach {target}"
        time.sleep(0.05)


def exercise_datapack(rcon):
    def command(text):
        return rcon_command(rcon, text)

    def score(holder):
        player, has, value, in_word, objective = command(
            f"scoreboard players get {holder} zip_ticks"
        ).split()
        assert (player, has, in_word, objective) == (holder, "has", "in", "zip_ticks")
        return int(value)

    def game_time():
        return int(command("time query gametime").split()[-1])

    assert command("datapack list") == "1 packs: wire.zip"
    assert score("startup") == 701
    until_value(lambda: score("heartbeat"), 2)
    assert command("function zip:ping") == "ran 1 commands"
    assert score("called") == 767

    assert command("datapack disable absent") == "Unknown datapack 'absent'"
    assert command("datapack disable wire.zip").startswith("Disabled datapack")
    assert command("datapack list enabled") == "0 packs: "
    assert command("datapack list available") == "1 packs: wire.zip"
    assert command("function zip:ping") == "ran 0 commands"
    assert command("reload") == "Reloading! (0 pack(s))"
    command("gamerule doDaylightCycle true")
    until_value(game_time, game_time() + 2)
    stopped = score("heartbeat")
    until_value(game_time, game_time() + 4)
    assert score("heartbeat") == stopped, "a disabled pack kept ticking"

    command("scoreboard players set startup zip_ticks 0")
    assert command("datapack enable wire.zip").startswith("Enabled datapack")
    assert command("datapack list available") == "0 packs: "
    assert score("startup") == 701
    until_value(lambda: score("heartbeat"), 2)
    command("scoreboard players set startup zip_ticks 0")
    assert command("reload") == "Reloading! (1 pack(s))"
    assert score("startup") == 701
    until_value(lambda: score("heartbeat"), 2)


def main(binary, root):
    (root / "config_packets.bin").symlink_to(ROOT / "config_packets.bin")
    (root / "assets").symlink_to(ROOT / "assets", target_is_directory=True)
    create_zip(root)
    with server(binary, root) as (game, rcon), CommandClient(game, PLAYER) as client:
        client.login()
        assert_initial_permissions(client, rcon)
        exercise_datapack(rcon)
        # Both explicit reloads above resend the command tree to the online OP.
        assert_tree(client.next_tree(), True)
        assert_tree(client.next_tree(), True)
        ops_entries = json.loads((root / "ops.json").read_text())
        assert [
            (entry["name"].lower() if isinstance(entry, dict) else str(entry).lower())
            for entry in ops_entries
        ] == [PLAYER.lower()]
    with server(binary, root) as (game, rcon), CommandClient(game, PLAYER) as client:
        client.login()
        assert_tree(client.initial_tree, True)
        client.apply_command("time set 23456", "Set time to 23456")
        assert rcon_command(rcon, f"deop {PLAYER.lower()}").endswith(
            "no longer a server operator"
        )
        assert_tree(client.next_tree(), False)
        assert client.suggest(9, "/time s")[2] == set()
        client.apply_command(
            "time set 31337", "You do not have permission to run that command"
        )
        assert json.loads((root / "ops.json").read_text()) == []
        (root / "ops.json").write_text(json.dumps([PLAYER]))
        assert rcon_command(rcon, "reload") == "Reloading! (1 pack(s))"
        assert_tree(client.next_tree(), True)
        client.apply_command("time set 31337", "Set time to 31337")
    print("command Brigadier, operators and ZIP datapacks protocol 767: PASS")


if __name__ == "__main__":
    binary = (
        Path(sys.argv[1]).resolve()
        if len(sys.argv) > 1
        else ROOT / "target/debug/megu"
    )
    with tempfile.TemporaryDirectory(prefix="spine-brigadier-wire-") as directory:
        main(binary, Path(directory))
