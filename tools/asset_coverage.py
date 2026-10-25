#!/usr/bin/env python3
"""Measure how much of the block asset the server actually uses.

Four separate fixes (hardness, canHarvest, tool kind, drops) were all the same
finding, and each one was measured by hand with the same throwaway script. This is
that script, so the remaining fields cost one command each instead of one survey.

The asset is the owner of block knowledge. `assets/blocks_1211.json` carries fields per
block; `assets/block_states_1211.json` (generated from the 1.21.1 jar by
tools/vanilla_data/) carries luminance, light opacity, flags and FULL-face bits per *state*.
The former per-state `assets/pblocks.json` (collision shapes, slipperiness, ...) was removed
because it was byte-identical to a file from another project and was not 1.21.1 data (see
THIRD_PARTY.md); fields that only it carried are listed below as "REMOVED".

  --list                     every field, with whether the server reads it and what
                             covering it would take
  --field F [--rust-fn R]    compare a hand-written match table against field F over
                             every state, and report the divergences

A data-backed field with no hand table is not "fine": it is unmodelled, and the tool
says so rather than reporting zero divergences.

LIMITATION: the comparison assumes the hand table is keyed by *state* id, which is true
of the four tables already fixed. A table keyed by item id (item_stack_max is) will not
line up, and the overlap heuristic will then rank unrelated tables. Check the id space
before trusting a comparison.
"""
import argparse
import json
import re
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SRC = [ROOT / "src" / f for f in ("main.rs", "feature.rs", "tree.rs")]

# Where each field comes from and what a consumer would need. The point of listing the
# note is that "read the field" and "handle the field" are different sizes of job.
FIELDS = {
    # field: (source, note)
    "hardness": ("blocks", "destroy time; already derived from the asset"),
    "material": ("blocks", "which tool mines it; already derived"),
    "harvestTools": ("blocks", "canHarvest; already derived"),
    "drops": ("blocks", "default drop; already derived (first of two only)"),
    "stackSize": ("blocks", "item stacking; hand table in item_stack_max"),
    "resistance": ("blocks", "explosion resistance; no model at all"),
    "emitLight": ("blocks", "light emission; no model at all"),
    "filterLight": ("blocks", "occlusion; partly via state_flags"),
    "transparent": ("blocks", "occlusion; partly via state_flags"),
    "boundingBox": ("blocks", "collision; no shape model"),
    "diggable": ("blocks", "covered by hardness < 0"),
    "collision_shapes": ("removed", "per-state collision; pblocks.json removed - needs extractor extension"),
    "outline_shapes": ("removed", "selection box; pblocks.json removed - needs extractor extension"),
    "luminance": ("states", "per-state light; read by the registry from block_states_1211.json"),
    "opacity": ("states", "per-state light opacity; read by the registry from block_states_1211.json"),
    "blast_resistance": ("removed", "per-state explosion resistance; pblocks.json removed"),
    "slipperiness": ("removed", "ice/slime friction; pblocks.json removed"),
    "velocity_multiplier": ("removed", "honey/soul sand; pblocks.json removed"),
    "jump_velocity_multiplier": ("removed", "honey; pblocks.json removed"),
    "instrument": ("removed", "note block sound; pblocks.json removed"),
    "map_color": ("removed", "map rendering; pblocks.json removed"),
    "piston_behavior": ("removed", "pistons; pblocks.json removed"),
    "state_flags": ("states", "air/replaceable/liquid/solid/fluid bits; read by the registry"),
    "side_flags": ("states", "full_sides bits; read by the registry"),
}


def load_blocks():
    return json.loads((ROOT / "assets" / "blocks_1211.json").read_text())


STATE_FILE_KEYS = {
    "luminance": "luminance",
    "opacity": "light_block",
    "state_flags": "flags",
    "side_flags": "full_sides",
}


def load_states():
    return json.loads((ROOT / "assets" / "block_states_1211.json").read_text())


def per_state(source, field):
    """state id -> value, for every state the asset covers."""
    out = {}
    if source == "blocks":
        for e in load_blocks():
            if field not in e:
                continue
            for st in range(e["minStateId"], e["maxStateId"] + 1):
                out[st] = e[field]
    elif source == "states":
        arr = load_states()[STATE_FILE_KEYS[field]]
        out = dict(enumerate(arr))
    # source == "removed": the field has no data in the repository any more; it reports
    # zero coverage, which is the truth, and the --list note says why.
    return out


