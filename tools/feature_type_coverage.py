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
        raise ValueError("missing pinned worldgen directory: " + str(d))
    for f in sorted(d.rglob("*.json")):
        out[f.relative_to(d).with_suffix('').as_posix()] = json.loads(f.read_text())
    if not out:
        raise ValueError("empty pinned worldgen directory: " + str(d))
    return out


def implemented_arms():
    src = (ROOT / "src/feature.rs").read_text()
    m = re.search(r"fn p_cf\(j: &J\) -> Option<Cf> \{(.*?)\n\}\n", src, re.S)
    if not m:
        raise ValueError("configured-feature parser was not found")
    # This is the formatted outer match's literal arms, not every JSON key and
    # block name mentioned inside a branch. It does not prove a branch works.
    arms = set()
    for pattern in re.findall(r'^        ((?:"[a-z_0-9]+"\s*\|\s*)*"[a-z_0-9]+")\s*=>', m.group(1), re.M):
        arms.update(re.findall(r'"([a-z_0-9]+)"', pattern))
    if not arms:
        raise ValueError("no outer configured-feature arms found")
    return arms


def normalize_inline(cfs, pfs):
    """Give inline configured features identities without confusing providers."""
    inline = {}

    def walk(value, origin, path):
        if isinstance(value, list):
            return [walk(v, origin, path + '/' + str(i)) for i, v in enumerate(value)]
        if not isinstance(value, dict):
            return value
        out = {k: walk(v, origin, path + '/' + k) for k, v in value.items()}
        feature = out.get('feature')
        if isinstance(feature, dict) and 'type' in feature:
            name = '@inline/' + origin + path + '/feature'
            inline[name] = feature
            out['feature'] = name
        return out

    configured = {name: walk(value, 'configured/' + name, '') for name, value in cfs.items()}
    placed = {name: walk(value, 'placed/' + name, '') for name, value in pfs.items()}
    configured.update(inline)
    return configured, placed, set(inline)


def configured_reach(cfs, pfs):
    """Resolve named and inline placed/configured references with their roles."""
    feature_keys = {'feature', 'features', 'default', 'vegetation_feature', 'tree_feature'}

    def walk_config(node, seen):
        result = set()
        if isinstance(node, dict):
            if 'feature' in node and 'placement' in node:
                return visit_cf(node['feature'], seen)
            for key, value in node.items():
                if key in feature_keys:
                    result.update(visit_pf(value, seen))
                elif isinstance(value, (dict, list)):
                    result.update(walk_config(value, seen))
        elif isinstance(node, list):
            for value in node:
                result.update(walk_config(value, seen))
        return result

    def visit_cf(node, seen):
        if not isinstance(node, str):
            raise ValueError('configured reference was not normalized')
        name = node.removeprefix('minecraft:')
        marker = ('configured', name)
        if marker in seen:
            raise ValueError('cyclic configured feature: ' + name)
        if name not in cfs:
            raise ValueError('missing configured feature: ' + name)
        cfg = cfs[name]
        return {name} | walk_config(cfg.get('config', cfg), seen | {marker})

    def visit_pf(node, seen):
        if isinstance(node, str):
            name = node.removeprefix('minecraft:')
            marker = ('placed', name)
            if marker in seen:
                raise ValueError('cyclic placed feature: ' + name)
            if name not in pfs:
                raise ValueError('missing placed feature: ' + name)
            return visit_pf(pfs[name], seen | {marker})
        if isinstance(node, list):
            return set().union(*(visit_pf(value, seen) for value in node))
        if isinstance(node, dict):
            return walk_config(node, seen)
        raise ValueError('invalid placed feature reference')

    return {name: visit_pf(value, {('placed', name)}) for name, value in pfs.items()}


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
    file_count = len(cfs)
    cfs, pfs, inline = normalize_inline(cfs, pfs)
    biomes = load_dir("biome")
    arms = implemented_arms()

    types = defaultdict(list)
    for name, cfg in cfs.items():
        types[cfg.get("type", "?").replace("minecraft:", "")].append(name)

    # Configured references inside an inline placed feature stay configured;
    # selector references are placed. Their names need not be interchangeable.
    reach = configured_reach(cfs, pfs)
    pf_of_cf = defaultdict(set)
    for pf, configured in reach.items():
        for cf in configured:
            pf_of_cf[cf].add(pf)

    def biomes_reaching(pf):
        # a biome's `features` is one list per generation step
        names = [b for b, cfg in biomes.items()
                 if any(pf in reach.get(f.removeprefix("minecraft:"), set())
                        for step in cfg.get("features", [])
                        for f in step)]
        return names

    unimplemented = {t: v for t, v in types.items() if t not in arms}
    print(f"configured features: {file_count} files + {len(inline)} inline, {len(types)} types, "
          f"{len(arms)} arms in p_cf, {len(unimplemented)} types with no arm "
          f"({sum(len(v) for v in unimplemented.values())} files)")
    rows = []
    for t, files in sorted(unimplemented.items(), key=lambda kv: -len(kv[1])):
        placed = sorted({pf for f in files for pf in pf_of_cf.get(f, ())})
        biome_names = sorted({b for cf in files for b in biomes_reaching(cf)})
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
