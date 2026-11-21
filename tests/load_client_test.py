#!/usr/bin/env python3
"""Pin the protocol-767 behaviours that made the live load comparison invalid.

Measured 2026-11-21 (`evidence/perf/m3-warm-worldgen.md`): vanilla delivered nine
login chunks and zero during the movement drive while mc-spine delivered its whole
view, so the CPU numbers measured different work. The cause was the driver, not
the servers: a protocol 767 server stops after Chunk Batch Finished (0x0C) until
the client answers Chunk Batch Received (0x08), and a client that is awaiting a
teleport confirm ignores movement. Each test below drives the real CLI against a
loopback mock server that sends the second batch only after the first is
acknowledged, so a driver that regresses to one-batch-and-stall fails here.

Never connects to a live Minecraft service.
"""
import json
import re
import socket
import struct
import subprocess
import sys
import threading
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CLIENT = ROOT / "tools" / "load_client.py"
sys.path.insert(0, str(ROOT / "tests"))
from wire_smoke import frame, recv_exact, recv_vint, vint  # noqa: E402

LOAD_LINE = re.compile(
    r"^LOAD ok packets=(?P<packets>\d+) chunks_login=(?P<chunks_login>\d+) "
    r"chunks_drive=(?P<chunks_drive>\d+) unloads=(?P<unloads>\d+) "
    r"batches=(?P<batches>\d+) chunks_moved=(?P<chunks_moved>\d+)$")


def varint_at(data, pos=0):
    """The driver's own VarInt reader is not imported: this stays black-box."""
    value = 0
    for i in range(5):
        byte = data[pos + i]
        value |= (byte & 0x7F) << (7 * i)
        if not byte & 0x80:
            return value, pos + i + 1
    raise AssertionError("oversized VarInt")


class MockServer(threading.Thread):
    """Loopback protocol-767 server that records every packet the driver sends.

    The pacing is the point: it waits for the client's 0x08 before sending the
    next batch, exactly as vanilla does, so an unacknowledged batch leaves the
    test's second chunk count at zero instead of passing quietly.
    """

    def __init__(self, *, login_chunks=3, drive_chunks=6, chunk_bytes=64,
                 spawn=(0.5, 70.0, 0.5), teleport_id=7,
                 keepalive=1_700_000_000_000, unloads=2):
        super().__init__(daemon=True)
        self.listener = socket.socket()
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(1)
        self.port = self.listener.getsockname()[1]
        self.login_chunks, self.drive_chunks = login_chunks, drive_chunks
        self.chunk_bytes, self.spawn = chunk_bytes, spawn
        self.teleport_id, self.keepalive, self.unloads = teleport_id, keepalive, unloads
        self.received = []  # (phase, packet id, body)
        self.errors = []
        self.conn = None

    def _read(self):
        length = recv_vint(self.conn)
        data = recv_exact(self.conn, length)
        pid, offset = varint_at(data)
        return pid, data[offset:]

    def _expect(self, wanted, phase):
        pid, body = self._read()
        self.received.append((phase, pid, body))
        assert pid == wanted, f"expected 0x{wanted:02x} in {phase}, got 0x{pid:02x}"
        return body

    def _await(self, wanted, phase="play"):
        # Movement keeps arriving while the client reads, so the packet that
        # follows a batch need not be the acknowledgement.
        while True:
            pid, body = self._read()
            self.received.append((phase, pid, body))
            if pid == wanted:
                return body

    def _send(self, pid, body=b""):
        self.conn.sendall(frame(pid, body))

    def _batch(self, count):
        self._send(0x0B)  # Chunk Batch Start
        for i in range(count):
            self._send(0x27, bytes([i & 0xFF]) * self.chunk_bytes)
        self._send(0x0C, vint(count))  # Chunk Batch Finished

    def _serve(self):
        self._expect(0x00, "handshake")
        self._expect(0x00, "login")
        self._send(0x02, bytes(16) + vint(0) + vint(0))  # login success
        self._expect(0x03, "login")
        self._send(0x0E, vint(0))  # select known packs
        # Configuration 0x0C is Feature Flags, not Chunk Batch Finished. Answering
        # it as a batch made vanilla drop the client ("unknown packet id 8").
        self._send(0x0C, vint(0))
        self._expect(0x00, "config")  # client information
        self._expect(0x07, "config")  # select known packs
        self._send(0x03)  # finish configuration
        self._expect(0x03, "config")
        self._batch(self.login_chunks)
        self._await(0x08)  # the ack that unlocks the next batch
        self._send(0x40, struct.pack(">dddffB", *self.spawn, 0.0, 0.0, 0)
                   + vint(self.teleport_id))
        self._await(0x00)  # teleport confirm
        self._await(0x1A)  # first movement
        self._batch(self.drive_chunks)
        self._await(0x08)
        self._send(0x26, struct.pack(">q", self.keepalive))
        for _ in range(self.unloads):
            self._send(0x21, struct.pack(">ii", 1, 2))
        while True:
            try:
                pid, body = self._read()
            except (AssertionError, OSError):
                return
            self.received.append(("play", pid, body))

    def run(self):
        try:
            self.conn, _ = self.listener.accept()
            self.conn.settimeout(10)
            self._serve()
        except Exception as exc:  # reported to the test, not swallowed
            self.errors.append(exc)
        finally:
            if self.conn is not None:
                self.conn.close()

    def close(self):
        self.listener.close()


