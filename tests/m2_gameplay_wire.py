#!/usr/bin/env python3
"""M2 gameplay contracts observed over the normal protocol-767 server connection."""
import json
import math
import os
from pathlib import Path
import struct
import sys
import tempfile
import time

from container_wire import stack, wire_stack
from protocol767 import Client, ContractFailure, Reader, local_server, position, require, string, vint

os.environ["MEGU_ALLOW_PLAYER_ADMIN_COMMANDS"] = "1"
os.environ.setdefault("MC_PROBE_DEADLINE", "30")

OAK_LOG = 132
OAK_PLANKS = 36
RAW_IRON = 810
IRON_INGOT = 811
COAL = 803


class M2Client(Client):
    """Observe play packets even when the login/chunk fixture consumes them."""

    def __init__(self, game, name):
        super().__init__(game, name)
        self.entity_id = None
        self.time_updates = []
        self.weather_events = []
        self.health_updates = []
        self.hurt_events = []
        self.positions = []
        self.slots = {}
        self.furnace_contents = []

    def read(self, timeout):
        pid, body = super().read(timeout)
        if self.phase != "play":
            return pid, body
        if pid == 0x2b:
            self.entity_id = struct.unpack(">i", body[:4])[0]
        elif pid == 0x64:
            require(len(body) == 16, "time-packet-length")
            self.time_updates.append(struct.unpack(">qq", body))
        elif pid == 0x22:
            require(len(body) == 5, "weather-event-length")
            self.weather_events.append((body[0], struct.unpack(">f", body[1:])[0]))
        elif pid == 0x5d:
            require(len(body) >= 4, "health-packet-length")
            self.health_updates.append(struct.unpack(">f", body[:4])[0])
        elif pid == 0x24:
            self.hurt_events.append(Reader(body).varint())
        elif pid == 0x40:
            self.positions.append(self.pos)
        elif pid == 0x15:
            r = Reader(body)
            window = r.number("B")
            r.varint()
            slot = r.number(">h")
            value = stack(r)
            require(r.pos == len(body), "set-slot-complete")
            self.slots[(window, slot)] = value
        elif pid == 0x13 and body[0] == 2:
            r = Reader(body)
            require(r.number("B") == 2, "furnace-window-id")
            r.varint()
            require(r.varint() == 39, "furnace-slot-count")
            slots = [stack(r) for _ in range(39)]
            stack(r)
            require(r.pos == len(body), "furnace-content-complete")
            self.furnace_contents.append(slots)
        return pid, body


def command(client, text):
    # Serverbound chat_command (0x04) — plain chat is relayed, not executed.
    client.send(0x04, string(text.lstrip("/")))
    time.sleep(0.16)  # Keep below the chat spam throttle.


def await_slot(client, window, slot, value, contract):
    client.until(lambda *_: client.slots.get((window, slot)) == value, contract)


def click(client, window, slot, button=0):
    client.send(0x0e, bytes([window]) + vint(0) + struct.pack(">hB", slot, button)
                + vint(0) + vint(0) + wire_stack())


def set_creative(client, slot, item, count):
    client.send(0x32, struct.pack(">h", slot) + wire_stack(item, count))
    await_slot(client, 0, slot, (item, count), "creative-inventory-update")


def use_on(client, location):
    client.send(0x38, vint(0) + position(*location) + vint(1)
                + struct.pack(">fff?", 0.5, 1.0, 0.5, False) + vint(41))


def open_furnace(client, location):
    use_on(client, location)
    r = Reader(client.until_id(0x33, "furnace-open-menu"))
    require(r.varint() == 2 and r.varint() == 14, "furnace-menu-type")
    client.until(lambda *_: bool(client.furnace_contents), "furnace-initial-content")
    return client.furnace_contents[-1]


def login(client):
    client.login()
    if client.pos is None:
        client.until_id(0x40, "spawn-position")
    require(client.entity_id is not None, "join-entity-id")


def time_weather(game):
    with M2Client(game, "M2Climate") as client:
        login(client)
        command(client, "gamerule doDaylightCycle false")
        command(client, "gamerule doWeatherCycle false")
        command(client, "time set 12345")
        client.until(lambda *_: any(day == 12345 for _, day in client.time_updates),
                     "time-set-update-0x64")

        for text, expected in (("rain", 1), ("thunder", 1), ("clear", 2)):
            before = len(client.weather_events)
            command(client, "weather " + text)
            client.until(lambda *_, before=before: len(client.weather_events) >= before + 2,
                         "weather-transition-0x22-" + text)
            events = client.weather_events[before:]
            require(any(code == expected for code, _ in events),
                    "weather-event-kind-" + text)
            require(any(code == 8 and strength == (1.0 if text == "thunder" else 0.0)
                        for code, strength in events), "thunder-strength-" + text)

        before = len(client.weather_events)
        command(client, "weather thunder")
        client.until(lambda *_: len(client.weather_events) >= before + 2,
                     "thunder-reenabled")
        require([kind for kind, _ in client.weather_events[before:before + 2]] == [8, 1],
                "thunder-event-order")
        require(client.weather_events[before][1] == 1.0, "thunder-reenabled-strength")

        with M2Client(game, "M2Observer") as observer:
            login(observer)
            require(any(code == 1 for code, _ in observer.weather_events),
                    "login-rain-state")
            require(any(code == 8 and strength == 1.0 for code, strength in observer.weather_events),
                    "login-thunder-state")
            observer.until(lambda *_: any(day == 12345 for _, day in observer.time_updates),
                           "login-time-state")
        command(client, "save-all")
    return {"time": 12345, "weather": "thunder", "transitions": ["rain", "thunder", "clear"]}


