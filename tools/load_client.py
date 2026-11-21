#!/usr/bin/env python3
"""Representative-load CLI driver for the server comparison.

Speaks protocol 767 with or without negotiated compression, so the same driver
drives both vanilla 1.21.1 and mc-spine from the same movement script. It consumes and
acknowledges packets but never decodes chunk bodies: the point is to make both
servers do the same work (stream chunks for a moving viewer, tick the world),
not to assert one server's chunk format.

  tools/load_client.py PORT SECONDS [NAME] [--view-distance N]
                       [--chunks-per-tick F] [--wait-login-chunks N] [--json]

Delivery is paced by the client. A protocol 767 server sends a batch of chunks
between Chunk Batch Start (0x0B) and Chunk Batch Finished (0x0C), and after
0x0C it sends nothing more until the client answers with Chunk Batch Received
(0x08) carrying a big-endian f32 chunks-per-tick. A driver that never answers
receives exactly one batch and stalls; measured, vanilla delivered nine login
chunks and zero during the movement drive while mc-spine sent its whole view.
This driver acknowledges every batch, confirms the initial Synchronize Player
Position (0x40) before moving - a server that is awaiting a teleport confirm
ignores movement - and drains every readable packet between 20 Hz movement
steps so neither server waits on TCP backpressure.

The viewer climbs at a legal rate (four blocks per packet, inside vanilla's
ten-block-per-packet allowance even when two steps land in one tick) while
sweeping above the terrain. It has no world model,
so walking at ground level makes vanilla reject the position whenever the
terrain rises; `tools/load_compare.sh` therefore starts vanilla in creative,
where the server-side collision check is skipped. Chunk streaming is per
column, so the altitude does not change the work being measured.

It prints one line,
`LOAD ok packets=<n> chunks_login=<n> chunks_drive=<n> unloads=<n> batches=<n> chunks_moved=<blocks>`
(or the same fields as JSON with `--json`), and exits non-zero if login or the
play handshake did not complete. It is not a gate on gameplay; gameplay is
checked by tests/gameplay_wire.py.
"""
import argparse
import hashlib
import json
import math
import select
import socket
import struct
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests"))
from wire_smoke import recv_exact, recv_vint, vint  # noqa: E402
import protocol767 as P  # noqa: E402

LOGIN_TIMEOUT = 15.0
# Initial world generation may take longer than the login/configuration exchange.
# This budget ends before movement timing begins, for either server.
PLAY_READY_TIMEOUT = 120.0
LOGIN_CHUNK_TIMEOUT = 30.0
MOVE_INTERVAL = 0.05  # 20 Hz, the rate a vanilla client sends position updates
CRUISE_Y = 250.0     # above the terrain; chunk streaming is per column
# Blocks per packet. Vanilla allows a squared distance of 100 per movement
# packet since the last tick, so two packets in one tick must stay under 200:
# 4 keeps up to six batched steps inside the allowance.
ASCENT_STEP = 4.0

# clientbound play
CHUNK_BATCH_FINISHED = 0x0C
UNLOAD_CHUNK = 0x21
KEEP_ALIVE = 0x26
CHUNK_DATA = 0x27
SYNC_POSITION = 0x40
# serverbound play
TELEPORT_CONFIRM = 0x00
CHUNK_BATCH_RECEIVED = 0x08
KEEP_ALIVE_ACK = 0x18
SET_POSITION = 0x1A
# configuration (packet IDs are scoped to their protocol state)
CONFIG_KEEP_ALIVE = 0x04
CONFIG_PING = 0x05


def read_varint(data, pos=0):
    """Return (value, next_pos), or (None, pos) when `data` ends mid-VarInt."""
    value = 0
    for i in range(5):
        if pos + i >= len(data):
            return None, pos
        byte = data[pos + i]
        value |= (byte & 0x7F) << (7 * i)
        if not byte & 0x80:
            return value, pos + i + 1
    raise ValueError("oversized VarInt")


