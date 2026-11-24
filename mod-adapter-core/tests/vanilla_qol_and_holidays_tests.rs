// SPDX-License-Identifier: LGPL-3.0-or-later

//! Integration tests for
//! [`mod_adapter_core::vanilla_qol_and_holidays`]: the vanilla
//! Halloween/Christmas calendar events, the zero-clutter recipe
//! conflict resolver, and the strictly opt-in Quark-compatible QoL
//! hooks. All dates are caller-supplied — no wall clock anywhere — and
//! every mod id below is invented test data.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use mod_adapter_core::vanilla_qol_and_holidays::*;
use mod_adapter_core::world_extensions::SortableStack;

fn d(month: u8, day: u8) -> CalendarDate {
    CalendarDate { month, day }
}

fn holidays() -> HolidayConfig {
    HolidayConfig::default()
}

fn qol_all_on() -> QuarkCompatQolConfig {
    QuarkCompatQolConfig {
        right_click_harvest: true,
        ladder_extend_down: true,
        quick_stack_to_nearby_chests: true,
    }
}

fn cand(id: &str) -> RecipeCandidate {
    RecipeCandidate {
        recipe_id: id.to_string(),
        output_item: format!("{}_out", id.replace(':', "_")),
        output_count: 1,
        mod_id: id.split(':').next().unwrap_or("minecraft").to_string(),
    }
}

fn stack(item: &str, count: u32) -> Option<SortableStack> {
    Some(SortableStack {
        item_id: item.to_string(),
        component_hash: 0,
        count,
        max_stack_size: 64,
    })
}

fn totals(
    inv: &[Option<SortableStack>],
    chests: &[Vec<Option<SortableStack>>],
) -> BTreeMap<String, u64> {
    let mut t: BTreeMap<String, u64> = BTreeMap::new();
    for s in inv
        .iter()
        .flatten()
        .chain(chests.iter().flat_map(|c| c.iter().flatten()))
    {
        *t.entry(s.item_id.clone()).or_default() += u64::from(s.count);
    }
    t
}

// ===========================================================================
// 1a. CalendarDate & holiday windows
// ===========================================================================

#[test]
fn calendar_date_validates_ranges() {
    assert_eq!(CalendarDate::new(10, 31), Some(d(10, 31)));
    assert_eq!(CalendarDate::new(1, 1), Some(d(1, 1)));
    assert_eq!(CalendarDate::new(12, 26), Some(d(12, 26)));
    assert_eq!(CalendarDate::new(0, 10), None);
    assert_eq!(CalendarDate::new(13, 10), None);
    assert_eq!(CalendarDate::new(10, 0), None);
    assert_eq!(CalendarDate::new(10, 32), None);
    assert!(!d(0, 10).is_valid());
    assert!(!d(10, 99).is_valid());
}

#[test]
fn halloween_is_october_31_only() {
    assert!(is_halloween(d(10, 31)));
    assert!(!is_halloween(d(10, 30)));
    assert!(!is_halloween(d(11, 1)));
    assert!(!is_halloween(d(10, 32))); // invalid date never holidays
}

#[test]
fn christmas_window_is_dec_24_to_26() {
    assert!(!is_christmas_window(d(12, 23)));
    assert!(is_christmas_window(d(12, 24)));
    assert!(is_christmas_window(d(12, 25)));
    assert!(is_christmas_window(d(12, 26)));
    assert!(!is_christmas_window(d(12, 27)));
    assert!(!is_christmas_window(d(11, 25)));
}

#[test]
fn bat_halloween_window_matches_natural_spawner() {
    // SharedConstants.isAroundHalloween: Oct 20 through Nov 3 inclusive.
    assert!(!is_bat_halloween_window(d(10, 19)));
    assert!(is_bat_halloween_window(d(10, 20)));
    assert!(is_bat_halloween_window(d(10, 31)));
    assert!(is_bat_halloween_window(d(11, 1)));
    assert!(is_bat_halloween_window(d(11, 3)));
    assert!(!is_bat_halloween_window(d(11, 4)));
    assert!(!is_bat_halloween_window(d(9, 30)));
}

// ===========================================================================
// 1b. Halloween headgear roll
// ===========================================================================

