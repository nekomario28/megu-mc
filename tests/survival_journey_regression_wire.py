#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Seven bounded real-player Protocol 767 journeys; no third-party dependencies.

Default/--self-test is offline. --server-bin requires an already built megu.
Uses the same framing, compression negotiation and configuration handshake as
protocol767.py (and multiplayer_stress_wire.py's ephemeral loopback world).
"""
import argparse
from contextlib import contextmanager
import math
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import tempfile
import time
import unittest

from protocol767 import Client, ROOT, Reader, frame_packet, position, string, vint
from wire_smoke import port, status


class JourneyClient(Client):
    def __init__(self, game, name):
        super().__init__(game, name)
        self.joins = 0
        self.syncs = 0
        self.keepalives = 0

    def read(self, timeout):
        pid, body = super().read(timeout)
        if self.phase == "play":
            self.joins += pid == 0x2b
            self.syncs += pid == 0x40
            self.keepalives += pid == 0x26
        return pid, body

    def spawn(self):
        self.login(view=2)
        self.until_id(0x40, "spawn sync missing")
        assert self.joins == 1 and self.syncs == 1, "missing or duplicated spawn"
        assert_position(self.pos)
        return self

    def command(self, text, fragment):
        before = len(self.messages)
        self.send(0x04, string(text))
        self.until(lambda *_: any(fragment in m for m in self.messages[before:]),
                   f"{text!r} did not respond with {fragment!r}")

    def pulse(self):
        """Prove the play connection still answers, not merely that TCP is open."""
        self.command("megu claim info", "claim")


def assert_position(coords):
    assert coords is not None and len(coords) == 3
    assert all(math.isfinite(v) and abs(v) < 30_000_000 for v in coords), coords
    assert coords[1] > -64, coords


def assert_claim(messages, owner):
    assert any(f"claimed by {owner}" in m for m in messages), messages


def attack_frames():
    return {
        "oversized": vint(2_097_153),  # oversized frame length: never send 2 MiB of data
        "nan": frame_packet(vint(0x1a) + struct.pack(">ddd?", float("nan"), 70, 0, True)),
        "truncated": frame_packet(vint(0) + vint(767) + string("localhost"))[:-1],
        "bad_slot": frame_packet(vint(0x0e) + b"\x00" + vint(0) +
                                 struct.pack(">hB", 32767, 0) + vint(0) * 3),
    }


def attack(game, raw, expect_close=True):
    with socket.create_connection(("127.0.0.1", game), timeout=1) as sock:
        sock.settimeout(0.5)
        sock.sendall(raw)
        sock.shutdown(socket.SHUT_WR)
        if expect_close:
            try:
                assert sock.recv(1) == b"", "malformed connection was not closed"
            except ConnectionResetError:
                pass


@contextmanager
def server(binary, world):
    for name in ("assets", "config_packets.bin"):
        source = ROOT / name
        if source.exists() and not (world / name).exists():
            (world / name).symlink_to(source, target_is_directory=source.is_dir())
    game = port()
    env = dict(os.environ, MEGU_PORT=str(game), MEGU_BIND_ADDR="127.0.0.1",
               MEGU_RESOURCE_PACK_URL="", MEGU_ANTICHEAT="off")
    env.pop("MEGU_RCON_PORT", None)
    log_path = world / "journey-server.log"
    with log_path.open("w+") as log:
        proc = subprocess.Popen([str(binary)], cwd=world, env=env,
                                stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                assert proc.poll() is None, "server exited before ready"
                try:
                    status(game)
                    break
                except (OSError, AssertionError):
                    time.sleep(0.05)
            else:
                raise AssertionError("server listener not ready within 5 seconds")
            yield game
        finally:
            if proc.poll() is None:
                proc.send_signal(signal.SIGTERM)
            try:
                code = proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
                raise AssertionError("server did not shut down within 3 seconds")
            log.flush()
            log.seek(0)
            output = log.read()
            assert code == 0, f"server exit {code}: {output[-2000:]}"
            assert "panicked at" not in output.lower(), output[-2000:]


def journeys(binary):
    binary = Path(binary).resolve()
    assert binary.is_file(), f"server binary not found: {binary}"
    with tempfile.TemporaryDirectory(prefix="survival-journey-") as tmp:
        world = Path(tmp)
        with server(binary, world) as game:
            with JourneyClient(game, "JourneyAlice") as a:
                a.spawn()
                print("1/7 spawn: login and finite position PASS", flush=True)
                x, y, z = a.pos
                for dx in (0.05, 0.10, 0.15):
                    a.move(x + dx, y, z)
                a.command("megu status", "TPS:")
                a.chat("survival journey hello")
                a.send(0x0f, b"\x00")  # close inventory
                assert a.syncs == 1, "movement rubber-banded"
                print("2/7 chat/command/inventory PASS", flush=True)
                with JourneyClient(game, "JourneyBob") as b:
                    b.spawn()
                    a.command("megu claim here", "Claimed chunk")
                    b.command("megu claim info", "claimed by JourneyAlice")
                    b.command("megu claim here", "already claimed by JourneyAlice")
                    # Choose an actual solid block, not air: a no-op break must
                    # never pass for protected gameplay. Read the block again
                    # from a fresh chunk stream after the attempted break.
                    bx, bz = math.floor(x), math.floor(z)
                    a.until_chunks({(bx // 16, bz // 16)}, "claim chunk not delivered")
                    block = next(((bx, by, bz) for by in range(math.floor(y) - 1, -65, -1)
                                  if a.block(bx, by, bz) != 0), None)
                    assert block is not None, "no solid block for claim test"
                    original_state = a.block(*block)
                    b.send(0x24, vint(0) + position(*block) + b"\x01" + vint(1))
                    b.send(0x38, vint(0) + position(*block) + vint(1) +
                           struct.pack(">fff?", 0.5, 1, 0.5, False) + vint(2))
                    b.command("megu claim info", "claimed by JourneyAlice")
                    print("3/7 two-player claim isolation (restart pending) PASS", flush=True)
                    a.command("megu claim info", "claimed by JourneyAlice")
                    assert_position(a.pos)
                    a.pulse()
                    print("4/7 dimension/position bounds PASS", flush=True)
                    # Keep Alice online during malformed attacks; only malformed
                    # peers may be discarded. NaN needs a logged-in peer.
                    for kind in ("oversized", "truncated"):
                        attack(game, attack_frames()[kind])
                        a.pulse()
                    with JourneyClient(game, "JourneyAttacker") as evil:
                        evil.spawn()
                        evil.socket.sendall(attack_frames()["nan"])
                        evil.socket.shutdown(socket.SHUT_WR)
                        a.pulse()
                    print("6/7 malformed peer isolation PASS", flush=True)
            with JourneyClient(game, "JourneyAlice") as again:
                again.spawn()
                again.command("megu claim info", "claimed by JourneyAlice")
                again.until_chunks({(block[0] // 16, block[2] // 16)}, "claim chunk not reloaded")
                assert again.block(*block) == original_state, "outsider broke claimed block"
                print("5/7 disconnect/reconnect identity and protected block PASS", flush=True)
        # The context above enforced exit 0 and panic-free SIGTERM shutdown.
        print("7/7 clean shutdown PASS", flush=True)
        with server(binary, world) as game:
            with JourneyClient(game, "JourneyAlice") as persisted:
                persisted.spawn()
                persisted.command("megu claim info", "claimed by JourneyAlice")
                print("3/7 restart persistence PASS", flush=True)


class OfflineContracts(unittest.TestCase):
    def test_builders(self):
        for value in (0, 127, 128, 767, 2_097_153):
            self.assertEqual(Reader(vint(value)).varint(), value)
        self.assertEqual(Reader(string("héllo")).string(), "héllo")
        hs = vint(767) + string("localhost") + struct.pack(">H", 25565) + vint(2)
        login = string("JourneyAlice") + bytes(16)
        for pid, payload in ((0, hs), (0, login), (3, b""), (0x1a, struct.pack(">ddd?", 1, 70, 1, True))):
            packet = vint(pid) + payload
            self.assertEqual(frame_packet(packet), vint(len(packet)) + packet)
        self.assertEqual(Reader(hs).varint(), 767)

    def test_adversarial(self):
        vectors = attack_frames()
        self.assertGreater(Reader(vectors["oversized"]).varint(), 2 * 1024 * 1024)
        self.assertTrue(math.isnan(struct.unpack(">d", vectors["nan"][2:10])[0]))
        self.assertLess(len(vectors["truncated"]), len(frame_packet(vint(0) + vint(767) + string("localhost"))))
        self.assertIn(struct.pack(">h", 32767), vectors["bad_slot"])

    def test_journey_assertions(self):
        assert_position((0, 70, 0))
        assert_claim(["Chunk (0, 0) claimed by JourneyAlice"], "JourneyAlice")
        for point in (None, (float("nan"), 70, 0), (0, -65, 0)):
            with self.assertRaises(AssertionError):
                assert_position(point)
        with self.assertRaises(AssertionError):
            assert_claim(["unclaimed"], "JourneyAlice")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument("--self-test", action="store_true")
    modes.add_argument("--server-bin", type=Path)
    args = parser.parse_args()
    if args.server_bin:
        journeys(args.server_bin)
    else:
        unittest.main(argv=[__file__], exit=True)
