// SPDX-License-Identifier: LGPL-3.0-or-later
//! Unit, parity, and throughput tests for `megu_core::block_flags_lut`.

use megu_core::block_flags_lut::{
    pack_word, word_emission, word_opacity, BlockFlagsLut, SectionBitTree4096,
    EMISSION_MASK, EMISSION_SHIFT, FLAG_AIR, FLAG_BITS_MASK, FLAG_BLOCK_ENTITY, FLAG_CLIMBABLE,
    FLAG_END_STONE, FLAG_FIRE, FLAG_FLUID, FLAG_FULL_CUBE, FLAG_GRAVITY_AFFECTED, FLAG_HAZARD,
    FLAG_LAVA, FLAG_LIGHT_EMITTER, FLAG_OPAQUE, FLAG_RANDOM_TICKABLE, FLAG_REDSTONE_CONNECT,
    FLAG_REPLACEABLE, FLAG_SOLID, FLAG_TRANSPARENT, FLAG_WATER, OPACITY_MASK, OPACITY_SHIFT,
    SECTION_VOLUME, SECTION_WORDS,
};
use megu_core::perf_kit::splitmix64;
use std::collections::BTreeSet;

/// All 18 boolean flag constants, in bit order.
const ALL_FLAGS: [u64; 18] = [
    FLAG_AIR,
    FLAG_SOLID,
    FLAG_FULL_CUBE,
    FLAG_OPAQUE,
    FLAG_TRANSPARENT,
    FLAG_FLUID,
    FLAG_WATER,
    FLAG_LAVA,
    FLAG_FIRE,
    FLAG_CLIMBABLE,
    FLAG_REPLACEABLE,
    FLAG_RANDOM_TICKABLE,
    FLAG_LIGHT_EMITTER,
    FLAG_BLOCK_ENTITY,
    FLAG_REDSTONE_CONNECT,
    FLAG_HAZARD,
    FLAG_END_STONE,
    FLAG_GRAVITY_AFFECTED,
];

// ---------------------------------------------------------------------------
// Flag word layout / packing
// ---------------------------------------------------------------------------

#[test]
fn flag_constants_use_their_documented_bit_positions() {
    // The word layout is a cross-lane contract: pinning the concrete bits
    // stops a silent renumbering that every symbolic-mask check would miss.
    let expected: [u64; 18] = [
        1 << 0,  // AIR
        1 << 1,  // SOLID
        1 << 2,  // FULL_CUBE
        1 << 3,  // OPAQUE
        1 << 4,  // TRANSPARENT
        1 << 5,  // FLUID
        1 << 6,  // WATER
        1 << 7,  // LAVA
        1 << 8,  // FIRE
        1 << 9,  // CLIMBABLE
        1 << 10, // REPLACEABLE
        1 << 11, // RANDOM_TICKABLE
        1 << 12, // LIGHT_EMITTER
        1 << 13, // BLOCK_ENTITY
        1 << 14, // REDSTONE_CONNECT
        1 << 15, // HAZARD
        1 << 16, // END_STONE
        1 << 17, // GRAVITY_AFFECTED
    ];
    assert_eq!(ALL_FLAGS, expected);
    assert_eq!(EMISSION_SHIFT, 48);
    assert_eq!(OPACITY_SHIFT, 52);
}

#[test]
fn flag_constants_are_distinct_single_bits_below_the_nibbles() {
    let mut seen = 0u64;
    for flag in ALL_FLAGS {
        assert_eq!(flag.count_ones(), 1, "flag {flag:#x} must be one bit");
        assert_eq!(seen & flag, 0, "flag {flag:#x} collides with another");
        assert_eq!(
            flag & FLAG_BITS_MASK,
            flag,
            "flag {flag:#x} outside bit zone"
        );
        seen |= flag;
    }
    assert_eq!(seen.count_ones(), 18);
    assert_eq!(seen & !FLAG_BITS_MASK, 0, "flags stay below the nibbles");
    assert_eq!(EMISSION_MASK, 0xF << EMISSION_SHIFT);
    assert_eq!(OPACITY_MASK, 0xF << OPACITY_SHIFT);
    assert_eq!(EMISSION_MASK & FLAG_BITS_MASK, 0);
    assert_eq!(OPACITY_MASK & FLAG_BITS_MASK, 0);
    assert_eq!(EMISSION_MASK & OPACITY_MASK, 0);
}

