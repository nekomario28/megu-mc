struct Eff {
    id: i32,
    amp: i32,
    left: i32,
} // left: server ticks (TICK_MS)

// ---------- arrows ----------
struct Arrow {
    eid: i32,
    owner: i32,
    x: f64,
    y: f64,
    z: f64,
    dx: f64,
    dy: f64,
    dz: f64,
    ttl: i32,
    dmg: f32,
    fire: i32,
    punch: i32,
    kind: u8,
    loy: i64,
    ret_item: i64,
    ret_comps: Comps,
    pierce: i32,
    last_hit: i32,
    stuck: bool,
}
static ARROWS: OnceLock<Mutex<Vec<Arrow>>> = OnceLock::new();
fn arrows() -> &'static Mutex<Vec<Arrow>> {
    ARROWS.get_or_init(|| Mutex::new(Vec::new()))
}
// left: server ticks of fuse remaining. Vanilla holds the fuse in game ticks
// (80 for ordinary TNT), so a loaded world cannot shift it under tick lag.
struct Primed {
    eid: i32,
    x: f64,
    y: f64,
    z: f64,
    left: i32,
}
static PRIMED: OnceLock<Mutex<Vec<Primed>>> = OnceLock::new();
fn primed() -> &'static Mutex<Vec<Primed>> {
    PRIMED.get_or_init(|| Mutex::new(Vec::new()))
}

struct Bobber {
    eid: i32,
    owner: i32,
    x: f64,
    y: f64,
    z: f64,
    dx: f64,
    dy: f64,
    dz: f64,
    age: i32,
    bit: bool,
}
static BOBBERS: OnceLock<Mutex<Vec<Bobber>>> = OnceLock::new();
fn bobbers() -> &'static Mutex<Vec<Bobber>> {
    BOBBERS.get_or_init(|| Mutex::new(Vec::new()))
}
struct Pearl {
    eid: i32,
    owner: i32,
    x: f64,
    y: f64,
    z: f64,
    dx: f64,
    dy: f64,
    dz: f64,
    ttl: i32,
}
static PEARLS: OnceLock<Mutex<Vec<Pearl>>> = OnceLock::new();
fn pearls() -> &'static Mutex<Vec<Pearl>> {
    PEARLS.get_or_init(|| Mutex::new(Vec::new()))
}
struct Snowball {
    eid: i32,
    #[allow(dead_code)]
    owner: i32,
    x: f64,
    y: f64,
    z: f64,
    dx: f64,
    dy: f64,
    dz: f64,
    ttl: i32,
    egg: bool,
}
static SNOWBALLS: OnceLock<Mutex<Vec<Snowball>>> = OnceLock::new();
fn snowballs() -> &'static Mutex<Vec<Snowball>> {
    SNOWBALLS.get_or_init(|| Mutex::new(Vec::new()))
}

// ---------- xp orbs ----------
// age: server ticks since spawn. Vanilla counts entity age in game ticks, so a
// despawn window cannot shift when a loaded world is ticked under lag.
struct Xp {
    eid: i32,
    x: f64,
    y: f64,
    z: f64,
    val: i16,
    age: i32,
}
static XPS: OnceLock<Mutex<Vec<Xp>>> = OnceLock::new();
fn xps() -> &'static Mutex<Vec<Xp>> {
    XPS.get_or_init(|| Mutex::new(Vec::new()))
}

// vanilla 1.9+ attack cooldown: seconds per full charge by tool kind.
// sword 1.6 (0.625s); axe ~1.0s avg across tiers; hoe 1.5s; fist 0.25s.
fn attack_cooldown(item: i64) -> f64 {
    if !(818..=847).contains(&item) {
        return 0.25;
    }
    match (item - 818) % 5 {
        0 => 0.625, // sword
        1 => 1.0,   // shovel
        2 => 0.833, // pickaxe
        3 => 1.0,   // axe (0.8-1.25 by tier, 1.0 avg)
        _ => 1.5,   // hoe
    }
}

fn melee_charge(me: &Arc<Client>, item: i64) -> f32 {
    let speed = me.effs.lock().unwrap().iter().fold(1.0_f32, |speed, effect| {
        match effect.id {
            2 | 3 => speed * effects::effect_attribute_modifier(
                effect.id + 1, effect.amp, effects::EffectAttribute::AttackSpeed,
            ),
            _ => speed,
        }
    });
    let cd = attack_cooldown(item) / f64::from(speed.max(0.001));
    let mut lm = me.last_melee.lock().unwrap();
    let c = (lm.elapsed().as_secs_f64() / cd).min(1.0) as f32;
    *lm = Instant::now();
    c
}
fn enchantment_targets_undead(etype: i64) -> bool {
    entity_spec_by_id(etype).is_some_and(|entity| {
        matches!(
            entity.name.strip_prefix("minecraft:"),
            Some(
                "drowned"
                    | "husk"
                    | "phantom"
                    | "skeleton"
                    | "skeleton_horse"
                    | "stray"
                    | "wither"
                    | "wither_skeleton"
                    | "zombie"
                    | "zombie_horse"
                    | "zombie_villager"
                    | "zombified_piglin"
                    | "zoglin"
            )
        )
    })
}
fn enchantment_targets_arthropod(etype: i64) -> bool {
    entity_spec_by_id(etype).is_some_and(|entity| {
        matches!(
            entity.name.strip_prefix("minecraft:"),
            Some("bee" | "cave_spider" | "endermite" | "silverfish" | "spider")
        )
    })
}
fn melee_critical_allowed(
    charge: f32,
    grounded: bool,
    falling: bool,
    fall_flying: bool,
    sprinting: bool,
    in_water: bool,
    climbing: bool,
    blind: bool,
) -> bool {
    charge > 0.9
        && !grounded
        && falling
        && !fall_flying
        && !sprinting
        && !in_water
        && !climbing
        && !blind
}
fn fire_aspect_duration_ticks(level: i64) -> i32 {
    server_ticks(80 * level.max(0) as u64) as i32
}
fn fire_tick_update(remaining: i32, tick: u64, fire_immune: bool) -> (i32, bool) {
    let damage = remaining > 0
        && tick.is_multiple_of(server_ticks(MOB_BURN_TICK_GT))
        && !fire_immune;
    (remaining.saturating_sub(1).max(0), damage)
}

// vanilla: blocking with a shield pays floor(blocked)+1 durability
fn damage_shield(me: &Arc<Client>, blocked: f32) {
    let sh = name_to_item("shield").unwrap_or(-9);
    let slots = [45i16, 36 + *me.held.lock().unwrap()];
    let mut inv = me.inv.lock().unwrap();
    for slot in slots {
        let Some(st) = inv.get_mut(&slot) else {
            continue;
        };
        if st.0 != sh {
            continue;
        }
        if comp_get(&st.2, COMP_UNBREAKABLE).is_some() {
            return;
        }
        let unb = enchant_level(&st.2, 39);
        let maxd = comp_varint(&st.2, COMP_MAX_DAMAGE).unwrap_or(337);
        let mut add = 0i64;
        for _ in 0..(blocked.floor() as i64 + 1) {
            if unb == 0 || world_rng::next_u64().is_multiple_of((unb + 1) as u64) {
                add += 1
            }
        }
        let dmg = comp_varint(&st.2, COMP_DAMAGE).unwrap_or(0) + add;
        if dmg >= maxd {
            inv.remove(&slot);
        } else {
            comp_set_varint(&mut st.2, COMP_DAMAGE, dmg);
        }
        return;
    }
}
// vanilla hurtArmor: each worn piece pays max(1, floor(raw/4)); each
// piece rolls its own Unbreaking. Returns slots that broke (for resync).
fn damage_armor(me: &Arc<Client>, raw_dmg: f32) {
    let per = (raw_dmg / 4.0).floor().max(1.0) as i64;
    let mut broke = false;
    {
        let mut inv = me.inv.lock().unwrap();
        for slot in [5i16, 6, 7, 8] {
            let Some(st) = inv.get_mut(&slot) else {
                continue;
            };
            let Some(maxd) = comp_varint(&st.2, COMP_MAX_DAMAGE).or_else(|| armor_max_damage(st.0))
            else {
                continue;
            };
            if comp_get(&st.2, COMP_UNBREAKABLE).is_some() {
                continue;
            }
            let unb = enchant_level(&st.2, 39);
            let mut add = 0i64;
            for _ in 0..per {
                let roll = (world_rng::next_u64() >> 40) as f32 / (1u32 << 24) as f32;
                if unbreaking_consumes_durability(unb as i32, roll, true) {
                    add += 1
                }
            }
            let dmg = comp_varint(&st.2, COMP_DAMAGE).unwrap_or(0) + add;
            if dmg >= maxd {
                inv.remove(&slot);
                broke = true;
            } else {
                comp_set_varint(&mut st.2, COMP_DAMAGE, dmg);
            }
        }
        *me.armor_pts.lock().unwrap() = armor_pts_of(&inv);
    }
    if broke {
        let (x, y, z, ..) = {
            let p = me.pos.lock().unwrap();
            *p
        };
        let snd = sound_pkt_named("entity.item.break", x, y + 1.0, z);
        broadcast(-1, |_c| (0x68, snd.clone()));
    }
    // resync the four armor slots (cheap: keeps client honest)
    for slot in [5i16, 6, 7, 8] {
        let st = me
            .inv
            .lock()
            .unwrap()
            .get(&slot)
            .cloned()
            .unwrap_or((0, 0, Vec::new()));
        let mut p = vec![0u8];
        write_varint(&mut p, 0);
        p.extend_from_slice(&slot.to_be_bytes());
        p.extend_from_slice(&slot_wire(&st));
        if let Ok(mut w) = me.sock.lock() {
            let _ = send_packet(&mut w, 0x15, p);
        }
    }
}

