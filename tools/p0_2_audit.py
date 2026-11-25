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
  5. Every biome mob/weighted group and spawn cost against the 1.21.1 entity
     registry, parser, actual dimension dispatch and live main tick call chain.

Exits 1 if any reachable `noise_router` density function, `surface_rule`,
`surface_condition`, `biome_is`, `Cond::Biome(u128)` mask bound, or `placement`
modifier lacks a parser arm or target reference in the pinned datapack, or when
the `Cond::Biome(u128)` type anchor is reverted or mismatched in src/worldgen.rs.
Unmapped, malformed, or unreachable natural spawn entries also fail the audit.
"""
import argparse
import json
import math
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
SPAWNER_CATEGORIES = {
    "monster": "Monster", "creature": "Creature", "ambient": "Ambient",
    "water_creature": "WaterCreature", "water_ambient": "WaterAmbient",
    "underground_water_creature": "UndergroundWaterCreature",
    "axolotls": "Axolotls", "misc": "Misc",
}


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

        def walk_df(node, parent_key=None, parent_type=None, *,
                    noise_missing=noise_missing, seen_refs=seen_refs,
                    df_refs=df_refs, df_missing=df_missing, df_types=df_types):
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

        def walk_sr(node, *, sr_types=sr_types):
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

        def walk_sc(node, *, sc_types=sc_types, biome_refs=biome_refs):
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

        def visit_pf(node, *, seen_pf=seen_pf, missing_pf=missing_pf):
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

        def walk_node(node, *, pm_types=pm_types):
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

        def visit_cf(node, *, seen_cf=seen_cf, missing_cf=missing_cf, cf_types=cf_types):
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


def rust_function(source, name):
    """Take a named Rust function body, excluding comments and adjacent functions."""
    source = re.sub(r"/\*.*?\*/|//[^\n]*", "", source, flags=re.S)
    match = re.search(r"\bfn\s+" + re.escape(name) + r"\s*\(", source)
    if not match:
        raise AuditError(f"missing Rust function anchor: {name}")
    start = source.find("{", match.end())
    if start < 0:
        raise AuditError(f"missing body for Rust function: {name}")
    depth = 0
    for offset in range(start, len(source)):
        if source[offset] == "{":
            depth += 1
        elif source[offset] == "}":
            depth -= 1
            if depth == 0:
                return source[start:offset + 1]
    raise AuditError(f"unclosed Rust function: {name}")


def require_runtime(body, pattern, label):
    if not re.search(pattern, body, re.S):
        raise AuditError(f"missing live spawn anchor: {label}")


def parse_spawn_runtime(root=ROOT, sources=None):
    """Read executable spawn dispatch rather than trusting function names alone."""
    if sources is None:
        sources = {
            name: (root / f"src/{name}.rs").read_text()
            for name in ("entity_registry", "feature", "spawn_costs", "natural_spawner",
                         "nether_spawn_rules", "spawner", "main")
        }
    registry = {}
    for entity, category in re.findall(
        r'entity_spec_1211!\(\s*\d+\s*,\s*"([a-z0-9_]+)"\s*,\s*'
        r'[^,]+,\s*[^,]+,\s*[^,]+,\s*(\w+)\s*,',
        sources["entity_registry"],
    ):
        key = f"minecraft:{entity}"
        if key in registry or category not in SPAWNER_CATEGORIES.values():
            raise AuditError(f"invalid/duplicate entity registry row: {key} ({category})")
        registry[key] = category
    if not registry or not re.search(r"ENTITY_REGISTRY_1211:\s*\[EntitySpec;\s*" + str(len(registry)) + r"\]", sources["entity_registry"]):
        raise AuditError("incomplete ENTITY_REGISTRY_1211 mapping")

    feat = sources["feature"]
    for fn, anchors in {
        "p_spawn_cost": (r'jnum\(j,\s*"energy_budget"\)\?', r'jnum\(j,\s*"charge"\)\?'),
        "p_spawner_data": (r'jstr\(j,\s*"type"\)\?', r'jint\(j,\s*"minCount"',
                           r'jint\(j,\s*"maxCount"', r'jint\(j,\s*"weight"'),
        "spawners_for": (r'\.spawners\.get\(biome\)',),
        "spawn_cost_for": (r'\.spawn_costs\.get\(biome\)',),
    }.items():
        body = rust_function(feat, fn)
        for anchor in anchors:
            require_runtime(body, anchor, f"feature::{fn} / {anchor}")
    for anchor in ("p_spawner_data(e)", "p_spawn_cost(cost)", "by_entity.insert", "entries.push"):
        require_runtime(feat, re.escape(anchor), f"feature registry {anchor}")

    costs = sources["spawn_costs"]
    for fn, anchor in (("cost_at", "feature::spawn_cost_for"),
                       ("can_spawn", "self.cost_at"), ("after_spawn", "self.potential.add_charge")):
        require_runtime(rust_function(costs, fn), re.escape(anchor), f"SpawnCostState::{fn}")

    natural = sources["natural_spawner"]
    potential_impl = natural.split("impl PotentialCalculator {", 1)
    if len(potential_impl) != 2:
        raise AuditError("missing PotentialCalculator implementation")
    require_runtime(
        rust_function(potential_impl[1], "can_spawn"),
        r"self\.get_potential_energy_change\(pos,\s*cost\.charge\)\s*<=\s*cost\.energy_budget",
        "PotentialCalculator uses both charge and energy_budget",
    )
    require_runtime(rust_function(potential_impl[1], "add_charge"),
                    r"self\.charges\.push\(", "PotentialCalculator accumulates charges")
    nightly = rust_function(natural, "spawn_nightly_hostile_scratch")
    nether = rust_function(natural, "spawn_nether_natural_dim_scratch")
    require_runtime(nightly, r"nightly_monster_spawners\(pos\)", "Overworld biome monster lookup")
    require_runtime(nightly, r"choose_nightly_spawner\(entries\)", "Overworld weighted monster selection")
    require_runtime(nightly, r"!\s*matches!\(\s*entry\.kind\.as_str\(\)", "Overworld monster exclusion guard")
    require_runtime(nether, r"rules\.spawners_at\(", "Nether biome spawner lookup")
    require_runtime(nether, r"choose_nether_spawner\(entries", "Nether weighted spawner selection")
    require_runtime(nether, r"entry\.min_count\s*\+\s*rng\.next_bound\(entry\.max_count\s*-\s*entry\.min_count\s*\+\s*1\)",
                    "Nether weighted group-size sampling")
    require_runtime(nether, r"NetherSpawnMob::from_id\(&entry\.kind\)", "Nether mob dispatch")
    for marker in ("SpawnCostState::from_existing", "costs.can_spawn(&entry.kind", "costs.after_spawn(&entry.kind"):
        require_runtime(nether, re.escape(marker), f"Nether spawn cost {marker}")
    category_loop = re.search(r"for\s+category\s+in\s+\[([^]]+)\]", nether)
    if not category_loop:
        raise AuditError("missing Nether live category loop")
    nether_categories = set(re.findall(r"MobCategory::(\w+)", category_loop.group(1)))

    nether_rules = rust_function(sources["nether_spawn_rules"], "spawners_at")
    nether_lookup = {}
    for category, serialized in re.findall(r'MobCategory::(\w+)\s*=>\s*"([a-z_]+)"', nether_rules):
        nether_lookup[serialized] = category
    for category in SPAWNER_CATEGORIES.keys() - {"misc"}:
        if nether_lookup.get(category) != SPAWNER_CATEGORIES[category]:
            raise AuditError(f"missing NetherSpawnRules category lookup: {category}")
    require_runtime(nether_rules, r"feature::spawners_for\(biome,\s*category\)", "Nether rule biome lookup")
    nether_dispatch = {
        key for key in re.findall(r'"(minecraft:[a-z_]+)"\s*=>\s*Self::',
                                  rust_function(sources["nether_spawn_rules"], "from_id"))
    }

    ow_dispatch_match = re.search(r"entry\.kind\.as_str\(\),\s*(.*?)\s*\)", nightly, re.S)
    if not ow_dispatch_match:
        raise AuditError("missing Overworld nightly monster dispatch")
    overworld_monsters = set(re.findall(r'"(minecraft:[a-z_]+)"', ow_dispatch_match.group(1)))
    require_runtime(nightly, r"entry\.kind\.as_str\(\)", "Overworld monster kind guard")

    main = sources["main"]
    tick = rust_function(main, "environment_time_tick")
    require_runtime(tick, r"GR_MOB_SPAWNING\.load\(Ordering::Relaxed\)\s*==\s*1", "gamerule tick gate")
    live_calls = ("spawn_nightly_hostile", "spawn_nether_natural", "spawn_passive_natural",
                  "spawn_aquatic_natural", "spawn_end_natural", "spawn_chunkgen_creatures")
    for fn in live_calls:
        require_runtime(tick, rf"\b{fn}\(t\)\s*;", f"main tick -> {fn}(t)")
        body = rust_function(main if fn not in ("spawn_nightly_hostile", "spawn_nether_natural") else natural, fn)
        if fn == "spawn_nightly_hostile":
            require_runtime(body, r"spawn_nightly_hostile_scratch", "nightly wrapper -> scratch")
        if fn == "spawn_nether_natural":
            require_runtime(body, r"spawn_nether_natural_dim_scratch", "Nether wrapper -> dimension scratch")

    # A category is reachable only when the gated tick calls a weighted
    # biome-table selector whose selected mob reaches the live insertion path.
    scratch = rust_function(natural, "spawn_natural_category_scratch")
    for pattern, label in (
        (r"feature::spawners_for\(&biome,\s*mob_category_key\(category\)\)", "weighted category biome table"),
        (r"choose_nightly_spawner\(entries\)", "weighted category selection"),
        (r"OverworldSpawnMob::from_id\(&entry.kind\)", "category entity dispatch"),
        (r"category\.global_cap\(", "category mob cap"),
        (r"natural_spawn_placement_allows\(", "category fluid and placement guard"),
        (r"rules\.allows\(", "category species predicate"),
        (r"live_natural_spawn\(", "category live insertion"),
    ):
        require_runtime(scratch, pattern, label)
    mob_keys = rust_function(natural, "mob_category_key")
    for category, enum in SPAWNER_CATEGORIES.items():
        if category == "misc":
            continue
        require_runtime(mob_keys, rf'MobCategory::{enum}\s*=>\s*"{category}"',
                        f"weighted category key {category}")
    aquatic = rust_function(main, "spawn_aquatic_natural")
    ending = rust_function(main, "spawn_end_natural")
    overworld_categories = set()
    for name, body, dimension in (("spawn_aquatic_natural", aquatic, "Overworld"),
                                  ("spawn_end_natural", ending, "End")):
        for enum in re.findall(
            rf"spawn_natural_category_scratch\(&clients,\s*DimId::{dimension},\s*MobCategory::(\w+),\s*tick\)",
            body,
        ):
            if enum not in SPAWNER_CATEGORIES.values():
                raise AuditError(f"unknown live category in {name}: {enum}")
            if dimension == "Overworld":
                overworld_categories.add(enum)
    end_categories = set(re.findall(
        r"spawn_natural_category_scratch\(&clients,\s*DimId::End,\s*MobCategory::(\w+),\s*tick\)",
        ending,
    ))
    require_runtime(tick, r"\bspawn_underground_natural\(t\)\s*;", "main tick -> underground categories")
    underground = rust_function(main, "spawn_underground_natural")
    category_loop = re.search(r"for\s+category\s+in\s+\[([^]]+)\]", underground)
    if not category_loop:
        raise AuditError("missing live underground category loop")
    underground_categories = set(re.findall(r"MobCategory::(\w+)", category_loop.group(1)))
    for pattern, label in (
        (r"feature::spawners_for\(&biome,\s*mob_category_key\(category\)\)", "underground biome table"),
        (r"biome_name_at\(x,\s*y,\s*z\)", "underground biome sampled at spawn depth"),
        (r"choose_nightly_spawner\(entries\)", "underground weighted selection"),
        (r"category\.global_cap\(", "underground mob cap"),
        (r"live_underground_spawn_attempt\(", "underground live attempt"),
    ):
        require_runtime(underground, pattern, label)
    subterranean_attempt = rust_function(main, "live_underground_spawn_attempt")
    for pattern, label in (
        (r"OverworldSpawnMob::from_id\(&entry.kind\)", "underground entity dispatch"),
        (r"natural_spawn_placement_allows\(", "underground fluid and placement guard"),
        (r"rules\.allows\(", "underground darkness and species rules"),
        (r"live_category_count\(", "underground cap enforcement"),
        (r"live_natural_spawn\(", "underground live insertion"),
    ):
        require_runtime(subterranean_attempt, pattern, label)
    overworld_categories.update(underground_categories)
    natural_dispatch = set(re.findall(
        r'"(minecraft:[a-z_]+)"\s*=>\s*Self::',
        rust_function(natural, "from_id"),
    ))

    chunkgen = rust_function(main, "live_chunkgen_run")
    require_runtime(chunkgen, r"spawner::spawn_mobs_for_chunk_generation\(", "chunkgen -> spawner")
    require_runtime(chunkgen, r"feature::spawners_for\(b,\s*category\)", "chunkgen biome entry lookup")
    require_runtime(rust_function(main, "spawn_chunkgen_creatures"), r"live_chunkgen_visit\(", "tick -> chunkgen visit")
    require_runtime(rust_function(main, "live_chunkgen_visit"), r"live_chunkgen_run\(", "chunkgen visit -> run")
    require_runtime(rust_function(sources["spawner"], "spawn_mobs_for_chunk_generation"),
                    r'spawners_for\(center_biome,\s*"creature"\)', "chunkgen creature weighted entries")
    for anchor in ("choose_creature(&entries", "entry.min_count", "entry.max_count", "world.add_creature"):
        require_runtime(sources["spawner"], re.escape(anchor), f"chunkgen {anchor}")
    require_runtime(rust_function(main, "add_creature"), r"live_natural_spawn\(", "live chunkgen insertion")
    require_runtime(rust_function(main, "live_natural_spawn"), r"entity_spec_by_name\(kind\)", "live entity registry dispatch")
    require_runtime(rust_function(main, "spawn_passive_natural"),
                    r'feature::spawners_for\(&biome,\s*"creature"\)', "passive biome spawners")

    return {
        "registry": registry,
        "nether_categories": nether_categories,
        "nether_dispatch": nether_dispatch,
        "overworld_monsters": overworld_monsters,
        "overworld_categories": overworld_categories,
        "end_categories": end_categories,
        "natural_dispatch": natural_dispatch,
        "tick_calls": list(live_calls),
    }


def audit_spawn_reachability(biomes, dim_biomes, runtime):
    """Count every biome entry and identify entries with no live weighted dispatch."""
    failures = []
    summary = Counter()
    categories = Counter()
    unreachable = defaultdict(set)
    bad_costs = []
    registry = runtime["registry"]
    # Biomes may be shared by more than one dimension. Audit each reachable use.
    dimensions = defaultdict(set)
    for dim, names in dim_biomes.items():
        for name in names:
            dimensions[name].add(dim)
    for biome, data in sorted(biomes.items()):
        spawners = data.get("spawners", {})
        costs = data.get("spawn_costs", {})
        if not isinstance(spawners, dict) or not isinstance(costs, dict):
            failures.append(f"{biome}: spawn data must be JSON objects")
            continue
        for category, entries in sorted(spawners.items()):
            if category not in SPAWNER_CATEGORIES or not isinstance(entries, list):
                failures.append(f"{biome}: unknown or malformed spawner category {category}")
                continue
            for index, entry in enumerate(entries):
                summary["spawner_entries"] += 1
                categories[category] += 1
                if not isinstance(entry, dict):
                    failures.append(f"{biome}/{category}[{index}]: malformed spawner entry")
                    continue
                entity = entry.get("type")
                if not isinstance(entity, str) or not entity.startswith("minecraft:") or entity not in registry:
                    failures.append(f"{biome}/{category}[{index}]: unmapped spawner entity {entity!r}")
                    continue
                expected_category = SPAWNER_CATEGORIES[category]
                # Vanilla Java 1.21.1's Ocelot has registry category Creature,
                # yet its exactly two biome entries are Monster list slot 8
                # in jungle and bamboo_jungle. The nightly Monster list picks
                # it and its live instance still counts under the Creature cap.
                # Every other registry/biome category mismatch is a hard error.
                vanilla_ocelot_quirk = (
                    entity == "minecraft:ocelot" and category == "monster" and index == 8
                    and biome in ("minecraft:jungle", "minecraft:bamboo_jungle")
                    and registry[entity] == "Creature"
                    and entity in runtime["overworld_monsters"]
                )
                if registry[entity] != expected_category and not vanilla_ocelot_quirk:
                    failures.append(f"{biome}/{category}[{index}]: {entity} registry category "
                                    f"{registry[entity]} differs from biome {expected_category}")
                if any(type(entry.get(key)) is not int for key in ("weight", "minCount", "maxCount")) or (
                    entry.get("weight", 0) <= 0 or entry.get("minCount", 0) <= 0
                    or entry.get("maxCount", 0) < entry.get("minCount", 0)
                    or any(entry.get(key, 0) > 2**31 - 1 for key in ("weight", "minCount", "maxCount"))
                ):
                    failures.append(f"{biome}/{category}[{index}]: invalid weight or group bounds for {entity}")
                    continue
                summary["valid_spawner_entries"] += 1
                if not dimensions[biome]:
                    summary["unreachable_spawner_uses"] += 1
                    unreachable[("unreferenced", category, entity)].add(biome)
                for dim in sorted(dimensions[biome]):
                    supported = (
                        (dim == "nether" and expected_category in runtime["nether_categories"]
                         and entity in runtime["nether_dispatch"])
                        or (dim == "overworld" and category == "creature")
                        or (dim == "overworld" and category == "monster"
                            and entity in runtime["overworld_monsters"])
                        or (dim == "overworld"
                            and expected_category in runtime["overworld_categories"]
                            and entity in runtime["natural_dispatch"])
                        or (dim == "end" and expected_category in runtime["end_categories"]
                            and entity in runtime["natural_dispatch"])
                    )
                    if supported:
                        summary["reachable_spawner_uses"] += 1
                    else:
                        summary["unreachable_spawner_uses"] += 1
                        unreachable[(dim, category, entity)].add(biome)

        for entity, cost in sorted(costs.items()):
            summary["spawn_cost_entries"] += 1
            if entity not in registry:
                bad_costs.append(f"{biome}: unmapped spawn_costs entity {entity!r}")
                continue
            if not isinstance(cost, dict) or any(
                type(cost.get(key)) not in (int, float)
                or not math.isfinite(cost[key]) or cost[key] < 0
                for key in ("energy_budget", "charge")
            ):
                bad_costs.append(f"{biome}: invalid energy_budget/charge for {entity}")
                continue
            if not dimensions[biome]:
                bad_costs.append(f"{biome}: {entity} spawn_costs biome absent from dimension trees")
            for dim in sorted(dimensions[biome]):
                paired = any(
                    entry.get("type") == entity
                    for entries in spawners.values() if isinstance(entries, list)
                    for entry in entries if isinstance(entry, dict)
                )
                if dim == "nether" and entity in runtime["nether_dispatch"] and paired:
                    summary["reachable_spawn_cost_uses"] += 1
                else:
                    bad_costs.append(f"{biome}: {entity} spawn_costs lacks {dim} live spawn dispatch")
    failures.extend(bad_costs)
    gaps = [
        {"dimension": dim, "category": category, "entity": entity, "biomes": sorted(names)}
        for (dim, category, entity), names in sorted(unreachable.items())
    ]
    failures.extend(
        f"{gap['dimension']}: unreachable weighted {gap['category']} {gap['entity']} "
        f"in {len(gap['biomes'])} biome(s): {', '.join(gap['biomes'][:3])}"
        for gap in gaps
    )
    return {
        "registry_entities": len(registry),
        "tick_calls": runtime["tick_calls"],
        "categories": dict(sorted(categories.items())),
        "spawner_entries": summary["spawner_entries"],
        "valid_spawner_entries": summary["valid_spawner_entries"],
        "reachable_spawner_uses": summary["reachable_spawner_uses"],
        "unreachable_spawner_uses": summary["unreachable_spawner_uses"],
        "unreachable_spawners": gaps,
        "spawn_cost_entries": summary["spawn_cost_entries"],
        "reachable_spawn_cost_uses": summary["reachable_spawn_cost_uses"],
        "invalid_spawn_costs": bad_costs,
    }, failures


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
    spawn_runtime = parse_spawn_runtime(root)
    spawn, f3 = audit_spawn_reachability(biomes, dim_biomes, spawn_runtime)

    return {
        "parsers": {k: len(v) for k, v in parsers.items()},
        "cond_biome_anchor": anchor,
        "density_and_surface": df_surf,
        "features_and_placements": feat_pm,
        "biomes_and_structures": bio_struct,
        "spawn_reachability": spawn,
        "failures": f1 + f2 + f3,
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
    spawn = report["spawn_reachability"]
    lines.append(
        f"[spawn_reachability] registry={spawn['registry_entities']} "
        f"spawners={spawn['spawner_entries']} valid={spawn['valid_spawner_entries']} "
        f"reachable_uses={spawn['reachable_spawner_uses']} "
        f"unreachable_uses={spawn['unreachable_spawner_uses']} "
        f"spawn_costs={spawn['spawn_cost_entries']} "
        f"reachable_cost_uses={spawn['reachable_spawn_cost_uses']}"
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
