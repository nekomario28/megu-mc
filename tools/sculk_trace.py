#!/usr/bin/env python3
"""Extract and compare sculk charge-cursor traces (native agent vs the port).

    tools/sculk_trace.py patches <run.log> [--out <file>] [--origins <file>]
    tools/sculk_trace.py extract <run.log> [--out <trace>] [--rounds N] [--origin x,y,z] [--lines N]
    tools/sculk_trace.py compare <native.patches> <port.log> [--quiet]

Why: a cell set says *that* two implementations differ; a cursor's trajectory says *where*
they part. `tools/carve-agent` prints one `SCURSOR` line per `ChargeCursor.update` call -
`SCURSOR r=<round> i=<index> x y z charge decayDelay updateDelay facingMask`, where `r` counts
`updateCursors` calls and `i` is the cursor's index inside that round (a cursor that reaches
charge 0 is dropped, so the list shifts, and without both numbers the two traces cannot be
aligned past the first dead cursor). The port prints the same line under `SPC_CURSOR_TRACE=1`.

`compare` aligns by `(r, i)` - never by position in the file - and exits non-zero when any
shared key differs, so it can be used as a gate. Two captures of the same fixture do not agree
on *which* patch comes first, or on the global `r` (it counts every `updateCursors` call in the
run), so a trace is compared **patch by patch**: `extract --origin x,y,z` starts at the patch
whose first cursor sits at that cell and rewrites `r` as the patch's own round index. Measured:
that patch's trajectory is byte-identical across two independent captures (60/60 lines). The mask is Direction's declaration order
(down, up, north, south, west, east), the same index the port's face mask uses; `-1` is a null
facing set (the SAME_POSITION-only spreader) and `0` an empty one (the all-types spreader).
"""
import argparse
import re
import sys
from pathlib import Path

LINE = re.compile(r"SCURSOR r=(-?\d+) i=(-?\d+) (-?\d+) (-?\d+) (-?\d+) (-?\d+) (-?\d+) (-?\d+) (-?\d+)")
PATCH = re.compile(r"SPATCH (-?\d+) (-?\d+) (-?\d+)")


def parse(path):
    """(round, index) -> the cursor's state, from either side's log or an extracted trace."""
    out = {}
    for line in Path(path).read_text().splitlines():
        m = LINE.search(line)
        if m:
            key = (int(m.group(1)), int(m.group(2)))
            out[key] = tuple(int(m.group(k)) for k in range(3, 10))
    return out


def parse_patches(path):
    """(patch origin, round within that patch, index) -> state, from a log with `SPATCH` markers.

    Keying by the patch's origin is what makes two runs comparable at all: captures order
    patches differently and `r` counts every `updateCursors` call in the whole run, so the same
    patch can be round 0 in one run and round 192 in another. Within a patch, `r` is rewritten
    as the patch's own round index (`SCURSOR r=` changes mark a new round, exactly as the agent
    emits them: one round = one `updateCursors` call = one index-i line per live cursor).
    """
    out, origin, patch_round, last_round = {}, None, -1, None
    for line in Path(path).read_text().splitlines():
        pm = PATCH.search(line)
        if pm:
            origin = tuple(int(pm.group(k)) for k in range(1, 4))
            patch_round, last_round = -1, None
            continue
        m = LINE.search(line)
        if m and origin is not None:
            round_no, idx = int(m.group(1)), int(m.group(2))
            if round_no != last_round:
                patch_round += 1
                last_round = round_no
            out[(origin, patch_round, idx)] = tuple(int(m.group(k)) for k in range(3, 10))
    return out