#[cfg(test)]
fn xp_level(total: i64) -> (i32, f32) {
    let mut l = 0i32;
    let mut acc = 0i64;
    let mut next = 7i64;
    while total >= acc + next {
        acc += next;
        l += 1;
        next = xp_needed_for_next_level(l);
    }
    (l, ((total - acc) as f32 / next as f32).clamp(0.0, 1.0))
}
fn xp_orb_pkt(eid: i32, x: f64, y: f64, z: f64, count: i16) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, eid as i64);
    p.extend_from_slice(&x.to_be_bytes());
    p.extend_from_slice(&y.to_be_bytes());
    p.extend_from_slice(&z.to_be_bytes());
    p.extend_from_slice(&count.to_be_bytes());
    p
}

fn explosion_pkt(x: f64, y: f64, z: f64, records: &[(i8, i8, i8)]) -> Vec<u8> {
    explosion_pkt_with_power(x, y, z, 3.0, records)
}
fn explosion_pkt_with_power(
    x: f64, y: f64, z: f64, power: f32, records: &[(i8, i8, i8)],
) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&x.to_be_bytes());
    p.extend_from_slice(&y.to_be_bytes());
    p.extend_from_slice(&z.to_be_bytes());
    p.extend_from_slice(&power.to_be_bytes());
    write_varint(&mut p, records.len() as i64);
    for &(ox, oy, oz) in records {
        p.push(ox as u8);
        p.push(oy as u8);
        p.push(oz as u8);
    }
    p.extend_from_slice(&[0u8; 12]); // player motion xyz f32
    write_varint(&mut p, 0); // block_interaction = destroy
    write_varint(&mut p, 53); // small particle: poof
    write_varint(&mut p, 22); // large particle: explosion
    write_varint(&mut p, 577); // sound: entity.generic.explode
    p
}
// vanilla xp on death: hostile mobs 5, animals 1-3
fn mob_xp(etype: i64) -> i16 {
    if [22i64, 19, 77, 87].contains(&etype) {
        (1 + world_rng::next_u64() % 3) as i16
    } else {
        5
    }
}
// vanilla /kill applies void damage — the same loot+xp+cleanup path as
// combat death, not a silent retain
fn kill_mob(deid: i32, etype: i64, mx: f64, my: f64, mz: f64) {
    let Some((mob, state)) = remove_live_mob(deid) else { return };
    let burning = *mob.fire_ticks.lock().unwrap() > 0;
    live_mob_death(&mob, state, [mx, my, mz]);
    broadcast_seen_destroy(deid);
    let was_sheared = sheared().lock().unwrap().remove(&deid);
    for (item, cnt, comps) in mob_loot(etype, burning)
        .into_iter()
        .filter(|st| !(was_sheared && st.0 == 202))
        .filter(|_| GR_MOB_LOOT.load(Ordering::Relaxed) == 1)
    {
        let d = Drop {
            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
            x: mx,
            y: my,
            z: mz,
            item,
            cnt,
            comps,
            age: 0,
            target: None,
            thrower: None,
        };
        drops().lock().unwrap().push(d);
    }
    let xeid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
    xps().lock().unwrap().push(Xp {
        eid: xeid,
        x: mx,
        y: my,
        z: mz,
        val: mob_xp(etype),
        age: 0,
    });
    // spawn streams via interest_tick
}

fn mob_env_die(deid: i32, etype: i64, mx: f64, my: f64, mz: f64, burning: bool) {
    // environmental death (suffocation/drowning/fall): drops but no XP —
    // vanilla only drops XP when recently hurt by a player
    let Some((mob, state)) = remove_live_mob(deid) else { return };
    live_mob_death(&mob, state, [mx, my, mz]);
    broadcast_seen_destroy(deid);
    sheared().lock().unwrap().remove(&deid);
    for (item, cnt, comps) in mob_loot(etype, burning)
        .into_iter()
        .filter(|_| GR_MOB_LOOT.load(Ordering::Relaxed) == 1)
    {
        drops().lock().unwrap().push(Drop {
            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
            x: mx,
            y: my,
            z: mz,
            item,
            cnt,
            comps,
            age: 0,
            target: None,
            thrower: None,
        });
    }
}
static LOOT_SEQ: AtomicU64 = AtomicU64::new(0);
fn roll_loot(pools: &[LootPool]) -> Vec<Stack> {
    let mut out: Vec<Stack> = Vec::new();
    for pl in pools {
        let h = LOOT_SEQ
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(6364136223846793005)
            ^ TICK.load(Ordering::Relaxed);
        if pl.chance < 1.0 && (h % 1000) as f32 / 1000.0 > pl.chance {
            continue;
        }
        for r in 0..pl.rolls {
            let e = &pl.entries[((h >> (r * 8 + 3)) as usize) % pl.entries.len()];
            let span = (e.max - e.min + 1) as u64;
            let cnt = e.min + (((h >> (r * 8 + 17)) % span.max(1)) as u8);
            if cnt > 0 {
                let mut st: Stack = (e.item, cnt, Vec::new());
                apply_loot_funcs(&mut st, &pl.funcs);
                apply_loot_funcs(&mut st, &e.funcs);
                out.push(st);
            }
        }
    }
    out
}
fn roll_loot_seeded(pools: &[LootPool], seed: i64) -> Vec<Stack> {
    let mut rng = crate::generation_math::Lcg::new(seed as u64);
    let mut out: Vec<Stack> = Vec::new();
    for pl in pools {
        if pl.chance < 1.0 && rng.next_f32() > pl.chance {
            continue;
        }
        for _ in 0..pl.rolls {
            if pl.entries.is_empty() {
                continue;
            }
            let idx = rng.next_bound(pl.entries.len() as i32) as usize;
            let e = &pl.entries[idx];
            let span = (e.max - e.min + 1) as i32;
            let cnt = e.min + (rng.next_bound(span.max(1)) as u8);
            if cnt > 0 {
                let mut st: Stack = (e.item, cnt, Vec::new());
                apply_loot_funcs(&mut st, &pl.funcs);
                apply_loot_funcs(&mut st, &e.funcs);
                out.push(st);
            }
        }
    }
    out
}
fn cooked_drop(item: i64) -> Option<i64> {
    for (r, c) in [
        ("beef", "cooked_beef"),
        ("chicken", "cooked_chicken"),
        ("porkchop", "cooked_porkchop"),
        ("mutton", "cooked_mutton"),
        ("rabbit", "cooked_rabbit"),
        ("cod", "cooked_cod"),
        ("salmon", "cooked_salmon"),
        ("potato", "baked_potato"),
    ] {
        if name_to_item(r) == Some(item) {
            return name_to_item(c);
        }
    }
    None
}
fn mob_loot(etype: i64, burning: bool) -> Vec<Stack> {
    if let Some(pools) = mob_loot_tables().lock().unwrap().get(&etype).map(|p| {
        p.iter()
            .map(|pl| LootPool {
                rolls: pl.rolls,
                chance: pl.chance,
                funcs: pl.funcs.clone(),
                entries: pl
                    .entries
                    .iter()
                    .map(|e| LootEntry {
                        item: e.item,
                        min: e.min,
                        max: e.max,
                        funcs: e.funcs.clone(),
                    })
                    .collect(),
            })
            .collect::<Vec<_>>()
    }) {
        return roll_loot(&pools)
            .into_iter()
            .map(|mut st| {
                if burning {
                    if let Some(c) = cooked_drop(st.0) {
                        st.0 = c;
                    }
                }
                st
            })
            .collect();
    }
    let names: &[&str] = match etype {
        124 => &["rotten_flesh"],
        22 => &["beef", "leather"],
        23 => &["gunpowder"],
        91 => &["bone", "arrow"],
        19 => &["chicken", "feather"],
        77 => &["porkchop"],
        87 => &["mutton", "white_wool"],
        _ => &[],
    };
    names
        .iter()
        .filter_map(|n| name_to_item(n).map(|i| (i, 1u8, Vec::new())))
        .map(|mut st| {
            if burning {
                if let Some(c) = cooked_drop(st.0) {
                    st.0 = c;
                }
            }
            st
        })
        .collect()
}

