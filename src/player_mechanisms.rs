// R7 GP-5: player mechanisms for Java Edition 1.21.1.
// Included at the crate root by main.rs.

// Lane P5e: name-keyed item registry facts (#217) and the typed block entity
// models (#216) share this file's flat crate-root scope, the same way
// commands_core.rs includes datapack.rs.
include!("item_registry.rs");
include!("block_entity.rs");

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
#[derive(Debug, Default, PartialEq, Eq)]
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
        let mut rng = enchantment_slot_rng(seed, slot);
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
#[derive(Default)]
struct EnchantmentMenu {
    item: Stack,
    lapis: Stack,
    enchantment_seed: i32,
    offers: [EnchantmentOffer; 3],
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
enum EnchantingClick {
    Rejected,
    Accepted,
    Enchanted,
}

#[cfg_attr(not(test), allow(dead_code))]
impl EnchantmentMenu {
    fn refresh_offers(&mut self, seed: i32, bookshelves: i32) {
        self.enchantment_seed = seed;
        self.offers = enchanting_offers(&self.item, seed, bookshelves);
    }

    /// Handle a button against the offers last sent for the current slots.
    fn click_button(
        &mut self,
        button: i32,
        xp: &mut PlayerXpState,
        seed: &mut i32,
        creative: bool,
        player_rng: &mut generation_math::Lcg,
    ) -> EnchantingClick {
        let Ok(slot @ 0..=2) = usize::try_from(button) else {
            return EnchantingClick::Rejected;
        };
        let price = slot as u8 + 1;
        let offer = &self.offers[slot];
        if self.item.1 == 0 || self.item.0 == 0 || offer.required_level <= 0 {
            return EnchantingClick::Rejected;
        }
        let has_lapis = self.lapis.1 >= price && item_to_name(self.lapis.0) == Some("lapis_lazuli");
        if !creative
            && (!has_lapis || xp.level < offer.required_level || xp.level < i32::from(price))
        {
            return EnchantingClick::Rejected;
        }
        // Vanilla rolls the current item at the cost already shown by the menu.
        let item = item_to_name(self.item.0).unwrap_or_default();
        let mut rng = enchantment_slot_rng(self.enchantment_seed, slot);
        let rolled = roll_table_enchantments(
            &mut rng,
            item,
            item_enchantability(item),
            offer.required_level,
        );
        if rolled.is_empty() {
            return EnchantingClick::Accepted;
        }

        xp.give_experience_levels(-i32::from(price));
        *seed = player_rng.next_i32();
        if item == "book" {
            self.item.0 = name_to_item("enchanted_book").expect("vanilla enchanted book");
        }
        let component = crafting_enchantment_component(&self.item);
        let mut applied = crafting_enchantments(&self.item);
        for (id, level) in rolled {
            if let Some((_, current)) = applied.iter_mut().find(|(existing, _)| *existing == id) {
                *current = (*current).max(level);
            } else {
                applied.push((id, level));
            }
        }
        let show = comp_get(&self.item.2, component).and_then(|data| data.last()) != Some(&0);
        comp_set(
            &mut self.item.2,
            component,
            enchantments_data(&applied, show),
        );
        if !creative {
            consume_menu_stack(&mut self.lapis, price);
        }
        self.enchantment_seed = *seed;
        self.offers = Default::default();
        EnchantingClick::Enchanted
    }
}

/// The property payload is window ID (u8), property (i16 BE), value (i16 BE).
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowPropertyUpdate {
    pub property: i16,
    pub value: i16,
}

impl WindowPropertyUpdate {
    #[cfg_attr(not(test), allow(dead_code))]
    fn encode(self, window_id: u8) -> [u8; 5] {
        let property = self.property.to_be_bytes();
        let value = self.value.to_be_bytes();
        [window_id, property[0], property[1], value[0], value[1]]
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn enchanting_window_properties(menu: &EnchantmentMenu) -> [WindowPropertyUpdate; 10] {
    let offers = &menu.offers;
    std::array::from_fn(|property| {
        let value = match property {
            0..=2 => offers[property].required_level as i16,
            3 => (menu.enchantment_seed & 0xffff) as i16,
            4..=6 => offers[property - 4].clue.map_or(-1, |(id, _)| id as i16),
            _ => offers[property - 7]
                .clue
                .map_or(-1, |(_, level)| level as i16),
        };
        WindowPropertyUpdate {
            property: property as i16,
            value,
        }
    })
}

#[cfg_attr(not(test), allow(dead_code))]
fn enchantment_slot_rng(seed: i32, slot: usize) -> generation_math::Lcg {
    generation_math::Lcg::new(i64::from(seed.wrapping_add(slot as i32)) as u64)
}

#[cfg_attr(not(test), allow(dead_code))]
fn roll_table_enchantments(
    rng: &mut generation_math::Lcg,
    item: &str,
    enchantability: i32,
    required_level: i32,
) -> Vec<(i64, i64)> {
    if enchantability <= 0 {
        return Vec::new();
    }
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
fn crafting_enchantment_component(stack: &Stack) -> i64 {
    if item_to_name(stack.0) == Some("enchanted_book") {
        COMP_STORED_ENCHANTMENTS
    } else {
        COMP_ENCHANTMENTS
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn crafting_enchantments(stack: &Stack) -> Vec<(i64, i64)> {
    if crafting_enchantment_component(stack) == COMP_ENCHANTMENTS {
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
    let mut enchantments = crafting_enchantments(output);
    let mut cost = 0;
    let mut accepted = false;
    let mut rejected = false;
    for (incoming, incoming_level) in crafting_enchantments(sacrifice) {
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
        let component = crafting_enchantment_component(output);
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

// StringUtil.isBlank treats only printable, non-space ASCII as nonblank.
#[cfg_attr(not(test), allow(dead_code))]
fn anvil_name_is_blank(name: &str) -> bool {
    !name.bytes().any(|byte| byte.is_ascii_graphic())
}

// Pure combat, enchantment, mining, and item durability mechanics for Java
// Edition 1.21.1. These are kept independent from the server loop so callers
// can supply world- and entity-specific facts without making this module stateful.

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
struct PlayerAabb {
    min: [f64; 3],
    max: [f64; 3],
}

#[cfg_attr(not(test), allow(dead_code))]
impl PlayerAabb {
    fn inflate(self, horizontal: f64, vertical: f64) -> Self {
        Self {
            min: [
                self.min[0] - horizontal,
                self.min[1] - vertical,
                self.min[2] - horizontal,
            ],
            max: [
                self.max[0] + horizontal,
                self.max[1] + vertical,
                self.max[2] + horizontal,
            ],
        }
    }

    fn intersects(self, other: Self) -> bool {
        (0..3).all(|axis| self.max[axis] >= other.min[axis] && self.min[axis] <= other.max[axis])
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
struct SweepAttack {
    eligible: bool,
    damage: f32,
    search_box: PlayerAabb,
}

#[cfg_attr(not(test), allow(dead_code))]
fn sweeping_edge_damage(level: i32, base_attack_damage: f32) -> f32 {
    let level = level.max(0) as f32;
    1.0 + (level / (level + 1.0)) * base_attack_damage
}

#[cfg_attr(not(test), allow(dead_code))]
fn sweeping_attack(
    attack_strength: f32,
    on_ground: bool,
    sprinting: bool,
    critical: bool,
    attacker_position: [f64; 3],
    target_box: PlayerAabb,
    candidate_box: PlayerAabb,
    base_attack_damage: f32,
    sweeping_edge_level: i32,
) -> SweepAttack {
    let target_center = [
        (target_box.min[0] + target_box.max[0]) * 0.5,
        (target_box.min[1] + target_box.max[1]) * 0.5,
        (target_box.min[2] + target_box.max[2]) * 0.5,
    ];
    let dx = attacker_position[0] - target_center[0];
    let dz = attacker_position[2] - target_center[2];
    let eligible = attack_strength >= 0.9
        && on_ground
        && !sprinting
        && !critical
        && dx * dx + dz * dz <= 9.0
        && target_box.inflate(1.0, 0.25).intersects(candidate_box);
    SweepAttack {
        eligible,
        damage: if eligible {
            sweeping_edge_damage(sweeping_edge_level, base_attack_damage)
        } else {
            0.0
        },
        search_box: target_box.inflate(1.0, 0.25),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn sweeping_target_in_range(search_box: PlayerAabb, candidate_box: PlayerAabb) -> bool {
    search_box.intersects(candidate_box)
}

#[cfg_attr(not(test), allow(dead_code))]
fn knockback_strength(base_strength: f32, knockback_level: i32) -> f32 {
    base_strength + knockback_level.max(0) as f32 * 0.5
}

#[cfg_attr(not(test), allow(dead_code))]
fn unbreaking_consumes_durability(level: i32, random_0_to_1: f32, is_armor: bool) -> bool {
    let level = level.max(0) as f32;
    let damage_chance = if is_armor {
        0.6 + 0.4 / (level + 1.0)
    } else {
        1.0 / (level + 1.0)
    };
    random_0_to_1 >= 0.0 && random_0_to_1 < damage_chance
}

#[cfg_attr(not(test), allow(dead_code))]
fn lure_wait_ticks(base_wait_ticks: i32, level: i32) -> i32 {
    (base_wait_ticks - level.max(0).saturating_mul(100)).max(0)
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FishingLootWeights {
    treasure: i32,
    junk: i32,
    fish: i32,
}

#[cfg_attr(not(test), allow(dead_code))]
fn luck_of_the_sea_weights(level: i32, base: FishingLootWeights) -> FishingLootWeights {
    let luck = level.max(0);
    FishingLootWeights {
        treasure: (base.treasure + 2 * luck).max(0),
        junk: (base.junk - luck).max(0),
        fish: base.fish.max(0),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn thorns_triggers(level: i32, random_0_to_1: f32) -> bool {
    let chance = (level.max(0) as f32 * 0.15).min(1.0);
    random_0_to_1 >= 0.0 && random_0_to_1 < chance
}

#[cfg_attr(not(test), allow(dead_code))]
fn thorns_damage(level: i32, random_1_to_4: u32) -> u32 {
    if level <= 0 {
        0
    } else if level > 10 {
        level as u32
    } else {
        random_1_to_4.clamp(1, 4)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn respiration_air_ticks(level: i32) -> i32 {
    300_i32.saturating_mul(level.max(0).saturating_add(1))
}

#[cfg_attr(not(test), allow(dead_code))]
fn respiration_preserves_air(level: i32, random_0_to_1: f32) -> bool {
    level > 0 && random_0_to_1 >= 0.0 && random_0_to_1 < level as f32 / (level as f32 + 1.0)
}

#[cfg_attr(not(test), allow(dead_code))]
fn depth_strider_underwater_factor(level: i32) -> f32 {
    level.clamp(0, 3) as f32 / 3.0
}

#[cfg_attr(not(test), allow(dead_code))]
fn soul_speed_multiplier(level: i32) -> f32 {
    1.0 + level.max(0) as f32 * 0.03
}

#[cfg_attr(not(test), allow(dead_code))]
fn swift_sneak_speed_multiplier(level: i32) -> f32 {
    (0.3 + level.max(0) as f32 * 0.15).min(1.0)
}

#[cfg_attr(not(test), allow(dead_code))]
fn tool_base_mining_speed(item: &str) -> f32 {
    let Some((tier, kind)) = tool_parts(item) else {
        return 1.0;
    };
    if kind == "sword" {
        return 1.5;
    }
    match tier {
        "wooden" => 2.0,
        "stone" => 4.0,
        "iron" => 6.0,
        "diamond" => 8.0,
        "golden" => 12.0,
        "netherite" => 9.0,
        _ => 1.0,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn mining_speed_with_modifiers(
    base_speed: f32,
    effective_tool: bool,
    efficiency_level: i32,
    haste_amplifier: Option<i32>,
    mining_fatigue_amplifier: Option<i32>,
    in_water_without_aqua_affinity: bool,
    on_ground: bool,
) -> f32 {
    let mut speed = base_speed.max(0.0);
    if effective_tool && efficiency_level > 0 {
        let level = efficiency_level as f32;
        speed += level * level + 1.0;
    }
    if let Some(amplifier) = haste_amplifier {
        speed *= 1.0 + 0.2 * (amplifier.max(0) as f32 + 1.0);
    }
    if let Some(amplifier) = mining_fatigue_amplifier {
        // `LivingEntity`'s switch table hardcodes each level; amplifier 2 is
        // 0.0027, not 0.3^3.
        const FATIGUE: [f32; 4] = [0.3, 0.09, 0.0027, 0.00081];
        speed *= FATIGUE[amplifier.clamp(0, 3) as usize];
    }
    if in_water_without_aqua_affinity {
        speed *= 0.2;
    }
    if !on_ground {
        speed *= 0.2;
    }
    speed
}

#[cfg_attr(not(test), allow(dead_code))]
fn mining_damage_per_tick(speed: f32, block_hardness: f32) -> f32 {
    if block_hardness <= 0.0 {
        return 1.0;
    }
    speed.max(0.0) / block_hardness / 30.0
}

#[cfg_attr(not(test), allow(dead_code))]
fn instant_mining_break(speed: f32, block_hardness: f32) -> bool {
    block_hardness <= 0.0 || mining_damage_per_tick(speed, block_hardness) >= 1.0
}

/// Every input vanilla 1.21.1 `Player.getDestroySpeed` consults, pre-resolved
/// so the evaluator stays pure: the held item's `minecraft:tool` component
/// answers the base speed, then Efficiency, the dig-speed effect, Mining
/// Fatigue, the water penalty and the airborne penalty apply in that order.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default)]
struct MiningSpeedContext<'a> {
    /// Held item name, e.g. "diamond_pickaxe"; anything without a tool
    /// component (including ""/"air", the bare hand) mines at speed 1.
    held_item: &'a str,
    /// Block being mined, e.g. "cobweb".
    block: &'a str,
    /// The block's `mineable/<kind>` tag in the encoding `BReg::tool_kind_of`
    /// uses: shovel = 1, pickaxe = 2, axe = 3, hoe = 4, anything else = none.
    preferred_tool: i8,
    efficiency_level: i32,
    /// Haste amplifier, `None` when the effect is absent.
    haste_amplifier: Option<i32>,
    /// Conduit Power amplifier, `None` when the effect is absent.
    conduit_power_amplifier: Option<i32>,
    /// Mining Fatigue amplifier, `None` when the effect is absent.
    mining_fatigue_amplifier: Option<i32>,
    /// `isEyeInFluid(FluidTags.WATER)`.
    submerged_in_water: bool,
    has_aqua_affinity: bool,
    /// `onGround()` - being airborne divides the speed by five.
    on_ground: bool,
}

/// The `minecraft:tool` component half of `ItemStack.getDestroySpeed`: the held
/// item's speed against `block`. `preferred_tool` is the block's
/// `mineable/<kind>` tag as `BReg::tool_kind_of` encodes it - shovel 1,
/// pickaxe 2, axe 3, hoe 4, anything else no preferred tool.
#[cfg_attr(not(test), allow(dead_code))]
fn mining_tool_speed(item: &str, block: &str, preferred_tool: i8) -> f32 {
    let item = item.strip_prefix("minecraft:").unwrap_or(item);
    let block = block.strip_prefix("minecraft:").unwrap_or(block);
    if item == "shears" {
        // ShearsItem's Tool component: cobweb and leaves 15, wool 5,
        // vine and glow lichen 2, everything else the default 1.
        return if block == "cobweb" || block.ends_with("_leaves") {
            15.0
        } else if block.ends_with("_wool") {
            5.0
        } else if matches!(block, "vine" | "glow_lichen") {
            2.0
        } else {
            1.0
        };
    }
    let Some((tier, kind)) = tool_parts(item) else {
        return 1.0;
    };
    if kind == "sword" {
        // SwordItem's Tool component: cobweb 15; the 1.21.1 instant break is
        // BambooBlock.calcBlockBreakingDelta answering 1 for any sword;
        // sword_efficient 1.5; everything else the default 1.
        return if block == "cobweb" {
            15.0
        } else if matches!(block, "bamboo" | "bamboo_sapling") {
            f32::MAX
        } else if sword_efficient(block) {
            1.5
        } else {
            1.0
        };
    }
    let mines = match kind {
        "shovel" => 1,
        "pickaxe" => 2,
        "axe" => 3,
        "hoe" => 4,
        _ => return 1.0,
    };
    if preferred_tool != mines {
        return 1.0;
    }
    match tier {
        "wooden" => 2.0,
        "stone" => 4.0,
        "iron" => 6.0,
        "diamond" => 8.0,
        "netherite" => 9.0,
        "golden" => 12.0,
        _ => 1.0,
    }
}

/// The 1.21.1 `sword_efficient` block tag: all leaves, vine, glow lichen,
/// pumpkins, melon, cocoa, big dripleaf, and both chorus blocks.
#[cfg_attr(not(test), allow(dead_code))]
fn sword_efficient(block: &str) -> bool {
    block.ends_with("_leaves")
        || matches!(
            block,
            "vine"
                | "glow_lichen"
                | "pumpkin"
                | "carved_pumpkin"
                | "jack_o_lantern"
                | "melon"
                | "cocoa"
                | "big_dripleaf"
                | "big_dripleaf_stem"
                | "chorus_plant"
                | "chorus_flower"
        )
}

/// Vanilla 1.21.1 `Player.getDestroySpeed` (`digSpeed` in yarn): the tool
/// component speed, Efficiency only when the tool already beats bare hands,
/// `MobEffectUtil.getDigSpeedAmplification` (the stronger of Haste and Conduit
/// Power), the Mining Fatigue switch table, the /5 water penalty without Aqua
/// Affinity, and the /5 airborne penalty.
#[cfg_attr(not(test), allow(dead_code))]
fn vanilla_destroy_speed(ctx: &MiningSpeedContext) -> f32 {
    let mut speed = mining_tool_speed(ctx.held_item, ctx.block, ctx.preferred_tool);
    if speed > 1.0 && ctx.efficiency_level > 0 {
        speed += (ctx.efficiency_level * ctx.efficiency_level + 1) as f32;
    }
    if let Some(amplifier) = ctx.haste_amplifier.max(ctx.conduit_power_amplifier) {
        speed *= 1.0 + (amplifier.max(0) + 1) as f32 * 0.2;
    }
    if let Some(amplifier) = ctx.mining_fatigue_amplifier {
        // `LivingEntity.getDigSpeed`'s switch table, verbatim.
        const FATIGUE: [f32; 4] = [0.3, 0.09, 0.0027, 0.00081];
        speed *= FATIGUE[amplifier.clamp(0, 3) as usize];
    }
    if ctx.submerged_in_water && !ctx.has_aqua_affinity {
        speed /= 5.0;
    }
    if !ctx.on_ground {
        speed /= 5.0;
    }
    speed
}

/// How many ticks `getDestroyProgress` accumulation takes to break the block:
/// `damage_per_tick = speed / hardness / (can_harvest ? 30 : 100)` and the
/// block drops once the running sum reaches 1. `0` means instant (zero
/// hardness, or `damage_per_tick >= 1` on the first check); `u32::MAX` means
/// never - a negative hardness block like bedrock keeps returning zero
/// progress, and so does mining with no effective speed.
#[cfg_attr(not(test), allow(dead_code))]
fn vanilla_mining_ticks(hardness: f32, can_harvest: bool, speed: f32) -> u32 {
    if hardness == 0.0 {
        return 0;
    }
    if hardness < 0.0 || speed <= 0.0 {
        return u32::MAX;
    }
    let damage_per_tick = speed / hardness / if can_harvest { 30.0 } else { 100.0 };
    if damage_per_tick >= 1.0 {
        return 0;
    }
    (1.0 / damage_per_tick).ceil() as u32
}

/// The block state's `mineable/<kind>` tag through the loaded 1.21.1 registry,
/// in `mining_tool_speed`'s encoding; -1 when the registry is not loaded or
/// the block has no preferred tool.
#[cfg_attr(not(test), allow(dead_code))]
fn preferred_mining_tool(state: u32) -> i8 {
    breg().map_or(-1, |reg| reg.tool_kind_of(state))
}

/// `max_damage` by item name; the canonical table is `max_durability_by_name`
/// in `item_registry.rs` so every caller shares one name-keyed source (#217:
/// crossbow is 465, golden armour is x7, not x15).
#[cfg_attr(not(test), allow(dead_code))]
fn item_max_durability(item: &str) -> Option<u32> {
    max_durability_by_name(item).map(u32::from)
}

#[cfg(test)]
mod player_mechanisms_combat_mining_durability_tests {
    use super::*;

    fn box_at(x: f64, y: f64, z: f64) -> PlayerAabb {
        PlayerAabb {
            min: [x, y, z],
            max: [x + 1.0, y + 1.0, z + 1.0],
        }
    }

    #[test]
    fn sweeping_attack_checks_conditions_damage_distance_and_aabb() {
        let target = box_at(0.0, 0.0, 0.0);
        let candidate = box_at(1.5, 0.0, 0.0);
        let sweep = sweeping_attack(
            0.9,
            true,
            false,
            false,
            [-2.0, 0.0, 0.5],
            target,
            candidate,
            8.0,
            3,
        );
        assert!(sweep.eligible);
        assert!((sweep.damage - 7.0).abs() < 0.0001);
        assert_eq!(sweep.search_box, target.inflate(1.0, 0.25));
        assert!(sweeping_target_in_range(sweep.search_box, candidate));
        assert!(
            !sweeping_attack(
                0.89,
                true,
                false,
                false,
                [-2.0, 0.0, 0.5],
                target,
                candidate,
                8.0,
                0
            )
            .eligible
        );
        assert!(
            !sweeping_attack(
                1.0,
                true,
                true,
                false,
                [-2.0, 0.0, 0.5],
                target,
                candidate,
                8.0,
                0
            )
            .eligible
        );
        assert!(
            !sweeping_attack(
                1.0,
                false,
                false,
                false,
                [-2.0, 0.0, 0.5],
                target,
                candidate,
                8.0,
                0
            )
            .eligible
        );
        assert!(
            !sweeping_attack(
                1.0,
                true,
                false,
                true,
                [-2.0, 0.0, 0.5],
                target,
                candidate,
                8.0,
                0
            )
            .eligible
        );
        assert!(
            !sweeping_attack(
                1.0,
                true,
                false,
                false,
                [-4.0, 0.0, 0.5],
                target,
                candidate,
                8.0,
                0
            )
            .eligible
        );
        assert_eq!(sweeping_edge_damage(0, 8.0), 1.0);
        assert!((sweeping_edge_damage(1, 8.0) - 5.0).abs() < 0.0001);
    }

    #[test]
    fn remaining_enchantment_calculations_match_requested_scaling() {
        assert_eq!(knockback_strength(1.0, 2), 2.0);
        assert!(unbreaking_consumes_durability(2, 0.32, false));
        assert!(!unbreaking_consumes_durability(2, 0.34, false));
        assert!(unbreaking_consumes_durability(3, 0.69, true));
        assert!(!unbreaking_consumes_durability(3, 0.71, true));
        assert_eq!(lure_wait_ticks(600, 2), 400);
        assert_eq!(lure_wait_ticks(100, 2), 0);
        assert_eq!(
            luck_of_the_sea_weights(
                2,
                FishingLootWeights {
                    treasure: 5,
                    junk: 10,
                    fish: 85
                }
            ),
            FishingLootWeights {
                treasure: 9,
                junk: 8,
                fish: 85
            }
        );
        assert!(thorns_triggers(2, 0.29));
        assert!(!thorns_triggers(2, 0.30));
        assert_eq!(thorns_damage(3, 9), 4);
        assert_eq!(thorns_damage(11, 9), 11);
        assert_eq!(respiration_air_ticks(3), 1_200);
        assert!(respiration_preserves_air(2, 0.66));
        assert!(!respiration_preserves_air(2, 0.67));
        assert_eq!(depth_strider_underwater_factor(2), 2.0 / 3.0);
        assert_eq!(depth_strider_underwater_factor(9), 1.0);
        assert!((soul_speed_multiplier(3) - 1.09).abs() < 0.0001);
        assert!((swift_sneak_speed_multiplier(3) - 0.75).abs() < 0.0001);
    }

    #[test]
    fn mining_speed_applies_tool_enchantments_effects_and_break_threshold() {
        assert_eq!(tool_base_mining_speed("wooden_pickaxe"), 2.0);
        assert_eq!(tool_base_mining_speed("diamond_sword"), 1.5);
        assert_eq!(tool_base_mining_speed("golden_axe"), 12.0);
        assert_eq!(tool_base_mining_speed("shears"), 1.0);
        assert_eq!(
            mining_speed_with_modifiers(6.0, true, 2, None, None, false, true),
            11.0
        );
        assert_eq!(
            mining_speed_with_modifiers(6.0, false, 2, None, None, false, true),
            6.0
        );
        assert!(
            (mining_speed_with_modifiers(6.0, false, 0, Some(1), None, false, true) - 8.4).abs()
                < 0.0001
        );
        assert!(
            (mining_speed_with_modifiers(6.0, false, 0, None, Some(1), true, false) - 0.0216).abs()
                < 0.0001
        );
        assert!(
            (mining_speed_with_modifiers(6.0, false, 0, None, Some(3), false, true) - 0.00486)
                .abs()
                < 0.00001
        );
        assert!(instant_mining_break(30.0, 1.0));
        assert!(!instant_mining_break(29.9, 1.0));
        assert!(instant_mining_break(0.0, 0.0));
        assert!((mining_damage_per_tick(6.0, 2.0) - 0.1).abs() < 0.0001);
    }

    #[test]
    fn durability_lookup_covers_every_damageable_item_family() {
        for tier_and_expected in [
            ("wooden", 59),
            ("stone", 131),
            ("iron", 250),
            ("golden", 32),
            ("diamond", 1_561),
            ("netherite", 2_031),
        ] {
            for kind in ["sword", "pickaxe", "axe", "shovel", "hoe"] {
                assert_eq!(
                    item_max_durability(&format!("{}_{}", tier_and_expected.0, kind)),
                    Some(tier_and_expected.1)
                );
            }
        }
        assert_eq!(item_max_durability("leather_helmet"), Some(55));
        assert_eq!(item_max_durability("iron_chestplate"), Some(240));
        assert_eq!(item_max_durability("diamond_leggings"), Some(495));
        assert_eq!(item_max_durability("netherite_boots"), Some(481));
        assert_eq!(item_max_durability("turtle_helmet"), Some(275));
        for (item, durability) in [
            ("bow", 384),
            ("crossbow", 465),
            ("trident", 250),
            ("elytra", 432),
            ("shield", 336),
            ("flint_and_steel", 64),
            ("shears", 238),
            ("brush", 64),
            ("mace", 500),
            ("wolf_armor", 64),
            ("fishing_rod", 64),
            ("carrot_on_a_stick", 25),
            ("warped_fungus_on_a_stick", 100),
        ] {
            assert_eq!(item_max_durability(item), Some(durability), "{item}");
        }
        assert_eq!(item_max_durability("stone"), None);
    }
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
                || comp_get(&right.2, COMP_STORED_ENCHANTMENTS).is_some();
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
        let new_name = (!anvil_name_is_blank(name)).then(|| nbt_string(name));
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
    if repair_cost != 0 {
        comp_set_varint(&mut output.2, COMP_PRIOR_WORK, i64::from(repair_cost));
    } else {
        // A zero repair cost belongs to the default component map.
        output
            .2
            .retain(|(component, _)| *component != COMP_PRIOR_WORK);
    }
    Ok(AnvilResult {
        stack: output,
        level_cost,
        right_consumed,
    })
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default)]
struct AnvilMenu {
    left: Stack,
    right: Stack,
    rename: Option<String>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl AnvilMenu {
    fn rename(&mut self, text: &str) -> bool {
        let filtered: String = text
            .chars()
            .filter(|&ch| ch >= ' ' && ch != '\u{7f}' && ch != '§')
            .collect();
        if filtered.encode_utf16().count() > 50 || self.rename.as_ref() == Some(&filtered) {
            return false;
        }
        self.rename = Some(filtered);
        true
    }

    /// `left_name` is the plain hover name of the left input, including custom
    /// styles flattened to text or the server's translation of its item name.
    fn result(&self, left_name: &str, creative: bool) -> Result<AnvilResult, AnvilError> {
        let text = self.rename.as_deref().unwrap_or_default();
        let rename = (anvil_name_is_blank(text) || text != left_name).then_some(text);
        anvil_result(&self.left, Some(&self.right), rename, creative)
    }

    /// The destination preflight must cover the entire result. The wear callback
    /// supplies a player RNG sample in [0, 1) if the block is still an anvil.
    fn take_result(
        &mut self,
        left_name: &str,
        xp: &mut PlayerXpState,
        creative: bool,
        can_receive: impl FnOnce(&Stack) -> bool,
        anvil_damage_roll: impl FnOnce() -> Option<f32>,
    ) -> Option<AnvilTakeResult> {
        let result = self.result(left_name, creative).ok()?;
        if result.level_cost <= 0
            || (!creative && xp.level < result.level_cost)
            || !can_receive(&result.stack)
        {
            return None;
        }
        let mut damage_anvil = false;
        if !creative {
            if let Some(roll) = anvil_damage_roll() {
                if !(0.0..1.0).contains(&roll) {
                    return None;
                }
                damage_anvil = roll < 0.12;
            }
            xp.give_experience_levels(-result.level_cost);
        }
        self.left = Stack::default();
        consume_menu_stack(&mut self.right, result.right_consumed);
        Some(AnvilTakeResult {
            output: result.stack,
            damage_anvil,
        })
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
struct AnvilTakeResult {
    output: Stack,
    damage_anvil: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
fn anvil_window_properties(repair_cost: i32) -> [WindowPropertyUpdate; 1] {
    [WindowPropertyUpdate {
        property: 0,
        value: repair_cost as i16,
    }]
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

impl BrewingIngredient {
    // PotionBrewing bootstraps these mixes in code in 1.21.1. GlassBottle
    // represents the dragon-breath remainder and cannot be placed here.
    #[cfg_attr(not(test), allow(dead_code))]
    fn from_item(name: &str) -> Option<Self> {
        Some(match name {
            "nether_wart" => Self::NetherWart,
            "fermented_spider_eye" => Self::FermentedSpiderEye,
            "golden_carrot" => Self::GoldenCarrot,
            "rabbit_foot" => Self::RabbitFoot,
            "magma_cream" => Self::MagmaCream,
            "sugar" => Self::Sugar,
            "turtle_helmet" => Self::TurtleHelmet,
            "pufferfish" => Self::Pufferfish,
            "glistering_melon_slice" => Self::GlisteringMelonSlice,
            "spider_eye" => Self::SpiderEye,
            "ghast_tear" => Self::GhastTear,
            "blaze_powder" => Self::BlazePowder,
            "phantom_membrane" => Self::PhantomMembrane,
            "breeze_rod" => Self::BreezeRod,
            "slime_block" => Self::SlimeBlock,
            "stone" => Self::Stone,
            "cobweb" => Self::Cobweb,
            "redstone" => Self::Redstone,
            "glowstone_dust" => Self::GlowstoneDust,
            "gunpowder" => Self::Gunpowder,
            "dragon_breath" => Self::DragonBreath,
            _ => return None,
        })
    }
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
fn brewing_window_properties(stand: &BrewingStand) -> [WindowPropertyUpdate; 2] {
    [
        WindowPropertyUpdate {
            property: 0,
            value: stand.brew_time as i16,
        },
        WindowPropertyUpdate {
            property: 1,
            value: i16::from(stand.fuel),
        },
    ]
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default)]
struct BrewingMenu {
    slots: [Stack; 5],
}

#[cfg_attr(not(test), allow(dead_code))]
impl BrewingMenu {
    /// Handle a primary or secondary pickup click on a stand slot. The
    /// stand's block entity owns these five stacks when a menu is open.
    fn click_slot(&mut self, slot: i16, button: i8, cursor: &mut Stack) -> bool {
        let Ok(slot) = usize::try_from(slot) else {
            return false;
        };
        let Some(target) = self.slots.get_mut(slot) else {
            return false;
        };
        if !matches!(button, 0 | 1) {
            return false;
        }

        if cursor.1 == 0 {
            if target.1 == 0 {
                return false;
            }
            let count = if button == 0 {
                target.1
            } else {
                target.1.div_ceil(2)
            };
            *cursor = (target.0, count, target.2.clone());
            consume_menu_stack(target, count);
            return true;
        }

        let same_item = target.0 == cursor.0
            && target.2.len() == cursor.2.len()
            && target
                .2
                .iter()
                .all(|(id, data)| comp_get(&cursor.2, *id) == Some(data.as_slice()));
        let maximum = stack_max_of(cursor) as u8;
        let limit = match (slot, item_to_name(cursor.0)) {
            (0..=2, Some("potion" | "splash_potion" | "lingering_potion" | "glass_bottle")) => 1,
            (3, Some(name)) if BrewingIngredient::from_item(name).is_some() => maximum,
            (4, Some("blaze_powder")) => maximum,
            _ => {
                // A returned glass bottle cannot be placed in the ingredient
                // slot, but its contents can refill a matching carried stack.
                let moved = target.1.min(maximum.saturating_sub(cursor.1));
                if !same_item || moved == 0 {
                    return false;
                }
                cursor.1 += moved;
                consume_menu_stack(target, moved);
                return true;
            }
        };

        if target.1 == 0 || same_item {
            let requested = if button == 0 { cursor.1 } else { 1 };
            let moved = requested.min(limit.saturating_sub(target.1));
            if moved == 0 {
                return false;
            }
            *target = (cursor.0, target.1 + moved, cursor.2.clone());
            consume_menu_stack(cursor, moved);
            true
        } else if cursor.1 <= limit {
            std::mem::swap(cursor, target);
            true
        } else {
            false
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
#[derive(Default)]
struct SmithingMenu {
    template: Stack,
    base: Stack,
    addition: Stack,
}

#[cfg_attr(not(test), allow(dead_code))]
impl SmithingMenu {
    fn result(&self) -> Option<Stack> {
        smithing_result(&self.template, &self.base, &self.addition)
    }

    fn take_result(&mut self, can_receive: impl FnOnce(&Stack) -> bool) -> Option<Stack> {
        let result = self.result().filter(can_receive)?;
        for stack in [&mut self.template, &mut self.base, &mut self.addition] {
            consume_menu_stack(stack, 1);
        }
        Some(result)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn consume_menu_stack(stack: &mut Stack, count: u8) {
    stack.1 = stack.1.saturating_sub(count);
    if stack.1 == 0 {
        *stack = Stack::default();
    }
}

fn xp_needed_for_next_level(level: i32) -> i64 {
    let level = i64::from(level);
    match level {
        30.. => 112 + (level - 30) * 9,
        15..=29 => 37 + (level - 15) * 5,
        _ => 7 + level * 2,
    }
}

fn xp_points_for_level(level: i32) -> i64 {
    (0..level).map(xp_needed_for_next_level).sum()
}

/// /xp command state, with the level bar tracked separately from total XP.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
struct PlayerXpState {
    pub level: i32,
    pub progress: f32,
    pub total_experience: i32,
}

#[cfg_attr(not(test), allow(dead_code))]
enum ExperienceUnit {
    Levels,
    Points,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
enum ExperienceSetError {
    NegativeAmount,
    PointsOutOfRange,
}

impl PlayerXpState {
    // Player#getXpNeededForNextLevel uses Java int arithmetic.
    #[cfg_attr(not(test), allow(dead_code))]
    fn points_to_next_level(&self) -> i32 {
        xp_needed_for_next_level(self.level) as i32
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn give_experience_points(&mut self, pts: i32) {
        self.progress += pts as f32 / self.points_to_next_level() as f32;
        self.total_experience = self.total_experience.wrapping_add(pts).max(0);
        while self.progress < 0.0 {
            let below = self.progress * self.points_to_next_level() as f32;
            if self.level > 0 {
                self.give_experience_levels(-1);
                self.progress = 1.0 + below / self.points_to_next_level() as f32;
            } else {
                self.give_experience_levels(-1);
                self.progress = 0.0;
            }
        }
        while self.progress >= 1.0 {
            self.progress = (self.progress - 1.0) * self.points_to_next_level() as f32;
            self.give_experience_levels(1);
            self.progress /= self.points_to_next_level() as f32;
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn give_experience_levels(&mut self, delta: i32) {
        let new_level = self.level.wrapping_add(delta);
        if new_level < 0 {
            self.level = 0;
            self.progress = 0.0;
            self.total_experience = 0;
        } else {
            self.level = new_level;
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn set(&mut self, amount: i32, unit: ExperienceUnit) -> Result<(), ExperienceSetError> {
        if amount < 0 {
            return Err(ExperienceSetError::NegativeAmount);
        }
        match unit {
            ExperienceUnit::Levels => self.level = amount,
            ExperienceUnit::Points => {
                let needed = self.points_to_next_level();
                if amount >= needed {
                    return Err(ExperienceSetError::PointsOutOfRange);
                }
                let needed = needed as f32;
                self.progress = (amount as f32 / needed).clamp(0.0, (needed - 1.0) / needed);
            }
        }
        Ok(())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn query_experience_points(&self) -> i32 {
        (self.progress * self.points_to_next_level() as f32).floor() as i32
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn query_experience_levels(&self) -> i32 {
        self.level
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
    food_level: i32,
    last_food_level: i32,
    saturation: f32,
    exhaustion: f32,
    tick_timer: i32,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
enum FoodTick {
    None,
    Heal(f32),
    Starve,
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
        self.exhaustion += amount;
        if self.exhaustion > 40.0 {
            self.exhaustion = 40.0;
        }
    }

    /// Run once per game tick after adding movement and Hunger effect exhaustion.
    /// Apply the returned healing or starvation damage through the player before
    /// the next tick, then synchronize food, saturation and health.
    #[cfg_attr(not(test), allow(dead_code))]
    fn tick(
        &mut self,
        health: f32,
        max_health: f32,
        difficulty: HungerDifficulty,
        natural_regeneration: bool,
    ) -> FoodTick {
        self.last_food_level = self.food_level;
        if self.exhaustion > 4.0 {
            self.exhaustion -= 4.0;
            if self.saturation > 0.0 {
                self.saturation = (self.saturation - 1.0).max(0.0);
            } else if difficulty != HungerDifficulty::Peaceful {
                self.food_level = self.food_level.wrapping_sub(1).max(0);
            }
        }

        let can_heal = natural_regeneration && health > 0.0 && health < max_health;
        if can_heal && self.food_level >= 20 && self.saturation > 0.0 {
            self.tick_timer = self.tick_timer.wrapping_add(1);
            if self.tick_timer >= 10 {
                let spent = self.saturation.min(6.0);
                self.add_exhaustion(spent);
                self.tick_timer = 0;
                return FoodTick::Heal(spent / 6.0);
            }
        } else if can_heal && self.food_level >= 18 {
            self.tick_timer = self.tick_timer.wrapping_add(1);
            if self.tick_timer >= 80 {
                self.add_exhaustion(6.0);
                self.tick_timer = 0;
                return FoodTick::Heal(1.0);
            }
        } else if self.food_level <= 0 {
            self.tick_timer = self.tick_timer.wrapping_add(1);
            if self.tick_timer >= 80 {
                self.tick_timer = 0;
                if health > 10.0
                    || difficulty == HungerDifficulty::Hard
                    || (health > 1.0 && difficulty == HungerDifficulty::Normal)
                {
                    return FoodTick::Starve;
                }
            }
        } else {
            self.tick_timer = 0;
        }
        FoodTick::None
    }
}

// ---------- P2: advancement rewards + remaining live gamerules ----------
//
// Every read goes through `gamerule_live_value` (world_mechanisms.rs): the
// atomic registry `/gamerule` writes first, then the stored command map —
// the same state the command reports and mutates.

/// The `rewards` object of a vanilla advancement JSON: experience to grant,
/// recipe ids to unlock in the player's book, loot tables to roll into drops
/// at the player's feet, and an optional function to run as the player.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
struct AdvancementRewards {
    experience: i32,
    recipes: Vec<String>,
    loot: Vec<String>,
    function: Option<String>,
}

/// `Advancement.performRewards`: a freshly completed advancement pays its
/// rewards once — XP through the session bar, recipe unlocks through the
/// same `0x41` path `/recipe give` uses, loot rolled into drops at the
/// player, and the function hook run with the player as the command source.
#[cfg_attr(not(test), allow(dead_code))]
fn apply_advancement_rewards(client: &Arc<Client>, rewards: &AdvancementRewards) {
    if rewards.experience != 0 {
        let _ = send_player_xp(client, |xp| add_player_xp_points(xp, rewards.experience));
    }
    if !rewards.recipes.is_empty() {
        let universe: Vec<String> = recipe_ids().lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect();
        let packets = {
            let mut books = recipe_books().lock().unwrap_or_else(|e| e.into_inner());
            let book = books
                .entry(client.uuid)
                .or_insert_with(PlayerRecipeBook::new);
            rewards
                .recipes
                .iter()
                .filter_map(|id| {
                    exec_recipe_command(book, RecipeCommandVerb::Give, &universe, id)
                })
                .flat_map(|outcome| outcome.packets)
                .collect::<Vec<_>>()
        };
        for payload in packets {
            if let Ok(mut socket) = client.sock.lock() {
                let _ = send_packet(&mut socket, 0x41, payload);
            }
        }
    }
    if !rewards.loot.is_empty() {
        let (x, y, z, ..) = *client.pos.lock().unwrap_or_else(|e| e.into_inner());
        for table in &rewards.loot {
            let pools: Vec<LootPool> = loot_tables()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(table)
                .map(|pools| {
                    pools
                        .iter()
                        .map(|pool| LootPool {
                            rolls: pool.rolls,
                            chance: pool.chance,
                            funcs: pool.funcs.clone(),
                            entries: pool
                                .entries
                                .iter()
                                .map(|entry| LootEntry {
                                    item: entry.item,
                                    min: entry.min,
                                    max: entry.max,
                                    funcs: entry.funcs.clone(),
                                })
                                .collect(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            for (item, count, comps) in roll_loot_seeded(&pools, world_rng::next_u64() as i64) {
                drops().lock().unwrap_or_else(|e| e.into_inner()).push(Drop {
                    eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                    x,
                    y,
                    z,
                    item,
                    cnt: count,
                    comps,
                    age: 0,
                    target: Some(client.uuid),
                    thrower: None,
                });
            }
        }
    }
    if let Some(function) = &rewards.function {
        run_function(function, Some(client.clone()), &mut None);
    }
}

/// Grant-path hook called when an advancement completes for `player`: the
/// spec on the definition resolves to the rewards the live client receives.
/// A player who is offline earns nothing — matching the delta, which also
/// only ships to a connected session.
#[cfg_attr(not(test), allow(dead_code))]
fn live_grant_advancement_rewards(player: &str, advancement_id: &str) {
    let Some(spec) = advancement_reward_spec(advancement_id) else {
        return;
    };
    let Some(client) = client_snapshot()
        .into_iter()
        .find(|client| client.name == player)
    else {
        return;
    };
    apply_advancement_rewards(&client, &spec.to_rewards());
}

/// `GameRules.RULE_ANNOUNCE_ADVANCEMENTS`: whether a grant writes the
/// "<player> has made the advancement" line to chat — the live broadcast
/// reads the same atomic this accessor exposes.
#[cfg_attr(not(test), allow(dead_code))]
fn announce_advancements_enabled() -> bool {
    gamerule_live_value("announceAdvancements") != 0
}

/// The four environmental damage rules plus `naturalRegeneration` all gate
/// whole damage/regen arms in vanilla `Player.tick`/`hurt`: when the rule is
/// off the event simply does not happen.
#[cfg_attr(not(test), allow(dead_code))]
fn player_damage_rule_enabled(name: &str) -> bool {
    gamerule_live_value(name) != 0
}

#[cfg_attr(not(test), allow(dead_code))]
fn drowning_damage_enabled() -> bool {
    player_damage_rule_enabled("drowningDamage")
}

#[cfg_attr(not(test), allow(dead_code))]
fn fall_damage_enabled() -> bool {
    player_damage_rule_enabled("fallDamage")
}

#[cfg_attr(not(test), allow(dead_code))]
fn fire_damage_enabled() -> bool {
    player_damage_rule_enabled("fireDamage")
}

#[cfg_attr(not(test), allow(dead_code))]
fn freeze_damage_enabled() -> bool {
    player_damage_rule_enabled("freezeDamage")
}

#[cfg_attr(not(test), allow(dead_code))]
fn natural_regeneration_enabled() -> bool {
    player_damage_rule_enabled("naturalRegeneration")
}

/// Vanilla's rule-gated damage arms return no damage at all when the rule is
/// off — there is no partial or reduced mode.
#[cfg_attr(not(test), allow(dead_code))]
fn eval_environmental_damage(rule_enabled: bool, raw_damage: f32) -> f32 {
    if rule_enabled {
        raw_damage
    } else {
        0.0
    }
}

/// `GameRules.RULE_DO_IMMEDIATE_RESPAWN`: death screens skip the respawn
/// button and revive the player on the spot.
#[cfg_attr(not(test), allow(dead_code))]
fn do_immediate_respawn_enabled() -> bool {
    gamerule_live_value("doImmediateRespawn") != 0
}

/// `GameRules.RULE_DO_LIMITED_CRAFTING`: crafting is restricted to recipes
/// the player's book has unlocked.
#[cfg_attr(not(test), allow(dead_code))]
fn do_limited_crafting_enabled() -> bool {
    gamerule_live_value("doLimitedCrafting") != 0
}

/// `ServerRecipeBook`-style check: with `doLimitedCrafting` on only an
/// unlocked recipe may produce a result; off means everything crafts.
#[cfg_attr(not(test), allow(dead_code))]
fn eval_limited_crafting_allowed(limited_crafting: bool, recipe_unlocked: bool) -> bool {
    !limited_crafting || recipe_unlocked
}

/// `spawnRadius`: the half-width of the square around the world spawn a
/// respawning player scatters into.
#[cfg_attr(not(test), allow(dead_code))]
fn spawn_radius_blocks() -> i32 {
    gamerule_live_value("spawnRadius").max(0)
}

/// `PlayerRespawnLogic`: a uniform offset in `[-radius, radius]` along each
/// horizontal axis, drawn from one raw roll.
#[cfg_attr(not(test), allow(dead_code))]
fn eval_spawn_offset(radius: i32, random_roll: u32) -> i32 {
    if radius <= 0 {
        return 0;
    }
    let span = (radius * 2 + 1) as u32;
    (random_roll % span) as i32 - radius
}

/// `playersNetherPortalDefaultDelay` / `playersNetherPortalCreativeDelay`:
/// game ticks a player stands inside a portal before the dimension change —
/// 80 in survival, 1 in creative by default.
#[cfg_attr(not(test), allow(dead_code))]
fn players_nether_portal_delay_ticks(creative: bool) -> i32 {
    gamerule_live_value(if creative {
        "playersNetherPortalCreativeDelay"
    } else {
        "playersNetherPortalDefaultDelay"
    })
    .max(0)
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
        let component = crafting_enchantment_component(&result);
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
        for (level, total) in [(30, 1395), (31, 1507), (32, 1628)] {
            assert_eq!(xp_points_for_level(level), total);
            assert_eq!(xp_level(total), (level, 0.0));
        }
    }

    #[test]
    fn xp_set_levels_preserves_progress_and_total_experience() {
        let mut xp = PlayerXpState {
            level: 20,
            progress: 0.75,
            total_experience: 1300,
        };
        xp.set(31, ExperienceUnit::Levels).unwrap();
        assert_eq!(
            xp,
            PlayerXpState {
                level: 31,
                progress: 0.75,
                total_experience: 1300
            }
        );
        assert_eq!(xp.query_experience_levels(), 31);
        assert_eq!(xp.query_experience_points(), 90);
        xp.set(0, ExperienceUnit::Levels).unwrap();
        assert_eq!(
            (xp.level, xp.progress, xp.total_experience),
            (0, 0.75, 1300)
        );
        assert_eq!(
            xp.set(-10, ExperienceUnit::Levels),
            Err(ExperienceSetError::NegativeAmount)
        );
        assert_eq!(
            (xp.level, xp.progress, xp.total_experience),
            (0, 0.75, 1300)
        );
    }

    #[test]
    fn xp_set_points_rejects_out_of_bar_values_without_mutation() {
        let mut xp = PlayerXpState {
            level: 30,
            progress: 0.5,
            total_experience: 1395,
        };
        assert_eq!(
            xp.set(113, ExperienceUnit::Points),
            Err(ExperienceSetError::PointsOutOfRange)
        );
        // 1.21.1 ExperienceCommand also rejects exactly the next-level cost.
        assert_eq!(
            xp.set(112, ExperienceUnit::Points),
            Err(ExperienceSetError::PointsOutOfRange)
        );
        assert_eq!(
            xp.set(-1, ExperienceUnit::Points),
            Err(ExperienceSetError::NegativeAmount)
        );
        assert_eq!(
            xp,
            PlayerXpState {
                level: 30,
                progress: 0.5,
                total_experience: 1395
            }
        );
        assert_eq!(xp.set(111, ExperienceUnit::Points), Ok(()));
        assert_eq!(xp.progress, 111.0 / 112.0);
        assert_eq!(xp.query_experience_points(), 111);
        assert_eq!(xp.total_experience, 1395);
        xp.set(0, ExperienceUnit::Levels).unwrap();
        xp.set(6, ExperienceUnit::Points).unwrap();
        assert_eq!(xp.query_experience_points(), 6);
        assert_eq!(xp.progress, 6.0 / 7.0);
    }

    #[test]
    fn xp_add_crosses_30_31_bar_and_handles_negative_underflow() {
        let mut xp = PlayerXpState {
            level: 30,
            progress: 0.5,
            total_experience: 1500,
        };
        xp.give_experience_points(56);
        assert_eq!(
            (xp.level, xp.progress, xp.total_experience),
            (31, 0.0, 1556)
        );
        xp.give_experience_points(121);
        assert_eq!(
            (xp.level, xp.progress, xp.total_experience),
            (32, 0.0, 1677)
        );
        xp.give_experience_points(-122);
        assert_eq!(xp.level, 30);
        let expected_progress = 111.0 / 112.0;
        assert!((xp.progress - expected_progress).abs() < 1e-6);
        xp.give_experience_levels(1);
        assert_eq!(xp.level, 31);
        assert!((xp.progress - expected_progress).abs() < 1e-6);
        xp.give_experience_levels(-2);
        assert_eq!(xp.level, 29);
        assert!((xp.progress - expected_progress).abs() < 1e-6);
        xp.give_experience_levels(-29);
        assert_eq!(xp.level, 0);
        assert!((xp.progress - expected_progress).abs() < 1e-6);
        xp.give_experience_levels(-1);
        assert_eq!((xp.level, xp.progress, xp.total_experience), (0, 0.0, 0));

        xp.give_experience_points(7);
        assert_eq!((xp.level, xp.progress, xp.total_experience), (1, 0.0, 7));
        xp.give_experience_points(-500);
        assert_eq!((xp.level, xp.progress, xp.total_experience), (0, 0.0, 0));
    }

    #[test]
    fn window_properties_encode_protocol_767_menu_slots() {
        let mut menu = EnchantmentMenu {
            item: stack("diamond_sword", 1),
            ..EnchantmentMenu::default()
        };
        menu.refresh_offers(12345, 15);
        let props = enchanting_window_properties(&menu);
        let offers = &menu.offers;
        assert_eq!(props.map(|p| p.property), [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(props[0].value, 6);
        assert_eq!(props[1].value, 20);
        assert_eq!(props[2].value, 30);
        assert_eq!(props[3].encode(7), [7, 0, 3, 0x30, 0x39]);
        for slot in 0..3 {
            assert_eq!(props[4 + slot].value, offers[slot].clue.unwrap().0 as i16);
            assert_eq!(props[7 + slot].value, offers[slot].clue.unwrap().1 as i16);
        }
        menu.item = stack("dirt", 1);
        menu.refresh_offers(-1, 0);
        let props = enchanting_window_properties(&menu);
        assert_eq!(props[3].value, -1);
        assert_eq!(
            props[4..7].iter().map(|p| p.value).collect::<Vec<_>>(),
            [-1; 3]
        );
        assert_eq!(
            props[7..10].iter().map(|p| p.value).collect::<Vec<_>>(),
            [-1; 3]
        );

        let mut stand = BrewingStand {
            brew_time: 400,
            fuel: 20,
            ..BrewingStand::default()
        };
        assert_eq!(
            brewing_window_properties(&stand),
            [
                WindowPropertyUpdate {
                    property: 0,
                    value: 400
                },
                WindowPropertyUpdate {
                    property: 1,
                    value: 20
                }
            ]
        );
        stand.brew_time = 600;
        stand.fuel = 100;
        assert_eq!(brewing_window_properties(&stand)[0].value, 600);
        assert_eq!(brewing_window_properties(&stand)[1].value, 100);
        assert_eq!(
            brewing_window_properties(&BrewingStand::default())[0].encode(4),
            [4, 0, 0, 0, 0]
        );
        assert_eq!(
            anvil_window_properties(39),
            [WindowPropertyUpdate {
                property: 0,
                value: 39
            }]
        );
        assert_eq!(anvil_window_properties(40)[0].encode(9), [9, 0, 0, 0, 40]);
    }

    #[test]
    fn anvil_take_checks_costs_and_damage_chance() {
        let repair = || {
            let mut left = stack("diamond_sword", 1);
            comp_set_varint(&mut left.2, COMP_DAMAGE, 100);
            AnvilMenu {
                left,
                right: stack("diamond", 2),
                ..AnvilMenu::default()
            }
        };
        let mut menu = repair();
        let mut xp = experience(0);
        assert!(menu
            .take_result(
                "Diamond Sword",
                &mut xp,
                false,
                |_| true,
                || panic!("unaffordable")
            )
            .is_none());
        xp.level = 10;
        assert!(menu
            .take_result(
                "Diamond Sword",
                &mut xp,
                false,
                |_| false,
                || panic!("destination full")
            )
            .is_none());
        assert!(menu
            .take_result("Diamond Sword", &mut xp, false, |_| true, || Some(f32::NAN))
            .is_none());
        assert_eq!(xp, experience(10));
        assert_eq!(menu.right, stack("diamond", 2));

        let preview = menu.result("Diamond Sword", false).unwrap();
        let taken = menu
            .take_result("Diamond Sword", &mut xp, false, |_| true, || Some(0.1199))
            .unwrap();
        assert_eq!(taken.output, preview.stack);
        assert_eq!(menu.right, stack("diamond", 1));
        assert_eq!(xp, experience(9));
        assert!(taken.damage_anvil);
        assert!(
            !repair()
                .take_result("Diamond Sword", &mut xp, false, |_| true, || Some(0.12))
                .unwrap()
                .damage_anvil
        );

        let mut menu = repair();
        comp_set_varint(&mut menu.left.2, COMP_PRIOR_WORK, 39);
        xp.level = 40;
        assert_eq!(
            menu.result("Diamond Sword", false),
            Err(AnvilError::TooExpensive(40))
        );
        assert!(menu
            .take_result(
                "Diamond Sword",
                &mut xp,
                false,
                |_| true,
                || panic!("too expensive")
            )
            .is_none());
        xp.level = 0;
        let taken = menu
            .take_result(
                "Diamond Sword",
                &mut xp,
                true,
                |_| true,
                || panic!("creative"),
            )
            .unwrap();
        assert_eq!(xp, experience(0));
        assert!(!taken.damage_anvil);
        assert!(menu
            .take_result("Diamond Sword", &mut xp, true, |_| true, || None)
            .is_none());
    }

    #[test]
    fn smithing_take_preserves_components_and_supports_trims() {
        let mut menu = SmithingMenu {
            template: stack("netherite_upgrade_smithing_template", 4),
            base: enchanted_stack("diamond_sword", &[("sharpness", 3)]),
            addition: stack("netherite_ingot", 9),
        };
        comp_set_varint(&mut menu.base.2, COMP_DAMAGE, 40);
        let components = menu.base.2.clone();
        let result = menu.take_result(|_| true).unwrap();
        assert_eq!(item_to_name(result.0), Some("netherite_sword"));
        assert_eq!(result.1, 1);
        assert_eq!(result.2, components);
        assert_eq!((menu.template.1, menu.base.1, menu.addition.1), (3, 0, 8));
        menu.template.1 = 0;
        menu.base = stack("diamond_sword", 1);
        assert_eq!(menu.take_result(|_| true), None);

        let mut trim = SmithingMenu {
            template: stack("bolt_armor_trim_smithing_template", 1),
            base: stack("iron_chestplate", 1),
            addition: stack("amethyst_shard", 1),
        };
        let result = trim.take_result(|_| true).unwrap();
        assert_eq!(
            comp_get(&result.2, COMP_ARMOR_TRIM),
            Some([1, 1, 1].as_slice())
        );
    }

    #[test]
    fn xp_set_points_checks_the_current_level_and_only_changes_progress() {
        for level in [0, 14, 15, 29, 30, 1_000_000] {
            let mut xp = PlayerXpState {
                level,
                progress: 0.25,
                total_experience: 726,
            };
            let needed = xp_needed_for_next_level(level) as i32;
            xp.set(30, ExperienceUnit::Levels).unwrap();
            assert_eq!(
                (xp.level, xp.progress, xp.total_experience),
                (30, 0.25, 726)
            );
            xp.set(level, ExperienceUnit::Levels).unwrap();
            let original = xp;
            assert_eq!(
                xp.set(needed, ExperienceUnit::Points),
                Err(ExperienceSetError::PointsOutOfRange)
            );
            assert_eq!(
                xp.set(-1, ExperienceUnit::Points),
                Err(ExperienceSetError::NegativeAmount)
            );
            assert_eq!(xp, original);
            xp.set(needed - 1, ExperienceUnit::Points).unwrap();
            assert_eq!(xp.progress, (needed - 1) as f32 / needed as f32);
            xp.set(0, ExperienceUnit::Points).unwrap();
            assert_eq!(
                xp,
                PlayerXpState {
                    progress: 0.0,
                    ..original
                }
            );
        }

        let mut xp = PlayerXpState {
            level: 300_000_000,
            progress: 0.5,
            total_experience: 7,
        };
        assert_eq!(
            xp.set(0, ExperienceUnit::Points),
            Err(ExperienceSetError::PointsOutOfRange)
        );
        assert_eq!(xp.progress, 0.5);
    }

    #[test]
    fn hunger_consumes_one_exhaustion_chunk_with_saturation_first() {
        let mut food = FoodData {
            exhaustion: 4.0,
            saturation: 0.5,
            ..FoodData::default()
        };
        let health = 20.0;
        food.tick(health, 20.0, HungerDifficulty::Normal, true);
        assert_eq!(
            (food.exhaustion, food.saturation, food.food_level),
            (4.0, 0.5, 20)
        );
        food.add_exhaustion(5.0);
        food.tick(health, 20.0, HungerDifficulty::Normal, true);
        assert_eq!(
            (food.exhaustion, food.saturation, food.food_level),
            (5.0, 0.0, 20)
        );
        food.tick(health, 20.0, HungerDifficulty::Normal, true);
        assert_eq!(
            (food.exhaustion, food.food_level, food.last_food_level),
            (1.0, 19, 20)
        );

        food.add_exhaustion(100.0);
        assert_eq!(food.exhaustion, 40.0);
        food.tick(health, 20.0, HungerDifficulty::Peaceful, true);
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
        for _ in 0..9 {
            assert_eq!(
                rapid.tick(10.0, 20.0, HungerDifficulty::Normal, true),
                FoodTick::None
            );
        }
        assert_eq!(
            rapid.tick(10.0, 20.0, HungerDifficulty::Normal, true),
            FoodTick::Heal(0.5)
        );
        assert_eq!((rapid.exhaustion, rapid.saturation), (3.0, 3.0));

        for _ in 0..79 {
            assert_eq!(
                standard.tick(10.5, 20.0, HungerDifficulty::Easy, true),
                FoodTick::None
            );
        }
        assert_eq!(
            standard.tick(10.5, 20.0, HungerDifficulty::Easy, false),
            FoodTick::None
        );
        assert_eq!(standard.tick_timer, 0);
        for _ in 0..79 {
            assert_eq!(
                standard.tick(10.5, 20.0, HungerDifficulty::Easy, true),
                FoodTick::None
            );
        }
        assert_eq!(
            standard.tick(10.5, 20.0, HungerDifficulty::Easy, true),
            FoodTick::Heal(1.0)
        );
        assert_eq!(standard.exhaustion, 6.0);

        let mut shared = FoodData {
            food_level: 18,
            ..FoodData::default()
        };
        for _ in 0..9 {
            assert_eq!(
                shared.tick(11.5, 20.0, HungerDifficulty::Normal, true),
                FoodTick::None
            );
        }
        shared.food_level = 20;
        shared.saturation = 8.0;
        assert_eq!(
            shared.tick(11.5, 20.0, HungerDifficulty::Normal, true),
            FoodTick::Heal(1.0)
        );
        assert_eq!(shared.exhaustion, 6.0);
    }

    #[test]
    fn hunger_uses_post_exhaustion_saturation_and_requests_unclipped_healing() {
        let mut food = FoodData {
            saturation: 4.0,
            exhaustion: 4.5,
            tick_timer: 9,
            ..FoodData::default()
        };
        assert_eq!(
            food.tick(19.75, 20.0, HungerDifficulty::Peaceful, true),
            FoodTick::Heal(0.5)
        );
        assert_eq!(
            (food.exhaustion, food.saturation, food.food_level),
            (3.5, 3.0, 20)
        );

        food.tick_timer = 9;
        assert_eq!(
            food.tick(20.0, 20.0, HungerDifficulty::Normal, true),
            FoodTick::None
        );
        assert_eq!(food.tick_timer, 0);
    }

    #[test]
    fn hunger_starves_every_eighty_ticks_with_difficulty_limits() {
        for (difficulty, health, blocked) in [
            (HungerDifficulty::Peaceful, 10.25, Some(10.0)),
            (HungerDifficulty::Easy, 10.25, Some(10.0)),
            (HungerDifficulty::Normal, 1.25, Some(1.0)),
            (HungerDifficulty::Hard, 1.0, None),
        ] {
            let mut food = FoodData {
                food_level: 1,
                saturation: 0.0,
                exhaustion: 5.0,
                ..FoodData::default()
            };
            // Exhaustion can start the starvation timer on this same tick.
            if difficulty == HungerDifficulty::Peaceful {
                food.food_level = 0;
            }
            for _ in 0..79 {
                assert_eq!(food.tick(health, 20.0, difficulty, false), FoodTick::None);
            }
            assert_eq!(food.food_level, 0);
            assert_eq!(food.tick(health, 20.0, difficulty, false), FoodTick::Starve);
            assert_eq!(food.tick_timer, 0);
            if let Some(health) = blocked {
                food.tick_timer = 79;
                assert_eq!(food.tick(health, 20.0, difficulty, true), FoodTick::None);
                assert_eq!(food.tick_timer, 0);
            }
        }
    }

    fn experience(level: i32) -> PlayerXpState {
        PlayerXpState {
            level,
            progress: 0.5,
            total_experience: 42,
        }
    }

    #[test]
    fn enchanting_menu_uses_displayed_offer_and_charges_the_selected_price() {
        let mut menu = EnchantmentMenu {
            item: stack("diamond_sword", 1),
            lapis: stack("lapis_lazuli", 2),
            ..EnchantmentMenu::default()
        };
        let mut seed = 12345;
        let mut rng = generation_math::Lcg::new(12345);
        let rng_before = rng.state48();
        menu.refresh_offers(seed, 15);
        let mut xp = experience(menu.offers[2].required_level);
        let before = xp;
        let selected = menu.offers[2].enchantments.clone();
        for invalid in [-1, 3, 2] {
            assert_eq!(
                menu.click_button(invalid, &mut xp, &mut seed, false, &mut rng),
                EnchantingClick::Rejected
            );
        }
        assert_eq!((xp, seed, rng.state48()), (before, 12345, rng_before));

        assert!(menu.item.2.is_empty());
        menu.lapis = stack("redstone", 4);
        assert_eq!(
            menu.click_button(2, &mut xp, &mut seed, false, &mut rng),
            EnchantingClick::Rejected
        );
        menu.lapis = stack("lapis_lazuli", 4);
        xp.level -= 1;
        assert_eq!(
            menu.click_button(2, &mut xp, &mut seed, false, &mut rng),
            EnchantingClick::Rejected
        );
        xp.level += 1;
        assert_eq!(
            menu.click_button(2, &mut xp, &mut seed, false, &mut rng),
            EnchantingClick::Enchanted
        );
        assert_eq!(xp.level, before.level - 3);
        assert_eq!((xp.progress, xp.total_experience), (0.5, 42));
        assert_eq!(seed, 1_553_932_502);
        assert_eq!(menu.lapis, stack("lapis_lazuli", 1));
        assert_eq!(enchantments_get(&menu.item.2), selected);
        assert_eq!(menu.enchantment_seed, seed);
        assert_eq!(
            menu.click_button(0, &mut xp, &mut seed, false, &mut rng),
            EnchantingClick::Rejected
        );
        assert!(menu.offers.iter().all(|offer| offer.required_level == 0));
    }

    #[test]
    fn enchanting_menu_creative_uses_no_lapis_and_resets_xp_below_the_price() {
        let mut menu = EnchantmentMenu {
            item: stack("book", 1),
            ..EnchantmentMenu::default()
        };
        let mut xp = experience(0);
        let mut seed = 12345;
        let mut rng = generation_math::Lcg::new(5678);
        menu.refresh_offers(seed, 15);
        assert_eq!(
            menu.click_button(1, &mut xp, &mut seed, true, &mut rng),
            EnchantingClick::Enchanted
        );
        assert_eq!(
            xp,
            PlayerXpState {
                level: 0,
                progress: 0.0,
                total_experience: 0
            }
        );
        assert_eq!(item_to_name(menu.item.0), Some("enchanted_book"));
        assert_eq!(menu.lapis, Stack::default());
    }

    #[test]
    fn enchanting_menu_converts_a_book_and_preserves_its_custom_name() {
        for slot in 0..2 {
            let mut menu = EnchantmentMenu {
                item: stack("book", 1),
                lapis: stack("lapis_lazuli", 3),
                ..EnchantmentMenu::default()
            };
            let name = nbt_string("Field Notes");
            comp_set(&mut menu.item.2, COMP_CUSTOM_NAME, name.clone());
            let mut xp = experience(30);
            let mut seed = 12345;
            let mut rng = generation_math::Lcg::new(12345);
            menu.refresh_offers(seed, 15);
            assert_eq!(
                menu.click_button(slot, &mut xp, &mut seed, false, &mut rng),
                EnchantingClick::Enchanted
            );
            assert_eq!(xp.level, 29 - slot);
            assert_eq!(menu.lapis.1, 2 - slot as u8);
            assert_eq!(item_to_name(menu.item.0), Some("enchanted_book"));
            assert!(comp_get(&menu.item.2, COMP_STORED_ENCHANTMENTS).is_some());
            assert_eq!(
                comp_get(&menu.item.2, COMP_CUSTOM_NAME),
                Some(name.as_slice())
            );
        }
    }

    #[test]
    fn enchanting_menu_accepts_an_empty_roll_without_spending_or_advancing_rng() {
        let mut menu = EnchantmentMenu {
            item: stack("dirt", 1),
            ..EnchantmentMenu::default()
        };
        menu.offers[0].required_level = 1;
        let mut xp = experience(30);
        let mut seed = 12345;
        let mut rng = generation_math::Lcg::new(5678);
        let before = rng.state48();
        assert_eq!(
            menu.click_button(0, &mut xp, &mut seed, true, &mut rng),
            EnchantingClick::Accepted
        );
        assert_eq!(xp, experience(30));
        assert_eq!((seed, rng.state48()), (12345, before));
        assert_eq!(menu.item, stack("dirt", 1));
    }

    #[test]
    fn anvil_menu_filters_rename_and_preserves_the_prior_work_of_rename_only() {
        let mut menu = AnvilMenu {
            left: stack("diamond_sword", 1),
            ..AnvilMenu::default()
        };
        comp_set_varint(&mut menu.left.2, COMP_PRIOR_WORK, 63);

        assert!(menu.rename("Diamond Sword"));
        assert_eq!(
            menu.result("Diamond Sword", false),
            Err(AnvilError::InvalidInput)
        );
        assert!(menu.rename(&format!("R{}", "🗡".repeat(24))));
        assert!(!menu.rename(&format!("R{}", "🗡".repeat(25))));
        assert!(menu.rename("N§a\nme\u{7f}"));
        assert!(!menu.rename("Name"));

        let result = menu.result("Diamond Sword", false).unwrap();
        assert_eq!(result.level_cost, 39);
        assert_eq!(comp_varint(&result.stack.2, COMP_PRIOR_WORK), Some(63));
        assert_eq!(
            comp_get(&result.stack.2, COMP_CUSTOM_NAME),
            Some(nbt_string("Name").as_slice())
        );
        let mut xp = experience(38);
        assert!(menu
            .take_result("Diamond Sword", &mut xp, false, |_| true, || None)
            .is_none());
        xp.level = 39;
        assert!(menu
            .take_result("Diamond Sword", &mut xp, false, |_| false, || None)
            .is_none());
        assert_eq!(xp.level, 39);
        assert_eq!(
            menu.take_result("Diamond Sword", &mut xp, false, |_| true, || None),
            Some(AnvilTakeResult {
                output: result.stack,
                damage_anvil: false
            })
        );
        assert_eq!(xp.level, 0);
        assert_eq!((xp.progress, xp.total_experience), (0.5, 42));
        assert!(menu
            .take_result("Diamond Sword", &mut xp, false, |_| true, || None)
            .is_none());
    }

    #[test]
    fn anvil_menu_takes_only_material_used_and_advances_the_repair_cost() {
        let mut menu = AnvilMenu {
            left: stack("diamond_sword", 1),
            right: stack("diamond", 6),
            ..AnvilMenu::default()
        };
        comp_set_varint(&mut menu.left.2, COMP_DAMAGE, 600);
        comp_set_varint(&mut menu.left.2, COMP_PRIOR_WORK, 3);
        comp_set(&mut menu.left.2, COMP_CUSTOM_NAME, nbt_string("Old"));
        assert!(menu.rename("é"));
        let mut xp = experience(6);
        let result = menu
            .take_result("Old", &mut xp, false, |_| true, || None)
            .unwrap();
        assert_eq!(xp.level, 0);
        assert_eq!(menu.left, Stack::default());
        assert_eq!(menu.right, stack("diamond", 4));
        assert_eq!(comp_varint(&result.output.2, COMP_PRIOR_WORK), Some(7));
        assert_eq!(comp_get(&result.output.2, COMP_CUSTOM_NAME), None);
    }

    #[test]
    fn anvil_menu_does_not_add_prior_work_to_fresh_rename() {
        let mut menu = AnvilMenu {
            left: stack("diamond_sword", 1),
            ..AnvilMenu::default()
        };
        assert!(menu.rename("Field Sword"));
        let result = menu.result("Diamond Sword", false).unwrap();
        assert_eq!(result.level_cost, 1);
        assert_eq!(comp_get(&result.stack.2, COMP_PRIOR_WORK), None);
        comp_set_varint(&mut menu.left.2, COMP_PRIOR_WORK, 0);
        assert_eq!(menu.result("Diamond Sword", false).unwrap(), result);
    }

    #[test]
    fn smithing_menu_rechecks_three_inputs_and_consumes_one_each_on_take() {
        let mut menu = SmithingMenu {
            template: stack("netherite_upgrade_smithing_template", 2),
            base: stack("diamond_pickaxe", 1),
            addition: stack("netherite_ingot", 2),
        };
        comp_set_varint(&mut menu.base.2, COMP_DAMAGE, 100);
        let preview = menu.result().unwrap();
        assert_eq!(item_to_name(preview.0), Some("netherite_pickaxe"));
        assert_eq!(comp_varint(&preview.2, COMP_DAMAGE), Some(100));
        assert!(menu.take_result(|_| false).is_none());
        menu.addition = stack("diamond", 2);
        assert!(menu.take_result(|_| true).is_none());
        assert_eq!((menu.template.1, menu.base.1, menu.addition.1), (2, 1, 2));

        menu.addition = stack("netherite_ingot", 2);
        assert_eq!(menu.take_result(|_| true), Some(preview));
        assert_eq!((menu.template.1, menu.base.1, menu.addition.1), (1, 0, 1));
        assert!(menu.take_result(|_| true).is_none());
    }

    #[test]
    fn brewing_menu_enforces_bottle_limits_and_ingredient_and_fuel_roles() {
        let mut menu = BrewingMenu::default();
        let mut cursor = stack("glass_bottle", 3);
        assert!(!menu.click_slot(-1, 0, &mut cursor));
        assert!(!menu.click_slot(5, 0, &mut cursor));
        assert!(!menu.click_slot(3, 0, &mut cursor));
        assert!(menu.click_slot(0, 0, &mut cursor));
        assert_eq!(cursor.1, 2);
        assert!(!menu.click_slot(0, 0, &mut cursor));
        assert_eq!(menu.slots[0], stack("glass_bottle", 1));

        for (slot, bottle) in [(0, "potion"), (1, "splash_potion"), (2, "lingering_potion")] {
            cursor = stack(bottle, 1);
            assert!(menu.click_slot(slot, 1, &mut cursor));
            assert_eq!(menu.slots[slot as usize], stack(bottle, 1));
        }

        cursor = stack("breeze_rod", 4);
        assert!(!menu.click_slot(4, 0, &mut cursor));
        assert!(!menu.click_slot(0, 0, &mut cursor));
        assert!(menu.click_slot(3, 1, &mut cursor));
        assert_eq!(cursor.1, 3);
        cursor = stack("blaze_powder", 2);
        assert!(menu.click_slot(3, 0, &mut cursor));
        assert_eq!(cursor, stack("breeze_rod", 1));
        cursor = stack("blaze_powder", 4);
        assert!(menu.click_slot(4, 0, &mut cursor));
        assert_eq!(menu.slots[4], stack("blaze_powder", 4));
        assert_eq!(cursor, Stack::default());
        assert!(menu.click_slot(4, 1, &mut cursor));
        assert_eq!((cursor.1, menu.slots[4].1), (2, 2));

        menu.slots[3] = stack("glass_bottle", 1);
        cursor = stack("glass_bottle", 2);
        assert!(menu.click_slot(3, 0, &mut cursor));
        assert_eq!(cursor.1, 3);
        assert_eq!(menu.slots[3], Stack::default());

        // Vanilla pulls only what fits, even on a secondary click.
        menu.slots[3] = stack("glass_bottle", 4);
        cursor.1 = (stack_max_of(&cursor) - 1) as u8;
        assert!(menu.click_slot(3, 1, &mut cursor));
        assert_eq!(i64::from(cursor.1), stack_max_of(&cursor));
        assert_eq!(menu.slots[3], stack("glass_bottle", 3));
    }

    #[test]
    fn brewing_menu_merges_equal_components_and_respects_their_stack_limit() {
        let mut menu = BrewingMenu::default();
        let mut cursor = stack("blaze_powder", 3);
        comp_set_varint(&mut cursor.2, 1, 3);
        comp_set(&mut cursor.2, COMP_CUSTOM_NAME, nbt_string("Fuel"));
        menu.slots[4] = (cursor.0, 1, cursor.2.iter().rev().cloned().collect());
        assert!(menu.click_slot(4, 0, &mut cursor));
        assert_eq!((menu.slots[4].1, cursor.1), (3, 1));
        assert!(!menu.click_slot(4, 1, &mut cursor));
    }
}

/// P2 parity tests: advancement reward application and the live gamerule
/// accessors the player damage/regen/portal/spawn paths consult.
#[cfg(test)]
mod p2_parity_player_tests {
    use super::*;

    /// Restores gamerule atomics a test changed, on every exit path.
    struct Rules(Vec<(&'static str, i32)>);
    impl Rules {
        fn take(names: &[&'static str]) -> Self {
            Rules(
                names
                    .iter()
                    .map(|name| (*name, gamerule_atomic_value(name).unwrap_or(0)))
                    .collect(),
            )
        }
    }
    impl std::ops::Drop for Rules {
        fn drop(&mut self) {
            for (name, value) in self.0.drain(..) {
                gamerule_atomic_store(name, value);
            }
        }
    }

    /// Restores stored-only gamerule map entries a test changed.
    struct StoredRules(Vec<(&'static str, i32)>);
    impl StoredRules {
        fn take(names: &[&'static str]) -> Self {
            let map = command_gamerules().lock().unwrap_or_else(|e| e.into_inner());
            StoredRules(
                names
                    .iter()
                    .map(|name| (*name, map.get(*name).copied().unwrap_or(0)))
                    .collect(),
            )
        }
        fn set(&self, name: &'static str, value: i32) {
            command_gamerules().lock().unwrap_or_else(|e| e.into_inner()).insert(name, value);
        }
    }
    impl std::ops::Drop for StoredRules {
        fn drop(&mut self) {
            let mut map = command_gamerules().lock().unwrap_or_else(|e| e.into_inner());
            for (name, value) in self.0.drain(..) {
                map.insert(name, value);
            }
        }
    }

    #[test]
    fn p2_parity_environmental_damage_rules_read_live_and_gate() {
        let _rules = Rules::take(&[
            "drowningDamage",
            "fallDamage",
            "fireDamage",
            "freezeDamage",
            "naturalRegeneration",
        ]);
        for (name, enabled) in [
            ("drowningDamage", drowning_damage_enabled as fn() -> bool),
            ("fallDamage", fall_damage_enabled as fn() -> bool),
            ("fireDamage", fire_damage_enabled as fn() -> bool),
            ("freezeDamage", freeze_damage_enabled as fn() -> bool),
            ("naturalRegeneration", natural_regeneration_enabled as fn() -> bool),
        ] {
            gamerule_atomic_store(name, 0);
            assert!(!enabled(), "{name}=false must disable the arm");
            gamerule_atomic_store(name, 1);
            assert!(enabled(), "{name}=true must enable the arm");
            gamerule_atomic_store(name, 1);
        }
        // A gated arm returns no damage at all when its rule is off.
        assert_eq!(eval_environmental_damage(false, 2.0), 0.0);
        assert_eq!(eval_environmental_damage(true, 2.0), 2.0);
    }

    #[test]
    fn p2_parity_natural_regeneration_gates_food_healing() {
        let _rules = Rules::take(&["naturalRegeneration"]);
        gamerule_atomic_store("naturalRegeneration", 0);
        let mut food = FoodData {
            food_level: 20,
            saturation: 5.0,
            ..FoodData::default()
        };
        for _ in 0..40 {
            assert_eq!(
                food.tick(10.0, 20.0, HungerDifficulty::Normal, natural_regeneration_enabled()),
                FoodTick::None,
                "naturalRegeneration=false must suppress saturated healing"
            );
        }
        gamerule_atomic_store("naturalRegeneration", 1);
        let mut healed = false;
        for _ in 0..40 {
            if food.tick(10.0, 20.0, HungerDifficulty::Normal, natural_regeneration_enabled())
                != FoodTick::None
            {
                healed = true;
                break;
            }
        }
        assert!(healed, "naturalRegeneration=true must restore saturated healing");
    }

    #[test]
    fn p2_parity_immediate_respawn_and_limited_crafting_rules() {
        let _rules = Rules::take(&["doImmediateRespawn"]);
        let _stored = StoredRules::take(&["doLimitedCrafting"]);
        gamerule_atomic_store("doImmediateRespawn", 1);
        assert!(do_immediate_respawn_enabled());
        gamerule_atomic_store("doImmediateRespawn", 0);
        assert!(!do_immediate_respawn_enabled());

        _stored.set("doLimitedCrafting", 1);
        assert!(do_limited_crafting_enabled());
        assert!(!eval_limited_crafting_allowed(true, false));
        assert!(eval_limited_crafting_allowed(true, true));
        _stored.set("doLimitedCrafting", 0);
        assert!(!do_limited_crafting_enabled());
        assert!(eval_limited_crafting_allowed(false, false));
        assert!(eval_limited_crafting_allowed(false, true));
    }

    #[test]
    fn p2_parity_portal_delays_and_spawn_radius_read_live() {
        let _stored = StoredRules::take(&[
            "playersNetherPortalDefaultDelay",
            "playersNetherPortalCreativeDelay",
            "spawnRadius",
        ]);
        _stored.set("playersNetherPortalDefaultDelay", 42);
        _stored.set("playersNetherPortalCreativeDelay", 3);
        _stored.set("spawnRadius", 7);
        assert_eq!(players_nether_portal_delay_ticks(false), 42);
        assert_eq!(players_nether_portal_delay_ticks(true), 3);
        assert_eq!(spawn_radius_blocks(), 7);
        for roll in 0..1000u32 {
            let offset = eval_spawn_offset(7, roll);
            assert!((-7..=7).contains(&offset), "offset {offset} outside ±7");
        }
        assert_eq!(eval_spawn_offset(0, u32::MAX), 0);
    }

    #[test]
    fn p2_parity_announce_advancements_toggles_the_broadcast_rule() {
        let _rules = Rules::take(&["announceAdvancements"]);
        gamerule_atomic_store("announceAdvancements", 0);
        assert!(!announce_advancements_enabled());
        gamerule_atomic_store("announceAdvancements", 1);
        assert!(announce_advancements_enabled());
    }

    #[test]
    fn p2_parity_advancement_rewards_spec_converts_and_unlocks_recipes() {
        let spec = AdvancementRewardSpec {
            experience: 100,
            recipes: &["minecraft:stone_pickaxe", "minecraft:stone_axe"],
            loot: &["minecraft:entities/pig"],
            function: Some("minecraft:test/reward_fn"),
        };
        let rewards = spec.to_rewards();
        assert_eq!(
            rewards,
            AdvancementRewards {
                experience: 100,
                recipes: vec![
                    "minecraft:stone_pickaxe".to_string(),
                    "minecraft:stone_axe".to_string()
                ],
                loot: vec!["minecraft:entities/pig".to_string()],
                function: Some("minecraft:test/reward_fn".to_string()),
            }
        );

        // The recipe arm applies the same book update `/recipe give` runs.
        let mut book = PlayerRecipeBook::new();
        let universe = vec!["minecraft:stone_pickaxe".to_string()];
        let outcome = exec_recipe_command(
            &mut book,
            RecipeCommandVerb::Give,
            &universe,
            "minecraft:stone_pickaxe",
        )
        .expect("known recipe gives");
        assert!(book.is_unlocked("minecraft:stone_pickaxe"));
        assert_eq!(outcome.changed, vec!["minecraft:stone_pickaxe".to_string()]);
        assert_eq!(outcome.packets.len(), 1);
        // Unknown ids in the table are skipped exactly like a filtered /recipe.
        assert!(
            exec_recipe_command(
                &mut book,
                RecipeCommandVerb::Give,
                &universe,
                "minecraft:stone_axe",
            )
            .is_none()
        );

        // The XP arm adds points through the vanilla curve.
        let mut xp = PlayerXpState {
            level: 0,
            progress: 0.0,
            total_experience: 0,
        };
        add_player_xp_points(&mut xp, rewards.experience).unwrap();
        assert_eq!(xp.total_experience, 100);
        assert!(xp.level > 0 || xp.progress > 0.0);
    }
}

#[cfg(test)]
mod p5e_parity_mining_tests {
    use super::*;

    fn ctx<'a>(item: &'a str, block: &'a str, preferred_tool: i8) -> MiningSpeedContext<'a> {
        MiningSpeedContext {
            held_item: item,
            block,
            preferred_tool,
            efficiency_level: 0,
            haste_amplifier: None,
            conduit_power_amplifier: None,
            mining_fatigue_amplifier: None,
            submerged_in_water: false,
            has_aqua_affinity: false,
            on_ground: true,
        }
    }

    #[test]
    fn p5e_parity_tool_speed_matches_tier_when_the_tool_is_preferred() {
        // mineable/<kind>: shovel=1, pickaxe=2, axe=3, hoe=4, none=-1.
        for (tier, speed) in [
            ("wooden", 2.0_f32),
            ("stone", 4.0),
            ("iron", 6.0),
            ("diamond", 8.0),
            ("netherite", 9.0),
            ("golden", 12.0),
        ] {
            assert_eq!(
                mining_tool_speed(&format!("{tier}_pickaxe"), "stone", 2),
                speed,
                "{tier}_pickaxe on mineable/pickaxe"
            );
            // A pickaxe against a shovel block mines at bare-hand speed.
            assert_eq!(mining_tool_speed(&format!("{tier}_pickaxe"), "dirt", 1), 1.0);
            // Swords never get the tier speed.
            assert_eq!(mining_tool_speed(&format!("{tier}_sword"), "stone", 2), 1.0);
        }
        // Bare hand and non-tools are always 1.
        assert_eq!(mining_tool_speed("", "stone", 2), 1.0);
        assert_eq!(mining_tool_speed("apple", "stone", 2), 1.0);
        // A matching item on a block with no preferred tool is still 1.
        assert_eq!(mining_tool_speed("diamond_pickaxe", "glass", -1), 1.0);
        // The registry-side resolver answers the same encoding or -1.
        assert!((-1..=4).contains(&preferred_mining_tool(1)));
    }

    #[test]
    fn p5e_parity_shears_and_sword_have_their_component_speeds() {
        // Shears: cobweb/leaves 15, wool 5, vine & glow lichen 2, else 1.
        assert_eq!(mining_tool_speed("shears", "cobweb", -1), 15.0);
        assert_eq!(mining_tool_speed("shears", "oak_leaves", 4), 15.0);
        assert_eq!(mining_tool_speed("shears", "red_wool", -1), 5.0);
        assert_eq!(mining_tool_speed("shears", "vine", 3), 2.0);
        assert_eq!(mining_tool_speed("shears", "glow_lichen", 3), 2.0);
        assert_eq!(mining_tool_speed("shears", "stone", 2), 1.0);
        // Sword: cobweb 15, instantly-mined bamboo maximal, sword_efficient 1.5.
        assert_eq!(mining_tool_speed("diamond_sword", "cobweb", -1), 15.0);
        assert!(mining_tool_speed("wooden_sword", "bamboo", 3) > 1.0e30);
        assert!(mining_tool_speed("netherite_sword", "bamboo_sapling", -1) > 1.0e30);
        assert_eq!(mining_tool_speed("iron_sword", "oak_leaves", 4), 1.5);
        assert_eq!(mining_tool_speed("iron_sword", "pumpkin", 3), 1.5);
        assert_eq!(mining_tool_speed("iron_sword", "chorus_plant", 3), 1.5);
        assert_eq!(mining_tool_speed("iron_sword", "stone", 2), 1.0);
    }

    #[test]
    fn p5e_parity_efficiency_only_applies_above_speed_one() {
        // Diamond pickaxe + Efficiency V: 8 + 26 = 34.
        let mut c = ctx("diamond_pickaxe", "stone", 2);
        c.efficiency_level = 5;
        assert_eq!(vanilla_destroy_speed(&c), 34.0);
        // Efficiency never applies to a bare hand or a wrong tool (speed 1).
        let mut wrong = ctx("diamond_pickaxe", "dirt", 1);
        wrong.efficiency_level = 5;
        assert_eq!(vanilla_destroy_speed(&wrong), 1.0);
        let mut hand = ctx("", "stone", 2);
        hand.efficiency_level = 5;
        assert_eq!(vanilla_destroy_speed(&hand), 1.0);
        // But it does apply to a 1.5 sword-efficient match.
        let mut sword = ctx("iron_sword", "oak_leaves", 4);
        sword.efficiency_level = 2;
        assert_eq!(vanilla_destroy_speed(&sword), 1.5 + 5.0);
    }

    #[test]
    fn p5e_parity_haste_conduit_and_the_fatigue_switch_table() {
        // Diamond pickaxe + Efficiency V + Haste II: 34 * 1.4 = 47.6.
        let mut c = ctx("diamond_pickaxe", "stone", 2);
        c.efficiency_level = 5;
        c.haste_amplifier = Some(1);
        assert!((vanilla_destroy_speed(&c) - 47.6).abs() < 0.001);
        // Conduit Power I (amp 0) alone: 34 * 1.2 = 40.8.
        c.haste_amplifier = None;
        c.conduit_power_amplifier = Some(0);
        assert!((vanilla_destroy_speed(&c) - 40.8).abs() < 0.001);
        // The stronger of the two wins: Haste 0 vs Conduit 2 -> x1.6.
        c.haste_amplifier = Some(0);
        c.conduit_power_amplifier = Some(2);
        assert!((vanilla_destroy_speed(&c) - 54.4).abs() < 0.001);
        // Mining Fatigue I-IV is the hardcoded LivingEntity switch table.
        for (amplifier, factor) in [(0, 0.3), (1, 0.09), (2, 0.0027), (3, 0.00081)] {
            let mut tired = ctx("diamond_pickaxe", "stone", 2);
            tired.efficiency_level = 5;
            tired.mining_fatigue_amplifier = Some(amplifier);
            assert_eq!(
                vanilla_destroy_speed(&tired),
                34.0 * factor as f32,
                "fatigue amplifier {amplifier}"
            );
        }
        // Amplifier above III clamps to the last switch arm, like vanilla's default.
        let mut over = ctx("diamond_pickaxe", "stone", 2);
        over.efficiency_level = 5;
        over.mining_fatigue_amplifier = Some(7);
        assert!((vanilla_destroy_speed(&over) - 34.0 * 0.00081).abs() < 1e-6);
    }

    #[test]
    fn p5e_parity_water_and_airborne_penalties() {
        let base = ctx("diamond_pickaxe", "stone", 2);
        assert_eq!(vanilla_destroy_speed(&base), 8.0);
        // Underwater without Aqua Affinity: x0.2.
        let mut wet = base;
        wet.submerged_in_water = true;
        assert_eq!(vanilla_destroy_speed(&wet), 1.6);
        // Aqua Affinity cancels the water penalty completely.
        wet.has_aqua_affinity = true;
        assert_eq!(vanilla_destroy_speed(&wet), 8.0);
        // Airborne (jumping / flying / falling): x0.2 even on dry land.
        let mut air = base;
        air.on_ground = false;
        assert_eq!(vanilla_destroy_speed(&air), 1.6);
        // Wet and airborne stack to x0.04, in vanilla order.
        air.submerged_in_water = true;
        assert_eq!(vanilla_destroy_speed(&air), 0.32);
        air.has_aqua_affinity = true;
        assert_eq!(vanilla_destroy_speed(&air), 1.6);
    }

    #[test]
    fn p5e_parity_mining_ticks_harvest_and_instant_thresholds() {
        // Stone (1.5) with a correct tool at diamond speed (8): 8/1.5/30 -> 6 ticks.
        assert_eq!(vanilla_mining_ticks(1.5, true, 8.0), 6);
        // The same tool without drops pays the /100 rate: 8/1.5/100 -> 19 ticks.
        assert_eq!(vanilla_mining_ticks(1.5, false, 8.0), 19);
        // Zero-hardness blocks break instantly, as does any fast enough tool.
        assert_eq!(vanilla_mining_ticks(0.0, true, 1.0), 0);
        assert_eq!(vanilla_mining_ticks(0.0, false, 1.0), 0);
        assert_eq!(vanilla_mining_ticks(1.0, true, 30.0), 0);
        // Just under the threshold needs a second tick of progress.
        assert_eq!(vanilla_mining_ticks(1.0, true, 29.9), 2);
        // Bedrock (-1) is unbreakable; so is zero effective speed.
        assert_eq!(vanilla_mining_ticks(-1.0, true, 34.0), u32::MAX);
        assert_eq!(vanilla_mining_ticks(50.0, true, 0.0), u32::MAX);
        // Obsidian with a diamond pickaxe: 8/50/30 -> ceil(187.5) = 188 ticks.
        assert_eq!(vanilla_mining_ticks(50.0, true, 8.0), 188);
        // Sword instantly mines bamboo through the tool rule.
        let bamboo = ctx("wooden_sword", "bamboo", 3);
        assert_eq!(vanilla_mining_ticks(1.0, false, vanilla_destroy_speed(&bamboo)), 0);
    }

    #[test]
    fn p5e_parity_legacy_modifier_path_matches_the_fatigue_table() {
        // The pre-existing helper had to learn the same switch table; a
        // fatigue-II run must produce 0.0027, not the old 0.3^3 = 0.027.
        assert_eq!(
            mining_speed_with_modifiers(6.0, false, 0, None, Some(2), false, true),
            6.0 * 0.0027
        );
        assert_eq!(
            mining_speed_with_modifiers(6.0, false, 0, None, Some(3), false, true),
            6.0 * 0.00081
        );
    }
}
