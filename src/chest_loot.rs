//! Seeded 1.21.1 `minecraft:chests/abandoned_mineshaft` chest-loot table fill.
//! Produces exact vanilla `(slot, item, count, stored_enchantment)` entries from
//! a 48-bit `Lcg` seed without depending on runtime inventory or network types.
use crate::generation_math::Lcg;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};

// Minimal JSON value + parser for the generic loot evaluator. The
// module-layer ratchet forbids adding a `chest_loot -> json` source edge, so
// the canonical `megu_core::json` reader is used directly and lowered into
// the private `J` shape below. It accepts the subset vanilla loot tables use:
// objects, arrays, strings (with basic escapes), numbers and the
// true/false/null literals.
#[derive(Clone, Debug)]
pub(crate) enum J {
    Null,
    Num(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    pub(crate) fn num(&self) -> f64 {
        match self {
            J::Num(n) => *n,
            _ => 0.0,
        }
    }
    pub(crate) fn str(&self) -> &str {
        match self {
            J::Str(s) => s,
            _ => "",
        }
    }
    pub(crate) fn as_arr(&self) -> Option<&Vec<J>> {
        match self {
            J::Arr(a) => Some(a),
            _ => None,
        }
    }
    pub(crate) fn get(&self, key: &str) -> Option<&J> {
        match self {
            J::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

/// Bounds for loot-table documents: operator-supplied datapack JSON, so
/// node/byte ceilings stay off; the depth cap keeps the canonical recursive
/// reader fail-closed where the retired cursor recursed unbounded.
const LOOT_JSON_LIMITS: megu_core::json::JsonLimits = megu_core::json::JsonLimits {
    max_depth: 128,
    max_nodes: usize::MAX,
    max_bytes: usize::MAX,
};

/// Lower the canonical [`megu_core::json::JsonValue`] tree into `J`.
/// `true`/`false` surface as `Num(1.0)`/`Num(0.0)` exactly as the retired
/// cursor spelled them, so `jbool`/`jnum` callers see no change.
fn lower_json(value: megu_core::json::JsonValue) -> J {
    use megu_core::json::JsonValue as Core;
    match value {
        Core::Null => J::Null,
        Core::Bool(b) => J::Num(if b { 1.0 } else { 0.0 }),
        Core::Number(n) => J::Num(n),
        Core::String(s) => J::Str(s),
        Core::Array(items) => J::Arr(items.into_iter().map(lower_json).collect()),
        Core::Object(pairs) => J::Obj(
            pairs
                .into_iter()
                .map(|(k, v)| (k, lower_json(v)))
                .collect(),
        ),
    }
}

pub(crate) fn jparse(bytes: &[u8]) -> Option<J> {
    let text = std::str::from_utf8(bytes).ok()?;
    megu_core::json::parse_json_with_limits(text, LOOT_JSON_LIMITS, false)
        .ok()
        .map(lower_json)
}

pub(crate) fn js<'a>(j: &'a J, key: &str) -> Option<&'a J> {
    j.get(key)
}

pub(crate) fn jstr(j: &J, key: &str) -> Option<String> {
    js(j, key).map(|v| v.str().to_string())
}

pub(crate) fn jnum(j: &J, key: &str) -> Option<f64> {
    js(j, key).map(|v| v.num())
}

pub(crate) fn jint(j: &J, key: &str, default: i32) -> i32 {
    jnum(j, key).unwrap_or(default as f64) as i32
}

pub(crate) fn jbool(j: &J, key: &str, default: bool) -> bool {
    match js(j, key) {
        Some(J::Num(n)) => *n != 0.0,
        Some(J::Str(s)) => s == "true",
        _ => default,
    }
}

pub(crate) fn jarr<'a>(j: &'a J, key: &str) -> Option<&'a Vec<J>> {
    js(j, key).and_then(J::as_arr)
}

/// `J` renders numbers without a fractional part as integers, so a component
/// patch keeps `16` instead of `16.0` like the JSON the author wrote.
fn json_number_text(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

fn push_json_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Render one parsed value back to compact JSON. Component payloads stay JSON
/// text rather than a second typed model, so a datapack's component patch
/// round-trips without this module having to know every component's schema.
fn render_json(value: &J, out: &mut String) {
    match value {
        J::Null => out.push_str("null"),
        J::Num(n) => out.push_str(&json_number_text(*n)),
        J::Str(text) => push_json_string(out, text),
        J::Arr(items) => {
            out.push('[');
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                render_json(item, out);
            }
            out.push(']');
        }
        J::Obj(fields) => {
            out.push('{');
            for (idx, (key, value)) in fields.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                push_json_string(out, key);
                out.push(':');
                render_json(value, out);
            }
            out.push('}');
        }
    }
}

