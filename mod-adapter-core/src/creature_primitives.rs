// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Generic creature / pet mod primitives — pure `std`, zero new dependencies,
//! 100% declarative, and entirely free of mod-specific code.
//!
//! Three families live here, each driven by data so that **any** creature,
//! animal, or pet mod works out of the box:
//!
//! 1. [`CreatureSpec`]/[`CreatureState`]/[`PetCommandMode`]: a generic tame
//!    & breed state machine. [`interact_with_item`] deterministically
//!    resolves right-click interactions (taming rolls, healing, love mode,
//!    baby-growth acceleration, and the classic `Follow -> Sit -> Wander`
//!    command cycle) while [`try_breed_pair`] and [`tick_lifecycle`] cover
//!    pair breeding, baby growth, breed cooldowns, owner-follow teleports,
//!    and aquatic air suffocation.
//! 2. [`MovementProfile`]: five declarative movement domains (`Land`,
//!    `Amphibious`, `Aquatic`, `GlidingFlight`, `WallClimb`) integrated by
//!    the deterministic [`step_creature_movement`] tick integrator.
//! 3. [`DynamicEntityMetadata`]: a typed, dirty-tracked entity metadata
//!    table with an exact Protocol 767 `0x58` `SetEntityData` dirty-delta
//!    codec so any mod mob can sync animation ids, variants, poses, and
//!    tamed/sitting bitflags without a line of mod-specific code.
//!
//! Clean-room note: every rule here is an original declarative design
//! mirroring vanilla's *contracts* (24000-tick baby growth, 600-tick love
//! mode, 10%-of-remaining feeding acceleration, 8-block breed reach,
//! 12-block follow teleport) — no mod code, constants, or data tables are
//! ported.

use std::collections::{BTreeMap, BTreeSet};

use crate::neoforge_wire::{read_varint, write_varint};

// ===========================================================================
// 1. Tamable & breedable state machine
// ===========================================================================

/// The classic three-mode right-click command cycle used across pet and
/// creature mods.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PetCommandMode {
    /// Follow the owner; teleport close when separated by more than
    /// [`OWNER_TELEPORT_DISTANCE`] blocks.
    Follow,
    /// Sit in place; ignore owner movement entirely.
    Sit,
    /// Wander freely without following (only reachable when the spec sets
    /// `supports_wander_mode`).
    Wander,
}

/// Vanilla love-mode duration in ticks (`AgeableMob` uses 600).
pub const LOVE_TICKS: u32 = 600;

/// Vanilla baby maturation time in ticks (20 minutes).
pub const DEFAULT_BABY_GROWTH_TICKS: u32 = 24_000;

/// Vanilla post-breeding parent cooldown in ticks (5 minutes).
pub const DEFAULT_BREED_COOLDOWN_TICKS: u32 = 6_000;

/// Maximum horizontal distance (squared) between two parents for
/// [`try_breed_pair`] to succeed — 8 blocks, matching vanilla's breed reach.
pub const BREED_RANGE_SQ: f64 = 8.0 * 8.0;

/// Separation (squared) beyond which a `Follow`ing pet teleports to its
/// owner — 12 blocks.
pub const OWNER_TELEPORT_DISTANCE_SQ: f64 = 12.0 * 12.0;

/// How far along +X a teleporting pet is placed relative to its owner so it
/// lands beside rather than inside the player.
pub const OWNER_TELEPORT_OFFSET: [f64; 3] = [1.0, 0.0, 0.0];

/// Suffocation damage applied each time an aquatic creature exhausts its
/// `suffocates_in_air_ticks` budget.
pub const AQUATIC_SUFFOCATION_DAMAGE: f32 = 1.0;

