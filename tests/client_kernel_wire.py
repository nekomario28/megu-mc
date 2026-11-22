#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
"""Build and test the Protocol 767 client kernel against a disposable server.

Run with python3 tests/client_kernel_wire.py from the checkout. Requires the
offline Cargo cache and a C11 compiler for the public-header layout probe.
The observer checks the server's accepted and rebroadcast player movement.
"""
import ctypes as C
import gzip
import hashlib
import math
import os
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

sys.dont_write_bytecode = True

from client_prediction_wire import (
    WATER_SOURCE,
    WATER_STATES,
    WATER_VERTICAL_DRAG,
    PredictionClient,
    chat_command,
    chunk_window,
    set_creative_stone,
)
from protocol767 import (
    AIR_STATES,
    ROOT,
    STATE_COUNT,
    ContractFailure,
    Reader,
    local_server,
    position,
    require,
    vint,
)

sys.path.insert(0, str(ROOT))
from tools.client_kernel_wire import abi
from tools.client_kernel_wire.driver import BLOCKS, STONE, KernelClient, Movement


def verify_abi(kernel, include_dir, temporary_dir):
    incompatible = temporary_dir / "abi-v2.so"
    abi.compile_c(include_dir, incompatible,
                  '#include "megu_kernel.h"\n'
                  'uint32_t megu_kernel_abi_version(void) { return 2; }\n',
                  "-shared", "-fPIC")
    try:
        abi.Kernel(incompatible)
    except ContractFailure as error:
        require(error.code.startswith("c-abi-version:"), "incompatible-library-version")
    else:
        require(False, "accepted-incompatible-library-version")

    with kernel.owned("palette_new_single", "palette_free", STONE) as section:
        sentinel = 0xFFFFFFFF
        states = (abi.U32 * abi.SECTION_CELLS)(*([sentinel] * abi.SECTION_CELLS))
        kernel.call("palette_unpack_into", section, states, len(states) - 1,
                    status=abi.BUFFER_TOO_SMALL)
        require(all(state == sentinel for state in states), "short-unpack-wrote-output")
        value = abi.U32(sentinel)
        kernel.call("palette_get", section, len(states), C.byref(value), status=abi.OUT_OF_RANGE)
        require(value.value == sentinel, "out-of-range-get-wrote-output")
        kernel.call("palette_set", section, 0, STATE_COUNT, status=abi.INVALID_STATE)
        kernel.call("palette_get", section, 0, C.byref(value))
        require(value.value == STONE, "invalid-state-changed-section")


def verify_nbt(kernel):
    # A complete named level compound with a long Seed tag.
    named = b"\x0a\x00\x05level\x04\x00\x04Seed" + struct.pack(">q", 4242) + b"\x00"
    kernel.call("nbt_read_named_validate", named, len(named))
    require(kernel.bytes("nbt_roundtrip_named", named, len(named)) == named, "named-nbt-bytes")
    invalid_named = (named[:-1], b"\x7f" + named[1:], named + b"\x00")
    for payload in invalid_named:
        kernel.call("nbt_read_named_validate", payload, len(payload), status=abi.INVALID_NBT)
        output = abi.Bytes(None, 42)
        kernel.call("nbt_roundtrip_named", payload, len(payload), C.byref(output),
                    status=abi.INVALID_NBT)
        require(not output.data and output.len == 42, "invalid-nbt-wrote-output")

    gzipped = gzip.compress(named, mtime=0)
    kernel.call("nbt_read_gzip_validate", gzipped, len(gzipped), len(named))
    kernel.call("nbt_read_gzip_validate", gzipped, len(gzipped), len(named) - 1,
                status=abi.BYTE_BUDGET)
    checksum_bad = gzipped[:-8] + bytes([gzipped[-8] ^ 1]) + gzipped[-7:]
    for payload in (gzipped[:-1], checksum_bad, gzipped + b"\x00"):
        kernel.call("nbt_read_gzip_validate", payload, len(payload), len(named),
                    status=abi.INVALID_GZIP)


