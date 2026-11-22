// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Behavior-pinning tests for `dye_and_items`: every expectation is a literal
//! extracted from the pinned vanilla 1.21.1 jar (sha1
//! `59353fb40c36d304f2035d51e7d6e6baa98dc05c`) — `DyeColor.<clinit>` RGB
//! triples, `DyedItemColor.applyDyes` outputs, `Foods`/`ComposterBlock` tables,
//! `AxeItem.STRIPPABLES`/`HoeItem.TILLABLES`/`ShovelItem.FLATTENABLES`,
//! `HoneycombItem.WAXABLES`, `WeatheringCopper.NEXT_BY_BLOCK` and the datapack
//! `*_food` item tags.

use std::collections::BTreeSet;

use megu_core::dye_and_items::*;

// ---------------------------------------------------------------------------
// DyeColor
// ---------------------------------------------------------------------------

/// The complete `DyeColor.<clinit>` triple table — (textureDiffuse, firework,
/// text) for all 16 colors in registry order.
const VANILLA_DYE_TABLE: [(&str, u32, u32, u32); 16] = [
    ("white", 0xF9FFFE, 0xF0F0F0, 0xFFFFFF),
    ("orange", 0xF9801D, 0xEB8844, 0xFF681F),
    ("magenta", 0xC74EBD, 0xC354CD, 0xFF00FF),
    ("light_blue", 0x3AB3DA, 0x6689D3, 0x9AC0CD),
    ("yellow", 0xFED83D, 0xDECF2A, 0xFFFF00),
    ("lime", 0x80C71F, 0x41CD34, 0xBFFF00),
    ("pink", 0xF38BAA, 0xD88198, 0xFF69B4),
    ("gray", 0x474F52, 0x434343, 0x808080),
    ("light_gray", 0x9D9D97, 0xABABAB, 0xD3D3D3),
    ("cyan", 0x169C9C, 0x287697, 0x00FFFF),
    ("purple", 0x8932B8, 0x7B2FBE, 0xA020F0),
    ("blue", 0x3C44AA, 0x253192, 0x0000FF),
    ("brown", 0x835432, 0x51301A, 0x8B4513),
    ("green", 0x5E7C16, 0x3B511A, 0x00FF00),
    ("red", 0xB02E26, 0xB3312C, 0xFF0000),
    ("black", 0x1D1D21, 0x1E1B1B, 0x000000),
];

#[test]
fn dye_color_table_matches_vanilla_clinit() {
    assert_eq!(DyeColor::ALL.len(), 16);
    for (i, (name, diffuse, firework, text)) in VANILLA_DYE_TABLE.iter().enumerate() {
        let c = DyeColor::by_id(i as u8).unwrap();
        assert_eq!(c.name(), *name);
        assert_eq!(c.id() as usize, i);
        assert_eq!(c.texture_diffuse_rgb(), *diffuse, "{name} diffuse");
        assert_eq!(c.texture_diffuse_argb(), 0xFF00_0000 | diffuse);
        assert_eq!(c.firework_rgb(), *firework, "{name} firework");
        assert_eq!(c.text_rgb(), *text, "{name} text");
        assert_eq!(c.item_id(), format!("minecraft:{name}_dye"));
        assert_eq!(DyeColor::from_item(c.item_id()), Some(c));
        assert_eq!(DyeColor::from_item(&format!("{name}_dye")), Some(c));
    }
    assert!(DyeColor::by_id(16).is_none());
    // 1.21.1: cocoa beans are a BlockItem, not a dye.
    assert_eq!(DyeColor::from_item("minecraft:cocoa_beans"), None);
    assert_eq!(DyeColor::from_item("bone_meal"), None);
}

// ---------------------------------------------------------------------------
// mix_dye_color — DyedItemColor.applyDyes integer algorithm
// ---------------------------------------------------------------------------

#[test]
fn mix_dye_color_single_and_identity() {
    // One contributor reproduces itself (renormalization factor is 1).
    for c in DyeColor::ALL {
        assert_eq!(mix_dye_color(None, &[c]), c.texture_diffuse_rgb());
    }
    assert_eq!(mix_dye_color(Some(0xA06540), &[]), 0xA06540);
    assert_eq!(mix_dye_color(Some(0xFF0000), &[]), 0xFF0000);
    assert_eq!(mix_dye_color(None, &[]), 0);
}

#[test]
fn mix_dye_color_pinned_vectors() {
    use DyeColor::*;
    // Computed by the same javap-verified arithmetic; leather base is
    // DyedItemColor.LEATHER_COLOR = 0xA06540.
    const LEATHER: u32 = LEATHER_DEFAULT_RGB;
    let cases: &[(Option<u32>, &[DyeColor], u32)] = &[
        (None, &[Red], 0xB02E26),
        (None, &[White], 0xF9FFFE),
        (None, &[Red, White], 0xD79894),
        (None, &[Red, Blue], 0xAD5398),
        (None, &[White, Black], 0x8B8E90),
        (Some(LEATHER), &[White], 0xCFB5A1),
        (Some(LEATHER), &[Black], 0x604231),
        (Some(LEATHER), &[Red, Blue], 0xA85A73),
        (Some(LEATHER), &[White, Black], 0x958277),
        (Some(0xFF0000), &[White], 0xFF8080),
        (Some(LEATHER), &[Green, Green, Green], 0x7B8524),
    ];
    for (base, dyes, want) in cases {
        assert_eq!(mix_dye_color(*base, dyes), *want, "{base:?} {dyes:?}");
    }
}