#[test]
fn pack_word_round_trips_flags_and_nibbles() {
    for flags in ALL_FLAGS {
        for emission in [0u8, 1, 6, 15] {
            for opacity in [0u8, 1, 15] {
                let word = pack_word(flags, emission, opacity);
                assert_eq!(word & FLAG_BITS_MASK, flags);
                assert_eq!(word_emission(word), emission);
                assert_eq!(word_opacity(word), opacity);
            }
        }
    }
    // A word with no flags still round-trips the nibbles.
    let word = pack_word(0, 14, 15);
    assert_eq!(word & FLAG_BITS_MASK, 0);
    assert_eq!(word_emission(word), 14);
    assert_eq!(word_opacity(word), 15);
}

#[test]
fn pack_word_clamps_levels_to_nibble() {
    let word = pack_word(FLAG_FIRE, 42, 255);
    assert_eq!(word & FLAG_BITS_MASK, FLAG_FIRE);
    assert_eq!(word_emission(word), 15);
    assert_eq!(word_opacity(word), 15);
    // Stray flag bits inside the nibble zone are dropped, not folded in.
    let polluted = pack_word(1u64 << EMISSION_SHIFT, 0, 0);
    assert_eq!(polluted, 0);
    assert_eq!(word_emission(polluted), 0);
}

// ---------------------------------------------------------------------------
// BlockFlagsLut
// ---------------------------------------------------------------------------

fn sample_registry() -> Vec<&'static str> {
    vec![
        "minecraft:air",                  // 0
        "minecraft:stone",                // 1
        "minecraft:fire",                 // 2
        "minecraft:soul_fire",            // 3
        "minecraft:end_stone",            // 4
        "minecraft:water",                // 5
        "minecraft:lava",                 // 6
        "minecraft:torch",                // 7
        "minecraft:glowstone",            // 8
        "minecraft:oak_leaves",           // 9
        "minecraft:sand",                 // 10
        "minecraft:ladder",               // 11
        "minecraft:chest",                // 12
        "minecraft:redstone_wire",        // 13
        "minecraft:wheat",                // 14
        "minecraft:magma_block",          // 15
        "minecraft:glass",                // 16
        "minecraft:tinted_glass",         // 17
        "minecraft:furnace[lit=true]",    // 18
        "minecraft:furnace[lit=false]",   // 19
        "minecraft:campfire",             // 20
        "minecraft:powder_snow",          // 21
        "minecraft:white_shulker_box",    // 22
        "minecraft:tube_coral_block",     // 23
        "minecraft:dead_brain_coral_fan", // 24
    ]
}

