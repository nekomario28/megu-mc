#!/usr/bin/env python3
"""Live protocol-767 Nether portal transit using the real megu server.

Builds a 4x5 obsidian frame, ignites it with `/setblock fire` (the same
`BaseFireBlock.onPlace` path flint & steel and spread take), then exercises
the survival delay and both dimension transitions.

Usage: python3 tests/portal_transit_wire.py target/release/megu
"""
import json
import math
import os
from pathlib import Path
import socket
import sys
import tempfile
import time

from dimension_wire import await_respawn, block_of, chunk_of, command, square
from protocol767 import Client, ContractFailure, local_server, require

os.environ["MEGU_ALLOW_PLAYER_ADMIN_COMMANDS"] = "1"
os.environ.setdefault("MC_PROBE_DEADLINE", "30")


def wait_for_update(client, pos, timeout=5.0, since=0):
    # `since` slices off history: earlier edits at `pos` (the hollow fill's air,
    # the ignition fire) would otherwise satisfy `any` before the new update
    # is read and return a stale state.
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for updated, state in reversed(client.updates[since:]):
            if updated == pos:
                return state
        try:
            client.read(min(0.2, deadline - time.monotonic()))
        except (socket.timeout, TimeoutError):
            pass
    raise ContractFailure("block-update-timeout")


def wait_for_state(client, pos, states, timeout=5.0, since=0):
    # Straggler updates from earlier writes can land after `since`; wait for
    # the target state itself rather than the first new update at `pos`.
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for updated, state in client.updates[since:]:
            if updated == pos and state in states:
                return state
        try:
            client.read(min(0.2, deadline - time.monotonic()))
        except (socket.timeout, TimeoutError):
            pass
    raise ContractFailure("block-state-timeout")


def exercise(binary, root):
    summary = {"protocol": 767, "result": "FAIL", "steps": []}
    try:
        with local_server(binary, root) as game:
            client = Client(game, "PortalTransit")
            with client:
                client.login()
                if client.pos is None:
                    client.until_id(0x40)
                require(client.dimension_name == "minecraft:overworld", "login-overworld")
                spawn = client.pos
                require(spawn is not None, "login-position")
                client.until_chunks(square(*chunk_of(spawn)))
                summary["steps"].append({"stage": "login", "status": "passed"})

                # A 4x5 frame has a 2x3 opening. Its plane faces along X at fixed Z.
                sx, sy, sz = block_of(spawn)
                x, y, z = sx + 3, sy, sz + 2
                command(client, "gamemode survival")
                # A 1-thick box is all boundary under vanilla `hollow`, so the
                # ring is built as a solid fill plus an interior air fill.
                command(client, f"fill {x} {y} {z} {x + 3} {y + 4} {z} "
                               "minecraft:obsidian")
                command(client, f"fill {x + 1} {y + 1} {z} {x + 2} {y + 3} {z} "
                               "minecraft:air")
                summary["frame"] = {"status": "passed", "from": [x, y, z],
                                    "to": [x + 3, y + 4, z], "opening": [x + 1, y + 1, z]}

                interior = (x + 1, y + 1, z)
                fire_since = len(client.updates)
                command(client, f"setblock {interior[0]} {interior[1]} {interior[2]} "
                                "minecraft:fire")
                # The fire write ignites the frame: every interior cell becomes
                # a portal block whose axis matches the frame's X span (5864).
                first_state = wait_for_state(client, interior, (5864, 5865), since=fire_since)
                summary["frame_ignition"] = {"status": "passed", "block_state": first_state,
                                             "axis": "x" if first_state == 5864 else "z"}
                summary["steps"].append({"stage": "portal-trigger-placed", "status": "passed",
                                         "block_state": first_state})

                # Survival default is 80 game ticks (4 seconds); the transition is
                # initiated only after the player stands in the portal for that delay.
                transit_pos = (interior[0] + 0.5, float(interior[1]), interior[2] + 0.5)
                client.move(*transit_pos)
                time.sleep(4.25)
                client.move(*transit_pos)
                await_respawn(client, "minecraft:the_nether",
                              (interior[0] / 8, interior[1], interior[2] / 8))
                require(client.section_count == 16 and not client.has_skylight,
                        "nether-dimension-window")
                summary["steps"].append({"stage": "overworld-to-nether", "status": "passed",
                                         "packet": "0x47", "dimension": client.dimension_name,
                                         "section_count": client.section_count})

                # Leave the materialized arrival portal to clear the per-player
                # bounce guard, then re-enter and wait the same survival delay.
                arrival = client.pos
                require(arrival is not None, "nether-position-sync")
                bx, by, bz = block_of(arrival)
                outside = (arrival[0] + 2.0, arrival[1], arrival[2])
                client.move(*outside)
                time.sleep(0.25)
                client.move(*arrival)
                time.sleep(4.25)
                client.move(*arrival)
                await_respawn(client, "minecraft:overworld", spawn)
                require(client.section_count == 24 and client.has_skylight,
                        "overworld-dimension-window")
                summary["steps"].append({"stage": "nether-to-overworld", "status": "passed",
                                         "packet": "0x47", "dimension": client.dimension_name,
                                         "section_count": client.section_count,
                                         "arrival_portal_block": [bx, by, bz]})
        summary["result"] = "PASS"
    except Exception as exc:
        summary["error"] = getattr(exc, "code", str(exc))
        summary["result"] = "FAIL"
    print(json.dumps(summary, sort_keys=True))
    return 0 if summary["result"] == "PASS" else 1


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(json.dumps({"protocol": 767, "result": "FAIL",
                          "error": "usage: portal_transit_wire.py <path-to-megu>"}, sort_keys=True))
        raise SystemExit(2)
    with tempfile.TemporaryDirectory(prefix="spine-portal-transit-") as world:
        raise SystemExit(exercise(Path(sys.argv[1]).resolve(), Path(world)))
