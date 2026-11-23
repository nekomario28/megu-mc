#!/usr/bin/env python3
"""Live villager trade and chest transfer over the protocol-767 wire."""
import json
import math
import os
from pathlib import Path
import struct
import sys
import tempfile
import time

from container_wire import RCON_SECRET, Rcon, local_server_rcon, place, set_creative, use_on, wire_stack
from protocol767 import Client, ContractFailure, Reader, WIRE_BLOCKS, require, vint


def state_stack(reader):
    count = reader.varint()
    if count == 0:
        return (0, 0)
    item = reader.varint()
    added, removed = reader.varint(), reader.varint()
    # All fixture stacks here are ordinary vanilla items. Parse bounded component
    # lists so malformed or unexpected payloads fail instead of shifting fields.
    for _ in range(added):
        reader.varint()  # component type
        raise ContractFailure("unexpected-component-payload")
    require(removed == 0, "unexpected-removed-components")
    return (item, count)


def opened_container(client, pos):
    use_on(client, pos, 10)
    open_body = client.until_id(0x33, "chest-open")
    opened = Reader(open_body)
    window, menu = opened.varint(), opened.varint()
    require(menu == 2, "chest-generic-9x3-menu")
    content = Reader(client.until_id(0x13, "chest-content"))
    require(content.number("B") == window, "chest-window-id")
    state_id = content.varint()
    require(content.varint() == 63, "chest-slot-count")
    slots = [state_stack(content) for _ in range(63)]
    state_stack(content)  # carried stack
    require(content.pos == len(content.data), "chest-content-complete")
    return window, state_id, slots


def click(client, window, state_id, slot, contract):
    client.send(0x0e, bytes([window]) + vint(state_id) + struct.pack(">hB", slot, 0)
                + vint(0) + vint(0) + wire_stack())
    observed = {}

    def matches(pid, body):
        if pid != 0x15:
            return False
        reader = Reader(body)
        wid, sid, key = reader.number("B"), reader.varint(), reader.number(">h")
        value = state_stack(reader)
        require(reader.pos == len(reader.data), "set-slot-complete")
        if wid == window:
            observed["state_id"] = sid
            if key == slot:
                observed["value"] = value
                return True
        return False

    client.until(matches, contract)
    return observed["state_id"], observed["value"]


def open_and_read_offers(client, entity_id):
    # Interact (action=0), main hand (0).
    client.send(0x16, vint(entity_id) + vint(0) + vint(0))
    screen = Reader(client.until_id(0x33, "merchant-open-screen"))
    window, menu = screen.varint(), screen.varint()
    body = client.until_id(0x2d, "merchant-offers-packet")
    offers = Reader(body)
    offer_window, count = offers.varint(), offers.varint()
    require(offer_window == window, "merchant-window-id")
    require(count > 0, "merchant-has-offers")
    cost_item, cost_count, component_count = offers.varint(), offers.varint(), offers.varint()
    require(component_count == 0, "plain-farmer-cost")
    require(cost_count > 0, "merchant-cost-count")
    # The subsequent result/cost records are validated by packet framing and
    # exact offer count via the server's known vanilla encoder fields.
    # Skip first result slot and the optional second cost, then bound remaining
    # offer payload by requiring the known farmer table's two rows.
    # Result stacks are ordinary: count, item id, added count, removed count.
    result = state_stack(offers)
    result_item = result[0]
    require(result[0] > 0, "merchant-result-item")
    has_second = offers.number("B")
    require(has_second == 0, "farmer-no-second-cost")
    out_of_stock = offers.number("B")
    offers.take(24)  # uses, max uses, xp, special price, multiplier, demand
    require(out_of_stock == 0, "merchant-offer-in-stock")
    for _ in range(count - 1):
        offers.varint(); offers.varint()
        components = offers.varint()
        require(components == 0, "plain-farmer-cost")
        result = state_stack(offers)
        require(result[0] > 0, "merchant-result-item")
        require(offers.number("B") == 0, "farmer-no-second-cost")
        offers.number("B")
        offers.take(24)
    offers.varint()  # villager level
    offers.varint()  # villager XP
    offers.take(2)   # progress, restock
    require(offers.pos == len(offers.data), "merchant-offers-complete")
    require(menu > 0, "merchant-menu-type")
    return window, cost_item, cost_count, result_item


def merchant_state_id(client, window):
    content = Reader(client.until_id(0x13, "merchant-selection-content"))
    require(content.number("B") == window, "merchant-content-window")
    state_id = content.varint()
    count = content.varint()
    require(count >= 3, "merchant-content-slots")
    for _ in range(count):
        state_stack(content)
    state_stack(content)  # carried
    require(content.pos == len(content.data), "merchant-content-complete")
    return state_id


def trade_click(client, window, state_id, result_item):
    # clickType 1 = vanilla quick-move: the result stack moves straight into
    # the player inventory instead of riding the cursor.
    client.send(0x0e, bytes([window]) + vint(state_id) + struct.pack(">hB", 2, 0)
                + vint(1) + vint(0) + wire_stack())
    content = Reader(client.until_id(0x13, "trade-player-inventory-update"))
    require(content.number("B") == window, "trade-content-window")
    content.varint()  # new state id
    count = content.varint()
    require(count >= 39, "trade-full-menu-content")
    slots = [state_stack(content) for _ in range(count)]
    state_stack(content)  # carried
    require(content.pos == len(content.data), "trade-content-complete")
    require(slots[2] == (0, 0), "trade-result-slot-consumed")
    inventory = slots[3:]
    results = [stack for stack in inventory if stack[0] == result_item]
    require(bool(results), "trade-inventory-result-item")
    return results[0]