def verify_commands(kernel):
    # Register out of lexical order, with a case-insensitive sort tie.
    with kernel.owned("dispatcher_new", "dispatcher_free") as dispatcher:
        for literal in (b"setblock", b"time", b"SetBlock", b"say", b"save-all"):
            kernel.call("dispatcher_register_literal", dispatcher, literal, len(literal))
        for command, executable in ((b"setblock", 1), (b"unknown", 0)):
            result = abi.CommandParse()
            kernel.call("dispatcher_parse", dispatcher, command, len(command), C.byref(result))
            require((result.cursor, result.executable) == (len(command) if executable else 0,
                                                            executable), "brigadier-parse")
        query = b"s ignored"
        with kernel.owned("dispatcher_complete_suggestions", "suggestions_free",
                          dispatcher, query, len(query), 1) as suggestions:
            info = abi.SuggestionInfo()
            kernel.call("suggestions_info", suggestions, C.byref(info))
            require((info.start, info.end, info.count) == (0, 1, 4), "brigadier-replacement-range")
            texts = []
            for index in range(info.count):
                text = abi.Text()
                kernel.call("suggestions_get", suggestions, index, C.byref(text))
                texts.append(C.string_at(text.data, text.len))
            require(texts == [b"save-all", b"say", b"setblock", b"SetBlock"],
                    f"brigadier-completion-order:{texts}")


def login(client):
    client.login(view=2)
    if client.pos is None:
        client.until_id(0x40, "initial-position-sync")
    require(client.entity_id is not None, "initial-join")
    require(client.dimension_name == "minecraft:overworld", "arena-dimension")


def wait_block(client, cell, state, packet, label):
    start = len(client.updates)

    def updated(pid, _):
        nonlocal start
        matching_update = (cell, state) in client.updates[start:]
        start = len(client.updates)
        return pid == packet and matching_update and client.block(*cell) == state

    client.until(updated, label)


def setblock(client, cell, name):
    x, y, z = cell
    chat_command(client, f"setblock {x} {y} {z} minecraft:{name}")
    wait_block(client, cell, BLOCKS[name], 0x49, f"setblock-0x49:{cell}:{name}")


def use_block(client, cell, sequence):
    client.send(0x38, vint(0) + position(*cell) + vint(1)
                + struct.pack(">fff?", 0.5, 1.0, 0.5, False) + vint(sequence))


