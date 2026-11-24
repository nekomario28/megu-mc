//! Seeded 1.21.1 `minecraft:chests/abandoned_mineshaft` chest-loot table fill.
//! Produces exact vanilla `(slot, item, count, stored_enchantment)` entries from
//! a 48-bit `Lcg` seed without depending on runtime inventory or network types.
use crate::generation_math::Lcg;
use std::collections::BTreeMap;

// Minimal JSON value + parser for the generic loot evaluator. The module-layer
// ratchet forbids adding a `chest_loot -> json` source edge, so this stays
// private here rather than reusing `crate::json`. It accepts the subset vanilla
// loot tables use: objects, arrays, strings (with basic escapes), numbers and
// the true/false/null literals.
#[allow(dead_code)]
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

#[allow(dead_code)]
struct LootJsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

#[allow(dead_code)]
impl<'a> LootJsonParser<'a> {
    fn ws(&mut self) {
        while matches!(self.bytes.get(self.pos), Some(b) if b.is_ascii_whitespace()) {
            self.pos += 1;
        }
    }

    fn string(&mut self) -> Option<String> {
        if self.bytes.get(self.pos) != Some(&b'"') {
            return None;
        }
        self.pos += 1;
        let mut out = String::new();
        loop {
            let b = *self.bytes.get(self.pos)?;
            self.pos += 1;
            match b {
                b'"' => return Some(out),
                b'\\' => {
                    let escaped = *self.bytes.get(self.pos)?;
                    self.pos += 1;
                    out.push(match escaped {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'u' => {
                            let hex = self.bytes.get(self.pos..self.pos + 4)?;
                            self.pos += 4;
                            let hex = std::str::from_utf8(hex).ok()?;
                            char::from_u32(u32::from_str_radix(hex, 16).ok()?)?
                        }
                        other => other as char,
                    });
                }
                other => out.push(other as char),
            }
        }
    }

    fn value(&mut self) -> Option<J> {
        self.ws();
        match *self.bytes.get(self.pos)? {
            b'{' => {
                self.pos += 1;
                let mut fields = Vec::new();
                loop {
                    self.ws();
                    if self.bytes.get(self.pos) == Some(&b'}') {
                        self.pos += 1;
                        return Some(J::Obj(fields));
                    }
                    let key = self.string()?;
                    self.ws();
                    if self.bytes.get(self.pos) != Some(&b':') {
                        return None;
                    }
                    self.pos += 1;
                    let value = self.value()?;
                    fields.push((key, value));
                    self.ws();
                    match self.bytes.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => {
                            self.pos += 1;
                            return Some(J::Obj(fields));
                        }
                        _ => return None,
                    }
                }
            }
            b'[' => {
                self.pos += 1;
                let mut items = Vec::new();
                loop {
                    self.ws();
                    if self.bytes.get(self.pos) == Some(&b']') {
                        self.pos += 1;
                        return Some(J::Arr(items));
                    }
                    items.push(self.value()?);
                    self.ws();
                    match self.bytes.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {
                            self.pos += 1;
                            return Some(J::Arr(items));
                        }
                        _ => return None,
                    }
                }
            }
            b'"' => self.string().map(J::Str),
            b't' => {
                self.pos += 4;
                Some(J::Num(1.0))
            }
            b'f' => {
                self.pos += 5;
                Some(J::Num(0.0))
            }
            b'n' => {
                self.pos += 4;
                Some(J::Null)
            }
            _ => {
                let start = self.pos;
                while matches!(
                    self.bytes.get(self.pos),
                    Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                ) {
                    self.pos += 1;
                }
                if start == self.pos {
                    return None;
                }
                std::str::from_utf8(&self.bytes[start..self.pos])
                    .ok()?
                    .parse()
                    .ok()
                    .map(J::Num)
            }
        }
    }
}

