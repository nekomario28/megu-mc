// 1.21.1 Entity.push(Entity) (`bsr.h(Lbsr;)V`) & LivingEntity.pushEntities() (`btn.r()V`)
// Pinned jar SHA-1: 59353fb40c36d304f2035d51e7d6e6baa98dc05c
// Official mappings: 03f8985492bda0afc0898465341eb0acef35f570
//   net.minecraft.world.entity.Entity         -> bsr
//   net.minecraft.world.entity.LivingEntity   -> btn
//   net.minecraft.world.entity.EntitySelector -> bsw
//   net.minecraft.world.level.GameRules       -> dcs (RULE_MAX_ENTITY_CRAMMING -> dcs.u, default 24)
//   net.minecraft.util.Mth                    -> ayo (absMax -> ayo.a(DD)D)

const ENTITY_PUSH_MIN_ABS_MAX: f64 = 0.009999999776482582; // 0.01F as f64
const ENTITY_PUSH_STRENGTH: f64 = 0.05000000074505806; // 0.05F as f64
const ENTITY_CRAMMING_DAMAGE: f32 = 6.0f32;
const ENTITY_CRAMMING_DEFAULT: i32 = 24;
/// `LivingEntity.hurt`'s invulnerability window, in ticks.
///
/// `pushEntities()` does not damage directly: it calls `this.hurt(cramming(), 6.0F)`
/// (`btn.r()` offset 176 -> `btn.a(Lbrk;F)`). That method (`btn.a(Lbrk;F)`, offsets
/// 228-311) rejects a hit while `invulnerableTime > 10` (field `am:I`) *and* the hit is
/// no stronger than `lastHurt` (`bm:F`) - a 6.0 cramming hit at 6.0 `lastHurt` is
/// rejected that way - and an accepted hit sets `invulnerableTime = 20`. `LivingEntity.tick`
/// decrements the counter once per tick, so the window shuts for exactly ten ticks: a
/// stacked mob takes 6.0 damage every 10 ticks, not every time the `nextInt(4) == 0` roll
/// fires. Without this gate a pile of 25 cows damages ~6 of itself per tick, which kills
/// the pile roughly four times faster than vanilla.
const ENTITY_HURT_INVULNERABLE_TICKS: u64 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
enum TeamCollisionRule {
    Always,
    Never,
    PushOtherTeams,
    PushOwnTeam,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct PushEntitiesStepResult {
    self_delta: [f64; 2],
    other_impulses: Vec<(usize, [f64; 2])>,
    cramming_damage: f32,
}

/// 1.21.1 `Entity.push(Entity)` (`bsr.h(Lbsr;)V`, offsets 24–121) horizontal impulse magnitude.
/// Returns `Some([dx, dz])` where `self` receives `(-dx, -dz)` and `other` receives `(+dx, +dz)`.
#[inline]
fn entity_push_delta(self_x: f64, self_z: f64, other_x: f64, other_z: f64) -> Option<[f64; 2]> {
    let mut dx = other_x - self_x;
    let mut dz = other_z - self_z;
    let mut d = dx.abs().max(dz.abs());
    if d >= ENTITY_PUSH_MIN_ABS_MAX {
        d = d.sqrt();
        dx /= d;
        dz /= d;
        let scale = (1.0 / d).min(1.0);
        dx *= scale;
        dz *= scale;
        dx *= ENTITY_PUSH_STRENGTH;
        dz *= ENTITY_PUSH_STRENGTH;
        Some([dx, dz])
    } else {
        None
    }
}

/// 1.21.1 `Entity.push(Entity)` (`bsr.h(Lbsr;)V`, offsets 0–169) full pair evaluation.
/// Returns `(self_delta, other_delta)`.
#[inline]
fn entity_push_pair(
    self_ent: &SpatialEntity,
    other_ent: &SpatialEntity,
    same_vehicle: bool,
) -> ([f64; 2], [f64; 2]) {
    if same_vehicle || self_ent.no_physics || other_ent.no_physics {
        return ([0.0, 0.0], [0.0, 0.0]);
    }
    let Some([dx, dz]) = entity_push_delta(self_ent.x, self_ent.z, other_ent.x, other_ent.z) else {
        return ([0.0, 0.0], [0.0, 0.0]);
    };
    let s_delta = if !self_ent.is_vehicle && self_ent.is_pushable {
        [-dx, -dz]
    } else {
        [0.0, 0.0]
    };
    let o_delta = if !other_ent.is_vehicle && other_ent.is_pushable {
        [dx, dz]
    } else {
        [0.0, 0.0]
    };
    (s_delta, o_delta)
}

/// 1.21.1 `EntitySelector.pushableBy(Entity)` (`bsw.a(Lbsr;)Predicate`).
///
/// `bsw.a(bsr)` = `bsw.c(candidate).and(<team/spectator lambda>)`, where `bsw.c` is
/// `!candidate.isSpectator()`. The lambda (`bsw.a(Lbsr;Leyi;Leyi$a;Lbsr;)Z`) reads, in
/// order: `!candidate.isPushable()` -> false; client-side spectator player -> false;
/// candidate's collision rule `NEVER` -> false; then the two team conditions, which are
/// *both* denies: `(PUSH_OTHER_TEAMS on either side) && !sameTeam -> false`, and
/// `(PUSH_OWN_TEAM on either side) && sameTeam -> false` (`eyi$a` order is ALWAYS, NEVER,
/// PUSH_OTHER_TEAMS (`c`), PUSH_OWN_TEAM (`d`)). The predicate is unused at runtime today
/// (no team storage is wired, so every entity is `ALWAYS`), but the shape has to match the
/// bytecode or a later team slice inherits the inversion.
#[inline]
fn entity_selector_pushable_by(
    pusher: &SpatialEntity,
    candidate: &SpatialEntity,
    same_vehicle: bool,
    pusher_rule: TeamCollisionRule,
    candidate_rule: TeamCollisionRule,
    same_team: bool,
) -> bool {
    if pusher_rule == TeamCollisionRule::Never {
        return false;
    }
    if candidate.eid == pusher.eid || candidate.is_spectator || !candidate.is_pushable {
        return false;
    }
    if same_vehicle {
        return false;
    }
    if candidate_rule == TeamCollisionRule::Never {
        return false;
    }
    // `(rule == c || rule2 == c) && flag -> false` (offsets 114-135).
    if (pusher_rule == TeamCollisionRule::PushOtherTeams
        || candidate_rule == TeamCollisionRule::PushOtherTeams)
        && !same_team
    {
        return false;
    }
    // `(rule == d || rule2 == d) && flag -> false` (offsets 92-113).
    if (pusher_rule == TeamCollisionRule::PushOwnTeam
        || candidate_rule == TeamCollisionRule::PushOwnTeam)
        && same_team
    {
        return false;
    }
    true
}

/// Shared owner for 1.21.1 `LivingEntity.pushEntities()` (`btn.r()V`, offsets 43–216).
fn push_entities_step_indexed(
    pusher: &SpatialEntity,
    entities: &[SpatialEntity],
    candidate_slots: &[u32],
    max_cramming: i32,
    roll_cramming: &mut impl FnMut() -> u64,
    pushable_buf: &mut Vec<u32>,
    out_impulses: &mut Vec<(usize, [f64; 2])>,
) -> ([f64; 2], f32) {
    pushable_buf.clear();
    out_impulses.clear();
    for &slot in candidate_slots {
        let cand = &entities[slot as usize];
        if cand.aabb.intersects(pusher.aabb)
            && entity_selector_pushable_by(
                pusher,
                cand,
                false,
                TeamCollisionRule::Always,
                TeamCollisionRule::Always,
                false,
            )
        {
            pushable_buf.push(slot);
        }
    }
    if pushable_buf.is_empty() {
        return ([0.0, 0.0], 0.0);
    }

    let cramming_damage = if max_cramming > 0
        && pushable_buf.len() > (max_cramming - 1) as usize
        && (roll_cramming() % 4) == 0
    {
        let non_passengers = pushable_buf
            .iter()
            .filter(|&&slot| !entities[slot as usize].is_passenger)
            .count();
        if non_passengers > (max_cramming - 1) as usize {
            ENTITY_CRAMMING_DAMAGE
        } else {
            0.0
        }
    } else {
        0.0
    };

    let mut self_delta = [0.0f64; 2];
    for &slot in pushable_buf.iter() {
        let cand = &entities[slot as usize];
        // LivingEntity.doPush(Entity) (`btn.E(Lbsr;)V`) calls `entity.push(this)`.
        let (cand_d, push_d) = entity_push_pair(cand, pusher, false);
        self_delta[0] += push_d[0];
        self_delta[1] += push_d[1];
        if cand_d[0] != 0.0 || cand_d[1] != 0.0 {
            out_impulses.push((slot as usize, cand_d));
        }
    }
    (self_delta, cramming_damage)
}

#[allow(dead_code)]
fn push_entities_step_with(
    pusher: &SpatialEntity,
    candidates: &[SpatialEntity],
    max_cramming: i32,
    cram_roll: u64,
) -> PushEntitiesStepResult {
    let slots: Vec<u32> = (0..candidates.len() as u32).collect();
    let mut pushable_buf = Vec::new();
    let mut other_impulses = Vec::new();
    let mut roll = || cram_roll;
    let (self_delta, cramming_damage) = push_entities_step_indexed(
        pusher,
        candidates,
        &slots,
        max_cramming,
        &mut roll,
        &mut pushable_buf,
        &mut other_impulses,
    );
    PushEntitiesStepResult {
        self_delta,
        other_impulses,
        cramming_damage,
    }
}

#[inline]
fn clamp_entity_velocity_i16(v: f64) -> i16 {
    (v.clamp(-3.9, 3.9) * 8000.0) as i16
}

/// One mob's `LivingEntity.hurt` window: `true` when a hit is accepted at tick `now`, and
/// an accepted hit restarts the window. A rejected hit leaves the window alone, exactly like
/// `btn.a(Lbrk;F)` returning `false` before touching `invulnerableTime`.
#[inline]
fn hurt_window_accepts(last_hurt_tick: &mut Option<u64>, now: u64) -> bool {
    if let Some(prev) = *last_hurt_tick {
        if now.saturating_sub(prev) < ENTITY_HURT_INVULNERABLE_TICKS {
            return false;
        }
    }
    *last_hurt_tick = Some(now);
    true
}

fn apply_entity_push_step(
    mobs_snap: &[Arc<Mob>],
    cls_snap: &[Arc<Client>],
    region: &Region,
    grid: &EntitySpatialGrid,
) -> Vec<bool> {
    let mut dead_by_cramming = vec![false; mobs_snap.len()];
    if mobs_snap.is_empty() {
        return dead_by_cramming;
    }
    let max_cramming = GR_MAX_ENTITY_CRAMMING.load(Ordering::Relaxed);
    let mut mob_deltas = vec![[0.0f64; 2]; mobs_snap.len()];
    let mut client_deltas = vec![[0.0f64; 2]; cls_snap.len()];
    let mut cram_dmgs = vec![0.0f32; mobs_snap.len()];

    let mut scratch = SpatialScratch::default();
    let mut cand_slots = Vec::with_capacity(16);
    let mut pushable_buf = Vec::with_capacity(16);
    let mut out_impulses = Vec::with_capacity(16);

    for mob_slot in 0..mobs_snap.len() {
        let pusher = &grid.entities[mob_slot];
        if !pusher.is_mob
            || !pusher.is_pushable
            || pusher.no_physics
            || !region.is_hot(pusher.x as i32, pusher.z as i32)
        {
            continue;
        }
        grid.query_aabb_into(pusher.aabb, pusher.eid, &mut scratch, &mut cand_slots);
        if cand_slots.is_empty() {
            continue;
        }
        let mut roll = || world_rng::next_u64();
        let (self_delta, cram_dmg) = push_entities_step_indexed(
            pusher,
            &grid.entities,
            &cand_slots,
            max_cramming,
            &mut roll,
            &mut pushable_buf,
            &mut out_impulses,
        );
        mob_deltas[pusher.idx][0] += self_delta[0];
        mob_deltas[pusher.idx][1] += self_delta[1];
        cram_dmgs[pusher.idx] = cram_dmg;
        for &(cslot, cdelta) in &out_impulses {
            let cand = &grid.entities[cslot];
            if cand.is_mob {
                mob_deltas[cand.idx][0] += cdelta[0];
                mob_deltas[cand.idx][1] += cdelta[1];
            } else {
                client_deltas[cand.idx][0] += cdelta[0];
                client_deltas[cand.idx][1] += cdelta[1];
            }
        }
    }

    for (idx, m) in mobs_snap.iter().enumerate() {
        let dmg = cram_dmgs[idx];
        // `this.hurt(cramming(), 6.0F)`: the invulnerability window can reject the hit
        // outright, and a rejected hit does not stop the entity moving this tick.
        let accepted = if dmg > 0.0 {
            let mut last = m.last_hurt_tick.lock().unwrap();
            hurt_window_accepts(&mut last, TICK.load(Ordering::Relaxed))
        } else {
            false
        };
        if accepted {
            let mut hp = m.hp.lock().unwrap();
            *hp -= dmg;
            let dead = *hp <= 0.0;
            drop(hp);
            broadcast_seen(m.eid, |_c| (0x24, hurt_anim(m.eid)));
            if dead {
                let (mx, my, mz, _, _) = *m.pos.lock().unwrap();
                mob_env_die(m.eid, m.etype, mx, my, mz, *m.fuse.lock().unwrap() > 0);
                dead_by_cramming[idx] = true;
                continue;
            }
        }
        let [dx, dz] = mob_deltas[idx];
        if dx != 0.0 || dz != 0.0 {
            let mut mp = m.pos.lock().unwrap();
            let (prev_x, prev_z) = (mp.0, mp.2);
            mob_collide_move(m, &mut mp, dx, dz);
            if (mp.0 - prev_x).abs() > 1e-9 || (mp.2 - prev_z).abs() > 1e-9 {
                let (x, y, z, yaw, pitch) = *mp;
                drop(mp);
                let on_ground = mob_has_support(m.etype, x, y, z);
                broadcast_seen(m.eid, |_c| {
                    (0x70, entity_teleport(m.eid, x, y, z, yaw, pitch, on_ground))
                });
            }
        }
    }

    for (idx, cl) in cls_snap.iter().enumerate() {
        let [dx, dz] = client_deltas[idx];
        if dx != 0.0 || dz != 0.0 {
            let vx = clamp_entity_velocity_i16(dx);
            let vz = clamp_entity_velocity_i16(dz);
            if vx != 0 || vz != 0 {
                let pkt = velocity_pkt(cl.eid, vx, 0, vz);
                if let Ok(mut w) = cl.sock.lock() {
                    let _ = send_packet(&mut w, 0x5a, pkt);
                }
            }
        }
    }

    dead_by_cramming
}

#[cfg(test)]
mod entity_push_tests {
    use super::*;

