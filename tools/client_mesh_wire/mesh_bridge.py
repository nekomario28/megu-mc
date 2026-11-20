#!/usr/bin/env python3
"""Protocol 767 chunk/light receiver and client-port C ABI bridge.

This module deliberately keeps wire decoding and mesh ABI conversion on the
caller side of client-port, matching the ownership boundary in its C header.
"""
from __future__ import annotations

import ctypes as C
import json
import os
from pathlib import Path
import subprocess
import sys
from dataclasses import dataclass

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tests"))
from protocol767 import Client, Reader, decode_chunk_light, signed  # noqa: E402

EDGE = 16
CELLS = 4096
LIGHT_BYTES = 2048
FACE_CELLS = 256
ABI_VERSION = 1
ABI_OK = 0
MISSING_AIR = 0

OPAQUE = 1 << 0
CUTOUT = 1 << 1
TRANSLUCENT = 1 << 2
SAME_STATE_CULL = 1 << 6
ALL_FACES = 0x3f
NO_FLUID, WATER, LAVA = 0, 1, 2
DIR_DOWN, DIR_UP, DIR_NORTH, DIR_SOUTH, DIR_WEST, DIR_EAST = range(6)


class MeshBlock(C.Structure):
    _fields_ = [("state_id", C.c_uint32), ("tint", C.c_uint32),
                ("fluid_tint", C.c_uint32), ("flags", C.c_uint8),
                ("occlusion", C.c_uint8), ("fluid_kind", C.c_uint8),
                ("fluid_level", C.c_uint8)]


class MeshSample(C.Structure):
    _fields_ = [("block", MeshBlock), ("light", C.c_uint8),
                ("reserved", C.c_uint8 * 3)]


class MeshInput(C.Structure):
    _fields_ = [("abi_version", C.c_uint32), ("missing", C.c_uint32),
                ("blocks", C.POINTER(MeshBlock)),
                ("block_light", C.POINTER(C.c_uint8)),
                ("sky_light", C.POINTER(C.c_uint8)),
                ("borders", C.POINTER(MeshSample) * 6),
                ("edges_and_corners", C.POINTER(MeshSample))]


class MeshQuad(C.Structure):
    _fields_ = [("positions", (C.c_float * 3) * 4),
                ("state_id", C.c_uint32), ("tint", C.c_uint32),
                ("flow", C.c_float * 2), ("ao", C.c_uint8 * 4),
                ("block_light", C.c_uint8 * 4), ("sky_light", C.c_uint8 * 4),
                ("face_light", C.c_uint8), ("direction", C.c_uint8),
                ("layer", C.c_uint8), ("diagonal", C.c_uint8),
                ("fluid_kind", C.c_uint8), ("flags", C.c_uint8),
                ("reserved", C.c_uint8)]


assert C.sizeof(MeshBlock) == 16 and C.sizeof(MeshSample) == 20
assert C.sizeof(MeshQuad) == 84


def load_library(path: Path | None = None, build: bool = True):
    path = path or ROOT / "client-port/target/release/libmc_spine_client_mesh.so"
    if not path.exists() and build:
        (ROOT / "target/tmp").mkdir(parents=True, exist_ok=True)
        env = dict(os.environ, TMPDIR=str(ROOT / "target/tmp"))
        subprocess.run(["cargo", "build", "--release", "--offline", "--locked", "--lib"],
                       cwd=ROOT / "client-port", env=env, check=True)
    lib = C.CDLL(str(path.resolve()))
    lib.mc_spine_mesh_abi_version.argtypes = []
    lib.mc_spine_mesh_abi_version.restype = C.c_uint32
    lib.mc_spine_mesh_section.argtypes = [C.POINTER(MeshInput), C.POINTER(MeshQuad),
                                          C.c_size_t, C.POINTER(C.c_size_t)]
    lib.mc_spine_mesh_section.restype = C.c_int32
    if lib.mc_spine_mesh_abi_version() != ABI_VERSION:
        raise RuntimeError("unsupported client mesh ABI version")
    return lib


