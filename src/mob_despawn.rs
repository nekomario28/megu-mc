// Runtime state for naturally spawned monsters in the Overworld and Nether.
// See the pinned 1.21.1 server's Mob.checkDespawn, ServerLevel.tick and Monster.updateNoActionTime.

use std::cell::{OnceCell, RefCell};
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::Weak;

/// Entity persistence causes independent of the nearest-player distance.
/// The live Mob type is shared by other lanes, so these flags live beside it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct MobPersistenceFlags {
    custom_named: bool,
    leashed: bool,
    riding_or_ridden: bool,
    persistence_required: bool,
    tamed: bool,
}

fn should_exempt_from_despawn(flags: MobPersistenceFlags) -> bool {
    flags.custom_named
        || flags.leashed
        || flags.riding_or_ridden
        || flags.persistence_required
        || flags.tamed
}

/// Call from an entity interaction whenever its persistent attributes change.
/// Entries are keyed by the live entity id, which is unique within a session.
#[cfg_attr(not(test), allow(dead_code))]
fn mob_persistence_flags() -> &'static Mutex<HashMap<i32, MobPersistenceFlags>> {
    static FLAGS: OnceLock<Mutex<HashMap<i32, MobPersistenceFlags>>> = OnceLock::new();
    FLAGS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg_attr(not(test), allow(dead_code))]
fn set_mob_persistence_flags(eid: i32, flags: MobPersistenceFlags) {
    let mut known = mob_persistence_flags().lock().unwrap_or_else(|e| e.into_inner());
    if should_exempt_from_despawn(flags) {
        known.insert(eid, flags);
    } else {
        known.remove(&eid);
    }
}

/// Read-modify-write one mob's flags through the exempt-or-remove contract
/// of `set_mob_persistence_flags`.
fn update_mob_persistence_flags(eid: i32, mutate: impl FnOnce(&mut MobPersistenceFlags)) {
    let mut flags = mob_persistence_flags()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&eid)
        .copied()
        .unwrap_or_default();
    mutate(&mut flags);
    set_mob_persistence_flags(eid, flags);
}

/// `NameTagItem.mobInteract` sets `PersistenceRequired` together with the
/// custom name, so a name-tagged mob stays exempt even past the 128-block
/// instant-despawn radius or after 600 idle ticks.
pub(crate) fn mark_mob_named_persistent(eid: i32) {
    update_mob_persistence_flags(eid, |flags| {
        flags.custom_named = true;
        flags.persistence_required = true;
    });
}

/// Items a live mob picked up off the ground (entity id -> item id). Vanilla
/// `Mob`/`Fox`/`Allay` pickup implies `PersistenceRequired`: a mob carrying a
/// found item never despawns.
pub(crate) fn mob_picked_up_items() -> &'static Mutex<HashMap<i32, u32>> {
    static ITEMS: OnceLock<Mutex<HashMap<i32, u32>>> = OnceLock::new();
    ITEMS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record one picked-up item and set `PersistenceRequired`, matching the
/// vanilla pickup path (`Mob.setItemSlot` + `canPickUpLoot`).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn mark_mob_picked_up_item(eid: i32, item_id: u32) {
    mob_picked_up_items()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(eid, item_id);
    update_mob_persistence_flags(eid, |flags| flags.persistence_required = true);
}

/// `canPickUpLoot`/item-holding types in the pinned 1.21.1 registry: zombies
/// and their variants, skeletons and their variants, piglins, and the
/// item-carrying fox and allay. A mob not in this list drops nothing into
/// `mob_picked_up_items` and keeps the normal despawn schedule.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn mob_can_pick_up_dropped_item(etype: i64) -> bool {
    let Some(spec) = entity_spec_by_id(etype) else {
        return false;
    };
    matches!(
        spec.name,
        "minecraft:allay"
            | "minecraft:bogged"
            | "minecraft:drowned"
            | "minecraft:fox"
            | "minecraft:husk"
            | "minecraft:piglin"
            | "minecraft:skeleton"
            | "minecraft:stray"
            | "minecraft:wither_skeleton"
            | "minecraft:zombie"
            | "minecraft:zombie_villager"
            | "minecraft:zombified_piglin"
    )
}

/// A mob takes one dropped item only when its type picks items up and it is
/// not already carrying a found one. The pickup marks the mob persistent, so
/// a zombie/husk/drowned/skeleton/piglin holding an item never despawns.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn try_mob_pickup_item(mob: &Mob, item_id: u32) -> bool {
    if !mob_can_pick_up_dropped_item(mob.etype)
        || mob_picked_up_items()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&mob.eid)
    {
        return false;
    }
    mark_mob_picked_up_item(mob.eid, item_id);
    true
}

/// Drop every despawn-immunity record for a removed or dead mob.
pub(crate) fn clear_mob_persistence_state(eid: i32) {
    mob_persistence_flags()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&eid);
    mob_picked_up_items()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&eid);
}

/// Vanilla name-tag easter eggs, resolved from the entity type plus the exact
/// custom name. Every name check is case-sensitive, matching
/// `LivingEntityRenderer.isEntityUpsideDown` and the per-type name compares.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NameTagEasterEgg {
    /// "Dinnerbone" or "Grumm" flips the model of any named living entity.
    UpsideDown,
    /// "jeb_" sheep fades its wool through the 16 dye colors.
    RainbowSheep,
    /// "Toast" rabbits render with the memorial black-and-white texture.
    ToastRabbit,
    /// "Johnny" vindicators attack every non-illager entity in range.
    JohnnyVindicator,
}

/// The upside-down names apply to every living entity; the other eggs bind
/// one name to one entity type. Exact, case-sensitive matching only.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn classify_name_tag_easter_egg(
    etype: i64,
    custom_name: &str,
) -> Option<NameTagEasterEgg> {
    if matches!(custom_name, "Dinnerbone" | "Grumm") {
        return Some(NameTagEasterEgg::UpsideDown);
    }
    let egg = match (
        entity_spec_by_id(etype).map(|spec| spec.name),
        custom_name,
    ) {
        (Some("minecraft:sheep"), "jeb_") => NameTagEasterEgg::RainbowSheep,
        (Some("minecraft:rabbit"), "Toast") => NameTagEasterEgg::ToastRabbit,
        (Some("minecraft:vindicator"), "Johnny") => NameTagEasterEgg::JohnnyVindicator,
        _ => return None,
    };
    Some(egg)
}