def crafting_and_furnace(game):
    with M2Client(game, "M2Crafter") as client:
        login(client)
        command(client, "gamemode creative")
        set_creative(client, 36, OAK_LOG, 1)
        click(client, 0, 36)  # lift actual log stack into the cursor
        await_slot(client, 0, 36, (0, 0), "craft-lift-log")
        click(client, 0, 1)  # place log into 2x2 grid
        await_slot(client, 0, 0, (OAK_PLANKS, 4), "craft-recipe-result")
        click(client, 0, 0)  # take crafted output, consuming the log
        await_slot(client, 0, 1, (0, 0), "craft-consumes-ingredient")
        click(client, 0, 37)
        await_slot(client, 0, 37, (OAK_PLANKS, 4), "craft-output-taken")

        set_creative(client, 36, RAW_IRON, 1)
        set_creative(client, 38, COAL, 1)
        sx, sy, sz = client.pos
        location = (math.floor(sx) + 1, math.floor(sy), math.floor(sz))
        command(client, f"setblock {location[0]} {location[1]} {location[2]} furnace")
        client.until(lambda *_: any(p == location for p, _ in client.updates),
                     "furnace-placed")
        initial = open_furnace(client, location)
        require(initial[:3] == [(0, 0)] * 3, "furnace-empty-before")
        click(client, 2, 30)  # player slot 36 maps to furnace slot 30
        await_slot(client, 2, 30, (0, 0), "furnace-lift-ore")
        click(client, 2, 0)
        await_slot(client, 2, 0, (RAW_IRON, 1), "furnace-input-slot")
        click(client, 2, 32)  # player slot 38
        await_slot(client, 2, 32, (0, 0), "furnace-lift-coal")
        click(client, 2, 1)
        # The server consumes the coal on the next tick and must smelt the ore.
        client.until(lambda *_: any(rows[2] == (IRON_INGOT, 1)
                                    for rows in client.furnace_contents),
                     "furnace-smelt-output")
        require(any(rows[0] == (RAW_IRON, 1) and rows[1] == (0, 0)
                    for rows in client.furnace_contents), "furnace-burning-progress")
        command(client, "save-all")
    return location


def combat_and_respawn(game):
    with M2Client(game, "M2Attacker") as attacker, M2Client(game, "M2Victim") as victim:
        login(attacker)
        login(victim)
        command(attacker, "gamemode survival M2Victim")
        victim.move(*attacker.pos)
        attacker.move(*attacker.pos)
        time.sleep(0.2)
        victim.until(lambda *_: bool(victim.health_updates) and victim.health_updates[-1] > 0,
                     "pvp-victim-initial-health")
        attacker.send(0x16, vint(victim.entity_id) + vint(1) + bytes([0]))
        victim.until(lambda *_: any(0 < health < 20 for health in victim.health_updates),
                     "pvp-damage-health")
        if victim.entity_id not in victim.hurt_events:
            victim.until(lambda *_: victim.entity_id in victim.hurt_events,
                         "pvp-hurt-animation")
        command(attacker, "kill M2Victim")
        victim.until(lambda *_: any(health <= 0 for health in victim.health_updates),
                     "death-health-zero")
        before = len(victim.health_updates)
        before_positions = len(victim.positions)
        victim.send(0x09, vint(0))  # ClientStatus perform_respawn
        victim.until_id(0x47, "respawn-0x47")
        victim.until(lambda *_: len(victim.positions) > before_positions and
                     any(h > 0 for h in victim.health_updates[before:]),
                     "respawn-health-position-restored")
        require(victim.health_updates[-1] > 0, "respawn-positive-health")
    return {"pvp_damage": True, "death": True, "respawn": True}


def reload(binary, root, furnace):
    with local_server(binary, root) as game:
        with M2Client(game, "M2Reload") as climate:
            login(climate)
            climate.until(lambda *_: any(day == 12345 for _, day in climate.time_updates),
                          "reload-time-persisted")
            require(any(code == 1 for code, _ in climate.weather_events),
                    "reload-rain-persisted")
            require(any(code == 8 and value == 1 for code, value in climate.weather_events),
                    "reload-thunder-persisted")
            command(climate, "weather clear")
            climate.until(lambda *_: any(code == 2 for code, _ in climate.weather_events),
                          "reload-weather-clear")
        with M2Client(game, "M2Crafter") as crafter:
            login(crafter)
            initial = open_furnace(crafter, furnace)
            require(initial[2] == (IRON_INGOT, 1), "reload-furnace-output")
            click(crafter, 2, 2)
            await_slot(crafter, 2, 2, (0, 0), "reload-take-smelt")
            click(crafter, 2, 30)
            await_slot(crafter, 2, 30, (IRON_INGOT, 1), "reload-cooked-in-inventory")
    return {"smelting": "raw_iron -> iron_ingot", "furnace_saved": True}


def exercise(binary, root):
    with local_server(binary, root) as game:
        weather = time_weather(game)
        furnace = crafting_and_furnace(game)
        combat = combat_and_respawn(game)
    persisted = reload(binary, root, furnace)
    print(json.dumps({"protocol": 767, **weather, **combat, **persisted,
                      "crafting": "oak_log -> 4 oak_planks"}, sort_keys=True))


if __name__ == "__main__":
    binary = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else (
        Path(__file__).resolve().parents[1] / "target/debug/megu")
    try:
        with tempfile.TemporaryDirectory(prefix="spine-m2-wire-") as root:
            exercise(binary, Path(root))
    except ContractFailure as error:
        print(f"m2 gameplay wire: FAIL {error.code}", file=sys.stderr)
        raise
    print("m2 gameplay wire: PASS")