// vanilla mob damage scales with difficulty: easy min(d/2+1,d),
// normal d, hard d*1.5; peaceful hostiles don't exist.
fn diff_mob_dmg(d: f32) -> f32 {
    match DIFFICULTY.load(Ordering::Relaxed) {
        0 => 0.0,
        1 => (d / 2.0 + 1.0).min(d),
        3 => d * 1.5,
        _ => d,
    }
}
// explosion damage scales differently: easy x0.5, hard x1.5
fn diff_expl_dmg(d: f32) -> f32 {
    match DIFFICULTY.load(Ordering::Relaxed) {
        0 => 0.0,
        1 => d * 0.5,
        3 => d * 1.5,
        _ => d,
    }
}
fn armor_mult(c: &Client) -> f32 {
    let pts = *c.armor_pts.lock().unwrap();
    (1.0 - 0.04 * pts as f32).max(0.2)
}
// vanilla EPF: protection-family enchantments on worn armor add a
// post-armor reduction of min(epf,20)*4%. kind: 0 generic (prot only),
// 1 fall (feather_falling 3x), 2 explosion (blast 2x), 3 projectile
// (proj 2x), 4 fire (fire_prot 2x)
fn enchant_epf(c: &Client, kind: u8) -> i64 {
    let inv = c.inv.lock().unwrap();
    let mut epf = 0i64;
    for slot in [5i16, 6, 7, 8] {
        if let Some((_, _, comps)) = inv.get(&slot) {
            epf += enchant_level(comps, 27); // protection
            match kind {
                1 => epf += 3 * enchant_level(comps, 9),  // feather_falling
                2 => epf += 2 * enchant_level(comps, 3),  // blast_protection
                3 => epf += 2 * enchant_level(comps, 26), // projectile_protection
                4 => epf += 2 * enchant_level(comps, 11), // fire_protection
                _ => {}
            }
        }
    }
    epf.min(20)
}
fn dmg_after(c: &Client, raw: f32, kind: u8) -> f32 {
    let arm = raw * armor_mult(c);
    arm * (1.0 - 0.04 * enchant_epf(c, kind) as f32)
}
// call AFTER the hp write: if hp hit 0 and a totem is held (main or
// offhand), consume it — vanilla sets hp to 1, fires entity_status 35,
// plays the totem animation+sound, grants absorption/regen/fire_resist
fn totem_check(c: &Client) {
    if *c.hp.lock().unwrap() > 0.0 || *c.gm.lock().unwrap() == 1 {
        return;
    }
    let mut inv = c.inv.lock().unwrap();
    let held_k = 36i16 + (*c.held.lock().unwrap());
    for k in [45i16, held_k] {
        if inv.get(&k).map(|v| v.0) == Some(1163) {
            let st = inv.get(&k).unwrap().clone();
            if st.1 <= 1 {
                inv.remove(&k);
            } else {
                inv.insert(k, (st.0, st.1 - 1, st.2));
            }
            let now = inv.get(&k).cloned().unwrap_or((0, 0, Vec::new()));
            drop(inv);
            *c.hp.lock().unwrap() = 1.0;
            resync_slot(c, 0, k, &now);
            // entity_status 35 = totem use; i32 entity id + i8 status
            let mut st_pkt = Vec::new();
            st_pkt.extend_from_slice(&c.eid.to_be_bytes());
            st_pkt.push(35);
            let _ = send_packet(&mut c.sock.lock().unwrap(), 0x1f, st_pkt.clone());
            broadcast_seen(c.eid, |_c| (0x1f, st_pkt.clone()));
            let (px, py, pz, ..) = {
                let p = c.pos.lock().unwrap();
                *p
            };
            let snd = sound_pkt_named("item.totem.use", px, py, pz);
            broadcast_seen(c.eid, |_c| (0x68, snd.clone()));
            // Death protection replaces existing effects with the vanilla grant set.
            grant_totem_effects(c, &mut c.effs.lock().unwrap());
            let _ = send_packet(&mut c.sock.lock().unwrap(), 0x5d, health_pkt_c(1.0, c));
            return;
        }
    }
    drop(inv);
}
// Both death-protection paths own the effect guard; do not acquire it here.
fn totem_effects(ef: &mut Vec<Eff>) {
    ef.clear();
    // Vanilla grants 900 / 100 / 800 game ticks. At 20TPS a server tick is a game
    // tick, so these are also the stored server-tick values and the wire takes
    // them unchanged; there is no conversion left to apply.
    for (name, amp, dur) in [
        ("regeneration", 1, 900),
        ("absorption", 1, 100),
        ("fire_resistance", 0, 800),
    ] {
        let id = name_to_effect(name).expect("totem effect must exist in the effect registry");
        ef.push(Eff {
            id,
            amp,
            left: server_ticks(dur) as i32,
        });
    }
}
fn grant_totem_effects(c: &Client, ef: &mut Vec<Eff>) {
    for e in ef.iter() {
        let packet = remove_effect_pkt(c.eid, e.id);
        if let Ok(mut w) = c.sock.lock() {
            let _ = send_packet(&mut w, 0x43, packet.clone());
        }
        broadcast_seen(c.eid, |_| (0x43, packet.clone()));
    }
    totem_effects(ef);
    *c.absorb.lock().unwrap() = 8.0;
    for e in ef.iter() {
        // The wire uses game ticks even while the simulation stores server ticks.
        let packet = entity_effect_pkt(c.eid, e.id, e.amp, e.left);
        if let Ok(mut w) = c.sock.lock() {
            let _ = send_packet(&mut w, 0x76, packet.clone());
        }
        broadcast_seen(c.eid, |_| (0x76, packet.clone()));
    }
}
fn armor_pts_of(inv: &HashMap<i16, Stack>) -> i32 {
    let mut pts = 0i32;
    let slots: [(i16, &[(&str, i32)]); 4] = [
        (
            5,
            &[
                ("leather_helmet", 1),
                ("golden_helmet", 2),
                ("chainmail_helmet", 2),
                ("iron_helmet", 2),
                ("diamond_helmet", 3),
                ("netherite_helmet", 3),
                ("turtle_helmet", 2),
            ],
        ),
        (
            6,
            &[
                ("leather_chestplate", 3),
                ("golden_chestplate", 5),
                ("chainmail_chestplate", 5),
                ("iron_chestplate", 6),
                ("diamond_chestplate", 8),
                ("netherite_chestplate", 8),
                ("elytra", 0),
            ],
        ),
        (
            7,
            &[
                ("leather_leggings", 2),
                ("golden_leggings", 3),
                ("chainmail_leggings", 4),
                ("iron_leggings", 5),
                ("diamond_leggings", 6),
                ("netherite_leggings", 6),
            ],
        ),
        (
            8,
            &[
                ("leather_boots", 1),
                ("golden_boots", 1),
                ("chainmail_boots", 1),
                ("iron_boots", 2),
                ("diamond_boots", 3),
                ("netherite_boots", 3),
            ],
        ),
    ];
    for (slot, mats) in slots {
        if let Some((item, _, _)) = inv.get(&slot) {
            for (n, p) in mats {
                if name_to_item(n) == Some(*item) {
                    pts += p
                }
            }
        }
    }
    pts
}