fn render_json_text(value: &J) -> String {
    let mut out = String::new();
    render_json(value, &mut out);
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LootEntry {
    pub slot: i16,
    pub item: &'static str,
    pub count: u8,
    pub enchantment: Option<(&'static str, i32)>,
}

#[derive(Clone, Debug)]
struct Stack {
    item: &'static str,
    count: i32,
    enchantment: Option<(&'static str, i32)>,
}

/// `Mth.nextInt(RandomSource, int, int)`: returns `min` with zero RNG draws
/// when `min >= max`.
fn mth_next_int(rng: &mut Lcg, min: i32, max: i32) -> i32 {
    if min >= max {
        min
    } else {
        rng.next_bound(max - min + 1) + min
    }
}

/// `Util.shuffle(List, RandomSource)` (Fisher-Yates from the tail).
fn util_shuffle<T>(list: &mut [T], rng: &mut Lcg) {
    for i in (2..=list.len()).rev() {
        let j = rng.next_bound(i as i32) as usize;
        list.swap(i - 1, j);
    }
}

/// Exact `#minecraft:on_random_loot` enchantment registry expansion in 1.21.1:
/// 35 `#minecraft:non_treasure` entries followed by the 4 explicit treasure entries.
const ON_RANDOM_LOOT: &[(&str, i32)] = &[
    ("minecraft:protection", 4),
    ("minecraft:fire_protection", 4),
    ("minecraft:feather_falling", 4),
    ("minecraft:blast_protection", 4),
    ("minecraft:projectile_protection", 4),
    ("minecraft:respiration", 3),
    ("minecraft:aqua_affinity", 1),
    ("minecraft:thorns", 3),
    ("minecraft:depth_strider", 3),
    ("minecraft:sharpness", 5),
    ("minecraft:smite", 5),
    ("minecraft:bane_of_arthropods", 5),
    ("minecraft:knockback", 2),
    ("minecraft:fire_aspect", 2),
    ("minecraft:looting", 3),
    ("minecraft:sweeping_edge", 3),
    ("minecraft:efficiency", 5),
    ("minecraft:silk_touch", 1),
    ("minecraft:unbreaking", 3),
    ("minecraft:fortune", 3),
    ("minecraft:power", 5),
    ("minecraft:punch", 2),
    ("minecraft:flame", 1),
    ("minecraft:infinity", 1),
    ("minecraft:luck_of_the_sea", 3),
    ("minecraft:lure", 3),
    ("minecraft:loyalty", 3),
    ("minecraft:impaling", 5),
    ("minecraft:riptide", 3),
    ("minecraft:channeling", 1),
    ("minecraft:multishot", 1),
    ("minecraft:quick_charge", 3),
    ("minecraft:piercing", 4),
    ("minecraft:density", 5),
    ("minecraft:breach", 4),
    ("minecraft:binding_curse", 1),
    ("minecraft:vanishing_curse", 1),
    ("minecraft:frost_walker", 2),
    ("minecraft:mending", 1),
];

enum PoolEntry {
    Item {
        name: &'static str,
        weight: i32,
        min_count: i32,
        max_count: i32,
        enchant_randomly: bool,
    },
    Empty {
        weight: i32,
    },
}

const POOL_0: &[PoolEntry] = &[
    PoolEntry::Item {
        name: "minecraft:golden_apple",
        weight: 20,
        min_count: 1,
        max_count: 1,
        enchant_randomly: false,
    },
    PoolEntry::Item {
        name: "minecraft:enchanted_golden_apple",
        weight: 1,
        min_count: 1,
        max_count: 1,
        enchant_randomly: false,
    },
    PoolEntry::Item {
        name: "minecraft:name_tag",
        weight: 30,
        min_count: 1,
        max_count: 1,
        enchant_randomly: false,
    },
    PoolEntry::Item {
        name: "minecraft:book",
        weight: 10,
        min_count: 1,
        max_count: 1,
        enchant_randomly: true,
    },
    PoolEntry::Item {
        name: "minecraft:iron_pickaxe",
        weight: 5,
        min_count: 1,
        max_count: 1,
        enchant_randomly: false,
    },
    PoolEntry::Empty { weight: 5 },
];

const POOL_1: &[PoolEntry] = &[
    PoolEntry::Item {
        name: "minecraft:iron_ingot",
        weight: 10,
        min_count: 1,
        max_count: 5,
        enchant_randomly: false,
    },
    PoolEntry::Item {
        name: "minecraft:gold_ingot",
        weight: 5,
        min_count: 1,
        max_count: 3,
        enchant_randomly: false,
    },
    PoolEntry::Item {
        name: "minecraft:redstone",
        weight: 5,
        min_count: 4,
        max_count: 9,
        enchant_randomly: false,
    },
    PoolEntry::Item {
        name: "minecraft:lapis_lazuli",
        weight: 5,
        min_count: 4,
        max_count: 9,
        enchant_randomly: false,
    },
    PoolEntry::Item {
        name: "minecraft:diamond",
        weight: 3,
        min_count: 1,
        max_count: 2,
        enchant_randomly: false,
    },
    PoolEntry::Item {
        name: "minecraft:coal",
        weight: 10,
        min_count: 3,
        max_count: 8,
        enchant_randomly: false,
    },
    PoolEntry::Item {
        name: "minecraft:bread",
        weight: 15,
        min_count: 1,
        max_count: 3,
        enchant_randomly: false,
    },
    PoolEntry::Item {
        name: "minecraft:glow_berries",
        weight: 15,
        min_count: 3,
        max_count: 6,
        enchant_randomly: false,
    },
    PoolEntry::Item {
        name: "minecraft:melon_seeds",
        weight: 10,
        min_count: 2,
        max_count: 4,
        enchant_randomly: false,
    },
    PoolEntry::Item {
        name: "minecraft:pumpkin_seeds",
        weight: 10,
        min_count: 2,
        max_count: 4,
        enchant_randomly: false,
    },
    PoolEntry::Item {
        name: "minecraft:beetroot_seeds",
        weight: 10,
        min_count: 2,
        max_count: 4,
        enchant_randomly: false,
    },
];

const POOL_2: &[PoolEntry] = &[
    PoolEntry::Item {
        name: "minecraft:rail",
        weight: 20,
        min_count: 4,
        max_count: 8,
        enchant_randomly: false,
    },
    PoolEntry::Item {
        name: "minecraft:powered_rail",
        weight: 5,
        min_count: 1,
        max_count: 4,
        enchant_randomly: false,
    },
    PoolEntry::Item {
        name: "minecraft:detector_rail",
        weight: 5,
        min_count: 1,
        max_count: 4,
        enchant_randomly: false,
    },
    PoolEntry::Item {
        name: "minecraft:activator_rail",
        weight: 5,
        min_count: 1,
        max_count: 4,
        enchant_randomly: false,
    },
    PoolEntry::Item {
        name: "minecraft:torch",
        weight: 15,
        min_count: 1,
        max_count: 16,
        enchant_randomly: false,
    },
];

fn roll_pool(entries: &[PoolEntry], rolls: i32, rng: &mut Lcg, out: &mut Vec<Stack>) {
    let total_weight: i32 = entries
        .iter()
        .map(|e| match e {
            PoolEntry::Item { weight, .. } | PoolEntry::Empty { weight } => *weight,
        })
        .sum();
    for _ in 0..rolls {
        let mut pick = rng.next_bound(total_weight);
        for entry in entries {
            let weight = match entry {
                PoolEntry::Item { weight, .. } | PoolEntry::Empty { weight } => *weight,
            };
            pick -= weight;
            if pick < 0 {
                if let PoolEntry::Item {
                    name,
                    min_count,
                    max_count,
                    enchant_randomly,
                    ..
                } = entry
                {
                    let count = if *min_count == 1 && *max_count == 1 {
                        1
                    } else {
                        mth_next_int(rng, *min_count, *max_count)
                    };
                    let mut item = *name;
                    let mut enchantment = None;
                    if *enchant_randomly {
                        let (ench_name, max_lvl) =
                            ON_RANDOM_LOOT[rng.next_bound(ON_RANDOM_LOOT.len() as i32) as usize];
                        let lvl = mth_next_int(rng, 1, max_lvl);
                        item = "minecraft:enchanted_book";
                        enchantment = Some((ench_name, lvl));
                    }
                    out.push(Stack {
                        item,
                        count,
                        enchantment,
                    });
                }
                break;
            }
        }
    }
}

fn shuffle_and_split_items(items: Vec<Stack>, empty_slots_count: usize, rng: &mut Lcg) -> Vec<Stack> {
    let mut keep = Vec::new();
    let mut splitable = Vec::new();
    for item in items {
        if item.count <= 0 {
            continue;
        }
        if item.count > 1 {
            splitable.push(item);
        } else {
            keep.push(item);
        }
    }
    let mut available = empty_slots_count as isize - keep.len() as isize - splitable.len() as isize;
    while available > 0 && !splitable.is_empty() {
        available -= 1;
        let idx = mth_next_int(rng, 0, splitable.len() as i32 - 1) as usize;
        let mut stack = splitable.remove(idx);
        let take = mth_next_int(rng, 1, stack.count / 2);
        stack.count -= take;
        let split = Stack {
            item: stack.item,
            count: take,
            enchantment: stack.enchantment,
        };
        for part in [stack, split] {
            if part.count > 1 && rng.next_bool() {
                splitable.push(part);
            } else {
                keep.push(part);
            }
        }
    }
    keep.extend(splitable);
    util_shuffle(&mut keep, rng);
    keep
}

/// A datapack override for the mineshaft table uses the same complete JSON
/// evaluator as generated chest loot. The legacy entry type cannot carry
/// arbitrary component patches (the caller historically supported only stored
/// enchantments), but weights, roll providers, nested entries, splitting and
/// slot placement all follow the overriding document instead of bypassing it.
fn datapack_abandoned_mineshaft(seed: i64) -> Option<Vec<LootEntry>> {
    let json = crate::loot_table_json("minecraft:chests/abandoned_mineshaft")?;
    let table = jparse(json.as_bytes())?;
    let resolvers = LootResolvers {
        tags: &vanilla_loot_tags,
        smelt: &vanilla_loot_smelt,
        max_damage: &vanilla_loot_max_damage,
    };
    let mut rng = Lcg::new(catalogue_rng_seed(seed));
    let items = evaluate_loot_table(&table, &LootContext::default(), &mut rng, &resolvers);
    Some(loot_items_to_container_entries(items, &mut rng, true))
}

pub(crate) fn abandoned_mineshaft(seed: i64) -> Vec<LootEntry> {
    if let Some(entries) = datapack_abandoned_mineshaft(seed) {
        return entries;
    }
    let mut rng = Lcg::new(seed as u64);
    let mut items = Vec::new();
    // Pool 0: constant 1 roll, bonus_rolls 0.0
    roll_pool(POOL_0, 1, &mut rng, &mut items);
    // Pool 1: uniform(2, 4) rolls, bonus_rolls 0.0
    let rolls_1 = mth_next_int(&mut rng, 2, 4);
    roll_pool(POOL_1, rolls_1, &mut rng, &mut items);
    // Pool 2: constant 3 rolls, bonus_rolls 0.0
    roll_pool(POOL_2, 3, &mut rng, &mut items);

    let mut slots: Vec<i16> = (0..27).collect();
    util_shuffle(&mut slots, &mut rng);
    let split = shuffle_and_split_items(items, slots.len(), &mut rng);

    let mut out = Vec::with_capacity(split.len());
    for item in split {
        let Some(slot) = slots.pop() else {
            break;
        };
        out.push(LootEntry {
            slot,
            item: item.item,
            count: item.count as u8,
            enchantment: item.enchantment,
        });
    }
    out.sort_by_key(|e| e.slot);
    out
}

// ---------------------------------------------------------------------------
// Generic vanilla 1.21.1 loot-table evaluation.
//
// Each rule below is taken from the fixed server jar's bytecode (`javap -p -c`
// on `META-INF/versions/1.21.1/server-1.21.1.jar`):
//   `erv`  LootPool: rolls + floor(bonus_rolls * luck) draws, weighted pick
//          with effective weight = weight + floor(quality * luck).
//   `ery`  Alternatives: first child whose conditions pass wins.
//   `esl`  Sequence: children in order; stop at the first failure.
//   `esd`  Group: every child that passes.
//   `esm`  TagEntry: expand=true emits every tag item, false one random item.
//   `evi`  RandomChanceWithEnchantedBonus: level>0 uses the linear provider
//          base + per_level_above_first * (level - 1), else the flat chance.
//   `esw`  EnchantedCountIncrease: grow by round(level * count), optional limit.
//   `etw`  SetCount: (add ? current_count : 0) + number provider.
//   `etx`  SetDamage: floor((1 - clamp(provider + add, 0, 1)) * max_damage).
//   `eug`  FurnaceSmelt: smelted result keeping the original stack count.
//   `eso`  ApplyBonus: ore_drops / binomial_with_bonus_count / uniform_bonus_count.
//   `esx`  ExplosionDecay: per item of the stack, keep iff nextFloat <= 1/radius.
//   `esy`  LimitCount: clamp the count into the provider's [min, max].
//   `esz`  CopyState / CopyBlockState: named block properties of the looted
//          block, skipped when the state does not carry them.
//   SurvivesExplosion / BlockStateProperty / MatchTool / TableBonus follow the
//   same 1/radius, block-state, item-predicate and `chances[min(level, len-1)]`
//   rules their bytecode spells out.
// ---------------------------------------------------------------------------

/// Registries the pure evaluator cannot know on its own. Callers supply
/// callbacks so this module stays independent of runtime item/tag registries.
pub(crate) struct LootResolvers<'a> {
    /// `minecraft:tag`: tag id -> item ids contained in the tag.
    pub tags: &'a dyn Fn(&str) -> Vec<&'static str>,
    /// `minecraft:furnace_smelt`: item id -> smelted item id, if smeltable.
    pub smelt: &'a dyn Fn(&str) -> Option<&'static str>,
    /// `minecraft:set_damage`: item id -> maximum damage (durability).
    pub max_damage: &'a dyn Fn(&str) -> i32,
}

/// Non-RNG inputs to condition and function evaluation.
#[derive(Clone, Debug, Default)]
pub(crate) struct LootContext {
    /// `minecraft:killed_by_player`.
    pub killed_by_player: bool,
    /// Enchantment levels on the tool/attacker used for this roll.
    pub enchantments: BTreeMap<String, i32>,
    /// Luck used by pool `bonus_rolls` and entry `quality`.
    pub luck: f32,
    /// `minecraft:entity_properties` flags for the looted entity itself.
    pub this_on_fire: bool,
    pub this_is_baby: bool,
    /// `minecraft:entity_properties` flags for the attacker/killer entity.
    pub attacker_on_fire: bool,
    pub attacker_is_baby: bool,
    /// Entity-type predicates: `this` is the looted entity, `attacker` /
    /// `direct_attacker` the entity that damaged or killed it. `None` means
    /// the caller cannot prove a type, so type predicates fail closed.
    pub this_entity_type: Option<String>,
    pub attacker_entity_type: Option<String>,
    /// `type_specific.size` for slime-like entities.
    pub this_entity_size: Option<i32>,
    /// `type_specific.in_open_water` for `minecraft:fishing_hook`.
    pub this_in_open_water: bool,
    /// `type_specific.is_captain` for `minecraft:raider`.
    pub this_is_captain: bool,
    /// `damage_source_properties` lightning tag (`minecraft:is_lightning`).
    pub damage_source_is_lightning: bool,
    /// `location_check` biome predicate input; `None` fails closed.
    pub biome: Option<String>,
    /// `minecraft:block_state_property` / `minecraft:copy_state`: the looted
    /// block, when the caller can name it, plus the state properties it reads.
    /// An absent id fails those conditions closed instead of guessing.
    pub block_id: Option<String>,
    pub block_properties: BTreeMap<String, String>,
    /// `LootContextParams.EXPLOSION_RADIUS`: `None` outside an explosion, which
    /// is what makes `survives_explosion` and `explosion_decay` inert.
    pub explosion_radius: Option<f32>,
    /// `minecraft:match_tool`: the tool/item the roll was made with.
    pub tool_item: Option<String>,
    /// `minecraft:copy_components`: components (`id -> JSON value`) the looted
    /// block entity carries. Populated by the container/fill layer; an empty
    /// map copies nothing.
    pub source_components: BTreeMap<String, String>,
}

impl LootContext {
    /// Accept both `looting` and `minecraft:looting` spellings in the map
    /// and in the queried id.
    pub(crate) fn enchantment_level(&self, id: &str) -> i32 {
        let bare = id.strip_prefix("minecraft:").unwrap_or(id);
        self.enchantments
            .get(id)
            .or_else(|| self.enchantments.get(bare))
            .copied()
            .unwrap_or(0)
    }
}

/// One stack produced by the generic evaluator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LootItem {
    pub item: String,
    pub count: i32,
    pub enchantment: Option<(String, i32)>,
    pub damage: i32,
    /// `set_potion` / `set_stew_effect` / `set_ominous_bottle_amplifier`
    /// payload: the potion/effect id the stack carries, when one was rolled.
    pub potion: Option<String>,
    /// `exploration_map` parameters. The map id and target coordinates are
    /// container/player-context dependent, so the evaluator carries the recipe
    /// of the map and the fill layer materializes the real map record.
    pub exploration_map: Option<ExplorationMapLoot>,
    /// Component patch (`id -> compact JSON value`) applied by `set_name`,
    /// `set_components`, `copy_components`, `copy_state` and `set_instrument`.
    /// A `minecraft:max_stack_size` patch also caps the count this module can
    /// emit, since the legacy container entry type only models durability and
    /// stored enchantments.
    pub components: BTreeMap<String, String>,
}

/// Parameters carried by the vanilla `exploration_map` loot function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExplorationMapLoot {
    pub destination: String,
    pub decoration: String,
    pub zoom: i32,
}

impl LootItem {
    fn new(item: &str) -> Self {
        LootItem {
            item: item.to_string(),
            count: 1,
            enchantment: None,
            damage: 0,
            potion: None,
            exploration_map: None,
            components: BTreeMap::new(),
        }
    }
}

/// Evaluate a `minecraft:linear` number provider at an enchantment level:
/// `base + per_level_above_first * (level - 1)` (`daj$e.a(int)`).
fn linear_f32(j: &J, level: i32) -> f32 {
    let base = jnum(j, "base").unwrap_or(0.0) as f32;
    let per = jnum(j, "per_level_above_first").unwrap_or(0.0) as f32;
    base + per * (level - 1) as f32
}

/// Float number provider: a bare number, `minecraft:uniform`, or
/// `minecraft:linear` (which is level-aware).
fn number_f32(j: Option<&J>, rng: &mut Lcg, level: i32) -> f32 {
    let Some(j) = j else { return 0.0 };
    match j {
        J::Num(n) => *n as f32,
        J::Obj(_) => match jstr(j, "type").unwrap_or_default().as_str() {
            "minecraft:constant" => jnum(j, "value").unwrap_or(0.0) as f32,
            "minecraft:uniform" => {
                let min = jnum(j, "min").unwrap_or(0.0) as f32;
                let max = jnum(j, "max").unwrap_or(0.0) as f32;
                min + (max - min) * rng.next_f32()
            }
            "minecraft:binomial" => number_binomial_i32(j, rng, level) as f32,
            "minecraft:linear" => linear_f32(j, level),
            _ => 0.0,
        },
        _ => 0.0,
    }
}


/// `minecraft:binomial`: `n` independent Bernoulli trials at probability `p`.
/// Both parameters are number providers in vanilla, so their draws remain in
/// the same continuous stream before the trial draws.
fn number_binomial_i32(j: &J, rng: &mut Lcg, level: i32) -> i32 {
    let n = number_i32(j.get("n"), rng, level).max(0);
    let p = number_f32(j.get("p"), rng, level).clamp(0.0, 1.0);
    let mut successes = 0;
    for _ in 0..n {
        if rng.next_f32() < p {
            successes += 1;
        }
    }
    successes
}

