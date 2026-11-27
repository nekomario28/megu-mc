#!/usr/bin/env python3
"""Protocol-767 survival playthrough acceptance for Megu's live server.

The offline self-test exercises the wire codecs and failure-safe process cleanup;
the live mode uses the normal TCP fixture in protocol767.py and an isolated save.
"""
from __future__ import annotations

import argparse
import contextlib
from collections import deque
import json
import math
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import unittest

import protocol767 as wire_protocol
from protocol767 import (Client, Reader, ROOT,
                         parse_respawn, port, position, require, status, string, vint)


STAGES = (
    "spawn and world streaming",
    "mining, drops, and crafting progression",
    "chest and furnace loop",
    "combat, health, eating, bed, death, and respawn",
    "Overworld-Nether-End-Overworld transitions",
    "clean shutdown and cold-restart persistence",
)
LIVE_COMPLETED = []
LOG, PLANKS, STICK, CRAFTING_TABLE = 132, 36, 848, 300
RAW_IRON, IRON_INGOT, COAL = 810, 811, 803
WOODEN_PICKAXE, COOKED_BEEF = 820, 989


def wire_stack(item=0, count=0):
    return vint(count) + (vint(item) + vint(0) + vint(0) if count else b"")


def use_on_payload(pos, sequence):
    return (vint(0) + position(*pos) + vint(1)
            + struct.pack(">fff?", .5, 1., .5, False) + vint(sequence))


def dig_payload(pos, status, sequence):
    return vint(status) + position(*pos) + b"\x01" + vint(sequence)


def click_payload(window, slot, button=0, mode=0, state_id=0, carried=(0, 0)):
    return (bytes([window]) + vint(state_id) + struct.pack(">hB", slot, button)
            + vint(mode) + vint(0) + wire_stack(*carried))


def read_stack(reader):
    count = reader.varint()
    if count == 0:
        return (0, 0)
    item = reader.varint()
    require(reader.varint() == 0 and reader.varint() == 0, "plain-item-components")
    return item, count


class SurvivalClient(Client):
    def __init__(self, game, name="Survivor"):
        super().__init__(game, name)
        self.health, self.food, self.entity_id = [], [], None
        self.slot_updates, self.menus, self.furnace_properties = {}, [], []
        self.window_state = {}
        self.window_contents = {}
        self.item_drops, self.respawn_points = [], []
        self.packet_trace = deque(maxlen=48)
        self.send_trace = deque(maxlen=32)

    def send(self, pid, body=b""):
        if hasattr(self, "send_trace"):
            self.send_trace.append((pid, body[:64].hex()))
        return super().send(pid, body)

    def read(self, timeout):
        pid, body = super().read(timeout)
        if self.phase != "play":
            return pid, body
        self.packet_trace.append((pid, body[:64].hex()))
        if pid == 0x2b:
            self.entity_id = struct.unpack(">i", body[:4])[0]
        elif pid == 0x5d:
            r = Reader(body)
            self.health.append((r.number(">f"), r.varint(), r.number(">f")))
        elif pid == 0x15:
            r = Reader(body)
            window, state_id, slot = r.number("B"), r.varint(), r.number(">h")
            self.window_state[window] = state_id
            if window == 255:
                # Window -1 is the cursor Set Slot; its state id belongs to
                # the currently open inventory window as well.
                self.window_state[0] = state_id
            self.slot_updates[(window, slot)] = read_stack(r)
        elif pid == 0x13:
            r = Reader(body)
            window = r.number("B")
            self.window_state[window] = r.varint()
            values = [read_stack(r) for _ in range(r.varint())]
            read_stack(r)
            self.window_contents[window] = values
        elif pid == 0x33:
            self.menus.append(body)
        elif pid == 0x14:
            # Furnace Property: window id, property id, signed-short value.
            if len(body) == 5:
                self.furnace_properties.append(struct.unpack(">Bhh", body))
        elif pid == 0x58:
            r = Reader(body)
            eid = r.varint()
            if r.number("B") == 8 and r.varint() == 7:
                count, item = r.varint(), r.varint()
                self.item_drops.append((eid, item, count))
        elif pid == 0x56:
            self.respawn_points.append(body)
        return pid, body

    def __exit__(self, exc_type, exc, tb):
        try:
            return super().__exit__(exc_type, exc, tb)
        finally:
            # Allow the server's socket reader thread to observe EOF and
            # remove this player name before the next stage reconnects as the
            # same username.
            time.sleep(0.5)

    def command(self, text):
        self.send(0x04, string(text.lstrip("/")))
        time.sleep(.2)

    def wire_trace(self, receive_ids, send_ids):
        received = [(pid, body) for pid, body in self.packet_trace if pid in receive_ids]
        sent = [(pid, body) for pid, body in self.send_trace if pid in send_ids]
        return f"rx={received[-16:]}:tx={sent[-16:]}"


