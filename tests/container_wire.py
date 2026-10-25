#!/usr/bin/env python3
"""Drive real protocol-767 chest clicks; assertions read only server replies."""
import json
import math
from pathlib import Path
import struct
import sys
from protocol767 import Client, ContractFailure, Reader, WIRE_BLOCKS, local_server, position, require, vint


def wire_stack(item=0, count=0):
    return vint(count) + (vint(item) + vint(0) + vint(0) if count else b"")


def stack(reader):
    count = reader.varint()
    if count == 0:
        return (0, 0)
    item = reader.varint()
    require(reader.varint() == 0 and reader.varint() == 0, "plain-container-components")
    return (item, count)


def set_creative(client, slot, item=0, count=0):
    client.send(0x32, struct.pack(">h", slot) + wire_stack(item, count))
    return changed(client, 0, slot)


def changed(client, window, slot):
    result = None

    def matches(pid, body):
        nonlocal result
        if pid != 0x15:
            return False
        reader = Reader(body)
        wid = reader.number("B")
        reader.varint()  # state id
        key = reader.number(">h")
        value = stack(reader)
        require(reader.pos == len(body), "complete-set-slot")
        if (wid, key) == (window, slot):
            result = value
            return True
        return False

    client.until(matches, "container-set-slot")
    return result


def click(client, slot, button=0, mode=0):
    # The server must compute the result: no client-predicted changed slots.
    client.send(0x0e, bytes([1]) + vint(0) + struct.pack(">hB", slot, button)
                + vint(mode) + vint(0) + wire_stack())
    return changed(client, 1, slot)


def use_on(client, pos, sequence):
    client.send(0x38, vint(0) + position(*pos) + vint(1)
                + struct.pack(">fff?", 0.5, 1.0, 0.5, False) + vint(sequence))


def open_chest(client, pos):
    use_on(client, pos, 2)
    reader = Reader(client.until_id(0x33, "chest-open"))
    require(reader.varint() == 1 and reader.varint() == 2, "single-chest-menu")
    reader = Reader(client.until_id(0x13, "chest-content"))
    require(reader.number("B") == 1, "chest-window-id")
    reader.varint()
    require(reader.varint() == 63, "complete-chest-content")
    slots = [stack(reader) for _ in range(63)]
    stack(reader)  # carried
    require(reader.pos == len(reader.data), "complete-chest-packet")
    return slots


def exercise(game):
    by_name = {block["name"]: block for block in WIRE_BLOCKS}
    # Item ids are the pinned 1.21.1 protocol registry ids, not block-state ids.
    stone, dirt, chest = 1, 28, 299
    with Client(game, "ContainerTester") as client:
        client.login()
        if client.pos is None:
            client.until_id(0x40)
        sx, sy, sz = client.pos
        x, z = math.floor(sx) + 1, math.floor(sz)
        solid = {state for b in WIRE_BLOCKS if b["boundingBox"] == "block"
                 for state in range(b["minStateId"], b["maxStateId"] + 1)}
        y = next(y for y in range(math.floor(sy) - 1, -65, -1) if client.block(x, y, z) in solid)
        pos = (x, y + 1, z)
        client.move(sx, y + 1.0, sz)
        set_creative(client, 36, chest, 1)
        use_on(client, (x, y, z), 1)
        client.until(lambda *_: any(p == pos for p, _ in client.updates), "chest-placement")
        require(by_name["chest"]["minStateId"] <= client.block(*pos)
                <= by_name["chest"]["maxStateId"], "placed-chest-state")
        for slot in range(9, 45):
            set_creative(client, slot)
        set_creative(client, 36, stone, 60)
        set_creative(client, 37, stone, 10)
        set_creative(client, 38, dirt, 3)
        open_chest(client, pos)
        require(click(client, 54) == (0, 0), "take-inventory-stack")
        require(click(client, 0) == (stone, 60), "put-chest-stack")
        click(client, 55)
        require(click(client, 0) == (stone, 64), "chest-partial-merge")
        require(click(client, 1) == (stone, 6), "chest-cursor-remainder")
        require(click(client, 1, button=1) == (stone, 3), "chest-half-pickup")
        require(click(client, 3, button=1) == (stone, 1), "chest-right-place-one")
        require(click(client, 4) == (stone, 2), "chest-right-place-remainder")
        click(client, 1)
        require(click(client, 56, button=1) == (stone, 3), "chest-right-swap")
        require(click(client, 2) == (dirt, 3), "right-swap-carried-result")
        set_creative(client, 38)
        click(client, 0, mode=1)
        client.send(0x0f, bytes([1]))
        slots = open_chest(client, pos)
        require(slots[0] == (0, 0), "quick-move-removes-source")
        require(slots[62] == (stone, 64), "quick-move-reverse-inventory-order")
        require(all(value == (0, 0) for value in slots[27:62]), "quick-move-no-other-slot")
        print(json.dumps({"protocol": 767, "partial_merge": True, "carried_remainder": 6,
                          "right_swap": True, "shift_destination": 62}, sort_keys=True))


if __name__ == "__main__":
    try:
        with local_server(Path(sys.argv[1]).resolve()) as game:
            exercise(game)
    except ContractFailure as error:
        print(f"containers wire: FAIL {error.code}", file=sys.stderr)
        raise
    print("containers wire: PASS")
