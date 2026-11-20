// R7 GP-5: player mechanisms for Java Edition 1.21.1.
// Included at the crate root by main.rs.

#[cfg_attr(not(test), allow(dead_code))]
fn count_enchanting_bookshelves(
    (x, y, z): (i32, i32, i32),
    mut is_bookshelf: impl FnMut(i32, i32, i32) -> bool,
    mut transmits_power: impl FnMut(i32, i32, i32) -> bool,
) -> i32 {
    let mut count = 0;
    for dx in -2i32..=2 {
        for dz in -2i32..=2 {
            if dx.abs() != 2 && dz.abs() != 2 {
                continue;
            }
            for dy in 0..=1 {
                // Java integer division truncates toward zero for negative offsets.
                if is_bookshelf(x + dx, y + dy, z + dz)
                    && transmits_power(x + dx / 2, y + dy, z + dz / 2)
                {
                    count += 1;
                    if count == 15 {
                        return count;
                    }
                }
            }
        }
    }
    count
}

#[cfg_attr(not(test), allow(dead_code))]
fn enchantment_cost(
    rng: &mut generation_math::Lcg,
    slot: usize,
    bookshelves: i32,
    enchantability: i32,
) -> i32 {
    if enchantability <= 0 || slot >= 3 {
        return 0;
    }
    let shelves = bookshelves.clamp(0, 15);
    let base = rng.next_bound(8) + 1 + (shelves >> 1) + rng.next_bound(shelves + 1);
    match slot {
        0 => (base / 3).max(1),
        1 => base * 2 / 3 + 1,
        _ => base.max(shelves * 2),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy)]
enum EnchantmentItems {
    Armor,
    Head,
    Chest,
    Legs,
    Feet,
    Sword,
    SharpWeapon,
    Weapon,
    FireAspect,
    Mining,
    MiningLoot,
    Bow,
    Crossbow,
    Fishing,
    Trident,
    Mace,
    Durable,
    Equippable,
    Vanishing,
}

