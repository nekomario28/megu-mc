//! Seeded 1.21.1 `minecraft:chests/abandoned_mineshaft` chest-loot table fill.
//! Produces exact vanilla `(slot, item, count, stored_enchantment)` entries from
//! a 48-bit `Lcg` seed without depending on runtime inventory or network types.
use crate::generation_math::Lcg;

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

#[cfg(test)]
#[path = "chest_loot_tests.rs"]
mod tests;