def run_client(seconds=1.6, extra=(), **server_kwargs):
    # The drive climbs four blocks per step to the cruise altitude, so a short
    # window would mostly measure the climb rather than the sweep.
    server = MockServer(**server_kwargs)
    server.start()
    try:
        proc = subprocess.run(
            [sys.executable, str(CLIENT), str(server.port), str(seconds), "LoadTest", *extra],
            capture_output=True, text=True, timeout=60)
    finally:
        server.join(timeout=15)
        server.close()
    return proc, server


def play_packets(server):
    return [(pid, body) for phase, pid, body in server.received if phase == "play"]


class LoadClientTests(unittest.TestCase):
    def assert_clean(self, proc, server):
        self.assertEqual(server.errors, [], f"mock server failed: {server.errors}")
        self.assertEqual(proc.returncode, 0, proc.stderr[-2000:])

    def test_chunk_batch_finished_is_acknowledged_with_big_endian_f32(self):
        proc, server = run_client(extra=("--chunks-per-tick", "64"))
        self.assert_clean(proc, server)
        acks = [body for pid, body in play_packets(server) if pid == 0x08]
        self.assertEqual(len(acks), 2, "one ack per finished batch")
        for body in acks:
            self.assertEqual(len(body), 4)
            self.assertEqual(struct.unpack(">f", body)[0], 64.0)

    def test_spawn_position_is_confirmed_before_movement(self):
        proc, server = run_client(teleport_id=23)
        self.assert_clean(proc, server)
        packets = play_packets(server)
        confirm_at = next(i for i, (pid, _) in enumerate(packets) if pid == 0x00)
        move_at = next(i for i, (pid, _) in enumerate(packets) if pid == 0x1A)
        self.assertLess(confirm_at, move_at, "movement before the teleport confirm")
        value, _ = varint_at(packets[confirm_at][1])
        self.assertEqual(value, 23)

    def test_chunk_burst_is_drained_without_stalling(self):
        proc, server = run_client(login_chunks=12, drive_chunks=16, chunk_bytes=4096)
        self.assert_clean(proc, server)
        match = LOAD_LINE.match(proc.stdout.strip().splitlines()[-1])
        self.assertIsNotNone(match, proc.stdout)
        stats = match.groupdict()
        self.assertEqual(int(stats["chunks_login"]), 12)
        self.assertEqual(int(stats["chunks_drive"]), 16)
        self.assertEqual(int(stats["batches"]), 2)

    def test_keep_alive_is_echoed(self):
        proc, server = run_client(keepalive=987654321)
        self.assert_clean(proc, server)
        echoes = [body for pid, body in play_packets(server) if pid == 0x18]
        self.assertEqual(echoes, [struct.pack(">q", 987654321)])

    def test_summary_line_reports_login_drive_batches_and_unloads(self):
        proc, server = run_client(login_chunks=4, drive_chunks=5, unloads=3)
        self.assert_clean(proc, server)
        line = proc.stdout.strip().splitlines()[-1]
        match = LOAD_LINE.match(line)
        self.assertIsNotNone(match, line)
        stats = match.groupdict()
        self.assertEqual(int(stats["chunks_login"]), 4)
        self.assertEqual(int(stats["chunks_drive"]), 5)
        self.assertEqual(int(stats["unloads"]), 3)
        self.assertEqual(int(stats["batches"]), 2)
        self.assertGreater(int(stats["chunks_moved"]), 0)
        self.assertGreaterEqual(int(stats["packets"]), 4 + 5 + 3 + 2)

    def test_json_summary_carries_the_same_counts(self):
        proc, server = run_client(login_chunks=2, drive_chunks=2, extra=("--json",))
        self.assert_clean(proc, server)
        stats = json.loads(proc.stdout)
        self.assertEqual((stats["chunks_login"], stats["chunks_drive"], stats["batches"]),
                         (2, 2, 2))
        self.assertIn("chunks_moved", stats)
        self.assertIn("packets", stats)


if __name__ == "__main__":
    unittest.main(verbosity=2)