class RawClient:
    def __init__(self, port, name, view=4, chunks_per_tick=20.0, wait_login_chunks=0):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=10)
        self.port, self.name, self.view = port, name, view
        self.chunks_per_tick = chunks_per_tick
        self.wait_login_chunks = wait_login_chunks
        self.phase = "login"
        self.compression_threshold = -1
        self.pos = None
        self.buffer = bytearray()
        self.packets = 0
        self.chunks_login = 0
        self.chunks_drive = 0
        self.unloads = 0
        self.chunk_batches = 0
        self.keepalives = 0
        self.teleports = 0
        self.rtt_ms = None
        self.distance = 0.0

    def send(self, pid, body=b""):
        self.sock.sendall(P.frame_packet(vint(pid) + body, self.compression_threshold))

    def decode(self, data):
        payload = P.unframe_packet(data, self.compression_threshold)
        pid, offset = read_varint(payload)
        if pid is None:
            raise ValueError("missing packet ID")
        return pid, payload[offset:]

    def read(self, timeout):
        self.sock.settimeout(timeout)
        length = recv_vint(self.sock)
        data = recv_exact(self.sock, length)
        return self.decode(data)

    def note_rtt(self, body):
        """Vanilla puts its send time in the keepalive id, so its age is an RTT.

        mc-spine counts keepalives (1, 2, 3, ...), which is not a timestamp: the
        sample is left at None rather than reported as a nonsense duration.
        """
        if len(body) < 8:
            return
        sent = struct.unpack(">q", body[:8])[0]
        now = time.time() * 1000.0
        if 0 <= now - sent <= 60_000:
            self.rtt_ms = now - sent

    def handle(self, pid, body, stage):
        """Consume one packet. `stage` is "login" or "drive" for chunk accounting.

        Packet IDs are state-scoped. Configuration 0x0C is Feature Flags, not
        Chunk Batch Finished. Answering it as a batch acknowledgement made
        vanilla drop the connection ("unknown packet id 8").
        """
        self.packets += 1
        if self.phase == "login":
            if pid == 0x03:  # Set Compression, sent with uncompressed framing
                threshold, end = read_varint(body)
                if (threshold is None or end != len(body) or self.compression_threshold >= 0):
                    raise AssertionError("invalid or duplicate Set Compression")
                self.compression_threshold = threshold
            elif pid == 0x00:
                raise ConnectionError("disconnected during login")
            return
        if self.phase == "config":
            if pid == CONFIG_KEEP_ALIVE:
                if len(body) != 8:
                    raise AssertionError("invalid configuration KeepAlive")
                self.send(CONFIG_KEEP_ALIVE, body)
            elif pid == CONFIG_PING:
                if len(body) != 4:
                    raise AssertionError("invalid configuration Ping")
                self.send(CONFIG_PING, body)
            elif pid == 0x02:
                raise ConnectionError("disconnected during configuration")
            return
        if self.phase != "play":
            return
        if pid == CHUNK_BATCH_FINISHED:
            # Without this answer the server sends no further batch at all.
            self.chunk_batches += 1
            self.send(CHUNK_BATCH_RECEIVED, struct.pack(">f", self.chunks_per_tick))
        elif pid == CHUNK_DATA:
            if stage == "login":
                self.chunks_login += 1
            else:
                self.chunks_drive += 1
        elif pid == UNLOAD_CHUNK:
            self.unloads += 1
        elif pid == KEEP_ALIVE:
            self.keepalives += 1
            self.note_rtt(body)
            self.send(KEEP_ALIVE_ACK, body)
        elif pid == SYNC_POSITION:
            # x, y, z doubles, yaw and pitch floats, flags byte, then the id.
            if len(body) < 33:
                raise AssertionError("truncated Synchronize Player Position")
            self.teleports += 1
            confirm, _ = read_varint(body, 33)
            self.send(TELEPORT_CONFIRM, vint(confirm))
            self.pos = struct.unpack(">ddd", body[:24])

    def read_until(self, predicate, deadline):
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"{self.phase} did not complete before the deadline")
            pid, body = self.read(remaining)
            self.handle(pid, body, "login")
            if predicate(pid, body):
                return pid, body

    def login(self):
        self.send(0, vint(767) + P.string("localhost") + struct.pack(">H", self.port) + vint(2))
        uid = bytearray(hashlib.md5(("OfflinePlayer:" + self.name).encode()).digest())
        uid[6] = (uid[6] & 15) | 48
        uid[8] = (uid[8] & 63) | 128
        self.send(0, P.string(self.name) + bytes(uid))
        deadline = time.monotonic() + LOGIN_TIMEOUT
        assert self.read_until(lambda pid, _: pid == 2, deadline)[0] == 2, "login success"
        self.send(3)
        self.phase = "config"
        # configuration: wait for the known-packs step, send settings, then
        # finish configuration and enter play.
        self.read_until(lambda pid, _: pid == 0x0E, deadline)
        self.send(0, P.settings(self.view))
        self.send(7, vint(0))
        self.read_until(lambda pid, _: pid == 3, deadline)
        self.send(3)
        self.phase = "play"
        # Initial chunk generation can exceed the login/configuration deadline.
        # Wait for the play teleport before starting the measured movement window.
        self.read_until(lambda pid, _: pid == SYNC_POSITION,
                        time.monotonic() + PLAY_READY_TIMEOUT)
        if self.chunks_login < self.wait_login_chunks:
            self.wait_for_login_chunks()

    def wait_for_login_chunks(self):
        deadline = time.monotonic() + LOGIN_CHUNK_TIMEOUT
        while self.chunks_login < self.wait_login_chunks:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                print(f"load_client: only {self.chunks_login} of "
                      f"{self.wait_login_chunks} requested login chunks arrived",
                      file=sys.stderr)
                return
            try:
                pid, body = self.read(min(1.0, remaining))
            except (TimeoutError, socket.timeout):
                continue
            self.handle(pid, body, "login")

    def parse(self):
        """Handle every complete frame in the buffer; returns how many."""
        received = 0
        while True:
            length, start = read_varint(self.buffer)
            if length is None or len(self.buffer) < start + length:
                return received
            payload = bytes(self.buffer[start:start + length])
            del self.buffer[:start + length]
            pid, body = self.decode(payload)
            self.handle(pid, body, "drive")
            received += 1

    def drain(self, timeout):
        """Read everything currently readable, waiting at most `timeout`.

        Draining (rather than one packet per step) keeps a chunk burst from
        backing up the socket while the client is pacing movement.
        """
        ready, _, _ = select.select([self.sock], [], [], max(0.0, timeout))
        if ready:
            self.sock.setblocking(False)
            try:
                while True:
                    try:
                        data = self.sock.recv(65536)
                    except (BlockingIOError, InterruptedError):
                        break
                    if not data:
                        raise ConnectionError("server closed the connection")
                    self.buffer.extend(data)
            finally:
                # Movement and acknowledgements are sent from this loop; a
                # non-blocking socket would make sendall raise when the send
                # buffer is momentarily full.
                self.sock.settimeout(10)
        return self.parse()

    def drive(self, seconds):
        x, y, z = self.pos or (0.5, 80.0, 0.5)
        origin = (x, z)
        vx, vz = 3.0, 1.5
        anchored = self.pos
        started = time.monotonic()
        deadline = started + seconds
        next_step = started
        last = (x, z)
        self.distance = 0.0
        while True:
            now = time.monotonic()
            if now >= deadline:
                break
            # A server correction re-anchors the sweep: continuing from the old
            # origin would look like another implausible jump and be rejected.
            if self.pos is not None and self.pos != anchored:
                x, y, z = self.pos
                origin, anchored = (x, z), self.pos
                vx, vz = 3.0, 1.5
                last = (x, z)
            if now >= next_step:
                if y < CRUISE_Y:
                    # Vanilla rejects a move whose squared distance from the
                    # server's position exceeds 100 per packet since its last
                    # tick, and a raw client has no terrain height to walk on.
                    # Climbing at a legal rate while sweeping keeps the path
                    # accepted; chunk streaming is per column, so the altitude
                    # does not change the work being measured.
                    y = min(CRUISE_Y, y + ASCENT_STEP)
                x += vx * MOVE_INTERVAL
                z += vz * MOVE_INTERVAL
                # Sweep a wide area by bouncing instead of wrapping: a wrap
                # is a 160-block jump, which vanilla rejects and rubber-bands.
                if x > origin[0] + 80.0 or x < origin[0] - 80.0:
                    x = 2.0 * (origin[0] + math.copysign(80.0, x - origin[0])) - x
                    vx = -vx
                if z > origin[1] + 48.0 or z < origin[1] - 48.0:
                    z = 2.0 * (origin[1] + math.copysign(48.0, z - origin[1])) - z
                    vz = -vz
                self.distance += abs(x - last[0]) + abs(z - last[1])
                last = (x, z)
                self.send(SET_POSITION, struct.pack(">ddd?", x, y, z, False))
                next_step += MOVE_INTERVAL
                if next_step < now:  # fell behind; do not burst to catch up
                    next_step = now + MOVE_INTERVAL
                wait = 0.0
            else:
                wait = min(next_step, deadline) - now
            self.drain(wait)
        self.drain(0.0)
        return self.distance

    def summary(self):
        return {
            "packets": self.packets,
            "chunks_login": self.chunks_login,
            "chunks_drive": self.chunks_drive,
            "unloads": self.unloads,
            "batches": self.chunk_batches,
            "chunks_moved": round(self.distance),
            "keepalives": self.keepalives,
            "teleports": self.teleports,
            "rtt_ms": None if self.rtt_ms is None else round(self.rtt_ms, 1),
        }


def format_line(stats):
    return ("LOAD ok packets={packets} chunks_login={chunks_login} "
            "chunks_drive={chunks_drive} unloads={unloads} batches={batches} "
            "chunks_moved={chunks_moved}").format(**stats)


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("port", type=int)
    parser.add_argument("seconds", type=float)
    parser.add_argument("name", nargs="?", default="LoadProbe")
    parser.add_argument("--view-distance", type=int, default=4)
    parser.add_argument("--chunks-per-tick", type=float, default=20.0)
    parser.add_argument("--wait-login-chunks", type=int, default=0)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    if not math.isfinite(args.chunks_per_tick) or args.chunks_per_tick <= 0:
        parser.error("--chunks-per-tick must be a finite positive rate")
    client = RawClient(args.port, args.name, view=args.view_distance,
                       chunks_per_tick=args.chunks_per_tick,
                       wait_login_chunks=args.wait_login_chunks)
    try:
        client.login()
        client.drive(args.seconds)
        stats = client.summary()
        print(json.dumps(stats) if args.json else format_line(stats))
    finally:
        client.sock.close()


if __name__ == "__main__":
    main()
