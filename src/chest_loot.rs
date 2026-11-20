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
}

impl LootItem {
    fn new(item: &str) -> Self {
        LootItem {
            item: item.to_string(),
            count: 1,
            enchantment: None,
            damage: 0,
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

/// `minecraft:entity_properties`: this slice implements the `flags`
/// predicate (on_fire / is_baby); other predicate keys are not evaluated.
fn entity_properties(condition: &J, ctx: &LootContext) -> bool {
    let entity = jstr(condition, "entity").unwrap_or_else(|| "this".to_string());
    let Some(predicate) = condition.get("predicate") else {
        return false;
    };
    let Some(flags) = predicate.get("flags") else {
        return true;
    };
    let (on_fire, is_baby) = match entity.as_str() {
        "this" => (ctx.this_on_fire, ctx.this_is_baby),
        _ => (ctx.attacker_on_fire, ctx.attacker_is_baby),
    };
    flag_matches(flags, "is_on_fire", on_fire) && flag_matches(flags, "is_baby", is_baby)
}

/// Evaluate one loot condition. Unknown conditions fail closed.
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
        "any_of" => jarr(condition, "terms")
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
        _ => {}
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
        _ => true,
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
    let Some(pools) = jarr(table, "pools") else {
        return out;
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
    out
}

#[cfg(test)]
#[path = "chest_loot_tests.rs"]
mod tests;
