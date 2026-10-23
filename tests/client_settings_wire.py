#!/usr/bin/env python3
"""Drive production configuration/play paths and assert decoded chunk sets."""
from pathlib import Path
import sys
from protocol767 import Client, ContractFailure, local_server, require, settings


def square(radius, cx=0, cz=0):
    return {(cx + x, cz + z) for x in range(-radius, radius + 1) for z in range(-radius, radius + 1)}


def session(binary, late):
    with local_server(binary) as game, Client(game) as client:
        try:
            client.login(late=late)
            require(set(client.chunks) == square(2), f"config-{'late' if late else 'early'}-window")
            if not late:
                # a request that grows the window is answered by chunks, paced by
                # this client's acknowledgements; a request that shrinks it is
                # answered by the centre update alone
                for requested, radius, barrier in [(3, 3, None), (2, 2, 0x54),
                                                   (127, 8, None), (-1, 2, 0x54)]:
                    client.send(0x0a, settings(requested))
                    if barrier is None:
                        before = len(client.chunk_batches)
                        client.until_chunks(square(radius), f"play-view-{requested}")
                        # the requested rate (30 chunks/tick) cannot carry the 17x17
                        # window in one batch, so the acknowledgement gated at least
                        # one more: that is the pacing this asserts
                        if requested == 127:
                            require(len(client.chunk_batches) - before >= 2,
                                    "chunk-batch-ack-pacing")
                    else:
                        client.until_id(barrier, f"play-view-{requested}")
                    require(set(client.chunks) == square(radius), f"play-window-{requested}")
                client.move(-24.0, 16.0, 24.0)
                client.until_id(0x0c)
                assert set(client.chunks) == square(2, -2, 1), "movement must retain the selected radius"
                client.send(0x0a, settings(3))
                client.until_id(0x0c)
                assert set(client.chunks) == square(3, -2, 1), "settings must use current player position"
        except ContractFailure as error:
            if client.contract_started:
                print(f"client settings wire: FAIL {error.code}", file=sys.stderr)
            raise
    print(f"client settings wire: configuration {'late' if late else 'early'} PASS")


if __name__ == "__main__":
    for late in [False, True]:
        session(Path(sys.argv[1]).resolve(), late)
    print("client settings wire: PASS")