/// Integer number provider. Integral uniform bounds use the same
/// `nextInt(max - min + 1) + min` draw as `Mth.nextInt`.
fn number_i32(j: Option<&J>, rng: &mut Lcg, level: i32) -> i32 {
    let Some(j) = j else { return 0 };
    match j {
        J::Num(n) => *n as i32,
        J::Obj(_) => match jstr(j, "type").unwrap_or_default().as_str() {
            "minecraft:constant" => jnum(j, "value").unwrap_or(0.0) as i32,
            "minecraft:uniform" => {
                let min = jnum(j, "min").unwrap_or(0.0);
                let max = jnum(j, "max").unwrap_or(0.0);
                if min.fract() == 0.0 && max.fract() == 0.0 {
                    mth_next_int(rng, min as i32, max as i32)
                } else {
                    number_f32(Some(j), rng, level).floor() as i32
                }
            }
            "minecraft:binomial" => number_binomial_i32(j, rng, level),
            "minecraft:linear" => linear_f32(j, level).floor() as i32,
            _ => 0,
        },
        _ => 0,
    }
}

/// A predicate flag absent from the JSON is not checked; when present the
/// entity's flag must equal it (including an explicit `false`).
fn flag_matches(flags: &J, key: &str, actual: bool) -> bool {
    match flags.get(key) {
        None => true,
        Some(J::Num(n)) => (*n != 0.0) == actual,
        Some(J::Str(s)) => (s == "true") == actual,
        _ => true,
    }
}

/// `#minecraft:skeletons` entity-type tag, the only entity tag the 1.21.1
/// loot tables query through `entity_properties`.
const ENTITY_TYPE_SKELETONS: &[&str] = &[
    "minecraft:skeleton",
    "minecraft:stray",
    "minecraft:wither_skeleton",
    "minecraft:bogged",
];

/// Enchantment tag `#minecraft:smelts_loot` (fire aspect) used by the
/// `equipment.mainhand` predicate on cooked-drop functions.
const ENCHANTMENT_SMELTS_LOOT: &[&str] = &["minecraft:fire_aspect", "fire_aspect"];

/// Enchantment id sets the 1.21.1 tables name by tag instead of by id. This is
/// a registry lookup the evaluator cannot perform, so the tag members the
/// vanilla tables query are carried as id lists — the same shape
/// `#minecraft:skeletons` uses for entity types.
fn enchantment_tag_members(tag: &str) -> &'static [&'static str] {
    match tag {
        "minecraft:smelts_loot" => ENCHANTMENT_SMELTS_LOOT,
        _ => &[],
    }
}

/// Instrument tags `set_instrument` can name. Goat horns are the only vanilla
/// loot item carrying an instrument component, and both tag halves are
/// declared by `data/minecraft/tags/instrument/*.json`.
const REGULAR_GOAT_HORNS: &[&str] = &[
    "minecraft:ponder_goat_horn",
    "minecraft:sing_goat_horn",
    "minecraft:seek_goat_horn",
    "minecraft:feel_goat_horn",
];
const SCREAMING_GOAT_HORNS: &[&str] = &[
    "minecraft:admire_goat_horn",
    "minecraft:call_goat_horn",
    "minecraft:yearn_goat_horn",
    "minecraft:dream_goat_horn",
];

fn instrument_tag_members(tag: &str) -> &'static [&'static str] {
    match tag {
        "minecraft:regular_goat_horns" => REGULAR_GOAT_HORNS,
        "minecraft:screaming_goat_horns" => SCREAMING_GOAT_HORNS,
        _ => &[],
    }
}

