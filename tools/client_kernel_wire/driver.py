# SPDX-License-Identifier: LGPL-3.0-or-later
"""Protocol 767 chunk and movement bridge for the kernel wire fixture."""
import ctypes as C
import math
import struct
from contextlib import ExitStack

from client_prediction_wire import (
    MOVE_IDS,
    PLAYER_HALF_WIDTH,
    PLAYER_HEIGHT,
    WATER_SOURCE,
    WATER_STATES,
    PredictionClient,
    last_entity_pose,
    wire_position_q,
)
from protocol767 import AIR_STATES, STATE_COUNT, WIRE_BLOCKS, Reader, palette, require

from . import abi

BLOCKS = {block["name"]: block["defaultState"] for block in WIRE_BLOCKS}
STONE = BLOCKS["stone"]
BOTTOM_SLAB = BLOCKS["stone_slab"]


def block_containers(client, coord):
    """Slice the original server bytes using the shared palette decoder."""
    reader = Reader(client.raw_chunk[coord])
    require((reader.number(">i"), reader.number(">i")) == coord, "chunk-coordinates")
    reader.nbt()
    data = Reader(reader.take(reader.varint()))
    biome_count = client.registry_counts["minecraft:worldgen/biome"]
    result = []
    for _ in range(client.section_count):
        non_air = data.number(">h")
        start = data.pos
        palette(data, abi.SECTION_CELLS, 8, 4, STATE_COUNT)
        result.append((non_air, data.data[start:data.pos]))
        palette(data, 64, 3, 1, biome_count)
    require(data.pos == len(data.data), "chunk-section-span")
    return result


