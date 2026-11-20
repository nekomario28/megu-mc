#!/usr/bin/env python3
"""Headless 1.21.1 client movement prediction checked against real server packets.

The client predicts dry movement and sinking in still water from decoded 0x27
chunk palettes and block updates. A second protocol-767 client independently
decodes the world and receives the server's authoritative 0x70 movement broadcasts.
"""
import argparse
import copy
import json
import math
import os
from pathlib import Path
import socket
import struct
import subprocess
import tempfile
import time

from protocol767 import (AIR_STATES, Client, ContractFailure, Reader, ROOT,
                         WIRE_BLOCKS, decode_chunk, frame, local_server, parse_respawn,
                         position, require, vint)


os.environ["MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS"] = "1"
os.environ.setdefault("MC_PROBE_DEADLINE", "40")

STONE_STATE = 1
STONE_ITEM = 1
WATER_BLOCK = next(block for block in WIRE_BLOCKS if block["name"] == "water")
WATER_SOURCE = WATER_BLOCK["defaultState"]
WATER_STATES = range(WATER_BLOCK["minStateId"], WATER_BLOCK["maxStateId"] + 1)
PLAYER_HALF_WIDTH = 0.3
PLAYER_HEIGHT = 1.8
# LivingEntity.travel widens 0.8F to double before applying vertical drag.
WATER_VERTICAL_DRAG = 0.800000011920929
OVERWORLD = ("minecraft:overworld", -64, 384, 24, True)
NETHER = ("minecraft:the_nether", 0, 256, 16, False)
FULL_BLOCK_STATES = {
    state
    for block in WIRE_BLOCKS if block["boundingBox"] == "block"
    for state in range(block["minStateId"], block["maxStateId"] + 1)
}


class PredictionClient(Client):
    def __init__(self, game, name):
        super().__init__(game, name)
        self.entity_id = None
        self.position_syncs = []
        self.tracked = set()
        self.teleports = []

    def read(self, timeout):
        packet, body = super().read(timeout)
        if self.phase == "play":
            if packet == 0x2b:
                self.entity_id = struct.unpack(">i", body[:4])[0]
            elif packet == 0x40:
                self.position_syncs.append(self.pos)
            elif packet == 0x47:
                self.tracked.clear()
            elif packet == 0x01:
                self.tracked.add(Reader(body).varint())
            elif packet == 0x42:
                reader = Reader(body)
                for _ in range(reader.varint()):
                    self.tracked.discard(reader.varint())
            elif packet == 0x70:
                reader = Reader(body)
                eid = reader.varint()
                coords = tuple(reader.number(">d") for _ in range(3))
                reader.take(2)  # encoded yaw and pitch
                grounded = bool(reader.number("B"))
                require(reader.pos == len(body), "entity-teleport-complete")
                self.teleports.append((eid, coords, grounded))
        return packet, body


def chunk_window(center):
    cx, cz = math.floor(center[0] / 16), math.floor(center[2] / 16)
    return {(cx + dx, cz + dz) for dx in range(-2, 3) for dz in range(-2, 3)}


def require_layout(client, layout):
    require((client.dimension_name, client.min_y, client.height, client.section_count,
             client.has_skylight) == layout, f"section-layout:{layout[0]}")


def inspect_chunks(client, layout=OVERWORLD):
    require_layout(client, layout)
    _, _, _, count, skylight = layout
    if client.pos is None:
        client.until(lambda *_: client.pos is not None, "initial-position-sync")
    client.until_chunks(chunk_window(client.pos), "complete-25-chunk-window")
    # Independently re-decode raw 0x27 packets with protocol767's 1.21.1
    # PalettedContainer implementation (64-bit words, no cross-word entries).
    for coord, raw in client.raw_chunk.items():
        got_coord, sections = decode_chunk(raw, client.registry_counts["minecraft:worldgen/biome"],
                                           section_count=count, has_skylight=skylight)
        require(got_coord == coord, "paletted-chunk-coordinates")
        require(len(sections) == count and all(len(section) == 4096 for section in sections),
                f"paletted-{count}-by-4096")
        require(sections == client.chunks[coord], "independent-chunk-decode")
    return len(client.chunks)


def aabb_cells(x, y, z, inset):
    for bx in range(math.floor(x - PLAYER_HALF_WIDTH + inset),
                    math.floor(x + PLAYER_HALF_WIDTH - inset) + 1):
        for bz in range(math.floor(z - PLAYER_HALF_WIDTH + inset),
                        math.floor(z + PLAYER_HALF_WIDTH - inset) + 1):
            for by in range(math.floor(y + inset), math.floor(y + PLAYER_HEIGHT - inset) + 1):
                yield bx, by, bz