/// Expand an `ItemPredicate`/`HolderSet` style value — one id, one `#tag`, or a
/// list of either — into the ids it names.
fn id_set_members(value: &J, tag_members: fn(&str) -> &'static [&'static str]) -> Vec<String> {
    fn extend(value: &J, tag_members: fn(&str) -> &'static [&'static str], out: &mut Vec<String>) {
        match value {
            J::Str(id) => match id.strip_prefix('#') {
                Some(tag) => out.extend(tag_members(tag).iter().map(|id| (*id).to_string())),
                None => out.push(id.clone()),
            },
            J::Arr(items) => items.iter().for_each(|item| extend(item, tag_members, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    extend(value, tag_members, &mut out);
    out
}

/// `MinMaxBounds.Ints`-style bounds: a bare value, or `{min,max}` where either
/// side may be absent. `default_min` is the lower bound used when `min` is
/// missing, which is what keeps an absent `levels` from matching level 0 for a
/// predicate that only means "has this enchantment".
fn int_bounds_hold(bounds: Option<&J>, actual: i32, default_min: i32) -> bool {
    match bounds {
        None => actual >= default_min,
        Some(J::Num(n)) => actual == *n as i32,
        Some(bounds) => {
            let min = jnum(bounds, "min").map_or(default_min, |v| v as i32);
            let max = jnum(bounds, "max").map_or(i32::MAX, |v| v as i32);
            actual >= min && actual <= max
        }
    }
}

/// Match an entity `type` predicate value: a plain id or a `#`-prefixed tag.
/// Unknown tags and absent context fail closed.
fn entity_type_matches(expected: &str, actual: Option<&str>) -> bool {
    let Some(actual) = actual else {
        return false;
    };
    if let Some(tag) = expected.strip_prefix('#') {
        let members: &[&str] = match tag {
            "minecraft:skeletons" => ENTITY_TYPE_SKELETONS,
            _ => return false,
        };
        members.contains(&actual)
    } else {
        expected == actual
    }
}

/// `predicate.type_specific`: slime `size`, fishing-hook `in_open_water` and
/// raider `is_captain` are the only type-specific keys the 1.21.1 tables use.
fn type_specific_matches(specific: &J, ctx: &LootContext, this_type: Option<&str>) -> bool {
    if let Some(expected) = specific.get("type") {
        if !entity_type_matches(expected.str(), this_type) {
            return false;
        }
    }
    if let Some(size) = specific.get("size") {
        let Some(actual) = ctx.this_entity_size else {
            return false;
        };
        let ok = match size {
            J::Num(n) => actual == *n as i32,
            J::Obj(_) => {
                let min = jint(size, "min", i32::MIN);
                let max = jint(size, "max", i32::MAX);
                actual >= min && actual <= max
            }
            _ => false,
        };
        if !ok {
            return false;
        }
    }
    if let Some(want) = specific.get("in_open_water") {
        let want = match want {
            J::Num(n) => *n != 0.0,
            J::Str(s) => s == "true",
            _ => false,
        };
        if ctx.this_in_open_water != want {
            return false;
        }
    }
    if let Some(want) = specific.get("is_captain") {
        let want = match want {
            J::Num(n) => *n != 0.0,
            J::Str(s) => s == "true",
            _ => false,
        };
        if ctx.this_is_captain != want {
            return false;
        }
    }
    true
}

/// `predicate.equipment.mainhand`: the attacker holds an item whose
/// enchantments satisfy the nested item predicate. Only the
/// `minecraft:enchantments` tag form used by `#minecraft:smelts_loot` is
/// evaluated; anything else fails closed.
fn equipment_matches(equipment: &J, ctx: &LootContext) -> bool {
    let Some(mainhand) = equipment.get("mainhand") else {
        return false;
    };
    let Some(predicates) = mainhand.get("predicates") else {
        return true;
    };
    for (key, expected) in match predicates {
        J::Obj(fields) => fields.iter(),
        _ => return false,
    } {
        if key.as_str() != "minecraft:enchantments" {
            return false;
        }
        let Some(list) = expected.as_arr() else {
            return false;
        };
        for entry in list {
            let Some(wanted) = entry.get("enchantments") else {
                continue;
            };
            let Some(tag) = wanted.str().strip_prefix('#') else {
                return false;
            };
            let members = enchantment_tag_members(tag);
            if members.is_empty() {
                return false;
            }
            let has = ctx
                .enchantments
                .keys()
                .any(|held| members.contains(&held.as_str()));
            if !has {
                return false;
            }
        }
    }
    true
}

/// `minecraft:entity_properties`: evaluates the `flags`, `type`,
/// `type_specific` and `equipment` predicates the 1.21.1 tables use. `entity`
/// selects `this` vs the attacker (`attacker`/`direct_attacker`); predicate
/// keys without context fail closed.
fn entity_properties(condition: &J, ctx: &LootContext) -> bool {
    let entity = jstr(condition, "entity").unwrap_or_else(|| "this".to_string());
    let Some(predicate) = condition.get("predicate") else {
        return false;
    };
    let this_side = entity == "this";
    if let Some(flags) = predicate.get("flags") {
        let (on_fire, is_baby) = if this_side {
            (ctx.this_on_fire, ctx.this_is_baby)
        } else {
            (ctx.attacker_on_fire, ctx.attacker_is_baby)
        };
        if !(flag_matches(flags, "is_on_fire", on_fire)
            && flag_matches(flags, "is_baby", is_baby))
        {
            return false;
        }
    }
    if let Some(expected) = predicate.get("type") {
        let actual = if this_side {
            ctx.this_entity_type.as_deref()
        } else {
            ctx.attacker_entity_type.as_deref()
        };
        if !entity_type_matches(expected.str(), actual) {
            return false;
        }
    }
    if let Some(specific) = predicate.get("type_specific") {
        if !type_specific_matches(specific, ctx, ctx.this_entity_type.as_deref()) {
            return false;
        }
    }
    if let Some(equipment) = predicate.get("equipment") {
        if !equipment_matches(equipment, ctx) {
            return false;
        }
    }
    true
}

/// `minecraft:damage_source_properties`: the 1.21.1 tables only query the
/// `minecraft:is_lightning` damage-source tag (turtle defeat) and frog
/// `source_entity` predicates (slime/magma cube). Source-entity identity has
/// no context here, so those predicates fail closed.
fn damage_source_properties(condition: &J, ctx: &LootContext) -> bool {
    let Some(predicate) = condition.get("predicate") else {
        return false;
    };
    if predicate.get("source_entity").is_some() {
        return false;
    }
    for tag in predicate
        .get("tags")
        .and_then(J::as_arr)
        .map(|v| v.as_slice())
        .unwrap_or(&[])
    {
        if tag.get("id").map(J::str) == Some("minecraft:is_lightning") {
            let expected = match tag.get("expected") {
                Some(J::Num(n)) => *n != 0.0,
                Some(J::Str(s)) => s == "true",
                _ => true,
            };
            if ctx.damage_source_is_lightning != expected {
                return false;
            }
        }
    }
    true
}

/// `minecraft:location_check`: only the `biomes` list is used (fishing junk
/// bamboo in jungle biomes). Without a biome in context it fails closed.
fn location_check(condition: &J, ctx: &LootContext) -> bool {
    let Some(predicate) = condition.get("predicate") else {
        return false;
    };
    let Some(biomes) = predicate.get("biomes").and_then(J::as_arr) else {
        return true;
    };
    let Some(biome) = ctx.biome.as_deref() else {
        return false;
    };
    biomes.iter().any(|entry| entry.str() == biome)
}

/// `minecraft:survives_explosion`: outside an explosion (`EXPLOSION_RADIUS`
/// absent) the drop always survives; inside one it survives at `1 / radius`
/// odds, which is the ratio `SurvivesExplosionCondition` draws. Java float
/// division is used directly so a zero radius behaves like `1.0F / 0.0F`.
fn survives_explosion(ctx: &LootContext, rng: &mut Lcg) -> bool {
    match ctx.explosion_radius {
        None => true,
        Some(radius) => rng.next_f32() <= 1.0 / radius,
    }
}

/// One `block_state_property`/`copy_state` style value test: an exact value or
/// the list of values the property accepts.
fn state_value_matches(allowed: &J, actual: &str) -> bool {
    match allowed {
        J::Str(expected) => expected == actual,
        J::Num(n) => json_number_text(*n) == actual,
        J::Arr(list) => list.iter().any(|value| state_value_matches(value, actual)),
        _ => false,
    }
}

/// `minecraft:block_state_property`: the looted block must be `block`, and
/// every listed property must equal one of its permitted values. The vanilla
/// codec requires `block`, so a context that cannot name the block fails closed
/// rather than claiming the property holds.
fn block_state_property(condition: &J, ctx: &LootContext) -> bool {
    let Some(block) = jstr(condition, "block") else {
        return false;
    };
    if ctx.block_id.as_deref() != Some(block.as_str()) {
        return false;
    }
    let Some(properties) = condition.get("properties") else {
        return true;
    };
    let J::Obj(entries) = properties else {
        return false;
    };
    entries.iter().all(|(name, allowed)| {
        ctx.block_properties
            .get(name)
            .is_some_and(|actual| state_value_matches(allowed, actual))
    })
}

/// Item-set test for an `ItemPredicate.items` value: an id, a `#tag` expanded
/// through the caller's item-tag resolver, or a list of either.
fn item_set_matches(items: &J, tool: &str, res: &LootResolvers<'_>) -> bool {
    match items {
        J::Str(id) => match id.strip_prefix('#') {
            Some(tag) => (res.tags)(tag).contains(&tool),
            None => id == tool,
        },
        J::Arr(list) => list
            .iter()
            .any(|value| item_set_matches(value, tool, res)),
        _ => false,
    }
}

/// `predicates.minecraft:enchantments`: every listed entry must be satisfied by
/// an enchantment the tool actually carries, with the entry's level bounds
/// applied (`min` absent still means "present", as an absent entry in
/// `ItemEnchantments` cannot match).
fn enchantments_predicate(expected: &J, ctx: &LootContext) -> bool {
    let Some(entries) = expected.as_arr() else {
        return false;
    };
    entries.iter().all(|entry| {
        let Some(wanted) = entry.get("enchantments") else {
            return false;
        };
        let members = id_set_members(wanted, enchantment_tag_members);
        if members.is_empty() {
            return false;
        }
        let levels = entry.get("levels");
        members
            .iter()
            .any(|id| int_bounds_hold(levels, ctx.enchantment_level(id), 1))
    })
}

/// `minecraft:match_tool`: the tool in context must satisfy the item predicate.
/// `items`, `count` (a tool is a single stack in this model) and the
/// `minecraft:enchantments` sub-predicate are evaluated; other sub-predicates
/// need item context this module does not carry and fail closed.
fn match_tool(condition: &J, ctx: &LootContext, res: &LootResolvers<'_>) -> bool {
    let Some(predicate) = condition.get("predicate") else {
        return false;
    };
    if let Some(items) = predicate.get("items") {
        let Some(tool) = ctx.tool_item.as_deref() else {
            return false;
        };
        if !item_set_matches(items, tool, res) {
            return false;
        }
    }
    if let Some(count) = predicate.get("count") {
        if !int_bounds_hold(Some(count), 1, 0) {
            return false;
        }
    }
    let Some(predicates) = predicate.get("predicates") else {
        return true;
    };
    let J::Obj(fields) = predicates else {
        return false;
    };
    fields.iter().all(|(id, expected)| {
        if id == "minecraft:enchantments" {
            enchantments_predicate(expected, ctx)
        } else {
            false
        }
    })
}

/// `minecraft:table_bonus`: the enchantment level indexes `chances` (clamped to
/// the last entry) and one draw decides the drop.
fn table_bonus(condition: &J, ctx: &LootContext, rng: &mut Lcg) -> bool {
    let Some(chances) = jarr(condition, "chances").filter(|list| !list.is_empty()) else {
        return false;
    };
    let level = ctx
        .enchantment_level(&jstr(condition, "enchantment").unwrap_or_default())
        .max(0) as usize;
    let chance = chances[level.min(chances.len() - 1)].num() as f32;
    rng.next_f32() < chance
}

// ---------------------------------------------------------------------------
// Modded loot conditions and functions — a pluggable table instead of a silent
// `_ => false` / `_ => {}`.
// ---------------------------------------------------------------------------

/// A loot condition a mod defines (`mymod:is_cursed`): the parsed condition, the
/// evaluation context and the roll's RNG. `true` keeps the entry.
pub(crate) type CustomLootCondition =
    std::sync::Arc<dyn Fn(&J, &LootContext, &mut Lcg) -> bool + Send + Sync>;

/// A loot function a mod defines (`mymod:double_drop`): the parsed function, the
/// stack it may rewrite, the context and the roll's RNG.
pub(crate) type CustomLootFunction =
    std::sync::Arc<dyn Fn(&J, &mut LootItem, &LootContext, &mut Lcg) + Send + Sync>;

/// Bound on the registered handlers, conditions and functions together.
const MAX_CUSTOM_LOOT_HANDLERS: usize = 256;
/// Bound on the distinct unregistered type names retained for observation.
const MAX_OBSERVED_LOOT_TYPES: usize = 512;

#[derive(Default)]
struct CustomLootHandlers {
    conditions: HashMap<String, CustomLootCondition>,
    functions: HashMap<String, CustomLootFunction>,
    /// Type ids a loot table named that no handler answers, in first-seen order.
    unregistered: Vec<String>,
}

impl CustomLootHandlers {
    fn handler_count(&self) -> usize {
        self.conditions.len() + self.functions.len()
    }

    fn remember_unregistered(&mut self, id: &str) {
        if self.unregistered.len() >= MAX_OBSERVED_LOOT_TYPES
            || self.unregistered.iter().any(|held| held == id)
        {
            return;
        }
        self.unregistered.push(id.to_owned());
    }
}

static CUSTOM_LOOT_HANDLERS: OnceLock<Mutex<CustomLootHandlers>> = OnceLock::new();
fn custom_loot_handlers() -> &'static Mutex<CustomLootHandlers> {
    CUSTOM_LOOT_HANDLERS.get_or_init(|| Mutex::new(CustomLootHandlers::default()))
}

/// Live handler count, mirrored outside the registry lock so the vanilla path skips
/// the lock entirely with one relaxed load.
static CUSTOM_LOOT_HANDLER_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// The registry key for a loot type id: a bare id is the `minecraft` namespace, so a
/// handler registered as `random_chance` and a table writing `minecraft:random_chance`
/// meet in one slot.
fn custom_loot_type_id(id: &str) -> String {
    let id = id.trim();
    if id.contains(':') {
        id.to_owned()
    } else {
        format!("minecraft:{id}")
    }
}

/// Register (or replace) a mod's loot-condition handler. `Err` names why the
/// registry refused it: an empty id, or the table already holding
/// `MAX_CUSTOM_LOOT_HANDLERS`.
pub(crate) fn register_custom_loot_condition(
    id: &str,
    handler: CustomLootCondition,
) -> Result<(), String> {
    let id = custom_loot_type_id(id);
    if id == "minecraft:" {
        return Err("custom loot condition id is empty".to_string());
    }
    let mut handlers = custom_loot_handlers()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !handlers.conditions.contains_key(&id) && handlers.handler_count() >= MAX_CUSTOM_LOOT_HANDLERS {
        return Err(format!("custom loot handler table is full; refusing '{id}'"));
    }
    handlers.conditions.insert(id, handler);
    CUSTOM_LOOT_HANDLER_COUNT.store(handlers.handler_count(), std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// Register (or replace) a mod's loot-function handler.
pub(crate) fn register_custom_loot_function(
    id: &str,
    handler: CustomLootFunction,
) -> Result<(), String> {
    let id = custom_loot_type_id(id);
    if id == "minecraft:" {
        return Err("custom loot function id is empty".to_string());
    }
    let mut handlers = custom_loot_handlers()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !handlers.functions.contains_key(&id) && handlers.handler_count() >= MAX_CUSTOM_LOOT_HANDLERS {
        return Err(format!("custom loot handler table is full; refusing '{id}'"));
    }
    handlers.functions.insert(id, handler);
    CUSTOM_LOOT_HANDLER_COUNT.store(handlers.handler_count(), std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// Drop every handler registered under `id`; `true` when one existed.
pub(crate) fn unregister_custom_loot_handlers(id: &str) -> bool {
    let id = custom_loot_type_id(id);
    let mut handlers = custom_loot_handlers()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let condition = handlers.conditions.remove(&id).is_some();
    let function = handlers.functions.remove(&id).is_some();
    CUSTOM_LOOT_HANDLER_COUNT.store(handlers.handler_count(), std::sync::atomic::Ordering::Relaxed);
    condition || function
}

/// The type ids loot tables named that no handler answers, in first-seen order: an
/// unregistered mod condition or function is observable here instead of failing
/// closed with no trace of the type a consumer is missing.
pub(crate) fn unregistered_custom_loot_types() -> Vec<String> {
    custom_loot_handlers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .unregistered
        .clone()
}

// A mod loader owns the registration side, so nothing in this crate calls it.
// Pinning the entry points keeps the published surface type-checked.
const _: fn(&str, CustomLootCondition) -> Result<(), String> = register_custom_loot_condition;
const _: fn(&str, CustomLootFunction) -> Result<(), String> = register_custom_loot_function;
const _: fn(&str) -> bool = unregister_custom_loot_handlers;
const _: fn() -> Vec<String> = unregistered_custom_loot_types;

/// The registered handler for one condition id. The fast path is a single relaxed
/// load: with nothing registered the table is never locked.
fn custom_loot_condition(id: &str) -> Option<CustomLootCondition> {
    if CUSTOM_LOOT_HANDLER_COUNT.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        return None;
    }
    custom_loot_handlers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .conditions
        .get(id)
        .cloned()
}

/// The registered handler for one function id, behind the same count gate.
fn custom_loot_function(id: &str) -> Option<CustomLootFunction> {
    if CUSTOM_LOOT_HANDLER_COUNT.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        return None;
    }
    custom_loot_handlers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .functions
        .get(id)
        .cloned()
}

/// Record one type id no handler answered. Only the fail-closed path calls this, so a
/// vanilla table never pays for it.
fn remember_unregistered_loot_type(id: &str) {
    custom_loot_handlers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remember_unregistered(id);
}

/// Evaluate one loot condition. Unknown conditions consult the mod table, which fails
/// closed (and records the type) when no handler answers.
fn condition_holds(
    condition: &J,
    ctx: &LootContext,
    rng: &mut Lcg,
    res: &LootResolvers<'_>,
) -> bool {
    let id = jstr(condition, "condition").unwrap_or_default();
    match id.trim_start_matches("minecraft:") {
        "inverted" => !condition
            .get("term")
            .map(|t| condition_holds(t, ctx, rng, res))
            .unwrap_or(false),
        "any_of" | "alternative" => jarr(condition, "terms")
            .map(|terms| terms.iter().any(|t| condition_holds(t, ctx, rng, res)))
            .unwrap_or(false),
        "all_of" => jarr(condition, "terms")
            .map(|terms| terms.iter().all(|t| condition_holds(t, ctx, rng, res)))
            .unwrap_or(true),
        "random_chance" => rng.next_f32() < jnum(condition, "chance").unwrap_or(0.0) as f32,
        "random_chance_with_enchanted_bonus" => {
            let enchantment = jstr(condition, "enchantment").unwrap_or_default();
            let level = ctx.enchantment_level(&enchantment);
            let chance = if level > 0 {
                condition
                    .get("enchanted_chance")
                    .map(|provider| linear_f32(provider, level))
                    .unwrap_or(0.0)
            } else {
                jnum(condition, "unenchanted_chance").unwrap_or(0.0) as f32
            };
            rng.next_f32() < chance
        }
        "killed_by_player" => ctx.killed_by_player,
        "entity_properties" => entity_properties(condition, ctx),
        "damage_source_properties" => damage_source_properties(condition, ctx),
        "location_check" => location_check(condition, ctx),
        "survives_explosion" => survives_explosion(ctx, rng),
        "block_state_property" => block_state_property(condition, ctx),
        "match_tool" => match_tool(condition, ctx, res),
        "table_bonus" => table_bonus(condition, ctx, rng),
        _ => {
            let key = custom_loot_type_id(&id);
            match custom_loot_condition(&key) {
                Some(handler) => handler(condition, ctx, rng),
                None => {
                    remember_unregistered_loot_type(&key);
                    false
                }
            }
        }
    }
}

/// An absent/empty condition list passes, as in vanilla.
fn conditions_pass(
    conditions: Option<&J>,
    ctx: &LootContext,
    rng: &mut Lcg,
    res: &LootResolvers<'_>,
) -> bool {
    match conditions.and_then(J::as_arr) {
        None => true,
        Some(list) => list.iter().all(|c| condition_holds(c, ctx, rng, res)),
    }
}

/// Enchantment level used by a function's number provider (`apply_bonus` and
/// `enchanted_count_increase` both read the `enchantment` field).
fn function_level(function: &J, ctx: &LootContext) -> i32 {
    jstr(function, "enchantment")
        .map(|id| ctx.enchantment_level(&id))
        .unwrap_or(0)
}

fn apply_one_function(
    item: &mut LootItem,
    function: &J,
    ctx: &LootContext,
    rng: &mut Lcg,
    res: &LootResolvers<'_>,
) {
    let id = jstr(function, "function").unwrap_or_default();
    match id.trim_start_matches("minecraft:") {
        "set_count" => {
            let add = jbool(function, "add", false);
            let count = number_i32(
                function.get("count"),
                rng,
                function_level(function, ctx),
            );
            item.count = if add { item.count + count } else { count };
        }
        "enchanted_count_increase" => {
            let level = function_level(function, ctx);
            if level == 0 {
                return;
            }
            let count = number_f32(function.get("count"), rng, level);
            item.count += (level as f32 * count).round() as i32;
            let limit = jint(function, "limit", 0);
            if limit > 0 {
                item.count = item.count.min(limit);
            }
        }
        "furnace_smelt" => {
            if let Some(result) = (res.smelt)(&item.item) {
                item.item = result.to_string();
            }
        }
        "set_damage" => {
            let max_damage = (res.max_damage)(&item.item);
            if max_damage <= 0 {
                return;
            }
            let add = jbool(function, "add", false);
            let remaining = if add {
                1.0 - item.damage as f32 / max_damage as f32
            } else {
                0.0
            };
            let requested = number_f32(function.get("damage"), rng, 0) + remaining;
            let clamped = requested.clamp(0.0, 1.0);
            item.damage = ((1.0 - clamped) * max_damage as f32).floor() as i32;
        }
        "apply_bonus" => {
            let level = function_level(function, ctx);
            let formula = jstr(function, "formula").unwrap_or_default();
            let parameters = function.get("parameters");
            item.count = match formula.trim_start_matches("minecraft:") {
                "ore_drops" => {
                    if level > 0 {
                        let roll = rng.next_bound(level + 2) - 1;
                        item.count * (roll.max(0) + 1)
                    } else {
                        item.count
                    }
                }
                "binomial_with_bonus_count" => {
                    let extra = parameters.map(|p| jint(p, "extra", 0)).unwrap_or(0);
                    let probability = parameters
                        .map(|p| jnum(p, "probability").unwrap_or(0.0) as f32)
                        .unwrap_or(0.0);
                    let mut count = item.count;
                    for _ in 0..(level + extra) {
                        if rng.next_f32() < probability {
                            count += 1;
                        }
                    }
                    count
                }
                "uniform_bonus_count" => {
                    let multiplier = parameters
                        .map(|p| jint(p, "bonusMultiplier", 0))
                        .unwrap_or(0);
                    item.count + rng.next_bound(level * multiplier + 1)
                }
                _ => item.count,
            };
        }
        "enchant_randomly" => {
            let (name, max) = roll_enchant_option(function.get("options"), rng);
            let level = mth_next_int(rng, 1, max);
            if item.item == "minecraft:book" {
                item.item = "minecraft:enchanted_book".to_string();
            }
            item.enchantment = Some((name, level));
        }
        "enchant_with_levels" => {
            let levels = number_i32(function.get("levels"), rng, 0);
            let (name, max) = roll_enchant_option(function.get("options"), rng);
            let level = mth_next_int(rng, 1, max.min(levels.max(1)));
            if item.item == "minecraft:book" {
                item.item = "minecraft:enchanted_book".to_string();
            }
            item.enchantment = Some((name, level));
        }
        "set_potion" => {
            if let Some(id) = function.get("id") {
                let id = id.str();
                if !id.is_empty() {
                    item.potion = Some(id.to_string());
                }
            }
        }
        "set_stew_effect" => {
            if let Some(effect) = jarr(function, "effects")
                .and_then(|list| list.first())
                .and_then(|entry| entry.get("type"))
            {
                let id = effect.str();
                if !id.is_empty() {
                    item.potion = Some(id.to_string());
                }
            }
        }
        "set_ominous_bottle_amplifier" => {
            let amplifier = number_i32(function.get("amplifier"), rng, 0);
            item.potion = Some(format!("ominous_bottle:{amplifier}"));
        }
        "set_enchantments" => {
            if let Some(J::Obj(fields)) = function.get("enchantments") {
                if let Some((name, level)) = fields.first() {
                    if item.item == "minecraft:book" {
                        item.item = "minecraft:enchanted_book".to_string();
                    }
                    item.enchantment = Some((name.clone(), level.num() as i32));
                }
            }
        }
        "exploration_map" => {
            item.item = "minecraft:filled_map".to_string();
            item.exploration_map = Some(ExplorationMapLoot {
                destination: jstr(function, "destination")
                    .unwrap_or_else(|| "#minecraft:on_treasure_maps".to_string()),
                decoration: jstr(function, "decoration")
                    .unwrap_or_else(|| "minecraft:red_x".to_string()),
                zoom: jint(function, "zoom", 1),
            });
        }
        // `set_name` writes the text component onto the stack's name component:
        // `item_name` for a fixed display name, `custom_name` otherwise (the
        // vanilla default target).
        "set_name" => {
            let Some(name) = function.get("name") else {
                return;
            };
            let component = match jstr(function, "target").unwrap_or_default().as_str() {
                "item_name" => "minecraft:item_name",
                _ => "minecraft:custom_name",
            };
            item.components
                .insert(component.to_string(), render_json_text(name));
        }
        // `set_instrument` resolves `options` (an instrument id, a list, or a
        // goat-horn tag) and picks one member, exactly like
        // `HolderSet.getRandomElement`.
        "set_instrument" => {
            let Some(options) = function.get("options") else {
                return;
            };
            let members = id_set_members(options, instrument_tag_members);
            if members.is_empty() {
                return;
            }
            let picked = &members[rng.next_bound(members.len() as i32) as usize];
            item.components
                .insert("minecraft:instrument".to_string(), picked.clone());
        }
        // `copy_components` copies the block-entity components `include` names
        // (or all of them when `include` is absent), minus `exclude`.
        "copy_components" => {
            if jstr(function, "source").unwrap_or_else(|| "block_entity".to_string())
                != "block_entity"
            {
                return;
            }
            let include = jarr(function, "include")
                .map(|list| list.iter().map(J::str).collect::<Vec<_>>());
            let exclude = jarr(function, "exclude")
                .map(|list| list.iter().map(J::str).collect::<Vec<_>>());
            for (id, value) in &ctx.source_components {
                if include
                    .as_ref()
                    .is_some_and(|list| !list.contains(&id.as_str()))
                {
                    continue;
                }
                if exclude
                    .as_ref()
                    .is_some_and(|list| list.contains(&id.as_str()))
                {
                    continue;
                }
                item.components.insert(id.clone(), value.clone());
            }
        }
        // `explosion_decay` keeps each item of the stack independently at
        // `1 / radius` odds, so a blast scatters a stack instead of voiding it.
        "explosion_decay" => {
            let Some(radius) = ctx.explosion_radius else {
                return;
            };
            if item.count <= 0 {
                return;
            }
            let threshold = 1.0 / radius;
            let mut kept = 0;
            for _ in 0..item.count {
                if rng.next_f32() <= threshold {
                    kept += 1;
                }
            }
            item.count = kept;
        }
        // `set_components` applies a component patch: each entry replaces that
        // component on the stack.
        "set_components" => {
            let Some(J::Obj(fields)) = function.get("components") else {
                return;
            };
            for (id, value) in fields {
                item.components.insert(id.clone(), render_json_text(value));
            }
        }
        // `limit_count` clamps the count into the provider's `[min, max]`; a
        // missing side leaves that end unbounded, as in
        // `LimitCountLootFunction`.
        "limit_count" => {
            let limit = function.get("limit");
            let min = limit
                .and_then(|l| l.get("min"))
                .map(|j| number_i32(Some(j), rng, 0))
                .unwrap_or(0);
            let max = limit
                .and_then(|l| l.get("max"))
                .map(|j| number_i32(Some(j), rng, 0))
                .unwrap_or(i32::MAX);
            item.count = item.count.min(max).max(min);
        }
        // `copy_state` carries the named block properties of the looted block
        // onto the stack. A different block, or a property the state does not
        // have, is skipped rather than defaulted.
        "copy_state" => {
            let Some(block) = jstr(function, "block") else {
                return;
            };
            if ctx.block_id.as_deref() != Some(block.as_str()) {
                return;
            }
            let Some(properties) = jarr(function, "properties") else {
                return;
            };
            let mut state = Vec::new();
            for name in properties {
                let name = name.str();
                if let Some(value) = ctx.block_properties.get(name) {
                    state.push((name.to_string(), J::Str(value.clone())));
                }
            }
            if !state.is_empty() {
                item.components.insert(
                    "minecraft:block_state".to_string(),
                    render_json_text(&J::Obj(state)),
                );
            }
        }
        _ => {
            let key = custom_loot_type_id(&id);
            match custom_loot_function(&key) {
                Some(handler) => handler(function, item, ctx, rng),
                None => remember_unregistered_loot_type(&key),
            }
        }
    }
}

/// Maximum level for an enchantment id used by `enchant_randomly` /
/// `enchant_with_levels`: the `ON_RANDOM_LOOT` table first, then the three
/// treasure/curse ids the 1.21.1 tables name explicitly.
fn enchant_max_level(name: &str) -> i32 {
    let bare = name.strip_prefix("minecraft:").unwrap_or(name);
    if let Some((_, max)) = ON_RANDOM_LOOT
        .iter()
        .find(|(known, _)| known.strip_prefix("minecraft:").unwrap_or(known) == bare)
    {
        return *max;
    }
    match bare {
        "soul_speed" | "swift_sneak" | "wind_burst" => 3,
        _ => 1,
    }
}

/// Pick `(enchantment id, max level)` for `enchant_randomly` /
/// `enchant_with_levels`. `options` is absent (uniform over
/// `#minecraft:on_random_loot`), a `#`-tag, a single id, or a list of ids.
fn roll_enchant_option(options: Option<&J>, rng: &mut Lcg) -> (String, i32) {
    match options {
        None => {
            let (name, max) =
                ON_RANDOM_LOOT[rng.next_bound(ON_RANDOM_LOOT.len() as i32) as usize];
            (name.to_string(), max)
        }
        Some(J::Str(tag)) if tag.starts_with('#') => {
            let (name, max) =
                ON_RANDOM_LOOT[rng.next_bound(ON_RANDOM_LOOT.len() as i32) as usize];
            (name.to_string(), max)
        }
        Some(J::Str(name)) => {
            let id = name.trim_start_matches("minecraft:");
            let full = if name.starts_with("minecraft:") {
                name.clone()
            } else {
                format!("minecraft:{id}")
            };
            let max = enchant_max_level(&full);
            (full, max)
        }
        Some(J::Arr(list)) if !list.is_empty() => {
            let pick = &list[rng.next_bound(list.len() as i32) as usize];
            roll_enchant_option(Some(pick), rng)
        }
        _ => {
            let (name, max) =
                ON_RANDOM_LOOT[rng.next_bound(ON_RANDOM_LOOT.len() as i32) as usize];
            (name.to_string(), max)
        }
    }
}

/// Apply an entry's or pool's `functions` array. A function's own
/// `conditions` gate it; a missing list means no gating and no RNG draws.
fn apply_functions(
    item: &mut LootItem,
    functions: Option<&J>,
    ctx: &LootContext,
    rng: &mut Lcg,
    res: &LootResolvers<'_>,
) {
    let Some(list) = functions.and_then(J::as_arr) else {
        return;
    };
    for function in list {
        if !conditions_pass(function.get("conditions"), ctx, rng, res) {
            continue;
        }
        apply_one_function(item, function, ctx, rng, res);
    }
}

/// Composite entries use `children`; pools use `entries`.
fn children_of(entry: &J) -> &[J] {
    jarr(entry, "children")
        .or_else(|| jarr(entry, "entries"))
        .map(|v| v.as_slice())
        .unwrap_or(&[])
}

/// Expand one entry into stacks. The bool result mirrors vanilla
/// `LootPoolEntry.expand`: whether the entry's conditions passed, which is
/// what alternatives/sequence use to decide whether to continue.
fn expand_entry(
    entry: &J,
    ctx: &LootContext,
    rng: &mut Lcg,
    res: &LootResolvers<'_>,
    out: &mut Vec<LootItem>,
) -> bool {
    if !conditions_pass(entry.get("conditions"), ctx, rng, res) {
        return false;
    }
    let id = jstr(entry, "type").unwrap_or_default();
    match id.trim_start_matches("minecraft:") {
        "item" => {
            let name = jstr(entry, "name").unwrap_or_default();
            let mut item = LootItem::new(&name);
            apply_functions(&mut item, entry.get("functions"), ctx, rng, res);
            if item.count > 0 {
                out.push(item);
            }
            true
        }
        "empty" => true,
        "tag" => {
            let name = jstr(entry, "name").unwrap_or_default();
            let items = (res.tags)(&name);
            if jbool(entry, "expand", false) {
                for tag_item in items {
                    let mut item = LootItem::new(tag_item);
                    apply_functions(&mut item, entry.get("functions"), ctx, rng, res);
                    if item.count > 0 {
                        out.push(item);
                    }
                }
            } else if !items.is_empty() {
                let picked = items[rng.next_bound(items.len() as i32) as usize];
                let mut item = LootItem::new(picked);
                apply_functions(&mut item, entry.get("functions"), ctx, rng, res);
                if item.count > 0 {
                    out.push(item);
                }
            }
            true
        }
        "alternatives" => children_of(entry)
            .iter()
            .any(|child| expand_entry(child, ctx, rng, res, out)),
        "group" => {
            for child in children_of(entry) {
                expand_entry(child, ctx, rng, res, out);
            }
            true
        }
        "sequence" => children_of(entry)
            .iter()
            .all(|child| expand_entry(child, ctx, rng, res, out)),
        "loot_table" => {
            // `value` is either a table id resolved through the runtime
            // datapack/JAR lookup or an inline table object.
            let mut nested = Vec::new();
            let expanded = match entry.get("value") {
                Some(J::Str(name)) => loot_table_body(name)
                    .and_then(|json| jparse(json.as_bytes()))
                    .map(|table| {
                        roll_loot_pools(&table, ctx, rng, res, &mut nested);
                        true
                    })
                    .unwrap_or(false),
                Some(value @ J::Obj(_)) => {
                    roll_loot_pools(value, ctx, rng, res, &mut nested);
                    true
                }
                _ => false,
            };
            out.extend(nested);
            expanded
        }
        _ => true,
    }
}

/// Roll every pool of a parsed table into `out`: each passing pool rolls
/// `rolls` times plus `floor(bonus_rolls * luck)`, picks an entry by effective
/// weight, expands it, then applies the pool's functions to every produced
/// stack. Shared by `evaluate_loot_table` and nested `loot_table` entries.
fn roll_loot_pools(
    table: &J,
    ctx: &LootContext,
    rng: &mut Lcg,
    res: &LootResolvers<'_>,
    out: &mut Vec<LootItem>,
) {
    let Some(pools) = jarr(table, "pools") else {
        return;
    };
    for pool in pools {
        if !conditions_pass(pool.get("conditions"), ctx, rng, res) {
            continue;
        }
        let rolls = number_i32(pool.get("rolls"), rng, 0);
        let bonus_rolls = number_f32(pool.get("bonus_rolls"), rng, 0);
        let total_rolls = rolls + (bonus_rolls * ctx.luck).floor() as i32;
        let Some(entries) = jarr(pool, "entries") else {
            continue;
        };
        let weights: Vec<i32> = entries
            .iter()
            .map(|entry| {
                let weight = jint(entry, "weight", 1);
                let quality = jint(entry, "quality", 0);
                weight + (quality as f32 * ctx.luck).floor() as i32
            })
            .collect();
        let total_weight: i32 = weights.iter().sum();
        let mut produced = Vec::new();
        for _ in 0..total_rolls.max(0) {
            if total_weight <= 0 {
                break;
            }
            let mut pick = rng.next_bound(total_weight);
            let mut chosen = 0;
            for (idx, weight) in weights.iter().enumerate() {
                pick -= *weight;
                if pick < 0 {
                    chosen = idx;
                    break;
                }
            }
            expand_entry(&entries[chosen], ctx, rng, res, &mut produced);
        }
        for mut item in produced {
            apply_functions(&mut item, pool.get("functions"), ctx, rng, res);
            if item.count > 0 {
                out.push(item);
            }
        }
    }
}

/// Evaluate a whole loot table: each passing pool rolls `rolls` times plus
/// `floor(bonus_rolls * luck)`, picks an entry by effective weight, expands it,
/// then applies the pool's functions to every produced stack.
pub(crate) fn evaluate_loot_table(
    table: &J,
    ctx: &LootContext,
    rng: &mut Lcg,
    res: &LootResolvers<'_>,
) -> Vec<LootItem> {
    let mut out = Vec::new();
    roll_loot_pools(table, ctx, rng, res, &mut out);
    out
}

// Vanilla 1.21.1 loot-table bodies are not embedded here: the table documents
// are Mojang data, so they are read at runtime from the enabled datapacks, the
// mounted vanilla pack, or the operator's local 1.21.1 JAR. What stays ours is
// the evaluator above — pooled rolls, weighted picks, roll providers, nested
// entries and the fill/split pass — which runs over whatever document the
// install provides.

/// `id -> body` cache for built-in table bodies read from the mount or JAR.
/// A given install cannot change them under us, so each id is read once and
/// interned for the process lifetime. Datapack overrides are deliberately not
/// cached: `/reload` replaces them, so they are re-read on every lookup.
fn vanilla_loot_table_bodies() -> &'static Mutex<HashMap<String, &'static str>> {
    static BODIES: OnceLock<Mutex<HashMap<String, &'static str>>> = OnceLock::new();
    BODIES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether `path` is a safe `data/minecraft/loot_table/` relative path. Ids
/// reach this module from datapack JSON, so separators, `..` and absolute paths
/// are refused instead of being pasted into a filesystem read.
fn loot_table_path_ok(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && path.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'_' | b'-' | b'/' | b'.')
        })
        && !path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
}

/// Read a built-in vanilla table body from the mounted vanilla pack, or from
/// the local 1.21.1 JAR when no extracted pack is authoritative.
fn read_builtin_loot_table(id: &str) -> Option<String> {
    let path = id.strip_prefix("minecraft:").unwrap_or(id);
    if !loot_table_path_ok(path) {
        return None;
    }
    let relative = format!("data/minecraft/loot_table/{path}.json");
    if crate::extracted_vanilla_pack_is_valid() {
        let file = std::path::Path::new(crate::DATAPACK_DIRECTORY)
            .join("vanilla")
            .join(&relative);
        if let Ok(body) = std::fs::read_to_string(file) {
            return Some(body);
        }
    }
    crate::vanilla_jar_data_read(&relative)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .map(str::to_owned)
}

/// The built-in vanilla table body for `minecraft:<path>`, or `None` when this
/// install mounts no vanilla data at all. Callers that must honor datapack
/// overrides use [`loot_table_body`] instead.
pub(crate) fn vanilla_loot_table_json(id: &str) -> Option<&'static str> {
    let id = if id.contains(':') {
        id.to_owned()
    } else {
        format!("minecraft:{id}")
    };
    if let Some(body) = vanilla_loot_table_bodies()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .copied()
    {
        return Some(body);
    }
    let body: &'static str = Box::leak(read_builtin_loot_table(&id)?.into_boxed_str());
    vanilla_loot_table_bodies()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, body);
    Some(body)
}

