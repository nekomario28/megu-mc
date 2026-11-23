#!/usr/bin/env python3
"""Exercise live GP-1 goals and GP-6 effects with a protocol 767 client."""
import math
import os
from pathlib import Path
import struct
import sys
import time

from protocol767 import Client, Reader, local_server, require, string, vint


ROOT = Path(__file__).resolve().parents[1]
FLOOR = 96
Y = FLOOR + 1


class MobClient(Client):
    def __init__(self, game):
        super().__init__(game, "MobWire")
        self.eid = None
        self.spawns = {}
        self.moves = {}
        self.removed = set()
        self.heads = set()
        self.metadata = {}
        self.hurt = set()
        self.velocity = set()
        self.health = 20.0
        self.last_command = 0.0

    def read(self, timeout):
        pid, body = super().read(timeout)
        if self.phase != "play":
            return pid, body
        r = Reader(body)
        if pid == 0x2b:
            self.eid = r.number(">i")
        elif pid == 0x01:
            eid = r.varint()
            r.take(16)
            etype = r.varint()
            self.spawns[eid] = etype, struct.unpack(">ddd", r.take(24))
        elif pid == 0x70:
            eid = r.varint()
            self.moves.setdefault(eid, []).append(struct.unpack(">ddd", r.take(24)))
        elif pid in (0x2e, 0x2f):
            # Relative entity move/rotate: vanilla i16 deltas in 1/4096 blocks.
            eid = r.varint()
            dx, dy, dz = struct.unpack(">hhh", r.take(6))
            if pid == 0x2f:
                r.take(2)  # yaw/pitch
            r.take(1)      # on_ground
            path = self.moves.setdefault(eid, [])
            last = path[-1] if path else self.spawns.get(eid, (0, (0.0, 0.0, 0.0)))[1]
            path.append((last[0] + dx / 4096, last[1] + dy / 4096, last[2] + dz / 4096))
        elif pid == 0x42:
            self.removed.update(r.varint() for _ in range(r.varint()))
        elif pid == 0x48:
            self.heads.add(r.varint())
        elif pid == 0x58:
            eid = r.varint()
            self.metadata.setdefault(eid, set()).add(body[r.pos:])
        elif pid == 0x24:
            self.hurt.add(r.varint())
        elif pid == 0x5a:
            eid = r.varint()
            if struct.unpack(">hhh", r.take(6)) != (0, 0, 0):
                self.velocity.add(eid)
        elif pid == 0x5d:
            self.health = r.number(">f")
        return pid, body

    def command(self, command, message=None):
        # ChatThrottle drains 20 points per second and each command costs 20.
        time.sleep(max(0.0, self.last_command + 1.0 - time.monotonic()))
        self.last_command = time.monotonic()
        before = len(self.messages)
        # Serverbound chat_command (0x04): plain chat is relayed, not executed.
        self.send(0x04, string(command.lstrip("/")))
        if message:
            self.until(lambda *_: any(message in reply for reply in self.messages[before:]),
                       "command-" + command)

    def summon(self, name, etype, x, z):
        before = set(self.spawns)
        self.command(f"summon minecraft:{name} {x} {Y} {z}", f"Summoned minecraft:{name}")
        self.until(lambda *_: any(eid not in before and kind == etype
                                  for eid, (kind, _) in self.spawns.items()),
                   "spawn-" + name)
        return next(eid for eid, (kind, _) in self.spawns.items()
                    if eid not in before and kind == etype)

    def kill(self, eid, name):
        self.command(f"kill @e[type=minecraft:{name}]")
        self.until(lambda *_: eid in self.removed, "removed-" + name)


def zombie(client):
    client.command(f"fill 0 {Y} -2 0 {Y} 2 stone")
    client.until(lambda *_: all(any(p == (0, Y, z) and st != 0 for p, st in client.updates)
                                for z in range(-2, 3)), "zombie-wall-on-wire")
    eid = client.summon("zombie", 124, -4.5, 0.5)
    client.until(lambda *_: client.health < 20.0 and client.eid in client.velocity,
                 "zombie-reached-player-with-hurt-health-and-knockback")
    path = client.moves.get(eid, [])
    require(client.spawns[eid][1][0] < 0.0 and any(x > 1.0 for x, _, _ in path),
            "zombie-crossed-barrier")
    require(any(y >= Y + 0.9 or not -2.0 < z < 3.0 for _, y, z in path),
            "zombie-stepped-over-or-detoured")
    require(client.eid in client.hurt and eid in client.heads, "zombie-hurt-and-head-rotation")
    client.kill(eid, "zombie")
    client.command(f"fill 0 {Y} -2 0 {Y} 2 air")
    print("GP-1 zombie navigated the wall and struck the player")


