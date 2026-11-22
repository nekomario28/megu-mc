#!/usr/bin/env python3
"""Check the live Nether spawn census and Protocol 767 Add Entity packets."""

import contextlib
import math
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time

from command_brigadier_ops_wire import SECRET
from command_datapack_wire import Rcon
from protocol767 import Client, ROOT, Reader
from wire_smoke import port, status

NETHER = "minecraft:the_nether"
# Seed 0, the soul_sand_valley around quart (880, *, 1610) in the pinned biome fixture.
X, Y, Z = 3520, 160, 6440
FLOOR = Y - 1
VALLEY_MONSTERS = {33, 45, 91}  # enderman, ghast, skeleton (protocol 767)
STRIDER = 103


@contextlib.contextmanager
def server(binary, root):
    for name in ("assets", "config_packets.bin", "datapacks"):
        (root / name).symlink_to(ROOT / name)
    game, rcon = port(), port()
    while rcon == game:
        rcon = port()
    env = dict(os.environ, MEGU_PORT=str(game), MEGU_BIND_ADDR="127.0.0.1",
               MEGU_RCON_PORT=str(rcon), MEGU_RCON_BIND_ADDR="127.0.0.1",
               MEGU_RCON_PASS=SECRET, MEGU_WG="vanilla", MEGU_SEED="0",
               MEGU_RESOURCE_PACK_URL="", MEGU_TEST_NETHER_SPAWN_EVERY_TICK="1",
               MEGU_NOFEAT="1", MEGU_NOCARVE="1")
    with (root / "server.log").open("w+") as log:
        with subprocess.Popen([str(binary)], cwd=root, env=env, stdout=log, stderr=log) as process:
            try:
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    assert process.poll() is None, "server exited during startup"
                    try:
                        status(game)
                        with socket.create_connection(("127.0.0.1", rcon), timeout=0.5):
                            break
                    except OSError:
                        time.sleep(0.05)
                else:
                    raise AssertionError("server startup deadline")
                yield game, rcon
            except Exception:
                log.seek(0)
                print(log.read()[-6000:], file=sys.stderr)
                raise
            finally:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()


class NetherClient(Client):
    def __init__(self, game):
        super().__init__(game, "NetherWire")
        self.spawns = {}

    def read(self, timeout):
        pid, body = super().read(timeout)
        if self.phase == "play" and self.dimension_name == NETHER and pid == 0x01:
            reader = Reader(body)
            eid = reader.varint()
            reader.take(16)
            kind = reader.varint()
            self.spawns.setdefault(eid, (kind, tuple(reader.number(">d") for _ in range(3))))
        return pid, body

    def observe(self, seconds):
        deadline = time.monotonic() + seconds
        while (remaining := deadline - time.monotonic()) > 0:
            try:
                self.read(remaining)
            except TimeoutError:
                break


def exercise(game, rcon):
    def command(text, expected):
        with Rcon(rcon, SECRET) as client:
            client.sock.settimeout(30)
            reply = client.command(text)
        assert expected in reply, (text, reply)
        return reply

    def nether(text):
        command(f"execute in {NETHER} run {text}", "executed 1 command(s)")

    command("gamerule doMobSpawning false", "false")
    command("time set noon", "6000")
    # Dark platforms on the roof provide open ground around the 32-block spawn ring.
    for dx, dz in [(32, 0), (-32, 0), (0, 32), (0, -32)]:
        x, z = X + dx, Z + dz
        nether(f"fill {x-7} {FLOOR} {z-7} {x+7} {FLOOR} {z+7} netherrack")
    nether(f"fill {X-2} {FLOOR-3} {Z-2} {X+2} {FLOOR-3} {Z+2} netherrack")
    # Charges below the arena cannot collide with its natural spawn candidates and
    # are creatures, so they cannot fill the monster cap.
    for _ in range(24):
        nether(f"summon minecraft:strider {X+0.5} {FLOOR-2} {Z+0.5}")
    with NetherClient(game) as client:
        client.login(view=2)
        client.until_id(0x40, "Overworld login teleport")
        command(f"gamemode creative {client.name}", "creative")
        nether(f"tp {client.name} {X+0.5} {Y} {Z+0.5}")
        client.until_id(0x47, "Nether respawn packet")
        assert client.dimension_name == NETHER
        client.until_id(0x40, "Nether teleport")
        client.until(lambda *_: len(client.chunks) >= 25, "Nether chunks")
        client.until(lambda *_: len(client.spawns) >= 24, "summoned striders visible")
        seeds = set(client.spawns)
        assert len(seeds) == 24 and {kind for kind, _ in client.spawns.values()} == {STRIDER}, client.spawns

        def game_tick():
            return int(command("time query gametime", "The gametime is ").split()[-1])

        command("gamerule doMobSpawning true", "true")
        start = game_tick()
        deadline = time.monotonic() + 45
        while game_tick() - start < 100:
            assert time.monotonic() < deadline, "insufficient live spawn cycles"
            client.observe(2)
            assert set(client.spawns) == seeds, "natural spawn despite the soul_sand_valley budget"

        command("gamerule doMobSpawning false", "false")
        client.observe(0.5)
        assert set(client.spawns) == seeds, "natural spawn despite the soul_sand_valley budget"
        command("kill @e[type=minecraft:strider]", "Killed 24")
        command("difficulty peaceful", "peaceful")
        command("gamerule doMobSpawning true", "true")
        start = game_tick()
        deadline = time.monotonic() + 45
        while game_tick() - start < 100:
            assert time.monotonic() < deadline, "insufficient live peaceful spawn cycles"
            client.observe(2)
        assert not any(
            eid not in seeds and kind in VALLEY_MONSTERS
            for eid, (kind, _) in client.spawns.items()
        ), "natural Nether monster on Peaceful"

        seeds = set(client.spawns)
        command("difficulty normal", "normal")
        client.until(lambda *_: bool(set(client.spawns) - seeds), "natural Add Entity after removing charges")
        client.observe(5)
        natural = [spawn for eid, spawn in client.spawns.items() if eid not in seeds]
        assert {kind for kind, _ in natural} <= VALLEY_MONSTERS, natural
        for _, pos in natural:
            assert Y - 1 <= pos[1] <= Y + 1, pos
            assert 24.0 < math.dist(pos, (X + 0.5, Y, Z + 0.5)) <= 128.0, pos
        print(f"GP-2 Nether wire PASS: 0 natural spawns with 24 charges, {len(natural)} after removal")


if __name__ == "__main__":
    binary = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else ROOT / "target/debug/megu"
    os.environ.setdefault("MC_PROBE_DEADLINE", "30")
    with tempfile.TemporaryDirectory(prefix="spine-gp2-wire-") as directory:
        with server(binary, Path(directory)) as ports:
            exercise(*ports)