    struct PushTestWorldSnapshot {
        world: HashMap<(i32, i32), HashMap<u32, u32>>,
        mobs: Vec<Arc<Mob>>,
        drops: Vec<Drop>,
        region: Region,
        clients: Vec<Arc<Client>>,
        cramming: i32,
        tick: u64,
        rng: u64,
    }

    impl PushTestWorldSnapshot {
        fn take() -> Self {
            Self {
                world: std::mem::take(&mut *world().lock().unwrap()),
                mobs: std::mem::take(&mut *mobs().lock().unwrap()),
                drops: std::mem::take(&mut *drops().lock().unwrap()),
                region: std::mem::take(&mut *region().lock().unwrap()),
                clients: std::mem::take(&mut *clients().lock().unwrap()),
                cramming: GR_MAX_ENTITY_CRAMMING.load(Ordering::Relaxed),
                tick: TICK.load(Ordering::Relaxed),
                rng: world_rng::saved_state(),
            }
        }
    }

    impl std::ops::Drop for PushTestWorldSnapshot {
        fn drop(&mut self) {
            world_rng::restore(self.rng);
            TICK.store(self.tick, Ordering::Relaxed);
            GR_MAX_ENTITY_CRAMMING.store(self.cramming, Ordering::Relaxed);
            *clients().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.clients);
            *region().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.region);
            *drops().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.drops);
            *mobs().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.mobs);
            *world().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.world);
        }
    }

    fn make_spatial(
        idx: usize,
        eid: i32,
        x: f64,
        y: f64,
        z: f64,
        pushable: bool,
        spectator: bool,
        passenger: bool,
        vehicle: bool,
    ) -> SpatialEntity {
        SpatialEntity {
            idx,
            eid,
            is_mob: true,
            is_spectator: spectator,
            is_pushable: pushable,
            is_passenger: passenger,
            is_vehicle: vehicle,
            no_physics: spectator,
            etype: 22,
            aabb: EntityAabb::from_dimensions(x, y, z, 0.9, 1.4),
            x,
            z,
        }
    }

    fn spawn_test_mob(eid: i32, etype: i64, x: f64, y: f64, z: f64, hp: f32) -> Arc<Mob> {
        Arc::new(Mob {
            eid,
            uuid: [eid as u8; 16],
            etype,
            pos: Mutex::new((x, y, z, 0.0, 0.0)),
            hp: Mutex::new(hp),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            last_hurt_tick: Mutex::new(None),
            fuse: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
        })
    }

    fn make_dummy_client(eid: i32, x: f64, y: f64, z: f64, gm: u8) -> (Arc<Client>, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let (peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let client = Arc::new(Client {
            eid,
            uuid: [9u8; 16],
            name: format!("push_tester_{eid}"),
            pos: Mutex::new((x, y, z, 0.0, 0.0, true)),
            hp: Mutex::new(20.0),
            sock: Mutex::new(stream),
            inv: Mutex::new(HashMap::new()),
            cursor: Mutex::new((0, 0, Vec::new())),
            drag: Mutex::new((0, Vec::new())),
            open_chest: Mutex::new(None),
            chest_sync: Mutex::new(None),
            open_furnace: Mutex::new(None),
            craft: Mutex::new(HashMap::new()),
            craft9: Mutex::new(HashMap::new()),
            open_table: Mutex::new(false),
            open_merchant: Mutex::new(false),
            trade_sel: Mutex::new(0),
            merchant_in: Mutex::new([(0, 0, Vec::new()), (0, 0, Vec::new())]),
            bow_draw: Mutex::new(None),
            dig: Mutex::new(None),
            eat_start: Mutex::new(None),
            blocking: Mutex::new(false),
            sprinting: Mutex::new(false),
            sneaking: Mutex::new(false),
            last_melee: Mutex::new(Instant::now() - Duration::from_secs(10)),
            fall_fly: Mutex::new(false),
            invulnerable: Mutex::new(0),
            last_hurt: Mutex::new(0.0),
            shield_cd: Mutex::new(Instant::now() - Duration::from_secs(10)),
            absorb: Mutex::new(0.0),
            pearl_cd: Mutex::new(Instant::now() - Duration::from_secs(10)),
            xp: Mutex::new(0),
            skin: Mutex::new(None),
            seen: Mutex::new(HashSet::new()),
            chunks: Mutex::new(HashSet::from([(-1, -1), (-1, 0), (0, -1), (0, 0)])),
            chunk_ack: Mutex::new(ChunkFeedback::new(1)),
            gen_sources: Mutex::new(GenSources::default()),
            gm: Mutex::new(gm),
            fall_from: Mutex::new(y),
            food: Mutex::new(20),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
            armor_pts: Mutex::new(0),
            sat: Mutex::new(5.0),
            walk_acc: Mutex::new(0.0),
            lastp: Mutex::new((x, y, z)),
            spawn_pt: Mutex::new(None),
            held: Mutex::new(0),
            effs: Mutex::new(Vec::new()),
        });
        (client, peer)
    }

    #[test]
    fn entity_push_exact_constants_sign_clamp_and_guards() {
        // Exact overlap (absMax == 0) and below 0.01F threshold are no-ops.
        assert_eq!(entity_push_delta(1.0, 2.0, 1.0, 2.0), None);
        assert_eq!(entity_push_delta(1.0, 2.0, 1.0099, 2.005), None);

        // At exact 0.01F boundary: d = sqrt(0.01F), 1/d > 1.0 clamped to 1.0.
        let min_d = f64::from(0.01f32);
        let at_thresh = entity_push_delta(0.0, 0.0, min_d, 0.0).unwrap();
        let expected_at_thresh = (min_d / min_d.sqrt()) * f64::from(0.05f32);
        assert!((at_thresh[0] - expected_at_thresh).abs() < 1e-15);
        assert_eq!(at_thresh[1], 0.0);

        // d = 0.64 -> sqrt = 0.8 -> 1/d = 1.25 clamped to 1.0.
        let clamped = entity_push_delta(0.0, 0.0, 0.64, 0.0).unwrap();
        let expected_clamped = (0.64 / 0.8) * f64::from(0.05f32);
        assert!((clamped[0] - expected_clamped).abs() < 1e-15);

        // d = 1.44 -> sqrt = 1.2 -> 1/d = 1/1.2 < 1.0 (unclamped!).
        let unclamped = entity_push_delta(0.0, 0.0, 1.44, 0.36).unwrap();
        let sqrt_144 = 1.44f64.sqrt();
        let scale = 1.0 / sqrt_144;
        let exp_x = (1.44 / sqrt_144) * scale * f64::from(0.05f32);
        let exp_z = (0.36 / sqrt_144) * scale * f64::from(0.05f32);
        assert!((unclamped[0] - exp_x).abs() < 1e-15);
        assert!((unclamped[1] - exp_z).abs() < 1e-15);

        // Sign & guard rules: self receives (-dx, -dz), other receives (+dx, +dz).
        let a = make_spatial(0, 1, 0.0, 64.0, 0.0, true, false, false, false);
        let b = make_spatial(1, 2, 0.5, 64.0, 0.0, true, false, false, false);
        let (da, db) = entity_push_pair(&a, &b, false);
        assert!(da[0] < 0.0 && (da[0] + db[0]).abs() < 1e-15);

        // Spectator (noPhysics), same_vehicle, is_vehicle, and !is_pushable suppress impulse.
        let spec = make_spatial(2, 3, 0.5, 64.0, 0.0, false, true, false, false);
        assert_eq!(entity_push_pair(&a, &spec, false), ([0.0, 0.0], [0.0, 0.0]));
        assert_eq!(entity_push_pair(&a, &b, true), ([0.0, 0.0], [0.0, 0.0]));

        let veh = make_spatial(3, 4, 0.5, 64.0, 0.0, true, false, false, true);
        let (da_v, db_v) = entity_push_pair(&a, &veh, false);
        assert!(da_v[0] < 0.0);
        assert_eq!(db_v, [0.0, 0.0]);
    }

    #[test]
    fn entity_push_cramming_threshold_passengers_and_gamerule_zero() {
        let pusher = make_spatial(0, 1, 0.5, 64.0, 0.5, true, false, false, false);
        let cands_23: Vec<SpatialEntity> = (0..23)
            .map(|i| make_spatial(i + 1, 10 + i as i32, 0.6, 64.0, 0.5, true, false, false, false))
            .collect();
        // 23 overlapping pushables <= maxCramming - 1 (23): 0 cramming damage even when roll == 0.
        let res_23 = push_entities_step_with(&pusher, &cands_23, 24, 0);
        assert_eq!(res_23.cramming_damage, 0.0);
        assert_eq!(res_23.other_impulses.len(), 23);

        // 24 overlapping pushables > 23: roll == 0 deals 6.0 damage, roll != 0 deals 0.0.
        let mut cands_24 = cands_23.clone();
        cands_24.push(make_spatial(24, 99, 0.6, 64.0, 0.5, true, false, false, false));
        assert_eq!(
            push_entities_step_with(&pusher, &cands_24, 24, 0).cramming_damage,
            6.0
        );
        for non_zero_roll in [1u64, 2, 3, 5] {
            assert_eq!(
                push_entities_step_with(&pusher, &cands_24, 24, non_zero_roll).cramming_damage,
                0.0
            );
        }

        // Non-passenger count check: if 1 of the 24 is a passenger, non-passenger count == 23 -> 0 damage.
        let mut cands_24_with_passenger = cands_23;
        cands_24_with_passenger.push(make_spatial(
            24, 100, 0.6, 64.0, 0.5, true, false, true, false,
        ));
        assert_eq!(
            push_entities_step_with(&pusher, &cands_24_with_passenger, 24, 0).cramming_damage,
            0.0
        );

        // maxEntityCramming <= 0 disables cramming damage completely.
        assert_eq!(
            push_entities_step_with(&pusher, &cands_24, 0, 0).cramming_damage,
            0.0
        );
        assert_eq!(
            push_entities_step_with(&pusher, &cands_24, -1, 0).cramming_damage,
            0.0
        );
    }

    #[test]
    fn entity_push_spatial_grid_parity_across_chunk_boundaries() {
        // Place entities straddling chunk x = -16, 0, 16 boundaries and verify grid push matches brute force.
        let coords = [
            (-16.1, 64.0, 0.1),
            (-15.9, 64.0, -0.1),
            (-0.1, 64.0, 0.0),
            (0.1, 64.0, 0.1),
            (0.2, 64.0, -0.1),
            (15.95, 64.0, 15.95),
            (16.1, 64.0, 16.0),
            (16.25, 64.0, 16.1),
        ];
        let entities: Vec<SpatialEntity> = coords
            .iter()
            .enumerate()
            .map(|(i, &(x, y, z))| make_spatial(i, 500 + i as i32, x, y, z, true, false, false, false))
            .collect();
        let grid = EntitySpatialGrid::from_entities(entities.clone());
        let mut scratch = SpatialScratch::default();
        let mut cand_slots = Vec::new();
        let mut push_buf = Vec::new();
        let mut imp_buf = Vec::new();

        for ent in &entities {
            grid.query_aabb_into(ent.aabb, ent.eid, &mut scratch, &mut cand_slots);
            let mut roll1 = || 1u64;
            let (grid_self, grid_dmg) = push_entities_step_indexed(
                ent,
                &grid.entities,
                &cand_slots,
                24,
                &mut roll1,
                &mut push_buf,
                &mut imp_buf,
            );
            let brute = push_entities_step_with(ent, &entities, 24, 1);
            assert_eq!(grid_dmg, brute.cramming_damage);
            assert!(
                (grid_self[0] - brute.self_delta[0]).abs() < 1e-15
                    && (grid_self[1] - brute.self_delta[1]).abs() < 1e-15,
                "grid vs brute push mismatch for eid={}",
                ent.eid
            );
            assert_eq!(imp_buf, brute.other_impulses);
        }
    }

    #[test]
    fn entity_push_integration_mob_tick_walls_spectator_cramming_and_breeding() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _snap = PushTestWorldSnapshot::take();
        let stone = name_to_state("stone").unwrap();

        for x in -4..=12 {
            for z in -4..=12 {
                set_block(x, 63, z, stone);
                for y in 64..=68 {
                    set_block(x, y, z, 0);
                }
            }
        }
        // Solid stone wall at x = 1 (from z = -1..=1, y = 64..=66).
        for z in -1..=1 {
            set_block(1, 64, z, stone);
            set_block(1, 65, z, stone);
        }

        // 1. Two overlapping cows: free cow pushes away, cow against stone wall at x=1 stops before x=0.55.
        let cow_a = spawn_test_mob(9601, 22, 0.35, 64.0, 0.5, 10.0);
        let cow_b = spawn_test_mob(9602, 22, 0.54, 64.0, 0.5, 10.0); // half-width 0.45 -> max_x = 0.99 against wall x=1.0

        // 2. Spectator client overlapping cow_spec must NOT push cow_spec.
        let cow_spec = spawn_test_mob(9603, 22, 4.5, 64.0, 0.5, 10.0);
        let (spec_client, _spec_peer) = make_dummy_client(9691, 4.7, 64.0, 0.5, 3);

        // 3. Survival client overlapping cow_surv MUST push cow_surv and receive 0x5a velocity pkt.
        let cow_surv = spawn_test_mob(9604, 22, 8.5, 64.0, 0.5, 10.0);
        let (surv_client, mut surv_peer) = make_dummy_client(9692, 8.7, 64.0, 0.5, 0);

        // 4. Armor stand (3) & Bat (6) are non-pushable: overlapping cow_nonpush does not move.
        let armor_stand = spawn_test_mob(9605, 3, 4.5, 64.0, 4.5, 20.0);
        let cow_nonpush = spawn_test_mob(9606, 22, 4.6, 64.0, 4.5, 10.0);

        // 5. Breeding cows within 3.0 blocks using EntitySpatialGrid::query_radius_xz_into.
        let breed_a = spawn_test_mob(9607, 22, 4.5, 64.0, 8.5, 10.0);
        let breed_b = spawn_test_mob(9608, 22, 7.0, 64.0, 8.5, 10.0);
        *breed_a.love.lock().unwrap() = 20;
        *breed_b.love.lock().unwrap() = 20;

        clients()
            .lock()
            .unwrap()
            .extend([spec_client, surv_client.clone()]);
        mobs().lock().unwrap().extend([
            cow_a.clone(),
            cow_b.clone(),
            cow_spec.clone(),
            cow_surv.clone(),
            armor_stand.clone(),
            cow_nonpush.clone(),
            breed_a.clone(),
            breed_b.clone(),
        ]);

        refresh_hot();
        TICK.store(1, Ordering::Relaxed);
        mob_tick();

        let ax = cow_a.pos.lock().unwrap().0;
        let bx = cow_b.pos.lock().unwrap().0;
        assert!(ax < 0.35, "cow_a should be pushed -X away from cow_b, got {ax}");
        assert!(
            bx <= 0.55 + 1e-6,
            "cow_b must NOT penetrate stone wall at x=1.0 (max center 0.55), got {bx}"
        );
        assert!(
            (cow_spec.pos.lock().unwrap().0 - 4.5).abs() < 1e-12,
            "spectator client must NOT push cow_spec"
        );
        assert!(
            cow_surv.pos.lock().unwrap().0 < 8.5,
            "survival client must push cow_surv -X, got {}",
            cow_surv.pos.lock().unwrap().0
        );
        assert!(
            (cow_nonpush.pos.lock().unwrap().0 - 4.6).abs() < 1e-12,
            "armor stand must NOT push cow_nonpush"
        );
        // Survival client should have received a 0x5a SetEntityMotion packet.
        let pkt_len = read_varint_from_stream(&mut surv_peer);
        assert!(pkt_len > 0, "survival client must receive 0x5a push velocity packet");
        let mut body = vec![0u8; pkt_len as usize];
        std::io::Read::read_exact(&mut surv_peer, &mut body).unwrap();
        let mut r = &body[..];
        assert_eq!(read_varint(&mut r), Some(0x5a));
        assert_eq!(read_varint(&mut r), Some(9692));

        // Breeding must have spawned a baby cow + consumed love.
        assert_eq!(*breed_a.love.lock().unwrap(), 0);
        assert_eq!(*breed_b.love.lock().unwrap(), 0);
        assert!(
            mobs().lock().unwrap().len() >= 9,
            "breeding must spawn baby cow via spatial grid"
        );

        // 6. Test maxEntityCramming gamerule command + cramming hurt/death in mob_tick.
        let console_ctx = CmdCtx {
            me: None,
            x: 0.5,
            y: 64.0,
            z: 0.5,
        };
        assert_eq!(
            run_command_ctx(&console_ctx, "/gamerule maxEntityCramming", &mut None),
            "maxEntityCramming = 24"
        );
        assert_eq!(
            run_command_ctx(&console_ctx, "/gamerule maxEntityCramming 2", &mut None),
            "Gamerule maxEntityCramming is now set to: 2"
        );
        assert_eq!(GR_MAX_ENTITY_CRAMMING.load(Ordering::Relaxed), 2);
        assert!(
            run_command_ctx(&console_ctx, "/gamerule maxEntityCramming nope", &mut None)
                .starts_with("usage:")
        );

        // Reset mobs to 25 stacked cows at (0.5, 64.0, 4.5) with maxEntityCramming=24 and hp=6.0.
        // Run mob_tick until at least one cramming roll (% 4 == 0) fires and kills a cow via mob_env_die.
        assert_eq!(
            run_command_ctx(&console_ctx, "/gamerule maxEntityCramming 24", &mut None),
            "Gamerule maxEntityCramming is now set to: 24"
        );
        mobs().lock().unwrap().clear();
        drops().lock().unwrap().clear();
        for i in 0..25 {
            mobs().lock().unwrap().push(spawn_test_mob(
                9700 + i,
                22,
                0.5 + (i as f64) * 1e-4,
                64.0,
                4.5,
                6.0,
            ));
        }
        // Seed RNG deterministically so roll % 4 == 0 triggers cleanly within a few ticks.
        world_rng::restore(0x1234_5678_9abc_def0);
        for _ in 0..8 {
            TICK.fetch_add(1, Ordering::Relaxed);
            mob_tick();
            if mobs().lock().unwrap().len() < 25 {
                break;
            }
        }
        let alive_after = mobs().lock().unwrap().len();
        assert!(
            alive_after < 25,
            "25 stacked cows with maxEntityCramming=24 must suffer lethal cramming, alive={alive_after}"
        );
        assert!(
            !drops().lock().unwrap().is_empty(),
            "lethal cramming must drop mob loot via mob_env_die"
        );

        // 7. The damage goes through `LivingEntity.hurt`, so the 10-tick invulnerability window
        //    applies: no mob can lose more than one 6.0 hit inside two ticks. Without the window
        //    a pile takes damage on every tick its own `nextInt(4) == 0` roll fires.
        mobs().lock().unwrap().clear();
        drops().lock().unwrap().clear();
        for i in 0..25 {
            mobs().lock().unwrap().push(spawn_test_mob(
                9800 + i,
                22,
                0.5 + (i as f64) * 1e-4,
                64.0,
                4.5,
                20.0,
            ));
        }
        world_rng::restore(0x0f1e_2d3c_4b5a_6978);
        for _ in 0..2 {
            TICK.fetch_add(1, Ordering::Relaxed);
            mob_tick();
        }
        {
            let ms = mobs().lock().unwrap();
            assert_eq!(ms.len(), 25, "two ticks of cramming must not kill a 20 hp cow");
            for m in ms.iter() {
                let lost = 20.0 - *m.hp.lock().unwrap();
                assert!(
                    lost <= ENTITY_CRAMMING_DAMAGE + 1e-6,
                    "mob {} lost {lost} hp in two ticks; the hurt window allows one 6.0 hit",
                    m.eid
                );
            }
        }
        // The pile is still lethal, just at vanilla's rate (4 accepted hits, 10 ticks apart).
        for _ in 0..80 {
            TICK.fetch_add(1, Ordering::Relaxed);
            mob_tick();
            if mobs().lock().unwrap().len() < 25 {
                break;
            }
        }
        assert!(
            mobs().lock().unwrap().len() < 25,
            "cramming must still kill a 20 hp pile at the window-limited rate"
        );
    }

    #[test]
    fn entity_hurt_window_rejects_equal_hits_for_ten_ticks() {
        // `btn.a(Lbrk;F)`: `invulnerableTime > 10 && amount <= lastHurt` rejects, and an accepted
        // hit sets `invulnerableTime = 20`; `tick()` decrements once, so the next equal hit lands
        // ten ticks later (`> 10` is false at 20 - 10).
        let mut last = None;
        assert!(hurt_window_accepts(&mut last, 1));
        for tick in 2..=10 {
            assert!(
                !hurt_window_accepts(&mut last, tick),
                "tick {tick} is inside the window opened at tick 1"
            );
        }
        assert!(hurt_window_accepts(&mut last, 11), "tick 11 is 10 ticks after tick 1");
        assert!(!hurt_window_accepts(&mut last, 20));
        assert!(hurt_window_accepts(&mut last, 21));
        // A rejected hit must not extend the window.
        let mut last = Some(100u64);
        assert!(!hurt_window_accepts(&mut last, 109));
        assert_eq!(last, Some(100));
        assert!(hurt_window_accepts(&mut last, 110));
        // A tick counter that went backwards (a fresh test world) reads as "no time passed" and
        // keeps the window shut; fresh entities start at `None`, so only a reused mob can see this.
        let mut last = Some(500u64);
        assert!(!hurt_window_accepts(&mut last, 3));
        assert_eq!(last, Some(500));
    }

    #[test]
    fn entity_selector_team_rules_match_the_bytecode_deny_shape() {
        // `bsw.a(Lbsr;Leyi;Leyi$a;Lbsr;)Z`: PUSH_OTHER_TEAMS on either side denies a
        // *different-team* candidate (offsets 114-135: `(rule == c || rule2 == c) && !flag ->
        // false`), PUSH_OWN_TEAM on either side denies a *same-team* candidate (offsets 92-113:
        // `(rule == d || rule2 == d) && flag -> false`), and a candidate whose own rule is NEVER
        // is never pushed. The constant names read inverted; the bytecode is the authority.
        let pusher = make_spatial(0, 1, 0.0, 64.0, 0.0, true, false, false, false);
        let cand = make_spatial(1, 2, 0.5, 64.0, 0.0, true, false, false, false);
        use TeamCollisionRule::*;
        // (pusher rule, candidate rule, same team) -> pushed?
        let table = [
            (Always, Always, false, true),
            (Always, Always, true, true),
            (Never, Always, true, false),
            (Always, Never, true, false),
            (PushOtherTeams, Always, true, true),
            (PushOtherTeams, Always, false, false),
            (Always, PushOtherTeams, false, false),
            (PushOwnTeam, Always, true, false),
            (PushOwnTeam, Always, false, true),
            (Always, PushOwnTeam, true, false),
        ];
        for (pr, cr, same_team, expected) in table {
            assert_eq!(
                entity_selector_pushable_by(&pusher, &cand, false, pr, cr, same_team),
                expected,
                "pusher={pr:?} candidate={cr:?} same_team={same_team}"
            );
        }
        // The spectator/passenger/vehicle guards are independent of the team rules.
        let spec = make_spatial(2, 3, 0.5, 64.0, 0.0, true, true, false, false);
        assert!(!entity_selector_pushable_by(&pusher, &spec, false, Always, Always, false));
        assert!(!entity_selector_pushable_by(&pusher, &cand, true, Always, Always, false));
        let unpushable = make_spatial(3, 4, 0.5, 64.0, 0.0, false, false, false, false);
        assert!(!entity_selector_pushable_by(&pusher, &unpushable, false, Always, Always, false));
    }

    #[test]
    fn mob_tick_does_no_mob_work_without_a_client_so_the_tick_profile_cannot_reach_it() {
        // `profile_game_tick`'s crowded_actors / dense_actors / entity_heavy scenarios populate
        // `actors()`, `mobs()` and `drops()` but never `clients()`, and `mob_tick` returns at
        // `if cls.is_empty()` before the push step. That is why those scenarios' `mobs` stage costs
        // ~35 ns/tick with and without the spatial index: the code under test never runs there.
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _snap = PushTestWorldSnapshot::take();
        let stone = name_to_state("stone").unwrap();
        for x in -2..=2 {
            for z in -2..=2 {
                set_block(x, 63, z, stone);
            }
        }
        let cow_a = spawn_test_mob(9501, 22, 0.35, 64.0, 0.5, 10.0);
        let cow_b = spawn_test_mob(9502, 22, 0.54, 64.0, 0.5, 10.0);
        mobs().lock().unwrap().extend([cow_a.clone(), cow_b.clone()]);
        refresh_hot();
        TICK.store(1, Ordering::Relaxed);
        assert!(
            clients().lock().unwrap().is_empty(),
            "this fixture must start without a client for the gate to be the thing measured"
        );
        mob_tick();
        assert_eq!(
            *cow_a.pos.lock().unwrap(),
            (0.35, 64.0, 0.5, 0.0, 0.0),
            "with no client, mob_tick must not touch mobs at all"
        );
        // The same overlapping pair with one client present: now the loop runs.
        let (client, _peer) = make_dummy_client(9591, 0.9, 64.0, 0.5, 0);
        clients().lock().unwrap().push(client);
        refresh_hot();
        mob_tick();
        assert_ne!(
            cow_a.pos.lock().unwrap().0,
            0.35,
            "with a client present, mob_tick must do mob work"
        );
    }

    #[test]
    fn entity_push_step_cost_grid_vs_full_scan() {
        // Costs the part the spatial index replaced - discovering each mob's neighbourhood - with
        // the *same* downstream work in both arms (`push_entities_step_indexed`, so cramming
        // counting and impulse accumulation are identical; only candidate discovery differs).
        // Default test run checks all three layouts fast; set `MC_SPINE_PUSH_BENCH=1` for full sweep.
        use std::time::Instant;
        let bench_mode = std::env::var_os("MC_SPINE_PUSH_BENCH").is_some();
        let counts: &[usize] = if bench_mode { &[600, 2000] } else { &[80] };
        for (label, spacing, full_iters) in [
            ("entity_heavy_16b", 16.0f64, 200usize),
            ("pile_1b", 1.0, 200),
            ("pile_0.1b", 0.1, 100),
        ] {
            let iters = if bench_mode { full_iters } else { 1 };
            for &n in counts {
                let entities: Vec<SpatialEntity> = (0..n)
                    .map(|i| {
                        let (col, row) = (i % 20, i / 20);
                        make_spatial(
                            i,
                            40_000 + i as i32,
                            1.0 + col as f64 * spacing,
                            70.0,
                            1.0 + row as f64 * spacing,
                            true,
                            false,
                            false,
                            false,
                        )
                    })
                    .collect();
                let grid = EntitySpatialGrid::from_entities(entities.clone());
                let all_slots: Vec<u32> = (0..n as u32).collect();
                let mut scratch = SpatialScratch::default();
                let mut cands = Vec::with_capacity(64);
                let mut buf = Vec::with_capacity(64);
                let mut imp = Vec::with_capacity(64);
                let mut roll = || 1u64;
                let mut neighbours = 0usize;

                // The two discovery paths must agree, or the benchmark is comparing different work.
                for ent in entities.iter() {
                    grid.query_aabb_into(ent.aabb, ent.eid, &mut scratch, &mut cands);
                    let mut brute: Vec<u32> = all_slots
                        .iter()
                        .copied()
                        .filter(|&s| {
                            let c = &entities[s as usize];
                            c.eid != ent.eid && c.aabb.intersects(ent.aabb)
                        })
                        .collect();
                    brute.sort_unstable();
                    assert_eq!(cands, brute, "grid neighbourhood differs from the full scan");
                }

                let started = Instant::now();
                for _ in 0..iters {
                    for ent in entities.iter() {
                        grid.query_aabb_into(ent.aabb, ent.eid, &mut scratch, &mut cands);
                        neighbours = cands.len();
                        push_entities_step_indexed(
                            ent,
                            &grid.entities,
                            &cands,
                            24,
                            &mut roll,
                            &mut buf,
                            &mut imp,
                        );
                    }
                }
                let grid_ns = started.elapsed().as_nanos() as f64 / (iters * n) as f64;

                let started = Instant::now();
                for _ in 0..iters {
                    for ent in entities.iter() {
                        push_entities_step_indexed(
                            ent,
                            &grid.entities,
                            &all_slots,
                            24,
                            &mut roll,
                            &mut buf,
                            &mut imp,
                        );
                    }
                }
                let scan_ns = started.elapsed().as_nanos() as f64 / (iters * n) as f64;
                println!(
                    "PUSHCOST\t{label}\tn={n}\tgrid_ns_per_mob={grid_ns:.1}\tfull_scan_ns_per_mob={scan_ns:.1}\tratio={:.1}x\tneighbours={neighbours}",
                    scan_ns / grid_ns
                );
            }
        }
    }

    #[test]
    fn entity_push_gamerule_cramming_persists_across_save_reload() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        let _guard = world_guard();
        GR_MAX_ENTITY_CRAMMING.store(9, Ordering::Relaxed);
        let mut buf = Vec::new();
        save_world_into(&mut buf);
        assert!(buf.windows(4).any(|w| w == b"GCR1"));
        GR_MAX_ENTITY_CRAMMING.store(ENTITY_CRAMMING_DEFAULT, Ordering::Relaxed);
        load_world_from(&buf).unwrap();
        assert_eq!(GR_MAX_ENTITY_CRAMMING.load(Ordering::Relaxed), 9);

        GR_MAX_ENTITY_CRAMMING.store(ENTITY_CRAMMING_DEFAULT, Ordering::Relaxed);
        let mut default_buf = Vec::new();
        save_world_into(&mut default_buf);
        assert!(!default_buf.windows(4).any(|w| w == b"GCR1"));
        GR_MAX_ENTITY_CRAMMING.store(7, Ordering::Relaxed);
        load_world_from(&default_buf).unwrap();
        assert_eq!(
            GR_MAX_ENTITY_CRAMMING.load(Ordering::Relaxed),
            ENTITY_CRAMMING_DEFAULT
        );
    }

    fn read_varint_from_stream(s: &mut TcpStream) -> i64 {
        let mut val = 0i64;
        for shift in 0..5 {
            let mut b = [0u8; 1];
            std::io::Read::read_exact(s, &mut b).unwrap();
            val |= ((b[0] & 0x7f) as i64) << (7 * shift);
            if b[0] & 0x80 == 0 {
                return val;
            }
        }
        val
    }
}
