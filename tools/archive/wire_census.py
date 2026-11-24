#!/usr/bin/env python3
"""Wire census: launch megu, play a scripted session, and histogram every S2C
packet id actually emitted, per phase.

Static coverage audits (`v767_total_parity_audit.py`) look for literal
`send_packet(s, 0xNN, …)` sites and miss payload-builder dispatch — a trivial
login session already emits ~29 play packets the static audit calls "missing".
This probe measures the wire itself, which is the only ground truth for
"never sent" claims.

Usage:
    python3 tools/wire_census.py [BINARY] [--view N] [--settle S] [--json]
    BINARY defaults to target/release/megu then target/debug/megu.

The session covers login, configuration, paced chunk delivery, two teleports,
one chat message and two chat commands. Event-driven packets (combat, weather,
containers, mobs) are outside this session's reach — "never observed" is an
upper bound, not proof of absence.
"""
import argparse
import json
import re
import socket
import sys
import tempfile
import time
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests"))
from protocol767 import Client, local_server  # noqa: E402


def v767_s2c_table(phase="Play"):
    """Parse proto/src/data_v767.rs for (id -> name) of clientbound packets."""
    text = (ROOT / "proto/src/data_v767.rs").read_text()
    out = {}
    for m in re.finditer(
        r'PacketPhase::' + phase + r',\s*PacketDir::Clientbound,\s*"([^"]+)",\s*(0x[0-9a-fA-F]+|\d+)',
        text,
    ):
        out[int(m.group(2), 0)] = m.group(1)
    return out


def census(binary, view, settle):
    hist = Counter()

    class CensusClient(Client):
        def read(self, timeout):
            pid, body = super().read(timeout)
            hist[(self.phase, pid)] += 1
            return pid, body

    with local_server(Path(binary).resolve()) as game, CensusClient(game, "Census") as c:
        c.login(view=view)
        deadline = time.monotonic() + settle
        while time.monotonic() < deadline:
            try:
                c.read(0.5)
            except (TimeoutError, socket.timeout):
                continue
        if c.pos:
            c.move(c.pos[0] + 8, c.pos[1], c.pos[2] + 8)
            c.move(c.pos[0] + 16, c.pos[1], c.pos[2])
        c.chat("megu census marker")
        c.send(0x04, b"\x05help")
        c.send(0x04, b"\x0ctime set day")
        deadline = time.monotonic() + min(settle, 8)
        while time.monotonic() < deadline:
            try:
                c.read(0.5)
            except (TimeoutError, socket.timeout):
                continue
    return hist


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("binary", nargs="?", type=Path)
    ap.add_argument("--view", type=int, default=4)
    ap.add_argument("--settle", type=float, default=12.0,
                    help="seconds to drain after login (default 12)")
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()
    binary = args.binary or next(
        (p for p in (ROOT / "target/release/megu", ROOT / "target/debug/megu") if p.is_file()),
        None)
    if binary is None:
        sys.exit("no built megu binary found; pass one explicitly")
    hist = census(binary, args.view, args.settle)
    table = v767_s2c_table()
    play = {pid: n for (ph, pid), n in hist.items() if ph == "play"}
    other = {(ph, pid): n for (ph, pid), n in hist.items() if ph != "play"}
    unseen = sorted(set(table) - set(play))
    if args.json:
        print(json.dumps({
            "binary": str(binary),
            "phases": {f"{ph}/0x{pid:02x}": n for (ph, pid), n in sorted(hist.items())},
            "play_observed": {f"0x{pid:02x}": {"count": n, "name": table.get(pid)}
                              for pid, n in sorted(play.items())},
            "play_never_observed": {f"0x{pid:02x}": table[pid] for pid in unseen},
        }, indent=1))
        return
    print(f"=== login/config S2C: {len(other)} distinct ===")
    for (ph, pid), n in sorted(other.items()):
        print(f"  {ph} 0x{pid:02x} x{n}")
    print(f"\n=== play S2C observed: {len(play)} distinct ===")
    for pid, n in sorted(play.items()):
        print(f"  0x{pid:02x} x{n:5d}  {table.get(pid, '???not-in-table???')}")
    print(f"\n=== play S2C never observed this session (upper bound): {len(unseen)} ===")
    for pid in unseen:
        print(f"  0x{pid:02x}  {table[pid]}")


if __name__ == "__main__":
    main()
