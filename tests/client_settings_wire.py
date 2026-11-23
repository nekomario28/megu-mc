#!/usr/bin/env python3
"""Drive production configuration/play paths and assert decoded chunk sets."""
import os
from pathlib import Path
import sys

# The 17x17 view-distance-127 window is ~290 paced chunks; give the bounded
# waits room on loaded hosts (verify.sh exports 30 already).
os.environ.setdefault("MC_PROBE_DEADLINE", "45")

from protocol767 import Client, ContractFailure, local_server, require, settings


def square(radius, cx=0, cz=0):
    return {(cx + x, cz + z) for x in range(-radius, radius + 1) for z in range(-radius, radius + 1)}


def disc(radius, cx=0, cz=0):
    """Chunks the server's ChunkWindow.delivers for a negotiated view radius:
    a (radius+1) bounding square filtered by the circular contains() rule
    `min(dx,dz)^2 + max(|dx|,|dz| - 1, 0)^2 < radius^2`."""
    cells = set()
    for dx in range(-radius - 1, radius + 2):
        for dz in range(-radius - 1, radius + 2):
            ax, az = abs(dx), abs(dz)
            lo = min(ax, az)
            hi = max(ax, az) - 1
            hi = max(hi, 0)
            if lo * lo + hi * hi < radius * radius:
                cells.add((cx + dx, cz + dz))
    return cells


def session(binary, late):
    with local_server(binary) as game, Client(game) as client:
        try:
            client.login(late=late)
            # Login leads with the 3x3 ring and steady-state delivery overshoots
            # the requested view by vanilla's +1 bounding ring: the contract is
            # containment of the requested square, not set equality.
            client.until(lambda *_: square(2) <= set(client.chunks),
                         f"config-{'late' if late else 'early'}-window")
            if not late:
                # a request that grows the window is answered by chunks, paced by
                # this client's acknowledgements; a request that shrinks it is
                # answered by the centre update alone
                for requested, radius, barrier in [(3, 3, None), (2, 2, 0x54),
                                                   (127, 8, None), (-1, 2, 0x54)]:
                    client.send(0x0a, settings(requested))
                    # The clamped 17x17 request fills a circular window: its
                    # square corners sit outside the disc and never arrive.
                    want = disc(radius) if requested == 127 else square(radius)
                    if barrier is None:
                        before = len(client.chunk_batches)
                        client.until(lambda *_: want <= set(client.chunks),
                                     f"play-view-{requested}")
                        # the requested rate (30 chunks/tick) cannot carry the 17x17
                        # window in one batch, so the acknowledgement gated at least
                        # one more: that is the pacing this asserts
                        if requested == 127:
                            require(len(client.chunk_batches) - before >= 2,
                                    "chunk-batch-ack-pacing")
                    else:
                        client.until_id(barrier, f"play-view-{requested}")
                    require(want <= set(client.chunks), f"play-window-{requested}")
                client.move(-24.0, 16.0, 24.0)
                client.until_id(0x0c)
                client.until(lambda *_: square(2, -2, 1) <= set(client.chunks),
                             "movement must retain the selected radius")
                client.send(0x0a, settings(3))
                client.until_id(0x0c)
                client.until(lambda *_: square(3, -2, 1) <= set(client.chunks),
                             "settings must use current player position")
        except ContractFailure as error:
            if client.contract_started:
                print(f"client settings wire: FAIL {error.code}", file=sys.stderr)
            raise
    print(f"client settings wire: configuration {'late' if late else 'early'} PASS")


if __name__ == "__main__":
    for late in [False, True]:
        session(Path(sys.argv[1]).resolve(), late)
    print("client settings wire: PASS")