#[test]
fn leather_and_wolf_armor_are_dyeable() {
    for n in [
        "leather_helmet",
        "leather_chestplate",
        "leather_leggings",
        "leather_boots",
        "leather_horse_armor",
        "wolf_armor",
    ] {
        assert!(is_dyeable_leather(n), "{n}");
        assert!(is_dyeable_leather(&format!("minecraft:{n}")));
    }
    assert!(!is_dyeable_leather("iron_helmet"));
    assert!(!is_dyeable_leather("elytra"));
}

#[test]
fn cauldron_wash_decrements() {
    assert_eq!(cauldron_wash_dyed_item(Some(0xABCDEF), 3), Some((None, 2)));
    assert_eq!(cauldron_wash_dyed_item(Some(0xABCDEF), 1), Some((None, 0)));
    assert_eq!(cauldron_wash_dyed_item(Some(0xABCDEF), 0), None);
    assert_eq!(cauldron_wash_dyed_item(None, 3), None);
}

// ---------------------------------------------------------------------------
// apply_dye_target — sign / sheep / collar / craft / firework rules
// ---------------------------------------------------------------------------

#[test]
fn dye_applicator_resolution() {
    assert_eq!(
        dye_applicator_for_item("minecraft:blue_dye"),
        Some(DyeApplicator::Dye(DyeColor::Blue))
    );
    assert_eq!(dye_applicator_for_item("ink_sac"), Some(DyeApplicator::InkSac));
    assert_eq!(
        dye_applicator_for_item("glow_ink_sac"),
        Some(DyeApplicator::GlowInkSac)
    );
    assert_eq!(
        dye_applicator_for_item("honeycomb"),
        Some(DyeApplicator::Honeycomb)
    );
    assert_eq!(dye_applicator_for_item("iron_ingot"), None);
}

#[test]
fn sign_dye_rules() {
    let sign = |has_text, glowing, waxed| DyeTarget::Sign {
        has_text,
        text_color: Some(DyeColor::Black),
        glowing,
        waxed,
    };
    // Normal path.
    assert_eq!(
        apply_dye_target(&sign(true, false, false), DyeApplicator::Dye(DyeColor::Lime)),
        Ok(DyeApplied::SignColor(DyeColor::Lime))
    );
    // Vanilla applies the color operator unconditionally — even same color.
    assert_eq!(
        apply_dye_target(&sign(true, false, false), DyeApplicator::Dye(DyeColor::Black)),
        Ok(DyeApplied::SignColor(DyeColor::Black))
    );
    // No text → every applicator fails (SignText.hasMessage gate).
    for app in [
        DyeApplicator::Dye(DyeColor::Red),
        DyeApplicator::GlowInkSac,
        DyeApplicator::InkSac,
        DyeApplicator::Honeycomb,
    ] {
        assert_eq!(
            apply_dye_target(&sign(false, false, false), app),
            Err(DyeReject::SignNoText),
            "{app:?}"
        );
    }
    // Waxed → dye/ink rejected; honeycomb reports already-applied.
    assert_eq!(
        apply_dye_target(&sign(true, false, true), DyeApplicator::Dye(DyeColor::Red)),
        Err(DyeReject::SignWaxed)
    );
    assert_eq!(
        apply_dye_target(&sign(true, false, true), DyeApplicator::InkSac),
        Err(DyeReject::SignWaxed)
    );
    assert_eq!(
        apply_dye_target(&sign(true, false, true), DyeApplicator::Honeycomb),
        Err(DyeReject::AlreadyApplied)
    );
    // Glow/normal ink toggle.
    assert_eq!(
        apply_dye_target(&sign(true, false, false), DyeApplicator::GlowInkSac),
        Ok(DyeApplied::SignGlow(true))
    );
    assert_eq!(
        apply_dye_target(&sign(true, true, false), DyeApplicator::GlowInkSac),
        Err(DyeReject::AlreadyApplied)
    );
    assert_eq!(
        apply_dye_target(&sign(true, true, false), DyeApplicator::InkSac),
        Ok(DyeApplied::SignGlow(false))
    );
    assert_eq!(
        apply_dye_target(&sign(true, false, false), DyeApplicator::InkSac),
        Err(DyeReject::AlreadyApplied)
    );
    // Wax.
    assert_eq!(
        apply_dye_target(&sign(true, false, false), DyeApplicator::Honeycomb),
        Ok(DyeApplied::SignWaxed)
    );
}