/// Declarative description of one creature type's tame/breed/movement
/// contract. Profiles are plain data — loaders may build them from JSON
/// manifests with no Rust changes per mod.
///
/// Item lists accept both `"namespace:item"` literals and `"#namespace:tag"`
/// tag references, resolved against the caller-supplied tag set of the held
/// item.
#[derive(Clone, Debug, PartialEq)]
pub struct CreatureSpec {
    /// Entity type id (e.g. `"anyzoo:bear"`). Babies spawned by
    /// [`try_breed_pair`] inherit this id, and parents must match it.
    pub entity_type: String,
    /// Maximum health; also the clamp applied to healing.
    pub max_health: f32,
    /// Items (or `#tags`) that trigger a taming roll on an untamed creature.
    pub tame_items: Vec<String>,
    /// Probability in `[0, 1]` that one taming item use succeeds.
    pub tame_chance: f32,
    /// Items (or `#tags`) that put a ready adult into love mode and
    /// accelerate a baby's growth.
    pub breed_items: Vec<String>,
    /// `(item_or_tag, heal_amount)` entries usable on a hurt tamed creature.
    pub heal_items: Vec<(String, f32)>,
    /// Ticks a baby takes to reach adulthood (default
    /// [`DEFAULT_BABY_GROWTH_TICKS`]).
    pub baby_growth_ticks: u32,
    /// Ticks parents must wait before breeding again (default
    /// [`DEFAULT_BREED_COOLDOWN_TICKS`]).
    pub breed_cooldown_ticks: u32,
    /// Whether the command cycle includes `Wander` (`Follow -> Sit -> Wander`
    /// when true, `Follow <-> Sit` when false).
    pub supports_wander_mode: bool,
    /// Informational flag for birds/perch pets that can ride a shoulder;
    /// carried through for profile consumers, not enforced here.
    pub can_shoulder_perch: bool,
    /// Informational flag marking the creature mountable once tamed.
    pub rideable_when_tamed: bool,
    /// Movement domain used by [`step_creature_movement`] and air rules used
    /// by [`tick_lifecycle`].
    pub movement: MovementProfile,
}

/// Per-entity runtime state for the tame/breed/follow lifecycle.
///
/// `age` follows the vanilla convention: negative while a baby (counting up
/// to 0 at adulthood), `0` for an adult.
#[derive(Clone, Debug, PartialEq)]
pub struct CreatureState {
    /// Unique runtime id; breeding requires distinct ids.
    pub entity_id: u64,
    /// Entity type id; must equal the partner's and the spec's to breed.
    pub entity_type: String,
    /// Taming owner's UUID (RFC 4122 byte form), `None` while wild.
    pub owner_uuid: Option<[u8; 16]>,
    /// Current command mode; forced to [`PetCommandMode::Follow`] on tame.
    pub command_mode: PetCommandMode,
    /// Current health, clamped to `spec.max_health` by healing.
    pub health: f32,
    /// Vanilla-style age: `< 0` counts up toward adulthood at `0`.
    pub age: i32,
    /// Remaining love-mode ticks (`> 0` while seeking a partner).
    pub in_love_ticks: u32,
    /// Remaining post-breeding cooldown ticks.
    pub breed_cooldown_ticks: u32,
    /// Current position, used for breed reach and follow teleports.
    pub pos: [f64; 3],
    /// Ticks spent out of water while `Aquatic`; reset on re-submersion.
    pub out_of_water_ticks: u32,
}

impl CreatureState {
    /// A fresh wild adult of `entity_type` at `pos`.
    pub fn new(entity_id: u64, entity_type: impl Into<String>, health: f32, pos: [f64; 3]) -> Self {
        Self {
            entity_id,
            entity_type: entity_type.into(),
            owner_uuid: None,
            command_mode: PetCommandMode::Wander,
            health,
            age: 0,
            in_love_ticks: 0,
            breed_cooldown_ticks: 0,
            pos,
            out_of_water_ticks: 0,
        }
    }

    /// `true` while `age < 0`.
    pub fn is_baby(&self) -> bool {
        self.age < 0
    }

    /// `true` once any owner is recorded.
    pub fn is_tamed(&self) -> bool {
        self.owner_uuid.is_some()
    }

    /// `true` when `uuid` is the recorded owner.
    pub fn owned_by(&self, uuid: &[u8; 16]) -> bool {
        self.owner_uuid.as_ref() == Some(uuid)
    }
}

/// One list entry: `#namespace:tag` matches when `held_item_tags` contains
/// `namespace:tag`; anything else requires exact string equality.
fn entry_matches(entry: &str, held_item: &str, held_item_tags: &BTreeSet<String>) -> bool {
    match entry.strip_prefix('#') {
        Some(tag) => held_item_tags.contains(tag),
        None => entry == held_item,
    }
}

/// `true` when any entry in `list` matches the held item or one of its tags.
fn list_matches(list: &[String], held_item: &str, held_item_tags: &BTreeSet<String>) -> bool {
    list.iter()
        .any(|entry| entry_matches(entry, held_item, held_item_tags))
}

