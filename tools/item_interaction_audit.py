#!/usr/bin/env python3
"""Exhaustive 1.21.1 item-interaction coverage audit.

Reads the authoritative `minecraft:item` registry out of
`proto/src/data_v767.rs` (`V767_ITEMS`, 1333 entries — read-only), cross-checks
every item against the `ITEM_USE_CLASSIFICATION` table in
`core/src/dye_and_items.rs`, and scans `src/play_protocol.rs` +
`src/menus_and_items.rs` (both read-only) for live-server handling evidence.

It then prints the Category-by-Category Live Server Wiring Scoreboard: for each
`ItemUseFamily` — how many registry items exist, how many the kernel classifies
("Kernel Covered"), how many appear in the live sources under a non-table
mention ("Live Wired"), and how many still need a hook ("Missing Live Hook").

Heuristic honesty notes (read before trusting Live Wired):

* "Live Wired" is a *mention* scan: the item name (bare `bow` or namespaced
  `minecraft:bow`) must appear in `src/play_protocol.rs` or
  `src/menus_and_items.rs` on a line that is not a pure name-tuple table row.
  Giant data tables (an `ITEM_NAMES`-style mapping that lists every registry
  name once) would otherwise mark ~all 1333 items "wired" without handling a
  single one. Lines matching PURE_TABLE_ROW are skipped and counted, and the
  skip totals are printed — a silently dropped line is indistinguishable from
  a negative result otherwise.
* A generic dispatch (the server calling `dye_and_items::classify_item_use`)
  does not mention item names at all; that is reported separately as the
  `kernel-dispatch referenced` flag, not folded into per-item counts.
* The audit fails (exit 1) on *inconsistency* — a registry item absent or
  duplicated in the kernel table, an extra name not in the registry, or
  unparsed rows inside the table block. "Missing Live Hook" is reported, not
  failed — the scoreboard exists precisely to enumerate that wiring backlog.
  Pass `--fail-on-missing-hooks` to also fail when hooks are missing.

  --scoreboard     print only the scoreboard (default output)
  --verbose        additionally list every missing-hook item per family
  --fail-on-missing-hooks   exit 1 when any item lacks a live mention
"""
import argparse
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REGISTRY_RS = ROOT / "proto" / "src" / "data_v767.rs"
KERNEL_RS = ROOT / "core" / "src" / "dye_and_items.rs"
LIVE_RS = [ROOT / "src" / "play_protocol.rs", ROOT / "src" / "menus_and_items.rs"]

FAMILIES = [
    "ToolBlockTransform",
    "FluidContainer",
    "Consumable",
    "RangedOrThrowable",
    "Equipable",
    "EntityUse",
    "WorldSpecial",
    "PureMaterial",
]

# A line carrying nothing but quoted `name`/`minecraft:name` literals, commas
# and whitespace is a data-table row, not interaction logic — it is skipped
# (and logged per file so a silently dropped line is never read as absence).
PURE_TABLE_ROW = re.compile(r'^\s*("(?:minecraft:)?[a-z0-9_]+",?\s*)+$')
# `"name" => <scalar>,` map arms are id/lookup tables (menus_and_items.rs
# carries the 1333-entry `name_to_item` map) — registry data, not handling.
# An arm whose RHS calls code (`handle_bow()`, `Some(`) stays a hit.
MAP_TABLE_ROW = re.compile(
    r'^\s*"(?:minecraft:)?[a-z0-9_]+"\s*=>\s*[^;{()]*,?\s*$'
)
NAME_LITERAL = re.compile(r'"minecraft:([a-z0-9_]+)"')
KERNEL_ROW = re.compile(r'\("([a-z0-9_]+)",\s*ItemUseFamily::([A-Za-z]+)\)')


@dataclass
class AuditResult:
    registry: list = field(default_factory=list)          # bare names
    kernel: dict = field(default_factory=dict)            # name -> family
    kernel_duplicates: list = field(default_factory=list)
    kernel_extra: list = field(default_factory=list)      # in table, not registry
    kernel_skipped_lines: list = field(default_factory=list)
    live_hits: dict = field(default_factory=dict)         # name -> {file: count}
    skipped_table_rows: dict = field(default_factory=dict)  # file -> count
    dispatch_refs: dict = field(default_factory=dict)     # file -> count
    errors: list = field(default_factory=list)

    def missing_kernel(self):
        ks = set(self.kernel)
        return [n for n in self.registry if n not in ks]


