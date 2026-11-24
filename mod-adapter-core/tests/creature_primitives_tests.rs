// SPDX-License-Identifier: LGPL-3.0-or-later

//! Integration tests for [`mod_adapter_core::creature_primitives`]:
//! declarative creature profiles exercised through the generic tame/breed
//! state machine, all five [`MovementProfile`] domains, and the
//! [`DynamicEntityMetadata`] `0x58` `SetEntityData` wire codec. Every
//! profile below is invented test data — no real mod names are referenced.

use std::collections::BTreeSet;

use mod_adapter_core::creature_primitives::*;

fn uuid(tag: u8) -> [u8; 16] {
    let mut u = [0u8; 16];
    u[15] = tag;
    u
}

fn tags(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|s| (*s).to_string()).collect()
}

fn no_tags() -> BTreeSet<String> {
    BTreeSet::new()
}

// ===========================================================================
// Declarative creature profiles (generic stand-ins, zero mod names)
// ===========================================================================

/// Bear-style `Land` pet: tag + literal tame items, full 3-mode command
/// cycle, healable with meat, breedable with berries.
fn bear_spec() -> CreatureSpec {
    CreatureSpec {
        entity_type: "anyzoo:bear".to_string(),
        max_health: 30.0,
        tame_items: vec![
            "anyzoo:honeycomb".to_string(),
            "#anyzoo:raw_fish".to_string(),
        ],
        tame_chance: 0.5,
        breed_items: vec!["anyzoo:berries".to_string()],
        heal_items: vec![("anyzoo:meat_chunk".to_string(), 4.0)],
        baby_growth_ticks: DEFAULT_BABY_GROWTH_TICKS,
        breed_cooldown_ticks: DEFAULT_BREED_COOLDOWN_TICKS,
        supports_wander_mode: true,
        can_shoulder_perch: false,
        rideable_when_tamed: true,
        movement: MovementProfile::Land { ground_speed: 0.25 },
    }
}

/// Deer-style `Land` pet that only supports `Follow`/`Sit`.
fn deer_spec() -> CreatureSpec {
    CreatureSpec {
        supports_wander_mode: false,
        rideable_when_tamed: false,
        ..bear_spec()
    }
}

/// Alligator-style `Amphibious` mob — fast in water, slow ashore.
fn alligator_spec() -> CreatureSpec {
    CreatureSpec {
        movement: MovementProfile::Amphibious {
            land_speed: 0.1,
            swim_speed: 0.6,
        },
        ..bear_spec()
    }
}

/// Bird-style `GlidingFlight` shoulder pet.
fn finch_spec() -> CreatureSpec {
    CreatureSpec {
        can_shoulder_perch: true,
        movement: MovementProfile::GlidingFlight {
            ground_speed: 0.1,
            fly_speed: 0.6,
            glide_fall_scale: 0.25,
        },
        ..bear_spec()
    }
}

/// Fish/whale-style `Aquatic` mob with a short air budget for tests.
fn whale_spec() -> CreatureSpec {
    CreatureSpec {
        movement: MovementProfile::Aquatic {
            swim_speed: 0.8,
            suffocates_in_air_ticks: 3,
        },
        ..bear_spec()
    }
}

/// Gecko-style `WallClimb` mob.
fn gecko_spec() -> CreatureSpec {
    CreatureSpec {
        movement: MovementProfile::WallClimb {
            ground_speed: 0.2,
            climb_speed: 0.3,
        },
        ..bear_spec()
    }
}

fn wild_bear(id: u64) -> CreatureState {
    CreatureState::new(id, "anyzoo:bear", 30.0, [0.0, 64.0, 0.0])
}

fn tamed_bear(id: u64, owner: [u8; 16]) -> CreatureState {
    let mut c = wild_bear(id);
    c.owner_uuid = Some(owner);
    c.command_mode = PetCommandMode::Follow;
    c
}

// ===========================================================================
// Taming
// ===========================================================================

#[test]
fn tame_roll_below_chance_sets_owner_and_follow() {
    let spec = bear_spec();
    let mut bear = wild_bear(1);
    let owner = uuid(1);

    let result = interact_with_item(
        &mut bear,
        &spec,
        &owner,
        Some("anyzoo:honeycomb"),
        &no_tags(),
        0.49,
    );
    assert_eq!(
        result,
        CreatureInteractResult::TameAttempt { succeeded: true }
    );
    assert!(result.consumed_item());
    assert_eq!(bear.owner_uuid, Some(owner));
    assert_eq!(bear.command_mode, PetCommandMode::Follow);
}