#[test]
fn sheep_and_collar_rules() {
    let sheep = DyeTarget::Sheep {
        wool_color: DyeColor::White,
        sheared: false,
    };
    assert_eq!(
        apply_dye_target(&sheep, DyeApplicator::Dye(DyeColor::Red)),
        Ok(DyeApplied::SheepColor(DyeColor::Red))
    );
    assert_eq!(
        apply_dye_target(&sheep, DyeApplicator::Dye(DyeColor::White)),
        Err(DyeReject::SameColor)
    );
    let sheared = DyeTarget::Sheep {
        wool_color: DyeColor::White,
        sheared: true,
    };
    assert_eq!(
        apply_dye_target(&sheared, DyeApplicator::Dye(DyeColor::Red)),
        Err(DyeReject::Sheared)
    );
    assert_eq!(
        apply_dye_target(&sheep, DyeApplicator::Honeycomb),
        Err(DyeReject::NotAnApplicator)
    );

    for pet in [CollaredPet::Wolf, CollaredPet::Cat] {
        let collar = DyeTarget::Collar {
            pet,
            color: DyeColor::Red,
            by_owner: true,
        };
        assert_eq!(
            apply_dye_target(&collar, DyeApplicator::Dye(DyeColor::Blue)),
            Ok(DyeApplied::CollarColor(DyeColor::Blue))
        );
        assert_eq!(
            apply_dye_target(&collar, DyeApplicator::Dye(DyeColor::Red)),
            Err(DyeReject::SameColor)
        );
        let stranger = DyeTarget::Collar {
            pet,
            color: DyeColor::Red,
            by_owner: false,
        };
        assert_eq!(
            apply_dye_target(&stranger, DyeApplicator::Dye(DyeColor::Blue)),
            Err(DyeReject::NotOwner)
        );
    }
}

#[test]
fn craft_dye_rules() {
    // Families that recolor from ANY member (incl. already colored).
    for (item, dye, want) in [
        ("white_wool", DyeColor::Red, "red_wool"),
        ("blue_wool", DyeColor::Red, "red_wool"),
        ("minecraft:gray_carpet", DyeColor::Lime, "lime_carpet"),
        ("black_bed", DyeColor::White, "white_bed"),
        ("shulker_box", DyeColor::Purple, "purple_shulker_box"),
        ("red_shulker_box", DyeColor::Purple, "purple_shulker_box"),
    ] {
        assert_eq!(
            apply_dye_target(&DyeTarget::BlockCraft { item }, DyeApplicator::Dye(dye)),
            Ok(DyeApplied::CraftRecolor(want)),
            "{item} + {dye:?}"
        );
    }
    // Initial-color-only families: colored members cannot be re-dyed.
    for (item, dye) in [
        ("red_terracotta", DyeColor::Blue),
        ("blue_stained_glass", DyeColor::Red),
        ("green_stained_glass_pane", DyeColor::Black),
        ("white_candle", DyeColor::Red),
        ("white_concrete_powder", DyeColor::Blue),
        ("bundle", DyeColor::Red),
    ] {
        assert_eq!(
            apply_dye_target(&DyeTarget::BlockCraft { item }, DyeApplicator::Dye(dye)),
            Err(DyeReject::NotDyeable),
            "{item} re-dye must fail"
        );
    }
    // But their uncolored bases accept dye.
    assert_eq!(craft_dye_result("terracotta", DyeColor::Cyan), Some("cyan_terracotta"));
    assert_eq!(craft_dye_result("glass", DyeColor::Cyan), Some("cyan_stained_glass"));
    assert_eq!(
        craft_dye_result("glass_pane", DyeColor::Cyan),
        Some("cyan_stained_glass_pane")
    );
    assert_eq!(craft_dye_result("candle", DyeColor::Cyan), Some("cyan_candle"));
    // Non-applicators never dye crafts.
    assert_eq!(
        apply_dye_target(
            &DyeTarget::BlockCraft { item: "white_wool" },
            DyeApplicator::GlowInkSac
        ),
        Err(DyeReject::NotAnApplicator)
    );
    assert_eq!(concrete_powder_item(DyeColor::Orange), "orange_concrete_powder");
}

#[test]
fn craft_recipe_shapes_match_jar() {
    assert_eq!(craft_dye_recipe(CraftDyeKind::Wool), CraftDyeRecipe::SingleRecolor);
    assert_eq!(craft_dye_recipe(CraftDyeKind::Carpet), CraftDyeRecipe::SingleRecolor);
    assert_eq!(craft_dye_recipe(CraftDyeKind::Bed), CraftDyeRecipe::SingleRecolor);
    assert_eq!(
        craft_dye_recipe(CraftDyeKind::Candle),
        CraftDyeRecipe::SingleUncoloredOnly
    );
    assert_eq!(
        craft_dye_recipe(CraftDyeKind::ShulkerBox),
        CraftDyeRecipe::SpecialShulkerBox
    );
    assert_eq!(
        craft_dye_recipe(CraftDyeKind::Terracotta),
        CraftDyeRecipe::EightAroundInitial
    );
    assert_eq!(
        craft_dye_recipe(CraftDyeKind::StainedGlass),
        CraftDyeRecipe::EightAroundInitial
    );
    assert_eq!(
        craft_dye_recipe(CraftDyeKind::StainedGlassPane),
        CraftDyeRecipe::EightAroundInitial
    );
    assert_eq!(
        craft_dye_recipe(CraftDyeKind::ConcretePowder),
        CraftDyeRecipe::InitialConcretePowder
    );
    // Bundle dyeing only exists in 1.21.2+ — 1.21.1 has no recipe.
    assert_eq!(
        craft_dye_recipe(CraftDyeKind::Bundle),
        CraftDyeRecipe::NoRecipe1211
    );
}

