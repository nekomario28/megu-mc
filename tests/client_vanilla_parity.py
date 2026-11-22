#!/usr/bin/env python3
"""Differential Protocol 767 client. Default runs two megu consistency sessions."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools/client_diff"))
from parity_client import (AIR_STATES, BOTTOM_SLAB, EMPTY_STACK, JAR, OAK_LOG,  # noqa: E402
                           OAK_PLANKS, ROOT as HARNESS_ROOT, SLAB_END, STONE, WATER, run_target)

SCENARIOS = (
    "bootstrap", "chunks", "grounded_walk", "sprint", "jump_arc",
    "block_place", "block_break", "fluid_entry", "submerged_ticks",
    "slab_step_up", "crafting_planks", "combat_hit_or_damage",
)


MOVEMENT_STEPS = {
    "grounded_walk": 6, "sprint": 6, "jump_arc": 12,
    "fluid_entry": 3, "submerged_ticks": 8, "slab_step_up": 8,
}

COMPARISON_FIELDS = {
    "bootstrap": ("login_success", "configuration_finish", "join_game_0x2b",
                  "border_0x25", "border_details", "advancements_0x74",
                  "advancement_payload_bytes", "advancement_payload_sha256",
                  "initial_position_0x40", "initial_position", "position_flags_raw",
                  "compression_threshold", "login_packet_ids", "configuration_packet_ids"),
    "chunks": ("decoded_0x27", "chunks_decoded", "chunks_active", "origin_chunk",
               "sections", "states_per_section", "unmatched_unloads"),
    "grounded_walk": ("sent", "accepted", "corrections_0x40", "max_server_offset"),
    "sprint": ("sent", "accepted", "corrections_0x40", "max_server_offset"),
    "jump_arc": ("sent", "accepted", "corrections_0x40", "max_server_offset", "apex_y"),
    "block_place": ("state", "packet_0x09", "updates"),
    "block_break": ("state", "packet_0x09", "updates"),
    "fluid_entry": ("sent", "accepted", "corrections_0x40", "max_server_offset", "water_state"),
    "submerged_ticks": ("sent", "accepted", "corrections_0x40", "max_server_offset",
                        "water_state", "first_y", "last_y", "tick_tolerance"),
    "slab_step_up": ("sent", "accepted", "corrections_0x40", "max_server_offset",
                     "slab_state", "slab_updates", "final_position"),
    "crafting_planks": ("slot_updates",),
    "combat_hit_or_damage": ("initial_health_0x5d", "hit_health_0x5d",
                             "hit_entity_events_0x1f", "hit_damage_types_0x1a",
                             "hit_hurt_angles_0x24", "hit_velocity_0x5a",
                             "death_health_0x5d", "death_entity_events_0x1f"),
}


def validate(name, observed):
    """Require real success on each target, even if both targets fail identically."""
    failures = []

    def require(condition, message):
        if not condition:
            failures.append(message)

    if name == "bootstrap":
        for field in ("login_success", "configuration_finish", "join_game_0x2b",
                      "border_0x25", "advancements_0x74", "initial_position_0x40"):
            require(observed.get(field) is True, f"missing {field}")
        require(observed.get("border_details") is not None, "initial 0x25 not parsed")
        payloads = observed.get("advancement_payload_bytes", [])
        digests = observed.get("advancement_payload_sha256", [])
        require(bool(payloads) and all(n > 0 for n in payloads), "empty advancement 0x74")
        require(len(payloads) == len(digests), "missing advancement hashes")
        require(observed.get("login_packet_ids", [])[-1:] == ["0x02"], "login phase did not finish")
        require(observed.get("configuration_packet_ids", [])[-1:] == ["0x03"],
                "configuration phase did not finish")
        require(len(observed.get("initial_position", [])) == 3, "initial position absent")
    elif name == "chunks":
        require(observed.get("decoded_0x27") is True, "no decoded 0x27")
        require(observed.get("origin_chunk") is True, "origin chunk absent")
        require(observed.get("chunks_decoded", 0) > 0, "no decoded chunks")
        require(observed.get("chunks_active", 0) > 0, "no active chunks")
        require(observed.get("sections", 0) > 0, "zero chunk sections")
        require(observed.get("states_per_section") == 4096, "invalid section size")
        require(observed.get("unmatched_unloads") == 0, "unmatched chunk unload packets")
    elif name in MOVEMENT_STEPS:
        expected = MOVEMENT_STEPS[name]
        require(observed.get("sent") == expected, f"expected {expected} movement packets")
        require(observed.get("accepted") == expected, "authoritative RCON position disagrees")
        require(observed.get("max_server_offset", float("inf")) <= 1e-5,
                "server position differs from scripted trace")
        require(observed.get("corrections_0x40") == 0, "server issued correction 0x40")
        require(len(observed.get("server_positions", [])) == expected,
                "missing per-step authoritative positions")
        if name == "jump_arc":
            require(observed.get("apex_y", 0) > 241, "no jump apex")
        if name in ("fluid_entry", "submerged_ticks"):
            require(observed.get("water_state") == WATER, "source water missing")
        if name == "submerged_ticks":
            require(observed.get("tick_tolerance") == 0.03, "water tolerance unspecified")
            require(observed.get("first_y", 0) > observed.get("last_y", 0),
                    "submerged trace does not descend")
        if name == "slab_step_up":
            require(observed.get("slab_state") == BOTTOM_SLAB, "dry bottom slab missing")
            require(BOTTOM_SLAB in observed.get("slab_updates", []), "bottom slab update missing")
            require(observed.get("final_position") == SLAB_END, "did not finish on the slab")
    elif name == "crafting_planks":
        stages = observed.get("slot_updates", {})
        for phase, slot, value in (
            ("given", 36, OAK_LOG),
            ("lift_log", 36, EMPTY_STACK),
            ("place_ingredient", 1, OAK_LOG),
            ("place_ingredient", 0, OAK_PLANKS),
            ("take_result", 1, EMPTY_STACK),
            ("take_result", 0, EMPTY_STACK),
            ("store_planks", 37, OAK_PLANKS),
        ):
            values = [item for packet, index, item in stages.get(phase, [])
                      if packet in ("0x15", "0x13") and index == slot]
            require(values[-1:] == [value], f"{phase}: expected slot {slot} to end as {value}")
    elif name == "combat_hit_or_damage":
        require(observed.get("initial_health_0x5d") == 20.0, "victim did not start at full health")
        require(any(0 < hp < 20 for hp in observed.get("hit_health_0x5d", [])),
                "attack did not reduce victim health")
        require(bool(observed.get("hit_hurt_angles_0x24")), "victim hurt animation missing")
        require(any(vx > 0 and vz == 0 for vx, _, vz in observed.get("hit_velocity_0x5a", [])),
                "victim knockback away from attacker missing")
        require(0.0 in observed.get("death_health_0x5d", []), "victim death health missing")
    elif name in ("block_place", "block_break"):
        state = STONE if name == "block_place" else 0
        require(observed.get("state") == state, "incorrect final block state")
        require(observed.get("packet_0x09") is True, "block update 0x09 missing")
        require([6, 240, 1] in [entry[0] for entry in observed.get("updates", [])
                                  if entry[1] == state], "missing exact state update")
        if name == "block_break":
            require(state in AIR_STATES, "expected air state is not air")
    return failures


def compare(left, right):
    """Report observed wire differences and each side's independent acceptance."""
    comparisons = {}
    for name in SCENARIOS:
        a, b = left.get(name), right.get(name)
        if a is None or b is None:
            comparisons[name] = {"status": "MISMATCH", "reason": "scenario missing"}
            continue
        failures = {side: errors for side, errors in
                    (("megu", validate(name, a)), ("reference", validate(name, b)))
                    if errors}
        diffs = {key: {"megu": a.get(key), "reference": b.get(key)}
                 for key in COMPARISON_FIELDS[name] if a.get(key) != b.get(key)}
        row = {"status": "MISMATCH" if failures or diffs else "PASS",
               "differences": diffs, "validation_failures": failures}
        if name in MOVEMENT_STEPS:
            positions_a, positions_b = a.get("server_positions", []), b.get("server_positions", [])
            limit = 0.03 if name == "submerged_ticks" else 1e-5
            if len(positions_a) != len(positions_b):
                diffs["server_positions_length"] = {
                    "megu": len(positions_a), "reference": len(positions_b)}
            else:
                delta = max((abs(x - y) for pa, pb in zip(positions_a, positions_b)
                             for x, y in zip(pa, pb)), default=0)
                row["max_position_delta"] = delta
                row["position_tolerance"] = limit
                if delta > limit:
                    diffs["server_positions"] = {
                        "megu": positions_a, "reference": positions_b,
                        "max_delta": delta, "tolerance": limit}
            if diffs:
                row["status"] = "MISMATCH"
        comparisons[name] = row
    return comparisons


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--with-vanilla", action="store_true", help="spawn pinned official JVM")
    parser.add_argument("--vanilla-jar", type=Path, default=JAR)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/megu")
    parser.add_argument("--build", action="store_true", help="build Rust with offline lockfile")
    args = parser.parse_args()
    (ROOT / "target/tmp").mkdir(parents=True, exist_ok=True)
    os.environ["TMPDIR"] = str(ROOT / "target/tmp")
    if args.build or not args.binary.is_file():
        subprocess.run(["cargo", "build", "--offline", "--locked"], cwd=ROOT, check=True)
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error(f"missing server binary {binary}")
    os.environ.setdefault("MC_PROBE_DEADLINE", "30")
    assert HARNESS_ROOT == ROOT
    left = run_target("megu", binary, args.vanilla_jar)
    right_kind = "vanilla" if args.with_vanilla else "megu"
    right = run_target(right_kind, binary, args.vanilla_jar)
    comparisons = compare(left, right)
    report = {
        "protocol": 767,
        "mode": "vanilla-differential" if args.with_vanilla else "self-consistency",
        "seed": 4242,
        "megu": left,
        "reference": right,
        "scenarios": comparisons,
    }
    print(json.dumps(report, indent=2, sort_keys=True))
    count = sum(row["status"] == "PASS" for row in comparisons.values())
    print(f"client vanilla parity: {count}/{len(comparisons)} PASS, "
          f"{len(comparisons) - count} MISMATCH ({report['mode']})")
    if count != len(comparisons):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
