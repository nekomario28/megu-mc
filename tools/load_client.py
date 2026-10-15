#!/usr/bin/env python3
"""Representative-load CLI driver for the server comparison.

Speaks protocol 767 with compression disabled, so the same driver drives both
vanilla 1.21.1 and mc-spine from the same movement script. It consumes and
acknowledges packets but never decodes chunk bodies: the point is to make both
servers do the same work (stream chunks for a moving viewer, tick the world),
not to assert one server's chunk format.

  tools/load_client.py PORT SECONDS [NAME]

It prints one line, `LOAD ok packets=<n> chunks_moved=<blocks>`, and exits
non-zero if login or the play handshake did not complete. It is not a gate on
gameplay; gameplay is checked by tests/gameplay_wire.py.
"""
import argparse
import hashlib
import socket
import struct
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests"))
from wire_smoke import frame, recv_exact, recv_vint, vint  # noqa: E402
import protocol767 as P  # noqa: E402


class RawClient:
    def __init__(self, port, name, view=4):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=10)
        self.port, self.name, self.view = port, name, view
        self.pos = (0.5, 80.0, 0.5)
        self.packets = 0

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

    def login(self):
        self.send(0, vint(767) + P.string("localhost") + struct.pack(">H", self.port) + vint(2))
        uid = bytearray(hashlib.md5(("OfflinePlayer:" + self.name).encode()).digest())
        uid[6] = (uid[6] & 15) | 48
        uid[8] = (uid[8] & 63) | 128
        self.send(0, P.string(self.name) + bytes(uid))
        deadline = time.monotonic() + 15
        assert self.read(15)[0] == 2, "login success"
        self.send(3)
        # configuration: wait for the known-packs/config step, send settings,
        # then finish configuration and enter play.
        while time.monotonic() < deadline:
            if self.read(deadline - time.monotonic())[0] == 0x0E:
                break
        self.send(0, P.settings(self.view))
        self.send(7, vint(0))
        while time.monotonic() < deadline:
            if self.read(deadline - time.monotonic())[0] == 3:
                break
        self.send(3)
        while time.monotonic() < deadline:
            if self.read(deadline - time.monotonic())[0] == 0x0C:
                return
        raise AssertionError("play login not received")

    def drive(self, seconds):
        started = time.monotonic()
        deadline = started + seconds
        distance = 0.0
        last = (0.5, 80.0, 0.5)
        while time.monotonic() < deadline:
            elapsed = time.monotonic() - started
            # Sweep across a wide area so both servers stream and unload many
            # chunks, not just the spawn region.
            x = 0.5 + (elapsed * 3.0) % 160.0
            z = 0.5 + (elapsed * 1.5) % 96.0
            self.send(0x1A, struct.pack(">ddd?", x, 80.0, z, True))
            distance += abs(x - last[0]) + abs(z - last[2])
            last = (x, 80.0, z)
            try:
                pid, body = self.read(0.02)
            except (TimeoutError, socket.timeout):
                continue
            self.packets += 1
            if pid == 0x26:  # keep alive -> acknowledge
                self.send(0x18, body)
            elif pid == 0x40:  # teleport -> confirm the id (first varint)
                confirm, shift, i = 0, 0, 0
                while True:
                    b = body[i]
                    i += 1
                    confirm |= (b & 0x7F) << shift
                    shift += 7
                    if not b & 0x80:
                        break
                self.send(0, vint(confirm))
        return distance


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("port", type=int)
    parser.add_argument("seconds", type=float)
    parser.add_argument("name", nargs="?", default="LoadProbe")
    args = parser.parse_args()
    client = RawClient(args.port, args.name)
    try:
        client.login()
        distance = client.drive(args.seconds)
        print(f"LOAD ok packets={client.packets} chunks_moved={distance:.0f}")
    finally:
        client.sock.close()


if __name__ == "__main__":
    main()