def run(game, rcon):
    solid_states = {state for block in WIRE_BLOCKS if block["boundingBox"] == "block"
                    for state in range(block["minStateId"], block["maxStateId"] + 1)}
    result = {"protocol": 767, "villager_trade": "NOT RUN", "container_transfer": "NOT RUN"}
    with Client(game, "TradeContainer") as client:
        client.login()
        if client.pos is None:
            client.until_id(0x40, "login-teleport")
        sx, sy, sz = client.pos
        rcon.command(f"gamemode creative {client.name}")
        mob_x, mob_y, mob_z = math.floor(sx), math.floor(sy), math.floor(sz)
        # A creative player floats: without a floor the summoned villager falls
        # out of the 3.2 interact range before the packet is read.
        rcon.command(f"fill {mob_x - 1} {mob_y - 1} {mob_z - 1} "
                     f"{mob_x + 1} {mob_y - 1} {mob_z + 1} minecraft:stone")
        rcon.command(f"summon minecraft:villager {mob_x} {mob_y} {mob_z} {{NoAI:1b,PersistenceRequired:1b}}")
        client.move(mob_x + 0.5, mob_y, mob_z + 0.5)

        entity_id = None
        spawn_deadline = time.monotonic() + float(os.environ.get("MC_PROBE_DEADLINE", "30"))
        while entity_id is None:
            remaining = spawn_deadline - time.monotonic()
            require(remaining > 0, "villager-spawn-timeout")
            pid, body = client.read(min(10, remaining))
            if pid != 0x01:
                continue
            add = Reader(body)
            eid = add.varint()
            add.take(16)
            etype = add.varint()
            if etype == 113:  # minecraft:villager in the protocol-767 entity registry
                entity_id = eid
        merchant_window, cost_item, cost_count, result_item = open_and_read_offers(client, entity_id)
        # Trade selection fills the input slot from this exact wire-reported cost.
        set_creative(client, 36, cost_item, max(cost_count, 20))
        client.send(0x2d, vint(0))
        trade_state = merchant_state_id(client, merchant_window)
        # Merchant result slot 2 performs the selected trade; inventory updates
        # arrive as authoritative set-slot packets on the same live connection.
        traded_stack = trade_click(client, merchant_window, trade_state, result_item)
        result["villager_trade"] = "PASS"
        result["trade_output"] = list(traded_stack)

        # Place a chest on a nearby solid shelf cell and transfer stone into it.
        bx, bz = mob_x + 1, mob_z
        ground = next((yy for yy in range(mob_y, -65, -1)
                       if client.block(bx, yy, bz) in solid_states), None)
        require(ground is not None, "chest-ground-found")
        chest_pos = (bx, ground + 1, bz)
        client.move(mob_x + 0.5, ground + 1.0, mob_z + 0.5)
        client.send(0x1c, struct.pack(">ff", 0.0, 0.0) + bytes([0]))
        set_creative(client, 36, 299, 1)
        place(client, (bx, ground, bz))
        client.until(lambda *_: client.block(*chest_pos) != 0, "chest-placed")
        set_creative(client, 36, 1, 12)
        window, state_id, _ = opened_container(client, chest_pos)
        state_id, picked_slot = click(client, window, state_id, 54, "pick-up-player-stack")
        require(picked_slot == (0, 0), "player-stack-picked-up")
        state_id, placed_stack = click(client, window, state_id, 0, "place-stack-in-chest")
        require(placed_stack == (1, 12), "stone-placed-in-chest")
        client.send(0x0f, bytes([window]))

    # New protocol client on the same server reads the persisted live container.
    with Client(game, "FreshChestReader") as reader:
        reader.login()
        reader.move(mob_x + 0.5, ground + 1.0, mob_z + 0.5)
        _, _, slots = opened_container(reader, chest_pos)
        require(slots[0] == (1, 12), "fresh-reader-chest-slot-zero")
        reader.send(0x0f, b"\x01")
    result["container_transfer"] = "PASS"
    result["chest_slot0"] = [1, 12]
    result["villager_entity_id"] = entity_id
    return result


if __name__ == "__main__":
    binary = Path(sys.argv[1]).resolve()
    summary = {"protocol": 767, "villager_trade": "NOT RUN", "container_transfer": "NOT RUN"}
    try:
        with tempfile.TemporaryDirectory(prefix="spine-trade-container-") as root:
            with local_server_rcon(binary, Path(root)) as (game, rcon_port):
                with Rcon(rcon_port, RCON_SECRET) as rcon:
                    summary = run(game, rcon)
    except Exception as error:
        summary["status"] = "FAIL"
        summary["failure"] = getattr(error, "code", str(error))
        print(json.dumps(summary, sort_keys=True))
        raise SystemExit(1)
    summary["status"] = "PASS"
    print(json.dumps(summary, sort_keys=True))
