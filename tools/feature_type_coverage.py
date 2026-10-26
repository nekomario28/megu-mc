#!/usr/bin/env python3
"""Which configured-feature types the pack uses, which this port implements, and who reaches them.

  tools/feature_type_coverage.py [--unimplemented]

Why: the feature layer's residue is dominated by types that silently do nothing
(`p_cf` has no arm for them, so the placed feature is accepted and places 0
blocks). Counting them by name is not enough - the question a plan needs is
*which of them the overworld actually reaches*, because that decides the order to
implement them in. This reads the pack the server reads:

- every `configured_feature/*.json` type, with a file count per type
- the arms in `p_cf` (`src/feature.rs`), which is what the port can accept
- for each type, the placed features that reference it, transitively (a
  `random_patch`/`sequence`/`random_selector` nests other placed features), and
  the biomes whose feature lists reach those placed features

It is a *static* reader: it does not run generation. `CF_MISSING=1` and
`PF_UNKNOWN=1` are the runtime counterparts - they name what actually failed to
parse during a run, which is the stronger evidence when the two disagree.
"""
import json
import os
import re
import sys
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DATA = ROOT / "datapacks/vanilla/data/minecraft/worldgen"


def load_dir(kind):
    out = {}
    d = DATA / kind
    if not d.is_dir():
        return out
    for f in sorted(d.glob("*.json")):
        try:
            out[f.stem] = json.loads(f.read_text())
        except json.JSONDecodeError:
            continue
    return out


def implemented_arms():
    src = (ROOT / "src/feature.rs").read_text()
    m = re.search(r"fn p_cf\(j: &J\) -> Option<Cf> \{(.*?)\n\}\n", src, re.S)
    return set(re.findall(r'"([a-z_0-9]+)"', m.group(1))) if m else set()


def nested_names(node, acc):
    """Every placed-feature name a configured-feature config refers to."""
    if isinstance(node, dict):
        if "feature" in node and isinstance(node["feature"], str):
            acc.add(node["feature"].replace("minecraft:", ""))
        for k, v in node.items():
            if k in ("features", "feature", "default", "data"):
                nested_names(v, acc)
            elif isinstance(v, (dict, list)):
                nested_names(v, acc)
    elif isinstance(node, list):
        for v in node:
            nested_names(v, acc)


# Name-based, because the pack's biome files do not name their dimension: the
# nether and end biomes are a fixed, small set. Anything else is treated as
# overworld, which is the scope the roadmap's feature work is about.
NON_OVERWORLD = {
    "nether_wastes", "soul_sand_valley", "crimson_forest", "warped_forest",
    "basalt_deltas", "the_end", "end_highlands", "end_midlands",
    "small_end_islands", "end_barrens", "the_void",
}


def main():
    only_unimplemented = "--unimplemented" in sys.argv
    overworld_only = "--overworld" in sys.argv
    cfs = load_dir("configured_feature")
    pfs = load_dir("placed_feature")
    biomes = load_dir("biome")
    arms = implemented_arms()

    types = defaultdict(list)
    for name, cfg in cfs.items():
        types[cfg.get("type", "?").replace("minecraft:", "")].append(name)

    # which placed features reference each configured feature, and what they nest
    pf_of_cf = defaultdict(set)
    for pf, cfg in pfs.items():
        pf_of_cf[cfg.get("feature", "").replace("minecraft:", "")].add(pf)
    reach = {}

    def reached_by(pf, seen):
        if pf in seen:
            return set()
        seen.add(pf)
        out = {pf}
        cfg = cfs.get(pfs.get(pf, {}).get("feature", "").replace("minecraft:", ""))
        if cfg:
            acc = set()
            nested_names(cfg, acc)
            for nested in acc:
                out |= reached_by(nested, seen)
        return out

    for pf in list(pfs):
        reach[pf] = reached_by(pf, set())

    def biomes_reaching(pf):
        # a biome's `features` is one list per generation step
        names = [b for b, cfg in biomes.items()
                 if any(pf in reach.get(f.replace("minecraft:", ""), set())
                        for step in cfg.get("features", [])
                        for f in step)]
        return names

    unimplemented = {t: v for t, v in types.items() if t not in arms}
    print(f"configured features: {len(cfs)} files, {len(types)} types, "
          f"{len(arms)} arms in p_cf, {len(unimplemented)} types with no arm "
          f"({sum(len(v) for v in unimplemented.values())} files)")
    rows = []
    for t, files in sorted(unimplemented.items(), key=lambda kv: -len(kv[1])):
        placed = sorted({pf for f in files for pf in pf_of_cf.get(f, ())})
        biome_names = sorted({b for pf in placed for b in biomes_reaching(pf)})
        if overworld_only:
            biome_names = [b for b in biome_names if b not in NON_OVERWORLD]
        rows.append((t, len(files), len(placed), len(biome_names)))
    if only_unimplemented:
        print(f"{'type':28} {'files':>5} {'placed':>6} {'biomes':>6}")
        for t, nf, npf, nb in rows:
            print(f"{t:28} {nf:5d} {npf:6d} {nb:6d}")
    else:
        for t, nf, npf, nb in rows:
            print(f"  {t:28} {nf:3d} files  {npf:3d} placed features  {nb:3d} biomes reach it")
    scope = "overworld" if overworld_only else "any"
    reaching_any_biome = [r for r in rows if r[3] > 0]
    print(f"unimplemented types a biome reaches ({scope}): {len(reaching_any_biome)} "
          f"({', '.join(r[0] for r in reaching_any_biome)})")
    if overworld_only:
        print(f"  the widest of them: "
              + ", ".join(f"{t} in {nb} biomes" for t, _, _, nb in
                          sorted(reaching_any_biome, key=lambda r: -r[3])[:5]))
    return 0


if __name__ == "__main__":
    sys.exit(main())