/// Resolve a table body for evaluation. A datapack override wins and is re-read
/// on every call so `/reload` is visible immediately; the built-in body is the
/// cached fallback.
fn loot_table_body(id: &str) -> Option<Cow<'static, str>> {
    if let Some(body) = crate::loot_table_json(id) {
        return Some(Cow::Owned(body));
    }
    vanilla_loot_table_json(id).map(Cow::Borrowed)
}

/// Every item id the built-in tables can emit (entry `name`s, tag expansions
/// and function-produced items), sorted for binary-search interning into the
/// `&'static str` `LootEntry.item` field. A datapack that emits an id this
/// build does not register still drops out, so no unregistered name leaks.
static VANILLA_LOOT_ITEMS: &[&str] = &[
    "minecraft:acacia_log",
    "minecraft:acacia_planks",
    "minecraft:acacia_sapling",
    "minecraft:amethyst_shard",
    "minecraft:ancient_debris",
    "minecraft:angler_pottery_sherd",
    "minecraft:apple",
    "minecraft:archer_pottery_sherd",
    "minecraft:arms_up_pottery_sherd",
    "minecraft:arrow",
    "minecraft:baked_potato",
    "minecraft:bamboo",
    "minecraft:bamboo_hanging_sign",
    "minecraft:bamboo_planks",
    "minecraft:barrel",
    "minecraft:beef",
    "minecraft:beetroot_seeds",
    "minecraft:beetroot_soup",
    "minecraft:bell",
    "minecraft:birch_log",
    "minecraft:black_wool",
    "minecraft:blackstone",
    "minecraft:blade_pottery_sherd",
    "minecraft:blaze_rod",
    "minecraft:blue_dye",
    "minecraft:blue_ice",
    "minecraft:blue_stained_glass_pane",
    "minecraft:blue_wool",
    "minecraft:bolt_armor_trim_smithing_template",
    "minecraft:bone",
    "minecraft:bone_block",
    "minecraft:bone_meal",
    "minecraft:book",
    "minecraft:bow",
    "minecraft:bowl",
    "minecraft:bread",
    "minecraft:breeze_rod",
    "minecraft:brewer_pottery_sherd",
    "minecraft:brick",
    "minecraft:brown_candle",
    "minecraft:brown_mushroom",
    "minecraft:brown_wool",
    "minecraft:bucket",
    "minecraft:burn_pottery_sherd",
    "minecraft:cactus",
    "minecraft:cake",
    "minecraft:candle",
    "minecraft:carrot",
    "minecraft:chain",
    "minecraft:chainmail_boots",
    "minecraft:chainmail_chestplate",
    "minecraft:chainmail_helmet",
    "minecraft:chainmail_leggings",
    "minecraft:chicken",
    "minecraft:clay",
    "minecraft:clay_ball",
    "minecraft:clock",
    "minecraft:coal",
    "minecraft:coast_armor_trim_smithing_template",
    "minecraft:cod",
    "minecraft:compass",
    "minecraft:cooked_beef",
    "minecraft:cooked_chicken",
    "minecraft:cooked_cod",
    "minecraft:cooked_mutton",
    "minecraft:cooked_porkchop",
    "minecraft:cooked_rabbit",
    "minecraft:cooked_salmon",
    "minecraft:cookie",
    "minecraft:copper_ingot",
    "minecraft:crimson_fungus",
    "minecraft:crimson_nylium",
    "minecraft:crimson_roots",
    "minecraft:crossbow",
    "minecraft:crying_obsidian",
    "minecraft:cyan_wool",
    "minecraft:dandelion",
    "minecraft:danger_pottery_sherd",
    "minecraft:dark_oak_log",
    "minecraft:dead_bush",
    "minecraft:diamond",
    "minecraft:diamond_axe",
    "minecraft:diamond_block",
    "minecraft:diamond_boots",
    "minecraft:diamond_chestplate",
    "minecraft:diamond_helmet",
    "minecraft:diamond_hoe",
    "minecraft:diamond_horse_armor",
    "minecraft:diamond_leggings",
    "minecraft:diamond_pickaxe",
    "minecraft:diamond_shovel",
    "minecraft:diamond_sword",
    "minecraft:disc_fragment_5",
    "minecraft:dune_armor_trim_smithing_template",
    "minecraft:echo_shard",
    "minecraft:egg",
    "minecraft:emerald",
    "minecraft:emerald_block",
    "minecraft:enchanted_book",
    "minecraft:enchanted_golden_apple",
    "minecraft:ender_pearl",
    "minecraft:experience_bottle",
    "minecraft:explorer_pottery_sherd",
    "minecraft:eye_armor_trim_smithing_template",
    "minecraft:feather",
    "minecraft:fern",
    "minecraft:filled_map",
    "minecraft:fire_charge",
    "minecraft:fishing_rod",
    "minecraft:flint",
    "minecraft:flint_and_steel",
    "minecraft:flow_armor_trim_smithing_template",
    "minecraft:flow_banner_pattern",
    "minecraft:flower_pot",
    "minecraft:friend_pottery_sherd",
    "minecraft:furnace",
    "minecraft:ghast_tear",
    "minecraft:gilded_blackstone",
    "minecraft:glass_bottle",
    "minecraft:glistering_melon_slice",
    "minecraft:glow_berries",
    "minecraft:glow_ink_sac",
    "minecraft:glowstone",
    "minecraft:glowstone_dust",
    "minecraft:goat_horn",
    "minecraft:gold_block",
    "minecraft:gold_ingot",
    "minecraft:gold_nugget",
    "minecraft:golden_apple",
    "minecraft:golden_axe",
    "minecraft:golden_boots",
    "minecraft:golden_carrot",
    "minecraft:golden_chestplate",
    "minecraft:golden_helmet",
    "minecraft:golden_hoe",
    "minecraft:golden_horse_armor",
    "minecraft:golden_leggings",
    "minecraft:golden_pickaxe",
    "minecraft:golden_shovel",
    "minecraft:golden_sword",
    "minecraft:gravel",
    "minecraft:gray_wool",
    "minecraft:green_candle",
    "minecraft:green_dye",
    "minecraft:green_wool",
    "minecraft:gunpowder",
    "minecraft:guster_banner_pattern",
    "minecraft:heart_of_the_sea",
    "minecraft:heart_pottery_sherd",
    "minecraft:heartbreak_pottery_sherd",
    "minecraft:heavy_core",
    "minecraft:honey_bottle",
    "minecraft:honeycomb",
    "minecraft:host_armor_trim_smithing_template",
    "minecraft:howl_pottery_sherd",
    "minecraft:ink_sac",
    "minecraft:iron_axe",
    "minecraft:iron_block",
    "minecraft:iron_boots",
    "minecraft:iron_chestplate",
    "minecraft:iron_helmet",
    "minecraft:iron_horse_armor",
    "minecraft:iron_ingot",
    "minecraft:iron_leggings",
    "minecraft:iron_nugget",
    "minecraft:iron_pickaxe",
    "minecraft:iron_shovel",
    "minecraft:iron_sword",
    "minecraft:jungle_log",
    "minecraft:lapis_lazuli",
    "minecraft:large_fern",
    "minecraft:lead",
    "minecraft:leather",
    "minecraft:leather_boots",
    "minecraft:leather_chestplate",
    "minecraft:leather_helmet",
    "minecraft:leather_leggings",
    "minecraft:light_blue_dye",
    "minecraft:light_blue_stained_glass_pane",
    "minecraft:light_blue_wool",
    "minecraft:light_gray_wool",
    "minecraft:light_weighted_pressure_plate",
    "minecraft:lily_pad",
    "minecraft:lime_wool",
    "minecraft:lingering_potion",
    "minecraft:lodestone",
    "minecraft:magenta_stained_glass_pane",
    "minecraft:magenta_wool",
    "minecraft:magma_cream",
    "minecraft:mangrove_log",
    "minecraft:map",
    "minecraft:melon_seeds",
    "minecraft:milk_bucket",
    "minecraft:miner_pottery_sherd",
    "minecraft:moss_block",
    "minecraft:mourner_pottery_sherd",
    "minecraft:music_disc_11",
    "minecraft:music_disc_13",
    "minecraft:music_disc_blocks",
    "minecraft:music_disc_cat",
    "minecraft:music_disc_chirp",
    "minecraft:music_disc_creator",
    "minecraft:music_disc_creator_music_box",
    "minecraft:music_disc_far",
    "minecraft:music_disc_mall",
    "minecraft:music_disc_mellohi",
    "minecraft:music_disc_otherside",
    "minecraft:music_disc_pigstep",
    "minecraft:music_disc_precipice",
    "minecraft:music_disc_relic",
    "minecraft:music_disc_stal",
    "minecraft:music_disc_strad",
    "minecraft:music_disc_wait",
    "minecraft:music_disc_ward",
    "minecraft:mutton",
    "minecraft:name_tag",
    "minecraft:nautilus_shell",
    "minecraft:nether_brick",
    "minecraft:nether_wart",
    "minecraft:netherite_ingot",
    "minecraft:netherite_scrap",
    "minecraft:netherite_upgrade_smithing_template",
    "minecraft:oak_hanging_sign",
    "minecraft:oak_log",
    "minecraft:oak_planks",
    "minecraft:oak_sapling",
    "minecraft:obsidian",
    "minecraft:ochre_froglight",
    "minecraft:ominous_bottle",
    "minecraft:ominous_trial_key",
    "minecraft:orange_dye",
    "minecraft:orange_wool",
    "minecraft:packed_ice",
    "minecraft:paper",
    "minecraft:pearlescent_froglight",
    "minecraft:phantom_membrane",
    "minecraft:piglin_banner_pattern",
    "minecraft:pink_stained_glass_pane",
    "minecraft:pink_wool",
    "minecraft:pitcher_pod",
    "minecraft:plenty_pottery_sherd",
    "minecraft:poisonous_potato",
    "minecraft:poppy",
    "minecraft:porkchop",
    "minecraft:potato",
    "minecraft:potion",
    "minecraft:prismarine_crystals",
    "minecraft:prismarine_shard",
    "minecraft:prize_pottery_sherd",
    "minecraft:pufferfish",
    "minecraft:pumpkin",
    "minecraft:pumpkin_pie",
    "minecraft:pumpkin_seeds",
    "minecraft:purple_candle",
    "minecraft:purple_stained_glass_pane",
    "minecraft:purple_wool",
    "minecraft:quartz",
    "minecraft:rabbit",
    "minecraft:rabbit_foot",
    "minecraft:rabbit_hide",
    "minecraft:raiser_armor_trim_smithing_template",
    "minecraft:red_candle",
    "minecraft:red_mushroom",
    "minecraft:red_stained_glass_pane",
    "minecraft:red_wool",
    "minecraft:redstone",
    "minecraft:rib_armor_trim_smithing_template",
    "minecraft:rotten_flesh",
    "minecraft:saddle",
    "minecraft:salmon",
    "minecraft:sand",
    "minecraft:scaffolding",
    "minecraft:sculk",
    "minecraft:sculk_catalyst",
    "minecraft:sculk_sensor",
    "minecraft:seagrass",
    "minecraft:sentry_armor_trim_smithing_template",
    "minecraft:shaper_armor_trim_smithing_template",
    "minecraft:sheaf_pottery_sherd",
    "minecraft:shears",
    "minecraft:shelter_pottery_sherd",
    "minecraft:shield",
    "minecraft:short_grass",
    "minecraft:shulker_shell",
    "minecraft:silence_armor_trim_smithing_template",
    "minecraft:skull_pottery_sherd",
    "minecraft:slime_ball",
    "minecraft:smooth_stone",
    "minecraft:sniffer_egg",
    "minecraft:snort_pottery_sherd",
    "minecraft:snout_armor_trim_smithing_template",
    "minecraft:snow_block",
    "minecraft:snowball",
    "minecraft:soul_sand",
    "minecraft:soul_torch",
    "minecraft:spectral_arrow",
    "minecraft:spider_eye",
    "minecraft:spire_armor_trim_smithing_template",
    "minecraft:splash_potion",
    "minecraft:spruce_hanging_sign",
    "minecraft:spruce_log",
    "minecraft:spruce_sapling",
    "minecraft:spruce_sign",
    "minecraft:stick",
    "minecraft:stone",
    "minecraft:stone_axe",
    "minecraft:stone_bricks",
    "minecraft:stone_hoe",
    "minecraft:stone_pickaxe",
    "minecraft:stone_shovel",
    "minecraft:string",
    "minecraft:sugar",
    "minecraft:suspicious_stew",
    "minecraft:sweet_berries",
    "minecraft:tall_grass",
    "minecraft:tide_armor_trim_smithing_template",
    "minecraft:tipped_arrow",
    "minecraft:tnt",
    "minecraft:torch",
    "minecraft:torchflower_seeds",
    "minecraft:totem_of_undying",
    "minecraft:trial_key",
    "minecraft:trident",
    "minecraft:tripwire_hook",
    "minecraft:tropical_fish",
    "minecraft:tuff",
    "minecraft:verdant_froglight",
    "minecraft:vex_armor_trim_smithing_template",
    "minecraft:ward_armor_trim_smithing_template",
    "minecraft:water_bucket",
    "minecraft:wayfinder_armor_trim_smithing_template",
    "minecraft:wet_sponge",
    "minecraft:wheat",
    "minecraft:wheat_seeds",
    "minecraft:white_dye",
    "minecraft:white_wool",
    "minecraft:wild_armor_trim_smithing_template",
    "minecraft:wind_charge",
    "minecraft:wither_skeleton_skull",
    "minecraft:wooden_axe",
    "minecraft:wooden_hoe",
    "minecraft:wooden_pickaxe",
    "minecraft:yellow_dye",
    "minecraft:yellow_stained_glass_pane",
    "minecraft:yellow_wool",
];