#[test]
fn firework_star_dye_rules() {
    let star = |p, f| DyeTarget::FireworkStar {
        primary_dyes: p,
        fade_dyes: f,
    };
    assert_eq!(
        apply_dye_target(&star(0, 0), DyeApplicator::Dye(DyeColor::Red)),
        Ok(DyeApplied::FireworkPrimaryAdded(DyeColor::Red))
    );
    // Full primary list (8) → dyes go to fade; full both → reject.
    assert_eq!(
        apply_dye_target(&star(8, 0), DyeApplicator::Dye(DyeColor::Red)),
        Ok(DyeApplied::FireworkFadeAdded(DyeColor::Red))
    );
    assert_eq!(
        apply_dye_target(&star(8, 8), DyeApplicator::Dye(DyeColor::Red)),
        Err(DyeReject::FireworkFull)
    );
}

// ---------------------------------------------------------------------------
// Mission 2 — classification coverage vs the authoritative registry
// ---------------------------------------------------------------------------

/// Parse `V767_ITEMS` straight out of `proto/src/data_v767.rs` (read-only,
/// same source `tools/item_interaction_audit.py` audits against).
fn registry_items() -> Vec<String> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../proto/src/data_v767.rs");
    let src = std::fs::read_to_string(path).expect("data_v767.rs readable");
    let start = src.find("V767_ITEMS").expect("V767_ITEMS present");
    let tail = &src[start..];
    let end = tail.find("];").expect("array end");
    let body = &tail[..end];
    body.split('"')
        .skip(1)
        .step_by(2)
        .filter(|s| s.contains(':'))
        .map(|s| s.to_string())
        .collect()
}

#[test]
fn classification_covers_entire_registry() {
    let items = registry_items();
    assert_eq!(items.len(), 1333, "registry size drifted");
    assert_eq!(
        ITEM_USE_CLASSIFICATION.len(),
        items.len(),
        "kernel table must cover every registry item exactly once"
    );
    let kernel_names: BTreeSet<&str> = ITEM_USE_CLASSIFICATION.iter().map(|(n, _)| *n).collect();
    for item in &items {
        let bare = item.strip_prefix("minecraft:").unwrap();
        assert!(
            kernel_names.contains(bare),
            "registry item {item} missing from ITEM_USE_CLASSIFICATION"
        );
    }
    // Table is sorted for binary search and contains no duplicates.
    let mut sorted = ITEM_USE_CLASSIFICATION.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    assert_eq!(sorted.len(), kernel_names.len(), "duplicate names in table");
    assert_eq!(ITEM_USE_CLASSIFICATION, sorted.as_slice());
}

#[test]
fn classification_family_counts() {
    use ItemUseFamily::*;
    let mut counts = std::collections::BTreeMap::new();
    for (_, f) in ITEM_USE_CLASSIFICATION {
        *counts.entry(f).or_insert(0usize) += 1;
    }
    assert_eq!(counts[&WorldSpecial], 971);
    assert_eq!(counts[&PureMaterial], 127);
    assert_eq!(counts[&EntityUse], 105);
    assert_eq!(counts[&Consumable], 42);
    assert_eq!(counts[&Equipable], 40);
    assert_eq!(counts[&ToolBlockTransform], 22);
    assert_eq!(counts[&FluidContainer], 13);
    assert_eq!(counts[&RangedOrThrowable], 13);
}

#[test]
fn classification_spot_checks() {
    use ItemUseFamily::*;
    let expect = [
        ("bow", RangedOrThrowable),
        ("minecraft:bow", RangedOrThrowable),
        ("iron_axe", ToolBlockTransform),
        ("shears", ToolBlockTransform),
        ("honeycomb", ToolBlockTransform),
        ("flint_and_steel", ToolBlockTransform),
        ("fire_charge", ToolBlockTransform),
        ("water_bucket", FluidContainer),
        ("milk_bucket", FluidContainer),
        ("glass_bottle", FluidContainer),
        ("bowl", FluidContainer),
        ("golden_apple", Consumable),
        ("potion", Consumable),
        ("ominous_bottle", Consumable),
        ("ender_pearl", RangedOrThrowable),
        ("fishing_rod", RangedOrThrowable),
        ("elytra", Equipable),
        ("shield", Equipable),
        ("carved_pumpkin", Equipable),
        ("wolf_armor", Equipable),
        ("creeper_head", Equipable),
        ("creeper_spawn_egg", EntityUse),
        ("lead", EntityUse),
        ("name_tag", EntityUse),
        ("saddle", EntityUse),
        ("brush", EntityUse),
        ("red_dye", EntityUse),
        ("ink_sac", EntityUse),
        ("bone", EntityUse),
        ("carrot_on_a_stick", EntityUse),
        ("oak_log", WorldSpecial),
        ("wheat_seeds", WorldSpecial),
        ("cocoa_beans", WorldSpecial),
        ("redstone", WorldSpecial),
        ("string", WorldSpecial),
        ("bone_meal", WorldSpecial),
        ("music_disc_13", WorldSpecial),
        ("oak_boat", WorldSpecial),
        ("bamboo_raft", WorldSpecial),
        ("chest_minecart", WorldSpecial),
        ("end_crystal", WorldSpecial),
        ("goat_horn", WorldSpecial),
        ("spyglass", WorldSpecial),
        ("iron_ingot", PureMaterial),
        ("diamond_sword", PureMaterial),
        ("netherite_pickaxe", PureMaterial),
        ("mace", PureMaterial),
        ("wheat", PureMaterial), // material, not the crop block item
        ("air", PureMaterial),
        ("bundle", PureMaterial),
        ("arrow", PureMaterial),
        ("recovery_compass", PureMaterial),
        ("totem_of_undying", PureMaterial),
        ("not_a_real_item", PureMaterial), // unknown → inert
    ];
    for (item, family) in expect {
        assert_eq!(classify_item_use(item), family, "{item}");
    }
}

