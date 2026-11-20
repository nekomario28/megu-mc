#!/usr/bin/env python3
"""Live GP-5 and GP-7 command, world and player-state contracts on protocol 767."""
import math
import os
from pathlib import Path
import sys
import time

from protocol767 import Client, ContractFailure, Reader, WIRE_BLOCKS, local_server, require, signed, string

BLOCK = {block["name"]: block["defaultState"] for block in WIRE_BLOCKS}


class GameplayClient(Client):
    def __init__(self, game, name):
        super().__init__(game, name)
        self.play = {}

    def read(self, timeout):
        pid, body = super().read(timeout)
        if self.phase == "play":
            self.play.setdefault(pid, []).append(body)
        return pid, body

    def command(self, text, feedback, packet_id=None):
        before = len(self.messages)
        index = len(self.play.get(packet_id, ()))
        self.send(0x04, string(text))
        self.until(lambda *_: len(self.messages) > before, f"feedback-{text}")
        actual = self.messages[before]
        require(actual.startswith(feedback), f"feedback-{text}: {actual!r}")
        if packet_id is not None:
            return Reader(self.await_packet(packet_id, index))

    def await_packet(self, packet_id, index=0, contract=None):
        packets = self.play.setdefault(packet_id, [])
        if len(packets) <= index:
            self.until(lambda *_: len(packets) > index, contract or f"play-packet-{packet_id:#04x}")
        return packets[index]

    def block_changes(self, expected):
        def matches(*_):
            return all(self.block(*pos) == state for pos, state in expected.items())

        if not matches():
            self.until(matches, f"world-blocks-{expected}")

    def track_health(self, predicate, contract, start=0):
        index = start
        while True:
            value = decode_health(self.await_packet(0x5D, index, contract))
            if predicate(value):
                return value
            index += 1


def end(reader, contract):
    require(reader.pos == len(reader.data), f"{contract}-complete")


def decode_border(body):
    reader = Reader(body)
    values = tuple(reader.number(">d") for _ in range(4))
    values += (reader.varlong(),) + tuple(reader.varint() for _ in range(3))
    end(reader, "initialize-border")
    return values


def borders(game):
    with GameplayClient(game, "BorderTester") as client:
        client.login()
        actual = decode_border(client.await_packet(0x25))
        require(actual == (0, 0, 59999968, 59999968, 0, 29999984, 5, 15), "border-default")
        reader = client.command("worldborder set 128", "Set the world border to 128", 0x4F)
        require(reader.number(">d") == 128, "border-size")
        end(reader, "border-size")

        reader = client.command("worldborder center 12.5 -8.25", "Set the world border center to", 0x4D)
        require((reader.number(">d"), reader.number(">d")) == (12.5, -8.25), "border-center")
        end(reader, "border-center")
        for kind, value, packet_id in (("distance", 9, 0x51), ("time", 20, 0x50)):
            reader = client.command(f"worldborder warning {kind} {value}",
                                    f"Set the world border warning {kind} to {value}", packet_id)
            require(reader.varint() == value, f"border-warning-{kind}")
            end(reader, "border-warning")

        reader = client.command("worldborder set 64 1", "Resizing the world border to 64", 0x4E)
        require((reader.number(">d"), reader.number(">d"), reader.varlong()) == (128, 64, 1000),
                "border-lerp")
        end(reader, "border-lerp")
        time.sleep(1.1)
        client.command("worldborder get", "The world border is 64 blocks wide")
        with GameplayClient(game, "BorderObserver") as observer:
            observer.login()
            require(decode_border(observer.await_packet(0x25)) ==
                    (12.5, -8.25, 64, 64, 0, 29999984, 9, 20), "border-state-on-join")


def read_objective(reader, mode):
    require((reader.string(), reader.number("B")) == ("lane", mode), "objective-name-and-mode")
    if mode == 0:
        require((reader.nbt(), reader.varint(), reader.number("B")) == ("Lane Score", 0, 0),
                "objective-display-render-and-format")
    end(reader, "objective")


def scoreboard(game):
    with GameplayClient(game, "ScoreTester") as client:
        client.login()
        read_objective(client.command("scoreboard objectives add lane dummy Lane Score",
                                      "Added objective lane", 0x5E), 0)
        reader = client.command("scoreboard players set @s lane -7", "ScoreTester has -7 in lane", 0x61)
        require((reader.string(), reader.string(), signed(reader.varint(), 32)) ==
                ("ScoreTester", "lane", -7), "score-value")
        require((reader.number("B"), reader.number("B")) == (0, 0), "score-optional-fields")
        end(reader, "score")

        reader = client.command("scoreboard objectives setdisplay sidebar lane", "Displaying lane", 0x57)
        require((reader.varint(), reader.string()) == (1, "lane"), "scoreboard-sidebar")
        end(reader, "scoreboard-sidebar")
        with GameplayClient(game, "ScoreObserver") as observer:
            observer.login()
            read_objective(Reader(observer.await_packet(0x5E)), 0)
            require(observer.await_packet(0x61) == client.play[0x61][-1], "score-state-on-join")
            require(observer.await_packet(0x57) == client.play[0x57][-1], "sidebar-state-on-join")

        reader = client.command("scoreboard players reset ScoreTester lane", "Reset 1 scores", 0x44)
        require((reader.string(), reader.number("B"), reader.string()) == ("ScoreTester", 1, "lane"),
                "score-reset")
        end(reader, "score-reset")
        read_objective(client.command("scoreboard objectives remove lane", "Removed objective lane",
                                      0x5E), 1)


