#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Live Protocol 767 block-shape/redstone checks against megu and Vanilla.

The two servers receive the same ordered C2S placement/break/interact trace
and RCON fixture commands. Block-state observations come from the actual 0x27/
0x09/0x49 wire decoder and are independently checked using RCON ``execute if
block``. A failed probe is never silently replaced with an expected value.
"""

import argparse
import hashlib
import json
from pathlib import Path
import re
import socket
import struct
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests"))
sys.path.insert(0, str(ROOT / "tools" / "client_diff"))

from parity_client import (JAR, JAR_SHA1, NAME, RCON_PASS, ParityClient,  # noqa: E402
                           live_server, read_for, until)
from protocol767 import WIRE_BLOCKS, position, vint  # noqa: E402
from wire_smoke import rcon_frame, rcon_response  # noqa: E402

SCHEMA = "block-shape-redstone-oracle/1"
SCENARIO_NAMES = (
    "double_door_auto_hinge",
    "double_chest_join_and_sneak",
    "stairs_inner_outer_corners",
    "slab_double_combine",
    "wall_fence_pane_connections",
    "rail_curve_and_slope",
    "redstone_strong_vs_weak_power",
    "redstone_slab_one_way_diode",
    "redstone_repeater_delay_and_lock",
    "redstone_comparator_container_and_subtract",
    "redstone_piston_qc_and_observer_copper_bulb",
)

BLOCKS = {block["name"]: block for block in WIRE_BLOCKS}
# Some blocks drop a different item (stone), or no item (glass pane).
# These are their pinned 1.21.1 *placeable* item IDs.
PLACEABLE_ITEMS = {"stone": 1, "glass_pane": 357}
STATE_BLOCK = {
    state: block for block in WIRE_BLOCKS
    for state in range(block["minStateId"], block["maxStateId"] + 1)
}
ORIGIN_Y = 240


def decode_state(state_id):
    """Decode the pinned 1.21.1 block-state ordering, including all properties."""
    block = STATE_BLOCK.get(state_id)
    if block is None:
        return {"state_id": state_id, "block": "UNKNOWN", "properties": {}}
    offset = state_id - block["minStateId"]
    properties = {}
    for prop in reversed(block["states"]):
        offset, value_index = divmod(offset, prop["num_values"])
        values = prop.get("values", ("true", "false"))
        properties[prop["name"]] = values[value_index]
    return {"state_id": state_id, "block": block["name"],
            "properties": dict(sorted(properties.items()))}


class Rcon:
    """Persistent, strictly matched request IDs using the existing wire codec."""

    def __init__(self, port):
        deadline = time.monotonic() + 15
        while True:
            try:
                self.sock = socket.create_connection(("127.0.0.1", port), timeout=2)
                break
            except OSError:
                if time.monotonic() >= deadline:
                    raise
                time.sleep(0.1)
        self.sock.settimeout(10)
        self.ident = 100
        self.sock.sendall(rcon_frame(self.ident, 3, RCON_PASS))
        reply = rcon_response(self.sock)
        if struct.unpack("<ii", reply[:8]) != (self.ident, 2):
            raise RuntimeError("RCON authentication failed")

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.sock.close()

    def command(self, text):
        self.ident += 1
        self.sock.sendall(rcon_frame(self.ident, 2, text))
        reply = rcon_response(self.sock)
        if struct.unpack("<ii", reply[:8]) != (self.ident, 0) or reply[-2:] != b"\0\0":
            raise RuntimeError(f"invalid RCON response for {text!r}")
        return reply[8:-2].decode("utf-8", "replace")

    def required(self, text):
        reply = self.command(text)
        if (not reply or re.search(r"unknown|incorrect|expected|error|failed|cannot|unrecognized",
                                   reply, re.IGNORECASE)):
            raise RuntimeError(f"fixture command {text!r} rejected: {reply!r}")
        return reply

    def matches(self, pos, block, props):
        x, y, z = pos
        selector = f"minecraft:{block}"
        if props:
            selector += "[" + ",".join(f"{key}={value}" for key, value in props.items()) + "]"
        reply = self.command(f"execute if block {x} {y} {z} {selector}")
        if reply == "Test passed":
            return True
        if reply == "Test failed":
            return False
        raise RuntimeError(f"unrecognized RCON block probe reply {reply!r}")


class Scenario:
    def __init__(self, client, rcon):
        self.client = client
        self.rcon = rcon
        self.sequence = 0
        self.checks = []
        self.log = []

    def setup(self):
        # Reset the same small loaded chunk area before every independent case.
        self.rcon.required("fill 4 240 4 14 246 14 minecraft:air")
        self.rcon.required("fill 4 239 4 14 239 14 minecraft:stone")
        self.teleport(9.5, 240.0, 7.5, 0)

    def teleport(self, x, y, z, yaw=0):
        self.rcon.required(f"tp {NAME} {x} {y} {z} {yaw} 0")
        read_for(self.client, 0.13)

    def yaw(self, yaw):
        self.client.send(0x1c, struct.pack(">ff?", float(yaw), 0.0, True))
        read_for(self.client, 0.08)

    def item(self, name):
        drops = BLOCKS[name]["drops"]
        item_id = PLACEABLE_ITEMS.get(name, drops[0] if drops else None)
        if item_id is None:
            raise ValueError(f"no placeable item ID in pinned registry: {name}")
        if getattr(self.client, "_shape_selected_item", None) == item_id:
            return
        self.client.send(0x32, struct.pack(">h", 36) + vint(1)
                         + vint(item_id) + vint(0) + vint(0))
        read_for(self.client, 0.15)
        self.client._shape_selected_item = item_id

    def use(self, pos, face=1, hit=(0.5, 1.0, 0.5)):
        self.sequence += 1
        self.client.send(0x38, vint(0) + position(*pos) + vint(face)
                         + struct.pack(">fff?", *hit, False) + vint(self.sequence))
        read_for(self.client, 0.15)

    def place(self, name, x, z, y=ORIGIN_Y, yaw=None):
        if yaw is not None:
            self.yaw(yaw)
        self.item(name)
        self.use((x, y - 1, z))
        return (x, y, z)

    def break_block(self, pos):
        self.sequence += 1
        self.client.send(0x24, vint(0) + position(*pos) + b"\x01" + vint(self.sequence))
        read_for(self.client, 0.20)

    def sneak(self, enabled):
        if self.client.entity_id is None:
            raise RuntimeError("missing player entity id for sneak packet")
        self.client.send(0x25, vint(self.client.entity_id)
                         + vint(0 if enabled else 1) + vint(0))
        read_for(self.client, 0.12)

    def command(self, text):
        reply = self.rcon.required(text)
        read_for(self.client, 0.11)
        return reply

    def state(self, pos):
        try:
            state_id = self.client.block(*pos)
        except (AssertionError, KeyError) as error:
            return {"block": "UNKNOWN", "properties": {}, "state_id": None,
                    "rcon_confirmed": False, "error": str(error)}
        state = decode_state(state_id)
        state["rcon_confirmed"] = (state["block"] != "UNKNOWN" and
                                   self.rcon.matches(pos, state["block"], state["properties"]))
        return state

    def check(self, label, pos, name, **expected):
        read_for(self.client, 0.08)
        state = self.state(pos)
        good = (state["rcon_confirmed"] and state["block"] == name and
                all(state["properties"].get(key) == str(value)
                    for key, value in expected.items()))
        self.checks.append({"label": label, "expected": {"block": name, "properties": expected},
                            "actual": state, "pass": good})
        return state

    def assert_value(self, label, actual, expected, passed):
        self.checks.append({"label": label, "expected": expected,
                            "actual": actual, "pass": bool(passed)})

    def assert_power(self, label, pos, positive):
        state = self.check(label, pos, "redstone_wire")
        power = state["properties"].get("power")
        value = power is not None and power.isdecimal() and int(power) > 0
        self.assert_value(label + ".power", power, ">0" if positive else "0",
                          value if positive else power == "0")

    def update_trace(self, pos, since):
        # Record transitions seen on the actual gameplay packet stream.
        return [decode_state(state) for point, state in self.client.updates[since:]
                if point == pos]


def double_door_auto_hinge(s):
    # For a south-facing pair, install the eastern half first so the second
    # door selects the mirrored hinge from its existing neighbor.
    b, a = s.place("oak_door", 10, 10), s.place("oak_door", 9, 10)
    states = {}
    for label, pos in (("a_lower", a), ("a_upper", (9, 241, 10)),
                       ("b_lower", b), ("b_upper", (10, 241, 10))):
        states[label] = s.check(label, pos, "oak_door", half="upper" if "upper" in label else "lower")
    hinges = [states[k]["properties"].get("hinge") for k in ("a_lower", "b_lower")]
    s.assert_value("opposite_hinges", hinges, ["left", "right"], set(hinges) == {"left", "right"})
    for lower, upper in (("a_lower", "a_upper"), ("b_lower", "b_upper")):
        pair = [states[k]["properties"].get("hinge") for k in (lower, upper)]
        s.assert_value(lower + ".matching_halves", pair, "same hinge", pair[0] in ("left", "right") and pair[0] == pair[1])
    s.break_block(a)
    s.check("broken_lower", a, "air")
    s.check("broken_upper", (9, 241, 10), "air")


def double_chest_join_and_sneak(s):
    left = s.place("chest", 9, 10)
    right = s.place("chest", 10, 10)
    a = s.check("joined_a", left, "chest", facing="north")
    b = s.check("joined_b", right, "chest", facing="north")
    types = [a["properties"].get("type"), b["properties"].get("type")]
    s.assert_value("opposite_chest_types", types, ["left", "right"], set(types) == {"left", "right"})
    lone = s.place("chest", 7, 12)
    s.sneak(True)
    try:
        neighbor = s.place("chest", 8, 12)
    finally:
        s.sneak(False)
    s.check("sneak_single_a", lone, "chest", type="single")
    s.check("sneak_single_b", neighbor, "chest", type="single")
    s.break_block(left)
    s.check("broken_chest", left, "air")
    s.check("surviving_single", right, "chest", type="single")


def stairs_inner_outer_corners(s):
    # South-facing centers with each east/west-facing neighbor in front/behind.
    cases = ((7, 8, 1, 90), (11, 8, 1, 270),
             (7, 12, -1, 90), (11, 12, -1, 270))
    shapes = []
    for index, (x, z, neighbor_offset, neighbor_yaw) in enumerate(cases):
        s.teleport(x + 0.5, 240, z - 2.0)
        center = s.place("oak_stairs", x, z, yaw=0)
        neighbor = s.place("oak_stairs", x, z + neighbor_offset, yaw=neighbor_yaw)
        result = s.check(f"corner_{index}", center, "oak_stairs")
        s.check(f"corner_{index}_neighbor", neighbor, "oak_stairs")
        shapes.append(result["properties"].get("shape"))
    s.assert_value("all_four_corner_shapes", shapes,
                   ["inner_left", "inner_right", "outer_left", "outer_right"],
                   set(shapes) == {"inner_left", "inner_right", "outer_left", "outer_right"})
    # Place the top-half stair against the side of an existing block.
    s.command("setblock 13 240 5 minecraft:stone")
    s.teleport(12.5, 240.0, 6.5, 0)
    s.item("oak_stairs")
    s.use((13, 240, 5), face=5, hit=(1.0, 0.9, 0.5))
    s.check("upper_half_click", (14, 240, 5), "oak_stairs", half="top")


def slab_double_combine(s):
    slab = s.place("stone_slab", 9, 10)
    s.check("bottom_slab", slab, "stone_slab", type="bottom")
    s.item("stone_slab")
    s.use(slab, face=1)
    s.check("combined_double", slab, "stone_slab", type="double")


def wall_fence_pane_connections(s):
    wall = [s.place("cobblestone_wall", x, 10) for x in (8, 9, 10)]
    s.place("cobblestone_wall", 9, 9)
    s.place("cobblestone_wall", 9, 11)
    s.place("cobblestone_wall", 9, 10, y=241)
    middle = s.check("wall_middle", wall[1], "cobblestone_wall")
    cardinal = [middle["properties"].get(k) for k in ("north", "south", "east", "west")]
    s.assert_value("wall_connects_all_sides", cardinal,
                   "low or tall on each side", all(value in ("low", "tall") for value in cardinal))
    s.assert_value("wall_up_boolean", middle["properties"].get("up"),
                   "true or false", middle["properties"].get("up") in ("true", "false"))
    s.check("wall_above", (9, 241, 10), "cobblestone_wall")
    s.setup()
    s.command("setblock 7 240 12 minecraft:stone")
    a, b = s.place("oak_fence", 8, 12), s.place("oak_fence", 9, 12)
    s.check("fence_solid_and_fence", a, "oak_fence", east="true", west="true")
    s.check("fence_pair", b, "oak_fence", west="true")
    s.command("setblock 13 240 12 minecraft:stone")
    a, b = s.place("glass_pane", 11, 12), s.place("glass_pane", 12, 12)
    s.check("pane_pair", a, "glass_pane", east="true")
    s.check("pane_solid_and_pane", b, "glass_pane", east="true", west="true")


def rail_curve_and_slope(s):
    for x, z in ((8, 10), (8, 11), (9, 11)):
        s.place("rail", x, z)
    corner = s.check("l_corner", (8, 240, 11), "rail")
    s.assert_value("rail_turned", corner["properties"].get("shape"),
                   "south_east|north_west|south_west|north_east",
                   corner["properties"].get("shape") in
                   ("south_east", "north_west", "south_west", "north_east"))
    s.place("rail", 11, 11)
    s.command("setblock 12 240 11 minecraft:stone")
    s.place("rail", 12, 11, y=241)
    slope = s.check("ascending_rail", (11, 240, 11), "rail")
    s.assert_value("rail_slopes_up", slope["properties"].get("shape"),
                   "ascending_east", slope["properties"].get("shape") == "ascending_east")


def redstone_strong_vs_weak_power(s):
    s.command("setblock 9 240 10 minecraft:stone")
    lever = (9, 241, 10)
    s.item("lever")
    s.use((9, 240, 10))
    s.check("lever_placed", lever, "lever", powered="false")
    s.place("redstone_wire", 10, 10)
    s.place("redstone_lamp", 9, 11)
    s.use(lever)
    read_for(s.client, 0.35)
    s.check("lever_on", lever, "lever", powered="true")
    s.check("strong_wire", (10, 240, 10), "redstone_wire", power="15")
    s.check("strong_lamp", (9, 240, 11), "redstone_lamp", lit="true")

    s.setup()
    s.command("setblock 9 240 10 minecraft:stone")
    s.place("redstone_wire", 8, 10)
    s.place("redstone_wire", 10, 10)
    s.place("redstone_lamp", 9, 11)
    s.place("redstone_block", 7, 10)
    read_for(s.client, 0.35)
    s.assert_power("weak_input", (8, 240, 10), True)
    s.check("weak_lamp", (9, 240, 11), "redstone_lamp", lit="true")
    s.check("weak_other_side", (10, 240, 10), "redstone_wire", power="0")


def redstone_slab_one_way_diode(s):
    lower = s.place("redstone_wire", 8, 10)
    s.command("setblock 9 240 10 minecraft:stone_slab[type=top,waterlogged=false]")
    upper = s.place("redstone_wire", 9, 10, y=241)
    s.place("redstone_block", 7, 10)
    read_for(s.client, 0.30)
    s.assert_power("forward_lower", lower, True)
    s.assert_power("forward_upper", upper, True)
    s.break_block((7, 240, 10))
    s.command("setblock 10 240 10 minecraft:stone")
    s.place("redstone_block", 10, 10, y=241)
    s.check("reverse_source", (10, 241, 10), "redstone_block")
    read_for(s.client, 0.30)
    s.assert_power("reverse_upper", upper, True)
    s.check("reverse_lower", lower, "redstone_wire", power="0")


def redstone_repeater_delay_and_lock(s):
    repeater = s.place("repeater", 9, 10, yaw=270)
    s.check("repeater_initial", repeater, "repeater", delay="1", powered="false")
    for delay in (2, 3, 4):
        s.use(repeater)
        s.check(f"delay_{delay}", repeater, "repeater", delay=str(delay))
    wire = s.place("redstone_wire", 10, 10)
    before = len(s.client.updates)
    s.place("redstone_block", 8, 10)
    s.check("repeater_source", (8, 240, 10), "redstone_block")
    read_for(s.client, 0.55)
    transition = [block["properties"].get("powered")
                  for block in s.update_trace(repeater, before) if block["block"] == "repeater"]
    s.assert_value("delayed_output_edge", transition, "eventually powered=true", "true" in transition)
    s.check("repeater_output", repeater, "repeater", delay="4", powered="true")
    s.assert_power("repeater_output_wire", wire, True)
    # A repeater pointing into its side locks the main repeater while powered.
    s.place("repeater", 9, 9, yaw=0)
    s.place("redstone_block", 9, 8)
    read_for(s.client, 0.50)
    s.check("locked_repeater", repeater, "repeater", locked="true", powered="true")
    s.break_block((8, 240, 10))
    read_for(s.client, 0.50)
    s.check("locked_retains_output", repeater, "repeater", locked="true", powered="true")


def redstone_comparator_container_and_subtract(s):
    chest = s.place("chest", 8, 10)
    s.check("comparator_chest", chest, "chest", type="single")
    # Populate the placed chest through the same RCON item command on both
    # runtimes, then query its actual block-entity contents as independent proof.
    s.command("item replace block 8 240 10 container.0 with minecraft:stone 64")
    contents = s.rcon.command("data get block 8 240 10 Items")
    s.assert_value("container_has_stone", contents, "minecraft:stone and count 64",
                   "minecraft:stone" in contents and bool(re.search(r"(?:count|Count):\s*64", contents)))
    comparator = s.place("comparator", 9, 10, yaw=270)
    wire = s.place("redstone_wire", 10, 10)
    read_for(s.client, 0.50)
    s.check("comparator_compare", comparator, "comparator", mode="compare")
    s.assert_power("container_output", wire, True)
    s.use(comparator)
    s.check("comparator_subtract", comparator, "comparator", mode="subtract")
    s.place("redstone_block", 9, 9)
    read_for(s.client, 0.55)
    s.check("subtract_output", wire, "redstone_wire", power="0")


def redstone_piston_qc_and_observer_copper_bulb(s):
    piston = s.place("piston", 9, 10, yaw=90)
    s.check("piston_before_qc", piston, "piston", extended="false")
    s.command("setblock 9 241 10 minecraft:stone")
    s.place("redstone_block", 9, 10, y=242)
    s.check("qc_power_source_two_blocks_above", (9, 242, 10), "redstone_block")
    s.check("qc_no_direct_update", piston, "piston", extended="false")
    s.place("stone", 8, 10)
    read_for(s.client, 0.35)
    s.check("qc_after_neighbor_update", piston, "piston", extended="true")

    s.setup()
    observer = s.place("observer", 9, 10, yaw=90)
    facing = s.check("observer_faces_watched", observer, "observer", facing="west")
    s.assert_value("observer_orientation", facing["properties"].get("facing"), "west",
                   facing["properties"].get("facing") == "west")
    read_for(s.client, 0.4)  # Consume the observer's placement pulse first.
    before = len(s.client.updates)
    s.place("stone", 8, 10)
    read_for(s.client, 0.55)
    edges = [block["properties"].get("powered")
             for block in s.update_trace(observer, before) if block["block"] == "observer"]
    s.assert_value("observer_two_tick_pulse_edges", edges, ["true", "false"],
                   "true" in edges and "false" in edges and edges.index("true") < edges.index("false"))
    s.check("observer_pulse_ended", observer, "observer", powered="false")

    s.setup()
    bulb = s.place("copper_bulb", 9, 10)
    s.check("bulb_initial", bulb, "copper_bulb", lit="false")
    s.place("redstone_block", 8, 10)
    read_for(s.client, 0.30)
    s.check("bulb_rising_on", bulb, "copper_bulb", lit="true", powered="true")
    s.break_block((8, 240, 10))
    read_for(s.client, 0.30)
    s.check("bulb_falling_unchanged", bulb, "copper_bulb", lit="true", powered="false")
    s.place("redstone_block", 8, 10)
    read_for(s.client, 0.30)
    s.check("bulb_second_rising_off", bulb, "copper_bulb", lit="false", powered="true")


SCENARIOS = {name: globals()[name] for name in SCENARIO_NAMES}


def run_target(kind, binary, jar):
    results = {}
    with tempfile.TemporaryDirectory(prefix=f"block-shape-{kind}-") as directory:
        with live_server(kind, binary, Path(directory), jar) as (game, rcon_port):
            with ParityClient(game) as client, Rcon(rcon_port) as admin:
                client.login()
                until(client, lambda: client.pos is not None and (0, 0) in client.chunks,
                      "oracle login + origin chunk", timeout=45)
                admin.required("gamerule doDaylightCycle false")
                admin.required("difficulty peaceful")
                # Vanilla returns an empty reply when creative is already set.
                mode_reply = admin.command(f"gamemode creative {NAME}")
                if re.search(r"unknown|incorrect|expected|error|failed|cannot|unrecognized",
                             mode_reply, re.IGNORECASE):
                    raise RuntimeError(f"gamemode command rejected: {mode_reply!r}")
                read_for(client, 0.25)
                for name, run_case in SCENARIOS.items():
                    case = Scenario(client, admin)
                    try:
                        case.setup()
                        run_case(case)
                    except Exception as error:
                        case.checks.append({"label": "scenario_error", "expected": "completed",
                                            "actual": f"{type(error).__name__}: {error}", "pass": False})
                    results[name] = {"status": "PASS" if case.checks and all(
                        check["pass"] for check in case.checks) else "MISMATCH",
                        "checks": case.checks}
    return results


def compare(name, spine, vanilla):
    a = spine[name]
    b = vanilla.get(name) if vanilla is not None else None
    differences = []
    if b is not None:
        left = {check["label"]: check for check in a["checks"]}
        right = {check["label"]: check for check in b["checks"]}
        for label in sorted(left.keys() | right.keys()):
            x, y = left.get(label), right.get(label)
            if x is None or y is None or x["actual"] != y["actual"]:
                differences.append({"check": label, "megu": x["actual"] if x else None,
                                    "vanilla": y["actual"] if y else None})
    status = ("PASS" if a["status"] == "PASS" and
              (b is None or b["status"] == "PASS") and not differences else "MISMATCH")
    return {"name": name, "status": status, "megu": a,
            "vanilla": b, "differences": differences}


def report(spine, vanilla, binary, jar):
    scenarios = [compare(name, spine, vanilla) for name in SCENARIO_NAMES]
    return {"schema": SCHEMA, "minecraft": "1.21.1", "protocol": 767,
            "vanilla_jar_sha1": JAR_SHA1,
            "binary": str(binary), "vanilla_jar": str(jar) if vanilla is not None else None,
            "scenarios": scenarios,
            "summary": {"pass": sum(row["status"] == "PASS" for row in scenarios),
                        "mismatch": sum(row["status"] == "MISMATCH" for row in scenarios),
                        "vanilla_executed": vanilla is not None}}


def self_test():
    assert len(SCENARIOS) == len(SCENARIO_NAMES) == 11
    assert set(SCENARIOS) == set(SCENARIO_NAMES)
    for name, case in SCENARIOS.items():
        assert callable(case), name
    good = {"status": "PASS", "checks": [{"label": "shape", "expected": "left",
                                           "actual": {"properties": {"hinge": "left"}}, "pass": True}]}
    bad = {"status": "PASS", "checks": [{"label": "shape", "expected": "left",
                                          "actual": {"properties": {"hinge": "right"}}, "pass": True}]}
    assert compare(SCENARIO_NAMES[0], {SCENARIO_NAMES[0]: good},
                   {SCENARIO_NAMES[0]: bad})["status"] == "MISMATCH"
    assert compare(SCENARIO_NAMES[0], {SCENARIO_NAMES[0]: good},
                   {SCENARIO_NAMES[0]: good})["status"] == "PASS"
    sample = report(dict.fromkeys(SCENARIO_NAMES, good),
                    dict.fromkeys(SCENARIO_NAMES, good), Path("megu"), Path("server.jar"))
    encoded = json.loads(json.dumps(sample, sort_keys=True))
    assert encoded["schema"] == SCHEMA and len(encoded["scenarios"]) == 11
    assert encoded["summary"] == {"pass": 11, "mismatch": 0, "vanilla_executed": True}
    assert all({"name", "status", "megu", "vanilla", "differences"} <= set(row)
               for row in encoded["scenarios"])
    print("PASS: 11 scenarios, mismatch detector, JSON schema")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/megu")
    parser.add_argument("--with-vanilla", action="store_true")
    parser.add_argument("--vanilla-jar", type=Path, default=JAR)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return 0
    binary, jar = args.binary.resolve(), args.vanilla_jar.resolve()
    if not binary.is_file():
        parser.error(f"binary absent: {binary} (build with cargo build -j 1 --locked)")
    if args.with_vanilla:
        if not jar.is_file():
            parser.error(f"vanilla JAR absent: {jar}")
        digest = hashlib.file_digest(jar.open("rb"), "sha1").hexdigest()
        if digest != JAR_SHA1:
            parser.error(f"untrusted Vanilla JAR SHA-1 {digest}, expected {JAR_SHA1}")
    spine = run_target("megu", binary, jar)
    vanilla = run_target("vanilla", binary, jar) if args.with_vanilla else None
    result = report(spine, vanilla, binary, jar)
    if args.json:
        print(json.dumps(result, indent=2, ensure_ascii=False, sort_keys=True))
    else:
        print(f"{'Scenario':48} {'megu':9} {'Vanilla':9} {'Differential':12}")
        for row in result["scenarios"]:
            vanilla_status = row["vanilla"]["status"] if row["vanilla"] else "NOT RUN"
            print(f"{row['name']:48} {row['megu']['status']:9} "
                  f"{vanilla_status:9} {row['status']:12}")
            for server in ("megu", "vanilla"):
                case = row[server]
                if case is not None:
                    failed = [check["label"] for check in case["checks"] if not check["pass"]]
                    if failed:
                        print(f"  {server} failed: {', '.join(failed)}")
            if row["differences"]:
                print("  differing checks: " + ", ".join(d["check"] for d in row["differences"]))
        print("Summary: {pass} PASS, {mismatch} MISMATCH".format(**result["summary"]))
    return 1 if result["summary"]["mismatch"] else 0


if __name__ == "__main__":
    sys.exit(main())