def patches(args):
    """Write a patch-scoped fixture: one row per cursor-round, `origin r i state...`."""
    keep = None
    if args.origins:
        keep = {tuple(int(v) for v in ln.split())
                for ln in Path(args.origins).read_text().splitlines() if ln.strip()}
    table = parse_patches(args.log)
    out = Path(args.out) if args.out else None
    lines = []
    written = set()
    for (origin, rnd, idx), state in sorted(table.items()):
        if keep is not None and origin not in keep:
            continue
        lines.append(" ".join(str(v) for v in (origin + (rnd, idx) + state)))
        written.add(origin)
    text = "\n".join(lines) + "\n" if lines else ""
    if out:
        out.write_text(text)
    print(f"sculk_trace: {len(lines)} lines, {len(written)} patches"
          + (f" -> {out}" if out else ""))
    return 0


def read_fixture(path):
    out = {}
    for line in Path(path).read_text().splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        f = [int(v) for v in line.split()]
        out[(tuple(f[0:3]), f[3], f[4])] = tuple(f[5:12])
    return out


def extract(args):
    origin = tuple(int(v) for v in args.origin.split(",")) if args.origin else None
    rows = []
    started = origin is None
    patch_round = -1
    last_round = None
    for line in Path(args.log).read_text().splitlines():
        m = LINE.search(line)
        if not m:
            continue
        round_no, idx = int(m.group(1)), int(m.group(2))
        state = tuple(int(m.group(k)) for k in range(3, 10))
        if not started:
            if state[:3] == origin and idx == 0:
                started = True
            else:
                continue
        if args.rounds is not None and round_no > args.rounds:
            continue
        if origin is not None:
            # the global round counts every updateCursors call in the run; the patch's own
            # round counter is what its trajectory is indexed by
            if round_no != last_round:
                patch_round += 1
                last_round = round_no
            round_no = patch_round
        rows.append(f"SCURSOR r={round_no} i={idx} " + " ".join(str(v) for v in state))
        if args.lines is not None and len(rows) >= args.lines:
            break
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


def compare_patches(args):
    """Join the native fixture and the port's log on `(origin, patch_round, index)`, per patch."""
    native = read_fixture(args.native)
    port = parse_patches(args.port)
    shared = sorted(set(native) & set(port))
    total = sum(1 for k in shared if native[k] == port[k])
    by_patch = {}
    for k in shared:
        origin = k[0]
        ok, seen = by_patch.get(origin, (0, 0))
        by_patch[origin] = (ok + (1 if native[k] == port[k] else 0), seen + 1)
    full = sum(1 for ok, seen in by_patch.values() if ok == seen)
    print(f"sculk_trace: native {len(native)} lines / {len({k[0] for k in native})} patches, "
          f"port {len(port)} lines / {len({k[0] for k in port})} patches")
    print(f"sculk_trace: {len(shared)} shared cursor-rounds, {total} identical, "
          f"{len(by_patch)} shared patches, {full} fully identical")
    worst = sorted(by_patch.items(), key=lambda kv: kv[1][0] - kv[1][1], reverse=True)[:3]
    for origin, (ok, seen) in worst:
        print(f"  patch {origin[0]},{origin[1]},{origin[2]}: {seen - ok} of {seen} differ")
    if total != len(shared):
        for k in shared:
            if native[k] != port[k]:
                print(f"sculk_trace: first divergence {k}: native {native[k]} port {port[k]}")
                break
    return 0 if total == len(shared) else 1


def main():
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest="cmd", required=True)
    e = sub.add_parser("extract", help="pull SCURSOR lines out of a run log")
    e.add_argument("log")
    e.add_argument("--out", type=Path)
    e.add_argument("--rounds", type=int, help="keep only rounds <= this")
    e.add_argument("--origin", help="start at the patch whose first cursor is at x,y,z, and "
                                    "index rounds from that patch (captures order patches "
                                    "differently and number rounds globally)")
    e.add_argument("--lines", type=int, help="stop after this many lines")
    pt = sub.add_parser("patches", help="write a patch-scoped fixture from a log with SPATCH markers")
    pt.add_argument("log")
    pt.add_argument("--out", help="where to write the fixture")
    pt.add_argument("--origins", help="only these patches (one `x y z` per line)")
    pt.set_defaults(func=patches)
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