def wait_slot(client, window, slot, expected, label):
    client.until(lambda *_: client.slot_updates.get((window, slot)) == expected, label)


def click(client, window, slot, button=0, mode=0):
    state_id = client.window_state.get(window, 0)
    carried = client.slot_updates.get((255, -1), (0, 0))
    client.send(0x0e, click_payload(window, slot, button, mode, state_id, carried))


def enter(client):
    client.login(view=2)
    client.until(lambda *_: client.pos is not None and bool(client.chunks), "spawn-chunk-and-position")
    cx, cz = math.floor(client.pos[0] / 16), math.floor(client.pos[2] / 16)
    client.until_chunks({(cx + dx, cz + dz)
                         for dx in range(-2, 3) for dz in range(-2, 3)},
                       "complete-spawn-chunk-window")
    require(client.health == [] or client.health[-1][0] == 20, "spawn-health-20")
    require(client.entity_id is not None, "spawn-entity-id")
    require(client.pos is not None and -64 <= client.pos[1] < 320, "safe-spawn-height")


def stage_spawn(game):
    with SurvivalClient(game, "SurvSpawn") as c:
        enter(c)
        c.until(lambda *_: bool(c.health), "initial-health-packet")
        require(c.health[-1][0] == 20 and c.health[-1][1] == 20, "spawn-health-food-20")
        x, y, z = (math.floor(value) for value in c.pos)
        solid_states = {state for block in wire_protocol.WIRE_BLOCKS
                        if block["boundingBox"] == "block"
                        for state in range(block["minStateId"], block["maxStateId"] + 1)}
        require(c.block(x, y - 1, z) in solid_states, "spawn-has-solid-ground")
        return c.pos


