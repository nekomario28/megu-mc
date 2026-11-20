#!/usr/bin/env python3
"""Trace blank and matching Protocol 767 inventory pickup predictions."""
import argparse
from hashlib import file_digest
import json
import os
from pathlib import Path
import struct
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools/client_diff"))
from parity_client import (EMPTY_STACK, JAR, OAK_LOG, ParityClient, Reader,  # noqa: E402
                           command, live_server, stack, string, until, vint, wire_stack)

class SlotTrace(ParityClient):
    """Record every window-0/window-255 0x15 set-slot packet with its raw bytes."""

    def __init__(self, port):
        super().__init__(port)
        self.slot_trace = []

    def observe_gameplay(self, packet, body):
        super().observe_gameplay(packet, body)
        if packet != 0x15 or body[0] not in (0, 255):
            return
        reader = Reader(body)
        window, state, slot = reader.number("B"), reader.varint(), reader.number(">h")
        value = stack(reader)
        assert reader.pos == len(body), "complete set-slot packet"
        self.slot_trace.append({"window": window, "state": state, "slot": slot,
                                "stack": value, "packet_hex": (b"\x15" + body).hex()})


def click(client, slot, changes, carried, barrier):
    """0x0e click using the observed State ID, fenced by a Play suggestion barrier."""
    state = client.inventory_state
    changed = b"".join(struct.pack(">h", key) + wire_stack(*value)
                       for key, value in changes)
    body = (b"\0" + vint(state) + struct.pack(">hB", slot, 0) + vint(0)
            + vint(len(changes)) + changed + wire_stack(*carried))
    start = len(client.slot_trace)
    client.send(0x0e, body)
    client.send(0x0b, vint(barrier) + string("/time "))
    client.until(lambda pid, payload: pid == 0x0f and Reader(payload).varint() == barrier,
                 "Play suggestion reply after inventory click")
    return {"request_hex": (b"\x0e" + body).hex(), "menu_state": state,
            "updates": client.slot_trace[start:]}


def pickup(kind, binary, jar, matching, scratch):
    """Run one fresh server; capture one blank-or-matching click and the next click."""
    with tempfile.TemporaryDirectory(prefix="pickup-game-", dir=scratch) as path:
        with live_server(kind, binary, Path(path), jar) as (game, rcon_port):
            with SlotTrace(game) as client:
                client.login()
                until(client, lambda: client.pos is not None and (0, 0) in client.chunks,
                      "initial Play and origin chunk")
                command(rcon_port, f"gamemode survival {client.name}")
                command(rcon_port, f"give {client.name} minecraft:oak_log 1")
                until(client, lambda: any(row["slot"] == 36 and row["stack"] == OAK_LOG
                                          for row in client.slot_trace), "oak log in slot 36")
                first = (click(client, 36, [(36, EMPTY_STACK)], OAK_LOG, 1) if matching
                         else click(client, 36, [], EMPTY_STACK, 1))
                if not matching:
                    assert any(row["window"] == 0 and row["slot"] == 36
                               and row["stack"] == EMPTY_STACK for row in first["updates"]), (
                        "blank-prediction pickup did not clear slot 36"
                    )
                second = click(client, 37, [], EMPTY_STACK, 2)
                assert any(row["window"] == 0 and row["slot"] == 37
                           and row["stack"] == OAK_LOG
                           for row in second["updates"]), "server did not move its carried log"
                return {"pickup": first, "place_carried_into_slot_37": second}


def collect(kind, binary, jar, scratch):
    return {name: pickup(kind, binary, jar, matching, scratch)
            for name, matching in (("blank", False), ("matching", True))}


def blank_corrections(trace):
    # State IDs count updates on an individual connection. Keep their raw values
    # in the report and compare the order and contents of the affected slots.
    return [(row["window"], row["slot"], row["stack"])
            for row in trace["blank"]["pickup"]["updates"]
            if row["window"] == 255 or (row["window"] == 0 and row["slot"] in (-1, 36))]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--with-vanilla", action="store_true",
                        help="also run the pinned official JVM and compare observed packets")
    parser.add_argument("--vanilla-jar", type=Path, default=JAR)
    parser.add_argument(
        "--binary", type=Path,
        default=Path(os.environ.get("MC_SPINE_BIN", str(ROOT / "target/debug/mc-spine"))),
    )
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error(f"missing or non-executable server binary {binary}")
    scratch = ROOT / "target/tmp"
    scratch.mkdir(parents=True, exist_ok=True)
    os.environ["TMPDIR"] = str(scratch)
    os.environ.setdefault("MC_PROBE_DEADLINE", "30")

    with binary.open("rb") as stream:
        binary_sha256 = file_digest(stream, "sha256").hexdigest()
    left = collect("mc-spine", binary, args.vanilla_jar, scratch)
    report = {"protocol": 767, "server_sha256": binary_sha256, "mc_spine": left}
    if args.with_vanilla:
        right = collect("vanilla", binary, args.vanilla_jar, scratch)
        report["reference"] = right
        report["blank_corrections_match"] = blank_corrections(left) == blank_corrections(right)
    print(json.dumps(report, indent=2, sort_keys=True))
    if report.get("blank_corrections_match") is False:
        raise SystemExit("client inventory pickup wire: blank correction mismatch")


if __name__ == "__main__":
    try:
        main()
    except AssertionError as error:
        raise SystemExit(f"client inventory pickup wire: FAIL (fixture: {error})") from error