/// Live lookup for AI targeting: a vindicator carrying the "Johnny" custom
/// name is hostile to every non-illager entity (vanilla `Vindicator` JOHNNY).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn vindicator_is_johnny(eid: i32, etype: i64) -> bool {
    mob_custom_names()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&eid)
        .is_some_and(|name| {
            classify_name_tag_easter_egg(etype, name) == Some(NameTagEasterEgg::JohnnyVindicator)
        })
}

/// Vanilla ground-item reach for `Mob.aiStep`/`Fox`/`Allay` pickup scans: the
/// search box extends about this far horizontally and vertically past the mob.
#[cfg_attr(not(test), allow(dead_code))]
const MOB_ITEM_PICKUP_RADIUS: f64 = 1.45;

/// A "Johnny" vindicator sweeps targets inside the standard monster follow
/// range (`Monster.createMonsterAttributes` registers 16.0 blocks).
#[cfg_attr(not(test), allow(dead_code))]
const JOHNNY_VINDICATOR_FOLLOW_RANGE: f64 = 16.0;

/// One dropped-item entity as a pickup candidate: its entity id, the carried
/// item's protocol id and its world position for the distance check.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DroppedItemCandidate {
    pub item_eid: i32,
    pub item_id: i32,
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// A mob/dropped-item pair the tick accepted: the caller applies the held
/// item, removes the item entity and broadcasts the equipment update.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MobPickupAccepted {
    pub mob_eid: i32,
    pub item_eid: i32,
    pub item_id: i32,
}

/// Tick-level pass over the vanilla ground-item pickup rule: each alive,
/// empty-handed pickup-capable mob takes the nearest unclaimed dropped item
/// inside `pickup_radius`. A claimed `item_eid` leaves the candidacy pool
/// immediately, so two mobs can never pick the same item in one tick.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn evaluate_mob_item_pickups(
    mobs: &mut [Mob],
    dropped_items: &[DroppedItemCandidate],
    pickup_radius: f64,
) -> Vec<MobPickupAccepted> {
    let mut accepted = Vec::new();
    if !(pickup_radius.is_finite() && pickup_radius > 0.0) {
        return accepted;
    }
    let reach_sq = pickup_radius * pickup_radius;
    let mut claimed = std::collections::HashSet::new();
    for mob in mobs.iter_mut() {
        if !mob_can_pick_up_dropped_item(mob.etype)
            || *mob.hp.lock().unwrap_or_else(|e| e.into_inner()) <= 0.0
            || mob_picked_up_items()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&mob.eid)
        {
            continue;
        }
        let (mx, my, mz, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
        let nearest = dropped_items
            .iter()
            .filter(|item| !claimed.contains(&item.item_eid))
            .filter_map(|item| {
                let dist_sq =
                    (item.x - mx).powi(2) + (item.y - my).powi(2) + (item.z - mz).powi(2);
                (dist_sq.is_finite() && dist_sq <= reach_sq).then_some((dist_sq, item))
            })
            .min_by(|(a_dist, a_item), (b_dist, b_item)| {
                a_dist
                    .total_cmp(b_dist)
                    .then(a_item.item_eid.cmp(&b_item.item_eid))
            });
        let Some((_, item)) = nearest else {
            continue;
        };
        let Ok(item_id) = u32::try_from(item.item_id) else {
            continue;
        };
        if try_mob_pickup_item(mob, item_id) {
            claimed.insert(item.item_eid);
            accepted.push(MobPickupAccepted {
                mob_eid: mob.eid,
                item_eid: item.item_eid,
                item_id: item.item_id,
            });
        }
    }
    accepted
}

/// The nearest legal target of a "Johnny" vindicator's hostile sweep: any
/// alive non-vindicator mob inside `max_range`, with the entity id breaking
/// exact distance ties so the pick is deterministic across calls.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn select_johnny_vindicator_target(
    vindicator: &Mob,
    candidates: &[Mob],
    max_range: f64,
) -> Option<i32> {
    if !vindicator_is_johnny(vindicator.eid, vindicator.etype)
        || *vindicator.hp.lock().unwrap_or_else(|e| e.into_inner()) <= 0.0
        || !(max_range.is_finite() && max_range > 0.0)
    {
        return None;
    }
    let (vx, vy, vz, ..) = *vindicator.pos.lock().unwrap_or_else(|e| e.into_inner());
    let range_sq = max_range * max_range;
    candidates
        .iter()
        .filter(|candidate| {
            candidate.eid != vindicator.eid
                && entity_spec_by_id(candidate.etype).map(|spec| spec.name)
                    != Some("minecraft:vindicator")
                && *candidate.hp.lock().unwrap_or_else(|e| e.into_inner()) > 0.0
        })
        .filter_map(|candidate| {
            let (cx, cy, cz, ..) = *candidate.pos.lock().unwrap_or_else(|e| e.into_inner());
            let dist_sq = (cx - vx).powi(2) + (cy - vy).powi(2) + (cz - vz).powi(2);
            (dist_sq.is_finite() && dist_sq <= range_sq).then_some((dist_sq, candidate.eid))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)))
        .map(|(_, eid)| eid)
}

/// Single tick entry point for this lane: ground-item pickup (which writes
/// `PersistenceRequired` per accepted pair) and every Johnny vindicator's
/// hostile target sweep. Returns the accepted pickups and the
/// attacker -> target eid pairs for the caller to apply.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn tick_mob_persistence_and_easter_egg_ai(
    mobs: &mut [Mob],
    dropped_items: &[DroppedItemCandidate],
) -> (Vec<MobPickupAccepted>, Vec<(i32, i32)>) {
    let pickups = evaluate_mob_item_pickups(mobs, dropped_items, MOB_ITEM_PICKUP_RADIUS);
    let johnny_targets = mobs
        .iter()
        .filter_map(|mob| {
            select_johnny_vindicator_target(mob, mobs, JOHNNY_VINDICATOR_FOLLOW_RANGE)
                .map(|target| (mob.eid, target))
        })
        .collect();
    (pickups, johnny_targets)
}

