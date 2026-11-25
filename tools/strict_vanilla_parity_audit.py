#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""strict_vanilla_parity_audit.py -- 10-gate Vanilla 1.21.1 survival parity scorecard.

Schema: "megu-strict-vanilla-parity/1".

Static live-wiring audit of the repo: every gate reads live (non-#[cfg(test)])
Rust source through the comment/string-aware lexer in
tools/vanilla_parity_audit.py, then asserts the exact wiring that closed a
previously discovered silent parity gap:

  gate1_fishing          reel/tick wires luck_of_the_sea + lure + open_water
                         (5x4x5) + built-in gameplay/fishing fallback; the old
                         2-fish `(age % 5)` stub is flagged.
  gate2_enchanting       all 42 1.21.1 enchantments (incl. density/breach/
                         wind_burst), enchantment_cost, roll_table_enchantments,
                         conflict rules, LiveMenu::Enchanting/Anvil live.
  gate3_structure_loot   desert_pyramid (chests + archaeology + TNT),
                         jungle_temple (+dispenser), simple_dungeon,
                         abandoned_mineshaft, trial_chambers (trial_spawner +
                         vault), and roll_loot_container_slots falls back to
                         built-in chest_loot tables.
  gate4_dimensions       Overworld/Nether -> End (100,48,0) obsidian platform,
                         End -> Overworld return, End gateway warp, dragon exit
                         portal, and chorus_fruit never uses Overworld-only
                         surface_y.
  gate5_spawning         LiveNaturalSpawnQueries::is_world_gen is not stubbed
                         false; Overworld/Nether/End all run natural spawning.
  gate6_disconnect       S2C player_info_remove (0x3d) emitted on disconnect.
  gate7_cramming         GR_MAX_ENTITY_CRAMMING default 24, 6.0 cramming
                         damage, ENTITY_HURT_INVULNERABLE_TICKS = 10 live.
  gate8_survival         brewing, villager POI/trading/restock, raids, beds &
                         respawn anchors, shields, bows/crossbows, tridents,
                         elytra/fireworks, redstone/pistons/hoppers/comparators,
                         weather/lightning — all live-wired.
  gate9_builtin_bypass   built-in chest_loot tables must be reachable from
                         roll_loot_container_slots and the fishing handler —
                         never silently bypassed.
  gate10_dimension_leak  dimension-aware handlers (enter_portal, chorus_fruit,
                         end_gateway_warp, live_mob_tick_dimension) never call
                         Overworld-only surface_y( / block_at( unguarded.

Pure Python 3 stdlib. Run: python3 tools/strict_vanilla_parity_audit.py
[--root DIR] [--json] [--strict]. Default prints a per-gate scorecard and
exits 0; --strict exits 1 when any gate FAILs; exit 2 is a fail-closed
input/parse error.
"""

import argparse
import json
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(Path(__file__).resolve().parent))

import vanilla_parity_audit as vpa  # noqa: E402

SCHEMA = "megu-strict-vanilla-parity/1"

FN_HEAD = re.compile(
    r"(?m)^[ \t]*(?:pub(?:\s*\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?"
    r"(?:const\s+)?fn\s+\w+"
)

BUILTIN_NEEDLES = (
    "chest_loot",
    "vanilla_loot_roll",
    "unpack_loot_table_json",
    "evaluate_loot_table",
    "VANILLA_LOOT_GENERATORS",
)

V1211_ENCHANTMENTS = (
    "aqua_affinity", "bane_of_arthropods", "binding_curse", "blast_protection",
    "breach", "channeling", "density", "depth_strider", "efficiency",
    "feather_falling", "fire_aspect", "fire_protection", "flame", "fortune",
    "frost_walker", "impaling", "infinity", "knockback", "looting", "loyalty",
    "luck_of_the_sea", "lure", "mending", "multishot", "piercing", "power",
    "projectile_protection", "protection", "punch", "quick_charge",
    "respiration", "riptide", "sharpness", "silk_touch", "smite", "soul_speed",
    "sweeping_edge", "swift_sneak", "thorns", "unbreaking", "vanishing_curse",
    "wind_burst",
)


class AuditError(Exception):
    """Fail-closed input error -> exit 2."""


# ---------------------------------------------------------------------------
# text loading + small structural helpers
# ---------------------------------------------------------------------------

# Every src/*.rs file a gate reads. Keeping the set explicit (instead of
# globbing all of src/) keeps a full audit under a second and makes the
# inputs a gate sees deterministic.
AUDIT_FILES = (
    "chest_loot.rs",
    "combat_and_health.rs",
    "entity_push.rs",
    "feature.rs",
    "generated_content.rs",
    "main.rs",
    "menus_and_items.rs",
    "mob_behaviors.rs",
    "natural_spawner.rs",
    "play_protocol.rs",
    "player_mechanisms.rs",
    "redstone.rs",
    "redstone_world.rs",
    "single_piece_structures.rs",
    "structure_template.rs",
    "villager_raid.rs",
    "world_mechanisms.rs",
)


def load_texts(root):
    """rel -> (live, test_text, skel) for each AUDIT_FILES member that exists.

    `live` has comments and every #[cfg(test)] item blanked; `skel` is the
    same-length code skeleton used for brace matching. Missing files are
    skipped: every gate treats an absent file as "no evidence", not a crash.
    """
    src = root / "src"
    if not src.is_dir():
        raise AuditError(f"no src/ directory under root {root}")
    texts = {}
    for name in AUDIT_FILES:
        path = src / name
        if not path.is_file():
            continue
        try:
            raw = path.read_text(encoding="utf-8", errors="replace")
        except OSError as exc:
            raise AuditError(f"unreadable {path}: {exc}") from exc
        live, tests = vpa.mask_test_regions(
            vpa.live_text(raw), vpa.code_skeleton(raw)
        )
        texts[f"src/{path.name}"] = (live, tests, vpa.code_skeleton(live))
    return texts


def texts_for_files(files):
    """Synthetic-tree loader for tests: {rel: source} -> texts dict."""
    out = {}
    for rel, source in files.items():
        live, tests = vpa.mask_test_regions(
            vpa.live_text(source), vpa.code_skeleton(source)
        )
        out[rel] = (live, tests, vpa.code_skeleton(live))
    return out


def _live(texts, rel):
    return texts.get(rel, ("", "", ""))[0]


def _skel(texts, rel):
    return texts.get(rel, ("", "", ""))[2]


def fn_bodies(skel, name):
    """All (start, end) spans of `fn name(...) { ... }`, impl methods included."""
    pat = re.compile(
        r"(?m)^[ \t]*(?:pub(?:\s*\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?"
        r"(?:const\s+)?fn\s+" + re.escape(name) + r"\b"
    )
    out = []
    for m in pat.finditer(skel):
        brace = skel.find("{", m.end())
        if brace < 0 or ";" in skel[m.end():brace]:
            continue
        close = vpa.match_bracket(skel, brace)
        if close >= 0:
            out.append((m.start(), close + 1))
    return out


def enclosing_fn(skel, pos):
    """Smallest top-level/impl fn span containing pos, or None."""
    best = None
    for m in FN_HEAD.finditer(skel):
        brace = skel.find("{", m.end())
        if brace < 0 or ";" in skel[m.end():brace]:
            continue
        close = vpa.match_bracket(skel, brace)
        if close < 0:
            continue
        if m.start() <= pos < close and (best is None or m.start() > best[0]):
            best = (m.start(), close + 1)
    return best


def call_sites(live, name):
    """Positions of `name(` not immediately preceded by `fn ` (a definition)."""
    out = []
    for m in re.finditer(r"\b" + re.escape(name) + r"\s*\(", live):
        head = live[max(0, m.start() - 12):m.start()]
        if re.search(r"\bfn\s*$", head):
            continue
        out.append(m.start())
    return out


def has_live_call(texts, name):
    return any(call_sites(live, name) for live, _, _ in texts.values())


def live_wired(texts, name):
    """`name` is wired: a `name(` call site, or referenced (e.g. passed as a
    function value to `run("stage", name)`) somewhere besides its definition."""
    if has_live_call(texts, name):
        return True
    refs = sum(len(re.findall(r"\b" + re.escape(name) + r"\b", live))
               for live, _, _ in texts.values())
    return refs >= 2


def _line_of(text, pos):
    return text.count("\n", 0, pos) + 1


def dim_guarded(body, pos):
    """Is the Overworld-only lookup at `pos` guarded by `dim == DimId::Overworld`?

    Statement-level heuristic: scans back to the previous ';' and inspects the
    guard expression in front of the call. Handles `if cond &&` chains,
    ternary/if-else arms and `match` patterns:

      guarded   `if dim == DimId::Overworld && f(block_at(x, y, z))`
      guarded   `let y = if dim == DimId::Overworld { surface_y(x, z) } else { a }`
      guarded   `DimId::Overworld => surface_y(x, z)`
      unguarded `let y = surface_y(x, z);`
      unguarded `if dim == DimId::Overworld { a } else { surface_y(x, z) }`
      unguarded `DimId::Overworld | DimId::Nether => surface_y(x, z)`
    """
    start = body.rfind(";", 0, pos) + 1
    w = body[start:pos]
    o = w.rfind("DimId::Overworld")
    if o < 0:
        return False
    pre = w[max(0, o - 5):o]
    tail = w[o + len("DimId::Overworld"):]
    if "!=" in pre:
        # then-branch is non-Overworld: the call is safe only in else/wildcard.
        return bool(re.search(r"\belse\b", tail) or "_ =>" in tail)
    if re.search(r"\belse\b", tail):
        return False
    arrows = [mm.start() for mm in re.finditer(r"=>", tail)]
    if arrows:
        first = tail[:arrows[0]]
        if "," not in first:
            # This `=>` belongs to the arm whose pattern holds the guard token;
            # a `|`/`_` alternative still lets other dims through.
            if "|" in first or re.search(r"\b_\b", first):
                return False
            arrows = arrows[1:]
        if arrows:
            return False
    return True


_BARE_LOOKUP = re.compile(r"\b(?:surface_y|block_at)\s*\(")


def dimension_leaks(live, skel, body_span, base_pos):
    """Unguarded surface_y(/block_at( call sites inside body_span."""
    body = live[body_span[0]:body_span[1]]
    hits = []
    for m in _BARE_LOOKUP.finditer(body):
        if not dim_guarded(body, m.start()):
            abs_pos = base_pos + body_span[0] + m.start()
            hits.append((_line_of(live, abs_pos), m.group(0).rstrip(" (")))
    return hits


def chorus_fruit_arms(live, skel):
    """Body spans of `if`-arms whose condition references chorus_fruit."""
    out = []
    for m in re.finditer(r"chorus_fruit", live):
        st = max(
            live.rfind(";", 0, m.start()),
            live.rfind("{", 0, m.start()),
            live.rfind("}", 0, m.start()),
        ) + 1
        if not re.search(r"\bif\b", live[st:m.start()]):
            continue
        brace = skel.find("{", m.end())
        if brace < 0:
            continue
        seg = live[m.end():brace]
        if any(c in seg for c in ";{}") or "=>" in seg:
            continue
        close = vpa.match_bracket(skel, brace)
        if close >= 0:
            out.append((brace, close + 1))
    return out


def _gate(gid, title):
    return {"id": gid, "title": title, "status": "PASS",
            "findings": [], "warnings": [], "evidence": []}


def _finish(g):
    if g["findings"]:
        g["status"] = "FAIL"
    elif g["warnings"]:
        g["status"] = "WARN"
    return g


# ---------------------------------------------------------------------------
# Gates
# ---------------------------------------------------------------------------

def gate1_fishing(texts):
    g = _gate("gate1_fishing", "Fishing parity (luck/lure/open-water/loot)")
    live = _live(texts, "src/menus_and_items.rs")
    skel = _skel(texts, "src/menus_and_items.rs")
    idx = live.find('.get("minecraft:gameplay/fishing")')
    if idx < 0:
        idx = live.find("gameplay/fishing")
    span = enclosing_fn(skel, idx) if idx >= 0 else None
    if span is None:
        g["findings"].append(
            "src/menus_and_items.rs: no fishing reel-in handler rolls "
            "minecraft:gameplay/fishing")
        return _finish(g)
    body = live[span[0]:span[1]]
    g["evidence"].append(
        f"reel handler src/menus_and_items.rs:{_line_of(live, span[0])}")
    has_builtin = any(n in body for n in BUILTIN_NEEDLES)
    stub = bool(re.search(r"%\s*5\b|is_multiple_of\(5\)", body))
    if not re.search(r"fishing_luck|luck_of_the_sea", body):
        g["findings"].append(
            "reel-in ignores Luck of the Sea: fishing_luck weights are never "
            "consulted where the catch is rolled")
    if not re.search(r"open_water|in_open_water", body):
        g["findings"].append(
            "reel-in has no open-water (5x4x5) check: treasure eligibility "
            "cannot follow vanilla")
    if not has_builtin:
        g["findings"].append(
            "catch falls back to a hardcoded fish pick instead of the "
            "built-in minecraft:gameplay/fishing table in src/chest_loot.rs")
        if stub:
            g["findings"].append(
                "2-fish `(age % 5)` stub is the only non-datapack fallback")
    elif stub:
        g["warnings"].append(
            "leftover `(age % 5)` fish picker still sits in the reel path "
            "after the loot-table fallback")
    if not live_wired(texts, "apply_fishing_enchants") \
            and not live_wired(texts, "lure_wait_ticks"):
        g["findings"].append(
            "Lure is never applied: no live call to apply_fishing_enchants "
            "or lure_wait_ticks")
    return _finish(g)


def gate2_enchanting(texts):
    g = _gate("gate2_enchanting", "Enchanting table & anvil parity (42 enchantments)")
    live = _live(texts, "src/menus_and_items.rs")
    skel = _skel(texts, "src/menus_and_items.rs")
    bodies = fn_bodies(skel, "enchant_id")
    if not bodies:
        g["findings"].append("no enchant_id name->id map in menus_and_items.rs")
    else:
        table = live[bodies[0][0]:bodies[0][1]]
        missing = [n for n in V1211_ENCHANTMENTS if f'"{n}"' not in table]
        if missing:
            g["findings"].append(
                "enchant_id misses 1.21.1 enchantments: " + ", ".join(missing))
    for fn in ("enchantment_cost", "roll_table_enchantments"):
        if not live_wired(texts, fn):
            g["findings"].append(f"{fn} has no live call site (dead roller)")
    if not live_wired(texts, "enchant_conflicts") \
            and not live_wired(texts, "player_enchantments_conflict"):
        g["findings"].append("no live enchantment conflict rules")
    for variant in ("LiveMenu::Enchanting", "LiveMenu::Anvil"):
        if variant not in live:
            g["findings"].append(f"{variant} is not wired in live menus")
    return _finish(g)


def gate3_structure_loot(texts):
    g = _gate("gate3_structure_loot",
              "Structure chests, dispensers & traps parity")
    # Emission must come from a consumer: chest_loot.rs only *registers* the
    # table names, so it never proves a structure emits the container.
    all_live = "".join(live for rel, (live, _, _) in texts.items()
                       if rel != "src/chest_loot.rs")
    for table in (
        "minecraft:chests/desert_pyramid",
        "minecraft:archaeology/desert_pyramid",
        "minecraft:chests/jungle_temple",
        "minecraft:chests/jungle_temple_dispenser",
        "minecraft:chests/simple_dungeon",
        "minecraft:chests/abandoned_mineshaft",
        "minecraft:chests/trial_chambers",
    ):
        if table not in all_live:
            g["findings"].append(f"no live structure emits loot table {table}")
    structures = _live(texts, "src/single_piece_structures.rs")
    if "desert_pyramid" in structures and "tnt" not in structures:
        g["findings"].append(
            "desert_pyramid has no TNT trap in structure generation")
    if not live_wired(texts, "tick_live_trial_spawner"):
        g["findings"].append("tick_live_trial_spawner has no live call site")
    if "LiveBlockEntity::Vault" not in all_live and "Vault::from_nbt" not in all_live:
        g["findings"].append("no live vault block-entity handling")
    live = _live(texts, "src/menus_and_items.rs")
    skel = _skel(texts, "src/menus_and_items.rs")
    bodies = fn_bodies(skel, "roll_loot_container_slots")
    if not bodies:
        g["findings"].append("roll_loot_container_slots not found")
    else:
        body = live[bodies[0][0]:bodies[0][1]]
        if not any(n in body for n in BUILTIN_NEEDLES):
            builtins = _builtin_table_ids(texts)
            chests = sorted(t for t in builtins
                            if t.startswith(("minecraft:chests/",
                                             "minecraft:archaeology/")))
            g["findings"].append(
                "roll_loot_container_slots only routes "
                "abandoned_mineshaft to a built-in table; the other "
                f"{len(chests)} built-in container tables in chest_loot.rs "
                "are unreachable without a datapack override "
                f"(e.g. {', '.join(chests[:4])})")
    return _finish(g)


def gate4_dimensions(texts):
    g = _gate("gate4_dimensions",
              "End & Nether dimension/portal/chorus parity")
    pp = _live(texts, "src/play_protocol.rs")
    if not re.search(r"100\s*\+\s*\w+\s*,\s*48", pp):
        g["findings"].append(
            "no (100, 48, 0) obsidian platform materialisation for End entry")
    if "obsidian" not in pp:
        g["findings"].append("no obsidian platform block placement found")
    skel = _skel(texts, "src/play_protocol.rs")
    bodies = fn_bodies(skel, "enter_portal")
    if not bodies:
        g["findings"].append("enter_portal not found")
    else:
        body = pp[bodies[0][0]:bodies[0][1]]
        if "DimId::End" not in body or "DimId::Overworld" not in body:
            g["findings"].append(
                "enter_portal does not route both End entry and End->Overworld "
                "return")
        if not re.search(r"respawn|spawn", body):
            g["findings"].append(
                "End->Overworld return does not land on the player "
                "spawn/respawn point")
    for fn in ("end_gateway_warp", "tick_end_gateway_entries"):
        if not live_wired(texts, fn):
            g["findings"].append(f"{fn} has no live call site")
    if not live_wired(texts, "place_bedrock_exit_portal") \
            and "place_bedrock_exit_portal" not in _live(texts, "src/main.rs"):
        g["findings"].append("dragon exit portal placement is not live-wired")
    # chorus fruit dimension awareness
    live = _live(texts, "src/menus_and_items.rs")
    mskel = _skel(texts, "src/menus_and_items.rs")
    arms = chorus_fruit_arms(live, mskel)
    if not arms:
        g["warnings"].append("no chorus_fruit consumption handler found")
    for a, b in arms:
        body = live[a:b]
        leaks = dimension_leaks(live, mskel, (a, b), 0)
        for line, fn in leaks:
            g["findings"].append(
                f"src/menus_and_items.rs:{line}: chorus_fruit teleport uses "
                f"Overworld-only `{fn}(` — leaks the Overworld surface into "
                "Nether/End teleports")
        if not re.search(r"\b\w+_dim\s*\(", body):
            g["findings"].append(
                "chorus_fruit arm performs no dimension-aware block lookup "
                "(*_dim); landing search cannot see the current dimension")
    return _finish(g)


def gate5_spawning(texts):
    g = _gate("gate5_spawning", "Spawning & slime-chunk live-adapter parity")
    live = _live(texts, "src/natural_spawner.rs")
    skel = _skel(texts, "src/natural_spawner.rs")
    impl = re.search(
        r"impl\s+OverworldSpawnQueries\s+for\s+LiveNaturalSpawnQueries", skel)
    if not impl:
        g["findings"].append(
            "LiveNaturalSpawnQueries does not implement OverworldSpawnQueries")
    else:
        bodies = [s for s in fn_bodies(skel, "is_world_gen")
                  if s[0] > impl.start()]
        if not bodies:
            g["findings"].append(
                "LiveNaturalSpawnQueries::is_world_gen is missing")
        else:
            body = live[bodies[0][0]:bodies[0][1]]
            inner = body[body.find("{") + 1:body.rfind("}")].strip()
            if inner == "false":
                g["findings"].append(
                    "LiveNaturalSpawnQueries::is_world_gen is stubbed to "
                    "false: slime chunks never evaluate in live spawning")
    for label, names in (
        ("Overworld", ("spawn_nightly_hostile", "spawn_passive_natural",
                       "spawn_aquatic_natural")),
        ("Nether", ("spawn_nether_natural",)),
        ("End", ("spawn_end_natural",)),
    ):
        if not any(live_wired(texts, n) for n in names):
            g["findings"].append(
                f"no live natural-spawn pass for {label}: "
                f"none of {names} is called")
    return _finish(g)


def gate6_disconnect(texts):
    g = _gate("gate6_disconnect", "Multiplayer disconnect & tab-list parity")
    live = _live(texts, "src/play_protocol.rs")
    skel = _skel(texts, "src/play_protocol.rs")
    idx = live.find("broadcast_seen_destroy(me_cleanup")
    if idx < 0:
        idx = live.find("me_cleanup.eid")
    span = enclosing_fn(skel, idx) if idx >= 0 else None
    if span is None:
        g["findings"].append(
            "no disconnect cleanup path found (me_cleanup destroy broadcast)")
        return _finish(g)
    body = live[span[0]:span[1]]
    if not re.search(r"0x3d|0x3D|player_info_remove|PLAYER_INFO_REMOVE", body):
        g["findings"].append(
            f"disconnect cleanup src/play_protocol.rs:"
            f"{_line_of(live, span[0])} never emits S2C player_info_remove "
            "(0x3d): stale tab-list ghosts for remaining players")
    return _finish(g)


def gate7_cramming(texts):
    g = _gate("gate7_cramming", "Entity cramming & combat i-frames parity")
    ep = _live(texts, "src/entity_push.rs")
    all_live = "".join(live for live, _, _ in texts.values())
    if not re.search(r"ENTITY_CRAMMING_DEFAULT\s*:\s*\w+\s*=\s*24", ep):
        g["findings"].append("maxEntityCramming default is not 24")
    if not re.search(
            r"GR_MAX_ENTITY_CRAMMING\s*:\s*Atomic\w+\s*=\s*Atomic\w+::new"
            r"\(ENTITY_CRAMMING_DEFAULT\)", all_live):
        g["findings"].append(
            "GR_MAX_ENTITY_CRAMMING gamerule is not backed by the cramming "
            "default atomic")
    if not re.search(r"GR_MAX_ENTITY_CRAMMING\.load", all_live):
        g["findings"].append("GR_MAX_ENTITY_CRAMMING is never read live")
    if not re.search(r"ENTITY_CRAMMING_DAMAGE\s*:\s*f32\s*=\s*6\.0", ep) \
            and not re.search(r"cramming\(\),\s*6\.0|6\.0f?32?\b.*cramm", ep):
        g["findings"].append("cramming damage is not the vanilla 6.0")
    if not re.search(
            r"ENTITY_HURT_INVULNERABLE_TICKS\s*:\s*\w+\s*=\s*10\b", ep):
        g["findings"].append(
            "ENTITY_HURT_INVULNERABLE_TICKS is not the vanilla 10 ticks")
    elif len(re.findall(r"\bENTITY_HURT_INVULNERABLE_TICKS\b", ep)) < 2:
        g["findings"].append(
            "ENTITY_HURT_INVULNERABLE_TICKS is defined but never applied")
    return _finish(g)


_GATE8 = (
    ("brewing", {"subs": ("LiveMenu::Brewing",)}),
    ("villager trading & restock",
     {"subs": ("LiveMenu::Merchant", "restock")}),
    ("raids", {"calls": ("live_raid_step",)}),
    ("beds & respawn anchors",
     {"calls": ("respawn_anchor_use", "live_spawn_position_ok")}),
    ("shields",
     {"calls": ("damage_shield", "evaluate_shield_block", "shield_raised")}),
    ("bows & crossbows",
     {"calls": ("crossbow_fire_loaded", "w1_arrow_tick")}),
    ("tridents",
     {"calls": ("evaluate_trident_special", "channeling_strikes_lightning",
                "trident_seen")}),
    ("elytra & fireworks",
     {"calls": ("elytra_wall_collision_tick", "fireworks_step",
                "firework_explode")}),
    ("redstone/pistons/hoppers/comparators",
     {"calls": ("live_redstone_tick",),
      "subs": ("piston", "hopper", "comparator")}),
    ("weather & lightning",
     {"subs": ("GR_WEATHER_CYCLE", "thunder", "lightning")}),
)


def gate8_survival(texts):
    g = _gate("gate8_survival", "Survival core mechanics matrix")
    all_live = "".join(live for live, _, _ in texts.values())
    for label, spec in _GATE8:
        ok = True
        if "calls" in spec:
            ok = any(live_wired(texts, n) for n in spec["calls"])
        if "subs" in spec:
            ok = ok and all(s in all_live for s in spec["subs"])
        if not ok:
            g["findings"].append(f"{label}: no live wiring found")
    return _finish(g)


_BUILTIN_PAIR = re.compile(r'\(\s*"(minecraft:[^"]+)"\s*,')


def _builtin_table_ids(texts):
    live = _live(texts, "src/chest_loot.rs")
    skel = _skel(texts, "src/chest_loot.rs")
    region = vpa.region_after(skel, "VANILLA_LOOT_GENERATORS")
    if region is None:
        return set()
    return set(_BUILTIN_PAIR.findall(live[region[0]:region[1]]))


def gate9_builtin_bypass(texts):
    g = _gate("gate9_builtin_bypass", "Built-in table vs live bypass detector")
    builtins = _builtin_table_ids(texts)
    if not builtins:
        g["findings"].append(
            "VANILLA_LOOT_GENERATORS registry missing/empty in chest_loot.rs")
        return _finish(g)
    g["evidence"].append(f"{len(builtins)} built-in vanilla loot tables")
    live = _live(texts, "src/menus_and_items.rs")
    skel = _skel(texts, "src/menus_and_items.rs")
    bodies = fn_bodies(skel, "roll_loot_container_slots")
    roller_ok = bool(bodies) and any(
        n in live[bodies[0][0]:bodies[0][1]] for n in BUILTIN_NEEDLES)
    if not bodies:
        g["findings"].append("roll_loot_container_slots not found")
    elif not roller_ok:
        chests = sorted(t for t in builtins
                        if t.startswith(("minecraft:chests/",
                                         "minecraft:archaeology/")))
        g["findings"].append(
            "roll_loot_container_slots cannot reach the built-in tables: "
            f"{len(chests)} chest/archaeology tables in chest_loot.rs are "
            "silently bypassed unless a datapack shadows them "
            f"({', '.join(chests[:5])}...)")
    idx = live.find("gameplay/fishing")
    span = enclosing_fn(skel, idx) if idx >= 0 else None
    if span is not None:
        body = live[span[0]:span[1]]
        if "gameplay/fishing" in builtins \
                or "minecraft:gameplay/fishing" in builtins:
            if not any(n in body for n in BUILTIN_NEEDLES):
                g["findings"].append(
                    "fishing catch bypasses the built-in gameplay/fishing "
                    "tables (datapack lookup or hardcoded pick only)")
    return _finish(g)


_DIM_HANDLERS = (
    ("src/play_protocol.rs", "enter_portal"),
    ("src/play_protocol.rs", "end_gateway_warp"),
    ("src/play_protocol.rs", "end_gateway_entry_check"),
    ("src/main.rs", "live_mob_tick_dimension"),
)


def gate10_dimension_leak(texts):
    g = _gate("gate10_dimension_leak", "Dimension-leak static detector")
    for rel, name in _DIM_HANDLERS:
        live = _live(texts, rel)
        skel = _skel(texts, rel)
        for span in fn_bodies(skel, name):
            for line, fn in dimension_leaks(live, skel, span, 0):
                g["findings"].append(
                    f"{rel}:{line}: `{name}` calls Overworld-only `{fn}(` "
                    "without a DimId::Overworld guard")
    live = _live(texts, "src/menus_and_items.rs")
    mskel = _skel(texts, "src/menus_and_items.rs")
    for a, b in chorus_fruit_arms(live, mskel):
        for line, fn in dimension_leaks(live, mskel, (a, b), 0):
            g["findings"].append(
                f"src/menus_and_items.rs:{line}: chorus_fruit handler calls "
                f"Overworld-only `{fn}(` without a DimId::Overworld guard")
    return _finish(g)


GATES = (
    gate1_fishing,
    gate2_enchanting,
    gate3_structure_loot,
    gate4_dimensions,
    gate5_spawning,
    gate6_disconnect,
    gate7_cramming,
    gate8_survival,
    gate9_builtin_bypass,
    gate10_dimension_leak,
)


def run_gates(texts):
    """Run all 10 gates over a texts dict -> report."""
    gates = [fn(texts) for fn in GATES]
    summary = {
        "pass": sum(1 for g in gates if g["status"] == "PASS"),
        "warn": sum(1 for g in gates if g["status"] == "WARN"),
        "fail": sum(1 for g in gates if g["status"] == "FAIL"),
        "total": len(gates),
    }
    return {"schema": SCHEMA, "gates": gates, "summary": summary}


def audit_repo(root):
    return run_gates(load_texts(root))


# ---------------------------------------------------------------------------
# reporting
# ---------------------------------------------------------------------------

_ICON = {"PASS": "PASS", "WARN": "WARN", "FAIL": "FAIL"}


def render_text(report):
    out = ["strict vanilla parity audit (1.21.1 survival scorecard)", ""]
    for g in report["gates"]:
        out.append(f"[{_ICON[g['status']]:4}] {g['id']}: {g['title']}")
        for ev in g["evidence"]:
            out.append(f"       evidence: {ev}")
        for w in g["warnings"]:
            out.append(f"       warn: {w}")
        for f in g["findings"]:
            out.append(f"       gap: {f}")
    s = report["summary"]
    out.append("")
    out.append(f"score: {s['pass']}/{s['total']} PASS"
               f" ({s['warn']} warn, {s['fail']} fail)")
    return "\n".join(out)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=REPO_ROOT,
                        help="repo root containing src/ (default: this repo)")
    parser.add_argument("--json", action="store_true",
                        help="emit the schema'd JSON report instead of text")
    parser.add_argument("--strict", action="store_true",
                        help="exit 1 when any gate FAILs (CI wiring)")
    options = parser.parse_args(argv)
    root = options.root.resolve()
    try:
        report = audit_repo(root)
    except AuditError as exc:
        print(f"strict vanilla parity audit: ERROR {exc}", file=sys.stderr)
        return 2
    report["root"] = str(root)
    if options.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(render_text(report))
    if options.strict and report["summary"]["fail"]:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