#[test]
fn tame_roll_at_chance_fails_but_still_consumes() {
    let spec = bear_spec();
    let mut bear = wild_bear(1);

    let result = interact_with_item(
        &mut bear,
        &spec,
        &uuid(1),
        Some("anyzoo:honeycomb"),
        &no_tags(),
        0.5, // == tame_chance: strictly-less roll means failure
    );
    assert_eq!(
        result,
        CreatureInteractResult::TameAttempt { succeeded: false }
    );
    assert!(result.consumed_item());
    assert_eq!(bear.owner_uuid, None);
    assert_eq!(bear.command_mode, PetCommandMode::Wander);
}

#[test]
fn tag_prefixed_tame_items_resolve_against_item_tags() {
    let spec = bear_spec();
    let mut bear = wild_bear(1);

    // `#anyzoo:raw_fish` matches because the held item carries the tag.
    let result = interact_with_item(
        &mut bear,
        &spec,
        &uuid(1),
        Some("anyzoo:salmon"),
        &tags(&["anyzoo:raw_fish", "anyzoo:seafood"]),
        0.1,
    );
    assert_eq!(
        result,
        CreatureInteractResult::TameAttempt { succeeded: true }
    );

    // A different item without the tag does not match.
    let mut bear2 = wild_bear(2);
    let result = interact_with_item(
        &mut bear2,
        &spec,
        &uuid(1),
        Some("anyzoo:bread"),
        &no_tags(),
        0.1,
    );
    assert_eq!(result, CreatureInteractResult::Ignored);
    assert!(!result.consumed_item());
    assert_eq!(bear2.owner_uuid, None);
}

#[test]
fn non_owner_can_never_steal_ownership_or_toggle_mode() {
    let spec = bear_spec();
    let owner = uuid(1);
    let stranger = uuid(2);
    let mut bear = tamed_bear(1, owner);
    bear.command_mode = PetCommandMode::Sit;

    for held in [None, Some("anyzoo:honeycomb"), Some("anyzoo:meat_chunk")] {
        let result = interact_with_item(&mut bear, &spec, &stranger, held, &no_tags(), 0.0);
        assert_eq!(result, CreatureInteractResult::NotOwner, "held={held:?}");
        assert!(!result.consumed_item());
        assert_eq!(bear.owner_uuid, Some(owner));
        assert_eq!(bear.command_mode, PetCommandMode::Sit);
    }
}

// ===========================================================================
// Command mode cycle, healing, breeding
// ===========================================================================

#[test]
fn empty_hand_cycles_follow_sit_wander_follow() {
    let spec = bear_spec();
    let owner = uuid(1);
    let mut bear = tamed_bear(1, owner);

    let cycle =
        |mode: &mut CreatureState| interact_with_item(mode, &spec, &owner, None, &no_tags(), 0.0);
    assert_eq!(
        cycle(&mut bear),
        CreatureInteractResult::CommandModeChanged {
            new_mode: PetCommandMode::Sit
        }
    );
    assert_eq!(
        cycle(&mut bear),
        CreatureInteractResult::CommandModeChanged {
            new_mode: PetCommandMode::Wander
        }
    );
    assert_eq!(
        cycle(&mut bear),
        CreatureInteractResult::CommandModeChanged {
            new_mode: PetCommandMode::Follow
        }
    );
    assert_eq!(bear.command_mode, PetCommandMode::Follow);
}

#[test]
fn empty_hand_cycles_follow_sit_when_wander_unsupported() {
    let spec = deer_spec();
    let owner = uuid(1);
    let mut deer = tamed_bear(1, owner);
    deer.entity_type = spec.entity_type.clone();

    let r1 = interact_with_item(&mut deer, &spec, &owner, None, &no_tags(), 0.0);
    let r2 = interact_with_item(&mut deer, &spec, &owner, None, &no_tags(), 0.0);
    assert_eq!(
        r1,
        CreatureInteractResult::CommandModeChanged {
            new_mode: PetCommandMode::Sit
        }
    );
    assert_eq!(
        r2,
        CreatureInteractResult::CommandModeChanged {
            new_mode: PetCommandMode::Follow
        }
    );
}