/// Vanilla merges xp orbs closer than one block into a single higher-value orb.
///
/// Returns the eaten eids in the order the merge produced them, which is the order
/// the destroy packet goes out in, so it is part of the contract rather than an
/// artifact. Duplicates are possible and are kept: an orb eaten by an earlier `i`
/// is still a candidate for a later one, because the removal happens after the
/// whole scan (`retain` once, not per pair).
///
/// Bucketed by integer cell. Two positions less than 1.0 apart differ by at most
/// one on each axis after `floor`, so only the own cell and its 26 neighbours can
/// satisfy `dist^2 < 1.0`; a cell whose coordinates are at the i32 limit has no
/// neighbour there and `checked_add` says so rather than wrapping into a cell that
/// aliases. Iterating `i` in order, collecting its candidates in ascending index
/// order and breaking on the first match is the same sequence of comparisons the
/// full pairwise scan made, so which orb is eaten does not change. Sorting the orbs
/// first, or visiting candidates in any other order, *would* change it.
fn merge_xp_orbs(xl: &mut [Xp]) -> Vec<i32> {
    // Nothing to pair, and the index below allocates: an idle world must not pay for a
    // merge it cannot perform.
    if xl.len() < 2 {
        return Vec::new();
    }
    let mut buckets = CellBuckets::new(xl.len());
    for (idx, x) in xl.iter().enumerate() {
        buckets.push(
            (x.x.floor() as i32, x.y.floor() as i32, x.z.floor() as i32),
            idx as u32,
        );
    }
    let mut merged: Vec<i32> = Vec::new();
    let mut candidates: Vec<usize> = Vec::new();
    for i in 0..xl.len() {
        let (cx, cy, cz) = (
            xl[i].x.floor() as i32,
            xl[i].y.floor() as i32,
            xl[i].z.floor() as i32,
        );
        candidates.clear();
        for dx in -1..=1i32 {
            for dy in -1..=1i32 {
                for dz in -1..=1i32 {
                    let key = (
                        cx.checked_add(dx),
                        cy.checked_add(dy),
                        cz.checked_add(dz),
                    );
                    if let (Some(kx), Some(ky), Some(kz)) = key {
                        collect_members(&buckets, (kx, ky, kz), i, &mut candidates);
                    }
                }
            }
        }
        candidates.sort_unstable();
        for &j in candidates.iter() {
            let (ax, ay, az, aage) = {
                let a = &xl[i];
                (a.x, a.y, a.z, a.age)
            };
            let (bx, by, bz, bage) = {
                let b = &xl[j];
                (b.x, b.y, b.z, b.age)
            };
            if (ax - bx).powi(2) + (ay - by).powi(2) + (az - bz).powi(2) < 1.0 {
                let (keep, eat) = if aage <= bage { (i, j) } else { (j, i) };
                xl[keep].val = xl[keep].val.saturating_add(xl[eat].val);
                merged.push(xl[eat].eid);
                break;
            }
        }
    }
    merged
}

/// Finite segment geometry. Unlike Valence's inclusive infinite ray, pinned
/// AABB.clip tests entering faces, with epsilon on transverse bounds and open t.
struct Aabb {
    min: [f64; 3],
    max: [f64; 3],
}
impl Aabb {
    fn new(a: [f64; 3], b: [f64; 3]) -> Self {
        Self {
            min: std::array::from_fn(|i| a[i].min(b[i])),
            max: std::array::from_fn(|i| a[i].max(b[i])),
        }
    }
    fn contains(&self, point: [f64; 3]) -> bool {
        (0..3).all(|i| self.min[i] <= point[i] && point[i] < self.max[i])
    }
    fn clip(&self, start: [f64; 3], end: [f64; 3]) -> Option<[f64; 3]> {
        const EPSILON: f64 = 1.0e-7;
        let delta: [f64; 3] = std::array::from_fn(|i| end[i] - start[i]);
        let mut nearest = 1.0;
        let mut hit = false;
        for axis in 0..3 {
            let face = if delta[axis] > EPSILON {
                self.min[axis]
            } else if delta[axis] < -EPSILON {
                self.max[axis]
            } else {
                continue;
            };
            let t = (face - start[axis]) / delta[axis];
            let j = (axis + 1) % 3;
            let k = (axis + 2) % 3;
            let p = start[j] + t * delta[j];
            let q = start[k] + t * delta[k];
            if 0.0 < t
                && t < nearest
                && self.min[j] - EPSILON < p
                && p < self.max[j] + EPSILON
                && self.min[k] - EPSILON < q
                && q < self.max[k] + EPSILON
            {
                nearest = t;
                hit = true;
            }
        }
        hit.then(|| std::array::from_fn(|i| start[i] + nearest * delta[i]))
    }
}

/// Keep the existing approximate target boxes and start-inside contact policy;
/// only sampling is retired here, not pose dimensions or nearest-target ordering.
fn projectile_segment_hits(
    start: [f64; 3],
    end: [f64; 3],
    target: (f64, f64, f64),
    rx: f64,
    y_lo: f64,
    y_hi: f64,
) -> bool {
    let bounds = Aabb::new(
        [target.0 - rx, target.1 - y_lo, target.2 - rx],
        [target.0 + rx, target.1 + y_hi, target.2 + rx],
    );
    bounds.contains(start) || bounds.clip(start, end).is_some()
}