// ---------------------------------------------------------------------------
// tool_block_transform — STRIPPABLES / WAXABLES / NEXT_BY_BLOCK / TILLABLES /
// FLATTENABLES / shears / ignite
// ---------------------------------------------------------------------------

#[test]
fn axe_transforms_in_vanilla_order() {
    // Strip.
    let r = tool_block_transform("iron_axe", "oak_log").unwrap();
    assert_eq!(r.action, ToolAction::Strip);
    assert_eq!(r.result_block, Some("stripped_oak_log"));
    assert_eq!(tool_block_transform("diamond_axe", "minecraft:warped_hyphae").unwrap().result_block, Some("stripped_warped_hyphae"));
    assert_eq!(tool_block_transform("netherite_axe", "bamboo_block").unwrap().result_block, Some("stripped_bamboo_block"));
    // Waxed copper unwaxes FIRST (before scrape/strip).
    let r = tool_block_transform("iron_axe", "waxed_weathered_cut_copper").unwrap();
    assert_eq!(r.action, ToolAction::UnwaxCopper);
    assert_eq!(r.result_block, Some("weathered_cut_copper"));
    // Unwaxed oxidized copper scrapes back one stage.
    let r = tool_block_transform("iron_axe", "oxidized_copper_grate").unwrap();
    assert_eq!(r.action, ToolAction::ScrapeCopper);
    assert_eq!(r.result_block, Some("weathered_copper_grate"));
    // Base oxidation stage cannot scrape (it is unwaxed → strip/strip-none).
    assert!(tool_block_transform("iron_axe", "copper_block").is_none());
    assert!(tool_block_transform("iron_axe", "stone").is_none());
    // Every copper family member resolves.
    for b in ["copper_door", "copper_trapdoor", "copper_bulb", "cut_copper_slab"] {
        let w = format!("waxed_{b}");
        assert_eq!(tool_block_transform("iron_axe", &w).unwrap().action, ToolAction::UnwaxCopper, "{w}");
    }
}

#[test]
fn hoe_shovel_transforms() {
    for (block, result, air, drop) in [
        ("grass_block", "farmland", true, None),
        ("dirt", "farmland", true, None),
        ("dirt_path", "farmland", true, None),
        ("coarse_dirt", "dirt", true, None),
        ("rooted_dirt", "dirt", false, Some("hanging_roots")),
    ] {
        let r = tool_block_transform("stone_hoe", block).unwrap();
        assert_eq!(r.action, ToolAction::Till);
        assert_eq!(r.result_block, Some(result), "{block}");
        assert_eq!(r.requires_air_above, air, "{block}");
        assert_eq!(r.bonus_drop, drop, "{block}");
    }
    for block in ["grass_block", "dirt", "podzol", "coarse_dirt", "mycelium", "rooted_dirt"] {
        let r = tool_block_transform("golden_shovel", block).unwrap();
        assert_eq!(r.action, ToolAction::Flatten, "{block}");
        assert_eq!(r.result_block, Some("dirt_path"));
        assert!(r.requires_air_above);
    }
    for cf in ["campfire", "soul_campfire"] {
        let r = tool_block_transform("iron_shovel", cf).unwrap();
        assert_eq!(r.action, ToolAction::ExtinguishCampfire);
    }
    assert!(tool_block_transform("iron_shovel", "sand").is_none());
    assert!(tool_block_transform("iron_hoe", "mud").is_none());
}

#[test]
fn shears_ignite_wax_transforms() {
    let r = tool_block_transform("shears", "pumpkin").unwrap();
    assert_eq!(r.action, ToolAction::CarvePumpkin);
    assert_eq!(r.result_block, Some("carved_pumpkin"));
    assert_eq!(r.bonus_drop, Some("pumpkin_seeds"));
    for hive in ["bee_nest", "beehive"] {
        assert_eq!(tool_block_transform("shears", hive).unwrap().action, ToolAction::HarvestBeehive);
    }
    for tip in ["weeping_vines", "twisting_vines", "kelp", "cave_vines"] {
        assert_eq!(tool_block_transform("shears", tip).unwrap().action, ToolAction::StopGrowthTip, "{tip}");
    }
    assert_eq!(tool_block_transform("shears", "tripwire").unwrap().action, ToolAction::DisarmTripwire);
    assert!(tool_block_transform("shears", "oak_log").is_none());

    for tool in ["flint_and_steel", "fire_charge"] {
        let r = tool_block_transform(tool, "netherrack").unwrap();
        assert_eq!(r.action, ToolAction::Ignite);
    }

    // Honeycomb: wax sign (any *_sign) and wax copper.
    assert_eq!(tool_block_transform("honeycomb", "oak_sign").unwrap().action, ToolAction::WaxSign);
    assert_eq!(tool_block_transform("honeycomb", "warped_wall_hanging_sign").unwrap().action, ToolAction::WaxSign);
    let r = tool_block_transform("honeycomb", "exposed_cut_copper_stairs").unwrap();
    assert_eq!(r.action, ToolAction::WaxCopper);
    assert_eq!(r.result_block, Some("waxed_exposed_cut_copper_stairs"));
    assert!(tool_block_transform("honeycomb", "waxed_copper_block").is_none());
    assert!(tool_block_transform("honeycomb", "stone").is_none());
}