#[test]
fn halloween_roll_exact_probability_thresholds() {
    let cfg = holidays();
    let oct31 = d(10, 31);
    // 2.5% inner gate -> jack o'lantern.
    let jol = roll_halloween_headgear("zombie", oct31, &cfg, 0.01).unwrap();
    assert_eq!(jol.kind, HalloweenHeadgearKind::JackOLantern);
    assert_eq!(jol.head_drop_chance, 0.0);
    // Boundary: 0.025 is no longer jack o'lantern but still in the 25% gate.
    let carved_edge = roll_halloween_headgear("zombie", oct31, &cfg, 0.025).unwrap();
    assert_eq!(carved_edge.kind, HalloweenHeadgearKind::CarvedPumpkin);
    let carved = roll_halloween_headgear("zombie", oct31, &cfg, 0.10).unwrap();
    assert_eq!(carved.kind, HalloweenHeadgearKind::CarvedPumpkin);
    // Boundary + miss: 0.25 is the outer gate edge; 0.30 a clear miss.
    assert_eq!(roll_halloween_headgear("zombie", oct31, &cfg, 0.25), None);
    assert_eq!(roll_halloween_headgear("zombie", oct31, &cfg, 0.30), None);
}

#[test]
fn halloween_eligible_mob_set_is_exact() {
    let cfg = holidays();
    let oct31 = d(10, 31);
    for mob in HALLOWEEN_HEADGEAR_MOBS {
        assert!(
            roll_halloween_headgear(mob, oct31, &cfg, 0.10).is_some(),
            "{mob} must be eligible"
        );
    }
    // Namespaced vanilla ids are equivalent; modded lookalikes are not.
    assert!(roll_halloween_headgear("minecraft:skeleton", oct31, &cfg, 0.10).is_some());
    assert_eq!(
        roll_halloween_headgear("mymod:zombie", oct31, &cfg, 0.10),
        None
    );
    // Vanilla mobs outside the zombie/skeleton families never roll.
    for mob in ["creeper", "cow", "minecraft:creeper", "pig", "wither"] {
        assert_eq!(roll_halloween_headgear(mob, oct31, &cfg, 0.10), None);
    }
}

#[test]
fn halloween_requires_the_real_date_unless_forced() {
    let cfg = holidays();
    assert_eq!(roll_halloween_headgear("zombie", d(10, 30), &cfg, 0.01), None);
    assert_eq!(roll_halloween_headgear("zombie", d(11, 1), &cfg, 0.01), None);
    // Forced server event fires on any date...
    let forced = HolidayConfig {
        enabled: true,
        force_event: Some(HolidayEvent::Halloween),
    };
    assert!(roll_halloween_headgear("zombie", d(6, 15), &forced, 0.01).is_some());
    // ...but only that event — forcing Halloween suppresses Christmas.
    let forced_on_xmas = HolidayConfig {
        enabled: true,
        force_event: Some(HolidayEvent::Halloween),
    };
    assert_eq!(
        chest_holiday_variant("minecraft:chest", d(12, 25), &forced_on_xmas),
        ChestVisualVariant::Standard
    );
}

#[test]
fn halloween_respects_disabled_config_and_bad_rolls() {
    let disabled = HolidayConfig {
        enabled: false,
        force_event: None,
    };
    assert_eq!(
        roll_halloween_headgear("zombie", d(10, 31), &disabled, 0.01),
        None
    );
    let cfg = holidays();
    assert_eq!(
        roll_halloween_headgear("zombie", d(10, 31), &cfg, f32::NAN),
        None
    );
    assert_eq!(roll_halloween_headgear("zombie", d(10, 31), &cfg, -0.5), None);
}

// ===========================================================================
// 1c. Christmas chest reskin
// ===========================================================================