def cow(client):
    # Supply wheat in the selected hotbar slot through the creative inventory packet.
    client.command("gamemode creative")
    client.send(0x32, struct.pack(">h", 36) + vint(1) + vint(854) + b"\0\0")
    client.until(lambda pid, body: pid == 0x15 and body.endswith(vint(1) + vint(854) + b"\0\0"),
                 "wheat-in-held-slot")
    eid = client.summon("cow", 22, -3.5, 1.5)
    initial = math.dist(client.spawns[eid][1], client.pos)
    client.until(lambda *_: any(math.dist(p, client.pos) < initial - 1.5
                                for p in client.moves.get(eid, [])) and eid in client.heads,
                 "cow-tempted-toward-held-wheat")
    client.kill(eid, "cow")
    client.command("gamemode survival")
    print("GP-1 cow followed held wheat and rotated its head")


def slime(client):
    eid = client.summon("slime", 93, -3.5, -3.5)
    client.until(lambda *_: bytes([16, 1, 4, 255]) in client.metadata.get(eid, ()),
                 "large-slime-size")
    client.kill(eid, "slime")
    children = lambda: [child for child, (kind, _) in client.spawns.items()
                        if child != eid and kind == 93]
    client.until(lambda *_: len(children()) >= 2 and all(
        bytes([16, 1, 2, 255]) in client.metadata.get(child, ()) for child in children()),
        "slime-split-children-with-medium-size")
    require(2 <= len(children()) <= 4, "slime-split-count")
    print("GP-6 large slime split into medium slimes")


def enderman(client):
    x, z = -5.5, 5.5
    eid = client.summon("enderman", 33, x, z)
    client.command(f"fill -7 {Y} 4 -5 {Y} 6 water")

    def escaped():
        last = (x, Y, z)
        for point in client.moves.get(eid, ()):
            px, _, pz = point
            if math.dist(point, last) > 3.0 and not (-7 <= px < -4 and 4 <= pz < 7):
                return True
            last = point
        return False

    client.until(lambda *_: escaped() and any(
        point == (math.floor(x), Y, math.floor(z)) and 80 <= state <= 95
        for point, state in client.updates
    ), "enderman-water-teleport-outside-pool")
    client.kill(eid, "enderman")
    print("GP-6 Enderman teleported out of water")


def blaze(client):
    initial = client.health
    client.hurt.discard(client.eid)
    eid = client.summon("blaze", 8, -4.5, 0.5)
    client.until(lambda *_: bytes([16, 0, 1, 255]) in client.metadata.get(eid, ()),
                 "blaze-charge-flag")
    client.until(lambda *_: any(kind == 94 for kind, _ in client.spawns.values()),
                 "blaze-small-fireball")
    client.until(lambda *_: client.eid in client.hurt and client.health < initial,
                 "blaze-fireball-hit-player")
    client.kill(eid, "blaze")
    print("GP-6 Blaze charged, launched a small fireball, and damaged the player")


def piglin(client):
    eid = client.summon("piglin", 78, -4.5, -4.5)
    def converted():
        last_position = client.moves.get(eid, [client.spawns[eid][1]])[-1]
        return eid in client.removed and any(
            kind == 127 and math.dist(pos, last_position) < 1.0
            for kind, pos in client.spawns.values()
        )

    client.until(lambda *_: converted(), "piglin-converted-in-overworld")
    print("GP-6 Piglin converted into a zombified piglin")


def main(binary):
    os.environ["MEGU_ALLOW_PLAYER_ADMIN_COMMANDS"] = "1"
    os.environ.setdefault("MC_PROBE_DEADLINE", "30")
    with local_server(binary) as game, MobClient(game) as client:
        client.login()
        if client.pos is None:
            client.until_id(0x40, "initial-player-position")
        client.command("gamerule doMobSpawning false", "doMobSpawning is now set to: false")
        client.command("gamerule doDaylightCycle false", "doDaylightCycle is now set to: false")
        client.command("difficulty normal")
        client.command("time set 18000")
        client.command("gamerule doWeatherCycle false")
        client.command("weather clear")
        client.command(f"fill -24 {FLOOR} -24 24 {FLOOR} 24 stone")
        client.command(f"tp 3.5 {Y} 0.5")
        client.until(lambda *_: client.pos == (3.5, Y, 0.5), "teleport-to-mob-arena")
        client.command("gamemode survival")
        for scenario in (zombie, cow, slime, enderman, blaze, piglin):
            scenario(client)
    print("gp1/gp6 wire: PASS")


if __name__ == "__main__":
    main(Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else ROOT / "target/debug/megu")