fn live_despawn_exempt(mob: &Mob) -> bool {
    let mut flags = mob_persistence_flags()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&mob.eid)
        .copied()
        .unwrap_or_default();
    flags.tamed |= live_mob_states()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&mob.eid)
        .and_then(|state| match state.behavior.as_ref() {
            Some(MobBehaviorState::Wolf(pet) | MobBehaviorState::Cat(pet)) => pet.owner,
            _ => None,
        })
        .is_some();
    // The live registries are authoritative even when nobody set a stored
    // flag: a name tag, a lead tether or a held found item each exempt.
    flags.custom_named |= mob_custom_names()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&mob.eid);
    flags.leashed |= entity_tethers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&mob.eid);
    flags.persistence_required |= mob_picked_up_items()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&mob.eid);
    // Live mounting is authoritative. A passenger or a ridden mob cannot
    // despawn merely because nobody explicitly set the persistence bit.
    let mounts = mounts().lock().unwrap_or_else(|e| e.into_inner());
    flags.riding_or_ridden |= mounts.registry.vehicle_of.contains_key(&mob.eid)
        || !mounts.registry.passengers_of(mob.eid).is_empty();
    should_exempt_from_despawn(flags)
}

thread_local! {
    // Spawn insertion and despawn checks run on the game tick thread. The first despawn
    // tick enables tracking. Weak references don't extend the entity's lifetime.
    static NATURAL_HOSTILES: OnceCell<RefCell<Vec<NaturalHostile>>> = const { OnceCell::new() };
}

struct NaturalHostile {
    mob: Weak<Mob>,
    no_action_time: i32,
    persistent_or_passenger: bool,
    rng: Lcg,
    previous_hp: f32,
    previous_hurt_at: Instant,
    previous_hurt_tick: Option<u64>,
}

impl NaturalHostile {
    fn new(mob: &Arc<Mob>, seed: u64) -> Self {
        Self {
            mob: Arc::downgrade(mob),
            no_action_time: 0,
            // Retained for the existing natural-spawner persistence contract.
            persistent_or_passenger: false,
            rng: Lcg::new(seed),
            previous_hp: *mob.hp.lock().unwrap_or_else(|e| e.into_inner()),
            previous_hurt_at: *mob.hurt_at.lock().unwrap_or_else(|e| e.into_inner()),
            previous_hurt_tick: *mob.last_hurt_tick.lock().unwrap_or_else(|e| e.into_inner()),
        }
    }
}

impl NaturalHostile {
    fn observe_hurt(&mut self, mob: &Mob, hp: f32) {
        let hurt_at = *mob.hurt_at.lock().unwrap_or_else(|e| e.into_inner());
        let hurt_tick = *mob.last_hurt_tick.lock().unwrap_or_else(|e| e.into_inner());
        // Melee and cramming have hurt stamps. Other live damage paths write HP directly.
        if hp < self.previous_hp
            || hurt_at != self.previous_hurt_at
            || hurt_tick != self.previous_hurt_tick
        {
            self.no_action_time = 0;
        }
        self.previous_hp = hp;
        self.previous_hurt_at = hurt_at;
        self.previous_hurt_tick = hurt_tick;
    }

    fn check_despawn(&mut self, peaceful: bool, distance_sq: Option<f64>) -> bool {
        let persistent = self.persistent_or_passenger
            || self.mob.upgrade().is_some_and(|mob| live_despawn_exempt(&mob));
        // Mob.checkDespawn draws before testing the inner radius, even after a far discard.
        // These naturally spawned monsters inherit both removal policies.
        let roll = if !peaceful
            && !persistent
            && distance_sq.is_some()
            && self.no_action_time > 600
        {
            self.rng.next_bound(800)
        } else {
            1
        };
        match evaluate_despawn_with_peaceful(
            peaceful,
            true,
            distance_sq,
            self.no_action_time,
            persistent,
            true,
            MobCategory::Monster,
            roll,
        ) {
            DespawnDecision::DespawnNow | DespawnDecision::DespawnRandom => true,
            DespawnDecision::ResetNoActionTime => {
                self.no_action_time = 0;
                false
            }
            DespawnDecision::Keep => false,
        }
    }

    fn advance_ai_step(&mut self, bright: bool) {
        // Monster.updateNoActionTime adds 2 before Mob.serverAiStep adds 1.
        self.no_action_time = self.no_action_time.wrapping_add(if bright { 3 } else { 1 });
    }
}

fn push_natural_hostile(mobs: &mut Vec<Arc<Mob>>, mob: Arc<Mob>) {
    NATURAL_HOSTILES.with(|hostiles| {
        if let Some(hostiles) = hostiles.get() {
            // Entity.random is an independently seeded legacy Java RNG. The Rust AI has
            // no shared per-mob RNG yet, so this stream serves despawn alone.
            let seed = RandomState::new().hash_one(mob.eid);
            hostiles.borrow_mut().push(NaturalHostile::new(&mob, seed));
        }
    });
    mobs.push(mob);
}