#[test]
fn shearable_entity_table() {
    assert_eq!(shearable_entity("sheep"), Some(("wool_of_current_color", None)));
    assert_eq!(
        shearable_entity("minecraft:mooshroom"),
        Some(("red_mushroom", Some("minecraft:cow")))
    );
    assert!(shearable_entity("snow_golem").is_some());
    assert!(shearable_entity("bogged").is_some());
    assert!(shearable_entity("cow").is_none());
}

// ---------------------------------------------------------------------------
// food_properties — the complete Foods table
// ---------------------------------------------------------------------------

#[test]
fn foods_table_pins_vanilla() {
    // (item, nutrition, saturation_modifier, always, fast, converts_to)
    type FoodRow<'a> = (&'a str, u8, f32, bool, bool, Option<&'a str>);
    let plain: &[FoodRow<'_>] = &[
        ("apple", 4, 0.3, false, false, None),
        ("baked_potato", 5, 0.6, false, false, None),
        ("beef", 3, 0.3, false, false, None),
        ("beetroot", 1, 0.6, false, false, None),
        ("beetroot_soup", 6, 0.6, false, false, Some("bowl")),
        ("bread", 5, 0.6, false, false, None),
        ("carrot", 3, 0.6, false, false, None),
        ("chicken", 2, 0.3, false, false, None),
        ("chorus_fruit", 4, 0.3, true, false, None),
        ("cod", 2, 0.1, false, false, None),
        ("cooked_beef", 8, 0.8, false, false, None),
        ("cooked_chicken", 6, 0.6, false, false, None),
        ("cooked_cod", 5, 0.6, false, false, None),
        ("cooked_mutton", 6, 0.8, false, false, None),
        ("cooked_porkchop", 8, 0.8, false, false, None),
        ("cooked_rabbit", 5, 0.6, false, false, None),
        ("cooked_salmon", 6, 0.8, false, false, None),
        ("cookie", 2, 0.1, false, false, None),
        ("dried_kelp", 1, 0.3, false, true, None),
        ("enchanted_golden_apple", 4, 1.2, true, false, None),
        ("golden_apple", 4, 1.2, true, false, None),
        ("golden_carrot", 6, 1.2, false, false, None),
        ("honey_bottle", 6, 0.1, false, false, None),
        ("melon_slice", 2, 0.3, false, false, None),
        ("mushroom_stew", 6, 0.6, false, false, Some("bowl")),
        ("mutton", 2, 0.3, false, false, None),
        ("poisonous_potato", 2, 0.3, false, false, None),
        ("porkchop", 3, 0.3, false, false, None),
        ("potato", 1, 0.3, false, false, None),
        ("pufferfish", 1, 0.1, false, false, None),
        ("pumpkin_pie", 8, 0.3, false, false, None),
        ("rabbit", 3, 0.3, false, false, None),
        ("rabbit_stew", 10, 0.6, false, false, Some("bowl")),
        ("rotten_flesh", 4, 0.1, false, false, None),
        ("salmon", 2, 0.1, false, false, None),
        ("spider_eye", 2, 0.8, false, false, None),
        ("suspicious_stew", 6, 0.6, true, false, Some("bowl")),
        ("sweet_berries", 2, 0.1, false, false, None),
        ("glow_berries", 2, 0.1, false, false, None),
        ("tropical_fish", 1, 0.1, false, false, None),
        ("ominous_bottle", 1, 0.1, false, false, None),
    ];
    assert_eq!(plain.len(), 41, "vanilla 1.21.1 Foods has 41 constants");
    for (item, n, s, always, fast, conv) in plain {
        let f = food_properties(item).unwrap_or_else(|| panic!("{item} is food"));
        assert_eq!(f.nutrition, *n, "{item}");
        assert_eq!(f.saturation_modifier, *s, "{item}");
        assert_eq!(f.can_always_eat, *always, "{item}");
        assert_eq!(f.fast_eat, *fast, "{item}");
        assert_eq!(f.using_converts_to, *conv, "{item}");
    }
    assert_eq!(food_properties("wheat"), None);
    assert_eq!(food_properties("potion"), None); // drink, not a Foods entry
    assert_eq!(food_properties("stick"), None);
}

#[test]
fn food_effects_match_vanilla() {
    let one = |item| food_properties(item).unwrap().effects.to_vec();
    let eff = |e, d, a, p| FoodEffect {
        effect: e,
        duration_ticks: d,
        amplifier: a,
        probability: p,
    };
    assert_eq!(one("chicken"), vec![eff("minecraft:hunger", 600, 0, 0.3)]);
    assert_eq!(one("rotten_flesh"), vec![eff("minecraft:hunger", 600, 0, 0.8)]);
    assert_eq!(one("poisonous_potato"), vec![eff("minecraft:poison", 100, 0, 0.6)]);
    assert_eq!(one("spider_eye"), vec![eff("minecraft:poison", 100, 0, 1.0)]);
    assert_eq!(
        one("pufferfish"),
        vec![
            eff("minecraft:poison", 1200, 1, 1.0),
            eff("minecraft:hunger", 300, 2, 1.0),
            eff("minecraft:nausea", 300, 0, 1.0),
        ]
    );
    assert_eq!(
        one("golden_apple"),
        vec![
            eff("minecraft:regeneration", 100, 1, 1.0),
            eff("minecraft:absorption", 2400, 0, 1.0),
        ]
    );
    assert_eq!(
        one("enchanted_golden_apple"),
        vec![
            eff("minecraft:regeneration", 400, 1, 1.0),
            eff("minecraft:resistance", 6000, 0, 1.0),
            eff("minecraft:fire_resistance", 6000, 0, 1.0),
            eff("minecraft:absorption", 2400, 3, 1.0),
        ]
    );
    assert!(one("apple").is_empty());
}