#[test]
fn christmas_reskins_chest_and_trapped_chest_only() {
    let cfg = holidays();
    for day in 24..=26 {
        let date = d(12, day);
        assert_eq!(
            chest_holiday_variant("minecraft:chest", date, &cfg),
            ChestVisualVariant::ChristmasPresent
        );
        assert_eq!(
            chest_holiday_variant("minecraft:trapped_chest", date, &cfg),
            ChestVisualVariant::ChristmasPresent
        );
        // Vanilla ender chests NEVER turn into presents.
        assert_eq!(
            chest_holiday_variant("minecraft:ender_chest", date, &cfg),
            ChestVisualVariant::Standard
        );
        // Other blocks and modded chests stay standard.
        assert_eq!(
            chest_holiday_variant("minecraft:furnace", date, &cfg),
            ChestVisualVariant::Standard
        );
        assert_eq!(
            chest_holiday_variant("mymod:chest", date, &cfg),
            ChestVisualVariant::Standard
        );
    }
    // Outside the window everything is standard.
    for day in [23, 27] {
        assert_eq!(
            chest_holiday_variant("minecraft:chest", d(12, day), &cfg),
            ChestVisualVariant::Standard
        );
    }
    // Disabled config suppresses the reskin entirely.
    let disabled = HolidayConfig {
        enabled: false,
        force_event: None,
    };
    assert_eq!(
        chest_holiday_variant("minecraft:chest", d(12, 25), &disabled),
        ChestVisualVariant::Standard
    );
    // Forced Christmas event works on any date.
    let forced = HolidayConfig {
        enabled: true,
        force_event: Some(HolidayEvent::Christmas),
    };
    assert_eq!(
        chest_holiday_variant("minecraft:chest", d(7, 4), &forced),
        ChestVisualVariant::ChristmasPresent
    );
}

// ===========================================================================
// 2. Recipe conflict resolver
// ===========================================================================

#[test]
fn resolver_hidden_for_zero_one_or_disabled() {
    let sig = recipe_grid_signature(&["minecraft:iron_ingot"; 9]);
    let mut r = RecipeConflictResolver::new(RecipeConflictConfig::default());
    r.set_candidates(sig, vec![]);
    assert_eq!(r.ui_visibility(), ConflictUiVisibility::Hidden);
    r.set_candidates(sig, vec![cand("minecraft:iron_pickaxe")]);
    assert_eq!(r.ui_visibility(), ConflictUiVisibility::Hidden);

    let mut disabled = RecipeConflictResolver::new(RecipeConflictConfig {
        enabled: false,
        external_conflict_mod_present: false,
    });
    disabled.set_candidates(sig, vec![cand("a:x"), cand("b:y")]);
    assert_eq!(disabled.ui_visibility(), ConflictUiVisibility::Hidden);
}

#[test]
fn resolver_yields_to_external_conflict_mod() {
    let mut r = RecipeConflictResolver::new(RecipeConflictConfig {
        enabled: true,
        external_conflict_mod_present: true,
    });
    r.set_candidates(1, vec![cand("a:x"), cand("b:y"), cand("c:z")]);
    assert_eq!(r.ui_visibility(), ConflictUiVisibility::Hidden);
}

#[test]
fn resolver_sorts_vanilla_first_then_alphabetical() {
    let mut r = RecipeConflictResolver::new(RecipeConflictConfig::default());
    r.set_candidates(
        7,
        vec![
            cand("mymod:z_pick"),
            cand("mymod:a_pick"),
            cand("minecraft:iron_pickaxe"),
        ],
    );
    let order: Vec<&str> = r
        .candidates()
        .iter()
        .map(|c| c.recipe_id.as_str())
        .collect();
    assert_eq!(
        order,
        ["minecraft:iron_pickaxe", "mymod:a_pick", "mymod:z_pick"]
    );
    assert_eq!(
        r.ui_visibility(),
        ConflictUiVisibility::CompactSwitcher {
            selected_index: 0,
            total: 3
        }
    );
}

#[test]
fn resolver_next_prev_and_select_wrap_cleanly() {
    let mut r = RecipeConflictResolver::new(RecipeConflictConfig::default());
    r.set_candidates(3, vec![cand("a:x"), cand("b:y")]);
    // Forward wrap: 0 -> 1 -> 0.
    r.next();
    assert_eq!(r.selected_index(), 1);
    r.next();
    assert_eq!(r.selected_index(), 0);
    // Backward wrap: 0 -> 1.
    r.prev();
    assert_eq!(r.selected_index(), 1);
    r.prev();
    assert_eq!(r.selected_index(), 0);
    // select() wraps raw indices.
    r.select(7);
    assert_eq!(r.selected_index(), 7 % 2);
    assert_eq!(
        r.ui_visibility(),
        ConflictUiVisibility::CompactSwitcher {
            selected_index: 1,
            total: 2
        }
    );
    assert_eq!(r.selected_recipe().unwrap().recipe_id, "b:y");
}

