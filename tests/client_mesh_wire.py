#!/usr/bin/env python3
"""Offline C ABI checks and loopback Protocol 767 to client-port E2E."""
import os
import math
import struct
import ctypes as C
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
os.environ["MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS"] = "1"
os.environ.setdefault("MC_PROBE_DEADLINE", "60")
os.environ.setdefault("TMPDIR", str(ROOT / "target/tmp"))

import sys
sys.path.insert(0, str(ROOT / "tools/client_mesh_wire"))
from mesh_bridge import (
    BlockCatalog, MeshWireClient, OPAQUE, WorldSections, load_library, nibble, section_arrays,
)

sys.path.insert(0, str(ROOT / "tests"))
from protocol767 import Client, WIRE_BLOCKS, local_server, position
from client_prediction_wire import chat_command
from wire_smoke import vint


def check(condition, label):
    if not condition:
        raise AssertionError(label)


def offline_contract(lib, catalog):
    world = WorldSections(catalog)
    left = [0] * 4096
    right = [0] * 4096
    left[(8 << 8) | (8 << 4) | 15] = 1
    right[(8 << 8) | (8 << 4)] = 1
    # Two 24-section chunks, with the test cells in matching sections.
    for cx, sections in ((0, [list([0] * 4096) for _ in range(24)]),
                         (1, [list([0] * 4096) for _ in range(24)])):
        sections[8] = left if cx == 0 else right
        world.put_chunk((cx, 0), sections, [0, 0, 0, 0], [], [], 24)
    a, b = (0, 8, 0), (1, 8, 0)
    qa = world.mesh(lib, a)
    qb = world.mesh(lib, b)
    check(len(qa) == 5 and len(qb) == 5, "stitched chunk border culls shared faces")
    check(all(q.state_id == 1 and q.layer == 0 for q in qa + qb), "C ABI block metadata")
    first, second = world.mesh(lib, a), world.mesh(lib, a)
    def quad_value(q):
        return (tuple(tuple(v for v in p) for p in q.positions), q.state_id, q.tint,
                tuple(q.flow), tuple(q.ao), tuple(q.block_light), tuple(q.sky_light),
                q.face_light, q.direction, q.layer, q.diagonal, q.fluid_kind, q.flags)
    check([quad_value(q) for q in first] == [quad_value(q) for q in second],
          "deterministic C ABI output")
    world.dirty.clear()
    world.set_block(15, -64 + 8 * 16 + 8, 8, -64, 0)
    check(a in world.dirty and b in world.dirty, "face edit invalidates both loaded sections")
    solid_world=WorldSections(catalog)
    cube=[1]*4096
    for sy in range(24):
        solid_world.sections[(0,sy,0)]=type(world.sections[a])(
            cube.copy() if sy==8 else [0]*4096,bytearray(2048),bytearray(2048))
    solid_world.dirty.add((0,8,0))
    faces=naive_unit_faces(solid_world,catalog,(0,8,0))
    merged=solid_world.mesh(lib,(0,8,0))
    check(faces==1536 and len(merged)<32 and faces//len(merged)>=48,
          f"dense cube reduction contract: faces={faces}, quads={len(merged)}")
    return {"stitched_pair_quads": len(qa) + len(qb), "dirty_sections": len(world.dirty),
            "dense_unit_faces":faces,"dense_greedy_quads":len(merged)}


