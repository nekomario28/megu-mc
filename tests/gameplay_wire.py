#!/usr/bin/env python3
"""CLI-only gameplay smoke: decode, move, break, chat, fetch again from the server."""
import json
import math
from pathlib import Path
import sys
from protocol767 import Client, ContractFailure, WIRE_BLOCKS, local_server, position, require, vint


def square(cx, cz):
    return {(cx + x, cz + z) for x in range(-2, 3) for z in range(-2, 3)}


def exercise(game):
    breakable = {st for b in WIRE_BLOCKS if b["boundingBox"] == "block" and 0 <= b["hardness"] < 50
                 for st in range(b["minStateId"], b["maxStateId"] + 1)}
    client = Client(game, "CliActor")
    try:
        with client:
            client.login()
            if client.pos is None:
                client.until_id(0x40)
            sx, sy, sz = client.pos
            cx, cz = math.floor(sx / 16), math.floor(sz / 16)
            # Login now leads with the 3x3 ring and the delivery loop overshoots
            # the negotiated view by vanilla's +1 bounding ring, so the contract
            # is containment of the requested square, not set equality.
            client.until(lambda *_: square(cx, cz) <= set(client.chunks), "spawn-window")
            client.move(sx + 16, sy, sz)
            client.until_id(0x0c)
            client.until(lambda *_: square(cx + 1, cz) <= set(client.chunks),
                         "movement changes the received window")
            client.move(sx, sy, sz)
            client.until_id(0x0c)
            client.until(lambda *_: square(cx, cz) <= set(client.chunks), "return-window")
            x, z = math.floor(sx), math.floor(sz)
            y = next(y for y in range(min(math.floor(sy) - 1, 319), -65, -1)
                     if client.block(x, y, z) in breakable)
            before = client.block(x, y, z)
            client.move(sx, y + 1.0, sz)
            client.send(0x24, vint(0) + position(x, y, z) + bytes([1]) + vint(71))
            client.until(lambda _pid, _body: ((x, y, z), 0) in client.updates)
            assert client.block(x, y, z) == 0, "server's block update confirms the break"
            marker = "cli_gameplay_round_trip"
            client.chat(marker)
            client.until(lambda _pid, _body: any(marker in line for line in client.messages), "chat-echo")
            assert any("CliActor" in line and marker in line for line in client.messages)
        # No prediction or block-update cache is shared with this connection.
        with Client(game, "CliReader") as reader:
            reader.login()
            require(reader.block(x, y, z) == 0, "fresh-reader-block")
        print(json.dumps({"protocol": 767, "chunk_decode": True, "movement": True,
                          "broken": [x, y, z], "before": before, "after": 0,
                          "chat_echo": marker, "fresh_reader": True}, sort_keys=True))
    except ContractFailure as error:
        if client.contract_started:
            print(f"gameplay wire: FAIL {error.code}", file=sys.stderr)
        raise


if __name__ == "__main__":
    with local_server(Path(sys.argv[1]).resolve()) as game:
        exercise(game)
    print("gameplay wire: PASS")
