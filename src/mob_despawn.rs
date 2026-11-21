// Runtime state for naturally spawned monsters in the Overworld and Nether.
// See the pinned 1.21.1 server's Mob.checkDespawn, ServerLevel.tick and Monster.updateNoActionTime.

use std::cell::{OnceCell, RefCell};
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::Weak;

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
            // Live Mob currently has no pickup, name-tag, leash or riding state.
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
        // Mob.checkDespawn draws before testing the inner radius, even after a far discard.
        // These naturally spawned monsters inherit both removal policies.
        let roll = if !peaceful
            && !self.persistent_or_passenger
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
            self.persistent_or_passenger,
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
            let distance = nearest_player_distance_sq(&players[mob.dim.index()], [x, y, z]);
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
        players[dim.index()].push([x, y, z]);
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
        broadcast_seen_destroy(eid);
    }
}

#[cfg(test)]
mod mob_despawn_tests {
    use super::*;
    use crate::effect_tests::client_pair;

    // The first Java nextInt(800) for seed 923 is zero.
    const ZERO_ROLL_SEED: u64 = 923;

    fn zombie(eid: i32, pos: (f64, f64, f64)) -> Arc<Mob> {
        let past = Instant::now() - Duration::from_secs(10);
        Arc::new(Mob {
            dim: DimId::Overworld,
            eid,
            etype: name_to_etype("zombie").unwrap(),
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
        })
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
}
