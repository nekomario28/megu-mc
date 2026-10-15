#!/usr/bin/env python3
"""Same-input falling-block oracle.

Drives a protocol-767 server with `/setblock` and observes the block updates it
sends, so the landing cell of a dropped sand block can be compared against
vanilla without decoding chunk bodies.

  tools/falling_oracle.py PORT X Z [FLOOR_Y] [SECONDS]

Prints the block updates at (X,*,Z) in the order they arrived, then the final
states, as JSON on one line. The caller compares this across servers.
"""
import argparse
import hashlib
import json
import socket
import struct
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests"))
from wire_smoke import frame, recv_exact, recv_vint, vint  # noqa: E402
import protocol767 as P  # noqa: E402


class Oracle:
    def __init__(self, port, name):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=10)
        self.port, self.name = port, name
        self.updates = []
        self.messages = []

    def send(self, pid, body=b""):
        self.sock.sendall(frame(pid, body))

    def read(self, timeout):
        self.sock.settimeout(timeout)
        length = recv_vint(self.sock)
        data = recv_exact(self.sock, length)
        pid, shift, i = 0, 0, 0
        while True:
            b = data[i]
            i += 1
            pid |= (b & 0x7F) << shift
            shift += 7
            if not b & 0x80:
                break
        return pid, data[i:]

    def login(self, view=4):
        self.send(0, vint(767) + P.string("localhost") + struct.pack(">H", self.port) + vint(2))
        uid = bytearray(hashlib.md5(("OfflinePlayer:" + self.name).encode()).digest())
        uid[6] = (uid[6] & 15) | 48
        uid[8] = (uid[8] & 63) | 128
        self.send(0, P.string(self.name) + bytes(uid))
        deadline = time.monotonic() + 15
        assert self.read(15)[0] == 2, "login success"
        self.send(3)
        while time.monotonic() < deadline:
            if self.read(deadline - time.monotonic())[0] == 0x0E:
                break
        self.send(0, P.settings(view))
        self.send(7, vint(0))
        while time.monotonic() < deadline:
            if self.read(deadline - time.monotonic())[0] == 3:
                break
        self.send(3)
        while time.monotonic() < deadline:
            if self.read(deadline - time.monotonic())[0] == 0x0C:
                return
        raise AssertionError("play login not received")

    def command(self, text):
        # A chat message beginning with '/' is routed to the command handler; this is the same
        # packet the CLI gameplay fixture uses.
        body = P.string(text) + struct.pack(">qq", int(time.time() * 1000), 0) + bytes(5)
        self.send(0x06, body)

    def drain(self, seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            try:
                pid, body = self.read(0.1)
            except (TimeoutError, socket.timeout):
                continue
            except AssertionError:
                return
            if pid == 0x26:
                self.send(0x18, body)
            elif pid == 0x6C:
                try:
                    text = P.Reader(body).nbt()
                    self.messages.append(text if isinstance(text, str) else json.dumps(text))
                except Exception:
                    pass
            elif pid == 0x49:
                r = P.Reader(body)
                section = r.number(">Q")
                sx = P.signed(section >> 42, 22)
                sz = P.signed((section >> 20) & 0x3FFFFF, 22)
                sy = P.signed(section & 0xFFFFF, 20)
                for _ in range(r.varint()):
                    record = r.varlong()
                    x = sx * 16 + ((record >> 8) & 15)
                    y = sy * 16 + (record & 15)
                    z = sz * 16 + ((record >> 4) & 15)
                    self.updates.append((x, y, z, record >> 12))
            elif pid == 0x09 and len(body) >= 9:
                x, y, z = P.unpack_position(body[:8])
                state = 0
                shift, i = 0, 8
                while i < len(body):
                    b = body[i]
                    i += 1
                    state |= (b & 0x7F) << shift
                    shift += 7
                    if not b & 0x80:
                        break
                self.updates.append((x, y, z, state))

    def column_final(self, x, z):
        latest = {}
        for (ux, uy, uz, st) in self.updates:
            if ux == x and uz == z:
                latest[uy] = st
        return latest


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("port", type=int)
    ap.add_argument("x", type=int)
    ap.add_argument("z", type=int)
    ap.add_argument("floor_y", type=int, nargs="?", default=150)
    ap.add_argument("seconds", type=float, nargs="?", default=4.0)
    ap.add_argument("name", nargs="?", default="Oracle")
    args = ap.parse_args()
    o = Oracle(args.port, args.name)
    try:
        o.login()
        # Let chunk streaming register the client as holding the column's chunk before any
        # block update is produced, or the server has nobody to send it to.
        o.drain(3.0)
        # A stone floor, a cleared column above it, then sand three cells up.
        o.command(f"/setblock {args.x} {args.floor_y} {args.z} minecraft:stone")
        for y in range(args.floor_y + 1, args.floor_y + 4):
            o.command(f"/setblock {args.x} {y} {args.z} minecraft:air")
        o.drain(0.5)
        start = time.monotonic()
        o.command(f"/setblock {args.x} {args.floor_y + 3} {args.z} minecraft:sand")
        o.drain(args.seconds)
        final = o.column_final(args.x, args.z)
        # The landed block is the highest sand; report the whole final column.
        result = {
            "server_port": args.port,
            "floor_y": args.floor_y,
            "final_column": {str(y): st for y, st in sorted(final.items())},
            "messages": o.messages[-6:],
            "elapsed": round(time.monotonic() - start, 2),
        }
        print(json.dumps(result, sort_keys=True))
    finally:
        o.sock.close()


if __name__ == "__main__":
    main()