/// Outcome of [`interact_with_item`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CreatureInteractResult {
    /// Nothing matched — no item consumed, no state changed.
    Ignored,
    /// The creature is tamed but `actor_uuid` is not the owner: ownership
    /// is never stolen and command mode is never toggled.
    NotOwner,
    /// A tame item was consumed; `succeeded` reports the `rng_roll` verdict.
    TameAttempt {
        /// `rng_roll < tame_chance`: ownership was set and mode is `Follow`.
        succeeded: bool,
    },
    /// A heal item was consumed on a hurt creature.
    Healed {
        /// Health actually restored (clamped to `max_health`).
        healed: f32,
    },
    /// A breed item was consumed; the creature is in love for
    /// [`LOVE_TICKS`].
    LoveModeStarted,
    /// A breed item was consumed by a baby; its remaining growth shrank by
    /// 10% of the remaining time (vanilla `AgeableMob.ageUp` behaviour).
    BabyGrowthAccelerated,
    /// An empty-hand owner click advanced the command cycle.
    CommandModeChanged {
        /// Mode now active.
        new_mode: PetCommandMode,
    },
}

impl CreatureInteractResult {
    /// Whether the interaction consumed one of the held item.
    pub fn consumed_item(&self) -> bool {
        matches!(
            self,
            Self::TameAttempt { .. }
                | Self::Healed { .. }
                | Self::LoveModeStarted
                | Self::BabyGrowthAccelerated
        )
    }
}

/// Deterministic right-click resolution shared by every pet mod:
///
/// * **Untamed** + matching `tame_items` — consumes 1 item, succeeds when
///   `rng_roll < spec.tame_chance`; on success the actor becomes owner and
///   the mode is forced to `Follow`.
/// * **Owner** + matching `heal_items` while `health < max_health` — heals
///   and consumes 1 item (checked before breed items, so a hurt creature is
///   patched up before it can breed, mirroring vanilla priority).
/// * **Owner** + matching `breed_items` — a baby consumes the item to shed
///   10% of its remaining growth; a ready adult (`age >= 0`, no love
///   active, cooldown elapsed) consumes it and enters love mode.
/// * **Owner** + empty hand — cycles `Follow -> Sit -> Wander -> Follow`,
///   or `Follow <-> Sit` when `!spec.supports_wander_mode`.
/// * **Non-owner** — always [`CreatureInteractResult::NotOwner`]: nothing
///   is consumed, ownership and command mode are never touched.
///
/// `held_item` is `None` for an empty hand; `held_item_tags` is the set of
/// item-tag ids (`"namespace:tag"`) the held item belongs to.
pub fn interact_with_item(
    state: &mut CreatureState,
    spec: &CreatureSpec,
    actor_uuid: &[u8; 16],
    held_item: Option<&str>,
    held_item_tags: &BTreeSet<String>,
    rng_roll: f32,
) -> CreatureInteractResult {
    let Some(owner) = state.owner_uuid else {
        // Wild creature: only a matching tame item does anything.
        if let Some(item) = held_item {
            if list_matches(&spec.tame_items, item, held_item_tags) {
                let succeeded = rng_roll < spec.tame_chance;
                if succeeded {
                    state.owner_uuid = Some(*actor_uuid);
                    state.command_mode = PetCommandMode::Follow;
                }
                return CreatureInteractResult::TameAttempt { succeeded };
            }
        }
        return CreatureInteractResult::Ignored;
    };

    if owner != *actor_uuid {
        return CreatureInteractResult::NotOwner;
    }

    let Some(item) = held_item else {
        let new_mode = match state.command_mode {
            PetCommandMode::Follow => PetCommandMode::Sit,
            PetCommandMode::Sit if spec.supports_wander_mode => PetCommandMode::Wander,
            PetCommandMode::Sit | PetCommandMode::Wander => PetCommandMode::Follow,
        };
        state.command_mode = new_mode;
        return CreatureInteractResult::CommandModeChanged { new_mode };
    };

    // A hurt creature is healed before it can breed, matching vanilla's
    // food-heals-injured-pet priority.
    if state.health < spec.max_health {
        if let Some((_, amount)) = spec
            .heal_items
            .iter()
            .find(|(entry, _)| entry_matches(entry, item, held_item_tags))
        {
            let healed = amount.min(spec.max_health - state.health);
            state.health += healed;
            return CreatureInteractResult::Healed { healed };
        }
    }

    if list_matches(&spec.breed_items, item, held_item_tags) {
        if state.is_baby() {
            let boost = (-state.age / 10).max(1);
            state.age = (state.age + boost).min(0);
            return CreatureInteractResult::BabyGrowthAccelerated;
        }
        if state.in_love_ticks == 0 && state.breed_cooldown_ticks == 0 {
            state.in_love_ticks = LOVE_TICKS;
            return CreatureInteractResult::LoveModeStarted;
        }
    }

    CreatureInteractResult::Ignored
}