fn arrow_tick() {
    let mut dead: Vec<i32> = vec![];
    let mut hits: Vec<(i32, f32)> = vec![]; // (victim eid, arrow dmg)
    let mut mob_dead: Vec<(i32, i64, f64, f64, f64)> = vec![];
    {
        let mut ar = arrows().lock().unwrap();
        let region = region_guard();
        for a in ar.iter_mut() {
            let eid = a.eid;
            if !region.is_hot(a.x as i32, a.z as i32) {
                continue;
            }
            if a.stuck {
                a.ttl -= 1;
                if a.ttl <= 0 {
                    dead.push(eid);
                    continue;
                }
                // Vanilla: a stuck arrow is picked up on contact (survival only).
                let (ax, ay, az) = (a.x, a.y, a.z);
                let grab = {
                    clients()
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|c| *c.gm.lock().unwrap() != 1)
                        .find(|c| {
                            let p = c.pos.lock().unwrap();
                            let (dx, dy, dz) = (p.0 - ax, (p.1 + 0.9) - ay, p.2 - az);
                            dx * dx + dy * dy + dz * dz < 1.3
                        })
                        .cloned()
                };
                if let Some(c) = grab {
                    let mut inv = c.inv.lock().unwrap();
                    let slot: i16 = (9..=44i16)
                        .find(|sl| {
                            inv.get(sl)
                                .map(|(i, c2, _)| *i == 802 && *c2 < 64)
                                .unwrap_or(false)
                        })
                        .or_else(|| (9..=44).find(|sl| !inv.contains_key(sl)))
                        .unwrap_or(9);
                    let mut st = inv.get(&slot).cloned().unwrap_or((802, 0, Vec::new()));
                    st.1 += 1;
                    inv.insert(slot, st);
                    drop(inv);
                    let st =
                        c.inv
                            .lock()
                            .unwrap()
                            .get(&slot)
                            .cloned()
                            .unwrap_or((802, 1, Vec::new()));
                    let mut p = vec![0u8];
                    write_varint(&mut p, 0);
                    p.extend_from_slice(&slot.to_be_bytes());
                    p.extend_from_slice(&slot_wire(&st));
                    if let Ok(mut w) = c.sock.lock() {
                        let _ = send_packet(&mut w, 0x15, p);
                    }
                    dead.push(eid);
                }
                continue;
            }
            // vanilla AbstractArrow: scale the velocity by 0.99, add -0.05, then
            // integrate position from that velocity. Applying drag to the
            // displacement instead would leave the horizontal velocity undamped.
            a.dx *= 0.99;
            a.dy *= 0.99;
            a.dz *= 0.99;
            a.dy -= 0.05;
            a.x += a.dx;
            a.y += a.dy;
            a.z += a.dz;
            let (ax, ay, az) = (a.x, a.y, a.z);
            let segment_start = [ax - a.dx, ay - a.dy, az - a.dz];
            let segment_end = [ax, ay, az];
            // block collision: arrows stick into solid blocks (vanilla)
            let bst = block_at(ax.floor() as i32, ay.floor() as i32, az.floor() as i32);
            if bst != 0 && !mob_passable(bst) {
                // tridents stick like arrows in vanilla too, but our trident
                // return path handles its own landing — only plain arrows stick
                if a.kind == 1 {
                    dead.push(eid);
                } else {
                    a.stuck = true;
                    a.ttl = server_ticks(ARROW_TTL_GT) as i32;
                    a.dx = 0.0;
                    a.dy = 0.0;
                    a.dz = 0.0;
                }
                continue;
            }
            broadcast_seen(eid, |_c| {
                (0x70, entity_teleport(eid, ax, ay, az, 0.0, 0.0, true))
            });
            if a.owner >= 0 {
                // Player-shot arrow: intersect the whole segment with mobs and
                // other players. The owner and last pierced target are skipped.
                let ml = mobs().lock().unwrap();
                'outer: for m in ml.iter() {
                    if m.eid == a.last_hit || m.dim != DimId::Overworld {
                        continue;
                    }
                    let (mx, my, mz, ..) = *m.pos.lock().unwrap();
                    if projectile_segment_hits(
                        segment_start,
                        segment_end,
                        (mx, my, mz),
                        1.2,
                        0.4,
                        2.1,
                    ) {
                        let kind = if a.kind == 0 { BossHit::Arrow } else { BossHit::Other };
                        if !live_mob_hit_allowed(m, a.owner, kind) {
                            dead.push(eid);
                            break;
                        }
                        *m.hurt_at.lock().unwrap() = Instant::now();
                        let mut h = m.hp.lock().unwrap();
                        *h -= a.dmg;
                        let dh = *h;
                        drop(h);
                        if a.fire > 0 && !mob_fire_immune(m.etype) {
                            *m.fire_ticks.lock().unwrap() = a.fire;
                        }
                        if a.punch > 0 && a.kind == 0 {
                            let vl = (a.dx * a.dx + a.dz * a.dz).sqrt().max(0.01);
                            let kb = 0.3 * a.punch as f64;
                            let mut p = m.pos.lock().unwrap();
                            p.0 += a.dx / vl * kb;
                            p.2 += a.dz / vl * kb;
                            drop(p);
                            broadcast_seen(m.eid, |_c| {
                                (
                                    0x70,
                                    entity_teleport(
                                        m.eid,
                                        m.pos.lock().unwrap().0,
                                        m.pos.lock().unwrap().1,
                                        m.pos.lock().unwrap().2,
                                        m.pos.lock().unwrap().3,
                                        m.pos.lock().unwrap().4,
                                        true,
                                    ),
                                )
                            });
                        }
                        broadcast_seen(m.eid, |_c| (0x24, hurt_anim(m.eid)));
                        // channeling: trident hit in thunder calls lightning
                        // (punch slot carries the level for kind==1)
                        if a.kind == 1 && a.punch > 0 && THUNDERING.load(Ordering::Relaxed) > 0 {
                            {
                                let mut h2 = m.hp.lock().unwrap();
                                *h2 -= 5.0;
                            }
                            *m.fuse.lock().unwrap() = server_ticks(CREEPER_FUSE_GT) as i32; // ignite ~5s
                            let be = NEXT_EID.fetch_add(1, Ordering::SeqCst);
                            bolts()
                                .lock()
                                .unwrap()
                                .push((be, server_ticks(BOLT_LIFE_GT) as i32));
                            broadcast(-1, |_c| (0x01, spawn_typed_pkt(be, 64, mx, my, mz)));
                            let ts = sound_pkt_named("entity.lightning_bolt.thunder", mx, my, mz);
                            broadcast(-1, |_c| (0x68, ts.clone()));
                        }
                        if dh <= 0.0 {
                            mob_dead.push((m.eid, m.etype, mx, my, mz));
                            let on = clients()
                                .lock()
                                .unwrap()
                                .iter()
                                .find(|c| c.eid == a.owner)
                                .map(|c| c.name.clone());
                            if let Some(on) = on {
                                adv_event(&on, "player_killed_entity", Some(m.etype));
                            }
                        }
                        if a.pierce > 0 {
                            a.pierce -= 1;
                            a.last_hit = m.eid;
                        } else {
                            dead.push(eid);
                        }
                        break 'outer;
                    }
                }
                drop(ml);
                if !dead.contains(&eid) {
                    let cls: Vec<Arc<Client>> = client_snapshot();
                    'p2: for c in cls.iter() {
                        if c.eid == a.owner {
                            continue;
                        }
                        let (px, py, pz, ..) = {
                            let p = c.pos.lock().unwrap();
                            *p
                        };
                        if c.eid == a.last_hit {
                            continue;
                        }
                        if projectile_segment_hits(
                            segment_start,
                            segment_end,
                            (px, py, pz),
                            0.7,
                            0.3,
                            2.0,
                        ) {
                            hits.push((c.eid, a.dmg));
                            if a.pierce > 0 {
                                a.pierce -= 1;
                                a.last_hit = c.eid;
                            } else {
                                dead.push(eid);
                            }
                            break 'p2;
                        }
                    }
                }
            } else {
                let cls: Vec<Arc<Client>> = client_snapshot();
                for c in cls.iter() {
                    let (px, py, pz, ..) = {
                        let p = c.pos.lock().unwrap();
                        *p
                    };
                    if projectile_segment_hits(
                        segment_start,
                        segment_end,
                        (px, py, pz),
                        0.7,
                        0.3,
                        2.0,
                    ) {
                        hits.push((c.eid, a.dmg));
                        dead.push(eid);
                        break;
                    }
                }
            }
        }
        // thrown tridents land as pickupable drops; loyalty returns at owner's feet
        let landed: Vec<(i32, f64, f64, f64, i64, i64, Comps)> = ar
            .iter()
            .filter(|a| dead.contains(&a.eid) && a.ret_item > 0)
            .map(|a| {
                (
                    a.owner,
                    a.x,
                    a.y,
                    a.z,
                    a.loy,
                    a.ret_item,
                    a.ret_comps.clone(),
                )
            })
            .collect();
        ar.retain(|a| !dead.contains(&a.eid));
        for (ow, lx, ly, lz, loy, ri, rc) in landed {
            let (tx, ty, tz) = if loy > 0 {
                clients()
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|c| c.eid == ow)
                    .map(|c| {
                        let p = c.pos.lock().unwrap();
                        (p.0, p.1, p.2)
                    })
                    .unwrap_or((lx, ly, lz))
            } else {
                (lx, ly, lz)
            };
            drops().lock().unwrap().push(Drop {
                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                x: tx,
                y: ty,
                z: tz,
                item: ri,
                cnt: 1,
                comps: rc,
                age: 0,
                target: None,
                thrower: None,
            });
        }
    }
    for (deid, etype, mx, my, mz) in mob_dead {
        kill_mob(deid, etype, mx, my, mz);
    }
    for eid in &dead {
        broadcast_seen_destroy(*eid);
    }
    for (veid, admg) in hits {
        let cls: Vec<Arc<Client>> = client_snapshot();
        if let Some(c) = cls.iter().find(|c| c.eid == veid) {
            if *c.gm.lock().unwrap() == 1 {
                continue;
            }
            let blocked_ar = *c.blocking.lock().unwrap()
                && c.shield_cd.lock().unwrap().elapsed() > Duration::from_secs(5);
            // a rejected hit must not cost shield durability (vanilla returns before that)
            let Some((ad, _)) = damage_after_invulnerability(c, if blocked_ar { 0.0 } else { admg })
            else {
                continue;
            };
            if blocked_ar {
                damage_shield(c, admg);
            }
            let mut h = c.hp.lock().unwrap();
            *h = (*h
                - if ad > 0.0 {
                    hurt_absorb(c, dmg_after(c, ad, 0))
                } else {
                    0.0
                })
            .max(0.0);
            if *h <= 0.0 {
                drop(h);
                totem_check(c);
                h = c.hp.lock().unwrap();
            }
            let hh = *h;
            drop(h);
            if ad > 0.0 {
                damage_armor(c, ad);
            }
            if let Ok(mut w) = c.sock.lock() {
                let _ = send_packet(&mut w, 0x24, hurt_anim(veid));
                let _ = send_packet(&mut w, 0x5d, health_pkt_c(hh, c));
            }
            let vname = c.name.clone();
            drop(cls);
            broadcast_seen_ex(veid, veid, |_c| (0x24, hurt_anim(veid)));
            if hh <= 0.0 {
                let line = system_chat(&format!("{} was shot by Skeleton", vname));
                broadcast(-1, |_c| (0x6c, line.clone()));
            }
        }
    }
}
fn explode_at(ex: f64, ey: f64, ez: f64, power: f64) {
    let (bx, by, bz) = (ex.floor() as i32, ey.floor() as i32, ez.floor() as i32);
    let mut offs: Vec<(i8, i8, i8)> = Vec::new();
    for dx in -2..=2 {
        for dy in -1..=2 {
            for dz in -2..=2 {
                if dx * dx + dy * dy + dz * dz <= 5 {
                    let (tx, ty, tz) = (bx + dx, by + dy, bz + dz);
                    // vanilla: TNT destroyed by an explosion primes itself —
                    // short fuse (0.5-1.5s), this is what makes chains work
                    if (2094..=2095).contains(&block_at(tx, ty, tz)) {
                        let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
                        let jitter =
                            ((tx.wrapping_mul(31) ^ tz.wrapping_mul(17) ^ ty) & 0x3ff) as u64;
                        primed().lock().unwrap().push(Primed {
                            eid,
                            x: tx as f64 + 0.5,
                            y: ty as f64,
                            z: tz as f64 + 0.5,
                            left: chain_fuse_left(jitter) as i32,
                        });
                    }
                    offs.push((dx as i8, dy as i8, dz as i8));
                    set_block(tx, ty, tz, 0);
                }
            }
        }
    }
    broadcast(-1, |_c| (0x20, explosion_pkt(ex, ey, ez, &offs)));
    let cs: Vec<Arc<Client>> = client_snapshot();
    for cl in cs {
        let (px, py, pz, ..) = {
            let p = cl.pos.lock().unwrap();
            *p
        };
        let dd = ((px - ex).powi(2) + (py - ey).powi(2) + (pz - ez).powi(2)).sqrt();
        if dd < power * 1.5 && *cl.gm.lock().unwrap() != 1 {
            let raw = ((power * 1.5 - dd) * 3.0) as f32;
            let Some((accepted, _)) = damage_after_invulnerability(&cl, raw) else {
                continue;
            };
            let mut h = cl.hp.lock().unwrap();
            *h = (*h - hurt_absorb(&cl, dmg_after(&cl, accepted, 2))).max(0.0);
            if *h <= 0.0 {
                drop(h);
                totem_check(&cl);
                h = cl.hp.lock().unwrap();
            }
            let hh = *h;
            drop(h);
            damage_armor(&cl, raw);
            if let Ok(mut w) = cl.sock.lock() {
                let _ = send_packet(&mut w, 0x24, hurt_anim(cl.eid));
                let _ = send_packet(&mut w, 0x5d, health_pkt_c(hh, &cl));
            }
        }
    }
}