def offline_light_update_contract(catalog):
    world=WorldSections(catalog)
    world.sections[(0,8,0)]=type("SectionFixture",(),{})()
    world.sections[(0,8,0)].blocks=[0]*4096
    world.sections[(0,8,0)].block_light=bytearray(2048)
    world.sections[(0,8,0)].sky_light=bytearray(2048)
    wire=object.__new__(MeshWireClient)
    wire.world=world
    wire.section_count=24
    bit=1<<9
    body=bytearray(vint(0)+vint(0))
    for mask in (bit,bit,0,0):
        body.extend(vint(1 if mask else 0))
        if mask: body.extend(mask.to_bytes(8,"big"))
    for value in (0x37,0xA5):
        body.extend(vint(1)+vint(2048)+bytes([value])*2048)
    wire._apply_light_update(bytes(body))
    section=world.sections[(0,8,0)]
    check(section.sky_light[0]==0x37 and section.block_light[0]==0xA5,
          "0x2a light arrays retain packed sky and block nibbles")
    reset=bytearray(vint(0)+vint(0))
    for mask in (0,0,bit,bit):
        reset.extend(vint(1 if mask else 0))
        if mask: reset.extend(mask.to_bytes(8,"big"))
    reset.extend(vint(0)+vint(0))
    wire._apply_light_update(bytes(reset))
    check(not any(section.sky_light) and not any(section.block_light),
          "0x2a empty masks clear both light channels")


def chunk_window(pos):
    cx, cz = math.floor(pos[0] / 16), math.floor(pos[2] / 16)
    return {(cx + dx, cz + dz) for dx in range(-2, 3) for dz in range(-2, 3)}


def light_level(client, pos, *, sky=False, snapshot=False):
    x, y, z = pos
    cx, sy, cz = x // 16, (y - client.min_y) // 16, z // 16
    index = ((y & 15) << 8) | ((z & 15) << 4) | (x & 15)
    if snapshot:
        masks, sky_rows, block_rows = client.chunk_light[(cx, cz)]
        mask, rows = (masks[0], sky_rows) if sky else (masks[1], block_rows)
        light = section_arrays(mask, rows, client.section_count).get(sy)
        return nibble(light, index) if light is not None else 0
    section = client.world.sections[(cx, sy, cz)]
    return nibble(section.sky_light if sky else section.block_light, index)


