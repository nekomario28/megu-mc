#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Hard mid-play chaos, race and SIGKILL power-cut recovery E2E suite.

Default/--self-test is offline (<0.5s) and validates every chaos packet
builder. --server-bin runs five real-binary chaos scenarios against a live
megu process while an innocent bystander (Player A) stays online:

1. container click & race chaos (out-of-bounds slot, drop-outside, negative
   item count, stale stateId, click-after-close, concurrent 2-client clicks),
2. world-boundary, dead-state and coordinate chaos (Y=-65/321, +-29,999,999,
   NaN, +-Inf, extreme yaw/pitch, out-of-bounds dig/place),
3. custom-protocol fuzzing over the megu:* channel set (truncated,
   wrong-magic, oversized and random frames),
4. rapid disconnect/reconnect mid-action storm (5x RST drop, same-name
   session handoff), and
5. SIGKILL sudden power-cut recovery: kill the server mid-play, restart on
   the same world directory and prove the claim state survived intact.

Uses the same framing, compression negotiation and configuration handshake
as protocol767.py and survival_journey_regression_wire.py. No third-party
dependencies.
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
import threading
import time
import unittest

from protocol767 import (Client, ROOT, Reader, frame_packet, position, string,
                         unpack_position, vint)
from wire_smoke import port, status

CHANNELS = (
    "megu:portal_seamless_v1", "megu:subworld_sync_v1", "megu:subworld_helm_v1",
    "megu:mod_sync_v1", "megu:registry_sync_v1", "megu:mod_registry_sync_v1",
)
BOUNDARIES = (
    (0.0, -65.0, 0.0), (0.0, 321.0, 0.0),
    (29_999_999.0, 64.0, 0.0), (-29_999_999.0, 64.0, 0.0),
    (float("nan"), 64.0, 0.0), (0.0, float("nan"), 0.0),
    (0.0, 64.0, float("nan")), (float("inf"), 64.0, 0.0),
    (0.0, -float("inf"), 0.0),
)
RECONNECT_ROUNDS = 5


# ------------------------------------------------------------ builders ----

def click_body(wid=0, state_id=0, slot=0, button=0, mode=0, changed=(),
               carried=b"\x00"):
    """C2S 0x0e container_click body: wid u8, stateId varint, slot i16,
    button i8, mode varint, changed (slot i16 + item blob) pairs, carried."""
    body = bytes([wid & 0xFF]) + vint(state_id) + struct.pack(">h", slot)
    body += bytes([button & 0xFF]) + vint(mode) + vint(len(changed))
    for changed_slot, item in changed:
        body += struct.pack(">h", changed_slot) + item
    return body + carried


def chaos_clicks():
    """Container clicks that crash real servers: each value alone is legal to
    decode; it is the semantic that is hostile."""
    return {
        "oob_slot": click_body(slot=9999),
        "drop_outside": click_body(slot=-999),
        "negative_slot": click_body(slot=-32768),
        "negative_count": click_body(changed=[(0, vint(0xFFFFFFFF))]),
        "stale_state": click_body(state_id=0x7FFF, slot=9),
        "wild_mode": click_body(mode=0x7FFFFFFF, slot=9, button=127),
    }


def payload_body(channel, data=b""):
    """C2S 0x12 custom_payload body: channel identifier + raw payload."""
    return string(channel) + data


def fuzz_payloads():
    """Truncated, wrong-magic, oversized and random custom-payload frames."""
    valid = b"MSP1" + bytes([1]) + bytes(range(32))
    return {
        "truncated": valid[:3],
        "wrong_magic": b"XXXX" + valid[4:],
        "oversized": valid + bytes(65536),
        "random": bytes(((i * 37) + 11) & 0xFF for i in range(97)),
        "empty": b"",
    }


def move_pos_body(x, y, z):
    return struct.pack(">ddd?", x, y, z, True)


def move_rot_body(x, y, z, yaw, pitch):
    return struct.pack(">dddff?", x, y, z, yaw, pitch, True)


def dig_body(dig_status, x, y, z, face=1, seq=0):
    """C2S 0x24 player_action body."""
    return vint(dig_status) + position(x, y, z) + bytes([face & 0xFF]) + vint(seq)