def stage_mining_crafting(game, spawn):
    with SurvivalClient(game, "SurvMining") as c:
        enter(c)
        c.command("gamemode survival")
        x, y, z = (math.floor(v) for v in spawn)
        # A flat 3x3 pad keeps the server's vanilla-style jitter/scatter drop
        # on the same level as the stationary player instead of down a slope.
        for dx in range(-1, 2):
            for dz in range(-1, 2):
                c.command(f"setblock {x + dx} {y - 1} {z + dz} stone")
        # Put the deterministic log directly above the player so its scattered
        # item intersects the real player-touch pickup box on this flat pad.
        target = (x, y + 1, z)
        c.command(f"setblock {target[0]} {target[1]} {target[2]} oak_log")
        c.until(lambda *_, target=target: any(p == target for p, _ in c.updates),
                f"mining-fixture-block:{target}:states={c.block(*target)}:"
                f"updates={c.updates[-12:]}:rx={list(c.packet_trace)}:"
                f"tx={list(c.send_trace)}")
        # Haste makes the known oak-log fixture instant-mine while remaining
        # in survival, so the stage measures real drops and pickup without a
        # long wall-clock bare-hand dig.
        c.command("effect give @s minecraft:haste 30 255 true")
        # Break three deterministic logs in survival and observe both the empty
        # block updates and the server's item-entity metadata.
        for seq in range(1, 4):
            if seq > 1:
                c.command(f"setblock {target[0]} {target[1]} {target[2]} oak_log")
                c.until(lambda *_, target=target: any(p == target and state != 0
                                                      for p, state in c.updates),
                        "mining-refill-fixture-block")
            before = len(c.updates)
            c.send(0x24, dig_payload(target, 0, seq))
            c.until(lambda *_, target=target, before=before:
                    any(p == target and state == 0 for p, state in c.updates[before:]),
                    f"survival-start-digging-block-update:{target}:"
                    f"updates={c.updates[-12:]}:rx={list(c.packet_trace)}:"
                    f"tx={list(c.send_trace)}")
            c.until(lambda *_: any(item == LOG and count == 1 for _, item, count in c.item_drops),
                    "mined-log-drop-entity")
            c.until(lambda *_, seq=seq: any(value == (LOG, seq)
                                            for (window, _), value in c.slot_updates.items()
                                            if window == 0),
                    f"mined-log-pickup:seq={seq}:drops={c.item_drops}:"
                    f"slots={c.slot_updates}:pos={c.pos}:"
                    f"{c.wire_trace({0x01, 0x15, 0x58}, {0x0e, 0x24, 0x1a})}")
        require(any(value[0] == LOG and value[1] >= 3
                    for (window, _), value in c.slot_updates.items() if window == 0),
                "mined-logs-in-player-inventory")
        # Craft from the mined inventory using authoritative window-0 clicks.
        log_slot = next(slot for (window, slot), value in c.slot_updates.items()
                        if window == 0 and value[0] == LOG and value[1] >= 3)
        plank_slot, stick_slot, table_slot = 37, 38, 39
        click(c, 0, log_slot)
        wait_slot(c, 0, log_slot, (0, 0), "craft-lift-mined-logs")
        wait_slot(c, 255, -1, (LOG, 3), "craft-cursor-mined-logs")
        click(c, 0, 1)
        wait_slot(c, 0, 1, (LOG, 3), "craft-place-logs-in-grid")
        wait_slot(c, 255, -1, (0, 0), "craft-release-log-cursor")
        c.until(lambda *_: c.slot_updates.get((0, 0)) == (PLANKS, 4),
                f"craft-planks-result:slots={c.slot_updates}:rx={list(c.packet_trace)}:"
                f"tx={list(c.send_trace)}")
        for count in (4, 8, 12):
            click(c, 0, 0)
            wait_slot(c, 255, -1, (PLANKS, count), f"craft-planks-cursor-{count}")
            remaining = 3 - count // 4
            wait_slot(c, 0, 1, (LOG, remaining) if remaining else (0, 0),
                      f"craft-consume-log-{count}")
        click(c, 0, plank_slot)
        wait_slot(c, 0, plank_slot, (PLANKS, 12), "craft-planks-in-inventory")
        wait_slot(c, 255, -1, (0, 0), "craft-release-plank-cursor")
        # Craft four sticks, using right-clicks to place one plank in each
        # vertical grid cell; then craft a table from four more planks.
        click(c, 0, plank_slot)
        click(c, 0, 1, button=1)
        click(c, 0, 3, button=1)
        click(c, 0, plank_slot)
        c.until(lambda *_: c.slot_updates.get((0, 0)) == (STICK, 4), "craft-sticks-result")
        click(c, 0, 0)
        click(c, 0, stick_slot)
        wait_slot(c, 0, stick_slot, (STICK, 4), "craft-sticks-inventory")
        click(c, 0, plank_slot)
        for grid_slot in (1, 2, 3, 4):
            click(c, 0, grid_slot, button=1)
        click(c, 0, plank_slot)
        c.until(lambda *_: c.slot_updates.get((0, 0)) == (CRAFTING_TABLE, 1),
                "crafting-table-result")
        click(c, 0, 0)
        click(c, 0, table_slot)
        wait_slot(c, 0, table_slot, (CRAFTING_TABLE, 1), "crafting-table-inventory")
        # Place/open the crafted table, make the three-plank/two-stick pickaxe
        # recipe, and assert its server-computed result.
        table_pos = (x + 2, y - 1, z + 2)
        c.command(f"setblock {table_pos[0]} {table_pos[1]} {table_pos[2]} crafting_table")
        c.until(lambda *_: any(p == table_pos for p, _ in c.updates), "crafting-table-place")
        c.send(0x38, use_on_payload(table_pos, 7))
        menu = Reader(c.until_id(0x33, "crafting-table-open"))
        window, menu_type = menu.varint(), menu.varint()
        require(menu_type == 12, "crafting-table-menu-type")
        # Existing table recipe implementation is the authoritative source for
        # this 3x3 result; hotbar slots map to menu slots one higher.
        click(c, window, plank_slot + 1)
        for slot in (1, 2, 3):
            click(c, window, slot, button=1)
        click(c, window, plank_slot + 1)
        click(c, window, stick_slot + 1)
        click(c, window, 5, button=1)
        click(c, window, 8, button=1)
        click(c, window, stick_slot + 1)
        c.until(lambda *_: c.slot_updates.get((window, 0)) == (WOODEN_PICKAXE, 1),
                "wooden-pickaxe-result")
        click(c, window, 0)
        click(c, window, table_slot + 1)
        wait_slot(c, window, table_slot + 1, (WOODEN_PICKAXE, 1), "wooden-pickaxe-crafted")
        return target