#[allow(dead_code)]
pub(crate) fn jparse(bytes: &[u8]) -> Option<J> {
    LootJsonParser { bytes, pos: 0 }.value()
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

pub(crate) fn abandoned_mineshaft(seed: i64) -> Vec<LootEntry> {
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
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LootItem {
    pub item: String,
    pub count: i32,
    pub enchantment: Option<(String, i32)>,
    pub damage: i32,
    /// `set_potion` / `set_stew_effect` / `set_ominous_bottle_amplifier`
    /// payload: the potion/effect id the stack carries, when one was rolled.
    pub potion: Option<String>,
}

impl LootItem {
    fn new(item: &str) -> Self {
        LootItem {
            item: item.to_string(),
            count: 1,
            enchantment: None,
            damage: 0,
            potion: None,
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
            "minecraft:uniform" => {
                let min = jnum(j, "min").unwrap_or(0.0) as f32;
                let max = jnum(j, "max").unwrap_or(0.0) as f32;
                min + (max - min) * rng.next_f32()
            }
            "minecraft:linear" => linear_f32(j, level),
            _ => 0.0,
        },
        _ => 0.0,
    }
}

/// Integer number provider. Integral uniform bounds use the same
/// `nextInt(max - min + 1) + min` draw as `Mth.nextInt`.
fn number_i32(j: Option<&J>, rng: &mut Lcg, level: i32) -> i32 {
    let Some(j) = j else { return 0 };
    match j {
        J::Num(n) => *n as i32,
        J::Obj(_) => match jstr(j, "type").unwrap_or_default().as_str() {
            "minecraft:uniform" => {
                let min = jnum(j, "min").unwrap_or(0.0);
                let max = jnum(j, "max").unwrap_or(0.0);
                if min.fract() == 0.0 && max.fract() == 0.0 {
                    mth_next_int(rng, min as i32, max as i32)
                } else {
                    number_f32(Some(j), rng, level).floor() as i32
                }
            }
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
            let wanted = wanted.str();
            let members: &[&str] = match wanted {
                "#minecraft:smelts_loot" => ENCHANTMENT_SMELTS_LOOT,
                _ => return false,
            };
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

/// Evaluate one loot condition. Unknown conditions fail closed.
fn condition_holds(
    condition: &J,
    ctx: &LootContext,
    rng: &mut Lcg,
    _res: &LootResolvers<'_>,
) -> bool {
    let id = jstr(condition, "condition").unwrap_or_default();
    match id.trim_start_matches("minecraft:") {
        "inverted" => !condition
            .get("term")
            .map(|t| condition_holds(t, ctx, rng, _res))
            .unwrap_or(false),
        "any_of" => jarr(condition, "terms")
            .map(|terms| terms.iter().any(|t| condition_holds(t, ctx, rng, _res)))
            .unwrap_or(false),
        "all_of" => jarr(condition, "terms")
            .map(|terms| terms.iter().all(|t| condition_holds(t, ctx, rng, _res)))
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
        _ => false,
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
        }
        _ => {}
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
            // `value` is either a `minecraft:` table id resolved through the
            // vanilla catalogue or an inline table object.
            let mut nested = Vec::new();
            let expanded = match entry.get("value") {
                Some(J::Str(name)) => vanilla_loot_table_json(name)
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
#[allow(dead_code)]
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

// Vanilla 1.21.1 loot-table catalogue: clean-room transcription of the
// `data/minecraft/loot_table/**` documents for every table this build does not
// already generate through a dedicated path. The table bodies are the vanilla
// JSON (minified), evaluated by the deterministic
// `evaluate_loot_table`/`roll_loot_pools` roller above with `Lcg`-seeded
// randomness. `vanilla_loot_roll` is the precise entry point; the named
// per-table generators below give each table base a dedicated producer and
// feed the `vanilla_loot_generate` dispatch. Tables that reference other
// tables (`loot_table` entries, e.g. `entities/sheep/<color>` ->
// `entities/sheep`) resolve through the same catalogue. Not yet wired into a
// live container path, hence the dead-code allowances on the generated items.

/// `(loot table id, minified vanilla table JSON)` pairs, sorted by id.
#[allow(dead_code)]
pub(crate) static VANILLA_LOOT_TABLES: &[(&str, &str)] = &[
    ("minecraft:archaeology/desert_pyramid", r##"{"type":"minecraft:archaeology","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:archer_pottery_sherd"},{"type":"minecraft:item","name":"minecraft:miner_pottery_sherd"},{"type":"minecraft:item","name":"minecraft:prize_pottery_sherd"},{"type":"minecraft:item","name":"minecraft:skull_pottery_sherd"},{"type":"minecraft:item","name":"minecraft:diamond"},{"type":"minecraft:item","name":"minecraft:tnt"},{"type":"minecraft:item","name":"minecraft:gunpowder"},{"type":"minecraft:item","name":"minecraft:emerald"}],"rolls":1.0}],"random_sequence":"minecraft:archaeology/desert_pyramid"}"##),
    ("minecraft:archaeology/desert_well", r##"{"type":"minecraft:archaeology","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:arms_up_pottery_sherd","weight":2},{"type":"minecraft:item","name":"minecraft:brewer_pottery_sherd","weight":2},{"type":"minecraft:item","name":"minecraft:brick"},{"type":"minecraft:item","name":"minecraft:emerald"},{"type":"minecraft:item","name":"minecraft:stick"},{"type":"minecraft:item","functions":[{"effects":[{"type":"minecraft:night_vision","duration":{"type":"minecraft:uniform","max":10.0,"min":7.0}},{"type":"minecraft:jump_boost","duration":{"type":"minecraft:uniform","max":10.0,"min":7.0}},{"type":"minecraft:weakness","duration":{"type":"minecraft:uniform","max":8.0,"min":6.0}},{"type":"minecraft:blindness","duration":{"type":"minecraft:uniform","max":7.0,"min":5.0}},{"type":"minecraft:poison","duration":{"type":"minecraft:uniform","max":20.0,"min":10.0}},{"type":"minecraft:saturation","duration":{"type":"minecraft:uniform","max":10.0,"min":7.0}}],"function":"minecraft:set_stew_effect"}],"name":"minecraft:suspicious_stew"}],"rolls":1.0}],"random_sequence":"minecraft:archaeology/desert_well"}"##),
    ("minecraft:archaeology/ocean_ruin_cold", r##"{"type":"minecraft:archaeology","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:blade_pottery_sherd"},{"type":"minecraft:item","name":"minecraft:explorer_pottery_sherd"},{"type":"minecraft:item","name":"minecraft:mourner_pottery_sherd"},{"type":"minecraft:item","name":"minecraft:plenty_pottery_sherd"},{"type":"minecraft:item","name":"minecraft:iron_axe"},{"type":"minecraft:item","name":"minecraft:emerald","weight":2},{"type":"minecraft:item","name":"minecraft:wheat","weight":2},{"type":"minecraft:item","name":"minecraft:wooden_hoe","weight":2},{"type":"minecraft:item","name":"minecraft:coal","weight":2},{"type":"minecraft:item","name":"minecraft:gold_nugget","weight":2}],"rolls":1.0}],"random_sequence":"minecraft:archaeology/ocean_ruin_cold"}"##),
    ("minecraft:archaeology/ocean_ruin_warm", r##"{"type":"minecraft:archaeology","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:angler_pottery_sherd"},{"type":"minecraft:item","name":"minecraft:shelter_pottery_sherd"},{"type":"minecraft:item","name":"minecraft:snort_pottery_sherd"},{"type":"minecraft:item","name":"minecraft:sniffer_egg"},{"type":"minecraft:item","name":"minecraft:iron_axe"},{"type":"minecraft:item","name":"minecraft:emerald","weight":2},{"type":"minecraft:item","name":"minecraft:wheat","weight":2},{"type":"minecraft:item","name":"minecraft:wooden_hoe","weight":2},{"type":"minecraft:item","name":"minecraft:coal","weight":2},{"type":"minecraft:item","name":"minecraft:gold_nugget","weight":2}],"rolls":1.0}],"random_sequence":"minecraft:archaeology/ocean_ruin_warm"}"##),
    ("minecraft:archaeology/trail_ruins_common", r##"{"type":"minecraft:archaeology","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:emerald","weight":2},{"type":"minecraft:item","name":"minecraft:wheat","weight":2},{"type":"minecraft:item","name":"minecraft:wooden_hoe","weight":2},{"type":"minecraft:item","name":"minecraft:clay","weight":2},{"type":"minecraft:item","name":"minecraft:brick","weight":2},{"type":"minecraft:item","name":"minecraft:yellow_dye","weight":2},{"type":"minecraft:item","name":"minecraft:blue_dye","weight":2},{"type":"minecraft:item","name":"minecraft:light_blue_dye","weight":2},{"type":"minecraft:item","name":"minecraft:white_dye","weight":2},{"type":"minecraft:item","name":"minecraft:orange_dye","weight":2},{"type":"minecraft:item","name":"minecraft:red_candle","weight":2},{"type":"minecraft:item","name":"minecraft:green_candle","weight":2},{"type":"minecraft:item","name":"minecraft:purple_candle","weight":2},{"type":"minecraft:item","name":"minecraft:brown_candle","weight":2},{"type":"minecraft:item","name":"minecraft:magenta_stained_glass_pane"},{"type":"minecraft:item","name":"minecraft:pink_stained_glass_pane"},{"type":"minecraft:item","name":"minecraft:blue_stained_glass_pane"},{"type":"minecraft:item","name":"minecraft:light_blue_stained_glass_pane"},{"type":"minecraft:item","name":"minecraft:red_stained_glass_pane"},{"type":"minecraft:item","name":"minecraft:yellow_stained_glass_pane"},{"type":"minecraft:item","name":"minecraft:purple_stained_glass_pane"},{"type":"minecraft:item","name":"minecraft:spruce_hanging_sign"},{"type":"minecraft:item","name":"minecraft:oak_hanging_sign"},{"type":"minecraft:item","name":"minecraft:gold_nugget"},{"type":"minecraft:item","name":"minecraft:coal"},{"type":"minecraft:item","name":"minecraft:wheat_seeds"},{"type":"minecraft:item","name":"minecraft:beetroot_seeds"},{"type":"minecraft:item","name":"minecraft:dead_bush"},{"type":"minecraft:item","name":"minecraft:flower_pot"},{"type":"minecraft:item","name":"minecraft:string"},{"type":"minecraft:item","name":"minecraft:lead"}],"rolls":1.0}],"random_sequence":"minecraft:archaeology/trail_ruins_common"}"##),
    ("minecraft:archaeology/trail_ruins_rare", r##"{"type":"minecraft:archaeology","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:burn_pottery_sherd"},{"type":"minecraft:item","name":"minecraft:danger_pottery_sherd"},{"type":"minecraft:item","name":"minecraft:friend_pottery_sherd"},{"type":"minecraft:item","name":"minecraft:heart_pottery_sherd"},{"type":"minecraft:item","name":"minecraft:heartbreak_pottery_sherd"},{"type":"minecraft:item","name":"minecraft:howl_pottery_sherd"},{"type":"minecraft:item","name":"minecraft:sheaf_pottery_sherd"},{"type":"minecraft:item","name":"minecraft:wayfinder_armor_trim_smithing_template"},{"type":"minecraft:item","name":"minecraft:raiser_armor_trim_smithing_template"},{"type":"minecraft:item","name":"minecraft:shaper_armor_trim_smithing_template"},{"type":"minecraft:item","name":"minecraft:host_armor_trim_smithing_template"},{"type":"minecraft:item","name":"minecraft:music_disc_relic"}],"rolls":1.0}],"random_sequence":"minecraft:archaeology/trail_ruins_rare"}"##),
    ("minecraft:chests/ancient_city", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:enchanted_golden_apple"},{"type":"minecraft:item","name":"minecraft:music_disc_otherside"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:compass","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:sculk_catalyst","weight":2},{"type":"minecraft:item","name":"minecraft:name_tag","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"add":false,"damage":{"type":"minecraft:uniform","max":1.0,"min":0.8},"function":"minecraft:set_damage"},{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":50.0,"min":30.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_hoe","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:lead","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:diamond_horse_armor","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:saddle","weight":2},{"type":"minecraft:item","name":"minecraft:music_disc_13","weight":2},{"type":"minecraft:item","name":"minecraft:music_disc_cat","weight":2},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":50.0,"min":30.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_leggings","weight":2},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"minecraft:swift_sneak"}],"name":"minecraft:book","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":10.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:sculk","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:sculk_sensor","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:candle","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":15.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:amethyst_shard","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:experience_bottle","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":15.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:glow_berries","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":39.0,"min":20.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:iron_leggings","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:echo_shard","weight":4},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:disc_fragment_5","weight":4},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:strong_regeneration"}],"name":"minecraft:potion","weight":5},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:book","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":10.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:book","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":15.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bone","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":15.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:soul_torch","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":15.0,"min":6.0},"function":"minecraft:set_count"}],"name":"minecraft:coal","weight":7}],"rolls":{"type":"minecraft:uniform","max":10.0,"min":5.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:empty","weight":75},{"type":"minecraft:item","name":"minecraft:ward_armor_trim_smithing_template","weight":4},{"type":"minecraft:item","name":"minecraft:silence_armor_trim_smithing_template"}],"rolls":1.0}],"random_sequence":"minecraft:chests/ancient_city"}"##),
    ("minecraft:chests/ancient_city_ice_box", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"effects":[{"type":"minecraft:night_vision","duration":{"type":"minecraft:uniform","max":10.0,"min":7.0}},{"type":"minecraft:blindness","duration":{"type":"minecraft:uniform","max":7.0,"min":5.0}}],"function":"minecraft:set_stew_effect"},{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:suspicious_stew"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":10.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:golden_carrot"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":10.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:baked_potato"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:packed_ice","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:snowball","weight":4}],"rolls":{"type":"minecraft:uniform","max":10.0,"min":4.0}}],"random_sequence":"minecraft:chests/ancient_city_ice_box"}"##),
    ("minecraft:chests/bastion_bridge", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:lodestone"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"damage":{"type":"minecraft:uniform","max":0.5,"min":0.1},"function":"minecraft:set_damage"},{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:crossbow"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":28.0,"min":10.0},"function":"minecraft:set_count"}],"name":"minecraft:spectral_arrow"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":12.0,"min":8.0},"function":"minecraft:set_count"}],"name":"minecraft:gilded_blackstone"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:crying_obsidian"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:gold_block"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":9.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_ingot"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":9.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_ingot"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:golden_sword"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:golden_chestplate"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:golden_helmet"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:golden_leggings"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:golden_boots"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:golden_axe"}],"rolls":{"type":"minecraft:uniform","max":2.0,"min":1.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:string"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:leather"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":17.0,"min":5.0},"function":"minecraft:set_count"}],"name":"minecraft:arrow"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_nugget"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_nugget"}],"rolls":{"type":"minecraft:uniform","max":4.0,"min":2.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:empty","weight":11},{"type":"minecraft:item","name":"minecraft:snout_armor_trim_smithing_template"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:empty","weight":9},{"type":"minecraft:item","name":"minecraft:netherite_upgrade_smithing_template"}],"rolls":1.0}],"random_sequence":"minecraft:chests/bastion_bridge"}"##),
    ("minecraft:chests/bastion_hoglin_stable", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"damage":{"type":"minecraft:uniform","max":0.8,"min":0.15},"function":"minecraft:set_damage"},{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_shovel","weight":15},{"type":"minecraft:item","functions":[{"add":false,"damage":{"type":"minecraft:uniform","max":0.95,"min":0.15},"function":"minecraft:set_damage"},{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_pickaxe","weight":12},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:netherite_scrap","weight":8},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:ancient_debris","weight":12},{"type":"minecraft:item","functions":[{"add":false,"count":2.0,"function":"minecraft:set_count"}],"name":"minecraft:ancient_debris","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:saddle","weight":12},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_block","weight":16},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":17.0,"min":8.0},"function":"minecraft:set_count"}],"name":"minecraft:golden_carrot","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:golden_apple","weight":10}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:golden_axe"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:crying_obsidian"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:glowstone"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:gilded_blackstone"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:soul_sand"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:crimson_nylium"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_nugget"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:leather"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":17.0,"min":5.0},"function":"minecraft:set_count"}],"name":"minecraft:arrow"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:string"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:porkchop"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:cooked_porkchop"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:crimson_fungus"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:crimson_roots"}],"rolls":{"type":"minecraft:uniform","max":4.0,"min":3.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:empty","weight":11},{"type":"minecraft:item","name":"minecraft:snout_armor_trim_smithing_template"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:empty","weight":9},{"type":"minecraft:item","name":"minecraft:netherite_upgrade_smithing_template"}],"rolls":1.0}],"random_sequence":"minecraft:chests/bastion_hoglin_stable"}"##),
    ("minecraft:chests/bastion_other", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_pickaxe","weight":6},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:diamond_shovel","weight":6},{"type":"minecraft:item","functions":[{"add":false,"damage":{"type":"minecraft:uniform","max":0.9,"min":0.1},"function":"minecraft:set_damage"},{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:crossbow","weight":6},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:ancient_debris","weight":12},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:netherite_scrap","weight":4},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":22.0,"min":10.0},"function":"minecraft:set_count"}],"name":"minecraft:spectral_arrow","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:piglin_banner_pattern","weight":9},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:music_disc_pigstep","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":17.0,"min":6.0},"function":"minecraft:set_count"}],"name":"minecraft:golden_carrot","weight":12},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:golden_apple","weight":9},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"minecraft:soul_speed"}],"name":"minecraft:book","weight":10}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"damage":{"type":"minecraft:uniform","max":0.9,"min":0.1},"function":"minecraft:set_damage"},{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:iron_sword","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:iron_block","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:enchant_randomly","options":"minecraft:soul_speed"}],"name":"minecraft:golden_boots"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:golden_axe"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:gold_block","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:crossbow"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_ingot","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_ingot","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:golden_sword"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:golden_chestplate"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:golden_helmet"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:golden_leggings"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:golden_boots"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:crying_obsidian","weight":2}],"rolls":2.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gilded_blackstone","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":10.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:chain"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:magma_cream","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:bone_block"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_nugget"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:obsidian"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_nugget"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:string"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":17.0,"min":5.0},"function":"minecraft:set_count"}],"name":"minecraft:arrow","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:cooked_porkchop"}],"rolls":{"type":"minecraft:uniform","max":4.0,"min":3.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:empty","weight":11},{"type":"minecraft:item","name":"minecraft:snout_armor_trim_smithing_template"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:empty","weight":9},{"type":"minecraft:item","name":"minecraft:netherite_upgrade_smithing_template"}],"rolls":1.0}],"random_sequence":"minecraft:chests/bastion_other"}"##),
    ("minecraft:chests/bastion_treasure", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:netherite_ingot","weight":15},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:ancient_debris","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:netherite_scrap","weight":8},{"type":"minecraft:item","functions":[{"add":false,"count":2.0,"function":"minecraft:set_count"}],"name":"minecraft:ancient_debris","weight":4},{"type":"minecraft:item","functions":[{"add":false,"damage":{"type":"minecraft:uniform","max":1.0,"min":0.8},"function":"minecraft:set_damage"},{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_sword","weight":6},{"type":"minecraft:item","functions":[{"add":false,"damage":{"type":"minecraft:uniform","max":1.0,"min":0.8},"function":"minecraft:set_damage"},{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_chestplate","weight":6},{"type":"minecraft:item","functions":[{"add":false,"damage":{"type":"minecraft:uniform","max":1.0,"min":0.8},"function":"minecraft:set_damage"},{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_helmet","weight":6},{"type":"minecraft:item","functions":[{"add":false,"damage":{"type":"minecraft:uniform","max":1.0,"min":0.8},"function":"minecraft:set_damage"},{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_leggings","weight":6},{"type":"minecraft:item","functions":[{"add":false,"damage":{"type":"minecraft:uniform","max":1.0,"min":0.8},"function":"minecraft:set_damage"},{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_boots","weight":6},{"type":"minecraft:item","name":"minecraft:diamond_sword","weight":6},{"type":"minecraft:item","name":"minecraft:diamond_chestplate","weight":5},{"type":"minecraft:item","name":"minecraft:diamond_helmet","weight":5},{"type":"minecraft:item","name":"minecraft:diamond_boots","weight":5},{"type":"minecraft:item","name":"minecraft:diamond_leggings","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:diamond","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:enchanted_golden_apple","weight":2}],"rolls":3.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":25.0,"min":12.0},"function":"minecraft:set_count"}],"name":"minecraft:spectral_arrow"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_block"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_block"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":9.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_ingot"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":9.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_ingot"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:crying_obsidian"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":23.0,"min":8.0},"function":"minecraft:set_count"}],"name":"minecraft:quartz"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":15.0,"min":5.0},"function":"minecraft:set_count"}],"name":"minecraft:gilded_blackstone"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:magma_cream"}],"rolls":{"type":"minecraft:uniform","max":4.0,"min":3.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:empty","weight":11},{"type":"minecraft:item","name":"minecraft:snout_armor_trim_smithing_template"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:netherite_upgrade_smithing_template"}],"rolls":1.0}],"random_sequence":"minecraft:chests/bastion_treasure"}"##),
    ("minecraft:chests/buried_treasure", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:heart_of_the_sea"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_ingot","weight":20},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_ingot","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:tnt","weight":5}],"rolls":{"type":"minecraft:uniform","max":8.0,"min":5.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:emerald","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:diamond","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:prismarine_crystals","weight":5}],"rolls":{"type":"minecraft:uniform","max":3.0,"min":1.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:leather_chestplate"},{"type":"minecraft:item","name":"minecraft:iron_sword"}],"rolls":{"type":"minecraft:uniform","max":1.0,"min":0.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:cooked_cod"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:cooked_salmon"}],"rolls":2.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:potion"}],"functions":[{"function":"minecraft:set_potion","id":"minecraft:water_breathing"}],"rolls":{"type":"minecraft:uniform","max":2.0,"min":0.0}}],"random_sequence":"minecraft:chests/buried_treasure"}"##),
    ("minecraft:chests/desert_pyramid", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:diamond","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_ingot","weight":15},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_ingot","weight":15},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:emerald","weight":15},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:bone","weight":25},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:spider_eye","weight":25},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:rotten_flesh","weight":25},{"type":"minecraft:item","name":"minecraft:saddle","weight":20},{"type":"minecraft:item","name":"minecraft:iron_horse_armor","weight":15},{"type":"minecraft:item","name":"minecraft:golden_horse_armor","weight":10},{"type":"minecraft:item","name":"minecraft:diamond_horse_armor","weight":5},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:book","weight":20},{"type":"minecraft:item","name":"minecraft:golden_apple","weight":20},{"type":"minecraft:item","name":"minecraft:enchanted_golden_apple","weight":2},{"type":"minecraft:empty","weight":15}],"rolls":{"type":"minecraft:uniform","max":4.0,"min":2.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bone","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gunpowder","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:rotten_flesh","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:string","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:sand","weight":10}],"rolls":4.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:empty","weight":6},{"type":"minecraft:item","functions":[{"add":false,"count":2.0,"function":"minecraft:set_count"}],"name":"minecraft:dune_armor_trim_smithing_template"}],"rolls":1.0}],"random_sequence":"minecraft:chests/desert_pyramid"}"##),
    ("minecraft:chests/end_city_treasure", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:diamond","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_ingot","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_ingot","weight":15},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:emerald","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":10.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:beetroot_seeds","weight":5},{"type":"minecraft:item","name":"minecraft:saddle","weight":3},{"type":"minecraft:item","name":"minecraft:iron_horse_armor"},{"type":"minecraft:item","name":"minecraft:golden_horse_armor"},{"type":"minecraft:item","name":"minecraft:diamond_horse_armor"},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":39.0,"min":20.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_sword","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":39.0,"min":20.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_boots","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":39.0,"min":20.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_chestplate","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":39.0,"min":20.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_leggings","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":39.0,"min":20.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_helmet","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":39.0,"min":20.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_pickaxe","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":39.0,"min":20.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_shovel","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":39.0,"min":20.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:iron_sword","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":39.0,"min":20.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:iron_boots","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":39.0,"min":20.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:iron_chestplate","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":39.0,"min":20.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:iron_leggings","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":39.0,"min":20.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:iron_helmet","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":39.0,"min":20.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:iron_pickaxe","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":39.0,"min":20.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:iron_shovel","weight":3}],"rolls":{"type":"minecraft:uniform","max":6.0,"min":2.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:empty","weight":14},{"type":"minecraft:item","name":"minecraft:spire_armor_trim_smithing_template"}],"rolls":1.0}],"random_sequence":"minecraft:chests/end_city_treasure"}"##),
    ("minecraft:chests/igloo_chest", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:apple","weight":15},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:coal","weight":15},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_nugget","weight":10},{"type":"minecraft:item","name":"minecraft:stone_axe","weight":2},{"type":"minecraft:item","name":"minecraft:rotten_flesh","weight":10},{"type":"minecraft:item","name":"minecraft:emerald"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:wheat","weight":10}],"rolls":{"type":"minecraft:uniform","max":8.0,"min":2.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:golden_apple"}],"rolls":1.0}],"random_sequence":"minecraft:chests/igloo_chest"}"##),
    ("minecraft:chests/jungle_temple", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:diamond","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_ingot","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_ingot","weight":15},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bamboo","weight":15},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:emerald","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:bone","weight":20},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:rotten_flesh","weight":16},{"type":"minecraft:item","name":"minecraft:saddle","weight":3},{"type":"minecraft:item","name":"minecraft:iron_horse_armor"},{"type":"minecraft:item","name":"minecraft:golden_horse_armor"},{"type":"minecraft:item","name":"minecraft:diamond_horse_armor"},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":30.0,"options":"#minecraft:on_random_loot"}],"name":"minecraft:book"}],"rolls":{"type":"minecraft:uniform","max":6.0,"min":2.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:empty","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":2.0,"function":"minecraft:set_count"}],"name":"minecraft:wild_armor_trim_smithing_template"}],"rolls":1.0}],"random_sequence":"minecraft:chests/jungle_temple"}"##),
    ("minecraft:chests/jungle_temple_dispenser", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:arrow","weight":30}],"rolls":{"type":"minecraft:uniform","max":2.0,"min":1.0}}],"random_sequence":"minecraft:chests/jungle_temple_dispenser"}"##),
    ("minecraft:chests/nether_bridge", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:diamond","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_ingot","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_ingot","weight":15},{"type":"minecraft:item","name":"minecraft:golden_sword","weight":5},{"type":"minecraft:item","name":"minecraft:golden_chestplate","weight":5},{"type":"minecraft:item","name":"minecraft:flint_and_steel","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:nether_wart","weight":5},{"type":"minecraft:item","name":"minecraft:saddle","weight":10},{"type":"minecraft:item","name":"minecraft:golden_horse_armor","weight":8},{"type":"minecraft:item","name":"minecraft:iron_horse_armor","weight":5},{"type":"minecraft:item","name":"minecraft:diamond_horse_armor","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:obsidian","weight":2}],"rolls":{"type":"minecraft:uniform","max":4.0,"min":2.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:empty","weight":14},{"type":"minecraft:item","name":"minecraft:rib_armor_trim_smithing_template"}],"rolls":1.0}],"random_sequence":"minecraft:chests/nether_bridge"}"##),
    ("minecraft:chests/pillager_outpost", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:crossbow"}],"rolls":{"type":"minecraft:uniform","max":1.0,"min":0.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:wheat","weight":7},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:potato","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:carrot","weight":5}],"rolls":{"type":"minecraft:uniform","max":3.0,"min":2.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:dark_oak_log"}],"rolls":{"type":"minecraft:uniform","max":3.0,"min":1.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:experience_bottle","weight":7},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:string","weight":4},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:arrow","weight":4},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:tripwire_hook","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_ingot","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:book"}],"rolls":{"type":"minecraft:uniform","max":3.0,"min":2.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:goat_horn"}],"functions":[{"function":"minecraft:set_instrument","options":"#minecraft:regular_goat_horns"}],"rolls":{"type":"minecraft:uniform","max":1.0,"min":0.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:empty","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":2.0,"function":"minecraft:set_count"}],"name":"minecraft:sentry_armor_trim_smithing_template"}],"rolls":1.0}],"random_sequence":"minecraft:chests/pillager_outpost"}"##),
    ("minecraft:chests/ruined_portal", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:obsidian","weight":40},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:flint","weight":40},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":18.0,"min":9.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_nugget","weight":40},{"type":"minecraft:item","name":"minecraft:flint_and_steel","weight":40},{"type":"minecraft:item","name":"minecraft:fire_charge","weight":40},{"type":"minecraft:item","name":"minecraft:golden_apple","weight":15},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":24.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_nugget","weight":15},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:golden_sword","weight":15},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:golden_axe","weight":15},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:golden_hoe","weight":15},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:golden_shovel","weight":15},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:golden_pickaxe","weight":15},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:golden_boots","weight":15},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:golden_chestplate","weight":15},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:golden_helmet","weight":15},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:golden_leggings","weight":15},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":12.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:glistering_melon_slice","weight":5},{"type":"minecraft:item","name":"minecraft:golden_horse_armor","weight":5},{"type":"minecraft:item","name":"minecraft:light_weighted_pressure_plate","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":12.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:golden_carrot","weight":5},{"type":"minecraft:item","name":"minecraft:clock","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_ingot","weight":5},{"type":"minecraft:item","name":"minecraft:bell"},{"type":"minecraft:item","name":"minecraft:enchanted_golden_apple"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_block"}],"rolls":{"type":"minecraft:uniform","max":8.0,"min":4.0}}],"random_sequence":"minecraft:chests/ruined_portal"}"##),
    ("minecraft:chests/shipwreck_map", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"decoration":"minecraft:red_x","function":"minecraft:exploration_map","skip_existing_chunks":false,"zoom":1},{"function":"minecraft:set_name","name":{"translate":"filled_map.buried_treasure"},"target":"item_name"}],"name":"minecraft:map"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:compass"},{"type":"minecraft:item","name":"minecraft:map"},{"type":"minecraft:item","name":"minecraft:clock"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":10.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:paper","weight":20},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:feather","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:book","weight":5}],"rolls":3.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:empty","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":2.0,"function":"minecraft:set_count"}],"name":"minecraft:coast_armor_trim_smithing_template"}],"rolls":1.0}],"random_sequence":"minecraft:chests/shipwreck_map"}"##),
    ("minecraft:chests/shipwreck_supply", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":12.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:paper","weight":8},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:potato","weight":7},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:moss_block","weight":7},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:poisonous_potato","weight":7},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:carrot","weight":7},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":21.0,"min":8.0},"function":"minecraft:set_count"}],"name":"minecraft:wheat","weight":7},{"type":"minecraft:item","functions":[{"effects":[{"type":"minecraft:night_vision","duration":{"type":"minecraft:uniform","max":10.0,"min":7.0}},{"type":"minecraft:jump_boost","duration":{"type":"minecraft:uniform","max":10.0,"min":7.0}},{"type":"minecraft:weakness","duration":{"type":"minecraft:uniform","max":8.0,"min":6.0}},{"type":"minecraft:blindness","duration":{"type":"minecraft:uniform","max":7.0,"min":5.0}},{"type":"minecraft:poison","duration":{"type":"minecraft:uniform","max":20.0,"min":10.0}},{"type":"minecraft:saturation","duration":{"type":"minecraft:uniform","max":10.0,"min":7.0}}],"function":"minecraft:set_stew_effect"}],"name":"minecraft:suspicious_stew","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:coal","weight":6},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":24.0,"min":5.0},"function":"minecraft:set_count"}],"name":"minecraft:rotten_flesh","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:pumpkin","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bamboo","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gunpowder","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:tnt"},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:leather_helmet","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:leather_chestplate","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:leather_leggings","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:leather_boots","weight":3}],"rolls":{"type":"minecraft:uniform","max":10.0,"min":3.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:empty","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":2.0,"function":"minecraft:set_count"}],"name":"minecraft:coast_armor_trim_smithing_template"}],"rolls":1.0}],"random_sequence":"minecraft:chests/shipwreck_supply"}"##),
    ("minecraft:chests/shipwreck_treasure", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_ingot","weight":90},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_ingot","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:emerald","weight":40},{"type":"minecraft:item","name":"minecraft:diamond","weight":5},{"type":"minecraft:item","name":"minecraft:experience_bottle","weight":5}],"rolls":{"type":"minecraft:uniform","max":6.0,"min":3.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":10.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_nugget","weight":50},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":10.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_nugget","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":10.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:lapis_lazuli","weight":20}],"rolls":{"type":"minecraft:uniform","max":5.0,"min":2.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:empty","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":2.0,"function":"minecraft:set_count"}],"name":"minecraft:coast_armor_trim_smithing_template"}],"rolls":1.0}],"random_sequence":"minecraft:chests/shipwreck_treasure"}"##),
    ("minecraft:chests/simple_dungeon", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:saddle","weight":20},{"type":"minecraft:item","name":"minecraft:golden_apple","weight":15},{"type":"minecraft:item","name":"minecraft:enchanted_golden_apple","weight":2},{"type":"minecraft:item","name":"minecraft:music_disc_otherside","weight":2},{"type":"minecraft:item","name":"minecraft:music_disc_13","weight":15},{"type":"minecraft:item","name":"minecraft:music_disc_cat","weight":15},{"type":"minecraft:item","name":"minecraft:name_tag","weight":20},{"type":"minecraft:item","name":"minecraft:golden_horse_armor","weight":10},{"type":"minecraft:item","name":"minecraft:iron_horse_armor","weight":15},{"type":"minecraft:item","name":"minecraft:diamond_horse_armor","weight":5},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:book","weight":10}],"rolls":{"type":"minecraft:uniform","max":3.0,"min":1.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_ingot","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_ingot","weight":5},{"type":"minecraft:item","name":"minecraft:bread","weight":20},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:wheat","weight":20},{"type":"minecraft:item","name":"minecraft:bucket","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:redstone","weight":15},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:coal","weight":15},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:melon_seeds","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:pumpkin_seeds","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:beetroot_seeds","weight":10}],"rolls":{"type":"minecraft:uniform","max":4.0,"min":1.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bone","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gunpowder","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:rotten_flesh","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:string","weight":10}],"rolls":3.0}],"random_sequence":"minecraft:chests/simple_dungeon"}"##),
    ("minecraft:chests/spawn_bonus_chest", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:stone_axe"},{"type":"minecraft:item","name":"minecraft:wooden_axe","weight":3}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:stone_pickaxe"},{"type":"minecraft:item","name":"minecraft:wooden_pickaxe","weight":3}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:apple","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bread","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:salmon","weight":3}],"rolls":3.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":12.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:stick","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":12.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:oak_planks","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:oak_log","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:spruce_log","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:birch_log","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:jungle_log","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:acacia_log","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:dark_oak_log","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:mangrove_log","weight":3}],"rolls":4.0}],"random_sequence":"minecraft:chests/spawn_bonus_chest"}"##),
    ("minecraft:chests/stronghold_corridor", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:ender_pearl","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:diamond","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_ingot","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_ingot","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":9.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:redstone","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bread","weight":15},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:apple","weight":15},{"type":"minecraft:item","name":"minecraft:iron_pickaxe","weight":5},{"type":"minecraft:item","name":"minecraft:iron_sword","weight":5},{"type":"minecraft:item","name":"minecraft:iron_chestplate","weight":5},{"type":"minecraft:item","name":"minecraft:iron_helmet","weight":5},{"type":"minecraft:item","name":"minecraft:iron_leggings","weight":5},{"type":"minecraft:item","name":"minecraft:iron_boots","weight":5},{"type":"minecraft:item","name":"minecraft:golden_apple"},{"type":"minecraft:item","name":"minecraft:saddle"},{"type":"minecraft:item","name":"minecraft:iron_horse_armor"},{"type":"minecraft:item","name":"minecraft:golden_horse_armor"},{"type":"minecraft:item","name":"minecraft:diamond_horse_armor"},{"type":"minecraft:item","name":"minecraft:music_disc_otherside"},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":30.0,"options":"#minecraft:on_random_loot"}],"name":"minecraft:book"}],"rolls":{"type":"minecraft:uniform","max":3.0,"min":2.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:empty","weight":9},{"type":"minecraft:item","name":"minecraft:eye_armor_trim_smithing_template"}],"rolls":1.0}],"random_sequence":"minecraft:chests/stronghold_corridor"}"##),
    ("minecraft:chests/stronghold_crossing", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_ingot","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_ingot","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":9.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:redstone","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:coal","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bread","weight":15},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:apple","weight":15},{"type":"minecraft:item","name":"minecraft:iron_pickaxe"},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":30.0,"options":"#minecraft:on_random_loot"}],"name":"minecraft:book"}],"rolls":{"type":"minecraft:uniform","max":4.0,"min":1.0}}],"random_sequence":"minecraft:chests/stronghold_crossing"}"##),
    ("minecraft:chests/stronghold_library", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:book","weight":20},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:paper","weight":20},{"type":"minecraft:item","name":"minecraft:map"},{"type":"minecraft:item","name":"minecraft:compass"},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":30.0,"options":"#minecraft:on_random_loot"}],"name":"minecraft:book","weight":10}],"rolls":{"type":"minecraft:uniform","max":10.0,"min":2.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:eye_armor_trim_smithing_template"}],"rolls":1.0}],"random_sequence":"minecraft:chests/stronghold_library"}"##),
    ("minecraft:chests/trial_chambers/corridor", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"add":false,"damage":{"type":"minecraft:uniform","max":0.9,"min":0.4},"function":"minecraft:set_damage"},{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:iron_axe"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:honeycomb"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"add":false,"damage":{"type":"minecraft:uniform","max":0.8,"min":0.15},"function":"minecraft:set_damage"}],"name":"minecraft:stone_axe","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"add":false,"damage":{"type":"minecraft:uniform","max":0.8,"min":0.15},"function":"minecraft:set_damage"}],"name":"minecraft:stone_pickaxe","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:ender_pearl","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bamboo_hanging_sign","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:bamboo_planks","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":10.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:scaffolding","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:torch","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":20.0,"min":8.0},"function":"minecraft:set_count"}],"name":"minecraft:tuff","weight":3}],"rolls":{"type":"minecraft:uniform","max":3.0,"min":1.0}}],"random_sequence":"minecraft:chests/trial_chambers/corridor"}"##),
    ("minecraft:chests/trial_chambers/entrance", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:trial_key"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:stick","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:wooden_axe","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:honeycomb","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":10.0,"min":5.0},"function":"minecraft:set_count"}],"name":"minecraft:arrow","weight":10}],"rolls":{"type":"minecraft:uniform","max":3.0,"min":2.0}}],"random_sequence":"minecraft:chests/trial_chambers/entrance"}"##),
    ("minecraft:chests/trial_chambers/intersection", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:diamond_block"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:emerald_block","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"add":false,"damage":{"type":"minecraft:uniform","max":0.5,"min":0.1},"function":"minecraft:set_damage"}],"name":"minecraft:diamond_axe","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"add":false,"damage":{"type":"minecraft:uniform","max":0.5,"min":0.1},"function":"minecraft:set_damage"}],"name":"minecraft:diamond_pickaxe","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:diamond","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:cake","weight":20},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":20.0,"min":8.0},"function":"minecraft:set_count"}],"name":"minecraft:amethyst_shard","weight":20},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_block","weight":20}],"rolls":{"type":"minecraft:uniform","max":3.0,"min":1.0}}],"random_sequence":"minecraft:chests/trial_chambers/intersection"}"##),
    ("minecraft:chests/trial_chambers/intersection_barrel", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"add":false,"damage":{"type":"minecraft:uniform","max":0.9,"min":0.4},"function":"minecraft:set_damage"},{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_axe"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"add":false,"damage":{"type":"minecraft:uniform","max":0.8,"min":0.15},"function":"minecraft:set_damage"}],"name":"minecraft:diamond_pickaxe"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:diamond"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"add":false,"damage":{"type":"minecraft:uniform","max":0.8,"min":0.15},"function":"minecraft:set_damage"}],"name":"minecraft:compass"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bucket"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"add":false,"damage":{"type":"minecraft:uniform","max":0.8,"min":0.15},"function":"minecraft:set_damage"}],"name":"minecraft:golden_axe","weight":4},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"add":false,"damage":{"type":"minecraft:uniform","max":0.8,"min":0.15},"function":"minecraft:set_damage"}],"name":"minecraft:golden_pickaxe","weight":4},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":15.0,"min":5.0},"function":"minecraft:set_count"}],"name":"minecraft:bamboo_planks","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":10.0,"min":6.0},"function":"minecraft:set_count"}],"name":"minecraft:baked_potato","weight":10}],"rolls":{"type":"minecraft:uniform","max":3.0,"min":1.0}}],"random_sequence":"minecraft:chests/trial_chambers/intersection_barrel"}"##),
    ("minecraft:chests/trial_chambers/reward", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:chests/trial_chambers/reward_rare","weight":8},{"type":"minecraft:loot_table","value":"minecraft:chests/trial_chambers/reward_common","weight":2}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:chests/trial_chambers/reward_common"}],"rolls":{"type":"minecraft:uniform","max":3.0,"min":1.0}},{"bonus_rolls":0.0,"conditions":[{"chance":0.25,"condition":"minecraft:random_chance"}],"entries":[{"type":"minecraft:loot_table","value":"minecraft:chests/trial_chambers/reward_unique"}],"rolls":1.0}],"random_sequence":"minecraft:chests/trial_chambers/reward"}"##),
    ("minecraft:chests/trial_chambers/reward_common", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:arrow","weight":4},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":2.0},"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:poison"}],"name":"minecraft:tipped_arrow","weight":4},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:emerald","weight":4},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:wind_charge","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_ingot","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:honey_bottle","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"amplifier":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_ominous_bottle_amplifier"}],"name":"minecraft:ominous_bottle","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":12.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:wind_charge"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:diamond"}],"rolls":1.0}],"random_sequence":"minecraft:chests/trial_chambers/reward_common"}"##),
    ("minecraft:chests/trial_chambers/reward_ominous", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:chests/trial_chambers/reward_ominous_rare","weight":8},{"type":"minecraft:loot_table","value":"minecraft:chests/trial_chambers/reward_ominous_common","weight":2}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:chests/trial_chambers/reward_ominous_common"}],"rolls":{"type":"minecraft:uniform","max":3.0,"min":1.0}},{"bonus_rolls":0.0,"conditions":[{"chance":0.75,"condition":"minecraft:random_chance"}],"entries":[{"type":"minecraft:loot_table","value":"minecraft:chests/trial_chambers/reward_ominous_unique"}],"rolls":1.0}],"random_sequence":"minecraft:chests/trial_chambers/reward_ominous"}"##),
    ("minecraft:chests/trial_chambers/reward_ominous_common", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":10.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:emerald","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":12.0,"min":8.0},"function":"minecraft:set_count"}],"name":"minecraft:wind_charge","weight":4},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":12.0,"min":4.0},"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:strong_slowness"}],"name":"minecraft:tipped_arrow","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:diamond","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"amplifier":{"type":"minecraft:uniform","max":4.0,"min":2.0},"function":"minecraft:set_ominous_bottle_amplifier"}],"name":"minecraft:ominous_bottle"}],"rolls":1.0}],"random_sequence":"minecraft:chests/trial_chambers/reward_ominous_common"}"##),
    ("minecraft:chests/trial_chambers/reward_ominous_rare", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:emerald_block","weight":5},{"type":"minecraft:item","name":"minecraft:iron_block","weight":4},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":20.0,"min":5.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:crossbow","weight":4},{"type":"minecraft:item","name":"minecraft:golden_apple","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":20.0,"min":10.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_axe","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":20.0,"min":10.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_chestplate","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":["minecraft:knockback","minecraft:punch","minecraft:smite","minecraft:looting","minecraft:multishot"]}],"name":"minecraft:book","weight":2},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":["minecraft:breach","minecraft:density"]}],"name":"minecraft:book","weight":2},{"type":"minecraft:item","functions":[{"add":false,"enchantments":{"minecraft:wind_burst":1.0},"function":"minecraft:set_enchantments"}],"name":"minecraft:book","weight":2},{"type":"minecraft:item","name":"minecraft:diamond_block"}],"rolls":1.0}],"random_sequence":"minecraft:chests/trial_chambers/reward_ominous_rare"}"##),
    ("minecraft:chests/trial_chambers/reward_ominous_unique", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:enchanted_golden_apple","weight":3},{"type":"minecraft:item","name":"minecraft:flow_armor_trim_smithing_template","weight":3},{"type":"minecraft:item","name":"minecraft:flow_banner_pattern","weight":2},{"type":"minecraft:item","name":"minecraft:music_disc_creator"},{"type":"minecraft:item","name":"minecraft:heavy_core"}],"rolls":1.0}],"random_sequence":"minecraft:chests/trial_chambers/reward_ominous_unique"}"##),
    ("minecraft:chests/trial_chambers/reward_rare", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:emerald","weight":3},{"type":"minecraft:item","functions":[{"add":false,"damage":{"type":"minecraft:uniform","max":1.0,"min":0.5},"function":"minecraft:set_damage"}],"name":"minecraft:shield","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":15.0,"min":5.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:bow","weight":3},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":20.0,"min":5.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:crossbow","weight":2},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":10.0,"min":0.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:iron_axe","weight":2},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":10.0,"min":0.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:iron_chestplate","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:golden_carrot","weight":2},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":["minecraft:sharpness","minecraft:bane_of_arthropods","minecraft:efficiency","minecraft:fortune","minecraft:silk_touch","minecraft:feather_falling"]}],"name":"minecraft:book","weight":2},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":["minecraft:riptide","minecraft:loyalty","minecraft:channeling","minecraft:impaling","minecraft:mending"]}],"name":"minecraft:book","weight":2},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":15.0,"min":5.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_chestplate"},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":{"type":"minecraft:uniform","max":15.0,"min":5.0},"options":"#minecraft:on_random_loot"}],"name":"minecraft:diamond_axe"}],"rolls":1.0}],"random_sequence":"minecraft:chests/trial_chambers/reward_rare"}"##),
    ("minecraft:chests/trial_chambers/reward_unique", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:golden_apple","weight":4},{"type":"minecraft:item","name":"minecraft:bolt_armor_trim_smithing_template","weight":3},{"type":"minecraft:item","name":"minecraft:guster_banner_pattern","weight":2},{"type":"minecraft:item","name":"minecraft:music_disc_precipice","weight":2},{"type":"minecraft:item","name":"minecraft:trident"}],"rolls":1.0}],"random_sequence":"minecraft:chests/trial_chambers/reward_unique"}"##),
    ("minecraft:chests/trial_chambers/supply", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":14.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:arrow","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":4.0},"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:poison"}],"name":"minecraft:tipped_arrow"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":4.0},"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:slowness"}],"name":"minecraft:tipped_arrow"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:baked_potato","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":10.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:glow_berries","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:acacia_planks"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:moss_block"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:bone_meal"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":10.0,"min":5.0},"function":"minecraft:set_count"}],"name":"minecraft:tuff"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:torch"},{"type":"minecraft:item","functions":[{"add":false,"count":2.0,"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:regeneration"}],"name":"minecraft:potion"},{"type":"minecraft:item","functions":[{"add":false,"count":2.0,"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:strength"}],"name":"minecraft:potion"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"add":false,"damage":{"type":"minecraft:uniform","max":0.8,"min":0.15},"function":"minecraft:set_damage"}],"name":"minecraft:stone_pickaxe","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:milk_bucket"}],"rolls":{"type":"minecraft:uniform","max":5.0,"min":3.0}}],"random_sequence":"minecraft:chests/trial_chambers/supply"}"##),
    ("minecraft:chests/underwater_ruin_big", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:coal","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_nugget","weight":10},{"type":"minecraft:item","name":"minecraft:emerald"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:wheat","weight":10}],"rolls":{"type":"minecraft:uniform","max":8.0,"min":2.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:golden_apple"},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:book","weight":5},{"type":"minecraft:item","name":"minecraft:leather_chestplate"},{"type":"minecraft:item","name":"minecraft:golden_helmet"},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:fishing_rod","weight":5},{"type":"minecraft:item","functions":[{"decoration":"minecraft:red_x","function":"minecraft:exploration_map","skip_existing_chunks":false,"zoom":1},{"function":"minecraft:set_name","name":{"translate":"filled_map.buried_treasure"},"target":"item_name"}],"name":"minecraft:map","weight":10}],"rolls":1.0}],"random_sequence":"minecraft:chests/underwater_ruin_big"}"##),
    ("minecraft:chests/underwater_ruin_small", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:coal","weight":10},{"type":"minecraft:item","name":"minecraft:stone_axe","weight":2},{"type":"minecraft:item","name":"minecraft:rotten_flesh","weight":5},{"type":"minecraft:item","name":"minecraft:emerald"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:wheat","weight":10}],"rolls":{"type":"minecraft:uniform","max":8.0,"min":2.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:leather_chestplate"},{"type":"minecraft:item","name":"minecraft:golden_helmet"},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:fishing_rod","weight":5},{"type":"minecraft:item","functions":[{"decoration":"minecraft:red_x","function":"minecraft:exploration_map","skip_existing_chunks":false,"zoom":1},{"function":"minecraft:set_name","name":{"translate":"filled_map.buried_treasure"},"target":"item_name"}],"name":"minecraft:map","weight":5}],"rolls":1.0}],"random_sequence":"minecraft:chests/underwater_ruin_small"}"##),
    ("minecraft:chests/village/village_armorer", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_ingot","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bread","weight":4},{"type":"minecraft:item","name":"minecraft:iron_helmet"},{"type":"minecraft:item","name":"minecraft:emerald"}],"rolls":{"type":"minecraft:uniform","max":5.0,"min":1.0}}],"random_sequence":"minecraft:chests/village/village_armorer"}"##),
    ("minecraft:chests/village/village_butcher", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:emerald"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:porkchop","weight":6},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:wheat","weight":6},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:beef","weight":6},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:mutton","weight":6},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:coal","weight":3}],"rolls":{"type":"minecraft:uniform","max":5.0,"min":1.0}}],"random_sequence":"minecraft:chests/village/village_butcher"}"##),
    ("minecraft:chests/village/village_cartographer", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:map","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:paper","weight":15},{"type":"minecraft:item","name":"minecraft:compass","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bread","weight":15},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:stick","weight":5}],"rolls":{"type":"minecraft:uniform","max":5.0,"min":1.0}}],"random_sequence":"minecraft:chests/village/village_cartographer"}"##),
    ("minecraft:chests/village/village_desert_house", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:clay_ball"},{"type":"minecraft:item","name":"minecraft:green_dye"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:cactus","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:wheat","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bread","weight":10},{"type":"minecraft:item","name":"minecraft:book"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:dead_bush","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:emerald"}],"rolls":{"type":"minecraft:uniform","max":8.0,"min":3.0}}],"random_sequence":"minecraft:chests/village/village_desert_house"}"##),
    ("minecraft:chests/village/village_fisher", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:emerald"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:cod","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:salmon"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:water_bucket"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:barrel"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:wheat_seeds","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:coal","weight":2}],"rolls":{"type":"minecraft:uniform","max":5.0,"min":1.0}}],"random_sequence":"minecraft:chests/village/village_fisher"}"##),
    ("minecraft:chests/village/village_fletcher", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:emerald"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:arrow","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:feather","weight":6},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:egg","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:flint","weight":6},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:stick","weight":6}],"rolls":{"type":"minecraft:uniform","max":5.0,"min":1.0}}],"random_sequence":"minecraft:chests/village/village_fletcher"}"##),
    ("minecraft:chests/village/village_mason", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:clay_ball"},{"type":"minecraft:item","name":"minecraft:flower_pot"},{"type":"minecraft:item","name":"minecraft:stone","weight":2},{"type":"minecraft:item","name":"minecraft:stone_bricks","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bread","weight":4},{"type":"minecraft:item","name":"minecraft:yellow_dye"},{"type":"minecraft:item","name":"minecraft:smooth_stone"},{"type":"minecraft:item","name":"minecraft:emerald"}],"rolls":{"type":"minecraft:uniform","max":5.0,"min":1.0}}],"random_sequence":"minecraft:chests/village/village_mason"}"##),
    ("minecraft:chests/village/village_plains_house", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_nugget"},{"type":"minecraft:item","name":"minecraft:dandelion","weight":2},{"type":"minecraft:item","name":"minecraft:poppy"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:potato","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bread","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:apple","weight":10},{"type":"minecraft:item","name":"minecraft:book"},{"type":"minecraft:item","name":"minecraft:feather"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:emerald","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:oak_sapling","weight":5}],"rolls":{"type":"minecraft:uniform","max":8.0,"min":3.0}}],"random_sequence":"minecraft:chests/village/village_plains_house"}"##),
    ("minecraft:chests/village/village_savanna_house", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_nugget"},{"type":"minecraft:item","name":"minecraft:short_grass","weight":5},{"type":"minecraft:item","name":"minecraft:tall_grass","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bread","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:wheat_seeds","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:emerald","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:acacia_sapling","weight":10},{"type":"minecraft:item","name":"minecraft:saddle"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:torch"},{"type":"minecraft:item","name":"minecraft:bucket"}],"rolls":{"type":"minecraft:uniform","max":8.0,"min":3.0}}],"random_sequence":"minecraft:chests/village/village_savanna_house"}"##),
    ("minecraft:chests/village/village_shepherd", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:white_wool","weight":6},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:black_wool","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gray_wool","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:brown_wool","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:light_gray_wool","weight":2},{"type":"minecraft:item","name":"minecraft:emerald"},{"type":"minecraft:item","name":"minecraft:shears"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":6.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:wheat","weight":6}],"rolls":{"type":"minecraft:uniform","max":5.0,"min":1.0}}],"random_sequence":"minecraft:chests/village/village_shepherd"}"##),
    ("minecraft:chests/village/village_snowy_house", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:blue_ice"},{"type":"minecraft:item","name":"minecraft:snow_block","weight":4},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:potato","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bread","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:beetroot_seeds","weight":10},{"type":"minecraft:item","name":"minecraft:beetroot_soup"},{"type":"minecraft:item","name":"minecraft:furnace"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:emerald"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:snowball","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:coal","weight":5}],"rolls":{"type":"minecraft:uniform","max":8.0,"min":3.0}}],"random_sequence":"minecraft:chests/village/village_snowy_house"}"##),
    ("minecraft:chests/village/village_taiga_house", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_nugget"},{"type":"minecraft:item","name":"minecraft:fern","weight":2},{"type":"minecraft:item","name":"minecraft:large_fern","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:potato","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:sweet_berries","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bread","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:pumpkin_seeds","weight":5},{"type":"minecraft:item","name":"minecraft:pumpkin_pie"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:emerald","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:spruce_sapling","weight":5},{"type":"minecraft:item","name":"minecraft:spruce_sign"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:spruce_log","weight":10}],"rolls":{"type":"minecraft:uniform","max":8.0,"min":3.0}}],"random_sequence":"minecraft:chests/village/village_taiga_house"}"##),
    ("minecraft:chests/village/village_tannery", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:leather"},{"type":"minecraft:item","name":"minecraft:leather_chestplate","weight":2},{"type":"minecraft:item","name":"minecraft:leather_boots","weight":2},{"type":"minecraft:item","name":"minecraft:leather_helmet","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bread","weight":5},{"type":"minecraft:item","name":"minecraft:leather_leggings","weight":2},{"type":"minecraft:item","name":"minecraft:saddle"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:emerald"}],"rolls":{"type":"minecraft:uniform","max":5.0,"min":1.0}}],"random_sequence":"minecraft:chests/village/village_tannery"}"##),
    ("minecraft:chests/village/village_temple", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:redstone","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bread","weight":7},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:rotten_flesh","weight":7},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:lapis_lazuli"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_ingot"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:emerald"}],"rolls":{"type":"minecraft:uniform","max":8.0,"min":3.0}}],"random_sequence":"minecraft:chests/village/village_temple"}"##),
    ("minecraft:chests/village/village_toolsmith", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:diamond"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_ingot","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_ingot"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bread","weight":15},{"type":"minecraft:item","name":"minecraft:iron_pickaxe","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:coal"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:stick","weight":20},{"type":"minecraft:item","name":"minecraft:iron_shovel","weight":5}],"rolls":{"type":"minecraft:uniform","max":8.0,"min":3.0}}],"random_sequence":"minecraft:chests/village/village_toolsmith"}"##),
    ("minecraft:chests/village/village_weaponsmith", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:diamond","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_ingot","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_ingot","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bread","weight":15},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:apple","weight":15},{"type":"minecraft:item","name":"minecraft:iron_pickaxe","weight":5},{"type":"minecraft:item","name":"minecraft:iron_sword","weight":5},{"type":"minecraft:item","name":"minecraft:iron_chestplate","weight":5},{"type":"minecraft:item","name":"minecraft:iron_helmet","weight":5},{"type":"minecraft:item","name":"minecraft:iron_leggings","weight":5},{"type":"minecraft:item","name":"minecraft:iron_boots","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:obsidian","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":7.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:oak_sapling","weight":5},{"type":"minecraft:item","name":"minecraft:saddle","weight":3},{"type":"minecraft:item","name":"minecraft:iron_horse_armor"},{"type":"minecraft:item","name":"minecraft:golden_horse_armor"},{"type":"minecraft:item","name":"minecraft:diamond_horse_armor"}],"rolls":{"type":"minecraft:uniform","max":8.0,"min":3.0}}],"random_sequence":"minecraft:chests/village/village_weaponsmith"}"##),
    ("minecraft:chests/woodland_mansion", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:lead","weight":20},{"type":"minecraft:item","name":"minecraft:golden_apple","weight":15},{"type":"minecraft:item","name":"minecraft:enchanted_golden_apple","weight":2},{"type":"minecraft:item","name":"minecraft:music_disc_13","weight":15},{"type":"minecraft:item","name":"minecraft:music_disc_cat","weight":15},{"type":"minecraft:item","name":"minecraft:name_tag","weight":20},{"type":"minecraft:item","name":"minecraft:chainmail_chestplate","weight":10},{"type":"minecraft:item","name":"minecraft:diamond_hoe","weight":15},{"type":"minecraft:item","name":"minecraft:diamond_chestplate","weight":5},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"#minecraft:on_random_loot"}],"name":"minecraft:book","weight":10}],"rolls":{"type":"minecraft:uniform","max":3.0,"min":1.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_ingot","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gold_ingot","weight":5},{"type":"minecraft:item","name":"minecraft:bread","weight":20},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:wheat","weight":20},{"type":"minecraft:item","name":"minecraft:bucket","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:redstone","weight":15},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:coal","weight":15},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:melon_seeds","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:pumpkin_seeds","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:beetroot_seeds","weight":10}],"rolls":{"type":"minecraft:uniform","max":4.0,"min":1.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bone","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:gunpowder","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:rotten_flesh","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:string","weight":10}],"rolls":3.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:empty"},{"type":"minecraft:item","name":"minecraft:vex_armor_trim_smithing_template"}],"rolls":1.0}],"random_sequence":"minecraft:chests/woodland_mansion"}"##),
    ("minecraft:dispensers/trial_chambers/chamber", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:water_bucket","weight":4},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:arrow","weight":4},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:snowball","weight":6},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:egg","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:fire_charge","weight":6},{"type":"minecraft:item","functions":[{"function":"minecraft:set_potion","id":"minecraft:slowness"},{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:splash_potion"},{"type":"minecraft:item","functions":[{"function":"minecraft:set_potion","id":"minecraft:poison"},{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:splash_potion"},{"type":"minecraft:item","functions":[{"function":"minecraft:set_potion","id":"minecraft:weakness"},{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:splash_potion"},{"type":"minecraft:item","functions":[{"function":"minecraft:set_potion","id":"minecraft:slowness"},{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:lingering_potion"},{"type":"minecraft:item","functions":[{"function":"minecraft:set_potion","id":"minecraft:poison"},{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:lingering_potion"},{"type":"minecraft:item","functions":[{"function":"minecraft:set_potion","id":"minecraft:weakness"},{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:lingering_potion"},{"type":"minecraft:item","functions":[{"function":"minecraft:set_potion","id":"minecraft:healing"},{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:lingering_potion"}],"rolls":1.0}],"random_sequence":"minecraft:dispensers/trial_chambers/chamber"}"##),
    ("minecraft:dispensers/trial_chambers/corridor", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":4.0},"function":"minecraft:set_count"}],"name":"minecraft:arrow"}],"rolls":1.0}],"random_sequence":"minecraft:dispensers/trial_chambers/corridor"}"##),
    ("minecraft:dispensers/trial_chambers/water", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:water_bucket"}],"rolls":1.0}],"random_sequence":"minecraft:dispensers/trial_chambers/water"}"##),
    ("minecraft:entities/allay", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/allay"}"##),
    ("minecraft:entities/armadillo", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/armadillo"}"##),
    ("minecraft:entities/armor_stand", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/armor_stand"}"##),
    ("minecraft:entities/axolotl", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/axolotl"}"##),
    ("minecraft:entities/bat", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/bat"}"##),
    ("minecraft:entities/bee", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/bee"}"##),
    ("minecraft:entities/blaze", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:killed_by_player"}],"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:blaze_rod"}],"rolls":1.0}],"random_sequence":"minecraft:entities/blaze"}"##),
    ("minecraft:entities/bogged", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:arrow"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:bone"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:killed_by_player"}],"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase","limit":1},{"function":"minecraft:set_potion","id":"minecraft:poison"}],"name":"minecraft:tipped_arrow"}],"rolls":1.0}],"random_sequence":"minecraft:entities/bogged"}"##),
    ("minecraft:entities/breeze", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:killed_by_player"}],"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:breeze_rod"}],"rolls":1.0}],"random_sequence":"minecraft:entities/breeze"}"##),
    ("minecraft:entities/camel", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/camel"}"##),
    ("minecraft:entities/cat", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"}],"name":"minecraft:string"}],"rolls":1.0}],"random_sequence":"minecraft:entities/cat"}"##),
    ("minecraft:entities/cave_spider", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:string"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:killed_by_player"}],"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":-1.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:spider_eye"}],"rolls":1.0}],"random_sequence":"minecraft:entities/cave_spider"}"##),
    ("minecraft:entities/chicken", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:feather"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"conditions":[{"condition":"minecraft:any_of","terms":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"flags":{"is_on_fire":true}}},{"condition":"minecraft:entity_properties","entity":"direct_attacker","predicate":{"equipment":{"mainhand":{"predicates":{"minecraft:enchantments":[{"enchantments":"#minecraft:smelts_loot"}]}}}}}]}],"function":"minecraft:furnace_smelt"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:chicken"}],"rolls":1.0}],"random_sequence":"minecraft:entities/chicken"}"##),
    ("minecraft:entities/cod", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"conditions":[{"condition":"minecraft:any_of","terms":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"flags":{"is_on_fire":true}}},{"condition":"minecraft:entity_properties","entity":"direct_attacker","predicate":{"equipment":{"mainhand":{"predicates":{"minecraft:enchantments":[{"enchantments":"#minecraft:smelts_loot"}]}}}}}]}],"function":"minecraft:furnace_smelt"}],"name":"minecraft:cod"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"chance":0.05,"condition":"minecraft:random_chance"}],"entries":[{"type":"minecraft:item","name":"minecraft:bone_meal"}],"rolls":1.0}],"random_sequence":"minecraft:entities/cod"}"##),
    ("minecraft:entities/cow", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:leather"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"},{"conditions":[{"condition":"minecraft:any_of","terms":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"flags":{"is_on_fire":true}}},{"condition":"minecraft:entity_properties","entity":"direct_attacker","predicate":{"equipment":{"mainhand":{"predicates":{"minecraft:enchantments":[{"enchantments":"#minecraft:smelts_loot"}]}}}}}]}],"function":"minecraft:furnace_smelt"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:beef"}],"rolls":1.0}],"random_sequence":"minecraft:entities/cow"}"##),
    ("minecraft:entities/creeper", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:gunpowder"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:entity_properties","entity":"attacker","predicate":{"type":"#minecraft:skeletons"}}],"entries":[{"type":"minecraft:tag","expand":true,"name":"minecraft:creeper_drop_music_discs"}],"rolls":1.0}],"random_sequence":"minecraft:entities/creeper"}"##),
    ("minecraft:entities/dolphin", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"},{"conditions":[{"condition":"minecraft:any_of","terms":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"flags":{"is_on_fire":true}}},{"condition":"minecraft:entity_properties","entity":"direct_attacker","predicate":{"equipment":{"mainhand":{"predicates":{"minecraft:enchantments":[{"enchantments":"#minecraft:smelts_loot"}]}}}}}]}],"function":"minecraft:furnace_smelt"}],"name":"minecraft:cod"}],"rolls":1.0}],"random_sequence":"minecraft:entities/dolphin"}"##),
    ("minecraft:entities/donkey", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:leather"}],"rolls":1.0}],"random_sequence":"minecraft:entities/donkey"}"##),
    ("minecraft:entities/drowned", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:rotten_flesh"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:killed_by_player"},{"condition":"minecraft:random_chance_with_enchanted_bonus","enchanted_chance":{"type":"minecraft:linear","base":0.13,"per_level_above_first":0.02},"enchantment":"minecraft:looting","unenchanted_chance":0.11}],"entries":[{"type":"minecraft:item","name":"minecraft:copper_ingot"}],"rolls":1.0}],"random_sequence":"minecraft:entities/drowned"}"##),
    ("minecraft:entities/elder_guardian", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:prismarine_shard"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"},{"conditions":[{"condition":"minecraft:any_of","terms":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"flags":{"is_on_fire":true}}},{"condition":"minecraft:entity_properties","entity":"direct_attacker","predicate":{"equipment":{"mainhand":{"predicates":{"minecraft:enchantments":[{"enchantments":"#minecraft:smelts_loot"}]}}}}}]}],"function":"minecraft:furnace_smelt"}],"name":"minecraft:cod","weight":3},{"type":"minecraft:item","functions":[{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:prismarine_crystals","weight":2},{"type":"minecraft:empty"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:killed_by_player"}],"entries":[{"type":"minecraft:item","name":"minecraft:wet_sponge"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:killed_by_player"},{"condition":"minecraft:random_chance_with_enchanted_bonus","enchanted_chance":{"type":"minecraft:linear","base":0.035,"per_level_above_first":0.01},"enchantment":"minecraft:looting","unenchanted_chance":0.025}],"entries":[{"type":"minecraft:loot_table","functions":[{"conditions":[{"condition":"minecraft:any_of","terms":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"flags":{"is_on_fire":true}}},{"condition":"minecraft:entity_properties","entity":"direct_attacker","predicate":{"equipment":{"mainhand":{"predicates":{"minecraft:enchantments":[{"enchantments":"#minecraft:smelts_loot"}]}}}}}]}],"function":"minecraft:furnace_smelt"}],"value":"minecraft:gameplay/fishing/fish"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:empty","weight":4},{"type":"minecraft:item","name":"minecraft:tide_armor_trim_smithing_template"}],"rolls":1.0}],"random_sequence":"minecraft:entities/elder_guardian"}"##),
    ("minecraft:entities/ender_dragon", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/ender_dragon"}"##),
    ("minecraft:entities/enderman", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:ender_pearl"}],"rolls":1.0}],"random_sequence":"minecraft:entities/enderman"}"##),
    ("minecraft:entities/endermite", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/endermite"}"##),
    ("minecraft:entities/evoker", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:totem_of_undying"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:killed_by_player"}],"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:emerald"}],"rolls":1.0}],"random_sequence":"minecraft:entities/evoker"}"##),
    ("minecraft:entities/fox", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/fox"}"##),
    ("minecraft:entities/frog", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/frog"}"##),
    ("minecraft:entities/ghast", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:ghast_tear"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:gunpowder"}],"rolls":1.0}],"random_sequence":"minecraft:entities/ghast"}"##),
    ("minecraft:entities/giant", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/giant"}"##),
    ("minecraft:entities/glow_squid", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:glow_ink_sac"}],"rolls":1.0}],"random_sequence":"minecraft:entities/glow_squid"}"##),
    ("minecraft:entities/goat", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/goat"}"##),
    ("minecraft:entities/guardian", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:prismarine_shard"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"},{"conditions":[{"condition":"minecraft:any_of","terms":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"flags":{"is_on_fire":true}}},{"condition":"minecraft:entity_properties","entity":"direct_attacker","predicate":{"equipment":{"mainhand":{"predicates":{"minecraft:enchantments":[{"enchantments":"#minecraft:smelts_loot"}]}}}}}]}],"function":"minecraft:furnace_smelt"}],"name":"minecraft:cod","weight":2},{"type":"minecraft:item","functions":[{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:prismarine_crystals","weight":2},{"type":"minecraft:empty"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:killed_by_player"},{"condition":"minecraft:random_chance_with_enchanted_bonus","enchanted_chance":{"type":"minecraft:linear","base":0.035,"per_level_above_first":0.01},"enchantment":"minecraft:looting","unenchanted_chance":0.025}],"entries":[{"type":"minecraft:loot_table","functions":[{"conditions":[{"condition":"minecraft:any_of","terms":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"flags":{"is_on_fire":true}}},{"condition":"minecraft:entity_properties","entity":"direct_attacker","predicate":{"equipment":{"mainhand":{"predicates":{"minecraft:enchantments":[{"enchantments":"#minecraft:smelts_loot"}]}}}}}]}],"function":"minecraft:furnace_smelt"}],"value":"minecraft:gameplay/fishing/fish"}],"rolls":1.0}],"random_sequence":"minecraft:entities/guardian"}"##),
    ("minecraft:entities/hoglin", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":2.0},"function":"minecraft:set_count"},{"conditions":[{"condition":"minecraft:any_of","terms":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"flags":{"is_on_fire":true}}},{"condition":"minecraft:entity_properties","entity":"direct_attacker","predicate":{"equipment":{"mainhand":{"predicates":{"minecraft:enchantments":[{"enchantments":"#minecraft:smelts_loot"}]}}}}}]}],"function":"minecraft:furnace_smelt"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:porkchop"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:leather"}],"rolls":1.0}],"random_sequence":"minecraft:entities/hoglin"}"##),
    ("minecraft:entities/horse", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:leather"}],"rolls":1.0}],"random_sequence":"minecraft:entities/horse"}"##),
    ("minecraft:entities/husk", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:rotten_flesh"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:killed_by_player"},{"condition":"minecraft:random_chance_with_enchanted_bonus","enchanted_chance":{"type":"minecraft:linear","base":0.035,"per_level_above_first":0.01},"enchantment":"minecraft:looting","unenchanted_chance":0.025}],"entries":[{"type":"minecraft:item","name":"minecraft:iron_ingot"},{"type":"minecraft:item","name":"minecraft:carrot"},{"type":"minecraft:item","functions":[{"conditions":[{"condition":"minecraft:any_of","terms":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"flags":{"is_on_fire":true}}},{"condition":"minecraft:entity_properties","entity":"direct_attacker","predicate":{"equipment":{"mainhand":{"predicates":{"minecraft:enchantments":[{"enchantments":"#minecraft:smelts_loot"}]}}}}}]}],"function":"minecraft:furnace_smelt"}],"name":"minecraft:potato"}],"rolls":1.0}],"random_sequence":"minecraft:entities/husk"}"##),
    ("minecraft:entities/illusioner", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/illusioner"}"##),
    ("minecraft:entities/iron_golem", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"}],"name":"minecraft:poppy"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_ingot"}],"rolls":1.0}],"random_sequence":"minecraft:entities/iron_golem"}"##),
    ("minecraft:entities/llama", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:leather"}],"rolls":1.0}],"random_sequence":"minecraft:entities/llama"}"##),
    ("minecraft:entities/magma_cube", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","conditions":[{"condition":"minecraft:inverted","term":{"condition":"minecraft:damage_source_properties","predicate":{"source_entity":{"type":"minecraft:frog"}}}},{"condition":"minecraft:entity_properties","entity":"this","predicate":{"type_specific":{"type":"minecraft:slime","size":{"min":2}}}}],"functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":-2.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:magma_cream"},{"type":"minecraft:item","conditions":[{"condition":"minecraft:damage_source_properties","predicate":{"source_entity":{"type":"minecraft:frog","type_specific":{"type":"minecraft:frog","variant":"minecraft:warm"}}}}],"functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:pearlescent_froglight"},{"type":"minecraft:item","conditions":[{"condition":"minecraft:damage_source_properties","predicate":{"source_entity":{"type":"minecraft:frog","type_specific":{"type":"minecraft:frog","variant":"minecraft:cold"}}}}],"functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:verdant_froglight"},{"type":"minecraft:item","conditions":[{"condition":"minecraft:damage_source_properties","predicate":{"source_entity":{"type":"minecraft:frog","type_specific":{"type":"minecraft:frog","variant":"minecraft:temperate"}}}}],"functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:ochre_froglight"}],"rolls":1.0}],"random_sequence":"minecraft:entities/magma_cube"}"##),
    ("minecraft:entities/mooshroom", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:leather"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"},{"conditions":[{"condition":"minecraft:any_of","terms":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"flags":{"is_on_fire":true}}},{"condition":"minecraft:entity_properties","entity":"direct_attacker","predicate":{"equipment":{"mainhand":{"predicates":{"minecraft:enchantments":[{"enchantments":"#minecraft:smelts_loot"}]}}}}}]}],"function":"minecraft:furnace_smelt"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:beef"}],"rolls":1.0}],"random_sequence":"minecraft:entities/mooshroom"}"##),
    ("minecraft:entities/mule", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:leather"}],"rolls":1.0}],"random_sequence":"minecraft:entities/mule"}"##),
    ("minecraft:entities/ocelot", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/ocelot"}"##),
    ("minecraft:entities/panda", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:bamboo"}],"rolls":1.0}],"random_sequence":"minecraft:entities/panda"}"##),
    ("minecraft:entities/parrot", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:feather"}],"rolls":1.0}],"random_sequence":"minecraft:entities/parrot"}"##),
    ("minecraft:entities/phantom", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:killed_by_player"}],"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:phantom_membrane"}],"rolls":1.0}],"random_sequence":"minecraft:entities/phantom"}"##),
    ("minecraft:entities/pig", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"},{"conditions":[{"condition":"minecraft:any_of","terms":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"flags":{"is_on_fire":true}}},{"condition":"minecraft:entity_properties","entity":"direct_attacker","predicate":{"equipment":{"mainhand":{"predicates":{"minecraft:enchantments":[{"enchantments":"#minecraft:smelts_loot"}]}}}}}]}],"function":"minecraft:furnace_smelt"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:porkchop"}],"rolls":1.0}],"random_sequence":"minecraft:entities/pig"}"##),
    ("minecraft:entities/piglin", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/piglin"}"##),
    ("minecraft:entities/piglin_brute", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/piglin_brute"}"##),
    ("minecraft:entities/pillager", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"type_specific":{"type":"minecraft:raider","is_captain":true}}}],"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"amplifier":{"type":"minecraft:uniform","max":4.0,"min":0.0},"function":"minecraft:set_ominous_bottle_amplifier"}],"name":"minecraft:ominous_bottle"}],"rolls":1.0}],"random_sequence":"minecraft:entities/pillager"}"##),
    ("minecraft:entities/player", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/player"}"##),
    ("minecraft:entities/polar_bear", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"conditions":[{"condition":"minecraft:any_of","terms":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"flags":{"is_on_fire":true}}},{"condition":"minecraft:entity_properties","entity":"direct_attacker","predicate":{"equipment":{"mainhand":{"predicates":{"minecraft:enchantments":[{"enchantments":"#minecraft:smelts_loot"}]}}}}}]}],"function":"minecraft:furnace_smelt"},{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:cod","weight":3},{"type":"minecraft:item","functions":[{"conditions":[{"condition":"minecraft:any_of","terms":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"flags":{"is_on_fire":true}}},{"condition":"minecraft:entity_properties","entity":"direct_attacker","predicate":{"equipment":{"mainhand":{"predicates":{"minecraft:enchantments":[{"enchantments":"#minecraft:smelts_loot"}]}}}}}]}],"function":"minecraft:furnace_smelt"},{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:salmon"}],"rolls":1.0}],"random_sequence":"minecraft:entities/polar_bear"}"##),
    ("minecraft:entities/pufferfish", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:pufferfish"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"chance":0.05,"condition":"minecraft:random_chance"}],"entries":[{"type":"minecraft:item","name":"minecraft:bone_meal"}],"rolls":1.0}],"random_sequence":"minecraft:entities/pufferfish"}"##),
    ("minecraft:entities/rabbit", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:rabbit_hide"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"conditions":[{"condition":"minecraft:any_of","terms":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"flags":{"is_on_fire":true}}},{"condition":"minecraft:entity_properties","entity":"direct_attacker","predicate":{"equipment":{"mainhand":{"predicates":{"minecraft:enchantments":[{"enchantments":"#minecraft:smelts_loot"}]}}}}}]}],"function":"minecraft:furnace_smelt"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:rabbit"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:killed_by_player"},{"condition":"minecraft:random_chance_with_enchanted_bonus","enchanted_chance":{"type":"minecraft:linear","base":0.13,"per_level_above_first":0.03},"enchantment":"minecraft:looting","unenchanted_chance":0.1}],"entries":[{"type":"minecraft:item","name":"minecraft:rabbit_foot"}],"rolls":1.0}],"random_sequence":"minecraft:entities/rabbit"}"##),
    ("minecraft:entities/ravager", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:saddle"}],"rolls":1.0}],"random_sequence":"minecraft:entities/ravager"}"##),
    ("minecraft:entities/salmon", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"conditions":[{"condition":"minecraft:any_of","terms":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"flags":{"is_on_fire":true}}},{"condition":"minecraft:entity_properties","entity":"direct_attacker","predicate":{"equipment":{"mainhand":{"predicates":{"minecraft:enchantments":[{"enchantments":"#minecraft:smelts_loot"}]}}}}}]}],"function":"minecraft:furnace_smelt"}],"name":"minecraft:salmon"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"chance":0.05,"condition":"minecraft:random_chance"}],"entries":[{"type":"minecraft:item","name":"minecraft:bone_meal"}],"rolls":1.0}],"random_sequence":"minecraft:entities/salmon"}"##),
    ("minecraft:entities/sheep", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"},{"conditions":[{"condition":"minecraft:any_of","terms":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"flags":{"is_on_fire":true}}},{"condition":"minecraft:entity_properties","entity":"direct_attacker","predicate":{"equipment":{"mainhand":{"predicates":{"minecraft:enchantments":[{"enchantments":"#minecraft:smelts_loot"}]}}}}}]}],"function":"minecraft:furnace_smelt"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:mutton"}],"rolls":1.0}],"random_sequence":"minecraft:entities/sheep"}"##),
    ("minecraft:entities/sheep/black", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:black_wool"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:entities/sheep"}],"rolls":1.0}],"random_sequence":"minecraft:entities/sheep/black"}"##),
    ("minecraft:entities/sheep/blue", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:blue_wool"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:entities/sheep"}],"rolls":1.0}],"random_sequence":"minecraft:entities/sheep/blue"}"##),
    ("minecraft:entities/sheep/brown", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:brown_wool"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:entities/sheep"}],"rolls":1.0}],"random_sequence":"minecraft:entities/sheep/brown"}"##),
    ("minecraft:entities/sheep/cyan", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:cyan_wool"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:entities/sheep"}],"rolls":1.0}],"random_sequence":"minecraft:entities/sheep/cyan"}"##),
    ("minecraft:entities/sheep/gray", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:gray_wool"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:entities/sheep"}],"rolls":1.0}],"random_sequence":"minecraft:entities/sheep/gray"}"##),
    ("minecraft:entities/sheep/green", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:green_wool"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:entities/sheep"}],"rolls":1.0}],"random_sequence":"minecraft:entities/sheep/green"}"##),
    ("minecraft:entities/sheep/light_blue", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:light_blue_wool"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:entities/sheep"}],"rolls":1.0}],"random_sequence":"minecraft:entities/sheep/light_blue"}"##),
    ("minecraft:entities/sheep/light_gray", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:light_gray_wool"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:entities/sheep"}],"rolls":1.0}],"random_sequence":"minecraft:entities/sheep/light_gray"}"##),
    ("minecraft:entities/sheep/lime", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:lime_wool"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:entities/sheep"}],"rolls":1.0}],"random_sequence":"minecraft:entities/sheep/lime"}"##),
    ("minecraft:entities/sheep/magenta", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:magenta_wool"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:entities/sheep"}],"rolls":1.0}],"random_sequence":"minecraft:entities/sheep/magenta"}"##),
    ("minecraft:entities/sheep/orange", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:orange_wool"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:entities/sheep"}],"rolls":1.0}],"random_sequence":"minecraft:entities/sheep/orange"}"##),
    ("minecraft:entities/sheep/pink", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:pink_wool"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:entities/sheep"}],"rolls":1.0}],"random_sequence":"minecraft:entities/sheep/pink"}"##),
    ("minecraft:entities/sheep/purple", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:purple_wool"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:entities/sheep"}],"rolls":1.0}],"random_sequence":"minecraft:entities/sheep/purple"}"##),
    ("minecraft:entities/sheep/red", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:red_wool"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:entities/sheep"}],"rolls":1.0}],"random_sequence":"minecraft:entities/sheep/red"}"##),
    ("minecraft:entities/sheep/white", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:white_wool"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:entities/sheep"}],"rolls":1.0}],"random_sequence":"minecraft:entities/sheep/white"}"##),
    ("minecraft:entities/sheep/yellow", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:yellow_wool"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:entities/sheep"}],"rolls":1.0}],"random_sequence":"minecraft:entities/sheep/yellow"}"##),
    ("minecraft:entities/shulker", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:random_chance_with_enchanted_bonus","enchanted_chance":{"type":"minecraft:linear","base":0.5625,"per_level_above_first":0.0625},"enchantment":"minecraft:looting","unenchanted_chance":0.5}],"entries":[{"type":"minecraft:item","name":"minecraft:shulker_shell"}],"rolls":1.0}],"random_sequence":"minecraft:entities/shulker"}"##),
    ("minecraft:entities/silverfish", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/silverfish"}"##),
    ("minecraft:entities/skeleton", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:arrow"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:bone"}],"rolls":1.0}],"random_sequence":"minecraft:entities/skeleton"}"##),
    ("minecraft:entities/skeleton_horse", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:bone"}],"rolls":1.0}],"random_sequence":"minecraft:entities/skeleton_horse"}"##),
    ("minecraft:entities/slime", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"type_specific":{"type":"minecraft:slime","size":1}}}],"entries":[{"type":"minecraft:item","conditions":[{"condition":"minecraft:inverted","term":{"condition":"minecraft:damage_source_properties","predicate":{"source_entity":{"type":"minecraft:frog"}}}}],"functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:slime_ball"},{"type":"minecraft:item","conditions":[{"condition":"minecraft:damage_source_properties","predicate":{"source_entity":{"type":"minecraft:frog"}}}],"functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:slime_ball"}],"rolls":1.0}],"random_sequence":"minecraft:entities/slime"}"##),
    ("minecraft:entities/sniffer", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/sniffer"}"##),
    ("minecraft:entities/snow_golem", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":15.0,"min":0.0},"function":"minecraft:set_count"}],"name":"minecraft:snowball"}],"rolls":1.0}],"random_sequence":"minecraft:entities/snow_golem"}"##),
    ("minecraft:entities/spider", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:string"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:killed_by_player"}],"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":-1.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:spider_eye"}],"rolls":1.0}],"random_sequence":"minecraft:entities/spider"}"##),
    ("minecraft:entities/squid", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:ink_sac"}],"rolls":1.0}],"random_sequence":"minecraft:entities/squid"}"##),
    ("minecraft:entities/stray", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:arrow"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:bone"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:killed_by_player"}],"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase","limit":1},{"function":"minecraft:set_potion","id":"minecraft:slowness"}],"name":"minecraft:tipped_arrow"}],"rolls":1.0}],"random_sequence":"minecraft:entities/stray"}"##),
    ("minecraft:entities/strider", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":5.0,"min":2.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:string"}],"rolls":1.0}],"random_sequence":"minecraft:entities/strider"}"##),
    ("minecraft:entities/tadpole", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/tadpole"}"##),
    ("minecraft:entities/trader_llama", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:leather"}],"rolls":1.0}],"random_sequence":"minecraft:entities/trader_llama"}"##),
    ("minecraft:entities/tropical_fish", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:tropical_fish"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"chance":0.05,"condition":"minecraft:random_chance"}],"entries":[{"type":"minecraft:item","name":"minecraft:bone_meal"}],"rolls":1.0}],"random_sequence":"minecraft:entities/tropical_fish"}"##),
    ("minecraft:entities/turtle", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:seagrass","weight":3}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:damage_source_properties","predicate":{"tags":[{"expected":true,"id":"minecraft:is_lightning"}]}}],"entries":[{"type":"minecraft:item","name":"minecraft:bowl"}],"rolls":1.0}],"random_sequence":"minecraft:entities/turtle"}"##),
    ("minecraft:entities/vex", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/vex"}"##),
    ("minecraft:entities/villager", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/villager"}"##),
    ("minecraft:entities/vindicator", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:killed_by_player"}],"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:emerald"}],"rolls":1.0}],"random_sequence":"minecraft:entities/vindicator"}"##),
    ("minecraft:entities/wandering_trader", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/wandering_trader"}"##),
    ("minecraft:entities/warden", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:sculk_catalyst"}],"rolls":1.0}],"random_sequence":"minecraft:entities/warden"}"##),
    ("minecraft:entities/witch", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:glowstone_dust"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:sugar"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:spider_eye"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:glass_bottle"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:gunpowder"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:stick","weight":2}],"rolls":{"type":"minecraft:uniform","max":3.0,"min":1.0}},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":4.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:redstone"}],"rolls":1.0}],"random_sequence":"minecraft:entities/witch"}"##),
    ("minecraft:entities/wither", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/wither"}"##),
    ("minecraft:entities/wither_skeleton", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":-1.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:coal"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:bone"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:killed_by_player"},{"condition":"minecraft:random_chance_with_enchanted_bonus","enchanted_chance":{"type":"minecraft:linear","base":0.035,"per_level_above_first":0.01},"enchantment":"minecraft:looting","unenchanted_chance":0.025}],"entries":[{"type":"minecraft:item","name":"minecraft:wither_skeleton_skull"}],"rolls":1.0}],"random_sequence":"minecraft:entities/wither_skeleton"}"##),
    ("minecraft:entities/wolf", r##"{"type":"minecraft:entity","random_sequence":"minecraft:entities/wolf"}"##),
    ("minecraft:entities/zoglin", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:rotten_flesh"}],"rolls":1.0}],"random_sequence":"minecraft:entities/zoglin"}"##),
    ("minecraft:entities/zombie", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:rotten_flesh"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:killed_by_player"},{"condition":"minecraft:random_chance_with_enchanted_bonus","enchanted_chance":{"type":"minecraft:linear","base":0.035,"per_level_above_first":0.01},"enchantment":"minecraft:looting","unenchanted_chance":0.025}],"entries":[{"type":"minecraft:item","name":"minecraft:iron_ingot"},{"type":"minecraft:item","name":"minecraft:carrot"},{"type":"minecraft:item","functions":[{"conditions":[{"condition":"minecraft:any_of","terms":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"flags":{"is_on_fire":true}}},{"condition":"minecraft:entity_properties","entity":"direct_attacker","predicate":{"equipment":{"mainhand":{"predicates":{"minecraft:enchantments":[{"enchantments":"#minecraft:smelts_loot"}]}}}}}]}],"function":"minecraft:furnace_smelt"}],"name":"minecraft:potato"}],"rolls":1.0}],"random_sequence":"minecraft:entities/zombie"}"##),
    ("minecraft:entities/zombie_horse", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:rotten_flesh"}],"rolls":1.0}],"random_sequence":"minecraft:entities/zombie_horse"}"##),
    ("minecraft:entities/zombie_villager", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:rotten_flesh"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:killed_by_player"},{"condition":"minecraft:random_chance_with_enchanted_bonus","enchanted_chance":{"type":"minecraft:linear","base":0.035,"per_level_above_first":0.01},"enchantment":"minecraft:looting","unenchanted_chance":0.025}],"entries":[{"type":"minecraft:item","name":"minecraft:iron_ingot"},{"type":"minecraft:item","name":"minecraft:carrot"},{"type":"minecraft:item","functions":[{"conditions":[{"condition":"minecraft:any_of","terms":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"flags":{"is_on_fire":true}}},{"condition":"minecraft:entity_properties","entity":"direct_attacker","predicate":{"equipment":{"mainhand":{"predicates":{"minecraft:enchantments":[{"enchantments":"#minecraft:smelts_loot"}]}}}}}]}],"function":"minecraft:furnace_smelt"}],"name":"minecraft:potato"}],"rolls":1.0}],"random_sequence":"minecraft:entities/zombie_villager"}"##),
    ("minecraft:entities/zombified_piglin", r##"{"type":"minecraft:entity","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:rotten_flesh"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"enchantment":"minecraft:looting","function":"minecraft:enchanted_count_increase"}],"name":"minecraft:gold_nugget"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"condition":"minecraft:killed_by_player"},{"condition":"minecraft:random_chance_with_enchanted_bonus","enchanted_chance":{"type":"minecraft:linear","base":0.035,"per_level_above_first":0.01},"enchantment":"minecraft:looting","unenchanted_chance":0.025}],"entries":[{"type":"minecraft:item","name":"minecraft:gold_ingot"}],"rolls":1.0}],"random_sequence":"minecraft:entities/zombified_piglin"}"##),
    ("minecraft:equipment/trial_chamber", r##"{"type":"minecraft:equipment","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":{"pools":[{"bonus_rolls":0.0,"conditions":[{"chance":0.5,"condition":"minecraft:random_chance"}],"entries":[{"type":"minecraft:item","functions":[{"components":{"minecraft:trim":{"material":"minecraft:copper","pattern":"minecraft:bolt"}},"function":"minecraft:set_components"},{"add":false,"enchantments":{"minecraft:fire_protection":4.0,"minecraft:projectile_protection":4.0,"minecraft:protection":4.0},"function":"minecraft:set_enchantments"}],"name":"minecraft:chainmail_helmet"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"chance":0.5,"condition":"minecraft:random_chance"}],"entries":[{"type":"minecraft:item","functions":[{"components":{"minecraft:trim":{"material":"minecraft:copper","pattern":"minecraft:bolt"}},"function":"minecraft:set_components"},{"add":false,"enchantments":{"minecraft:fire_protection":4.0,"minecraft:projectile_protection":4.0,"minecraft:protection":4.0},"function":"minecraft:set_enchantments"}],"name":"minecraft:chainmail_chestplate"}],"rolls":1.0}]},"weight":4},{"type":"minecraft:loot_table","value":{"pools":[{"bonus_rolls":0.0,"conditions":[{"chance":0.5,"condition":"minecraft:random_chance"}],"entries":[{"type":"minecraft:item","functions":[{"components":{"minecraft:trim":{"material":"minecraft:copper","pattern":"minecraft:flow"}},"function":"minecraft:set_components"},{"add":false,"enchantments":{"minecraft:fire_protection":4.0,"minecraft:projectile_protection":4.0,"minecraft:protection":4.0},"function":"minecraft:set_enchantments"}],"name":"minecraft:iron_helmet"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"chance":0.5,"condition":"minecraft:random_chance"}],"entries":[{"type":"minecraft:item","functions":[{"components":{"minecraft:trim":{"material":"minecraft:copper","pattern":"minecraft:flow"}},"function":"minecraft:set_components"},{"add":false,"enchantments":{"minecraft:fire_protection":4.0,"minecraft:projectile_protection":4.0,"minecraft:protection":4.0},"function":"minecraft:set_enchantments"}],"name":"minecraft:iron_chestplate"}],"rolls":1.0}]},"weight":2},{"type":"minecraft:loot_table","value":{"pools":[{"bonus_rolls":0.0,"conditions":[{"chance":0.5,"condition":"minecraft:random_chance"}],"entries":[{"type":"minecraft:item","functions":[{"components":{"minecraft:trim":{"material":"minecraft:copper","pattern":"minecraft:flow"}},"function":"minecraft:set_components"},{"add":false,"enchantments":{"minecraft:fire_protection":4.0,"minecraft:projectile_protection":4.0,"minecraft:protection":4.0},"function":"minecraft:set_enchantments"}],"name":"minecraft:diamond_helmet"}],"rolls":1.0},{"bonus_rolls":0.0,"conditions":[{"chance":0.5,"condition":"minecraft:random_chance"}],"entries":[{"type":"minecraft:item","functions":[{"components":{"minecraft:trim":{"material":"minecraft:copper","pattern":"minecraft:flow"}},"function":"minecraft:set_components"},{"add":false,"enchantments":{"minecraft:fire_protection":4.0,"minecraft:projectile_protection":4.0,"minecraft:protection":4.0},"function":"minecraft:set_enchantments"}],"name":"minecraft:diamond_chestplate"}],"rolls":1.0}]}}],"rolls":1.0}],"random_sequence":"minecraft:equipment/trial_chamber"}"##),
    ("minecraft:equipment/trial_chamber_melee", r##"{"type":"minecraft:equipment","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:equipment/trial_chamber"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:iron_sword","weight":4},{"type":"minecraft:item","functions":[{"add":false,"enchantments":{"minecraft:sharpness":1.0},"function":"minecraft:set_enchantments"}],"name":"minecraft:iron_sword"},{"type":"minecraft:item","functions":[{"add":false,"enchantments":{"minecraft:knockback":1.0},"function":"minecraft:set_enchantments"}],"name":"minecraft:iron_sword"},{"type":"minecraft:item","name":"minecraft:diamond_sword"}],"rolls":1.0}],"random_sequence":"minecraft:equipment/trial_chamber_melee"}"##),
    ("minecraft:equipment/trial_chamber_ranged", r##"{"type":"minecraft:equipment","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","value":"minecraft:equipment/trial_chamber"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:bow","weight":2},{"type":"minecraft:item","functions":[{"add":false,"enchantments":{"minecraft:power":1.0},"function":"minecraft:set_enchantments"}],"name":"minecraft:bow"},{"type":"minecraft:item","functions":[{"add":false,"enchantments":{"minecraft:punch":1.0},"function":"minecraft:set_enchantments"}],"name":"minecraft:bow"}],"rolls":1.0}],"random_sequence":"minecraft:equipment/trial_chamber_ranged"}"##),
    ("minecraft:gameplay/cat_morning_gift", r##"{"type":"minecraft:gift","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:rabbit_hide","weight":10},{"type":"minecraft:item","name":"minecraft:rabbit_foot","weight":10},{"type":"minecraft:item","name":"minecraft:chicken","weight":10},{"type":"minecraft:item","name":"minecraft:feather","weight":10},{"type":"minecraft:item","name":"minecraft:rotten_flesh","weight":10},{"type":"minecraft:item","name":"minecraft:string","weight":10},{"type":"minecraft:item","name":"minecraft:phantom_membrane","weight":2}],"rolls":1.0}],"random_sequence":"minecraft:gameplay/cat_morning_gift"}"##),
    ("minecraft:gameplay/fishing", r##"{"type":"minecraft:fishing","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:loot_table","quality":-2,"value":"minecraft:gameplay/fishing/junk","weight":10},{"type":"minecraft:loot_table","conditions":[{"condition":"minecraft:entity_properties","entity":"this","predicate":{"type_specific":{"type":"minecraft:fishing_hook","in_open_water":true}}}],"quality":2,"value":"minecraft:gameplay/fishing/treasure","weight":5},{"type":"minecraft:loot_table","quality":-1,"value":"minecraft:gameplay/fishing/fish","weight":85}],"rolls":1.0}],"random_sequence":"minecraft:gameplay/fishing"}"##),
    ("minecraft:gameplay/fishing/fish", r##"{"type":"minecraft:fishing","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:cod","weight":60},{"type":"minecraft:item","name":"minecraft:salmon","weight":25},{"type":"minecraft:item","name":"minecraft:tropical_fish","weight":2},{"type":"minecraft:item","name":"minecraft:pufferfish","weight":13}],"rolls":1.0}],"random_sequence":"minecraft:gameplay/fishing/fish"}"##),
    ("minecraft:gameplay/fishing/junk", r##"{"type":"minecraft:fishing","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:lily_pad","weight":17},{"type":"minecraft:item","functions":[{"add":false,"damage":{"type":"minecraft:uniform","max":0.9,"min":0.0},"function":"minecraft:set_damage"}],"name":"minecraft:leather_boots","weight":10},{"type":"minecraft:item","name":"minecraft:leather","weight":10},{"type":"minecraft:item","name":"minecraft:bone","weight":10},{"type":"minecraft:item","functions":[{"function":"minecraft:set_potion","id":"minecraft:water"}],"name":"minecraft:potion","weight":10},{"type":"minecraft:item","name":"minecraft:string","weight":5},{"type":"minecraft:item","functions":[{"add":false,"damage":{"type":"minecraft:uniform","max":0.9,"min":0.0},"function":"minecraft:set_damage"}],"name":"minecraft:fishing_rod","weight":2},{"type":"minecraft:item","name":"minecraft:bowl","weight":10},{"type":"minecraft:item","name":"minecraft:stick","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":10.0,"function":"minecraft:set_count"}],"name":"minecraft:ink_sac"},{"type":"minecraft:item","name":"minecraft:tripwire_hook","weight":10},{"type":"minecraft:item","name":"minecraft:rotten_flesh","weight":10},{"type":"minecraft:item","conditions":[{"condition":"minecraft:location_check","predicate":{"biomes":["minecraft:jungle","minecraft:sparse_jungle","minecraft:bamboo_jungle"]}}],"name":"minecraft:bamboo","weight":10}],"rolls":1.0}],"random_sequence":"minecraft:gameplay/fishing/junk"}"##),
    ("minecraft:gameplay/fishing/treasure", r##"{"type":"minecraft:fishing","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:name_tag"},{"type":"minecraft:item","name":"minecraft:saddle"},{"type":"minecraft:item","functions":[{"add":false,"damage":{"type":"minecraft:uniform","max":0.25,"min":0.0},"function":"minecraft:set_damage"},{"function":"minecraft:enchant_with_levels","levels":30.0,"options":"#minecraft:on_random_loot"}],"name":"minecraft:bow"},{"type":"minecraft:item","functions":[{"add":false,"damage":{"type":"minecraft:uniform","max":0.25,"min":0.0},"function":"minecraft:set_damage"},{"function":"minecraft:enchant_with_levels","levels":30.0,"options":"#minecraft:on_random_loot"}],"name":"minecraft:fishing_rod"},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_with_levels","levels":30.0,"options":"#minecraft:on_random_loot"}],"name":"minecraft:book"},{"type":"minecraft:item","name":"minecraft:nautilus_shell"}],"rolls":1.0}],"random_sequence":"minecraft:gameplay/fishing/treasure"}"##),
    ("minecraft:gameplay/hero_of_the_village/armorer_gift", r##"{"type":"minecraft:gift","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:chainmail_helmet"},{"type":"minecraft:item","name":"minecraft:chainmail_chestplate"},{"type":"minecraft:item","name":"minecraft:chainmail_leggings"},{"type":"minecraft:item","name":"minecraft:chainmail_boots"}],"rolls":1.0}],"random_sequence":"minecraft:gameplay/hero_of_the_village/armorer_gift"}"##),
    ("minecraft:gameplay/hero_of_the_village/butcher_gift", r##"{"type":"minecraft:gift","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:cooked_rabbit"},{"type":"minecraft:item","name":"minecraft:cooked_chicken"},{"type":"minecraft:item","name":"minecraft:cooked_porkchop"},{"type":"minecraft:item","name":"minecraft:cooked_beef"},{"type":"minecraft:item","name":"minecraft:cooked_mutton"}],"rolls":1.0}],"random_sequence":"minecraft:gameplay/hero_of_the_village/butcher_gift"}"##),
    ("minecraft:gameplay/hero_of_the_village/cartographer_gift", r##"{"type":"minecraft:gift","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:map"},{"type":"minecraft:item","name":"minecraft:paper"}],"rolls":1.0}],"random_sequence":"minecraft:gameplay/hero_of_the_village/cartographer_gift"}"##),
    ("minecraft:gameplay/hero_of_the_village/cleric_gift", r##"{"type":"minecraft:gift","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:redstone"},{"type":"minecraft:item","name":"minecraft:lapis_lazuli"}],"rolls":1.0}],"random_sequence":"minecraft:gameplay/hero_of_the_village/cleric_gift"}"##),
    ("minecraft:gameplay/hero_of_the_village/farmer_gift", r##"{"type":"minecraft:gift","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:bread"},{"type":"minecraft:item","name":"minecraft:pumpkin_pie"},{"type":"minecraft:item","name":"minecraft:cookie"}],"rolls":1.0}],"random_sequence":"minecraft:gameplay/hero_of_the_village/farmer_gift"}"##),
    ("minecraft:gameplay/hero_of_the_village/fisherman_gift", r##"{"type":"minecraft:gift","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:cod"},{"type":"minecraft:item","name":"minecraft:salmon"}],"rolls":1.0}],"random_sequence":"minecraft:gameplay/hero_of_the_village/fisherman_gift"}"##),
    ("minecraft:gameplay/hero_of_the_village/fletcher_gift", r##"{"type":"minecraft:gift","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:arrow","weight":26},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:swiftness"}],"name":"minecraft:tipped_arrow"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:slowness"}],"name":"minecraft:tipped_arrow"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:strength"}],"name":"minecraft:tipped_arrow"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:healing"}],"name":"minecraft:tipped_arrow"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:harming"}],"name":"minecraft:tipped_arrow"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:leaping"}],"name":"minecraft:tipped_arrow"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:regeneration"}],"name":"minecraft:tipped_arrow"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:fire_resistance"}],"name":"minecraft:tipped_arrow"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:water_breathing"}],"name":"minecraft:tipped_arrow"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:invisibility"}],"name":"minecraft:tipped_arrow"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:night_vision"}],"name":"minecraft:tipped_arrow"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:weakness"}],"name":"minecraft:tipped_arrow"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":1.0,"min":0.0},"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:poison"}],"name":"minecraft:tipped_arrow"}],"rolls":1.0}],"random_sequence":"minecraft:gameplay/hero_of_the_village/fletcher_gift"}"##),
    ("minecraft:gameplay/hero_of_the_village/leatherworker_gift", r##"{"type":"minecraft:gift","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:leather"}],"rolls":1.0}],"random_sequence":"minecraft:gameplay/hero_of_the_village/leatherworker_gift"}"##),
    ("minecraft:gameplay/hero_of_the_village/librarian_gift", r##"{"type":"minecraft:gift","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:book"}],"rolls":1.0}],"random_sequence":"minecraft:gameplay/hero_of_the_village/librarian_gift"}"##),
    ("minecraft:gameplay/hero_of_the_village/mason_gift", r##"{"type":"minecraft:gift","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:clay"}],"rolls":1.0}],"random_sequence":"minecraft:gameplay/hero_of_the_village/mason_gift"}"##),
    ("minecraft:gameplay/hero_of_the_village/shepherd_gift", r##"{"type":"minecraft:gift","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:white_wool"},{"type":"minecraft:item","name":"minecraft:orange_wool"},{"type":"minecraft:item","name":"minecraft:magenta_wool"},{"type":"minecraft:item","name":"minecraft:light_blue_wool"},{"type":"minecraft:item","name":"minecraft:yellow_wool"},{"type":"minecraft:item","name":"minecraft:lime_wool"},{"type":"minecraft:item","name":"minecraft:pink_wool"},{"type":"minecraft:item","name":"minecraft:gray_wool"},{"type":"minecraft:item","name":"minecraft:light_gray_wool"},{"type":"minecraft:item","name":"minecraft:cyan_wool"},{"type":"minecraft:item","name":"minecraft:purple_wool"},{"type":"minecraft:item","name":"minecraft:blue_wool"},{"type":"minecraft:item","name":"minecraft:brown_wool"},{"type":"minecraft:item","name":"minecraft:green_wool"},{"type":"minecraft:item","name":"minecraft:red_wool"},{"type":"minecraft:item","name":"minecraft:black_wool"}],"rolls":1.0}],"random_sequence":"minecraft:gameplay/hero_of_the_village/shepherd_gift"}"##),
    ("minecraft:gameplay/hero_of_the_village/toolsmith_gift", r##"{"type":"minecraft:gift","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:stone_pickaxe"},{"type":"minecraft:item","name":"minecraft:stone_axe"},{"type":"minecraft:item","name":"minecraft:stone_hoe"},{"type":"minecraft:item","name":"minecraft:stone_shovel"}],"rolls":1.0}],"random_sequence":"minecraft:gameplay/hero_of_the_village/toolsmith_gift"}"##),
    ("minecraft:gameplay/hero_of_the_village/weaponsmith_gift", r##"{"type":"minecraft:gift","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:stone_axe"},{"type":"minecraft:item","name":"minecraft:golden_axe"},{"type":"minecraft:item","name":"minecraft:iron_axe"}],"rolls":1.0}],"random_sequence":"minecraft:gameplay/hero_of_the_village/weaponsmith_gift"}"##),
    ("minecraft:gameplay/panda_sneeze", r##"{"type":"minecraft:gift","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:slime_ball"},{"type":"minecraft:empty","weight":699}],"rolls":1.0}],"random_sequence":"minecraft:gameplay/panda_sneeze"}"##),
    ("minecraft:gameplay/piglin_bartering", r##"{"type":"minecraft:barter","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"minecraft:soul_speed"}],"name":"minecraft:book","weight":5},{"type":"minecraft:item","functions":[{"function":"minecraft:enchant_randomly","options":"minecraft:soul_speed"}],"name":"minecraft:iron_boots","weight":8},{"type":"minecraft:item","functions":[{"function":"minecraft:set_potion","id":"minecraft:fire_resistance"}],"name":"minecraft:potion","weight":8},{"type":"minecraft:item","functions":[{"function":"minecraft:set_potion","id":"minecraft:fire_resistance"}],"name":"minecraft:splash_potion","weight":8},{"type":"minecraft:item","functions":[{"function":"minecraft:set_potion","id":"minecraft:water"}],"name":"minecraft:potion","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":36.0,"min":10.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_nugget","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:ender_pearl","weight":10},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":9.0,"min":3.0},"function":"minecraft:set_count"}],"name":"minecraft:string","weight":20},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":12.0,"min":5.0},"function":"minecraft:set_count"}],"name":"minecraft:quartz","weight":20},{"type":"minecraft:item","name":"minecraft:obsidian","weight":40},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:crying_obsidian","weight":40},{"type":"minecraft:item","name":"minecraft:fire_charge","weight":40},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:leather","weight":40},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:soul_sand","weight":40},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:nether_brick","weight":40},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":12.0,"min":6.0},"function":"minecraft:set_count"}],"name":"minecraft:spectral_arrow","weight":40},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":16.0,"min":8.0},"function":"minecraft:set_count"}],"name":"minecraft:gravel","weight":40},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":16.0,"min":8.0},"function":"minecraft:set_count"}],"name":"minecraft:blackstone","weight":40}],"rolls":1.0}],"random_sequence":"minecraft:gameplay/piglin_bartering"}"##),
    ("minecraft:gameplay/sniffer_digging", r##"{"type":"minecraft:gift","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:torchflower_seeds"},{"type":"minecraft:item","name":"minecraft:pitcher_pod"}],"rolls":1.0}],"random_sequence":"minecraft:gameplay/sniffer_digging"}"##),
    ("minecraft:pots/trial_chambers/corridor", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:emerald","weight":125},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":8.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:arrow","weight":100},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:iron_ingot","weight":100},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:trial_key","weight":10},{"type":"minecraft:item","name":"minecraft:music_disc_creator_music_box","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:diamond","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:emerald_block","weight":5},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:diamond_block"}],"rolls":1.0}],"random_sequence":"minecraft:pots/trial_chambers/corridor"}"##),
    ("minecraft:shearing/bogged", r##"{"type":"minecraft:shearing","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:brown_mushroom"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:red_mushroom"}],"rolls":2.0}],"random_sequence":"minecraft:shearing/bogged"}"##),
    ("minecraft:spawners/ominous/trial_chamber/consumables", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:cooked_beef","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":4.0,"min":2.0},"function":"minecraft:set_count"}],"name":"minecraft:baked_potato","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":2.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:golden_carrot","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:regeneration"}],"name":"minecraft:potion"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:strength"}],"name":"minecraft:potion"}],"rolls":1.0}],"random_sequence":"minecraft:spawners/ominous/trial_chamber/consumables"}"##),
    ("minecraft:spawners/ominous/trial_chamber/key", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:ominous_trial_key"}],"rolls":1.0}],"random_sequence":"minecraft:spawners/ominous/trial_chamber/key"}"##),
    ("minecraft:spawners/trial_chamber/consumables", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:cooked_chicken","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:bread","weight":3},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:baked_potato","weight":2},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:regeneration"}],"name":"minecraft:potion"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:swiftness"}],"name":"minecraft:potion"}],"rolls":1.0}],"random_sequence":"minecraft:spawners/trial_chamber/consumables"}"##),
    ("minecraft:spawners/trial_chamber/items_to_drop_when_ominous", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:wind_charged"}],"name":"minecraft:lingering_potion"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:oozing"}],"name":"minecraft:lingering_potion"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:weaving"}],"name":"minecraft:lingering_potion"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:infested"}],"name":"minecraft:lingering_potion"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:strength"}],"name":"minecraft:lingering_potion"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:swiftness"}],"name":"minecraft:lingering_potion"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:slow_falling"}],"name":"minecraft:lingering_potion"}],"rolls":1.0},{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"}],"name":"minecraft:arrow"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:poison"}],"name":"minecraft:arrow"},{"type":"minecraft:item","functions":[{"add":false,"count":1.0,"function":"minecraft:set_count"},{"function":"minecraft:set_potion","id":"minecraft:strong_slowness"}],"name":"minecraft:arrow"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:fire_charge"},{"type":"minecraft:item","functions":[{"add":false,"count":{"type":"minecraft:uniform","max":3.0,"min":1.0},"function":"minecraft:set_count"}],"name":"minecraft:wind_charge"}],"rolls":1.0}],"random_sequence":"minecraft:spawners/trial_chamber/items_to_drop_when_ominous"}"##),
    ("minecraft:spawners/trial_chamber/key", r##"{"type":"minecraft:chest","pools":[{"bonus_rolls":0.0,"entries":[{"type":"minecraft:item","name":"minecraft:trial_key"}],"rolls":1.0}],"random_sequence":"minecraft:spawners/trial_chamber/key"}"##),
];

/// Look up a vanilla catalogue table body by `minecraft:`-prefixed id.
#[allow(dead_code)]
pub(crate) fn vanilla_loot_table_json(id: &str) -> Option<&'static str> {
    VANILLA_LOOT_TABLES
        .binary_search_by_key(&id, |(table, _)| *table)
        .ok()
        .map(|idx| VANILLA_LOOT_TABLES[idx].1)
}


/// Every item id the catalogue can emit (entry `name`s, tag expansions and
/// function-produced items), sorted for binary-search interning into the
/// `&'static str` `LootEntry.item` field.
#[allow(dead_code)]
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

#[allow(dead_code)]
fn vanilla_loot_item(name: &str) -> Option<&'static str> {
    VANILLA_LOOT_ITEMS
        .binary_search(&name)
        .ok()
        .map(|idx| VANILLA_LOOT_ITEMS[idx])
}


