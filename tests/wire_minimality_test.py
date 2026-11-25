#!/usr/bin/env python3
"""Wire-minimality pins: megu must not emit packets vanilla does not.

Each check replays one action and counts the exact update packets the wire
carries. A violation is a real over-notification bug, not a style nit:
every extra packet costs bandwidth on every client in range and can flicker
client state (see evidence/foundation/live-diff-20261123.md — measured live
against the pinned 1.21.1 server.jar: 5/12 scenario mismatches, three of them
this exact family).

Checks
  C1   bootstrap emission set — the join sequence keeps its required packets
       (pins the wire-census surface; must already pass).
  LD1a `setblock` emits exactly one update record for the position, carrying
       the new state — not a duplicate, not a stale-then-new pair.
  LD1b a creative dig emits exactly one update record, carrying air.
  LD3  a wid-0 inventory click emits `container_set_slot` (0x15) deltas only —
       no `container_set_content` (0x13) re-sends of the whole window.
  ST1  a wid-0 click whose stateId is deliberately wrong emits exactly one
       `container_set_content` (0x13) resync — and the click still applies
       (verified on the pinned vanilla 1.21.1 jar: `handleContainerClick`
       runs `clicked` unconditionally; the stale flag only selects
       `broadcastFullState` over `broadcastChanges`).

--tolerate LD1a,LD3,... marks named checks report-only: they still print their
measured packet streams but do not fail. CI runs with the currently-observed
violation list; shrink the list as each bug is fixed (same contract as
tools/debt_ratchet.sh — a tolerated violation that stops reproducing should
also fail review, because the list itself is a claim).

Run: python3 tests/wire_minimality_test.py BINARY [--tolerate LD1a]
"""
import argparse
import math
import socket
import struct
import sys
import time
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests"))
from container_wire import stack as read_stack, wire_stack  # noqa: E402
from protocol767 import (Client, Reader, WIRE_BLOCKS, local_server, position,  # noqa: E402
                         vint)
from wire_smoke import port, rcon_frame, rcon_response  # noqa: E402

RCON_PASS = "wire-minimality-local"
STONE = next(b["defaultState"] for b in WIRE_BLOCKS if b["name"] == "stone")
STONE_ITEM = 1  # item registry id for minecraft:stone on protocol 767


def rcon(port, text):
    with socket.create_connection(("127.0.0.1", port), timeout=5) as sock:
        sock.sendall(rcon_frame(1, 3, RCON_PASS))
        auth = rcon_response(sock)
        if len(auth) < 10:
            raise AssertionError("rcon auth refused")
        sock.sendall(rcon_frame(2, 2, text))
        return rcon_response(sock)


class MinClient(Client):
    """Client that records every play-phase packet id and the raw update stream."""

    def __init__(self, game, name):
        super().__init__(game, name)
        self.pids = Counter()
        self.phase_pids = {"configuration": Counter(), "play": Counter()}
        # Last stateId seen per containerId, like the vanilla client's
        # `AbstractContainerMenu.stateId`: 0x13/0x15 carry it as a varint right
        # after the window id, and clicks echo the open window's value.
        # -1/-2 packets land under their own ids — as in vanilla they do not
        # update the echoed menu stateId.
        self.state_ids = {}
        # wid-0 0x15 echoes seen: (window, slot, (item, count)).
        self.setslot = []

    def read(self, timeout):
        pid, body = super().read(timeout)
        if self.phase in self.phase_pids:
            self.phase_pids[self.phase][pid] += 1
        if self.phase == "play" and pid in (0x13, 0x15) and body:
            wid = struct.unpack(">b", body[:1])[0]
            self.state_ids[wid] = Reader(body[1:]).varint()
            if pid == 0x15:
                r = Reader(body[1:])
                r.varint()
                self.setslot.append((wid, r.number(">h"), read_stack(r)))
        return pid, body

    def click(self, slot, btn=0, state_id=None):
        """wid-0 mode-0 click; echoes the tracked stateId unless overridden."""
        if state_id is None:
            state_id = self.state_ids.get(0, 0)
        self.send(0x0e, bytes([0]) + vint(state_id) + struct.pack(">hB", slot, btn)
                  + vint(0) + vint(0) + wire_stack())


def drain(client, seconds):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            client.read(0.25)
        except (TimeoutError, socket.timeout):
            return