fn primed_tick() {
    let mut boom: Vec<Primed> = Vec::new();
    {
        let mut pl = primed().lock().unwrap();
        let region = region_guard();
        let mut i = 0;
        while i < pl.len() {
            if !region.is_hot(pl[i].x as i32, pl[i].z as i32) {
                i += 1; // fuse pauses while cold: no tick, no decrement
            } else {
                pl[i].left -= 1;
                if pl[i].left <= 0 {
                    boom.push(pl.remove(i));
                } else {
                    i += 1;
                }
            }
        }
    }
    for p in boom {
        broadcast_seen_destroy(p.eid);
        explode_at(p.x, p.y, p.z, 4.0);
    }
}

fn live_mob_hit_allowed(mob: &Mob, source: i32, hit: BossHit) -> bool {
    if !matches!(mob.etype, 31 | 88 | 116 | 119) {
        return true;
    }
    let mut states = live_mob_states().lock().unwrap();
    let state = states
        .entry(mob.eid)
        .or_insert_with(|| LiveMobState::new(mob.etype));
    match &mut state.behavior {
        Some(MobBehaviorState::Wither(wither)) => !wither.immune_to(hit),
        Some(MobBehaviorState::EnderDragon(dragon)) => {
            !matches!(hit, BossHit::Arrow) || !dragon.is_sitting()
        }
        Some(MobBehaviorState::Shulker(shulker)) => {
            !matches!(hit, BossHit::Arrow) || shulker.peek() != 0
        }
        Some(MobBehaviorState::Warden(warden)) => {
            if source > 0 {
                warden.anger.add_vibration(source);
            }
            true
        }
        _ => true,
    }
}

fn live_mob_damage_player(
    player: &Arc<Client>,
    origin: [f64; 3],
    damage: f32,
    protection_kind: Option<u8>,
    knockback_y: Option<f64>,
) -> bool {
    if !matches!(*player.gm.lock().unwrap(), 0 | 2) || *player.hp.lock().unwrap() <= 0.0 {
        return false;
    }
    if protection_kind == Some(0)
        && *player.blocking.lock().unwrap()
        && player.shield_cd.lock().unwrap().elapsed() > Duration::from_secs(5)
    {
        damage_shield(player, damage);
        return false;
    }
    if damage <= 0.0 {
        return false;
    }
    let Some((accepted, _)) = damage_after_invulnerability(player, damage) else {
        return false;
    };
    let damage_taken = protection_kind.map_or(accepted, |kind| dmg_after(player, accepted, kind));
    {
        let mut hp = player.hp.lock().unwrap();
        *hp = (*hp - hurt_absorb(player, damage_taken)).max(0.0);
    }
    totem_check(player);
    if protection_kind.is_some() {
        damage_armor(player, damage);
    }
    let health = *player.hp.lock().unwrap();
    let velocity = knockback_y.map(|up| {
        let (x, _, z, ..) = *player.pos.lock().unwrap();
        let [dx, _, dz] = mob_behavior_unit([x - origin[0], 0.0, z - origin[2]]);
        velocity_pkt(
            player.eid,
            (dx * 3200.0) as i16,
            (up * 8000.0) as i16,
            (dz * 3200.0) as i16,
        )
    });
    if let Ok(mut sock) = player.sock.lock() {
        let _ = send_packet(&mut sock, 0x24, hurt_anim(player.eid));
        let _ = send_packet(&mut sock, 0x5d, health_pkt_c(health, player));
        if let Some(packet) = &velocity {
            let _ = send_packet(&mut sock, 0x5a, packet.clone());
        }
    }
    broadcast_seen_ex(player.eid, player.eid, |_| (0x24, hurt_anim(player.eid)));
    if let Some(packet) = velocity {
        broadcast_seen_ex(player.eid, player.eid, |_| (0x5a, packet.clone()));
    }
    true
}

fn live_mob_melee(mob: &Mob, player: &Arc<Client>, pos: [f64; 3], amount: f32, up: f64) {
    if !live_mob_damage_player(player, pos, diff_mob_dmg(amount), Some(0), Some(up)) {
        return;
    }
    let thorns = {
        let inv = player.inv.lock().unwrap();
        (5i16..=8)
            .filter_map(|slot| inv.get(&slot))
            .map(|stack| enchant_level(&stack.2, 38))
            .sum::<i64>()
    };
    if thorns > 0 && world_rng::next_u64() % 100 < (thorns * 15) as u64 {
        *mob.hp.lock().unwrap() -= (1 + world_rng::next_u64() % 4) as f32;
        *mob.hurt_at.lock().unwrap() = Instant::now();
        broadcast_seen(mob.eid, |_| (0x24, hurt_anim(mob.eid)));
    }
    if *player.hp.lock().unwrap() <= 0.0 {
        let name = entity_spec_by_id(mob.etype).map_or("mob", |spec| {
            spec.name.strip_prefix("minecraft:").unwrap_or(spec.name)
        });
        let line = system_chat(&format!("{} was slain by {}", player.name, name));
        broadcast(-1, |_| (0x6c, line.clone()));
    }
}