/// Everything needed to spawn the baby produced by [`try_breed_pair`].
#[derive(Clone, Debug, PartialEq)]
pub struct BabySpawnSpec {
    /// Inherited entity type id.
    pub entity_type: String,
    /// Preserved when both parents share one owner; `None` otherwise.
    pub owner_uuid: Option<[u8; 16]>,
    /// Initial vanilla-style age: `-baby_growth_ticks`.
    pub age: i32,
    /// Midpoint between the parents.
    pub pos: [f64; 3],
    /// Baby's health, equal to the spec's `max_health`.
    pub health: f32,
}

/// Attempt to breed `a` with `b` under `spec`.
///
/// Both parents must be adults (`age >= 0`) in love (`in_love_ticks > 0`),
/// have distinct `entity_id`s, share `spec.entity_type`, and stand within 8
/// blocks of each other. On success both parents leave love mode and start
/// `spec.breed_cooldown_ticks`; the baby keeps the owner only when both
/// parents are owned by the same player.
pub fn try_breed_pair(
    a: &mut CreatureState,
    b: &mut CreatureState,
    spec: &CreatureSpec,
) -> Option<BabySpawnSpec> {
    let breedable = a.entity_id != b.entity_id
        && a.entity_type == spec.entity_type
        && b.entity_type == spec.entity_type
        && a.in_love_ticks > 0
        && b.in_love_ticks > 0
        && !a.is_baby()
        && !b.is_baby()
        && dist_sq(a.pos, b.pos) <= BREED_RANGE_SQ;
    if !breedable {
        return None;
    }

    a.in_love_ticks = 0;
    b.in_love_ticks = 0;
    a.breed_cooldown_ticks = spec.breed_cooldown_ticks;
    b.breed_cooldown_ticks = spec.breed_cooldown_ticks;

    let owner_uuid = match (a.owner_uuid, b.owner_uuid) {
        (Some(oa), Some(ob)) if oa == ob => Some(oa),
        _ => None,
    };
    Some(BabySpawnSpec {
        entity_type: spec.entity_type.clone(),
        owner_uuid,
        age: -(spec.baby_growth_ticks as i32),
        pos: [
            (a.pos[0] + b.pos[0]) * 0.5,
            (a.pos[1] + b.pos[1]) * 0.5,
            (a.pos[2] + b.pos[2]) * 0.5,
        ],
        health: spec.max_health,
    })
}

/// Per-tick environmental inputs to [`tick_lifecycle`].
#[derive(Clone, Copy, Debug, Default)]
pub struct LifecycleInput {
    /// Owner's position when the owner is loaded in the same dimension;
    /// `None` disables follow teleports this tick.
    pub owner_pos: Option<[f64; 3]>,
    /// Whether the creature's head is currently submerged.
    pub in_water: bool,
}

/// Events produced by one [`tick_lifecycle`] call.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LifecycleEvents {
    /// The creature teleported to its owner this tick.
    pub teleported_to_owner: bool,
    /// Suffocation damage dealt this tick (aquatic profiles only).
    pub suffocation_damage: f32,
    /// `age` reached `0` this tick — the baby just became an adult.
    pub grew_up: bool,
}

/// Advance one creature one tick: baby maturation, love-mode and breed
/// cooldown countdowns, aquatic air suffocation, and the `Follow`-mode
/// owner teleport.
///
/// * `age < 0` increments toward `0`; reaching `0` sets
///   [`LifecycleEvents::grew_up`].
/// * `in_love_ticks` and `breed_cooldown_ticks` saturate-decrement.
/// * `Aquatic` profiles accumulate `out_of_water_ticks` while dry; each
///   full `suffocates_in_air_ticks` budget deals
///   [`AQUATIC_SUFFOCATION_DAMAGE`] and restarts the timer.
/// * A `Follow`ing owned creature more than 12 blocks from `owner_pos`
///   teleports to `owner_pos + `[`OWNER_TELEPORT_OFFSET`]; `Sit` and
///   `Wander` never teleport.
pub fn tick_lifecycle(
    state: &mut CreatureState,
    spec: &CreatureSpec,
    input: &LifecycleInput,
) -> LifecycleEvents {
    let mut events = LifecycleEvents::default();

    if state.age < 0 {
        state.age += 1;
        events.grew_up = state.age == 0;
    }
    state.in_love_ticks = state.in_love_ticks.saturating_sub(1);
    state.breed_cooldown_ticks = state.breed_cooldown_ticks.saturating_sub(1);

    if let MovementProfile::Aquatic {
        suffocates_in_air_ticks,
        ..
    } = spec.movement
    {
        if input.in_water {
            state.out_of_water_ticks = 0;
        } else {
            state.out_of_water_ticks += 1;
            if state.out_of_water_ticks >= suffocates_in_air_ticks {
                state.out_of_water_ticks = 0;
                state.health = (state.health - AQUATIC_SUFFOCATION_DAMAGE).max(0.0);
                events.suffocation_damage = AQUATIC_SUFFOCATION_DAMAGE;
            }
        }
    } else {
        state.out_of_water_ticks = 0;
    }

    if state.command_mode == PetCommandMode::Follow && state.is_tamed() {
        if let Some(owner_pos) = input.owner_pos {
            if dist_sq(state.pos, owner_pos) > OWNER_TELEPORT_DISTANCE_SQ {
                state.pos = [
                    owner_pos[0] + OWNER_TELEPORT_OFFSET[0],
                    owner_pos[1] + OWNER_TELEPORT_OFFSET[1],
                    owner_pos[2] + OWNER_TELEPORT_OFFSET[2],
                ];
                events.teleported_to_owner = true;
            }
        }
    }

    events
}

