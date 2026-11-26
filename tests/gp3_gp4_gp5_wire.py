#!/usr/bin/env python3
"""Exercise live redstone, merchant and advanced menu packets on protocol 767."""

from collections import Counter
import math
import os
from pathlib import Path
import struct
import sys
import tempfile

from protocol767 import Client, ContractFailure, ROOT, Reader, WIRE_BLOCKS, local_server, position, require, string, vint

BLOCK = {block["name"]: block for block in WIRE_BLOCKS}
ITEM = {
    "stone": 1, "emerald": 806, "wheat": 854, "bread": 855, "lapis_lazuli": 807, "book": 925,
    "enchanted_book": 1114, "nether_wart": 997, "potion": 998, "blaze_powder": 1002,
    "diamond_sword": 838, "netherite_sword": 843, "netherite_ingot": 816,
    "netherite_upgrade_smithing_template": 1269, "sentry_armor_trim_smithing_template": 1270,
    "diamond_chestplate": 869, "iron_ingot": 811,
}
for name in ("redstone_wire", "redstone_block", "redstone_lamp", "repeater", "lever",
             "stone_button", "observer", "anvil", "enchanting_table", "brewing_stand", "smithing_table"):
    ITEM[name] = BLOCK[name]["drops"][0]

EMPTY = (0, 0, {})


def block_properties(name, state):
    block = BLOCK[name]
    if state is None or not block["minStateId"] <= state <= block["maxStateId"]:
        return {}
    offset = state - block["minStateId"]
    props = {}
    for prop in reversed(block["states"]):
        offset, index = divmod(offset, prop["num_values"])
        values = prop.get("values", ("true", "false"))
        props[prop["name"]] = values[index]
    return props


def stack(reader):
    count = reader.varint()
    if not count:
        return EMPTY
    item, added, removed = reader.varint(), reader.varint(), reader.varint()
    components = {}
    for _ in range(added):
        kind = reader.varint()
        if kind == 5:
            value = reader.nbt()
        elif kind == 16:
            value = reader.varint()
        elif kind in (9, 23):
            value = {reader.varint(): reader.varint() for _ in range(reader.varint())}
            require(reader.number("B") in (0, 1), "enchantment-tooltip-flag")
        elif kind == 31:
            require(reader.number("B") == 1, "potion-holder-present")
            value = reader.varint()
            require((reader.number("B"), reader.varint()) == (0, 0), "plain-potion-contents")
        elif kind == 35:
            value = reader.varint(), reader.varint(), reader.number("B")
        else:
            raise ContractFailure(f"unexpected-stack-component-{kind}")
        components[kind] = value
    for _ in range(removed):
        reader.varint()
    return item, count, components


