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
#[derive(Clone, Copy)]
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
/// A firework rocket in flight (`charged_projectiles` and `firework_rocket`
/// item uses). `explosions` is the count of `firework_explosion` stars, which
/// vanilla turns into `5 + 2 * n` blast damage on detonation.
pub(crate) struct Firework {
    pub(crate) eid: i32,
    pub(crate) owner: i32,
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) z: f64,
    pub(crate) dx: f64,
    pub(crate) dy: f64,
    pub(crate) dz: f64,
    pub(crate) ttl: i32,
    pub(crate) explosions: i64,
}
static FIREWORKS: OnceLock<Mutex<Vec<Firework>>> = OnceLock::new();
pub(crate) fn fireworks() -> &'static Mutex<Vec<Firework>> {
    FIREWORKS.get_or_init(|| Mutex::new(Vec::new()))
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

// ---------- fresh-orb merge on spawn/tick (MEGA-SCALE §3.2) ----------
// Orbs spawned through `xp_spawn` since the last `drop_tick` merge pass. The
// per-eid marker lets a spawn storm collapse inside the tick it lands in —
// before the O(pool) env merge or the pickup scan ever sees 2,000 fresh
// entities — while orbs that already existed keep the env pass's pinned
// keep/eat decision.
static XP_PENDING: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();
fn xp_pending() -> &'static Mutex<HashSet<i32>> {
    XP_PENDING.get_or_init(|| Mutex::new(HashSet::new()))
}

/// The pool size at which merging fresh spawns pays. Below it the per-tick
/// env merge is already cheap, so staying inert there adds nothing and keeps
/// this path unobservable to the pinned save-payload fixture.
const XP_MERGE_FLOOR: usize = 256;
/// One orb may not exceed the `i16` wire value; an over-valued survivor emits
/// the conserved remainder as additional bounded orbs instead of overflowing.
const XP_ORB_MAX_VAL: u64 = i16::MAX as u64;

/// Spawn an experience orb, marking it merge-pending so the next `drop_tick`
/// pass can collapse a spawn burst inside the same tick.
fn xp_spawn(x: f64, y: f64, z: f64, val: i16) -> i32 {
    let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
    xps().lock().unwrap_or_else(|e| e.into_inner()).push(Xp {
        eid,
        x,
        y,
        z,
        val,
        age: 0,
    });
    xp_pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(eid);
    eid
}

/// The merge kernel over one partitioned pool (keepers first, donors after,
/// both in `xps` order, parallel `vals`). Under `mod-suite` this is
/// `mod_offload::entity_spatial::merge_xp_orbs` directly; the local build runs
/// the same cell-bucketed scan — cell size 2.0, radius 1.0, every orb feeding
/// its whole value to the earliest surviving candidate — so both builds agree.
#[cfg(feature = "mod-suite")]
fn xp_merge_run(pool: &[usize], xs: &[Xp], vals: &mut [u64]) {
    let mut orbs: Vec<mod_offload::entity_spatial::XpOrb> = pool
        .iter()
        .enumerate()
        .map(|(k, &i)| mod_offload::entity_spatial::XpOrb {
            id: k as u32,
            pos: [xs[i].x, xs[i].y, xs[i].z],
            vel: [0.0; 3],
            value: vals[k],
        })
        .collect();
    mod_offload::entity_spatial::merge_xp_orbs(&mut orbs);
    for o in &orbs {
        vals[o.id as usize] = o.value;
    }
}

#[cfg(not(feature = "mod-suite"))]
fn xp_merge_run(pool: &[usize], xs: &[Xp], vals: &mut [u64]) {
    const CELL: f64 = 2.0;
    let mut cells: HashMap<(i32, i32, i32), Vec<usize>> = HashMap::with_capacity(vals.len());
    let mut candidates: Vec<usize> = Vec::new();
    for i in 0..vals.len() {
        if vals[i] == 0 {
            continue;
        }
        let (px, py, pz) = (xs[pool[i]].x, xs[pool[i]].y, xs[pool[i]].z);
        let (cx, cy, cz) = (
            (px / CELL).floor() as i32,
            (py / CELL).floor() as i32,
            (pz / CELL).floor() as i32,
        );
        candidates.clear();
        for dx in -1..=1i32 {
            for dy in -1..=1i32 {
                for dz in -1..=1i32 {
                    if let (Some(kx), Some(ky), Some(kz)) = (
                        cx.checked_add(dx),
                        cy.checked_add(dy),
                        cz.checked_add(dz),
                    ) {
                        if let Some(list) = cells.get(&(kx, ky, kz)) {
                            candidates.extend_from_slice(list);
                        }
                    }
                }
            }
        }
        candidates.sort_unstable();
        let mut merged = false;
        for &j in &candidates {
            let (qx, qy, qz) = (xs[pool[j]].x, xs[pool[j]].y, xs[pool[j]].z);
            if (px - qx).powi(2) + (py - qy).powi(2) + (pz - qz).powi(2) <= 1.0 {
                vals[j] = vals[j].saturating_add(vals[i]);
                vals[i] = 0;
                merged = true;
                break;
            }
        }
        if !merged {
            cells.entry((cx, cy, cz)).or_default().push(i);
        }
    }
}

/// Merge `pending` donor eids into the locked orb pool. Keepers (pre-existing
/// orbs) precede donors in the pool, so a fresh orb feeds the earliest
/// surviving neighbour within 1.0 and donors collapse left-to-right when none
/// is in range. Tombstoned orbs are removed in one `retain` pass; an
/// over-valued survivor is clamped to `i16::MAX` and the conserved remainder
/// respawns as fresh bounded orbs. Returns the absorbed eids in merge order,
/// which is the order the destroy packets go out in.
fn xp_merge_pending_locked(xs: &mut Vec<Xp>, pending: &HashSet<i32>) -> Vec<i32> {
    let mut pool: Vec<usize> = Vec::with_capacity(xs.len());
    pool.extend(
        xs.iter()
            .enumerate()
            .filter(|(_, x)| !pending.contains(&x.eid) && x.val > 0)
            .map(|(i, _)| i),
    );
    pool.extend(
        xs.iter()
            .enumerate()
            .filter(|(_, x)| pending.contains(&x.eid) && x.val > 0)
            .map(|(i, _)| i),
    );
    if pool.len() < 2 {
        return Vec::new();
    }
    let mut vals: Vec<u64> = pool.iter().map(|&i| xs[i].val as u64).collect();
    xp_merge_run(&pool, xs, &mut vals);
    let mut eaten = HashSet::new();
    let mut order = Vec::new();
    let mut spill = Vec::new();
    for (k, &i) in pool.iter().enumerate() {
        if vals[k] == 0 {
            if eaten.insert(xs[i].eid) {
                order.push(xs[i].eid);
            }
        } else {
            let keep = vals[k].min(XP_ORB_MAX_VAL);
            let excess = vals[k] - keep;
            xs[i].val = keep as i16;
            if excess > 0 {
                spill.push((xs[i].x, xs[i].y, xs[i].z, excess));
            }
        }
    }
    if !eaten.is_empty() {
        xs.retain(|x| !eaten.contains(&x.eid));
    }
    for (x, y, z, mut left) in spill {
        while left > 0 {
            let take = left.min(XP_ORB_MAX_VAL) as i16;
            xs.push(Xp {
                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                x,
                y,
                z,
                val: take,
                age: 0,
            });
            left -= u64::from(take as u16);
        }
    }
    order
}

/// Collapse freshly spawned orbs into their older neighbours — the merge the
/// env pass would do, done the same tick the storm lands so a farm's 2,000
/// orbs reach the pickup scan as a handful. Runs only once `xps()` outgrows
/// `XP_MERGE_FLOOR`: below it the env merge is already cheap, and the pinned
/// save-payload fixture — which never routes a spawn through `xp_spawn` —
/// cannot observe this path at all.
///
/// The `xps` store is the Overworld orb list, so merged orbs are same-dimension
/// by construction and the 1.0-block radius is the only spatial gate.
fn xp_merge_pending() {
    let pending: HashSet<i32> = {
        let mut p = xp_pending().lock().unwrap_or_else(|e| e.into_inner());
        if p.is_empty() {
            return;
        }
        std::mem::take(&mut *p)
    };
    let mut xs = xps().lock().unwrap_or_else(|e| e.into_inner());
    if xs.len() <= XP_MERGE_FLOOR {
        return;
    }
    let eaten = xp_merge_pending_locked(&mut xs, &pending);
    drop(xs);
    for eid in eaten {
        broadcast_seen_destroy(eid);
    }
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

// ---------- attribute modifiers on equipped items ----------
// Protocol 767 attribute registry ids (`minecraft:attribute`); the same order
// `attr_key_id` encodes on the wire.
const ATTR_ARMOR: i32 = 0;
const ATTR_ARMOR_TOUGHNESS: i32 = 1;
const ATTR_ATTACK_DAMAGE: i32 = 2;
const ATTR_ATTACK_SPEED: i32 = 4;
const ATTR_KNOCKBACK_RESISTANCE: i32 = 13;
const ATTR_MAX_HEALTH: i32 = 16;

/// Vanilla `AttributeInstance.calculateValue` order: `(base + Σadd_value) *
/// (1 + Σadd_multiplied_base) * Π(1 + add_multiplied_total)`, where only
/// modifiers whose slot group matches the slot the item is equipped in count.
fn apply_item_attribute_modifiers(
    base: f64,
    equipped: &[(ItemAttrSlotGroup, &Stack)],
    attribute_id: i32,
) -> f64 {
    let mut add = 0.0;
    let mut mult_base = 0.0;
    let mut mult_total = 1.0;
    for (equipped_in, stack) in equipped {
        let Some(amc) = stack_attr_modifiers(&stack.2) else {
            continue;
        };
        for m in &amc.modifiers {
            if m.attribute_id != attribute_id || !m.slot.applies_to(*equipped_in) {
                continue;
            }
            match m.operation {
                ItemAttrOp::AddValue => add += m.amount,
                ItemAttrOp::AddMultipliedBase => mult_base += m.amount,
                ItemAttrOp::AddMultipliedTotal => mult_total *= 1.0 + m.amount,
            }
        }
    }
    (base + add) * (1.0 + mult_base) * mult_total
}

/// The items vanilla's `getAttributeModifiers` sees for a living player:
/// main hand, off hand, and the four armor slots. Stacks are cloned so no
/// inventory lock is held while the modifiers are evaluated.
fn equipped_pairs(c: &Client) -> Vec<(ItemAttrSlotGroup, Stack)> {
    let inv = c.inv.lock().unwrap_or_else(|e| e.into_inner());
    let held_slot = *c.held.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::with_capacity(6);
    if let Some(st) = inv.get(&(36 + held_slot)) {
        out.push((ItemAttrSlotGroup::MainHand, st.clone()));
    }
    if let Some(st) = inv.get(&45) {
        out.push((ItemAttrSlotGroup::OffHand, st.clone()));
    }
    for (slot, grp) in [
        (5i16, ItemAttrSlotGroup::Head),
        (6, ItemAttrSlotGroup::Chest),
        (7, ItemAttrSlotGroup::Legs),
        (8, ItemAttrSlotGroup::Feet),
    ] {
        if let Some(st) = inv.get(&slot) {
            out.push((grp, st.clone()));
        }
    }
    out
}

/// Per-player cache of the (add_value sum, multiplied_base sum,
/// multiplied_total product) triple for each attribute, computed from the
/// equipped items. `attr_of` is reached from paths that already hold the
/// health lock (main.rs player tick), so it must not take the inventory lock;
/// the cache is refreshed on the exhaustion hook (per tick per client) and on
/// every melee charge instead.
type EquipAttributeModifiers = HashMap<i32, (f64, f64, f64)>;
type EquippedAttributes = HashMap<String, EquipAttributeModifiers>;

static EQUIP_ATTRS: OnceLock<Mutex<EquippedAttributes>> = OnceLock::new();
fn equip_attrs() -> &'static Mutex<EquippedAttributes> {
    EQUIP_ATTRS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn refresh_equip_attrs(c: &Client) {
    let equipped = equipped_pairs(c);
    let pairs: Vec<(ItemAttrSlotGroup, &Stack)> =
        equipped.iter().map(|(g, s)| (*g, s)).collect();
    let mut m: HashMap<i32, (f64, f64, f64)> = HashMap::new();
    for id in [
        ATTR_ARMOR,
        ATTR_ARMOR_TOUGHNESS,
        ATTR_ATTACK_DAMAGE,
        ATTR_ATTACK_SPEED,
        ATTR_KNOCKBACK_RESISTANCE,
        ATTR_MAX_HEALTH,
    ] {
        let (mut add, mut mb, mut mt) = (0.0, 0.0, 1.0);
        for (equipped_in, stack) in &pairs {
            let Some(amc) = stack_attr_modifiers(&stack.2) else {
                continue;
            };
            for md in &amc.modifiers {
                if md.attribute_id != id || !md.slot.applies_to(*equipped_in) {
                    continue;
                }
                match md.operation {
                    ItemAttrOp::AddValue => add += md.amount,
                    ItemAttrOp::AddMultipliedBase => mb += md.amount,
                    ItemAttrOp::AddMultipliedTotal => mt *= 1.0 + md.amount,
                }
            }
        }
        if add != 0.0 || mb != 0.0 || mt != 1.0 {
            m.insert(id, (add, mb, mt));
        }
    }
    equip_attrs().lock().unwrap_or_else(|e| e.into_inner()).insert(c.name.clone(), m);
}

/// Vanilla `Player.currentItemAttackStrengthDelay` source: the effective
/// `generic.attack_speed`. A stack that carries an `attribute_modifiers`
/// component replaces the item's vanilla default (vanilla defaults are
/// components), so the whole attribute is resolved through the codec; a stack
/// without one keeps the table speed — the table encodes `4.0 + item delta`,
/// so the delta is preserved on top of any `/attribute`-set base.
fn melee_attack_speed(me: &Client, item: i64) -> f64 {
    let base = attrs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&format!("p:{}", me.name))
        .and_then(|m| m.get("generic.attack_speed"))
        .copied()
        .unwrap_or(4.0);
    let equipped = equipped_pairs(me);
    let mainhand_has_component = equipped
        .iter()
        .find(|(g, _)| *g == ItemAttrSlotGroup::MainHand)
        .is_some_and(|(_, st)| comp_get(&st.2, COMP_ATTRIBUTE_MODIFIERS).is_some());
    if mainhand_has_component {
        let pairs: Vec<(ItemAttrSlotGroup, &Stack)> =
            equipped.iter().map(|(g, s)| (*g, s)).collect();
        apply_item_attribute_modifiers(base, &pairs, ATTR_ATTACK_SPEED)
    } else {
        let table_speed = 1.0 / attack_cooldown(item);
        let pairs: Vec<(ItemAttrSlotGroup, &Stack)> = equipped
            .iter()
            .filter(|(g, _)| *g != ItemAttrSlotGroup::MainHand)
            .map(|(g, s)| (*g, s))
            .collect();
        apply_item_attribute_modifiers(base + table_speed - 4.0, &pairs, ATTR_ATTACK_SPEED)
    }
}

/// Per-entity attack-strength state (`Player.attackStrengthTicker`): the tick
/// of the last attack reset, the held slot/item it was taken with, and the
/// wall stamp mirrored into `Client.last_melee` (a wall stamp that does not
/// match is an out-of-band write — tests set the field directly — and is then
/// honoured as a reset instead of being treated as cooldown progress).
struct AttackTicker {
    tick: u64,
    held_slot: i16,
    item: i64,
    wall: Option<Instant>,
}
static ATTACK_TICKERS: OnceLock<Mutex<HashMap<i32, AttackTicker>>> = OnceLock::new();
fn attack_tickers() -> &'static Mutex<HashMap<i32, AttackTicker>> {
    ATTACK_TICKERS.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Vanilla `Player.resetAttackStrengthTicker`: called on attack and on
/// held-item switch; the next charge measurement starts from this tick.
/// melee_charge detects the switch itself; this is the reset entry point for
/// swing paths outside this file.
#[allow(dead_code)]
fn reset_attack_ticker(eid: i32) {
    let mut map = attack_tickers().lock().unwrap_or_else(|e| e.into_inner());
    let tick = TICK_IDX.load(Ordering::Relaxed);
    map.entry(eid)
        .and_modify(|st| {
            st.tick = tick;
            st.wall = None;
        })
        .or_insert(AttackTicker {
            tick,
            held_slot: -1,
            item: -1,
            wall: None,
        });
}