def parse_registry_items(path=REGISTRY_RS):
    """Extract `V767_ITEMS` → list of bare item names (registry order)."""
    src = Path(path).read_text()
    m = re.search(r"V767_ITEMS\s*:\s*\[&str;\s*\d+\]\s*=\s*\[(.*?)\];", src, re.S)
    if not m:
        raise ValueError(f"V767_ITEMS array not found in {path}")
    names = NAME_LITERAL.findall(m.group(1))
    if not names:
        raise ValueError(f"V767_ITEMS parsed empty from {path}")
    return names


def parse_kernel_table(path=KERNEL_RS):
    """Extract `ITEM_USE_CLASSIFICATION` → (name->family dict, dup, skipped)."""
    src = Path(path).read_text()
    m = re.search(
        r"ITEM_USE_CLASSIFICATION\s*:\s*&\[\(&str,\s*ItemUseFamily\)\]\s*=\s*&\[(.*?)\n\s*\];",
        src,
        re.S,
    )
    if not m:
        raise ValueError(f"ITEM_USE_CLASSIFICATION table not found in {path}")
    table, dup, skipped = {}, [], []
    for line in m.group(1).splitlines():
        s = line.strip()
        if not s or s.startswith("//"):
            continue
        rows = KERNEL_ROW.findall(line)
        # Anything left after removing the row tuples is unparsed junk —
        # a silently dropped token must not read as a negative result.
        residual = KERNEL_ROW.sub("", line).replace(",", "").strip()
        if not rows or residual:
            skipped.append(s[:120])
        for name, fam in rows:
            if name in table:
                dup.append(name)
            table[name] = fam
    return table, dup, skipped


def strip_comments_and_table_rows(src):
    """Return (non-table source text, skipped row count)."""
    kept, skipped = [], 0
    in_block_comment = False
    for line in src.splitlines():
        # Strip `//` comments first so a `/*` inside one cannot wedge the
        # block-comment tracker.
        s = line.split("//", 1)[0]
        if "/*" in s:
            in_block_comment = True
        if in_block_comment:
            if "*/" in s:
                in_block_comment = False
                s = s.split("*/", 1)[1]
            else:
                continue
        if PURE_TABLE_ROW.match(s) or MAP_TABLE_ROW.match(s):
            skipped += 1
            continue
        kept.append(s)
    return "\n".join(kept), skipped


def scan_live_sources(paths=LIVE_RS):
    """Build per-item mention counts over non-table lines of the live sources.

    Returns (hits{name: {file: count}}, skipped{file: rows}, dispatch{file: n}).
    """
    hits, skipped_rows, dispatch = {}, {}, {}
    for path in paths:
        path = Path(path)
        if not path.exists():
            continue
        body, skipped = strip_comments_and_table_rows(path.read_text())
        skipped_rows[path.name] = skipped
        dispatch[path.name] = len(
            re.findall(r"dye_and_items|classify_item_use", body)
        )
        # Every literal/identifier token that could name an item.
        tokens = set(re.findall(r'"(?:minecraft:)?([a-z0-9_]+)"', body))
        tokens |= set(re.findall(r"\b([a-z][a-z0-9_]{2,})\b", body))
        for tok in tokens:
            hits.setdefault(tok, {})[path.name] = hits.get(tok, {}).get(
                path.name, 0
            ) + 1
    return hits, skipped_rows, dispatch


