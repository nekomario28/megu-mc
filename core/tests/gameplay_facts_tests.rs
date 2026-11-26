// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_core::version::TargetVersion;
use megu_core::{
    worldgen_epoch_id, DimensionBounds, GameplayFactPack, VersionFeature, WorldgenEpochSpec,
};

#[test]
fn vanilla_1_21_1_food_defaults_match_the_registry_and_item_use() {
    // Food component stats plus the hunger check, duration and container
    // from the default item's use path in the Mojang 1.21.1 server JAR.
    let expected = [
        ("apple", 4, 0.3, false, 32, None),
        ("baked_potato", 5, 0.6, false, 32, None),
        ("beef", 3, 0.3, false, 32, None),
        ("beetroot", 1, 0.6, false, 32, None),
        ("beetroot_soup", 6, 0.6, false, 32, Some("bowl")),
        ("bread", 5, 0.6, false, 32, None),
        ("carrot", 3, 0.6, false, 32, None),
        ("chicken", 2, 0.3, false, 32, None),
        ("chorus_fruit", 4, 0.3, true, 32, None),
        ("cod", 2, 0.1, false, 32, None),
        ("cooked_beef", 8, 0.8, false, 32, None),
        ("cooked_chicken", 6, 0.6, false, 32, None),
        ("cooked_cod", 5, 0.6, false, 32, None),
        ("cooked_mutton", 6, 0.8, false, 32, None),
        ("cooked_porkchop", 8, 0.8, false, 32, None),
        ("cooked_rabbit", 5, 0.6, false, 32, None),
        ("cooked_salmon", 6, 0.8, false, 32, None),
        ("cookie", 2, 0.1, false, 32, None),
        ("dried_kelp", 1, 0.3, false, 16, None),
        ("enchanted_golden_apple", 4, 1.2, true, 32, None),
        ("glow_berries", 2, 0.1, false, 32, None),
        ("golden_apple", 4, 1.2, true, 32, None),
        ("golden_carrot", 6, 1.2, false, 32, None),
        ("honey_bottle", 6, 0.1, true, 40, Some("glass_bottle")),
        ("melon_slice", 2, 0.3, false, 32, None),
        ("mushroom_stew", 6, 0.6, false, 32, Some("bowl")),
        ("mutton", 2, 0.3, false, 32, None),
        ("ominous_bottle", 1, 0.1, true, 32, None),
        ("poisonous_potato", 2, 0.3, false, 32, None),
        ("porkchop", 3, 0.3, false, 32, None),
        ("potato", 1, 0.3, false, 32, None),
        ("pufferfish", 1, 0.1, false, 32, None),
        ("pumpkin_pie", 8, 0.3, false, 32, None),
        ("rabbit", 3, 0.3, false, 32, None),
        ("rabbit_stew", 10, 0.6, false, 32, Some("bowl")),
        ("rotten_flesh", 4, 0.1, false, 32, None),
        ("salmon", 2, 0.1, false, 32, None),
        ("spider_eye", 2, 0.8, false, 32, None),
        ("suspicious_stew", 6, 0.6, true, 32, Some("bowl")),
        ("sweet_berries", 2, 0.1, false, 32, None),
        ("tropical_fish", 1, 0.1, false, 32, None),
    ];
    let pack = GameplayFactPack::for_version(TargetVersion::V1_21_1);
    assert_eq!(expected.len(), 41);
    assert_eq!(pack.food_facts().len(), expected.len());
    for (listed, (item, nutrition, modifier, always_eat, ticks, remainder)) in
        pack.food_facts().zip(expected)
    {
        assert_eq!(listed.item_id, item);
        for name in [item.to_owned(), format!("minecraft:{item}")] {
            let fact = pack.food_by_name(&name).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(fact.item_id, item);
            assert_eq!(fact.nutrition, nutrition, "{item}");
            assert_eq!(fact.saturation_modifier, modifier, "{item}");
            assert_eq!(fact.can_always_eat, always_eat, "{item}");
            assert_eq!(fact.eat_ticks, ticks, "{item}");
            assert_eq!(fact.converts_to, remainder, "{item}");
        }
    }
    for item in [
        "cake",
        "milk_bucket",
        "potion",
        "example:cooked_beef",
        "minecraft:minecraft:cooked_beef",
        "minecraft:",
    ] {
        assert_eq!(pack.food_by_name(item), None, "{item}");
    }
}