def is_blocked(client, x, y, z):
    """Axis-aligned player AABB versus full-cube block collision shapes."""
    for cell in aabb_cells(x, y, z, 1e-9):
        state = client.block(*cell)
        require(state in FULL_BLOCK_STATES or state in AIR_STATES or state in WATER_STATES,
                "arena-only-full-cube-collision")
        if state in FULL_BLOCK_STATES:
            return True
    return False


def immersed_cells(world, x, y, z):
    # FlowingFluid.getHeight returns a full block under the same fluid. A
    # source otherwise has height 8/9. The fixture keeps the player in sources.
    cells = set()
    for bx, by, bz in aabb_cells(x, y, z, 0.001):
        if world.block(bx, by, bz) == WATER_SOURCE:
            surface = by + 8 / 9
            if by + 1 < world.max_y and world.block(bx, by + 1, bz) in WATER_STATES:
                surface = by + 1
            if surface >= y + 0.001:
                cells.add((bx, by, bz))
    return cells


class Predictor:
    """Discrete client tick with vanilla 1.21.1 gravity and block collision."""
    def __init__(self, world, x, y, z):
        self.world = world
        self.x, self.y, self.z = x, y, z
        self.vertical_speed = 0.0
        self.grounded = False
        self.wall_hits = 0

    def step(self, east=0.0):
        immersed = immersed_cells(self.world, self.x, self.y, self.z)
        velocity = self.vertical_speed if immersed else (self.vertical_speed - 0.08) * 0.98
        next_y = self.y + velocity
        self.grounded = False
        if velocity < 0:
            # A downward sweep can cross multiple blocks in one tick. The
            # highest intersected top face is the first collision.
            for bx in range(math.floor(self.x - PLAYER_HALF_WIDTH + 1e-9),
                            math.floor(self.x + PLAYER_HALF_WIDTH - 1e-9) + 1):
                for bz in range(math.floor(self.z - PLAYER_HALF_WIDTH + 1e-9),
                                math.floor(self.z + PLAYER_HALF_WIDTH - 1e-9) + 1):
                    for by in range(math.floor(next_y), math.floor(self.y)):
                        top = by + 1.0
                        if next_y <= top <= self.y and self.world.block(bx, by, bz) in FULL_BLOCK_STATES:
                            next_y = max(next_y, top)
                            self.grounded = True
        self.y = next_y
        self.vertical_speed = 0.0 if self.grounded else velocity
        if immersed:
            # Still water, no swimming input or effects. Movement uses the
            # incoming velocity, then applies drag and fluid gravity for next tick.
            self.vertical_speed = self.vertical_speed * WATER_VERTICAL_DRAG - 0.08 / 16
        if east:
            candidate = self.x + east
            if is_blocked(self.world, candidate, self.y, self.z):
                self.wall_hits += 1
            else:
                self.x = candidate
        require(not is_blocked(self.world, self.x, self.y, self.z), "predicted-player-intersection")
        return (self.x, self.y, self.z), self.grounded


def chat_command(client, command):
    client.chat("/" + command)
    time.sleep(0.18)  # Avoid the server's chat spam throttle.


def all_states(client, cells, expected):
    return all(client.block(*cell) == expected for cell in cells)


def wait_states(client, cells, expected, label):
    client.until(lambda *_: all_states(client, cells, expected), label)
    require(all_states(client, cells, expected), label)


def drain(client, duration=0.20):
    deadline = time.monotonic() + duration
    while time.monotonic() < deadline:
        try:
            client.read(min(0.05, deadline - time.monotonic()))
        except (socket.timeout, TimeoutError):
            continue


def await_authority(observer, entity_id, expected, grounded):
    def moved(packet, body):
        return packet == 0x70 and observer.teleports[-1][0] == entity_id

    observer.until(moved, "server-entity-teleport")
    _, authoritative, server_ground = observer.teleports[-1]
    require(authoritative == expected, "server-position-drift")
    require(server_ground == grounded, "server-ground-flag-drift")


def verify_prediction(actor, observer, actor_predictor, observer_predictor, east=0.0):
    predicted, grounded = actor_predictor.step(east)
    independent, independent_ground = observer_predictor.step(east)
    require(predicted == independent and grounded == independent_ground,
            "observer-collision-drift")
    actor.send(0x1a, struct.pack(">ddd?", *predicted, grounded))
    await_authority(observer, actor.entity_id, predicted, grounded)
    return grounded


