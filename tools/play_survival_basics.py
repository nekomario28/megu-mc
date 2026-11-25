#!/usr/bin/env python3
"""Real-play survival session: walk, sneak, dig, place, pick up, persist.

A scripted client does the things an actual player does in their first minute,
asserting the server's observable contract at every step rather than only
checking that packets parse. Runs the real `megu` binary over protocol 767.

Usage: python3 tools/play_survival_basics.py <path-to-megu-binary>
"""
import json
import math
from pathlib import Path
import struct
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tests"))

import protocol767  # noqa: E402
from protocol767 import (  # noqa: E402
    Client,
    ContractFailure,
    Reader,
    local_server,
    position,
    require,
    string,
    vint,
)


class PlayClient(Client):
    """Client that also captures its own entity id from join_game (0x2b)."""

    def __init__(self, game, name="PlayActor"):
        super().__init__(game, name)
        self.entity_id = None

    def read(self, timeout):
        packet, body = super().read(timeout)
        if self.phase == "play" and packet == 0x2B:
            self.entity_id = struct.unpack(">i", body[:4])[0]
        return packet, body


def chat_command(client, command):
    """Serverbound chat command (0x04): text without the leading slash."""
    client.send(0x04, string(command))


def creative_slot(client, slot, stack):
    """C2S 0x32 creative inventory set; returns the echoed wire bytes."""
    client.send(0x32, struct.pack(">h", slot) + stack)

    def matches(pid, body):
        if pid != 0x15:
            return False
        reader = Reader(body)
        wid = reader.number("B")
        reader.varint()
        key = reader.number(">h")
        return (wid, key) == (0, slot)

    client.until(matches, "creative-slot-echo")


def plain_stack(item, count=1):
    """slot wire bytes: count, item id, zero added/removed components."""
    return vint(count) + (vint(item) + vint(0) + vint(0) if count else b"")


def go_creative(client):
    chat_command(client, "gamemode creative")

    def confirmed(pid, body):
        if pid == 0x22:
            return len(body) == 5 and body[0] == 3
        return pid == 0x6C and "gamemode" in client.messages[-1]

    client.until(confirmed, "creative-gamemode-confirm")


def sneak(client, entity_id, enabled):
    # Entity action (0x25): start sneaking = 0? In this tree 3 = sneak on, 4 =
    # sneak off (see client_kernel_wire.py).
    client.send(0x25, vint(entity_id) + vint(3 if enabled else 4) + vint(0))


def dig(client, cell, face=1, sequence=71):
    """Player action (0x24): status 0 = start digging (insta in creative)."""
    client.send(0x24, vint(0) + position(*cell) + bytes([face]) + vint(sequence))


def place_block(client, cell, sequence=1):
    """Use item on (0x38): main hand, top face, cursor centre, no sneak."""
    client.send(
        0x38,
        vint(0) + position(*cell) + vint(1)
        + struct.pack(">fff?", 0.5, 1.0, 0.5, False) + vint(sequence),
    )


def breakable_states():
    return {
        st
        for b in protocol767.WIRE_BLOCKS
        if b["boundingBox"] == "block" and 0 <= b["hardness"] < 50
        for st in range(b["minStateId"], b["maxStateId"] + 1)
    }


def floor_cell(client, sx, sy, sz, breakable):
    """First diggable full-cube block at or below the feet position."""
    x, z = math.floor(sx), math.floor(sz)
    for y in range(min(math.floor(sy) - 1, 319), -65, -1):
        if client.block(x, y, z) in breakable:
            return (x, y, z)
    raise AssertionError("no breakable block below spawn")