fn dist_sq(a: [f64; 3], b: [f64; 3]) -> f64 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    dx * dx + dy * dy + dz * dz
}

// ===========================================================================
// 2. Movement profiles
// ===========================================================================

/// Per-tick gravity acceleration applied to airborne creatures (blocks per
/// tick squared, matching vanilla's `0.08`).
pub const GRAVITY_PER_TICK: f64 = 0.08;

/// Terminal downward speed for non-gliding creatures (vanilla `~3.92`).
pub const TERMINAL_FALL_SPEED: f64 = 3.92;

/// Reduced gravity used while submerged (blocks per tick squared).
pub const WATER_GRAVITY_PER_TICK: f64 = 0.02;

/// Downward speed cap for submerged non-swimmers.
pub const WATER_SINK_SPEED: f64 = 0.5;

/// Declarative movement domain of a [`CreatureSpec`]. Each variant carries
/// the speeds [`step_creature_movement`] integrates per tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MovementProfile {
    /// Ordinary terrestrial locomotion at `ground_speed`.
    Land {
        /// Horizontal control speed on solid ground (blocks/tick).
        ground_speed: f64,
    },
    /// Split-domain locomotion: `land_speed` dry, `swim_speed` submerged.
    Amphibious {
        /// Horizontal speed on land.
        land_speed: f64,
        /// Full 3-axis speed in water.
        swim_speed: f64,
    },
    /// Water-only locomotion; helpless and suffocating in air.
    Aquatic {
        /// Full 3-axis speed in water.
        swim_speed: f64,
        /// Out-of-water tick budget between
        /// [`AQUATIC_SUFFOCATION_DAMAGE`] hits (enforced by
        /// [`tick_lifecycle`]).
        suffocates_in_air_ticks: u32,
    },
    /// Flying/gliding locomotion: full fly control off the ground, ordinary
    /// walk speed while grounded, gravity scaled by `glide_fall_scale`
    /// whenever the creature is not actively climbing.
    GlidingFlight {
        /// Horizontal speed while on the ground.
        ground_speed: f64,
        /// Horizontal and climb speed while airborne.
        fly_speed: f64,
        /// Gravity multiplier while gliding (`1.0` = normal fall, `0.0` =
        /// hover, typical birds `0.1..=0.4`).
        glide_fall_scale: f64,
    },
    /// Terrestrial locomotion that converts wall contact into vertical
    /// climb speed.
    WallClimb {
        /// Horizontal speed on the ground.
        ground_speed: f64,
        /// Upward speed while pressed against a wall.
        climb_speed: f64,
    },
}