def set_creative_stone(client):
    # Inventory slot 36 is the selected hotbar slot. Components count is zero.
    client.send(0x32, struct.pack(">h", 36) + vint(1) + vint(STONE_ITEM) + vint(0) + vint(0))

    def acknowledged(packet, body):
        if packet != 0x15:
            return False
        reader = Reader(body)
        window = reader.number("B")
        reader.varint()  # state id
        slot = reader.number(">h")
        count = reader.varint()
        item = reader.varint() if count else 0
        if count:
            require(reader.varint() == 0 and reader.varint() == 0, "stone-components-empty")
        return window == 0 and slot == 36 and item == STONE_ITEM and count == 1

    client.until(acknowledged, "creative-stone-slot-ack")


def decode_single_water_update(actor, cell):
    # mc-spine batches water edits into 0x49. Exercise the 1.21.1 single
    # update decoder with a separate socket over a copy of the live chunk.
    x, _, z = cell
    coord = (x // 16, z // 16)
    probe = copy.copy(actor)
    probe.chunks = {coord: [list(section) for section in actor.chunks[coord]]}
    probe.updates = []
    require(probe.block(*cell) == STONE_STATE, "single-update-before-water")
    sender, receiver = socket.socketpair()
    with sender, receiver:
        probe.socket = receiver
        sender.sendall(frame(0x09, position(*cell) + vint(WATER_SOURCE)))
        require(Client.read(probe, 1)[0] == 0x09, "single-water-update-packet")
    require(probe.updates == [(cell, WATER_SOURCE)], "single-water-update-decode")
    return probe.block(*cell)


def exercise_water(game, actor, x, y, z):
    # A sealed tank of sources keeps the flow vector zero around the player.
    chat_command(actor, f"fill {x - 2} {y - 1} {z - 2} {x + 2} {y + 3} {z + 2} minecraft:stone")
    cells = [(x, py, z) for py in range(y, y + 4)]
    wait_states(actor, cells, STONE_STATE, "tank-stone-before-water")
    single_update = decode_single_water_update(actor, cells[0])

    chat_command(actor, f"fill {x - 1} {y} {z - 1} {x + 1} {y + 3} {z + 1} minecraft:water")
    actor.until(lambda pid, _: pid == 0x49 and all_states(actor, cells, WATER_SOURCE),
                "actor-water-section-update")
    require(actor.block(*cells[0]) == single_update, "single-and-section-water-decode-agree")

    with PredictionClient(game, "WaterObserver") as observer:
        observer.login(view=2)
        inspect_chunks(observer)
        require(all_states(observer, cells, WATER_SOURCE), "observer-water-0x27-palette")
        start = (x + 0.5, y + 1.5, z + 0.5)
        observer.move(*start)
        actor.send(0x1a, struct.pack(">ddd?", *start, False))
        observer.until(lambda *_: actor.entity_id in observer.tracked, "water-observer-tracks-actor")
        drain(observer)
        actor_prediction = Predictor(actor, *start)
        observer_prediction = Predictor(observer, *start)
        ticks = 8
        min_immersed = len(cells)
        # Initial still-water displacements for zero starting velocity.
        expected_y = (start[1], start[1] - 0.005, start[1] - 0.005 - 0.009000000059604645)
        for tick in range(ticks):
            actor_cells = immersed_cells(actor, actor_prediction.x, actor_prediction.y, actor_prediction.z)
            observer_cells = immersed_cells(observer, observer_prediction.x, observer_prediction.y,
                                             observer_prediction.z)
            require(actor_cells == observer_cells and len(actor_cells) >= 2, "multi-block-water-immersion")
            min_immersed = min(min_immersed, len(actor_cells))
            require(not verify_prediction(actor, observer, actor_prediction, observer_prediction),
                    "submerged-player-off-floor")
            if tick < len(expected_y):
                require(actor_prediction.y == expected_y[tick], "still-water-vertical-step")
        require(actor_prediction.y < start[1], "still-water-sink")
        drain(actor)
        require(len(actor.position_syncs) == len(observer.position_syncs) == 1,
                "no-water-position-correction")
    return {"ticks": ticks, "min_immersed_cells": min_immersed,
            "live_update": "0x49", "palette": "0x27", "single_update_fixture": "0x09",
            "position_drift": 0}


def exercise_dimensions(actor, observer, x, overworld_floor, z):
    nether_floor = 160  # Clear air above the Nether roof in the fixture.
    chat_command(actor, f"execute in the_nether run setblock {x} {nether_floor} {z} minecraft:stone")
    dimensions = []
    for wire_id, layout, floor in ((3, NETHER, nether_floor), (0, OVERWORLD, overworld_floor)):
        name, min_y, _, sections, _ = layout
        start = (x + 0.5, floor + 3.0, z + 0.5)
        for client in (actor, observer):
            require(bool(client.chunks), "populated-cache-before-respawn")
            require(client.dimension_name != name, "cross-dimension-respawn")
        chat_command(actor, f"execute in {name} run tp @a {start[0]} {start[1]} {start[2]}")
        for client in (actor, observer):
            respawn = client.until_id(0x47, "prediction-respawn-0x47")
            require(parse_respawn(respawn) == (wire_id, name), "respawn-dimension-type")
            require_layout(client, layout)
            require(not (client.chunks or client.raw_chunk or client.chunk_light), "respawn-clears-chunks")
            client.until_id(0x40, "position-sync-after-respawn")
            require(client.pos == start, "respawn-position-drift")
            inspect_chunks(client, layout)
            require(client.block(x, floor, z) == STONE_STATE, "dimension-floor-from-palette")
            require(all(client.block(x, py, z) in AIR_STATES for py in range(floor + 1, floor + 5)),
                    "dimension-air-above-floor-from-palette")
        observer.until(lambda *_: actor.entity_id in observer.tracked, "respawn-observer-tracks-actor")
        drain(observer)
        actor_prediction = Predictor(actor, *actor.pos)
        observer_prediction = Predictor(observer, *observer.pos)
        for _ in range(12):
            verify_prediction(actor, observer, actor_prediction, observer_prediction)
        require(actor_prediction.grounded and actor_prediction.y == floor + 1.0,
                "landing-on-rebound-dimension-floor")
        drain(actor)
        dimensions.append({"dimension": name, "min_y": min_y, "sections": sections,
                           "chunks_per_client": len(actor.chunks), "decode_errors": 0,
                           "position_drift": 0})
    require(len(actor.position_syncs) == len(observer.position_syncs) == 3,
            "only-expected-respawn-position-syncs")
    return dimensions


def exercise(game, gravity_ticks, horizontal_ticks):
    with PredictionClient(game, "ClientPredictor") as actor, \
            PredictionClient(game, "ClientObserver") as observer:
        actor.login(view=2)
        require(actor.entity_id is not None, "actor-join-0x2b")
        actor_chunks = inspect_chunks(actor)
        observer.login(view=2)
        require(observer.entity_id is not None, "observer-join-0x2b")
        observer_chunks = inspect_chunks(observer)

        spawn_x, _, spawn_z = actor.pos
        x, z = math.floor(spawn_x), math.floor(spawn_z)
        floor_y = 240
        # The decoded vanilla world must have an empty test volume. Abort if
        # generated terrain or another persistent world occupies these blocks.
        air_cells = [(px, py, pz)
                     for px in range(x - 1, x + 5)
                     for pz in range(z - 1, z + 2)
                     for py in range(floor_y, floor_y + 16)]
        require(all_states(actor, air_cells, 0) and all_states(observer, air_cells, 0),
                "arena-air-from-paletted-chunks")

        start = (x + 0.5, floor_y + 14.0, z + 0.5)
        observer.move(x + 0.5, floor_y + 14.0, z + 0.5)
        actor.send(0x1a, struct.pack(">ddd?", *start, False))
        observer.until(lambda *_: actor.entity_id in observer.tracked,
                       "observer-tracks-actor")
        drain(observer)
        require(actor.entity_id in observer.tracked, "observer-remains-in-range")

        floor = [(px, floor_y, pz)
                 for px in range(x - 1, x + 5) for pz in range(z - 1, z + 2)]
        chat_command(actor, f"fill {x - 1} {floor_y} {z - 1} "
                            f"{x + 4} {floor_y} {z + 1} minecraft:stone")
        wait_states(actor, floor, STONE_STATE, "actor-platform-block-updates")
        wait_states(observer, floor, STONE_STATE, "observer-platform-block-updates")

        actor_prediction = Predictor(actor, *start)
        observer_prediction = Predictor(observer, *start)
        landed_at = None
        for tick in range(1, gravity_ticks + 1):
            if verify_prediction(actor, observer, actor_prediction, observer_prediction) and landed_at is None:
                landed_at = tick
        require(landed_at is not None and landed_at < gravity_ticks,
                "vanilla-gravity-lands-on-palette-platform")
        require(actor_prediction.y == floor_y + 1.0, "floor-top-collision")
        require(actor_prediction.vertical_speed == 0.0, "standing-vertical-speed-zero")

        set_creative_stone(actor)
        wall = (x + 2, floor_y + 1, z)
        actor.send(0x38, vint(0) + position(x + 2, floor_y, z) + vint(1)
                   + struct.pack(">fff?", 0.5, 1.0, 0.5, False) + vint(81))
        wait_states(actor, [wall], STONE_STATE, "actor-stone-place-0x09")
        wait_states(observer, [wall], STONE_STATE, "observer-stone-place-0x09")

        for _ in range(horizontal_ticks):
            verify_prediction(actor, observer, actor_prediction, observer_prediction, east=0.24)
        require(actor_prediction.wall_hits > 0, "wall-collision-registered")
        require(actor_prediction.x + PLAYER_HALF_WIDTH <= wall[0] + 1e-9,
                "player-stops-before-wall")

        actor.send(0x24, vint(0) + position(*wall) + bytes([1]) + vint(82))
        wait_states(actor, [wall], 0, "actor-stone-break-0x09")
        wait_states(observer, [wall], 0, "observer-stone-break-0x09")
        previous_x = actor_prediction.x
        for _ in range(horizontal_ticks):
            verify_prediction(actor, observer, actor_prediction, observer_prediction, east=0.24)
        require(actor_prediction.x > previous_x, "movement-resumes-after-break")
        require(actor_prediction.x > wall[0] + PLAYER_HALF_WIDTH,
                "player-crosses-removed-wall")
        require(actor_prediction.grounded, "standing-after-break")
        require(len(actor.position_syncs) == 1, "no-actor-correction-0x40")
        require(len(observer.position_syncs) == 1, "no-observer-correction-0x40")

        authoritative_movements = sum(entry[0] == actor.entity_id for entry in observer.teleports)
        water = exercise_water(game, actor, x + 8, floor_y + 12, z)
        dimensions = exercise_dimensions(actor, observer, x, floor_y, z)

        return {
            "protocol": 767,
            "chunk_packet": "0x27",
            "join_packet": "0x2b",
            "spawn_packet": "0x40",
            "chunks": {"actor": actor_chunks, "observer": observer_chunks},
            "sections_per_chunk": actor.section_count,
            "blocks_per_section": 4096,
            "gravity_ticks": gravity_ticks,
            "landing_tick": landed_at,
            "horizontal_ticks": horizontal_ticks * 2,
            "wall_collisions": actor_prediction.wall_hits,
            "authoritative_movements": authoritative_movements,
            "water": water,
            "respawn_packet": "0x47",
            "dimension_transitions": dimensions,
            "position_drift": 0,
            "collision_drift": 0,
            "block_state_drift": 0,
            "unexpected_resyncs": 0,
        }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path,
                        default=Path(os.environ.get("MC_SPINE_BIN", ROOT / "target/debug/mc-spine")))
    parser.add_argument("--build", action="store_true", help="build server with cargo before test")
    parser.add_argument("--world-dir", type=Path, help="optional persistent test world directory")
    parser.add_argument("--gravity-ticks", type=int, default=36)
    parser.add_argument("--horizontal-ticks", type=int, default=8)
    args = parser.parse_args()
    if args.gravity_ticks < 3 or args.horizontal_ticks < 8:
        parser.error("--gravity-ticks must be >=3 and --horizontal-ticks must be >=8")
    if args.build or (not args.binary.is_file() and args.binary == ROOT / "target/debug/mc-spine"):
        subprocess.run(["cargo", "build", "--offline", "--locked"], cwd=ROOT, check=True)
    binary = args.binary.resolve()
    require(binary.is_file() and os.access(binary, os.X_OK), "server-binary-required")
    if args.world_dir is not None:
        root = args.world_dir.resolve()
        root.mkdir(parents=True, exist_ok=True)
        with local_server(binary, root) as game:
            report = exercise(game, args.gravity_ticks, args.horizontal_ticks)
    else:
        with tempfile.TemporaryDirectory(prefix="spine-prediction-") as world:
            with local_server(binary, Path(world)) as game:
                report = exercise(game, args.gravity_ticks, args.horizontal_ticks)
    print(json.dumps(report, sort_keys=True))
    print("client prediction wire: PASS")


if __name__ == "__main__":
    try:
        main()
    except (ContractFailure, AssertionError) as error:
        raise SystemExit(f"client prediction wire: FAIL ({error})") from error