#[test]
fn heal_item_restores_health_clamped_to_max() {
    let spec = bear_spec();
    let owner = uuid(1);
    let mut bear = tamed_bear(1, owner);
    bear.health = 10.0;

    let result = interact_with_item(
        &mut bear,
        &spec,
        &owner,
        Some("anyzoo:meat_chunk"),
        &no_tags(),
        0.0,
    );
    assert_eq!(result, CreatureInteractResult::Healed { healed: 4.0 });
    assert!(result.consumed_item());
    assert_eq!(bear.health, 14.0);

    // Near-max health: heal clamps and reports only the applied delta.
    bear.health = 29.0;
    let result = interact_with_item(
        &mut bear,
        &spec,
        &owner,
        Some("anyzoo:meat_chunk"),
        &no_tags(),
        0.0,
    );
    assert_eq!(result, CreatureInteractResult::Healed { healed: 1.0 });
    assert_eq!(bear.health, 30.0);
}

#[test]
fn full_health_creature_ignores_heal_item() {
    let spec = bear_spec();
    let owner = uuid(1);
    let mut bear = tamed_bear(1, owner);

    let result = interact_with_item(
        &mut bear,
        &spec,
        &owner,
        Some("anyzoo:meat_chunk"),
        &no_tags(),
        0.0,
    );
    assert_eq!(result, CreatureInteractResult::Ignored);
    assert!(!result.consumed_item());
    assert_eq!(bear.health, 30.0);
}

#[test]
fn breed_item_enters_love_only_when_adult_and_off_cooldown() {
    let spec = bear_spec();
    let owner = uuid(1);
    let mut bear = tamed_bear(1, owner);

    let result = interact_with_item(
        &mut bear,
        &spec,
        &owner,
        Some("anyzoo:berries"),
        &no_tags(),
        0.0,
    );
    assert_eq!(result, CreatureInteractResult::LoveModeStarted);
    assert_eq!(bear.in_love_ticks, LOVE_TICKS);

    // Already in love: the item is not consumed again.
    let result = interact_with_item(
        &mut bear,
        &spec,
        &owner,
        Some("anyzoo:berries"),
        &no_tags(),
        0.0,
    );
    assert_eq!(result, CreatureInteractResult::Ignored);

    // Under cooldown: refused without consumption.
    bear.in_love_ticks = 0;
    bear.breed_cooldown_ticks = 100;
    let result = interact_with_item(
        &mut bear,
        &spec,
        &owner,
        Some("anyzoo:berries"),
        &no_tags(),
        0.0,
    );
    assert_eq!(result, CreatureInteractResult::Ignored);
    assert!(!result.consumed_item());
}

#[test]
fn breed_pair_produces_baby_and_cools_down_parents() {
    let spec = bear_spec();
    let owner = uuid(7);
    let mut a = tamed_bear(1, owner);
    let mut b = tamed_bear(2, owner);
    a.in_love_ticks = 600;
    b.in_love_ticks = 300;
    b.pos = [4.0, 64.0, 0.0];

    let baby = try_breed_pair(&mut a, &mut b, &spec).expect("both in love in range");

    assert_eq!(baby.entity_type, "anyzoo:bear");
    assert_eq!(baby.age, -24_000);
    assert_eq!(baby.owner_uuid, Some(owner));
    assert_eq!(baby.pos, [2.0, 64.0, 0.0]);
    assert_eq!(baby.health, 30.0);
    assert_eq!(a.in_love_ticks, 0);
    assert_eq!(b.in_love_ticks, 0);
    assert_eq!(a.breed_cooldown_ticks, 6_000);
    assert_eq!(b.breed_cooldown_ticks, 6_000);
}

#[test]
fn breed_pair_drops_owner_when_parents_have_different_owners() {
    let spec = bear_spec();
    let mut a = tamed_bear(1, uuid(1));
    let mut b = tamed_bear(2, uuid(2));
    a.in_love_ticks = 10;
    b.in_love_ticks = 10;

    let baby = try_breed_pair(&mut a, &mut b, &spec).unwrap();
    assert_eq!(baby.owner_uuid, None);
}

