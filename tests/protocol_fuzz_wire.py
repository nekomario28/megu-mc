#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Seeded, deterministic adversarial Protocol 767 wire fuzz suite for mc-spine.

Launches an isolated loopback mc-spine server (temporary world under
target/tmp, ephemeral ports, RCON enabled, MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS=0)
and drives structured edge-case packets through the live protocol:

  wave 1  malformed framing / VarInt / handshake / configuration abuse
  wave 2  movement NaN/Inf/extreme coordinates (0x1a/0x1b/0x1c/0x1d)
  wave 3  inventory, container and creative-slot boundaries (non-OP survival)
  wave 4  privilege escalation and command injection (non-OP player)
  wave 5  sign / beacon / dig / use-item-on / pick-item abuse

After every wave a status probe must still answer; at the end a fresh clean
client must log in, receive chunks, chat and move, and the server log must
contain no panic/segfault/poison signatures. Persisted playerdata (anvil
playerdata/*.dat) must show survival gamemode, finite positions and no
unauthorized items for every fuzz account.

Usage:
    python3 tests/protocol_fuzz_wire.py [server_binary] [--seed N] [--cases N]
    python3 tests/protocol_fuzz_wire.py --self-test
"""

import argparse
import contextlib
import gzip
import json
import math
import os
import random
import socket
import struct
import subprocess
import sys
import tempfile
import time
import traceback
import zlib
from pathlib import Path

from protocol767 import (Client, Reader, ROOT, frame_packet, position,
                         settings, spawn_window, string, vint)
from wire_smoke import (port, recv_exact, recv_vint, rcon_frame,
                        rcon_response, status)

PROTOCOL = 767
RCON_SECRET = "local-fuzz-wire-secret"
ANVIL_DIR_NAME = "anvil-world"
WORLD_BOUND = 30_000_000.0

# Serverbound play packet ids, from the dispatch table in src/play_protocol.rs.
# NOTE: the task brief calls the sign-edit packet 0x31; in this server 0x31 is
# set_command_minecart and the sign update lives at 0x35. Both are fuzzed.
P_WINDOW_CLICK = 0x0E
P_CLOSE_WINDOW = 0x0F
P_CHAT_COMMAND = 0x04
P_CHAT_SIGNED = 0x05
P_PICK_ITEM = 0x20
P_DIG = 0x24
P_BEACON = 0x2E
P_HELD = 0x2F
P_MOVE_POS = 0x1A
P_MOVE_POS_ROT = 0x1B
P_LOOK = 0x1C
P_FLYING = 0x1D
P_COMMAND_BLOCK = 0x30
P_COMMAND_MINECART = 0x31
P_CREATIVE_SLOT = 0x32
P_JIGSAW = 0x33
P_STRUCTURE = 0x34
P_SIGN_UPDATE = 0x35
P_USE_ITEM_ON = 0x38
P_USE_ITEM = 0x39

CRASH_MARKERS = ("panicked at", "SIGSEGV", "SIGABRT", "SIGBUS",
                 "PoisonError", "poisoned", "RUST_BACKTRACE")

# --------------------------------------------------------------------------
# Wire helpers
# --------------------------------------------------------------------------

def f64(v):
    return struct.pack(">d", float(v))


def f32(v):
    try:
        return struct.pack(">f", float(v))
    except OverflowError:
        return struct.pack(">f", math.copysign(math.inf, float(v)))


def vint_i32(v):
    """protocol VarInt carrying an i32 (u32 payload, sign lives in the cast)."""
    return vint(v & 0xFFFFFFFF)


def vint_i64(v):
    """signed VarLong: the low 64 bits encode the same bytes either way."""
    return vint(v & 0xFFFFFFFFFFFFFFFF)


def hs_body(protocol=PROTOCOL, host=b"localhost", port=25565, nxt=2):
    return (vint(protocol) + vint(len(host)) + host
            + struct.pack(">H", port & 0xFFFF) + vint(nxt))


def login_start(name, uuid=None):
    raw = name if isinstance(name, bytes) else name.encode("utf-8")
    return vint(len(raw)) + raw + (uuid if uuid is not None else bytes(16))


def login_frame(pid, body=b""):
    """Uncompressed login-phase frame: VarInt len + VarInt pid + body."""
    inner = vint(pid) + body
    return vint(len(inner)) + inner


def move_pos_body(x, y, z, on_ground=1):
    return f64(x) + f64(y) + f64(z) + bytes([on_ground & 0xFF])


def move_pos_rot_body(x, y, z, yaw, pitch, on_ground=1):
    return (f64(x) + f64(y) + f64(z) + f32(yaw) + f32(pitch)
            + bytes([on_ground & 0xFF]))


def look_body(yaw, pitch, on_ground=1):
    return f32(yaw) + f32(pitch) + bytes([on_ground & 0xFF])


def flying_body(on_ground=1):
    return bytes([on_ground & 0xFF])


def stack_wire(count=0, item=0, added=(), removed=()):
    """slot_read_stack payload: count first, then item/components when >0."""
    out = vint(count)
    if count:
        out += vint(item & 0xFFFFFFFF) + vint(len(added)) + vint(len(removed))
        for ty, payload in added:
            out += vint(ty) + payload
        for ty in removed:
            out += vint(ty)
    return out


def creative_slot_body(slot, stack):
    return struct.pack(">h", slot) + stack


def click_body(wid=0, state_id=0, slot=0, button=0, mode=0, tail=b""):
    return (bytes([wid & 0xFF]) + vint_i32(state_id)
            + struct.pack(">h", slot) + bytes([button & 0xFF])
            + vint_i32(mode) + tail)


def carried_tail(stack):
    """window_click tail: changedSlots count 0 + carried stack payload."""
    return vint(0) + stack


def dig_body(status=0, x=0, y=0, z=0, face=1, seq=0):
    return (vint_i32(status) + position(x, y, z)
            + bytes([face & 0xFF]) + vint_i32(seq))


def use_item_on_body(hand=0, x=0, y=0, z=0, face=1, cursor=(0.0, 0.0, 0.0),
                     inside=0, seq=0):
    return (vint_i32(hand) + position(x, y, z) + vint_i32(face)
            + b"".join(f32(v) for v in cursor) + bytes([inside & 0xFF])
            + vint_i32(seq))


def beacon_body(primary_marker, primary=None, secondary_marker=None,
                secondary=None):
    """0x2e set_beacon: two Optional<varint> effect slots."""
    out = bytes([primary_marker & 0xFF])
    if primary_marker:
        out += vint_i32(primary if primary is not None else 0)
    out += bytes([(secondary_marker or 0) & 0xFF])
    if secondary_marker:
        out += vint_i32(secondary if secondary is not None else 0)
    return out


def sign_body(x, y, z, front=0, lines=(), raw_lines=None):
    out = position(x, y, z) + bytes([front & 0xFF])
    for i in range(4):
        if raw_lines is not None and i < len(raw_lines):
            line = raw_lines[i]
            if line is None:
                continue
            out += line if isinstance(line, bytes) else string(line)
        elif i < len(lines):
            out += string(lines[i])
        else:
            out += vint(0)
    return out


def command_body(text, declared_len=None, trailing=b""):
    """0x04/0x05 chat_command: varint length + utf8 (no leading slash)."""
    raw = text.encode("utf-8", "surrogateescape") if isinstance(text, str) else text
    n = len(raw) if declared_len is None else declared_len
    return vint(n) + raw + trailing


# --------------------------------------------------------------------------
# Seeded case generation (pure functions of random.Random -> comparable data)
# --------------------------------------------------------------------------

NAN = float("nan")
INF = float("inf")
F64_EDGE = [NAN, INF, -INF, 1e300, -1e300, 3.0e7, -3.0e7, 29999999.0,
            -29999999.0, 2.0e7, -2.0e7, 1e9, -1e9, 5e-324, -0.0,
            1.7976931348623157e308, 64.0, 0.0]
F32_EDGE = [NAN, INF, -INF, 3.0e38, -3.0e38, 1e30, -1e30, 0.0, -0.0, 90.0]


def _raw(data):
    return ("raw", data)


def _pkt(pid, body=b""):
    return ("pkt", pid, body)


def gen_wave1(rng, budget, game_port):
    """Malformed framing / handshake / login / configuration cases.

    Case: {"stage", "sends", "meta"}. `sends` is a list of ("raw", bytes) —
    verbatim socket bytes — or ("pkt", pid, body) — a properly framed packet
    using whatever compression state the session has negotiated.
    """
    by_stage = {}

    def add(stage, sends, meta):
        if isinstance(sends, (bytes, bytearray)):
            sends = [_raw(bytes(sends))]
        elif isinstance(sends, tuple):
            sends = [sends]
        by_stage.setdefault(stage, []).append(
            {"wave": 1, "stage": stage, "sends": list(sends), "meta": meta})

    # -- "pre": bytes seen before/at the first frame ----------------------
    pre = [
        (b"", "empty-write"),
        (b"\x00", "single-zero"),
        (b"\x80", "lone-continuation"),
        (b"\x80\x80\x80", "three-continuation-varint"),
        (b"\xff" * 8, "ff-run"),
        (b"GET / HTTP/1.1\r\n\r\n", "http-probe"),
        (struct.pack(">I", 0x7FFFFFFF) + b"A" * 64, "huge-be-length"),
        (vint(0x1FFFFF) + b"\x00" * 32, "max-frame-claim-partial"),
        (vint(0x1FFFFF) + b"\x00" * 16 + login_frame(0, hs_body()),
         "max-frame-then-valid"),
        (login_frame(0, hs_body())[:-3], "truncated-valid-frame"),
        (login_frame(0), "empty-packet-id-frame"),
        (login_frame(0x7FFFFFFF, b"x"), "giant-packet-id"),
        (login_frame(-1 & 0xFFFFFFFF, b"x"), "negative-packet-id"),
        (vint(5) + b"\x80\x80\x80\x00" + b"rest", "noncanonical-pid-varint"),
    ]
    for data, meta in pre:
        add("pre", data, meta)
    for _ in range(6):
        n = rng.choice([1, 2, 3, 7, 16, 64, 300, 1400])
        add("pre", rng.randbytes(n), f"random-{n}B")
    for _ in range(4):
        add("pre", login_frame(rng.randrange(0, 1 << 20), rng.randbytes(rng.randrange(0, 40))),
            "random-frame")

    # -- "hs": valid frame(0, ...) handshake mutations ---------------------
    hs = [
        (hs_body(), "valid-handshake-control"),
        (hs_body(protocol=0), "proto-0"),
        (hs_body(protocol=1), "proto-1"),
        (hs_body(protocol=765), "proto-765"),
        (hs_body(protocol=766), "proto-766"),
        (hs_body(protocol=768), "proto-768"),
        (hs_body(protocol=999999999), "proto-huge"),
        (vint(1 << 40) + vint(9) + b"localhost" + struct.pack(">H", game_port) + vint(2),
         "proto-6byte-varint"),
        (hs_body(host=b""), "empty-host"),
        (hs_body(host=b"A" * 255), "host-255"),
        (hs_body(host=b"A" * 256), "host-256"),
        (hs_body(host=b"\xff\xfe"), "host-invalid-utf8"),
        (hs_body(host=b"local\x00host"), "host-nul"),
        (hs_body(port=0), "port-0"),
        (hs_body(port=65535), "port-max"),
        (hs_body(nxt=0), "next-state-0"),
        (hs_body(nxt=1), "next-state-1-status"),
        (hs_body(nxt=3), "next-state-3"),
        (hs_body(nxt=99), "next-state-99"),
        (hs_body(nxt=-1 & 0xFFFFFFFF), "next-state-neg"),
        (hs_body()[:-1], "handshake-truncated"),
        (hs_body() + b"\xde\xad\xbe\xef", "handshake-trailing"),
        (hs_body() + login_frame(0, login_start("FuzzW1a")),
         "handshake-plus-loginstart"),
    ]
    for body, meta in hs:
        add("hs", login_frame(0, body), meta)

    # -- "post_hs": bytes after a valid handshake frame --------------------
    post_hs = [
        (login_frame(0, login_start("FuzzW1a")), "loginstart-immediately"),
        (login_frame(0, login_start("")), "loginstart-empty-name"),
        (login_frame(0, login_start("A" * 17)), "loginstart-17char-name"),
        (login_frame(0, login_start("A" * 16)), "loginstart-16char-name"),
        (login_frame(0, login_start("has space")), "loginstart-space-name"),
        (login_frame(0, login_start("bad\x01name")), "loginstart-control-name"),
        (login_frame(0, login_start(b"\xff\xfe\xfd")), "loginstart-invalid-utf8"),
        (login_frame(0, login_start("FuzzW1a")[:-4]), "loginstart-truncated-uuid"),
        (login_frame(0, login_start("FuzzW1a") + b"\x00" * 40),
         "loginstart-extra"),
        (login_frame(0), "loginstart-empty-body"),
        (login_frame(0, vint(300) + b"A" * 4), "loginstart-huge-len"),
        (login_frame(1, login_start("FuzzW1a")), "wrong-packet-id-1"),
        (login_frame(0x7FFFFFFF, b"x"), "giant-pid-after-hs"),
        (vint(0x80) + b"\x80\x80\x80\x80\x80\x01" + login_frame(0, hs_body()),
         "overlong-varint-then-handshake"),
    ]
    for data, meta in post_hs:
        add("post_hs", data, meta)

    # -- "post_login": after Set Compression + Login Success ---------------
    # sends here are ("pkt", pid, body) — framed with the negotiated
    # compression threshold by the session.
    post_login = [
        ([_pkt(3)], "login-ack-empty"),
        ([_pkt(0, hs_body())], "re-handshake-in-login"),
        ([_pkt(4, b"junk")], "unknown-login-pid-4"),
        ([_pkt(3, b"\xff\xff\xff\xff\x0f")], "login-ack-long-body"),
        ([_pkt(3), _pkt(3)], "double-login-ack"),
        ([_raw(vint(0) + b"\x03" + b"x" * 40)], "raw-unframed-junk"),
        ([_pkt(0x1A, move_pos_body(0.5, 64.0, 0.5))],
         "play-packet-in-login-state"),
        ([_pkt(3), _pkt(3, b"x" * 300)], "login-ack-then-oversize"),
    ]
    for sends, meta in post_login:
        add("post_login", sends, meta)

    # -- "config": after login ack, during configuration -------------------
    cfg_info_bad = [
        (settings(2)[:-1], "client-info-truncated"),
        (vint(300) + b"en_us" + bytes([2, 0, 1, 127, 1, 0, 1]),
         "client-info-huge-locale"),
        (vint(5) + b"\xff\xfe\xfd\xfc\xfb" + bytes([2, 0, 1, 127, 1, 0, 1]),
         "client-info-bad-utf8-locale"),
        (vint(5) + b"en_us" + bytes([3, 0, 1, 127, 1, 0, 1]),
         "client-info-view-3"),
        (vint(5) + b"en_us" + bytes([127, 0, 1, 127, 1, 0, 1]),
         "client-info-view-127"),
        (vint(5) + b"en_us" + bytes([2, 3, 1, 127, 1, 0, 1]),
         "client-info-chatmode-3"),
        (vint(5) + b"en_us" + bytes([2, 99, 1, 127, 1, 0, 1]),
         "client-info-chatmode-99"),
        (settings(2) + b"\x00", "client-info-trailing-byte"),
        (b"", "client-info-empty"),
        (vint(0), "client-info-zero-len"),
    ]
    for body, meta in cfg_info_bad:
        add("config", [_pkt(0, body)], "cfg-" + meta)
    cfg_misc = [
        ([_pkt(7, vint(0))], "known-packs-empty"),
        ([_pkt(7, vint(0xFFFFFFFF))], "known-packs-huge-count"),
        ([_pkt(7, b"\xff" * 10)], "known-packs-garbage"),
        ([_pkt(7)], "known-packs-truncated"),
        ([_pkt(1, b"junk")], "cfg-unknown-pid-1"),
        ([_pkt(2, b"junk")], "cfg-unknown-pid-2"),
        ([_pkt(5, b"junk")], "cfg-unknown-pid-5"),
        ([_pkt(6, b"junk")], "cfg-unknown-pid-6"),
        ([_pkt(8, b"junk")], "cfg-unknown-pid-8"),
        ([_pkt(0x7F, b"junk")], "cfg-unknown-pid-127"),
        ([_pkt(0, settings(2)), _pkt(7, vint(0)), _pkt(3, b"early-finish")],
         "config-finish-early"),
        ([_pkt(7, vint(0)), _pkt(0, settings(2))], "known-packs-before-info"),
        ([_pkt(0, settings(2)), _pkt(7, vint(0)), _pkt(3), _pkt(0, settings(2))],
         "config-info-after-finish"),
    ]
    for sends, meta in cfg_misc:
        add("config", sends, meta)

    # -- "play": full login, then frame-level abuse -----------------------
    play = [
        (_raw(b"\x00"), "frame-len-zero"),
        (_raw(b"\x80"), "frame-len-lone-continuation"),
        (_raw(b"\x80\x80\x80"), "frame-len-3byte-continuation"),
        (_raw(b"\x80\x80\x80\x80"), "frame-len-4byte-varint"),
        (_raw(vint(0x1FFFFF) + b"\x00" * 8), "play-max-frame-partial"),
        ([_pkt(0x60)], "play-unknown-pid-0x60"),
        ([_pkt(0x7F, b"junk")], "play-unknown-pid-0x7f"),
        ([_pkt(-1 & 0xFFFFFFFF, b"x")], "play-negative-pid"),
        (_raw(vint(5) + b"\x80\x80\x80\x00" + b"rest"), "play-noncanonical-pid"),
        ([_pkt(0x08, struct.pack(">f", float("nan")))], "chunk-ack-nan"),
        ([_pkt(0x26, b"\xff" * 16)], "keepalive-garbage"),
        ([_pkt(0x00, b"")], "teleport-confirm-empty"),
        ([_pkt(0x00, b"\xff" * 8)], "teleport-confirm-garbage"),
    ]
    for sends, meta in play:
        add("play", sends, meta)

    # seeded mutations of well-formed material, distributed across stages
    mutation_seeds = {
        "hs": [login_frame(0, hs_body()), login_frame(0, hs_body(protocol=766))],
        "post_hs": [login_frame(0, login_start("FuzzW1a"))],
        "config": [login_frame(0, hs_body())],
        "play": [login_frame(0, hs_body())],
    }
    stages = ["hs", "post_hs", "config", "play"]
    for i in range(max(0, budget // 5)):
        stage = stages[i % len(stages)]
        base = bytearray(rng.choice(mutation_seeds[stage]))
        for _ in range(rng.randrange(1, 5)):
            base[rng.randrange(len(base))] = rng.randrange(256)
        if rng.random() < 0.4 and len(base) > 2:
            base = base[:rng.randrange(1, len(base))]
        add(stage, bytes(base), f"mutated-{stage}-{i}")

    # allocate budget per stage, preserving coverage of every stage
    quotas = {"pre": 0.16, "hs": 0.18, "post_hs": 0.16, "post_login": 0.10,
              "config": 0.22, "play": 0.18}
    out = []
    remaining = budget
    stage_items = list(by_stage.items())
    # first pass: every stage gets its quota share (or all it has)
    deferred = []
    for stage, items in stage_items:
        rng.shuffle(items)
        want = max(3, int(round(budget * quotas.get(stage, 0.1))))
        take = min(len(items), want, max(0, remaining))
        out.extend(items[:take])
        remaining -= take
        deferred.extend(items[take:])
    # second pass: distribute leftovers round-robin
    di = 0
    while remaining > 0 and deferred:
        out.append(deferred[di % len(deferred)])
        deferred.pop(di % len(deferred))
        di += 1
        remaining -= 1
    rng.shuffle(out)
    return out[:budget] if len(out) > budget else out


def gen_wave2(rng, budget):
    """Movement NaN/Inf/extreme-coordinate cases.

    Each case: {"pid", "body", "expect"} where expect is "reject" (server must
    drop the connection via ingress_guard) or "survive" (connection stays up
    and the server still answers).
    """
    cases = []

    def expect_for(pid, body):
        if pid in (P_MOVE_POS, P_MOVE_POS_ROT):
            req = 33 if pid == P_MOVE_POS_ROT else 25
            if len(body) != req:
                return "reject"
            for off in (0, 8, 16):
                v = struct.unpack(">d", body[off:off + 8])[0]
                if not math.isfinite(v) or abs(v) > WORLD_BOUND:
                    return "reject"
            if pid == P_MOVE_POS_ROT:
                yaw = struct.unpack(">f", body[24:28])[0]
                pitch = struct.unpack(">f", body[28:32])[0]
                if not math.isfinite(yaw) or not math.isfinite(pitch):
                    return "reject"
        return "survive"

    def add(pid, body, meta):
        cases.append({"wave": 2, "pid": pid, "body": body,
                      "expect": expect_for(pid, body), "meta": meta})

    base = (0.5, 64.0, 0.5)
    for i, v in enumerate(F64_EDGE):
        for axis in range(3):
            coords = list(base)
            coords[axis] = v
            add(P_MOVE_POS, move_pos_body(*coords), f"0x1a-axis{axis}-v{i}")
    combos = [(NAN, NAN, NAN), (INF, 64.0, 0.0), (0.0, 1e9, 0.0),
              (3e7, 64.0, 3e7), (-3e7, -1e9, -3e7), (2e7, 2e7, 2e7),
              (-2e7, -2e7, -2e7), (29999999.0, 319.0, -29999999.0),
              (5e-324, -0.0, 1.7976931348623157e308)]
    for i, c in enumerate(combos):
        add(P_MOVE_POS, move_pos_body(*c), f"0x1a-combo{i}")
    for i, v in enumerate(F32_EDGE):
        add(P_MOVE_POS_ROT, move_pos_rot_body(0.5, 64.0, 0.5, v, 0.0),
            f"0x1b-yaw{i}")
        add(P_MOVE_POS_ROT, move_pos_rot_body(0.5, 64.0, 0.5, 0.0, v),
            f"0x1b-pitch{i}")
    for i, c in enumerate(combos[:6]):
        add(P_MOVE_POS_ROT, move_pos_rot_body(*c, 45.0, 10.0), f"0x1b-coord{i}")
    for n in (0, 1, 8, 24, 26, 32, 34, 40):
        add(P_MOVE_POS, rng.randbytes(n), f"0x1a-len{n}")
        add(P_MOVE_POS_ROT, rng.randbytes(n), f"0x1b-len{n}")
    for i, v in enumerate(F32_EDGE):
        add(P_LOOK, look_body(v, 0.0), f"0x1c-yaw{i}")
        add(P_LOOK, look_body(0.0, v), f"0x1c-pitch{i}")
    for n in (0, 1, 8, 10, 12):
        add(P_LOOK, rng.randbytes(n), f"0x1c-len{n}")
    for i in range(4):
        add(P_FLYING, rng.randbytes(rng.randrange(0, 4)), f"0x1d-{i}")

    rng.shuffle(cases)
    return cases[:budget] if budget < len(cases) else cases


def gen_wave3(rng, budget):
    """Inventory / container / creative-slot boundary cases (non-OP survival)."""
    cases = []

    def add(kind, body, meta):
        cases.append({"wave": 3, "kind": kind, "body": body, "meta": meta})

    stacks = [
        stack_wire(0),
        stack_wire(1, 1),
        stack_wire(64, 999),
        stack_wire(1, 1, added=[(48, vint(4) + b"name")]),
        stack_wire(1, 1, added=[(48, vint(9000) + b"A" * 9000)]),
        stack_wire(1, 1, added=[(48, vint(40000) + b"B" * 40000)]),
        stack_wire(1, 1, added=[(200, b"\x00")]),
        stack_wire(1, 1, added=[(57, b"\x00")]),
        stack_wire(128, 1),
        stack_wire(1, -1 & 0xFFFFFFFF),
        stack_wire(1, 1, added=[(48, vint(98301) + b"C" * 98301)]),
        stack_wire(1, 1, removed=[1, 2, 3]),
        stack_wire(1, 1, added=[(0, b"\x0a\x00\x00")]),
        b"\x00" * 5,
        b"\xff" * 32,
    ]
    for i, st in enumerate(stacks):
        for slot in (0, 9, 36, 44, 45, 46, -1, -999, 30000):
            add("creative", creative_slot_body(slot, st), f"creative-{i}-slot{slot}")
    for n in (8192, 9000, 32768):
        big = stack_wire(1, 1, added=[(48, vint(n) + b"D" * n)])
        add("creative", creative_slot_body(9, big), f"creative-big-{n}")

    slots = [-999, -32768, -2, -1, 0, 1, 2, 3, 4, 5, 8, 9, 17, 36, 44, 45,
             46, 100, 30000, 32767]
    modes = [0, 1, 2, 4, 5, 6, 7, 8, 100, 255, -1 & 0xFFFFFFFF]
    combos = [(s, m) for s in slots for m in modes]
    rng.shuffle(combos)
    click_budget = max(0, budget // 2)
    for i, (slot, mode) in enumerate(combos[:click_budget]):
        add("click", click_body(0, 0, slot, 0, mode, carried_tail(stack_wire(0))),
            f"click-{i}-s{slot}-m{mode}")
    for sid in (0, 1, -1 & 0xFFFFFFFF, 0x7FFFFFFF):
        for wid in (0, 1, 2, 3, 4, 255):
            add("click", click_body(wid, sid, 9, 0, 0, carried_tail(stack_wire(0))),
                f"click-sid{sid}-wid{wid}")
    for btn in (0, 1, 2, 5, 6, 255):
        add("click", click_body(0, 0, 9, btn, 0, carried_tail(stack_wire(0))),
            f"click-btn{btn}")
    tails = [
        b"",
        b"\x00",
        b"\x01\x02\x03\x04\x05",
        vint(1) + b"garbage",
        carried_tail(stack_wire(1, 1, added=[(48, vint(9000) + b"E" * 9000)])),
        carried_tail(stack_wire(1, 1, added=[(48, vint(40000) + b"F" * 40000)])),
        carried_tail(stack_wire(128, 1)),
        carried_tail(stack_wire(1, -1 & 0xFFFFFFFF)),
        carried_tail(stack_wire(1, 1)[:-1]),
        b"\xff" * 64,
        rng.randbytes(128),
    ]
    for i, tail in enumerate(tails):
        add("click", click_body(0, 0, 9, 0, 0, tail), f"click-tail{i}")
        add("click", click_body(0, 0, 0, 0, 0, tail), f"click-tail{i}-craft")
    for i in range(10):
        seq = []
        for _ in range(rng.randrange(3, 7)):
            seq.append(click_body(
                0, 0, rng.choice([0, 1, 9, 10, 36, 44, 45, -999]),
                rng.choice([0, 1, 2]), rng.choice([0, 1, 2, 4, 5, 6]),
                carried_tail(stack_wire(0))))
        cases.append({"wave": 3, "kind": "seq", "bodies": seq,
                      "meta": f"click-seq-{i}"})
    add("close", b"", "close-window-empty")
    add("close", bytes([0]), "close-window-0")
    add("close", bytes([255]), "close-window-255")

    rng.shuffle(cases)
    return cases[:budget] if budget < len(cases) else cases


WAVE4_COMMANDS = [
    # privilege / gamemode
    "op FuzzW4a", "op FuzzW4b", "deop FuzzW4a", "gamemode creative",
    "gamemode creative FuzzW4a", "gamemode spectator @a", "gamemode 1 FuzzW4a",
    "gamemode adventure FuzzW4a extra", "defaultgamemode creative",
    # world edits aimed at / near the probe region
    "setblock 5 205 5 minecraft:air", "setblock 5 205 5 minecraft:gold_block",
    "setblock 100000 200 100000 minecraft:stone",
    "fill 0 200 0 15 215 15 minecraft:air", "fill 20 200 0 35 215 15 minecraft:tnt",
    "fill 0 200 0 15 215 15 minecraft:stone hollow",
    "fillbiome 0 200 0 15 215 15 minecraft:plains minecraft:desert",
    "clone 0 200 0 15 215 15 40 200 0", "place structure probe",
    # items
    "give FuzzW4a diamond 64", "give @a diamond", "give @p minecraft:diamond_sword",
    "give FuzzW4a minecraft:command_block",
    "item replace entity FuzzW4a weapon.mainhand with minecraft:diamond_sword",
    "clear FuzzW4a", "clear @a", "loot give FuzzW4a loot minecraft:chests/simple_dungeon",
    # execute pipelines
    "execute as @a run setblock 5 205 5 minecraft:air",
    "execute at @s run give @s diamond",
    "execute if block 5 205 5 minecraft:stone run setblock 5 205 5 minecraft:air",
    "execute positioned 0 200 0 run fill 0 200 0 1 200 1 minecraft:air",
    "execute store result score probe probe run time query gametime",
    "execute summon minecraft:zombie",
    "execute as @a[gamemode=creative] run give @s diamond",
    # datapack / reload / server control
    "datapack disable file/bukkit", "datapack enable file/bukkit",
    "datapack list available enabled", "reload", "save-all flush", "save-off",
    "stop", "publish", "seed", "counts",
    # gamerules / time / weather / difficulty
    "gamerule doDaylightCycle false", "gamerule keepInventory true",
    "gamerule mobGriefing false", "gamerule doImmediateRespawn true",
    "gamerule", "time set 0", "time set day", "time add 1000",
    "time query daytime", "tick freeze", "tick sprint", "tick rate 40",
    "weather clear", "weather rain 1000", "difficulty hard",
    "difficulty peaceful",
    # entities / players
    "kill @a", "kill FuzzW4a", "damage FuzzW4a 100 minecraft:generic",
    "effect give @a minecraft:speed 999 255", "effect clear @a",
    "enchant @s minecraft:sharpness 255", "experience add @a 100 levels",
    "xp add FuzzW4a 100 levels", "scoreboard objectives add probe dummy",
    "scoreboard players set FuzzW4a probe 100", "team add probe",
    "tag @a add pwned", "advancement grant @a everything",
    "recipe give @a *", "summon minecraft:zombie ~ ~ ~",
    "summon minecraft:tnt ~ ~ ~", "function minecraft:probe",
    "schedule function probe 1t", "forceload add 0 0",
    "spreadplayers 0 0 0 1 1 false @a", "tp FuzzW4a 0 200 0",
    "tp @a 0 200 0", "teleport FuzzW4a 100 200 100",
    "ride @e[type=zombie] mount @s", "spectate FuzzW4a", "kick FuzzW4a",
    "ban FuzzW4a", "pardon FuzzW4a", "whitelist add FuzzW4a", "whitelist on",
    "setworldspawn 0 200 0", "spawnpoint FuzzW4a 0 200 0",
    # data / nbt writes
    "data merge block 5 205 5 {id:\"minecraft:sign\"}",
    "data modify entity FuzzW4a Pos set value [0.0d,300.0d,0.0d]",
    "data merge entity FuzzW4a {playerGameType:1}",
    "data remove entity FuzzW4a Inventory", "data get entity FuzzW4a Pos",
    # misc admin
    "attribute @s minecraft:generic.movement_speed base set 10",
    "bossbar add probe {\"text\":\"x\"}", "bossbar set probe players @a",
    "jfr start", "perf", "debug start", "watchdog", "megu policy set all",
    # malformed / injection-ish
    "op", "gamemode", "setblock", "nonexistent_command_xyz",
    "/op FuzzW4a", " op FuzzW4a", "OP FuzzW4a", "o\u00a0p FuzzW4a",
    "op \"FuzzW4a\"", "op 'FuzzW4a'", "execute run op FuzzW4a",
    "execute as @a run op FuzzW4a", "execute if entity @a run gamemode creative @a",
    "say hi\" ; op FuzzW4a", "msg FuzzW4b line1\nline2",
    "unknowncmd arg1 arg2 arg3", "help op", "help gamemode",
    "execute run " * 12 + "setblock 5 205 5 minecraft:air",
    # public controls (must keep working)
    "list", "me fuzz probe", "msg FuzzW4b hello", "w FuzzW4b hi",
    "tell FuzzW4b yo", "teammsg fuzz", "tm fuzz", "trigger probe",
    "trigger probe add",
]

WAVE4_DISCONNECT_CASES = [
    ("A" * 257, "cmd-257-chars-disconnect"),
    ("op " + "B" * 400, "cmd-oversized-disconnect"),
    ("say \u00a7cRED", "cmd-section-sign-disconnect"),
    ("say \u0007bell", "cmd-control-char-disconnect"),
    ("say \u007fdel", "cmd-del-char-disconnect"),
    (b"op \xff\xfe badutf8", "cmd-invalid-utf8"),
    ("A" * 256, "cmd-256-chars-boundary"),
]


def gen_wave4(rng, budget):
    """Privilege-escalation / command-injection cases."""
    cases = []
    pool = list(WAVE4_COMMANDS)
    rng.shuffle(pool)
    n_cmd = max(0, budget - len(WAVE4_DISCONNECT_CASES) - 4)
    for i, cmd in enumerate(pool[:n_cmd]):
        via = 6 if rng.random() < 0.25 else 4
        # chat_text_valid rejects §, C0 controls, DEL and >256 chars with a
        # disconnect — a safe rejection, so tolerate it for those commands.
        illegal = (len(cmd) > 256 or any(
            ch == "§" or ch < " " or ch == "\x7f" for ch in cmd))
        cases.append({"wave": 4, "cmd": cmd, "via": via,
                      "disconnect_ok": illegal, "meta": f"cmd-{i}"})
    for cmd, meta in WAVE4_DISCONNECT_CASES:
        raw = cmd if isinstance(cmd, bytes) else None
        cases.append({"wave": 4, "cmd": None if raw is not None else cmd,
                      "raw": raw, "via": 4, "disconnect_ok": True,
                      "meta": meta})
    for i in range(4):
        text = rng.choice(["op FuzzW4a", "give FuzzW4a diamond", "list"])
        raw_text = text.encode()
        declared = max(0, len(raw_text) - rng.randrange(0, 3))
        cases.append({"wave": 4, "cmd": None,
                      "raw": command_body(text, declared_len=declared,
                                          trailing=rng.randbytes(rng.randrange(0, 6))),
                      "via": 4, "disconnect_ok": False,
                      "meta": f"cmd-len-diff-{i}"})
    rng.shuffle(cases)
    return cases[:budget] if budget < len(cases) else cases


def gen_wave5(rng, budget):
    """Sign / beacon / dig / use-item-on / pick-item / command-block abuse."""
    cases = []

    def add(pid, body, meta):
        cases.append({"wave": 5, "pid": pid, "body": body, "meta": meta})

    sign_coords = [(0, 70, 0), (1, 65, 1), (100000, 70, 100000),
                   (-5000, 300, 42), (0, -60, 0), (30000000, 70, -30000000)]
    line_sets = [
        ["a", "b", "c", "d"],
        ["", "", "", ""],
        ["x" * 256, "y" * 300, "z", "w"],
        ["\u00a7cred", "\u0001ctl", "\U0001f600emoji", "tail"],
        ["line1", "line2"],
        ["l1", "l2", "l3", "l4", "l5extra"],
    ]
    for i, (x, y, z) in enumerate(sign_coords):
        for j, lines in enumerate(line_sets):
            add(P_SIGN_UPDATE, sign_body(x, y, z, j % 2, lines),
                f"sign-{i}-{j}")
    add(P_SIGN_UPDATE, position(0, 70, 0) + b"\x00" + vint(0xFFFFFFFF)
        + b"A" * 4, "sign-huge-line-len")
    add(P_SIGN_UPDATE, position(0, 70, 0) + b"\x00"
        + vint(5) + b"\xff\xfe\xfd\xfc\xfb" + vint(0) + vint(0) + vint(0),
        "sign-bad-utf8-line")
    add(P_SIGN_UPDATE, position(0, 70, 0) + b"\x02" + vint(1) + b"x" * 3,
        "sign-front-2-truncated")
    add(P_SIGN_UPDATE, b"\xff" * 8 + b"\x00" + vint(0) * 4, "sign-ff-pos")
    add(P_SIGN_UPDATE, position(0, 70, 0) + b"\x00" + b"\x00" * 4,
        "sign-empty-lines")
    add(P_SIGN_UPDATE, b"\x00" * 30, "sign-zeros")
    for pid, name in ((P_COMMAND_MINECART, "minecart"), (P_COMMAND_BLOCK, "cmdbk"),
                      (P_JIGSAW, "jigsaw"), (P_STRUCTURE, "struct")):
        for i in range(3):
            add(pid, rng.randbytes(rng.randrange(0, 80)), f"{name}-rand{i}")
        add(pid, position(0, 70, 0) + bytes([0]) + string("say hi") + b"\x00\x00",
            f"{name}-plausible")
    for i, body in enumerate([
        beacon_body(0, 0, 0),
        beacon_body(1, 1, 0),
        beacon_body(0, 0, 1, 16),
        beacon_body(1, -1, 1, -1),
        beacon_body(1, 999999, 1, -999999),
        beacon_body(1, 0x7FFFFFFF, 1, -0x80000000),
        beacon_body(2, 1, 0),
        beacon_body(1, 1),
        beacon_body(1, 1, 2, 2),
        bytes([1]) + vint_i64(1),
        bytes([1]) + b"\xff" * 9 + b"\x01" + bytes([0]),
        b"",
        b"\xff" * 12,
        rng.randbytes(9),
    ]):
        add(P_BEACON, body, f"beacon-{i}")
    dig_statuses = [0, 1, 2, 3, 4, 5, 6, 7, 255, -1, -999, 0x7FFFFFFF,
                    -0x80000000]
    dig_faces = [-1, 0, 1, 2, 3, 4, 5, 6, 7, 255]
    dig_coords = [(0, 70, 0), (0, 71, 0), (0, -60, 0), (100000, 70, 100000),
                  (-30000000, 319, 30000000), (0, 320, 0)]
    i = 0
    for status in dig_statuses:
        x, y, z = dig_coords[i % len(dig_coords)]
        add(P_DIG, dig_body(status, x, y, z, dig_faces[i % len(dig_faces)], i),
            f"dig-status{status}")
        i += 1
    for j, (x, y, z) in enumerate(dig_coords):
        add(P_DIG, dig_body(2, x, y, z, 1, j), f"dig-finish-{j}")
        add(P_DIG, dig_body(0, x, y, z, 255, j), f"dig-start-badface-{j}")
    add(P_DIG, b"\xff" * 5 + position(0, 70, 0) + b"\x01" + b"\xff" * 5,
        "dig-varint-overflow")
    add(P_DIG, b"", "dig-empty")
    add(P_DIG, b"\x00" * 3, "dig-tiny")
    add(P_DIG, vint_i32(2) + position(0, 70, 0) + b"\x01", "dig-no-seq")
    add(P_DIG, vint_i32(2) + position(0, 70, 0) + b"\x01" + b"\xff" * 5,
        "dig-huge-seq")
    cursors = [(0.0, 0.0, 0.0), (NAN, 0.0, 0.0), (INF, -INF, 5.0),
               (1e30, -1e30, 3.4e38), (5.0, 5.0, 5.0)]
    for i, cur in enumerate(cursors):
        add(P_USE_ITEM_ON, use_item_on_body(0, 0, 70, 0, 1, cur, 0, i),
            f"useitem-cursor{i}")
        add(P_USE_ITEM_ON, use_item_on_body(1, 0, 70, 0, 2, cur, 255, i),
            f"useitem-hand1-cursor{i}")
    for face in (-1, 0, 1, 5, 6, 7, 255, -999):
        add(P_USE_ITEM_ON, use_item_on_body(0, 0, 70, 0, face,
                                          (0.5, 0.5, 0.5), 0, 0),
            f"useitem-face{face}")
    for hand in (-1, 0, 1, 2, 255, 999999):
        add(P_USE_ITEM_ON, use_item_on_body(hand, 0, 70, 0, 1),
            f"useitem-hand{hand}")
    add(P_USE_ITEM_ON, use_item_on_body(0, -1, 255, -1, -1, (0, 0, 0), 0, 0),
        "useitem-air-sentinel")
    add(P_USE_ITEM_ON, use_item_on_body(0, -1, 255, -1, 0),
        "useitem-sentinel-face0")
    add(P_USE_ITEM_ON, b"", "useitem-empty")
    add(P_USE_ITEM_ON, b"\x00" * 4, "useitem-tiny")
    add(P_USE_ITEM_ON, use_item_on_body(0, 0, 70, 0, 1)[:-3],
        "useitem-truncated")
    for i, body in enumerate([vint_i32(0) + vint_i32(0),
                              vint_i32(1) + vint_i32(1),
                              vint_i32(255) + vint_i32(-1), b"", b"\xff" * 8,
                              b"\xff" * 5 + b"\x00" * 8]):
        add(P_USE_ITEM, body, f"useitem-air-{i}")
    for i, body in enumerate([b"", vint_i32(0), vint_i32(-1), vint_i32(9999),
                              b"\xff" * 8, rng.randbytes(6)]):
        add(P_PICK_ITEM, body, f"pickitem-{i}")
    for i, slot in enumerate([-32768, -1, 0, 8, 9, 300, 32767]):
        add(P_HELD, struct.pack(">h", slot), f"held-{slot}")
    add(P_HELD, b"\x00", "held-1byte")
    add(P_HELD, b"", "held-empty")
    add(P_CLOSE_WINDOW, bytes([0]), "closew-0")

    rng.shuffle(cases)
    return cases[:budget] if budget < len(cases) else cases


def allocate_cases(seed, total):
    """Distribute `total` cases across waves 1..5 deterministically."""
    rng = random.Random(seed ^ 0x5EED)
    weights = [0.20, 0.20, 0.26, 0.22, 0.12]
    mins = [12, 10, 16, 14, 8]
    counts = [max(mins[i], int(total * weights[i])) for i in range(5)]
    while sum(counts) > total:
        i = max(range(5), key=lambda j: counts[j] - mins[j])
        if counts[i] <= mins[i]:
            break
        counts[i] -= 1
    return counts


def all_cases(seed, total, game_port=25565):
    counts = allocate_cases(seed, total)
    return {
        1: gen_wave1(random.Random(seed + 1), counts[0], game_port),
        2: gen_wave2(random.Random(seed + 2), counts[1]),
        3: gen_wave3(random.Random(seed + 3), counts[2]),
        4: gen_wave4(random.Random(seed + 4), counts[3]),
        5: gen_wave5(random.Random(seed + 5), counts[4]),
    }


# --------------------------------------------------------------------------
# Server fixture
# --------------------------------------------------------------------------

@contextlib.contextmanager
def fuzz_server(binary, root):
    """Isolated mc-spine: loopback, RCON, anvil playerdata, admin cmds OFF."""
    game, rcon = port(), port()
    while rcon == game:
        rcon = port()
    anvil = root / ANVIL_DIR_NAME
    anvil.mkdir(parents=True, exist_ok=True)
    env = dict(
        os.environ,
        MC_SPINE_PORT=str(game),
        MC_SPINE_BIND_ADDR="127.0.0.1",
        MC_SPINE_RCON_PORT=str(rcon),
        MC_SPINE_RCON_BIND_ADDR="127.0.0.1",
        MC_SPINE_RCON_PASS=RCON_SECRET,
        MC_SPINE_WG="old",
        MC_SPINE_RESOURCE_PACK_URL="",
        MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS="0",
        MC_SPINE_ANVIL_WORLD_DIR=str(anvil),
        MC_SPINE_SEED="0",
    )
    env.pop("MC_SPINE_WAL_PATH", None)
    log_path = root / "server.log"
    with log_path.open("w+") as log:
        proc = subprocess.Popen([str(binary)], cwd=root, env=env,
                                stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                assert proc.poll() is None, "server exited at startup"
                try:
                    status(game)
                    with socket.create_connection(("127.0.0.1", rcon), timeout=2):
                        break
                except OSError:
                    time.sleep(0.05)
            else:
                raise AssertionError("listeners did not become ready")
            yield {"game": game, "rcon": rcon, "root": root, "proc": proc,
                   "log": log_path, "anvil": anvil}
        except Exception:
            log.flush()
            log.seek(0)
            print(log.read()[-6000:], file=sys.stderr)
            raise
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)


def link_server_assets(root):
    for link, target in (("config_packets.bin", ROOT / "config_packets.bin"),
                         ("assets", ROOT / "assets"),
                         ("datapacks", ROOT / "datapacks")):
        if target.exists() and not (root / link).exists():
            (root / link).symlink_to(target, target_is_directory=target.is_dir())


# --------------------------------------------------------------------------
# RCON
# --------------------------------------------------------------------------

def rcon_command(rcon_port, text, timeout=10):
    with socket.create_connection(("127.0.0.1", rcon_port), timeout=timeout) as conn:
        conn.settimeout(timeout)
        conn.sendall(rcon_frame(1, 3, RCON_SECRET))
        assert rcon_response(conn) == struct.pack("<ii", 1, 2) + b"\0\0", \
            "RCON auth failed"
        conn.sendall(rcon_frame(2, 2, text))
        reply = rcon_response(conn)
        assert reply[:8] == struct.pack("<ii", 2, 0) and reply.endswith(b"\0\0"), reply
        return reply[8:-2].decode("utf-8", "replace")


def rcon_try(rcon_port, text, timeout=10):
    try:
        return rcon_command(rcon_port, text, timeout)
    except (OSError, AssertionError) as exc:
        return f"<rcon-error: {exc}>"


# --------------------------------------------------------------------------
# Persisted-state checks (pure detectors — also exercised by --self-test)
# --------------------------------------------------------------------------

class Nbt:
    """Minimal named-root NBT reader for anvil playerdata/*.dat (gzip)."""

    def __init__(self, data):
        self.d, self.p = data, 0

    def take(self, n):
        if n < 0 or self.p + n > len(self.d):
            raise ValueError("NBT truncated")
        out = self.d[self.p:self.p + n]
        self.p += n
        return out

    def u8(self):
        return self.take(1)[0]

    def i16(self):
        return struct.unpack(">h", self.take(2))[0]

    def i32(self):
        return struct.unpack(">i", self.take(4))[0]

    def i64(self):
        return struct.unpack(">q", self.take(8))[0]

    def f32(self):
        return struct.unpack(">f", self.take(4))[0]

    def f64(self):
        return struct.unpack(">d", self.take(8))[0]

    def text(self):
        return self.take(struct.unpack(">H", self.take(2))[0]).decode("utf-8")

    def payload(self, tag, depth=0):
        if depth > 64:
            raise ValueError("NBT too deep")
        if tag == 1:
            return struct.unpack("b", self.take(1))[0]
        if tag == 2:
            return self.i16()
        if tag == 3:
            return self.i32()
        if tag == 4:
            return self.i64()
        if tag == 5:
            return self.f32()
        if tag == 6:
            return self.f64()
        if tag == 7:
            return self.take(self.i32())
        if tag == 8:
            return self.text()
        if tag == 9:
            child, n = self.u8(), self.i32()
            if not 0 <= n <= 1_048_576:
                raise ValueError("NBT list bound")
            return [self.payload(child, depth + 1) for _ in range(n)]
        if tag == 10:
            out = {}
            while True:
                child = self.u8()
                if child == 0:
                    return out
                name = self.text()
                out[name] = self.payload(child, depth + 1)
        if tag == 11:
            n = self.i32()
            if not 0 <= n <= 1_048_576:
                raise ValueError("NBT int array bound")
            return [self.i32() for _ in range(n)]
        if tag == 12:
            n = self.i32()
            if not 0 <= n <= 1_048_576:
                raise ValueError("NBT long array bound")
            return [self.i64() for _ in range(n)]
        raise ValueError(f"unknown NBT tag {tag}")

    def root(self):
        tag = self.u8()
        name_len = struct.unpack(">H", self.take(2))[0]
        self.take(name_len)
        return self.payload(tag)


def read_playerdata(path):
    raw = gzip.decompress(Path(path).read_bytes())
    return Nbt(raw).root()


def record_violations(name, nbt, inventory_baseline=None):
    """Invariant checks on one parsed playerdata record. Returns violation list.

    `inventory_baseline`: optional expected item count; when None the record
    must have an empty Inventory (fresh survival players gain nothing here).
    """
    v = []
    if not isinstance(nbt, dict):
        return [f"{name}: playerdata root is not a compound"]
    gm = nbt.get("playerGameType")
    if gm != 0:
        v.append(f"{name}: playerGameType={gm!r} (expected survival 0)")
    pos = nbt.get("Pos")
    if not (isinstance(pos, list) and len(pos) == 3):
        v.append(f"{name}: Pos missing or malformed: {pos!r}")
    else:
        for axis, val in zip("xyz", pos):
            if not isinstance(val, float) or not math.isfinite(val):
                v.append(f"{name}: Pos.{axis}={val!r} not finite")
            elif abs(val) > WORLD_BOUND:
                v.append(f"{name}: Pos.{axis}={val!r} beyond world bound")
    rot = nbt.get("Rotation")
    if isinstance(rot, list) and rot:
        for i, val in enumerate(rot):
            if isinstance(val, float) and not math.isfinite(val):
                v.append(f"{name}: Rotation[{i}]={val!r} not finite")
    inv = nbt.get("Inventory", [])
    count = len(inv) if isinstance(inv, list) else -1
    if inventory_baseline is None:
        if count != 0:
            v.append(f"{name}: Inventory has {count} entries (expected empty)")
    elif count != inventory_baseline:
        v.append(f"{name}: Inventory has {count} entries "
                 f"(baseline {inventory_baseline})")
    dim = nbt.get("Dimension")
    if dim not in (None, "minecraft:overworld"):
        v.append(f"{name}: Dimension={dim!r}")
    return v


def read_ops_files(root):
    """Snapshot ops/ban/whitelist JSON files (absent == empty list)."""
    out = {}
    for name in ("ops.json", "whitelist.json", "banned-players.json",
                 "banned-ips.json"):
        p = root / name
        try:
            out[name] = json.loads(p.read_text()) if p.is_file() else []
        except (OSError, ValueError):
            out[name] = f"<unreadable:{p}>"
    return out


def scan_log_text(text):
    return [ln for ln in text.splitlines()
            if any(m in ln for m in CRASH_MARKERS)]


def scan_log(path):
    try:
        return scan_log_text(Path(path).read_text(errors="replace"))
    except OSError:
        return ["<server log unreadable>"]


# --------------------------------------------------------------------------
# Raw protocol session for wave 1
# --------------------------------------------------------------------------

class RawSession:
    """One TCP connection tracked through compression-state transitions."""

    def __init__(self, game):
        self.sock = socket.create_connection(("127.0.0.1", game), timeout=5)
        self.sock.settimeout(5)
        self.threshold = -1

    def send_raw(self, data):
        self.sock.sendall(data)

    def send_frame(self, pid, body=b""):
        """A packet framed with the negotiated compression state."""
        self.sock.sendall(frame_packet(vint(pid) + body, self.threshold))

    def read_packet(self, timeout=5.0):
        """Next server packet as (pid, body); None on EOF/timeout/garbage."""
        self.sock.settimeout(timeout)
        try:
            n = recv_vint(self.sock)
            data = recv_exact(self.sock, n)
            inner = unframe_packet_safe(data, self.threshold)
            reader = Reader(inner)
            pid = reader.varint()
            return pid, reader.take(len(reader.data) - reader.pos)
        except (OSError, AssertionError, EOFError, ValueError, zlib.error):
            return None

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


def unframe_packet_safe(frame_data, threshold=-1):
    """unframe_packet that never asserts on hostile compressed input."""
    if threshold < 0:
        return frame_data
    reader = Reader(frame_data)
    data_len = reader.varint()
    rest = reader.take(len(reader.data) - reader.pos)
    if data_len == 0:
        return rest
    if not (0 <= data_len <= 16 * 1024 * 1024):
        raise ValueError("bad compressed length")
    packet = zlib.decompress(rest)
    if len(packet) != data_len:
        raise ValueError("decompressed length mismatch")
    return packet


def advance_to_login_success(sess, game, name="FuzzW1a", uuid=None):
    """Drive handshake + login start; True once Login Success arrives."""
    sess.send_raw(login_frame(0, hs_body(PROTOCOL, b"localhost", game, 2)))
    sess.send_raw(login_frame(0, login_start(name, uuid)))
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        pkt = sess.read_packet(timeout=max(0.1, deadline - time.monotonic()))
        if pkt is None:
            return False
        pid, body = pkt
        if pid == 0x03:
            try:
                sess.threshold = Reader(body).varint()
            except AssertionError:
                pass
        elif pid == 0x02:
            return True
    return False


def drain_config_start(sess, timeout=4.0):
    """After login-ack, consume the config burst through its 0x0e marker."""
    sess.send_frame(3)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        pkt = sess.read_packet(timeout=max(0.1, deadline - time.monotonic()))
        if pkt is None:
            return False
        if pkt[0] == 0x0E:
            return True
    return False


def advance_to_play(sess):
    """login -> config -> play. True once the play state is entered."""
    sess.send_frame(0, settings(2))
    sess.send_frame(7, vint(0))
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        pkt = sess.read_packet(timeout=max(0.1, deadline - time.monotonic()))
        if pkt is None:
            return False
        if pkt[0] == 0x03:
            sess.send_frame(3)
            return True
    return False


def run_wave1(srv, cases, problems, notes):
    game = srv["game"]
    done = 0
    for idx, case in enumerate(cases):
        stage, sends, meta = case["stage"], case["sends"], case["meta"]
        sess = None
        try:
            if stage == "pre":
                for kind, *rest in sends:
                    data = rest[0]
                    with socket.create_connection(("127.0.0.1", game),
                                                  timeout=5) as s:
                        s.settimeout(3)
                        if data:
                            s.sendall(data)
                        time.sleep(0.03)
                done += 1
                continue
            sess = RawSession(game)
            if stage == "hs":
                for kind, *rest in sends:
                    sess.send_raw(rest[0])
            elif stage == "post_hs":
                sess.send_raw(login_frame(0, hs_body(PROTOCOL, b"localhost",
                                                   game, 2)))
                for kind, *rest in sends:
                    sess.send_raw(rest[0])
            elif stage == "post_login":
                ident = (f"FzW1{idx:03d}", idx.to_bytes(16, "big"))
                if not advance_to_login_success(sess, game, *ident):
                    notes.append(f"w1[{idx}] {meta}: login never completed")
                    continue
                for item in sends:
                    if item[0] == "raw":
                        sess.send_raw(item[1])
                    else:
                        sess.send_frame(item[1], item[2])
            elif stage == "config":
                ident = (f"FzW1{idx:03d}", idx.to_bytes(16, "big"))
                if not advance_to_login_success(sess, game, *ident):
                    notes.append(f"w1[{idx}] {meta}: login never completed")
                    continue
                drain_config_start(sess)
                for item in sends:
                    if item[0] == "raw":
                        sess.send_raw(item[1])
                    else:
                        sess.send_frame(item[1], item[2])
            elif stage == "play":
                ident = (f"FzW1{idx:03d}", idx.to_bytes(16, "big"))
                if not advance_to_login_success(sess, game, *ident):
                    notes.append(f"w1[{idx}] {meta}: login never completed")
                    continue
                if not drain_config_start(sess):
                    notes.append(f"w1[{idx}] {meta}: config burst incomplete")
                    continue
                if not advance_to_play(sess):
                    notes.append(f"w1[{idx}] {meta}: never reached play")
                    continue
                for item in sends:
                    if item[0] == "raw":
                        sess.send_raw(item[1])
                    else:
                        sess.send_frame(item[1], item[2])
            else:
                problems.append(f"w1[{idx}] unknown stage {stage!r}")
                continue
            done += 1
        except (OSError, AssertionError) as exc:
            notes.append(f"w1[{idx}] {meta}: {type(exc).__name__}: {exc}")
        finally:
            if sess is not None:
                sess.close()
        if srv["proc"].poll() is not None:
            problems.append(f"w1[{idx}] {meta}: SERVER EXITED "
                            f"(rc={srv['proc'].returncode})")
            return
    notes.append(f"wave1: {done}/{len(cases)} framing cases delivered")


# --------------------------------------------------------------------------
# Wave 2: movement
# --------------------------------------------------------------------------

def client_alive(c, window):
    """Drain `c` through Client.read (keepalive/chunk acks keep flowing)
    until `window` seconds pass -> True, or the peer closes -> False."""
    deadline = time.monotonic() + window
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return True
        try:
            c.read(min(0.2, remaining))
        except (socket.timeout, TimeoutError):
            continue
        except (AssertionError, OSError):
            return False


def run_wave2(srv, cases, problems, notes, clients):
    game = srv["game"]
    pool = ["FzMv00", "FzMv01", "FzMv02", "FzMv03", "FzMv04", "FzMv05",
            "FzMv06", "FzMv07"]
    n_reject = n_survive = 0
    for idx, case in enumerate(cases):
        name = pool[idx % len(pool)]
        c = None
        try:
            c = Client(game, name)
            clients.append(c)
            c.login()
            c.until_id(0x40)
            c.send(case["pid"], case["body"])
            time.sleep(0.1)
            if case["expect"] == "reject":
                n_reject += 1
                # Generous window: the play loop interleaves chunk pumping
                # with packet reads, so a disconnect can lag the send by
                # a beat under load. client_alive returns the moment the
                # peer closes, so the wait only costs time on real bugs.
                if client_alive(c, 2.5):
                    problems.append(
                        f"w2[{idx}] {case['meta']}: invalid movement frame "
                        f"{case['pid']:#x} body={case['body'][:24].hex()}... "
                        f"was NOT rejected")
            else:
                n_survive += 1
                if not client_alive(c, 0.4):
                    problems.append(
                        f"w2[{idx}] {case['meta']}: valid movement "
                        f"{case['pid']:#x} killed the connection")
                else:
                    try:
                        c.chat(f"w2-alive-{idx}")
                        deadline = time.monotonic() + 3
                        echoed = False
                        needle = f"w2-alive-{idx}".encode()
                        while time.monotonic() < deadline and not echoed:
                            try:
                                pid, body = c.read(
                                    max(0.05, deadline - time.monotonic()))
                            except (socket.timeout, TimeoutError,
                                    AssertionError, OSError):
                                break
                            if pid in (0x6C, 0x39) and needle in body:
                                echoed = True
                            if any(f"w2-alive-{idx}" in m
                                   for m in c.messages):
                                echoed = True
                        if not echoed:
                            problems.append(
                                f"w2[{idx}] {case['meta']}: connection open "
                                f"but server never echoed chat")
                    except (OSError, AssertionError) as exc:
                        problems.append(
                            f"w2[{idx}] {case['meta']}: post-move chat "
                            f"failed: {exc}")
        except (OSError, AssertionError, TimeoutError) as exc:
            if srv["proc"].poll() is not None:
                problems.append(f"w2[{idx}] {case['meta']}: SERVER EXITED "
                                f"rc={srv['proc'].returncode}")
                return
            problems.append(f"w2[{idx}] {case['meta']}: client error "
                            f"{type(exc).__name__}: {exc}")
        finally:
            if c is not None:
                try:
                    c.socket.close()
                except OSError:
                    pass
                if c in clients:
                    clients.remove(c)
    notes.append(f"wave2: {n_reject} reject-expected, {n_survive} "
                 f"survive-expected movement cases")


# --------------------------------------------------------------------------
# Wave 3: inventory / container
# --------------------------------------------------------------------------

def run_wave3(srv, cases, problems, notes, clients):
    game, rcon = srv["game"], srv["rcon"]
    names = ["FzInvA", "FzInvB"]
    sessions = {}
    baselines = {}
    sent = 0
    failures = 0

    def session_for(i):
        name = names[i % len(names)]
        if name not in sessions:
            c = Client(game, name)
            clients.append(c)
            c.login()
            c.until_id(0x40)
            sessions[name] = c
            baselines[name] = rcon_try(rcon, f"data get entity {name} Inventory")
        return sessions[name]

    def inventory_ok():
        ok = True
        for name in sessions:
            now = rcon_try(rcon, f"data get entity {name} Inventory")
            # "No entity found" can be a transient selector/snapshot miss
            # while the client socket is still healthy; retry briefly, then
            # cross-check liveness before calling it a real diff.
            for _ in range(3):
                if now != "No entity found" or not client_alive(sessions[name], 0.4):
                    break
                time.sleep(0.4)
                now = rcon_try(rcon, f"data get entity {name} Inventory")
            if now == "No entity found":
                problems.append(
                    f"w3: {name} vanished from entity lookup "
                    f"(socket {'dead' if not client_alive(sessions[name], 0.3) else 'alive'})")
                ok = False
            elif now != baselines[name]:
                problems.append(
                    f"w3: {name} Inventory changed without authorization:\n"
                    f"  baseline: {baselines[name]}\n  now:      {now}")
                ok = False
        return ok

    try:
        for idx, case in enumerate(cases):
            if failures >= 3:
                break
            c = session_for(idx)
            try:
                if case["kind"] == "seq":
                    for body in case["bodies"]:
                        c.send(P_WINDOW_CLICK, body)
                elif case["kind"] == "click":
                    c.send(P_WINDOW_CLICK, case["body"])
                elif case["kind"] == "creative":
                    c.send(P_CREATIVE_SLOT, case["body"])
                elif case["kind"] == "close":
                    c.send(P_CLOSE_WINDOW, case["body"])
            except OSError as exc:
                problems.append(f"w3[{idx}] {case['meta']}: send failed: {exc}")
                failures += 1
                sessions.pop(c.name, None)
                continue
            sent += 1
            if sent % 12 == 0:
                time.sleep(0.05)
                # Drain every live session: keepalive replies are only sent
                # when the client reads, and the server drops a connection
                # ~15s after an unanswered challenge.
                dead = None
                for sname, sc in list(sessions.items()):
                    if not client_alive(sc, 0.25):
                        dead = sname
                        break
                if dead is not None:
                    problems.append(f"w3[{idx}] {case['meta']}: {dead} "
                                    f"connection dropped by "
                                    f"{case['kind']} traffic")
                    failures += 1
                    sc = sessions.pop(dead)
                    try:
                        sc.socket.close()
                    except OSError:
                        pass
                    if sc in clients:
                        clients.remove(sc)
                    continue
                if not inventory_ok():
                    problems.append(f"w3[{idx}] {case['meta']}: inventory "
                                    f"diff detected at this case")
                    failures += 1
                    break
        if srv["proc"].poll() is not None:
            problems.append(f"w3: SERVER EXITED rc={srv['proc'].returncode}")
        # Final inventory diff must run while the fuzz clients are still
        # connected — closing them first makes every lookup miss.
        if failures < 3 and not inventory_ok():
            problems.append("w3: final inventory check failed")
    except (OSError, AssertionError, TimeoutError) as exc:
        problems.append(f"w3: executor error {type(exc).__name__}: {exc}")
    finally:
        for c in sessions.values():
            try:
                c.socket.close()
            except OSError:
                pass
            if c in clients:
                clients.remove(c)
    notes.append(f"wave3: {sent} container/creative cases delivered")


# --------------------------------------------------------------------------
# Wave 4: privilege escalation / command injection
# --------------------------------------------------------------------------

DENIAL_MARKERS = ("do not have permission", "unknown command",
                  "invalid command", "expected a command", "usage:",
                  "<--[here]", "incorrect argument")


def run_wave4(srv, cases, problems, notes, clients):
    game, rcon = srv["game"], srv["rcon"]
    names = ["FzCmdA", "FzCmdB", "FzCmdC", "FzCmdD"]
    replies = []
    sent = 0
    idx = 0
    consecutive_failures = 0
    while idx < len(cases) and consecutive_failures < 3:
        batch = cases[idx:idx + 7]
        name = names[(idx // 7) % len(names)]
        c = None
        try:
            c = Client(game, name)
            clients.append(c)
            c.login()
            c.until_id(0x40)
            batch_failed = False
            for case in batch:
                meta = case["meta"]
                before = len(c.messages)
                try:
                    if case.get("raw") is not None:
                        c.send(case["via"], case["raw"])
                    elif case.get("cmd") is not None:
                        if case["via"] == 6:
                            c.chat("/" + case["cmd"])
                        else:
                            c.send(P_CHAT_COMMAND, string(case["cmd"]))
                    else:
                        idx += 1
                        continue
                except OSError as exc:
                    idx += 1
                    if case.get("disconnect_ok"):
                        notes.append(f"w4[{idx}] {meta}: send failed after "
                                     f"expected disconnect ({exc})")
                    else:
                        problems.append(f"w4[{idx}] {meta}: send failed: {exc}")
                        batch_failed = True
                    break
                idx += 1
                sent += 1
                # drain replies: command feedback arrives as 0x6c system chat
                # and Client.read appends it to c.messages automatically.
                deadline = time.monotonic() + 1.0
                quiet_since = time.monotonic()
                saw_reply = False
                while time.monotonic() < deadline:
                    try:
                        c.read(0.15)
                    except (socket.timeout, TimeoutError):
                        pass
                    except (AssertionError, OSError):
                        break
                    if len(c.messages) > before:
                        saw_reply = True
                        quiet_since = time.monotonic()
                    elif saw_reply and time.monotonic() - quiet_since > 0.25:
                        break
                    elif not saw_reply and time.monotonic() - quiet_since > 0.6:
                        break
                for text in c.messages[before:]:
                    replies.append((meta, text))
                if not client_alive(c, 0.25):
                    if case.get("disconnect_ok"):
                        notes.append(f"w4[{idx}] {meta}: rejected as "
                                     f"expected (disconnect)")
                    else:
                        problems.append(
                            f"w4[{idx}] {meta}: UNEXPECTED disconnect "
                            f"after {case.get('cmd') or case.get('raw')!r}")
                        batch_failed = True
                    break
            consecutive_failures = consecutive_failures + 1 if batch_failed else 0
        except (OSError, AssertionError, TimeoutError) as exc:
            problems.append(f"w4 batch@{idx}: {type(exc).__name__}: {exc}")
            consecutive_failures += 1
            idx += len(batch)
        finally:
            if c is not None:
                try:
                    c.socket.close()
                except OSError:
                    pass
                if c in clients:
                    clients.remove(c)
    # Replies that are neither denials nor parser errors are worth reporting;
    # the authoritative escalation check is the RCON/playerdata diff.
    interesting = []
    for m, t in replies:
        t = t if isinstance(t, str) else str(t)
        if t and not any(k in t.lower() for k in DENIAL_MARKERS):
            interesting.append((m, t))
    if interesting:
        notes.append(f"wave4: {len(interesting)} non-denial replies "
                     f"(state diff decides): "
                     + "; ".join(f"{m} -> {t[:70]!r}" for m, t in interesting[:10]))
    notes.append(f"wave4: {sent} command cases, {len(replies)} replies")


# --------------------------------------------------------------------------
# Wave 5: sign / beacon / dig / use-item
# --------------------------------------------------------------------------

def run_wave5(srv, cases, problems, notes, clients):
    game = srv["game"]
    names = ["FzSgnA", "FzSgnB"]
    sessions = []
    sent = 0
    try:
        for name in names:
            c = Client(game, name)
            clients.append(c)
            c.login()
            c.until_id(0x40)
            sessions.append(c)
        for c in sessions:
            c.send(P_LOOK, look_body(0.0, -90.0))
        time.sleep(0.1)
        for idx, case in enumerate(cases):
            c = sessions[idx % len(sessions)]
            try:
                c.send(case["pid"], case["body"])
            except OSError as exc:
                problems.append(f"w5[{idx}] {case['meta']}: send failed: {exc}")
                break
            sent += 1
            if sent % 10 == 0:
                time.sleep(0.05)
                # Drain every session so keepalive replies stay prompt.
                dropped = False
                for sc in sessions:
                    if not client_alive(sc, 0.25):
                        problems.append(f"w5[{idx}] {case['meta']}: "
                                        f"connection dropped by pid "
                                        f"{case['pid']:#x}")
                        dropped = True
                        break
                if dropped:
                    break
        time.sleep(0.3)
        for c in sessions:
            if not client_alive(c, 0.4):
                problems.append("w5: a fuzz connection was closed unexpectedly")
    except (OSError, AssertionError, TimeoutError) as exc:
        problems.append(f"w5: executor error {type(exc).__name__}: {exc}")
    finally:
        for c in sessions:
            try:
                c.socket.close()
            except OSError:
                pass
            if c in clients:
                clients.remove(c)
    notes.append(f"wave5: {sent} interaction cases delivered")


# --------------------------------------------------------------------------
# Post-fuzz verification
# --------------------------------------------------------------------------

def probe_region_ok(rcon):
    # region A: fill 0 200 0 15 215 15 ; region B: fill 20 200 0 35 215 15.
    # `if blocks <from> <to> <dest> all` compares A against the equal-size
    # volume starting at dest (20 200 0 .. 35 215 15).
    reply = rcon_try(rcon, "execute if blocks 0 200 0 15 215 15 "
                          "20 200 0 all")
    return ("Test passed" in reply), reply


def setup_probe_region(rcon):
    a = rcon_try(rcon, "fill 0 200 0 15 215 15 minecraft:stone")
    b = rcon_try(rcon, "fill 20 200 0 35 215 15 minecraft:stone")
    ok, probe = probe_region_ok(rcon)
    return ok, (a, b, probe)


def state_snapshot(srv):
    rcon = srv["rcon"]
    return {
        "ops_files": read_ops_files(srv["root"]),
        "gamerules": rcon_try(rcon, "gamerule"),
        "datapacks": rcon_try(rcon, "datapack list"),
        "difficulty": rcon_try(rcon, "difficulty"),
        "seed": rcon_try(rcon, "seed"),
        "probe": probe_region_ok(srv["rcon"]),
    }


def state_diff(before, after):
    diffs = []
    for key in ("ops_files", "gamerules", "datapacks", "difficulty", "seed"):
        if before.get(key) != after.get(key):
            diffs.append(f"{key}: {before.get(key)!r} -> {after.get(key)!r}")
    if before.get("probe") != after.get("probe"):
        diffs.append(f"probe-region: {before.get('probe')} -> {after.get('probe')}")
    return diffs


def playerdata_sweep(anvil_dir, problems):
    pdir = Path(anvil_dir) / "playerdata"
    if not pdir.is_dir():
        problems.append("playerdata directory missing after save-all")
        return 0
    files = sorted(pdir.glob("*.dat"))
    if not files:
        problems.append("no playerdata/*.dat records after save-all")
        return 0
    for f in files:
        try:
            nbt = read_playerdata(f)
        except Exception as exc:
            problems.append(f"{f.name}: playerdata parse failed: {exc}")
            continue
        for v in record_violations(f.stem, nbt):
            problems.append(v)
    return len(files)


def health_barrier(srv, problems):
    """Fresh clean client: login -> chunks -> teleport -> chat -> move."""
    try:
        with Client(srv["game"], "FuzzProbe") as c:
            c.login()
            got = c.until_chunks(set(spawn_window()), contract="barrier-chunks")
            if not got:
                problems.append("health barrier: no chunks received")
            if c.pos is None:
                c.until_id(0x40, "barrier-teleport")
            if c.pos is None or not all(math.isfinite(v) for v in c.pos):
                problems.append(f"health barrier: bad spawn pos {c.pos!r}")
            c.chat("fuzz barrier probe")
            deadline = time.monotonic() + 5
            echoed = False
            while time.monotonic() < deadline and not echoed:
                try:
                    pid, body = c.read(max(0.05, deadline - time.monotonic()))
                except (socket.timeout, TimeoutError, AssertionError, OSError):
                    break
                if pid in (0x6C, 0x39) and b"fuzz barrier probe" in body:
                    echoed = True
                if any("fuzz barrier probe" in m for m in c.messages):
                    echoed = True
            if not echoed:
                problems.append("health barrier: chat echo never arrived")
            if c.pos is not None:
                c.move(c.pos[0] + 0.5, c.pos[1], c.pos[2] + 0.5)
                c.send(P_LOOK, look_body(90.0, 0.0))
                time.sleep(0.3)
    except Exception as exc:
        problems.append(f"health barrier failed: {type(exc).__name__}: {exc}")


def run_fuzz(binary, seed, total):
    problems, notes = [], []
    tmp_parent = Path.cwd() / "target" / "tmp"
    tmp_parent.mkdir(parents=True, exist_ok=True)
    cases = all_cases(seed, total)
    # The handshake's claimed server port is cosmetic — the server routes on
    # the socket's actual address — so cases are generated before launch.
    with tempfile.TemporaryDirectory(prefix="mc-fuzz-", dir=tmp_parent) as td:
        root = Path(td)
        link_server_assets(root)
        with fuzz_server(binary, root) as srv:
            ok, setup = setup_probe_region(srv["rcon"])
            if not ok:
                problems.append(f"probe region setup failed: {setup}")
            before = state_snapshot(srv)
            clients = []
            waves = [
                (1, run_wave1), (2, run_wave2), (3, run_wave3),
                (4, run_wave4), (5, run_wave5),
            ]
            for wnum, runner in waves:
                if srv["proc"].poll() is not None:
                    problems.append(f"server exited before wave {wnum}")
                    break
                try:
                    status(srv["game"])
                except (OSError, AssertionError) as exc:
                    problems.append(f"server unresponsive before wave {wnum}: {exc}")
                    break
                try:
                    if wnum == 1:
                        runner(srv, cases[wnum], problems, notes)
                    else:
                        runner(srv, cases[wnum], problems, notes, clients)
                except Exception:
                    problems.append(f"wave {wnum} executor crashed:\n"
                                    + traceback.format_exc()[-2000:])
                try:
                    status(srv["game"])
                    rcon_try(srv["rcon"], "list", timeout=5)
                except Exception as exc:
                    problems.append(f"server unhealthy after wave {wnum}: {exc}")
                if srv["proc"].poll() is not None:
                    problems.append(f"server exited during/after wave {wnum} "
                                    f"rc={srv['proc'].returncode}")
                    break
            for c in list(clients):
                try:
                    c.socket.close()
                except OSError:
                    pass
            clients.clear()
            time.sleep(0.5)
            rcon_try(srv["rcon"], "save-all", timeout=30)
            time.sleep(0.5)
            health_barrier(srv, problems)
            after = state_snapshot(srv)
            for d in state_diff(before, after):
                problems.append(f"unauthorized state change: {d}")
            n_records = playerdata_sweep(srv["anvil"], problems)
            notes.append(f"playerdata: {n_records} records checked")
            hits = scan_log(srv["log"])
            if hits:
                problems.append(f"server log crash signatures ({len(hits)}):\n"
                                + "\n".join(hits[:20]))
            if srv["proc"].poll() is not None:
                problems.append(f"server process exited rc={srv['proc'].returncode}")
    return problems, notes, cases


# --------------------------------------------------------------------------
# Self test (offline)
# --------------------------------------------------------------------------

def self_test():
    fails = []

    def check(cond, msg):
        if not cond:
            fails.append(msg)

    for seed in (0, 1, 1211, 424242):
        a = all_cases(seed, 120)
        b = all_cases(seed, 120)
        check(a == b, f"seed {seed}: case generation not deterministic")
    check(all_cases(1, 80) != all_cases(2, 80),
          "different seeds produced identical cases")
    counts = allocate_cases(1211, 250)
    check(0 < sum(counts) <= 250 and all(c > 0 for c in counts),
          f"case allocation broken: {counts}")

    check(len(move_pos_body(0.5, 64.0, 0.5)) == 25, "move_pos len")
    check(len(move_pos_rot_body(0.5, 64.0, 0.5, 0.0, 0.0)) == 33,
          "move_pos_rot len")
    check(len(look_body(0.0, 0.0)) == 9, "look len")
    check(len(click_body(0, 0, 9, 0, 0, carried_tail(stack_wire(0)))) >= 6,
          "click body layout")
    check(len(login_frame(0, hs_body())) > 0, "login frame")

    dirty = "[spine] ok\nthread 'x' panicked at src/a.rs:1:1:\nboom\n"
    check(any("panicked at" in h for h in scan_log_text(dirty)),
          "crash detector missed synthetic panic")
    check(not scan_log_text("[spine] all good\n"),
          "crash detector false-positive on clean log")
    check(any("poisoned" in h for h in
              scan_log_text("lock poisoned while saving\n")),
          "crash detector missed poisoned-lock line")

    bad = {"playerGameType": 1, "Pos": [float("nan"), 64.0, 0.0],
           "Inventory": [{"id": "minecraft:diamond", "Count": 64}],
           "Dimension": "minecraft:the_end"}
    v = record_violations("Synthetic", bad)
    check(len(v) >= 4, f"escalation detector under-reported: {v}")
    good = {"playerGameType": 0, "Pos": [0.5, 64.0, -0.5],
            "Rotation": [0.0, 0.0], "Inventory": [],
            "Dimension": "minecraft:overworld"}
    check(record_violations("Clean", good) == [],
          f"escalation detector false-positive: "
          f"{record_violations('Clean', good)}")
    baseline = record_violations("Base", good, inventory_baseline=3)
    check(any("baseline" in x for x in baseline),
          "inventory baseline check broken")

    def w_named(tag, name, payload):
        return (bytes([tag]) + struct.pack(">H", len(name))
                + name.encode() + payload)
    compound = bytes([3]) + struct.pack(">H", 4) + b"Test" + struct.pack(">i", 7)
    compound += b"\x00"
    blob = gzip.compress(w_named(10, "", compound))
    tmp = Path(tempfile.mkdtemp()) / "t.dat"
    tmp.write_bytes(blob)
    try:
        parsed = read_playerdata(tmp)
        check(parsed == {"Test": 7}, f"NBT round-trip failed: {parsed}")
    except Exception as exc:
        fails.append(f"NBT reader crashed: {exc}")

    if fails:
        print("self-test FAILURES:")
        for f in fails:
            print(" -", f)
        return 1
    print("self-test PASS!")
    return 0


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("server_binary", nargs="?", default="target/debug/mc-spine")
    ap.add_argument("--seed", type=int, default=1211)
    ap.add_argument("--cases", type=int, default=250)
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args()

    if args.self_test:
        return self_test()

    binary = Path(args.server_binary).resolve()
    if not binary.is_file():
        print(f"server binary not found: {binary}", file=sys.stderr)
        return 2
    cases = all_cases(args.seed, args.cases)
    total = sum(len(v) for v in cases.values())
    print(f"protocol fuzz: seed={args.seed} cases={total} "
          f"(w1={len(cases[1])} w2={len(cases[2])} w3={len(cases[3])} "
          f"w4={len(cases[4])} w5={len(cases[5])}) binary={binary}")
    t0 = time.monotonic()
    problems, notes, _ = run_fuzz(binary, args.seed, args.cases)
    elapsed = time.monotonic() - t0
    print(f"--- notes ({len(notes)}) ---")
    for n in notes:
        print("  " + n)
    if problems:
        print(f"--- FAILURES ({len(problems)}) after {elapsed:.1f}s ---")
        for p in problems:
            print("  " + p)
        return 1
    print(f"protocol fuzz PASS! {total} cases in {elapsed:.1f}s, "
          "0 panics / 0 deadlocks / 0 privilege escalations")
    return 0


if __name__ == "__main__":
    sys.exit(main())