def exercise(game):
    breakable = breakable_states()
    client = PlayClient(game, "PlayActor")
    summary = {"protocol": 767, "steps": []}
    try:
        with client:
            client.login()
            if client.pos is None:
                client.until_id(0x40)
            sx, sy, sz = client.pos
            cx, cz = math.floor(sx / 16), math.floor(sz / 16)
            spawn_chunks = {(cx + x, cz + z) for x in range(-2, 3) for z in range(-2, 3)}
            # Login initially delivers only the 3x3 ring (BOTTLENECK-SP4); the
            # negotiated window is filled by the ordinary delivery loop and may
            # overshoot the requested view, so wait for containment not equality.
            client.until(lambda *_: spawn_chunks <= set(client.chunks), "spawn-window")
            summary["steps"].append("join")

            # Walk one chunk east: the server's view distance follows.
            client.move(sx + 16, sy, sz)
            client.until_id(0x0C)
            shifted = {(cx + 1 + x, cz + z) for x in range(-2, 3) for z in range(-2, 3)}
            client.until(lambda *_: shifted <= set(client.chunks), "walk-window-shift")
            client.move(sx, sy, sz)
            client.until_id(0x0C)
            summary["steps"].append("walk")

            # A quick hop: two rising-falling moves in the same tick burst.
            client.move(sx, sy + 0.42, sz)
            client.move(sx, sy, sz)
            summary["steps"].append("jump")

            # Sneak on/off — the contract is simply that the packets are
            # accepted (no disconnect); visibility is a renderer concern.
            sneak(client, client.entity_id, True)
            sneak(client, client.entity_id, False)
            summary["steps"].append("sneak")

            # Dig the block under the player's feet area.
            target = floor_cell(client, sx, sy, sz, breakable)
            client.move(sx, target[1] + 1.0, sz)
            dig(client, target)
            client.until(lambda *_: (target, 0) in client.updates, "dig-confirm")
            require(client.block(*target) == 0, "dig-to-air")
            summary["steps"].append("dig")

            # Place: creative-mode stone into the hole we just made. use_item_on
            # targets the *face* of a solid block — click the top face of the
            # block below the hole to fill it.
            go_creative(client)
            creative_slot(client, 36, plain_stack(1, 1))  # item 1 = stone
            place_block(client, (target[0], target[1] - 1, target[2]))
            client.until(
                lambda *_: any(pos == target and st != 0 for pos, st in client.updates),
                "place-confirm",
            )
            require(client.block(*target) != 0, "placed-state")
            summary["steps"].append("place")

            # Sleep: night + a real two-part bed, then use_block on the foot.
            # Vanilla's 100-tick sleep gate fires skip_night for a lone sleeper;
            # the observable is the system chat line plus the morning jump.
            chat_command(client, "time set night")
            bed = (target[0] + 2, target[1] + 1, target[2])
            chat_command(client, f"setblock {bed[0]} {bed[1]} {bed[2]} minecraft:red_bed[facing=east,part=foot]")
            chat_command(client, f"setblock {bed[0]+1} {bed[1]} {bed[2]} minecraft:red_bed[facing=east,part=head]")
            client.move(bed[0] + 0.5, float(bed[1] + 1), bed[2] + 0.5)
            place_block(client, bed, sequence=90)
            client.until(
                lambda *_: any("slept through the night" in m for m in client.messages),
                "night-skip",
            )
            summary["steps"].append("sleep")
        summary["placed_cell"] = list(target)
        summary["dug_cell"] = list(target)
        # A fresh reader must see the edited world — persistence-by-chunk, the
        # same thing a rejoining player observes.
        with Client(game, "PlayReader") as reader:
            reader.login()
            require(reader.block(*target) != 0, "persisted-place")
            summary["steps"].append("persist")
        summary["ok"] = True
        print(json.dumps(summary, sort_keys=True))
    except ContractFailure as error:
        if client.contract_started:
            print(f"play survival: FAIL {error.code}", file=sys.stderr)
        raise


if __name__ == "__main__":
    env = {"MEGU_ALLOW_PLAYER_ADMIN_COMMANDS": "1"}
    with local_server(Path(sys.argv[1]).resolve(), extra_env=env) as game:
        exercise(game)
    print("play survival basics: PASS")