// vanilla 1.9+ `Player.getAttackStrengthScale(0.5)`: ticks since the last
// attack reset scaled by `20 / attack_speed`, clamped to 1.0. Driven by
// TICK_IDX so `/tick freeze` and tick lag freeze the recharge instead of
// drifting on wall-clock time.
fn melee_charge(me: &Arc<Client>, item: i64) -> f32 {
    let speed_eff = me.effs.lock().unwrap_or_else(|e| e.into_inner()).iter().fold(1.0_f32, |speed, effect| {
        match effect.id {
            2 | 3 => speed * effects::effect_attribute_modifier(
                effect.id + 1, effect.amp, effects::EffectAttribute::AttackSpeed,
            ),
            _ => speed,
        }
    });
    let speed = melee_attack_speed(me, item) * f64::from(speed_eff.max(0.001));
    let delay_ticks = 20.0 / speed.max(0.001);
    let now_tick = TICK_IDX.load(Ordering::Relaxed);
    let held_slot = *me.held.lock().unwrap_or_else(|e| e.into_inner());
    let mut lm = me.last_melee.lock().unwrap_or_else(|e| e.into_inner());
    let wall_ticks =
        u64::try_from(lm.elapsed().as_millis() / u128::from(TICK_MS)).unwrap_or(u64::MAX);
    let mut map = attack_tickers().lock().unwrap_or_else(|e| e.into_inner());
    let elapsed = match map.get(&me.eid) {
        // Ticker is live and the wall stamp is untouched: game ticks only.
        Some(st) if st.wall == Some(*lm) => {
            if st.held_slot == held_slot && st.item == item {
                now_tick.saturating_sub(st.tick)
            } else {
                // Held-item switch: vanilla resets the ticker, so zero ticks
                // have elapsed on the new item's clock.
                0
            }
        }
        // Ticker reset by reset_attack_ticker (no wall stamp recorded yet).
        Some(st) if st.wall.is_none() => now_tick.saturating_sub(st.tick),
        // Out-of-band write of last_melee (or first attack): treat the wall
        // age as the tick age, so direct field writes still mean what they
        // used to mean for callers that never touch TICK_IDX.
        _ => wall_ticks,
    };
    let charge = ((elapsed as f64 + 0.5) / delay_ticks).clamp(0.0, 1.0) as f32;
    *lm = Instant::now();
    map.insert(
        me.eid,
        AttackTicker {
            tick: now_tick,
            held_slot,
            item,
            wall: Some(*lm),
        },
    );
    refresh_equip_attrs(me);
    charge
}
// `#minecraft:undead` (1.21.1): every member Smite's +2.5/level bonus applies to.
fn enchantment_targets_undead(etype: i64) -> bool {
    entity_spec_by_id(etype).is_some_and(|entity| {
        matches!(
            entity.name.strip_prefix("minecraft:"),
            Some(
                "bogged"
                    | "drowned"
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
// `#minecraft:arthropod` (1.21.1): Bane of Arthropods' damage/slowdown family.
fn enchantment_targets_arthropod(etype: i64) -> bool {
    entity_spec_by_id(etype).is_some_and(|entity| {
        matches!(
            entity.name.strip_prefix("minecraft:"),
            Some("bee" | "cave_spider" | "endermite" | "silverfish" | "spider")
        )
    })
}
// `#minecraft:sensitive_to_impaling` (1.21.1) — the whole aquatic family. The
// tag name differs from the issue's `#minecraft:aquatic` shorthand; the
// membership is the same 12 rows vanilla lists.
#[cfg_attr(not(test), allow(dead_code))]
fn enchantment_targets_aquatic(etype: i64) -> bool {
    entity_spec_by_id(etype).is_some_and(|entity| {
        matches!(
            entity.name.strip_prefix("minecraft:"),
            Some(
                "axolotl"
                    | "cod"
                    | "dolphin"
                    | "elder_guardian"
                    | "glow_squid"
                    | "guardian"
                    | "pufferfish"
                    | "salmon"
                    | "squid"
                    | "tadpole"
                    | "tropical_fish"
                    | "turtle"
            )
        )
    })
}
/// `EnchantmentHelper.getDamageBonus` for the typed families (1.21.1): each
/// pays a flat +2.5 per level against its tag and nothing otherwise.
/// Smite=34 undead, Bane of Arthropods=1 arthropods, Impaling=15 aquatic.
#[cfg_attr(not(test), allow(dead_code))]
fn typed_damage_bonus(enchant: i64, level: i64, etype: i64) -> f32 {
    let applies = match enchant {
        34 => enchantment_targets_undead(etype),
        1 => enchantment_targets_arthropod(etype),
        15 => enchantment_targets_aquatic(etype),
        _ => false,
    };
    if applies {
        2.5 * level as f32
    } else {
        0.0
    }
}
/// Bane of Arthropods' post-hit rider (vanilla `DamageEnchantment.doPostAttack`):
/// Slowness IV (amplifier 3) for a random `20 + nextInt(10 * level)` ticks,
/// i.e. the inclusive window `20..=(20 + 10 * level)`.
#[cfg_attr(not(test), allow(dead_code))]
fn bane_slowness_iv(level: i64) -> Option<(i32, i32, i32)> {
    (level >= 1).then(|| (3, 20, 20 + 10 * level as i32))
}
/// Facts `Player.attack` (1.21.1) evaluates for its critical-hit flag, captured
/// at swing time so the packet handler stays stateless.
#[derive(Clone, Copy, Debug, Default)]
struct CritContext {
    attack_charge: f32,
    fall_distance: f32,
    on_ground: bool,
    on_climbable: bool,
    in_water: bool,
    has_blindness: bool,
    is_passenger: bool,
    is_sprinting: bool,
    /// Elytra flight is not part of the vanilla predicate, but this server
    /// already bars airborne elytra crits; kept so live behaviour is preserved.
    fall_flying: bool,
    /// Vanilla requires `entity instanceof LivingEntity` — boats, item frames
    /// and other non-living entities never take a critical hit. (Armor stands
    /// *are* LivingEntity in vanilla and therefore can be.)
    target_is_living: bool,
}

/// `instanceof LivingEntity` for the crit predicate: living registry rows are
/// the ones with health — boats, item frames, minecarts and projectiles carry
/// none. Armor stands are LivingEntity in vanilla and read true here.
#[cfg_attr(not(test), allow(dead_code))]
fn entity_is_living(etype: i64) -> bool {
    entity_spec_by_id(etype).is_some_and(|spec| spec.max_health > 0.0)
}

/// Vanilla 1.21.1 `Player.attack` crit predicate: a charged falling blow that
/// is not grounded, climbing, wet, blinded, riding, or sprinting, against a
/// living target. Sprint-hits pay knockback instead of the 1.5x.
fn is_vanilla_critical_hit(ctx: &CritContext) -> bool {
    ctx.attack_charge > 0.9
        && ctx.fall_distance > 0.0
        && !ctx.on_ground
        && !ctx.on_climbable
        && !ctx.in_water
        && !ctx.has_blindness
        && !ctx.is_passenger
        && !ctx.is_sprinting
        && !ctx.fall_flying
        && ctx.target_is_living
}

#[expect(clippy::too_many_arguments, reason = "preserve the established placement and simulation call signature")]
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
    is_vanilla_critical_hit(&CritContext {
        attack_charge: charge,
        fall_distance: if falling { 1.0 } else { 0.0 },
        on_ground: grounded,
        on_climbable: climbing,
        in_water,
        has_blindness: blind,
        is_passenger: false,
        is_sprinting: sprinting,
        fall_flying,
        target_is_living: true,
    })
}

/// Vanilla `Player.attack` damage order (1.21.1): the attribute base is scaled
/// by `0.2 + 0.8 * charge^2`, the critical multiplier applies to that scaled
/// base only, and the enchantment bonus (`getDamageBonus`, scaled by a bare
/// `charge`) is added afterwards — a crit never multiplies Smite/Bane/Sharpness.
#[cfg_attr(not(test), allow(dead_code))]
fn melee_hit_damage(base: f32, enchant_bonus: f32, charge: f32, critical: bool) -> f32 {
    let scaled_base = base * (0.2 + 0.8 * charge * charge);
    let scaled_bonus = enchant_bonus * charge;
    scaled_base * if critical { 1.5 } else { 1.0 } + scaled_bonus
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

// ---------- per-mob fire ticks (issue #78) ----------
// `Entity.remainingFireTicks` keyed by entity id, held here rather than on the
// mob record so weapon ignition can never alias the creeper fuse countdown or
// the sunlight-burn refresh the environment tick owns. Values are server
// ticks, matching `fire_aspect_duration_ticks`/`fire_tick_update`.
static MOB_FIRE_TICKS: OnceLock<Mutex<HashMap<i32, u32>>> = OnceLock::new();
fn mob_fire_ticks() -> &'static Mutex<HashMap<i32, u32>> {
    MOB_FIRE_TICKS.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Vanilla `Entity.setRemainingFireTicks`: store a fresh burn without touching
/// `Mob.fuse` or `Mob.fire_ticks` bookkeeping owned elsewhere.
fn ignite_mob(eid: i32, ticks: u32) {
    mob_fire_ticks().lock().unwrap_or_else(|e| e.into_inner()).insert(eid, ticks);
}
/// Vanilla `Player.attack`'s `target.igniteForSeconds(4 * fireAspectLevel)`:
/// Fire Aspect I burns 80 game ticks (4s), Fire Aspect II 160 (8s).
#[cfg_attr(not(test), allow(dead_code))]
fn ignite_mob_fire_aspect(eid: i32, level: i64) {
    if level > 0 {
        ignite_mob(eid, fire_aspect_duration_ticks(level).max(0) as u32);
    }
}
/// The mob's remaining weapon/environmental burn, in server ticks.
#[cfg_attr(not(test), allow(dead_code))]
fn mob_fire_ticks_of(eid: i32) -> u32 {
    mob_fire_ticks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&eid)
        .copied()
        .unwrap_or(0)
}
/// Vanilla water/rain extinguish (`Entity.isInWaterOrRain`): clears the burn.
fn extinguish_mob_fire(eid: i32) {
    mob_fire_ticks().lock().unwrap_or_else(|e| e.into_inner()).remove(&eid);
}
/// One server tick of burning on the map store: decrements cleanly, pays
/// `1.0` HP every `MOB_BURN_TICK_GT` game ticks, never damages a fire-immune
/// mob, and goes out in water or rain. Returns the fire damage for the tick.
#[cfg_attr(not(test), allow(dead_code))]
fn tick_mob_fire(eid: i32, tick: u64, fire_immune: bool, wet: bool) -> bool {
    let mut map = mob_fire_ticks().lock().unwrap_or_else(|e| e.into_inner());
    if wet {
        map.remove(&eid);
        return false;
    }
    let Some(entry) = map.get_mut(&eid) else {
        return false;
    };
    let (remaining, damage) = fire_tick_update(*entry as i32, tick, fire_immune);
    if remaining > 0 {
        *entry = remaining as u32;
    } else {
        map.remove(&eid);
    }
    damage
}

// ---------- sword sweeping (issue #80) ----------
/// Vanilla `Player.attack`'s sweep knockback strength (`knockback(0.4F, ...)`).
#[cfg_attr(not(test), allow(dead_code))]
const SWEEP_KNOCKBACK: f64 = 0.4;
/// The vanilla gate around `sweepingAttack`: any sword (Sweeping Edge not
/// required), `attackStrengthScale > 0.9`, no crit, not sprinting, on ground.
/// The horizontal-speed cap (`walkDist - walkDistO < getSpeed()`) is checked
/// separately by `sweeping_movement_ok` since it needs live movement data.
#[cfg_attr(not(test), allow(dead_code))]
fn sweeping_attack_gate(charge: f32, on_ground: bool, sprinting: bool, critical: bool) -> bool {
    charge > 0.9 && on_ground && !sprinting && !critical
}
/// Vanilla `Player.attack`: a sweep is refused while the attacker is moving
/// faster than their own movement speed (e.g. sprint-jumping through the hit).
#[cfg_attr(not(test), allow(dead_code))]
fn sweeping_movement_ok(horizontal_speed: f64, speed_cap: f64) -> bool {
    horizontal_speed < speed_cap
}

// vanilla: blocking with a shield pays floor(blocked)+1 durability
fn damage_shield(me: &Arc<Client>, blocked: f32) {
    damage_shield_attempts(me, vanilla_shield_attempts(blocked))
}
/// Vanilla `LivingEntity.hurtCurrentlyUsedShield`: `1 + floor(amount)`
/// durability attempts, and only when the blocked hit is worth 3+.
fn vanilla_shield_attempts(damage: f32) -> i64 {
    if damage >= 3.0 {
        1 + damage.floor() as i64
    } else {
        0
    }
}
fn damage_shield_attempts(me: &Arc<Client>, attempts: i64) {
    let sh = name_to_item("shield").unwrap_or(-9);
    let slots = [45i16, 36 + *me.held.lock().unwrap_or_else(|e| e.into_inner())];
    let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
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
        for _ in 0..attempts.max(0) {
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

// ---------- VANILLA-COMBAT-2: missing 1.21.1 combat & survival parity ----------
//
// Wind-charge bursts (launch, fall reset, TRIGGER block interaction), the
// shield's 180° block arc with its 5-tick raise warmup and 100-tick axe
// disable, totem death protection with the void/`/kill` source exclusions, and
// trident Loyalty/Riptide/Channeling. Constants were read out of the 1.21.1
// server jar (`coh`/`cog`/`dcp`/`ddn`/`dco`, `cmx`, `buk`, `bsu`).

/// Which charge produced a wind burst — a thrown `WindCharge` item or the
/// Breeze's own `BreezeWindCharge` projectile.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
enum WindChargeKind {
    Player,
    Breeze,
}

/// The `Level.explode` arguments each charge supplies, plus the direct-hit
/// damage its `onHitEntity` pays (the burst itself always deals 0).
#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
struct WindChargeProfile {
    /// Explosion power; the launch reach is `radius * 2` (vanilla `hurtEntities`
    /// scans `power * 2`).
    radius: f64,
    /// `AdvancedExplosionDamageCalculator.getKnockbackMultiplier`.
    knockback_multiplier: f64,
    /// `onHitEntity` damage.
    direct_hit_damage: f32,
}

#[cfg_attr(not(test), allow(dead_code))]
fn wind_charge_profile(kind: WindChargeKind) -> WindChargeProfile {
    match kind {
        // `coh` (`WindCharge`): explode power 1.2, `Optional.of(1.22f)` in the
        // `ddn` damage calculator, `ExplosionInteraction.TRIGGER`.
        WindChargeKind::Player => WindChargeProfile {
            radius: 1.2,
            knockback_multiplier: 1.22,
            direct_hit_damage: 1.0,
        },
        // `cog` (`BreezeWindCharge`): explode power 3.0, same TRIGGER
        // interaction. The lane spec pins the launch multiplier at 0.6 —
        // "40% less knockback than a regular explosion".
        WindChargeKind::Breeze => WindChargeProfile {
            radius: 3.0,
            knockback_multiplier: 0.6,
            direct_hit_damage: 1.0,
        },
    }
}

/// `Explosion.hurtEntities`' knockback arm: `(1 - dist/(power*2))` falloff,
/// `seenPercent`, the charge's `knockbackMultiplier` and the target's
/// knockback resistance, applied along the normalized direction to the
/// target's eye as an additive delta velocity. Vanilla skips entities at the
/// exact burst center; non-finite inputs scrub to `None`.
#[cfg_attr(not(test), allow(dead_code))]
fn evaluate_wind_charge_burst(
    kind: WindChargeKind,
    impact: [f64; 3],
    target_pos: [f64; 3],
    target_eye_y: f64,
    seen_percent: f64,
    knockback_resist: f64,
) -> Option<[f64; 3]> {
    let profile = wind_charge_profile(kind);
    if !impact.iter().all(|v| v.is_finite())
        || !target_pos.iter().all(|v| v.is_finite())
        || !target_eye_y.is_finite()
        || !seen_percent.is_finite()
        || !knockback_resist.is_finite()
    {
        return None;
    }
    let d = [
        target_pos[0] - impact[0],
        target_eye_y - impact[1],
        target_pos[2] - impact[2],
    ];
    let dist = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    let reach = profile.radius * 2.0;
    if dist <= 1e-9 || dist / reach > 1.0 || seen_percent <= 0.0 {
        return None;
    }
    let strength = (1.0 - dist / reach)
        * seen_percent.clamp(0.0, 1.0)
        * profile.knockback_multiplier
        * (1.0 - knockback_resist.clamp(0.0, 1.0));
    Some([
        d[0] / dist * strength,
        d[1] / dist * strength,
        d[2] / dist * strength,
    ])
}

/// The wind-burst fall-distance reset: vanilla records the burst's Y so the
/// mace combo only counts the drop below it ("accumulates fall damage only
/// when falling below the Y level where the wind burst hit").
#[cfg_attr(not(test), allow(dead_code))]
fn wind_charge_reset_fall(c: &Client, burst_y: f64) {
    if burst_y.is_finite() {
        *c.fall_from.lock().unwrap_or_else(|e| e.into_inner()) = burst_y;
    }
}

/// `ExplosionInteraction.TRIGGER` on one block: non-iron doors, trapdoors and
/// fence gates flip `open`; levers flip `powered`; buttons press (the redstone
/// pass schedules the vanilla release); bells ring. Blocks held by a redstone
/// signal and the iron variants never move. Returns whether it triggered.
#[cfg_attr(not(test), allow(dead_code))]
fn wind_charge_trigger_at(dim: DimId, x: i32, y: i32, z: i32) -> bool {
    let Some(reg) = breg() else {
        return false;
    };
    let st = block_at_dim(dim, x, y, z);
    if st == 0 {
        return false;
    }
    let name = reg.name_of(st);
    let powered = reg
        .get_prop(st, "powered")
        .map(|p| p == "true")
        .unwrap_or(false);
    let set = |st2: u32, prop: &str, val: &str, px: i32, py: i32, pz: i32| -> bool {
        let nst = reg.with_prop(st2, prop, val);
        if nst == st2 {
            return false;
        }
        set_block_dim(dim, px, py, pz, nst);
        note_block_update_dim(dim, px, py, pz, nst);
        true
    };
    if name == "minecraft:lever" {
        return set(st, "powered", if powered { "false" } else { "true" }, x, y, z);
    }
    if reg.in_tag(st, "minecraft:buttons") || name.ends_with("_button") {
        return !powered && set(st, "powered", "true", x, y, z);
    }
    if name == "minecraft:bell" {
        return !powered && set(st, "powered", "true", x, y, z);
    }
    let iron = name == "minecraft:iron_door" || name == "minecraft:iron_trapdoor";
    let toggleable =
        name.ends_with("_door") || name.ends_with("_trapdoor") || name.ends_with("_fence_gate");
    if !toggleable || iron || powered {
        return false;
    }
    let opened = reg
        .get_prop(st, "open")
        .map(|v| v == "true")
        .unwrap_or(false);
    let val = if opened { "false" } else { "true" };
    let ok = set(st, "open", val, x, y, z);
    // `DoorBlock.setOpen` flips both halves, not just the half the burst hit.
    if ok && name.ends_with("_door") {
        for oy in [-1i32, 1] {
            let ost = block_at_dim(dim, x, y + oy, z);
            if reg.name_of(ost) == name {
                set(ost, "open", val, x, y + oy, z);
            }
        }
    }
    ok
}

/// The TRIGGER pass over the burst's block radius.
#[cfg_attr(not(test), allow(dead_code))]
fn wind_charge_trigger_blocks(dim: DimId, impact: [f64; 3], radius: f64) -> usize {
    if !impact.iter().all(|v| v.is_finite()) || !radius.is_finite() {
        return 0;
    }
    let [bx, by, bz] = impact.map(|v| v.floor() as i32);
    let r = radius.ceil() as i32;
    let mut hits = 0;
    for dx in -r..=r {
        for dy in -r..=r {
            for dz in -r..=r {
                if (dx * dx + dy * dy + dz * dz) as f64 > radius * radius + 1e-9 {
                    continue;
                }
                if wind_charge_trigger_at(dim, bx + dx, by + dy, bz + dz) {
                    hits += 1;
                    wind_charge_note_neighbors(dim, bx + dx, by + dy, bz + dz);
                }
            }
        }
    }
    hits
}

/// The `neighborUpdate` a triggered block sends to each face: adjacent note
/// blocks re-evaluate their instrument and the redstone signal, and a rising
/// edge plays the stored note — the vanilla `NoteBlock.neighborChanged` the
/// wind-burst toggle provokes. A played note also refreshes every allay in
/// the vanilla hearing radius.
#[cfg_attr(not(test), allow(dead_code))]
fn wind_charge_note_neighbors(dim: DimId, x: i32, y: i32, z: i32) {
    let Some(reg) = breg() else {
        return;
    };
    let now = live_block_entity_time();
    let tick = TICK.load(Ordering::Relaxed);
    for (dx, dy, dz) in [
        (1, 0, 0),
        (-1, 0, 0),
        (0, 1, 0),
        (0, -1, 0),
        (0, 0, 1),
        (0, 0, -1),
    ] {
        let pos = (x + dx, y + dy, z + dz);
        if reg.name_of(block_at_dim(dim, pos.0, pos.1, pos.2)) != "minecraft:note_block" {
            continue;
        }
        let Some(instrument) = note_block_refresh_instrument_dim(dim, pos) else {
            continue;
        };
        let has_signal = live_crafter_powered(reg, (dim, pos.0, pos.1, pos.2), now);
        let Some((_, played)) = note_block_power_dim(dim, pos, has_signal) else {
            continue;
        };
        if !played {
            continue;
        }
        // Custom heads play the skull's `note_block_sound` component instead
        // of an instrument name (vanilla NoteBlock + skull sound override).
        let sound = if instrument.state_name() == "custom_head" {
            live_block_entity_snapshot((dim, pos.0, pos.1 + 1, pos.2))
                .and_then(|(_, tag)| {
                    tag.get("note_block_sound")
                        .and_then(|t| t.string())
                        .and_then(|s| s.to_utf8().ok())
                })
                .unwrap_or_else(|| "block.note_block.imitate.custom_head".to_string())
        } else {
            format!("block.note_block.{}", instrument.state_name())
        };
        let packet = sound_pkt_named(
            &sound,
            f64::from(pos.0) + 0.5,
            f64::from(pos.1) + 0.5,
            f64::from(pos.2) + 0.5,
        );
        broadcast_chunk_dim(dim, pos.0 >> 4, pos.2 >> 4, |_c| (0x5b, packet.clone()));
        for mob in mobs().lock().unwrap_or_else(|e| e.into_inner()).iter() {
            if Some(mob.etype) != name_to_etype("allay") {
                continue;
            }
            let (mx, my, mz, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
            let near = (mx - f64::from(pos.0)).powi(2)
                + (my - f64::from(pos.1)).powi(2)
                + (mz - f64::from(pos.2)).powi(2)
                <= 48.0_f64.powi(2);
            if mob.dim == dim && near {
                live_allay_note(mob.eid, pos, tick);
            }
        }
    }
}

/// Applies a wind-charge burst: launches entities (`hurtEntities`), resets the
/// thrower's fall distance to the burst Y, fires the TRIGGER block pass and
/// broadcasts the burst. `thrower_eid` is the charge's owner (-1 when none).
#[cfg_attr(not(test), allow(dead_code))]
fn wind_charge_burst(dim: DimId, kind: WindChargeKind, impact: [f64; 3], thrower_eid: i32) {
    let profile = wind_charge_profile(kind);
    for c in client_snapshot() {
        if *c.dim.lock().unwrap_or_else(|e| e.into_inner()) != dim
            || *c.gm.lock().unwrap_or_else(|e| e.into_inner()) == 3
        {
            continue;
        }
        let (x, y, z, ..) = {
            let p = c.pos.lock().unwrap_or_else(|e| e.into_inner());
            *p
        };
        let eye = y
            + f64::from(player_eye_height(
                *c.sneaking.lock().unwrap_or_else(|e| e.into_inner()),
                *c.fall_fly.lock().unwrap_or_else(|e| e.into_inner()),
            ));
        if let Some(dv) =
            evaluate_wind_charge_burst(kind, impact, [x, y, z], eye, 1.0, knockback_resist_of(&c))
        {
            let pkt = velocity_pkt(
                c.eid,
                (dv[0] * 8000.0) as i16,
                (dv[1] * 8000.0) as i16,
                (dv[2] * 8000.0) as i16,
            );
            if let Ok(mut w) = c.sock.lock() {
                let _ = send_packet(&mut w, 0x5a, pkt.clone());
            }
            broadcast_seen_ex(c.eid, c.eid, |_c| (0x5a, pkt.clone()));
        }
        if c.eid == thrower_eid {
            wind_charge_reset_fall(&c, impact[1]);
        }
    }
    for m in mobs().lock().unwrap_or_else(|e| e.into_inner()).iter() {
        if m.dim != dim {
            continue;
        }
        let (mx, my, mz, ..) = *m.pos.lock().unwrap_or_else(|e| e.into_inner());
        let eye = my + f64::from(entity_eye_height_1211(m.etype));
        if let Some(dv) =
            evaluate_wind_charge_burst(kind, impact, [mx, my, mz], eye, 1.0, 0.0)
        {
            let mut p = m.pos.lock().unwrap_or_else(|e| e.into_inner());
            p.0 += dv[0];
            p.1 += dv[1];
            p.2 += dv[2];
            let (nx, ny, nz, yaw, pitch) = *p;
            drop(p);
            broadcast_seen(m.eid, |_c| {
                (0x70, entity_teleport(m.eid, nx, ny, nz, yaw, pitch, true))
            });
        }
    }
    wind_charge_trigger_blocks(dim, impact, profile.radius);
    broadcast(-1, |_c| {
        (
            0x20,
            explosion_pkt_with_power(impact[0], impact[1], impact[2], profile.radius as f32, &[]),
        )
    });
    ac_note_explosion(dim, impact, profile.radius);
}

// ---------- shield arc / warmup / axe disable ----------

/// `getTicksUsingItem() >= 5`: the shield covers the player only after this
/// many game ticks of use.
const SHIELD_WARMUP_GT: u64 = 5;
/// `disableShield`/`ServerGamePacketListenerImpl` shield cooldown — 100 game
/// ticks (5s at 20 TPS).
const SHIELD_DISABLE_GT: u32 = 100;
/// Wall-clock form of the disable window the `shield_cd` timestamp tracks.
const SHIELD_DISABLE_DUR: Duration = Duration::from_secs(5);

/// eid -> server tick a raised shield was first observed (the warmup clock;
/// `blocking` itself flips in `menus_and_items`).
static SHIELD_RAISED_AT: OnceLock<Mutex<HashMap<i32, u64>>> = OnceLock::new();
fn shield_raised() -> &'static Mutex<HashMap<i32, u64>> {
    SHIELD_RAISED_AT.get_or_init(|| Mutex::new(HashMap::new()))
}
/// eid -> the server tick the Riptide spin attack ends (`startAutoSpinAttack`).
static RIPTIDE_SPIN: OnceLock<Mutex<HashMap<i32, u64>>> = OnceLock::new();
fn riptide_spin() -> &'static Mutex<HashMap<i32, u64>> {
    RIPTIDE_SPIN.get_or_init(|| Mutex::new(HashMap::new()))
}
/// kind==1 (trident) projectile eids already seen on a tick (Riptide resolves
/// on first observation).
static TRIDENT_SEEN: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();
fn trident_seen() -> &'static Mutex<HashSet<i32>> {
    TRIDENT_SEEN.get_or_init(|| Mutex::new(HashSet::new()))
}
/// kind==1 projectile eids currently in Loyalty return flight.
static TRIDENT_RETURNING: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();
fn trident_returning() -> &'static Mutex<HashSet<i32>> {
    TRIDENT_RETURNING.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Per-tick state maintenance for this lane: tracks shield raise times by
/// observing the `blocking` flag and prunes the eid-keyed lane maps.
fn combat2_state_tick() {
    let now = TICK_IDX.load(Ordering::Relaxed);
    let cls = client_snapshot();
    let mut live = HashSet::with_capacity(cls.len());
    let mut raised = shield_raised().lock().unwrap_or_else(|e| e.into_inner());
    for c in &cls {
        live.insert(c.eid);
        if *c.blocking.lock().unwrap_or_else(|e| e.into_inner()) {
            raised.entry(c.eid).or_insert(now);
        } else {
            raised.remove(&c.eid);
        }
    }
    raised.retain(|eid, _| live.contains(eid));
    drop(raised);
    riptide_spin()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|eid, _| live.contains(eid));
    wind_charge_projectile_step();
    fireworks_step();
}

/// Move each in-flight firework and detonate it when its component-derived
/// fuse expires. Vanilla firework explosions damage entities only — they do
/// not break blocks.
fn fireworks_step() {
    let mut expired = Vec::new();
    {
        let mut fws = fireworks().lock().unwrap_or_else(|e| e.into_inner());
        for f in fws.iter_mut() {
            f.ttl -= 1;
            f.x += f.dx;
            f.y += f.dy;
            f.z += f.dz;
            if f.ttl <= 0 {
                expired.push((f.eid, f.owner, f.x, f.y, f.z, f.explosions));
            }
        }
        fws.retain(|f| !expired.iter().any(|(e, ..)| *e == f.eid));
    }
    for (eid, _owner, x, y, z, explosions) in expired {
        firework_explode(x, y, z, explosions);
        broadcast_seen_destroy(eid);
    }
}

/// Vanilla `FireworkRocketEntity.explode`: `5 + 2 * explosions` blast damage
/// inside a ~5-block radius, no block destruction.
fn firework_explode(x: f64, y: f64, z: f64, explosions: i64) {
    let packet = explosion_pkt_with_power(x, y, z, 1.0, &[]);
    broadcast(-1, |_c| (0x20, packet.clone()));
    if explosions <= 0 {
        return;
    }
    let power = (5.0 + 2.0 * explosions as f64) as f32;
    for cl in client_snapshot() {
        let (px, py, pz, ..) = *cl.pos.lock().unwrap_or_else(|e| e.into_inner());
        let dd = ((px - x).powi(2) + (py - y).powi(2) + (pz - z).powi(2)).sqrt();
        if dd < 5.0 && *cl.gm.lock().unwrap_or_else(|e| e.into_inner()) != 1 {
            let raw = ((5.0 - dd) * (f64::from(power) / 5.0)) as f32;
            let Some((accepted, _)) = damage_after_invulnerability(&cl, raw) else {
                continue;
            };
            let mut h = cl.hp.lock().unwrap_or_else(|e| e.into_inner());
            *h = (*h - hurt_absorb(&cl, dmg_after(&cl, accepted, 2))).max(0.0);
            if *h <= 0.0 {
                drop(h);
                totem_check(&cl);
                h = cl.hp.lock().unwrap_or_else(|e| e.into_inner());
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
    for mob in mobs().lock().unwrap_or_else(|e| e.into_inner()).iter() {
        let (mx, my, mz, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
        let dd = ((mx - x).powi(2) + (my - y).powi(2) + (mz - z).powi(2)).sqrt();
        if dd < 5.0 {
            let mut hp = mob.hp.lock().unwrap_or_else(|e| e.into_inner());
            *hp = (*hp - power * (5.0 - dd) as f32 / 5.0).max(0.0);
        }
    }
}

/// Per-mob age for `wind_charge`/`breeze_wind_charge` projectile entities.
#[cfg_attr(not(test), allow(dead_code))]
fn wind_charge_ages() -> &'static Mutex<HashMap<i32, u16>> {
    static AGES: OnceLock<Mutex<HashMap<i32, u16>>> = OnceLock::new();
    AGES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Wind-charge projectiles (`wind_charge`, `breeze_wind_charge`) spawned by
/// `/summon` or a spawn egg live in the mob store: they are entities, not AI
/// mobs, and the generic velocity pass already moves them. Burst each charge
/// on the first solid-block contact or after its vanilla 200-tick lifetime.
#[cfg_attr(not(test), allow(dead_code))]
fn wind_charge_projectile_step() {
    let player_charge = name_to_etype("wind_charge");
    let breeze_charge = name_to_etype("breeze_wind_charge");
    let Some(reg) = breg() else {
        return;
    };
    let mut bursts = Vec::new();
    {
        let mut ages = wind_charge_ages().lock().unwrap_or_else(|e| e.into_inner());
        let mut seen = HashSet::new();
        for mob in mobs().lock().unwrap_or_else(|e| e.into_inner()).iter() {
            let kind = if Some(mob.etype) == player_charge {
                WindChargeKind::Player
            } else if Some(mob.etype) == breeze_charge {
                WindChargeKind::Breeze
            } else {
                continue;
            };
            let (mx, my, mz, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
            let age = ages.entry(mob.eid).or_insert(0);
            *age = age.saturating_add(1);
            let hit_solid = reg.is_solid(block_at_dim(
                mob.dim,
                mx.floor() as i32,
                my.floor() as i32,
                mz.floor() as i32,
            ));
            if hit_solid || *age >= 200 {
                ages.remove(&mob.eid);
                *mob.hp.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
                bursts.push((mob.dim, kind, [mx, my, mz]));
            } else {
                seen.insert(mob.eid);
            }
        }
        ages.retain(|eid, _| seen.contains(eid));
    }
    for (dim, kind, impact) in bursts {
        wind_charge_burst(dim, kind, impact, -1);
    }
}

/// Game ticks the player's shield has been up (vanilla `getTicksUsingItem`);
/// an unrecorded raise counts as 0 — vanilla refuses the block in that window.
fn shield_raised_ticks(c: &Client) -> u64 {
    let now = TICK_IDX.load(Ordering::Relaxed);
    shield_raised()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&c.eid)
        .map_or(0, |since| {
            now.saturating_sub(*since) * u64::from(GAME_TICKS_PER_TICK)
        })
}

/// The spin-attack window a Riptide launch armed for `eid`, in game ticks.
#[cfg_attr(not(test), allow(dead_code))]
fn spin_attack_ticks_left(eid: i32) -> u64 {
    let now = TICK_IDX.load(Ordering::Relaxed);
    riptide_spin()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&eid)
        .map_or(0, |until| {
            until.saturating_sub(now) * u64::from(GAME_TICKS_PER_TICK)
        })
}

/// The attack a shield may block: blocker's raise time and cooldown, the
/// damage-source gate, the two horizontal direction vectors, the attacker
/// disable data and the raw damage (for the durability calc).
#[derive(Clone, Copy, Debug)]
struct ShieldAttackCtx {
    /// Game ticks the shield has been raised (`getTicksUsingItem`).
    raised_ticks: u64,
    /// A previous 100-tick disable is still running.
    cooldown_active: bool,
    /// `DamageTypeTags.BYPASSES_SHIELD` — nothing blocks it.
    unblockable: bool,
    /// Horizontal unit view vector of the blocker.
    look: [f64; 2],
    /// Horizontal unit vector blocker -> damage source.
    to_attacker: [f64; 2],
    /// `LivingEntity.canDisableShield` — attacker main hand is an axe.
    attacker_can_disable: bool,
    /// Player attackers roll; disabling mobs (Vindicator/Piglin Brute) always.
    attacker_is_player: bool,
    attacker_sprinting: bool,
    /// Efficiency level on the attacking axe (player roll only).
    attacker_efficiency: i64,
    /// Pre-drawn roll in [0,1) for the player-attacker chance.
    disable_roll: f32,
    /// Raw incoming damage for the `hurtCurrentlyUsedShield` durability calc.
    damage: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ShieldBlock {
    blocked: bool,
    /// `1 + floor(damage)` when damage >= 3 else 0 (`hurtCurrentlyUsedShield`).
    durability_attempts: i64,
    /// `Some(100)` when `disableShield` arms the 100-tick cooldown.
    disable_ticks: Option<u32>,
}

/// `LivingEntity.isDamageSourceBlocked` + `blockUsingShield`: the frontal
/// 180° hemisphere (`dot(look_xz, to_attacker_xz) > 0`), the 5-tick warmup,
/// the unblockable-source and running-cooldown gates, the axe-disable roll
/// and the shield durability math.
fn evaluate_shield_block(ctx: &ShieldAttackCtx) -> ShieldBlock {
    let miss = ShieldBlock {
        blocked: false,
        durability_attempts: 0,
        disable_ticks: None,
    };
    if ctx.unblockable || ctx.cooldown_active || ctx.raised_ticks < SHIELD_WARMUP_GT {
        return miss;
    }
    let dot = ctx.look[0] * ctx.to_attacker[0] + ctx.look[1] * ctx.to_attacker[1];
    if dot.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
        return miss;
    }
    let durability_attempts = vanilla_shield_attempts(ctx.damage);
    let disable_ticks = if ctx.attacker_can_disable {
        let chance = if ctx.attacker_is_player {
            // `Player.disableShield`: 25% + 5%/Efficiency, +75% while sprinting.
            let base = 0.25 + 0.05 * ctx.attacker_efficiency as f32;
            if ctx.attacker_sprinting {
                base + 0.75
            } else {
                base
            }
        } else {
            1.0
        };
        (ctx.disable_roll < chance.min(1.0)).then_some(SHIELD_DISABLE_GT)
    } else {
        None
    };
    ShieldBlock {
        blocked: true,
        durability_attempts,
        disable_ticks,
    }
}

/// Builds the pure attack context for a hit on `c` from `attacker`: the raise
/// clock from `SHIELD_RAISED_AT`, the wall-clock cooldown flag and the two
/// horizontally-normalized direction vectors vanilla compares.
fn shield_hit_ctx(c: &Client, attacker: [f64; 3], damage: f32) -> ShieldAttackCtx {
    let (px, _, pz, yaw, _, _) = {
        let p = c.pos.lock().unwrap_or_else(|e| e.into_inner());
        *p
    };
    let yr = (yaw as f64).to_radians();
    // Vanilla getViewVector horizontal projection: (-sin yaw, cos yaw).
    let look_raw = [-yr.sin(), yr.cos()];
    let ln = (look_raw[0] * look_raw[0] + look_raw[1] * look_raw[1]).sqrt();
    let look = if ln > 1e-9 {
        [look_raw[0] / ln, look_raw[1] / ln]
    } else {
        [0.0, 1.0]
    };
    let to_raw = [attacker[0] - px, attacker[2] - pz];
    let tn = (to_raw[0] * to_raw[0] + to_raw[1] * to_raw[1]).sqrt();
    let to_attacker = if tn > 1e-9 {
        [to_raw[0] / tn, to_raw[1] / tn]
    } else {
        [0.0, 0.0]
    };
    ShieldAttackCtx {
        raised_ticks: shield_raised_ticks(c),
        cooldown_active: c
            .shield_cd
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .elapsed()
            <= SHIELD_DISABLE_DUR,
        unblockable: false,
        look,
        to_attacker,
        attacker_can_disable: false,
        attacker_is_player: false,
        attacker_sprinting: false,
        attacker_efficiency: 0,
        disable_roll: 0.0,
        damage,
    }
}

/// `LivingEntity.canDisableShield` for the mobs this registry fields: the
/// axe-wielding Vindicator (114) and Piglin Brute (79).
fn mob_can_disable_shield(etype: i64) -> bool {
    matches!(etype, 79 | 114)
}

/// `LivingEntity.blockUsingShield`'s disable arm applied to the live fields.
fn apply_shield_disable(c: &Client) {
    *c.blocking.lock().unwrap_or_else(|e| e.into_inner()) = false;
    *c.shield_cd.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
}

// ---------- totem of undying ----------

/// Vanilla `DamageTypeTags.BYPASSES_INVULNERABILITY` splits sources that can
/// trigger `checkTotemDeathProtection` from `fellOutOfWorld`/`generic_kill`,
/// which always bypass it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TotemDamageKind {
    Ordinary,
    Void,
    #[cfg_attr(not(test), allow(dead_code))]
    Kill,
}

/// `LivingEntity.checkTotemDeathProtection`: on lethal ordinary damage a held
/// totem (offhand first, then the selected main-hand slot) is consumed, hp is
/// set to 1, existing effects are cleared, the vanilla grant applies
/// (Regeneration II 900t, Absorption II 100t, Fire Resistance I 800t) and
/// entity status 35 goes out. Void and `/kill` sources bypass protection.
/// Returns whether a totem popped.
fn try_trigger_totem_of_undying(c: &Client, hp_after_damage: f32, kind: TotemDamageKind) -> bool {
    if kind != TotemDamageKind::Ordinary
        || hp_after_damage > 0.0
        || *c.gm.lock().unwrap_or_else(|e| e.into_inner()) == 1
    {
        return false;
    }
    let mut inv = c.inv.lock().unwrap_or_else(|e| e.into_inner());
    let held_k = 36i16 + (*c.held.lock().unwrap_or_else(|e| e.into_inner()));
    for k in [45i16, held_k] {
        if let Some(st) = inv.get(&k).cloned().filter(|s| s.0 == 1163) {
            if st.1 <= 1 {
                inv.remove(&k);
            } else {
                inv.insert(k, (st.0, st.1 - 1, st.2));
            }
            let now = inv.get(&k).cloned().unwrap_or((0, 0, Vec::new()));
            drop(inv);
            *c.hp.lock().unwrap_or_else(|e| e.into_inner()) = 1.0;
            resync_slot(c, 0, k, &now);
            // entity_status 35 = totem use; i32 entity id + i8 status
            let mut st_pkt = Vec::new();
            st_pkt.extend_from_slice(&c.eid.to_be_bytes());
            st_pkt.push(35);
            let _ = send_packet(&mut c.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x1f, st_pkt.clone());
            broadcast_seen(c.eid, |_c| (0x1f, st_pkt.clone()));
            let (px, py, pz, ..) = {
                let p = c.pos.lock().unwrap_or_else(|e| e.into_inner());
                *p
            };
            let snd = sound_pkt_named("item.totem.use", px, py, pz);
            broadcast_seen(c.eid, |_c| (0x68, snd.clone()));
            // Death protection replaces existing effects with the vanilla grant set.
            grant_totem_effects(c, &mut c.effs.lock().unwrap_or_else(|e| e.into_inner()));
            let _ = send_packet(&mut c.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x5d, health_pkt_c(1.0, c));
            return true;
        }
    }
    false
}

// ---------- trident specials ----------

/// `evaluate_trident_special` inputs: enchant levels read off the thrown
/// trident (Loyalty 19 / Riptide 31 / Channeling 5) and the situation flags
/// each vanilla branch reads.
#[derive(Clone, Copy, Debug, Default)]
struct TridentSpecialCtx {
    loyalty: i64,
    riptide: i64,
    channeling: i64,
    /// `Entity.isInWaterOrRain` for the thrower.
    wet: bool,
    /// `onGround` of the thrower — vanilla adds a small vertical boost.
    on_ground: bool,
    /// Unit look vector of the thrower.
    look: [f64; 3],
    /// Trident position (return origin) and the owner's eye position
    /// (return target).
    trident: [f64; 3],
    owner: [f64; 3],
    /// `level.isThundering()`.
    thundering: bool,
    /// `level.canSeeSky(hitBlockPos)`.
    open_sky: bool,
}

/// `TridentItem.releaseUsing`'s launch payload.
#[derive(Clone, Copy, Debug, PartialEq)]
struct RiptideLaunch {
    /// `push(look * (0.75 + 0.75 * riptideLevel))`.
    velocity: [f64; 3],
    /// `startAutoSpinAttack(20)`.
    spin_ticks: u32,
    /// `move(SELF, 0..1.2..0)` when released on the ground, else 0.
    ground_boost: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum RiptideOutcome {
    /// No Riptide on the trident.
    NotRiptide,
    /// Riptide present but the thrower is dry — `releaseUsing` refuses the
    /// throw entirely (no projectile, the item stays in hand).
    Refused,
    Launch(RiptideLaunch),
}

#[derive(Clone, Copy, Debug)]
struct TridentSpecial {
    /// `ThrownTrident`'s Loyalty return impulse `dir * 0.05 * loyalty` toward
    /// the owner's eye — it applies even below `min_y` (the return survives
    /// the void). `None` without Loyalty 1..=3.
    loyalty_return: Option<[f64; 3]>,
    riptide: RiptideOutcome,
    /// `ThrownTrident` channeling lightning: thunderstorm AND open sky.
    channeling_lightning: bool,
}

fn evaluate_trident_special(ctx: &TridentSpecialCtx) -> TridentSpecial {
    let loyalty_return = if (1..=3).contains(&ctx.loyalty)
        && ctx
            .trident
            .iter()
            .chain(ctx.owner.iter())
            .all(|v| v.is_finite())
    {
        let d = [
            ctx.owner[0] - ctx.trident[0],
            ctx.owner[1] - ctx.trident[1],
            ctx.owner[2] - ctx.trident[2],
        ];
        let n = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        if n > 1e-9 {
            let s = 0.05 * ctx.loyalty as f64;
            Some([d[0] / n * s, d[1] / n * s, d[2] / n * s])
        } else {
            None
        }
    } else {
        None
    };
    let riptide = if !(1..=3).contains(&ctx.riptide) {
        RiptideOutcome::NotRiptide
    } else if !ctx.wet || !ctx.look.iter().all(|v| v.is_finite()) {
        RiptideOutcome::Refused
    } else {
        let n = (ctx.look[0] * ctx.look[0] + ctx.look[1] * ctx.look[1] + ctx.look[2] * ctx.look[2])
            .sqrt();
        if n <= 1e-9 {
            RiptideOutcome::Refused
        } else {
            let speed = 0.75 + 0.75 * ctx.riptide as f64;
            RiptideOutcome::Launch(RiptideLaunch {
                velocity: [
                    ctx.look[0] / n * speed,
                    ctx.look[1] / n * speed,
                    ctx.look[2] / n * speed,
                ],
                spin_ticks: 20,
                ground_boost: if ctx.on_ground { 1.2 } else { 0.0 },
            })
        }
    };
    TridentSpecial {
        loyalty_return,
        riptide,
        channeling_lightning: ctx.channeling > 0 && ctx.thundering && ctx.open_sky,
    }
}

/// `Level.canSeeSky`: sky light reaches a position only through zero-opacity
/// states — air and glass pass, while leaves and water attenuate below 15.
fn can_see_sky(dim: DimId, x: i32, y: i32, z: i32) -> bool {
    if !dim.has_skylight() {
        return false;
    }
    let Some(reg) = breg() else {
        return false;
    };
    let top = dim.min_y() + dim.height();
    for yy in y.max(dim.min_y())..top {
        if reg.opacity_of(block_at_dim(dim, x, yy, z)) != 0 {
            return false;
        }
    }
    true
}

/// `Entity.isInWaterOrRain` for a client: the AABB touches water, or it is
/// raining at a position with open sky.
fn trident_thrower_wet(c: &Client) -> bool {
    let (x, y, z, ..) = {
        let p = c.pos.lock().unwrap_or_else(|e| e.into_inner());
        *p
    };
    let dim = *c.dim.lock().unwrap_or_else(|e| e.into_inner());
    let sneaking = *c.sneaking.lock().unwrap_or_else(|e| e.into_inner());
    let fall_fly = *c.fall_fly.lock().unwrap_or_else(|e| e.into_inner());
    if aabb_in_water_with(player_bounding_box(x, y, z, sneaking, fall_fly), &|bx, by, bz| {
        block_at_dim(dim, bx, by, bz)
    }) {
        return true;
    }
    RAINING.load(Ordering::Relaxed) > 0
        && can_see_sky(
            dim,
            x.floor() as i32,
            (y + f64::from(player_eye_height(sneaking, fall_fly))).floor() as i32,
            z.floor() as i32,
        )
}

/// Vanilla Riptide never despawns the item — `releaseUsing` keeps it in hand.
/// The local throw consumed it already, so it is pushed back into the held
/// slot (or dropped at the feet when the hand has moved on).
fn trident_restore(c: &Arc<Client>, item: i64, comps: &Comps) {
    if item <= 0 {
        return;
    }
    let hs = 36 + *c.held.lock().unwrap_or_else(|e| e.into_inner());
    {
        let mut inv = c.inv.lock().unwrap_or_else(|e| e.into_inner());
        if let std::collections::hash_map::Entry::Vacant(e) = inv.entry(hs) {
            e.insert((item, 1, comps.clone()));
            let st = inv.get(&hs).cloned().unwrap_or((0, 0, Vec::new()));
            drop(inv);
            resync_slot(c, 0, hs, &st);
            return;
        }
    }
    let (x, y, z, ..) = {
        let p = c.pos.lock().unwrap_or_else(|e| e.into_inner());
        *p
    };
    drops()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Drop {
            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
            x,
            y,
            z,
            item,
            cnt: 1,
            comps: comps.clone(),
            age: 0,
            target: None,
            thrower: None,
        });
}
// vanilla hurtArmor: each worn piece pays max(1, floor(raw/4)); each
// piece rolls its own Unbreaking. Returns slots that broke (for resync).
fn damage_armor(me: &Arc<Client>, raw_dmg: f32) {
    let per = (raw_dmg / 4.0).floor().max(1.0) as i64;
    let mut broke = false;
    {
        let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
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
        *me.armor_pts.lock().unwrap_or_else(|e| e.into_inner()) = armor_pts_of(&inv);
    }
    if broke {
        let (x, y, z, ..) = {
            let p = me.pos.lock().unwrap_or_else(|e| e.into_inner());
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
            .unwrap_or_else(|e| e.into_inner())
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
    extinguish_mob_fire(deid);
    let Some((mob, state)) = remove_live_mob(deid) else { return };
    let burning = *mob.fire_ticks.lock().unwrap_or_else(|e| e.into_inner()) > 0;
    live_mob_death(&mob, state, [mx, my, mz]);
    broadcast_seen_destroy(deid);
    let was_sheared = sheared().lock().unwrap_or_else(|e| e.into_inner()).remove(&deid);
    for (item, cnt, comps) in mob_loot(etype, burning)
        .into_iter()
        .filter(|st| !(was_sheared && st.0 == 202))
        .filter(|_| GR_MOB_LOOT.load(Ordering::Relaxed) == 1)
    {
        drop_spawn(mx, my, mz, item, cnt, comps, 0, None, None);
    }
    xp_spawn(mx, my, mz, mob_xp(etype));
    // spawn streams via interest_tick
}

fn mob_env_die(deid: i32, etype: i64, mx: f64, my: f64, mz: f64, burning: bool) {
    extinguish_mob_fire(deid);
    // environmental death (suffocation/drowning/fall): drops but no XP —
    // vanilla only drops XP when recently hurt by a player
    let Some((mob, state)) = remove_live_mob(deid) else { return };
    live_mob_death(&mob, state, [mx, my, mz]);
    broadcast_seen_destroy(deid);
    sheared().lock().unwrap_or_else(|e| e.into_inner()).remove(&deid);
    for (item, cnt, comps) in mob_loot(etype, burning)
        .into_iter()
        .filter(|_| GR_MOB_LOOT.load(Ordering::Relaxed) == 1)
    {
        drop_spawn(mx, my, mz, item, cnt, comps, 0, None, None);
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
            if pl.entries.is_empty() {
                continue;
            }
            // Datapack pools may declare up to 64 rolls; the per-roll windows
            // pass bit 63 at roll 8, and `r * 8` itself wraps a u8 at roll 32.
            let e = &pl.entries
                [(h.wrapping_shr(u32::from(r) * 8 + 3) as usize) % pl.entries.len()];
            let span = (e.max - e.min + 1) as u64;
            let cnt = e.min + (((h.wrapping_shr(u32::from(r) * 8 + 17)) % span.max(1)) as u8);
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
    if let Some(pools) = mob_loot_tables().lock().unwrap_or_else(|e| e.into_inner()).get(&etype).map(|p| {
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
// Vanilla default `generic.armor_toughness` baked into armor items as their
// default attribute_modifiers component (diamond +2, netherite +3 per piece).
// The default only counts while the piece carries no explicit component.
fn armor_default_toughness(item: i64) -> f64 {
    for (n, t) in [
        ("netherite_helmet", 3.0),
        ("netherite_chestplate", 3.0),
        ("netherite_leggings", 3.0),
        ("netherite_boots", 3.0),
        ("diamond_helmet", 2.0),
        ("diamond_chestplate", 2.0),
        ("diamond_leggings", 2.0),
        ("diamond_boots", 2.0),
    ] {
        if name_to_item(n) == Some(item) {
            return t;
        }
    }
    0.0
}
fn armor_default_knockback_resistance(item: i64) -> f64 {
    for n in [
        "netherite_helmet",
        "netherite_chestplate",
        "netherite_leggings",
        "netherite_boots",
    ] {
        if name_to_item(n) == Some(item) {
            return 0.1;
        }
    }
    0.0
}
fn armor_toughness_of(pairs: &[(ItemAttrSlotGroup, &Stack)]) -> f64 {
    let mut base = 0.0;
    for (grp, st) in pairs {
        if matches!(
            grp,
            ItemAttrSlotGroup::Head
                | ItemAttrSlotGroup::Chest
                | ItemAttrSlotGroup::Legs
                | ItemAttrSlotGroup::Feet
        ) && comp_get(&st.2, COMP_ATTRIBUTE_MODIFIERS).is_none()
        {
            base += armor_default_toughness(st.0);
        }
    }
    apply_item_attribute_modifiers(base, pairs, ATTR_ARMOR_TOUGHNESS)
}
fn knockback_resist_of(c: &Client) -> f64 {
    let equipped = equipped_pairs(c);
    let pairs: Vec<(ItemAttrSlotGroup, &Stack)> =
        equipped.iter().map(|(g, s)| (*g, s)).collect();
    let mut base = 0.0;
    for (grp, st) in &pairs {
        if matches!(
            grp,
            ItemAttrSlotGroup::Head
                | ItemAttrSlotGroup::Chest
                | ItemAttrSlotGroup::Legs
                | ItemAttrSlotGroup::Feet
        ) && comp_get(&st.2, COMP_ATTRIBUTE_MODIFIERS).is_none()
        {
            base += armor_default_knockback_resistance(st.0);
        }
    }
    // Vanilla LivingEntity.knockback clamps the resistance factor to 1.
    apply_item_attribute_modifiers(base, &pairs, ATTR_KNOCKBACK_RESISTANCE).clamp(0.0, 1.0)
}
// vanilla `CombatRules.getDamageAfterAbsorb`: armor points are weakened by
// the incoming damage scaled on armor toughness, the reduction clamped to
// [armor*0.2, 20] and capped at 20/25 = 80%.
fn armor_dmg_factor(c: &Client, raw: f32) -> f64 {
    let equipped = equipped_pairs(c);
    let pairs: Vec<(ItemAttrSlotGroup, &Stack)> =
        equipped.iter().map(|(g, s)| (*g, s)).collect();
    // armor_pts already carries the worn pieces' component modifiers; only
    // hand-held items still add on top.
    let hand_pairs: Vec<(ItemAttrSlotGroup, &Stack)> = pairs
        .iter()
        .filter(|(g, _)| {
            matches!(
                g,
                ItemAttrSlotGroup::MainHand | ItemAttrSlotGroup::OffHand
            )
        })
        .copied()
        .collect();
    let armor = f64::from(*c.armor_pts.lock().unwrap_or_else(|e| e.into_inner()))
        + apply_item_attribute_modifiers(0.0, &hand_pairs, ATTR_ARMOR);
    let f = 2.0 + armor_toughness_of(&pairs) / 4.0;
    let v = armor - f64::from(raw) / f;
    let lo = armor * 0.2;
    let g = if v < lo { lo } else { v.min(20.0) };
    1.0 - g / 25.0
}
// vanilla EPF: protection-family enchantments on worn armor add a
// post-armor reduction of min(epf,20)*4%. kind: 0 generic (prot only),
// 1 fall (feather_falling 3x), 2 explosion (blast 2x), 3 projectile
// (proj 2x), 4 fire (fire_prot 2x)
fn enchant_epf(c: &Client, kind: u8) -> i64 {
    let inv = c.inv.lock().unwrap_or_else(|e| e.into_inner());
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
    let arm = f64::from(raw) * armor_dmg_factor(c, raw);
    (arm * (1.0 - 0.04 * enchant_epf(c, kind) as f64)) as f32
}
// call AFTER the hp write: if hp hit 0 and a totem is held (main or
// offhand), consume it — vanilla sets hp to 1, fires entity_status 35,
// plays the totem animation+sound, grants absorption/regen/fire_resist.
// Damage taken below the world bottom is `fellOutOfWorld` (the void caller in
// the main tick only applies it there), a `BYPASSES_INVULNERABILITY` source
// vanilla's `checkTotemDeathProtection` never sees.
fn totem_check(c: &Client) {
    let hp = *c.hp.lock().unwrap_or_else(|e| e.into_inner());
    let kind = {
        let p = c.pos.lock().unwrap_or_else(|e| e.into_inner());
        let below = p.1 < f64::from(c.dim.lock().unwrap_or_else(|e| e.into_inner()).min_y());
        if below {
            TotemDamageKind::Void
        } else {
            TotemDamageKind::Ordinary
        }
    };
    let _ = try_trigger_totem_of_undying(c, hp, kind);
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
    *c.absorb.lock().unwrap_or_else(|e| e.into_inner()) = 8.0;
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
    let mut pairs: Vec<(ItemAttrSlotGroup, &Stack)> = Vec::new();
    for ((slot, mats), grp) in slots.iter().zip([
        ItemAttrSlotGroup::Head,
        ItemAttrSlotGroup::Chest,
        ItemAttrSlotGroup::Legs,
        ItemAttrSlotGroup::Feet,
    ]) {
        if let Some(st @ (item, _, comps)) = inv.get(slot) {
            // An explicit attribute_modifiers component replaces the piece's
            // vanilla default, so the table points only count without one.
            if comp_get(comps, COMP_ATTRIBUTE_MODIFIERS).is_none() {
                for &(n, p) in *mats {
                    if name_to_item(n) == Some(*item) {
                        pts += p
                    }
                }
            }
            pairs.push((grp, st));
        }
    }
    // The table points play the role of the item's vanilla default modifier,
    // so they join the base side of calculateValue's three-stage fold.
    apply_item_attribute_modifiers(f64::from(pts), &pairs, ATTR_ARMOR).round() as i32
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

/// Vanilla `LivingEntity.invulnerableTime` for mobs is ten game ticks
/// (`hurt`'s `invulnerableTime > 10` gate). The melee/sweep gates in
/// play_protocol.rs read `Mob.hurt_at` as a wall instant (500 ms), so this
/// per-tick pass keeps the wall stamp in agreement with the tick stamp:
/// while `TICK_IDX - last_hurt_tick < 10` the wall stamp is pushed into the
/// future (its `elapsed()` saturates to 0, so the gate stays shut even under
/// `/tick freeze` or lag), and once the window has closed the stamp is backed
/// out to exactly the hurt instant so downstream readers — `recently_hurt`'s
/// two-second XP window and despawn observation — see the same wall time they
/// always did. A fresh wall stamp with no tick stamp (paths that still write
/// only `hurt_at`, e.g. the melee gate) is adopted into the tick window on
/// the next tick.
const MOB_HURT_WINDOW_GT: u64 = 10;
fn mob_hurt_window_tick() {
    let now_tick = TICK_IDX.load(Ordering::Relaxed);
    let now = Instant::now();
    let reg = breg();
    for mob in mobs().lock().unwrap_or_else(|e| e.into_inner()).iter() {
        // `LayeredCauldronBlock.entityInside`: a burning entity standing in a
        // water or powder-snow cauldron is extinguished; water drops a level
        // (level 1 empties) and powder snow becomes water at the same level.
        if *mob.fire_ticks.lock().unwrap_or_else(|e| e.into_inner()) > 0 {
            if let Some(reg) = reg {
                let (mx, my, mz, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
                let (bx, by, bz) = (mx.floor() as i32, my.floor() as i32, mz.floor() as i32);
                let st = block_at_dim(mob.dim, bx, by, bz);
                if let Some(state) = cauldron_state_of(reg, st) {
                    let quenched = cauldron_extinguish(state);
                    if quenched != state {
                        if let Some(next) = quenched.to_state(reg) {
                            set_block_dim(mob.dim, bx, by, bz, next);
                            note_block_update_dim(mob.dim, bx, by, bz, next);
                        }
                        *mob.fire_ticks.lock().unwrap_or_else(|e| e.into_inner()) = 0;
                    }
                }
            }
        }
        let mut hurt_tick = mob.last_hurt_tick.lock().unwrap_or_else(|e| e.into_inner());
        let mut hurt_at = mob.hurt_at.lock().unwrap_or_else(|e| e.into_inner());
        match *hurt_tick {
            Some(t) if now_tick.saturating_sub(t) < MOB_HURT_WINDOW_GT => {
                *hurt_at = now + Duration::from_secs(3600);
            }
            Some(_) => {
                if hurt_at.elapsed() < Duration::from_millis(500) {
                    *hurt_at = now - Duration::from_millis(500);
                }
            }
            None => {
                if hurt_at.elapsed() < Duration::from_millis(500) {
                    *hurt_tick = Some(now_tick);
                    *hurt_at = now + Duration::from_secs(3600);
                }
            }
        }
    }
}

fn arrow_tick() {
    mob_hurt_window_tick();
    combat2_state_tick();
    let mut dead: Vec<i32> = vec![];
    let mut hits: Vec<(i32, f32, f64, f64)> = vec![]; // (victim eid, dmg, arrow x, z)
    let mut mob_dead: Vec<(i32, i64, f64, f64, f64)> = vec![];
    {
        let mut ar = arrows().lock().unwrap_or_else(|e| e.into_inner());
        let region = region_guard();
        let mut returning = trident_returning().lock().unwrap_or_else(|e| e.into_inner());
        let mut seen_tridents = trident_seen().lock().unwrap_or_else(|e| e.into_inner());
        for a in ar.iter_mut() {
            let eid = a.eid;
            if !region.is_hot(a.x as i32, a.z as i32) {
                continue;
            }
            if a.kind == 1 {
                // Loyalty return flight (`ThrownTrident` after `dealtDamage`):
                // `vel = vel*0.01 + dir(owner_eye)*0.05*loyalty` — no dimension
                // floor gates it, so a trident below `min_y` still comes home.
                if returning.contains(&eid) {
                    let owner_pos = clients()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .iter()
                        .find(|c| c.eid == a.owner)
                        .map(|c| {
                            let p = c.pos.lock().unwrap_or_else(|e| e.into_inner());
                            (p.0, p.1 + 0.9, p.2)
                        });
                    let Some((ox, oy, oz)) = owner_pos else {
                        returning.remove(&eid);
                        dead.push(eid);
                        continue;
                    };
                    let eval = evaluate_trident_special(&TridentSpecialCtx {
                        loyalty: a.loy,
                        trident: [a.x, a.y, a.z],
                        owner: [ox, oy, oz],
                        ..Default::default()
                    });
                    let d = [ox - a.x, oy - a.y, oz - a.z];
                    let n = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
                    if n <= 1.0 {
                        returning.remove(&eid);
                        dead.push(eid);
                        continue;
                    }
                    let imp = eval.loyalty_return.unwrap_or([0.0; 3]);
                    a.dx = a.dx * 0.01 + imp[0];
                    a.dy = a.dy * 0.01 + imp[1];
                    a.dz = a.dz * 0.01 + imp[2];
                    a.x += a.dx;
                    a.y += a.dy;
                    a.z += a.dz;
                    let (ax, ay, az) = (a.x, a.y, a.z);
                    broadcast_seen(eid, |_c| {
                        (0x70, entity_teleport(eid, ax, ay, az, 0.0, 0.0, true))
                    });
                    continue;
                }
                // Riptide resolves the first tick the projectile is observed:
                // vanilla `releaseUsing` never spawns one — the thrower is
                // propelled along the look vector (wet-only) and keeps the item.
                if seen_tridents.insert(eid) {
                    let riptide = enchant_level(&a.ret_comps, 31);
                    if (1..=3).contains(&riptide) {
                        let owner = clients()
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .iter()
                            .find(|c| c.eid == a.owner)
                            .cloned();
                        if let Some(oc) = owner {
                            let (x, y, z, yaw, pitch, on_ground) =
                                *oc.pos.lock().unwrap_or_else(|e| e.into_inner());
                            let (yr, pr) = ((yaw as f64).to_radians(), (pitch as f64).to_radians());
                            let eval = evaluate_trident_special(&TridentSpecialCtx {
                                riptide,
                                wet: trident_thrower_wet(&oc),
                                on_ground,
                                look: [
                                    -yr.sin() * pr.cos(),
                                    -pr.sin(),
                                    yr.cos() * pr.cos(),
                                ],
                                ..Default::default()
                            });
                            if let RiptideOutcome::Launch(launch) = eval.riptide {
                                let pkt = velocity_pkt(
                                    oc.eid,
                                    (launch.velocity[0] * 8000.0) as i16,
                                    (launch.velocity[1] * 8000.0) as i16,
                                    (launch.velocity[2] * 8000.0) as i16,
                                );
                                if let Ok(mut w) = oc.sock.lock() {
                                    let _ = send_packet(&mut w, 0x5a, pkt.clone());
                                }
                                broadcast_seen_ex(oc.eid, oc.eid, |_c| (0x5a, pkt.clone()));
                                riptide_spin()
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .insert(
                                        oc.eid,
                                        TICK_IDX.load(Ordering::Relaxed)
                                            + server_ticks(u64::from(launch.spin_ticks)),
                                    );
                                if launch.ground_boost > 0.0 {
                                    let mut p = oc.pos.lock().unwrap_or_else(|e| e.into_inner());
                                    p.1 += launch.ground_boost;
                                }
                                let snd = sound_pkt_named("entity.generic.explode", x, y, z);
                                broadcast_seen(oc.eid, |_c| (0x68, snd.clone()));
                            }
                            // Refused launches also never leave the hand.
                            trident_restore(&oc, a.ret_item, &a.ret_comps);
                            a.ret_item = 0;
                            dead.push(eid);
                            continue;
                        }
                    }
                }
            }
            if a.stuck {
                a.ttl -= 1;
                if a.ttl <= 0 {
                    dead.push(eid);
                    continue;
                }
                // `intangible_projectile` arrows (pillager shots) are never
                // picked up: vanilla `AbstractArrow.tryPickup` refuses them.
                if comp_get(&a.ret_comps, COMP_INTANGIBLE_PROJECTILE).is_some() {
                    continue;
                }
                // Vanilla: a stuck arrow is picked up on contact (survival only).
                let (ax, ay, az) = (a.x, a.y, a.z);
                let grab = {
                    clients()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .iter()
                        .filter(|c| *c.gm.lock().unwrap_or_else(|e| e.into_inner()) != 1)
                        .find(|c| {
                            let p = c.pos.lock().unwrap_or_else(|e| e.into_inner());
                            let (dx, dy, dz) = (p.0 - ax, (p.1 + 0.9) - ay, p.2 - az);
                            dx * dx + dy * dy + dz * dz < 1.3
                        })
                        .cloned()
                };
                if let Some(c) = grab {
                    let mut inv = c.inv.lock().unwrap_or_else(|e| e.into_inner());
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
                            .unwrap_or_else(|e| e.into_inner())
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
                    if a.loy > 0 {
                        returning.insert(eid);
                    } else {
                        dead.push(eid);
                    }
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
                let ml = mobs().lock().unwrap_or_else(|e| e.into_inner());
                'outer: for m in ml.iter() {
                    if m.eid == a.last_hit || m.dim != DimId::Overworld {
                        continue;
                    }
                    let (mx, my, mz, ..) = *m.pos.lock().unwrap_or_else(|e| e.into_inner());
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
                        *m.hurt_at.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
                        *m.last_hurt_tick.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(TICK_IDX.load(Ordering::Relaxed));
                        let mut h = m.hp.lock().unwrap_or_else(|e| e.into_inner());
                        *h -= a.dmg;
                        let dh = *h;
                        drop(h);
                        if a.fire > 0 && !mob_fire_immune(m.etype) {
                            *m.fire_ticks.lock().unwrap_or_else(|e| e.into_inner()) = a.fire;
                            ignite_mob(m.eid, a.fire.max(0) as u32);
                        }
                        if a.punch > 0 && a.kind == 0 {
                            let vl = (a.dx * a.dx + a.dz * a.dz).sqrt().max(0.01);
                            let kb = 0.3 * a.punch as f64;
                            let mut p = m.pos.lock().unwrap_or_else(|e| e.into_inner());
                            p.0 += a.dx / vl * kb;
                            p.2 += a.dz / vl * kb;
                            drop(p);
                            broadcast_seen(m.eid, |_c| {
                                (
                                    0x70,
                                    entity_teleport(
                                        m.eid,
                                        m.pos.lock().unwrap_or_else(|e| e.into_inner()).0,
                                        m.pos.lock().unwrap_or_else(|e| e.into_inner()).1,
                                        m.pos.lock().unwrap_or_else(|e| e.into_inner()).2,
                                        m.pos.lock().unwrap_or_else(|e| e.into_inner()).3,
                                        m.pos.lock().unwrap_or_else(|e| e.into_inner()).4,
                                        true,
                                    ),
                                )
                            });
                        }
                        broadcast_seen(m.eid, |_c| (0x24, hurt_anim(m.eid)));
                        // channeling: trident hit in thunder calls lightning
                        // (punch slot carries the level for kind==1) — only
                        // with open sky above the hit (vanilla canSeeSky)
                        let channeling_hit = a.kind == 1
                            && a.punch > 0
                            && evaluate_trident_special(&TridentSpecialCtx {
                                channeling: i64::from(a.punch),
                                thundering: THUNDERING.load(Ordering::Relaxed) > 0,
                                open_sky: THUNDERING.load(Ordering::Relaxed) > 0
                                    && can_see_sky(
                                        DimId::Overworld,
                                        mx.floor() as i32,
                                        my.floor() as i32,
                                        mz.floor() as i32,
                                    ),
                                ..Default::default()
                            })
                            .channeling_lightning;
                        if channeling_hit {
                            {
                                let mut h2 = m.hp.lock().unwrap_or_else(|e| e.into_inner());
                                *h2 -= 5.0;
                            }
                            *m.fuse.lock().unwrap_or_else(|e| e.into_inner()) = server_ticks(CREEPER_FUSE_GT) as i32; // ignite ~5s
                            let be = NEXT_EID.fetch_add(1, Ordering::SeqCst);
                            bolts()
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push((be, server_ticks(BOLT_LIFE_GT) as i32));
                            broadcast(-1, |_c| (0x01, spawn_typed_pkt(be, 64, mx, my, mz)));
                            let ts = sound_pkt_named("entity.lightning_bolt.thunder", mx, my, mz);
                            broadcast(-1, |_c| (0x68, ts.clone()));
                        }
                        if dh <= 0.0 {
                            mob_dead.push((m.eid, m.etype, mx, my, mz));
                            let on = clients()
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
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
                        } else if a.kind == 1 && a.loy > 0 {
                            returning.insert(eid);
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
                            let p = c.pos.lock().unwrap_or_else(|e| e.into_inner());
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
                            hits.push((c.eid, a.dmg, ax, az));
                            if a.pierce > 0 {
                                a.pierce -= 1;
                                a.last_hit = c.eid;
                            } else if a.kind == 1 && a.loy > 0 {
                                returning.insert(eid);
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
                        let p = c.pos.lock().unwrap_or_else(|e| e.into_inner());
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
                        hits.push((c.eid, a.dmg, ax, az));
                        if a.kind == 1 && a.loy > 0 {
                            returning.insert(eid);
                        } else {
                            dead.push(eid);
                        }
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
        returning.retain(|eid| ar.iter().any(|a| a.eid == *eid));
        seen_tridents.retain(|eid| ar.iter().any(|a| a.eid == *eid));
        for (ow, lx, ly, lz, loy, ri, rc) in landed {
            let (tx, ty, tz) = if loy > 0 {
                clients()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .find(|c| c.eid == ow)
                    .map(|c| {
                        let p = c.pos.lock().unwrap_or_else(|e| e.into_inner());
                        (p.0, p.1, p.2)
                    })
                    .unwrap_or((lx, ly, lz))
            } else {
                (lx, ly, lz)
            };
            drop_spawn(tx, ty, tz, ri, 1, rc, 0, None, None);
        }
    }
    for (deid, etype, mx, my, mz) in mob_dead {
        kill_mob(deid, etype, mx, my, mz);
    }
    for eid in &dead {
        broadcast_seen_destroy(*eid);
    }
    for (veid, admg, ax, az) in hits {
        let cls: Vec<Arc<Client>> = client_snapshot();
        if let Some(c) = cls.iter().find(|c| c.eid == veid) {
            if *c.gm.lock().unwrap_or_else(|e| e.into_inner()) == 1 {
                continue;
            }
            // the evaluator's raise clock already tracks `blocking` — a lowered
            // shield reports 0 raised ticks, below the warmup gate
            let eval = evaluate_shield_block(&shield_hit_ctx(c, [ax, 0.0, az], admg));
            let blocked_ar = eval.blocked
                && *c.blocking.lock().unwrap_or_else(|e| e.into_inner());
            // a rejected hit must not cost shield durability (vanilla returns before that)
            let Some((ad, _)) = damage_after_invulnerability(c, if blocked_ar { 0.0 } else { admg })
            else {
                continue;
            };
            if blocked_ar {
                damage_shield_attempts(c, eval.durability_attempts);
            }
            let mut h = c.hp.lock().unwrap_or_else(|e| e.into_inner());
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
                h = c.hp.lock().unwrap_or_else(|e| e.into_inner());
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
            if hh <= 0.0 && GR_SHOW_DEATH_MESSAGES.load(Ordering::Relaxed) != 0 {
                let line = system_chat(&format!("{} was shot by Skeleton", vname));
                broadcast(-1, |_c| (0x6c, line.clone()));
            }
        }
    }
}
// ---------- MEGA-SCALE T1: power-aware batched & tick-sliced explosions ----------

/// One tick's block-removal ceiling for a queued mega-blast slice. An ordinary
/// TNT (power 4) destroys a few dozen cells and never reaches it; a modded
/// power-24+ blast drains at this rate per `primed_tick`, so no single tick
/// ever freezes.
const MEGA_BLAST_MAX_PER_TICK: usize = 16_384;
/// Explosions at or below this power (vanilla TNT=4, creeper=3, end crystal=6)
/// apply in the same call with the exact vanilla `Explosion.collectBlocks`
/// march. Anything larger — and any blast whose destroyed set would not fit
/// one slice — queues concentric rings instead.
const MEGA_BLAST_IMMEDIATE_POWER: f64 = 8.0;
/// The scrubbed power ceiling. Larger requests still compute against the
/// clamped volume; a blast box beyond the kernel's own cell bound fails
/// closed with zero removals instead of allocating without bound.
const MEGA_BLAST_MAX_POWER: f64 = 64.0;
/// Pending blast plans retained across ticks. A fuller queue fails the newest
/// blast closed rather than letting continuations pile up without bound.
const MEGA_BLAST_QUEUE_CAP: usize = 64;
/// Stack size coalesced explosion drops are chunked to: at most 1/64 of the
/// naive one-entity-per-block count, with every item conserved.
const MEGA_BLAST_STACK: u32 = 64;
/// Vanilla's never-recorded resistance tier (`bedrock`, `barrier`, end portal
/// frames, command blocks): no ray can afford the cell anyway, but the flag
/// keeps the destroyed set honest regardless of float state.
const BLAST_INDESTRUCTIBLE_RESISTANCE: f32 = 3_600_000.0;
/// Fluid resistance floor for fluid-bearing states — vanilla's
/// `max(blockResistance, fluidResistance)` in `Explosion`.
const BLAST_FLUID_RESISTANCE: f32 = 100.0;

/// Per-state blast resistance, parsed once from the `resistance` column of
/// the same `blocks_1211.json` `breg` loads (the registry keeps `hardness`
/// but not this field, so the explosion kernel reads its own copy). A missing
/// or corrupt asset falls back to `hardness`, mapping the negative
/// unbreakable sentinel to vanilla's 3.6M immovable tier so the fail state
/// stays safe.
fn blast_resistance_of(st: u32) -> f32 {
    static TABLE: OnceLock<Option<Vec<f32>>> = OnceLock::new();
    match TABLE.get_or_init(blast_resistance_table).as_ref() {
        Some(t) => t.get(st as usize).copied().unwrap_or(0.6),
        None => {
            let h = breg().map(|r| r.hardness_of(st)).unwrap_or(0.6);
            if h < 0.0 {
                BLAST_INDESTRUCTIBLE_RESISTANCE
            } else {
                h
            }
        }
    }
}

fn blast_resistance_table() -> Option<Vec<f32>> {
    let paths = crate::block_registry::VersionAssetPaths::resolve();
    let md = crate::json::jparse(&std::fs::read(&paths.blocks).ok()?)?
        .as_arr()?
        .clone();
    let count = md
        .iter()
        .filter_map(|b| crate::json::jnum(b, "maxStateId"))
        .fold(0.0, f64::max) as usize
        + 1;
    let mut table = vec![0.6f32; count];
    for b in &md {
        let resistance = crate::json::jnum(b, "resistance").unwrap_or(0.6) as f32;
        let lo = crate::json::jnum(b, "minStateId")? as usize;
        let hi = (crate::json::jnum(b, "maxStateId")? as usize).min(count - 1);
        for slot in &mut table[lo..=hi] {
            *slot = resistance;
        }
    }
    Some(table)
}

/// One sampled world cell for the blast kernels. `state == 0` is air — the
/// ray pays only the step drain. A fluid-bearing cell resists at
/// `max(block, fluid)` like vanilla's `Explosion` fluid branch; `liquid()`
/// states (water/lava/bubble columns) and the 3.6M-resistance tier attenuate
/// rays but are never recorded destroyed.
#[derive(Clone, Copy)]
struct BlastSample {
    state: u32,
    resistance: f32,
    indestructible: bool,
}

fn blast_cell_at(x: i32, y: i32, z: i32) -> BlastSample {
    let st = block_at(x, y, z);
    if st == 0 {
        return BlastSample {
            state: 0,
            resistance: 0.0,
            indestructible: false,
        };
    }
    let mut resistance = blast_resistance_of(st);
    let mut indestructible = resistance >= BLAST_INDESTRUCTIBLE_RESISTANCE;
    if let Some(reg) = breg() {
        if reg.is_liquid(st) {
            resistance = resistance.max(BLAST_FLUID_RESISTANCE);
            indestructible |= reg.is_liquid_block(st);
        }
    }
    BlastSample {
        state: st,
        resistance,
        indestructible,
    }
}

/// One destroyed block recorded by a blast raycast.
#[derive(Clone, Copy)]
struct BlastRemoval {
    pos: [i32; 3],
}

/// The destroyed-cell set of one explosion, as the active kernel returned it.
enum BlastOut {
    #[cfg(feature = "mod-suite")]
    Offload(mod_offload::explosion_batch::BlastResult),
    #[cfg(not(feature = "mod-suite"))]
    Local(Vec<BlastRemoval>),
}

impl BlastOut {
    fn removal_count(&self) -> usize {
        match self {
            #[cfg(feature = "mod-suite")]
            Self::Offload(r) => r.cells.len(),
            #[cfg(not(feature = "mod-suite"))]
            Self::Local(v) => v.len(),
        }
    }

    fn into_removals(self) -> Vec<BlastRemoval> {
        match self {
            #[cfg(feature = "mod-suite")]
            Self::Offload(r) => r
                .cells
                .iter()
                .map(|c| BlastRemoval { pos: c.pos })
                .collect(),
            #[cfg(not(feature = "mod-suite"))]
            Self::Local(v) => v,
        }
    }
}

/// Compute one explosion's destroyed-cell set: the
/// `mod_offload::explosion_batch` kernel (snapshot cache + the 1,352-ray
/// vanilla march) under `mod-suite`, or the equivalent pure-std kernel
/// otherwise. `None` fails closed — the blast volume exceeded the bounded
/// cache and zero cells are removed.
#[cfg(feature = "mod-suite")]
fn blast_compute(origin: [f64; 3], power: f32, seed: u64) -> Option<BlastOut> {
    let source = |x: i32, y: i32, z: i32| {
        let s = blast_cell_at(x, y, z);
        mod_offload::explosion_batch::BlastCell {
            block_id: s.state,
            drop_item: 0,
            blast_resistance: s.resistance,
            drop_count: 0,
            flags: if s.indestructible {
                mod_offload::explosion_batch::FLAG_INDESTRUCTIBLE
            } else {
                0
            },
        }
    };
    blast_engine()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .blast(&source, origin, power, seed)
        .ok()
        .map(BlastOut::Offload)
}

#[cfg(feature = "mod-suite")]
fn blast_engine() -> &'static Mutex<mod_offload::explosion_batch::ExplosionBatch> {
    static E: OnceLock<Mutex<mod_offload::explosion_batch::ExplosionBatch>> = OnceLock::new();
    E.get_or_init(|| Mutex::new(mod_offload::explosion_batch::ExplosionBatch::new()))
}

#[cfg(not(feature = "mod-suite"))]
fn blast_compute(origin: [f64; 3], power: f32, seed: u64) -> Option<BlastOut> {
    let mut engine = blast_engine().lock().unwrap_or_else(|e| e.into_inner());
    if !(power.is_finite() && power > 0.0) || !engine.prepare(origin, power) {
        return if engine.prepared_empty() {
            Some(BlastOut::Local(Vec::new()))
        } else {
            None
        };
    }
    Some(BlastOut::Local(engine.march(origin, power, seed)))
}

#[cfg(not(feature = "mod-suite"))]
fn blast_engine() -> &'static Mutex<LocalBlastCache> {
    static E: OnceLock<Mutex<LocalBlastCache>> = OnceLock::new();
    E.get_or_init(|| Mutex::new(LocalBlastCache::default()))
}

// The local kernel is the same bounded snapshot + vanilla march the
// mod-offload engine runs: a flat cell array over the blast box, 1,352
// surface rays of the 16x16x16 direction grid, per-ray power
// `power * (0.7 + rand * 0.6)`, `0.3`-block steps paying
// `(resistance + 0.3) * 0.3` per non-air cell plus an unconditional `0.225`
// drain, and a per-cell density grid so duplicates collapse to one removal.
#[cfg(not(feature = "mod-suite"))]
const BLAST_RAY_GRID: i32 = 16;
#[cfg(not(feature = "mod-suite"))]
const BLAST_RAY_STEP: f64 = 0.3;
#[cfg(not(feature = "mod-suite"))]
const BLAST_STEP_DRAIN: f32 = 0.225;
#[cfg(not(feature = "mod-suite"))]
const BLAST_BLOCK_COST: f32 = 0.3;
#[cfg(not(feature = "mod-suite"))]
const BLAST_REACH_FACTOR: f64 = 1.75;
#[cfg(not(feature = "mod-suite"))]
const BLAST_BOUNDS_MARGIN: i64 = 2;
/// The fail-closed bound on the sampled blast volume.
#[cfg(not(feature = "mod-suite"))]
const BLAST_MAX_CACHE_CELLS: usize = 6_000_000;
/// Per-ray step cap; unreachable for finite powers (each step drains
/// `BLAST_STEP_DRAIN`) and only guards against pathological float states.
#[cfg(not(feature = "mod-suite"))]
const BLAST_MAX_RAY_STEPS: u32 = 1 << 20;

#[cfg(not(feature = "mod-suite"))]
fn local_blast_bounds(origin: [f64; 3], power: f32) -> Option<([i64; 3], [i64; 3])> {
    if !origin.iter().all(|v| v.is_finite()) {
        return None;
    }
    let extent =
        (f64::from(power) * BLAST_REACH_FACTOR).ceil() as i64 + BLAST_BOUNDS_MARGIN;
    let mut min = [0i64; 3];
    let mut max = [0i64; 3];
    for axis in 0..3 {
        let center = origin[axis].floor() as i64;
        min[axis] = center.saturating_sub(extent);
        max[axis] = center.saturating_add(extent);
    }
    Some((min, max))
}

/// Deterministic xorshift64* identical to the mod-offload stream, so a given
/// `(world, origin, power, seed)` marches identically in both builds.
#[cfg(not(feature = "mod-suite"))]
struct LocalBlastRng(u64);

#[cfg(not(feature = "mod-suite"))]
impl LocalBlastRng {
    fn new(seed: u64) -> Self {
        let mut state = seed
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(0xD1B5_4A32_D192_ED03);
        if state == 0 {
            state = 0x2545_F491_4F6C_DD1D;
        }
        Self(state)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_f32(&mut self) -> f32 {
        ((self.next_u64() >> 40) as f32) * (1.0 / 16_777_216.0)
    }
}

#[cfg(not(feature = "mod-suite"))]
#[derive(Default)]
struct LocalBlastCache {
    min: [i64; 3],
    dims: [usize; 3],
    cells: Vec<BlastSample>,
    density: Vec<f32>,
}

#[cfg(not(feature = "mod-suite"))]
impl LocalBlastCache {
    /// Rebuild the snapshot; `false` when the volume exceeds
    /// `BLAST_MAX_CACHE_CELLS` — the caller fails closed with zero removals.
    fn prepare(&mut self, origin: [f64; 3], power: f32) -> bool {
        let Some((min, max)) = local_blast_bounds(origin, power) else {
            self.dims = [0; 3];
            self.cells.clear();
            return true;
        };
        let mut dims = [0usize; 3];
        let mut volume = 1usize;
        for axis in 0..3 {
            dims[axis] = (max[axis] - min[axis] + 1).max(0) as usize;
            volume = volume.saturating_mul(dims[axis]);
        }
        if volume > BLAST_MAX_CACHE_CELLS {
            return false;
        }
        self.min = min;
        self.dims = dims;
        self.cells.clear();
        self.cells.reserve(volume);
        for lz in 0..dims[2] {
            for ly in 0..dims[1] {
                for lx in 0..dims[0] {
                    self.cells.push(blast_cell_at(
                        min[0].saturating_add(lx as i64) as i32,
                        min[1].saturating_add(ly as i64) as i32,
                        min[2].saturating_add(lz as i64) as i32,
                    ));
                }
            }
        }
        true
    }

    fn prepared_empty(&self) -> bool {
        self.cells.is_empty()
    }

    #[inline]
    fn index_of(&self, x: i32, y: i32, z: i32) -> Option<usize> {
        let (lx, ly, lz) = (
            x as i64 - self.min[0],
            y as i64 - self.min[1],
            z as i64 - self.min[2],
        );
        if lx < 0
            || ly < 0
            || lz < 0
            || lx >= self.dims[0] as i64
            || ly >= self.dims[1] as i64
            || lz >= self.dims[2] as i64
        {
            return None;
        }
        Some((lz * self.dims[1] as i64 + ly) as usize * self.dims[0] + lx as usize)
    }

    /// March all 1,352 surface rays and return the destroyed cells sorted by
    /// `(x, y, z)` — out-of-cache positions read as air and are never
    /// recorded, matching the bounded blast domain exactly.
    fn march(&mut self, origin: [f64; 3], power: f32, seed: u64) -> Vec<BlastRemoval> {
        self.density.clear();
        self.density.resize(self.cells.len(), 0.0);
        let mut rng = LocalBlastRng::new(seed);
        let grid = BLAST_RAY_GRID as usize;
        for i in 0..grid {
            for j in 0..grid {
                for k in 0..grid {
                    let (iu, ju, ku) = (i as i32, j as i32, k as i32);
                    if iu != 0
                        && iu != BLAST_RAY_GRID - 1
                        && ju != 0
                        && ju != BLAST_RAY_GRID - 1
                        && ku != 0
                        && ku != BLAST_RAY_GRID - 1
                    {
                        continue;
                    }
                    let mut dx = (f64::from(iu) / 15.0) * 2.0 - 1.0;
                    let mut dy = (f64::from(ju) / 15.0) * 2.0 - 1.0;
                    let mut dz = (f64::from(ku) / 15.0) * 2.0 - 1.0;
                    let len = (dx * dx + dy * dy + dz * dz).sqrt();
                    dx /= len;
                    dy /= len;
                    dz /= len;
                    let mut remaining = power * (0.7 + rng.next_f32() * 0.6);
                    let (mut px, mut py, mut pz) = (origin[0], origin[1], origin[2]);
                    let mut steps = 0u32;
                    while remaining > 0.0 && steps < BLAST_MAX_RAY_STEPS {
                        steps += 1;
                        let (bx, by, bz) =
                            (px.floor() as i32, py.floor() as i32, pz.floor() as i32);
                        let idx = self.index_of(bx, by, bz);
                        let cell = idx
                            .map(|at| self.cells[at])
                            .unwrap_or(BlastSample {
                                state: 0,
                                resistance: 0.0,
                                indestructible: false,
                            });
                        if cell.state != 0 {
                            let resistance = if cell.resistance.is_nan() {
                                0.0
                            } else {
                                cell.resistance.max(0.0)
                            };
                            remaining -= (resistance + BLAST_BLOCK_COST) * BLAST_BLOCK_COST;
                        }
                        if remaining > 0.0 && cell.state != 0 && !cell.indestructible {
                            if let Some(at) = idx {
                                let slot = &mut self.density[at];
                                if remaining > *slot {
                                    *slot = remaining;
                                }
                            }
                        }
                        px += dx * BLAST_RAY_STEP;
                        py += dy * BLAST_RAY_STEP;
                        pz += dz * BLAST_RAY_STEP;
                        remaining -= BLAST_STEP_DRAIN;
                    }
                }
            }
        }
        let (sx, sy) = (self.dims[0], self.dims[0] * self.dims[1]);
        let mut out: Vec<BlastRemoval> = self
            .density
            .iter()
            .enumerate()
            .filter(|(_, &d)| d > 0.0)
            .map(|(idx, _)| {
                let lz = idx / sy;
                let rem = idx % sy;
                BlastRemoval {
                    pos: [
                        self.min[0].saturating_add((rem % sx) as i64) as i32,
                        self.min[1].saturating_add((rem / sx) as i64) as i32,
                        self.min[2].saturating_add(lz as i64) as i32,
                    ],
                }
            })
            .collect();
        out.sort_by_key(|r| r.pos);
        out
    }
}

/// Concentric-ring slicing of a big destroyed set — `MegaBlastPlan` under
/// `mod-suite`, the same distance-sorted cursor otherwise. Each
/// `next_slice` emits at most `MEGA_BLAST_MAX_PER_TICK` removals.
struct MegaBlastSlices {
    #[cfg(feature = "mod-suite")]
    plan: mod_offload::explosion_batch::MegaBlastPlan,
    #[cfg(not(feature = "mod-suite"))]
    plan: LocalBlastPlan,
}

#[cfg(not(feature = "mod-suite"))]
struct LocalBlastPlan {
    removals: Vec<(f64, BlastRemoval)>,
    cursor: usize,
    budget: usize,
}

#[cfg(not(feature = "mod-suite"))]
impl LocalBlastPlan {
    fn new(center: [f64; 3], mut removals: Vec<BlastRemoval>, budget: usize) -> Self {
        let mut keyed: Vec<(f64, BlastRemoval)> = removals
            .drain(..)
            .map(|r| {
                let dx = f64::from(r.pos[0]) + 0.5 - center[0];
                let dy = f64::from(r.pos[1]) + 0.5 - center[1];
                let dz = f64::from(r.pos[2]) + 0.5 - center[2];
                (dx * dx + dy * dy + dz * dz, r)
            })
            .collect();
        // Concentric rings: nearest first, deterministic position tiebreak.
        keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.pos.cmp(&b.1.pos)));
        Self {
            removals: keyed,
            cursor: 0,
            budget: budget.max(1),
        }
    }

    fn is_complete(&self) -> bool {
        self.cursor >= self.removals.len()
    }

    fn next_slice(&mut self) -> Option<Vec<BlastRemoval>> {
        if self.is_complete() {
            return None;
        }
        let end = (self.cursor + self.budget).min(self.removals.len());
        let out = self.removals[self.cursor..end]
            .iter()
            .map(|&(_, r)| r)
            .collect();
        self.cursor = end;
        Some(out)
    }
}

/// One queued mega-blast: its plan plus the origin and power the per-slice
/// explosion packets carry.
struct MegaBlastJob {
    center: [f64; 3],
    power: f32,
    slices: MegaBlastSlices,
}

impl MegaBlastJob {
    fn new(center: [f64; 3], power: f32, out: BlastOut) -> Self {
        let slices = match out {
            #[cfg(feature = "mod-suite")]
            BlastOut::Offload(result) => MegaBlastSlices {
                plan: mod_offload::explosion_batch::MegaBlastPlan::new(
                    &result,
                    MEGA_BLAST_MAX_PER_TICK,
                ),
            },
            #[cfg(not(feature = "mod-suite"))]
            BlastOut::Local(removals) => MegaBlastSlices {
                plan: LocalBlastPlan::new(center, removals, MEGA_BLAST_MAX_PER_TICK),
            },
        };
        Self {
            center,
            power,
            slices,
        }
    }

    fn is_complete(&self) -> bool {
        self.slices.plan.is_complete()
    }

    fn next_slice(&mut self) -> Option<Vec<BlastRemoval>> {
        #[cfg(feature = "mod-suite")]
        {
            self.slices.plan.next_slice().map(|s| {
                s.removals
                    .iter()
                    .map(|r| BlastRemoval { pos: r.pos })
                    .collect()
            })
        }
        #[cfg(not(feature = "mod-suite"))]
        {
            self.slices.plan.next_slice()
        }
    }
}

/// Pending mega-blast ring plans, drained one bounded slice per
/// `primed_tick`.
fn mega_blast_queue() -> &'static Mutex<std::collections::VecDeque<MegaBlastJob>> {
    static Q: OnceLock<Mutex<std::collections::VecDeque<MegaBlastJob>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(std::collections::VecDeque::new()))
}

/// Collapse `(item, count)` pairs into <= `MEGA_BLAST_STACK` stacks in
/// ascending item order — `coalesce_item_counts` under `mod-suite`, the
/// same aggregate-and-chunk otherwise — conserving every dropped item.
#[cfg(feature = "mod-suite")]
fn coalesce_drop_counts(counts: &HashMap<i64, u64>) -> Vec<(i64, u32)> {
    mod_offload::explosion_batch::coalesce_item_counts(
        counts
            .iter()
            .map(|(&item, &count)| (item as u32, count.min(u64::from(u32::MAX)) as u32)),
        MEGA_BLAST_STACK,
    )
    .into_iter()
    .map(|s| (i64::from(s.item_id), s.count))
    .collect()
}

#[cfg(not(feature = "mod-suite"))]
fn coalesce_drop_counts(counts: &HashMap<i64, u64>) -> Vec<(i64, u32)> {
    let mut items: Vec<(i64, u64)> = counts.iter().map(|(&i, &c)| (i, c)).collect();
    items.sort_unstable();
    let mut out = Vec::new();
    for (item, mut left) in items {
        while left > 0 {
            let take = left.min(u64::from(MEGA_BLAST_STACK)) as u32;
            out.push((item, take));
            left -= u64::from(take);
        }
    }
    out
}

/// Apply one removal slice to the live world: re-read each cell (a chain
/// detonation or an earlier slice may have beaten us to it), prime live TNT
/// with the vanilla 0.5-1.5s chain fuse, coalesce the surviving drops into
/// `MEGA_BLAST_STACK` chunks, and return the packet offsets relative to
/// `base` (the explosion's floored origin cell).
fn apply_blast_slice(base: [i32; 3], removals: &[BlastRemoval]) -> Vec<(i8, i8, i8)> {
    let mut offs: Vec<(i8, i8, i8)> = Vec::with_capacity(removals.len());
    let mut counts: HashMap<i64, u64> = HashMap::new();
    let mut first_pos: HashMap<i64, [i32; 3]> = HashMap::new();
    let mut primed_new: Vec<[i32; 3]> = Vec::new();
    let tile_drops = GR_DO_TILE_DROPS.load(Ordering::Relaxed) != 0;
    for r in removals {
        let [x, y, z] = r.pos;
        let st = block_at(x, y, z);
        if st == 0 {
            continue;
        }
        // vanilla: TNT destroyed by an explosion primes itself — short fuse
        // (0.5-1.5s), this is what makes chains work
        if (2094..=2095).contains(&st) {
            primed_new.push([x, y, z]);
        } else if tile_drops && w1_explosion_drop_allowed() {
            for item in block_drops(st) {
                if item > 0 {
                    *counts.entry(item).or_insert(0) += 1;
                    first_pos.entry(item).or_insert([x, y, z]);
                }
            }
        }
        set_block(x, y, z, 0);
        offs.push((
            (x - base[0]).clamp(-128, 127) as i8,
            (y - base[1]).clamp(-128, 127) as i8,
            (z - base[2]).clamp(-128, 127) as i8,
        ));
    }
    for [tx, ty, tz] in primed_new {
        let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
        let jitter = ((tx.wrapping_mul(31) ^ tz.wrapping_mul(17) ^ ty) & 0x3ff) as u64;
        primed().lock().unwrap_or_else(|e| e.into_inner()).push(Primed {
            eid,
            x: tx as f64 + 0.5,
            y: ty as f64,
            z: tz as f64 + 0.5,
            left: chain_fuse_left(jitter) as i32,
        });
    }
    // One item entity per full stack, at the first destroyed cell of its
    // item, instead of one entity per block.
    for (item, count) in coalesce_drop_counts(&counts) {
        if let Some(&[px, py, pz]) = first_pos.get(&item) {
            drop_spawn(
                px as f64 + 0.5,
                py as f64 + 0.5,
                pz as f64 + 0.5,
                item,
                count as u8,
                Vec::new(),
                0,
                None,
                None,
            );
        }
    }
    offs
}

/// Apply at most one bounded slice from the oldest queued mega-blast and
/// emit its explosion packet. Runs at the top of every `primed_tick` — and
/// once after each `explode_at` enqueue — so a 100,000-cell modded blast
/// drains across ticks like vanilla entity processing instead of stalling a
/// single tick.
fn mega_blast_tick_drain() {
    let (center, power, removals) = {
        let mut q = mega_blast_queue().lock().unwrap_or_else(|e| e.into_inner());
        let Some(job) = q.front_mut() else {
            return;
        };
        let Some(removals) = job.next_slice() else {
            q.pop_front();
            return;
        };
        let (center, power) = (job.center, job.power);
        if job.is_complete() {
            q.pop_front();
        }
        (center, power, removals)
    };
    if removals.is_empty() {
        return;
    }
    let base = [
        center[0].floor() as i32,
        center[1].floor() as i32,
        center[2].floor() as i32,
    ];
    let offs = apply_blast_slice(base, &removals);
    broadcast(-1, |_c| {
        (
            0x20,
            explosion_pkt_with_power(center[0], center[1], center[2], power, &offs),
        )
    });
}

fn explode_at(ex: f64, ey: f64, ez: f64, power: f64) {
    // Fail closed: non-finite or non-positive input mutates nothing — a
    // NaN/infinite, zero or negative power is a no-op, not a hole in the world.
    if !(ex.is_finite() && ey.is_finite() && ez.is_finite() && power.is_finite() && power > 0.0)
    {
        return;
    }
    let power = power.min(MEGA_BLAST_MAX_POWER);
    let origin = [ex, ey, ez];
    let Some(out) = blast_compute(origin, power as f32, world_rng::next_u64()) else {
        // A blast volume beyond the kernel's bounded cache fails closed:
        // nothing is destroyed and nothing is queued.
        return;
    };
    if power > MEGA_BLAST_IMMEDIATE_POWER || out.removal_count() > MEGA_BLAST_MAX_PER_TICK {
        // Queue the concentric-ring plan, then take this tick's slice — the
        // first ring still lands immediately when the queue was empty.
        let job = MegaBlastJob::new(origin, power as f32, out);
        {
            let mut q = mega_blast_queue().lock().unwrap_or_else(|e| e.into_inner());
            if q.len() < MEGA_BLAST_QUEUE_CAP {
                q.push_back(job);
            }
        }
        mega_blast_tick_drain();
    } else {
        let base = [
            origin[0].floor() as i32,
            origin[1].floor() as i32,
            origin[2].floor() as i32,
        ];
        let offs = apply_blast_slice(base, &out.into_removals());
        broadcast(-1, |_c| (0x20, explosion_pkt(ex, ey, ez, &offs)));
    }
    ac_note_explosion(DimId::Overworld, [ex, ey, ez], power);
    let cs: Vec<Arc<Client>> = client_snapshot();
    for cl in cs {
        let (px, py, pz, ..) = {
            let p = cl.pos.lock().unwrap_or_else(|e| e.into_inner());
            *p
        };
        let dd = ((px - ex).powi(2) + (py - ey).powi(2) + (pz - ez).powi(2)).sqrt();
        if dd < power * 1.5 && *cl.gm.lock().unwrap_or_else(|e| e.into_inner()) != 1 {
            let raw = ((power * 1.5 - dd) * 3.0) as f32;
            let Some((accepted, _)) = damage_after_invulnerability(&cl, raw) else {
                continue;
            };
            let mut h = cl.hp.lock().unwrap_or_else(|e| e.into_inner());
            *h = (*h - hurt_absorb(&cl, dmg_after(&cl, accepted, 2))).max(0.0);
            if *h <= 0.0 {
                drop(h);
                totem_check(&cl);
                h = cl.hp.lock().unwrap_or_else(|e| e.into_inner());
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
    // Continuations first: one bounded slice from the oldest queued
    // mega-blast drains each tick, so a modded mega-TNT spreads across ticks
    // instead of stalling one.
    mega_blast_tick_drain();
    let mut boom: Vec<Primed> = Vec::new();
    {
        let mut pl = primed().lock().unwrap_or_else(|e| e.into_inner());
        let region = region_guard();
        // Single stable partition: fuse counts down only while the chunk is
        // hot, survivors keep their order, and detonating entries are moved
        // out once — a 10,000-strong chain costs one pass instead of an
        // O(N^2) `Vec::remove` shift per blast.
        let mut w = 0;
        for r in 0..pl.len() {
            let mut p = pl[r];
            if !region.is_hot(p.x as i32, p.z as i32) {
                // fuse pauses while cold: no tick, no decrement
                pl[w] = p;
                w += 1;
            } else {
                p.left -= 1;
                if p.left <= 0 {
                    boom.push(p);
                } else {
                    pl[w] = p;
                    w += 1;
                }
            }
        }
        pl.truncate(w);
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
    let mut states = live_mob_states().lock().unwrap_or_else(|e| e.into_inner());
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
    if !matches!(*player.gm.lock().unwrap_or_else(|e| e.into_inner()), 0 | 2) || *player.hp.lock().unwrap_or_else(|e| e.into_inner()) <= 0.0 {
        return false;
    }
    if protection_kind == Some(0) {
        // Vanilla `isDamageSourceBlocked`: raise warmup, 180° attacker arc and
        // the running disable cooldown all gate the block before durability.
        let eval = evaluate_shield_block(&shield_hit_ctx(player, origin, damage));
        if eval.blocked && *player.blocking.lock().unwrap_or_else(|e| e.into_inner()) {
            damage_shield_attempts(player, eval.durability_attempts);
            return false;
        }
    }
    if damage <= 0.0 {
        return false;
    }
    let Some((accepted, _)) = damage_after_invulnerability(player, damage) else {
        return false;
    };
    let damage_taken = protection_kind.map_or(accepted, |kind| dmg_after(player, accepted, kind));
    {
        let mut hp = player.hp.lock().unwrap_or_else(|e| e.into_inner());
        *hp = (*hp - hurt_absorb(player, damage_taken)).max(0.0);
    }
    totem_check(player);
    if protection_kind.is_some() {
        damage_armor(player, damage);
    }
    let health = *player.hp.lock().unwrap_or_else(|e| e.into_inner());
    // Vanilla LivingEntity.knockback scales the horizontal push by
    // (1 - knockback_resistance); the vertical component is unaffected.
    let kb_res = knockback_resist_of(player);
    let velocity = knockback_y.map(|up| {
        let (x, _, z, ..) = *player.pos.lock().unwrap_or_else(|e| e.into_inner());
        let [dx, _, dz] = mob_behavior_unit([x - origin[0], 0.0, z - origin[2]]);
        velocity_pkt(
            player.eid,
            (dx * 3200.0 * (1.0 - kb_res)) as i16,
            (up * 8000.0) as i16,
            (dz * 3200.0 * (1.0 - kb_res)) as i16,
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
    // Vanilla `blockUsingShield`: a blocking hit from an axe-wielding mob
    // (Vindicator / Piglin Brute — `canDisableShield`) arms the 100-tick
    // shield disable even though the blocked hit itself still costs durability.
    if mob_can_disable_shield(mob.etype) {
        let mut ctx = shield_hit_ctx(player, pos, diff_mob_dmg(amount));
        ctx.attacker_can_disable = true;
        let eval = evaluate_shield_block(&ctx);
        if eval.blocked && *player.blocking.lock().unwrap_or_else(|e| e.into_inner()) {
            damage_shield_attempts(player, eval.durability_attempts);
            if eval.disable_ticks.is_some() {
                apply_shield_disable(player);
            }
            return;
        }
    }
    if !live_mob_damage_player(player, pos, diff_mob_dmg(amount), Some(0), Some(up)) {
        return;
    }
    let thorns = {
        let inv = player.inv.lock().unwrap_or_else(|e| e.into_inner());
        (5i16..=8)
            .filter_map(|slot| inv.get(&slot))
            .map(|stack| enchant_level(&stack.2, 38))
            .sum::<i64>()
    };
    if thorns > 0 && world_rng::next_u64() % 100 < (thorns * 15) as u64 {
        *mob.hp.lock().unwrap_or_else(|e| e.into_inner()) -= (1 + world_rng::next_u64() % 4) as f32;
        *mob.hurt_at.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        *mob.last_hurt_tick.lock().unwrap_or_else(|e| e.into_inner()) = Some(TICK_IDX.load(Ordering::Relaxed));
        broadcast_seen(mob.eid, |_| (0x24, hurt_anim(mob.eid)));
    }
    if *player.hp.lock().unwrap_or_else(|e| e.into_inner()) <= 0.0
        && GR_SHOW_DEATH_MESSAGES.load(Ordering::Relaxed) != 0
    {
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
    let targets: Vec<(i32, (f64, f64, f64))> = players
        .iter()
        .filter(|player| player.dim == dim)
        .map(|player| (player.client.eid, player.pos))
        .collect();
    ac_note_explosion_for(pos, f64::from(power), &targets);
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
    if !matches!(*client.gm.lock().unwrap_or_else(|e| e.into_inner()), 0 | 2) || *client.hp.lock().unwrap_or_else(|e| e.into_inner()) <= 0.0 {
        return;
    }
    let ticks = server_ticks(u64::from(ticks)) as i32;
    let mut effects = client.effs.lock().unwrap_or_else(|e| e.into_inner());
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
    sitting_cats().lock().unwrap_or_else(|e| e.into_inner()).remove(&mob.eid);
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
            if client.player_attack_damage_id.is_none() || *client.dim.lock().unwrap_or_else(|e| e.into_inner()) != dim {
                return false;
            }
            client.eid == victim || client.seen.lock().unwrap_or_else(|e| e.into_inner()).contains(&victim)
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
    // Once per client per tick, before the health lock is taken downstream:
    // keep the equipment attribute view attr_of reports current.
    refresh_equip_attrs(c);
    let (last_x, _, last_z) = std::mem::replace(&mut *c.lastp.lock().unwrap_or_else(|e| e.into_inner()), (x, y, z));
    let distance = (x - last_x).hypot(z - last_z);
    let movement = if distance >= 10.0 {
        0.0
    } else if *c.sprinting.lock().unwrap_or_else(|e| e.into_inner()) {
        distance as f32 * 0.1
    } else if *c.sneaking.lock().unwrap_or_else(|e| e.into_inner()) {
        distance as f32 * 0.005
    } else {
        0.0
    };
    let hunger = c
        .effs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
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
    let mut sessions = player_mechanism_sessions().lock().unwrap_or_else(|e| e.into_inner());
    let Some(session) = sessions.get_mut(&c.eid) else {
        return false;
    };
    let (mut food, mut sat, mut acc) = (
        c.food.lock().unwrap_or_else(|e| e.into_inner()),
        c.sat.lock().unwrap_or_else(|e| e.into_inner()),
        c.walk_acc.lock().unwrap_or_else(|e| e.into_inner()),
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
    let effects = std::mem::take(&mut *c.effs.lock().unwrap_or_else(|e| e.into_inner()));
    for effect in effects {
        let packet = remove_effect_pkt(c.eid, effect.id);
        if let Ok(mut sock) = c.sock.lock() {
            let _ = send_packet(&mut sock, 0x43, packet.clone());
        }
        broadcast_seen(c.eid, |_| (0x43, packet.clone()));
    }
    *c.absorb.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
    *c.air.lock().unwrap_or_else(|e| e.into_inner()) = server_ticks(AIR_SUPPLY_GT) as i32;
    *c.invulnerable.lock().unwrap_or_else(|e| e.into_inner()) = 0;
    *c.last_hurt.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
    *c.blocking.lock().unwrap_or_else(|e| e.into_inner()) = false;
    *c.sprinting.lock().unwrap_or_else(|e| e.into_inner()) = false;
    *c.sneaking.lock().unwrap_or_else(|e| e.into_inner()) = false;
    *c.fall_fly.lock().unwrap_or_else(|e| e.into_inner()) = false;
    let mut sessions = player_mechanism_sessions().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(session) = sessions.get_mut(&c.eid) {
        session.food_timer = 0;
    }
    attack_tickers().lock().unwrap_or_else(|e| e.into_inner()).remove(&c.eid);
    equip_attrs().lock().unwrap_or_else(|e| e.into_inner()).remove(&c.name);
    let data = FoodData::default();
    *c.food.lock().unwrap_or_else(|e| e.into_inner()) = data.food_level;
    *c.sat.lock().unwrap_or_else(|e| e.into_inner()) = data.saturation;
    *c.walk_acc.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
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
    let mut total = c.xp.lock().unwrap_or_else(|e| e.into_inner());
    let previous_total = std::mem::take(&mut *total);
    let mut sessions = player_mechanism_sessions().lock().unwrap_or_else(|e| e.into_inner());
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
    let mut total = c.xp.lock().unwrap_or_else(|e| e.into_inner());
    let mut sessions = player_mechanism_sessions().lock().unwrap_or_else(|e| e.into_inner());
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
    health_pkt(hp, *c.food.lock().unwrap_or_else(|e| e.into_inner()), *c.sat.lock().unwrap_or_else(|e| e.into_inner()))
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
    let mut invulnerable = c.invulnerable.lock().unwrap_or_else(|e| e.into_inner());
    let mut last_hurt = c.last_hurt.lock().unwrap_or_else(|e| e.into_inner());
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
    let mut a = c.absorb.lock().unwrap_or_else(|e| e.into_inner());
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
/// `attr_key_id`'s inverse for NBT export (`type` names are namespaced).
fn attr_key_name(id: i64) -> Option<&'static str> {
    Some(match id {
        0 => "minecraft:generic.armor",
        1 => "minecraft:generic.armor_toughness",
        2 => "minecraft:generic.attack_damage",
        3 => "minecraft:generic.attack_knockback",
        4 => "minecraft:generic.attack_speed",
        5 => "minecraft:player.block_break_speed",
        6 => "minecraft:player.block_interaction_range",
        7 => "minecraft:player.entity_interaction_range",
        8 => "minecraft:generic.fall_damage_multiplier",
        9 => "minecraft:generic.flying_speed",
        10 => "minecraft:generic.follow_range",
        11 => "minecraft:generic.gravity",
        12 => "minecraft:generic.jump_strength",
        13 => "minecraft:generic.knockback_resistance",
        14 => "minecraft:generic.luck",
        15 => "minecraft:generic.max_absorption",
        16 => "minecraft:generic.max_health",
        17 => "minecraft:generic.movement_speed",
        18 => "minecraft:generic.safe_fall_distance",
        19 => "minecraft:generic.scale",
        20 => "minecraft:zombie.spawn_reinforcements",
        21 => "minecraft:generic.step_height",
        _ => return None,
    })
}
fn attr_of(key: &str, name: &str) -> f64 {
    let base = attrs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .and_then(|m| m.get(name))
        .copied()
        .unwrap_or_else(|| attr_default(name));
    // Vanilla attribute instances resolve equipment modifiers on top of the
    // base. The equipment side is cached (computed without taking locks here:
    // some callers hold the health lock, which orders after inventory).
    if let (Some(id), Some(pname)) = (attr_key_id(name), key.strip_prefix("p:")) {
        if let Some((add, mb, mt)) = equip_attrs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(pname)
            .and_then(|m| m.get(&(id as i32)))
            .copied()
        {
            return (base + add) * (1.0 + mb) * mt;
        }
    }
    base
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
        .unwrap_or_else(|e| e.into_inner())
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

#[cfg(test)]
mod p1_combat_tests {
    use super::*;
    use crate::effect_tests::client_pair;

    /// Restores the shared tick clock and clears the per-player combat maps the
    /// fixture eid/name can collide on across tests.
    struct CombatGuard {
        time: u64,
    }
    impl CombatGuard {
        fn new() -> Self {
            Self {
                time: TICK_IDX.swap(0, Ordering::Relaxed),
            }
        }
    }
    impl std::ops::Drop for CombatGuard {
        fn drop(&mut self) {
            TICK_IDX.store(self.time, Ordering::Relaxed);
            attack_tickers().lock().unwrap_or_else(|e| e.into_inner()).remove(&4242);
            equip_attrs().lock().unwrap_or_else(|e| e.into_inner()).remove("totem-test");
            mobs().lock().unwrap_or_else(|e| e.into_inner()).retain(|m| m.eid != 4242);
        }
    }

    fn attr_stack(mods: &[ItemAttributeModifier]) -> Stack {
        let mut comps = Vec::new();
        comp_set(
            &mut comps,
            COMP_ATTRIBUTE_MODIFIERS,
            attr_modifiers_data(&AttributeModifiersComponent {
                modifiers: mods.to_vec(),
                show_in_tooltip: true,
            }),
        );
        (name_to_item("diamond_sword").unwrap(), 1, comps)
    }
    fn attr_mod(attribute_id: i32, amount: f64, op: ItemAttrOp, slot: ItemAttrSlotGroup) -> ItemAttributeModifier {
        ItemAttributeModifier {
            attribute_id,
            modifier_id: "mc-spine:test".to_string(),
            amount,
            operation: op,
            slot,
        }
    }

    fn test_mob(eid: i32) -> Arc<Mob> {
        Arc::new(Mob {
            dim: DimId::Overworld,
            eid,
            uuid: [eid as u8; 16],
            etype: 124,
            pos: Mutex::new((0.0, 64.0, 0.0, 0.0, 0.0)),
            hp: Mutex::new(20.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            last_hurt_tick: Mutex::new(None),
            fuse: Mutex::new(0),
            fire_ticks: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
            vel: Mutex::new([0.0; 3]),
        })
    }

    #[test]
    fn p1_combat_attack_cooldown_counts_game_ticks() {
        let _g = CombatGuard::new();
        let (client, _rx) = client_pair();
        let client = Arc::new(client);
        let sword = name_to_item("diamond_sword").unwrap();
        client.inv.lock().unwrap_or_else(|e| e.into_inner()).insert(36, (sword, 1, Vec::new()));
        TICK_IDX.store(100, Ordering::Relaxed);
        // Never attacked: full charge (vanilla attackStrengthTicker starts ready).
        assert_eq!(melee_charge(&client, sword), 1.0);
        // Diamond sword attack speed 1.6 -> delay 12.5 game ticks; charge
        // (elapsed + 0.5) / delay.
        TICK_IDX.store(103, Ordering::Relaxed);
        let c = f64::from(melee_charge(&client, sword));
        assert!((c - 3.5 / 12.5).abs() < 1e-6, "{c}");
        TICK_IDX.store(112, Ordering::Relaxed);
        let c = f64::from(melee_charge(&client, sword));
        assert!((c - 9.5 / 12.5).abs() < 1e-6, "{c}");
        TICK_IDX.store(300, Ordering::Relaxed);
        assert_eq!(melee_charge(&client, sword), 1.0);
    }

    #[test]
    fn p1_combat_attack_cooldown_ignores_wall_clock() {
        let _g = CombatGuard::new();
        let (client, _rx) = client_pair();
        let client = Arc::new(client);
        let sword = name_to_item("diamond_sword").unwrap();
        client.inv.lock().unwrap_or_else(|e| e.into_inner()).insert(36, (sword, 1, Vec::new()));
        TICK_IDX.store(100, Ordering::Relaxed);
        assert_eq!(melee_charge(&client, sword), 1.0);
        // Wall time passes but TICK_IDX stands still (as under /tick freeze):
        // the charge must not recharge.
        std::thread::sleep(Duration::from_millis(650));
        let c = f64::from(melee_charge(&client, sword));
        assert!((c - 0.5 / 12.5).abs() < 1e-6, "frozen charge must be ~0.04, got {c}");
    }

    #[test]
    fn p1_combat_held_item_switch_resets_charge() {
        let _g = CombatGuard::new();
        let (client, _rx) = client_pair();
        let client = Arc::new(client);
        let sword = name_to_item("diamond_sword").unwrap();
        client.inv.lock().unwrap_or_else(|e| e.into_inner()).insert(36, (sword, 1, Vec::new()));
        client.inv.lock().unwrap_or_else(|e| e.into_inner()).insert(37, (sword, 1, Vec::new()));
        TICK_IDX.store(100, Ordering::Relaxed);
        assert_eq!(melee_charge(&client, sword), 1.0);
        TICK_IDX.store(112, Ordering::Relaxed);
        // 12 ticks of charge (0.76) on the old item, then vanilla
        // resetAttackStrengthTicker on the switch: the new item starts at 0.
        *client.held.lock().unwrap_or_else(|e| e.into_inner()) = 1;
        let c = f64::from(melee_charge(&client, sword));
        assert!((c - 0.5 / 12.5).abs() < 1e-6, "{c}");
    }

    #[test]
    fn p1_combat_mob_hurt_window_runs_on_ticks() {
        let _g = CombatGuard::new();
        let mob = test_mob(4242);
        mobs().lock().unwrap_or_else(|e| e.into_inner()).push(mob.clone());
        TICK_IDX.store(100, Ordering::Relaxed);
        // The melee gate writes only the wall stamp; the tick pass adopts it.
        *mob.hurt_at.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        mob_hurt_window_tick();
        assert_eq!(*mob.last_hurt_tick.lock().unwrap_or_else(|e| e.into_inner()), Some(100));
        assert!(
            mob.hurt_at.lock().unwrap_or_else(|e| e.into_inner()).elapsed() < Duration::from_millis(500),
            "window must be shut ten ticks"
        );
        // Frozen ticks: even after the wall window would have lapsed, the gate
        // stays shut.
        std::thread::sleep(Duration::from_millis(600));
        assert!(mob.hurt_at.lock().unwrap_or_else(|e| e.into_inner()).elapsed() < Duration::from_millis(500));
        // Ten game ticks later the window ends and the wall stamp is backed out
        // to the hurt instant (elapsed >= the 500ms gate).
        TICK_IDX.store(110, Ordering::Relaxed);
        mob_hurt_window_tick();
        assert!(mob.hurt_at.lock().unwrap_or_else(|e| e.into_inner()).elapsed() >= Duration::from_millis(500));
    }

    #[test]
    fn p1_combat_mob_hurt_window_stamped_paths_hold() {
        let _g = CombatGuard::new();
        let mob = test_mob(4242);
        mobs().lock().unwrap_or_else(|e| e.into_inner()).push(mob.clone());
        TICK_IDX.store(100, Ordering::Relaxed);
        *mob.last_hurt_tick.lock().unwrap_or_else(|e| e.into_inner()) = Some(100);
        *mob.hurt_at.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        mob_hurt_window_tick();
        TICK_IDX.store(109, Ordering::Relaxed);
        mob_hurt_window_tick();
        assert!(
            mob.hurt_at.lock().unwrap_or_else(|e| e.into_inner()).elapsed() < Duration::from_millis(500),
            "still inside the 10-tick window at tick 109"
        );
        TICK_IDX.store(110, Ordering::Relaxed);
        mob_hurt_window_tick();
        assert!(mob.hurt_at.lock().unwrap_or_else(|e| e.into_inner()).elapsed() >= Duration::from_millis(500));
    }

    #[test]
    fn p1_combat_apply_item_attribute_modifiers_vanilla_order() {
        let sword = attr_stack(&[
            attr_mod(ATTR_ATTACK_DAMAGE, 2.0, ItemAttrOp::AddValue, ItemAttrSlotGroup::MainHand),
            attr_mod(ATTR_ATTACK_DAMAGE, 0.5, ItemAttrOp::AddMultipliedBase, ItemAttrSlotGroup::Any),
            attr_mod(ATTR_ATTACK_DAMAGE, 0.5, ItemAttrOp::AddMultipliedTotal, ItemAttrSlotGroup::Hand),
            // Declared for feet: must not count while the sword is in hand.
            attr_mod(ATTR_ATTACK_DAMAGE, 100.0, ItemAttrOp::AddValue, ItemAttrSlotGroup::Feet),
        ]);
        let pairs: Vec<(ItemAttrSlotGroup, &Stack)> =
            vec![(ItemAttrSlotGroup::MainHand, &sword)];
        // (1 + 2) * (1 + 0.5) * (1 + 0.5) = 6.75
        assert_eq!(
            apply_item_attribute_modifiers(1.0, &pairs, ATTR_ATTACK_DAMAGE),
            6.75
        );
        // The same sword worn on the head sees only the Any modifier.
        let head_pairs: Vec<(ItemAttrSlotGroup, &Stack)> =
            vec![(ItemAttrSlotGroup::Head, &sword)];
        assert_eq!(
            apply_item_attribute_modifiers(1.0, &head_pairs, ATTR_ATTACK_DAMAGE),
            1.5
        );
        // On the feet slot the Feet and Any modifiers both count — the
        // MainHand and Hand ones do not: (1 + 100) * (1 + 0.5) = 151.5.
        let feet_pairs: Vec<(ItemAttrSlotGroup, &Stack)> =
            vec![(ItemAttrSlotGroup::Feet, &sword)];
        assert_eq!(
            apply_item_attribute_modifiers(1.0, &feet_pairs, ATTR_ATTACK_DAMAGE),
            151.5
        );
        // No component: base passes through untouched.
        let plain: Stack = (name_to_item("diamond_sword").unwrap(), 1, Vec::new());
        let pairs = vec![(ItemAttrSlotGroup::MainHand, &plain)];
        assert_eq!(
            apply_item_attribute_modifiers(4.0, &pairs, ATTR_ATTACK_SPEED),
            4.0
        );
    }

    #[test]
    fn p1_combat_armor_group_and_piece_defaults() {
        // A helmet whose modifier is declared for the armor group counts on any
        // armor slot; a piece with an explicit component replaces its vanilla
        // default points entirely.
        let helm = name_to_item("iron_helmet").unwrap();
        let mut inv = HashMap::new();
        inv.insert(5i16, (helm, 1, Vec::new()));
        assert_eq!(armor_pts_of(&inv), 2);
        let boosted = (
            helm,
            1,
            vec![(
                COMP_ATTRIBUTE_MODIFIERS,
                attr_modifiers_data(&AttributeModifiersComponent {
                    modifiers: vec![
                        attr_mod(ATTR_ARMOR, 4.0, ItemAttrOp::AddValue, ItemAttrSlotGroup::Armor),
                        attr_mod(ATTR_ARMOR_TOUGHNESS, 1.0, ItemAttrOp::AddValue, ItemAttrSlotGroup::Head),
                    ],
                    show_in_tooltip: true,
                }),
            )],
        );
        inv.insert(5, boosted);
        // Component replaces the iron helmet's default 2: total is 4.
        assert_eq!(armor_pts_of(&inv), 4);
        let stacks = inv.get(&5).cloned().unwrap();
        let pairs: Vec<(ItemAttrSlotGroup, &Stack)> =
            vec![(ItemAttrSlotGroup::Head, &stacks)];
        assert_eq!(armor_toughness_of(&pairs), 1.0);
    }

    #[test]
    fn p1_combat_live_melee_uses_attribute_components() {
        let _g = CombatGuard::new();
        let (client, _rx) = client_pair();
        let client = Arc::new(client);
        let sword = name_to_item("diamond_sword").unwrap();
        // +3 attack damage and a slower attack speed (1.2 < 1.6 table speed).
        client.inv.lock().unwrap_or_else(|e| e.into_inner()).insert(
            36,
            attr_stack(&[
                attr_mod(ATTR_ATTACK_DAMAGE, 3.0, ItemAttrOp::AddValue, ItemAttrSlotGroup::MainHand),
                attr_mod(ATTR_ATTACK_SPEED, -2.8, ItemAttrOp::AddValue, ItemAttrSlotGroup::MainHand),
            ]),
        );
        TICK_IDX.store(100, Ordering::Relaxed);
        // First charge seeds the equipment cache the attr path reads.
        assert_eq!(melee_charge(&client, sword), 1.0);
        // Live attack_damage attribute: base 1 + 3.
        assert_eq!(
            attr_of(&format!("p:{}", client.name), "generic.attack_damage"),
            4.0
        );
        // Live attack speed: component replaces the vanilla default, so
        // 4.0 - 2.8 = 1.2 -> delay 16.667 ticks; 3 elapsed -> ~0.21 charge
        // (a plain diamond sword would be 0.28).
        TICK_IDX.store(103, Ordering::Relaxed);
        let c = f64::from(melee_charge(&client, sword));
        assert!((c - 3.5 / (20.0 / 1.2)).abs() < 1e-6, "{c}");
    }

    #[test]
    fn p1_combat_knockback_resistance_from_component() {
        let _g = CombatGuard::new();
        let (client, _rx) = client_pair();
        let client = Arc::new(client);
        assert_eq!(knockback_resist_of(&client), 0.0);
        client.inv.lock().unwrap_or_else(|e| e.into_inner()).insert(
            45,
            attr_stack(&[attr_mod(
                ATTR_KNOCKBACK_RESISTANCE,
                0.6,
                ItemAttrOp::AddValue,
                ItemAttrSlotGroup::OffHand,
            )]),
        );
        assert_eq!(knockback_resist_of(&client), 0.6);
    }

    #[test]
    fn p1_combat_max_health_component_via_attr_cache() {
        let _g = CombatGuard::new();
        let (client, _rx) = client_pair();
        let client = Arc::new(client);
        assert_eq!(max_hp_of(&client.name), 20.0);
        client.inv.lock().unwrap_or_else(|e| e.into_inner()).insert(
            5,
            (
                name_to_item("iron_helmet").unwrap(),
                1,
                vec![(
                    COMP_ATTRIBUTE_MODIFIERS,
                    attr_modifiers_data(&AttributeModifiersComponent {
                        modifiers: vec![attr_mod(
                            ATTR_MAX_HEALTH,
                            4.0,
                            ItemAttrOp::AddValue,
                            ItemAttrSlotGroup::Head,
                        )],
                        show_in_tooltip: true,
                    }),
                )],
            ),
        );
        refresh_equip_attrs(&client);
        assert_eq!(max_hp_of(&client.name), 24.0);
    }

    #[test]
    fn bughunt_root_roll_loot_wide_rolls_and_empty_entries() {
        // A datapack pool may declare up to 64 rolls: the per-roll hash windows
        // pass bit 63 at roll 8 and `r * 8` overflows a u8 at roll 32, so the
        // extraction must wrap the shift instead of panicking. A pool whose
        // entries were all filtered out yields no drops rather than dividing
        // by zero, matching the seeded roller.
        let pool = |rolls: u8, entries: Vec<LootEntry>| LootPool {
            rolls,
            chance: 1.0,
            entries,
            funcs: Vec::new(),
        };
        let drops = roll_loot(&[pool(
            64,
            vec![LootEntry {
                item: 1,
                min: 1,
                max: 1,
                funcs: Vec::new(),
            }],
        )]);
        assert_eq!(drops.len(), 64);
        assert!(drops.iter().all(|stack| stack.0 == 1 && stack.1 == 1));
        assert!(roll_loot(&[pool(4, Vec::new())]).is_empty());
    }
}

#[cfg(test)]
mod p5a_parity_tests {
    use super::*;

    /// Restores the fire map and tick clock around a test so eids and tick
    /// counts cannot collide between cases.
    struct FireGuard {
        time: u64,
    }
    impl FireGuard {
        fn new() -> Self {
            Self {
                time: TICK_IDX.swap(0, Ordering::Relaxed),
            }
        }
    }
    impl std::ops::Drop for FireGuard {
        fn drop(&mut self) {
            TICK_IDX.store(self.time, Ordering::Relaxed);
            mob_fire_ticks().lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
    }

    fn mob(eid: i32, etype: i64) -> Mob {
        Mob {
            dim: DimId::Overworld,
            eid,
            uuid: [eid as u8; 16],
            etype,
            pos: Mutex::new((0.0, 64.0, 0.0, 0.0, 0.0)),
            hp: Mutex::new(20.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            last_hurt_tick: Mutex::new(None),
            fuse: Mutex::new(0),
            fire_ticks: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
            vel: Mutex::new([0.0; 3]),
        }
    }

    // #78 — Fire Aspect's burn lands on the dedicated map store, never on the
    // creeper's fuse countdown or the sunlight-burn field.
    #[test]
    fn p5a_parity_fire_aspect_never_touches_creeper_fuse() {
        let _g = FireGuard::new();
        let creeper = mob(4243, name_to_etype("creeper").unwrap());
        *creeper.fuse.lock().unwrap_or_else(|e| e.into_inner()) = 7;
        ignite_mob_fire_aspect(creeper.eid, 1);
        assert_eq!(
            mob_fire_ticks_of(creeper.eid),
            fire_aspect_duration_ticks(1) as u32
        );
        assert_eq!(
            mob_fire_ticks_of(creeper.eid),
            server_ticks(80) as u32,
            "Fire Aspect I is 4s"
        );
        ignite_mob_fire_aspect(creeper.eid, 2);
        assert_eq!(
            mob_fire_ticks_of(creeper.eid),
            server_ticks(160) as u32,
            "Fire Aspect II is 8s"
        );
        assert_eq!(*creeper.fuse.lock().unwrap_or_else(|e| e.into_inner()), 7, "fuse untouched");
        assert_eq!(*creeper.fire_ticks.lock().unwrap_or_else(|e| e.into_inner()), 0, "sunlight field untouched");
        ignite_mob_fire_aspect(creeper.eid, 0);
        assert_eq!(
            mob_fire_ticks_of(creeper.eid),
            server_ticks(160) as u32,
            "no Fire Aspect keeps the running burn"
        );
    }

    // #78 — burn damage pays 1hp every 20 game ticks, skips the fire-immune
    // family, decrements to zero, and water/rain extinguishes.
    #[test]
    fn p5a_parity_mob_fire_ticks_decrement_damage_and_extinguish() {
        let _g = FireGuard::new();
        let eid = 4244;
        ignite_mob(eid, server_ticks(80) as u32);
        let burn_interval = server_ticks(MOB_BURN_TICK_GT);
        for tick in 1..burn_interval {
            assert!(!tick_mob_fire(eid, tick, false, false), "tick {tick}");
        }
        assert_eq!(mob_fire_ticks_of(eid), server_ticks(80) as u32 - 19);
        assert!(tick_mob_fire(eid, burn_interval, false, false));
        assert_eq!(mob_fire_ticks_of(eid), server_ticks(80) as u32 - 20);

        // Fire-immune mobs (blaze, ghast, magma_cube, strider, wither,
        // wither_skeleton, zombified_piglin, warden, ender_dragon...) still
        // burn down on the clock but take no damage.
        ignite_mob(eid, server_ticks(80) as u32);
        assert!(!tick_mob_fire(eid, burn_interval * 2, true, false));
        assert_eq!(mob_fire_ticks_of(eid), server_ticks(80) as u32 - 1);

        // Water (or rain) clears the whole burn before the next decrement.
        assert!(mob_fire_ticks_of(eid) > 0);
        assert!(!tick_mob_fire(eid, burn_interval * 3, false, true));
        assert_eq!(mob_fire_ticks_of(eid), 0);
        extinguish_mob_fire(eid);

        // The burn decrements cleanly to zero and the entry is dropped.
        ignite_mob(eid, 1);
        assert!(!tick_mob_fire(eid, burn_interval * 4 + 1, false, false));
        assert_eq!(mob_fire_ticks_of(eid), 0);
        assert!(mob_fire_ticks().lock().unwrap_or_else(|e| e.into_inner()).get(&eid).is_none());
    }

    // #78 — the immune list keeps covering the vanilla fireImmune family.
    #[test]
    fn p5a_parity_fire_immune_family_matches_vanilla() {
        for name in [
            "blaze", "ghast", "magma_cube", "strider", "wither", "wither_skeleton",
            "zombified_piglin", "warden", "ender_dragon", "shulker", "vex", "zoglin",
        ] {
            assert!(
                mob_fire_immune(name_to_etype(name).unwrap()),
                "{name} must be fire immune"
            );
        }
        for name in ["creeper", "zombie", "skeleton", "cow", "spider"] {
            assert!(
                !mob_fire_immune(name_to_etype(name).unwrap()),
                "{name} burns"
            );
        }
    }

    // #79 — every gate of the vanilla crit predicate, and a clean falling blow.
    #[test]
    fn p5a_parity_vanilla_critical_hit_all_gates() {
        let clean = CritContext {
            attack_charge: 1.0,
            fall_distance: 0.5,
            target_is_living: true,
            ..Default::default()
        };
        assert!(is_vanilla_critical_hit(&clean));
        assert!(!is_vanilla_critical_hit(&CritContext {
            attack_charge: 0.9,
            ..clean
        }));
        assert!(!is_vanilla_critical_hit(&CritContext {
            fall_distance: 0.0,
            ..clean
        }));
        assert!(!is_vanilla_critical_hit(&CritContext {
            on_ground: true,
            ..clean
        }));
        assert!(!is_vanilla_critical_hit(&CritContext {
            on_climbable: true,
            ..clean
        }));
        assert!(!is_vanilla_critical_hit(&CritContext {
            in_water: true,
            ..clean
        }));
        assert!(!is_vanilla_critical_hit(&CritContext {
            has_blindness: true,
            ..clean
        }));
        assert!(!is_vanilla_critical_hit(&CritContext {
            is_passenger: true,
            ..clean
        }));
        assert!(!is_vanilla_critical_hit(&CritContext {
            is_sprinting: true,
            ..clean
        }));
        assert!(!is_vanilla_critical_hit(&CritContext {
            target_is_living: false,
            ..clean
        }));
        assert!(!is_vanilla_critical_hit(&CritContext {
            fall_flying: true,
            ..clean
        }));
        // Vanilla's `instanceof LivingEntity` gate: boats, item frames,
        // minecarts and other non-living targets can never be crit; armor
        // stands are LivingEntity in vanilla and *can*.
        for name in ["boat", "item_frame", "glow_item_frame", "chest_minecart", "arrow"] {
            assert!(
                !entity_is_living(name_to_etype(name).unwrap()),
                "{name} is not a living crit target"
            );
        }
        for name in ["zombie", "armor_stand", "wither", "bee"] {
            assert!(
                entity_is_living(name_to_etype(name).unwrap()),
                "{name} is a living crit target"
            );
        }
    }

    fn crit_ctx(charge: f32, grounded: bool, falling: bool) -> bool {
        is_vanilla_critical_hit(&CritContext {
            attack_charge: charge,
            fall_distance: if falling { 0.5 } else { 0.0 },
            on_ground: grounded,
            target_is_living: true,
            ..Default::default()
        })
    }

    // #79 — the ×1.5 lands on the charge-scaled base before the enchant bonus,
    // exactly the order vanilla's `Player.attack` computes f and f1.
    #[test]
    fn p5a_parity_crit_multiplies_scaled_base_only() {
        let base = 7.0f32; // diamond sword
        let sharp = 1.5f32; // sharpness III bonus
        // Full charge crit: (7 * 1.0) * 1.5 + 1.5 = 12.0, not (7 + 1.5) * 1.5.
        assert_eq!(melee_hit_damage(base, sharp, 1.0, true), 12.0);
        // Same blow without crit: 7 + 1.5 = 8.5.
        assert_eq!(melee_hit_damage(base, sharp, 1.0, false), 8.5);
        // Half charge: base pays the 0.2 + 0.8*c^2 scale, the enchant bonus
        // pays a bare charge factor and is never crit-multiplied.
        let dmg = melee_hit_damage(base, sharp, 0.5, true);
        let expected = 7.0 * (0.2 + 0.8 * 0.25) * 1.5 + 1.5 * 0.5;
        assert!((dmg - expected).abs() < 1e-5, "{dmg} vs {expected}");
        assert!(crit_ctx(1.0, false, true));
        assert!(!crit_ctx(1.0, true, true));
    }

    // #80 — the unenchanted sword still sweeps for 1.0, and levels I..III use
    // the exact `1 + base * lvl/(lvl+1)` vanilla formula.
    #[test]
    fn p5a_parity_sweeping_edge_formula() {
        for (base, expected) in [(1.0f32, [1.0, 1.5, 1.666_666_7, 1.75]),
                                (7.0f32, [1.0, 4.5, 5.666_666_5, 6.25]),
                                (10.0f32, [1.0, 6.0, 7.666_666_5, 8.5])]
        {
            for (level, want) in expected.iter().enumerate() {
                let got = sweeping_edge_damage(level as i32, base);
                assert!(
                    (got - want).abs() < 1e-5,
                    "level {level} base {base}: {got} vs {want}"
                );
            }
        }
        assert_eq!(sweeping_edge_damage(0, 7.0), 1.0);
    }

    // #80 — sweep requires a charged, grounded, non-crit, non-sprint sword hit
    // plus the walking-speed cap and the 0.4 knockback constant.
    #[test]
    fn p5a_parity_sweeping_gate_speed_and_knockback() {
        assert!(sweeping_attack_gate(0.95, true, false, false));
        assert!(!sweeping_attack_gate(0.9, true, false, false));
        assert!(!sweeping_attack_gate(0.95, false, false, false));
        assert!(!sweeping_attack_gate(0.95, true, true, false));
        assert!(!sweeping_attack_gate(0.95, true, false, true));
        assert!(sweeping_movement_ok(0.05, 0.1));
        assert!(!sweeping_movement_ok(0.1, 0.1));
        assert!(!sweeping_movement_ok(0.2, 0.1));
        assert_eq!(SWEEP_KNOCKBACK, 0.4);
        // Geometry through the live `sweeping_attack`: a neighbour inside the
        // 1.0/0.25 inflated target box within 9.0 squared takes the hit.
        let target = PlayerAabb {
            min: [3.0, 64.0, 3.0],
            max: [4.0, 65.0, 4.0],
        };
        let near = PlayerAabb {
            min: [4.5, 64.0, 3.5],
            max: [5.0, 65.0, 4.0],
        };
        let far = PlayerAabb {
            min: [8.0, 64.0, 3.0],
            max: [9.0, 65.0, 4.0],
        };
        let sweep = sweeping_attack(
            0.95, true, false, false,
            [2.0, 64.0, 1.0], target, near, 7.0, 0,
        );
        assert!(sweep.eligible);
        assert_eq!(sweep.damage, 1.0, "unenchanted sweep pays exactly 1.0");
        // A candidate outside the inflated target box is never swept.
        assert!(!sweeping_attack(
            0.95, true, false, false,
            [2.0, 64.0, 1.0], target, far, 7.0, 0,
        )
        .eligible);
        // An attacker farther than sqrt(9.0) from the target never sweeps.
        assert!(!sweeping_attack(
            0.95, true, false, false,
            [3.5, 64.0, -1.5], target, near, 7.0, 0,
        )
        .eligible);
        // A crit blocks the sweep; so does sprinting or a short charge.
        for (sprint, crit, charge) in [(true, false, 0.95), (false, true, 0.95), (false, false, 0.85)] {
            assert!(!sweeping_attack(
                charge, true, sprint, crit,
                [2.0, 64.0, 1.0], target, near, 7.0, 3,
            )
            .eligible);
        }
    }

    // #81 — Smite's undead family: every tag member pays +2.5/level.
    #[test]
    fn p5a_parity_smite_covers_full_undead_family() {
        for name in [
            "zombie", "zombie_villager", "husk", "drowned", "zombified_piglin",
            "zoglin", "skeleton", "stray", "bogged", "wither_skeleton",
            "skeleton_horse", "zombie_horse", "phantom", "wither",
        ] {
            let etype = name_to_etype(name).unwrap();
            assert!(enchantment_targets_undead(etype), "{name}");
            assert_eq!(typed_damage_bonus(34, 5, etype), 12.5, "{name} smite V");
        }
        for name in ["spider", "creeper", "zoglin_piglin_unknown", "cow", "piglin"] {
            let Some(etype) = name_to_etype(name) else { continue };
            assert!(!enchantment_targets_undead(etype), "{name}");
            assert_eq!(typed_damage_bonus(34, 5, etype), 0.0, "{name}");
        }
        // The live path's own enchant_level id for smite is 34.
        assert_eq!(enchant_id("minecraft:smite"), Some(34));
    }

    // #81 — Bane's arthropod family plus the Slowness IV rider.
    #[test]
    fn p5a_parity_bane_covers_arthropods_and_slowness_iv() {
        for name in ["spider", "cave_spider", "silverfish", "endermite", "bee"] {
            let etype = name_to_etype(name).unwrap();
            assert!(enchantment_targets_arthropod(etype), "{name}");
            assert_eq!(typed_damage_bonus(1, 3, etype), 7.5, "{name} bane III");
        }
        assert!(!enchantment_targets_arthropod(name_to_etype("zombie").unwrap()));
        assert_eq!(bane_slowness_iv(0), None);
        assert_eq!(bane_slowness_iv(1), Some((3, 20, 30)));
        assert_eq!(bane_slowness_iv(2), Some((3, 20, 40)));
        assert_eq!(bane_slowness_iv(5), Some((3, 20, 70)));
        // Amplifier 3 is vanilla's Slowness IV on the internal wire too.
        assert_eq!(name_to_effect("slowness"), Some(1));
        assert_eq!(enchant_id("minecraft:bane_of_arthropods"), Some(1));
    }

    // #81 — Impaling's aquatic family (vanilla `#minecraft:sensitive_to_impaling`).
    #[test]
    fn p5a_parity_impaling_covers_full_aquatic_family() {
        for name in [
            "cod", "salmon", "pufferfish", "tropical_fish", "squid", "glow_squid",
            "dolphin", "turtle", "guardian", "elder_guardian", "axolotl", "tadpole",
        ] {
            let etype = name_to_etype(name).unwrap();
            assert!(enchantment_targets_aquatic(etype), "{name}");
            assert_eq!(typed_damage_bonus(15, 2, etype), 5.0, "{name} impaling II");
        }
        for name in ["drowned", "frog", "guardian_z"] {
            let Some(etype) = name_to_etype(name) else { continue };
            assert!(!enchantment_targets_aquatic(etype), "{name}");
        }
        // Drowned is undead (Smite), not aquatic — the families stay disjoint.
        let drowned = name_to_etype("drowned").unwrap();
        assert!(!enchantment_targets_aquatic(drowned));
        assert!(enchantment_targets_undead(drowned));
        assert_eq!(enchant_id("minecraft:impaling"), Some(15));
    }

    // #78 + #81 — a sword-level Fire Aspect hit on a fire-immune mob records
    // the burn but pays no damage, matching the melee gate.
    #[test]
    fn p5a_parity_fire_aspect_immune_mob_records_no_damage() {
        let _g = FireGuard::new();
        let blaze = name_to_etype("blaze").unwrap();
        ignite_mob_fire_aspect(4245, 1);
        assert_eq!(mob_fire_ticks_of(4245), server_ticks(80) as u32);
        assert!(mob_fire_immune(blaze));
        for tick in 1..=server_ticks(40) {
            assert!(!tick_mob_fire(4245, tick, mob_fire_immune(blaze), false));
        }
    }
}

#[cfg(test)]
mod vanilla_combat2_tests {
    use super::effect_tests::client_pair;
    use super::*;

    /// Restores every global a test in this lane can touch.
    struct LaneGuard {
        tick: u64,
        raining: u64,
        thundering: u64,
        blocks: Vec<(DimId, i32, i32, i32, u32)>,
    }
    impl LaneGuard {
        fn new() -> Self {
            Self {
                tick: TICK_IDX.load(Ordering::Relaxed),
                raining: RAINING.load(Ordering::Relaxed),
                thundering: THUNDERING.load(Ordering::Relaxed),
                blocks: Vec::new(),
            }
        }
        fn set_block(&mut self, dim: DimId, x: i32, y: i32, z: i32, st: u32) {
            if self
                .blocks
                .iter()
                .all(|b| !(b.0 == dim && b.1 == x && b.2 == y && b.3 == z))
            {
                self.blocks.push((dim, x, y, z, block_at_dim(dim, x, y, z)));
            }
            set_block_dim(dim, x, y, z, st);
        }
    }
    impl std::ops::Drop for LaneGuard {
        fn drop(&mut self) {
            TICK_IDX.store(self.tick, Ordering::Relaxed);
            RAINING.store(self.raining, Ordering::Relaxed);
            THUNDERING.store(self.thundering, Ordering::Relaxed);
            for (dim, x, y, z, st) in self.blocks.drain(..) {
                set_block_dim(dim, x, y, z, st);
            }
            clients()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|c| c.eid != 4242);
            arrows()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|a| a.eid != 99901);
            drops()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|d| d.item != 9999);
            for m in [
                shield_raised(),
                riptide_spin(),
            ] {
                m.lock().unwrap_or_else(|e| e.into_inner()).remove(&4242);
            }
            trident_returning()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&99901);
            trident_seen()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&99901);
        }
    }

    fn shield_ctx() -> ShieldAttackCtx {
        ShieldAttackCtx {
            raised_ticks: SHIELD_WARMUP_GT,
            cooldown_active: false,
            unblockable: false,
            look: [0.0, 1.0],
            to_attacker: [0.0, 1.0],
            attacker_can_disable: false,
            attacker_is_player: false,
            attacker_sprinting: false,
            attacker_efficiency: 0,
            disable_roll: 0.0,
            damage: 5.0,
        }
    }

    // ---------- Wind Charge ----------

    #[test]
    fn vanilla_combat2_wind_charge_profiles() {
        let p = wind_charge_profile(WindChargeKind::Player);
        assert_eq!(p.radius, 1.2);
        assert_eq!(p.knockback_multiplier, 1.22);
        assert_eq!(p.direct_hit_damage, 1.0);
        let b = wind_charge_profile(WindChargeKind::Breeze);
        assert_eq!(b.radius, 3.0);
        assert_eq!(b.knockback_multiplier, 0.6);
        assert_eq!(b.direct_hit_damage, 1.0);
    }

    #[test]
    fn vanilla_combat2_wind_charge_launch_math() {
        // Impact at origin, target eye 1m up => dir (0,1,0), dist 1.
        // strength = (1 - 1/2.4) * 1.0 * 1.22 = 0.711666...
        let dv = evaluate_wind_charge_burst(
            WindChargeKind::Player,
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
            1.0,
            1.0,
            0.0,
        )
        .unwrap();
        let want = (1.0 - 1.0 / 2.4) * 1.22;
        assert!((dv[1] - want).abs() < 1e-9, "{:?} vs {want}", dv);
        assert!(dv[0].abs() < 1e-9 && dv[2].abs() < 1e-9);
        // Breeze: same geometry, reach 6.0, mult 0.6.
        let dv = evaluate_wind_charge_burst(
            WindChargeKind::Breeze,
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
            1.0,
            1.0,
            0.0,
        )
        .unwrap();
        let want = (1.0 - 1.0 / 6.0) * 0.6;
        assert!((dv[1] - want).abs() < 1e-9);
        // seen-percent and knockback resistance both scale the launch.
        let half_seen = evaluate_wind_charge_burst(
            WindChargeKind::Player,
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
            1.0,
            0.5,
            0.0,
        )
        .unwrap();
        let player_want = (1.0 - 1.0 / 2.4) * 1.22 * 0.5;
        assert!((half_seen[1] - player_want).abs() < 1e-9);
        let resisted = evaluate_wind_charge_burst(
            WindChargeKind::Player,
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
            1.0,
            1.0,
            1.0,
        )
        .unwrap();
        assert_eq!(resisted, [0.0, 0.0, 0.0]);
    }

    #[test]
    fn vanilla_combat2_wind_charge_edges_scrub_nonfinite() {
        // Exact center: vanilla skips.
        assert_eq!(
            evaluate_wind_charge_burst(
                WindChargeKind::Player,
                [1.0, 1.0, 1.0],
                [1.0, 1.0, 1.0],
                1.0,
                1.0,
                0.0,
            ),
            None
        );
        // Out of reach (dist > radius*2).
        assert_eq!(
            evaluate_wind_charge_burst(
                WindChargeKind::Player,
                [0.0, 0.0, 0.0],
                [0.0, 0.0, 0.0],
                2.5,
                1.0,
                0.0,
            ),
            None
        );
        // Exactly at the reach boundary the falloff hits zero.
        assert_eq!(
            evaluate_wind_charge_burst(
                WindChargeKind::Player,
                [0.0, 0.0, 0.0],
                [0.0, 0.0, 0.0],
                2.4,
                1.0,
                0.0,
            ),
            Some([0.0, 0.0, 0.0])
        );
        // Zero visibility.
        assert_eq!(
            evaluate_wind_charge_burst(
                WindChargeKind::Player,
                [0.0, 0.0, 0.0],
                [0.0, 0.0, 0.0],
                1.0,
                0.0,
                0.0,
            ),
            None
        );
        // Non-finite inputs are scrubbed, never propagated.
        for (impact, pos, eye, seen, kbr) in [
            ([f64::NAN, 0.0, 0.0], [0.0; 3], 1.0, 1.0, 0.0),
            ([0.0; 3], [f64::INFINITY, 0.0, 0.0], 1.0, 1.0, 0.0),
            ([0.0; 3], [0.0; 3], f64::NAN, 1.0, 0.0),
            ([0.0; 3], [0.0; 3], 1.0, f64::NAN, 0.0),
            ([0.0; 3], [0.0; 3], 1.0, 1.0, f64::NEG_INFINITY),
        ] {
            assert_eq!(
                evaluate_wind_charge_burst(
                    WindChargeKind::Player,
                    impact,
                    pos,
                    eye,
                    seen,
                    kbr
                ),
                None
            );
        }
    }

    #[test]
    fn vanilla_combat2_wind_charge_fall_reset() {
        let _g = LaneGuard::new();
        let (c, _r) = client_pair();
        *c.fall_from.lock().unwrap_or_else(|e| e.into_inner()) = 120.0;
        wind_charge_reset_fall(&c, 64.5);
        assert_eq!(*c.fall_from.lock().unwrap_or_else(|e| e.into_inner()), 64.5);
        // Non-finite burst heights never corrupt the fall baseline.
        wind_charge_reset_fall(&c, f64::NAN);
        assert_eq!(*c.fall_from.lock().unwrap_or_else(|e| e.into_inner()), 64.5);
    }

    #[test]
    fn vanilla_combat2_wind_charge_redstone_triggers() {
        let mut g = LaneGuard::new();
        let d = DimId::Overworld;
        // Toggleable: lever, button, bell, wooden door, trapdoor, fence gate.
        for (i, name) in [
            "lever",
            "stone_button",
            "bell",
            "oak_door",
            "oak_trapdoor",
            "oak_fence_gate",
        ]
        .iter()
        .enumerate()
        {
            let Some(st) = name_to_state(name) else { continue };
            let (x, y, z) = (9400 + i as i32 * 4, 200, 0);
            g.set_block(d, x, y, z, st);
            assert!(
                wind_charge_trigger_at(d, x, y, z),
                "{name} should trigger"
            );
            assert_ne!(
                block_at_dim(d, x, y, z),
                st,
                "{name} state should have flipped"
            );
        }
        // Iron variants and redstone-held doors: never move.
        for (i, (name, prop, val)) in [
            ("iron_door", "", ""),
            ("iron_trapdoor", "", ""),
            ("oak_door", "powered", "true"),
        ]
        .iter()
        .enumerate()
        {
            let Some(st) = name_to_state(name) else { continue };
            let (x, y, z) = (9500 + i as i32 * 4, 200, 0);
            let st = if prop.is_empty() {
                st
            } else {
                breg().unwrap().with_prop(st, prop, val)
            };
            g.set_block(d, x, y, z, st);
            assert!(
                !wind_charge_trigger_at(d, x, y, z),
                "{name} must not trigger"
            );
            assert_eq!(block_at_dim(d, x, y, z), st);
        }
        // The lever is itself the power source — a burst still pulls it off.
        if let Some(st) = name_to_state("lever") {
            let st = breg().unwrap().with_prop(st, "powered", "true");
            g.set_block(d, 9600, 200, 0, st);
            assert!(wind_charge_trigger_at(d, 9600, 200, 0));
            let st2 = block_at_dim(d, 9600, 200, 0);
            assert_eq!(
                breg().unwrap().get_prop(st2, "powered").as_deref(),
                Some("false")
            );
        }
    }

    #[test]
    fn vanilla_combat2_wind_charge_door_flips_both_halves() {
        let mut g = LaneGuard::new();
        let d = DimId::Overworld;
        let Some(lower) = name_to_state("oak_door") else { return };
        let reg = breg().unwrap();
        let lower = reg.with_prop(lower, "half", "lower");
        let upper = reg.with_prop(lower, "half", "upper");
        g.set_block(d, 9600, 200, 0, lower);
        g.set_block(d, 9600, 201, 0, upper);
        assert!(wind_charge_trigger_at(d, 9600, 200, 0));
        // Both halves carry `open=true` after one burst.
        for y in [200, 201] {
            let st = block_at_dim(d, 9600, y, 0);
            assert_eq!(reg.get_prop(st, "open").as_deref(), Some("true"), "half at y={y}");
        }
    }

    #[test]
    fn vanilla_combat2_wind_charge_burst_launches_and_resets() {
        let _g = LaneGuard::new();
        let (c, mut r) = client_pair();
        *c.fall_from.lock().unwrap_or_else(|e| e.into_inner()) = 200.0;
        {
            let mut p = c.pos.lock().unwrap_or_else(|e| e.into_inner());
            p.0 = 0.0;
            p.1 = 70.0;
            p.2 = 1.5; // 1.5 blocks north of the burst
        }
        let c = Arc::new(c);
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(c.clone());
        wind_charge_burst(DimId::Overworld, WindChargeKind::Player, [0.0, 70.0, 0.0], 4242);
        // Thrower fall baseline snapped to the burst Y.
        assert_eq!(
            *c.fall_from.lock().unwrap_or_else(|e| e.into_inner()),
            70.0
        );
        // A velocity packet with a negative-Z push reached the wire.
        let dl = Instant::now() + Duration::from_secs(1);
        let mut saw_vel = false;
        while let Some((id, body)) = read_packet(&mut r, dl) {
            if id == 0x5a {
                assert_eq!(body.len(), 8);
                let vy = i16::from_be_bytes([body[4], body[5]]);
                let vz = i16::from_be_bytes([body[6], body[7]]);
                assert!(vz > 0, "burst pushes away from impact, vz={vz}");
                assert_ne!(vy, 0);
                saw_vel = true;
                break;
            }
        }
        assert!(saw_vel, "expected a 0x5a velocity packet");
    }

    // ---------- shield ----------

    #[test]
    fn vanilla_combat2_shield_arc_front_180() {
        // Look = +Z. Sweep the attacker around the full circle in 15° steps.
        for deg in (0..360).step_by(15) {
            if deg == 90 || deg == 270 {
                continue; // exact flank covered below with exact vectors
            }
            let rad = (deg as f64).to_radians();
            let mut ctx = shield_ctx();
            ctx.to_attacker = [rad.sin(), rad.cos()];
            let blocked = evaluate_shield_block(&ctx).blocked;
            let ang = (deg as f64).min(360.0 - deg as f64);
            // dot(look, to) = cos(angle from +Z); strictly > 0 inside the
            // open 180° front hemisphere (|angle| < 90).
            assert_eq!(blocked, ang < 90.0, "deg={deg}");
        }
        // Exactly at the 90° flank: dot == 0, not blocked (strict inequality).
        let mut ctx = shield_ctx();
        ctx.to_attacker = [1.0, 0.0];
        assert!(!evaluate_shield_block(&ctx).blocked);
        ctx.to_attacker = [-1.0, 0.0];
        assert!(!evaluate_shield_block(&ctx).blocked);
        // Directly behind: dot < 0.
        ctx.to_attacker = [0.0, -1.0];
        assert!(!evaluate_shield_block(&ctx).blocked);
    }

    #[test]
    fn vanilla_combat2_shield_warmup_five_ticks() {
        for raised in 0..SHIELD_WARMUP_GT {
            let mut ctx = shield_ctx();
            ctx.raised_ticks = raised;
            assert!(
                !evaluate_shield_block(&ctx).blocked,
                "raised={raised} must not block"
            );
        }
        let mut ctx = shield_ctx();
        ctx.raised_ticks = SHIELD_WARMUP_GT;
        assert!(evaluate_shield_block(&ctx).blocked);
        ctx.raised_ticks = SHIELD_WARMUP_GT + 40;
        assert!(evaluate_shield_block(&ctx).blocked);
    }

    #[test]
    fn vanilla_combat2_shield_cooldown_and_unblockable() {
        let mut ctx = shield_ctx();
        ctx.cooldown_active = true;
        assert!(!evaluate_shield_block(&ctx).blocked);
        ctx.cooldown_active = false;
        ctx.unblockable = true;
        assert!(!evaluate_shield_block(&ctx).blocked);
    }

    #[test]
    fn vanilla_combat2_shield_axe_disable_100_ticks() {
        // Disabling mobs (Vindicator/Piglin Brute etypes) always apply the full
        // 100-tick cooldown — no roll.
        assert!(mob_can_disable_shield(79));
        assert!(mob_can_disable_shield(114));
        assert!(!mob_can_disable_shield(0));
        let mut ctx = shield_ctx();
        ctx.attacker_can_disable = true;
        ctx.attacker_is_player = false;
        ctx.disable_roll = 0.999;
        assert_eq!(evaluate_shield_block(&ctx).disable_ticks, Some(100));

        // Player axe: base 25% + 5%/Efficiency; roll below the chance disables.
        let mut ctx = shield_ctx();
        ctx.attacker_can_disable = true;
        ctx.attacker_is_player = true;
        ctx.disable_roll = 0.20;
        assert_eq!(evaluate_shield_block(&ctx).disable_ticks, Some(100));
        ctx.disable_roll = 0.30;
        assert_eq!(evaluate_shield_block(&ctx).disable_ticks, None);
        // Efficiency V lifts the chance to 50%.
        ctx.attacker_efficiency = 5;
        ctx.disable_roll = 0.45;
        assert_eq!(evaluate_shield_block(&ctx).disable_ticks, Some(100));
        ctx.attacker_efficiency = 0;
        // Sprinting player axe hit: 25% + 75% = 100% — any roll < 1 disables.
        ctx.attacker_sprinting = true;
        ctx.disable_roll = 0.999;
        assert_eq!(evaluate_shield_block(&ctx).disable_ticks, Some(100));
        // No axe: never disables.
        let mut ctx = shield_ctx();
        ctx.attacker_can_disable = false;
        ctx.attacker_is_player = true;
        ctx.attacker_sprinting = true;
        ctx.disable_roll = 0.0;
        assert_eq!(evaluate_shield_block(&ctx).disable_ticks, None);
    }

    #[test]
    fn vanilla_combat2_shield_durability_math() {
        // hurtCurrentlyUsedShield: nothing under 3 damage, else 1 + floor.
        assert_eq!(vanilla_shield_attempts(0.0), 0);
        assert_eq!(vanilla_shield_attempts(2.9), 0);
        assert_eq!(vanilla_shield_attempts(3.0), 4);
        assert_eq!(vanilla_shield_attempts(3.5), 4);
        assert_eq!(vanilla_shield_attempts(7.9), 8);
        let mut ctx = shield_ctx();
        ctx.damage = 2.9;
        assert_eq!(evaluate_shield_block(&ctx).durability_attempts, 0);
        ctx.damage = 7.9;
        assert_eq!(evaluate_shield_block(&ctx).durability_attempts, 8);

        let (c, _r) = client_pair();
        let c = Arc::new(c);
        let sh = name_to_item("shield").unwrap();
        let mut comps = Vec::new();
        comp_set_varint(&mut comps, COMP_MAX_DAMAGE, 337);
        c.inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(45, (sh, 1, comps));
        damage_shield_attempts(&c, 5);
        let dmg = comp_varint(
            &c.inv.lock().unwrap_or_else(|e| e.into_inner()).get(&45).unwrap().2,
            COMP_DAMAGE,
        );
        assert_eq!(dmg, Some(5));
    }

    #[test]
    fn vanilla_combat2_shield_unbreaking_rolls_per_attempt() {
        let (c, _r) = client_pair();
        let c = Arc::new(c);
        let sh = name_to_item("shield").unwrap();
        // No Unbreaking: every attempt lands — 10 attempts is exactly 10 damage.
        let mut comps = Vec::new();
        comp_set_varint(&mut comps, COMP_MAX_DAMAGE, 337);
        c.inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(45, (sh, 1, comps));
        damage_shield_attempts(&c, 10);
        assert_eq!(
            comp_varint(
                &c.inv.lock().unwrap_or_else(|e| e.into_inner()).get(&45).unwrap().2,
                COMP_DAMAGE
            ),
            Some(10)
        );
        // Unbreaking III: each attempt lands with p=1/4; 400 attempts must end
        // below the unmodified count (the all-fail path is (3/4)^400 ~ 0).
        let mut comps = Vec::new();
        comp_set_varint(&mut comps, COMP_MAX_DAMAGE, 337);
        comp_set(
            &mut comps,
            COMP_ENCHANTMENTS,
            enchantments_data(&[(39, 3)], true),
        );
        c.inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(45, (sh, 1, comps));
        damage_shield_attempts(&c, 400);
        let dmg = comp_varint(
            &c.inv.lock().unwrap_or_else(|e| e.into_inner()).get(&45).unwrap().2,
            COMP_DAMAGE,
        )
        .unwrap();
        assert!(dmg < 400, "unbreaking must skip attempts, dmg={dmg}");
        // Unbreakable shields take no durability at all.
        let mut comps = Vec::new();
        comp_set_varint(&mut comps, COMP_MAX_DAMAGE, 337);
        comp_set(&mut comps, COMP_UNBREAKABLE, vec![]);
        c.inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(45, (sh, 1, comps));
        damage_shield_attempts(&c, 20);
        assert_eq!(
            comp_varint(
                &c.inv.lock().unwrap_or_else(|e| e.into_inner()).get(&45).unwrap().2,
                COMP_DAMAGE
            ),
            None
        );
    }

    #[test]
    fn vanilla_combat2_shield_state_tick_and_block_gate() {
        let _g = LaneGuard::new();
        let (c, _r) = client_pair();
        let c = Arc::new(c);
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(c.clone());
        TICK_IDX.store(100, Ordering::Relaxed);
        combat2_state_tick();
        // Shield down: raised clock stays empty, ctx reports 0 -> below warmup.
        assert_eq!(shield_raised_ticks(&c), 0);
        *c.blocking.lock().unwrap_or_else(|e| e.into_inner()) = true;
        combat2_state_tick();
        assert_eq!(shield_raised_ticks(&c), 0); // just raised
        TICK_IDX.store(105, Ordering::Relaxed);
        combat2_state_tick();
        assert_eq!(shield_raised_ticks(&c), 5);
        // apply_shield_disable drops the shield and arms the wall-clock cd.
        apply_shield_disable(&c);
        assert!(!*c.blocking.lock().unwrap_or_else(|e| e.into_inner()));
        let ctx = shield_hit_ctx(&c, [5.0, 0.0, 0.0], 5.0);
        assert!(ctx.cooldown_active);
        assert!(!evaluate_shield_block(&ctx).blocked);
    }

    // ---------- totem of undying ----------

    #[test]
    fn vanilla_combat2_totem_offhand_pop_effects_status35() {
        let _g = LaneGuard::new();
        let (c, mut r) = client_pair();
        let totem = name_to_item("totem_of_undying").unwrap();
        c.inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(45, (totem, 2, Vec::new()));
        c.effs.lock().unwrap_or_else(|e| e.into_inner()).push(Eff {
            id: name_to_effect("poison").unwrap_or(19),
            amp: 2,
            left: 80,
        });
        *c.hp.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
        assert!(try_trigger_totem_of_undying(&c, 0.0, TotemDamageKind::Ordinary));
        // Totem consumed (one of two), hp = 1.
        assert_eq!(
            c.inv.lock().unwrap_or_else(|e| e.into_inner()).get(&45),
            Some(&(totem, 1, Vec::new()))
        );
        assert_eq!(*c.hp.lock().unwrap_or_else(|e| e.into_inner()), 1.0);
        // Negative effect cleared; exact vanilla grant.
        let effs: Vec<(i32, i32, i32)> = c
            .effs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|e| (e.id, e.amp, e.left))
            .collect();
        assert_eq!(effs.len(), 3);
        let regen = name_to_effect("regeneration").unwrap();
        let absorb = name_to_effect("absorption").unwrap();
        let fire = name_to_effect("fire_resistance").unwrap();
        for (id, amp, left) in [
            (regen, 1, server_ticks(900) as i32),
            (absorb, 1, server_ticks(100) as i32),
            (fire, 0, server_ticks(800) as i32),
        ] {
            assert!(
                effs.iter().any(|e| e.0 == id && e.1 == amp && e.2 == left),
                "missing effect id={id} amp={amp} left={left} in {effs:?}"
            );
        }
        assert_eq!(*c.absorb.lock().unwrap_or_else(|e| e.into_inner()), 8.0);
        // Entity status 35 on the wire: i32 eid + i8 status.
        let dl = Instant::now() + Duration::from_secs(1);
        let mut saw = false;
        while let Some((id, body)) = read_packet(&mut r, dl) {
            if id == 0x1f && body.len() == 5 && body[4] == 35 {
                assert_eq!(i32::from_be_bytes([body[0], body[1], body[2], body[3]]), 4242);
                saw = true;
                break;
            }
        }
        assert!(saw, "expected entity status 35 for totem pop");
    }

    #[test]
    fn vanilla_combat2_totem_mainhand_pop() {
        let _g = LaneGuard::new();
        let (c, _r) = client_pair();
        let totem = name_to_item("totem_of_undying").unwrap();
        *c.held.lock().unwrap_or_else(|e| e.into_inner()) = 3;
        c.inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(39, (totem, 1, Vec::new())); // main hand = 36 + held
        *c.hp.lock().unwrap_or_else(|e| e.into_inner()) = -4.0;
        assert!(try_trigger_totem_of_undying(
            &c,
            -4.0,
            TotemDamageKind::Ordinary
        ));
        assert!(!c.inv.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&39));
        assert_eq!(*c.hp.lock().unwrap_or_else(|e| e.into_inner()), 1.0);
    }

    #[test]
    fn vanilla_combat2_totem_void_and_kill_bypass() {
        let _g = LaneGuard::new();
        for kind in [TotemDamageKind::Void, TotemDamageKind::Kill] {
            let (c, _r) = client_pair();
            let totem = name_to_item("totem_of_undying").unwrap();
            c.inv
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(45, (totem, 1, Vec::new()));
            *c.hp.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
            assert!(
                !try_trigger_totem_of_undying(&c, 0.0, kind),
                "{kind:?} must bypass totem protection"
            );
            assert_eq!(
                c.inv.lock().unwrap_or_else(|e| e.into_inner()).get(&45),
                Some(&(totem, 1, Vec::new()))
            );
            assert_eq!(*c.hp.lock().unwrap_or_else(|e| e.into_inner()), 0.0);
        }
    }

    #[test]
    fn vanilla_combat2_totem_check_void_zone_no_pop() {
        let _g = LaneGuard::new();
        let (c, _r) = client_pair();
        let totem = name_to_item("totem_of_undying").unwrap();
        c.inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(45, (totem, 1, Vec::new()));
        // Below the overworld floor: fellOutOfWorld bypasses protection.
        {
            let mut p = c.pos.lock().unwrap_or_else(|e| e.into_inner());
            p.1 = f64::from(DimId::Overworld.min_y()) - 5.0;
        }
        *c.hp.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
        totem_check(&c);
        assert_eq!(*c.hp.lock().unwrap_or_else(|e| e.into_inner()), 0.0);
        assert_eq!(
            c.inv.lock().unwrap_or_else(|e| e.into_inner()).get(&45),
            Some(&(totem, 1, Vec::new()))
        );
        // Back inside the world the same lethal check pops the totem.
        {
            let mut p = c.pos.lock().unwrap_or_else(|e| e.into_inner());
            p.1 = 70.0;
        }
        totem_check(&c);
        assert_eq!(*c.hp.lock().unwrap_or_else(|e| e.into_inner()), 1.0);
    }

    #[test]
    fn vanilla_combat2_totem_nonlethal_and_empty_hand() {
        let _g = LaneGuard::new();
        let (c, _r) = client_pair();
        let totem = name_to_item("totem_of_undying").unwrap();
        c.inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(45, (totem, 1, Vec::new()));
        // Survived hit: hp still positive -> no pop.
        assert!(!try_trigger_totem_of_undying(&c, 0.5, TotemDamageKind::Ordinary));
        assert_eq!(
            c.inv.lock().unwrap_or_else(|e| e.into_inner()).get(&45),
            Some(&(totem, 1, Vec::new()))
        );
        // Lethal but no totem anywhere: still no pop.
        c.inv.lock().unwrap_or_else(|e| e.into_inner()).clear();
        assert!(!try_trigger_totem_of_undying(&c, 0.0, TotemDamageKind::Ordinary));
    }

    // ---------- trident specials ----------

    fn trident_ctx() -> TridentSpecialCtx {
        TridentSpecialCtx {
            loyalty: 0,
            riptide: 0,
            channeling: 0,
            wet: false,
            on_ground: false,
            look: [0.0, 0.0, 1.0],
            trident: [0.0, 0.0, 0.0],
            owner: [0.0, 0.0, 10.0],
            thundering: false,
            open_sky: true,
        }
    }

    #[test]
    fn vanilla_combat2_trident_loyalty_levels_return() {
        for level in 1..=3i64 {
            let mut ctx = trident_ctx();
            ctx.loyalty = level;
            ctx.trident = [0.0, 0.0, 0.0];
            ctx.owner = [0.0, 0.0, 10.0];
            let ret = evaluate_trident_special(&ctx)
                .loyalty_return
                .expect("loyalty must return");
            let want = 0.05 * level as f64;
            assert!(
                (ret[2] - want).abs() < 1e-9,
                "level {level}: {ret:?} vs z={want}"
            );
            assert!(ret[0].abs() < 1e-9 && ret[1].abs() < 1e-9);
        }
        // Out-of-range or absent loyalty never returns.
        for level in [0, 4, -1] {
            let mut ctx = trident_ctx();
            ctx.loyalty = level;
            assert_eq!(
                evaluate_trident_special(&ctx).loyalty_return,
                None,
                "level {level}"
            );
        }
    }

    #[test]
    fn vanilla_combat2_trident_loyalty_returns_from_void() {
        // Below min_y the return impulse still points at the owner's eye.
        let mut ctx = trident_ctx();
        ctx.loyalty = 2;
        ctx.trident = [3.0, f64::from(DimId::Overworld.min_y()) - 100.0, 0.0];
        ctx.owner = [0.0, 64.0, 0.0];
        let ret = evaluate_trident_special(&ctx)
            .loyalty_return
            .expect("void must not gate loyalty return");
        let mag = (ret[0] * ret[0] + ret[1] * ret[1] + ret[2] * ret[2]).sqrt();
        assert!((mag - 0.10).abs() < 1e-9, "speed 0.05*2, got {mag}");
        assert!(ret[1] > 0.0, "impulse points up toward the owner");
    }

    #[test]
    fn vanilla_combat2_trident_riptide_speed_spin_gates() {
        // Levels 1..=3, wet: velocity = look * (0.75 + 0.75*level), spin 20.
        for level in 1..=3i64 {
            let mut ctx = trident_ctx();
            ctx.riptide = level;
            ctx.wet = true;
            ctx.look = [0.0, 0.0, 1.0];
            let RiptideOutcome::Launch(l) = evaluate_trident_special(&ctx).riptide else {
                panic!("level {level} wet must launch");
            };
            let want = 0.75 + 0.75 * level as f64;
            assert!(
                (l.velocity[2] - want).abs() < 1e-9,
                "level {level}: {:?} vs {want}",
                l.velocity
            );
            assert_eq!(l.spin_ticks, 20);
            assert_eq!(l.ground_boost, 0.0);
        }
        // Non-unit look vectors normalize, launch stays along the look dir.
        let mut ctx = trident_ctx();
        ctx.riptide = 1;
        ctx.wet = true;
        ctx.look = [0.0, 3.0, 4.0];
        let RiptideOutcome::Launch(l) = evaluate_trident_special(&ctx).riptide else {
            panic!()
        };
        let mag =
            (l.velocity[0].powi(2) + l.velocity[1].powi(2) + l.velocity[2].powi(2)).sqrt();
        assert!((mag - 1.5).abs() < 1e-9);
        assert!(l.velocity[1] > 0.0 && l.velocity[2] > 0.0);
        // Ground release adds the vanilla 1.2 boost.
        ctx.on_ground = true;
        let RiptideOutcome::Launch(l) = evaluate_trident_special(&ctx).riptide else {
            panic!()
        };
        assert_eq!(l.ground_boost, 1.2);
    }

    #[test]
    fn vanilla_combat2_trident_riptide_dry_refused() {
        for level in 1..=3i64 {
            let mut ctx = trident_ctx();
            ctx.riptide = level;
            ctx.wet = false;
            assert_eq!(
                evaluate_trident_special(&ctx).riptide,
                RiptideOutcome::Refused,
                "dry riptide {level} must refuse"
            );
        }
        let mut ctx = trident_ctx();
        ctx.riptide = 0;
        ctx.wet = true;
        assert_eq!(
            evaluate_trident_special(&ctx).riptide,
            RiptideOutcome::NotRiptide
        );
        // A non-finite look vector refuses rather than NaN-ing the velocity.
        let mut ctx = trident_ctx();
        ctx.riptide = 1;
        ctx.wet = true;
        ctx.look = [f64::NAN, 0.0, 0.0];
        assert_eq!(
            evaluate_trident_special(&ctx).riptide,
            RiptideOutcome::Refused
        );
    }

    #[test]
    fn vanilla_combat2_trident_channeling_thunder_and_sky() {
        let mut ctx = trident_ctx();
        ctx.channeling = 1;
        ctx.thundering = true;
        ctx.open_sky = true;
        assert!(evaluate_trident_special(&ctx).channeling_lightning);
        // Rain without a thunderstorm: no lightning.
        ctx.thundering = false;
        assert!(!evaluate_trident_special(&ctx).channeling_lightning);
        // Covered sky during thunder: no lightning.
        ctx.thundering = true;
        ctx.open_sky = false;
        assert!(!evaluate_trident_special(&ctx).channeling_lightning);
        // No channeling on the trident: never.
        let mut ctx = trident_ctx();
        ctx.thundering = true;
        ctx.open_sky = true;
        assert!(!evaluate_trident_special(&ctx).channeling_lightning);
    }

    #[test]
    fn vanilla_combat2_trident_loyalty_return_tick() {
        let _g = LaneGuard::new();
        let (c, _r) = client_pair();
        {
            let mut p = c.pos.lock().unwrap_or_else(|e| e.into_inner());
            p.0 = 16.0;
            p.1 = 70.0;
            p.2 = 10.0;
        }
        let c = Arc::new(c);
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(c.clone());
        region()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .hot
            .insert((1, 0));
        let mut comps = Vec::new();
        comp_set(
            &mut comps,
            COMP_ENCHANTMENTS,
            enchantments_data(&[(19, 2)], true),
        );
        arrows().lock().unwrap_or_else(|e| e.into_inner()).push(Arrow {
            eid: 99901,
            owner: 4242,
            x: 22.0,
            y: 70.9,
            z: 10.0,
            dx: 0.0,
            dy: 0.0,
            dz: 0.0,
            ttl: 100,
            dmg: 8.0,
            fire: 0,
            punch: 0,
            kind: 1,
            loy: 2,
            ret_item: 9999,
            ret_comps: comps,
            pierce: 0,
            last_hit: -1,
            stuck: false,
        });
        trident_returning()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(99901);
        let before = {
            let ar = arrows().lock().unwrap_or_else(|e| e.into_inner());
            let a = ar.iter().find(|a| a.eid == 99901).unwrap();
            ((a.x - 16.0).powi(2) + (a.y - 70.9).powi(2)).sqrt()
        };
        arrow_tick();
        let after = {
            let ar = arrows().lock().unwrap_or_else(|e| e.into_inner());
            let a = ar.iter().find(|a| a.eid == 99901).unwrap();
            ((a.x - 16.0).powi(2) + (a.y - 70.9).powi(2)).sqrt()
        };
        assert!(
            after < before,
            "loyalty trident must close on its owner ({before} -> {after})"
        );
        // Drop it on the owner's eye: next tick lands it as a pickup there.
        {
            let mut ar = arrows().lock().unwrap_or_else(|e| e.into_inner());
            let a = ar.iter_mut().find(|a| a.eid == 99901).unwrap();
            a.x = 16.0;
            a.y = 70.9;
            a.z = 10.0;
        }
        arrow_tick();
        assert!(arrows()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .all(|a| a.eid != 99901));
        let drop_at_owner = drops()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|d| d.item == 9999 && (d.x - 16.0).abs() < 1.0);
        assert!(drop_at_owner, "returned trident lands at the owner's feet");
        region()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .hot
            .remove(&(1, 0));
    }

    #[test]
    fn vanilla_combat2_riptide_spin_state() {
        let _g = LaneGuard::new();
        TICK_IDX.store(1000, Ordering::Relaxed);
        riptide_spin()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(4242, 1000 + server_ticks(20));
        assert_eq!(spin_attack_ticks_left(4242), 20);
        TICK_IDX.store(1019, Ordering::Relaxed);
        assert_eq!(spin_attack_ticks_left(4242), 1);
        TICK_IDX.store(1020, Ordering::Relaxed);
        assert_eq!(spin_attack_ticks_left(4242), 0);
    }

    #[test]
    fn parity_live_wire_wind_totem_trident() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let mut lane = LaneGuard::new();
        let reg = crate::block_registry::breg().unwrap();

        // Wind charge: a charge entity contacting a solid block bursts
        // through the live `combat2_state_tick` — the same tick `arrow_tick`
        // runs every arrow pass.
        let charge_type = name_to_etype("wind_charge").expect("wind_charge etype");
        let charge = new_live_mob(
            charge_type,
            DimId::Overworld,
            (33_000.5, 81.0, 33_000.5, 0.0, 0.0),
            1.0,
            None,
        );
        mobs().lock().unwrap_or_else(|e| e.into_inner()).push(charge.clone());
        lane.set_block(DimId::Overworld, 33_000, 81, 33_000,
            reg.state_of("minecraft:stone", None));
        combat2_state_tick();
        assert_eq!(*charge.hp.lock().unwrap_or_else(|e| e.into_inner()), 0.0,
            "solid contact bursts the charge through wind_charge_burst");
        mobs().lock().unwrap_or_else(|e| e.into_inner())
            .retain(|m| m.eid != charge.eid);

        // Cauldron extinguish: a burning mob standing in a water cauldron is
        // quenched and the level drops — `mob_hurt_window_tick` runs this
        // every live arrow tick before the arrow sweep.
        let zombie = name_to_etype("zombie").expect("zombie etype");
        let mob = new_live_mob(
            zombie,
            DimId::Overworld,
            (33_010.5, 81.0, 33_000.5, 0.0, 0.0),
            20.0,
            None,
        );
        mobs().lock().unwrap_or_else(|e| e.into_inner()).push(mob.clone());
        *mob.fire_ticks.lock().unwrap_or_else(|e| e.into_inner()) = 80;
        let cauldron = reg.with_prop(
            reg.state_of("minecraft:water_cauldron", None), "level", "2");
        lane.set_block(DimId::Overworld, 33_010, 81, 33_000, cauldron);
        mob_hurt_window_tick();
        assert_eq!(*mob.fire_ticks.lock().unwrap_or_else(|e| e.into_inner()), 0,
            "water cauldron quenches a burning entity");
        assert_eq!(
            reg.get_prop(block_at_dim(DimId::Overworld, 33_010, 81, 33_000), "level").as_deref(),
            Some("1"),
            "extinguishing drops one cauldron level",
        );
        mobs().lock().unwrap_or_else(|e| e.into_inner()).retain(|m| m.eid != mob.eid);

        // Totem: the live damage path's totem check keeps the holder alive.
        let (client, _rx) = client_pair();
        client
            .inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(45, (1163, 1, Vec::new()));
        assert!(try_trigger_totem_of_undying(&client, -1.0, TotemDamageKind::Ordinary));
        assert_eq!(*client.hp.lock().unwrap_or_else(|e| e.into_inner()), 1.0);

        // Trident: a returning loyalty trident closes on its owner through
        // `arrow_tick`'s live `trident_returning` pass.
        let (owner, _r2) = client_pair();
        {
            let mut p = owner.pos.lock().unwrap_or_else(|e| e.into_inner());
            p.0 = 16.0;
            p.1 = 70.0;
            p.2 = 10.0;
        }
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(Arc::new(owner));
        region().lock().unwrap_or_else(|e| e.into_inner()).hot.insert((1, 0));
        let mut comps = Vec::new();
        comp_set(&mut comps, COMP_ENCHANTMENTS, enchantments_data(&[(19, 2)], true));
        arrows().lock().unwrap_or_else(|e| e.into_inner()).push(Arrow {
            eid: 99901,
            owner: 4242,
            x: 22.0,
            y: 70.9,
            z: 10.0,
            dx: 0.0,
            dy: 0.0,
            dz: 0.0,
            ttl: 100,
            dmg: 8.0,
            fire: 0,
            punch: 0,
            kind: 1,
            loy: 2,
            ret_item: 9999,
            ret_comps: comps,
            pierce: 0,
            last_hit: -1,
            stuck: false,
        });
        trident_returning().lock().unwrap_or_else(|e| e.into_inner()).insert(99901);
        let before = {
            let ar = arrows().lock().unwrap_or_else(|e| e.into_inner());
            let a = ar.iter().find(|a| a.eid == 99901).unwrap();
            ((a.x - 16.0).powi(2) + (a.y - 70.9).powi(2)).sqrt()
        };
        arrow_tick();
        let after = {
            let ar = arrows().lock().unwrap_or_else(|e| e.into_inner());
            let a = ar.iter().find(|a| a.eid == 99901).unwrap();
            ((a.x - 16.0).powi(2) + (a.y - 70.9).powi(2)).sqrt()
        };
        assert!(after < before, "loyalty trident returns through arrow_tick");
    }
}

/// MEGA-SCALE T1 + T3 fixtures: a large power-aware blast slices into bounded
/// per-tick rings, invalid input fails closed without a mutation, and a
/// 2,000-strong primed-TNT wave ticks in one pass.
#[cfg(test)]
mod mega_scale_t1t3_tests {
    use super::*;

    /// Save/restore for every shared store the blast and primed-TNT fixtures
    /// touch. Restored on `Drop`, so a failed assertion still leaves the next
    /// test a clean world.
    struct MegaWorld {
        world: HashMap<DimChunkKey, HashMap<u32, u32>>,
        primed: Vec<Primed>,
        drops: Vec<Drop>,
        drop_pending: HashSet<i32>,
        xps: Vec<Xp>,
        xp_pending: HashSet<i32>,
        queue: std::collections::VecDeque<MegaBlastJob>,
        clients: Vec<Arc<Client>>,
        forced: HashSet<(i32, i32)>,
        region: RegionSets,
        rng: u64,
        tile_drops: u8,
        scheduled: scheduled_ticks::ScheduledTicks,
    }

    impl MegaWorld {
        fn take() -> Self {
            let tile_drops = GR_DO_TILE_DROPS.swap(1, Ordering::Relaxed);
            Self {
                world: std::mem::take(&mut *world().lock().unwrap_or_else(|e| e.into_inner())),
                primed: std::mem::take(&mut *primed().lock().unwrap_or_else(|e| e.into_inner())),
                drops: std::mem::take(&mut *drops().lock().unwrap_or_else(|e| e.into_inner())),
                drop_pending: std::mem::take(
                    &mut *drop_pending().lock().unwrap_or_else(|e| e.into_inner()),
                ),
                xps: std::mem::take(&mut *xps().lock().unwrap_or_else(|e| e.into_inner())),
                xp_pending: std::mem::take(
                    &mut *xp_pending().lock().unwrap_or_else(|e| e.into_inner()),
                ),
                queue: std::mem::take(
                    &mut *mega_blast_queue().lock().unwrap_or_else(|e| e.into_inner()),
                ),
                clients: std::mem::take(&mut *clients().lock().unwrap_or_else(|e| e.into_inner())),
                forced: std::mem::take(
                    &mut *forceloaded().lock().unwrap_or_else(|e| e.into_inner()),
                ),
                region: test_region(),
                rng: world_rng::saved_state(),
                tile_drops,
                scheduled: std::mem::replace(
                    &mut *block_ticks().lock().unwrap_or_else(|e| e.into_inner()),
                    scheduled_ticks::ScheduledTicks::new(),
                ),
            }
        }
    }

    impl std::ops::Drop for MegaWorld {
        fn drop(&mut self) {
            *world().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.world);
            *primed().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.primed);
            *drops().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.drops);
            *drop_pending().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.drop_pending);
            *xps().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.xps);
            *xp_pending().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.xp_pending);
            *mega_blast_queue().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.queue);
            *clients().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.clients);
            *forceloaded().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.forced);
            set_test_region(self.region.0.clone(), self.region.1.clone());
            world_rng::restore(self.rng);
            GR_DO_TILE_DROPS.store(self.tile_drops, Ordering::Relaxed);
            *block_ticks().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::replace(
                &mut self.scheduled,
                scheduled_ticks::ScheduledTicks::new(),
            );
        }
    }

    /// Write `st` into every cell of the `(2*half+1)^3` cube centred on `at`,
    /// bypassing `set_block` so explicit entries — including air — stay in the
    /// overlay and `block_at` never evaluates worldgen inside the blast bounds.
    fn fill_cube(at: (i32, i32, i32), half: i32, st: u32) {
        let mut w = world().lock().unwrap_or_else(|e| e.into_inner());
        for x in at.0 - half..=at.0 + half {
            for z in at.2 - half..=at.2 + half {
                let c = w.entry((DimId::Overworld, x >> 4, z >> 4)).or_default();
                for y in at.1 - half..=at.1 + half {
                    c.insert(local_idx(x, y, z), st);
                }
            }
        }
    }

    /// Count cells still holding `st` inside the same cube, reading the overlay
    /// under one lock (a destroyed cell reads `Some(0)` or no entry at all).
    fn count_in_cube(at: (i32, i32, i32), half: i32, st: u32) -> usize {
        let w = world().lock().unwrap_or_else(|e| e.into_inner());
        let mut n = 0usize;
        for x in at.0 - half..=at.0 + half {
            for z in at.2 - half..=at.2 + half {
                let Some(c) = w.get(&(DimId::Overworld, x >> 4, z >> 4)) else {
                    continue;
                };
                for y in at.1 - half..=at.1 + half {
                    if c.get(&local_idx(x, y, z)) == Some(&st) {
                        n += 1;
                    }
                }
            }
        }
        n
    }

    /// A power-24 blast over a resistance-0 field destroys more than one
    /// `MEGA_BLAST_MAX_PER_TICK` slice can carry: the job queues, each tick
    /// removes a bounded ring, bedrock survives, and the coalesced drops
    /// conserve every item in stacks no larger than 64.
    #[test]
    fn mega_scale_t1_mega_blast_slices_across_ticks_and_coalesces_drops() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _w = MegaWorld::take();
        // Deterministic ray rolls: the per-ray power factors draw from the
        // world stream, so seeding pins the whole destroyed set.
        world_rng::seed(0x5eed_b1a57);
        let reg = breg().unwrap();
        // red_mushroom: blast resistance 0.0 (rays reach their full ~16-30
        // block envelope at power 24), drops exactly one of itself, emits no
        // light and is neither fluid nor leaf — the cleanest mass fill.
        let mushroom = reg.state_of("minecraft:red_mushroom", None);
        let bedrock = reg.state_of("minecraft:bedrock", None);
        let drop_item = reg.block_drop(mushroom).unwrap();
        let at = (500_032, 100, -500_032);
        // The kernel snapshots origin ±(power*1.75 + 2); fill that whole box so
        // `block_at` only ever reads the overlay.
        let half = (24.0f64 * 1.75).ceil() as i32 + 2;
        fill_cube(at, half, mushroom);
        let bedrock_cells = [
            (at.0 + 1, at.1, at.2),
            (at.0 - 2, at.1 + 1, at.2),
            (at.0, at.1 - 1, at.2 + 3),
            (at.0 + 4, at.1 + 4, at.2 - 4),
        ];
        for &(x, y, z) in &bedrock_cells {
            set_block(x, y, z, bedrock);
        }
        let before = count_in_cube(at, half, mushroom);
        explode_at(at.0 as f64 + 0.5, at.1 as f64, at.2 as f64 + 0.5, 24.0);
        let mut prev = count_in_cube(at, half, mushroom);
        let mut deltas = vec![before - prev];
        // The queue must still hold the continuation rings after the call.
        assert!(
            !mega_blast_queue()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "a >{MEGA_BLAST_MAX_PER_TICK}-cell blast must leave queued rings"
        );
        for _ in 0..128 {
            if mega_blast_queue()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
            {
                break;
            }
            // The production drain point: one bounded ring per primed_tick.
            primed_tick();
            let cur = count_in_cube(at, half, mushroom);
            deltas.push(prev - cur);
            prev = cur;
        }
        assert!(
            mega_blast_queue()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "the ring queue must drain to completion across ticks"
        );
        let destroyed = before - prev;
        assert!(
            destroyed > MEGA_BLAST_MAX_PER_TICK,
            "power 24 over resistance-0 blocks must destroy more than one \
             tick slice ({destroyed} cells)"
        );
        assert!(
            deltas.iter().all(|&d| d <= MEGA_BLAST_MAX_PER_TICK),
            "no tick may remove more than {MEGA_BLAST_MAX_PER_TICK} cells: {deltas:?}"
        );
        for &(x, y, z) in &bedrock_cells {
            assert_eq!(block_at(x, y, z), bedrock, "bedrock at {x},{y},{z} was destroyed");
        }
        // Every destroyed cell dropped exactly one item, coalesced into full
        // stacks: total conserved, every stack <= 64.
        let dp = drops().lock().unwrap_or_else(|e| e.into_inner());
        let blast_drops: Vec<&Drop> = dp.iter().filter(|d| d.item == drop_item).collect();
        assert!(!blast_drops.is_empty(), "the blast produced no drops");
        assert!(
            blast_drops.iter().all(|d| d.cnt <= MEGA_BLAST_STACK as u8),
            "a coalesced stack exceeds {MEGA_BLAST_STACK}"
        );
        let total: u64 = blast_drops.iter().map(|d| u64::from(d.cnt)).sum();
        assert_eq!(total, destroyed as u64, "dropped items must equal destroyed cells");
        assert!(
            dp.iter().all(|d| d.item == drop_item),
            "mushroom field must not produce foreign drops"
        );
    }

    /// NaN, infinite, zero and negative power — and non-finite origins — are
    /// inert: no cell mutates, nothing queues, no drop spawns.
    #[test]
    fn mega_scale_t1_nan_and_negative_power_fail_closed() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _w = MegaWorld::take();
        let reg = breg().unwrap();
        let mushroom = reg.state_of("minecraft:red_mushroom", None);
        let at = (600_032, 100, 600_032);
        let (half, cells) = (6, 13 * 13 * 13);
        fill_cube(at, half, mushroom);
        let assert_inert = |label: &str| {
            assert_eq!(
                count_in_cube(at, half, mushroom),
                cells,
                "{label} mutated cells"
            );
            assert!(
                mega_blast_queue()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty(),
                "{label} queued a ring plan"
            );
            assert!(
                drops()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty(),
                "{label} spawned drops"
            );
        };
        let (ox, oy, oz) = (at.0 as f64 + 0.5, at.1 as f64, at.2 as f64 + 0.5);
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -4.0] {
            explode_at(ox, oy, oz, bad);
            assert_inert(&format!("power {bad}"));
        }
        for (x, y, z) in [
            (f64::NAN, oy, oz),
            (ox, f64::INFINITY, oz),
            (ox, oy, f64::NEG_INFINITY),
        ] {
            explode_at(x, y, z, 4.0);
            assert_inert(&format!("origin ({x}, {y}, {z})"));
        }
        // Sanity that the path is armed at all: a finite positive power must
        // actually remove cells.
        explode_at(ox, oy, oz, 4.0);
        assert!(
            count_in_cube(at, half, mushroom) < cells,
            "a valid power-4 blast removed nothing"
        );
    }

    /// 2,000 primed TNTs share one fuse clock: every `primed_tick` decrements
    /// each survivor exactly once, and the detonation pass removes all 2,000
    /// in a single stable partition — no quadratic `Vec::remove` shifting.
    /// TNT blocks caught in the blasts prime with the vanilla 0.5-1.5s chain
    /// fuse, proving chains still work through the new path.
    #[test]
    fn mega_scale_t3_primed_tnt_2000_chain_ticks_linear_without_quadratic_stall() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _w = MegaWorld::take();
        let reg = breg().unwrap();
        let tnt = reg.state_of("minecraft:tnt", None);
        let at = (700_000, 100, -700_000);
        // Explicit air: each power-4 blast snapshots a 19^3 box of overlay
        // cells instead of evaluating worldgen per cell.
        fill_cube(at, 12, 0);
        // Live TNT blocks inside the blast envelope chain-prime once each.
        let tnt_cells = [
            (at.0 + 1, at.1, at.2),
            (at.0 - 2, at.1, at.2),
            (at.0, at.1, at.2 + 1),
        ];
        for &(x, y, z) in &tnt_cells {
            set_block(x, y, z, tnt);
        }
        let chunk = (at.0 >> 4, at.2 >> 4);
        set_test_region(HashSet::from([chunk]), HashSet::from([chunk]));
        const N: i32 = 2000;
        {
            let mut pl = primed().lock().unwrap_or_else(|e| e.into_inner());
            for i in 0..N {
                pl.push(Primed {
                    eid: 7_000_000 + i,
                    x: at.0 as f64 + 0.5,
                    y: at.1 as f64,
                    z: at.2 as f64 + 0.5,
                    left: 3,
                });
            }
        }
        for expected in [2, 1] {
            primed_tick();
            let pl = primed().lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(pl.len(), N as usize, "a fuse of {expected} must not detonate early");
            assert!(
                pl.iter().all(|p| p.left == expected),
                "every fuse must decrement exactly once per tick"
            );
        }
        primed_tick();
        let pl = primed().lock().unwrap_or_else(|e| e.into_inner());
        // Only the chain-primed TNTs survive the wave; each destroyed TNT
        // block produced exactly one.
        let chained = tnt_cells
            .iter()
            .filter(|&&(x, y, z)| block_at(x, y, z) == 0)
            .count();
        assert!(chained >= 1, "at least one TNT block must chain-prime");
        assert_eq!(
            pl.len(),
            chained,
            "every destroyed TNT block primes exactly once"
        );
        let (lo, hi) = (server_ticks(10) as i32, server_ticks(30) as i32);
        assert!(
            pl.iter().all(|p| (lo..=hi).contains(&p.left)),
            "chain fuses must stay inside the vanilla 0.5-1.5s window"
        );
    }
}
