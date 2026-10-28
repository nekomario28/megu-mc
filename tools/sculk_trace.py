#!/usr/bin/env python3
"""Extract and compare sculk charge-cursor traces (native agent vs the port).

    tools/sculk_trace.py extract <run.log> [--out <trace>] [--rounds N]
    tools/sculk_trace.py compare <native.trace> <port.trace> [--quiet]

Why: a cell set says *that* two implementations differ; a cursor's trajectory says *where*
they part. `tools/carve-agent` prints one `SCURSOR` line per `ChargeCursor.update` call -
`SCURSOR r=<round> i=<index> x y z charge decayDelay updateDelay facingMask`, where `r` counts
`updateCursors` calls and `i` is the cursor's index inside that round (a cursor that reaches
charge 0 is dropped, so the list shifts, and without both numbers the two traces cannot be
aligned past the first dead cursor). The port prints the same line under `SPC_CURSOR_TRACE=1`.

`compare` aligns by `(r, i)` - never by position in the file - and exits non-zero when any
shared key differs, so it can be used as a gate. The mask is Direction's declaration order
(down, up, north, south, west, east), the same index the port's face mask uses; `-1` is a null
facing set (the SAME_POSITION-only spreader) and `0` an empty one (the all-types spreader).
"""
import argparse
import re
import sys
from pathlib import Path

LINE = re.compile(r"SCURSOR r=(-?\d+) i=(-?\d+) (-?\d+) (-?\d+) (-?\d+) (-?\d+) (-?\d+) (-?\d+) (-?\d+)")


def parse(path):
    """(round, index) -> the cursor's state, from either side's log or an extracted trace."""
    out = {}
    for line in Path(path).read_text().splitlines():
        m = LINE.search(line)
        if m:
            key = (int(m.group(1)), int(m.group(2)))
            out[key] = tuple(int(m.group(k)) for k in range(3, 10))
    return out


def extract(args):
    rounds = args.rounds
    rows = []
    for line in Path(args.log).read_text().splitlines():
        m = LINE.search(line)
        if not m:
            continue
        if rounds is not None and int(m.group(1)) > rounds:
            continue
        rows.append(" ".join(m.group(0).split()))
    text = "\n".join(rows) + "\n"
    if args.out:
        Path(args.out).write_text(text)
        print(f"sculk_trace: {len(rows)} lines -> {args.out}")
    else:
        sys.stdout.write(text)
    return 0


def compare(args):
    native, port = parse(args.native), parse(args.port)
    shared = sorted(set(native) & set(port))
    differing = [k for k in shared if native[k] != port[k]]
    print(f"native keys {len(native)}, port keys {len(port)}, shared {len(shared)}")
    if shared:
        first = differing[0] if differing else None
        print(f"differing {len(differing)}" + (f"; first at r={first[0]} i={first[1]}" if first else ""))
        if first and not args.quiet:
            print(f"  native {native[first]}")
            print(f"  port   {port[first]}")
    if differing:
        return 1
    print("sculk_trace: every shared cursor state matches")
    return 0


def main():
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest="cmd", required=True)
    e = sub.add_parser("extract", help="pull SCURSOR lines out of a run log")
    e.add_argument("log")
    e.add_argument("--out", type=Path)
    e.add_argument("--rounds", type=int, help="keep only rounds <= this")
    e.set_defaults(func=extract)
    c = sub.add_parser("compare", help="align two traces by (round, index)")
    c.add_argument("native")
    c.add_argument("port")
    c.add_argument("--quiet", action="store_true")
    c.set_defaults(func=compare)
    args = p.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