class KernelClient(PredictionClient):
    def __init__(self, game, name, kernel):
        super().__init__(game, name)
        self.kernel = kernel
        self.palettes = {}
        self.initial_sections = 0
        self.wire_widths = set()
        self.update_counts = {0x09: 0, 0x49: 0}

    def __exit__(self, *args):
        try:
            with ExitStack() as cleanup:
                for _, resources in self.palettes.values():
                    cleanup.callback(resources.close)
                self.palettes.clear()
        finally:
            super().__exit__(*args)

    def read(self, timeout):
        update_start = len(self.updates)
        packet, body = super().read(timeout)
        if self.phase == "play":
            if packet == 0x27:
                self.load_chunk(struct.unpack_from(">ii", body))
            elif packet == 0x21:
                z, x = struct.unpack(">ii", body)
                _, resources = self.palettes.pop((x, z))
                resources.close()
            elif packet == 0x47:
                require(False, "kernel-wire-fixture-must-stay-in-overworld")
            elif packet in self.update_counts:
                for (x, y, z), state in self.updates[update_start:]:
                    chunk = self.palettes.get((x // 16, z // 16))
                    if chunk is not None:
                        handle, index = self.cell(x, y, z)
                        self.kernel.call("palette_set", handle, index, state)
                        self.update_counts[packet] += 1
        return packet, body

    def load_chunk(self, coord):
        with ExitStack() as resources:
            handles = []
            containers = block_containers(self, coord)
            for section, (states, (count, wire)) in enumerate(zip(self.chunks[coord], containers)):
                handle = resources.enter_context(self.kernel.owned(
                    "palette_new_single", "palette_free", states[0]))
                for index, state in enumerate(states):
                    if state != states[0]:
                        self.kernel.call("palette_set", handle, index, state)
                self.verify_section(handle, states, count, wire, (coord, section))
                handles.append(handle)
                self.wire_widths.add(wire[0])
                self.initial_sections += 1
            self.palettes[coord] = (handles, resources.pop_all())

    def verify_section(self, handle, states, count, wire, label):
        actual = (abi.U32 * abi.SECTION_CELLS)()
        self.kernel.call("palette_unpack_into", handle, actual, len(actual))
        require(list(actual) == states, f"c-abi-section-cells:{label}")
        non_air = abi.U16()
        self.kernel.call("palette_non_air", handle, C.byref(non_air))
        require(non_air.value == count, f"c-abi-section-count:{label}")
        actual_wire = self.kernel.bytes("palette_write_block_states_wire", handle)
        require(actual_wire == wire, f"c-abi-section-bytes:{label}")

    def check_snapshot(self, client, cell):
        x, y, z = cell
        coord, section = (x // 16, z // 16), (y - self.min_y) // 16
        handle, _ = self.cell(x, y, z)
        count, wire = block_containers(client, coord)[section]
        self.verify_section(handle, client.chunks[coord][section], count, wire, cell)

    def cell(self, x, y, z):
        require(self.min_y <= y < self.max_y, f"kernel-section-height:{(x, y, z)}")
        handles, _ = self.palettes[(x // 16, z // 16)]
        return handles[(y - self.min_y) // 16], (y & 15) * 256 + (z & 15) * 16 + (x & 15)

    def block(self, x, y, z):
        handle, index = self.cell(x, y, z)
        state = abi.U32()
        self.kernel.call("palette_get", handle, index, C.byref(state))
        return state.value


class Movement:
    def __init__(self, actor, observer, start):
        self.actor, self.observer = actor, observer
        self.coords = start
        self.grounded = True
        self.max_drift = 0.0
        self.samples = 0
        self.send(start, self.grounded)

    def send(self, coords, grounded):
        self.actor.send(0x1A, struct.pack(">ddd?", *coords, grounded))
        observer = self.observer
        expected_q = wire_position_q(coords)
        # A repeated pose inside the entity's update interval is culled before
        # it reaches the wire, so an already-matching pose needs no broadcast.
        known = last_entity_pose(observer, self.actor.entity_id)
        if (known is None or wire_position_q(known[0]) != expected_q
                or known[1] != grounded):
            observer.until(
                lambda pid, _: pid in MOVE_IDS and observer.teleports
                and observer.teleports[-1][0] == self.actor.entity_id
                and wire_position_q(observer.teleports[-1][1]) == expected_q
                and observer.teleports[-1][2] == grounded,
                "server-entity-position",
            )
            _, accepted, ground_flag = observer.teleports[-1]
            if observer.last_move_packet == 0x70:
                require(accepted == coords,
                        f"server-position-drift: predicted={coords}, server={accepted}")
            else:
                require(wire_position_q(accepted) == expected_q,
                        f"server-position-drift: predicted={coords}, server={accepted}")
            require(ground_flag == grounded, "server-ground-flag-drift")
        else:
            accepted = coords
        self.max_drift = max(self.max_drift, math.dist(coords, accepted))
        self.samples += 1

    def box(self):
        x, y, z = self.coords
        return abi.Aabb(abi.Vec(x - PLAYER_HALF_WIDTH, y, z - PLAYER_HALF_WIDTH),
                        abi.Vec(x + PLAYER_HALF_WIDTH, y + PLAYER_HEIGHT, z + PLAYER_HALF_WIDTH))

    def collision(self, delta, step_height=0.6):
        box = self.box()
        # Collect the union of the ordinary and step-up sweeps. The kernel
        # filters it for each sweep. The server visits cells in Y, Z, X order.
        bounds = [
            range(math.floor(lo + min(d, 0) - 1e-5) - 1,
                  math.floor(hi + max(d, up) + 1e-5) + 2)
            for lo, hi, d, up in zip(box.min, box.max, delta, (0, step_height, 0))
        ]
        shapes = []
        for y in bounds[1]:
            for z in bounds[2]:
                for x in bounds[0]:
                    state = self.actor.block(x, y, z)
                    if state in AIR_STATES or state in WATER_STATES:
                        continue
                    require(state in (STONE, BOTTOM_SLAB),
                            f"unsupported-arena-shape:{(x, y, z, state)}")
                    height = 0.5 if state == BOTTOM_SLAB else 1.0
                    shapes.append(abi.Aabb(abi.Vec(x, y, z), abi.Vec(x + 1, y + height, z + 1)))
        result = abi.Collision()
        self.actor.kernel.call("collide", C.byref(box), abi.Vec(*delta), self.grounded, step_height,
                               (abi.Aabb * len(shapes))(*shapes), len(shapes), C.byref(result))
        return result

    def step(self, delta):
        result = self.collision(delta)
        self.coords = tuple(p + d for p, d in zip(self.coords, result.delta))
        self.grounded = bool(result.flags & abi.BELOW)
        self.send(self.coords, self.grounded)
        return result

    def water(self, velocity):
        failures = []

        @abi.Lookup
        def lookup(_context, x, y, z, out):
            try:
                state = self.actor.block(x, y, z)
                if state in WATER_STATES:
                    level = state - WATER_SOURCE
                    amount, flags = (8, abi.FALLING) if level >= 8 else (8 - level, 0)
                    out[0] = abi.FluidFacts(abi.WATER, amount, flags, 0)
                elif state == STONE:
                    out[0] = abi.FluidFacts(0, 0, abi.BLOCKS_MOTION, abi.ALL_FACES)
                else:
                    require(state in AIR_STATES, f"unsupported-water-cell:{(x, y, z, state)}")
                return 0
            # ctypes callbacks must return normally even if snapshot lookup fails.
            except BaseException as error:  # noqa: BLE001
                failures.append(error)
                return 1

        query = abi.FluidInput(abi.Vec(*velocity), abi.WATER, abi.PUSHED | abi.PLAYER)
        result = abi.FluidResult()
        try:
            self.actor.kernel.call("fluid_impulse", C.byref(self.box()), C.byref(query), lookup,
                                   None, C.byref(result))
        except AssertionError:
            if failures:
                raise failures[0] from None
            raise
        return result
