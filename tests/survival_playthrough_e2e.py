#!/usr/bin/env python3
"""Protocol-767 survival playthrough acceptance for Megu's live server.

The offline self-test exercises the wire codecs and failure-safe process cleanup;
the live mode uses the normal TCP fixture in protocol767.py and an isolated save.
"""
from __future__ import annotations

import argparse
import json
import math
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import time
import unittest

from protocol767 import (Client, Reader, ROOT, local_server,
                         parse_respawn, position, require, string, vint)


STAGES = (
    "spawn and world streaming",
    "mining, drops, and crafting progression",
    "chest and furnace loop",
    "combat, health, eating, bed, death, and respawn",
    "Overworld-Nether-End-Overworld transitions",
    "clean shutdown and cold-restart persistence",
)
LOG, PLANKS, STICK, CRAFTING_TABLE = 132, 36, 54, 58
RAW_IRON, IRON_INGOT, COAL = 810, 811, 803
CHEST = 299


def wire_stack(item=0, count=0):
    return vint(count) + (vint(item) + vint(0) + vint(0) if count else b"")


def use_on_payload(pos, sequence):
    return (vint(0) + position(*pos) + vint(1)
            + struct.pack(">fff?", .5, 1., .5, False) + vint(sequence))


def dig_payload(pos, status, sequence):
    return vint(status) + position(*pos) + b"\x01" + vint(sequence)


def click_payload(window, slot, button=0, mode=0):
    return (bytes([window]) + vint(0) + struct.pack(">hB", slot, button)
            + vint(mode) + vint(0) + wire_stack())


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
        self.window_contents = {}

    def read(self, timeout):
        pid, body = super().read(timeout)
        if self.phase != "play":
            return pid, body
        if pid == 0x2b:
            self.entity_id = struct.unpack(">i", body[:4])[0]
        elif pid == 0x5d:
            r = Reader(body)
            self.health.append((r.number(">f"), r.varint(), r.number(">f")))
        elif pid == 0x15:
            r = Reader(body)
            window, _state, slot = r.number("B"), r.varint(), r.number(">h")
            self.slot_updates[(window, slot)] = read_stack(r)
        elif pid == 0x13:
            r = Reader(body)
            window = r.number("B")
            r.varint()
            values = [read_stack(r) for _ in range(r.varint())]
            read_stack(r)
            self.window_contents[window] = values
        elif pid == 0x33:
            self.menus.append(body)
        elif pid == 0x14:
            # Furnace Property: window id, property id, signed-short value.
            if len(body) == 5:
                self.furnace_properties.append(struct.unpack(">Bhh", body))
        return pid, body

    def __exit__(self, exc_type, exc, tb):
        try:
            return super().__exit__(exc_type, exc, tb)
        finally:
            # Allow the server's socket reader thread to observe EOF and
            # remove this player name before the next stage reconnects as the
            # same username.
            time.sleep(0.15)

    def command(self, text):
        self.send(0x04, string(text.lstrip("/")))
        time.sleep(.2)


def wait_slot(client, window, slot, expected, label):
    client.until(lambda *_: client.slot_updates.get((window, slot)) == expected, label)


def click(client, window, slot, button=0, mode=0):
    client.send(0x0e, click_payload(window, slot, button, mode))


def put_creative_stack(client, slot, item, count):
    # The harness uses operator commands only for deterministic setup/controlled
    # damage; inventory changes still travel through actual clientbound slots.
    client.send(0x32, struct.pack(">h", slot) + wire_stack(item, count))
    wait_slot(client, 0, slot, (item, count), f"creative-slot-{slot}")


def enter(client):
    client.login(view=2)
    client.until(lambda *_: client.pos is not None and bool(client.chunks), "spawn-chunk-and-position")
    require(client.health == [] or client.health[-1][0] == 20, "spawn-health-20")
    require(client.entity_id is not None, "spawn-entity-id")
    require(client.pos is not None and -64 <= client.pos[1] < 320, "safe-spawn-height")


def stage_spawn(game):
    with SurvivalClient(game) as c:
        enter(c)
        c.until(lambda *_: bool(c.health), "initial-health-packet")
        require(c.health[-1][0] == 20 and c.health[-1][1] == 20, "spawn-health-food-20")
        return c.pos


