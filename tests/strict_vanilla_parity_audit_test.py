#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Tests for tools/strict_vanilla_parity_audit.py (megu-strict-vanilla-parity/1).

Every one of the 11 gates is driven twice over synthetic Rust fixtures:
once against the buggy pattern the gate was built to catch (the fishing
`(age % 5)` stub, `surface_y` inside the chorus_fruit arm, a missing 0x3d
disconnect broadcast, a datapack-only roll_loot_container_slots, an
is_world_gen -> false stub, ...) and once against the fixed wiring. A gate
that cannot fail, or that still fails on fixed code, is decoration — these
fixtures keep all eleven honest.
"""

import contextlib
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TOOL = REPO / "tools" / "strict_vanilla_parity_audit.py"

spec = importlib.util.spec_from_file_location("strict_vanilla_parity_audit", TOOL)
svpa = importlib.util.module_from_spec(spec)
sys.modules["strict_vanilla_parity_audit"] = svpa
spec.loader.exec_module(svpa)


def run(files):
    """{rel: source} -> gate id -> gate result."""
    report = svpa.run_gates(svpa.texts_for_files(files))
    return {g["id"]: g for g in report["gates"]}


def gate(files, gid):
    return run(files)[gid]


# ---------------------------------------------------------------------------
# Shared fixture fragments
# ---------------------------------------------------------------------------

CHORUS_STUB = """
fn air_use_item(me: &Arc<Client>, held_slot: i16, seq: i64) {
    if it == name_to_item("chorus_fruit").unwrap_or(-9) {
        for _ in 0..16 {
            let nx = px + ((world_rng::next_u64() % 1600) as f64 / 100.0) - 8.0;
            let nz = pz + ((world_rng::next_u64() % 1600) as f64 / 100.0) - 8.0;
            let ny = surface_y(nx.round() as i32, nz.round() as i32);
            if (ny - py).abs() < 8.0 {
                teleport_in_dimension(me, &mut sock, (nx, ny, nz, yaw, pitch));
                break;
            }
        }
    }
}
"""

CHORUS_FIXED = """
fn air_use_item(me: &Arc<Client>, held_slot: i16, seq: i64, dim: DimId) {
    if it == name_to_item("chorus_fruit").unwrap_or(-9) {
        for _ in 0..16 {
            let nx = px + ((world_rng::next_u64() % 1600) as f64 / 100.0) - 8.0;
            let nz = pz + ((world_rng::next_u64() % 1600) as f64 / 100.0) - 8.0;
            let ny = surface_y_dim(dim, nx.round() as i32, nz.round() as i32);
            if block_at_dim(dim, nx as i32, ny as i32, nz as i32) != 0 {
                teleport_in_dimension(me, &mut sock, (nx, ny, nz, yaw, pitch));
                break;
            }
        }
    }
}
"""

FISHING_ENCHANTS = """
fn apply_fishing_enchants(c: &Client) {
    let shift = lure_wait_ticks(wait, lure as i32);
    b.age += shift;
}
fn lure_wait_ticks(base: i32, level: i32) -> i32 { (base - 100 * level).max(20) }
fn movement_tick(c: &Client) { apply_fishing_enchants(c); }
"""

BUILTIN_REGISTRY = """
type VanillaLootGenerator = (&'static str, fn(i64) -> Vec<LootEntry>);
pub(crate) static VANILLA_LOOT_GENERATORS: &[VanillaLootGenerator] = &[
    ("minecraft:chests/desert_pyramid", desert_pyramid),
    ("minecraft:chests/jungle_temple", jungle_temple),
    ("minecraft:chests/jungle_temple_dispenser", jungle_temple_dispenser),
    ("minecraft:chests/simple_dungeon", simple_dungeon),
    ("minecraft:archaeology/desert_pyramid", desert_pyramid_brush),
    ("minecraft:gameplay/fishing", fishing),
];
"""


# ---------------------------------------------------------------------------
# Gate 1 — fishing
# ---------------------------------------------------------------------------

class Gate1FishingTest(unittest.TestCase):
    STUB_REEL = """
fn air_use_item(me: &Arc<Client>, held_slot: i16, seq: i64) {
    let mut fb = bobbers().lock().unwrap();
    if let Some(ix) = fb.iter().position(|b| b.owner == me.eid) {
        let b = fb.remove(ix);
        if b.bit {
            let table: Option<Vec<LootPool>> = loot_tables()
                .lock().unwrap()
                .get("minecraft:gameplay/fishing")
                .map(|p| p.clone());
            let (fish, fcnt, fcomps) = table
                .and_then(|p| roll_loot(&p).into_iter().next())
                .unwrap_or((
                    if (b.age as u64).is_multiple_of(5) { 936 } else { 935 },
                    1,
                    Vec::new(),
                ));
            inv.insert(9, (fish, fcnt, fcomps));
        }
    }
}
"""
    FIXED_REEL = """
fn air_use_item(me: &Arc<Client>, held_slot: i16, seq: i64) {
    let mut fb = bobbers().lock().unwrap();
    if let Some(ix) = fb.iter().position(|b| b.owner == me.eid) {
        let b = fb.remove(ix);
        if b.bit {
            let weights = fishing_luck().lock().unwrap()
                .get(&me.eid).copied().unwrap_or(FISHING_BASE_WEIGHTS);
            let open_water = fishing_open_water_5x4x5(&b, dim);
            let table: Option<Vec<LootPool>> = loot_tables()
                .lock().unwrap()
                .get("minecraft:gameplay/fishing")
                .map(|p| p.clone());
            let (fish, fcnt, fcomps) = table
                .and_then(|p| roll_loot(&p).into_iter().next())
                .or_else(|| crate::chest_loot::vanilla_loot_roll(
                    "minecraft:gameplay/fishing", seed).into_iter().next())
                .map(|item| loot_json_stack(&item, weights, open_water))
                .unwrap_or((935, 1, Vec::new()));
            inv.insert(9, (fish, fcnt, fcomps));
        }
    }
}
"""

    def test_flags_two_fish_stub_and_missing_enchants(self):
        g = gate({
            "src/menus_and_items.rs": self.STUB_REEL,
            "src/player_mechanisms.rs": FISHING_ENCHANTS,
        }, "gate1_fishing")
        self.assertEqual(g["status"], "FAIL")
        text = "\n".join(g["findings"])
        self.assertIn("Luck of the Sea", text)
        self.assertIn("open-water", text)
        self.assertIn("gameplay/fishing", text)
        self.assertIn("age % 5", text)

    def test_passes_on_weighted_builtin_reel(self):
        g = gate({
            "src/menus_and_items.rs": self.FIXED_REEL,
            "src/player_mechanisms.rs": FISHING_ENCHANTS,
        }, "gate1_fishing")
        self.assertEqual(g["status"], "PASS", g["findings"])

    def test_flags_missing_reel_handler(self):
        g = gate({"src/menus_and_items.rs": "fn other() {}\n"},
                  "gate1_fishing")
        self.assertEqual(g["status"], "FAIL")
        self.assertIn("fishing", "\n".join(g["findings"]).lower())

    def test_flags_lure_never_applied(self):
        g = gate({
            "src/menus_and_items.rs": self.FIXED_REEL,
            "src/player_mechanisms.rs": "fn lure_wait_ticks(b: i32, l: i32) -> i32 { b }\n",
        }, "gate1_fishing")
        self.assertEqual(g["status"], "FAIL")
        self.assertIn("Lure", "\n".join(g["findings"]))


# ---------------------------------------------------------------------------
# Gate 2 — enchanting
# ---------------------------------------------------------------------------

_ENCH_NAMES = [
    "aqua_affinity", "bane_of_arthropods", "binding_curse",
    "blast_protection", "breach", "channeling", "density", "depth_strider",
    "efficiency", "feather_falling", "fire_aspect", "fire_protection",
    "flame", "fortune", "frost_walker", "impaling", "infinity", "knockback",
    "looting", "loyalty", "luck_of_the_sea", "lure", "mending", "multishot",
    "piercing", "power", "projectile_protection", "protection", "punch",
    "quick_charge", "respiration", "riptide", "sharpness", "silk_touch",
    "smite", "soul_speed", "sweeping_edge", "swift_sneak", "thorns",
    "unbreaking", "vanishing_curse", "wind_burst",
]


def _enchant_id_fn(names):
    arms = "\n".join(f'        "{n}" => {i},' for i, n in enumerate(names))
    return ("fn enchant_id(name: &str) -> Option<i64> {\n"
            "    Some(match name {\n" + arms + "\n        _ => return None,\n"
            "    })\n}\n")


class Gate2EnchantingTest(unittest.TestCase):
    WIRING = """
fn open_menu(block: &str) -> LiveMenu {
    match block {
        "minecraft:enchanting_table" => LiveMenu::Enchanting(EnchantmentMenu::default()),
        "minecraft:anvil" => LiveMenu::Anvil(AnvilMenu::default()),
        _ => LiveMenu::None,
    }
}
fn enchant_conflicts(a: i64, b: i64) -> bool { a == b }
fn offer_roll(rng: &mut Lcg, item: &str, level: i32) -> Vec<(i64, i64)> {
    let cost = enchantment_cost(rng, 0, 15, 10);
    roll_table_enchantments(rng, item, 10, cost)
}
fn enchantment_cost(r: &mut Lcg, s: i32, sh: i32, e: i32) -> i32 { 1 }
fn roll_table_enchantments(r: &mut Lcg, i: &str, e: i32, l: i32) -> Vec<(i64, i64)> {
    if enchant_conflicts(1, 2) { vec![] } else { vec![] }
}
"""

    def test_flags_missing_mace_enchantments_and_dead_roller(self):
        buggy_names = [n for n in _ENCH_NAMES
                       if n not in ("density", "breach", "wind_burst")]
        buggy = _enchant_id_fn(buggy_names) + """
fn open_menu(block: &str) -> LiveMenu {
    match block { "minecraft:anvil" => LiveMenu::Anvil(AnvilMenu::default()), _ => LiveMenu::None }
}
fn enchantment_cost(r: &mut Lcg, s: i32, sh: i32, e: i32) -> i32 { 1 }
fn roll_table_enchantments(r: &mut Lcg, i: &str, e: i32, l: i32) -> Vec<(i64, i64)> { vec![] }
"""
        g = gate({"src/menus_and_items.rs": buggy}, "gate2_enchanting")
        self.assertEqual(g["status"], "FAIL")
        text = "\n".join(g["findings"])
        for name in ("density", "breach", "wind_burst"):
            self.assertIn(name, text)
        self.assertIn("roll_table_enchantments", text)
        self.assertIn("LiveMenu::Enchanting", text)

    def test_passes_on_full_42_with_live_wiring(self):
        fixed = _enchant_id_fn(_ENCH_NAMES) + self.WIRING
        g = gate({
            "src/menus_and_items.rs": fixed,
            "src/player_mechanisms.rs": self.WIRING,
        }, "gate2_enchanting")
        self.assertEqual(g["status"], "PASS", g["findings"])
        self.assertEqual(len(svpa.V1211_ENCHANTMENTS), 42)


# ---------------------------------------------------------------------------
# Gate 3 — structure loot
# ---------------------------------------------------------------------------

_STRUCTURES = """
fn build_desert(w: &mut W) {
    let tnt = reg.state_of("minecraft:tnt", None);
    w.generate_box(9, -13, 9, 11, -13, 11, tnt, air, false);
    w.create_chest_world(rng, (0, 0, 0), "minecraft:chests/desert_pyramid");
    w.create_brushable((1, 2, 3), "minecraft:archaeology/desert_pyramid");
}
fn build_jungle(w: &mut W) {
    w.create_chest_local(rng, 8, -3, 3, "minecraft:chests/jungle_temple");
    w.create_dispenser((2, -3, 8), "minecraft:chests/jungle_temple_dispenser");
}
"""

_ROLLER_STUB = """
fn roll_loot_container_slots(kind: ContainerKind, loot_table: &str, loot_seed: Option<i64>) -> HashMap<i16, Stack> {
    let seed = loot_seed.unwrap_or(0);
    if loot_table == "minecraft:chests/abandoned_mineshaft" {
        roll_cart_loot(seed)
    } else {
        let pools = loot_tables().lock().unwrap().get(loot_table).map(|p| p.clone());
        HashMap::new()
    }
}
"""

_ROLLER_FIXED = """
fn roll_loot_container_slots(kind: ContainerKind, loot_table: &str, loot_seed: Option<i64>) -> HashMap<i16, Stack> {
    let seed = loot_seed.unwrap_or(0);
    if loot_table == "minecraft:chests/abandoned_mineshaft" {
        return roll_cart_loot(seed);
    }
    let pools = loot_tables().lock().unwrap().get(loot_table).map(|p| p.clone());
    if pools.is_none() {
        return crate::chest_loot::vanilla_loot_roll(loot_table, seed)
            .into_iter().enumerate().map(|(i, e)| (i as i16, e)).collect();
    }
    HashMap::new()
}
"""

_LIVE_TRIAL = """
fn be_tick() {
    match be {
        LiveBlockEntity::TrialSpawner(spawner) => {
            after = tick_live_trial_spawner(spawner, reg, key, after, now);
        }
        LiveBlockEntity::Vault(vault) => { vault.tick(now); }
    }
}
fn tick_live_trial_spawner(s: &TrialSpawner) -> u32 { 0 }
"""


def _gate3_files(roller):
    return {
        "src/single_piece_structures.rs": _STRUCTURES,
        "src/structure_template.rs":
            'fn f() { loot_table: "minecraft:chests/simple_dungeon".to_owned() }',
        "src/generated_content.rs":
            'fn g() { let t = "minecraft:chests/abandoned_mineshaft"; }',
        "src/menus_and_items.rs":
            'let vault_loot = "minecraft:chests/trial_chambers/reward";\n'
            + _LIVE_TRIAL + roller,
        "src/chest_loot.rs": BUILTIN_REGISTRY,
    }


class Gate3StructureLootTest(unittest.TestCase):
    def test_flags_datapack_only_roller(self):
        g = gate(_gate3_files(_ROLLER_STUB), "gate3_structure_loot")
        self.assertEqual(g["status"], "FAIL")
        text = "\n".join(g["findings"])
        self.assertIn("abandoned_mineshaft", text)
        self.assertIn("unreachable", text)

    def test_passes_on_builtin_fallback_roller(self):
        g = gate(_gate3_files(_ROLLER_FIXED), "gate3_structure_loot")
        self.assertEqual(g["status"], "PASS", g["findings"])

    def test_flags_missing_structure_tables(self):
        files = _gate3_files(_ROLLER_FIXED)
        files["src/single_piece_structures.rs"] = "fn none() {}"
        g = gate(files, "gate3_structure_loot")
        self.assertEqual(g["status"], "FAIL")
        self.assertIn("desert_pyramid", "\n".join(g["findings"]))


# ---------------------------------------------------------------------------
# Gate 4 — dimensions
# ---------------------------------------------------------------------------

_PORTAL_FIXED = """
fn materialize_end_platform() {
    let obsidian = name_to_state("obsidian").unwrap_or(0);
    for dx in -2..=2 { for dz in -2..=2 {
        set_block_dim(DimId::End, 100 + dx, 48, dz, obsidian);
    }}
}
fn enter_portal(me: &Arc<Client>, st: u32, dim: DimId) {
    let (target_dim, landing) = if is_end_portal_state(st) {
        (DimId::End, (100.5, 49.0, 0.5))
    } else if dim == DimId::End {
        (DimId::Overworld, me.respawn_point())
    } else {
        (DimId::Nether, scaled(me.pos))
    };
    teleport(me, target_dim, landing);
}
fn end_gateway_warp(me: &Arc<Client>, gateway: (i32, i32, i32)) -> (f64, f64, f64) { (0.0, 0.0, 0.0) }
fn tick_end_gateway_entries() { for c in clients() { end_gateway_entry_check(c); } }
fn end_gateway_entry_check(c: &Arc<Client>) { end_gateway_warp(c, (0, 60, 0)); }
fn server_tick() { tick_end_gateway_entries(); }
"""

_PORTAL_NO_RETURN = _PORTAL_FIXED.replace(
    """    } else if dim == DimId::End {
        (DimId::Overworld, me.respawn_point())
    } else {""",
    """    } else {""")

_MOB_BEHAVIORS = """
impl DragonFightRewards {
    fn place_bedrock_exit_portal(&self, place: impl FnMut([i32; 3], &'static str)) {}
}
"""

_MAIN_EXIT_PORTAL = """
fn dragon_dead(rewards: DragonFightRewards) {
    rewards.place_bedrock_exit_portal(|[x, y, z], name| set_block(0, x, y, z));
}
"""


def _gate4_files(chorus, portal=_PORTAL_FIXED):
    return {
        "src/play_protocol.rs": portal,
        "src/menus_and_items.rs": chorus,
        "src/mob_behaviors.rs": _MOB_BEHAVIORS,
        "src/main.rs": _MAIN_EXIT_PORTAL,
    }


class Gate4DimensionsTest(unittest.TestCase):
    def test_flags_overworld_surface_in_chorus(self):
        g = gate(_gate4_files(CHORUS_STUB), "gate4_dimensions")
        self.assertEqual(g["status"], "FAIL")
        text = "\n".join(g["findings"])
        self.assertIn("surface_y", text)
        self.assertIn("dimension-aware", text)

    def test_passes_on_dimension_aware_chorus(self):
        g = gate(_gate4_files(CHORUS_FIXED), "gate4_dimensions")
        self.assertEqual(g["status"], "PASS", g["findings"])

    def test_flags_missing_end_return(self):
        g = gate(_gate4_files(CHORUS_FIXED, _PORTAL_NO_RETURN),
                 "gate4_dimensions")
        self.assertEqual(g["status"], "FAIL")
        self.assertIn("End->Overworld", "\n".join(g["findings"]))


# ---------------------------------------------------------------------------
# Gate 5 — spawning
# ---------------------------------------------------------------------------

_SPAWNER = """
struct LiveNaturalSpawnQueries<'a> { dim: DimId }
impl OverworldSpawnQueries for LiveNaturalSpawnQueries<'_> {
    fn is_world_gen(&self) -> bool {
        IS_WORLD_GEN
    }
}
"""

_SPAWN_CALLS = """
fn env_tick(t: u64) {
    spawn_nightly_hostile(t);
    spawn_passive_natural(t);
    spawn_nether_natural(t);
    spawn_end_natural(t);
}
fn spawn_nightly_hostile(t: u64) {}
fn spawn_passive_natural(t: u64) {}
fn spawn_nether_natural(t: u64) {}
fn spawn_end_natural(t: u64) {}
"""


def _gate5_files(world_gen_body, spawn_calls=_SPAWN_CALLS):
    return {
        "src/natural_spawner.rs": _SPAWNER.replace("IS_WORLD_GEN", world_gen_body),
        "src/main.rs": spawn_calls,
    }


class Gate5SpawningTest(unittest.TestCase):
    def test_flags_world_gen_stub(self):
        g = gate(_gate5_files("false"), "gate5_spawning")
        self.assertEqual(g["status"], "FAIL")
        self.assertIn("is_world_gen", "\n".join(g["findings"]))

    def test_flags_missing_dimension_spawn(self):
        g = gate(_gate5_files(
            "true",
            _SPAWN_CALLS.replace("    spawn_nether_natural(t);\n", "")),
            "gate5_spawning")
        self.assertEqual(g["status"], "FAIL")
        self.assertIn("Nether", "\n".join(g["findings"]))

    def test_passes_on_live_three_dimension_spawning(self):
        g = gate(_gate5_files("true"), "gate5_spawning")
        self.assertEqual(g["status"], "PASS", g["findings"])


# ---------------------------------------------------------------------------
# Gate 6 — disconnect
# ---------------------------------------------------------------------------

_CLEANUP = """
fn disconnect_cleanup(me_cleanup: &Arc<Client>) {
    cls.retain(|c| c.eid != me_cleanup.eid);
    broadcast_seen_destroy(me_cleanup.eid);
    EXTRA
}
"""

_REMOVE = """
    let mut p = Vec::new();
    write_varint(&mut p, 1);
    p.extend_from_slice(&me_cleanup.uuid);
    broadcast_packet(me_cleanup.eid, 0x3d, &p);
"""


class Gate6DisconnectTest(unittest.TestCase):
    def test_flags_missing_player_info_remove(self):
        src = _CLEANUP.replace("EXTRA", "autosave_world();")
        g = gate({"src/play_protocol.rs": src}, "gate6_disconnect")
        self.assertEqual(g["status"], "FAIL")
        self.assertIn("0x3d", "\n".join(g["findings"]))

    def test_passes_on_player_info_remove(self):
        src = _CLEANUP.replace("EXTRA", _REMOVE)
        g = gate({"src/play_protocol.rs": src}, "gate6_disconnect")
        self.assertEqual(g["status"], "PASS", g["findings"])

    def test_flags_missing_cleanup_path(self):
        g = gate({"src/play_protocol.rs": "fn idle() {}"},
                  "gate6_disconnect")
        self.assertEqual(g["status"], "FAIL")


# ---------------------------------------------------------------------------
# Gate 7 — cramming
# ---------------------------------------------------------------------------


def _gate7_files(default=24, damage="6.0f32", invuln=10, use_load=True):
    load = "GR_MAX_ENTITY_CRAMMING.load(Ordering::Relaxed)" if use_load else "24"
    return {
        "src/entity_push.rs": f"""
const ENTITY_CRAMMING_DEFAULT: i32 = {default};
const ENTITY_CRAMMING_DAMAGE: f32 = {damage};
const ENTITY_HURT_INVULNERABLE_TICKS: u64 = {invuln};
fn cramming_tick(now: u64, prev: u64) -> f32 {{
    let max = {load};
    if now.saturating_sub(prev) < ENTITY_HURT_INVULNERABLE_TICKS {{
        return 0.0;
    }}
    ENTITY_CRAMMING_DAMAGE * (max as f32)
}}
""",
        "src/main.rs": """
static GR_MAX_ENTITY_CRAMMING: AtomicI32 = AtomicI32::new(ENTITY_CRAMMING_DEFAULT);
""",
    }


class Gate7CrammingTest(unittest.TestCase):
    def test_flags_wrong_defaults(self):
        g = gate(_gate7_files(default=16, damage="4.0f32", invuln=5,
                            use_load=False), "gate7_cramming")
        self.assertEqual(g["status"], "FAIL")
        text = "\n".join(g["findings"])
        self.assertIn("24", text)
        self.assertIn("6.0", text)
        self.assertIn("10", text)

    def test_passes_on_vanilla_values(self):
        g = gate(_gate7_files(), "gate7_cramming")
        self.assertEqual(g["status"], "PASS", g["findings"])


# ---------------------------------------------------------------------------
# Gate 8 — survival matrix
# ---------------------------------------------------------------------------

_SURVIVAL = {
    "src/menus_and_items.rs": """
fn open_brewing() { let m = LiveMenu::Brewing(BrewingMenu::default()); }
fn open_trader() { let m = LiveMenu::Merchant(MerchantMenu::default()); }
fn crossbow_fire_loaded(me: &Arc<Client>, slot: i16) {}
fn shoot(me: &Arc<Client>) { crossbow_fire_loaded(me, 0); }
""",
    "src/villager_raid.rs": """
fn live_raid_step() {}
fn villager_raid_tick() {
    villager.restocks_today += 1;
    live_raid_step();
}
""",
    "src/world_mechanisms.rs": """
fn respawn_anchor_use(charges: u8) -> u8 { charges }
fn clicked() { respawn_anchor_use(3); }
""",
    "src/main.rs": """
fn live_spawn_position_ok() -> bool { true }
fn live_redstone_tick() {}
fn elytra_wall_collision_tick(c: &Arc<Client>) {}
fn fireworks_step() {}
fn stage() {
    live_spawn_position_ok();
    damage_shield(me, 4.0);
    evaluate_trident_special(ctx);
    elytra_wall_collision_tick(&c);
    fireworks_step();
    run("redstone", live_redstone_tick);
    if GR_WEATHER_CYCLE.load(Ordering::Relaxed) == 1 {
        thunder();
        lightning();
    }
}
""",
    "src/combat_and_health.rs": """
fn damage_shield(me: &Arc<Client>, blocked: f32) {}
fn evaluate_trident_special(ctx: &Ctx) -> u32 { 0 }
""",
    "src/redstone.rs": """
fn eval_blocks() { piston(); hopper(); comparator(); }
""",
}


class Gate8SurvivalTest(unittest.TestCase):
    def test_flags_missing_mechanism(self):
        buggy = dict(_SURVIVAL)
        buggy["src/villager_raid.rs"] = "fn live_raid_step() {}"
        buggy["src/redstone.rs"] = "fn eval_blocks() { piston(); hopper(); }"
        g = gate(buggy, "gate8_survival")
        self.assertEqual(g["status"], "FAIL")
        text = "\n".join(g["findings"])
        self.assertIn("raids", text)
        self.assertIn("redstone", text)

    def test_passes_on_full_matrix(self):
        g = gate(_SURVIVAL, "gate8_survival")
        self.assertEqual(g["status"], "PASS", g["findings"])



DYNAMIC_BUILTIN_PROVIDER = """
fn read_builtin_loot_table(id: &str) -> Option<String> {
    let relative = format!("data/minecraft/loot_table/{id}.json");
    crate::vanilla_jar_data_read(&relative).map(|bytes| String::new())
}
pub(crate) fn vanilla_loot_table_json(id: &str) -> Option<&'static str> {
    read_builtin_loot_table(id).map(|body| Box::leak(body.into_boxed_str()))
}
"""

DYNAMIC_ROLLER = """
fn roll_loot_container_slots(kind: ContainerKind, loot_table: &str, loot_seed: Option<i64>) -> HashMap<i16, Stack> {
    let table_json = loot_table_json(loot_table).or_else(|| {
        crate::chest_loot::vanilla_loot_table_json(loot_table).map(str::to_owned)
    });
    if let Some(table_json) = table_json {
        return unpack_loot_table_json_at(&table_json, seed, 27, None, Some(loot_table));
    }
    HashMap::new()
}
"""


# ---------------------------------------------------------------------------
# Gate 9 — built-in bypass
# ---------------------------------------------------------------------------


def _gate9_files(roller, reel):
    return {
        "src/chest_loot.rs": BUILTIN_REGISTRY,
        "src/menus_and_items.rs": roller + reel,
    }


class Gate9BuiltinBypassTest(unittest.TestCase):
    def test_flags_silently_bypassed_tables(self):
        g = gate(_gate9_files(_ROLLER_STUB, Gate1FishingTest.STUB_REEL),
                 "gate9_builtin_bypass")
        self.assertEqual(g["status"], "FAIL")
        text = "\n".join(g["findings"])
        self.assertIn("bypassed", text)
        self.assertIn("gameplay/fishing", text)

    def test_passes_when_consumers_reach_builtins(self):
        g = gate(_gate9_files(_ROLLER_FIXED, Gate1FishingTest.FIXED_REEL),
                 "gate9_builtin_bypass")
        self.assertEqual(g["status"], "PASS", g["findings"])

    def test_flags_missing_registry(self):
        g = gate({"src/menus_and_items.rs": _ROLLER_FIXED},
                  "gate9_builtin_bypass")
        self.assertEqual(g["status"], "FAIL")
        self.assertIn("VANILLA_LOOT_GENERATORS", "\n".join(g["findings"]))

    def test_dynamic_vanilla_pack_provider_satisfies_builtin_fallback(self):
        files = {
            "src/chest_loot.rs": DYNAMIC_BUILTIN_PROVIDER,
            "src/menus_and_items.rs": DYNAMIC_ROLLER + Gate1FishingTest.FIXED_REEL,
        }
        g = gate(files, "gate9_builtin_bypass")
        self.assertEqual(g["status"], "PASS", g["findings"])


# ---------------------------------------------------------------------------
# Gate 10 — dimension leak
# ---------------------------------------------------------------------------

_DIM_SAFE = {
    "src/play_protocol.rs": """
fn enter_portal(me: &Arc<Client>, dim: DimId, x: i32, z: i32) -> f64 {
    let st = block_at_dim(dim, x, 64, z);
    let y = if dim == DimId::Overworld { surface_y(x, z) } else { 64.0 };
    y
}
fn end_gateway_warp(me: &Arc<Client>, gateway: (i32, i32, i32)) -> (f64, f64, f64) {
    let st = block_at_dim(DimId::End, gateway.0, gateway.1, gateway.2);
    (0.0, 0.0, 0.0)
}
fn end_gateway_entry_check(c: &Arc<Client>, now: u64) -> bool {
    block_at_dim(DimId::End, 0, 60, 0) == gateway_state()
}
""",
    "src/main.rs": """
fn live_mob_tick_dimension(dim: DimId, bx: i32, by: i32, bz: i32) {
    if m.etype == 87
        && dim == DimId::Overworld
        && mob_env.below_or(epoch, || block_at(bx, by - 1, bz)) == grass
    {
        set_block(bx, by - 1, bz, dirt);
    }
    for mob in mobs {
        let h = block_at_dim(dim, bx, by, bz);
    }
}
""",
    "src/menus_and_items.rs": CHORUS_FIXED,
}


class Gate10DimensionLeakTest(unittest.TestCase):
    def test_flags_unguarded_overworld_lookups(self):
        buggy = dict(_DIM_SAFE)
        buggy["src/play_protocol.rs"] = _DIM_SAFE["src/play_protocol.rs"] + """
fn end_gateway_warp_2(me: &Arc<Client>) {
    let ground = surface_y(48, 0);
}
"""
        buggy["src/main.rs"] = _DIM_SAFE["src/main.rs"].replace(
            "if m.etype == 87\n        && dim == DimId::Overworld\n",
            "if m.etype == 87\n")
        buggy["src/menus_and_items.rs"] = CHORUS_STUB
        g = gate(buggy, "gate10_dimension_leak")
        self.assertEqual(g["status"], "FAIL")
        text = "\n".join(g["findings"])
        self.assertIn("surface_y", text)
        self.assertIn("live_mob_tick_dimension", text)

    def test_flags_else_branch_leak(self):
        buggy = dict(_DIM_SAFE)
        buggy["src/main.rs"] = """
fn live_mob_tick_dimension(dim: DimId, x: i32, z: i32) {
    let y = if dim == DimId::Overworld { 64.0 } else { surface_y(x, z) };
    match dim {
        DimId::Overworld => {}
        _ => { let g = block_at(x, 64, z); }
    }
}
"""
        g = gate(buggy, "gate10_dimension_leak")
        self.assertEqual(g["status"], "FAIL")
        text = "\n".join(g["findings"])
        self.assertIn("surface_y", text)
        self.assertIn("block_at", text)

    def test_passes_on_guarded_lookups(self):
        g = gate(_DIM_SAFE, "gate10_dimension_leak")
        self.assertEqual(g["status"], "PASS", g["findings"])


# ---------------------------------------------------------------------------
# gate11_branch_mining
# ---------------------------------------------------------------------------

_DIG_ARM_FIXED = """
        0x24 => {
            let eye_y = client_eye_y(&me, dim, px, py, pz);
            let modified_speed = mining_speed_with_modifiers(
                base_speed, false, 0, haste, fatigue, under, on_ground);
            let instant = instant_mining_break(
                modified_speed, block_hardness(old), can);
            if (gm_creative && status == 0) || instant {
                break_now = true;
            }
            if !break_now && old != 0 {
                note_block_update_dim(dim, x, y, z, old);
            }
            if break_now {
                finish_block_break(&me, dim, x, y, z, old);
            }
            let _ = send_packet(&mut sock, 0x05, ack(seq));
        }
"""

_DIG_ARM_STUB = """
        0x24 => {
            let req = dig_seconds(old, &held);
            let dd = (px - x).powi(2) + (py + 1.62 - y).powi(2) + (pz - z).powi(2);
            if (gm_creative && status == 0) || req == Some(0.0) {
                break_now = true;
            }
            if break_now {
                finish_block_break(&me, dim, x, y, z, old);
            }
            let _ = send_packet(&mut sock, 0x05, ack(seq));
        }
"""

_PLACE_ARM_FIXED = """
        0x38 => {
            let eye_y = client_eye_y(&me, dim, px, py, pz);
            let ddy = y as f64 + 0.5 - eye_y;
        }
"""

_PLACE_ARM_STUB = """
        0x38 => {
            let ddy = y as f64 + 0.5 - (py + 1.62);
        }
"""

_MINING_HELPERS_LIVE = """
fn mining_speed_with_modifiers(
    base_speed: f32,
    effective_tool: bool,
    efficiency_level: i32,
) -> f32 {
    base_speed + efficiency_level as f32
}
fn mining_damage_per_tick(speed: f32, h: f32, can_harvest: bool) -> f32 {
    speed / h
}
fn instant_mining_break(speed: f32, h: f32, can_harvest: bool) -> bool {
    h == 0.0 || mining_damage_per_tick(speed, h, can_harvest) >= 1.0
}
"""

_MINING_HELPERS_DEAD = """
#[cfg_attr(not(test), allow(dead_code))]
fn mining_speed_with_modifiers(base_speed: f32) -> f32 {
    base_speed
}
#[cfg_attr(not(test), allow(dead_code))]
fn mining_damage_per_tick(speed: f32, h: f32, can_harvest: bool) -> f32 {
    speed / h
}
#[cfg_attr(not(test), allow(dead_code))]
fn instant_mining_break(speed: f32, h: f32, can_harvest: bool) -> bool {
    h == 0.0 || mining_damage_per_tick(speed, h, can_harvest) >= 1.0
}
"""

_EYE_HELPER = """
fn player_eye_height_at(sneaking: bool, fall_fly: bool, swimming: bool) -> f32 {
    if fall_fly || swimming { 0.4 } else if sneaking { 1.27 } else { 1.62 }
}
"""


def _mining_files(dig_arm=_DIG_ARM_FIXED, place_arm=_PLACE_ARM_FIXED,
                  helpers=_MINING_HELPERS_LIVE, fluid=_EYE_HELPER):
    return {
        "src/play_protocol.rs": "fn handle() {\n    match pid {\n"
            + dig_arm + place_arm + "    }\n}\n",
        "src/player_mechanisms.rs": helpers,
        "src/entity_fluid.rs": fluid,
    }


class Gate11BranchMiningTest(unittest.TestCase):
    def test_flags_req_some_zero_regression(self):
        buggy = _mining_files(dig_arm=_DIG_ARM_FIXED.replace(
            "(gm_creative && status == 0) || instant",
            "(gm_creative && status == 0) || req == Some(0.0)"))
        g = gate(buggy, "gate11_branch_mining")
        self.assertEqual(g["status"], "FAIL")
        self.assertIn("req == Some(0.0)", "\n".join(g["findings"]))

    def test_flags_missing_canonical_instant_model(self):
        buggy = _mining_files(dig_arm=_DIG_ARM_STUB)
        g = gate(buggy, "gate11_branch_mining")
        self.assertEqual(g["status"], "FAIL")
        self.assertIn("break-progress model", "\n".join(g["findings"]))

    def test_flags_hardcoded_standing_eye(self):
        buggy = _mining_files(
            dig_arm=_DIG_ARM_FIXED.replace(
                "client_eye_y(&me, dim, px, py, pz)", "py + 1.62"),
            place_arm=_PLACE_ARM_STUB)
        g = gate(buggy, "gate11_branch_mining")
        self.assertEqual(g["status"], "FAIL")
        text = "\n".join(g["findings"])
        self.assertIn("+ 1.62", text)
        self.assertIn("0x38", text)

    def test_flags_missing_resync_before_ack(self):
        no_resync = _DIG_ARM_FIXED.replace(
            """            if !break_now && old != 0 {
                note_block_update_dim(dim, x, y, z, old);
            }
""", "")
        g = gate(_mining_files(dig_arm=no_resync), "gate11_branch_mining")
        self.assertEqual(g["status"], "FAIL")
        self.assertIn("resync", "\n".join(g["findings"]))

    def test_flags_resync_after_ack(self):
        after = _DIG_ARM_FIXED.replace(
            """            if !break_now && old != 0 {
                note_block_update_dim(dim, x, y, z, old);
            }
            if break_now {
                finish_block_break(&me, dim, x, y, z, old);
            }
            let _ = send_packet(&mut sock, 0x05, ack(seq));""",
            """            if break_now {
                finish_block_break(&me, dim, x, y, z, old);
            }
            let _ = send_packet(&mut sock, 0x05, ack(seq));
            if !break_now && old != 0 {
                note_block_update_dim(dim, x, y, z, old);
            }""")
        g = gate(_mining_files(dig_arm=after), "gate11_branch_mining")
        self.assertEqual(g["status"], "FAIL")

    def test_flags_dead_or_unwired_helpers(self):
        buggy = _mining_files(helpers=_MINING_HELPERS_DEAD)
        g = gate(buggy, "gate11_branch_mining")
        self.assertEqual(g["status"], "FAIL")
        self.assertIn("allow(dead_code)", "\n".join(g["findings"]))

    def test_flags_unwired_helper_no_call_site(self):
        no_call = _DIG_ARM_FIXED.replace(
            "instant_mining_break(", "breakable_instantly(")
        buggy = _mining_files(dig_arm=no_call)
        g = gate(buggy, "gate11_branch_mining")
        self.assertEqual(g["status"], "FAIL")

    def test_passes_on_canonical_wiring(self):
        g = gate(_mining_files(), "gate11_branch_mining")
        self.assertEqual(g["status"], "PASS", g["findings"])


# ---------------------------------------------------------------------------
# dim_guarded unit coverage
# ---------------------------------------------------------------------------

class DimGuardedTest(unittest.TestCase):
    def check(self, body, guarded=True):
        pos = body.find("surface_y(")
        if pos < 0:
            pos = body.find("block_at(")
        self.assertGreaterEqual(pos, 0)
        self.assertEqual(svpa.dim_guarded(body, pos), guarded,
                         f"body={body!r}")

    def test_if_chain_guard(self):
        self.check("if dim == DimId::Overworld && f(block_at(x, y, z)) {}")

    def test_ternary_guard(self):
        self.check("let y = if dim == DimId::Overworld { surface_y(x, z) } else { a };")

    def test_match_arm_guard(self):
        self.check("match dim { DimId::Overworld => surface_y(x, z), _ => 0 };")

    def test_bare_call_unguarded(self):
        self.check("let y = surface_y(x, z);", guarded=False)

    def test_else_branch_unguarded(self):
        self.check("let y = if dim == DimId::Overworld { a } else { surface_y(x, z) };",
                   guarded=False)

    def test_neq_then_branch_unguarded(self):
        self.check("let y = if dim != DimId::Overworld { surface_y(x, z) } else { a };",
                   guarded=False)

    def test_neq_else_branch_guarded(self):
        self.check("let y = if dim != DimId::Overworld { a } else { surface_y(x, z) };")

    def test_multi_dim_arm_unguarded(self):
        self.check("match dim { DimId::Overworld | DimId::Nether => surface_y(x, z), _ => 0 };",
                   guarded=False)

    def test_wildcard_arm_unguarded(self):
        self.check("match dim { DimId::Overworld => 0, _ => surface_y(x, z) };",
                   guarded=False)


# ---------------------------------------------------------------------------
# CLI / report plumbing
# ---------------------------------------------------------------------------

class CliTest(unittest.TestCase):
    def _main(self, *argv):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = svpa.main(list(argv))
        return code, out.getvalue()

    def test_scorecard_and_json_on_repo(self):
        code, text = self._main()
        self.assertEqual(code, 0)
        self.assertIn("score:", text)
        code, blob = self._main("--json")
        self.assertEqual(code, 0)
        report = json.loads(blob)
        self.assertEqual(report["schema"], "megu-strict-vanilla-parity/1")
        self.assertEqual(len(report["gates"]), 11)
        self.assertEqual(
            report["summary"]["total"],
            report["summary"]["pass"] + report["summary"]["warn"]
            + report["summary"]["fail"])

    def test_strict_exit_codes(self):
        code, _ = self._main("--strict")
        self.assertIn(code, (0, 1))
        report = svpa.run_gates(svpa.load_texts(REPO))
        want = 1 if report["summary"]["fail"] else 0
        self.assertEqual(code, want)

    def test_missing_root_is_exit_2(self):
        with tempfile.TemporaryDirectory() as td:
            code, _ = self._main("--root", td)
            self.assertEqual(code, 2)

    def test_synthetic_tree_loads_only_audit_files(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src"
            src.mkdir()
            (src / "entity_push.rs").write_text("fn f() {}")
            (src / "unrelated.rs").write_text("fn g() {}")
            texts = svpa.load_texts(Path(td))
            self.assertEqual(set(texts), {"src/entity_push.rs"})


if __name__ == "__main__":
    unittest.main()