#[test]
fn breed_pair_rejects_every_violation() {
    let spec = bear_spec();
    let base = || {
        let mut a = tamed_bear(1, uuid(1));
        let mut b = tamed_bear(2, uuid(1));
        a.in_love_ticks = 100;
        b.in_love_ticks = 100;
        (a, b)
    };

    // Not in love.
    {
        let (mut a, mut b) = base();
        a.in_love_ticks = 0;
        assert_eq!(try_breed_pair(&mut a, &mut b, &spec), None);
    }
    // Same entity id.
    {
        let (mut a, mut b) = base();
        b.entity_id = a.entity_id;
        assert_eq!(try_breed_pair(&mut a, &mut b, &spec), None);
    }
    // Different entity types.
    {
        let (mut a, mut b) = base();
        b.entity_type = "anyzoo:deer".to_string();
        assert_eq!(try_breed_pair(&mut a, &mut b, &spec), None);
    }
    // Mismatched spec.
    {
        let (mut a, mut b) = base();
        let other = CreatureSpec {
            entity_type: "anyzoo:deer".to_string(),
            ..bear_spec()
        };
        assert_eq!(try_breed_pair(&mut a, &mut b, &other), None);
    }
    // A baby parent cannot breed.
    {
        let (mut a, mut b) = base();
        a.age = -100;
        assert_eq!(try_breed_pair(&mut a, &mut b, &spec), None);
    }
    // Beyond the 8-block breed reach (8.01 blocks away).
    {
        let (mut a, mut b) = base();
        b.pos = [8.01, 64.0, 0.0];
        assert_eq!(try_breed_pair(&mut a, &mut b, &spec), None);
    }
    // Exactly 8 blocks is still in range.
    {
        let (mut a, mut b) = base();
        b.pos = [8.0, 64.0, 0.0];
        assert!(try_breed_pair(&mut a, &mut b, &spec).is_some());
    }
}

#[test]
fn baby_grows_up_and_food_accelerates_growth_by_ten_percent() {
    let spec = bear_spec();
    let owner = uuid(1);
    let mut baby = tamed_bear(9, owner);
    baby.age = -24_000;

    // Natural maturation: one tick closer per lifecycle tick.
    let input = LifecycleInput::default();
    let events = tick_lifecycle(&mut baby, &spec, &input);
    assert_eq!(baby.age, -23_999);
    assert!(!events.grew_up);

    // Feeding the breed item sheds 10% of the remaining time.
    let result = interact_with_item(
        &mut baby,
        &spec,
        &owner,
        Some("anyzoo:berries"),
        &no_tags(),
        0.0,
    );
    assert_eq!(result, CreatureInteractResult::BabyGrowthAccelerated);
    assert!(result.consumed_item());
    assert_eq!(baby.age, -23_999 + 2_399); // 10% of 23999 (integer div)

    // Reaching age 0 announces adulthood.
    baby.age = -1;
    let events = tick_lifecycle(&mut baby, &spec, &input);
    assert_eq!(baby.age, 0);
    assert!(events.grew_up);
}

#[test]
fn love_and_cooldown_timers_tick_down() {
    let spec = bear_spec();
    let mut bear = tamed_bear(1, uuid(1));
    bear.in_love_ticks = 2;
    bear.breed_cooldown_ticks = 1;
    let input = LifecycleInput::default();

    tick_lifecycle(&mut bear, &spec, &input);
    assert_eq!(bear.in_love_ticks, 1);
    assert_eq!(bear.breed_cooldown_ticks, 0);
    tick_lifecycle(&mut bear, &spec, &input);
    assert_eq!(bear.in_love_ticks, 0);
    assert_eq!(bear.breed_cooldown_ticks, 0); // saturates, no underflow
}