def run_audit(verbose=False, registry_path=None, kernel_path=None, live_paths=None):
    """Execute the audit; returns an AuditResult (never exits)."""
    res = AuditResult()
    try:
        res.registry = parse_registry_items(registry_path or REGISTRY_RS)
    except (OSError, ValueError) as e:
        res.errors.append(f"registry: {e}")
        return res
    try:
        res.kernel, res.kernel_duplicates, res.kernel_skipped_lines = (
            parse_kernel_table(kernel_path or KERNEL_RS)
        )
    except (OSError, ValueError) as e:
        res.errors.append(f"kernel: {e}")
        return res
    reg = set(res.registry)
    res.kernel_extra = sorted(n for n in res.kernel if n not in reg)
    res.live_hits, res.skipped_table_rows, res.dispatch_refs = scan_live_sources(
        live_paths or LIVE_RS
    )
    for n, fam in res.kernel.items():
        if fam not in FAMILIES:
            res.errors.append(f"unknown ItemUseFamily::{fam} on '{n}'")
    if res.kernel_duplicates:
        res.errors.append(
            f"duplicate kernel rows: {sorted(set(res.kernel_duplicates))[:10]}"
        )
    missing = res.missing_kernel()
    if missing:
        res.errors.append(
            f"{len(missing)} registry item(s) missing from kernel: "
            + ", ".join(missing[:20])
        )
    if res.kernel_extra:
        res.errors.append(
            f"{len(res.kernel_extra)} kernel row(s) not in registry: "
            + ", ".join(res.kernel_extra[:20])
        )
    if res.kernel_skipped_lines:
        res.errors.append(
            f"{len(res.kernel_skipped_lines)} unparsed line(s) inside "
            f"ITEM_USE_CLASSIFICATION: {res.kernel_skipped_lines[:5]}"
        )
    return res


def scoreboard(res):
    """Render the Category-by-Category Live Server Wiring Scoreboard."""
    fam_items = {f: [] for f in FAMILIES}
    for name in res.registry:
        fam_items[res.kernel.get(name, "PureMaterial")].append(name)
    wired = {n: bool(res.live_hits.get(n)) for n in res.registry}
    lines = []
    lines.append("=" * 78)
    lines.append("  1.21.1 ITEM INTERACTION COVERAGE — LIVE SERVER WIRING SCOREBOARD")
    lines.append("=" * 78)
    lines.append(
        f"  {'ItemUseFamily':<20} {'Items':>6} {'Kernel':>7} "
        f"{'Wired':>7} {'Missing':>8} {'Wired%':>7}"
    )
    lines.append("  " + "-" * 72)
    totals = [0, 0, 0, 0]
    for fam in FAMILIES:
        names = fam_items[fam]
        k = len(names)
        w = sum(1 for n in names if wired.get(n))
        miss = k - w
        totals[0] += k
        totals[1] += k
        totals[2] += w
        totals[3] += miss
        pct = (100.0 * w / k) if k else 100.0
        lines.append(
            f"  {fam:<20} {k:>6} {k:>7} {w:>7} {miss:>8} {pct:>6.1f}%"
        )
    lines.append("  " + "-" * 72)
    pct = 100.0 * totals[2] / max(totals[0], 1)
    lines.append(
        f"  {'TOTAL':<20} {totals[0]:>6} {totals[1]:>7} "
        f"{totals[2]:>7} {totals[3]:>8} {pct:>6.1f}%"
    )
    lines.append("")
    dispatch_any = sum(res.dispatch_refs.values())
    lines.append(
        f"  kernel-dispatch references in src/ (dye_and_items/classify_item_use): {dispatch_any}"
    )
    for f, n in sorted(res.skipped_table_rows.items()):
        lines.append(f"  skipped pure-table rows in {f}: {n}")
    if res.kernel_skipped_lines:
        lines.append(
            f"  UNPARSED kernel-table lines: {res.kernel_skipped_lines[:10]}"
        )
    return lines, fam_items, wired


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--verbose", action="store_true")
    ap.add_argument("--fail-on-missing-hooks", action="store_true")
    args = ap.parse_args(argv)
    res = run_audit(verbose=args.verbose)
    lines, fam_items, wired = scoreboard(res)
    for l in lines:
        print(l)
    for e in res.errors:
        print(f"  ERROR: {e}", file=sys.stderr)
    if args.verbose:
        for fam in FAMILIES:
            missing = [n for n in fam_items[fam] if not wired.get(n)]
            if missing:
                print(f"\n[{fam}] missing live hook ({len(missing)}):")
                for n in missing:
                    print(f"    minecraft:{n}")
    if res.errors:
        return 1
    if args.fail_on_missing_hooks and any(not wired.get(n) for n in res.registry):
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
