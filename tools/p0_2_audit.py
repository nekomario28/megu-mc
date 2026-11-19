#!/usr/bin/env python3
"""Static P0-2 reachability audit across datapack worldgen inputs and Rust parsers.

  tools/p0_2_audit.py [--json]

Walks the pinned vanilla datapack (`datapacks/vanilla/`) across Overworld,
Nether, and End and cross-checks what is actually reachable against the Rust
parsers in `src/worldgen.rs`, `src/generation_providers.rs`, and `src/feature.rs`:

  1. Density functions reachable from `noise_settings/{overworld,nether,end}`:
     implemented vs explicitly constant/zero-folded vs unsupported/missing refs.
  2. Surface rules & conditions + `biome_is` leaf coverage and `< 128` `Cond::Biome(u128)` mask guard.
  3. Placement modifiers (`p_mod`) & configured features (`p_cf`) reachable per dim.
  4. Biome `carvers`, `spawners`, `spawn_costs` and `structure` / `structure_set` totals.

Exits 1 if any reachable `noise_router` density function, `surface_rule`,
`surface_condition`, `biome_is`, `Cond::Biome(u128)` mask bound, or `placement`
modifier lacks a parser arm or target reference in the pinned datapack, or when
the `Cond::Biome(u128)` type anchor is reverted or mismatched in src/worldgen.rs.
"""
import argparse
import json
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DP = ROOT / "datapacks/vanilla"
WG = DP / "data/minecraft/worldgen"

ZERO_OR_CONST_DF = {
    "blend_alpha": "DF::C(1.0)",
    "blend_offset": "DF::Zero",
    "beardifier": "DF::Zero",
    "end_outer_islands": "DF::Zero",
    "distance_to_point": "DF::Zero",
    "slice": "DF::Zero",
}
END_BIOMES = {
    "minecraft:the_end",
    "minecraft:end_highlands",
    "minecraft:end_midlands",
    "minecraft:small_end_islands",
    "minecraft:end_barrens",
}
DF_CHILD_KEYS = {
    "argument", "argument1", "argument2", "left", "right", "input", "density",
    "from", "to", "coordinate", "value", "when_in_range", "when_out_of_range",
    "alpha", "first", "second", "shift_x", "shift_y", "shift_z", "upper_bound",
}
PF_FEATURE_KEYS = {
    "feature", "features", "default", "vegetation_feature", "tree_feature",
    "feature_true", "feature_false",
}
NOISE_DF_TYPES = {"noise", "shifted_noise", "shift_a", "shift_b", "shift_x", "shift_z", "weird_scaled_sampler"}


class AuditError(Exception):
    """Fail-closed error when expected datapack or Rust parser anchors are missing."""


def strip_ns(name):
    return name.removeprefix("minecraft:")


def extract_match_arms(src_text, fn_marker, end_marker=None):
    start = src_text.find(fn_marker)
    if start < 0:
        raise AuditError(f"missing parser marker: {fn_marker}")
    tail = src_text[start:]
    if end_marker:
        stop = tail.find(end_marker)
        if stop < 0:
            raise AuditError(f"missing parser end marker: {end_marker}")
        tail = tail[:stop]
    arms = set()
    for pat in re.findall(r'^\s+((?:"[a-z0-9_]+"\s*\|\s*)*"[a-z0-9_]+")\s*=>', tail, re.M):
        arms.update(re.findall(r'"([a-z0-9_]+)"', pat))
    if not arms:
        raise AuditError(f"found zero match arms under {fn_marker}")
    return arms