def parse_rust_arms(fn):
    """Numeric match arms of a Rust function, as [(lo, hi, value_text)]."""
    for path in SRC:
        if not path.exists():
            continue
        src = path.read_text()
        m = re.search(r"\bfn\s+%s\b" % re.escape(fn), src)
        if not m:
            continue
        end = src.index("\n}\n", m.start())
        body = src[m.start():end]
        arms = []
        for am in re.finditer(
            r"((?:\d+\s*(?:\.\.=\s*\d+)?\s*\|\s*)*\d+\s*(?:\.\.=\s*\d+)?)"
            r"\s*=>\s*(-?[\d.]+)",
            body,
        ):
            for part in am.group(1).split("|"):
                part = part.strip()
                if "..=" in part:
                    a, b = (int(x) for x in part.split("..="))
                    arms.append((a, b, am.group(2)))
                elif part.isdigit():
                    arms.append((int(part), int(part), am.group(2)))
        return path.name, arms
    return None, []


def arms_to_map(arms):
    out = {}
    for lo, hi, v in arms:
        text = v.strip()
        if text in ("true", "false"):
            val = text == "true"
        else:
            val = float(text)
        for s in range(lo, hi + 1):
            out[s] = val
    return out


def as_float(v):
    if isinstance(v, bool):
        return float(v)
    if isinstance(v, (int, float)):
        return float(v)
    return None


def cmd_list():
    print(f"{'field':26s} {'source':8s} {'states':>7s} {'distinct':>9s}  note")
    for field, (source, note) in FIELDS.items():
        cov = per_state(source, field)
        distinct = len({json.dumps(v, sort_keys=True) for v in cov.values()})
        print(f"{field:26s} {source:8s} {len(cov):7d} {distinct:9d}  {note}")
    print(
        "\nA field with states and no note saying 'already derived' is unmodelled or"
        " hand-tabled."
    )


def all_fn_names():
    names = set()
    for path in SRC:
        if not path.exists():
            continue
        for m in re.finditer(r"\bfn\s+([a-z_][a-z0-9_]*)", path.read_text()):
            names.add(m.group(1))
    return sorted(names)


def cmd_field(field):
    if field not in FIELDS:
        print(f"unknown field {field!r}; try --list", file=sys.stderr)
        return 2
    source, note = FIELDS[field]
    cov = per_state(source, field)
    if not cov:
        print(f"{field}: the asset has no values for this", file=sys.stderr)
        return 2
    distinct = Counter(json.dumps(v, sort_keys=True) for v in cov.values())
    print(f"{field} ({source}): {len(cov)} states, {len(distinct)} distinct values")
    print(f"  note: {note}")
    for val, n in distinct.most_common(6):
        print(f"    {n:6d} states = {val}")

    # Which hand table, if any, answers this field today? Any function whose numeric
    # arms land on this field's state ids is a candidate, ranked by overlap. This is
    # the step that used to be a guess.
    best = []
    for name in all_fn_names():
        path, arms = parse_rust_arms(name)
        if out := arms_to_map(arms):
            inter = len(set(out) & set(cov))
            if inter:
                best.append((inter, name, path, len(out)))
    best.sort(reverse=True)
    if best:
        print("  hand tables that overlap this field's states:")
        for inter, name, path, n in best[:6]:
            print(f"    {name:28s} {path:11s} {n:6d} states, {inter} overlapping")
        print(f"  run: --field {field} --rust-fn {best[0][1]}")
    else:
        print("  no hand table overlaps this field: it is unmodelled")
    return 0


def cmd_compare(field, rust_fn):
    source, _ = FIELDS[field]
    cov = per_state(source, field)
    path, arms = parse_rust_arms(rust_fn)
    if path is None:
        print(f"no Rust function named {rust_fn!r}", file=sys.stderr)
        return 2
    table = arms_to_map(arms)
    print(f"{rust_fn} ({path}): {len(arms)} arms covering {len(table)} states")
    print(f"asset {field} ({source}): {len(cov)} states")
    shared = sorted(set(table) & set(cov))
    if not shared:
        print("  the table and the asset do not overlap; check the id space")
        return 1
    diverge = []
    for st in shared:
        a, b = as_float(table[st]), as_float(cov[st])
        if a is None or b is None or abs(a - b) > 1e-9:
            diverge.append((st, table[st], cov[st]))
    missing = sorted(set(cov) - set(table))
    print(f"  agree:      {len(shared) - len(diverge)}")
    print(f"  diverge:    {len(diverge)}")
    print(f"  unlisted:   {len(missing)}  (the asset has a value, the table does not)")
    if diverge[:5]:
        print(f"  diverge e.g.: {diverge[:5]}")
    if missing[:5]:
        print(f"  unlisted e.g.: {missing[:5]}")
    # The interesting number is what fraction of the asset the table answers at all.
    if cov:
        print(f"  coverage:   {100.0 * len(table) / len(cov):.1f}% of {len(cov)} states")
    return 0


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--field")
    ap.add_argument("--rust-fn")
    a = ap.parse_args()
    if a.list:
        cmd_list()
        return 0
    if not a.field:
        ap.print_help()
        return 2
    if a.rust_fn:
        return cmd_compare(a.field, a.rust_fn)
    return cmd_field(a.field)


if __name__ == "__main__":
    sys.exit(main())
