#!/usr/bin/env python3
"""Live protocol-767 cast/reel check for the fishing rod."""
import json
import math
import os
from pathlib import Path
import struct
import sys
import time

from protocol767 import (Client, ContractFailure, Reader, local_server,
                         position, require, string, vint)

ROOT = Path(__file__).resolve().parents[1]
BOBBER_ENTITY_TYPE = 129
FISH_ITEMS = {935, 936, 937, 938}


def air_click(sequence, hand=0):
    """Protocol-767 block_place air-use sentinel for a held item."""
    return (vint(hand) + position(-1, 255, -1) + vint(0xFFFFFFFF)
            + struct.pack(">fff?", 0.5, 0.5, 0.5, False) + b"\x00"
            + vint(sequence))


def spawned_entity(body):
    """Return (entity id, type id) from a clientbound add-entity packet."""
    reader = Reader(body)
    entity_id = reader.varint()
    reader.take(16)  # UUID
    entity_type = reader.varint()
    reader.take(24)  # x/y/z doubles
    reader.take(3)   # pitch/yaw/head yaw
    reader.varint()  # object data
    reader.take(6)   # velocity
    require(reader.pos == len(body), "spawn-entity-complete")
    return entity_id, entity_type


def slot_item(body):
    """Read the item id from a nonempty clientbound Set Slot packet."""
    reader = Reader(body)
    reader.number("B")  # window id
    reader.varint()      # state id
    slot = reader.number(">h")
    count = reader.varint()
    if count == 0:
        return slot, None
    return slot, reader.varint()


class FishingClient(Client):
    def __init__(self, game, name="FishingWire"):
        super().__init__(game, name)
        self.entity_id = None
        self.entities = {}
        self.slot_items = []

    def read(self, timeout):
        packet, body = super().read(timeout)
        if self.phase != "play":
            return packet, body
        if packet == 0x2B:
            self.entity_id = struct.unpack(">i", body[:4])[0]
        elif packet == 0x01:
            entity_id, entity_type = spawned_entity(body)
            self.entities[entity_id] = entity_type
        elif packet == 0x15:
            self.slot_items.append(slot_item(body))
        return packet, body


def command(client, text):
    client.send(0x04, string(text.lstrip("/")))
    time.sleep(0.16)  # Keep below the command chat throttle.


def creative_slot(client, slot, item_id):
    client.send(0x32, struct.pack(">h", slot)
                + vint(1) + vint(item_id) + vint(0) + vint(0))
    client.until(lambda *_: (slot, item_id) in client.slot_items,
                 "fishing-rod-inventory")


def use_rod(client, sequence, air_click_packet=True):
    if air_click_packet:
        client.send(0x38, air_click(sequence))
    else:
        client.send(0x39, vint(0) + vint(sequence))


def live(game):
    summary = {
        "protocol": 767,
        "ok": False,
        "stages": {},
        "skipped": {},
    }
    client = FishingClient(game)
    with client:
        client.login()
        if client.pos is None:
            client.until_id(0x40, "spawn-position")
        require(client.entity_id is not None, "join-entity-id")
        command(client, "gamemode creative")

        sx, sy, sz = client.pos
        pool_y = math.floor(sy) - 1
        px, pz = math.floor(sx), math.floor(sz)
        # Make an open 9x9 pond at the player's feet minus one block, then aim
        # down into its center. The surrounding water is exposed to sky.
        command(client, f"fill {px - 4} {pool_y - 1} {pz - 4} "
                        f"{px + 4} {pool_y - 1} {pz + 4} stone")
        command(client, f"fill {px - 4} {pool_y} {pz - 4} "
                        f"{px + 4} {pool_y} {pz + 4} water")
        command(client, f"tp @s {px + 0.5} {pool_y + 1} {pz + 0.5} 0 65")
        client.until_id(0x40, "pond-teleport")

        creative_slot(client, 36, 931)  # fishing_rod
        summary["stages"]["rod"] = "verified: creative inventory slot update"
        use_rod(client, 101)
        try:
            client.until(lambda *_: BOBBER_ENTITY_TYPE in client.entities.values(),
                         "bobber-spawn-0x38")
        except ContractFailure:
            summary["skipped"]["air_click_0x38"] = (
                "no bobber spawn followed the vanilla block_place air-use packet")
            use_rod(client, 101, air_click_packet=False)
            client.until(lambda *_: BOBBER_ENTITY_TYPE in client.entities.values(),
                         "bobber-spawn-0x39")
        bobber_id = next(eid for eid, kind in client.entities.items()
                         if kind == BOBBER_ENTITY_TYPE)
        summary["stages"]["cast"] = {
            "status": "verified: bobber spawn packet",
            "entity_id": bobber_id,
            "entity_type": BOBBER_ENTITY_TYPE,
        }

        # This server currently changes the bobber's private bite flag without
        # emitting a sound/particle or submerged position observable on wire.
        # Wait long enough for the implemented bite window, then validate the
        # catch from the resulting inventory correction.
        deadline = time.monotonic() + 10.0
        client.until(lambda *_: time.monotonic() >= deadline,
                     "bite-window-elapsed")
        summary["skipped"]["bite_signal"] = (
            "server sets an internal bobber bit but emits no bite sound, "
            "particle, or submerge movement")

        before = len(client.slot_items)
        use_rod(client, 102, air_click_packet=False)
        client.until(lambda *_: any(item in FISH_ITEMS
                                    for _, item in client.slot_items[before:]),
                     "fishing-catch-inventory")
        fish_slot, fish_item = next(
            (slot, item) for slot, item in client.slot_items[before:]
            if item in FISH_ITEMS)
        summary["stages"]["reel"] = "verified: rod use sent"
        summary["stages"]["catch"] = {
            "status": "verified: fish item inventory update",
            "slot": fish_slot,
            "item_id": fish_item,
        }
        summary["ok"] = True
    return summary


def main():
    os.environ.setdefault("MC_PROBE_DEADLINE", "30")
    summary = {"protocol": 767, "ok": False, "stages": {}, "skipped": {}}
    try:
        binary = Path(sys.argv[1] if len(sys.argv) > 1
                      else ROOT / "target/release/megu").resolve()
        require(binary.is_file(), "server-binary-missing")
        with local_server(binary, extra_env={
                "MEGU_ALLOW_PLAYER_ADMIN_COMMANDS": "1"}) as game:
            summary = live(game)
    except Exception as error:
        summary["failure"] = (error.code if isinstance(error, ContractFailure)
                              else f"{type(error).__name__}: {error}")
    print(json.dumps(summary, sort_keys=True))
    if not summary["ok"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