#[test]
fn following_pet_teleports_to_owner_beyond_twelve_blocks() {
    let spec = bear_spec();
    let owner = uuid(1);
    let mut bear = tamed_bear(1, owner);
    bear.command_mode = PetCommandMode::Follow;

    // 20 blocks away -> teleport next to the owner.
    let input = LifecycleInput {
        owner_pos: Some([20.0, 64.0, 0.0]),
        in_water: false,
    };
    let events = tick_lifecycle(&mut bear, &spec, &input);
    assert!(events.teleported_to_owner);
    assert_eq!(bear.pos, [21.0, 64.0, 0.0]);

    // Within 12 blocks -> stays put.
    bear.pos = [20.0, 64.0, 0.0];
    let events = tick_lifecycle(&mut bear, &spec, &input);
    assert!(!events.teleported_to_owner);
    assert_eq!(bear.pos, [20.0, 64.0, 0.0]);
}

#[test]
fn sitting_wandering_and_wild_creatures_never_teleport() {
    let spec = bear_spec();
    let owner = uuid(1);
    let input = LifecycleInput {
        owner_pos: Some([100.0, 64.0, 0.0]),
        in_water: false,
    };

    let mut sitter = tamed_bear(1, owner);
    sitter.command_mode = PetCommandMode::Sit;
    assert!(!tick_lifecycle(&mut sitter, &spec, &input).teleported_to_owner);
    assert_eq!(sitter.pos, [0.0, 64.0, 0.0]);

    let mut wanderer = tamed_bear(2, owner);
    wanderer.command_mode = PetCommandMode::Wander;
    assert!(!tick_lifecycle(&mut wanderer, &spec, &input).teleported_to_owner);

    // Untamed creature has no owner to follow even in Follow mode.
    let mut wild = wild_bear(3);
    wild.command_mode = PetCommandMode::Follow;
    assert!(!tick_lifecycle(&mut wild, &spec, &input).teleported_to_owner);

    // No owner position loaded this tick -> no teleport either.
    let mut bear = tamed_bear(4, owner);
    let offline = LifecycleInput {
        owner_pos: None,
        in_water: false,
    };
    assert!(!tick_lifecycle(&mut bear, &spec, &offline).teleported_to_owner);
}

// ===========================================================================
// Movement profiles
// ===========================================================================

const NO_MOVE: [f64; 3] = [0.0, 0.0, 0.0];

#[test]
fn land_profile_walks_at_ground_speed_and_falls_with_gravity() {
    let spec = bear_spec();
    let MovementProfile::Land { ground_speed } = spec.movement else {
        panic!("bear must be Land");
    };
    let pos = [0.0, 64.0, 0.0];

    // Grounded walk: horizontal speed, no vertical motion.
    let (np, nv) = step_creature_movement(
        &spec.movement,
        pos,
        NO_MOVE,
        [1.0, 0.0, 0.0],
        false,
        true,
        false,
    );
    assert_eq!(nv, [ground_speed, 0.0, 0.0]);
    assert_eq!(np, [ground_speed, 64.0, 0.0]);

    // Airborne: gravity integrates and clamps at terminal velocity.
    let (_, nv) =
        step_creature_movement(&spec.movement, pos, NO_MOVE, NO_MOVE, false, false, false);
    assert_eq!(nv[1], -GRAVITY_PER_TICK);
    let (_, nv) = step_creature_movement(
        &spec.movement,
        pos,
        [0.0, -10.0, 0.0],
        NO_MOVE,
        false,
        false,
        false,
    );
    assert_eq!(nv[1], -TERMINAL_FALL_SPEED);

    // Submerged: slow sink, and a positive intent paddles upward.
    let (_, nv) = step_creature_movement(&spec.movement, pos, NO_MOVE, NO_MOVE, true, false, false);
    assert_eq!(nv[1], -WATER_GRAVITY_PER_TICK);
    let (_, nv) = step_creature_movement(
        &spec.movement,
        pos,
        NO_MOVE,
        [0.0, 1.0, 0.0],
        true,
        false,
        false,
    );
    assert_eq!(nv[1], ground_speed * 0.5);
}

#[test]
fn amphibious_profile_swaps_land_and_swim_speed() {
    let spec = alligator_spec();
    let pos = [0.0, 64.0, 0.0];

    // Ashore: slow land speed, ordinary vertical behaviour.
    let (np, nv) = step_creature_movement(
        &spec.movement,
        pos,
        NO_MOVE,
        [1.0, 0.0, 0.0],
        false,
        true,
        false,
    );
    assert_eq!(nv, [0.1, 0.0, 0.0]);
    assert_eq!(np, [0.1, 64.0, 0.0]);

    // Submerged: full 3-axis authority at swim speed, including dives.
    let (_, nv) = step_creature_movement(
        &spec.movement,
        pos,
        NO_MOVE,
        [1.0, -0.5, 0.0],
        true,
        false,
        false,
    );
    assert_eq!(nv, [0.6, -0.3, 0.0]);
}