def advancement_update(reader):
    reset = bool(reader.number("B"))
    definitions = {}
    for _ in range(reader.varint()):
        name = reader.string()
        parent = reader.string() if reader.number("B") else None
        require(reader.number("B") == 0, "core-advancement-display-absent")
        requirements = [[reader.string() for _ in range(reader.varint())]
                        for _ in range(reader.varint())]
        require(reader.number("B") == 1, "core-advancement-telemetry")
        definitions[name] = parent, requirements
    require(reader.varint() == 0, "advancement-empty-removals")
    progress = {}
    for _ in range(reader.varint()):
        name = reader.string()
        criteria = {}
        for _ in range(reader.varint()):
            criterion = reader.string()
            criteria[criterion] = reader.number(">q") if reader.number("B") else None
        progress[name] = criteria
    end(reader, "advancement")
    return reset, definitions, progress


def advancements(game):
    root = "minecraft:story/root"
    with GameplayClient(game, "AdvTester") as client:
        client.login()
        reset, definitions, progress = advancement_update(Reader(client.await_packet(0x74)))
        require(reset and not progress and definitions[root] == (None, [["crafting_table"]]),
                "advancement-initial-definitions")
        earliest = int(time.time() * 1000) - 1000
        reader = client.command("advancement grant @s only story/root", "Granted 1 advancement", 0x74)
        reset, definitions, progress = advancement_update(reader)
        timestamp = progress.get(root, {}).get("crafting_table")
        require(not reset and not definitions and timestamp is not None
                and earliest <= timestamp <= int(time.time() * 1000) + 1000,
                "advancement-grant-timestamp")
        reader = client.command("advancement revoke @s only story/root crafting_table",
                                "Revoked 1 advancement", 0x74)
        require(advancement_update(reader) == (False, {}, {root: {"crafting_table": None}}),
                "advancement-revoked-criterion")