#[test]
fn resolver_remembers_pick_per_grid_signature() {
    let mut r = RecipeConflictResolver::new(RecipeConflictConfig::default());
    let sig_a = recipe_grid_signature(&["mymod:copper_ingot", "", "", "", "", "", "", "", ""]);
    let sig_b = recipe_grid_signature(&["minecraft:iron_ingot", "", "", "", "", "", "", "", ""]);
    let picks = || vec![cand("minecraft:the_vanilla_way"), cand("mymod:the_mod_way")];

    r.set_candidates(sig_a, picks());
    r.select(1); // player prefers the mod output on grid A
    assert_eq!(r.selected_recipe().unwrap().recipe_id, "mymod:the_mod_way");

    // A different grid starts on the vanilla default.
    r.set_candidates(sig_b, picks());
    assert_eq!(r.selected_index(), 0);

    // Returning to grid A restores the remembered pick — zero extra clicks.
    r.set_candidates(sig_a, picks());
    assert_eq!(r.selected_recipe().unwrap().recipe_id, "mymod:the_mod_way");

    // If the remembered recipe stops matching, fall back to index 0.
    r.set_candidates(sig_a, vec![cand("minecraft:only_one"), cand("othermod:x")]);
    assert_eq!(r.selected_index(), 0);
}

#[test]
fn grid_signature_is_deterministic_and_order_sensitive() {
    let a = recipe_grid_signature(&["minecraft:stick", "", "minecraft:iron_ingot"]);
    assert_eq!(
        a,
        recipe_grid_signature(&["minecraft:stick", "", "minecraft:iron_ingot"])
    );
    assert_ne!(
        a,
        recipe_grid_signature(&["minecraft:iron_ingot", "", "minecraft:stick"])
    );
    assert_ne!(recipe_grid_signature(&["ab", "c"]), recipe_grid_signature(&["a", "bc"]));
}

// ===========================================================================
// 3. Opt-in Quark-compatible QoL hooks
// ===========================================================================

#[test]
fn qol_defaults_are_all_off_and_inert() {
    let cfg = QuarkCompatQolConfig::default();
    assert!(!cfg.right_click_harvest);
    assert!(!cfg.ladder_extend_down);
    assert!(!cfg.quick_stack_to_nearby_chests);

    assert_eq!(
        evaluate_right_click_harvest("minecraft:wheat", 7, 7, &cfg),
        None
    );
    assert_eq!(
        evaluate_ladder_extend_down(10, |_| LadderColumnCell::SupportedAir, &cfg),
        None
    );
    let plan = plan_quick_stack(
        &[stack("minecraft:stone", 64)],
        &[vec![stack("minecraft:stone", 10)]],
        &BTreeSet::new(),
        &cfg,
    );
    assert!(plan.is_empty());
}

#[test]
fn right_click_harvest_only_fires_on_mature_crops() {
    let cfg = qol_all_on();
    let plan = evaluate_right_click_harvest("minecraft:wheat", 7, 7, &cfg).unwrap();
    assert_eq!(plan.reset_age, 0);
    assert_eq!(plan.seeds_deducted, 1);
    // Immature crops never fire.
    for age in 0..7 {
        assert_eq!(
            evaluate_right_click_harvest("minecraft:wheat", age, 7, &cfg),
            None
        );
    }
    // Degenerate / malformed inputs never fire.
    assert_eq!(evaluate_right_click_harvest("minecraft:wheat", 0, 0, &cfg), None);
    assert_eq!(evaluate_right_click_harvest("", 7, 7, &cfg), None);
    // Beetroot-style max_age 3 works identically.
    assert!(evaluate_right_click_harvest("minecraft:beetroots", 3, 3, &cfg).is_some());
}

#[test]
fn ladder_extend_skips_rungs_and_lands_on_first_open_air() {
    let cfg = qol_all_on();
    // Clicked at y=10; rungs at 9 and 8, first supported air at 7.
    let probe = |y: i64| match y {
        9 | 8 => LadderColumnCell::Ladder,
        7 => LadderColumnCell::SupportedAir,
        _ => LadderColumnCell::Blocked,
    };
    assert_eq!(evaluate_ladder_extend_down(10, probe, &cfg), Some(7));
    // Air directly below the clicked rung.
    assert_eq!(
        evaluate_ladder_extend_down(10, |_| LadderColumnCell::SupportedAir, &cfg),
        Some(9)
    );
}