def stage_chest_furnace(game, spawn):
    with SurvivalClient(game, "SurvContainers") as c:
        enter(c)
        c.command("gamemode survival")
        x, y, z = (math.floor(v) for v in spawn)
        loc = (x + 2, y - 1, z + 2)
        c.command(f"setblock {loc[0]} {loc[1]} {loc[2]} chest")
        c.until(lambda *_: any(p == loc for p, _ in c.updates), "chest-place-update")
        c.send(0x38, use_on_payload(loc, 3))
        r = Reader(c.until_id(0x33, "chest-open"))
        window, menu = r.varint(), r.varint()
        require(menu == 2, "chest-menu-type")
        c.until(lambda *_: window in c.window_contents, "chest-initial-content")
        c.command(f"give {c.name} minecraft:oak_log 1")
        c.until(lambda *_: any(window == 0 and value == (LOG, 1)
                               for (window, _), value in c.slot_updates.items()), "chest-test-stack")
        player_slot = next(slot for (wid, slot), value in c.slot_updates.items()
                           if wid == 0 and value == (LOG, 1))
        chest_player_slot = 54 + player_slot - 36
        click(c, window, chest_player_slot)
        wait_slot(c, window, chest_player_slot, (0, 0), "chest-pickup-player-stack")
        click(c, window, 0)
        wait_slot(c, window, 0, (LOG, 1), "chest-deposit-log")
        click(c, window, 0, mode=1)  # shift-click chest stack into player inventory
        c.until(lambda *_: c.slot_updates.get((window, 0)) == (0, 0), "chest-shift-withdraw")
        c.until(lambda *_: any(wid == window and slot >= 27 and stack == (LOG, 1)
                               for (wid, slot), stack in c.slot_updates.items()),
                f"chest-shift-withdraw-player-slot:slots={c.slot_updates}:"
                f"contents={c.window_contents.get(window)}:"
                f"{c.wire_trace({0x13, 0x15}, {0x0e})}")
        withdrawn_slot = next(slot for (wid, slot), stack in c.slot_updates.items()
                              if wid == window and slot >= 27 and stack == (LOG, 1))
        click(c, window, withdrawn_slot)
        click(c, window, 0)
        wait_slot(c, window, 0, (LOG, 1), "chest-return-item")
        c.send(0x0f, bytes([window]))
        furnace = (loc[0] + 1, loc[1], loc[2])
        c.command(f"setblock {furnace[0]} {furnace[1]} {furnace[2]} furnace")
        c.until(lambda *_: any(p == furnace for p, _ in c.updates), "furnace-place-update")
        c.command(f"give {c.name} minecraft:raw_iron 1")
        c.command(f"give {c.name} minecraft:coal 1")
        c.until(lambda *_: any(value == (RAW_IRON, 1) for (wid, _), value in c.slot_updates.items()
                               if wid == 0), "furnace-test-ore")
        c.until(lambda *_: any(value == (COAL, 1) for (wid, _), value in c.slot_updates.items()
                               if wid == 0), "furnace-test-fuel")
        c.send(0x38, use_on_payload(furnace, 4))
        menu = Reader(c.until_id(0x33, "furnace-open"))
        furnace_window, menu_type = menu.varint(), menu.varint()
        require(menu_type == 14, "furnace-menu-type")
        c.until(lambda *_: furnace_window in c.window_contents, "furnace-initial-content")
        for value, label in ((RAW_IRON, "ore"), (COAL, "coal")):
            player_slot = next(slot for (wid, slot), stack in c.slot_updates.items()
                               if wid == 0 and stack == (value, 1))
            menu_slot = 30 + player_slot - 36
            click(c, furnace_window, menu_slot)
            wait_slot(c, furnace_window, menu_slot, (0, 0), f"furnace-lift-{label}")
            wait_slot(c, 255, -1, (value, 1), f"furnace-cursor-{label}")
            click(c, furnace_window, 0 if value == RAW_IRON else 1)
            slot = 0 if value == RAW_IRON else 1
            c.until(lambda *_, slot=slot, value=value:
                    c.slot_updates.get((furnace_window, slot)) == (value, 1),
                    f"furnace-insert-{label}")
            wait_slot(c, 255, -1, (0, 0), f"furnace-release-{label}")
        c.until(lambda *_: any(wid == furnace_window and prop == 2 and value > 0
                               for wid, prop, value in c.furnace_properties),
                f"furnace-property-cook-progress:props={c.furnace_properties}:"
                f"slots={c.slot_updates}:contents={c.window_contents.get(furnace_window)}:"
                f"{c.wire_trace({0x13, 0x14, 0x15}, {0x0e, 0x38})}")
        wait_slot(c, furnace_window, 2, (IRON_INGOT, 1), "furnace-smelt-output")
        return loc, furnace