#[test]
fn item_remainders() {
    for (item, rem) in [
        ("honey_bottle", "glass_bottle"),
        ("potion", "glass_bottle"),
        ("ominous_bottle", "glass_bottle"),
        ("milk_bucket", "bucket"),
        ("water_bucket", "bucket"),
        ("axolotl_bucket", "bucket"),
        ("mushroom_stew", "bowl"),
        ("suspicious_stew", "bowl"),
        ("beetroot_soup", "bowl"),
        ("rabbit_stew", "bowl"),
    ] {
        assert_eq!(item_use_remainder(item), Some(rem), "{item}");
    }
    assert_eq!(item_use_remainder("apple"), None);
}

// ---------------------------------------------------------------------------
// composter_chance — COMPOSTABLES bootstrap table
// ---------------------------------------------------------------------------

#[test]
fn composter_table() {
    // 101 entries across five tiers: 30/50/65/85/100 — `cake` and
    // `pumpkin_pie` are the only 1.0 compostables in 1.21.1.
    let mut total = 0usize;
    for probe in [
        "oak_leaves", "wheat_seeds", "dried_kelp", "pink_petals", "seagrass",
        "cactus", "sugar_cane", "tall_grass", "vine", "glow_lichen",
        "apple", "wheat", "pumpkin", "melon", "nether_wart", "lily_pad",
        "sea_pickle", "moss_block", "shroomlight", "wither_rose", "torchflower",
        "cake", "pumpkin_pie", "bread", "cookie", "hay_block",
        "nether_wart_block", "warped_wart_block", "pitcher_plant",
        "flowering_azalea", "baked_potato", "brown_mushroom_block",
        "red_mushroom_block",
    ] {
        if composter_chance(probe).is_some() {
            total += 1;
        }
    }
    assert!(total > 30);
    for (item, want) in [
        ("oak_sapling", 30), ("wheat_seeds", 30), ("dried_kelp", 30),
        ("tall_grass", 50), ("sugar_cane", 50), ("vine", 50),
        ("wheat", 65), ("pumpkin", 65), ("carrot", 65), ("potato", 65),
        ("cake", 100), ("pumpkin_pie", 100), ("hay_block", 85),
        ("nether_wart_block", 85), ("bread", 85), ("cookie", 85),
        ("torchflower", 85), ("pitcher_plant", 85), ("flowering_azalea", 85),
        ("brown_mushroom_block", 85), ("red_mushroom_block", 85),
        ("baked_potato", 85), ("warped_wart_block", 85),
    ] {
        assert_eq!(composter_chance(item), Some(want), "{item}");
    }
    assert_eq!(composter_chance("stone"), None);
    assert_eq!(composter_chance("iron_ingot"), None);
}

// ---------------------------------------------------------------------------
// EntityUse lookups — breed/tame foods, spawn eggs
// ---------------------------------------------------------------------------

#[test]
fn breed_foods_match_vanilla_tags() {
    let foods = |e: &str| breed_foods_for(e).iter().copied().collect::<BTreeSet<_>>();
    assert_eq!(foods("cow"), ["minecraft:wheat"].into());
    assert_eq!(foods("sheep"), ["minecraft:wheat"].into());
    assert_eq!(foods("goat"), ["minecraft:wheat"].into());
    assert_eq!(foods("mooshroom"), ["minecraft:wheat"].into());
    assert_eq!(
        foods("pig"),
        ["minecraft:carrot", "minecraft:potato", "minecraft:beetroot"].into()
    );
    assert_eq!(foods("chicken").len(), 6);
    assert!(foods("chicken").contains("minecraft:pitcher_pod"));
    assert_eq!(
        foods("rabbit"),
        ["minecraft:carrot", "minecraft:golden_carrot", "minecraft:dandelion"].into()
    );
    assert_eq!(
        foods("horse"),
        ["minecraft:golden_apple", "minecraft:golden_carrot"].into()
    );
    assert_eq!(foods("llama"), ["minecraft:hay_block"].into());
    assert_eq!(foods("wolf").len(), 11);
    assert!(foods("wolf").contains("minecraft:rotten_flesh")); // #meat expanded
    assert_eq!(
        foods("cat"),
        ["minecraft:cod", "minecraft:salmon"].into()
    );
    assert_eq!(foods("ocelot"), foods("cat"));
    assert_eq!(
        foods("fox"),
        ["minecraft:sweet_berries", "minecraft:glow_berries"].into()
    );
    assert_eq!(foods("panda"), ["minecraft:bamboo"].into());
    assert_eq!(foods("turtle"), ["minecraft:seagrass"].into());
    assert_eq!(foods("axolotl"), ["minecraft:tropical_fish_bucket"].into());
    assert_eq!(foods("frog"), ["minecraft:slime_ball"].into());
    assert_eq!(foods("hoglin"), ["minecraft:crimson_fungus"].into());
    assert_eq!(foods("strider"), ["minecraft:warped_fungus"].into());
    assert_eq!(foods("armadillo"), ["minecraft:spider_eye"].into());
    assert_eq!(foods("sniffer"), ["minecraft:torchflower_seeds"].into());
    assert_eq!(foods("camel"), ["minecraft:cactus"].into());
    assert_eq!(
        foods("piglin"),
        ["minecraft:porkchop", "minecraft:cooked_porkchop"].into()
    );
    assert!(foods("bee").len() >= 26); // #flowers expanded
    assert!(foods("bee").contains("minecraft:torchflower"));
    assert!(foods("zombie").is_empty());
    assert!(foods("villager").is_empty());
    // namespace-prefixed input works
    assert_eq!(foods("minecraft:cow"), foods("cow"));
}