def use_on_body(x, y, z, face=1, seq=0):
    """C2S 0x38 use_item_on body."""
    return (vint(0) + position(x, y, z) + vint(face)
            + struct.pack(">fff?", 0.5, 0.5, 0.5, False) + vint(seq))


def linger0():
    """SO_LINGER {on, 0s}: close() then emits RST — a mid-action power loss."""
    return struct.pack("ii", 1, 0)


# ------------------------------------------------------------- runtime ----

class ChaosClient(Client):
    def __init__(self, game, name):
        super().__init__(game, name)
        self.joins = 0
        self.keepalives = 0

    def read(self, timeout):
        pid, body = super().read(timeout)
        if self.phase == "play":
            self.joins += pid == 0x2B
            self.keepalives += pid == 0x26
        return pid, body

    def send(self, pid, body=b""):
        """A chaos peer may be disconnected mid-burst; that is data, not failure."""
        try:
            super().send(pid, body)
        except OSError:
            pass

    def spawn(self, view=2):
        self.login(view=view)
        self.until_id(0x40, "spawn sync missing")
        assert self.joins == 1, "missing or duplicated spawn"
        return self

    def command(self, text, fragment):
        before = len(self.messages)
        self.send(0x04, string(text))
        self.until(lambda *_: any(fragment in m for m in self.messages[before:]),
                   f"{text!r} did not respond with {fragment!r}")

    def pulse(self):
        """Bidirectional liveness: the server must still process our packets."""
        self.command("megu claim info", "claim")


def storm_login(game, name, attempts=2):
    """One session-handoff attempt may race the old ghost's reaping; a second
    must succeed or the handoff itself is broken."""
    for attempt in range(attempts):
        client = ChaosClient(game, name)
        try:
            return client.spawn(view=1)
        except (AssertionError, OSError, TimeoutError):
            client.socket.close()
            if attempt + 1 == attempts:
                raise
            time.sleep(0.25)


def spawn_server(binary, world, log_name):
    for link in ("assets", "datapacks", "config_packets.bin"):
        source = ROOT / link
        if source.exists() and not (world / link).exists():
            (world / link).symlink_to(source, target_is_directory=source.is_dir())
    game = port()
    env = dict(os.environ, MEGU_PORT=str(game), MEGU_BIND_ADDR="127.0.0.1",
               MEGU_SEED="0", MEGU_RESOURCE_PACK_URL="", MEGU_ANTICHEAT="off")
    env.pop("MEGU_RCON_PORT", None)
    log = (world / log_name).open("w+")
    proc = subprocess.Popen([str(binary)], cwd=world, env=env,
                            stdin=subprocess.DEVNULL, stdout=log, stderr=log)
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        assert proc.poll() is None, "server exited before ready"
        try:
            status(game)
            return proc, game, log
        except (OSError, AssertionError):
            time.sleep(0.05)
    proc.kill()
    proc.wait()
    log.close()
    raise AssertionError("server listener not ready")


@contextmanager
def chaos_server(binary, world, log_name="chaos-server.log", power_cut=False):
    proc, game, log = spawn_server(binary, world, log_name)
    try:
        yield game, proc
    finally:
        if proc.poll() is None:
            proc.send_signal(signal.SIGKILL if power_cut else signal.SIGTERM)
        try:
            code = proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
            log.close()
            raise AssertionError("server did not stop within 5 seconds") from None
        log.flush()
        log.seek(0)
        output = log.read()
        log.close()
        if power_cut:
            assert code == -signal.SIGKILL, f"expected SIGKILL exit, got {code}"
        else:
            assert code == 0, f"server exit {code}: {output[-2000:]}"
        assert "panicked at" not in output.lower(), output[-2000:]


def scenario_container_chaos(a, game):
    with ChaosClient(game, "ClickGremlin") as evil:
        evil.spawn(view=1)
        for body in chaos_clicks().values():
            evil.send(0x0E, body)
        # click-after-close: the window is gone, the clicks keep coming
        evil.send(0x0F, b"\x00")
        for body in chaos_clicks().values():
            evil.send(0x0E, body)
        with ChaosClient(game, "ClickGremlin2") as second:
            second.spawn(view=1)
            stop = threading.Event()

            def storm(client):
                while not stop.is_set():
                    client.send(0x0E, click_body(slot=9))
                    client.send(0x0E, click_body(slot=-999, button=1))

            threads = [threading.Thread(target=storm, args=(c,), daemon=True)
                       for c in (evil, second)]
            for t in threads:
                t.start()
            time.sleep(0.4)
            stop.set()
            for t in threads:
                t.join(1)
    a.pulse()