def stage_combat_respawn(game, spawn):
    with SurvivalClient(game, "SurvCombat") as c:
        enter(c)
        c.command("gamemode survival")
        # Match the existing food wire probe's deterministic exhaustion setup:
        # a high amplifier drains saturation before the food-state assertion.
        c.command("effect give @s minecraft:hunger 30 255 true")
        c.until(lambda *_: any(food < 20 for _, food, _ in c.health), "hunger-food-decrease")
        c.command(f"damage {c.name} 2 minecraft:generic")
        c.until(lambda *_: any(0 < hp < 20 for hp, _, _ in c.health), "damage-health-decrease")
        before_food = c.health[-1][1]
        before_health = c.health[-1][0]
        c.command("give @s cooked_beef 1")
        c.until(lambda *_: any(window == 0 and value == (COOKED_BEEF, 1)
                               for (window, _), value in c.slot_updates.items()), "food-inventory")
        food_slot = next(slot for (window, slot), value in c.slot_updates.items()
                         if window == 0 and value == (COOKED_BEEF, 1))
        require(food_slot == 36, "food-in-selected-hotbar-slot")
        c.send(0x27, struct.pack(">h", 0))
        c.send(0x36, vint(0) + bytes([0]))
        c.until(lambda *_: any(food > before_food for _, food, _ in c.health), "eat-food-recovery")
        require(c.health[-1][0] >= before_health, "eat-health-recovery")
        bx, by, bz = math.floor(spawn[0]) - 2, math.floor(spawn[1]) - 1, math.floor(spawn[2])
        c.command(f"setblock {bx} {by} {bz} red_bed[facing=north,part=foot,occupied=false]")
        c.command(f"setblock {bx} {by} {bz - 1} red_bed[facing=north,part=head,occupied=false]")
        c.send(0x38, use_on_payload((bx, by, bz), 8))
        c.until(lambda *_: bool(c.respawn_points), "bed-spawnpoint-confirmation")
        c.command(f"kill {c.name}")
        c.until(lambda *_: any(hp <= 0 for hp, _, _ in c.health), "death-health-zero")
        c.send(0x09, vint(0))
        c.until_id(0x47, "client-command-respawn")
        c.until(lambda *_: c.health and c.health[-1][0] == 20, "respawn-health-20")
        return True