/// Advance one creature's motion by one tick under its [`MovementProfile`].
///
/// `desired_dir` is the controller's intent: its `x`/`z` components scale
/// the horizontal speed directly and its `y` component expresses climb /
/// dive / flap intent. The integrator is deterministic and mirrors
/// vanilla's domain split:
///
/// * `Land` — horizontal control at `ground_speed`; ordinary gravity in
///   air, slowed sinking while submerged (a `desired_dir.y > 0` input
///   paddles upward).
/// * `Amphibious` — `land_speed` rules dry ground exactly like `Land`;
///   while `in_water` the creature has full 3-axis authority at
///   `swim_speed`.
/// * `Aquatic` — full 3-axis `swim_speed` while submerged; out of water it
///   has no horizontal control and simply falls.
/// * `GlidingFlight` — `ground_speed` walk while `on_ground`; otherwise
///   `fly_speed` horizontal control, `desired_dir.y > 0` flaps upward, and
///   otherwise gravity is damped by `glide_fall_scale`.
/// * `WallClimb` — walks like `Land`, but while `against_wall` any upward
///   or horizontal intent produces `climb_speed` ascent, a downward intent
///   produces a controlled descent, and no intent clings in place.
///
/// Returns `(new_pos, new_vel)` where `new_pos = pos + new_vel`.
pub fn step_creature_movement(
    profile: &MovementProfile,
    pos: [f64; 3],
    vel: [f64; 3],
    desired_dir: [f64; 3],
    in_water: bool,
    on_ground: bool,
    against_wall: bool,
) -> ([f64; 3], [f64; 3]) {
    let horizontal_intent = desired_dir[0] != 0.0 || desired_dir[2] != 0.0;
    let new_vel = match *profile {
        MovementProfile::Land { ground_speed } => [
            desired_dir[0] * ground_speed,
            land_vertical(vel[1], desired_dir[1], ground_speed, in_water, on_ground),
            desired_dir[2] * ground_speed,
        ],
        MovementProfile::Amphibious {
            land_speed,
            swim_speed,
        } => {
            if in_water {
                [
                    desired_dir[0] * swim_speed,
                    desired_dir[1] * swim_speed,
                    desired_dir[2] * swim_speed,
                ]
            } else {
                [
                    desired_dir[0] * land_speed,
                    land_vertical(vel[1], desired_dir[1], land_speed, in_water, on_ground),
                    desired_dir[2] * land_speed,
                ]
            }
        }
        MovementProfile::Aquatic { swim_speed, .. } => {
            if in_water {
                [
                    desired_dir[0] * swim_speed,
                    desired_dir[1] * swim_speed,
                    desired_dir[2] * swim_speed,
                ]
            } else if on_ground {
                [0.0, 0.0, 0.0]
            } else {
                [
                    0.0,
                    (vel[1] - GRAVITY_PER_TICK).max(-TERMINAL_FALL_SPEED),
                    0.0,
                ]
            }
        }
        MovementProfile::GlidingFlight {
            ground_speed,
            fly_speed,
            glide_fall_scale,
        } => {
            if on_ground {
                [
                    desired_dir[0] * ground_speed,
                    0.0,
                    desired_dir[2] * ground_speed,
                ]
            } else {
                let vy = if desired_dir[1] > 0.0 {
                    desired_dir[1] * fly_speed
                } else {
                    let sink = if in_water {
                        WATER_GRAVITY_PER_TICK
                    } else {
                        GRAVITY_PER_TICK
                    } * glide_fall_scale;
                    (vel[1] - sink).max(-TERMINAL_FALL_SPEED * glide_fall_scale)
                };
                [desired_dir[0] * fly_speed, vy, desired_dir[2] * fly_speed]
            }
        }
        MovementProfile::WallClimb {
            ground_speed,
            climb_speed,
        } => {
            let vy = if against_wall {
                if desired_dir[1] < 0.0 {
                    desired_dir[1] * climb_speed
                } else if desired_dir[1] > 0.0 || horizontal_intent {
                    climb_speed
                } else {
                    0.0
                }
            } else {
                land_vertical(vel[1], desired_dir[1], ground_speed, in_water, on_ground)
            };
            [
                desired_dir[0] * ground_speed,
                vy,
                desired_dir[2] * ground_speed,
            ]
        }
    };
    (
        [
            pos[0] + new_vel[0],
            pos[1] + new_vel[1],
            pos[2] + new_vel[2],
        ],
        new_vel,
    )
}

/// Shared vertical integrator for ground-bound profiles: zero velocity on
/// solid ground, gravity-limited fall in air, capped sinking in water with
/// a `desired_y > 0` paddle.
fn land_vertical(vel_y: f64, desired_y: f64, speed: f64, in_water: bool, on_ground: bool) -> f64 {
    if in_water {
        if desired_y > 0.0 {
            desired_y * speed * 0.5
        } else {
            (vel_y - WATER_GRAVITY_PER_TICK).max(-WATER_SINK_SPEED)
        }
    } else if on_ground {
        0.0
    } else {
        (vel_y - GRAVITY_PER_TICK).max(-TERMINAL_FALL_SPEED)
    }
}

// ===========================================================================
// 3. Dynamic entity metadata (Protocol 767 `0x58` SetEntityData)
// ===========================================================================

/// Clientbound Play `SetEntityData` packet id for protocol 767 (1.21.1).
pub const SET_ENTITY_DATA_PACKET_ID_767: u8 = 0x58;

/// Entry-stream terminator byte appended after the last metadata entry.
pub const METADATA_TERMINATOR: u8 = 0xFF;

/// Highest usable slot index — `0xFF` is the wire terminator.
pub const METADATA_MAX_SLOT: u8 = 0xFE;