#[test]
fn tame_and_feed_tables() {
    assert_eq!(tame_foods_for("wolf"), &["minecraft:bone"]);
    assert_eq!(
        tame_foods_for("cat"),
        &["minecraft:cod", "minecraft:salmon"]
    );
    assert_eq!(tame_foods_for("parrot").len(), 6);
    assert!(tame_foods_for("cow").is_empty());
    assert_eq!(parrot_poisonous_foods(), &["minecraft:cookie"]);
    assert_eq!(horse_feed_items().len(), 7);
    assert!(horse_feed_items().contains(&"minecraft:enchanted_golden_apple"));
    assert_eq!(llama_feed_items(), &["minecraft:wheat", "minecraft:hay_block"]);
    assert_eq!(
        strider_tempt_items(),
        &["minecraft:warped_fungus", "minecraft:warped_fungus_on_a_stick"]
    );
}

#[test]
fn spawn_egg_lookup() {
    assert_eq!(spawn_egg_entity("creeper_spawn_egg"), Some("minecraft:creeper"));
    assert_eq!(
        spawn_egg_entity("minecraft:armadillo_spawn_egg"),
        Some("minecraft:armadillo")
    );
    assert_eq!(spawn_egg_entity("trader_llama_spawn_egg"), Some("minecraft:trader_llama"));
    assert_eq!(spawn_egg_entity("iron_ingot"), None);
    // All 80 eggs resolve to a registered entity name.
    let entities: BTreeSet<String> = {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../proto/src/data_v767.rs"
        ))
        .unwrap();
        let start = src.find("V767_ENTITY_TYPES").unwrap();
        let body = &src[start..src[start..].find("];").unwrap() + start];
        body.split('"')
            .skip(1)
            .step_by(2)
            .filter(|s| s.contains(':'))
            .map(|s| s.to_string())
            .collect()
    };
    let mut n = 0;
    for (name, _) in ITEM_USE_CLASSIFICATION {
        if let Some(ent) = spawn_egg_entity(name) {
            n += 1;
            assert!(entities.contains(ent), "{ent} not in entity registry");
        }
    }
    assert_eq!(n, 80);
}

// ---------------------------------------------------------------------------
// equip_slot / is_music_disc / bucket_content
// ---------------------------------------------------------------------------

#[test]
fn equip_slots() {
    use EquipSlot::*;
    let cases = [
        ("iron_helmet", Some(Head)),
        ("turtle_helmet", Some(Head)),
        ("carved_pumpkin", Some(Head)),
        ("wither_skeleton_skull", Some(Head)),
        ("player_head", Some(Head)),
        ("diamond_chestplate", Some(Chest)),
        ("elytra", Some(Chest)),
        ("chainmail_leggings", Some(Legs)),
        ("netherite_boots", Some(Feet)),
        ("shield", Some(Offhand)),
        ("leather_horse_armor", Some(Body)),
        ("diamond_horse_armor", Some(Body)),
        ("wolf_armor", Some(Body)),
        ("saddle", Some(Body)),
        ("iron_sword", None),
        ("stick", None),
    ];
    for (item, slot) in cases {
        assert_eq!(equip_slot(item), slot, "{item}");
    }
}

#[test]
fn music_discs_and_buckets() {
    assert!(is_music_disc("music_disc_13"));
    assert!(is_music_disc("minecraft:music_disc_creator_music_box"));
    assert!(!is_music_disc("disc_fragment_5"));
    assert_eq!(bucket_content("bucket"), Some(BucketContent::Empty));
    assert_eq!(bucket_content("water_bucket"), Some(BucketContent::Water));
    assert_eq!(bucket_content("lava_bucket"), Some(BucketContent::Lava));
    assert_eq!(bucket_content("powder_snow_bucket"), Some(BucketContent::PowderSnow));
    assert_eq!(bucket_content("milk_bucket"), Some(BucketContent::Milk));
    assert_eq!(
        bucket_content("axolotl_bucket"),
        Some(BucketContent::Mob("minecraft:axolotl"))
    );
    assert_eq!(
        bucket_content("tadpole_bucket"),
        Some(BucketContent::Mob("minecraft:tadpole"))
    );
    assert_eq!(bucket_content("glass_bottle"), None);
}