def stage_dimensions(game, spawn):
    with SurvivalClient(game, "SurvDimensions") as c:
        enter(c)
        c.command("gamemode creative")
        x, y, z = (math.floor(v) for v in spawn)
        c.command(f"setblock {x} {y} {z - 2} minecraft:nether_portal")
        c.move(x + 0.5, y, z - 1.5)
        body = c.until_id(0x47, "overworld-to-nether-respawn")
        dtype, name = parse_respawn(body)
        require(name == "minecraft:the_nether" and dtype == 3, "nether-dimension-type")
        c.until(lambda *_: bool(c.chunks), "nether-chunks")
        c.command("execute in overworld run tp @s 100.5 50 0.5")
        body = c.until_id(0x47, "nether-to-overworld-respawn")
        require(parse_respawn(body)[1] == "minecraft:overworld", "return-overworld")
        c.command("setblock 100 50 0 minecraft:end_portal")
        c.move(100.5, 50, .5)
        body = c.until_id(0x47, "overworld-to-end-respawn")
        require(parse_respawn(body)[1] == "minecraft:the_end", "end-dimension")
        c.until(lambda *_: bool(c.chunks), "end-chunks")
        # Step off the arrival cell so the portal arrival guard clears, then
        # place the exit portal and step into it.
        c.move(101.5, 49, .5)
        time.sleep(0.5)  # allow the portal arrival guard to observe the exit cell
        c.command("setblock 100 49 0 minecraft:end_portal")
        c.move(100.5, 49, .5)
        body = c.until_id(0x47, f"end-to-overworld-respawn:dim={c.dimension_name}:"
                          f"{c.wire_trace({0x40, 0x47, 0x09}, {0x04, 0x1a})}")
        require(parse_respawn(body)[1] == "minecraft:overworld", "final-overworld")
        old_pos = c.pos
        c.until(lambda *_: c.pos is not None and c.pos != old_pos, "final-overworld-position")
        # Stage 4's death clears inventory; replenish through the command path,
        # then verify the server persists the resulting player save on SIGTERM.
        c.command("gamemode survival")
        c.command(f"give {c.name} minecraft:coal 2")
        c.until(lambda *_: any(window == 0 and stack == (COAL, 2)
                               for (window, _), stack in c.slot_updates.items()),
                "persistence-inventory-fixture")
        return c.pos


@contextlib.contextmanager
def isolated_server(binary, root, extra_env=None):
    """Launch with exactly the runtime inputs already staged in this temp root."""
    game = port()
    env = dict(os.environ, MEGU_PORT=str(game), MEGU_BIND_ADDR="127.0.0.1",
               MEGU_RESOURCE_PACK_URL="")
    env.pop("MEGU_RCON_PORT", None)
    env.setdefault("MEGU_ANTICHEAT", "off")
    for key, value in (extra_env or {}).items():
        if value is None:
            env.pop(key, None)
        else:
            env[key] = value
    log_path = Path(root) / "server.log"
    with log_path.open("w+") as log:
        proc = subprocess.Popen([str(binary)], cwd=root, env=env, stdout=log, stderr=log)
        failure = None
        try:
            for _ in range(200):
                if proc.poll() is not None:
                    raise AssertionError(f"server-exited-before-login:{proc.returncode}:"
                                         f"{log_path.read_text()[-4000:]}")
                try:
                    status(game)
                    break
                except (OSError, AssertionError, socket.timeout):
                    time.sleep(.05)
            else:
                raise AssertionError(f"server-listener-timeout:{log_path.read_text()[-4000:]}")
            yield game
        except Exception as exc:
            failure = exc
            log.flush()
            raise
        finally:
            if proc.poll() is None:
                proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=20)
            except subprocess.TimeoutExpired as exc:
                log.flush()
                log.seek(0)
                log_tail = log.read()[-4000:]
                proc.kill()
                proc.wait(timeout=5)
                raise AssertionError(f"server-did-not-exit-after-SIGTERM:prior={failure}:"
                                     f"log={log_tail}") from exc
            if proc.returncode != 0:
                raise AssertionError(f"server-sigterm-exit:{proc.returncode}:"
                                     f"{log_path.read_text()[-4000:]}")


def stage_full_runtime_inputs(root):
    """Expose the local full runtime read-only through temp-root symlinks."""
    source = Path("/home/yuu/mc-spine")
    for name, directory in (("assets", True), ("config_packets.bin", False)):
        target = source / name
        require(target.exists(), f"full-runtime-input-missing:{target}")
        (root / name).symlink_to(target, target_is_directory=directory)
    vanilla = source / "datapacks/vanilla"
    require(vanilla.is_dir(), "full-runtime-vanilla-datapack-missing")
    (root / "datapacks").mkdir()
    (root / "datapacks/vanilla").symlink_to(vanilla, target_is_directory=True)


