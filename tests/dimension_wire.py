#!/usr/bin/env python3
"""Cross-dimension client flow on the real wire (P2-3-S4).

Four claims, each observed from the bytes a client actually receives:

1. A portal moves a player with 0x47 Respawn, and the following chunks are decoded as a
   16-section Nether window (the fixture asserts skyLightMask == 0 while decoding).
2. A Nether block edit does not leak into an Overworld client sitting at the same (cx, cz).
3. Nether -> End -> Overworld all round-trip through 0x47 with the right registry id.
4. A player who disconnects in the Nether resumes in the Nether after a restart in the same
   root: 0x2b Join Game names minecraft:the_nether and the first window is 16-section.
"""
import math
import os
from pathlib import Path
import socket
import sys
import tempfile
import time

from protocol767 import Client, ContractFailure, local_server, parse_respawn, require, string

# The wire fixture drives the server as a player. This opt-in exists for local
# compatibility testing, and the commands below are exactly the cross-dimension ones S4 adds.
os.environ["MEGU_ALLOW_PLAYER_ADMIN_COMMANDS"] = "1"
os.environ.setdefault("MC_PROBE_DEADLINE", "30")


def square(cx, cz, view=2):
    # Vanilla 1.21.1 `ChunkTrackingView.Positioned`: the tracked window is a
    # rounded square — a (2d+3)² bounding box with the four far corners cut,
    # so d=2 delivers 45 chunks (rows 5,7,7,7,7,7,5), not a Chebyshev 25.
    out = set()
    for dx in range(-(view + 1), view + 2):
        for dz in range(-(view + 1), view + 2):
            ax, az = max(0, abs(dx) - 1), max(0, abs(dz) - 1)
            lo, hi = min(ax, az), max(0, max(ax, az) - 1)
            if lo * lo + hi * hi < view * view:
                out.add((cx + dx, cz + dz))
    return out


def block_of(pos):
    return (math.floor(pos[0]), math.floor(pos[1]), math.floor(pos[2]))


def chunk_of(pos):
    return (math.floor(pos[0] / 16), math.floor(pos[2] / 16))


def await_respawn(client, dim_name, center=None):
    """Consume the 0x47, the position sync and the new dimension's whole window."""
    body = client.until(lambda pid, _: pid == 0x47, "respawn-packet")
    dim_type, name = parse_respawn(body)
    require(name == dim_name, f"respawn-dimension:{name}")
    client.until_id(0x40)
    require(client.dimension_name == dim_name, f"client-dimension:{client.dimension_name}")
    require(client.section_count == 16 or dim_name == "minecraft:overworld",
            "respawn-section-count")
    target = client.pos if client.pos is not None else center
    client.until_chunks(square(*chunk_of(target)))
    return dim_type


def command(client, text):
    # Serverbound chat_command (0x04): plain chat is relayed, not executed.
    client.send(0x04, string(text.lstrip("/")))
    time.sleep(0.15)  # chat throttle drains 1 score / 50 ms; keep well under the kick line


def drain(client, seconds=0.6):
    """Read everything already in flight without failing on the quiet timeout."""
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            client.read(min(0.2, deadline - time.monotonic()))
        except (socket.timeout, TimeoutError):
            pass