fn live_mob_explosion(dim: DimId, pos: [f64; 3], power: f32, players: &[LiveMobPlayer<'_>]) {
    let [x, y, z] = pos;
    let [bx, by, bz] = pos.map(|n| n.floor() as i32);
    let mut records = Vec::new();
    if GR_MOB_GRIEFING.load(Ordering::Relaxed) != 0 {
        let radius = power.ceil() as i32;
        for dx in -radius..=radius {
            for dy in -radius..=radius {
                for dz in -radius..=radius {
                    if (dx * dx + dy * dy + dz * dz) as f32 > power * power {
                        continue;
                    }
                    let (x, y, z) = (bx + dx, by + dy, bz + dz);
                    let block = block_at_dim(dim, x, y, z);
                    if block != 0 && block != 79 {
                        set_block_dim(dim, x, y, z, 0);
                        note_block_update_dim(dim, x, y, z, 0);
                        records.push((dx as i8, dy as i8, dz as i8));
                    }
                }
            }
        }
    }
    let packet = explosion_pkt_with_power(x, y, z, power, &records);
    for player in players.iter().filter(|player| player.dim == dim) {
        if let Ok(mut sock) = player.client.sock.lock() {
            let _ = send_packet(&mut sock, 0x20, packet.clone());
        }
        let distance = entity_distance_sq((x, y, z), player.pos).sqrt();
        let damage = (power * 1.5 - distance as f32) * 3.0;
        live_mob_damage_player(
            player.client,
            pos,
            diff_expl_dmg(damage),
            Some(2),
            Some(0.3),
        );
    }
}

fn live_witch_effect_id(potion: WitchPotion) -> i32 {
    match potion {
        WitchPotion::WaterBreathing => 12,
        WitchPotion::FireResistance => 11,
        WitchPotion::Healing => 5,
        WitchPotion::Swiftness => 0,
        WitchPotion::Slowness => 1,
        WitchPotion::Poison => 18,
        WitchPotion::Weakness => 17,
        WitchPotion::Harming => 6,
    }
}

fn live_mob_player_effect(client: &Client, id: i32, ticks: u16) {
    if !matches!(*client.gm.lock().unwrap(), 0 | 2) || *client.hp.lock().unwrap() <= 0.0 {
        return;
    }
    let ticks = server_ticks(u64::from(ticks)) as i32;
    let mut effects = client.effs.lock().unwrap();
    if effects
        .iter()
        .any(|effect| effect.id == id && effect.left >= ticks)
    {
        return;
    }
    effects.retain(|effect| effect.id != id);
    effects.push(Eff {
        id,
        amp: 0,
        left: ticks,
    });
    drop(effects);
    let packet = entity_effect_pkt(client.eid, id, 0, ticks);
    if let Ok(mut sock) = client.sock.lock() {
        let _ = send_packet(&mut sock, 0x76, packet.clone());
    }
    broadcast_seen_ex(client.eid, client.eid, |_| (0x76, packet.clone()));
}

fn live_mob_death(mob: &Arc<Mob>, state: Option<LiveMobState>, pos: [f64; 3]) {
    let state = state.unwrap_or_else(|| LiveMobState::new(mob.etype));
    sitting_cats().lock().unwrap().remove(&mob.eid);
    if let Some(behavior) = state.behavior {
        apply_live_mob_effects(mob, mob.dim, pos, behavior.on_death(), &[]);
    }
}

fn default_mob_hp(id: &str) -> f32 {
    let short = id.strip_prefix("minecraft:").unwrap_or(id);
    if matches!(short, "magma_cube" | "slime") {
        return 16.0;
    }
    if short == "wither" {
        return WitherBehavior::MAX_HP / 3.0;
    }
    if matches!(
        short,
        "elder_guardian" | "witch" | "cat" | "cow" | "pig" | "shulker"
            | "evoker" | "vindicator" | "pillager" | "piglin" | "spider"
            | "piglin_brute" | "hoglin" | "iron_golem" | "cave_spider"
            | "silverfish" | "sheep" | "camel" | "ender_dragon" | "warden" | "wolf"
            | "ghast" | "enderman"
    ) {
        return entity_spec_by_name(id)
            .map(|spec| spec.max_health)
            .unwrap_or(20.0);
    }
    20.0
}

fn name_to_effect(n: &str) -> Option<i32> {
    let n = n.trim_start_matches("minecraft:");
    Some(match n {
        "speed" => 0,
        "slowness" => 1,
        "haste" => 2,
        "mining_fatigue" => 3,
        "strength" => 4,
        "instant_health" => 5,
        "instant_damage" => 6,
        "jump_boost" => 7,
        "nausea" => 8,
        "regeneration" => 9,
        "resistance" => 10,
        "fire_resistance" => 11,
        "water_breathing" => 12,
        "invisibility" => 13,
        "blindness" => 14,
        "night_vision" => 15,
        "hunger" => HUNGER_EFFECT,
        "weakness" => 17,
        "poison" => 18,
        "wither" => 19,
        "health_boost" => 20,
        "absorption" => 21,
        "saturation" => 22,
        "glowing" => 23,
        "levitation" => 24,
        "luck" => 25,
        "unluck" => 26,
        "slow_falling" => 27,
        "conduit_power" => 28,
        "dolphins_grace" => 29,
        "bad_omen" => 30,
        "hero_of_the_village" => 31,
        "darkness" => 32,
        _ => return None,
    })
}
// The live Hunger effect is consumed by FoodData. The legacy effect tick's
// direct food decrement is keyed to 16, which remains the protocol registry ID.
const HUNGER_EFFECT: i32 = -16;
fn effect_wire_id(id: i32) -> i64 {
    i64::from(if id == HUNGER_EFFECT { 16 } else { id })
}
fn entity_effect_pkt(eid: i32, id: i32, amp: i32, ticks: i32) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, eid as i64);
    write_varint(&mut p, effect_wire_id(id));
    write_varint(&mut p, amp as i64);
    write_varint(&mut p, ticks as i64);
    p.push(0x06); // particles+icon
    p
}
fn remove_effect_pkt(eid: i32, id: i32) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, eid as i64);
    write_varint(&mut p, effect_wire_id(id));
    p
}

fn hurt_anim(eid: i32) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, eid as i64);
    p.extend_from_slice(&0f32.to_be_bytes());
    p
}
fn broadcast_player_attack_damage(victim: i32, attacker: i32, dim: DimId) {
    fanout_in(
        &client_snapshot(),
        |client| {
            if client.player_attack_damage_id.is_none() || *client.dim.lock().unwrap() != dim {
                return false;
            }
            client.eid == victim || client.seen.lock().unwrap().contains(&victim)
        },
        |client| {
            let mut body = Vec::new();
            write_varint(&mut body, i64::from(victim));
            write_varint(
                &mut body,
                i64::from(client.player_attack_damage_id.unwrap()),
            );
            // Minecraft's optional entity IDs encode the ID plus one, reserving zero for None.
            write_varint(&mut body, i64::from(attacker) + 1);
            write_varint(&mut body, i64::from(attacker) + 1);
            body.push(0); // No explicit source position for an entity attack.
            (0x1a, body)
        },
    );
}
fn health_pkt(hp: f32, food: i32, sat: f32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&hp.to_be_bytes());
    write_varint(&mut p, food as i64);
    p.extend_from_slice(&sat.to_be_bytes());
    p
}

// Runtime state attached to the connection, alongside the legacy health and XP fields.
struct PlayerMechanismSession {
    xp_bar: (i32, f32),
    food_timer: i32,
    menus: Arc<LiveMenuConnection>,
}

fn player_mechanism_sessions() -> &'static LockedMap<i32, PlayerMechanismSession> {
    static SESSIONS: Registry<i32, PlayerMechanismSession> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn player_movement_exhaustion(c: &Client, (x, y, z): (f64, f64, f64)) -> f32 {
    let (last_x, _, last_z) = std::mem::replace(&mut *c.lastp.lock().unwrap(), (x, y, z));
    let distance = (x - last_x).hypot(z - last_z);
    let movement = if distance >= 10.0 {
        0.0
    } else if *c.sprinting.lock().unwrap() {
        distance as f32 * 0.1
    } else if *c.sneaking.lock().unwrap() {
        distance as f32 * 0.005
    } else {
        0.0
    };
    let hunger = c
        .effs
        .lock()
        .unwrap()
        .iter()
        .find(|effect| effect.id == HUNGER_EFFECT)
        .map_or(0.0, |effect| 0.005 * (effect.amp as f32 + 1.0));
    movement + hunger * GAME_TICKS_PER_TICK as f32
}

fn tick_player_food(c: &Client, hp: &mut f32, max_hp: f32, exhaustion: f32, t: u64) -> bool {
    let difficulty = match DIFFICULTY.load(Ordering::Relaxed) {
        0 => HungerDifficulty::Peaceful,
        1 => HungerDifficulty::Easy,
        3 => HungerDifficulty::Hard,
        _ => HungerDifficulty::Normal,
    };
    let natural_regeneration = GR_NATURAL_REGEN.load(Ordering::Relaxed) != 0;
    let mut sessions = player_mechanism_sessions().lock().unwrap();
    let Some(session) = sessions.get_mut(&c.eid) else {
        return false;
    };
    let (mut food, mut sat, mut acc) = (
        c.food.lock().unwrap(),
        c.sat.lock().unwrap(),
        c.walk_acc.lock().unwrap(),
    );
    let before = (*hp, *food, *sat);
    let mut data = FoodData {
        food_level: *food,
        last_food_level: *food,
        saturation: *sat,
        exhaustion: *acc as f32,
        tick_timer: session.food_timer,
    };
    data.add_exhaustion(exhaustion);
    // Peaceful recovery is separate from FoodData's exhaustion and food-based healing.
    if difficulty == HungerDifficulty::Peaceful && natural_regeneration {
        if t.is_multiple_of(server_ticks(20)) {
            *hp = (*hp + 1.0).min(max_hp);
            data.saturation = (data.saturation + 1.0).min(20.0);
        }
        if t.is_multiple_of(server_ticks(10)) {
            data.food_level = (data.food_level + 1).min(20);
        }
    }
    for _ in 0..GAME_TICKS_PER_TICK {
        match data.tick(*hp, max_hp, difficulty, natural_regeneration) {
            FoodTick::Heal(amount) => *hp = (*hp + amount).min(max_hp),
            FoodTick::Starve => {
                *hp = (*hp - 1.0).max(0.0);
            }
            FoodTick::None => {}
        }
    }
    *food = data.food_level;
    *sat = data.saturation;
    *acc = f64::from(data.exhaustion);
    session.food_timer = data.tick_timer;
    before != (*hp, *food, *sat)
}