def parse_rust_parsers(root=ROOT):
    wg_src = (root / "src/worldgen.rs").read_text()
    gp_src = (root / "src/generation_providers.rs").read_text()
    feat_src = (root / "src/feature.rs").read_text()
    return {
        "df": extract_match_arms(wg_src, "fn parse_df(", "fn eval("),
        "surface_rule": extract_match_arms(wg_src, "fn rule_node(", "fn resolve_cond("),
        "surface_cond": extract_match_arms(wg_src, "fn parse_cond(", "fn resolve_ref("),
        "placement_modifier": extract_match_arms(
            gp_src, "pub(crate) fn p_mod(", "// ---------- predicate / survival ----------"
        ),
        "configured_feature": extract_match_arms(
            feat_src, "fn p_cf(j: &J) -> Option<Cf> {", "\n}\n"
        ),
    }


def parse_cond_biome_anchor(wg_src):
    """Confirm the `Cond::Biome(u128)` type anchor exists in src/worldgen.rs.

    Fail-closed: raises AuditError when `enum Cond` or the `Biome(u128)`
    variant is missing or the variant type is not `u128`, so a revert of
    `Biome(u128)` to an older mask type can't pass the u128 bound audit
    silently. Returns the confirmed anchor type string ("u128").
    """
    m = re.search(r"enum\s+Cond\b", wg_src)
    if not m:
        raise AuditError("missing 'enum Cond' in src/worldgen.rs")
    m2 = re.search(r"\bBiome\s*\(\s*([A-Za-z0-9_]+)\s*\)", wg_src[m.end():])
    if not m2 or m2.group(1) != "u128":
        raise AuditError(
            f"Cond::Biome anchor is {m2.group(1) if m2 else 'missing'} in src/worldgen.rs; "
            "the u128 mask bound audit requires Biome(u128)"
        )
    return m2.group(1)


def load_json_dir(folder):
    d = WG / folder
    if not d.is_dir():
        raise AuditError(f"missing worldgen directory: {d}")
    out = {}
    for p in sorted(d.rglob("*.json")):
        key = "minecraft:" + p.relative_to(d).with_suffix("").as_posix()
        out[key] = json.loads(p.read_text())
    return out


def tree_biomes():
    tree_path = DP / "multi_noise_biome_tree.json"
    if not tree_path.is_file():
        raise AuditError(f"missing climate tree: {tree_path}")
    tree = json.loads(tree_path.read_text())

    def collect(node, out):
        if isinstance(node, dict):
            if node.get("_type") == "leaf" and "biome" in node:
                out.add(node["biome"])
            for v in node.values():
                collect(v, out)
        elif isinstance(node, list):
            for v in node:
                collect(v, out)

    by_dim = {"end": set(END_BIOMES)}
    for dim in ("overworld", "nether"):
        if dim not in tree:
            raise AuditError(f"missing {dim!r} preset in {tree_path}")
        s = set()
        collect(tree[dim], s)
        by_dim[dim] = s
    return by_dim