impl EnchantmentItems {
    #[cfg_attr(not(test), allow(dead_code))]
    fn contains(self, item: &str) -> bool {
        let armor = armor_parts(item).is_some();
        let tool = tool_parts(item).map(|(_, kind)| kind);
        let sword = tool == Some("sword");
        let axe = tool == Some("axe");
        let mining_tool = tool.is_some() && !sword;
        let skull = matches!(
            item,
            "player_head"
                | "creeper_head"
                | "zombie_head"
                | "skeleton_skull"
                | "wither_skeleton_skull"
                | "dragon_head"
                | "piglin_head"
        );
        match self {
            Self::Armor => armor,
            Self::Head => armor && item.ends_with("_helmet"),
            Self::Chest => armor && item.ends_with("_chestplate"),
            Self::Legs => armor && item.ends_with("_leggings"),
            Self::Feet => armor && item.ends_with("_boots"),
            Self::Sword => sword,
            Self::SharpWeapon => sword || axe,
            Self::Weapon => sword || axe || item == "mace",
            Self::FireAspect => sword || item == "mace",
            Self::Mining => mining_tool || item == "shears",
            Self::MiningLoot => mining_tool,
            Self::Bow => item == "bow",
            Self::Crossbow => item == "crossbow",
            Self::Fishing => item == "fishing_rod",
            Self::Trident => item == "trident",
            Self::Mace => item == "mace",
            Self::Durable => {
                armor
                    || sword
                    || mining_tool
                    || matches!(
                        item,
                        "elytra"
                            | "shield"
                            | "bow"
                            | "crossbow"
                            | "trident"
                            | "flint_and_steel"
                            | "shears"
                            | "brush"
                            | "fishing_rod"
                            | "carrot_on_a_stick"
                            | "warped_fungus_on_a_stick"
                            | "mace"
                    )
            }
            Self::Equippable => armor || skull || matches!(item, "elytra" | "carved_pumpkin"),
            Self::Vanishing => {
                Self::Durable.contains(item)
                    || skull
                    || matches!(item, "compass" | "carved_pumpkin")
            }
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn armor_parts(item: &str) -> Option<(&str, &str)> {
    let (material, piece) = item.split_once('_')?;
    if item == "turtle_helmet"
        || (matches!(
            material,
            "leather" | "chainmail" | "iron" | "golden" | "diamond" | "netherite"
        ) && matches!(piece, "helmet" | "chestplate" | "leggings" | "boots"))
    {
        Some((material, piece))
    } else {
        None
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn tool_parts(item: &str) -> Option<(&str, &str)> {
    let (tier, kind) = item.split_once('_')?;
    if matches!(
        tier,
        "wooden" | "stone" | "iron" | "golden" | "diamond" | "netherite"
    ) && matches!(kind, "sword" | "pickaxe" | "axe" | "shovel" | "hoe")
    {
        Some((tier, kind))
    } else {
        None
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn item_enchantability(item: &str) -> i32 {
    if let Some((tier, _)) = tool_parts(item) {
        return match tier {
            "wooden" | "netherite" => 15,
            "stone" => 5,
            "iron" => 14,
            "golden" => 22,
            "diamond" => 10,
            _ => 0,
        };
    }
    if let Some((material, _)) = armor_parts(item) {
        return match material {
            "leather" | "netherite" => 15,
            "chainmail" => 12,
            "iron" | "turtle" => 9,
            "golden" => 25,
            "diamond" => 10,
            _ => 0,
        };
    }
    match item {
        "book" | "bow" | "crossbow" | "fishing_rod" | "trident" => 1,
        "mace" => 15,
        _ => 0,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy)]
struct EnchantmentRule {
    name: &'static str,
    supported: EnchantmentItems,
    primary: EnchantmentItems,
    max_level: i32,
    anvil_cost: i32,
    table_weight: i32,
    min_cost: [i32; 2],
    max_cost: [i32; 2],
}

#[cfg_attr(not(test), allow(dead_code))]
const fn enchantment_rule(
    name: &'static str,
    supported: EnchantmentItems,
    primary: EnchantmentItems,
    (max_level, anvil_cost, table_weight): (i32, i32, i32),
    [min_base, min_step, max_base, max_step]: [i32; 4],
) -> EnchantmentRule {
    EnchantmentRule {
        name,
        supported,
        primary,
        max_level,
        anvil_cost,
        table_weight,
        min_cost: [min_base, min_step],
        max_cost: [max_base, max_step],
    }
}

// The table candidates are ordered by the 1.21.1 tag in_enchanting_table.
// The trailing entries have zero table weight and can only be combined here.
#[cfg_attr(not(test), allow(dead_code))]
#[rustfmt::skip]
const PLAYER_ENCHANTMENT_RULES: &[EnchantmentRule] = {
    use EnchantmentItems::*;
    &[
        // Name, supported items, primary items, (maximum level, anvil cost, weight),
        // [minimum base, minimum step, maximum base, maximum step].
        enchantment_rule("protection", Armor, Armor, (4, 1, 10), [1, 11, 12, 11]),
        enchantment_rule("fire_protection", Armor, Armor, (4, 2, 5), [10, 8, 18, 8]),
        enchantment_rule("feather_falling", Feet, Feet, (4, 2, 5), [5, 6, 11, 6]),
        enchantment_rule("blast_protection", Armor, Armor, (4, 4, 2), [5, 8, 13, 8]),
        enchantment_rule("projectile_protection", Armor, Armor, (4, 2, 5), [3, 6, 9, 6]),
        enchantment_rule("respiration", Head, Head, (3, 4, 2), [10, 10, 40, 10]),
        enchantment_rule("aqua_affinity", Head, Head, (1, 4, 2), [1, 0, 41, 0]),
        enchantment_rule("thorns", Armor, Chest, (3, 8, 1), [10, 20, 60, 20]),
        enchantment_rule("depth_strider", Feet, Feet, (3, 4, 2), [10, 10, 25, 10]),
        enchantment_rule("sharpness", SharpWeapon, Sword, (5, 1, 10), [1, 11, 21, 11]),
        enchantment_rule("smite", Weapon, Sword, (5, 2, 5), [5, 8, 25, 8]),
        enchantment_rule("bane_of_arthropods", Weapon, Sword, (5, 2, 5), [5, 8, 25, 8]),
        enchantment_rule("knockback", Sword, Sword, (2, 2, 5), [5, 20, 55, 20]),
        enchantment_rule("fire_aspect", FireAspect, Sword, (2, 4, 2), [10, 20, 60, 20]),
        enchantment_rule("looting", Sword, Sword, (3, 4, 2), [15, 9, 65, 9]),
        enchantment_rule("sweeping_edge", Sword, Sword, (3, 4, 2), [5, 9, 20, 9]),
        enchantment_rule("efficiency", Mining, Mining, (5, 1, 10), [1, 10, 51, 10]),
        enchantment_rule("silk_touch", MiningLoot, MiningLoot, (1, 8, 1), [15, 0, 65, 0]),
        enchantment_rule("unbreaking", Durable, Durable, (3, 2, 5), [5, 8, 55, 8]),
        enchantment_rule("fortune", MiningLoot, MiningLoot, (3, 4, 2), [15, 9, 65, 9]),
        enchantment_rule("power", Bow, Bow, (5, 1, 10), [1, 10, 16, 10]),
        enchantment_rule("punch", Bow, Bow, (2, 4, 2), [12, 20, 37, 20]),
        enchantment_rule("flame", Bow, Bow, (1, 4, 2), [20, 0, 50, 0]),
        enchantment_rule("infinity", Bow, Bow, (1, 8, 1), [20, 0, 50, 0]),
        enchantment_rule("luck_of_the_sea", Fishing, Fishing, (3, 4, 2), [15, 9, 65, 9]),
        enchantment_rule("lure", Fishing, Fishing, (3, 4, 2), [15, 9, 65, 9]),
        enchantment_rule("loyalty", Trident, Trident, (3, 2, 5), [12, 7, 50, 0]),
        enchantment_rule("impaling", Trident, Trident, (5, 4, 2), [1, 8, 21, 8]),
        enchantment_rule("riptide", Trident, Trident, (3, 4, 2), [17, 7, 50, 0]),
        enchantment_rule("channeling", Trident, Trident, (1, 8, 1), [25, 0, 50, 0]),
        enchantment_rule("multishot", Crossbow, Crossbow, (1, 4, 2), [20, 0, 50, 0]),
        enchantment_rule("quick_charge", Crossbow, Crossbow, (3, 2, 5), [12, 20, 50, 0]),
        enchantment_rule("piercing", Crossbow, Crossbow, (4, 1, 10), [1, 10, 50, 0]),
        enchantment_rule("density", Mace, Mace, (5, 2, 5), [5, 8, 25, 8]),
        enchantment_rule("breach", Mace, Mace, (4, 4, 2), [15, 9, 65, 9]),
        enchantment_rule("binding_curse", Equippable, Equippable, (1, 8, 0), [0; 4]),
        enchantment_rule("frost_walker", Feet, Feet, (2, 4, 0), [0; 4]),
        enchantment_rule("mending", Durable, Durable, (1, 4, 0), [0; 4]),
        enchantment_rule("soul_speed", Feet, Feet, (3, 8, 0), [0; 4]),
        enchantment_rule("swift_sneak", Legs, Legs, (3, 8, 0), [0; 4]),
        enchantment_rule("vanishing_curse", Vanishing, Vanishing, (1, 8, 0), [0; 4]),
        enchantment_rule("wind_burst", Mace, Mace, (3, 4, 0), [0; 4]),
    ]
};

#[cfg_attr(not(test), allow(dead_code))]
fn player_enchantments_conflict(a: i64, b: i64) -> bool {
    // The 1.21.1 riptide exclusive set applies only to Riptide itself.
    if matches!((a, b), (5, 19) | (19, 5)) {
        return false;
    }
    // Impaling is also part of the damage exclusive set.
    let impaling_conflict = |other| matches!(other, 1 | 4 | 6 | 32 | 34);
    (a == 15 && impaling_conflict(b))
        || (b == 15 && impaling_conflict(a))
        || enchant_conflicts(a, b)
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
struct EnchantmentOffer {
    required_level: i32,
    enchantments: Vec<(i64, i64)>,
    clue: Option<(i64, i64)>,
}

#[cfg_attr(not(test), allow(dead_code))]
fn enchanting_offers(stack: &Stack, seed: i32, bookshelves: i32) -> [EnchantmentOffer; 3] {
    let item = item_to_name(stack.0).unwrap_or_default();
    let enchantability = if stack.1 == 1 && enchantments_get(&stack.2).is_empty() {
        item_enchantability(item)
    } else {
        0
    };
    let mut cost_rng = generation_math::Lcg::new(i64::from(seed) as u64);
    std::array::from_fn(|slot| {
        let mut required_level = enchantment_cost(&mut cost_rng, slot, bookshelves, enchantability);
        if required_level < slot as i32 + 1 {
            required_level = 0;
        }
        let mut rng = generation_math::Lcg::new(i64::from(seed.wrapping_add(slot as i32)) as u64);
        let enchantments = if required_level > 0 {
            roll_table_enchantments(&mut rng, item, enchantability, required_level)
        } else {
            Vec::new()
        };
        let clue = if enchantments.is_empty() {
            None
        } else {
            Some(enchantments[rng.next_bound(enchantments.len() as i32) as usize])
        };
        EnchantmentOffer {
            required_level,
            enchantments,
            clue,
        }
    })
}

#[cfg_attr(not(test), allow(dead_code))]
fn roll_table_enchantments(
    rng: &mut generation_math::Lcg,
    item: &str,
    enchantability: i32,
    required_level: i32,
) -> Vec<(i64, i64)> {
    let bonus_bound = enchantability / 4 + 1;
    let adjusted = required_level + 1 + rng.next_bound(bonus_bound) + rng.next_bound(bonus_bound);
    let spread = (rng.next_f32() + rng.next_f32() - 1.0) * 0.15;
    let adjusted = adjusted as f32;
    let mut power = (adjusted + adjusted * spread).round().max(1.0) as i32;
    let mut candidates = Vec::new();
    for rule in PLAYER_ENCHANTMENT_RULES {
        if rule.table_weight == 0 || (item != "book" && !rule.primary.contains(item)) {
            continue;
        }
        for level in (1..=rule.max_level).rev() {
            let min = rule.min_cost[0] + (level - 1) * rule.min_cost[1];
            let max = rule.max_cost[0] + (level - 1) * rule.max_cost[1];
            if (min..=max).contains(&power) {
                candidates.push((
                    enchant_id(rule.name).expect("vanilla enchantment"),
                    i64::from(level),
                    rule.table_weight,
                ));
                break;
            }
        }
    }

    let mut selected = Vec::new();
    while !candidates.is_empty() {
        let mut draw = rng.next_bound(candidates.iter().map(|(_, _, weight)| *weight).sum());
        let (id, level, _) = *candidates
            .iter()
            .find(|(_, _, weight)| {
                draw -= *weight;
                draw < 0
            })
            .expect("positive enchantment weights");
        selected.push((id, level));
        if rng.next_bound(50) > power {
            break;
        }
        candidates
            .retain(|(other, _, _)| *other != id && !player_enchantments_conflict(id, *other));
        power /= 2;
    }
    if item == "book" && selected.len() > 1 {
        let index = rng.next_bound(selected.len() as i32) as usize;
        selected.remove(index);
    }
    selected
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
struct EnchantingPlayer {
    level: i32,
    lapis: u8,
    enchantment_seed: i32,
}

impl EnchantingPlayer {
    #[cfg_attr(not(test), allow(dead_code))]
    fn enchant(
        &mut self,
        stack: &mut Stack,
        slot: usize,
        bookshelves: i32,
        creative: bool,
        player_rng: &mut generation_math::Lcg,
    ) -> bool {
        if slot >= 3 {
            return false;
        }
        let offers = enchanting_offers(stack, self.enchantment_seed, bookshelves);
        let offer = &offers[slot];
        let price = slot as u8 + 1;
        if offer.enchantments.is_empty()
            || (!creative
                && (self.level < offer.required_level
                    || self.level < i32::from(price)
                    || self.lapis < price))
        {
            return false;
        }
        self.level = self.level.saturating_sub(i32::from(price)).max(0);
        self.enchantment_seed = player_rng.next_i32();
        if !creative {
            self.lapis -= price;
        }
        let component = if item_to_name(stack.0) == Some("book") {
            stack.0 = name_to_item("enchanted_book").expect("vanilla enchanted book");
            COMP_STORED_ENCHANTMENTS
        } else {
            COMP_ENCHANTMENTS
        };
        comp_set(
            &mut stack.2,
            component,
            enchantments_data(&offer.enchantments, true),
        );
        true
    }
}

#[cfg_attr(not(test), allow(dead_code))]
const COMP_PRIOR_WORK: i64 = 16;

#[cfg_attr(not(test), allow(dead_code))]
fn prior_work_cost(repair_cost_nbt: i32) -> i32 {
    repair_cost_nbt
}

#[cfg_attr(not(test), allow(dead_code))]
fn next_repair_cost(left_cost: i32, right_cost: i32) -> i32 {
    left_cost.max(right_cost).wrapping_mul(2).wrapping_add(1)
}

#[cfg_attr(not(test), allow(dead_code))]
fn anvil_max_damage(stack: &Stack) -> i64 {
    if comp_get(&stack.2, COMP_UNBREAKABLE).is_some() {
        return 0;
    }
    comp_varint(&stack.2, COMP_MAX_DAMAGE)
        .or_else(|| tool_max_damage(stack.0))
        .or_else(|| armor_max_damage(stack.0))
        .or_else(|| (item_to_name(stack.0) == Some("wolf_armor")).then_some(64))
        .unwrap_or(0)
        .clamp(0, i64::from(i32::MAX))
}

#[cfg_attr(not(test), allow(dead_code))]
fn is_anvil_repair_material(item: &str, material: &str) -> bool {
    if let Some((tier, _)) = tool_parts(item) {
        return match tier {
            "wooden" => material.ends_with("_planks"),
            "stone" => matches!(material, "cobblestone" | "blackstone" | "cobbled_deepslate"),
            "iron" => material == "iron_ingot",
            "golden" => material == "gold_ingot",
            "diamond" => material == "diamond",
            "netherite" => material == "netherite_ingot",
            _ => false,
        };
    }
    if let Some((tier, _)) = armor_parts(item) {
        return match tier {
            "leather" => material == "leather",
            "chainmail" | "iron" => material == "iron_ingot",
            "golden" => material == "gold_ingot",
            "diamond" => material == "diamond",
            "netherite" => material == "netherite_ingot",
            "turtle" => material == "turtle_scute",
            _ => false,
        };
    }
    match item {
        "elytra" => material == "phantom_membrane",
        "shield" => material.ends_with("_planks"),
        "mace" => material == "breeze_rod",
        "wolf_armor" => material == "armadillo_scute",
        _ => false,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn anvil_enchantments(stack: &Stack) -> Vec<(i64, i64)> {
    if item_to_name(stack.0) != Some("enchanted_book") {
        return enchantments_get(&stack.2);
    }
    let Some(mut data) = comp_get(&stack.2, COMP_STORED_ENCHANTMENTS) else {
        return Vec::new();
    };
    let count = read_varint(&mut data).unwrap_or_default().min(64);
    (0..count)
        .map_while(|_| Some((read_varint(&mut data)?, read_varint(&mut data)?)))
        .collect()
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
struct AnvilResult {
    stack: Stack,
    level_cost: i32,
    right_consumed: u8,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
enum AnvilError {
    InvalidInput,
    TooExpensive(i32),
}

#[cfg_attr(not(test), allow(dead_code))]
fn merge_anvil_enchantments(
    output: &mut Stack,
    sacrifice: &Stack,
    item: &str,
    from_book: bool,
    creative: bool,
) -> Result<i64, AnvilError> {
    let mut enchantments = anvil_enchantments(output);
    let mut cost = 0;
    let mut accepted = false;
    let mut rejected = false;
    for (incoming, incoming_level) in anvil_enchantments(sacrifice) {
        let conflicts = enchantments
            .iter()
            .filter(|(existing, _)| player_enchantments_conflict(*existing, incoming))
            .count() as i64;
        cost += conflicts;
        let Some(rule) = PLAYER_ENCHANTMENT_RULES
            .iter()
            .find(|rule| enchant_id(rule.name) == Some(incoming))
        else {
            rejected = true;
            continue;
        };
        if conflicts > 0 || !(creative || item == "enchanted_book" || rule.supported.contains(item))
        {
            rejected = true;
            continue;
        }
        let existing = enchantments.iter_mut().find(|(id, _)| *id == incoming);
        let previous = existing.as_ref().map_or(0, |entry| entry.1);
        let level = if previous == incoming_level {
            incoming_level.saturating_add(1)
        } else {
            previous.max(incoming_level)
        }
        .min(i64::from(rule.max_level));

        if let Some(entry) = existing {
            entry.1 = level;
        } else {
            enchantments.push((incoming, level));
        }
        let weight = if from_book {
            (rule.anvil_cost / 2).max(1)
        } else {
            rule.anvil_cost
        };
        cost += i64::from(weight) * level;
        accepted = true;
    }
    if !accepted && rejected {
        return Err(AnvilError::InvalidInput);
    }
    if accepted {
        let component = if item == "enchanted_book" {
            COMP_STORED_ENCHANTMENTS
        } else {
            COMP_ENCHANTMENTS
        };
        let show = comp_get(&output.2, component).and_then(|data| data.last()) != Some(&0);
        comp_set(
            &mut output.2,
            component,
            enchantments_data(&enchantments, show),
        );
        if output.1 > 1 {
            cost = 40;
        }
    }
    Ok(cost)
}

/// Preview a repair. A nonblank rename sets a custom name, and a blank rename clears it.
#[cfg_attr(not(test), allow(dead_code))]
fn anvil_result(
    left: &Stack,
    right: Option<&Stack>,
    rename: Option<&str>,
    creative: bool,
) -> Result<AnvilResult, AnvilError> {
    let item = item_to_name(left.0)
        .filter(|&item| left.1 > 0 && item != "air")
        .ok_or(AnvilError::InvalidInput)?;
    let mut output = left.clone();
    let left_work = comp_varint(&left.2, COMP_PRIOR_WORK).unwrap_or_default() as i32;
    let mut right_work = 0;
    let mut operation_cost = 0;
    let mut right_consumed = 0;

    if let Some(right) = right.filter(|stack| stack.1 > 0 && item_to_name(stack.0) != Some("air")) {
        right_work = comp_varint(&right.2, COMP_PRIOR_WORK).unwrap_or_default() as i32;
        let max_damage = anvil_max_damage(left);
        let damage = comp_varint(&left.2, COMP_DAMAGE)
            .unwrap_or_default()
            .clamp(0, max_damage);
        let material = item_to_name(right.0).unwrap_or_default();
        if max_damage > 0 && is_anvil_repair_material(item, material) {
            let unit = max_damage / 4;
            if damage == 0 || unit == 0 {
                return Err(AnvilError::InvalidInput);
            }
            let units = ((damage + unit - 1) / unit).min(i64::from(right.1));
            comp_set_varint(&mut output.2, COMP_DAMAGE, (damage - units * unit).max(0));
            operation_cost += units;
            right_consumed = units as u8;
        } else {
            let from_book = material == "enchanted_book"
                && comp_get(&right.2, COMP_STORED_ENCHANTMENTS).is_some();
            if !from_book && (left.0 != right.0 || max_damage == 0) {
                return Err(AnvilError::InvalidInput);
            }
            if !from_book {
                let right_max = anvil_max_damage(right);
                let right_damage = comp_varint(&right.2, COMP_DAMAGE)
                    .unwrap_or_default()
                    .clamp(0, right_max);
                let remaining =
                    max_damage - damage + right_max - right_damage + max_damage * 12 / 100;
                let combined_damage = (max_damage - remaining).max(0);
                if combined_damage < damage {
                    comp_set_varint(&mut output.2, COMP_DAMAGE, combined_damage);
                    operation_cost += 2;
                }
            }
            operation_cost +=
                merge_anvil_enchantments(&mut output, right, item, from_book, creative)?;
            right_consumed = right.1;
        }
    }

    let mut rename_cost = 0;
    if let Some(name) = rename {
        let new_name = (!name.trim().is_empty()).then(|| nbt_string(name));
        if comp_get(&output.2, COMP_CUSTOM_NAME) != new_name.as_deref() {
            rename_cost = 1;
            if let Some(name) = new_name {
                comp_set(&mut output.2, COMP_CUSTOM_NAME, name);
            } else {
                output
                    .2
                    .retain(|(component, _)| *component != COMP_CUSTOM_NAME);
            }
        }
    }
    if operation_cost + rename_cost == 0 {
        return Err(AnvilError::InvalidInput);
    }

    let prior = i64::from(prior_work_cost(left_work)) + i64::from(prior_work_cost(right_work));
    let mut level_cost =
        (prior + operation_cost + rename_cost).clamp(0, i64::from(i32::MAX)) as i32;
    if operation_cost == 0 {
        level_cost = level_cost.min(39);
    } else if level_cost >= 40 && !creative {
        return Err(AnvilError::TooExpensive(level_cost));
    }
    let repair_cost = if operation_cost == 0 {
        left_work.max(right_work)
    } else {
        next_repair_cost(left_work, right_work)
    };
    comp_set_varint(&mut output.2, COMP_PRIOR_WORK, i64::from(repair_cost));
    Ok(AnvilResult {
        stack: output,
        level_cost,
        right_consumed,
    })
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PotionKind {
    Water,
    Mundane,
    Thick,
    Awkward,
    NightVision,
    LongNightVision,
    Invisibility,
    LongInvisibility,
    Leaping,
    LongLeaping,
    StrongLeaping,
    FireResistance,
    LongFireResistance,
    Swiftness,
    LongSwiftness,
    StrongSwiftness,
    Slowness,
    LongSlowness,
    StrongSlowness,
    TurtleMaster,
    LongTurtleMaster,
    StrongTurtleMaster,
    WaterBreathing,
    LongWaterBreathing,
    Healing,
    StrongHealing,
    Harming,
    StrongHarming,
    Poison,
    LongPoison,
    StrongPoison,
    Regeneration,
    LongRegeneration,
    StrongRegeneration,
    Strength,
    LongStrength,
    StrongStrength,
    Weakness,
    LongWeakness,
    Luck,
    SlowFalling,
    LongSlowFalling,
    WindCharged,
    Oozing,
    Infested,
    Weaving,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PotionForm {
    Drinkable,
    Splash,
    Lingering,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BrewingPotion {
    kind: PotionKind,
    form: PotionForm,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BrewingIngredient {
    NetherWart,
    FermentedSpiderEye,
    GoldenCarrot,
    RabbitFoot,
    MagmaCream,
    Sugar,
    TurtleHelmet,
    Pufferfish,
    GlisteringMelonSlice,
    SpiderEye,
    GhastTear,
    BlazePowder,
    PhantomMembrane,
    BreezeRod,
    SlimeBlock,
    Stone,
    Cobweb,
    Redstone,
    GlowstoneDust,
    Gunpowder,
    DragonBreath,
    GlassBottle,
}

#[cfg_attr(not(test), allow(dead_code))]
fn brew_potion(potion: BrewingPotion, ingredient: BrewingIngredient) -> Option<BrewingPotion> {
    use BrewingIngredient::*;
    use PotionKind::*;

    let form = match (potion.form, ingredient) {
        (PotionForm::Drinkable, Gunpowder) => Some(PotionForm::Splash),
        (PotionForm::Splash, DragonBreath) => Some(PotionForm::Lingering),
        _ => None,
    };
    if let Some(form) = form {
        return Some(BrewingPotion { form, ..potion });
    }

    let kind = match (potion.kind, ingredient) {
        (Water, NetherWart) => Awkward,
        (Water, GlowstoneDust) => Thick,
        (
            Water,
            Redstone | BreezeRod | SlimeBlock | Stone | Cobweb | MagmaCream | RabbitFoot | Sugar
            | GlisteringMelonSlice | SpiderEye | GhastTear | BlazePowder,
        ) => Mundane,
        (Awkward, GoldenCarrot) => NightVision,
        (Awkward, MagmaCream) => FireResistance,
        (Awkward, RabbitFoot) => Leaping,
        (Awkward, TurtleHelmet) => TurtleMaster,
        (Awkward, Sugar) => Swiftness,
        (Awkward, Pufferfish) => WaterBreathing,
        (Awkward, GlisteringMelonSlice) => Healing,
        (Awkward, SpiderEye) => Poison,
        (Awkward, GhastTear) => Regeneration,
        (Awkward, BlazePowder) => Strength,
        (Awkward, PhantomMembrane) => SlowFalling,
        (Awkward, BreezeRod) => WindCharged,
        (Awkward, SlimeBlock) => Oozing,
        (Awkward, Stone) => Infested,
        (Awkward, Cobweb) => Weaving,
        (NightVision, Redstone) => LongNightVision,
        (Invisibility, Redstone) => LongInvisibility,
        (FireResistance, Redstone) => LongFireResistance,
        (Leaping, Redstone) => LongLeaping,
        (Slowness, Redstone) => LongSlowness,
        (TurtleMaster, Redstone) => LongTurtleMaster,
        (Swiftness, Redstone) => LongSwiftness,
        (WaterBreathing, Redstone) => LongWaterBreathing,
        (Poison, Redstone) => LongPoison,
        (Regeneration, Redstone) => LongRegeneration,
        (Strength, Redstone) => LongStrength,
        (Weakness, Redstone) => LongWeakness,
        (SlowFalling, Redstone) => LongSlowFalling,
        (Leaping, GlowstoneDust) => StrongLeaping,
        (Slowness, GlowstoneDust) => StrongSlowness,
        (TurtleMaster, GlowstoneDust) => StrongTurtleMaster,
        (Swiftness, GlowstoneDust) => StrongSwiftness,
        (Healing, GlowstoneDust) => StrongHealing,
        (Harming, GlowstoneDust) => StrongHarming,
        (Poison, GlowstoneDust) => StrongPoison,
        (Regeneration, GlowstoneDust) => StrongRegeneration,
        (Strength, GlowstoneDust) => StrongStrength,
        (Water, FermentedSpiderEye) => Weakness,
        (NightVision, FermentedSpiderEye) => Invisibility,
        (LongNightVision, FermentedSpiderEye) => LongInvisibility,
        (Swiftness | Leaping, FermentedSpiderEye) => Slowness,
        (LongSwiftness | LongLeaping, FermentedSpiderEye) => LongSlowness,
        (Healing | Poison | LongPoison, FermentedSpiderEye) => Harming,
        (StrongHealing | StrongPoison, FermentedSpiderEye) => StrongHarming,
        _ => return None,
    };
    Some(BrewingPotion { kind, ..potion })
}

#[cfg_attr(not(test), allow(dead_code))]
const BREW_TIME_TICKS: u16 = 400;

#[cfg_attr(not(test), allow(dead_code))]
const BREW_FUEL_PER_POWDER: u8 = 20;

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Default)]
struct BrewingStand {
    bottles: [Option<BrewingPotion>; 3],
    ingredient: Option<(BrewingIngredient, u8)>,
    blaze_powder: u8,
    fuel: u8,
    brew_time: u16,
    active_ingredient: Option<BrewingIngredient>,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
enum BrewingTick {
    Idle,
    Started,
    Cancelled,
    Completed { ejected_glass_bottle: bool },
}

impl BrewingStand {
    #[cfg_attr(not(test), allow(dead_code))]
    fn tick(&mut self) -> BrewingTick {
        if self.fuel == 0 && self.blaze_powder > 0 {
            self.blaze_powder -= 1;
            self.fuel = BREW_FUEL_PER_POWDER;
        }

        let ingredient = self.ingredient.filter(|&(_, count)| count > 0);
        let mixes = self
            .bottles
            .map(|bottle| brew_potion(bottle?, ingredient?.0));
        let brewable = mixes.iter().any(Option::is_some);
        if self.brew_time > 0 {
            self.brew_time -= 1;
            // Vanilla completes a valid mix on the last tick before checking for a swap.
            if let Some((item, count)) = ingredient.filter(|_| self.brew_time == 0 && brewable) {
                for (slot, mix) in self.bottles.iter_mut().zip(mixes) {
                    if mix.is_some() {
                        *slot = mix;
                    }
                }
                let remainder = item == BrewingIngredient::DragonBreath;
                self.ingredient = if count > 1 {
                    Some((item, count - 1))
                } else {
                    remainder.then_some((BrewingIngredient::GlassBottle, 1))
                };
                self.active_ingredient = None;
                BrewingTick::Completed {
                    ejected_glass_bottle: remainder && count > 1,
                }
            } else if !brewable || ingredient.map(|(item, _)| item) != self.active_ingredient {
                self.brew_time = 0;
                self.active_ingredient = None;
                BrewingTick::Cancelled
            } else {
                BrewingTick::Idle
            }
        } else if brewable && self.fuel > 0 {
            self.fuel -= 1;
            self.brew_time = BREW_TIME_TICKS;
            self.active_ingredient = ingredient.map(|(item, _)| item);
            BrewingTick::Started
        } else {
            BrewingTick::Idle
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
const COMP_ARMOR_TRIM: i64 = 35;

// Registry order in the 1.21.1 Configuration packet capture.
#[cfg_attr(not(test), allow(dead_code))]
const TRIM_PATTERNS: &[&str] = &[
    "bolt",
    "coast",
    "dune",
    "eye",
    "flow",
    "host",
    "raiser",
    "rib",
    "sentry",
    "shaper",
    "silence",
    "snout",
    "spire",
    "tide",
    "vex",
    "ward",
    "wayfinder",
    "wild",
];

#[cfg_attr(not(test), allow(dead_code))]
const TRIM_MATERIAL_ITEMS: &[&str] = &[
    "amethyst_shard",
    "copper_ingot",
    "diamond",
    "emerald",
    "gold_ingot",
    "iron_ingot",
    "lapis_lazuli",
    "netherite_ingot",
    "quartz",
    "redstone",
];

/// Preview a smithing recipe with one output item and the base stack's component patch.
#[cfg_attr(not(test), allow(dead_code))]
fn smithing_result(template: &Stack, base: &Stack, addition: &Stack) -> Option<Stack> {
    if template.1 == 0 || base.1 == 0 || addition.1 == 0 {
        return None;
    }
    let template = item_to_name(template.0)?;
    let base_item = item_to_name(base.0)?;
    let addition = item_to_name(addition.0)?;
    let mut result = (base.0, 1, base.2.clone());
    if template == "netherite_upgrade_smithing_template" && addition == "netherite_ingot" {
        let (tier, kind) = tool_parts(base_item).or_else(|| armor_parts(base_item))?;
        if tier != "diamond" {
            return None;
        }
        result.0 = name_to_item(&format!("netherite_{kind}"))?;
        return Some(result);
    }

    armor_parts(base_item)?;
    let pattern = template.strip_suffix("_armor_trim_smithing_template")?;
    let pattern_id = TRIM_PATTERNS.iter().position(|name| *name == pattern)?;
    let material_id = TRIM_MATERIAL_ITEMS
        .iter()
        .position(|name| *name == addition)?;

    // Holder streams reserve zero for inline definitions. Both registry IDs fit in one byte.
    let trim = [material_id as u8 + 1, pattern_id as u8 + 1, 1];
    let current = comp_get(&base.2, COMP_ARMOR_TRIM);
    if current.and_then(|data| data.get(..2)) == Some(&trim[..2]) {
        return None;
    }
    comp_set(&mut result.2, COMP_ARMOR_TRIM, trim.to_vec());
    Some(result)
}

#[cfg_attr(not(test), allow(dead_code))]
fn xp_needed_for_next_level(level: i32) -> i32 {
    match level {
        30.. => (level - 30).wrapping_mul(9).wrapping_add(112),
        15..=29 => 37 + (level - 15) * 5,
        _ => level.wrapping_mul(2).wrapping_add(7),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HungerDifficulty {
    Peaceful,
    Easy,
    Normal,
    Hard,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
struct FoodData {
    food_level: u8,
    last_food_level: u8,
    saturation: f32,
    exhaustion: f32,
    tick_timer: u8,
}

impl Default for FoodData {
    fn default() -> Self {
        Self {
            food_level: 20,
            last_food_level: 20,
            saturation: 5.0,
            exhaustion: 0.0,
            tick_timer: 0,
        }
    }
}

impl FoodData {
    /// The saturation argument is the food's absolute saturation gain.
    fn values_after_eating(
        food_level: i32,
        saturation: f32,
        nutrition: i32,
        saturation_gain: f32,
    ) -> (i32, f32) {
        let food_level = food_level.wrapping_add(nutrition).clamp(0, 20);
        let saturation = (saturation + saturation_gain).clamp(0.0, food_level as f32);
        (food_level, saturation)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn add_exhaustion(&mut self, amount: f32) {
        self.exhaustion = (self.exhaustion + amount).min(40.0);
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn tick(
        &mut self,
        health: &mut f32,
        max_health: f32,
        difficulty: HungerDifficulty,
        natural_regeneration: bool,
    ) {
        self.last_food_level = self.food_level;
        if self.exhaustion > 4.0 {
            self.exhaustion -= 4.0;
            if self.saturation > 0.0 {
                self.saturation = (self.saturation - 1.0).max(0.0);
            } else if difficulty != HungerDifficulty::Peaceful {
                self.food_level = self.food_level.saturating_sub(1);
            }
        }

        let can_heal = natural_regeneration && *health > 0.0 && *health < max_health;
        if can_heal && self.food_level >= 20 && self.saturation > 0.0 {
            self.tick_timer += 1;
            if self.tick_timer >= 10 {
                let spent = self.saturation.min(6.0);
                *health = (*health + spent / 6.0).min(max_health);
                self.add_exhaustion(spent);
                self.tick_timer = 0;
            }
        } else if can_heal && self.food_level >= 18 {
            self.tick_timer += 1;
            if self.tick_timer >= 80 {
                *health = (*health + 1.0).min(max_health);
                self.add_exhaustion(6.0);
                self.tick_timer = 0;
            }
        } else if self.food_level == 0 {
            self.tick_timer += 1;
            if self.tick_timer >= 80 {
                if *health > 10.0
                    || difficulty == HungerDifficulty::Hard
                    || (*health > 1.0 && difficulty == HungerDifficulty::Normal)
                {
                    *health = (*health - 1.0).max(0.0);
                }
                self.tick_timer = 0;
            }
        } else {
            self.tick_timer = 0;
        }
    }
}

#[cfg(test)]
mod player_mechanisms_tests {
    use super::*;

    #[test]
    fn eating_clamps_saturation_to_the_updated_food_level() {
        assert_eq!(FoodData::values_after_eating(4, 3.0, 2, 4.0), (6, 6.0));
        assert_eq!(FoodData::values_after_eating(17, 5.0, 8, 12.0), (20, 17.0));
        assert_eq!(FoodData::values_after_eating(2, 1.0, -3, -2.0), (0, 0.0));
    }

    fn stack(item: &str, count: u8) -> Stack {
        (
            name_to_item(item).expect("test item in registry"),
            count,
            Vec::new(),
        )
    }

    fn enchanted_stack(item: &str, enchantments: &[(&str, i64)]) -> Stack {
        let mut result = stack(item, 1);
        let list: Vec<_> = enchantments
            .iter()
            .map(|&(name, level)| (enchant_id(name).unwrap(), level))
            .collect();
        let component = if item == "enchanted_book" {
            COMP_STORED_ENCHANTMENTS
        } else {
            COMP_ENCHANTMENTS
        };
        comp_set(&mut result.2, component, enchantments_data(&list, true));
        result
    }

    #[test]
    fn bookshelves_use_the_two_layer_ring_and_same_height_gap() {
        let table = (100, 60, -30);
        let shelves = [
            (98, 60, -31),
            (98, 61, -31),
            (102, 61, -28),
            (99, 60, -31), // Inner ring.
            (98, 62, -31), // Above the ring.
        ];
        let is_shelf = |x, y, z| shelves.contains(&(x, y, z));
        assert_eq!(
            count_enchanting_bookshelves(table, is_shelf, |_, _, _| true),
            3
        );
        let transmits = |x, y, z| (x, y, z) != (99, 60, -30);
        assert_eq!(count_enchanting_bookshelves(table, is_shelf, transmits), 2);
        assert_eq!(
            count_enchanting_bookshelves(table, |_, _, _| true, |_, _, _| true),
            15
        );
    }

    #[test]
    fn enchantment_costs_match_java_seed_vectors_and_slot_bounds() {
        let costs = |seed, shelves, enchantability| {
            let mut rng = generation_math::Lcg::new(seed);
            std::array::from_fn(|slot| enchantment_cost(&mut rng, slot, shelves, enchantability))
        };
        assert_eq!(costs(12345, 0, 10), [1, 6, 7]);
        assert_eq!(costs(12345, 7, 10), [3, 13, 14]);
        assert_eq!(costs(12345, 15, 10), [6, 20, 30]);
        assert_eq!(costs(12345, 32, 10), [6, 20, 30]);
        assert_eq!(costs((-42i64) as u64, 0, 10), [1, 1, 3]);
        assert_eq!(costs((-42i64) as u64, 7, 10), [3, 7, 14]);
        assert_eq!(costs((-42i64) as u64, 15, 10), [5, 13, 30]);
        assert_eq!(costs(12345, 15, 0), [0; 3]);
        assert_eq!(costs(12345, 15, -1), [0; 3]);
        for seed in 0..128 {
            let [first, second, third] = costs(seed, 15, 1);
            assert!((2..=10).contains(&first));
            assert!((6..=21).contains(&second));
            assert_eq!(third, 30);
        }

        let diamond_sword = stack("diamond_sword", 1);
        let offers = enchanting_offers(&diamond_sword, 12345, 15);
        assert_eq!(
            offers.each_ref().map(|offer| offer.required_level),
            [6, 20, 30]
        );
        for offer in offers {
            assert!(offer.enchantments.contains(&offer.clue.unwrap()));
            assert!(offer
                .enchantments
                .iter()
                .all(|&(id, _)| id != enchant_id("mending").unwrap()));
        }
        assert_eq!(
            enchanting_offers(&stack("dirt", 1), 12345, 15).map(|offer| offer.required_level),
            [0; 3]
        );
    }

    #[test]
    fn enchanting_charges_the_slot_price_and_refreshes_the_seed_only_on_success() {
        let mut sword = stack("diamond_sword", 1);
        let mut player_rng = generation_math::Lcg::new(12345);
        let mut player = EnchantingPlayer {
            level: 29,
            lapis: 3,
            enchantment_seed: 12345,
        };
        let rng_before = player_rng.state48();
        assert!(!player.enchant(&mut sword, 2, 15, false, &mut player_rng));
        player.level = 30;
        player.lapis = 2;
        assert!(!player.enchant(&mut sword, 2, 15, false, &mut player_rng));
        assert_eq!(
            player,
            EnchantingPlayer {
                level: 30,
                lapis: 2,
                enchantment_seed: 12345
            }
        );
        assert_eq!(player_rng.state48(), rng_before);
        assert!(sword.2.is_empty());

        player.lapis = 4;
        let offer = enchanting_offers(&sword, 12345, 15);
        assert!(player.enchant(&mut sword, 2, 15, false, &mut player_rng));
        assert_eq!(
            player,
            EnchantingPlayer {
                level: 27,
                lapis: 1,
                enchantment_seed: 1_553_932_502
            }
        );
        assert_eq!(enchantments_get(&sword.2), offer[2].enchantments);
        assert!(!player.enchant(&mut sword, 0, 15, false, &mut player_rng));

        for slot in 0..2 {
            let mut book = stack("book", 1);
            comp_set(&mut book.2, COMP_CUSTOM_NAME, nbt_string("Field Notes"));
            let mut player = EnchantingPlayer {
                level: 30,
                lapis: 3,
                enchantment_seed: 12345,
            };
            assert!(player.enchant(&mut book, slot, 15, false, &mut player_rng));
            assert_eq!(player.level, 29 - slot as i32);
            assert_eq!(player.lapis, 2 - slot as u8);
            assert_eq!(item_to_name(book.0), Some("enchanted_book"));
            assert!(comp_get(&book.2, COMP_STORED_ENCHANTMENTS).is_some());
            assert_eq!(
                comp_get(&book.2, COMP_CUSTOM_NAME),
                Some(nbt_string("Field Notes").as_slice())
            );
        }
    }

    #[test]
    fn anvil_material_repair_progression_reaches_the_survival_limit() {
        let mut sword = stack("diamond_sword", 1);
        let diamonds = stack("diamond", 2);
        for (prior, next) in [(0, 1), (1, 3), (3, 7), (7, 15), (15, 31), (31, 63)] {
            comp_set_varint(&mut sword.2, COMP_DAMAGE, 1);
            let result = anvil_result(&sword, Some(&diamonds), None, false).unwrap();
            assert_eq!(result.level_cost, prior + 1);
            assert_eq!(result.right_consumed, 1);
            assert_eq!(comp_varint(&result.stack.2, COMP_DAMAGE), Some(0));
            assert_eq!(comp_varint(&result.stack.2, COMP_PRIOR_WORK), Some(next));
            sword = result.stack;
        }

        comp_set_varint(&mut sword.2, COMP_DAMAGE, 1000);
        assert_eq!(
            anvil_result(&sword, Some(&diamonds), None, false),
            Err(AnvilError::TooExpensive(65))
        );
        let creative = anvil_result(&sword, Some(&diamonds), None, true).unwrap();
        assert_eq!(creative.level_cost, 65);
        assert_eq!(creative.right_consumed, 2);
        assert_eq!(comp_varint(&creative.stack.2, COMP_DAMAGE), Some(220));
        assert_eq!(comp_varint(&creative.stack.2, COMP_PRIOR_WORK), Some(127));
        assert_eq!(comp_varint(&sword.2, COMP_DAMAGE), Some(1000));

        let renamed = anvil_result(&sword, None, Some("Old Faithful"), false).unwrap();
        assert_eq!(renamed.level_cost, 39);
        assert_eq!(renamed.right_consumed, 0);
        assert_eq!(comp_varint(&renamed.stack.2, COMP_PRIOR_WORK), Some(63));
        assert_eq!(
            comp_get(&renamed.stack.2, COMP_CUSTOM_NAME),
            Some(nbt_string("Old Faithful").as_slice())
        );
        assert_eq!(
            anvil_result(&renamed.stack, None, Some("Old Faithful"), false),
            Err(AnvilError::InvalidInput)
        );
        let cleared = anvil_result(&renamed.stack, None, Some(" "), false).unwrap();
        assert_eq!(cleared.level_cost, 39);
        assert_eq!(comp_get(&cleared.stack.2, COMP_CUSTOM_NAME), None);
    }

    #[test]
    fn anvil_combines_durability_and_applies_enchantment_costs() {
        let mut left = enchanted_stack("diamond_sword", &[("sharpness", 3), ("unbreaking", 3)]);
        let mut right = enchanted_stack("diamond_sword", &[("sharpness", 3), ("unbreaking", 1)]);
        for (item, damage, prior) in [(&mut left, 80, 3), (&mut right, 90, 7)] {
            comp_set_varint(&mut item.2, COMP_MAX_DAMAGE, 100);
            comp_set_varint(&mut item.2, COMP_DAMAGE, damage);
            comp_set_varint(&mut item.2, COMP_PRIOR_WORK, prior);
        }
        let result = anvil_result(&left, Some(&right), None, false).unwrap();
        assert_eq!(result.right_consumed, 1);
        assert_eq!(result.level_cost, 3 + 7 + 2 + 4 + 3 * 2);
        assert_eq!(comp_varint(&result.stack.2, COMP_DAMAGE), Some(58));
        assert_eq!(comp_varint(&result.stack.2, COMP_PRIOR_WORK), Some(15));
        assert_eq!(
            enchant_level(&result.stack.2, enchant_id("sharpness").unwrap()),
            4
        );
        assert_eq!(
            enchant_level(&result.stack.2, enchant_id("unbreaking").unwrap()),
            3
        );

        let result = anvil_result(
            &result.stack,
            Some(&enchanted_stack("enchanted_book", &[("unbreaking", 3)])),
            None,
            true,
        )
        .unwrap();
        assert_eq!(
            enchant_level(&result.stack.2, enchant_id("unbreaking").unwrap()),
            3
        );
        assert_eq!(result.level_cost, 15 + 3);
    }

    #[test]
    fn anvil_rejects_conflicts_and_charges_each_conflicting_pair_in_a_partial_merge() {
        let sword = enchanted_stack("diamond_sword", &[("sharpness", 3)]);
        let smite = enchanted_stack("enchanted_book", &[("smite", 3)]);
        assert_eq!(
            anvil_result(&sword, Some(&smite), None, false),
            Err(AnvilError::InvalidInput)
        );

        let trident = enchanted_stack("trident", &[("loyalty", 3), ("channeling", 1)]);
        let book = enchanted_stack("enchanted_book", &[("riptide", 3), ("impaling", 2)]);
        let result = anvil_result(&trident, Some(&book), None, false).unwrap();
        assert_eq!(result.level_cost, 2 + 2 * 2);
        assert_eq!(
            enchant_level(&result.stack.2, enchant_id("riptide").unwrap()),
            0
        );
        assert_eq!(
            enchant_level(&result.stack.2, enchant_id("impaling").unwrap()),
            2
        );
        assert_eq!(
            enchant_level(&result.stack.2, enchant_id("loyalty").unwrap()),
            3
        );
        assert_eq!(
            enchant_level(&result.stack.2, enchant_id("channeling").unwrap()),
            1
        );

        for (left, right) in [
            ("sharpness", "smite"),
            ("sharpness", "bane_of_arthropods"),
            ("protection", "blast_protection"),
            ("protection", "fire_protection"),
            ("protection", "projectile_protection"),
            ("infinity", "mending"),
            ("silk_touch", "fortune"),
            ("depth_strider", "frost_walker"),
            ("multishot", "piercing"),
            ("loyalty", "riptide"),
            ("channeling", "riptide"),
        ] {
            let (left, right) = (enchant_id(left).unwrap(), enchant_id(right).unwrap());
            assert!(player_enchantments_conflict(left, right));
            assert!(player_enchantments_conflict(right, left));
        }
        assert!(!player_enchantments_conflict(
            enchant_id("loyalty").unwrap(),
            enchant_id("channeling").unwrap()
        ));
    }

    fn drinkable(kind: PotionKind) -> BrewingPotion {
        BrewingPotion {
            kind,
            form: PotionForm::Drinkable,
        }
    }

    #[test]
    fn brewing_uses_400_ticks_and_one_charge_for_all_three_bottles() {
        use BrewingIngredient::*;
        use PotionKind::*;
        let mut stand = BrewingStand {
            bottles: [
                Some(drinkable(Water)),
                Some(drinkable(Weakness)),
                Some(drinkable(Water)),
            ],
            ingredient: Some((NetherWart, 20)),
            blaze_powder: 1,
            ..BrewingStand::default()
        };

        for charge in 1..=20 {
            assert_eq!(stand.tick(), BrewingTick::Started);
            assert_eq!(stand.fuel, 20 - charge);
            assert_eq!(stand.blaze_powder, 0);
            assert_eq!(stand.brew_time, 400);
            let before = stand.bottles;
            for _ in 0..399 {
                assert_eq!(stand.tick(), BrewingTick::Idle);
            }
            assert_eq!(stand.brew_time, 1);
            assert_eq!(stand.bottles, before);
            assert_eq!(
                stand.tick(),
                BrewingTick::Completed {
                    ejected_glass_bottle: false
                }
            );
            assert_eq!(
                stand.ingredient,
                (charge < 20).then_some((NetherWart, 20 - charge))
            );
            assert_eq!(
                stand.bottles,
                [
                    Some(drinkable(Awkward)),
                    Some(drinkable(Weakness)),
                    Some(drinkable(Awkward))
                ]
            );
            stand.bottles = before;
        }
        stand.ingredient = Some((NetherWart, 1));
        assert_eq!(stand.tick(), BrewingTick::Idle);
        assert_eq!(stand.brew_time, 0);
        assert_eq!(stand.ingredient, Some((NetherWart, 1)));
    }

    #[test]
    fn brewing_cancel_spends_the_charge_and_dragon_breath_returns_a_bottle() {
        use BrewingIngredient::*;
        let splash = BrewingPotion {
            form: PotionForm::Splash,
            kind: PotionKind::Water,
        };
        let mut stand = BrewingStand {
            bottles: [Some(splash), None, None],
            ingredient: Some((NetherWart, 1)),
            blaze_powder: 1,
            ..BrewingStand::default()
        };
        assert_eq!(stand.tick(), BrewingTick::Started);
        stand.ingredient = Some((DragonBreath, 1));
        assert_eq!(stand.tick(), BrewingTick::Cancelled);
        assert_eq!(stand.fuel, 19);
        assert_eq!(stand.bottles[0], Some(splash));
        assert_eq!(stand.tick(), BrewingTick::Started);
        assert_eq!(stand.fuel, 18);
        for _ in 0..399 {
            assert_eq!(stand.tick(), BrewingTick::Idle);
        }
        assert_eq!(
            stand.tick(),
            BrewingTick::Completed {
                ejected_glass_bottle: false
            }
        );
        assert_eq!(stand.ingredient, Some((GlassBottle, 1)));
        assert_eq!(
            stand.bottles[0],
            Some(BrewingPotion {
                form: PotionForm::Lingering,
                ..splash
            })
        );

        stand.bottles[0] = Some(splash);
        stand.ingredient = Some((DragonBreath, 2));
        assert_eq!(stand.tick(), BrewingTick::Started);
        for _ in 0..399 {
            assert_eq!(stand.tick(), BrewingTick::Idle);
        }
        assert_eq!(
            stand.tick(),
            BrewingTick::Completed {
                ejected_glass_bottle: true
            }
        );
        assert_eq!(stand.ingredient, Some((DragonBreath, 1)));
    }

    #[test]
    fn brewing_supports_base_recipes_variants_and_corruptions() {
        use BrewingIngredient::*;
        use PotionKind::*;
        for (input, reagent, output) in [
            (Water, NetherWart, Awkward),
            (Water, Redstone, Mundane),
            (Water, GlowstoneDust, Thick),
            (Water, BlazePowder, Mundane),
            (Water, FermentedSpiderEye, Weakness),
            (Awkward, GoldenCarrot, NightVision),
            (Awkward, RabbitFoot, Leaping),
            (Awkward, MagmaCream, FireResistance),
            (Awkward, Sugar, Swiftness),
            (Awkward, TurtleHelmet, TurtleMaster),
            (Awkward, Pufferfish, WaterBreathing),
            (Awkward, GlisteringMelonSlice, Healing),
            (Awkward, SpiderEye, Poison),
            (Awkward, GhastTear, Regeneration),
            (Awkward, BlazePowder, Strength),
            (Awkward, PhantomMembrane, SlowFalling),
            (Awkward, BreezeRod, WindCharged),
            (Awkward, SlimeBlock, Oozing),
            (Awkward, Stone, Infested),
            (Awkward, Cobweb, Weaving),
            (Swiftness, Redstone, LongSwiftness),
            (Swiftness, GlowstoneDust, StrongSwiftness),
            (NightVision, FermentedSpiderEye, Invisibility),
            (LongNightVision, FermentedSpiderEye, LongInvisibility),
            (Leaping, FermentedSpiderEye, Slowness),
            (Swiftness, FermentedSpiderEye, Slowness),
            (LongSwiftness, FermentedSpiderEye, LongSlowness),
            (LongLeaping, FermentedSpiderEye, LongSlowness),
            (Healing, FermentedSpiderEye, Harming),
            (Poison, FermentedSpiderEye, Harming),
            (LongPoison, FermentedSpiderEye, Harming),
            (StrongHealing, FermentedSpiderEye, StrongHarming),
            (StrongPoison, FermentedSpiderEye, StrongHarming),
            (Slowness, GlowstoneDust, StrongSlowness),
        ] {
            assert_eq!(
                brew_potion(drinkable(input), reagent),
                Some(drinkable(output)),
                "{input:?} + {reagent:?}"
            );
        }

        let swift = drinkable(Swiftness);
        let long = brew_potion(swift, Redstone).unwrap();
        let splash = brew_potion(long, Gunpowder).unwrap();
        assert_eq!(
            splash,
            BrewingPotion {
                kind: LongSwiftness,
                form: PotionForm::Splash
            }
        );
        assert_eq!(
            brew_potion(splash, DragonBreath),
            Some(BrewingPotion {
                form: PotionForm::Lingering,
                ..splash
            })
        );
        assert_eq!(brew_potion(swift, DragonBreath), None);
        assert_eq!(brew_potion(long, GlowstoneDust), None);
        assert_eq!(brew_potion(drinkable(StrongSwiftness), Redstone), None);
        assert_eq!(
            brew_potion(drinkable(StrongSwiftness), FermentedSpiderEye),
            None
        );
        assert_eq!(brew_potion(drinkable(Water), TurtleHelmet), None);
        assert_eq!(
            brew_potion(drinkable(Luck), Gunpowder),
            Some(BrewingPotion {
                kind: Luck,
                form: PotionForm::Splash
            })
        );
    }

    #[test]
    fn smithing_upgrades_every_diamond_tool_and_armor_piece_with_components() {
        let template = stack("netherite_upgrade_smithing_template", 1);
        let ingot = stack("netherite_ingot", 1);
        for kind in [
            "sword",
            "pickaxe",
            "axe",
            "shovel",
            "hoe",
            "helmet",
            "chestplate",
            "leggings",
            "boots",
        ] {
            let mut base = enchanted_stack(&format!("diamond_{kind}"), &[("unbreaking", 3)]);
            comp_set_varint(&mut base.2, COMP_DAMAGE, 271);
            comp_set(&mut base.2, COMP_CUSTOM_NAME, nbt_string("Treasure"));
            let result = smithing_result(&template, &base, &ingot).unwrap();
            assert_eq!(
                item_to_name(result.0),
                Some(format!("netherite_{kind}").as_str())
            );
            assert_eq!(result.1, 1);
            assert_eq!(result.2, base.2);
        }
        assert_eq!(
            smithing_result(&template, &stack("iron_sword", 1), &ingot),
            None
        );
        assert_eq!(
            smithing_result(&template, &stack("diamond_sword", 1), &stack("diamond", 1)),
            None
        );
    }

    #[test]
    fn smithing_validates_trims_and_preserves_existing_components() {
        let template = stack("bolt_armor_trim_smithing_template", 1);
        let material = stack("amethyst_shard", 1);
        let mut armor = enchanted_stack("turtle_helmet", &[("respiration", 3)]);
        comp_set_varint(&mut armor.2, COMP_DAMAGE, 20);
        let result = smithing_result(&template, &armor, &material).unwrap();
        assert_eq!(result.0, armor.0);
        assert_eq!(
            comp_get(&result.2, COMP_ARMOR_TRIM),
            Some([1, 1, 1].as_slice())
        );
        assert_eq!(&result.2[..armor.2.len()], armor.2.as_slice());
        assert_eq!(smithing_result(&template, &result, &material), None);
        let gold = smithing_result(&template, &result, &stack("gold_ingot", 1)).unwrap();
        assert_eq!(
            comp_get(&gold.2, COMP_ARMOR_TRIM),
            Some([5, 1, 1].as_slice())
        );
        assert_eq!(
            smithing_result(&template, &stack("diamond_sword", 1), &material),
            None
        );
        assert_eq!(
            smithing_result(&template, &armor, &stack("lapis_block", 1)),
            None
        );
    }

    #[test]
    fn xp_cost_matches_all_levels_through_35() {
        let expected = [
            7, 9, 11, 13, 15, 17, 19, 21, 23, 25, 27, 29, 31, 33, 35, 37, 42, 47, 52, 57, 62, 67,
            72, 77, 82, 87, 92, 97, 102, 107, 112, 121, 130, 139, 148, 157,
        ];
        assert_eq!(
            std::array::from_fn(|level| xp_needed_for_next_level(level as i32)),
            expected
        );
    }

    #[test]
    fn hunger_consumes_one_exhaustion_chunk_with_saturation_first() {
        let mut food = FoodData {
            exhaustion: 4.0,
            saturation: 0.5,
            ..FoodData::default()
        };
        let mut health = 20.0;
        food.tick(&mut health, 20.0, HungerDifficulty::Normal, true);
        assert_eq!(
            (food.exhaustion, food.saturation, food.food_level),
            (4.0, 0.5, 20)
        );
        food.add_exhaustion(5.0);
        food.tick(&mut health, 20.0, HungerDifficulty::Normal, true);
        assert_eq!(
            (food.exhaustion, food.saturation, food.food_level),
            (5.0, 0.0, 20)
        );
        food.tick(&mut health, 20.0, HungerDifficulty::Normal, true);
        assert_eq!(
            (food.exhaustion, food.food_level, food.last_food_level),
            (1.0, 19, 20)
        );

        food.add_exhaustion(100.0);
        assert_eq!(food.exhaustion, 40.0);
        food.tick(&mut health, 20.0, HungerDifficulty::Peaceful, true);
        assert_eq!(
            (food.exhaustion, food.food_level, food.last_food_level),
            (36.0, 19, 19)
        );
    }

    #[test]
    fn hunger_regenerates_at_ten_or_eighty_ticks_and_adds_exhaustion() {
        let mut rapid = FoodData {
            saturation: 3.0,
            ..FoodData::default()
        };
        let mut standard = FoodData {
            food_level: 18,
            saturation: 0.0,
            ..FoodData::default()
        };
        let mut health = 10.0;
        for _ in 0..9 {
            rapid.tick(&mut health, 20.0, HungerDifficulty::Normal, true);
        }
        assert_eq!(health, 10.0);
        rapid.tick(&mut health, 20.0, HungerDifficulty::Normal, true);
        assert_eq!(
            (health, rapid.exhaustion, rapid.saturation),
            (10.5, 3.0, 3.0)
        );

        for _ in 0..79 {
            standard.tick(&mut health, 20.0, HungerDifficulty::Easy, true);
        }
        assert_eq!(health, 10.5);
        standard.tick(&mut health, 20.0, HungerDifficulty::Easy, false);
        assert_eq!(standard.tick_timer, 0);
        for _ in 0..80 {
            standard.tick(&mut health, 20.0, HungerDifficulty::Easy, true);
        }
        assert_eq!((health, standard.exhaustion), (11.5, 6.0));

        let mut shared = FoodData::default();
        shared.food_level = 18;
        for _ in 0..9 {
            shared.tick(&mut health, 20.0, HungerDifficulty::Normal, true);
        }
        shared.food_level = 20;
        shared.saturation = 8.0;
        shared.tick(&mut health, 20.0, HungerDifficulty::Normal, true);
        assert_eq!((health, shared.exhaustion), (12.5, 6.0));
    }

    #[test]
    fn hunger_starves_every_eighty_ticks_with_difficulty_limits() {
        for (difficulty, starting, minimum) in [
            (HungerDifficulty::Easy, 11.0, 10.0),
            (HungerDifficulty::Normal, 2.0, 1.0),
            (HungerDifficulty::Hard, 1.0, 0.0),
        ] {
            let mut food = FoodData {
                food_level: 0,
                saturation: 0.0,
                ..FoodData::default()
            };
            let mut health = starting;
            for _ in 0..79 {
                food.tick(&mut health, 20.0, difficulty, false);
            }
            assert_eq!(health, starting);
            food.tick(&mut health, 20.0, difficulty, false);
            assert_eq!(health, minimum);
            for _ in 0..80 {
                food.tick(&mut health, 20.0, difficulty, true);
            }
            assert_eq!(health, minimum);
        }
    }
}
