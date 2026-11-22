#!/usr/bin/env python3
"""Two-client, real-server Alpha release acceptance for Minecraft protocol 767.

The seven steps correspond in order to docs/release-checklist.md section 6.
All observations come from server packets, with no synthetic game responses.
"""

import argparse
import json
import math
from pathlib import Path
import socket
import struct
import sys
import tempfile
import time
import unittest

from container_wire import stack, wire_stack
from protocol767 import (AIR_STATES, Client, Reader, ROOT,
                         WIRE_BLOCKS, frame_packet, local_server, position,
                         require, unpack_position, unframe_packet, vint)


CRITERIA = (
    "configuration registries and tags",
    "dry spawn chunks and movement without rubber-band",
    "block placement and break visible to second client",
    "chest open, deposit, withdraw, and close",
    "chat and /seed and /time set day",
    "death, respawn, disconnect, and reconnect",
    "stop, restart, world edit, inventory, and position persist",
)
STONE_ITEM = 1
CHEST_ITEM = 299
BLOCKS = {block["name"]: block for block in WIRE_BLOCKS}
SOLID_STATES = {
    state for block in WIRE_BLOCKS if block["boundingBox"] == "block"
    for state in range(block["minStateId"], block["maxStateId"] + 1)
}
FLUID_STATES = {
    state for name in ("water", "lava")
    for state in range(BLOCKS[name]["minStateId"], BLOCKS[name]["maxStateId"] + 1)
}


def use_on_payload(block, sequence):
    return (vint(0) + position(*block) + vint(1)
            + struct.pack(">fff?", 0.5, 1.0, 0.5, False) + vint(sequence))


def break_payload(block, sequence):
    return vint(0) + position(*block) + bytes([1]) + vint(sequence)


def click_payload(window, state, slot):
    # No predicted slot changes: the server remains authoritative.
    return (bytes([window]) + vint(state) + struct.pack(">hB", slot, 0)
            + vint(0) + vint(0) + wire_stack())


def move_payload(coords):
    return struct.pack(">ddd?", *coords, True)


def chat_component(body):
    root = Reader(body).nbt()
    return root if isinstance(root, str) else root.get("text", "")


class AlphaClient(Client):
    """Record messages the base Protocol 767 fixture would otherwise consume."""

    def __init__(self, game, name):
        super().__init__(game, name)
        self.config_packets = []
        self.position_syncs = []
        self.health = []
        self.entity_id = None
        self.tracked_entities = set()
        self.entity_movements = []
        self.chat_lines = []
        self.slot_updates = {}
        self.menu_state = {}
        self.window_contents = {}
        self.last_chat = 0.0

    def read(self, timeout):
        configuration = self.phase == "configuration"
        pid, body = super().read(timeout)
        if configuration:
            self.config_packets.append(pid)
        if self.phase != "play":
            return pid, body
        if pid == 0x2b:
            self.entity_id = struct.unpack(">i", body[:4])[0]
        elif pid == 0x40:
            self.position_syncs.append(self.pos)
        elif pid == 0x5d:
            self.health.append(Reader(body).number(">f"))
        elif pid == 0x01:
            self.tracked_entities.add(Reader(body).varint())
        elif pid in (0x2e, 0x2f, 0x30, 0x70):
            self.entity_movements.append(Reader(body).varint())
        elif pid == 0x1e:
            self.chat_lines.append(chat_component(body))
        elif pid == 0x13:
            r = Reader(body)
            window, state = r.number("B"), r.varint()
            values = [stack(r) for _ in range(r.varint())]
            stack(r)  # carried item
            require(r.pos == len(body), "complete-container-content")
            self.window_contents[window] = values
            self.menu_state[window] = state
        elif pid == 0x15:
            r = Reader(body)
            window, state, slot = r.number("B"), r.varint(), r.number(">h")
            value = stack(r)
            require(r.pos == len(body), "complete-set-slot")
            self.slot_updates[(window, slot)] = value
            self.menu_state[window] = state
            if window in self.window_contents and 0 <= slot < len(self.window_contents[window]):
                self.window_contents[window][slot] = value
        return pid, body

    def command(self, command, response):
        self.chat_action("/" + command)
        before = len(self.messages)
        # Responses arrive asynchronously; the command is confirmed only by its reply.
        self.until(lambda *_: any((response(message) if callable(response) else response in message)
                                  for message in self.messages[before:]),
                   f"command-response:{command}")

    def chat_action(self, text):
        time.sleep(max(0, self.last_chat + 1.1 - time.monotonic()))
        self.last_chat = time.monotonic()
        self.chat(text)