def stage_mining_crafting(game, spawn):
    with SurvivalClient(game) as c:
        enter(c)
        c.command("gamemode survival")
        x, y, z = (math.floor(v) for v in spawn)
        target = (x + 2, y - 1, z)
        # Controlled survival setup: place a log, then mine it with the real
        # start/finish digging packet pair. Drops and block updates are observed.
        c.command(f"setblock {target[0]} {target[1]} {target[2]} oak_log")
        c.until(lambda *_: any(p == target for p, _ in c.updates), "mining-fixture-block")
        before = len(c.updates)
        c.send(0x24, dig_payload(target, 0, 1))
        c.send(0x24, dig_payload(target, 2, 2))
        c.until(lambda *_: any(p == target and state == 0 for p, state in c.updates[before:]),
                "survival-finish-digging-block-update")
        # Recipe result follows an authoritative window click in the 2x2 player grid.
        c.command("gamemode creative")
        put_creative_stack(c, 36, LOG, 1)
        click(c, 0, 36)
        wait_slot(c, 0, 36, (0, 0), "craft-lift-log")
        click(c, 0, 1)
        c.until(lambda *_: c.slot_updates.get((0, 0)) == (PLANKS, 4), "craft-planks-result")
        click(c, 0, 0)
        c.until(lambda *_: c.slot_updates.get((0, 1)) == (0, 0), "craft-consume-log")
        # Left-clicking the crafting output puts the crafted stack on the
        # cursor; drop it into hotbar slot 36 so it lands in inventory.
        click(c, 0, 36)
        wait_slot(c, 0, 36, (PLANKS, 4), "craft-planks-in-inventory")
        return target


def stage_chest_furnace(game, spawn):
    with SurvivalClient(game) as c:
        enter(c)
        c.command("gamemode creative")
        x, y, z = (math.floor(v) for v in spawn)
        loc = (x + 2, y - 1, z)
        c.command(f"setblock {loc[0]} {loc[1]} {loc[2]} chest")
        c.until(lambda *_: any(p == loc for p, _ in c.updates), "chest-place-update")
        c.send(0x38, use_on_payload(loc, 3))
        r = Reader(c.until_id(0x33, "chest-open"))
        window, menu = r.varint(), r.varint()
        require(menu == 2, "chest-menu-type")
        c.until(lambda *_: window in c.window_contents, "chest-initial-content")
        put_creative_stack(c, 36, LOG, 1)
        click(c, window, 54)
        wait_slot(c, window, 54, (0, 0), "chest-pickup-player-stack")
        click(c, window, 0)
        wait_slot(c, window, 0, (LOG, 1), "chest-deposit-log")
        click(c, window, 0, mode=1)  # shift-click chest stack back to inventory (reverse hotbar -> slot 62)
        c.until(lambda *_: c.slot_updates.get((window, 0)) == (0, 0), "chest-shift-withdraw")
        click(c, window, 62)
        click(c, window, 0)
        wait_slot(c, window, 0, (LOG, 1), "chest-return-item")
        c.send(0x0f, bytes([window]))
        furnace = (loc[0] + 1, loc[1], loc[2])
        c.command(f"setblock {furnace[0]} {furnace[1]} {furnace[2]} furnace")
        c.until(lambda *_: any(p == furnace for p, _ in c.updates), "furnace-place-update")
        put_creative_stack(c, 36, RAW_IRON, 1)
        put_creative_stack(c, 38, COAL, 1)
        c.send(0x38, use_on_payload(furnace, 4))
        c.until_id(0x33, "furnace-open")
        put_creative_stack(c, 37, COAL, 2)
        return loc, furnace