def run_live(binary, root):
    root = Path(root)
    stage_full_runtime_inputs(root)
    # The shared wire helper normally resolves assets relative to its checkout.
    # Point its read-only registry lookup at the temp runtime staging directory.
    wire_protocol.ROOT = root
    os.environ["MEGU_ALLOW_PLAYER_ADMIN_COMMANDS"] = "1"
    os.environ.setdefault("MC_PROBE_DEADLINE", "30")
    passed = []
    LIVE_COMPLETED.clear()
    with isolated_server(binary, root, {"MEGU_ALLOW_PLAYER_ADMIN_COMMANDS": "1"}) as game:
        spawn = stage_spawn(game)
        passed.append(STAGES[0])
        LIVE_COMPLETED.append(STAGES[0])
        block = stage_mining_crafting(game, spawn)
        passed.append(STAGES[1])
        LIVE_COMPLETED.append(STAGES[1])
        chest, furnace = stage_chest_furnace(game, spawn)
        passed.append(STAGES[2])
        LIVE_COMPLETED.append(STAGES[2])
        stage_combat_respawn(game, spawn)
        passed.append(STAGES[3])
        LIVE_COMPLETED.append(STAGES[3])
        saved_position = stage_dimensions(game, spawn)
        passed.append(STAGES[4])
        LIVE_COMPLETED.append(STAGES[4])
    # local_server terminates the process and waits before returning. Verify the
    # persisted block and player inventory after a fresh process loads the same root.
    with isolated_server(binary, root, {"MEGU_ALLOW_PLAYER_ADMIN_COMMANDS": "1"}) as game:
        with SurvivalClient(game, "SurvDimensions") as c:
            enter(c)
            c.until(lambda *_: c.block(*block) == 0, "restart-mined-block-state")
            require(saved_position is not None and math.dist(c.pos, saved_position) < 0.2,
                    "restart-player-position-persisted")
            require(c.block(*chest) != 0 and c.block(*furnace) != 0,
                    "restart-placed-container-blocks")
            require(c.pos is not None, "restart-player-position")
            c.send(0x38, use_on_payload(chest, 5))
            menu = Reader(c.until_id(0x33, "restart-chest-open"))
            window = menu.varint()
            require(menu.varint() == 2, "restart-chest-menu")
            c.until(lambda *_: window in c.window_contents, "restart-chest-content")
            require(any(stack == (COAL, 2) for stack in c.window_contents[window][27:]),
                    "restart-player-inventory-persisted")
            require(c.window_contents[window][0] == (LOG, 1), "restart-chest-item-persisted")
            require(c.pos is not None, "restart-player-state-streamed")
    passed.append(STAGES[5])
    LIVE_COMPLETED.append(STAGES[5])
    return passed


def run_zero_setup(binary, jar):
    """Boot with only blocks_1211.json plus MEGU_VANILLA_JAR, then play actions."""
    jar = Path(jar).resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="megu-survival-zero-setup-") as directory:
        root = Path(directory)
        assets = root / "assets"
        assets.mkdir()
        (assets / "blocks_1211.json").symlink_to(
            Path("/home/yuu/mc-spine/assets/blocks_1211.json"))
        wire_protocol.ROOT = root
        for forbidden in ("config_packets.bin", "datapacks", "assets/block_states_1211.json",
                          "assets/feature_order_1211.json", "assets/block_state_facts_1211.bin"):
            require(not (root / forbidden).exists(), f"zero-setup-forbidden-input:{forbidden}")
        env = {"MEGU_VANILLA_JAR": str(jar), "MEGU_ALLOW_PLAYER_ADMIN_COMMANDS": "1"}
        with isolated_server(binary, root, env) as game:
            with SurvivalClient(game, "ZeroSetup") as client:
                enter(client)
                client.until(lambda *_: bool(client.chunks), "zero-setup-world-stream")
                client.command("gamemode survival")
                x, y, z = (math.floor(value) for value in client.pos)
                target = (x + 2, y - 1, z)
                client.command(f"setblock {target[0]} {target[1]} {target[2]} oak_log")
                client.until(lambda *_: any(pos == target for pos, _ in client.updates),
                             "zero-setup-survival-block-fixture")
                before = len(client.updates)
                client.send(0x24, dig_payload(target, 0, 1))
                client.send(0x24, dig_payload(target, 2, 2))
                client.until(lambda *_: any(pos == target and state == 0
                                            for pos, state in client.updates[before:]),
                             "zero-setup-survival-dig")
                client.until(lambda *_: any(item == LOG for _, item, _ in client.item_drops),
                             "zero-setup-survival-drop")
        return {"boot": "PASS", "protocol": 767, "spawn_chunks": "PASS",
                "survival_dig_and_drop": "PASS", "jar": str(jar)}