def exercise(binary, root):
    with local_server(binary, root) as game:
        traveller = Client(game, "DimTraveller")
        try:
            with traveller:
                traveller.login()
                if traveller.pos is None:
                    traveller.until_id(0x40)
                require(traveller.dimension_name == "minecraft:overworld", "login-overworld")
                spawn = traveller.pos
                traveller.until_chunks(square(*chunk_of(spawn)))

                # ---- (1) Overworld -> Nether through a real nether_portal block ----
                command(traveller, "gamemode creative")
                bx, by, bz = block_of(spawn)
                command(traveller, f"setblock {bx} {by} {bz} minecraft:nether_portal")
                traveller.move(*spawn)
                nether_type = await_respawn(
                    traveller, "minecraft:the_nether", (bx / 8, by, bz / 8))
                require(nether_type == 3, "nether-wire-id")
                require(not traveller.has_skylight, "nether-has-skylight")
                require(traveller.section_count == 16, "nether-section-count")
                nether_pos = traveller.pos
                require(nether_pos is not None, "nether-position-sync")

                # ---- (2) a Nether edit must not reach an Overworld client at the same coords ----
                observer = Client(game, "OverworldObs")
                with observer:
                    observer.login()
                    if observer.pos is None:
                        observer.until_id(0x40)
                    require(observer.dimension_name == "minecraft:overworld",
                            "observer-overworld")
                    observer.until_chunks(square(*chunk_of(observer.pos)))
                    before = len(observer.updates)
                    target = (2, 40, 2)
                    command(traveller, f"setblock {target[0]} {target[1]} {target[2]} "
                                       "minecraft:stone")
                    drain(observer)
                    # Random-tick corrections (burnout, decay) stream ambient
                    # same-dimension block updates even to an idle single client,
                    # so quiescence cannot be asserted; dimension isolation means
                    # the Nether edit's position never reaches this client.
                    leaked = observer.updates[before:]
                    require(not any(pos == target for pos, _ in leaked),
                            "nether-edit-leaked-to-overworld")
                    require(((target), 1) not in observer.updates,
                            "nether-stone-visible-in-overworld")

                # ---- (3a) Nether -> Overworld with cross-dimension /tp ----
                command(traveller, "execute in overworld run tp @s 100.5 50.0 0.5")
                await_respawn(traveller, "minecraft:overworld", (100.5, 50.0, 0.5))

                # ---- (3b) Overworld -> End through an end_portal block ----
                command(traveller, "setblock 100 50 0 minecraft:end_portal")
                traveller.move(100.5, 50.0, 0.5)
                end_type = await_respawn(traveller, "minecraft:the_end", (100.5, 49.0, 0.5))
                require(end_type == 2, "end-wire-id")
                require(traveller.section_count == 16 and not traveller.has_skylight,
                        "end-window")

                # ---- (3c) End -> Overworld through the return portal ----
                # Arrival holds the per-player portal guard; one move on the (air) spawn
                # platform clears it before the return portal is materialised.
                traveller.move(100.5, 49.0, 0.5)
                drain(traveller, 0.2)
                command(traveller, "setblock 100 49 0 minecraft:end_portal")
                traveller.move(100.5, 49.0, 0.5)
                await_respawn(traveller, "minecraft:overworld", spawn)

                # ---- (4) finish in the Nether at known coordinates, then disconnect ----
                saved = (32.5, 40.0, 48.5)
                command(traveller, "execute in the_nether run tp @s "
                                   f"{saved[0]} {saved[1]} {saved[2]}")
                await_respawn(traveller, "minecraft:the_nether", saved)
                saved_pos = traveller.pos
                require(saved_pos is not None, "saved-position-sync")
        except ContractFailure as error:
            if traveller.contract_started:
                print(f"dimension wire: FAIL {error.code}", file=sys.stderr)
            raise

    # A second server in the same root must load the save written on the first's shutdown.
    with local_server(binary, root) as game:
        returning = Client(game, "DimTraveller")
        with returning:
            returning.login()
            if returning.pos is None:
                returning.until_id(0x40)
            require(returning.dimension_name == "minecraft:the_nether",
                    "login-dimension-restored")
            require(returning.section_count == 16 and not returning.has_skylight,
                    "login-nether-window")
            require(returning.pos is not None, "login-position-sync")
            for got, want in zip(returning.pos, saved_pos):
                require(abs(got - want) < 0.01, "login-saved-coords")
            returning.until_chunks(square(*chunk_of(saved_pos)))
            # The chunk bodies are real Nether chunks: 16 sections and a zero sky mask,
            # enforced by the fixture decoder before a chunk is accepted into `chunks`.
            require(all(masks[0] == 0 for masks, _, _ in returning.chunk_light.values()),
                    "login-nether-sky-mask")


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="spine-dimension-") as root:
        exercise(Path(sys.argv[1]).resolve(), Path(root))
    print("dimension wire: PASS")