/// Maximum UTF-8 length a `MetaValue::String` may carry, matching the
/// vanilla `FriendlyByteBuf` cap.
pub const MAX_METADATA_STRING_LEN: usize = 32_767;

// Wire type ids for protocol 767 entity metadata serializers.
const META_TYPE_BYTE: u8 = 0;
const META_TYPE_VARINT: u8 = 1;
const META_TYPE_FLOAT: u8 = 3;
const META_TYPE_STRING: u8 = 4;
const META_TYPE_BOOL: u8 = 8;
const META_TYPE_OPTIONAL_UUID: u8 = 13;
const META_TYPE_POSE: u8 = 21;

/// A typed entity-metadata value covering the serializers creature mods
/// actually need: flag bytes, counters, animation id floats, variant name
/// strings, booleans, owner UUIDs, and entity poses.
#[derive(Clone, Debug, PartialEq)]
pub enum MetaValue {
    /// Serializer `0` — one signed byte (tamed/sitting/bitflag masks).
    Byte(i8),
    /// Serializer `1` — VarInt (variant ordinals, counters).
    VarInt(i32),
    /// Serializer `3` — big-endian `f32` (animation phase, scale).
    Float(f32),
    /// Serializer `4` — VarInt-length UTF-8 (variant names, custom keys).
    String(String),
    /// Serializer `8` — one byte `0`/`1`.
    Bool(bool),
    /// Serializer `13` — presence byte + optional 16-byte UUID (owner id).
    OptionalUuid(Option<[u8; 16]>),
    /// Serializer `21` — VarInt pose ordinal (`0` = standing, `2` =
    /// sleeping, `7` = sitting, ...).
    Pose(u8),
}

impl MetaValue {
    fn type_id(&self) -> u8 {
        match self {
            Self::Byte(_) => META_TYPE_BYTE,
            Self::VarInt(_) => META_TYPE_VARINT,
            Self::Float(_) => META_TYPE_FLOAT,
            Self::String(_) => META_TYPE_STRING,
            Self::Bool(_) => META_TYPE_BOOL,
            Self::OptionalUuid(_) => META_TYPE_OPTIONAL_UUID,
            Self::Pose(_) => META_TYPE_POSE,
        }
    }

    fn encode_value(&self, out: &mut Vec<u8>) {
        match self {
            Self::Byte(v) => out.push(*v as u8),
            Self::VarInt(v) => write_varint(out, *v as u32),
            Self::Float(v) => out.extend_from_slice(&v.to_bits().to_be_bytes()),
            Self::String(s) => {
                write_varint(out, s.len() as u32);
                out.extend_from_slice(s.as_bytes());
            }
            Self::Bool(v) => out.push(u8::from(*v)),
            Self::OptionalUuid(uuid) => match uuid {
                Some(bytes) => {
                    out.push(1);
                    out.extend_from_slice(bytes);
                }
                None => out.push(0),
            },
            Self::Pose(v) => write_varint(out, u32::from(*v)),
        }
    }

    fn decode_value(type_id: u8, bytes: &[u8], cursor: &mut usize) -> Option<Self> {
        match type_id {
            META_TYPE_BYTE => {
                let v = *bytes.get(*cursor)? as i8;
                *cursor += 1;
                Some(Self::Byte(v))
            }
            META_TYPE_VARINT => Some(Self::VarInt(read_varint(bytes, cursor)? as i32)),
            META_TYPE_FLOAT => {
                let bits = u32::from_be_bytes(bytes.get(*cursor..*cursor + 4)?.try_into().ok()?);
                *cursor += 4;
                Some(Self::Float(f32::from_bits(bits)))
            }
            META_TYPE_STRING => {
                let len = read_varint(bytes, cursor)? as usize;
                if len > MAX_METADATA_STRING_LEN {
                    return None;
                }
                let slice = bytes.get(*cursor..*cursor + len)?;
                *cursor += len;
                std::str::from_utf8(slice)
                    .ok()
                    .map(|s| Self::String(s.to_string()))
            }
            META_TYPE_BOOL => match *bytes.get(*cursor)? {
                0 => {
                    *cursor += 1;
                    Some(Self::Bool(false))
                }
                1 => {
                    *cursor += 1;
                    Some(Self::Bool(true))
                }
                _ => None,
            },
            META_TYPE_OPTIONAL_UUID => match *bytes.get(*cursor)? {
                0 => {
                    *cursor += 1;
                    Some(Self::OptionalUuid(None))
                }
                1 => {
                    let uuid: [u8; 16] = bytes.get(*cursor + 1..*cursor + 17)?.try_into().ok()?;
                    *cursor += 17;
                    Some(Self::OptionalUuid(Some(uuid)))
                }
                _ => None,
            },
            META_TYPE_POSE => {
                let v = read_varint(bytes, cursor)?;
                u8::try_from(v).ok().map(Self::Pose)
            }
            _ => None,
        }
    }
}