#[test]
fn vanilla_1_21_1_furnace_ticks_match_exact_items_and_tag_families() {
    let pack = GameplayFactPack::for_version(TargetVersion::V1_21_1);
    assert_eq!(pack.fuel_facts().len(), 248);
    for (item, burn_ticks) in [
        ("lava_bucket", 20_000),
        ("coal_block", 16_000),
        ("dried_kelp_block", 4_001),
        ("blaze_rod", 2_400),
        ("coal", 1_600),
        ("charcoal", 1_600),
        ("oak_chest_boat", 1_200),
        ("bamboo_chest_raft", 1_200),
        ("cherry_hanging_sign", 800),
        ("bamboo_mosaic", 300),
        ("bamboo_mosaic_stairs", 300),
        ("stripped_bamboo_block", 300),
        ("stripped_dark_oak_wood", 300),
        ("mangrove_log", 300),
        ("mangrove_roots", 300),
        ("jungle_planks", 300),
        ("birch_fence", 300),
        ("oak_trapdoor", 300),
        ("spruce_pressure_plate", 300),
        ("black_banner", 300),
        ("crafting_table", 300),
        ("trapped_chest", 300),
        ("chiseled_bookshelf", 300),
        ("bow", 300),
        ("crossbow", 300),
        ("fishing_rod", 300),
        ("ladder", 300),
        ("wooden_sword", 200),
        ("oak_sign", 200),
        ("oak_door", 200),
        ("oak_slab", 150),
        ("bamboo_mosaic_slab", 150),
        ("oak_button", 100),
        ("oak_sapling", 100),
        ("mangrove_propagule", 100),
        ("flowering_azalea", 100),
        ("dead_bush", 100),
        ("bowl", 100),
        ("stick", 100),
        ("white_wool", 100),
        ("light_blue_carpet", 67),
        ("bamboo", 50),
        ("scaffolding", 50),
    ] {
        assert_eq!(pack.fuel_ticks_by_name(item), Some(burn_ticks), "{item}");
        let qualified = format!("minecraft:{item}");
        assert_eq!(
            pack.fuel_ticks_by_name(&qualified),
            Some(burn_ticks),
            "{qualified}"
        );
    }
    for item in [
        "stone",
        "stone_stairs",
        "iron_door",
        "crimson_planks",
        "warped_hanging_sign",
        "crimson_fence_gate",
        "stripped_warped_hyphae",
        "bamboo_log",
        "bamboo_chest_boat",
        "mangrove_sapling",
        "oak_leaves",
        "oak_shelf",
        "pale_oak_planks",
        "wooden_spear",
        "example:coal",
        "minecraft:minecraft:coal",
        "minecraft:",
    ] {
        assert_eq!(pack.fuel_ticks_by_name(item), None, "{item}");
    }
}

#[test]
fn newer_target_inherits_food_and_fuel_with_verified_deltas() {
    let baseline = GameplayFactPack::for_version(TargetVersion::V1_21_1);
    let newer = GameplayFactPack::for_version(TargetVersion::V26_3);
    assert_eq!(newer.food_facts().len(), 40);
    for food in baseline.food_facts() {
        let next = newer.food_by_name(food.item_id);
        if food.item_id == "ominous_bottle" {
            assert_eq!(next, None);
        } else {
            assert!(std::ptr::eq(next.unwrap(), food), "{}", food.item_id);
        }
    }
    assert_eq!(newer.food_by_name("minecraft:cod_bucket"), None);

    assert_eq!(newer.fuel_facts().len(), 347);
    for fuel in baseline.fuel_facts() {
        assert_eq!(
            newer.fuel_ticks_by_name(&fuel.item_id),
            Some(fuel.burn_ticks)
        );
    }
    for (item, ticks) in [
        ("pale_oak_door", 200),
        ("minecraft:poplar_chest_boat", 1_200),
        ("bamboo_shelf", 300),
        ("minecraft:leaf_litter", 100),
        ("minecraft:white_cushion", 50),
        ("black_wool_slab", 50),
        ("light_gray_wool_stairs", 100),
        ("wooden_spear", 200),
    ] {
        assert_eq!(newer.fuel_ticks_by_name(item), Some(ticks), "{item}");
        assert_eq!(baseline.fuel_ticks_by_name(item), None, "{item}");
    }
    assert_eq!(newer.fuel_ticks_by_name("warped_planks"), None);
}

#[test]
fn both_epochs_pin_vanilla_build_bounds_version_metadata_and_features() {
    for (version, id, data_version, protocol_version, pack_format, new_features) in [
        (
            TargetVersion::V1_21_1,
            "mc-1.21.1-v1",
            3955,
            767,
            (48, 0),
            false,
        ),
        (
            TargetVersion::V26_3,
            "mc-26.3-v1",
            5023,
            777,
            (121, 0),
            true,
        ),
    ] {
        let pack = GameplayFactPack::for_version(version);
        let expected = WorldgenEpochSpec {
            id,
            overworld: DimensionBounds {
                min_y: -64,
                height: 384,
            },
            nether: DimensionBounds {
                min_y: 0,
                height: 256,
            },
            end: DimensionBounds {
                min_y: 0,
                height: 256,
            },
            data_version,
            protocol_version,
            pack_format,
        };
        assert_eq!(worldgen_epoch_id(version), id);
        assert_eq!(*pack.worldgen_epoch(), expected);
        for feature in [
            VersionFeature::DataComponents,
            VersionFeature::TrialChambers,
            VersionFeature::TallDimensionSupport,
        ] {
            assert!(pack.supports_feature(feature));
        }
        for feature in [
            VersionFeature::BundleDyeing,
            VersionFeature::SeparateConsumableComponent,
            VersionFeature::CookingFuelComponent,
        ] {
            assert_eq!(pack.supports_feature(feature), new_features, "{feature:?}");
        }
    }
}

#[test]
fn fingerprints_match_independently_serialized_vanilla_facts() {
    // Digests of the oracle rows from the pinned Mojang server JARs,
    // sorted by item ID and serialized with the v1 fingerprint format.
    let old = GameplayFactPack::for_version(TargetVersion::V1_21_1);
    let new = GameplayFactPack::for_version(TargetVersion::V26_3);
    assert_eq!(old.content_fingerprint(), 0xb573_f5f7_fafa_0aa8);
    assert_eq!(new.content_fingerprint(), 0xacdf_b7f0_f657_d75b);
    assert_ne!(old.content_fingerprint(), new.content_fingerprint());
    assert_eq!(
        GameplayFactPack::for_version(TargetVersion::V1_21_1).content_fingerprint(),
        old.content_fingerprint(),
    );
}
