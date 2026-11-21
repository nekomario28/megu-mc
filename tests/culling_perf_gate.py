#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
"""Culling/perf regression gate for the client-fork render architecture.

See specs/client-fork/sodium-culling-architecture.md for the layer contract
this gate enforces.

Two modes:

  --self-test   Pure-Python models of the culling math, bucket skipping and
                the server's chunk-packet cache invariants. No cargo, no
                network, no server binary; deterministic and fast.

  (default)     Live gate: builds client-port's release `lod_bench` and
                `mesh_bench` when absent, runs them, and asserts both the
                exact quad/vertex counts (zero output regression) and per-
                metric timing ceilings (zero perf regression vs the pinned
                budgets below). `--runs N` controls bench iterations.

Usage:
    python3 tests/culling_perf_gate.py --self-test
    python3 tests/culling_perf_gate.py [--runs 100] [--no-build]
"""

import argparse
import math
import os
from pathlib import Path
import re
import subprocess
import sys
import zlib
from collections import deque

ROOT = Path(__file__).resolve().parents[1]
CLIENT_PORT = ROOT / "client-port"
BENCH_DIR = CLIENT_PORT / "target" / "release"

# ---------------------------------------------------------------------------
# Python mirrors of the Rust contracts under test.
#
# Flag values and direction order are pinned to client-port/src/section.rs and
# src/entity_spatial.rs / src/connection_limits.rs / src/chunk_ack.rs /
# src/main.rs (MapChunkCache). The models exist so --self-test can prove the
# gate's expectations without compiling Rust; the live mode then checks the
# real binaries against the same invariants.
# ---------------------------------------------------------------------------

EDGE = 16
FACE_CELLS = EDGE * EDGE
SECTION_CELLS = EDGE * FACE_CELLS
LIGHT_BYTES = SECTION_CELLS // 2
ALL_FACES = 0x3F

# client-port/src/section.rs Block::flags bits.
OPAQUE = 1 << 0
CUTOUT = 1 << 1
TRANSLUCENT = 1 << 2
WATERLOGGED = 1 << 3
TINTED = 1 << 4
FLUID_TINTED = 1 << 5
SAME_STATE_CULL = 1 << 6
RENDER_FLAGS = OPAQUE | CUTOUT | TRANSLUCENT

NO_FLUID, WATER, LAVA = 0, 1, 2

# Direction::ALL order: Down, Up, North, South, West, East.
DOWN, UP, NORTH, SOUTH, WEST, EAST = range(6)
STEP = [-FACE_CELLS, FACE_CELLS, -EDGE, EDGE, -1, 1]
F64_EPSILON = 2.220446049250313e-16


def direction_bit(direction):
    return 1 << direction


def direction_opposite(direction):
    return direction ^ 1


def on_boundary(direction, p):
    x, y, z = p
    return ((direction == DOWN and y == 0) or (direction == UP and y == EDGE - 1)
            or (direction == NORTH and z == 0) or (direction == SOUTH and z == EDGE - 1)
            or (direction == WEST and x == 0) or (direction == EAST and x == EDGE - 1))


def section_index(x, y, z):
    return y * FACE_CELLS + z * EDGE + x


class Block:
    """Mirror of client-port Block (flags/occlusion/fluid fields)."""

    __slots__ = ("state_id", "tint", "fluid_tint", "flags", "occlusion",
                 "fluid_kind", "fluid_level")

    def __init__(self, state_id=0, tint=0, fluid_tint=0, flags=0, occlusion=0,
                 fluid_kind=NO_FLUID, fluid_level=0):
        self.state_id = state_id
        self.tint = tint
        self.fluid_tint = fluid_tint
        self.flags = flags
        self.occlusion = occlusion
        self.fluid_kind = fluid_kind
        self.fluid_level = fluid_level

    def occludes(self, direction):
        return self.flags & OPAQUE != 0 and self.occlusion & direction_bit(direction) != 0

    def renderable(self):
        return self.flags & RENDER_FLAGS != 0 or self.fluid() != NO_FLUID

    def fluid(self):
        if self.flags & WATERLOGGED != 0:
            return WATER
        return self.fluid_kind if self.fluid_kind in (WATER, LAVA) else NO_FLUID

    def seals(self):
        return (self.flags & OPAQUE != 0 and self.occlusion == ALL_FACES
                and self.fluid() == NO_FLUID)


AIR = Block()


def opaque_cube(state_id):
    return Block(state_id=state_id, flags=OPAQUE, occlusion=ALL_FACES)


def face_can_emit(block, neighbor, direction):
    """Mirror of cull.rs face_can_emit: conservative superset of the mesher."""
    neighbor_occludes = neighbor.occludes(direction_opposite(direction))
    if (block.flags & RENDER_FLAGS != 0 and not neighbor_occludes
            and not (block.flags & neighbor.flags & SAME_STATE_CULL != 0
                     and block.state_id == neighbor.state_id)):
        return True
    return (block.fluid() != NO_FLUID and not neighbor_occludes
            and not block.occludes(direction))