#[test]
fn ladder_extend_stops_at_solid_floor_and_depth_cap() {
    let cfg = qol_all_on();
    // Solid floor immediately below -> no placement.
    assert_eq!(
        evaluate_ladder_extend_down(10, |_| LadderColumnCell::Blocked, &cfg),
        None
    );
    // Rungs all the way down past the cap -> nothing within 64 cells.
    assert_eq!(
        evaluate_ladder_extend_down(100, |_| LadderColumnCell::Ladder, &cfg),
        None
    );
    // Supported air exactly at the 64th cell still lands; at 65 it is
    // out of reach.
    let probe = |y: i64| {
        if y == 36 {
            LadderColumnCell::SupportedAir
        } else {
            LadderColumnCell::Ladder
        }
    };
    assert_eq!(evaluate_ladder_extend_down(100, probe, &cfg), Some(36));
    let too_deep = |y: i64| {
        if y == 35 {
            LadderColumnCell::SupportedAir
        } else {
            LadderColumnCell::Ladder
        }
    };
    assert_eq!(evaluate_ladder_extend_down(100, too_deep, &cfg), None);
}

#[test]
fn quick_stack_moves_only_existing_items_and_conserves() {
    let cfg = qol_all_on();
    // Slots 0..=8 = hotbar (locked). Stone exists in chest 0; the modded
    // gem exists in chest 1; the apple exists in NO chest -> stays.
    let mut inv = vec![
        stack("minecraft:stone", 40),  // slot 0, hotbar -> locked
        None,
        stack("mymod:gem", 70),        // slot 2, locked too
        None,
        None,
        None,
        None,
        None,
        None,
        stack("minecraft:stone", 50),  // slot 9 -> stackable
        stack("mymod:gem", 70),        // slot 10 -> stacks across two piles
        stack("minecraft:apple", 5),   // slot 11 -> nowhere to go
    ];
    let mut chests = vec![
        vec![
            stack("minecraft:stone", 30), // 34 free -> fills to 64
            stack("minecraft:stone", 64), // full -> skipped
            None,                          // empty cells never get new stacks
        ],
        vec![stack("mymod:gem", 60), stack("mymod:gem", 64)],
    ];
    let locked: BTreeSet<usize> = (0..=8).collect();

    let before = totals(&inv, &chests);
    let plan = plan_quick_stack(&inv, &chests, &locked, &cfg);
    assert!(!plan.is_empty());

    // Locked slots were never touched.
    assert!(plan.moves.iter().all(|m| m.from_slot >= 9));
    // Apple has no matching chest stack -> never moved.
    assert!(plan.moves.iter().all(|m| m.item_id != "minecraft:apple"));
    // Stone: 50 - 34 = 16 left in slot 9 (only existing stacks filled).
    // Gem: 70 - 4 = 66 left in slot 10.
    let moved: BTreeMap<&str, u64> = plan
        .moves
        .iter()
        .fold(BTreeMap::new(), |mut acc, m| {
            *acc.entry(m.item_id.as_str()).or_default() += u64::from(m.count);
            acc
        });
    assert_eq!(moved["minecraft:stone"], 34);
    assert_eq!(moved["mymod:gem"], 4);

    assert!(plan.apply(&mut inv, &mut chests));
    // Exact conservation: nothing duped, nothing lost.
    assert_eq!(totals(&inv, &chests), before);
    assert_eq!(inv[9].as_ref().unwrap().count, 16);
    assert_eq!(inv[10].as_ref().unwrap().count, 66);
    assert_eq!(inv[0].as_ref().unwrap().count, 40); // locked, untouched
    assert_eq!(chests[0][0].as_ref().unwrap().count, 64);
    assert_eq!(chests[1][0].as_ref().unwrap().count, 64);
    assert_eq!(chests[0][2], None); // empty cell still empty
}

#[test]
fn quick_stack_empties_source_stacks_and_respects_max_stack() {
    let cfg = qol_all_on();
    // Slot 0 fully drains into the 4-free chest stack; slot 1 stays
    // because the destination hits its 64 cap — the planner never
    // overflows a stack.
    let mut inv = vec![
        stack("minecraft:cobblestone", 4),
        stack("minecraft:cobblestone", 12),
    ];
    let mut chests = vec![vec![stack("minecraft:cobblestone", 60)]];
    let plan = plan_quick_stack(&inv, &chests, &BTreeSet::new(), &cfg);
    assert_eq!(plan.total_moved(), 4);
    assert!(plan.apply(&mut inv, &mut chests));
    assert_eq!(inv[0], None); // fully drained slot clears
    assert_eq!(inv[1].as_ref().unwrap().count, 12);
    assert_eq!(chests[0][0].as_ref().unwrap().count, 64);
}