#[test]
fn names_classify_to_expected_flag_words() {
    let lut = BlockFlagsLut::from_names(sample_registry());

    // Air: no collision, transparent, replaceable.
    let air = lut.flags(0);
    assert_eq!(
        air & (FLAG_AIR | FLAG_TRANSPARENT | FLAG_REPLACEABLE),
        FLAG_AIR | FLAG_TRANSPARENT | FLAG_REPLACEABLE
    );
    assert_eq!(air & (FLAG_SOLID | FLAG_FULL_CUBE | FLAG_OPAQUE), 0);
    assert_eq!(lut.emission(0), 0);
    assert_eq!(lut.opacity(0), 0);

    // Stone: the modal vanilla opaque cube.
    assert_eq!(
        lut.flags(1),
        pack_word(FLAG_SOLID | FLAG_FULL_CUBE | FLAG_OPAQUE, 0, 15)
    );

    // Fire: the hot-loop name compare this LUT replaces.
    let fire = lut.flags(2);
    assert!(lut.has(2, FLAG_FIRE));
    assert_eq!(
        fire & (FLAG_HAZARD | FLAG_REPLACEABLE | FLAG_TRANSPARENT | FLAG_LIGHT_EMITTER),
        FLAG_HAZARD | FLAG_REPLACEABLE | FLAG_TRANSPARENT | FLAG_LIGHT_EMITTER
    );
    assert_eq!(lut.emission(2), 15);
    assert!(
        lut.has(2, FLAG_RANDOM_TICKABLE),
        "fire spreads by random tick"
    );

    // Soul fire burns but does not spread.
    assert!(lut.has(3, FLAG_FIRE | FLAG_HAZARD));
    assert!(!lut.has(3, FLAG_RANDOM_TICKABLE));
    assert_eq!(lut.emission(3), 10);

    // End stone: exact-name flag, plain opaque cube otherwise.
    assert_eq!(
        lut.flags(4),
        pack_word(
            FLAG_END_STONE | FLAG_SOLID | FLAG_FULL_CUBE | FLAG_OPAQUE,
            0,
            15
        )
    );

    // Fluids.
    let water = lut.flags(5);
    assert_eq!(
        water & (FLAG_FLUID | FLAG_WATER | FLAG_TRANSPARENT | FLAG_REPLACEABLE),
        FLAG_FLUID | FLAG_WATER | FLAG_TRANSPARENT | FLAG_REPLACEABLE
    );
    assert_eq!(lut.opacity(5), 1, "water diffuses skylight one extra level");
    assert!(!lut.has(5, FLAG_LAVA | FLAG_SOLID | FLAG_HAZARD));
    let lava = lut.flags(6);
    assert_eq!(
        lava & (FLAG_FLUID | FLAG_LAVA | FLAG_HAZARD | FLAG_LIGHT_EMITTER),
        FLAG_FLUID | FLAG_LAVA | FLAG_HAZARD | FLAG_LIGHT_EMITTER
    );
    assert_eq!(lut.emission(6), 15);
    assert!(!lut.has(6, FLAG_WATER));

    // Light emitters.
    assert_eq!(lut.emission(7), 14, "torch");
    assert!(lut.has(7, FLAG_LIGHT_EMITTER | FLAG_TRANSPARENT));
    assert_eq!(lut.emission(8), 15, "glowstone");
    assert!(lut.has(8, FLAG_OPAQUE), "glowstone still occludes");

    // Leaves diffuse skylight and decay by random tick.
    assert!(lut.has(
        9,
        FLAG_SOLID | FLAG_FULL_CUBE | FLAG_TRANSPARENT | FLAG_RANDOM_TICKABLE
    ));
    assert_eq!(lut.opacity(9), 1);

    // Gravity + shape flags.
    assert!(lut.has(
        10,
        FLAG_SOLID | FLAG_FULL_CUBE | FLAG_OPAQUE | FLAG_GRAVITY_AFFECTED
    ));
    assert!(lut.has(11, FLAG_SOLID | FLAG_TRANSPARENT | FLAG_CLIMBABLE));
    assert!(!lut.has(11, FLAG_FULL_CUBE));

    // Block entity + redstone connection.
    assert!(lut.has(12, FLAG_SOLID | FLAG_TRANSPARENT | FLAG_BLOCK_ENTITY));
    assert!(!lut.has(12, FLAG_FULL_CUBE), "chests are not full cubes");
    assert!(lut.has(13, FLAG_REDSTONE_CONNECT | FLAG_TRANSPARENT));

    // Random tick + hazard.
    assert!(lut.has(14, FLAG_RANDOM_TICKABLE | FLAG_TRANSPARENT));
    assert!(lut.has(15, FLAG_HAZARD | FLAG_OPAQUE));
    assert_eq!(lut.emission(15), 3);

    // Glass is a solid full cube that passes all light; tinted glass is opaque.
    assert_eq!(
        lut.flags(16),
        pack_word(FLAG_SOLID | FLAG_FULL_CUBE | FLAG_TRANSPARENT, 0, 0)
    );
    assert_eq!(
        lut.flags(17),
        pack_word(FLAG_SOLID | FLAG_FULL_CUBE | FLAG_OPAQUE, 0, 15)
    );

    // Conditional emitters keep the flag even at level 0.
    assert!(lut.has(18, FLAG_LIGHT_EMITTER | FLAG_BLOCK_ENTITY));
    assert_eq!(lut.emission(18), 13, "lit furnace");
    assert!(lut.has(19, FLAG_LIGHT_EMITTER));
    assert_eq!(lut.emission(19), 0, "unlit furnace");

    // Campfire: hazard + block entity + emitter.
    assert!(lut.has(20, FLAG_HAZARD | FLAG_BLOCK_ENTITY | FLAG_LIGHT_EMITTER));
    assert_eq!(lut.emission(20), 15);

    // Powder snow: no collision, hazardous.
    assert!(lut.has(21, FLAG_HAZARD | FLAG_TRANSPARENT));
    assert!(!lut.has(21, FLAG_SOLID));

    // Shulker boxes are closed opaque full cubes with a block entity.
    assert!(lut.has(
        22,
        FLAG_SOLID | FLAG_FULL_CUBE | FLAG_OPAQUE | FLAG_BLOCK_ENTITY
    ));

    // Coral blocks are opaque; coral fans are non-solid.
    assert!(lut.has(23, FLAG_SOLID | FLAG_FULL_CUBE | FLAG_OPAQUE));
    assert!(lut.has(24, FLAG_TRANSPARENT));
    assert!(!lut.has(24, FLAG_SOLID));
}