fn tick_natural_hostile_despawn() {
    NATURAL_HOSTILES.with(|hostiles| {
        let mut hostiles = hostiles.get_or_init(RefCell::default).borrow_mut();
        if hostiles.is_empty() {
            return;
        }

        let clients = client_snapshot();
        let players = despawn_players(&clients);
        let peaceful = DIFFICULTY.load(Ordering::Relaxed) == 0;
        let sky_darken = current_sky_darken();
        let mut lighting = HashMap::new();
        let mut discarded = HashMap::new();
        hostiles.retain_mut(|hostile| {
            let Some(mob) = hostile.mob.upgrade() else {
                return false;
            };
            let hp = *mob.hp.lock().unwrap_or_else(|e| e.into_inner());
            if hp <= 0.0 {
                return true; // Leave deaths and their drops to the existing combat/environment path.
            }
            hostile.observe_hurt(&mob, hp);

            let (x, y, z, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
            // `players` only tracks the three vanilla dimensions; a datapack
            // dimension slot indexes past the array, so it sees no players.
            let distance = players
                .get(mob.dim.index())
                .and_then(|players| nearest_player_distance_sq(players, [x, y, z]));
            if hostile.check_despawn(peaceful, distance) {
                discarded.insert(mob.eid, mob);
                return false;
            }

            // Match the current Mob tick gate. Distance checks also cover mobs that
            // stopped ticking when their player moved away.
            if !clients.is_empty() && region_guard().is_hot(x as i32, z as i32) {
                let (bx, by, bz) = (
                    x.floor() as i32,
                    (y + f64::from(entity_eye_height_1211(mob.etype))).floor() as i32,
                    z.floor() as i32,
                );
                let chunk = (bx >> 4, bz >> 4);
                let bright = if mob.dim == DimId::Overworld {
                    let light: &mut DespawnChunkLight = lighting.entry(chunk).or_default();
                    light.is_bright(&clients, sky_darken, bx, by, bz)
                } else {
                    settled_live_light(mob.dim, chunk.0, chunk.1).is_some_and(|light| {
                        nibble_at_dim(&light.block, mob.dim, bx, by, bz) >= 12
                    })
                };
                hostile.advance_ai_step(bright);
            }
            true
        });
        discard_natural_hostiles(&discarded);
    });
}

fn despawn_players(clients: &[Arc<Client>]) -> [Vec<[f64; 3]>; 3] {
    let mut players = [Vec::new(), Vec::new(), Vec::new()];
    for client in clients.iter().filter(|client| !client.is_spectator()) {
        let (dim, x, y, z, ..) = client_dim_pos(client);
        if let Some(players) = players.get_mut(dim.index()) {
            players.push([x, y, z]);
        }
    }
    players
}

fn nearest_player_distance_sq(players: &[[f64; 3]], [x, y, z]: [f64; 3]) -> Option<f64> {
    players
        .iter()
        .map(|&[px, py, pz]| (px - x).powi(2) + (py - y).powi(2) + (pz - z).powi(2))
        .min_by(f64::total_cmp)
}

#[derive(Default)]
struct DespawnChunkLight {
    sky_floors: Option<[i32; 256]>,
    sky: Option<Vec<[u8; 2048]>>,
    block: Option<Vec<[u8; 2048]>>,
}

impl DespawnChunkLight {
    fn is_bright(&mut self, clients: &[Arc<Client>], darken: i32, x: i32, y: i32, z: i32) -> bool {
        // Overworld ambient light is zero. LevelReader's f/(4-3f), where f is raw
        // brightness / 15, first rounds above 0.5 at light level 12 in Java float.
        const BRIGHT: i32 = 12;
        let (cx, cz) = (x >> 4, z >> 4);
        if 15 - darken >= BRIGHT {
            let floors = self
                .sky_floors
                .get_or_insert_with(|| sky_floors(cx, cz, &opacity_windowed));
            let column = (((z & 15) as usize) << 4) | (x & 15) as usize;
            if y > floors[column]
                || nibble_at(
                    self.sky
                        .get_or_insert_with(|| sky_light_sections_world(cx, cz)),
                    x,
                    y,
                    z,
                ) - darken
                    >= BRIGHT
            {
                return true;
            }
        }

        let block = self.block.get_or_insert_with(|| {
            let sources = nightly_spawn_light_sources(clients, cx, cz);
            block_light_sections(cx, cz, &sources)
        });
        nibble_at(block, x, y, z) >= BRIGHT
    }
}

fn discard_natural_hostiles(discarded: &HashMap<i32, Arc<Mob>>) {
    if discarded.is_empty() {
        return;
    }
    let removed = with_save_guard(|| {
        let mut removed = Vec::new();
        mobs().lock().unwrap_or_else(|e| e.into_inner()).retain(|mob| {
            let discard = discarded.get(&mob.eid).is_some_and(|candidate| {
                Arc::ptr_eq(mob, candidate) && *mob.hp.lock().unwrap_or_else(|e| e.into_inner()) > 0.0
            });
            if discard {
                removed.push(mob.eid);
            }
            !discard
        });
        let mut tags = tags().lock().unwrap_or_else(|e| e.into_inner());
        for &eid in &removed {
            tags.remove(&format!("e:{eid}"));
        }
        removed
    });
    for eid in removed {
        clear_mob_persistence_state(eid);
        broadcast_seen_destroy(eid);
    }
}

#[cfg(test)]
mod mob_despawn_tests {
    use super::*;
    use crate::effect_tests::client_pair;

    // The first Java nextInt(800) for seed 923 is zero.
    const ZERO_ROLL_SEED: u64 = 923;

    #[test]
    fn vanilla_mobs2_named_leashed_ridden_required_and_tamed_survive_both_distances() {
        let mob = zombie(981_238, (0.0, 70.0, 0.0));
        for flags in [
            MobPersistenceFlags { custom_named: true, ..Default::default() },
            MobPersistenceFlags { leashed: true, ..Default::default() },
            MobPersistenceFlags { riding_or_ridden: true, ..Default::default() },
            MobPersistenceFlags { persistence_required: true, ..Default::default() },
            MobPersistenceFlags { tamed: true, ..Default::default() },
        ] {
            set_mob_persistence_flags(mob.eid, flags);
            let mut hostile = NaturalHostile::new(&mob, ZERO_ROLL_SEED);
            hostile.no_action_time = 601;
            let initial_rng = hostile.rng.state48();
            assert!(!hostile.check_despawn(false, Some(129.0_f64.powi(2))));
            assert!(!hostile.check_despawn(false, Some(64.0_f64.powi(2))));
            assert_eq!(hostile.no_action_time, 0);
            assert_eq!(hostile.rng.state48(), initial_rng);
            assert!(hostile.check_despawn(true, None));
        }
        set_mob_persistence_flags(mob.eid, MobPersistenceFlags::default());
        let mut hostile = NaturalHostile::new(&mob, ZERO_ROLL_SEED);
        assert!(hostile.check_despawn(false, Some(129.0_f64.powi(2))));
    }

    fn zombie(eid: i32, pos: (f64, f64, f64)) -> Arc<Mob> {
        zombie_in(eid, pos, DimId::Overworld)
    }

    fn zombie_in(eid: i32, pos: (f64, f64, f64), dim: DimId) -> Arc<Mob> {
        mob_named(eid, pos, dim, "zombie")
    }

    fn mob_named(eid: i32, pos: (f64, f64, f64), dim: DimId, name: &str) -> Arc<Mob> {
        Arc::new(mob_at(eid, pos, dim, name))
    }

    fn mob_at(eid: i32, pos: (f64, f64, f64), dim: DimId, name: &str) -> Mob {
        let past = Instant::now() - Duration::from_secs(10);
        Mob {
            dim,
            eid,
            etype: name_to_etype(name).unwrap(),
            uuid: [0; 16],
            pos: Mutex::new((pos.0, pos.1, pos.2, 0.0, 0.0)),
            hp: Mutex::new(20.0),
            last_hit: Mutex::new(past),
            hurt_at: Mutex::new(past),
            last_hurt_tick: Mutex::new(None),
            vel: Mutex::new([0.0; 3]),
            fuse: Mutex::new(0),
            fire_ticks: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(300),
        }
    }

    #[test]
    fn random_despawn_starts_after_600_ai_ticks_and_requires_zero_roll() {
        let mob = zombie(10, (0.0, 70.0, 0.0));
        let mut hostile = NaturalHostile::new(&mob, ZERO_ROLL_SEED);
        let initial_rng = hostile.rng.state48();
        for _ in 0..600 {
            assert!(!hostile.check_despawn(false, Some(64.0_f64.powi(2))));
            hostile.advance_ai_step(false);
        }
        assert!(!hostile.check_despawn(false, Some(64.0_f64.powi(2))));
        assert_eq!(hostile.rng.state48(), initial_rng);
        hostile.advance_ai_step(false);
        assert!(hostile.check_despawn(false, Some(64.0_f64.powi(2))));

        let mut nonzero = NaturalHostile::new(&mob, 0);
        nonzero.no_action_time = 601;
        assert!(!nonzero.check_despawn(false, Some(64.0_f64.powi(2))));
    }

    #[test]
    fn distance_boundaries_and_proximity_reset() {
        let mob = zombie(10, (0.0, 70.0, 0.0));
        let mut hostile = NaturalHostile::new(&mob, ZERO_ROLL_SEED);
        assert!(!hostile.check_despawn(false, Some(128.0_f64.powi(2))));
        assert!(hostile.check_despawn(false, Some(128.01_f64.powi(2))));

        hostile.no_action_time = 599;
        assert!(!hostile.check_despawn(false, Some(32.0_f64.powi(2))));
        assert_eq!(hostile.no_action_time, 599);
        hostile.advance_ai_step(true);
        assert_eq!(hostile.no_action_time, 602);
        let initial_rng = hostile.rng.state48();
        assert!(!hostile.check_despawn(false, Some(31.99_f64.powi(2))));
        assert_ne!(hostile.rng.state48(), initial_rng);
        assert_eq!(hostile.no_action_time, 0);
    }

    #[test]
    fn no_players_keep_mob_and_peaceful_overrides_persistence() {
        let mob = zombie(10, (0.0, 70.0, 0.0));
        let mut hostile = NaturalHostile::new(&mob, ZERO_ROLL_SEED);
        hostile.no_action_time = 601;
        let initial_rng = hostile.rng.state48();
        assert!(!hostile.check_despawn(false, None));
        assert_eq!(hostile.no_action_time, 601);
        assert_eq!(hostile.rng.state48(), initial_rng);

        hostile.persistent_or_passenger = true;
        assert!(!hostile.check_despawn(false, Some(129.0_f64.powi(2))));
        assert_eq!(hostile.no_action_time, 0);
        assert_eq!(hostile.rng.state48(), initial_rng);
        assert!(hostile.check_despawn(true, None));
    }

    #[test]
    fn observed_damage_resets_idle_before_the_despawn_roll() {
        let mob = zombie(10, (0.0, 70.0, 0.0));
        let mut hostile = NaturalHostile::new(&mob, ZERO_ROLL_SEED);
        hostile.no_action_time = 601;
        *mob.hp.lock().unwrap_or_else(|e| e.into_inner()) = 19.0;
        hostile.observe_hurt(&mob, *mob.hp.lock().unwrap_or_else(|e| e.into_inner()));
        assert_eq!(hostile.no_action_time, 0);
        assert!(!hostile.check_despawn(false, Some(64.0_f64.powi(2))));
    }

    #[test]
    fn bright_idle_step_uses_eye_light_after_sky_darken() {
        let mut light = DespawnChunkLight {
            sky_floors: Some([70; 256]),
            sky: Some(vec![[0; 2048]; 26]),
            block: Some(vec![[0xbb; 2048]; 26]),
        };
        assert!(light.is_bright(&[], 3, 0, 71, 0));
        assert!(!light.is_bright(&[], 3, 0, 70, 0));
        assert!(!light.is_bright(&[], 4, 0, 71, 0));
        light.block = Some(vec![[0xcc; 2048]; 26]);
        assert!(light.is_bright(&[], 4, 0, 71, 0));
    }

    #[test]
    fn live_discard_ignores_spectators_and_frees_the_cap_and_tracker() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        DIFFICULTY.store(1, Ordering::Relaxed);
        tick_natural_hostile_despawn();

        let (spectator, mut receiver) = client_pair();
        *spectator.gm.lock().unwrap_or_else(|e| e.into_inner()) = 3;
        spectator.seen.lock().unwrap_or_else(|e| e.into_inner()).insert(40);
        let spectator = Arc::new(spectator);
        let (nether_player, _) = client_pair();
        *nether_player.dim.lock().unwrap_or_else(|e| e.into_inner()) = DimId::Nether;
        let (survival, _) = client_pair();
        *survival.pos.lock().unwrap_or_else(|e| e.into_inner()) = (0.0, 199.0, 0.0, 0.0, 0.0, true);
        let survival = Arc::new(survival);

        let natural = zombie(40, (0.0, 70.0, 0.0));
        let other = zombie(41, (0.0, 70.0, 0.0));
        {
            let mut mobs = mobs().lock().unwrap_or_else(|e| e.into_inner());
            push_natural_hostile(&mut mobs, natural);
            mobs.push(other);
            assert!(!has_room_for_nightly_monster(&mobs, 2));
        }
        crate::clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend([spectator, Arc::new(nether_player)]);
        tick_natural_hostile_despawn();
        assert_eq!(
            mobs().lock().unwrap_or_else(|e| e.into_inner()).len(),
            2,
            "no eligible Overworld player is present"
        );

        crate::clients().lock().unwrap_or_else(|e| e.into_inner()).push(survival);
        tick_natural_hostile_despawn();
        {
            let mobs = mobs().lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(mobs.iter().map(|mob| mob.eid).collect::<Vec<_>>(), [41]);
            assert!(has_room_for_nightly_monster(&mobs, 2));
        }
        let (packet, body) = read_packet(&mut receiver, Instant::now() + Duration::from_secs(1))
            .expect("tracking spectator receives Remove Entities");
        assert_eq!(packet, 0x42);
        let mut body = &body[..];
        assert_eq!(read_varint(&mut body), Some(1));
        assert_eq!(read_varint(&mut body), Some(40));
        assert!(body.is_empty());
    }

    #[test]
    fn live_random_despawn_waits_for_601_ai_steps() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        DIFFICULTY.store(1, Ordering::Relaxed);
        crate::WORLD_TIME.store(18000, Ordering::Relaxed);
        crate::set_test_region([(0, 0)].into(), Default::default());
        let (player, _) = client_pair();
        player.pos.lock().unwrap_or_else(|e| e.into_inner()).0 = 64.0;
        crate::clients().lock().unwrap_or_else(|e| e.into_inner()).push(Arc::new(player));
        tick_natural_hostile_despawn();
        push_natural_hostile(&mut mobs().lock().unwrap_or_else(|e| e.into_inner()), zombie(40, (0.0, 70.0, 0.0)));
        NATURAL_HOSTILES.with(|hostiles| {
            hostiles.get().unwrap().borrow_mut()[0].rng = Lcg::new(ZERO_ROLL_SEED);
        });

        for _ in 0..601 {
            tick_natural_hostile_despawn();
            assert_eq!(mobs().lock().unwrap_or_else(|e| e.into_inner()).len(), 1);
        }
        tick_natural_hostile_despawn();
        assert!(mobs().lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    }

    #[test]
    fn bughunt_root_despawn_tracks_only_vanilla_dimension_slots() {
        // A client in a datapack dimension (slot >= 3) indexed past the end of
        // the three-slot per-dimension player array.
        let (player, _) = client_pair();
        *player.dim.lock().unwrap_or_else(|e| e.into_inner()) = DimId::from_raw(3);
        let players = despawn_players(&[Arc::new(player)]);
        assert!(players.iter().all(Vec::is_empty));
    }

    #[test]
    fn bughunt_root_despawn_custom_dimension_hostile_keeps_ticking() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        DIFFICULTY.store(1, Ordering::Relaxed);
        tick_natural_hostile_despawn();
        let (player, _) = client_pair();
        crate::clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::new(player));
        // A mob in a datapack dimension slot indexed past the same array.
        push_natural_hostile(
            &mut mobs().lock().unwrap_or_else(|e| e.into_inner()),
            zombie_in(40, (0.0, 70.0, 0.0), DimId::from_raw(3)),
        );
        tick_natural_hostile_despawn();
        // With no players tracked for its dimension the mob stays loaded.
        assert_eq!(mobs().lock().unwrap_or_else(|e| e.into_inner()).len(), 1);
    }

    #[test]
    fn live_peaceful_discard_runs_without_players() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        DIFFICULTY.store(0, Ordering::Relaxed);
        tick_natural_hostile_despawn();
        push_natural_hostile(&mut mobs().lock().unwrap_or_else(|e| e.into_inner()), zombie(40, (0.0, 70.0, 0.0)));
        tick_natural_hostile_despawn();
        assert!(mobs().lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    }

    #[test]
    fn named_pickup_and_leashed_mobs_survive_the_instant_despawn_radius() {
        // Past 128 blocks an unprotected monster discards immediately.
        let far = Some(200.0_f64.powi(2));

        // A name tag lands both persistence bits the moment it is applied.
        let named = zombie(77_001, (0.0, 70.0, 0.0));
        mob_custom_names()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(named.eid, "Clyde".to_string());
        mark_mob_named_persistent(named.eid);
        let flags = mob_persistence_flags()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&named.eid)
            .copied()
            .unwrap_or_default();
        assert!(flags.custom_named && flags.persistence_required);
        let mut hostile = NaturalHostile::new(&named, ZERO_ROLL_SEED);
        hostile.no_action_time = 2000;
        assert!(!hostile.check_despawn(false, far));
        assert!(!hostile.check_despawn(false, far));

        // A zombie holding a found item never despawns.
        let holder = zombie(77_002, (0.0, 70.0, 0.0));
        assert!(try_mob_pickup_item(&holder, 42));
        assert!(!try_mob_pickup_item(&holder, 43), "one held item per mob");
        assert!(live_despawn_exempt(&holder));
        let mut hostile = NaturalHostile::new(&holder, ZERO_ROLL_SEED);
        hostile.no_action_time = 2000;
        assert!(!hostile.check_despawn(false, far));

        // A live lead tether exempts with no stored flag at all.
        let leashed = zombie(77_003, (0.0, 70.0, 0.0));
        entity_tethers()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                leashed.eid,
                vanilla_lead_tether(leashed.eid, TetherAnchor::Entity(77_900)),
            );
        assert!(live_despawn_exempt(&leashed));
        let mut hostile = NaturalHostile::new(&leashed, ZERO_ROLL_SEED);
        hostile.no_action_time = 2000;
        assert!(!hostile.check_despawn(false, far));

        // A plain zombie at the same distance still discards instantly: the
        // exemptions did not weaken the gate.
        let plain = zombie(77_004, (0.0, 70.0, 0.0));
        let mut hostile = NaturalHostile::new(&plain, ZERO_ROLL_SEED);
        hostile.no_action_time = 2000;
        assert!(hostile.check_despawn(false, far));

        // Peaceful still removes named/pickup/leashed hostiles, matching
        // vanilla Mob.checkDespawn's difficulty branch before persistence.
        let named = NaturalHostile::new(&named, ZERO_ROLL_SEED).check_despawn(true, None);
        let holder = NaturalHostile::new(&holder, ZERO_ROLL_SEED).check_despawn(true, None);
        assert!(named && holder);

        mob_custom_names()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&77_001);
        entity_tethers()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&77_003);
        clear_mob_persistence_state(77_001);
        clear_mob_persistence_state(77_002);
    }

    #[test]
    fn item_pickup_gates_on_type_and_clears_with_the_mob() {
        for name in [
            "zombie",
            "zombie_villager",
            "husk",
            "drowned",
            "skeleton",
            "stray",
            "bogged",
            "wither_skeleton",
            "piglin",
            "zombified_piglin",
            "fox",
            "allay",
        ] {
            assert!(mob_can_pick_up_dropped_item(name_to_etype(name).unwrap()), "{name}");
        }
        for name in ["cow", "creeper", "piglin_brute", "villager", "item"] {
            assert!(!mob_can_pick_up_dropped_item(name_to_etype(name).unwrap()), "{name}");
        }
        assert!(!mob_can_pick_up_dropped_item(i64::MAX));

        // A mob whose type never picks up items cannot take one.
        let cow = mob_named(77_005, (0.0, 70.0, 0.0), DimId::Overworld, "cow");
        assert!(!try_mob_pickup_item(&cow, 7));
        assert!(!live_despawn_exempt(&cow));

        // mark + clear round-trips both the held item and the flag.
        mark_mob_picked_up_item(77_006, 7);
        assert_eq!(
            mob_picked_up_items()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&77_006),
            Some(&7)
        );
        clear_mob_persistence_state(77_006);
        assert!(!mob_picked_up_items()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&77_006));
        assert!(!mob_persistence_flags()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&77_006));
    }

    #[test]
    fn name_tag_easter_eggs_match_vanilla_type_and_case() {
        let sheep = name_to_etype("sheep").unwrap();
        let rabbit = name_to_etype("rabbit").unwrap();
        let vindicator = name_to_etype("vindicator").unwrap();
        let zombie_t = name_to_etype("zombie").unwrap();

        // Dinnerbone/Grumm flips every living entity, whatever its type.
        for name in ["Dinnerbone", "Grumm"] {
            for etype in [sheep, rabbit, vindicator, zombie_t] {
                assert_eq!(
                    classify_name_tag_easter_egg(etype, name),
                    Some(NameTagEasterEgg::UpsideDown)
                );
            }
        }
        // The other eggs bind one name to exactly one entity type.
        assert_eq!(
            classify_name_tag_easter_egg(sheep, "jeb_"),
            Some(NameTagEasterEgg::RainbowSheep)
        );
        assert_eq!(classify_name_tag_easter_egg(rabbit, "jeb_"), None);
        assert_eq!(classify_name_tag_easter_egg(zombie_t, "jeb_"), None);
        assert_eq!(
            classify_name_tag_easter_egg(rabbit, "Toast"),
            Some(NameTagEasterEgg::ToastRabbit)
        );
        assert_eq!(classify_name_tag_easter_egg(sheep, "Toast"), None);
        assert_eq!(
            classify_name_tag_easter_egg(vindicator, "Johnny"),
            Some(NameTagEasterEgg::JohnnyVindicator)
        );
        assert_eq!(classify_name_tag_easter_egg(zombie_t, "Johnny"), None);
        assert_eq!(classify_name_tag_easter_egg(vindicator, "Toast"), None);

        // Vanilla compares the name exactly: wrong case or whitespace is no egg.
        for name in [
            "dinnerbone", "DINNERBONE", "grumm", "JEB_", "jeb", "toast", "johnny", " Dinnerbone",
            "Dinnerbone ", "jeb__", "",
        ] {
            for etype in [sheep, rabbit, vindicator, zombie_t] {
                assert_eq!(classify_name_tag_easter_egg(etype, name), None, "{name}");
            }
        }
        // An unknown type still gets the name-only upside-down egg.
        assert_eq!(
            classify_name_tag_easter_egg(i64::MAX, "Grumm"),
            Some(NameTagEasterEgg::UpsideDown)
        );
        assert_eq!(classify_name_tag_easter_egg(i64::MAX, "jeb_"), None);
    }

    #[test]
    fn johnny_vindicator_reads_the_live_custom_name() {
        let vindicator = name_to_etype("vindicator").unwrap();
        let zombie_t = name_to_etype("zombie").unwrap();
        mob_custom_names()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(77_007, "Johnny".to_string());
        assert!(vindicator_is_johnny(77_007, vindicator));
        // The egg checks the type too: a "Johnny" zombie is not one.
        assert!(!vindicator_is_johnny(77_007, zombie_t));
        // No custom name at all.
        assert!(!vindicator_is_johnny(77_008, vindicator));
        // Case-sensitive: lowercase johnny does not trigger it.
        mob_custom_names()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(77_007, "johnny".to_string());
        assert!(!vindicator_is_johnny(77_007, vindicator));
        mob_custom_names()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&77_007);
    }

    #[test]
    fn tick_item_pickup_claims_the_nearest_drop_for_one_mob_only() {
        // Two pickup-capable zombies share one spot; only the first in tick
        // order can take the single dropped sword. Creeper and cow types never
        // pick items up even inside the reach.
        let mut mobs = vec![
            mob_at(78_001, (0.0, 70.0, 0.0), DimId::Overworld, "zombie"),
            mob_at(78_002, (0.0, 70.0, 0.0), DimId::Overworld, "zombie"),
            mob_at(78_003, (0.0, 70.0, 0.0), DimId::Overworld, "creeper"),
            mob_at(78_004, (0.0, 70.0, 0.0), DimId::Overworld, "cow"),
        ];
        let sword = DroppedItemCandidate {
            item_eid: 78_101,
            item_id: 276,
            x: 0.5,
            y: 70.0,
            z: 0.5,
        };
        let out_of_reach = DroppedItemCandidate {
            item_eid: 78_102,
            item_id: 1,
            x: 3.0,
            y: 70.0,
            z: 0.0,
        };
        let accepted =
            evaluate_mob_item_pickups(&mut mobs, &[sword, out_of_reach], MOB_ITEM_PICKUP_RADIUS);
        assert_eq!(
            accepted,
            [MobPickupAccepted {
                mob_eid: 78_001,
                item_eid: 78_101,
                item_id: 276,
            }],
            "the second zombie at the same spot cannot duplicate-pick 78_101"
        );

        // The found item writes PersistenceRequired the moment it is claimed:
        // the holder is exempt from the 200-block instant-despawn radius.
        let holder = zombie(78_001, (0.0, 70.0, 0.0));
        let mut hostile = NaturalHostile::new(&holder, ZERO_ROLL_SEED);
        hostile.no_action_time = 2000;
        assert!(!hostile.check_despawn(false, Some(200.0_f64.powi(2))));

        clear_mob_persistence_state(78_001);
    }

    #[test]
    fn tick_item_pickup_rejects_bad_reach_non_finite_items_and_full_hands() {
        let mut mobs = vec![mob_at(78_010, (0.0, 70.0, 0.0), DimId::Overworld, "zombie")];
        let item = DroppedItemCandidate {
            item_eid: 78_110,
            item_id: 276,
            x: 0.0,
            y: 70.0,
            z: 0.0,
        };
        for radius in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(evaluate_mob_item_pickups(&mut mobs, &[item], radius).is_empty());
        }
        // A dead mob picks nothing up.
        *mobs[0].hp.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
        assert!(evaluate_mob_item_pickups(&mut mobs, &[item], 1.45).is_empty());
        *mobs[0].hp.lock().unwrap_or_else(|e| e.into_inner()) = 20.0;

        // Non-finite item coordinates never enter a claim.
        let nan_item = DroppedItemCandidate {
            item_eid: 78_111,
            item_id: 1,
            x: f64::NAN,
            y: 70.0,
            z: 0.0,
        };
        assert!(evaluate_mob_item_pickups(&mut mobs, &[nan_item], 1.45).is_empty());

        // A mob already holding a found item takes no second one.
        assert!(try_mob_pickup_item(&mobs[0], 1));
        let second = DroppedItemCandidate {
            item_eid: 78_112,
            item_id: 2,
            x: 0.0,
            y: 70.0,
            z: 0.0,
        };
        assert!(evaluate_mob_item_pickups(&mut mobs, &[second], 1.45).is_empty());
        clear_mob_persistence_state(78_010);
    }

    #[test]
    fn johnny_vindicator_targets_the_nearest_non_vindicator_mob() {
        mob_custom_names()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(78_020, "Johnny".to_string());
        let johnny = mob_at(78_020, (0.0, 70.0, 0.0), DimId::Overworld, "vindicator");
        let sheep_far = mob_at(78_021, (10.0, 70.0, 0.0), DimId::Overworld, "sheep");
        let cow_near = mob_at(78_022, (2.0, 70.0, 0.0), DimId::Overworld, "cow");
        let vindicator_other =
            mob_at(78_023, (1.0, 70.0, 0.0), DimId::Overworld, "vindicator");
        let zombie_out = mob_at(78_024, (17.0, 70.0, 0.0), DimId::Overworld, "zombie");
        let sheep_dead = mob_at(78_025, (0.5, 70.0, 0.0), DimId::Overworld, "sheep");
        *sheep_dead.hp.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
        let candidates = [sheep_far, cow_near, vindicator_other, zombie_out, sheep_dead];
        assert_eq!(
            select_johnny_vindicator_target(&johnny, &candidates, 16.0),
            Some(78_022),
            "nearest alive non-vindicator wins; vindicators, the dead and out-of-range are skipped"
        );
        // Everything legal but out of reach sweeps nothing.
        let far_only = [mob_at(78_026, (16.1, 70.0, 0.0), DimId::Overworld, "zombie")];
        assert_eq!(
            select_johnny_vindicator_target(&johnny, &far_only, 16.0),
            None
        );
        // An invalid follow range disables the sweep.
        for range in [0.0, -4.0, f64::NAN, f64::INFINITY] {
            assert_eq!(
                select_johnny_vindicator_target(&johnny, &candidates, range),
                None
            );
        }
        // A dead Johnny sweeps nothing.
        *johnny.hp.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
        assert_eq!(
            select_johnny_vindicator_target(&johnny, &candidates, 16.0),
            None
        );
        *johnny.hp.lock().unwrap_or_else(|e| e.into_inner()) = 20.0;

        // An unnamed vindicator never sweeps, and vanilla compares the name
        // exactly: lowercase "johnny" is not the egg.
        mob_custom_names()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&78_020);
        assert_eq!(
            select_johnny_vindicator_target(&johnny, &candidates, 16.0),
            None
        );
        mob_custom_names()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(78_020, "johnny".to_string());
        assert_eq!(
            select_johnny_vindicator_target(&johnny, &candidates, 16.0),
            None
        );
        mob_custom_names()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&78_020);
    }

    #[test]
    fn johnny_target_ties_break_on_entity_id() {
        mob_custom_names()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(78_030, "Johnny".to_string());
        let johnny = mob_at(78_030, (0.0, 70.0, 0.0), DimId::Overworld, "vindicator");
        // Two candidates at the exact same distance: the lower eid wins.
        let candidates = [
            mob_at(78_032, (2.0, 70.0, 0.0), DimId::Overworld, "pig"),
            mob_at(78_031, (-2.0, 70.0, 0.0), DimId::Overworld, "pig"),
        ];
        assert_eq!(
            select_johnny_vindicator_target(&johnny, &candidates, 16.0),
            Some(78_031)
        );
        mob_custom_names()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&78_030);
    }

    #[test]
    fn tick_entry_runs_pickup_and_johnny_sweep_deterministically() {
        mob_custom_names()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(78_040, "Johnny".to_string());
        let mut mobs = vec![
            mob_at(78_040, (0.0, 70.0, 0.0), DimId::Overworld, "vindicator"),
            mob_at(78_041, (0.0, 70.0, 0.0), DimId::Overworld, "zombie"),
            mob_at(78_042, (3.0, 70.0, 0.0), DimId::Overworld, "sheep"),
        ];
        let items = [DroppedItemCandidate {
            item_eid: 78_140,
            item_id: 276,
            x: 0.0,
            y: 70.0,
            z: 0.0,
        }];
        let (pickups, pairs) = tick_mob_persistence_and_easter_egg_ai(&mut mobs, &items);
        assert_eq!(
            pickups,
            [MobPickupAccepted {
                mob_eid: 78_041,
                item_eid: 78_140,
                item_id: 276,
            }]
        );
        // Johnny attacks the closest non-vindicator: the zombie sharing his
        // spot, not the sheep farther out.
        assert_eq!(pairs, [(78_040, 78_041)]);

        // A second call is stable: the holder takes no second item, and the
        // same target pair comes back.
        let (pickups, pairs) = tick_mob_persistence_and_easter_egg_ai(&mut mobs, &items);
        assert!(pickups.is_empty());
        assert_eq!(pairs, [(78_040, 78_041)]);

        mob_custom_names()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&78_040);
        clear_mob_persistence_state(78_041);
    }
}