def scenario_boundary_chaos(a, game):
    with ChaosClient(game, "VoidWalker") as evil:
        evil.spawn(view=1)
        for x, y, z in BOUNDARIES:
            evil.send(0x1A, move_pos_body(x, y, z))
        for yaw, pitch in ((float("nan"), 0.0), (float("inf"), float("-inf")),
                           (36000.0, -36000.0), (720.0, float("nan"))):
            evil.send(0x1B, move_rot_body(0.0, 64.0, 0.0, yaw, pitch))
        # dig and place beyond the buildable column
        for dig_status in (0, 1, 2):
            evil.send(0x24, dig_body(dig_status, 8, -65, 8))
            evil.send(0x24, dig_body(dig_status, 8, 321, 8))
        evil.send(0x38, use_on_body(8, -65, 8))
        evil.send(0x38, use_on_body(8, 321, 8))
    a.pulse()


def scenario_protocol_fuzz(a, game):
    with ChaosClient(game, "FuzzBot") as evil:
        evil.spawn(view=1)
        for channel in CHANNELS:
            for payload in fuzz_payloads().values():
                evil.send(0x12, payload_body(channel, payload))
        # and channels the server never registered at all
        for channel in ("megu:does_not_exist_v9", "minecraft:register",
                        "minecraft:unregister", "minecraft:brand"):
            evil.send(0x12, payload_body(channel, b"\xff" * 64))
    a.pulse()


def scenario_reconnect_storm(a, game):
    name = "StormRider"
    for _ in range(RECONNECT_ROUNDS):
        with storm_login(game, name) as rider:
            if rider.pos:
                rider.move(*rider.pos)
            rider.send(0x0E, click_body(slot=9))
            rider.socket.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, linger0())
            rider.socket.close()
    # the name still lands cleanly once the storm ends — no ghost, no deadlock
    with storm_login(game, name) as settled:
        settled.pulse()
    a.pulse()


def scenario_power_cut(binary, world):
    name = "ChaosAlice"
    with chaos_server(binary, world, log_name="pre-kill.log", power_cut=True) as (game, proc):
        with ChaosClient(game, name) as a:
            a.spawn()
            a.command("megu claim here", "Claimed chunk")
            a.command("megu claim info", f"claimed by {name}")
            proc.send_signal(signal.SIGKILL)
            proc.wait(timeout=5)
    with chaos_server(binary, world, log_name="post-kill.log") as (game, _):
        with ChaosClient(game, name) as a:
            a.spawn()
            a.command("megu claim info", f"claimed by {name}")
            a.pulse()


def chaos(binary):
    binary = Path(binary).resolve()
    assert binary.is_file(), f"server binary not found: {binary}"
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="midplay-chaos-") as tmp:
        world = Path(tmp)
        with chaos_server(binary, world) as (game, _proc):
            with ChaosClient(game, "ChaosAlice") as a:
                a.spawn()
                print("0/5 bystander online: login + spawn PASS", flush=True)
                scenario_container_chaos(a, game)
                print("1/5 container click & race chaos PASS", flush=True)
                scenario_boundary_chaos(a, game)
                print("2/5 world boundary & coordinate chaos PASS", flush=True)
                scenario_protocol_fuzz(a, game)
                print("3/5 custom protocol fuzzing PASS", flush=True)
                scenario_reconnect_storm(a, game)
                print("4/5 rapid disconnect/reconnect storm PASS", flush=True)
                keepalives = a.keepalives
        scenario_power_cut(binary, world)
        print("5/5 SIGKILL power-cut recovery PASS", flush=True)
    elapsed = time.monotonic() - started
    assert elapsed < 60, f"chaos suite overran: {elapsed:.1f}s"
    print(f"chaos mid-play stress: 5/5 scenarios PASS in {elapsed:.1f}s "
          f"(bystander saw {keepalives} keepalives)")


# ------------------------------------------------------------ self-test ---