#[test]
fn lut_is_fail_closed_out_of_bounds() {
    let lut = BlockFlagsLut::from_names(sample_registry());
    let len = lut.len() as u32;
    for id in [len, len + 1, u32::MAX] {
        assert_eq!(lut.flags(id), 0);
        assert!(!lut.has(id, u64::MAX));
        assert_eq!(lut.emission(id), 0);
        assert_eq!(lut.opacity(id), 0);
    }
    let empty = BlockFlagsLut::default();
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);
    assert_eq!(empty.flags(0), 0);
}

#[test]
fn lut_preserves_state_id_order_and_supports_custom_words() {
    let names = sample_registry();
    let lut = BlockFlagsLut::from_names(names.iter().copied());
    assert_eq!(lut.len(), names.len());
    // Custom words bypass classification entirely.
    let custom = BlockFlagsLut::from_words(vec![u64::MAX, pack_word(FLAG_WATER, 0, 1)]);
    assert_eq!(custom.flags(0), u64::MAX);
    assert_eq!(custom.flags(1), pack_word(FLAG_WATER, 0, 1));
    assert_eq!(custom.emission(0), 15);
}

#[test]
fn lut_matches_the_string_scan_it_replaces() {
    // Parity: every hot-loop string compare in the spec must agree with the
    // single-instruction mask check across the whole registry slice.
    let names = sample_registry();
    let lut = BlockFlagsLut::from_names(names.iter().copied());
    for (id, name) in names.iter().enumerate() {
        let id = id as u32;
        assert_eq!(
            *name == "minecraft:fire" || *name == "minecraft:soul_fire",
            lut.has(id, FLAG_FIRE),
            "fire parity failed for {name}"
        );
        assert_eq!(*name == "minecraft:end_stone", lut.has(id, FLAG_END_STONE));
        assert_eq!(*name == "minecraft:water", lut.has(id, FLAG_WATER));
        assert_eq!(*name == "minecraft:lava", lut.has(id, FLAG_LAVA));
        assert_eq!(
            *name == "minecraft:water" || *name == "minecraft:lava",
            lut.has(id, FLAG_FLUID)
        );
    }
}

#[test]
fn state_properties_drive_emission_levels() {
    // name, expected emission — exercises lit/charges/pickles/waterlogged/level.
    let cases: &[(&str, u8)] = &[
        ("minecraft:furnace[lit=true,facing=north]", 13),
        ("minecraft:furnace[lit=false]", 0),
        ("minecraft:blast_furnace[lit=true]", 13),
        ("minecraft:smoker[lit=true]", 13),
        ("minecraft:redstone_lamp[lit=true]", 15),
        ("minecraft:redstone_lamp[lit=false]", 0),
        ("minecraft:redstone_ore[lit=true]", 9),
        ("minecraft:deepslate_redstone_ore[lit=true]", 9),
        ("minecraft:candle[lit=true]", 3),
        ("minecraft:candle[lit=false]", 0),
        ("minecraft:copper_bulb[lit=true]", 15),
        ("minecraft:waxed_copper_bulb[lit=true]", 15),
        ("minecraft:sea_pickle[pickles=1,waterlogged=true]", 6),
        ("minecraft:sea_pickle[pickles=4,waterlogged=true]", 15),
        ("minecraft:sea_pickle[pickles=4,waterlogged=false]", 0),
        ("minecraft:respawn_anchor[charges=1]", 3),
        ("minecraft:respawn_anchor[charges=4]", 15),
        ("minecraft:respawn_anchor[charges=0]", 0),
        ("minecraft:light[level=15]", 15),
        ("minecraft:light[level=3]", 3),
        ("minecraft:campfire[lit=false]", 0),
        ("minecraft:campfire[lit=true]", 15),
        ("minecraft:soul_campfire[lit=true]", 10),
    ];
    for (name, expected) in cases {
        let lut = BlockFlagsLut::from_names([*name]);
        assert_eq!(lut.emission(0), *expected, "{name}");
        assert!(
            lut.has(0, FLAG_LIGHT_EMITTER),
            "{name} must carry emitter flag"
        );
    }
}