fn vanilla_loot_item(name: &str) -> Option<&'static str> {
    VANILLA_LOOT_ITEMS
        .binary_search(&name)
        .ok()
        .map(|idx| VANILLA_LOOT_ITEMS[idx])
}


/// Enchantment ids the built-in tables can record, sorted for interning into
/// `LootEntry.enchantment`.
static VANILLA_ENCHANTMENT_IDS: &[&str] = &[
    "minecraft:aqua_affinity",
    "minecraft:bane_of_arthropods",
    "minecraft:binding_curse",
    "minecraft:blast_protection",
    "minecraft:breach",
    "minecraft:channeling",
    "minecraft:density",
    "minecraft:depth_strider",
    "minecraft:efficiency",
    "minecraft:feather_falling",
    "minecraft:fire_aspect",
    "minecraft:fire_protection",
    "minecraft:flame",
    "minecraft:fortune",
    "minecraft:frost_walker",
    "minecraft:impaling",
    "minecraft:infinity",
    "minecraft:knockback",
    "minecraft:looting",
    "minecraft:loyalty",
    "minecraft:luck_of_the_sea",
    "minecraft:lure",
    "minecraft:mending",
    "minecraft:multishot",
    "minecraft:piercing",
    "minecraft:power",
    "minecraft:projectile_protection",
    "minecraft:protection",
    "minecraft:punch",
    "minecraft:quick_charge",
    "minecraft:respiration",
    "minecraft:riptide",
    "minecraft:sharpness",
    "minecraft:silk_touch",
    "minecraft:smite",
    "minecraft:soul_speed",
    "minecraft:sweeping_edge",
    "minecraft:swift_sneak",
    "minecraft:thorns",
    "minecraft:unbreaking",
    "minecraft:vanishing_curse",
    "minecraft:wind_burst",
];