class WireClient(Client):
    def __init__(self, game, name):
        super().__init__(game, name)
        self.sequence = 0
        self.window = None
        self.contents = {}
        self.properties = {}
        self.block_events = {}
        self.brew_clock = None
        self.clock_window = None
        self.xp_level = None

    def read(self, timeout):
        before = len(self.updates)
        pid, body = super().read(timeout)
        if self.phase != "play":
            return pid, body
        if pid == 0x14:
            wid, prop, value = struct.unpack(">Bhh", body)
            self.properties.setdefault(wid, {})[prop] = value
            if wid == self.clock_window and prop == 0:
                self.brew_clock = value
        elif pid == 0x13:
            reader = Reader(body)
            wid = reader.number("B")
            state = reader.varint()
            values = [stack(reader) for _ in range(reader.varint())]
            cursor = stack(reader)
            require(reader.pos == len(body), "complete-window-contents")
            self.contents[wid] = state, values, cursor
        elif pid == 0x5c:
            reader = Reader(body)
            reader.number(">f")
            self.xp_level = reader.varint()
        elif pid in (0x09, 0x49):
            for pos, state in self.updates[before:]:
                self.block_events.setdefault(pos, []).append((state, self.brew_clock))
        return pid, body

    def command(self, command, prefix):
        start = len(self.messages)
        self.send(0x04, string(command))
        self.until(lambda *_: any(message.startswith(prefix) for message in self.messages[start:]),
                   f"{command} expects {prefix!r}")

    def set_item(self, slot, name=None, count=1, component=b""):
        item = vint(0) if not name else (
            vint(count) + vint(ITEM[name]) + vint(bool(component)) + vint(0) + component
        )
        self.send(0x32, struct.pack(">h", slot) + item)

        def changed(pid, body):
            if pid != 0x15:
                return False
            reader = Reader(body)
            wid = reader.number("B")
            reader.varint()
            key = reader.number(">h")
            return wid == 0 and key == slot

        self.until(changed, f"creative-set-slot-{slot}")

    def use(self, pos):
        self.sequence += 1
        self.send(0x38, vint(0) + position(*pos) + vint(1)
                  + struct.pack(">fff?", 0.5, 1.0, 0.5, False) + vint(self.sequence))

    def destroy(self, pos):
        self.sequence += 1
        self.send(0x24, vint(0) + position(*pos) + b"\x01" + vint(self.sequence))

    def wait_block(self, pos, name, **props):
        ident = BLOCK[name]

        def observed(*_):
            state = self.block(*pos)
            return state is not None and ident["minStateId"] <= state <= ident["maxStateId"] and all(
                block_properties(name, state).get(key) == value for key, value in props.items()
            )

        if not observed():
            self.until(observed, f"block-{pos}-{name}-{props}")

    def place(self, pos, name, **props):
        self.set_item(36, name)
        x, y, z = pos
        self.use((x, y - 1, z))
        self.wait_block(pos, name, **props)

    def open(self, kind, slots, properties=()):
        reader = Reader(self.until_id(0x33, f"menu-{kind}-open"))
        self.window = reader.varint()
        require(reader.varint() == kind and bool(reader.nbt()), f"menu-{kind}-type-and-title")
        require(reader.pos == len(reader.data), f"menu-{kind}-open-payload")
        self.wait_contents()
        require(len(self.contents[self.window][1]) == slots + 36, f"menu-{kind}-slot-count")
        if properties:
            self.wait_properties(properties)
        return self.window

    def wait_contents(self, previous=None):
        if self.contents.get(self.window) is previous:
            self.until(lambda *_: self.contents.get(self.window) is not previous, "menu-contents")
        return self.contents[self.window][1:]

    def wait_properties(self, properties):
        def observed(*_):
            return all(key in self.properties.get(self.window, {}) for key in properties)

        if not observed():
            self.until(observed, "menu-property-set")
        return self.properties[self.window]

    def click(self, slot, mode=0):
        previous = self.contents[self.window]
        self.send(0x0e, bytes([self.window]) + vint(previous[0])
                  + struct.pack(">hB", slot, 0) + vint(mode) + vint(0) + vint(0))
        return self.wait_contents(previous)

    def move(self, menu_slots, hotbar_slot, target):
        self.click(menu_slots + 27 + hotbar_slot)
        values, cursor = self.click(target)
        require(cursor == EMPTY, f"target-{target}-accepted-input")
        return values[target]

    def close(self):
        old = self.contents.get(0)
        self.send(0x0f, bytes([self.window]))
        self.window = 0
        values, cursor = self.wait_contents(old)
        require(cursor == EMPTY, "closed-menu-clears-cursor")
        totals = Counter()
        for item, count, _ in values:
            if item:
                totals[item] += count
        return totals


