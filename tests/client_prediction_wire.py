#!/usr/bin/env python3
"""Headless 1.21.1 client movement prediction checked against real server packets.

The client predicts vanilla gravity and full-block collisions from decoded 0x27
chunk palettes. A second protocol-767 client independently decodes the world
and receives the server's authoritative 0x70 movement broadcasts.
"""
import argparse
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
                         WIRE_BLOCKS, decode_chunk, local_server, position, require, vint)


os.environ["MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS"] = "1"
os.environ.setdefault("MC_PROBE_DEADLINE", "40")

STONE_STATE = 1
STONE_ITEM = 1
PLAYER_HALF_WIDTH = 0.3
PLAYER_HEIGHT = 1.8
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


def inspect_chunks(client):
    require(client.dimension_name == "minecraft:overworld", "overworld-dimension")
    require((client.min_y, client.height, client.section_count) == (-64, 384, 24),
            "overworld-section-layout")
    if client.pos is None:
        client.until(lambda *_: client.pos is not None, "initial-position-sync")
    client.until_chunks(chunk_window(client.pos), "complete-25-chunk-window")
    # Independently re-decode raw 0x27 packets with protocol767's 1.21.1
    # PalettedContainer implementation (64-bit words, no cross-word entries).
    for coord, raw in client.raw_chunk.items():
        got_coord, sections = decode_chunk(raw, client.registry_counts["minecraft:worldgen/biome"])
        require(got_coord == coord, "paletted-chunk-coordinates")
        require(len(sections) == 24 and all(len(section) == 4096 for section in sections),
                "paletted-24-by-4096")
        require(sections == client.chunks[coord], "independent-chunk-decode")
    return len(client.chunks)


def is_blocked(client, x, y, z):
    """Axis-aligned player AABB versus full-cube block collision shapes."""
    for bx in range(math.floor(x - PLAYER_HALF_WIDTH + 1e-9),
                    math.floor(x + PLAYER_HALF_WIDTH - 1e-9) + 1):
        for bz in range(math.floor(z - PLAYER_HALF_WIDTH + 1e-9),
                        math.floor(z + PLAYER_HALF_WIDTH - 1e-9) + 1):
            for by in range(math.floor(y + 1e-9), math.floor(y + PLAYER_HEIGHT - 1e-9) + 1):
                state = client.block(bx, by, bz)
                require(state in FULL_BLOCK_STATES or state in AIR_STATES,
                        "arena-only-full-cube-collision")
                if state in FULL_BLOCK_STATES:
                    return True
    return False


class Predictor:
    """Discrete client tick with vanilla 1.21.1 gravity and block collision."""
    def __init__(self, world, x, y, z):
        self.world = world
        self.x, self.y, self.z = x, y, z
        self.vertical_speed = 0.0
        self.grounded = False
        self.wall_hits = 0

    def step(self, east=0.0):
        velocity = (self.vertical_speed - 0.08) * 0.98
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
            "authoritative_movements": len([entry for entry in observer.teleports
                                             if entry[0] == actor.entity_id]),
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