fn reset_player_food(c: &Client) {
    let effects = std::mem::take(&mut *c.effs.lock().unwrap());
    for effect in effects {
        let packet = remove_effect_pkt(c.eid, effect.id);
        if let Ok(mut sock) = c.sock.lock() {
            let _ = send_packet(&mut sock, 0x43, packet.clone());
        }
        broadcast_seen(c.eid, |_| (0x43, packet.clone()));
    }
    *c.absorb.lock().unwrap() = 0.0;
    *c.air.lock().unwrap() = server_ticks(AIR_SUPPLY_GT) as i32;
    *c.invulnerable.lock().unwrap() = 0;
    *c.last_hurt.lock().unwrap() = 0.0;
    *c.blocking.lock().unwrap() = false;
    *c.sprinting.lock().unwrap() = false;
    *c.sneaking.lock().unwrap() = false;
    *c.fall_fly.lock().unwrap() = false;
    let mut sessions = player_mechanism_sessions().lock().unwrap();
    if let Some(session) = sessions.get_mut(&c.eid) {
        session.food_timer = 0;
    }
    let data = FoodData::default();
    *c.food.lock().unwrap() = data.food_level;
    *c.sat.lock().unwrap() = data.saturation;
    *c.walk_acc.lock().unwrap() = 0.0;
}

fn add_player_xp_points(xp: &mut PlayerXpState, points: i32) -> Result<(), ExperienceSetError> {
    // The adapter uses the Java i32 threshold when adding points. A level set by
    // a command can exceed the range in which both it and the next level fit.
    if xp_needed_for_next_level(xp.level) > i64::from(i32::MAX) - 9 {
        return Err(ExperienceSetError::PointsOutOfRange);
    }
    xp.give_experience_points(points);
    Ok(())
}

fn take_dropped_experience(c: &Client) -> i64 {
    let mut total = c.xp.lock().unwrap();
    let previous_total = std::mem::take(&mut *total);
    let mut sessions = player_mechanism_sessions().lock().unwrap();
    let Some(session) = sessions.get_mut(&c.eid) else {
        return previous_total.clamp(0, 100);
    };
    let (level, progress) = std::mem::take(&mut session.xp_bar);
    // Preserve the existing 100-point drop cap using the player's current bar.
    // By level 8 the starting XP alone already exceeds the cap.
    let level = level.min(8);
    let points = xp_points_for_level(level)
        + (progress * xp_needed_for_next_level(level) as f32).round() as i64;
    points.clamp(0, 100)
}

fn send_player_xp(
    c: &Client,
    update: impl FnOnce(&mut PlayerXpState) -> Result<(), ExperienceSetError>,
) -> Result<(), ExperienceSetError> {
    // Serialize changes and their packets for this player, including concurrent orb pickups.
    let mut total = c.xp.lock().unwrap();
    let mut sessions = player_mechanism_sessions().lock().unwrap();
    let Some(session) = sessions.get_mut(&c.eid) else {
        return Ok(());
    };
    let mut xp = PlayerXpState {
        level: session.xp_bar.0,
        progress: session.xp_bar.1,
        total_experience: *total as i32,
    };
    update(&mut xp)?;
    session.xp_bar = (xp.level, xp.progress);
    *total = i64::from(xp.total_experience);
    drop(sessions);
    let mut p = xp.progress.to_be_bytes().to_vec();
    write_varint(&mut p, i64::from(xp.level));
    write_varint(&mut p, *total);
    if let Ok(mut w) = c.sock.lock() {
        let _ = send_packet(&mut w, 0x5c, p);
    }
    Ok(())
}

fn health_pkt_c(hp: f32, c: &Client) -> Vec<u8> {
    health_pkt(hp, *c.food.lock().unwrap(), *c.sat.lock().unwrap())
}
// vanilla food side-effects: (chance%%, effect id, amplifier, ticks@4/s)
fn food_effects(item: i64) -> &'static [(u64, i32, i32, i32)] {
    let g = |n| name_to_item(n).unwrap_or(-1);
    // Durations are vanilla game ticks. One server tick is one game tick at
    // 20TPS, so these literals are also the stored server-tick values.
    if item == g("golden_apple") {
        return &[(100, 21, 0, 2400), (100, 9, 1, 100)];
    }
    if item == g("enchanted_golden_apple") {
        return &[
            (100, 21, 3, 2400),
            (100, 9, 1, 400),
            (100, 10, 0, 6000),
            (100, 11, 0, 6000),
        ];
    }
    if item == g("rotten_flesh") {
        return &[(80, HUNGER_EFFECT, 0, 600)];
    }
    if item == g("chicken") {
        return &[(30, HUNGER_EFFECT, 0, 600)];
    }
    if item == g("pufferfish") {
        return &[(100, 16, 2, 300), (100, 8, 0, 300), (100, 18, 2, 1200)];
    }
    if item == g("spider_eye") {
        return &[(100, 18, 0, 100)];
    }
    &[]
}
// damage absorbed by the absorption-effect pool before touching hp
/// `LivingEntity.hurt`'s invulnerability rule, from the 1.21.1 bytecode: while
/// `invulnerableTime > 10` a hit that does not bypass the cooldown is rejected
/// when it is equal or weaker, and otherwise applies only the difference - and
/// the timer is *not* refreshed, so the full-immunity window still ends ten
/// ticks after the hit that started it. Otherwise the hit applies in full and
/// the timer is set to 20 ticks.
///
/// Returns accepted damage before armour, absorption and effects, plus whether
/// this starts a new hurt window. Vanilla sends Damage Event on that new window.
/// A rejected hit leaves health, armour and the shield alone.
fn damage_after_invulnerability(c: &Client, amount: f32) -> Option<(f32, bool)> {
    let mut invulnerable = c.invulnerable.lock().unwrap();
    let mut last_hurt = c.last_hurt.lock().unwrap();
    if *invulnerable > 10 {
        if amount <= *last_hurt {
            return None;
        }
        let difference = amount - *last_hurt;
        *last_hurt = amount;
        return Some((difference, false));
    }
    *last_hurt = amount;
    *invulnerable = server_ticks(20) as i32;
    Some((amount, true))
}

fn hurt_absorb(c: &Client, dmg: f32) -> f32 {
    let mut a = c.absorb.lock().unwrap();
    if *a <= 0.0 {
        return dmg;
    }
    let r = (dmg - *a).max(0.0);
    *a = (*a - dmg).max(0.0);
    r
}

// entity attributes: p:<player-name> persistent | e:<eid> transient
fn attrs() -> &'static Mutex<HashMap<String, HashMap<String, f64>>> {
    static R: OnceLock<Mutex<HashMap<String, HashMap<String, f64>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}
fn attr_key_id(name: &str) -> Option<i64> {
    let n = name.trim_start_matches("minecraft:");
    Some(match n {
        "generic.armor" => 0,
        "generic.armor_toughness" => 1,
        "generic.attack_damage" => 2,
        "generic.attack_knockback" => 3,
        "generic.attack_speed" => 4,
        "player.block_break_speed" => 5,
        "player.block_interaction_range" => 6,
        "player.entity_interaction_range" => 7,
        "generic.fall_damage_multiplier" => 8,
        "generic.flying_speed" => 9,
        "generic.follow_range" => 10,
        "generic.gravity" => 11,
        "generic.jump_strength" => 12,
        "generic.knockback_resistance" => 13,
        "generic.luck" => 14,
        "generic.max_absorption" => 15,
        "generic.max_health" => 16,
        "generic.movement_speed" => 17,
        "generic.safe_fall_distance" => 18,
        "generic.scale" => 19,
        "zombie.spawn_reinforcements" => 20,
        "generic.step_height" => 21,
        _ => return None,
    })
}
fn attr_default(name: &str) -> f64 {
    match name.trim_start_matches("minecraft:") {
        "generic.max_health" => 20.0,
        "generic.attack_damage" => 1.0,
        "generic.movement_speed" => 0.1,
        _ => 0.0,
    }
}
fn attr_of(key: &str, name: &str) -> f64 {
    attrs()
        .lock()
        .unwrap()
        .get(key)
        .and_then(|m| m.get(name))
        .copied()
        .unwrap_or_else(|| attr_default(name))
}
fn max_hp_of(pname: &str) -> f32 {
    attr_of(&format!("p:{}", pname), "generic.max_health") as f32
}
fn attrs_pkt(eid: i32, m: &HashMap<String, f64>) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, eid as i64);
    write_varint(&mut p, m.len() as i64);
    for (k, v) in m {
        if let Some(id) = attr_key_id(k) {
            write_varint(&mut p, id);
            p.extend_from_slice(&v.to_be_bytes());
            write_varint(&mut p, 0); // no modifiers
        }
    }
    p
}
// send a client's full attribute set to themselves (join/set paths)
fn send_attrs(c: &Arc<Client>) {
    let m = attrs()
        .lock()
        .unwrap()
        .get(&format!("p:{}", c.name))
        .cloned()
        .unwrap_or_default();
    if m.is_empty() {
        return;
    }
    let p = attrs_pkt(c.eid, &m);
    if let Ok(mut w) = c.sock.lock() {
        let _ = send_packet(&mut w, 0x75, p);
    }
}