#[test]
fn aquatic_profile_swims_in_3d_but_is_helpless_on_land() {
    let spec = whale_spec();
    let pos = [0.0, 64.0, 0.0];

    // Submerged: 3-axis swim control.
    let (np, nv) = step_creature_movement(
        &spec.movement,
        pos,
        NO_MOVE,
        [0.5, 0.25, -1.0],
        true,
        false,
        false,
    );
    assert_eq!(nv, [0.4, 0.2, -0.8]);
    assert_eq!(np, [0.4, 64.2, -0.8]);

    // Beached: no control at all — just falls until grounded.
    let (_, nv) = step_creature_movement(
        &spec.movement,
        pos,
        NO_MOVE,
        [1.0, 0.0, 0.0],
        false,
        false,
        false,
    );
    assert_eq!(nv, [0.0, -GRAVITY_PER_TICK, 0.0]);
    let (_, nv) = step_creature_movement(
        &spec.movement,
        pos,
        NO_MOVE,
        [1.0, 0.0, 0.0],
        false,
        true,
        false,
    );
    assert_eq!(nv, NO_MOVE);
}

#[test]
fn aquatic_air_timer_deals_periodic_suffocation_damage() {
    let spec = whale_spec();
    let mut whale = wild_bear(1);
    whale.entity_type = spec.entity_type.clone();
    whale.health = 20.0;

    let dry = LifecycleInput {
        owner_pos: None,
        in_water: false,
    };
    // Budget is 3 ticks: hits land on ticks 3, 6, 9, ...
    assert_eq!(
        tick_lifecycle(&mut whale, &spec, &dry).suffocation_damage,
        0.0
    );
    assert_eq!(
        tick_lifecycle(&mut whale, &spec, &dry).suffocation_damage,
        0.0
    );
    let hit = tick_lifecycle(&mut whale, &spec, &dry);
    assert_eq!(hit.suffocation_damage, AQUATIC_SUFFOCATION_DAMAGE);
    assert_eq!(whale.health, 19.0);
    assert_eq!(
        tick_lifecycle(&mut whale, &spec, &dry).suffocation_damage,
        0.0
    );

    // Re-submerging resets the budget entirely.
    let wet = LifecycleInput {
        owner_pos: None,
        in_water: true,
    };
    tick_lifecycle(&mut whale, &spec, &wet);
    assert_eq!(whale.out_of_water_ticks, 0);
    tick_lifecycle(&mut whale, &spec, &dry);
    tick_lifecycle(&mut whale, &spec, &dry);
    assert_eq!(
        tick_lifecycle(&mut whale, &spec, &dry).suffocation_damage,
        1.0
    );
    assert_eq!(whale.health, 18.0);
}

#[test]
fn non_aquatic_creatures_never_suffocate_in_air() {
    let spec = bear_spec();
    let mut bear = wild_bear(1);
    for _ in 0..20 {
        let events = tick_lifecycle(&mut bear, &spec, &LifecycleInput::default());
        assert_eq!(events.suffocation_damage, 0.0);
    }
    assert_eq!(bear.health, 30.0);
    assert_eq!(bear.out_of_water_ticks, 0);
}

#[test]
fn gliding_flight_walks_glides_and_flaps() {
    let spec = finch_spec();
    let pos = [0.0, 64.0, 0.0];

    // Grounded: ordinary walk speed.
    let (_, nv) = step_creature_movement(
        &spec.movement,
        pos,
        NO_MOVE,
        [1.0, 0.0, 0.0],
        false,
        true,
        false,
    );
    assert_eq!(nv, [0.1, 0.0, 0.0]);

    // Airborne with no climb intent: fly-speed horizontal control, gravity
    // scaled down to 25% -> a gentle 0.02/tick sink.
    let (_, nv) = step_creature_movement(
        &spec.movement,
        pos,
        NO_MOVE,
        [1.0, 0.0, 0.0],
        false,
        false,
        false,
    );
    assert_eq!(nv, [0.6, -0.08 * 0.25, 0.0]);

    // Climb intent flaps upward at fly speed.
    let (_, nv) = step_creature_movement(
        &spec.movement,
        pos,
        NO_MOVE,
        [0.0, 1.0, 0.0],
        false,
        false,
        false,
    );
    assert_eq!(nv[1], 0.6);

    // Terminal glide speed is also scaled: -3.92 * 0.25 = -0.98.
    let (_, nv) = step_creature_movement(
        &spec.movement,
        pos,
        [0.0, -5.0, 0.0],
        NO_MOVE,
        false,
        false,
        false,
    );
    assert_eq!(nv[1], -TERMINAL_FALL_SPEED * 0.25);
}