fn vanilla_loot_enchantment(name: &str) -> Option<&'static str> {
    VANILLA_ENCHANTMENT_IDS
        .binary_search(&name)
        .ok()
        .map(|idx| VANILLA_ENCHANTMENT_IDS[idx])
}


/// `#minecraft:creeper_drop_music_discs` item-tag expansion (the only
/// `minecraft:tag` loot entry in the 1.21.1 tables).
const CREEPER_DROP_MUSIC_DISCS: &[&str] = &[
    "minecraft:music_disc_11",
    "minecraft:music_disc_13",
    "minecraft:music_disc_blocks",
    "minecraft:music_disc_cat",
    "minecraft:music_disc_chirp",
    "minecraft:music_disc_far",
    "minecraft:music_disc_mall",
    "minecraft:music_disc_mellohi",
    "minecraft:music_disc_stal",
    "minecraft:music_disc_strad",
    "minecraft:music_disc_ward",
    "minecraft:music_disc_wait",
];

/// Tag resolver for built-in table rolls: item tags only; unknown tags expand
/// to nothing, matching a missing tag in vanilla.
fn vanilla_loot_tags(tag: &str) -> Vec<&'static str> {
    match tag {
        "minecraft:creeper_drop_music_discs" => CREEPER_DROP_MUSIC_DISCS.to_vec(),
        _ => Vec::new(),
    }
}

/// `furnace_smelt` results for the raw items the 1.21.1 tables actually smelt
/// (meat/fish/potato/rabbit drops and kelp).
fn vanilla_loot_smelt(item: &str) -> Option<&'static str> {
    Some(match item {
        "minecraft:beef" => "minecraft:cooked_beef",
        "minecraft:chicken" => "minecraft:cooked_chicken",
        "minecraft:cod" => "minecraft:cooked_cod",
        "minecraft:mutton" => "minecraft:cooked_mutton",
        "minecraft:porkchop" => "minecraft:cooked_porkchop",
        "minecraft:potato" => "minecraft:baked_potato",
        "minecraft:rabbit" => "minecraft:cooked_rabbit",
        "minecraft:salmon" => "minecraft:cooked_salmon",
        "minecraft:kelp" => "minecraft:dried_kelp",
        _ => return None,
    })
}