def wait_torch_update(client, torch, east, start_light, torch_state, levels, contract):
    owners = {(x // 16, z // 16) for x, _, z in (torch, east)}
    client.until(lambda *_: client.block(*torch) == torch_state
                 and owners <= set(client.light_updates[start_light:])
                 and (light_level(client, torch), light_level(client, east)) == levels,
                 contract)


def check_torch_snapshot(game, torch, east, levels):
    owners = {(x // 16, z // 16) for x, _, z in (torch, east)}
    with Client(game, "TorchSnapshot") as observer:
        observer.login(view=2)
        if not owners <= observer.chunk_light.keys():
            observer.until(lambda *_: owners <= observer.chunk_light.keys(), "fresh-torch-chunks")
        actual = tuple(light_level(observer, pos, snapshot=True) for pos in (torch, east))
        check(actual == levels, f"fresh 0x27 light {actual}, expected {levels}")


def naive_unit_faces(world, catalog, key):
    section = world.sections[key]
    cx, sy, cz = key
    count = 0
    for i, state in enumerate(section.blocks):
        block = catalog.block(state)
        if not block.flags & OPAQUE:
            continue
        x, z, y = i & 15, (i >> 4) & 15, i >> 8
        for dx, dy, dz in ((0,-1,0),(0,1,0),(0,0,-1),(0,0,1),(-1,0,0),(1,0,0)):
            nx, ny, nz = x+dx, y+dy, z+dz
            ncx, nsy, ncz = cx, sy, cz
            if nx < 0: ncx -= 1; nx = 15
            elif nx == 16: ncx += 1; nx = 0
            if ny < 0: nsy -= 1; ny = 15
            elif ny == 16: nsy += 1; ny = 0
            if nz < 0: ncz -= 1; nz = 15
            elif nz == 16: ncz += 1; nz = 0
            adjacent = world.sections.get((ncx,nsy,ncz))
            other = catalog.block(adjacent.blocks[(ny<<8)|(nz<<4)|nx]) if adjacent else catalog.block(0)
            direction={(0,-1,0):0,(0,1,0):1,(0,0,-1):2,(0,0,1):3,(-1,0,0):4,(1,0,0):5}[(dx,dy,dz)]
            if other.flags & OPAQUE and other.occlusion & (1 << (direction ^ 1)):
                continue
            count += 1
    return count


def best_live_reduction(client, lib):
    candidates=[]
    for key, section in client.world.sections.items():
        if not any(section.blocks):
            continue
        faces=naive_unit_faces(client.world,client.world.catalog,key)
        if faces < 100:
            continue
        quads=client.world.mesh(lib,key)
        if quads:
            candidates.append((faces/len(quads),faces,len(quads),key))
    check(candidates, "non-empty live ground/surface/underground mesh section")
    return max(candidates)


def live_wire_e2e(lib):
    binary=ROOT/"target/debug/mc-spine"
    check(binary.is_file(), "built target/debug/mc-spine")
    with local_server(binary) as game:
        with MeshWireClient(game) as client:
            client.login(view=2)
            client.until(lambda *_: client.pos is not None, "initial-position-sync")
            expected=chunk_window(client.pos)
            client.until_chunks(expected, "initial-overworld-25-chunks")
            check(client.dimension_name=="minecraft:overworld" and client.section_count==24,
                  "Overworld 24-section layout")
            check(len(client.world.sections)==len(client.chunks)*24, "decoded block and light section cache")
            score=best_live_reduction(client,lib)
            reduction,naive,quads,key=score
            check(reduction>=2.0, f"greedy reduction {naive} unit faces to {quads} quads")

            # Place at the east edge of a held chunk to exercise propagation across it.
            x=math.floor(client.pos[0]/16)*16+15; z=math.floor(client.pos[2])+5
            y=max(client.min_y+2,min(client.max_y-3,math.floor(client.pos[1])-3))
            target=(x,y,z)
            start_updates=len(client.updates)
            chat_command(client,f"setblock {x} {y} {z} minecraft:stone")
            client.until(lambda *_: len(client.updates)>start_updates and client.block(*target)==1,
                         "live-stone-0x09-or-0x49")
            stone_key=(x//16,(y-client.min_y)//16,z//16)
            stone_quads=client.world.mesh(lib,stone_key)
            check(stone_quads, "stone update remeshed section")

            torch = (x, y+1, z)
            east = (x+1, y+1, z)
            torch_key = (x//16, (y+1-client.min_y)//16, z//16)
            for pos in (torch, east):
                if client.block(*pos) != 0:
                    chat_command(client, f"setblock {pos[0]} {pos[1]} {pos[2]} minecraft:air")
                    client.until(lambda *_: client.block(*pos) == 0, "clear-torch-light-path")
            light_before = light_level(client, torch)
            east_before = light_level(client, east)
            start_light = len(client.light_updates)
            chat_command(client, f"setblock {x} {y+1} {z} minecraft:torch")
            wait_torch_update(client, torch, east, start_light, 2355, (14, 13),
                              "live-torch-0x2a-reaches-both-chunks")
            light_packets = len(client.light_updates) - start_light
            check(light_packets > 0, "torch placement sends Update Light")
            torch_quads = [q for q in client.world.mesh(lib, torch_key)
                           if q.layer == 1 and q.state_id == 2355]
            check(torch_quads, "torch cutout reaches mesh")
            max_light = max(max(q.block_light) for q in torch_quads)
            check(max_light > light_before, "greedy mesher receives live torch light")

            check_torch_snapshot(game, torch, east, (14, 13))

            start_light = len(client.light_updates)
            chat_command(client, f"setblock {x} {y+1} {z} minecraft:air")
            dark = (light_before, east_before)
            wait_torch_update(client, torch, east, start_light, 0, dark,
                              "setblock-torch-removal-clears-both-chunks")

            # Exercise the placement and dig handlers with the same lit and dark cells.
            chat_command(client, "gamemode creative")
            near_torch = (x - 1.5, float(y + 2), z + 0.5)
            chat_command(client, f"tp {near_torch[0]} {near_torch[1]} {near_torch[2]}")
            client.until(lambda *_: client.pos == near_torch, "player-in-placement-range")
            torch_item = next(block["drops"][0] for block in WIRE_BLOCKS if block["name"] == "torch")
            client.send(0x32, struct.pack(">h", 36) + vint(1) + vint(torch_item) + vint(0) + vint(0))
            start_light = len(client.light_updates)
            client.send(0x38, vint(0) + position(*target) + vint(1)
                        + struct.pack(">fff?", 0.5, 1.0, 0.5, False) + vint(1))
            wait_torch_update(client, torch, east, start_light, 2355, (14, 13),
                              "player-torch-placement-lights-both-chunks")
            start_light = len(client.light_updates)
            client.send(0x24, vint(0) + position(*torch) + b"\x01" + vint(2))
            wait_torch_update(client, torch, east, start_light, 0, dark,
                              "player-torch-break-clears-both-chunks")
            check_torch_snapshot(game, torch, east, dark)

            # An opaque block high in open air lowers the sky below it by one.
            roof = (x, y + 32, z)
            below_roof = (x, y + 31, z)
            check(light_level(client, below_roof, sky=True) == 15, "sky fixture starts at full light")
            for state, sky_level in (("stone", 14), ("air", 15)):
                start_light = len(client.light_updates)
                chat_command(client, f"setblock {roof[0]} {roof[1]} {roof[2]} minecraft:{state}")
                client.until(lambda *_: (x//16, z//16) in client.light_updates[start_light:]
                             and light_level(client, below_roof, sky=True) == sky_level,
                             f"live-sky-light-{state}")

            # Glass must not hide adjacent opaque stone; water must use the fluid layer.
            start_updates=len(client.updates)
            chat_command(client,f"setblock {x-1} {y} {z} minecraft:glass")
            client.until(lambda *_: len(client.updates)>start_updates and client.block(x-1,y,z)==519,
                         "live-glass-block-update")
            glass_mesh=client.world.mesh(lib,stone_key)
            check(any(q.state_id==1 for q in glass_mesh) and any(q.state_id==519 and q.layer==2 for q in glass_mesh),
                  "glass remains translucent and stone faces stay visible")
            start_updates=len(client.updates)
            chat_command(client,f"setblock {x-2} {y} {z} minecraft:water")
            client.until(lambda *_: len(client.updates)>start_updates and client.block(x-2,y,z)==80,
                         "live-water-block-update")
            water_mesh=client.world.mesh(lib,stone_key)
            check(any(q.state_id==80 and q.layer==3 for q in water_mesh), "water update reaches fluid mesh")

            # Break the first stone and require regenerated neighboring geometry.
            start_updates=len(client.updates)
            before_count=len(stone_quads)
            chat_command(client,f"setblock {x} {y} {z} minecraft:air")
            client.until(lambda *_: len(client.updates)>start_updates and client.block(*target)==0,
                         "live-break-0x09-or-0x49")
            after=client.world.mesh(lib,stone_key)
            check(len(after)!=before_count or any(q.state_id==1 for q in after),
                  "stone break regenerates affected geometry")
            return {"chunks":len(client.chunks),"sections":len(client.world.sections),
                    "quad_reduction":f"{naive}->{quads} ({reduction:.2f}x)",
                    "section":key,"stone_quads":len(stone_quads),"break_quads":len(after),
                    "torch_mesh_block_light":max_light,"pre_edit_block_light":light_before,
                    "update_light_packets_0x2a":light_packets,
                    "updates_0x09_or_0x49":len(client.updates),"ffi_errors":0}


def main():
    (ROOT/"target/tmp").mkdir(parents=True,exist_ok=True)
    lib=load_library()
    catalog=BlockCatalog()
    offline=offline_contract(lib,catalog)
    offline_light_update_contract(catalog)
    live=live_wire_e2e(lib)
    print("client_mesh_wire: PASS")
    print(f"offline: {offline}")
    print(f"live: {live}")


if __name__=="__main__":
    main()