def audit_density_and_surface(noise_settings, df_files, noise_files, dim_biomes, parsers):
    dims = {}
    failures = []
    for dim in ("overworld", "nether", "end"):
        key = f"minecraft:{dim}"
        if key not in noise_settings:
            raise AuditError(f"missing noise_settings/{dim}.json")
        ns = noise_settings[key]
        df_types = Counter()
        df_refs = set()
        df_missing = set()
        noise_missing = set()
        seen_refs = set()

        def walk_df(node, parent_key=None, parent_type=None):
            if isinstance(node, str) and parent_key in DF_CHILD_KEYS:
                if parent_key == "argument" and parent_type in ("shift_a", "shift_b", "shift_x", "shift_z"):
                    if node not in noise_files:
                        noise_missing.add(node)
                    return
                if node not in seen_refs:
                    seen_refs.add(node)
                    df_refs.add(node)
                    if node in df_files:
                        walk_df(df_files[node], None, None)
                    else:
                        df_missing.add(node)
            elif isinstance(node, dict):
                node_ty = strip_ns(node["type"]) if "type" in node else None
                if node_ty:
                    df_types[node_ty] += 1
                    if node_ty in NOISE_DF_TYPES and isinstance(node.get("noise"), str):
                        if node["noise"] not in noise_files:
                            noise_missing.add(node["noise"])
                for k, v in node.items():
                    if k in ("type", "noise"):
                        continue
                    walk_df(v, k, node_ty)
            elif isinstance(node, list):
                for v in node:
                    walk_df(v, parent_key, parent_type)

        for k, v in (ns.get("noise_router") or {}).items():
            walk_df(v, k, None)

        sr_types = Counter()
        sc_types = Counter()
        biome_refs = set()

        def walk_sr(node):
            if isinstance(node, dict):
                if "type" in node:
                    sr_types[strip_ns(node["type"])] += 1
                if "if_true" in node:
                    walk_sc(node["if_true"])
                for k in ("then_run", "sequence"):
                    if k in node:
                        walk_sr(node[k])
            elif isinstance(node, list):
                for v in node:
                    walk_sr(v)

        def walk_sc(node):
            if isinstance(node, dict):
                if "type" in node:
                    sc_types[strip_ns(node["type"])] += 1
                if strip_ns(node.get("type", "")) == "biome":
                    b = node.get("biome_is")
                    if isinstance(b, str):
                        biome_refs.add(b)
                    elif isinstance(b, list):
                        biome_refs.update(b)
                for k in ("then", "invert", "argument"):
                    if k in node:
                        walk_sc(node[k])
            elif isinstance(node, list):
                for v in node:
                    walk_sc(v)

        walk_sr(ns.get("surface_rule") or {})

        df_unsupported = sorted(t for t in df_types if t not in parsers["df"])
        df_zero_folded = sorted(t for t in df_types if t in ZERO_OR_CONST_DF)
        sr_unsupported = sorted(t for t in sr_types if t not in parsers["surface_rule"])
        sc_unsupported = sorted(t for t in sc_types if t not in parsers["surface_cond"])
        missing_bleaf = sorted(biome_refs - dim_biomes[dim])
        bleaf_count = len(dim_biomes[dim])

        if df_unsupported:
            failures.append(f"{dim}: unsupported density_function types {df_unsupported}")
        if df_missing:
            failures.append(f"{dim}: missing density_function refs {sorted(df_missing)}")
        if noise_missing:
            failures.append(f"{dim}: missing noise refs {sorted(noise_missing)}")
        if sr_unsupported:
            failures.append(f"{dim}: unsupported surface_rule types {sr_unsupported}")
        if sc_unsupported:
            failures.append(f"{dim}: unsupported surface_condition types {sc_unsupported}")
        if bleaf_count >= 128:
            failures.append(f"{dim}: bleaf count {bleaf_count} overflows Cond::Biome(u128)")
        if missing_bleaf:
            failures.append(f"{dim}: surface biome_is missing from dim bleaf {missing_bleaf}")

        dims[dim] = {
            "df_types": dict(sorted(df_types.items())),
            "df_refs_count": len(df_refs),
            "df_missing_refs": sorted(df_missing),
            "noise_missing_refs": sorted(noise_missing),
            "df_zero_folded_reachable": {t: ZERO_OR_CONST_DF[t] for t in df_zero_folded},
            "df_unsupported": df_unsupported,
            "surface_rule_types": dict(sorted(sr_types.items())),
            "surface_rule_unsupported": sr_unsupported,
            "surface_cond_types": dict(sorted(sc_types.items())),
            "surface_cond_unsupported": sc_unsupported,
            "bleaf_count": bleaf_count,
            "surface_biome_is_count": len(biome_refs),
            "surface_biome_is_missing_from_bleaf": missing_bleaf,
        }
    return dims, failures