/// Typed entity-metadata table with dirty tracking.
///
/// Slots `0..=`[`METADATA_MAX_SLOT`] hold one [`MetaValue`] each;
/// [`set`](Self::set) marks a slot dirty only when the value actually
/// changes, so [`encode_dirty_delta`](Self::encode_dirty_delta) emits the
/// minimal `0x58` delta.
#[derive(Clone, Debug, Default)]
pub struct DynamicEntityMetadata {
    slots: BTreeMap<u8, MetaValue>,
    dirty: BTreeSet<u8>,
}

impl DynamicEntityMetadata {
    /// An empty, clean table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Current value at `slot`, if any.
    pub fn get(&self, slot: u8) -> Option<&MetaValue> {
        self.slots.get(&slot)
    }

    /// Store `value` at `slot`, marking it dirty only when it differs from
    /// the current value.
    ///
    /// Returns `false` (no store, no dirty mark) when `slot` is the
    /// reserved `0xFF` terminator, when an oversized `String` would exceed
    /// [`MAX_METADATA_STRING_LEN`], or when the slot already holds an equal
    /// value.
    pub fn set(&mut self, slot: u8, value: MetaValue) -> bool {
        if slot > METADATA_MAX_SLOT {
            return false;
        }
        if let MetaValue::String(s) = &value {
            if s.len() > MAX_METADATA_STRING_LEN {
                return false;
            }
        }
        if self.slots.get(&slot) == Some(&value) {
            return false;
        }
        self.slots.insert(slot, value);
        self.dirty.insert(slot);
        true
    }

    /// `true` when at least one slot has unsent changes.
    pub fn has_dirty_slots(&self) -> bool {
        !self.dirty.is_empty()
    }

    /// `true` when `slot` has an unsent change.
    pub fn is_dirty(&self, slot: u8) -> bool {
        self.dirty.contains(&slot)
    }

    /// Dirty slot indices in ascending order.
    pub fn dirty_slots(&self) -> impl Iterator<Item = u8> + '_ {
        self.dirty.iter().copied()
    }

    /// Forget every dirty mark without emitting a packet.
    pub fn clear_dirty(&mut self) {
        self.dirty.clear();
    }

    /// Encode the dirty entries as a Protocol 767 `0x58` entry stream —
    /// `index | type | value` tuples in ascending slot order followed by
    /// the `0xFF` terminator — and clear the dirty set.
    pub fn encode_dirty_delta(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        for slot in self.dirty.iter().copied() {
            if let Some(value) = self.slots.get(&slot) {
                out.push(slot);
                write_varint(&mut out, u32::from(value.type_id()));
                value.encode_value(&mut out);
            }
        }
        out.push(METADATA_TERMINATOR);
        self.dirty.clear();
        out
    }
}

/// Build a complete clientbound `0x58` `SetEntityData` body for protocol
/// 767: `VarInt(packet_id) | VarInt(entity_id) | dirty entries | 0xFF`.
/// Clears `meta`'s dirty set.
pub fn build_set_entity_data_0x58(entity_id: i32, meta: &mut DynamicEntityMetadata) -> Vec<u8> {
    let mut out = Vec::new();
    write_varint(&mut out, u32::from(SET_ENTITY_DATA_PACKET_ID_767));
    write_varint(&mut out, entity_id as u32);
    out.extend_from_slice(&meta.encode_dirty_delta());
    out
}

/// Decode a Protocol 767 `0x58` entry stream (`index | type | value`
/// tuples terminated by `0xFF`) into a slot table.
///
/// Fail-closed: returns `None` on a missing terminator, truncation,
/// unknown type id, oversized or non-UTF-8 string, out-of-range pose, or
/// trailing bytes after the terminator.
pub fn decode_metadata_entries(bytes: &[u8]) -> Option<BTreeMap<u8, MetaValue>> {
    let mut cursor = 0usize;
    let mut slots = BTreeMap::new();
    loop {
        let index = *bytes.get(cursor)?;
        cursor += 1;
        if index == METADATA_TERMINATOR {
            // Strict frame: the terminator must be the last byte.
            return (cursor == bytes.len()).then_some(slots);
        }
        let type_id = u8::try_from(read_varint(bytes, &mut cursor)?).ok()?;
        let value = MetaValue::decode_value(type_id, bytes, &mut cursor)?;
        slots.insert(index, value);
    }
}