def snapshot(actor, game, name, cell):
    # A new client gets independently serialized 0x27 bytes after the edit.
    with PredictionClient(game, name) as reference:
        login(reference)
        coord = (cell[0] // 16, cell[2] // 16)
        if coord not in reference.chunks:
            reference.until(lambda *_: coord in reference.chunks, f"snapshot-chunk:{name}")
        actor.check_snapshot(reference, cell)


def exercise_barrel(actor, game, cell):
    # This server sends block edits in 0x49, and barrel opener changes in 0x09.
    setblock(actor, cell, "barrel")
    closed = BLOCKS["barrel"]
    # For this barrel's facing, the open=true state precedes open=false.
    opened = closed - 1
    use_block(actor, cell, 80)
    window = Reader(actor.until_id(0x33, "barrel-open-window")).varint()
    wait_block(actor, cell, opened, 0x09, "barrel-open-0x09")
    snapshot(actor, game, "OpenBarrel", cell)
    actor.send(0x0F, vint(window))
    wait_block(actor, cell, closed, 0x09, "barrel-close-0x09")
    snapshot(actor, game, "ClosedBarrel", cell)
    setblock(actor, cell, "air")


def sprint(actor, observer, enabled):
    entity_id = actor.entity_id
    actor.send(0x25, vint(entity_id) + vint(3 if enabled else 4) + vint(0))

    def flags(pid, body):
        if pid != 0x58:
            return False
        reader = Reader(body)
        if reader.varint() != entity_id or reader.number("B") != 0:
            return False
        require(reader.varint() == 0, "player-flags-metadata-type")
        require(bool(reader.number("B") & 0x08) == enabled, "server-sprint-flag")
        return True

    observer.until(flags, "server-sprint-metadata-0x58")


def exercise_dry(move, game, x, y, z):
    actor = move.actor
    wall = (x + 2, y, z)
    set_creative_stone(actor)
    use_block(actor, (wall[0], y - 1, z), 81)
    wait_block(actor, wall, STONE, 0x49, "place-stone-0x49")
    snapshot(actor, game, "PlacedStone", wall)

    for _ in range(6):
        result = move.step((0.215, -0.08, 0.0))
        require(move.grounded, "walk-grounded")
    require(result.flags & abi.HORIZONTAL and move.coords[0] == wall[0] - 0.3,
            "walk-stops-at-placed-wall")

    actor.send(0x24, vint(0) + position(*wall) + bytes([1]) + vint(82))
    wait_block(actor, wall, BLOCKS["air"], 0x49, "break-stone-0x49")
    snapshot(actor, game, "BrokenStone", wall)

    sprint(actor, move.observer, True)
    for _ in range(6):
        result = move.step((0.28, -0.08, 0.0))
        require(move.grounded and result.delta[0] == 0.28, "sprint-unobstructed")
    sprint(actor, move.observer, False)
    require(move.coords[0] > wall[0] + 1, "sprint-crosses-removed-wall")

    height, velocity, apex = move.coords[1], C.c_float(0.42).value, move.coords[1]
    for tick in range(16):
        move.step((0.0, velocity, 0.0))
        apex = max(apex, move.coords[1])
        if move.grounded:
            break
        velocity = (velocity - 0.08) * 0.98
    require(1.0 < apex - height < 1.5 and move.grounded and move.coords[1] == height,
            "jump-arc-rise-and-land")

    slab = (x + 4, y, z)
    setblock(actor, slab, "stone_slab")
    snapshot(actor, game, "BottomSlab", slab)
    delta = (0.22, -0.08, 0.0)
    for _ in range(3):
        without_step = move.collision(delta, step_height=0.0)
        stepped = move.step(delta).flags & abi.STEPPED_UP
        if stepped:
            require(without_step.flags & abi.HORIZONTAL and without_step.delta[1] == 0.0,
                    "slab-blocks-without-step-up")
            break
    require(stepped and move.coords[1] == y + 0.5, "slab-step-height")
    return tick + 1


def exercise_water(move, x, y, z):
    actor = move.actor
    # One source feeds an enclosed, one-cell-wide channel to the east.
    chat_command(actor, f"fill {x} {y - 1} {z - 1} {x + 6} {y} {z + 1} minecraft:stone")
    wait_block(actor, (x + 6, y, z), STONE, 0x49, "water-channel-shell")
    chat_command(actor, f"fill {x + 1} {y} {z} {x + 5} {y} {z} minecraft:air")
    wait_block(actor, (x + 5, y, z), BLOCKS["air"], 0x49, "water-channel-interior")
    setblock(actor, (x + 1, y, z), "water")
    flow_cell = (x + 2, y, z)
    actor.until(lambda *_: actor.block(*flow_cell) in WATER_STATES
                and actor.block(x + 3, y, z) in WATER_STATES, "water-level-gradient")
    states = [actor.block(x + offset, y, z) for offset in (1, 2, 3)]
    require(states[0] == WATER_SOURCE and states[0] < states[1] < states[2] < WATER_SOURCE + 8,
            f"water-gradient-states:{states}")

    start = (x + 2.5, y + 0.1, z + 0.5)
    move.send(start, False)
    move.coords, move.grounded = start, False
    velocity = (0.0, 0.0, 0.0)
    for _ in range(8):
        fluid = move.water(velocity)
        require(fluid.in_fluid == 1 and fluid.max_height > 0.0, "flow-immerses-player")
        require(fluid.push[0] > 0.0 and fluid.push[1] == fluid.push[2] == 0.0,
                "water-pushes-east")
        delta = tuple(v + push for v, push in zip(velocity, fluid.push))
        move.step(delta)
        velocity = (delta[0] * WATER_VERTICAL_DRAG,
                    (0.0 if move.grounded else delta[1]) * WATER_VERTICAL_DRAG - 0.08 / 16,
                    delta[2] * WATER_VERTICAL_DRAG)
    require(move.coords[0] > start[0] and move.coords[1] < start[1], "water-push-and-sink")


def exercise_wire(game, kernel):
    with KernelClient(game, "KernelActor", kernel) as actor, \
            PredictionClient(game, "KernelObserver") as observer:
        login(actor)
        window = chunk_window(actor.pos)
        actor.until_chunks(window, "actor-initial-chunk-window")
        require(actor.initial_sections == len(window) * actor.section_count, "initial-section-count")
        require(0 in actor.wire_widths and any(4 <= bits <= 8 for bits in actor.wire_widths),
                "live-single-and-indirect-palettes")
        login(observer)
        observer.until_chunks(window, "observer-initial-chunk-window")

        spawn = tuple(map(math.floor, actor.pos))
        column = [(spawn[0], y, spawn[2]) for y in range(spawn[1] - 12, spawn[1] + 3)]
        states = [actor.block(*cell) for cell in column]
        require(states == [observer.block(*cell) for cell in column], "spawn-column-from-two-clients")
        require(any(st in AIR_STATES for st in states) and any(st not in AIR_STATES for st in states),
                f"spawn-column-terrain-and-air:{spawn}")

        x, y, z = spawn[0] // 16 * 16 + 2, 241, spawn[2] // 16 * 16 + 4
        require(actor.block(x, y, z) == BLOCKS["air"], "empty-arena-from-0x27")
        chat_command(actor, f"fill {x - 1} {y - 1} {z - 1} {x + 6} {y - 1} {z + 1} minecraft:stone")
        wait_block(actor, (x, y - 1, z), STONE, 0x49, "arena-floor")
        observer.move(x + 0.5, y, z + 0.5)
        actor.move(x + 0.5, y, z + 0.5)
        observer.until(lambda *_: actor.entity_id in observer.tracked, "observer-tracks-actor")
        # An acknowledged sprint stop also drains any setup movement broadcasts.
        sprint(actor, observer, False)
        move = Movement(actor, observer, (x + 0.5, y, z + 0.5))

        exercise_barrel(actor, game, (x, y, z - 1))
        jump_ticks = exercise_dry(move, game, x, y, z)
        exercise_water(move, x + 5, y, z + 5)

        # A command response on each socket is a barrier after movement.
        for client in (actor, observer):
            chat_command(client, "time query daytime")
            client.until(
                lambda pid, _, client=client: pid == 0x6C
                and client.messages[-1].startswith("The time is "),
                "post-movement-roundtrip",
            )
        unexpected = len(actor.position_syncs) + len(observer.position_syncs) - 2
        require(unexpected == 0, f"unexpected-0x40-resyncs:{unexpected}")
        require(actor.update_counts[0x09] == 2, "two-live-barrel-state-updates")
        print(f"protocol=767 chunks={len(actor.palettes)} sections={actor.initial_sections} "
              f"palette_widths={sorted(actor.wire_widths)} live_0x09={actor.update_counts[0x09]} "
              f"live_0x49_cells={actor.update_counts[0x49]}")
        print(f"walk/sprint/jump/slab/flow: {move.samples} server samples, jump_ticks={jump_ticks}, "
              f"position_drift={move.max_drift:.6f}, unexpected_0x40={unexpected}")


def main():
    tmp = ROOT / "target/tmp"
    tmp.mkdir(parents=True, exist_ok=True)
    os.environ.update(TMPDIR=str(tmp), MEGU_WG="old", MEGU_SEED="4242",
                      MEGU_ALLOW_PLAYER_ADMIN_COMMANDS="1")
    for args, target in (([], ROOT / "target"),
                         (["--manifest-path", "client-kernel/Cargo.toml", "--release"],
                          ROOT / "client-kernel/target")):
        subprocess.run(["cargo", "build", "--offline", "--locked", *args], cwd=ROOT, check=True,
                       env=dict(os.environ, CARGO_TARGET_DIR=str(target)))
    library = ROOT / "client-kernel/target/release/libmegu_kernel.so"
    server = ROOT / "target/debug/megu"
    for artifact in (library, server):
        with artifact.open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        print(f"{artifact.relative_to(ROOT)} sha256={digest} mtime_ns={artifact.stat().st_mtime_ns}")
    kernel = abi.Kernel(library)
    with tempfile.TemporaryDirectory(dir=tmp, prefix="client-kernel-wire-") as directory:
        root = Path(directory)
        include_dir = ROOT / "client-kernel/include"
        abi.verify_layout(include_dir, root)
        verify_abi(kernel, include_dir, root)
        verify_nbt(kernel)
        verify_commands(kernel)
        print("ABI v1 layout/version/capacity, named/gzip NBT, and Brigadier: PASS")
        with local_server(server, root) as game:
            exercise_wire(game, kernel)
    print("client kernel wire: PASS")


if __name__ == "__main__":
    try:
        main()
    except AssertionError as error:
        raise SystemExit(f"client kernel wire: FAIL ({error})") from error