def audit_features_and_placements(biomes, placed_features, configured_features, dim_biomes, parsers):
    dims = {}
    failures = []
    for dim in ("overworld", "nether", "end"):
        seen_pf = set()
        seen_cf = set()
        pm_types = Counter()
        cf_types = Counter()
        missing_pf = set()
        missing_cf = set()

        def visit_pf(node):
            if isinstance(node, str):
                if node in seen_pf:
                    return
                seen_pf.add(node)
                if node not in placed_features:
                    missing_pf.add(node)
                    return
                walk_node(placed_features[node])
            elif isinstance(node, list):
                for item in node:
                    visit_pf(item)
            elif isinstance(node, dict):
                walk_node(node)

        def walk_node(node):
            if isinstance(node, dict):
                if "feature" in node and "placement" in node:
                    for pm in node.get("placement") or []:
                        if isinstance(pm, dict) and "type" in pm:
                            pm_types[strip_ns(pm["type"])] += 1
                    visit_cf(node["feature"])
                    return
                for k, v in node.items():
                    if k in PF_FEATURE_KEYS:
                        visit_pf(v)
                    elif isinstance(v, (dict, list)):
                        walk_node(v)
            elif isinstance(node, list):
                for v in node:
                    walk_node(v)

        def visit_cf(node):
            if isinstance(node, str):
                if node in seen_cf:
                    return
                seen_cf.add(node)
                if node not in configured_features:
                    missing_cf.add(node)
                    return
                cfg = configured_features[node]
                if isinstance(cfg, dict) and "type" in cfg:
                    cf_types[strip_ns(cfg["type"])] += 1
                    walk_node(cfg.get("config", cfg))
            elif isinstance(node, dict):
                if "feature" in node and "placement" in node:
                    walk_node(node)
                elif "type" in node:
                    cf_types[strip_ns(node["type"])] += 1
                    walk_node(node.get("config", node))

        for bname in sorted(dim_biomes[dim]):
            b = biomes.get(bname)
            if not b:
                raise AuditError(f"missing biome json for {bname}")
            for step in b.get("features") or []:
                for entry in step:
                    visit_pf(entry)

        pm_unsupported = sorted(t for t in pm_types if t not in parsers["placement_modifier"])
        cf_unsupported = sorted(t for t in cf_types if t not in parsers["configured_feature"])
        if pm_unsupported:
            failures.append(f"{dim}: unsupported placement modifiers {pm_unsupported}")
        if missing_pf:
            failures.append(f"{dim}: missing placed_feature refs {sorted(missing_pf)}")
        if missing_cf:
            failures.append(f"{dim}: missing configured_feature refs {sorted(missing_cf)}")

        dims[dim] = {
            "placed_features_reachable": len(seen_pf),
            "configured_features_reachable": len(seen_cf),
            "placement_modifier_types": dict(sorted(pm_types.items())),
            "placement_modifier_unsupported": pm_unsupported,
            "configured_feature_unimplemented_reachable": cf_unsupported,
            "missing_pf": sorted(missing_pf),
            "missing_cf": sorted(missing_cf),
        }
    return dims, failures


def audit_biomes_and_structures(biomes, structures, structure_sets):
    with_carvers = 0
    with_spawners = 0
    with_spawn_costs = {}
    for name, b in sorted(biomes.items()):
        carvers = b.get("carvers") or {}
        if isinstance(carvers, dict):
            if any(carvers.values()):
                with_carvers += 1
        elif carvers:
            with_carvers += 1
        spawners = b.get("spawners") or {}
        if any(spawners.values()):
            with_spawners += 1
        costs = b.get("spawn_costs") or {}
        if costs:
            with_spawn_costs[name] = len(costs)

    struct_types = defaultdict(list)
    for name, s in sorted(structures.items()):
        struct_types[s.get("type", "<none>")].append(strip_ns(name))

    return {
        "biomes_total": len(biomes),
        "biomes_with_air_carvers": with_carvers,
        "biomes_with_spawners": with_spawners,
        "biomes_with_spawn_costs": with_spawn_costs,
        "structures_total": len(structures),
        "structure_sets_total": len(structure_sets),
        "structure_types": {k: sorted(v) for k, v in sorted(struct_types.items())},
    }