class BlockCatalog:
    """Build conservative cube/fluid metadata from the pinned 1.21.1 assets."""
    def __init__(self):
        blocks = json.loads((ROOT / "assets/blocks_1211.json").read_text())
        self.by_state = [None] * (max(b["maxStateId"] for b in blocks) + 1)
        facts = json.loads((ROOT / "assets/block_states_1211.json").read_text())
        sides, luminance, state_flags = facts["full_sides"], facts["luminance"], facts["flags"]
        for b in blocks:
            name = b["name"].removeprefix("minecraft:")
            for state in range(b["minStateId"], b["maxStateId"] + 1):
                flags, occ, fluid, level = 0, 0, NO_FLUID, 0
                if name in ("water", "bubble_column"):
                    fluid = WATER
                    level = max(0, min(15, state - b["minStateId"]))
                elif name == "lava":
                    fluid = LAVA
                    level = max(0, min(15, state - b["minStateId"]))
                elif state_flags[state] & 1:
                    pass
                elif name in ("glass", "tinted_glass") or "_stained_glass" in name:
                    flags = TRANSLUCENT | SAME_STATE_CULL
                elif name in ("torch", "wall_torch", "redstone_torch", "soul_torch",
                              "soul_wall_torch", "redstone_wall_torch"):
                    flags = CUTOUT
                elif int(sides[state]) == ALL_FACES:
                    flags, occ = OPAQUE, ALL_FACES
                self.by_state[state] = MeshBlock(state, 0, 0, flags, occ, fluid, level)
        self.emission = luminance

    def block(self, state: int) -> MeshBlock:
        if not 0 <= state < len(self.by_state) or self.by_state[state] is None:
            raise ValueError(f"unknown block state id {state}")
        return self.by_state[state]


def nibble(data: bytes, index: int) -> int:
    byte = data[index >> 1]
    return (byte >> (4 if index & 1 else 0)) & 15


def set_nibble(data: bytearray, index: int, value: int) -> None:
    off, shift = index >> 1, (4 if index & 1 else 0)
    data[off] = (data[off] & ~(15 << shift)) | ((value & 15) << shift)


def section_arrays(mask: int, rows: list[bytes], section_count: int):
    result = {}
    for bit in range(section_count + 2):
        if mask & (1 << bit):
            sec = bit - 1
            if 0 <= sec < section_count:
                result[sec] = rows[(mask & ((1 << bit) - 1)).bit_count()]
    return result


@dataclass
class Section:
    blocks: list[int]
    block_light: bytearray
    sky_light: bytearray