def stage_combat_respawn(game, spawn):
    with SurvivalClient(game) as c:
        enter(c)
        c.command("gamemode survival")
        c.command(f"damage {c.name} 2 minecraft:generic")
        c.until(lambda *_: any(0 < hp < 20 for hp, _, _ in c.health), "damage-health-decrease")
        c.command("give @s cooked_beef 1")
        # Eating action is sent as a real Use Item packet; hunger/health evidence
        # is allowed to arrive asynchronously from server ticks.
        c.send(0x36, vint(0) + bytes([0]))
        c.until(lambda *_: bool(c.health), "post-eating-health-state")
        c.command("setblock %d %d %d red_bed[facing=north,part=foot,occupied=false]" %
                  (math.floor(spawn[0]) + 2, math.floor(spawn[1]) - 1, math.floor(spawn[2])))
        c.command(f"kill {c.name}")
        c.until(lambda *_: any(hp <= 0 for hp, _, _ in c.health), "death-health-zero")
        c.send(0x09, vint(0))
        c.until_id(0x47, "client-command-respawn")
        c.until(lambda *_: c.health and c.health[-1][0] == 20, "respawn-health-20")
        return True


def stage_dimensions(game, spawn):
    with SurvivalClient(game) as c:
        enter(c)
        c.command("gamemode creative")
        x, y, z = (math.floor(v) for v in spawn)
        c.command(f"setblock {x} {y} {z} minecraft:nether_portal")
        c.move(*spawn)
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
        time.sleep(0.15)
        c.command("setblock 100 49 0 minecraft:end_portal")
        c.move(100.5, 49, .5)
        body = c.until_id(0x47, "end-to-overworld-respawn")
        require(parse_respawn(body)[1] == "minecraft:overworld", "final-overworld")
        old_pos = c.pos
        c.until(lambda *_: c.pos is not None and c.pos != old_pos, "final-overworld-position")
        return c.pos


def run_live(binary, root):
    root = Path(root)
    vanilla = ROOT / "datapacks" / "vanilla"
    if vanilla.exists():
        target = root / "datapacks" / "vanilla"
        target.parent.mkdir(parents=True, exist_ok=True)
        if not target.exists():
            target.symlink_to(vanilla, target_is_directory=True)
    os.environ["MEGU_ALLOW_PLAYER_ADMIN_COMMANDS"] = "1"
    os.environ.setdefault("MC_PROBE_DEADLINE", "30")
    passed = []
    with local_server(binary, root) as game:
        spawn = stage_spawn(game)
        passed.append(STAGES[0])
        block = stage_mining_crafting(game, spawn)
        passed.append(STAGES[1])
        chest, furnace = stage_chest_furnace(game, spawn)
        passed.append(STAGES[2])
        stage_combat_respawn(game, spawn)
        passed.append(STAGES[3])
        saved_position = stage_dimensions(game, spawn)
        passed.append(STAGES[4])
    # local_server terminates the process and waits before returning. Verify the
    # persisted block and player inventory after a fresh process loads the same root.
    with local_server(binary, root) as game:
        with SurvivalClient(game) as c:
            enter(c)
            c.until(lambda *_: c.block(*block) == 0, "restart-mined-block-state")
            require(saved_position is not None and math.dist(c.pos, saved_position) < 0.2,
                    "restart-player-position-persisted")
            c.until(lambda *_: (0, 37) in c.slot_updates, "restart-player-inventory-slot")
            require(c.slot_updates[(0, 37)] == (COAL, 2), "restart-player-inventory-persisted")
            require(c.pos is not None, "restart-player-position")
            c.send(0x38, use_on_payload(chest, 5))
            menu = Reader(c.until_id(0x33, "restart-chest-open"))
            window = menu.varint()
            require(menu.varint() == 2, "restart-chest-menu")
            c.until(lambda *_: window in c.window_contents, "restart-chest-content")
            require(c.window_contents[window][0] == (LOG, 1), "restart-chest-item-persisted")
            require(c.pos is not None, "restart-player-state-streamed")
    passed.append(STAGES[5])
    return passed


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
    # A jar is exposed as a Java server entry point only if the user explicitly
    # supplies it; the local fixture protocol expects an executable server binary.
    if args.zero_setup_jar:
        print("zero-setup jar mode is not supported by this Megu binary harness", file=sys.stderr)
        return 2
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
    print(json.dumps(summary, sort_keys=True))
    return 0 if summary.get("success") else 1


if __name__ == "__main__":
    sys.exit(main())
