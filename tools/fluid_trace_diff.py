#!/usr/bin/env python3
"""Strict same-input 1.21.1 fluid-update event trace comparator.

This accepts already captured observations; it does not invoke Mojang code or
depend on, vendor, or translate third-party Minecraft server implementations.
"""
from __future__ import annotations

import argparse
from collections import Counter
from dataclasses import asdict, dataclass
import json
from pathlib import Path
import sys

FORMAT = "megu-fluid-trace-v1"
VERSION = "1.21.1"
HEADER = "tick\tsequence\tx\ty\tz\tstate"
STATE_COUNT = 26684


class TraceError(ValueError):
    pass


@dataclass(frozen=True)
class Event:
    tick: int
    sequence: int
    x: int
    y: int
    z: int
    state: int

    def outcome(self) -> tuple[int, int, int, int, int]:
        return self.tick, self.x, self.y, self.z, self.state


@dataclass(frozen=True)
class Trace:
    scenario: str
    events: tuple[Event, ...]


def load_trace(path: Path) -> Trace:
    try:
        raw = path.read_bytes()
        text = raw.decode("utf-8")
    except (OSError, UnicodeDecodeError) as e:
        raise TraceError(f"{path}: cannot decode trace: {e}") from e
    if not raw.endswith(b"\n"):
        raise TraceError(f"{path}: missing final newline (possibly truncated)")
    lines = text.splitlines()
    if len(lines) < 6:
        raise TraceError(f"{path}: missing metadata, event, or end marker")
    if lines[0] != f"# format={FORMAT}":
        raise TraceError(f"{path}: wrong format")
    if lines[1] != f"# version={VERSION}":
        raise TraceError(f"{path}: expected Minecraft {VERSION}")
    if not lines[2].startswith("# scenario=") or not lines[2][11:].strip():
        raise TraceError(f"{path}: missing scenario")
    scenario = lines[2][11:]
    if any(c.isspace() for c in scenario):
        raise TraceError(f"{path}: scenario must be a single token")
    if lines[3] != HEADER:
        raise TraceError(f"{path}: expected 6-column TSV header")
    if not lines[-1].startswith("# end=count="):
        raise TraceError(f"{path}: missing end/count marker (possibly truncated)")
    try:
        declared = int(lines[-1][12:])
    except ValueError as e:
        raise TraceError(f"{path}: invalid count marker") from e
    data = lines[4:-1]
    if not data or len(data) != declared:
        raise TraceError(f"{path}: empty or truncated trace ({len(data)} != {declared})")

    events: list[Event] = []
    last_tick = -1
    last_seq = -1
    for line_no, line in enumerate(data, start=5):
        fields = line.split("\t")
        if len(fields) != 6:
            raise TraceError(f"{path}:{line_no}: expected six fields")
        try:
            numbers = [int(field) for field in fields]
        except ValueError as e:
            raise TraceError(f"{path}:{line_no}: noninteger field") from e
        tick, sequence, x, y, z, state = numbers
        if tick < 0 or sequence < 0 or not (0 <= state < STATE_COUNT):
            raise TraceError(f"{path}:{line_no}: invalid tick, sequence or block state")
        if tick < last_tick:
            raise TraceError(f"{path}:{line_no}: out-of-order tick")
        if tick != last_tick and sequence != 0:
            raise TraceError(f"{path}:{line_no}: sequence must reset at each tick")
        if tick == last_tick and sequence != last_seq + 1:
            raise TraceError(f"{path}:{line_no}: missing/duplicate sequence")
        if not all(-(1 << 31) <= c < (1 << 31) for c in (x, y, z)):
            raise TraceError(f"{path}:{line_no}: invalid position")
        events.append(Event(*numbers))
        last_tick, last_seq = tick, sequence
    return Trace(scenario, tuple(events))


def compare(native: Trace, candidate: Trace, examples: int = 6) -> dict:
    if native.scenario != candidate.scenario:
        raise TraceError(f"scenario mismatch: {native.scenario} vs {candidate.scenario}")
    n = Counter(e.outcome() for e in native.events)
    c = Counter(e.outcome() for e in candidate.events)
    missing = n - c
    extra = c - n
    exact = native.events == candidate.events
    first = next(
        (i for i, (a, b) in enumerate(zip(native.events, candidate.events)) if a != b),
        min(len(native.events), len(candidate.events)),
    )
    same_events = not missing and not extra
    return {
        "status": "MATCH" if exact else "DIFF",
        "scenario": native.scenario,
        "version": VERSION,
        "native_events": len(native.events),
        "candidate_events": len(candidate.events),
        "missing_events": sum(missing.values()),
        "extra_events": sum(extra.values()),
        "order_only_difference": same_events and not exact,
        "first_mismatch_index": None if exact else first,
        "examples_missing": [
            {"tick": key[0], "x": key[1], "y": key[2], "z": key[3],
             "state": key[4], "count": count}
            for key, count in sorted(missing.items())[:examples]
        ],
        "examples_extra": [
            {"tick": key[0], "x": key[1], "y": key[2], "z": key[3],
             "state": key[4], "count": count}
            for key, count in sorted(extra.items())[:examples]
        ],
        "first_native": asdict(native.events[first]) if first < len(native.events) and not exact else None,
        "first_candidate": asdict(candidate.events[first]) if first < len(candidate.events) and not exact else None,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--max-examples", type=int, default=6)
    args = parser.parse_args()
    if args.max_examples < 1:
        parser.error("--max-examples must be positive")
    try:
        result = compare(load_trace(args.native), load_trace(args.candidate), args.max_examples)
    except TraceError as e:
        print(f"INVALID TRACE: {e}", file=sys.stderr)
        return 2
    print(json.dumps(result, sort_keys=True, ensure_ascii=False, indent=2))
    return 0 if result["status"] == "MATCH" else 1


if __name__ == "__main__":
    raise SystemExit(main())