class ChaosBuilders(unittest.TestCase):
    def test_click_body_layout(self):
        body = click_body(wid=2, state_id=9, slot=9999, button=1, mode=0)
        r = Reader(body)
        self.assertEqual(r.number("B"), 2)
        self.assertEqual(r.varint(), 9)
        self.assertEqual(r.number(">h"), 9999)
        self.assertEqual(r.number("b"), 1)
        self.assertEqual(r.varint(), 0)
        self.assertEqual(r.varint(), 0)
        self.assertEqual(r.varint(), 0)
        self.assertEqual(r.pos, len(body))

    def test_click_vectors(self):
        vectors = chaos_clicks()
        for key in ("oob_slot", "drop_outside", "negative_count",
                    "stale_state", "wild_mode"):
            self.assertIn(key, vectors)
        self.assertIn(struct.pack(">h", 9999), vectors["oob_slot"])
        self.assertIn(struct.pack(">h", -999), vectors["drop_outside"])
        self.assertIn(b"\xff\xff\xff\xff\x0f", vectors["negative_count"])
        self.assertIn(vint(0x7FFF), vectors["stale_state"])
        self.assertIn(frame_packet(vint(0x0E) + vectors["oob_slot"]),
                      frame_packet(vint(0x0E) + click_body(slot=9999)))

    def test_move_bodies(self):
        self.assertEqual(len(move_pos_body(1, 64, -1)), 25)
        self.assertEqual(len(move_rot_body(0, 64, 0, 90, -45)), 33)
        nan_body = move_pos_body(float("nan"), 0, 0)
        self.assertTrue(math.isnan(struct.unpack(">d", nan_body[:8])[0]))
        rot = move_rot_body(0, 64, 0, float("inf"), 720.0)
        self.assertTrue(math.isinf(struct.unpack(">f", rot[24:28])[0]))

    def test_boundary_vectors(self):
        ys = {y for _, y, _ in BOUNDARIES}
        self.assertIn(-65.0, ys)
        self.assertIn(321.0, ys)
        xs = {x for x, _, _ in BOUNDARIES}
        self.assertIn(29_999_999.0, xs)
        self.assertIn(-29_999_999.0, xs)
        flat = [v for point in BOUNDARIES for v in point]
        self.assertTrue(any(map(math.isnan, flat)))
        self.assertTrue(any(map(math.isinf, flat)))
        # benign and hostile frames wire-frame identically in length
        self.assertEqual(len(frame_packet(vint(0x1A) + move_pos_body(*BOUNDARIES[0]))),
                         len(frame_packet(vint(0x1A) + move_pos_body(0, 64, 0))))

    def test_dig_place_out_of_bounds(self):
        r = Reader(dig_body(0, 8, -65, 8))
        self.assertEqual(r.varint(), 0)
        self.assertEqual(struct.unpack(">Q", r.take(8))[0],
                         struct.unpack(">Q", position(8, -65, 8))[0])
        x, y, z = unpack_position(position(8, -65, 8))
        self.assertEqual((x, y, z), (8, -65, 8))
        self.assertEqual(unpack_position(position(8, 321, 8))[1], 321)

    def test_fuzz_frames_and_channels(self):
        frames = fuzz_payloads()
        for key in ("truncated", "wrong_magic", "oversized", "random", "empty"):
            self.assertIn(key, frames)
        self.assertTrue(frames["wrong_magic"].startswith(b"XXXX"))
        self.assertGreater(len(frames["oversized"]), 32768)
        self.assertEqual(frames["empty"], b"")
        body = payload_body("megu:portal_seamless_v1", frames["truncated"])
        self.assertEqual(Reader(body).string(), "megu:portal_seamless_v1")
        for channel in ("megu:portal_seamless_v1", "megu:subworld_sync_v1",
                        "megu:subworld_helm_v1", "megu:mod_sync_v1",
                        "megu:registry_sync_v1"):
            self.assertIn(channel, CHANNELS)

    def test_reconnect_state_machine(self):
        self.assertEqual(RECONNECT_ROUNDS, 5)
        self.assertEqual(struct.unpack("ii", linger0()), (1, 0))
        for name in ("ClickGremlin", "VoidWalker", "FuzzBot", "StormRider"):
            raw = name.encode()
            self.assertEqual(string(name), vint(len(raw)) + raw)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument("--self-test", action="store_true")
    modes.add_argument("--server-bin", type=Path)
    args = parser.parse_args()
    if args.server_bin:
        chaos(args.server_bin)
    else:
        unittest.main(argv=[__file__], exit=True)