#[test]
fn wall_climb_ascends_descends_and_clings_to_walls() {
    let spec = gecko_spec();
    let pos = [0.0, 64.0, 0.0];

    // Pressed against a wall with horizontal intent -> climbs upward.
    let (np, nv) = step_creature_movement(
        &spec.movement,
        pos,
        NO_MOVE,
        [1.0, 0.0, 0.0],
        false,
        false,
        true,
    );
    assert_eq!(nv, [0.2, 0.3, 0.0]);
    assert_eq!(np, [0.2, 64.3, 0.0]);

    // Explicit downward intent slides down at climb speed.
    let (_, nv) = step_creature_movement(
        &spec.movement,
        pos,
        NO_MOVE,
        [0.0, -1.0, 0.0],
        false,
        false,
        true,
    );
    assert_eq!(nv[1], -0.3);

    // No intent at all -> clings instead of falling.
    let (_, nv) = step_creature_movement(&spec.movement, pos, NO_MOVE, NO_MOVE, false, false, true);
    assert_eq!(nv[1], 0.0);

    // Off the wall it behaves like a land creature.
    let (_, nv) =
        step_creature_movement(&spec.movement, pos, NO_MOVE, NO_MOVE, false, false, false);
    assert_eq!(nv[1], -GRAVITY_PER_TICK);
}

// ===========================================================================
// DynamicEntityMetadata — 0x58 SetEntityData wire codec
// ===========================================================================

#[test]
fn set_marks_dirty_only_when_value_changes() {
    let mut meta = DynamicEntityMetadata::new();
    assert!(!meta.has_dirty_slots());

    assert!(meta.set(7, MetaValue::Bool(true)));
    assert!(meta.is_dirty(7));
    assert_eq!(meta.dirty_slots().collect::<Vec<_>>(), [7]);

    // Re-setting the same value leaves the dirty set untouched.
    assert!(!meta.set(7, MetaValue::Bool(true)));
    assert_eq!(meta.dirty_slots().collect::<Vec<_>>(), [7]);

    // A real change re-marks the slot.
    assert!(meta.set(7, MetaValue::Bool(false)));

    // Slot 0xFF is the wire terminator and can never be stored.
    assert!(!meta.set(0xFF, MetaValue::Byte(1)));
    assert_eq!(meta.get(0xFF), None);

    // Oversized strings fail closed at insertion time.
    let huge = "x".repeat(MAX_METADATA_STRING_LEN + 1);
    assert!(!meta.set(3, MetaValue::String(huge)));
    assert_eq!(meta.get(3), None);
}

#[test]
fn dirty_delta_encodes_only_changed_slots_in_order_and_terminates() {
    let mut meta = DynamicEntityMetadata::new();
    meta.set(0x11, MetaValue::Byte(0b0110_0011)); // tamed|sitting style flags
    meta.set(0x02, MetaValue::VarInt(1)); // variant ordinal
    meta.set(0x14, MetaValue::Bool(true)); // sleeping

    let bytes = meta.encode_dirty_delta();
    // Ascending slot order: 0x02 (VarInt type 1), 0x11 (Byte type 0),
    // 0x14 (Bool type 8), then the 0xFF terminator.
    assert_eq!(
        bytes,
        vec![
            0x02,
            0x01,
            0x01, // slot 2, type VarInt, value 1
            0x11,
            0x00,
            0b0110_0011, // slot 17, type Byte, flags
            0x14,
            0x08,
            0x01, // slot 20, type Bool, true
            0xFF,
        ]
    );
    assert!(!meta.has_dirty_slots());

    // A second delta with no changes is just the terminator.
    assert_eq!(meta.encode_dirty_delta(), vec![0xFF]);
}