def spawn_ready(client):
    client.login(view=2)
    client.until(lambda *_: client.pos is not None and
                 (math.floor(client.pos[0]) // 16, math.floor(client.pos[2]) // 16) in client.chunks,
                 "spawn-chunk-arrived")
    require(client.entity_id is not None, "join-entity-id")


def wait_block(client, block, predicate, label, since=0):
    client.until(lambda *_: any(p == block and predicate(state)
                               for p, state in client.updates[since:]), label)
    require(predicate(client.block(*block)), label + ":state")


def set_item(client, slot, item, count):
    client.slot_updates.pop((0, slot), None)
    client.send(0x32, struct.pack(">h", slot) + wire_stack(item, count))
    client.until(lambda *_: client.slot_updates.get((0, slot)) == (item, count),
                 f"creative-slot-{slot}")


def click(client, window, slot, result):
    client.slot_updates.pop((window, slot), None)
    client.send(0x0e, click_payload(window, client.menu_state[window], slot))
    client.until(lambda *_: client.slot_updates.get((window, slot)) == result,
                 f"click-{window}-{slot}-updates")


def chest_menu(client, block):
    client.send(0x38, use_on_payload(block, 60))
    r = Reader(client.until_id(0x33, "chest-open-menu"))
    window, menu = r.varint(), r.varint()
    require(menu == 2, "single-chest-menu-type")
    client.until(lambda *_: window in client.window_contents, "chest-initial-contents")
    require(len(client.window_contents[window]) == 63, "chest-27-plus-36-slots")
    return window


def surface(client):
    x, y, z = client.pos
    bx, by, bz = math.floor(x), math.floor(y), math.floor(z)
    require(client.min_y < by < client.max_y - 2, "spawn-height")
    # Vanilla clients may receive a spawn one block above the dry landing cell.
    # Require a safe landing immediately beneath the spawn, then simulate the fall.
    landing = next((feet for feet in (by, by - 1)
                    if client.block(bx, feet - 1, bz) in SOLID_STATES
                    and client.block(bx, feet, bz) in AIR_STATES), None)
    require(landing is not None, "spawn-safe-solid-landing")
    require(all(client.block(bx, yy, bz) in AIR_STATES for yy in range(landing, by + 2)),
            "spawn-head-and-feet-dry-and-unobstructed")
    require(all(client.block(bx, yy, bz) not in FLUID_STATES
                for yy in range(landing - 1, by + 2)),
            "spawn-not-in-fluid")
    return bx, landing, bz


def exercise(binary, root, passed):
    extra_env = {"MEGU_ALLOW_PLAYER_ADMIN_COMMANDS": "1", "MEGU_SAFE_SPAWN": "1"}
    persisted = {}
    with local_server(binary, root, extra_env=extra_env) as game:
        with AlphaClient(game, "Alice") as alice, AlphaClient(game, "Bob") as bob:
            spawn_ready(alice)
            spawn_ready(bob)
            for client in (alice, bob):
                require(client.contract_started and client.phase == "play", "configuration-complete")
                require(client.registry_counts.get("minecraft:dimension_type", 0) > 0,
                        "configuration-dimension-registry")
                require(client.registry_counts.get("minecraft:worldgen/biome", 0) > 0,
                        "configuration-biome-registry")
                require(0x0d in client.config_packets, "configuration-tags-delivered")
            passed.append(CRITERIA[0])

            ground = surface(alice)
            surface(bob)
            bob.until(lambda *_: alice.entity_id in bob.tracked_entities,
                      "Bob-tracks-Alice-before-movement")
            initial = alice.pos
            alice_syncs = len(alice.position_syncs)
            target = (initial[0] + 0.16, float(ground[1]), initial[2])
            alice.send(0x1a, move_payload(target))
            bob.until(lambda *_: alice.entity_id in bob.entity_movements,
                      "legal-movement-visible-to-Bob")
            # Drain a following tick so a delayed server correction is observable.
            deadline = time.monotonic() + 0.35
            while time.monotonic() < deadline:
                try:
                    alice.read(min(0.05, deadline - time.monotonic()))
                except socket.timeout:
                    continue
            require(len(alice.position_syncs) == alice_syncs, "movement-rubber-band-correction")
            passed.append(CRITERIA[1])

            alice.command("gamemode creative", "gamemode")
            # Work beside the spawn; the block under the adjacent cell must support placement.
            x, y, z = ground
            block = (x + 1, y, z)
            support = (x + 1, y - 1, z)
            require(alice.block(*block) in AIR_STATES and
                    alice.block(*support) in SOLID_STATES, "adjacent-placement-site")
            set_item(alice, 36, STONE_ITEM, 8)
            baseline = len(bob.updates)
            alice.send(0x38, use_on_payload(support, 61))
            wait_block(bob, block, lambda state: state == BLOCKS["stone"]["defaultState"],
                       "Bob-sees-Alice-place-stone", baseline)
            baseline = len(bob.updates)
            alice.send(0x24, break_payload(block, 62))
            wait_block(bob, block, lambda state: state in AIR_STATES,
                       "Bob-sees-Alice-break-stone", baseline)
            passed.append(CRITERIA[2])

            set_item(alice, 36, CHEST_ITEM, 1)
            baseline = len(alice.updates)
            alice.send(0x38, use_on_payload(support, 63))
            chest = BLOCKS["chest"]
            wait_block(alice, block,
                       lambda state: chest["minStateId"] <= state <= chest["maxStateId"],
                       "Alice-places-chest", baseline)
            set_item(alice, 36, STONE_ITEM, 6)
            window = chest_menu(alice, block)
            require(alice.window_contents[window][0] == (0, 0), "empty-chest-slot")
            # For a 27-slot chest, server slot 54 maps to player inventory slot 36.
            click(alice, window, 54, (0, 0))
            click(alice, window, 0, (STONE_ITEM, 6))
            click(alice, window, 0, (0, 0))
            click(alice, window, 55, (STONE_ITEM, 6))
            alice.send(0x0f, bytes([window]))
            passed.append(CRITERIA[3])

            phrase = "alpha-release-chat-check"
            alice.chat_action(phrase)
            bob.until(lambda *_: any(phrase in line and "Alice" in line
                                     for line in bob.chat_lines), "Bob-receives-Alice-chat")
            alice.command("seed", lambda reply: reply.strip().lstrip("-").isdigit())
            alice.command("time set day", "Set time to")
            passed.append(CRITERIA[4])

            alice.command("kill Alice", "Killed")
            alice.until(lambda *_: any(value <= 0 for value in alice.health), "Alice-death-health")
            before = len(alice.position_syncs)
            alice.send(0x09, vint(0))  # Client Command: perform respawn
            alice.until_id(0x47, "Alice-respawn-packet")
            alice.until(lambda *_: len(alice.position_syncs) > before and
                        alice.health and alice.health[-1] > 0, "Alice-respawn-health-position")
            alice.socket.close()
            with AlphaClient(game, "Alice") as returning:
                spawn_ready(returning)
                require(returning.pos is not None, "Alice-reconnected")
                passed.append(CRITERIA[5])
                returning.command("gamemode creative", "gamemode")
                set_item(returning, 37, STONE_ITEM, 13)
                initial = returning.pos
                saved_pos = (initial[0] + 0.125, initial[1], initial[2])
                returning.send(0x1a, move_payload(saved_pos))
                time.sleep(0.3)
                require(chest["minStateId"] <= returning.block(*block) <= chest["maxStateId"],
                        "world-edit-before-save")
                persisted = {"block": block, "position": saved_pos, "slot": 37,
                             "item": (STONE_ITEM, 13)}
    # local_server sends SIGTERM, waits for graceful world save, then exits.
    # Reuse precisely the same temporary world directory for the second process.
    with local_server(binary, root, extra_env=extra_env) as game:
        with AlphaClient(game, "Alice") as alice:
            spawn_ready(alice)
            block = persisted["block"]
            chest = BLOCKS["chest"]
            require(chest["minStateId"] <= alice.block(*block) <= chest["maxStateId"],
                    "restart-preserved-world-edit")
            require(math.dist(alice.pos, persisted["position"]) < 0.2,
                    "restart-preserved-player-position")
            if alice.slot_updates.get((0, persisted["slot"])) != persisted["item"]:
                alice.until(lambda *_: alice.slot_updates.get((0, persisted["slot"])) ==
                            persisted["item"], "restart-preserved-inventory")
            passed.append(CRITERIA[6])


class OfflineTests(unittest.TestCase):
    def test_checklist_steps(self):
        self.assertEqual(len(CRITERIA), 7)
        self.assertEqual(len(set(CRITERIA)), 7)
        self.assertIn("configuration", CRITERIA[0])
        self.assertIn("second client", CRITERIA[2])
        self.assertIn("restart", CRITERIA[6])

    def test_use_on(self):
        r = Reader(use_on_payload((-12, 73, 20), 41))
        self.assertEqual(r.varint(), 0)
        self.assertEqual(unpack_position(r.take(8)), (-12, 73, 20))
        self.assertEqual(r.varint(), 1)
        self.assertEqual(r.take(13), struct.pack(">fff?", 0.5, 1, 0.5, False))
        self.assertEqual(r.varint(), 41)
        self.assertEqual(r.pos, len(r.data))

    def test_break(self):
        r = Reader(break_payload((3, 70, -5), 71))
        self.assertEqual(r.varint(), 0)
        self.assertEqual(unpack_position(r.take(8)), (3, 70, -5))
        self.assertEqual(r.take(1), b"\x01")
        self.assertEqual(r.varint(), 71)
        self.assertEqual(r.pos, len(r.data))

    def test_click_and_stack(self):
        r = Reader(click_payload(1, 4, 54))
        self.assertEqual((r.number("B"), r.varint(), r.number(">h")), (1, 4, 54))
        self.assertEqual((r.number("B"), r.varint(), r.varint()), (0, 0, 0))
        self.assertEqual(stack(r), (0, 0))
        self.assertEqual(r.pos, len(r.data))
        self.assertEqual(stack(Reader(wire_stack(STONE_ITEM, 13))), (STONE_ITEM, 13))

    def test_move(self):
        coords = (0.66, 72.0, -0.125)
        self.assertEqual(struct.unpack(">ddd?", move_payload(coords)), (*coords, True))

    def test_chat(self):
        raw = b"\x0a\x08\x00\x04text\x00\x05hello\x00"
        self.assertEqual(chat_component(raw), "hello")
        self.assertEqual(chat_component(b"\x08\x00\x05hello"), "hello")

    def test_framing(self):
        packet = vint(0x38) + use_on_payload((1, 80, 1), 9)
        for threshold in (-1, 0, 256):
            encoded = frame_packet(packet, threshold)
            r = Reader(encoded)
            body = r.take(r.varint())
            self.assertEqual(unframe_packet(body, threshold), packet)

    def test_state_registries(self):
        self.assertIn(BLOCKS["stone"]["defaultState"], SOLID_STATES)
        self.assertIn(BLOCKS["water"]["defaultState"], FLUID_STATES)
        self.assertTrue(AIR_STATES.isdisjoint(SOLID_STATES | FLUID_STATES))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("server_binary", nargs="?", default=str(ROOT / "target/debug/megu"))
    parser.add_argument("--self-test", action="store_true", help="offline payload and step tests")
    parser.add_argument("--json", action="store_true", help="machine-readable summary only")
    args = parser.parse_args()
    if args.self_test:
        outcome = unittest.TestResult()
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(OfflineTests)
        suite.run(outcome)
        summary = {"mode": "self-test", "passed": outcome.testsRun - len(outcome.failures)
                   - len(outcome.errors), "total": outcome.testsRun,
                   "success": outcome.wasSuccessful()}
        if args.json:
            print(json.dumps(summary, sort_keys=True))
        else:
            for failure in outcome.failures + outcome.errors:
                print(failure[1], file=sys.stderr)
            print(f"alpha release self-test: {summary['passed']}/{summary['total']} "
                  f"{'PASS' if summary['success'] else 'FAIL'}")
        return 0 if outcome.wasSuccessful() else 1

    binary = Path(args.server_binary).resolve()
    passed = []
    failure = None
    try:
        require(binary.is_file(), f"missing-server-binary:{binary}")
        with tempfile.TemporaryDirectory(prefix="megu-alpha-") as root:
            exercise(binary, Path(root), passed)
    except (AssertionError, OSError, TimeoutError, ValueError) as exc:
        failure = f"{type(exc).__name__}: {exc}"
    summary = {"mode": "live", "protocol": 767, "passed": len(passed), "total": 7,
               "criteria": [{"step": idx, "name": name,
                             "status": "PASS" if name in passed else "FAIL" if idx == len(passed) + 1 else "NOT RUN"}
                            for idx, name in enumerate(CRITERIA, 1)],
               "success": failure is None and len(passed) == 7}
    if failure:
        summary["error"] = failure
    if args.json:
        print(json.dumps(summary, sort_keys=True))
    else:
        for row in summary["criteria"]:
            print(f"[{row['step']}/7] {row['status']}: {row['name']}")
        if failure:
            print(f"alpha release smoke: {failure}", file=sys.stderr)
        print(f"alpha release smoke: {len(passed)}/7 "
              f"{'PASS' if summary['success'] else 'FAIL'}")
    return 0 if summary["success"] else 1


if __name__ == "__main__":
    sys.exit(main())