def updates_since(client, pos, start):
    return [state for (p, state) in client.updates[start:] if p == pos]


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("binary", type=Path)
    ap.add_argument("--tolerate", default="", help="comma-separated check names to downgrade to report-only")
    args = ap.parse_args()
    tolerate = {t.strip() for t in args.tolerate.split(",") if t.strip()}
    failures = {}
    reports = {}

    def check(name, ok, detail):
        # Ratchet contract: a tolerated check that starts passing is itself a
        # failure — the tolerate list must shrink, like tools/debt_ratchet.sh.
        reports[name] = (ok, detail)
        if (not ok and name not in tolerate) or (ok and name in tolerate):
            failures[name] = detail

    game_port = port()
    rcon_port = port()
    while rcon_port == game_port:
        rcon_port = port()
    env = {"MEGU_RCON_PORT": str(rcon_port), "MEGU_RCON_PASS": RCON_PASS,
           "MEGU_RCON_BIND_ADDR": "127.0.0.1"}
    with local_server(args.binary.resolve(), extra_env=env) as game:
        with MinClient(game, "MinProbe") as c:
            c.login()
            if c.pos is None:
                c.until_id(0x40)
            drain(c, 2.0)

            # ---- C1: bootstrap emission pins -------------------------------
            play = c.phase_pids["play"]
            config = c.phase_pids["configuration"]
            required_play = {0x0c, 0x11, 0x25, 0x27, 0x2b, 0x40, 0x4b, 0x74, 0x77}
            required_config = {0x03, 0x07, 0x0c, 0x0d, 0x0e}
            check("C1", required_play <= set(play) and required_config <= set(config),
                  f"missing play={sorted(hex(p) for p in required_play - set(play))} "
                  f"config={sorted(hex(p) for p in required_config - set(config))}")

            # ---- LD1a: setblock emits exactly one update, the new state ----
            px, py, pz = (math.floor(v) for v in c.pos)
            target = (px, py - 1, pz)
            rcon(rcon_port, f"setblock {target[0]} {target[1]} {target[2]} minecraft:stone")
            start = len(c.updates)
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline and not updates_since(c, target, start):
                try:
                    c.read(0.25)
                except (TimeoutError, socket.timeout):
                    break
            drain(c, 1.0)
            seq = updates_since(c, target, start)
            check("LD1a", seq == [STONE], f"setblock updates {seq} (want exactly [{STONE}])")

            # ---- LD1b: creative dig emits exactly one update, air ----------
            rcon(rcon_port, f"gamemode creative {c.name}")
            drain(c, 1.0)
            start = len(c.updates)
            c.send(0x24, vint(0) + position(*target) + bytes([1]) + vint(71))
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline and not updates_since(c, target, start):
                try:
                    c.read(0.25)
                except (TimeoutError, socket.timeout):
                    break
            drain(c, 1.0)
            seq = updates_since(c, target, start)
            check("LD1b", seq == [0], f"dig updates {seq} (want exactly [0])")

            # ---- LD3: wid-0 inventory click emits 0x15 deltas only ---------
            rcon(rcon_port, f"give {c.name} minecraft:stone 4")
            rcon(rcon_port, f"gamemode survival {c.name}")
            drain(c, 1.5)
            base_13 = c.phase_pids["play"][0x13]
            base_15 = c.phase_pids["play"][0x15]
            # Pick the stack out of a hotbar slot (wid-0 slot 36) — one click
            # echoing the last stateId the server sent, like a real client.
            c.click(36)
            drain(c, 1.5)
            new_13 = c.phase_pids["play"][0x13] - base_13
            new_15 = c.phase_pids["play"][0x15] - base_15
            check("LD3", new_13 == 0 and new_15 >= 1,
                  f"click emitted 0x13 x{new_13}, 0x15 x{new_15} "
                  f"(want 0x13 x0, 0x15 >=1; 0x15==0 means wid-0 click unhandled)")

            # ---- ST1: stale stateId -> one 0x13 resync, click applied ------
            # Cursor now holds the 4xstone from the LD3 pickup. A click that
            # echoes a wrong stateId must resync the whole window — and the
            # click still applies (vanilla applies the click, then pushes
            # broadcastFullState).
            stale_sid = (c.state_ids.get(0, 0) + 4096) & 0x7fff
            base_13 = c.phase_pids["play"][0x13]
            base_15 = len(c.setslot)
            c.click(37, state_id=stale_sid)
            drain(c, 1.5)
            new_13 = c.phase_pids["play"][0x13] - base_13
            placed = [st for (w, s, st) in c.setslot[base_15:] if (w, s) == (0, 37)]
            check("ST1", new_13 == 1 and placed == [(STONE_ITEM, 4)],
                  f"stale click emitted 0x13 x{new_13}, slot37 echoes {placed} "
                  f"(want 0x13 x1, slot37 -> ({STONE_ITEM}, 4))")

    print("== wire minimality ==")
    for name, (ok, detail) in reports.items():
        verdict = ("STALE-TOLERATE" if ok and name in tolerate else
                   "TOLERATED" if name in tolerate else
                   "FAIL" if not ok else "PASS")
        print(f"  {verdict:15s} {name}: {detail}")
    if failures:
        print(f"wire minimality: FAIL {sorted(failures)}", file=sys.stderr)
        sys.exit(1)
    print("wire minimality: PASS" + (f" (tolerated: {sorted(tolerate)})" if tolerate else ""))


if __name__ == "__main__":
    main()