#[test]
fn modded_namespaces_and_unknown_names_classify_safely() {
    // Namespace is ignored, so modded states reuse the vanilla vocabulary.
    let lut = BlockFlagsLut::from_names(["mymod:soul_lantern", "mymod:fire"]);
    assert_eq!(lut.emission(0), 10);
    assert!(lut.has(1, FLAG_FIRE));
    // Unrecognized names default to the modal opaque cube — never a panic.
    let mystery = BlockFlagsLut::from_names(["mymod:unobtanium", ""]);
    assert_eq!(
        mystery.flags(0),
        pack_word(FLAG_SOLID | FLAG_FULL_CUBE | FLAG_OPAQUE, 0, 15)
    );
    assert_eq!(
        mystery.flags(1),
        pack_word(FLAG_SOLID | FLAG_FULL_CUBE | FLAG_OPAQUE, 0, 15)
    );
}

#[test]
fn lut_flag_scan_throughput_smoke() {
    // A registry-sized LUT swept like a chunk tick: no timing assertion,
    // just a fixed aggregate proving the hot loop runs end to end.
    let lut = BlockFlagsLut::from_names(sample_registry());
    let mut found = 0u64;
    for round in 0..4096u32 {
        for state in 0..lut.len() as u32 {
            if lut.has(
                (state + round) % lut.len() as u32,
                FLAG_FIRE | FLAG_END_STONE,
            ) {
                found += 1;
            }
        }
    }
    // Each round hits exactly the fire, soul_fire, and end_stone states.
    assert_eq!(found, 3 * 4096);
}

// ---------------------------------------------------------------------------
// SectionBitTree4096
// ---------------------------------------------------------------------------

#[test]
fn bit_tree_is_exactly_520_bytes() {
    assert_eq!(SECTION_VOLUME, 4096);
    assert_eq!(SECTION_WORDS, 64);
    assert_eq!(std::mem::size_of::<SectionBitTree4096>(), 520);
}

#[test]
fn bit_tree_set_clear_and_membership() {
    let mut tree = SectionBitTree4096::new();
    assert!(tree.is_empty());
    assert_eq!(tree.count(), 0);

    assert!(tree.set(0));
    assert!(tree.set(63));
    assert!(tree.set(64));
    assert!(tree.set(4095));
    assert!(!tree.set(64), "second set of the same bit is idempotent");
    assert_eq!(tree.count(), 4);
    for idx in [0, 63, 64, 4095] {
        assert!(tree.is_set(idx));
    }
    for idx in [1, 62, 65, 2048, 4094] {
        assert!(!tree.is_set(idx));
    }

    assert!(!tree.clear(1), "clearing an unset bit is a no-op");
    assert!(tree.clear(63));
    assert_eq!(tree.count(), 3);
    assert!(!tree.is_set(63));
    assert!(!tree.is_empty());
}

#[test]
fn bit_tree_for_each_set_visits_exact_indices_ascending() {
    let mut tree = SectionBitTree4096::new();
    let expected: Vec<usize> = vec![0, 1, 62, 63, 64, 65, 127, 128, 2048, 3000, 4094, 4095];
    for &i in &expected {
        tree.set(i);
    }
    let mut visited = Vec::new();
    tree.for_each_set(|i| visited.push(i));
    assert_eq!(visited, expected);
    assert_eq!(visited.len() as u32, tree.count() as u32);
}