#[test]
fn metadata_wire_roundtrip_covers_every_value_type() {
    let owner = uuid(9);
    let mut meta = DynamicEntityMetadata::new();
    meta.set(0, MetaValue::Byte(-3));
    meta.set(1, MetaValue::VarInt(-70));
    meta.set(2, MetaValue::Float(2.5));
    meta.set(3, MetaValue::String("anyzoo:spotted".to_string()));
    meta.set(4, MetaValue::Bool(true));
    meta.set(5, MetaValue::OptionalUuid(Some(owner)));
    meta.set(6, MetaValue::OptionalUuid(None));
    meta.set(7, MetaValue::Pose(2)); // sleeping pose

    let decoded = decode_metadata_entries(&meta.encode_dirty_delta())
        .expect("own encoder output must decode");
    assert_eq!(decoded.len(), 8);
    assert_eq!(decoded[&0], MetaValue::Byte(-3));
    assert_eq!(decoded[&1], MetaValue::VarInt(-70));
    assert_eq!(decoded[&2], MetaValue::Float(2.5));
    assert_eq!(decoded[&3], MetaValue::String("anyzoo:spotted".to_string()));
    assert_eq!(decoded[&4], MetaValue::Bool(true));
    assert_eq!(decoded[&5], MetaValue::OptionalUuid(Some(owner)));
    assert_eq!(decoded[&6], MetaValue::OptionalUuid(None));
    assert_eq!(decoded[&7], MetaValue::Pose(2));
}

#[test]
fn decoder_fails_closed_on_every_corruption() {
    // Missing terminator.
    assert_eq!(decode_metadata_entries(&[0x02, 0x01, 0x01]), None);
    // Truncated value (VarInt payload missing).
    assert_eq!(decode_metadata_entries(&[0x02, 0x01]), None);
    // Truncated float payload.
    assert_eq!(decode_metadata_entries(&[0x02, 0x03, 0x00, 0x00]), None);
    // Unknown serializer type id (0x63).
    assert_eq!(decode_metadata_entries(&[0x02, 0x63, 0x00, 0xFF]), None);
    // Bool payload other than 0/1.
    assert_eq!(decode_metadata_entries(&[0x02, 0x08, 0x02, 0xFF]), None);
    // Pose ordinal above u8 range (300).
    assert_eq!(
        decode_metadata_entries(&[0x02, 0x15, 0xAC, 0x02, 0xFF]),
        None
    );
    // Declared string length above the 32767 cap (40000).
    assert_eq!(
        decode_metadata_entries(&[0x02, 0x04, 0xC0, 0xB8, 0x02, 0xFF]),
        None
    );
    // Declared string length overruns the buffer.
    assert_eq!(
        decode_metadata_entries(&[0x02, 0x04, 0x05, b'a', 0xFF]),
        None
    );
    // Trailing garbage after the terminator.
    assert_eq!(decode_metadata_entries(&[0xFF, 0x00]), None);
    // Optional-UUID presence byte other than 0/1.
    assert_eq!(decode_metadata_entries(&[0x02, 0x0D, 0x07, 0xFF]), None);
    // Present UUID truncated mid-payload.
    assert_eq!(
        decode_metadata_entries(&[0x02, 0x0D, 0x01, 0x00, 0xFF]),
        None
    );
    // Empty input has no terminator.
    assert_eq!(decode_metadata_entries(&[]), None);
}

#[test]
fn set_entity_data_body_prefixes_packet_and_entity_varints() {
    let mut meta = DynamicEntityMetadata::new();
    meta.set(9, MetaValue::Pose(7)); // sitting pose
    let body = build_set_entity_data_0x58(4882, &mut meta);

    // 0x58 packet id, entity id 4882 as VarInt (0x92 0x26), entry, terminator.
    assert_eq!(body, vec![0x58, 0x92, 0x26, 0x09, 0x15, 0x07, 0xFF]);
    // Decode the entry-stream tail.
    let decoded = decode_metadata_entries(&body[3..]).unwrap();
    assert_eq!(decoded[&9], MetaValue::Pose(7));
}