def run_audit(root=ROOT):
    parsers = parse_rust_parsers(root)
    anchor = parse_cond_biome_anchor((root / "src/worldgen.rs").read_text())
    noise_settings = load_json_dir("noise_settings")
    df_files = load_json_dir("density_function")
    noise_files = load_json_dir("noise")
    biomes = load_json_dir("biome")
    placed_features = load_json_dir("placed_feature")
    configured_features = load_json_dir("configured_feature")
    structures = load_json_dir("structure")
    structure_sets = load_json_dir("structure_set")
    dim_biomes = tree_biomes()

    df_surf, f1 = audit_density_and_surface(noise_settings, df_files, noise_files, dim_biomes, parsers)
    feat_pm, f2 = audit_features_and_placements(biomes, placed_features, configured_features, dim_biomes, parsers)
    bio_struct = audit_biomes_and_structures(biomes, structures, structure_sets)

    return {
        "parsers": {k: len(v) for k, v in parsers.items()},
        "cond_biome_anchor": anchor,
        "density_and_surface": df_surf,
        "features_and_placements": feat_pm,
        "biomes_and_structures": bio_struct,
        "failures": f1 + f2,
    }


def format_report(report):
    lines = ["== P0-2 static & datapack reachability audit =="]
    lines.append(
        "parsers: " + ", ".join(f"{k}={v}" for k, v in sorted(report["parsers"].items()))
    )
    for dim in ("overworld", "nether", "end"):
        ds = report["density_and_surface"][dim]
        fp = report["features_and_placements"][dim]
        lines.append(
            f"[{dim}] bleaf={ds['bleaf_count']}/128 "
            f"df_types={len(ds['df_types'])} (zero_folded={list(ds['df_zero_folded_reachable'])}, unsupported={ds['df_unsupported']}) "
            f"surface=({len(ds['surface_rule_types'])} rules, {len(ds['surface_cond_types'])} conds, "
            f"biome_is={ds['surface_biome_is_count']}, missing_bleaf={ds['surface_biome_is_missing_from_bleaf']})"
        )
        lines.append(
            f"        pf_reachable={fp['placed_features_reachable']} pm_types={len(fp['placement_modifier_types'])} "
            f"(unsupported={fp['placement_modifier_unsupported']}) "
            f"cf_reachable={fp['configured_features_reachable']} unimpl_cf={fp['configured_feature_unimplemented_reachable']}"
        )
    bs = report["biomes_and_structures"]
    lines.append(
        f"[biomes] total={bs['biomes_total']} carvers={bs['biomes_with_air_carvers']} "
        f"spawners={bs['biomes_with_spawners']} spawn_costs={len(bs['biomes_with_spawn_costs'])} biomes"
    )
    lines.append(
        f"[structures] sets={bs['structure_sets_total']} definitions={bs['structures_total']} "
        f"types={len(bs['structure_types'])} (P2-1 gap; e.g. minecraft:mineshaft={bs['structure_types'].get('minecraft:mineshaft', [])})"
    )
    if report["failures"]:
        lines.append("FAILURES:")
        for f in report["failures"]:
            lines.append(f"  - {f}")
    else:
        lines.append("p0_2_audit: PASS (0 unsupported reachable parser nodes, 0 missing refs)")
    return "\n".join(lines) + "\n"


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--json", action="store_true", help="Emit JSON report")
    args = ap.parse_args(argv)
    try:
        report = run_audit()
    except AuditError as exc:
        print(f"p0_2_audit: ERROR: {exc}", file=sys.stderr)
        return 2
    if args.json:
        sys.stdout.write(json.dumps(report, indent=2) + "\n")
    else:
        sys.stdout.write(format_report(report))
    return 1 if report["failures"] else 0


if __name__ == "__main__":
    sys.exit(main())