#[test]
fn bit_tree_full_section_iterates_every_block_in_order() {
    let mut tree = SectionBitTree4096::new();
    for i in 0..SECTION_VOLUME {
        assert!(tree.set(i));
    }
    assert_eq!(tree.count(), SECTION_VOLUME);
    assert_eq!(tree.non_empty_leaf_count(), 64);
    let mut visited = Vec::with_capacity(SECTION_VOLUME);
    tree.for_each_set(|i| visited.push(i));
    assert_eq!(visited, (0..SECTION_VOLUME).collect::<Vec<_>>());
    // Clearing the middle of the middle leaf updates the summary correctly.
    tree.clear(2048);
    assert_eq!(tree.count(), SECTION_VOLUME - 1);
    assert_eq!(tree.non_empty_leaf_count(), 64);
    tree.reset();
    assert!(tree.is_empty());
    assert_eq!(tree.count(), 0);
}

#[test]
fn bit_tree_skips_empty_sections_in_o1() {
    let tree = SectionBitTree4096::new();
    assert!(tree.is_empty());
    assert_eq!(tree.non_empty_leaf_count(), 0);
    let mut calls = 0u32;
    tree.for_each_set(|_| calls += 1);
    assert_eq!(
        calls, 0,
        "an empty summary word must short-circuit the scan"
    );

    // A single set bit opens exactly one leaf word.
    let mut sparse = SectionBitTree4096::new();
    sparse.set(4095);
    assert_eq!(sparse.non_empty_leaf_count(), 1);
    let mut visited = 0u32;
    sparse.for_each_set(|_| visited += 1);
    assert_eq!(visited, 1);

    // Clearing back to empty restores the O(1) skip — summary stays exact.
    sparse.clear(4095);
    assert!(sparse.is_empty());
    assert_eq!(sparse.non_empty_leaf_count(), 0);
}

#[test]
fn bit_tree_is_fail_closed_out_of_bounds() {
    let mut tree = SectionBitTree4096::new();
    for bad in [SECTION_VOLUME, SECTION_VOLUME + 1, usize::MAX] {
        assert!(!tree.set(bad));
        assert!(!tree.clear(bad));
        assert!(!tree.is_set(bad));
    }
    assert!(tree.is_empty());
}

#[test]
fn bit_tree_matches_a_btreeset_oracle_under_churn() {
    // Parity: a deterministic splitmix sequence of set/clear ops must leave
    // the tree agreeing with a BTreeSet on contents, order and count.
    let mut tree = SectionBitTree4096::new();
    let mut oracle: BTreeSet<usize> = BTreeSet::new();
    let mut seed = 0xD1B5_4A32u64;
    for step in 0..20_000 {
        seed = splitmix64(seed);
        let index = (seed % SECTION_VOLUME as u64) as usize;
        if seed & 3 == 0 {
            assert_eq!(tree.clear(index), oracle.remove(&index));
        } else {
            assert_eq!(tree.set(index), oracle.insert(index));
        }
        if step % 977 == 0 {
            assert_eq!(tree.count(), oracle.len());
        }
    }
    let mut visited = Vec::new();
    tree.for_each_set(|i| visited.push(i));
    assert_eq!(visited, oracle.iter().copied().collect::<Vec<_>>());
    assert_eq!(tree.is_empty(), oracle.is_empty());
    for &i in oracle.iter().take(64) {
        assert!(tree.is_set(i));
    }
}

#[test]
fn bit_tree_predicate_sweep_throughput_smoke() {
    // Simulate building a random_tickable mask per section over many
    // sections: set every 7th bit, iterate, verify the aggregate.
    let mut tree = SectionBitTree4096::new();
    for i in (0..SECTION_VOLUME).step_by(7) {
        tree.set(i);
    }
    let expected = SECTION_VOLUME.div_ceil(7);
    assert_eq!(tree.count(), expected);
    let mut sum = 0u64;
    for _ in 0..256 {
        tree.for_each_set(|i| sum += i as u64);
    }
    let single: u64 = (0..SECTION_VOLUME).step_by(7).map(|i| i as u64).sum();
    assert_eq!(sum, single * 256);
}
