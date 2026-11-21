// Lane P5e (#217): version-neutral item facts keyed by registry name.
// Included from player_mechanisms.rs into the crate root (same flat scope - no
// `mod`), so every name it uses from the surrounding files resolves here.
//
// Numeric item ids shift between the 1.21.1 and 26.3 registries, so facts like
// max durability must be keyed by name: `max_durability_for_item_id` resolves
// the id to a name through the loaded registry table, then looks the name up.
// An id this table does not know - or a registry that was never loaded -
// answers None instead of a wrong durability.

/// Registry item id -> item name, through the loaded registry table.
#[cfg_attr(not(test), allow(dead_code))]
fn item_name(id: i32) -> Option<&'static str> {
    item_to_name(i64::from(id))
}

/// Item name -> registry id; an optional `minecraft:` prefix is stripped the
/// same way command arguments are.
#[cfg_attr(not(test), allow(dead_code))]
fn item_id(name: &str) -> Option<i32> {
    name_to_item(name).and_then(|id| i32::try_from(id).ok())
}

/// `minecraft:max_damage` for every damageable 1.21.1 item, keyed by name.
///
/// Tool tier values come from `Tier.getUses` / the item's `max_damage`
/// component: wood 59, stone 131, iron 250, gold 32, diamond 1561, netherite
/// 2031. Armor durability is `ArmorMaterial.durability` (a per-material
/// multiplier) times `ArmorItem.Type`'s slot base - helmet 11, chestplate 16,
/// leggings 15, boots 13: leather x5, chainmail x15, iron x15, gold x7,
/// diamond x33, netherite x37, turtle shell 275 flat. The remaining
/// damageable items are listed outright.
#[cfg_attr(not(test), allow(dead_code))]
fn max_durability_by_name(name: &str) -> Option<u16> {
    let name = name.strip_prefix("minecraft:").unwrap_or(name);
    if let Some((tier, _)) = tool_parts(name) {
        return Some(match tier {
            "wooden" => 59,
            "stone" => 131,
            "iron" => 250,
            "golden" => 32,
            "diamond" => 1_561,
            "netherite" => 2_031,
            _ => return None,
        });
    }
    if let Some((material, piece)) = armor_parts(name) {
        let multiplier = match material {
            "leather" => 5,
            "chainmail" | "iron" => 15,
            "golden" => 7,
            "diamond" => 33,
            "netherite" => 37,
            "turtle" => return Some(275),
            _ => return None,
        };
        let base = match piece {
            "helmet" => 11,
            "chestplate" => 16,
            "leggings" => 15,
            "boots" => 13,
            _ => return None,
        };
        return Some(multiplier * base);
    }
    Some(match name {
        "shears" => 238,
        "flint_and_steel" => 64,
        "fishing_rod" => 64,
        "carrot_on_a_stick" => 25,
        "warped_fungus_on_a_stick" => 100,
        "bow" => 384,
        "crossbow" => 465,
        "trident" => 250,
        "mace" => 500,
        "shield" => 336,
        "brush" => 64,
        "elytra" => 432,
        "wolf_armor" => 64,
        _ => return None,
    })
}

/// Durability by numeric registry id. Resolving through the name is the whole
/// point: the same item keeps its durability when a registry rev shifts ids.
#[cfg_attr(not(test), allow(dead_code))]
fn max_durability_for_item_id(id: i32) -> Option<u16> {
    item_name(id).and_then(max_durability_by_name)
}

#[cfg(test)]
mod p5e_parity_item_tests {
    use super::*;

    #[test]
    fn p5e_parity_durability_covers_every_1211_tool_tier() {
        for (tier, durability) in [
            ("wooden", 59),
            ("stone", 131),
            ("iron", 250),
            ("golden", 32),
            ("diamond", 1_561),
            ("netherite", 2_031),
        ] {
            for kind in ["sword", "pickaxe", "axe", "shovel", "hoe"] {
                assert_eq!(
                    max_durability_by_name(&format!("{tier}_{kind}")),
                    Some(durability),
                    "{tier}_{kind}"
                );
            }
        }
        // The `minecraft:` prefix must not change the answer.
        assert_eq!(max_durability_by_name("minecraft:iron_hoe"), Some(250));
    }

    #[test]
    fn p5e_parity_durability_covers_every_1211_armor_set() {
        for (material, multi) in [
            ("leather", 5u16),
            ("chainmail", 15),
            ("iron", 15),
            ("golden", 7),
            ("diamond", 33),
            ("netherite", 37),
        ] {
            for (piece, base) in [
                ("helmet", 11u16),
                ("chestplate", 16),
                ("leggings", 15),
                ("boots", 13),
            ] {
                assert_eq!(
                    max_durability_by_name(&format!("{material}_{piece}")),
                    Some(multi * base),
                    "{material}_{piece}"
                );
            }
        }
        assert_eq!(max_durability_by_name("turtle_helmet"), Some(275));
    }

    #[test]
    fn p5e_parity_durability_covers_named_1211_damageable_items() {
        for (name, durability) in [
            ("shears", 238),
            ("flint_and_steel", 64),
            ("fishing_rod", 64),
            ("carrot_on_a_stick", 25),
            ("warped_fungus_on_a_stick", 100),
            ("bow", 384),
            ("crossbow", 465),
            ("trident", 250),
            ("mace", 500),
            ("shield", 336),
            ("brush", 64),
            ("elytra", 432),
            ("wolf_armor", 64),
        ] {
            assert_eq!(max_durability_by_name(name), Some(durability), "{name}");
        }
        // Undamageable items and non-items have no durability at all.
        assert_eq!(max_durability_by_name("stone"), None);
        assert_eq!(max_durability_by_name("air"), None);
        assert_eq!(max_durability_by_name("definitely_not_an_item"), None);
    }

    #[test]
    fn p5e_parity_item_id_lookup_resolves_through_the_registry_table() {
        // `mace` is a 1.21 item added late in the table; `stone` is id 1.
        assert_eq!(item_id("stone"), Some(1));
        assert_eq!(item_id("minecraft:mace"), Some(1093));
        assert_eq!(item_name(1), Some("stone"));
        assert_eq!(item_name(item_id("mace").unwrap()), Some("mace"));
        assert_eq!(item_name(-1), None);
        assert_eq!(item_name(i32::MAX), None);
    }

    #[test]
    fn p5e_parity_durability_by_id_matches_the_name_lookup() {
        for name in [
            "diamond_pickaxe",
            "golden_chestplate",
            "netherite_boots",
            "crossbow",
            "mace",
            "wolf_armor",
            "brush",
            "elytra",
        ] {
            let id = item_id(name).expect(name);
            assert_eq!(
                max_durability_for_item_id(id),
                max_durability_by_name(name),
                "{name}"
            );
        }
        // An id with no name, and a named but undamageable item, both fail closed.
        assert_eq!(max_durability_for_item_id(-1), None);
        assert_eq!(max_durability_for_item_id(item_id("stone").unwrap()), None);
    }
}