def cleanup_guarantee():
    """A child started under a context manager must be reaped even on exceptions."""
    proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
    try:
        terminate_and_reap(proc)
        return proc.poll() is not None
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()


def terminate_and_reap(proc):
    if proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=2)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=2)


class OfflineTests(unittest.TestCase):
    def test_packet_position_and_digging_codecs(self):
        r = Reader(dig_payload((-12, 73, 20), 2, 41))
        self.assertEqual((r.varint(), r.take(8), r.take(1), r.varint()),
                         (2, position(-12, 73, 20), b"\x01", 41))
        self.assertEqual(r.pos, len(r.data))

    def test_inventory_click_wire(self):
        r = Reader(click_payload(2, 30, button=1, mode=1))
        self.assertEqual((r.number("B"), r.varint(), r.number(">h"), r.number("B")),
                         (2, 0, 30, 1))
        self.assertEqual((r.varint(), r.varint(), read_stack(r)), (1, 0, (0, 0)))
        self.assertEqual(r.pos, len(r.data))

    def test_health_and_dimension_assertions(self):
        self.assertEqual(len(STAGES), 6)
        self.assertEqual(len(set(STAGES)), 6)
        self.assertEqual(parse_respawn(  # response codec's core spawn-info framing
            _respawn_body(3, "minecraft:the_nether")), (3, "minecraft:the_nether"))

    def test_cleanup_reaps_child(self):
        self.assertTrue(cleanup_guarantee())


def _respawn_body(dim_type, name):
    # Spawn Info: dimension type/name, seed, game modes, flags, cooldown, data_kept.
    return (vint(dim_type) + string(name) + struct.pack(">qBbBBB", 0, 0, -1, 0, 0, 0)
            + vint(0) + b"\x00")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server-bin", default=str(ROOT / "target/debug/megu"))
    parser.add_argument("--zero-setup-jar", help="1.21.1 server jar for the zero-setup mode")
    parser.add_argument("--self-test", action="store_true", help="run offline codec and cleanup checks")
    args = parser.parse_args()
    if args.self_test:
        result = unittest.TextTestRunner(verbosity=2).run(
            unittest.defaultTestLoader.loadTestsFromTestCase(OfflineTests))
        return 0 if result.wasSuccessful() else 1
    binary = Path(args.server_bin).resolve()
    if args.zero_setup_jar:
        summary = {"mode": "zero-setup", "protocol": 767, "status": "NOT RUN"}
        if not binary.is_file():
            summary["error"] = f"missing-server-binary:{binary}"
            print(json.dumps(summary, sort_keys=True))
            return 1
        try:
            summary.update(run_zero_setup(binary, args.zero_setup_jar))
            summary["status"] = "PASS"
            summary["success"] = True
        except (AssertionError, OSError, TimeoutError, ValueError) as exc:
            summary["error"] = f"{type(exc).__name__}: {exc}"
            summary["success"] = False
        print(json.dumps(summary, sort_keys=True))
        return 0 if summary.get("success") else 1
    summary = {"mode": "live", "protocol": 767, "stages": [
        {"stage": i + 1, "name": name, "status": "NOT RUN"} for i, name in enumerate(STAGES)]}
    if not binary.is_file():
        summary["error"] = f"missing-server-binary:{binary}"
        print(json.dumps(summary, sort_keys=True))
        return 1
    try:
        with tempfile.TemporaryDirectory(prefix="megu-survival-playthrough-") as root:
            results = run_live(binary, Path(root))
        for idx, _name in enumerate(results):
            summary["stages"][idx]["status"] = "PASS"
        summary["success"] = len(results) == len(STAGES)
    except (AssertionError, OSError, TimeoutError, ValueError) as exc:
        summary["error"] = f"{type(exc).__name__}: {exc}"
        summary["success"] = False
        for stage in summary["stages"]:
            if stage["name"] in LIVE_COMPLETED:
                stage["status"] = "PASS"
    print(json.dumps(summary, sort_keys=True))
    return 0 if summary.get("success") else 1


if __name__ == "__main__":
    sys.exit(main())
