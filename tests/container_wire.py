#!/usr/bin/env python3
"""Drive real protocol-767 chest clicks; assertions read only server replies."""
import json
import math
from pathlib import Path
import struct
import sys
import tempfile
import time
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


def look(client, yaw):
    """Look rotation only; the port keeps yaw in degrees, 0 = south (the value vanilla's
    `getRotation()` uses), so yaw 0 makes a placed chest face north."""
    client.send(0x1c, struct.pack(">ff", yaw, 0.0) + bytes([0]))


def break_block(client, pos):
    """Start-destroy in creative breaks the block in the same call."""
    client.send(0x24, vint(0) + position(*pos) + bytes([1]) + vint(71))


# ---- chest block state ----
# `assets/blocks_1211.json` is the registry the server is built from, and it orders a
# chest's properties the way the jar declares them: `facing` (north, south, west, east)
# is the major stride, then `ChestType` (single, left, right - `dtu`'s field order in the
# 1.21.1 jar), then the boolean `waterlogged` (true, false). The default state proves the
# layout: chest's defaultState is 2955 = minStateId + 1, i.e. north/single/false.
CHEST_FACING = ("north", "south", "west", "east")
CHEST_TYPE = ("single", "left", "right")


def chest_props(by_name, state):
    for name in ("chest", "trapped_chest"):
        block = by_name[name]
        if block["minStateId"] <= state <= block["maxStateId"]:
            off = state - block["minStateId"]
            return {"name": name, "facing": CHEST_FACING[off // 6],
                    "type": CHEST_TYPE[(off // 2) % 3], "waterlogged": off % 2 == 0}
    return None


def chest_type_at(by_name, state):
    props = chest_props(by_name, state)
    return None if props is None else props["type"]


def block_event(body):
    """ClientboundBlockEventPacket: BlockPos, unsigned byte action, byte param, VarInt
    block id (the block registry's raw id, not a state id)."""
    reader = Reader(body)
    x, y, z = position_parts(reader.number(">Q"))
    return ((x, y, z), reader.number("B"), reader.number("B"), reader.varint())


def position_parts(packed):
    def signed(value, bits):
        return value - (1 << bits) if value & (1 << (bits - 1)) else value
    return signed(packed >> 38, 26), signed(packed & 4095, 12), signed((packed >> 12) & 0x3ffffff, 26)


def item_meta(body):
    """Set Entity Metadata carrying the item stack of a dropped entity."""
    reader = Reader(body)
    reader.varint()  # entity id
    require(reader.number("B") == 8, "drop-meta-key")
    require(reader.varint() == 7, "drop-meta-type")
    count, item = reader.varint(), reader.varint()
    return (item, count)  # item first, as the slot tuples elsewhere in this file


def drain(client, seconds=1.0):
    """Read what the server sends for a while. A chest contract is about what arrives
    unprompted, so nothing here asks for a specific packet."""
    out = {"events": [], "closes": [], "drops": [], "menus": [], "ids": set()}
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            pid, body = client.read(0.15)
        except (TimeoutError, OSError):
            continue
        out["ids"].add(pid)
        if pid == 0x08:
            out["events"].append(block_event(body))
        elif pid == 0x12:
            out["closes"].append(body)
        elif pid == 0x58:
            out["drops"].append(item_meta(body))
        elif pid == 0x33:
            out["menus"].append(body)
    return out


def open_container(client, pos, menu, slots):
    use_on(client, pos, 2)
    reader = Reader(client.until_id(0x33, "container-open"))
    require(reader.varint() == 1 and reader.varint() == menu, "container-menu")
    reader = Reader(client.until_id(0x13, "container-content"))
    require(reader.number("B") == 1, "container-window-id")
    reader.varint()
    require(reader.varint() == slots + 36, "complete-container-content")
    values = [stack(reader) for _ in range(slots + 36)]
    stack(reader)  # carried
    require(reader.pos == len(reader.data), "complete-container-packet")
    return values


def open_chest(client, pos):
    """A single chest: menu `generic_9x3`, 27 container slots."""
    return open_container(client, pos, 2, 27)


def open_double_chest(client, pos):
    """A paired chest: menu `generic_9x6`, 54 container slots."""
    return open_container(client, pos, 5, 54)


def enter(client):
    if client.pos is None:
        client.until_id(0x40)
    return client.pos


def place_pair(client, by_name, left, right):
    """Place two chests whose pair axis is x. Facing is north because the player looks
    yaw 0 (south) and `getStateForPlacement` takes the opposite direction."""
    look(client, 0.0)
    set_creative(client, 36, 299, 1)  # minecraft:chest item id
    place(client, (left[0], left[1] - 1, left[2]))
    client.until(lambda *_: client.block(*left) != 0, "pair-left-place")
    set_creative(client, 36, 299, 1)
    place(client, (right[0], right[1] - 1, right[2]))
    client.until(lambda *_: chest_type_at(by_name, client.block(*right)) not in (None, "single"),
                 "pair-right")
    lp = chest_props(by_name, client.block(*left))
    rp = chest_props(by_name, client.block(*right))
    require(lp is not None and rp is not None, "pair-states")
    require((lp["name"], rp["name"]) == ("chest", "chest"), "pair-kind")
    # `PlacedFeature`: the counter-clockwise neighbour becomes RIGHT, and the neighbour
    # it pairs with is repaired to LEFT (`ChestBlock.updateShape`).
    require((lp["type"], rp["type"]) == ("left", "right"), "pair-halves")
    require(lp["facing"] == rp["facing"], "pair-facing")
    return lp, rp


def place(client, pos):
    use_on(client, pos, 1)


def exercise(game):
    by_name = {block["name"]: block for block in WIRE_BLOCKS}
    # Item ids are the pinned 1.21.1 protocol registry ids, not block-state ids.
    stone, dirt, chest, trapped = 1, 28, 299, 678
    with Client(game, "ContainerTester") as client:
        client.login()
        sx, sy, sz = enter(client)
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
        client.send(0x0f, bytes([1]))

        # ---- pairing, halves, viewer count, invalidation and drop ----
        zc = z + 3
        left, right = (x, y + 1, zc), (x + 1, y + 1, zc)
        place_pair(client, by_name, left, right)
        chest_id = [b for b in WIRE_BLOCKS if b["name"] == "chest"][0]["id"]
        require(chest_id == 177, "chest-block-id")  # the wire's block id for the state

        with Client(game, "ContainerPeer") as peer:
            peer.login()
            enter(peer)
            # The world spawn need not be beside the chest: both viewers stand where the
            # chests were placed, or the server's interaction range rejects the right-click.
            peer.move(sx, y + 1.0, sz)
            # Viewer one opens the LEFT half; the window is the pair, and
            # `DoubleBlockCombiner` puts the RIGHT half in slots 0..26.
            open_double_chest(client, left)
            seen = drain(client, 0.8)
            require(sorted((event[0], event[2]) for event in seen["events"])
                    == sorted([(left, 1), (right, 1)]), "open-broadcasts-one-event-per-half")
            require({event[1] for event in seen["events"]} == {1}, "lid-action-id")
            require({event[3] for event in seen["events"]} == {chest_id}, "lid-block-id")

            open_double_chest(peer, right)
            for viewer in (client, peer):
                seen = drain(viewer, 0.8)
                require(sorted((event[0], event[2]) for event in seen["events"])
                        == sorted([(left, 2), (right, 2)]), "second-viewer-count")
            peer.send(0x0f, bytes([1]))
            for viewer in (client, peer):
                seen = drain(viewer, 0.8)
                require(sorted((event[0], event[2]) for event in seen["events"])
                        == sorted([(left, 1), (right, 1)]), "close-drops-one-count")
            # An item placed through window slot 0 lands in the RIGHT half: slots 0..26
            # come from `CompoundContainer(first = right, second = left)`.
            set_creative(client, 36, stone, 5)
            require(click(client, 54 + 27) == (0, 0), "carry-hotbar-stack")
            require(click(client, 0) == (stone, 5), "put-into-right-half")
            set_creative(client, 37, dirt, 3)
            require(click(client, 54 + 28) == (0, 0), "carry-second-hotbar-stack")
            require(click(client, 27) == (dirt, 3), "put-into-left-half")
            client.send(0x0f, bytes([1]))
            seen = drain(client, 0.8)
            require(sorted((event[0], event[2]) for event in seen["events"])
                    == sorted([(left, 0), (right, 0)]), "last-close-counts-zero")

            # Breaking the left half: the survivor reverts to `single`, the broken half's
            # own stack drops, and the right half keeps the stack slot 0 wrote.
            # Drain first, assert after: `until` throws away the packets that are not the
            # one it waits for, and the drop plus the repair arrive in the same burst.
            break_block(client, left)
            seen = drain(client, 1.5)
            require(client.block(*left) == 0, "break-left-half")
            require(chest_type_at(by_name, client.block(*right)) == "single",
                    "survivor-reverts-to-single")
            require((dirt, 3) in seen["drops"], "broken-half-drops-its-own-stack")
            require((stone, 5) not in seen["drops"], "the-other-half-keeps-its-stack")
            survivors = open_chest(client, right)
            require(survivors[0] == (stone, 5), "slot-zero-was-the-right-half")
            client.send(0x0f, bytes([1]))

            # Break while open: the menu dies with the block, the contents drop, and a
            # click into the closed window cannot re-create a container entry.
            open_chest(client, right)
            break_block(client, right)
            seen = drain(client, 1.5)
            require(client.block(*right) == 0, "break-open-half")
            require(seen["closes"] == [b"\x01"], "break-closes-window-one")
            require((stone, 5) in seen["drops"], "break-drops-the-open-contents")
            client.send(0x0e, bytes([1]) + vint(0) + struct.pack(">hB", 0, 0)
                        + vint(0) + vint(0) + wire_stack())
            seen = drain(client, 0.8)
            require(not seen["menus"], "stale-click-opens-nothing")
            require(not seen["events"], "stale-click-touches-no-container")
            # Re-opening the same position finds a fresh, empty chest: the old contents
            # did not survive as a phantom entry under a position with no container.
            set_creative(client, 36, chest, 1)
            place(client, (right[0], right[1] - 1, right[2]))
            client.until(lambda *_: client.block(*right) != 0, "chest-replaced")
            replaced = open_chest(client, right)
            # Only the 27 container slots matter: the rest of the window is the player's
            # inventory, which is expected to be where the earlier stacks ended up.
            require(all(value == (0, 0) for value in replaced[:27]), "no-phantom-contents")
            client.send(0x0f, bytes([1]))

        # ---- kinds never pair, and a third chest cannot join a pair ----
        # Every right-click below has to stay inside the server's interaction range
        # (5.6 blocks creative from the eye to the block centre), which is why the chests
        # are crowded around the one cell the player stands in.
        plain, trap = (x, y + 1, z - 1), (x - 1, y + 1, z - 1)
        look(client, 0.0)
        set_creative(client, 36, chest, 1)
        place(client, (plain[0], plain[1] - 1, plain[2]))
        client.until(lambda *_: client.block(*plain) != 0, "plain-place")
        set_creative(client, 36, trapped, 1)
        place(client, (trap[0], trap[1] - 1, trap[2]))
        client.until(lambda *_: client.block(*trap) != 0, "trap-place")
        require(chest_props(by_name, client.block(*plain))["name"] == "chest", "plain-kind")
        require(chest_props(by_name, client.block(*trap))["name"] == "trapped_chest",
                "trap-kind")
        require(chest_props(by_name, client.block(*plain))["type"] == "single",
                "chest-beside-trapped-chest-stays-single")
        require(chest_props(by_name, client.block(*trap))["type"] == "single",
                "trapped-chest-beside-chest-stays-single")

        row = [(x + 2, y + 1, z), (x + 3, y + 1, z), (x + 4, y + 1, z)]
        set_creative(client, 36, chest, 3)
        for cell in row:
            place(client, (cell[0], cell[1] - 1, cell[2]))
            client.until(lambda *_: client.block(*cell) != 0, "row-place")
        first, middle, third = (chest_props(by_name, client.block(*cell)) for cell in row)
        require((first["type"], middle["type"]) == ("left", "right"), "row-pairs-one-neighbour")
        require(third["type"] == "single", "third-chest-cannot-join-a-pair")

        # ---- what a restart must carry: halves in the block state, slots per half ----
        saved = ((x, y + 1, z - 2), (x + 1, y + 1, z - 2))
        place_pair(client, by_name, saved[0], saved[1])
        set_creative(client, 36, stone, 4)
        open_double_chest(client, saved[1])
        require(click(client, 54 + 27) == (0, 0), "restart-carry")
        require(click(client, 0) == (stone, 4), "restart-put-right-half")
        client.send(0x0f, bytes([1]))
        set_creative(client, 36, dirt, 2)
        open_double_chest(client, saved[0])
        require(click(client, 54 + 27) == (0, 0), "restart-carry-left")
        require(click(client, 27) == (dirt, 2), "restart-put-left-half")
        client.send(0x0f, bytes([1]))
        return {"pair": [list(saved[0]), list(saved[1])], "right_slot0": [stone, 4],
                "left_slot0": [dirt, 2], "single": list(pos),
                "stand": [sx, y + 1.0, sz]}
    raise AssertionError("unreachable")


def verify_restart(game, saved, by_name):
    """Not a fresh world: the chest whose state and slots were written before the
    restart still has them, and its halves are still the same two block states."""
    left = tuple(saved["pair"][0])
    right = tuple(saved["pair"][1])
    with Client(game, "RestartTester") as client:
        client.login()
        enter(client)
        # The chests are where the previous session left them, so the viewer walks back
        # to the same stand: the interaction range is a server-side check on this packet.
        client.move(*saved["stand"])
        client.send(0x1c, struct.pack(">ff", 0.0, 0.0) + bytes([0]))
        lp = chest_props(by_name, client.block(*left))
        rp = chest_props(by_name, client.block(*right))
        require(lp is not None and rp is not None, "restart-states")
        require((lp["type"], rp["type"]) == ("left", "right"), "restart-halves")
        require(lp["facing"] == rp["facing"], "restart-facing")
        slots = open_double_chest(client, left)
        require(slots[0] == tuple(saved["right_slot0"]), "restart-right-half-contents")
        require(slots[27] == tuple(saved["left_slot0"]), "restart-left-half-contents")
        client.send(0x0f, bytes([1]))
        lone = chest_props(by_name, client.block(*tuple(saved["single"])))
        require(lone is not None and lone["type"] == "single", "restart-single-chest-state")


if __name__ == "__main__":
    binary = Path(sys.argv[1]).resolve()
    by_name = {block["name"]: block for block in WIRE_BLOCKS}
    try:
        with tempfile.TemporaryDirectory(prefix="spine-chest-restart-") as root:
            with local_server(binary, Path(root)) as game:
                saved = exercise(game)
            with local_server(binary, Path(root)) as game:
                verify_restart(game, saved, by_name)
    except ContractFailure as error:
        print(f"containers wire: FAIL {error.code}", file=sys.stderr)
        raise
    print(json.dumps({"protocol": 767, "partial_merge": True, "carried_remainder": 6,
                      "right_swap": True, "shift_destination": 62, "pair_halves": True,
                      "viewer_events": True, "break_closes_window": True,
                      "restart_persisted": True}, sort_keys=True))
    print("containers wire: PASS")