/// Enchantment ids the catalogue can record, sorted for interning into
/// `LootEntry.enchantment`.
#[allow(dead_code)]
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

#[allow(dead_code)]
fn vanilla_loot_enchantment(name: &str) -> Option<&'static str> {
    VANILLA_ENCHANTMENT_IDS
        .binary_search(&name)
        .ok()
        .map(|idx| VANILLA_ENCHANTMENT_IDS[idx])
}


/// `#minecraft:creeper_drop_music_discs` item-tag expansion (the only
/// `minecraft:tag` loot entry in the 1.21.1 tables).
#[allow(dead_code)]
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

/// Tag resolver for catalogue rolls: item tags only; unknown tags expand to
/// nothing, matching a missing tag in vanilla.
#[allow(dead_code)]
fn vanilla_loot_tags(tag: &str) -> Vec<&'static str> {
    match tag {
        "minecraft:creeper_drop_music_discs" => CREEPER_DROP_MUSIC_DISCS.to_vec(),
        _ => Vec::new(),
    }
}

/// `furnace_smelt` results for the raw items the 1.21.1 tables actually smelt
/// (meat/fish/potato/rabbit drops and kelp).
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
fn loot_items_to_entries(items: Vec<LootItem>, rng: &mut Lcg) -> Vec<LootEntry> {
    let mut slots: Vec<i16> = (0..27).collect();
    util_shuffle(&mut slots, rng);
    let mut out = Vec::new();
    for item in items {
        let count = item.count.clamp(0, i32::from(u8::MAX));
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
#[allow(dead_code)]
fn catalogue_rng_seed(seed: i64) -> u64 {
    let mut z = (seed as u64).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Deterministically roll a vanilla catalogue table: `evaluate_loot_table`
/// over the embedded JSON with an `Lcg` seeded by `seed`.
#[allow(dead_code)]
pub(crate) fn vanilla_loot_roll(id: &str, seed: i64) -> Vec<LootEntry> {
    let Some(json) = vanilla_loot_table_json(id) else {
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
    let items = evaluate_loot_table(&table, &LootContext::default(), &mut rng, &resolvers);
    loot_items_to_entries(items, &mut rng)
}

// Dedicated per-table generators: one function per loot-table base name so
// each vanilla table has a named producer. For the few base names shared by
// two tables (`bogged`, `consumables`, `corridor`, `desert_pyramid`, `key`)
// the function rolls the alphabetically-first table id; `vanilla_loot_roll`
// reaches every id exactly.

#[allow(dead_code)]
fn allay(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/allay", seed)
}

#[allow(dead_code)]
fn ancient_city(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/ancient_city", seed)
}

#[allow(dead_code)]
fn ancient_city_ice_box(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/ancient_city_ice_box", seed)
}

#[allow(dead_code)]
fn armadillo(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/armadillo", seed)
}

#[allow(dead_code)]
fn armor_stand(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/armor_stand", seed)
}

#[allow(dead_code)]
fn armorer_gift(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:gameplay/hero_of_the_village/armorer_gift", seed)
}

#[allow(dead_code)]
fn axolotl(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/axolotl", seed)
}

#[allow(dead_code)]
fn bastion_bridge(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/bastion_bridge", seed)
}

#[allow(dead_code)]
fn bastion_hoglin_stable(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/bastion_hoglin_stable", seed)
}

#[allow(dead_code)]
fn bastion_other(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/bastion_other", seed)
}

#[allow(dead_code)]
fn bastion_treasure(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/bastion_treasure", seed)
}

#[allow(dead_code)]
fn bat(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/bat", seed)
}

#[allow(dead_code)]
fn bee(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/bee", seed)
}

#[allow(dead_code)]
fn black(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/sheep/black", seed)
}

#[allow(dead_code)]
fn blaze(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/blaze", seed)
}

#[allow(dead_code)]
fn blue(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/sheep/blue", seed)
}

#[allow(dead_code)]
fn bogged(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/bogged", seed)
}

#[allow(dead_code)]
fn breeze(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/breeze", seed)
}

#[allow(dead_code)]
fn brown(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/sheep/brown", seed)
}

#[allow(dead_code)]
fn buried_treasure(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/buried_treasure", seed)
}

#[allow(dead_code)]
fn butcher_gift(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:gameplay/hero_of_the_village/butcher_gift", seed)
}

#[allow(dead_code)]
fn camel(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/camel", seed)
}

#[allow(dead_code)]
fn cartographer_gift(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:gameplay/hero_of_the_village/cartographer_gift", seed)
}

#[allow(dead_code)]
fn cat(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/cat", seed)
}

#[allow(dead_code)]
fn cat_morning_gift(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:gameplay/cat_morning_gift", seed)
}

#[allow(dead_code)]
fn cave_spider(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/cave_spider", seed)
}

#[allow(dead_code)]
fn chamber(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:dispensers/trial_chambers/chamber", seed)
}

#[allow(dead_code)]
fn chicken(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/chicken", seed)
}

#[allow(dead_code)]
fn cleric_gift(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:gameplay/hero_of_the_village/cleric_gift", seed)
}

#[allow(dead_code)]
fn cod(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/cod", seed)
}

#[allow(dead_code)]
fn consumables(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:spawners/ominous/trial_chamber/consumables", seed)
}

#[allow(dead_code)]
fn corridor(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/trial_chambers/corridor", seed)
}

#[allow(dead_code)]
fn cow(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/cow", seed)
}

#[allow(dead_code)]
fn creeper(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/creeper", seed)
}

#[allow(dead_code)]
fn cyan(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/sheep/cyan", seed)
}

#[allow(dead_code)]
fn desert_pyramid(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:archaeology/desert_pyramid", seed)
}

#[allow(dead_code)]
fn desert_well(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:archaeology/desert_well", seed)
}

#[allow(dead_code)]
fn dolphin(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/dolphin", seed)
}

#[allow(dead_code)]
fn donkey(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/donkey", seed)
}

#[allow(dead_code)]
fn drowned(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/drowned", seed)
}

#[allow(dead_code)]
fn elder_guardian(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/elder_guardian", seed)
}

#[allow(dead_code)]
fn end_city_treasure(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/end_city_treasure", seed)
}

#[allow(dead_code)]
fn ender_dragon(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/ender_dragon", seed)
}

#[allow(dead_code)]
fn enderman(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/enderman", seed)
}

#[allow(dead_code)]
fn endermite(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/endermite", seed)
}

#[allow(dead_code)]
fn entrance(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/trial_chambers/entrance", seed)
}

#[allow(dead_code)]
fn evoker(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/evoker", seed)
}

#[allow(dead_code)]
fn farmer_gift(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:gameplay/hero_of_the_village/farmer_gift", seed)
}

#[allow(dead_code)]
fn fish(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:gameplay/fishing/fish", seed)
}

#[allow(dead_code)]
fn fisherman_gift(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:gameplay/hero_of_the_village/fisherman_gift", seed)
}

#[allow(dead_code)]
fn fishing(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:gameplay/fishing", seed)
}

#[allow(dead_code)]
fn fletcher_gift(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:gameplay/hero_of_the_village/fletcher_gift", seed)
}

#[allow(dead_code)]
fn fox(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/fox", seed)
}

#[allow(dead_code)]
fn frog(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/frog", seed)
}

#[allow(dead_code)]
fn ghast(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/ghast", seed)
}

#[allow(dead_code)]
fn giant(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/giant", seed)
}

#[allow(dead_code)]
fn glow_squid(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/glow_squid", seed)
}

#[allow(dead_code)]
fn goat(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/goat", seed)
}

#[allow(dead_code)]
fn gray(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/sheep/gray", seed)
}

#[allow(dead_code)]
fn green(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/sheep/green", seed)
}

#[allow(dead_code)]
fn guardian(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/guardian", seed)
}

#[allow(dead_code)]
fn hoglin(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/hoglin", seed)
}

#[allow(dead_code)]
fn horse(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/horse", seed)
}

#[allow(dead_code)]
fn husk(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/husk", seed)
}

#[allow(dead_code)]
fn igloo_chest(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/igloo_chest", seed)
}

#[allow(dead_code)]
fn illusioner(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/illusioner", seed)
}

#[allow(dead_code)]
fn intersection(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/trial_chambers/intersection", seed)
}

#[allow(dead_code)]
fn intersection_barrel(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/trial_chambers/intersection_barrel", seed)
}

#[allow(dead_code)]
fn iron_golem(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/iron_golem", seed)
}

#[allow(dead_code)]
fn items_to_drop_when_ominous(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:spawners/trial_chamber/items_to_drop_when_ominous", seed)
}

#[allow(dead_code)]
fn jungle_temple(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/jungle_temple", seed)
}

#[allow(dead_code)]
fn jungle_temple_dispenser(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/jungle_temple_dispenser", seed)
}

#[allow(dead_code)]
fn junk(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:gameplay/fishing/junk", seed)
}

#[allow(dead_code)]
fn key(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:spawners/ominous/trial_chamber/key", seed)
}

#[allow(dead_code)]
fn leatherworker_gift(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:gameplay/hero_of_the_village/leatherworker_gift", seed)
}

#[allow(dead_code)]
fn librarian_gift(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:gameplay/hero_of_the_village/librarian_gift", seed)
}

#[allow(dead_code)]
fn light_blue(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/sheep/light_blue", seed)
}

#[allow(dead_code)]
fn light_gray(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/sheep/light_gray", seed)
}

#[allow(dead_code)]
fn lime(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/sheep/lime", seed)
}

#[allow(dead_code)]
fn llama(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/llama", seed)
}

#[allow(dead_code)]
fn magenta(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/sheep/magenta", seed)
}

#[allow(dead_code)]
fn magma_cube(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/magma_cube", seed)
}

#[allow(dead_code)]
fn mason_gift(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:gameplay/hero_of_the_village/mason_gift", seed)
}

#[allow(dead_code)]
fn mooshroom(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/mooshroom", seed)
}

#[allow(dead_code)]
fn mule(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/mule", seed)
}

#[allow(dead_code)]
fn nether_bridge(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/nether_bridge", seed)
}

#[allow(dead_code)]
fn ocean_ruin_cold(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:archaeology/ocean_ruin_cold", seed)
}

#[allow(dead_code)]
fn ocean_ruin_warm(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:archaeology/ocean_ruin_warm", seed)
}

#[allow(dead_code)]
fn ocelot(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/ocelot", seed)
}

#[allow(dead_code)]
fn orange(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/sheep/orange", seed)
}

#[allow(dead_code)]
fn panda(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/panda", seed)
}

#[allow(dead_code)]
fn panda_sneeze(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:gameplay/panda_sneeze", seed)
}

#[allow(dead_code)]
fn parrot(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/parrot", seed)
}

#[allow(dead_code)]
fn phantom(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/phantom", seed)
}

#[allow(dead_code)]
fn pig(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/pig", seed)
}

#[allow(dead_code)]
fn piglin(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/piglin", seed)
}

#[allow(dead_code)]
fn piglin_bartering(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:gameplay/piglin_bartering", seed)
}

#[allow(dead_code)]
fn piglin_brute(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/piglin_brute", seed)
}

#[allow(dead_code)]
fn pillager(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/pillager", seed)
}

#[allow(dead_code)]
fn pillager_outpost(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/pillager_outpost", seed)
}

#[allow(dead_code)]
fn pink(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/sheep/pink", seed)
}

#[allow(dead_code)]
fn player(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/player", seed)
}

#[allow(dead_code)]
fn polar_bear(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/polar_bear", seed)
}

#[allow(dead_code)]
fn pufferfish(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/pufferfish", seed)
}

#[allow(dead_code)]
fn purple(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/sheep/purple", seed)
}

#[allow(dead_code)]
fn rabbit(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/rabbit", seed)
}

#[allow(dead_code)]
fn ravager(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/ravager", seed)
}

#[allow(dead_code)]
fn red(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/sheep/red", seed)
}

#[allow(dead_code)]
fn reward(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/trial_chambers/reward", seed)
}

#[allow(dead_code)]
fn reward_common(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/trial_chambers/reward_common", seed)
}

#[allow(dead_code)]
fn reward_ominous(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/trial_chambers/reward_ominous", seed)
}

#[allow(dead_code)]
fn reward_ominous_common(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/trial_chambers/reward_ominous_common", seed)
}

#[allow(dead_code)]
fn reward_ominous_rare(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/trial_chambers/reward_ominous_rare", seed)
}

#[allow(dead_code)]
fn reward_ominous_unique(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/trial_chambers/reward_ominous_unique", seed)
}

#[allow(dead_code)]
fn reward_rare(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/trial_chambers/reward_rare", seed)
}

#[allow(dead_code)]
fn reward_unique(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/trial_chambers/reward_unique", seed)
}

#[allow(dead_code)]
fn ruined_portal(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/ruined_portal", seed)
}

#[allow(dead_code)]
fn salmon(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/salmon", seed)
}

#[allow(dead_code)]
fn sheep(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/sheep", seed)
}

#[allow(dead_code)]
fn shepherd_gift(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:gameplay/hero_of_the_village/shepherd_gift", seed)
}

#[allow(dead_code)]
fn shipwreck_map(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/shipwreck_map", seed)
}

#[allow(dead_code)]
fn shipwreck_supply(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/shipwreck_supply", seed)
}

#[allow(dead_code)]
fn shipwreck_treasure(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/shipwreck_treasure", seed)
}

#[allow(dead_code)]
fn shulker(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/shulker", seed)
}

#[allow(dead_code)]
fn silverfish(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/silverfish", seed)
}

#[allow(dead_code)]
fn simple_dungeon(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/simple_dungeon", seed)
}

#[allow(dead_code)]
fn skeleton(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/skeleton", seed)
}

#[allow(dead_code)]
fn skeleton_horse(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/skeleton_horse", seed)
}

#[allow(dead_code)]
fn slime(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/slime", seed)
}

#[allow(dead_code)]
fn sniffer(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/sniffer", seed)
}

#[allow(dead_code)]
fn sniffer_digging(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:gameplay/sniffer_digging", seed)
}

#[allow(dead_code)]
fn snow_golem(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/snow_golem", seed)
}

#[allow(dead_code)]
fn spawn_bonus_chest(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/spawn_bonus_chest", seed)
}

#[allow(dead_code)]
fn spider(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/spider", seed)
}

#[allow(dead_code)]
fn squid(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/squid", seed)
}

#[allow(dead_code)]
fn stray(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/stray", seed)
}

#[allow(dead_code)]
fn strider(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/strider", seed)
}

#[allow(dead_code)]
fn stronghold_corridor(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/stronghold_corridor", seed)
}

#[allow(dead_code)]
fn stronghold_crossing(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/stronghold_crossing", seed)
}

#[allow(dead_code)]
fn stronghold_library(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/stronghold_library", seed)
}

#[allow(dead_code)]
fn supply(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/trial_chambers/supply", seed)
}

#[allow(dead_code)]
fn tadpole(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/tadpole", seed)
}

#[allow(dead_code)]
fn toolsmith_gift(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:gameplay/hero_of_the_village/toolsmith_gift", seed)
}

#[allow(dead_code)]
fn trader_llama(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/trader_llama", seed)
}

#[allow(dead_code)]
fn trail_ruins_common(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:archaeology/trail_ruins_common", seed)
}

#[allow(dead_code)]
fn trail_ruins_rare(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:archaeology/trail_ruins_rare", seed)
}

#[allow(dead_code)]
fn treasure(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:gameplay/fishing/treasure", seed)
}

#[allow(dead_code)]
fn trial_chamber(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:equipment/trial_chamber", seed)
}

#[allow(dead_code)]
fn trial_chamber_melee(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:equipment/trial_chamber_melee", seed)
}

#[allow(dead_code)]
fn trial_chamber_ranged(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:equipment/trial_chamber_ranged", seed)
}

#[allow(dead_code)]
fn tropical_fish(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/tropical_fish", seed)
}

#[allow(dead_code)]
fn turtle(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/turtle", seed)
}

#[allow(dead_code)]
fn underwater_ruin_big(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/underwater_ruin_big", seed)
}

#[allow(dead_code)]
fn underwater_ruin_small(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/underwater_ruin_small", seed)
}

#[allow(dead_code)]
fn vex(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/vex", seed)
}

#[allow(dead_code)]
fn village_armorer(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/village/village_armorer", seed)
}

#[allow(dead_code)]
fn village_butcher(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/village/village_butcher", seed)
}

#[allow(dead_code)]
fn village_cartographer(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/village/village_cartographer", seed)
}

#[allow(dead_code)]
fn village_desert_house(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/village/village_desert_house", seed)
}

#[allow(dead_code)]
fn village_fisher(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/village/village_fisher", seed)
}

#[allow(dead_code)]
fn village_fletcher(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/village/village_fletcher", seed)
}

#[allow(dead_code)]
fn village_mason(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/village/village_mason", seed)
}

#[allow(dead_code)]
fn village_plains_house(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/village/village_plains_house", seed)
}

#[allow(dead_code)]
fn village_savanna_house(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/village/village_savanna_house", seed)
}

#[allow(dead_code)]
fn village_shepherd(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/village/village_shepherd", seed)
}

#[allow(dead_code)]
fn village_snowy_house(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/village/village_snowy_house", seed)
}

#[allow(dead_code)]
fn village_taiga_house(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/village/village_taiga_house", seed)
}

#[allow(dead_code)]
fn village_tannery(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/village/village_tannery", seed)
}

#[allow(dead_code)]
fn village_temple(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/village/village_temple", seed)
}

#[allow(dead_code)]
fn village_toolsmith(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/village/village_toolsmith", seed)
}

#[allow(dead_code)]
fn village_weaponsmith(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/village/village_weaponsmith", seed)
}

#[allow(dead_code)]
fn villager(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/villager", seed)
}

#[allow(dead_code)]
fn vindicator(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/vindicator", seed)
}

#[allow(dead_code)]
fn wandering_trader(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/wandering_trader", seed)
}

#[allow(dead_code)]
fn warden(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/warden", seed)
}

#[allow(dead_code)]
fn water(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:dispensers/trial_chambers/water", seed)
}

#[allow(dead_code)]
fn weaponsmith_gift(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:gameplay/hero_of_the_village/weaponsmith_gift", seed)
}

#[allow(dead_code)]
fn white(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/sheep/white", seed)
}

#[allow(dead_code)]
fn witch(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/witch", seed)
}

#[allow(dead_code)]
fn wither(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/wither", seed)
}

#[allow(dead_code)]
fn wither_skeleton(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/wither_skeleton", seed)
}

#[allow(dead_code)]
fn wolf(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/wolf", seed)
}

#[allow(dead_code)]
fn woodland_mansion(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:chests/woodland_mansion", seed)
}

#[allow(dead_code)]
fn yellow(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/sheep/yellow", seed)
}

#[allow(dead_code)]
fn zoglin(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/zoglin", seed)
}

#[allow(dead_code)]
fn zombie(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/zombie", seed)
}

#[allow(dead_code)]
fn zombie_horse(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/zombie_horse", seed)
}

#[allow(dead_code)]
fn zombie_villager(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/zombie_villager", seed)
}

#[allow(dead_code)]
fn zombified_piglin(seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll("minecraft:entities/zombified_piglin", seed)
}


/// `(canonical vanilla table id, dedicated generator)` pair.
#[allow(dead_code)]
type VanillaLootGenerator = (&'static str, fn(i64) -> Vec<LootEntry>);

/// `(canonical vanilla table id, dedicated generator)` registry.
#[allow(dead_code)]
pub(crate) static VANILLA_LOOT_GENERATORS: &[VanillaLootGenerator] = &[
    ("minecraft:archaeology/desert_pyramid", desert_pyramid),
    ("minecraft:archaeology/desert_well", desert_well),
    ("minecraft:archaeology/ocean_ruin_cold", ocean_ruin_cold),
    ("minecraft:archaeology/ocean_ruin_warm", ocean_ruin_warm),
    ("minecraft:archaeology/trail_ruins_common", trail_ruins_common),
    ("minecraft:archaeology/trail_ruins_rare", trail_ruins_rare),
    ("minecraft:chests/ancient_city", ancient_city),
    ("minecraft:chests/ancient_city_ice_box", ancient_city_ice_box),
    ("minecraft:chests/bastion_bridge", bastion_bridge),
    ("minecraft:chests/bastion_hoglin_stable", bastion_hoglin_stable),
    ("minecraft:chests/bastion_other", bastion_other),
    ("minecraft:chests/bastion_treasure", bastion_treasure),
    ("minecraft:chests/buried_treasure", buried_treasure),
    ("minecraft:chests/end_city_treasure", end_city_treasure),
    ("minecraft:chests/igloo_chest", igloo_chest),
    ("minecraft:chests/jungle_temple", jungle_temple),
    ("minecraft:chests/jungle_temple_dispenser", jungle_temple_dispenser),
    ("minecraft:chests/nether_bridge", nether_bridge),
    ("minecraft:chests/pillager_outpost", pillager_outpost),
    ("minecraft:chests/ruined_portal", ruined_portal),
    ("minecraft:chests/shipwreck_map", shipwreck_map),
    ("minecraft:chests/shipwreck_supply", shipwreck_supply),
    ("minecraft:chests/shipwreck_treasure", shipwreck_treasure),
    ("minecraft:chests/simple_dungeon", simple_dungeon),
    ("minecraft:chests/spawn_bonus_chest", spawn_bonus_chest),
    ("minecraft:chests/stronghold_corridor", stronghold_corridor),
    ("minecraft:chests/stronghold_crossing", stronghold_crossing),
    ("minecraft:chests/stronghold_library", stronghold_library),
    ("minecraft:chests/trial_chambers/corridor", corridor),
    ("minecraft:chests/trial_chambers/entrance", entrance),
    ("minecraft:chests/trial_chambers/intersection", intersection),
    ("minecraft:chests/trial_chambers/intersection_barrel", intersection_barrel),
    ("minecraft:chests/trial_chambers/reward", reward),
    ("minecraft:chests/trial_chambers/reward_common", reward_common),
    ("minecraft:chests/trial_chambers/reward_ominous", reward_ominous),
    ("minecraft:chests/trial_chambers/reward_ominous_common", reward_ominous_common),
    ("minecraft:chests/trial_chambers/reward_ominous_rare", reward_ominous_rare),
    ("minecraft:chests/trial_chambers/reward_ominous_unique", reward_ominous_unique),
    ("minecraft:chests/trial_chambers/reward_rare", reward_rare),
    ("minecraft:chests/trial_chambers/reward_unique", reward_unique),
    ("minecraft:chests/trial_chambers/supply", supply),
    ("minecraft:chests/underwater_ruin_big", underwater_ruin_big),
    ("minecraft:chests/underwater_ruin_small", underwater_ruin_small),
    ("minecraft:chests/village/village_armorer", village_armorer),
    ("minecraft:chests/village/village_butcher", village_butcher),
    ("minecraft:chests/village/village_cartographer", village_cartographer),
    ("minecraft:chests/village/village_desert_house", village_desert_house),
    ("minecraft:chests/village/village_fisher", village_fisher),
    ("minecraft:chests/village/village_fletcher", village_fletcher),
    ("minecraft:chests/village/village_mason", village_mason),
    ("minecraft:chests/village/village_plains_house", village_plains_house),
    ("minecraft:chests/village/village_savanna_house", village_savanna_house),
    ("minecraft:chests/village/village_shepherd", village_shepherd),
    ("minecraft:chests/village/village_snowy_house", village_snowy_house),
    ("minecraft:chests/village/village_taiga_house", village_taiga_house),
    ("minecraft:chests/village/village_tannery", village_tannery),
    ("minecraft:chests/village/village_temple", village_temple),
    ("minecraft:chests/village/village_toolsmith", village_toolsmith),
    ("minecraft:chests/village/village_weaponsmith", village_weaponsmith),
    ("minecraft:chests/woodland_mansion", woodland_mansion),
    ("minecraft:dispensers/trial_chambers/chamber", chamber),
    ("minecraft:dispensers/trial_chambers/water", water),
    ("minecraft:entities/allay", allay),
    ("minecraft:entities/armadillo", armadillo),
    ("minecraft:entities/armor_stand", armor_stand),
    ("minecraft:entities/axolotl", axolotl),
    ("minecraft:entities/bat", bat),
    ("minecraft:entities/bee", bee),
    ("minecraft:entities/blaze", blaze),
    ("minecraft:entities/bogged", bogged),
    ("minecraft:entities/breeze", breeze),
    ("minecraft:entities/camel", camel),
    ("minecraft:entities/cat", cat),
    ("minecraft:entities/cave_spider", cave_spider),
    ("minecraft:entities/chicken", chicken),
    ("minecraft:entities/cod", cod),
    ("minecraft:entities/cow", cow),
    ("minecraft:entities/creeper", creeper),
    ("minecraft:entities/dolphin", dolphin),
    ("minecraft:entities/donkey", donkey),
    ("minecraft:entities/drowned", drowned),
    ("minecraft:entities/elder_guardian", elder_guardian),
    ("minecraft:entities/ender_dragon", ender_dragon),
    ("minecraft:entities/enderman", enderman),
    ("minecraft:entities/endermite", endermite),
    ("minecraft:entities/evoker", evoker),
    ("minecraft:entities/fox", fox),
    ("minecraft:entities/frog", frog),
    ("minecraft:entities/ghast", ghast),
    ("minecraft:entities/giant", giant),
    ("minecraft:entities/glow_squid", glow_squid),
    ("minecraft:entities/goat", goat),
    ("minecraft:entities/guardian", guardian),
    ("minecraft:entities/hoglin", hoglin),
    ("minecraft:entities/horse", horse),
    ("minecraft:entities/husk", husk),
    ("minecraft:entities/illusioner", illusioner),
    ("minecraft:entities/iron_golem", iron_golem),
    ("minecraft:entities/llama", llama),
    ("minecraft:entities/magma_cube", magma_cube),
    ("minecraft:entities/mooshroom", mooshroom),
    ("minecraft:entities/mule", mule),
    ("minecraft:entities/ocelot", ocelot),
    ("minecraft:entities/panda", panda),
    ("minecraft:entities/parrot", parrot),
    ("minecraft:entities/phantom", phantom),
    ("minecraft:entities/pig", pig),
    ("minecraft:entities/piglin", piglin),
    ("minecraft:entities/piglin_brute", piglin_brute),
    ("minecraft:entities/pillager", pillager),
    ("minecraft:entities/player", player),
    ("minecraft:entities/polar_bear", polar_bear),
    ("minecraft:entities/pufferfish", pufferfish),
    ("minecraft:entities/rabbit", rabbit),
    ("minecraft:entities/ravager", ravager),
    ("minecraft:entities/salmon", salmon),
    ("minecraft:entities/sheep", sheep),
    ("minecraft:entities/sheep/black", black),
    ("minecraft:entities/sheep/blue", blue),
    ("minecraft:entities/sheep/brown", brown),
    ("minecraft:entities/sheep/cyan", cyan),
    ("minecraft:entities/sheep/gray", gray),
    ("minecraft:entities/sheep/green", green),
    ("minecraft:entities/sheep/light_blue", light_blue),
    ("minecraft:entities/sheep/light_gray", light_gray),
    ("minecraft:entities/sheep/lime", lime),
    ("minecraft:entities/sheep/magenta", magenta),
    ("minecraft:entities/sheep/orange", orange),
    ("minecraft:entities/sheep/pink", pink),
    ("minecraft:entities/sheep/purple", purple),
    ("minecraft:entities/sheep/red", red),
    ("minecraft:entities/sheep/white", white),
    ("minecraft:entities/sheep/yellow", yellow),
    ("minecraft:entities/shulker", shulker),
    ("minecraft:entities/silverfish", silverfish),
    ("minecraft:entities/skeleton", skeleton),
    ("minecraft:entities/skeleton_horse", skeleton_horse),
    ("minecraft:entities/slime", slime),
    ("minecraft:entities/sniffer", sniffer),
    ("minecraft:entities/snow_golem", snow_golem),
    ("minecraft:entities/spider", spider),
    ("minecraft:entities/squid", squid),
    ("minecraft:entities/stray", stray),
    ("minecraft:entities/strider", strider),
    ("minecraft:entities/tadpole", tadpole),
    ("minecraft:entities/trader_llama", trader_llama),
    ("minecraft:entities/tropical_fish", tropical_fish),
    ("minecraft:entities/turtle", turtle),
    ("minecraft:entities/vex", vex),
    ("minecraft:entities/villager", villager),
    ("minecraft:entities/vindicator", vindicator),
    ("minecraft:entities/wandering_trader", wandering_trader),
    ("minecraft:entities/warden", warden),
    ("minecraft:entities/witch", witch),
    ("minecraft:entities/wither", wither),
    ("minecraft:entities/wither_skeleton", wither_skeleton),
    ("minecraft:entities/wolf", wolf),
    ("minecraft:entities/zoglin", zoglin),
    ("minecraft:entities/zombie", zombie),
    ("minecraft:entities/zombie_horse", zombie_horse),
    ("minecraft:entities/zombie_villager", zombie_villager),
    ("minecraft:entities/zombified_piglin", zombified_piglin),
    ("minecraft:equipment/trial_chamber", trial_chamber),
    ("minecraft:equipment/trial_chamber_melee", trial_chamber_melee),
    ("minecraft:equipment/trial_chamber_ranged", trial_chamber_ranged),
    ("minecraft:gameplay/cat_morning_gift", cat_morning_gift),
    ("minecraft:gameplay/fishing", fishing),
    ("minecraft:gameplay/fishing/fish", fish),
    ("minecraft:gameplay/fishing/junk", junk),
    ("minecraft:gameplay/fishing/treasure", treasure),
    ("minecraft:gameplay/hero_of_the_village/armorer_gift", armorer_gift),
    ("minecraft:gameplay/hero_of_the_village/butcher_gift", butcher_gift),
    ("minecraft:gameplay/hero_of_the_village/cartographer_gift", cartographer_gift),
    ("minecraft:gameplay/hero_of_the_village/cleric_gift", cleric_gift),
    ("minecraft:gameplay/hero_of_the_village/farmer_gift", farmer_gift),
    ("minecraft:gameplay/hero_of_the_village/fisherman_gift", fisherman_gift),
    ("minecraft:gameplay/hero_of_the_village/fletcher_gift", fletcher_gift),
    ("minecraft:gameplay/hero_of_the_village/leatherworker_gift", leatherworker_gift),
    ("minecraft:gameplay/hero_of_the_village/librarian_gift", librarian_gift),
    ("minecraft:gameplay/hero_of_the_village/mason_gift", mason_gift),
    ("minecraft:gameplay/hero_of_the_village/shepherd_gift", shepherd_gift),
    ("minecraft:gameplay/hero_of_the_village/toolsmith_gift", toolsmith_gift),
    ("minecraft:gameplay/hero_of_the_village/weaponsmith_gift", weaponsmith_gift),
    ("minecraft:gameplay/panda_sneeze", panda_sneeze),
    ("minecraft:gameplay/piglin_bartering", piglin_bartering),
    ("minecraft:gameplay/sniffer_digging", sniffer_digging),
    ("minecraft:spawners/ominous/trial_chamber/consumables", consumables),
    ("minecraft:spawners/ominous/trial_chamber/key", key),
    ("minecraft:spawners/trial_chamber/items_to_drop_when_ominous", items_to_drop_when_ominous),
];

/// Roll a catalogue table through its dedicated generator when one exists,
/// falling back to `vanilla_loot_roll` for shared-base secondary tables.
#[allow(dead_code)]
pub(crate) fn vanilla_loot_generate(id: &str, seed: i64) -> Vec<LootEntry> {
    if let Ok(idx) = VANILLA_LOOT_GENERATORS.binary_search_by_key(&id, |(table, _)| *table) {
        return VANILLA_LOOT_GENERATORS[idx].1(seed);
    }
    vanilla_loot_roll(id, seed)
}


#[cfg(all(test, megu_vanilla_data))]
#[path = "chest_loot_tests.rs"]
mod tests;