def boot(client, name):
    client.login()
    if client.pos is None:
        client.until_id(0x40)
    # Players spawn in the configured default gamemode (survival); the creative
    # inventory packet below is only honoured for a creative-mode client.
    client.command("gamemode creative", f"Set {client.name}'s gamemode to creative")
    x, y, z = map(math.floor, client.pos)
    needed = {(bx // 16, bz // 16) for bx in (x - 5, x + 5) for bz in (z - 5, z + 5)}
    client.until(lambda *_: needed.issubset(client.chunks), f"{name}-fixture-chunks")
    return x, y, z


def brewing_and_redstone(game):
    with WireClient(game, "CircuitBrewer") as client:
        x, y, z = boot(client, "circuit")
        source, wire, repeater, lamp = [(x + dx, y, z + 1) for dx in (-1, 0, 1, 2)]
        stand = x, y, z - 1
        observer, watched = (x + 3, y, z - 1), (x + 2, y, z - 1)
        client.command(f"fill {x - 1} {y - 1} {z - 1} {x + 3} {y - 1} {z + 2} stone", "Filled")
        client.wait_block((x + 3, y - 1, z + 2), "stone")

        client.place(wire, "redstone_wire", power="0")
        client.send(0x1c, struct.pack(">ff?", -90.0, 0.0, True))
        client.place(repeater, "repeater", facing="west", powered="false", delay="1")
        for delay in range(2, 5):
            client.use(repeater)
            client.wait_block(repeater, "repeater", delay=str(delay))
        client.place(lamp, "redstone_lamp", lit="false")
        client.place(stand, "brewing_stand", has_bottle_0="false")
        # Vanilla ObserverBlock faces the player's look direction (west = +90°).
        client.send(0x1c, struct.pack(">ff?", 90.0, 0.0, True))
        client.place(observer, "observer", facing="west", powered="false")
        client.set_item(36, "redstone_block")
        client.set_item(37, "potion", component=vint(31) + b"\x01\x00\x00\x00")
        client.set_item(38, "nether_wart")
        client.set_item(39, "blaze_powder")
        client.use(stand)
        client.clock_window = client.open(11, 5, (0, 1))
        require(client.properties[client.window] == {0: 0, 1: 0}, "initial-brewing-properties")
        require(client.move(5, 1, 0) == (ITEM["potion"], 1, {31: 0}), "water-bottle-placed")
        client.wait_block(stand, "brewing_stand", has_bottle_0="true")
        client.move(5, 2, 3)
        client.move(5, 3, 4)
        client.until(lambda *_: client.brew_clock == 400, "brew-starts-at-400-ticks")
        client.until(lambda *_: client.properties[client.window][1] == 19, "brew-spends-one-fuel")
        require(client.contents[client.window][1][4] == EMPTY, "brew-consumes-blaze-powder")

        def edge(pos, name, prop, value):
            client.wait_block(pos, name, **{prop: value})
            state = client.block(*pos)
            return next(clock for current, clock in reversed(client.block_events[pos]) if current == state)

        # Brewing's per-tick Container Set Data provides the server clock on the
        # same TCP stream as the circuit's section updates.
        x0, y0, z0 = source
        client.until(lambda *_: client.brew_clock == 399, "clock-before-source")
        client.use((x0, y0 - 1, z0))
        on = edge(wire, "redstone_wire", "power", "15")
        client.wait_block(source, "redstone_block")
        require(client.block_events[source][-1][1] == on, "wire-powers-on-source-placement-tick")
        require(block_properties("repeater", client.block(*repeater))["powered"] == "false",
                "wire-powers-before-repeater")
        require(block_properties("redstone_lamp", client.block(*lamp))["lit"] == "false",
                "lamp-waits-for-repeater")
        require(on - edge(repeater, "repeater", "powered", "true") == 8, "repeater-rising-delay-8gt")
        require(on - edge(lamp, "redstone_lamp", "lit", "true") == 8, "lamp-lights-on-repeater-tick")
        clock = client.brew_clock
        client.until(lambda *_: client.brew_clock != clock, "clock-before-source-break")
        client.destroy(source)
        off = edge(wire, "redstone_wire", "power", "0")
        require(block_properties("repeater", client.block(*repeater))["powered"] == "true",
                "wire-unpowers-before-repeater")
        require(off - edge(repeater, "repeater", "powered", "false") == 8, "repeater-falling-delay-8gt")
        require(off - edge(lamp, "redstone_lamp", "lit", "false") == 12, "lamp-unlights-after-4gt")

        source_support = x0, y0 - 1, z0
        client.set_item(36, "lever")
        client.use(source_support)
        client.wait_block(source, "lever", powered="false")
        client.use(source)
        edge(wire, "redstone_wire", "power", "15")
        client.use(source)
        edge(wire, "redstone_wire", "power", "0")
        client.destroy(source)
        client.wait_block(source, "air")
        client.set_item(36, "stone_button")
        client.use(source_support)
        client.wait_block(source, "stone_button", powered="false")
        clock = client.brew_clock
        client.until(lambda *_: client.brew_clock != clock, "clock-before-button")
        client.use(source)
        pressed = edge(wire, "redstone_wire", "power", "15")
        released = edge(wire, "redstone_wire", "power", "0")
        require(pressed - released == 20, f"button-releases-after-20gt: {pressed} -> {released}")
        client.set_item(36, "stone")
        clock = client.brew_clock
        client.until(lambda *_: client.brew_clock != clock, "clock-before-observer-input")
        client.use((watched[0], watched[1] - 1, watched[2]))
        client.wait_block(watched, "stone")
        changed = client.block_events[watched][-1][1]
        pulse = edge(observer, "observer", "powered", "true")
        require(changed - pulse == 2, "observer-starts-pulse-after-2gt")
        require(pulse - edge(observer, "observer", "powered", "false") == 2,
                "observer-pulse-width-2gt")
        print("protocol 767 redstone: PASS (repeater 8/8 gt, lamp +0/+4 gt, button 20 gt, observer 2/2 gt)",
              flush=True)

        original = client.window
        client.close()
        client.use(stand)
        client.clock_window = client.open(11, 5, (0, 1))
        require(client.window != original and 0 < client.properties[client.window][0] < 400,
                "brew-progress-persists-across-menu-close")
        for stage in (300, 200, 100):
            client.until(lambda *_, stage=stage: client.brew_clock is not None and 0 < client.brew_clock <= stage,
                         f"brewing-countdown-{stage}")
        client.until(lambda *_: client.brew_clock == 0
                     and client.contents[client.window][1][0][2] == {31: 3}
                     and client.properties[client.window][1] == 19, "brewing-completes-with-awkward-potion")
        require(client.contents[client.window][1][3] == EMPTY, "brew-consumes-ingredient")
        values, cursor = client.click(0)
        require(values[0] == EMPTY and cursor == (ITEM["potion"], 1, {31: 3}), "take-brewed-potion")
        require(client.close()[ITEM["potion"]] == 1, "brewed-potion-in-player-inventory")
        client.use(stand)
        client.open(11, 5, (0, 1))
        values = client.contents[client.window][1]
        bottle = next(i for i, value in enumerate(values) if value[0] == ITEM["potion"])
        client.click(bottle)
        client.click(0)
        client.close()
        return stand


def brewing_restart(game, stand):
    with WireClient(game, "CircuitBrewer") as client:
        boot(client, "brewing-restart")
        client.use(stand)
        client.open(11, 5, (0, 1))
        require(client.contents[client.window][1][0] == (ITEM["potion"], 1, {31: 3}),
                "brewing-stand-retains-potion-across-restart")
        require(client.properties[client.window] == {0: 0, 1: 19}, "brewing-fuel-persists")
        print("protocol 767 brewing: PASS (400 gt, fuel, potion, close/reopen, restart)", flush=True)


def merchant_offers(body):
    reader = Reader(body)
    window = reader.varint()

    def cost():
        item, count = reader.varint(), reader.varint()
        require(reader.varint() == 0, "starter-cost-component-predicate")
        return item, count

    offers = []
    for _ in range(reader.varint()):
        first, result = cost(), stack(reader)
        second = cost() if reader.number("B") else None
        disabled = reader.number("B")
        uses, maximum, xp, special, multiplier, demand = struct.unpack(">iiiifi", reader.take(24))
        offers.append((first, result, second, disabled, uses, maximum, xp, special, multiplier, demand))
    level, experience = reader.varint(), reader.varint()
    show, restock = reader.number("B"), reader.number("B")
    require(reader.pos == len(body) and (show, restock) == (1, 0), "complete-merchant-offers")
    return window, offers, level, experience


def merchant(game):
    with WireClient(game, "FarmerTrader") as client:
        x, y, z = boot(client, "merchant")
        client.set_item(37, "wheat", 25)
        # Pen the villager into its own cell first: the stroll goal walks it out
        # of the 3.2-block interact reach before the menu is reopened below.
        client.command(f"fill {x + 1} {y - 1} {z - 1} {x + 3} {y - 1} {z + 1} stone", "Filled")
        for wx, wz in ((x + 3, z), (x + 1, z), (x + 2, z - 1), (x + 2, z + 1)):
            client.command(f"fill {wx} {y} {wz} {wx} {y + 2} {wz} stone", "Filled")
        client.command(f"summon villager {x + 2.5} {y} {z + 0.5}", "Summoned villager")
        eid = None

        def villager(pid, body):
            nonlocal eid
            if pid != 0x01:
                return False
            reader = Reader(body)
            entity = reader.varint()
            reader.take(16)
            if reader.varint() == 113:
                eid = entity
                return True
            return False

        client.until(villager, "villager-spawn-entity")
        client.send(0x16, vint(eid) + vint(0) + vint(0) + b"\0")
        screen = Reader(client.until_id(0x33, "merchant-open"))
        client.window = screen.varint()
        require(screen.varint() == 19 and bool(screen.nbt()), "merchant-menu-type")
        window, offers, level, xp = merchant_offers(client.until_id(0x2d, "initial-offers"))
        require((window, level, xp) == (client.window, 1, 0) and len(offers) == 2, "starter-villager-offers")
        first, result, second, disabled, uses, maximum, reward, _, _, _ = offers[0]
        require(first == (ITEM["wheat"], 20) and result == (ITEM["emerald"], 1, {})
                and second is None and (disabled, uses, maximum, reward) == (0, 0, 16, 2),
                "initial-trade-payload")
        client.wait_contents()
        for index in (0, 1, 0):
            old = client.contents[window]
            client.send(0x2d, vint(index))
            slots, _ = client.wait_contents(old)
            if index == 1:
                require(slots[:3] == [EMPTY] * 3 and sum(s[1] for s in slots if s[0] == ITEM["wheat"]) == 25,
                        "select-trade-returns-old-payment")
        require(slots[0] == (ITEM["wheat"], 25, {}) and slots[2] == result, "trade-autofill-and-preview")
        previous = client.contents[window]
        client.send(0x0e, bytes([window]) + vint(previous[0]) + struct.pack(">hB", 2, 0) + b"\0\0\0")
        updated = merchant_offers(client.until_id(0x2d, "completed-trade-updates-offers"))
        require(updated[0] == window and updated[1][0][4] == 1 and updated[3] == reward,
                "trade-increments-offer-uses-and-villager-xp")
        slots, cursor = client.wait_contents(previous)
        require(slots[0] == (ITEM["wheat"], 5, {}) and slots[2] == EMPTY and cursor == result,
                "trade-spends-cost-and-transfers-result")
        client.click(30)
        inventory = client.close()
        require(inventory[ITEM["wheat"]] == 5 and inventory[ITEM["emerald"]] == 1,
                "merchant-close-returns-unused-wheat-and-keeps-result")
        client.send(0x16, vint(eid) + vint(0) + vint(0) + b"\0")
        reader = Reader(client.until_id(0x33, "merchant-reopen"))
        client.window = reader.varint()
        reopened = merchant_offers(client.until_id(0x2d, "merchant-reopened-offers"))
        require(reopened[1][0][4] == 1 and reopened[3] == reward, "per-villager-uses-survive-close")
        client.wait_contents()
        client.close()
        print("protocol 767 merchant: PASS (reselection, atomic trade, uses/XP, reopening)", flush=True)


def anvil(game):
    with WireClient(game, "AnvilTester") as client:
        x, y, z = boot(client, "anvil")
        pos = x + 1, y, z
        client.command(f"fill {x - 1} {y - 1} {z - 1} {x + 2} {y - 1} {z + 1} stone", "Filled")
        client.wait_block((x + 1, y - 1, z), "stone")
        client.place(pos, "anvil")
        client.set_item(37, "diamond_sword")
        client.set_item(38, "iron_ingot", 2)
        client.command("gamemode survival", "Set AnvilTester's gamemode to survival")
        client.use(pos)
        client.open(8, 3, (0,))
        require(client.properties[client.window] == {0: 0}, "initial-anvil-cost")
        client.move(3, 1, 0)
        previous = client.contents[client.window]
        client.send(0x2a, string("Lane Blade"))
        slots, _ = client.wait_contents(previous)
        require(slots[2][:2] == (ITEM["diamond_sword"], 1) and slots[2][2].get(5) == "Lane Blade",
                "anvil-rename-result-component")
        client.until(lambda *_: client.properties[client.window][0] == 1, "anvil-rename-cost-is-one")
        _, cursor = client.click(2)
        require(cursor == EMPTY, "anvil-rejects-insufficient-xp")
        client.command("xp set @s 5 levels", "set xp for 1 target")
        values, cursor = client.click(2)
        require(values[:3] == [EMPTY] * 3 and cursor[2].get(5) == "Lane Blade", "anvil-take-named-result")
        require(client.xp_level == 4, "anvil-deducts-one-level")
        require(client.close()[ITEM["diamond_sword"]] == 1, "anvil-keeps-result-on-close")
        client.use(pos)
        client.open(8, 3, (0,))
        client.move(3, 2, 1)
        require(client.close()[ITEM["iron_ingot"]] == 2, "anvil-close-returns-input")
        print("protocol 767 anvil: PASS (rename, XP gate and deduction, pickup, close)", flush=True)


def enchanting(game):
    with WireClient(game, "EnchantTester") as client:
        x, y, z = boot(client, "enchanting")
        pos = x, y, z - 2
        _, _, cz = pos
        client.command(f"fill {x - 2} {y - 1} {cz - 2} {x + 2} {y - 1} {cz + 2} stone", "Filled")
        client.wait_block((x, y - 1, cz), "stone")
        client.place(pos, "enchanting_table")
        for xa, za, xb, zb in ((x - 2, cz - 2, x + 2, cz - 2), (x - 2, cz + 2, x + 2, cz + 2),
                                (x - 2, cz - 1, x - 2, cz + 1), (x + 2, cz - 1, x + 2, cz + 1)):
            client.command(f"fill {xa} {y} {za} {xb} {y} {zb} bookshelf", "Filled")
        client.set_item(37, "book")
        client.set_item(38, "lapis_lazuli", 4)
        client.command("gamemode survival", "Set EnchantTester's gamemode to survival")
        client.command("xp set @s 40 levels", "set xp for 1 target")
        client.use(pos)
        client.open(13, 2, range(10))
        require(client.properties[client.window][0] == 0, "empty-enchanting-cost")
        client.move(2, 1, 0)
        client.move(2, 2, 1)
        client.until(lambda *_: client.properties[client.window][2] == 30, "bookshelf-count-produces-level-30")
        clue = client.properties[client.window][6]
        require(clue >= 0, "enchanting-clue-id")
        previous = client.contents[client.window]
        client.send(0x0d, vint(client.window) + vint(2))
        values, _ = client.wait_contents(previous)
        require(values[0][0] == ITEM["enchanted_book"] and values[0][2].get(23)
                and values[1] == (ITEM["lapis_lazuli"], 1, {}), "enchanted-book-and-lapis-cost")
        require(client.xp_level == 37, "enchanting-deducts-three-levels")
        inventory = client.close()
        require(inventory[ITEM["enchanted_book"]] == inventory[ITEM["lapis_lazuli"]] == 1,
                "enchanting-close-returns-book-and-lapis")
        print("protocol 767 enchanting: PASS (15 shelves, button, lapis, close)", flush=True)


def smithing(game):
    with WireClient(game, "SmithTester") as client:
        x, y, z = boot(client, "smithing")
        pos = x - 1, y, z
        client.command(f"fill {x - 2} {y - 1} {z - 1} {x + 1} {y - 1} {z + 1} stone", "Filled")
        client.wait_block((x - 1, y - 1, z), "stone")
        client.place(pos, "smithing_table")
        inputs = ("netherite_upgrade_smithing_template", "diamond_sword", "netherite_ingot",
                  "sentry_armor_trim_smithing_template", "diamond_chestplate", "iron_ingot")
        for i, name in enumerate(inputs, 37):
            client.set_item(i, name)
        client.use(pos)
        client.open(21, 4)
        for i in range(3):
            client.move(4, i + 1, i)
        require(client.contents[client.window][1][3] == (ITEM["netherite_sword"], 1, {}),
                "smithing-netherite-preview")
        values, _ = client.click(3, mode=1)
        require(values[:4] == [EMPTY] * 4 and any(s[0] == ITEM["netherite_sword"] for s in values[4:]),
                "smithing-shift-take-is-atomic")
        for i in range(3):
            client.move(4, i + 4, i)
        require(client.contents[client.window][1][3] == (ITEM["diamond_chestplate"], 1, {35: (6, 9, 1)}),
                "smithing-armor-trim-preview")
        values, cursor = client.click(3)
        require(values[:4] == [EMPTY] * 4 and cursor[2] == {35: (6, 9, 1)}, "smithing-trim-pickup")
        require(client.close()[ITEM["diamond_chestplate"]] == 1, "smithing-trim-in-inventory")
        client.set_item(42, "iron_ingot")
        client.use(pos)
        client.open(21, 4)
        client.move(4, 6, 2)
        require(client.close()[ITEM["iron_ingot"]] == 1, "smithing-close-returns-input")
        print("protocol 767 smithing: PASS (netherite upgrade, trim, close)", flush=True)


def main(binary):
    os.environ["MEGU_ALLOW_PLAYER_ADMIN_COMMANDS"] = "1"
    os.environ.setdefault("MC_PROBE_DEADLINE", "30")
    with tempfile.TemporaryDirectory(prefix="spine-gp3-gp4-gp5-") as directory:
        root = Path(directory)
        def fixture(name):
            world = root / name
            world.mkdir()
            (world / "datapacks").symlink_to(ROOT / "datapacks", target_is_directory=True)
            return world

        circuit = fixture("circuit")
        with local_server(binary, circuit) as game:
            stand = brewing_and_redstone(game)
        with local_server(binary, circuit) as game:
            brewing_restart(game, stand)
        for scenario in (merchant, anvil, enchanting, smithing):
            with local_server(binary, fixture(scenario.__name__)) as game:
                scenario(game)


if __name__ == "__main__":
    binary = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else ROOT / "target/debug/megu"
    main(binary)
    print("gp3/gp4/gp5 wire: PASS")