class WorldSections:
    def __init__(self, catalog: BlockCatalog):
        self.catalog = catalog
        self.sections: dict[tuple[int, int, int], Section] = {}
        self.dirty: set[tuple[int, int, int]] = set()

    def put_chunk(self, coord, blocks, masks, sky_rows, block_rows, section_count=24):
        sky = section_arrays(masks[0], sky_rows, section_count)
        block = section_arrays(masks[1], block_rows, section_count)
        cx, cz = coord
        for sy, states in enumerate(blocks):
            key = (cx, sy, cz)
            self.sections[key] = Section(states,
                bytearray(block.get(sy, bytes(LIGHT_BYTES))),
                bytearray(sky.get(sy, bytes(LIGHT_BYTES))))
            self.invalidate(key)
            self.invalidate_loaded_neighbors(key)

    def invalidate(self, key):
        self.dirty.add(key)

    def invalidate_loaded_neighbors(self, key):
        cx, sy, cz = key
        for dx, dy, dz in ((0,-1,0),(0,1,0),(0,0,-1),(0,0,1),(-1,0,0),(1,0,0)):
            neighbor=(cx+dx,sy+dy,cz+dz)
            if neighbor in self.sections:
                self.dirty.add(neighbor)

    def set_block(self, x, y, z, world_min_y, state):
        key = (x // 16, (y - world_min_y) // 16, z // 16)
        section = self.sections[key]
        i = ((y & 15) << 8) | ((z & 15) << 4) | (x & 15)
        section.blocks[i] = state
        self.invalidate(key)
        axes = [([0, -1] if x & 15 == 0 else [0, 1] if x & 15 == 15 else [0]),
                ([0, -1] if y & 15 == 0 else [0, 1] if y & 15 == 15 else [0]),
                ([0, -1] if z & 15 == 0 else [0, 1] if z & 15 == 15 else [0])]
        for dx in axes[0]:
            for dy in axes[1]:
                for dz in axes[2]:
                    neighbor=(key[0]+dx,key[1]+dy,key[2]+dz)
                    if (dx or dy or dz) and neighbor in self.sections:
                        self.dirty.add(neighbor)

    def set_light_section(self, cx, cz, section_y, block_data=None, sky_data=None):
        key = (cx, section_y, cz)
        section = self.sections.get(key)
        if section is None:
            return
        if block_data is not None: section.block_light[:] = block_data
        if sky_data is not None: section.sky_light[:] = sky_data
        self.invalidate(key)

    def border(self, key, direction):
        cx, sy, cz = key
        offsets = ((0,-1,0),(0,1,0),(0,0,-1),(0,0,1),(-1,0,0),(1,0,0))
        dx,dy,dz = offsets[direction]
        other = self.sections.get((cx+dx,sy+dy,cz+dz))
        if other is None:
            return None
        result = (MeshSample * FACE_CELLS)()
        for i in range(FACE_CELLS):
            if direction in (DIR_DOWN, DIR_UP):
                x,z=i%16,i//16; y=15 if direction==DIR_DOWN else 0
            elif direction in (DIR_NORTH, DIR_SOUTH):
                x,y=i%16,i//16; z=15 if direction==DIR_NORTH else 0
            else:
                z,y=i%16,i//16; x=15 if direction==DIR_WEST else 0
            idx=(y<<8)|(z<<4)|x
            result[i].block=self.catalog.block(other.blocks[idx])
            result[i].light=nibble(other.block_light,idx)|(nibble(other.sky_light,idx)<<4)
        return result

    def mesh(self, lib, key):
        section=self.sections[key]
        blocks=(MeshBlock*CELLS)(*(self.catalog.block(st) for st in section.blocks))
        bl=(C.c_uint8*LIGHT_BYTES).from_buffer(section.block_light)
        sl=(C.c_uint8*LIGHT_BYTES).from_buffer(section.sky_light)
        border_arrays=[self.border(key,d) for d in range(6)]
        inp=MeshInput(); inp.abi_version=ABI_VERSION; inp.missing=MISSING_AIR
        inp.blocks=blocks; inp.block_light=bl; inp.sky_light=sl
        for i,border in enumerate(border_arrays):
            if border is not None: inp.borders[i]=C.cast(border,C.POINTER(MeshSample))
        count=C.c_size_t()
        status=lib.mc_spine_mesh_section(C.byref(inp),None,0,C.byref(count))
        if status != ABI_OK: raise AssertionError(f"mesh count query status {status}")
        out=(MeshQuad*count.value)()
        status=lib.mc_spine_mesh_section(C.byref(inp),out,count.value,C.byref(count))
        if status != ABI_OK: raise AssertionError(f"mesh output status {status}")
        quads=list(out)
        for q in quads:
            assert all(0 <= int(v) <= 15 for v in (*q.block_light,*q.sky_light))
            assert all(0 <= int(v) <= 3 for v in q.ao)
        return quads


class MeshWireClient(Client):
    """Client subclass that retains decoded light with the inherited 0x27 decoder."""
    def __init__(self, game, name="MeshWire"):
        super().__init__(game,name)
        self.world=WorldSections(BlockCatalog())
        self.packet_ids=[]

    def read(self, timeout):
        pid,body=super().read(timeout)
        if self.phase=="play":
            self.packet_ids.append(pid)
            if pid==0x27:
                coord,states,masks,sky,block=decode_chunk_light(body,self.registry_counts["minecraft:worldgen/biome"],
                    section_count=self.section_count,has_skylight=self.has_skylight)
                self.world.put_chunk(coord,states,masks,sky,block,self.section_count)
            elif pid==0x2a:
                self._apply_light_update(body)
            elif pid==0x09:
                x,y,z=__import__("protocol767").unpack_position(body[:8])
                self.world.set_block(x,y,z,self.min_y,Reader(body[8:]).varint())
            elif pid==0x49:
                reader=Reader(body); packed=reader.number(">Q")
                sx=signed(packed>>42,22); sz=signed((packed>>20)&0x3fffff,22); sy=signed(packed&0xfffff,20)
                for _ in range(reader.varint()):
                    rec=reader.varlong(); x=sx*16+((rec>>8)&15); y=sy*16+(rec&15); z=sz*16+((rec>>4)&15)
                    self.world.set_block(x,y,z,self.min_y,rec>>12)
        return pid,body

    def _apply_light_update(self, body):
        r=Reader(body); cx=signed(r.varint(),32); cz=signed(r.varint(),32)
        masks=[]
        for _ in range(4):
            n=r.varint(); m=0
            for i in range(n): m |= r.number(">Q") << (64*i)
            masks.append(m)
        arrays=[]
        for mask in masks[:2]:
            n=r.varint(); rows=[]
            for _ in range(n):
                size=r.varint(); rows.append(r.take(size))
            arrays.append(section_arrays(mask,rows,self.section_count))
        sky_changed=set(arrays[0]) | {bit-1 for bit in range(self.section_count+2)
                                      if masks[2] & (1<<bit) and 0 < bit <= self.section_count}
        block_changed=set(arrays[1]) | {bit-1 for bit in range(self.section_count+2)
                                        if masks[3] & (1<<bit) and 0 < bit <= self.section_count}
        for sy in sky_changed | block_changed:
            sky=arrays[0].get(sy,bytes(LIGHT_BYTES) if sy in sky_changed else None)
            block=arrays[1].get(sy,bytes(LIGHT_BYTES) if sy in block_changed else None)
            self.world.set_light_section(cx,cz,sy,block,sky)