/// `set_damage` maximum durability for the items the 1.21.1 tables damage.
/// Items not listed return 0, which makes `set_damage` a no-op for them.
fn vanilla_loot_max_damage(item: &str) -> i32 {
    match item {
        "minecraft:bow" => 384,
        "minecraft:crossbow" => 465,
        "minecraft:fishing_rod" => 64,
        "minecraft:shield" => 336,
        "minecraft:stone_pickaxe" | "minecraft:stone_axe" => 131,
        "minecraft:iron_sword" | "minecraft:iron_axe" => 250,
        "minecraft:golden_pickaxe" | "minecraft:golden_axe" => 32,
        "minecraft:diamond_pickaxe"
        | "minecraft:diamond_axe"
        | "minecraft:diamond_shovel"
        | "minecraft:diamond_hoe"
        | "minecraft:diamond_sword" => 1561,
        "minecraft:diamond_helmet" => 363,
        "minecraft:diamond_chestplate" => 528,
        "minecraft:diamond_leggings" => 495,
        "minecraft:diamond_boots" => 429,
        "minecraft:leather_boots" => 55,
        _ => 0,
    }
}


/// Convert evaluated stacks into slot-assigned `LootEntry`s: a deterministic
/// Fisher-Yates scatter over the 27 container slots, mirroring the chest-fill
/// path. Unknown item or enchantment ids drop out rather than producing
/// unregistered names.
fn loot_items_to_entries(items: Vec<LootItem>, rng: &mut Lcg) -> Vec<LootEntry> {
    loot_items_to_container_entries(items, rng, false)
}

/// `LootTable.fill`'s item splitting pass, generic over the structured loot
/// item. Splitting consumes the same RNG stream after slot-order shuffling and
/// before final placement.
fn split_loot_items_for_container(
    items: Vec<LootItem>,
    empty_slots_count: usize,
    rng: &mut Lcg,
) -> Vec<LootItem> {
    let mut keep = Vec::new();
    let mut splitable = Vec::new();
    for item in items {
        if item.count > 1 {
            splitable.push(item);
        } else if item.count > 0 {
            keep.push(item);
        }
    }
    let mut available = empty_slots_count as isize - keep.len() as isize - splitable.len() as isize;
    while available > 0 && !splitable.is_empty() {
        available -= 1;
        let idx = mth_next_int(rng, 0, splitable.len() as i32 - 1) as usize;
        let mut stack = splitable.remove(idx);
        let take = mth_next_int(rng, 1, stack.count / 2);
        stack.count -= take;
        let mut split = stack.clone();
        split.count = take;
        for part in [stack, split] {
            if part.count > 1 && rng.next_bool() {
                splitable.push(part);
            } else {
                keep.push(part);
            }
        }
    }
    keep.extend(splitable);
    util_shuffle(&mut keep, rng);
    keep
}

fn loot_items_to_container_entries(
    items: Vec<LootItem>,
    rng: &mut Lcg,
    split: bool,
) -> Vec<LootEntry> {
    let mut slots: Vec<i16> = (0..27).collect();
    util_shuffle(&mut slots, rng);
    let items = if split {
        split_loot_items_for_container(items, slots.len(), rng)
    } else {
        items
    };
    let mut out = Vec::new();
    for item in items {
        // A `set_components`/`copy_components` patch can shrink the stack, and
        // the legacy entry type only carries the count, so apply the cap here.
        let max_stack = item
            .components
            .get("minecraft:max_stack_size")
            .and_then(|value| value.parse::<i32>().ok())
            .unwrap_or(i32::from(u8::MAX))
            .clamp(1, i32::from(u8::MAX));
        let count = item.count.clamp(0, max_stack);
        if count == 0 {
            continue;
        }
        let Some(name) = vanilla_loot_item(&item.item) else {
            continue;
        };
        let enchantment = item
            .enchantment
            .and_then(|(ench, level)| vanilla_loot_enchantment(&ench).map(|id| (id, level)));
        let Some(slot) = slots.pop() else {
            break;
        };
        out.push(LootEntry {
            slot,
            item: name,
            count: count as u8,
            enchantment,
        });
    }
    out.sort_by_key(|entry| entry.slot);
    out
}

/// Mix a caller seed through a splitmix64-style avalanche before handing it to
/// `Lcg::new`. The LCG state is `(seed ^ C) * A + B mod 2^48`, so seeds that
/// differ only in their low bits (consecutive chest/chunk seeds) would
/// otherwise produce nearly identical first draws and identical loot picks.
pub(crate) fn catalogue_rng_seed(seed: i64) -> u64 {
    let mut z = (seed as u64).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Roll a resolved vanilla table with caller supplied context (fishing luck,
/// open-water predicates, and other context-sensitive loot conditions).
pub(crate) fn vanilla_loot_roll_with_context(
    id: &str,
    seed: i64,
    context: &LootContext,
) -> Vec<LootEntry> {
    let Some(json) = loot_table_body(id) else {
        return Vec::new();
    };
    let Some(table) = jparse(json.as_bytes()) else {
        return Vec::new();
    };
    let resolvers = LootResolvers {
        tags: &vanilla_loot_tags,
        smelt: &vanilla_loot_smelt,
        max_damage: &vanilla_loot_max_damage,
    };
    let mut rng = Lcg::new(catalogue_rng_seed(seed));
    let items = evaluate_loot_table(&table, context, &mut rng, &resolvers);
    loot_items_to_entries(items, &mut rng)
}

#[cfg(test)]
#[path = "chest_loot_tests.rs"]
mod tests;

#[cfg(test)]
mod audit1_loot_tests {
    use super::*;

    fn evaluated_count(table_json: &str, seed: i64) -> usize {
        let table = jparse(table_json.as_bytes()).expect("test table parses");
        let resolvers = LootResolvers {
            tags: &vanilla_loot_tags,
            smelt: &vanilla_loot_smelt,
            max_damage: &vanilla_loot_max_damage,
        };
        let mut rng = Lcg::new(catalogue_rng_seed(seed));
        evaluate_loot_table(&table, &LootContext::default(), &mut rng, &resolvers).len()
    }

    #[test]
    fn audit_roll_count_providers_constant_and_binomial() {
        let constant = r#"{"pools":[{"rolls":{"type":"minecraft:constant","value":2.0},"entries":[{"type":"minecraft:item","name":"minecraft:diamond"}]}]}"#;
        assert_eq!(evaluated_count(constant, 5), 2);
        let binomial_all = r#"{"pools":[{"rolls":{"type":"minecraft:binomial","n":3.0,"p":1.0},"entries":[{"type":"minecraft:item","name":"minecraft:diamond"}]}]}"#;
        assert_eq!(evaluated_count(binomial_all, 5), 3);
        let binomial_none = r#"{"pools":[{"rolls":{"type":"minecraft:binomial","n":3.0,"p":0.0},"entries":[{"type":"minecraft:item","name":"minecraft:diamond"}]}]}"#;
        assert_eq!(evaluated_count(binomial_none, 5), 0);
    }

    #[test]
    fn audit_mineshaft_datapack_override_replaces_hardcoded_roll() {
        let _guard = crate::world_guard();
        let key = "minecraft:chests/abandoned_mineshaft".to_string();
        let previous = crate::raw_loot_tables()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                key.clone(),
                r#"{"pools":[{"rolls":{"type":"minecraft:constant","value":2.0},"entries":[{"type":"minecraft:item","name":"minecraft:diamond"}]}]}"#.to_string(),
            );
        let entries = abandoned_mineshaft(991);
        assert_eq!(entries.len(), 2, "the override's constant two rolls are honored");
        assert!(entries.iter().all(|entry| entry.item == "minecraft:diamond"));
        assert!(entries.iter().all(|entry| entry.count == 1));
        match previous {
            Some(previous) => {
                crate::raw_loot_tables().lock().unwrap_or_else(|e| e.into_inner()).insert(key, previous);
            }
            None => {
                crate::raw_loot_tables().lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
            }
        }
    }
}

/// The loader-facing half of the loot evaluator: a mod's own condition and function
/// types, and what happens to one nobody registered.
#[cfg(test)]
mod mod_loot_handler_tests {
    use super::*;

    fn no_tags(_: &str) -> Vec<&'static str> {
        Vec::new()
    }
    fn no_smelt(_: &str) -> Option<&'static str> {
        None
    }
    fn no_damage(_: &str) -> i32 {
        0
    }

    fn roll(table: &J) -> Vec<LootItem> {
        let resolvers = LootResolvers {
            tags: &no_tags,
            smelt: &no_smelt,
            max_damage: &no_damage,
        };
        let mut rng = Lcg::new(11);
        evaluate_loot_table(table, &LootContext::default(), &mut rng, &resolvers)
    }

    fn table_gated_by(condition: &str) -> J {
        let body = format!(
            r#"{{"pools":[{{"rolls":1,"conditions":[{{"condition":"{condition}"}}],"entries":[{{"type":"minecraft:item","name":"minecraft:diamond"}}]}}]}}"#
        );
        jparse(body.as_bytes()).expect("test table parses")
    }

    #[test]
    fn registered_custom_condition_gates_the_pool() {
        let table = table_gated_by("mymod:always_true");
        register_custom_loot_condition(
            "mymod:always_true",
            std::sync::Arc::new(|_: &J, _: &LootContext, _: &mut Lcg| true),
        )
        .expect("register mymod:always_true");
        assert_eq!(roll(&table).len(), 1, "the registered handler kept the entry");

        assert!(unregister_custom_loot_handlers("mymod:always_true"));
        assert!(
            roll(&table).is_empty(),
            "an unregistered custom condition fails closed"
        );
        assert!(
            unregistered_custom_loot_types().contains(&"mymod:always_true".to_string()),
            "the unregistered type is observable instead of silently ignored"
        );
    }

    #[test]
    fn unregistered_custom_condition_is_recorded() {
        let table = table_gated_by("mymod:needs_a_consumer");
        assert!(roll(&table).is_empty());
        assert!(unregistered_custom_loot_types().contains(&"mymod:needs_a_consumer".to_string()));
    }

    #[test]
    fn registered_custom_function_rewrites_the_stack() {
        register_custom_loot_function(
            "mymod:triple_drop",
            std::sync::Arc::new(
                |_: &J, item: &mut LootItem, _: &LootContext, _: &mut Lcg| item.count *= 3,
            ),
        )
        .expect("register mymod:triple_drop");
        let function = jparse(br#"{"function":"mymod:triple_drop"}"#).expect("function parses");
        let resolvers = LootResolvers {
            tags: &no_tags,
            smelt: &no_smelt,
            max_damage: &no_damage,
        };
        let mut rng = Lcg::new(5);
        let mut item = LootItem::new("minecraft:diamond");
        apply_one_function(
            &mut item,
            &function,
            &LootContext::default(),
            &mut rng,
            &resolvers,
        );
        assert_eq!(item.count, 3, "the registered handler rewrote the count");
        assert!(unregister_custom_loot_handlers("mymod:triple_drop"));
    }

    #[test]
    fn unregistered_custom_function_leaves_the_stack_alone() {
        let function = jparse(br#"{"function":"mymod:unknown_drop"}"#).expect("function parses");
        let resolvers = LootResolvers {
            tags: &no_tags,
            smelt: &no_smelt,
            max_damage: &no_damage,
        };
        let mut rng = Lcg::new(5);
        let mut item = LootItem::new("minecraft:emerald");
        apply_one_function(
            &mut item,
            &function,
            &LootContext::default(),
            &mut rng,
            &resolvers,
        );
        assert_eq!(item.item, "minecraft:emerald");
        assert_eq!(item.count, 1);
        assert!(unregistered_custom_loot_types().contains(&"mymod:unknown_drop".to_string()));
    }

    #[test]
    fn bare_type_ids_land_in_the_minecraft_namespace() {
        register_custom_loot_condition(
            "bare_condition",
            std::sync::Arc::new(|_: &J, _: &LootContext, _: &mut Lcg| true),
        )
        .expect("register bare_condition");
        let table = table_gated_by("minecraft:bare_condition");
        assert_eq!(roll(&table).len(), 1);
        assert!(unregister_custom_loot_handlers("minecraft:bare_condition"));
        assert!(!unregister_custom_loot_handlers("minecraft:never_registered"));
    }
}