def modifications(game):
    with GameplayClient(game, "BlockTester") as client:
        client.login()
        if client.pos is None:
            client.await_packet(0x40)
        x, _, z = map(math.floor, client.pos)
        y = 100
        needed = {(bx // 16, z // 16) for bx in range(x - 1, x + 8)}
        needed |= {(bx // 16, (z + 2) // 16) for bx in range(x, x + 3)}
        if not needed.issubset(client.chunks):
            client.until(lambda *_: needed.issubset(client.chunks), "modification-chunks-loaded")

        src, second, center = (x, y, z), (x + 1, y, z), (x + 1, y + 1, z + 1)
        target, next_target = (x + 5, y, z), (x + 6, y, z)
        box = f"{x} {y} {z} {x + 2} {y + 2} {z + 2}"
        stone, gold, air = BLOCK["stone"], BLOCK["gold_block"], BLOCK["air"]

        client.command(f"fill {box} stone replace", "Filled 27 blocks")
        client.block_changes({src: stone, center: stone})
        client.command(f"fill {box} gold_block outline", "Filled 26 blocks")
        client.block_changes({src: gold, center: stone})
        client.command(f"fill {box} gold_block hollow", "Filled 1 blocks")
        client.block_changes({src: gold, center: air})
        client.command(f"fill {box} stone keep", "Filled 1 blocks")
        client.block_changes({src: gold, center: stone})

        line = f"{x} {y} {z} {x + 1} {y} {z}"
        client.command(f"fill {x} {y} {z} {x} {y} {z} stone replace gold_block", "Filled 1 blocks")
        client.block_changes({src: stone, second: gold})
        client.command(f"clone {line} {x + 5} {y} {z} filtered stone move", "Cloned 2 blocks")
        client.block_changes({src: air, second: gold, target: stone, next_target: air})
        client.command(f"clone {line} {x + 1} {y} {z} replace normal", "Clone regions overlap")
        client.command(f"clone {line} {x - 1} {y} {z} masked force", "Cloned 1 blocks")
        client.block_changes({(x - 1, y, z): air, src: gold, second: gold})
        client.command(f"fill {x + 5} {y} {z} {x + 5} {y} {z} air destroy", "Filled 1 blocks")
        client.block_changes({target: air})

        # A fresh chunk stream must see the edits independently of the deltas above.
        with GameplayClient(game, "BlockObserver") as observer:
            observer.login()
            if not needed.issubset(observer.chunks):
                observer.until(lambda *_: needed.issubset(observer.chunks), "observer-modification-chunks")
            require({p: observer.block(*p) for p in (src, second, center, target)} ==
                    {src: gold, second: gold, center: stone, target: air}, "block-edits-in-fresh-chunks")


def experience(game):
    def check(reader, progress, level, total):
        require(math.isclose(reader.number(">f"), progress, abs_tol=1e-7), "experience-progress")
        require((reader.varint(), reader.varint()) == (level, total), "experience-level-and-total")
        end(reader, "experience")

    with GameplayClient(game, "XpTester") as client:
        client.login()
        check(Reader(client.await_packet(0x5C)), 0, 0, 0)
        check(client.command("xp add @s 4 points", "add xp for 1 target", 0x5C), 4 / 7, 0, 4)
        check(client.command("xp set @s 3 points", "set xp for 1 target", 0x5C), 3 / 7, 0, 4)
        check(client.command("xp set @s 17 levels", "set xp for 1 target", 0x5C), 3 / 7, 17, 4)
        check(client.command("xp set @s 8 points", "set xp for 1 target", 0x5C), 8 / 47, 17, 4)
        sent = len(client.play[0x5C])
        client.command("xp set @s 47 points", "Experience points must be below")
        require(len(client.play[0x5C]) == sent, "rejected-experience-keeps-state")


def decode_health(body):
    reader = Reader(body)
    values = reader.number(">f"), reader.varint(), reader.number(">f")
    end(reader, "set-health")
    return values


def hunger(game):
    with GameplayClient(game, "FoodTester") as client:
        client.login()
        require(decode_health(client.await_packet(0x5D)) == (20, 20, 5), "food-defaults")
        client.command("gamerule doMobSpawning false", "Gamerule doMobSpawning is now set to: false")
        client.command("gamemode survival", "Set FoodTester's gamemode to survival")
        client.command("difficulty normal", "Set difficulty to normal")
        client.command("damage @s 4", "Applied 4 damage")
        client.track_health(lambda v: v == (16, 20, 5), "food-healing-start")
        client.track_health(lambda v: 16 < v[0] < 17 and v[1:] == (20, 5), "fractional-natural-regen")
        client.command("gamerule naturalRegeneration false", "Gamerule naturalRegeneration is now set to: false")
        health_before_drain = decode_health(client.play[0x5D][-1])[0]
        client.command("difficulty peaceful", "Set difficulty to peaceful")
        effect = client.command("effect give @s hunger 15 255", "Applied effect to 1 target", 0x76)
        effect.varint()  # entity
        require((effect.varint(), effect.varint(), effect.varint()) == (16, 255, 300),
                "hunger-effect-keeps-protocol-registry-id")
        effect.number("B")  # display flags
        end(effect, "hunger-effect")
        client.track_health(lambda v: v[1:] == (20, 0), "exhaustion-consumed-saturation")
        before = len(client.play[0x5D])
        start_tick = int.from_bytes(client.play[0x64][-1][:8], "big")
        client.until(lambda *_: int.from_bytes(client.play[0x64][-1][:8], "big") >= start_tick + 30,
                     "peaceful-exhaustion-observed-over-ticks")
        require(all(decode_health(body)[1] == 20 for body in client.play[0x5D][before:]),
                "peaceful-exhaustion-keeps-food")

        client.command("difficulty normal", "Set difficulty to normal")
        client.track_health(lambda v: v[1:] == (19, 0), "exhaustion-consumed-food", before)
        empty = client.track_health(lambda v: v[1:] == (0, 0), "food-reached-zero", before)
        require(empty[0] == health_before_drain, "natural-regeneration-disabled-during-drain")
        starved = client.track_health(lambda v: v == (empty[0] - 1, 0, 0),
                                      "starvation-damage", before)
        client.command("difficulty peaceful", "Set difficulty to peaceful")
        client.track_health(lambda v: v == (starved[0] - 1, 0, 0),
                            "already-empty-food-starves-in-peaceful-with-regen-disabled", before)


def main(binary):
    os.environ["MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS"] = "1"
    os.environ.setdefault("MC_PROBE_DEADLINE", "30")
    with local_server(binary) as game:
        for contract in (borders, scoreboard, advancements, modifications, experience, hunger):
            contract(game)
            print(f"gp5/gp7 protocol 767 {contract.__name__}: PASS")


if __name__ == "__main__":
    binary = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else (
        Path(__file__).resolve().parents[1] / "target/debug/mc-spine")
    try:
        main(binary)
    except ContractFailure as error:
        print(f"gp5/gp7 wire: FAIL {error.code}", file=sys.stderr)
        raise
    print("gp5/gp7 wire: PASS")