def boundary_pos(direction, face):
    """Inverse of Direction::border_index (cull.rs boundary_pos)."""
    edge = (direction & 1) * (EDGE - 1)
    axis = (WEST, EAST).index(direction) if direction in (WEST, EAST) else (
        (DOWN, UP).index(direction) if direction in (DOWN, UP) else None)
    if axis == 0:
        return [edge, face % EDGE, face // EDGE]
    if axis == 1:
        return [face % EDGE, edge, face // EDGE]
    return [face % EDGE, face // EDGE, edge]


def section_vis(blocks, neighbor):
    """Mirror of SectionVis::scan_blocks.

    `blocks` is a 4096-entry list in section_index order; `neighbor(direction,
    p)` returns the Block outside a boundary position. Returns
    (boundary_faces, interior_faces, uniform_air).
    """
    boundary_faces = 0
    interior_faces = False
    any_renderable = False
    for y in range(EDGE):
        for z in range(EDGE):
            for x in range(EDGE):
                p = (x, y, z)
                index = section_index(x, y, z)
                block = blocks[index]
                if not block.renderable():
                    continue
                any_renderable = True
                for direction in range(6):
                    if on_boundary(direction, p):
                        nxt = neighbor(direction, p)
                    else:
                        nxt = blocks[index + STEP[direction]]
                    if face_can_emit(block, nxt, direction):
                        if on_boundary(direction, p):
                            boundary_faces |= direction_bit(direction)
                        else:
                            interior_faces = True
                if interior_faces and boundary_faces == ALL_FACES:
                    return boundary_faces, interior_faces, False
    return boundary_faces, interior_faces, not any_renderable


def emits_any(vis):
    boundary_faces, interior_faces, _ = vis
    return interior_faces or boundary_faces != 0


def visible_towards(vis, facing):
    boundary_faces, interior_faces, _ = vis
    return interior_faces or (boundary_faces & facing) != 0


def box_facing_mask(mn, camera):
    """Mirror of cull.rs box_facing_mask; box spans mn..=mn+EDGE per axis."""
    mask = 0
    for axis, (low, high) in enumerate(((WEST, EAST), (DOWN, UP), (NORTH, SOUTH))):
        if camera[axis] < mn[axis]:
            mask |= direction_bit(low)
        elif camera[axis] > mn[axis] + EDGE:
            mask |= direction_bit(high)
    return mask


def box_in_frustum(mn, planes):
    """Mirror of cull.rs box_in_frustum; planes are [nx,ny,nz,d], inside >= 0."""
    for nx, ny, nz, d in planes:
        p = [mn[0] + EDGE if nx >= 0 else mn[0],
             mn[1] + EDGE if ny >= 0 else mn[1],
             mn[2] + EDGE if nz >= 0 else mn[2]]
        if nx * p[0] + ny * p[1] + nz * p[2] + d < 0.0:
            return False
    return True


def emits_nothing(blocks, sealed_hint, side_sealed, face_block):
    """Mirror of cull.rs emits_nothing (the mesher-skip proof)."""
    all_sides_sealed = True
    for direction in range(6):
        if side_sealed(direction):
            continue
        all_sides_sealed = False
        for face in range(FACE_CELLS):
            p = boundary_pos(direction, face)
            cell = blocks[section_index(*p)]
            if face_can_emit(cell, face_block(direction, p), direction):
                return False
    if sealed_hint:
        return True
    any_renderable = False
    all_sealing = True
    for block in blocks:
        any_renderable |= block.renderable()
        all_sealing &= block.seals()
        if any_renderable and not all_sealing:
            return False
    return not any_renderable or (all_sealing and all_sides_sealed)


# ---------------------------------------------------------------------------
# Server-side models: prioritize_chunks, ChunkSendBudget, ChunkWindow,
# MapChunkCache. Mirrors of src/entity_spatial.rs, src/connection_limits.rs,
# src/chunk_ack.rs and src/main.rs.
# ---------------------------------------------------------------------------

def snap(component):
    return 0.0 if abs(component) < F64_EPSILON else component


def prioritize_chunks(chunks, center, look):
    """Mirror of entity_spatial.rs prioritize_chunks (stable total order)."""
    valid = all(math.isfinite(v) for v in look)
    scale = max(abs(look[0]), abs(look[1])) if valid else 0.0
    direction = (look[0] / scale, look[1] / scale) if scale > 0.0 else (0.0, 0.0)

    def key(c):
        dx = c[0] - center[0]
        dz = c[1] - center[1]
        dot = dx * direction[0] + dz * direction[1]
        rank = 0 if (dx == 0 and dz == 0) else 1 if dot > 0 else 2 if dot == 0 else 3
        distance = dx * dx + dz * dz
        return (rank, distance, c[0], c[1])

    chunks.sort(key=key)
    return chunks


class ChunkSendBudget:
    """Mirror of entity_spatial.rs ChunkSendBudget."""

    def __init__(self, max_chunks_per_tick):
        self.queued = []
        self.max_chunks_per_tick = max_chunks_per_tick

    def enqueue(self, chunk):
        if chunk not in self.queued:
            self.queued.append(chunk)

    def drain_tick(self, center, radius, look):
        r = int(radius)
        self.queued = [c for c in self.queued
                       if abs(c[0] - center[0]) <= r and abs(c[1] - center[1]) <= r]
        prioritize_chunks(self.queued, center, look)
        count = min(self.max_chunks_per_tick, len(self.queued))
        return self.queued[:count] if count else []

    def drain_tick_pop(self, center, radius, look):
        batch = self.drain_tick(center, radius, look)
        del self.queued[:len(batch)]
        return batch


class ChunkDelivery:
    """Mirror of connection_limits.rs ChunkDelivery.next_batch."""

    def __init__(self, window_batch=64):
        self.budget = ChunkSendBudget(min(window_batch, 4096))
        self.window = None
        self.last_batch_tick = None
        # The client's 0x08 ack gate (ChunkFeedback in src/chunk_ack.rs) is a
        # server-side pacing rule, not part of the window/budget math modeled
        # here; this model always has the gate open.

    def next_batch(self, window, sent, yaw_deg, tick):
        if self.window != window:
            self.window = window
            self.budget.queued = [c for c in self.budget.queued if window.contains(c)]
            for c in window.unsent(sent):
                self.budget.enqueue(c)
        if self.last_batch_tick == tick or not self.budget.queued:
            return []
        yaw = math.radians(yaw_deg % 360.0)
        look = (snap(-math.sin(yaw)), snap(math.cos(yaw)))
        batch = self.budget.drain_tick_pop(window.center, window.radius, look)
        if batch:
            self.last_batch_tick = tick
        return batch


class ChunkWindow:
    """Mirror of play_protocol.rs ChunkWindow (Chebyshev window)."""

    def __init__(self, center, radius):
        self.center = center
        self.radius = radius

    def __eq__(self, other):
        return (isinstance(other, ChunkWindow)
                and self.center == other.center and self.radius == other.radius)

    def contains(self, c):
        return max(abs(c[0] - self.center[0]), abs(c[1] - self.center[1])) <= self.radius

    @staticmethod
    def _cells(center, radius):
        return {(x, z)
                for x in range(center[0] - radius, center[0] + radius + 1)
                for z in range(center[1] - radius, center[1] + radius + 1)}

    def leaving(self, previous, sent):
        if previous is not None and previous.radius == self.radius:
            removed = self._cells(previous.center, self.radius) - self._cells(self.center, self.radius)
            return sorted(c for c in removed if c in sent)
        return sorted(c for c in sent if not self.contains(c))

    def unsent(self, sent):
        return (c for c in sorted(self._cells(self.center, self.radius)) if c not in sent)


class MapChunkCache:
    """Mirror of main.rs MapChunkCache + map_chunk invalidation contract.

    A hit requires identical 3x3 versions AND identical order-independent
    signature; a hit also restores the chunk's own emitters into `gen`.
    """

    FNV_OFFSET = 0xCBF29CE484222325
    FNV_PRIME = 0x100000001B3
    MASK64 = 0xFFFFFFFFFFFFFFFF

    def __init__(self, cap=8):
        self.cap = cap
        self.entries = {}
        self.order = deque()
        self.builds = 0

    @staticmethod
    def _flatten(item):
        if isinstance(item, int):
            yield item & 0xFFFFFFFF
        elif isinstance(item, str):
            yield zlib.crc32(item.encode())
        else:
            for part in item:
                yield from MapChunkCache._flatten(part)

    @classmethod
    def _fnv(cls, values):
        h = cls.FNV_OFFSET
        for v in values:
            h = ((h ^ v) * cls.FNV_PRIME) & cls.MASK64
        return h

    def sig(self, items):
        # Order-independent like map_chunk_sig: per-item hash, then summed.
        return sum(self._fnv(self._flatten(item)) for item in items) & self.MASK64

    def request(self, key, versions, sig_items, emitters, payload, gen):
        entry = self.entries.get(key)
        sig = self.sig(sig_items)
        if entry is not None and entry["versions"] == versions and entry["sig"] == sig:
            gen[key] = list(entry["emitters"])
            return entry["payload"], False
        self.builds += 1
        body = payload() if callable(payload) else payload
        if key not in self.entries:
            self.order.append(key)
        self.entries[key] = {"versions": versions, "sig": sig,
                             "emitters": list(emitters), "payload": body}
        while len(self.order) > self.cap:
            self.entries.pop(self.order.popleft(), None)
        gen[key] = list(emitters)
        return body, True


# ---------------------------------------------------------------------------
# client-port LOD model: lod.rs summarize_column.
# ---------------------------------------------------------------------------

LOD_COARSE = 4
LOD_FINE = EDGE
LOD_CELL_EMPTY_TOP = -1


def summarize_column(sections, edge):
    """Mirror of lod.rs summarize_column.

    `sections` is bottom-to-top; each section is a 4096-list in
    section_index order whose entries are Block or None (air). Returns
    edge*edge cells in x-fastest, z-major order: (state_id, tint, top_y,
    light) with top_y = -1 for empty footprints.
    """
    assert edge in (LOD_COARSE, LOD_FINE)
    tops = [None] * FACE_CELLS
    found = [False] * FACE_CELLS
    for si in range(len(sections) - 1, -1, -1):
        if all(found):
            break
        blocks = sections[si]
        for z in range(EDGE):
            for x in range(EDGE):
                col = z * EDGE + x
                if found[col]:
                    continue
                cell = None
                for y in range(EDGE - 1, -1, -1):
                    block = blocks[y * FACE_CELLS + col]
                    if block is not None and block.renderable():
                        cell = (block.state_id, block.tint, min(si * EDGE + y, 32767))
                        break
                tops[col] = cell
                if cell is not None:
                    found[col] = True
    foot = EDGE // edge
    out = []
    for cz in range(edge):
        for cx in range(edge):
            best = None
            for bz in range(foot):
                for bx in range(foot):
                    cell = tops[(cz * foot + bz) * EDGE + cx * foot + bx]
                    if cell is not None and (best is None or cell[2] > best[2]):
                        best = cell
            out.append(best if best is not None else (0, 0, LOD_CELL_EMPTY_TOP))
    return out


# ---------------------------------------------------------------------------
# client-port translucent sort model: packed.rs is_translucent/depth_key and
# ffi.rs mc_spine_client_mesh_sort_translucent ordering.
# ---------------------------------------------------------------------------

LAYER_OPAQUE, LAYER_CUTOUT, LAYER_TRANSLUCENT, LAYER_FLUID = range(4)


def is_translucent_quad(layer, fluid_kind):
    return layer == LAYER_TRANSLUCENT or (layer == LAYER_FLUID and fluid_kind == WATER)


def depth_key(positions, camera):
    cx = sum(p[0] for p in positions) * 0.25
    cy = sum(p[1] for p in positions) * 0.25
    cz = sum(p[2] for p in positions) * 0.25
    return (cx * (cx - 2.0 * camera[0]) + cy * (cy - 2.0 * camera[1])
            + cz * (cz - 2.0 * camera[2]))


def translucent_order(quads, camera):
    """quads: list of (layer, fluid_kind, positions). Returns back-to-front
    indices of translucent quads only (decreasing depth_key, index tie-break)."""
    keyed = [(depth_key(q[2], camera), i) for i, q in enumerate(quads)
             if is_translucent_quad(q[0], q[1])]
    keyed.sort(key=lambda item: (-item[0], item[1]))
    return [i for _, i in keyed]


# ---------------------------------------------------------------------------
# Self-test checks.
# ---------------------------------------------------------------------------

def check(condition, code):
    if not condition:
        raise AssertionError(code)


def solid_section(block):
    return [block] * SECTION_CELLS


def self_test():
    # --- face_can_emit rules ---
    stone = opaque_cube(1)
    air_sample = AIR
    for direction in range(6):
        check(face_can_emit(stone, air_sample, direction), f"solid-vs-air-face-{direction}")
        check(not face_can_emit(stone, stone, direction), f"solid-vs-solid-face-{direction}")
    # Same-state cull: identical cutout states on both sides emit nothing.
    glass_a = Block(state_id=42, flags=CUTOUT | SAME_STATE_CULL, occlusion=ALL_FACES)
    glass_b = Block(state_id=42, flags=CUTOUT | SAME_STATE_CULL, occlusion=ALL_FACES)
    glass_c = Block(state_id=43, flags=CUTOUT | SAME_STATE_CULL, occlusion=ALL_FACES)
    check(not face_can_emit(glass_a, glass_b, NORTH), "same-state-cull-miss")
    check(face_can_emit(glass_a, glass_c, NORTH), "same-state-cull-false-positive")
    check(face_can_emit(glass_a, AIR, NORTH), "cutout-vs-air-miss")
    # Partial occlusion: the side the neighbor does not occlude still emits.
    half = Block(state_id=9, flags=OPAQUE, occlusion=direction_bit(WEST))
    check(face_can_emit(stone, half, WEST), "partial-occlusion-open-side")
    check(not face_can_emit(stone, half, EAST), "partial-occlusion-covered-side")
    slab_like = Block(state_id=10, flags=OPAQUE, occlusion=0)
    check(face_can_emit(stone, slab_like, NORTH), "zero-occlusion-neighbor")
    # Fluid branch: water emits toward air, never into a full occluder.
    # client-app gives water flags=FLUID_TINTED (no cube render flag), so only
    # the fluid rule applies.
    water = Block(state_id=80, fluid_kind=WATER, fluid_level=0,
                  flags=FLUID_TINTED, occlusion=0)
    check(face_can_emit(water, air_sample, UP), "water-top-face")
    check(not face_can_emit(water, stone, UP), "water-vs-opaque")
    # The mirror drops the mesher's same-kind fluid merge rule, so it may
    # over-report water-vs-water; the contract is only "never culls a real
    # face". The directional check that must hold: water under a full
    # occluder emits nothing downward.
    check(not face_can_emit(water, stone, DOWN), "water-under-opaque")

    # --- SectionVis proofs (the mesher-skip contract) ---
    sealed_missing = lambda d, p: opaque_cube(1)
    open_missing = lambda d, p: AIR
    vis_sealed = section_vis(solid_section(stone), sealed_missing)
    check(vis_sealed == (0, False, False), f"sealed-vis-{vis_sealed}")
    check(not emits_any(vis_sealed), "sealed-emits-any")
    vis_open = section_vis(solid_section(stone), open_missing)
    check(vis_open[0] == ALL_FACES and not vis_open[1], f"open-vis-{vis_open}")
    check(emits_any(vis_open), "open-emits-any")
    vis_air = section_vis([AIR] * SECTION_CELLS, open_missing)
    check(vis_air[2] and not emits_any(vis_air), f"air-vis-{vis_air}")
    check(visible_towards(vis_open, direction_bit(WEST)), "open-visible-west")
    check(not visible_towards(vis_sealed, ALL_FACES), "sealed-never-visible")

    # A solid section whose west neighbor plane is air emits only westward.
    mixed_missing = lambda d, p: AIR if d == WEST else opaque_cube(1)
    vis_mixed = section_vis(solid_section(stone), mixed_missing)
    check(vis_mixed[0] == direction_bit(WEST), f"mixed-vis-{vis_mixed}")

    # emits_nothing: sealed proof path vs interior scan path.
    check(emits_nothing(solid_section(stone), True,
                        lambda d: True, lambda d, p: opaque_cube(1)),
          "emits-nothing-sealed-hint")
    check(not emits_nothing(solid_section(stone), False,
                            lambda d: d != WEST, lambda d, p: AIR),
          "emits-nothing-open-side")
    check(emits_nothing([AIR] * SECTION_CELLS, False,
                        lambda d: False, lambda d, p: AIR),
          "emits-nothing-uniform-air")

    # --- box_facing_mask / box_in_frustum ---
    mn = [0.0, 0.0, 0.0]
    check(box_facing_mask(mn, [-1.0, 8.0, 8.0]) == direction_bit(WEST), "facing-west")
    check(box_facing_mask(mn, [8.0, -1.0, 8.0]) == direction_bit(DOWN), "facing-down")
    check(box_facing_mask(mn, [8.0, 17.0, 8.0]) == direction_bit(UP), "facing-up")
    check(box_facing_mask(mn, [8.0, 8.0, -1.0]) == direction_bit(NORTH), "facing-north")
    check(box_facing_mask(mn, [8.0, 8.0, 17.0]) == direction_bit(SOUTH), "facing-south")
    check(box_facing_mask(mn, [17.0, 8.0, 8.0]) == direction_bit(EAST), "facing-east")
    check(box_facing_mask(mn, [8.0, 8.0, 8.0]) == 0, "facing-inside")
    # Plane x <= 20 keeps the box; plane x >= 30 culls it (p=16 corner test).
    check(box_in_frustum(mn, [[-1.0, 0.0, 0.0, 20.0]]), "frustum-keep")
    check(not box_in_frustum(mn, [[1.0, 0.0, 0.0, -30.0]]), "frustum-cull")
    check(box_in_frustum(mn, [[0.0, 0.0, 0.0, 0.0]]), "frustum-degenerate")

    # --- prioritize_chunks / ChunkSendBudget / ChunkDelivery / ChunkWindow ---
    chunks = [(-1, 0), (0, 0), (1, 0), (0, 1), (0, -1), (1, 1), (1, -1), (-1, 1),
              (-1, -1), (2, 0)]
    ordered = prioritize_chunks(list(chunks), (0, 0), (0.0, 1.0))  # look +z
    check(ordered[0] == (0, 0), "priority-center-first")
    check(all(c[1] > 0 for c in ordered[1:4]), "priority-forward-first")
    check(all(c[1] == 0 and c[0] != 0 for c in ordered[4:7]), "priority-side-second")
    check(all(c[1] < 0 for c in ordered[7:]), "priority-behind-last")
    check(sorted(ordered) == sorted(chunks), "priority-keeps-all")
    # Deterministic tie-break: distance then coordinates.
    tied = prioritize_chunks([(1, 1), (1, -1), (-1, 0)], (0, 0), (0.0, 0.0))
    check(tied == [(-1, 0), (1, -1), (1, 1)], f"priority-tie-order-{tied}")

    budget = ChunkSendBudget(2)
    for c in [(0, 0), (1, 0), (-1, 0), (0, 1), (5, 5), (1, 0)]:
        budget.enqueue(c)
    first = budget.drain_tick_pop((0, 0), 1, (1.0, 0.0))
    check(first == [(0, 0), (1, 0)], f"budget-drain-{first}")
    # (5,5) fell out of the window; the two survivors drain in look order:
    # (0,1) ranks ahead of (-1,0) for +x look.
    second = budget.drain_tick_pop((0, 0), 1, (1.0, 0.0))
    check(second == [(0, 1), (-1, 0)], f"budget-second-{second}")
    check(budget.drain_tick_pop((0, 0), 1, (1.0, 0.0)) == [], "budget-drained")

    window = ChunkWindow((0, 0), 1)
    sent = {(0, 0), (1, 0), (-1, -1)}
    moved = ChunkWindow((2, 0), 1)
    check(window.contains((1, 1)) and not window.contains((2, 0)), "window-contains")
    # Old window x in -1..1 minus new window x in 1..3 leaves x in {-1,0};
    # of those only (0,0) and (-1,-1) were actually sent.
    check(moved.leaving(window, sent) == [(-1, -1), (0, 0)], "window-leaving")
    check(set(moved.unsent(sent)) == {(1, -1), (1, 1), (2, -1), (2, 0), (2, 1),
                                    (3, -1), (3, 0), (3, 1)}, "window-unsent")

    delivery = ChunkDelivery(window_batch=4)
    w = ChunkWindow((0, 0), 2)
    sent_set = set()
    batch1 = delivery.next_batch(w, sent_set, 0.0, 10)
    check(len(batch1) == 4 and batch1[0] == (0, 0), f"delivery-first-{len(batch1)}")
    check(delivery.next_batch(w, sent_set, 0.0, 10) == [], "delivery-one-batch-per-tick")
    check(len(delivery.next_batch(w, sent_set, 0.0, 11)) == 4, "delivery-next-tick")
    # A window move drops pending chunks outside the new window.
    w2 = ChunkWindow((10, 10), 1)
    batch3 = delivery.next_batch(w2, sent_set, 0.0, 12)
    check(batch3 and all(w2.contains(c) for c in batch3), "delivery-window-move")

    # --- MapChunkCache invariants ---
    cache = MapChunkCache(cap=4)
    gen = {}
    versions9 = (1,) * 9
    items_a = [("sign", (1, 2, 3, 4, 5)), ("spawner", (10, 20, 30))]
    items_b = list(reversed(items_a))  # same set, different order
    payload_a = lambda: b"chunk-A"
    body, rebuilt = cache.request((0, 0, 0), versions9, items_a, [(5, 6, 7)], payload_a, gen)
    check(rebuilt and body == b"chunk-A", "cache-first-build")
    check(cache.builds == 1, "cache-build-count")
    check(gen[(0, 0, 0)] == [(5, 6, 7)], "cache-emitters-stored")
    body2, rebuilt2 = cache.request((0, 0, 0), versions9, items_b,
                                    [(5, 6, 7)], payload_a, gen)
    check(not rebuilt2 and body2 == b"chunk-A" and cache.builds == 1,
          "cache-hit-order-independent-sig")
    check(gen[(0, 0, 0)] == [(5, 6, 7)], "cache-hit-restores-emitters")
    # A neighbor write bumps one 3x3 version -> miss.
    bumped = list(versions9)
    bumped[4] += 1
    _, rebuilt3 = cache.request((0, 0, 0), tuple(bumped), items_a,
                                [(5, 6, 7)], payload_a, gen)
    check(rebuilt3 and cache.builds == 2, "cache-version-invalidation")
    # Same versions, different signature (sign text edit) -> miss.
    items_c = items_a + [("sign", (9, 9, 9, 9, 9))]
    _, rebuilt4 = cache.request((0, 0, 0), versions9, items_c,
                                [(5, 6, 7)], payload_a, gen)
    check(rebuilt4 and cache.builds == 3, "cache-sig-invalidation")
    # FIFO cap: 4 entries held; a 5th evicts the oldest.
    for i in range(1, 5):
        cache.request((i, 0, 0), versions9, items_a, [], b"p", gen)
    check(len(cache.entries) == 4, "cache-cap-bound")
    cache.request((9, 9, 9), versions9, items_a, [], b"p9", gen)
    check((0, 0, 0) not in cache.entries and len(cache.entries) == 4,
          "cache-fifo-eviction")
    _, rebuilt5 = cache.request((0, 0, 0), versions9, items_a, [], b"p", gen)
    check(rebuilt5, "cache-evicted-rebuilds")

    # --- LOD summarize_column ---
    def cell(state_id, tint=0, light=0, renderable=True):
        return Block(state_id=state_id, tint=tint, flags=OPAQUE if renderable else 0,
                     occlusion=ALL_FACES if renderable else 0) if renderable else AIR

    stone_cell = cell(1)
    gold_cell = Block(state_id=7, tint=9, flags=OPAQUE, occlusion=ALL_FACES)
    air_section = [None] * SECTION_CELLS
    bottom = [stone_cell] * SECTION_CELLS
    top = list(air_section)
    top[section_index(3, 10, 4)] = gold_cell  # x=3, z=4 column, y=10
    sections = [bottom, air_section, top]
    coarse = summarize_column(sections, LOD_COARSE)
    check(len(coarse) == LOD_COARSE * LOD_COARSE, "lod-coarse-size")
    # Coarse cell (cx=0, cz=1) covers x 0..3, z 4..7 -> contains the gold column.
    gold_coarse = coarse[1 * LOD_COARSE + 0]
    check(gold_coarse[0] == 7 and gold_coarse[2] == 2 * EDGE + 10,
          f"lod-coarse-top-{gold_coarse}")
    other = coarse[0]
    check(other[0] == 1 and other[2] == 15, f"lod-coarse-stone-{other}")
    fine = summarize_column(sections, LOD_FINE)
    check(fine[4 * EDGE + 3][0] == 7 and fine[4 * EDGE + 3][2] == 42,
          "lod-fine-gold-column")
    check(fine[0][0] == 1 and fine[0][2] == 15, "lod-fine-stone-column")
    empty = summarize_column([air_section, air_section], LOD_COARSE)
    check(all(c[2] == LOD_CELL_EMPTY_TOP for c in empty), "lod-empty-column")
    # Bucket skipping: an all-air column produces no cells to draw.
    check(all(c[2] < 0 for c in summarize_column([air_section], LOD_FINE)),
          "lod-air-skip")

    # --- translucent sort ---
    def quad(cx, cy, cz, layer=LAYER_TRANSLUCENT, fluid=WATER):
        return (layer, fluid, [[cx, cy, cz]] * 4)

    quads = [quad(0, 0, 0, LAYER_OPAQUE),      # excluded
             quad(10, 0, 0),                  # far
             quad(2, 0, 0),                   # near
             quad(10, 0, 0),                  # far tie -> lower index first
             quad(5, 0, 0, LAYER_FLUID, LAVA)]  # lava excluded
    order = translucent_order(quads, (0.0, 0.0, 0.0))
    check(order == [1, 3, 2], f"translucent-order-{order}")
    check(translucent_order([quad(1, 0, 0, LAYER_FLUID, WATER)], (0, 0, 0)) == [0],
          "water-fluid-included")


def self_test_main():
    self_test()
    print("culling perf gate self-test: PASS")
    return 0


# ---------------------------------------------------------------------------
# Live mode: run the real client-port benches and gate their output.
# ---------------------------------------------------------------------------

# Timing ceilings in microseconds, ~5-10x the measured values on the reference
# host (Ryzen 7 9800X3D, 2026-11-21, --runs 100). They exist to catch order-
# of-magnitude regressions, not to police noise; exact quad/vertex counts are
# the zero-output-regression assertion and are pinned separately.
CEILINGS_US = {
    "mesh_solid_us": 50.0,
    "mesh_checkerboard_us": 3000.0,
    "mesh_column_us": 1500.0,
    "lod_coarse_us": 500.0,
    "lod_fine_us": 600.0,
    "lod_sealed_mesh_us": 100.0,
    "lod_section_vis_us": 500.0,
    "lod_open_mesh_us": 100.0,
    "lod_sealed_hint_us": 200.0,
    "lod_sealed_scan_us": 1000.0,
    "lod_unsealed_us": 1500.0,
    "lod_overworld_us": 1500.0,
}

PINNED_COUNTS = {
    "mesh_solid_quads": 6,
    "mesh_checkerboard_quads": 12288,
    "mesh_column_quads": 2037,
    "mesh_column_vertices": 8148,
    "lod_unsealed_quads": 98,
    "lod_overworld_quads": 1872,
    "lod_max_top_y": 133,
}


def run_bench(binary, runs, timeout=600):
    proc = subprocess.run([str(binary), str(runs)], cwd=CLIENT_PORT,
                          capture_output=True, text=True, timeout=timeout,
                          env=dict(os.environ, CARGO_TERM_COLOR="never"))
    if proc.returncode != 0:
        raise RuntimeError(f"{binary.name} exited {proc.returncode}:\n{proc.stderr[-2000:]}")
    return proc.stdout


def parse_us(pattern, text, name):
    match = re.search(pattern, text)
    if not match:
        raise RuntimeError(f"{name}: pattern not found in bench output")
    return float(match.group(1))


def live_gate(runs, build):
    mesh_bench = BENCH_DIR / "mesh_bench"
    lod_bench = BENCH_DIR / "lod_bench"
    if not (mesh_bench.is_file() and lod_bench.is_file()):
        if not build:
            raise RuntimeError("client-port release benches missing; run without --no-build")
        print("[culling-gate] building client-port release benches", flush=True)
        subprocess.run(["cargo", "build", "--release", "--offline", "--locked", "--bins"],
                       cwd=CLIENT_PORT, check=True, timeout=3600)
    check(mesh_bench.is_file() and lod_bench.is_file(), "bench-binaries-missing")

    results = {}
    failures = []

    def record(name, value, ceiling=None, pinned=None):
        results[name] = value
        status = "ok"
        if pinned is not None and value != pinned:
            status = f"PINNED {pinned}"
            failures.append(f"{name}: got {value}, pinned {pinned}")
        if ceiling is not None and value > ceiling:
            status += f" OVER {ceiling}"
            failures.append(f"{name}: {value:.2f}us exceeds ceiling {ceiling}us")
        print(f"  {name}: {value}  [{status}]", flush=True)

    # --- mesh_bench ---
    out = run_bench(mesh_bench, runs)
    solid = re.search(r"solid: (\d+) quads, \d+ sections, ([\d.]+) us/section", out)
    check(solid is not None, "mesh_bench-solid-line")
    record("mesh_solid_quads", int(solid.group(1)), None, PINNED_COUNTS["mesh_solid_quads"])
    record("mesh_solid_us", float(solid.group(2)), CEILINGS_US["mesh_solid_us"], None)

    checker = re.search(r"checkerboard: (\d+) quads, \d+ sections, ([\d.]+) us/section",
                        out)
    check(checker is not None, "mesh_bench-checkerboard-line")
    record("mesh_checkerboard_quads", int(checker.group(1)), None,
           PINNED_COUNTS["mesh_checkerboard_quads"])
    record("mesh_checkerboard_us", float(checker.group(2)),
           CEILINGS_US["mesh_checkerboard_us"], None)

    col = re.search(r"Overworld -64\.\.319: (\d+) quads, (\d+) vertices, .*?"
                    r"([\d.]+) us/column", out)
    check(col is not None, "mesh_bench-column-line")
    record("mesh_column_quads", int(col.group(1)), None, PINNED_COUNTS["mesh_column_quads"])
    record("mesh_column_vertices", int(col.group(2)), None,
           PINNED_COUNTS["mesh_column_vertices"])
    record("mesh_column_us", float(col.group(3)), CEILINGS_US["mesh_column_us"], None)

    # --- lod_bench ---
    out = run_bench(lod_bench, runs)
    record("lod_coarse_us",
           parse_us(r"lod 4x4: ([\d.]+) us/column \(16 cells", out, "lod_bench"),
           CEILINGS_US["lod_coarse_us"], None)
    record("lod_fine_us",
           parse_us(r"lod 16x16: ([\d.]+) us/column \(256 cells", out, "lod_bench"),
           CEILINGS_US["lod_fine_us"], None)
    top = re.search(r"max top_y=(\d+)", out)
    check(top is not None, "lod_bench-top-line")
    record("lod_max_top_y", int(top.group(1)), None, PINNED_COUNTS["lod_max_top_y"])

    record("lod_sealed_mesh_us",
           parse_us(r"mesh sealed section \(culled\): ([\d.]+) us", out, "lod_bench"),
           CEILINGS_US["lod_sealed_mesh_us"], None)
    record("lod_section_vis_us",
           parse_us(r"SectionVis::scan sealed section \(mask only\): ([\d.]+) us",
                    out, "lod_bench"),
           CEILINGS_US["lod_section_vis_us"], None)
    record("lod_open_mesh_us",
           parse_us(r"mesh open solid section \(no cull possible\): ([\d.]+) us",
                    out, "lod_bench"),
           CEILINGS_US["lod_open_mesh_us"], None)
    record("lod_sealed_hint_us",
           parse_us(r"mesh 24 sealed-hinted sections \(culled\): ([\d.]+) us/column",
                    out, "lod_bench"),
           CEILINGS_US["lod_sealed_hint_us"], None)
    record("lod_sealed_scan_us",
           parse_us(r"mesh 24 sealed sections \(scanned\): ([\d.]+) us/column",
                    out, "lod_bench"),
           CEILINGS_US["lod_sealed_scan_us"], None)
    unsealed = re.search(r"mesh 24 unsealed solid sections \(full pipeline\): "
                         r"([\d.]+) us/column \((\d+) quads\)", out)
    check(unsealed is not None, "lod_bench-unsealed-line")
    record("lod_unsealed_quads", int(unsealed.group(2)), None,
           PINNED_COUNTS["lod_unsealed_quads"])
    record("lod_unsealed_us", float(unsealed.group(1)), CEILINGS_US["lod_unsealed_us"], None)
    over = re.search(r"mesh overworld column: ([\d.]+) us/column \((\d+) quads",
                     out)
    check(over is not None, "lod_bench-overworld-line")
    record("lod_overworld_quads", int(over.group(2)), None,
           PINNED_COUNTS["lod_overworld_quads"])
    record("lod_overworld_us", float(over.group(1)), CEILINGS_US["lod_overworld_us"], None)

    if failures:
        for failure in failures:
            print(f"culling perf gate: FAIL {failure}", file=sys.stderr)
        return 1
    print(f"culling perf gate: PASS ({runs} runs/bench, "
          f"{len(results)} metrics)")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--self-test", action="store_true",
                        help="run the pure-Python culling-math self-test only")
    parser.add_argument("--runs", type=int, default=100,
                        help="bench iterations per binary (default 100)")
    parser.add_argument("--no-build", action="store_true",
                        help="fail instead of building missing release benches")
    args = parser.parse_args(argv)
    if args.runs <= 0:
        parser.error("--runs must be positive")
    if args.self_test:
        return self_test_main()
    return live_gate(args.runs, build=not args.no_build)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (AssertionError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"culling perf gate: FAIL {error}", file=sys.stderr)
        sys.exit(1)
